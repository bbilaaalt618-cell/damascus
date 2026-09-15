//! The Fold Loop.
//!
//! For every atomic step:
//!   1. generate best-of-N candidate edit-sets (test-time scaling),
//!   2. verify each in an isolated sandbox (the objective gate),
//!   3. select the best passing candidate,
//!   4. if none pass, run reflexion repair, then recursively re-atomize,
//!   5. apply the winner to the real tree and record it.
//!
//! Quality is produced by the *process*, not the model: nothing is accepted
//! until it provably builds, passes its check, and clears lints.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use anyhow::{anyhow, Result};
use chrono::Utc;
use futures::StreamExt;

use crate::config::Config;
use crate::context;
use crate::edits::apply_blocks;
use crate::filter::{prefilter, Contract, Prefilter};
use crate::generate::{repair_once, sample_candidates, Candidate};
use crate::ledger::{new_run_id, Ledger, RunMeta, StepRecord};
use crate::plan::{make_plan, Plan, Step};
use crate::prompts;
use crate::provider::{ChatProvider, ChatRequest, Message};
use crate::sandbox::Sandbox;
use crate::select::{select_best, Scored};
use crate::slice::RepoIndex;
use crate::tree;
use crate::ui::Ui;
use crate::verify::{verify, Verdict};

/// Feature toggles, mainly for ablation/benchmarking. All default false (every
/// subsystem on).
#[derive(Debug, Clone, Copy, Default)]
pub struct Ablation {
    /// Disable AST slicing + micro-patch contract (use whole-file context).
    pub no_slice: bool,
    /// Disable the deterministic syntax/contract pre-filter (sandbox-only).
    pub no_filter: bool,
    /// Disable recursive re-atomization of hard steps.
    pub no_decompose: bool,
}

pub struct Orchestrator<'a> {
    provider: &'a dyn ChatProvider,
    cfg: &'a Config,
    root: PathBuf,
    ui: Ui,
    ablation: Ablation,
}

#[derive(Debug, Default)]
pub struct RunOutcome {
    pub steps_total: usize,
    pub steps_succeeded: usize,
    pub steps_failed: usize,
    pub final_review: Option<String>,
}

impl RunOutcome {
    pub fn all_passed(&self) -> bool {
        self.steps_failed == 0 && self.steps_total > 0
    }
}

enum StepStatus {
    Success {
        gates: String,
        candidates: usize,
        repairs: usize,
    },
    Failed {
        reason: String,
    },
}

/// Result of verifying a batch of candidates.
enum Collected {
    Passing {
        scored: Vec<Scored>,
        tried: usize,
    },
    NonePassed {
        failure_log: String,
        /// The closest failing candidate's raw output, for sequential refinement.
        best_attempt: Option<String>,
    },
}

impl<'a> Orchestrator<'a> {
    pub fn new(provider: &'a dyn ChatProvider, cfg: &'a Config, root: PathBuf, ui: Ui) -> Self {
        Orchestrator::with_ablation(provider, cfg, root, ui, Ablation::default())
    }

    pub fn with_ablation(
        provider: &'a dyn ChatProvider,
        cfg: &'a Config,
        root: PathBuf,
        ui: Ui,
        ablation: Ablation,
    ) -> Self {
        Orchestrator {
            provider,
            cfg,
            root,
            ui,
            ablation,
        }
    }

    /// Pre-filter respecting the `no_filter` ablation: when disabled, only patch
    /// applicability is checked (no syntax/contract gating).
    fn run_prefilter(&self, blocks: &[crate::edits::EditBlock], contract: &Contract) -> Prefilter {
        if self.ablation.no_filter {
            match crate::edits::compute_changes(&self.root, blocks) {
                Ok(c) => Prefilter::Pass(c),
                Err(e) => Prefilter::RejectApply(e.to_string()),
            }
        } else {
            prefilter(&self.root, blocks, contract)
        }
    }

    pub async fn run(&self, task: &str) -> Result<RunOutcome> {
        self.ui.banner(
            &self.cfg.models.drafter,
            self.cfg.scaling.candidates,
            self.cfg.scaling.repair_rounds,
        );

        self.ui.phase("plan", "decomposing task into atomic steps…");
        let summary = context::repo_summary(&self.root);
        let plan: Plan = make_plan(self.provider, self.cfg, task, &summary).await?;
        self.ui
            .success(&format!("plan ready: {} step(s)", plan.steps.len()));
        for (i, s) in plan.steps.iter().enumerate() {
            self.ui.dim(&format!("  {}. {}", i + 1, s.title));
        }

        let meta = RunMeta {
            id: new_run_id(),
            task: task.to_string(),
            started_at: Utc::now().to_rfc3339(),
            model_drafter: self.cfg.models.drafter.clone(),
            candidates: self.cfg.scaling.candidates,
        };
        let ledger = Ledger::create(&self.root, &meta)?;
        self.ui
            .dim(&format!("  ledger: {}", ledger.dir().display()));

        let mut outcome = RunOutcome::default();
        let mut changed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let total = plan.steps.len();
        let mut budget = self.cfg.scaling.max_steps;

        for (idx, step) in plan.steps.iter().enumerate() {
            if budget == 0 {
                self.ui.warn("step budget exhausted; stopping");
                break;
            }
            budget -= 1;
            outcome.steps_total += 1;
            self.ui.step(idx, total, &step.title);

            let status = self
                .process_step(task, step, 0, &mut budget, &mut changed)
                .await;
            let record = self.record_for(idx, step, &status, &mut outcome);
            ledger.record_step(&record)?;
        }

        if outcome.steps_succeeded > 0 {
            self.ui.phase("review", "running final critique…");
            if let Some(review) = self.final_review(task, &changed).await {
                outcome.final_review = Some(review);
            }
        }

        ledger.write_summary(&self.summary_md(&outcome))?;
        Ok(outcome)
    }

    fn record_for(
        &self,
        idx: usize,
        step: &Step,
        status: &StepStatus,
        outcome: &mut RunOutcome,
    ) -> StepRecord {
        match status {
            StepStatus::Success {
                gates,
                candidates,
                repairs,
            } => {
                self.ui.success(&format!("step accepted ({gates})"));
                outcome.steps_succeeded += 1;
                StepRecord {
                    index: idx,
                    title: step.title.clone(),
                    status: "success".into(),
                    detail: step.detail.clone(),
                    gates: gates.clone(),
                    candidates_tried: *candidates,
                    repair_rounds: *repairs,
                    recorded_at: Utc::now().to_rfc3339(),
                }
            }
            StepStatus::Failed { reason } => {
                self.ui.error(&format!("step failed: {reason}"));
                outcome.steps_failed += 1;
                StepRecord {
                    index: idx,
                    title: step.title.clone(),
                    status: "failed".into(),
                    detail: reason.clone(),
                    gates: String::new(),
                    candidates_tried: 0,
                    repair_rounds: 0,
                    recorded_at: Utc::now().to_rfc3339(),
                }
            }
        }
    }

    /// Process a single step. Recurses (re-atomization) up to `max_recursion`.
    fn process_step<'s>(
        &'s self,
        task: &'s str,
        step: &'s Step,
        depth: usize,
        budget: &'s mut usize,
        changed: &'s mut std::collections::BTreeSet<String>,
    ) -> Pin<Box<dyn Future<Output = StepStatus> + 's>> {
        Box::pin(async move {
            // Index the repo fresh so slices/contracts reflect the current tree.
            let index = RepoIndex::build(&self.root);
            let leaf = if self.ablation.no_slice {
                None
            } else {
                tree::plan_leaf(step, &index)
            };
            let (ctx, contract, micro) = match &leaf {
                Some(lp) => {
                    self.ui.dim(&format!(
                        "  leaf: {} ({})",
                        lp.label,
                        if lp.is_micro {
                            "micro-patch, scoped"
                        } else {
                            "file-scoped"
                        }
                    ));
                    (lp.context.clone(), lp.contract.clone(), lp.is_micro)
                }
                None => (
                    context::file_context(&self.root, &format!("{} {}", step.title, step.detail)),
                    Contract::unrestricted(),
                    false,
                ),
            };
            // When the leaf scopes to exactly one file, a model that ignores the
            // edit format and just emits code can still be used: its output is
            // treated as a full-file create for that path.
            let default_path = if contract.allowed_files.len() == 1 {
                Some(contract.allowed_files[0].clone())
            } else {
                None
            };
            let pool = match self.cfg.models.drafter_pool() {
                Ok(p) if !p.is_empty() => p,
                _ => {
                    return StepStatus::Failed {
                        reason: "no drafter model configured".into(),
                    }
                }
            };
            // The repairer role may point at a different model for diversity;
            // fall back to the first pool model if it cannot be resolved.
            let repairer = self
                .cfg
                .models
                .repairer_ref()
                .unwrap_or_else(|_| pool[0].clone());
            if pool.len() > 1 {
                self.ui.dim(&format!("  ensemble: {} models", pool.len()));
            }

            // --- 1. Best-of-N generation ---
            self.ui.phase(
                "draft",
                &format!("sampling {} candidate(s)…", self.cfg.scaling.candidates),
            );
            let candidates = sample_candidates(
                self.provider,
                &pool,
                self.cfg,
                if micro {
                    prompts::micro_patch_system()
                } else {
                    prompts::drafter_system()
                },
                if micro {
                    prompts::micro_patch_user(task, step, &ctx)
                } else {
                    prompts::drafter_user(task, step, &ctx)
                },
                self.cfg.scaling.candidates,
                default_path.clone(),
            )
            .await;

            // --- 2 & 3. Verify + select ---
            let (mut last_failure, best_attempt) = match self
                .collect(step, candidates, &contract)
                .await
            {
                Collected::Passing { mut scored, tried } => {
                    // Confirm the winner with a fresh re-verification before
                    // committing. A single sandbox pass can be a nondeterministic
                    // fluke (random/time-dependent tests); requiring a second,
                    // independent pass stops a flaky candidate from being selected
                    // — the main cause of higher-N regressions.
                    while !scored.is_empty() {
                        let best = select_best(self.provider, self.cfg, task, step, &scored).await;
                        let cand = scored.remove(best);
                        if let Prefilter::Pass(changes) =
                            self.run_prefilter(&cand.candidate.blocks, &contract)
                        {
                            // Surface path fixes the resolver applied (e.g.
                            // `path/to/x` → `x`) so corrections are visible.
                            for (from, to) in &changes.report.resolved_paths {
                                self.ui.dim(&format!("  path resolved: {from} → {to}"));
                            }
                            // The fresh confirmation verdict authorizes the
                            // apply, so its summary (not the stale collect
                            // verdict's) is what gets recorded.
                            match self.verify_changes(step, &changes).await {
                                Ok(fresh) if fresh.passed => {
                                    if let Err(e) =
                                        apply_blocks(&self.root, &cand.candidate.blocks)
                                    {
                                        return StepStatus::Failed {
                                            reason: format!("applying winner: {e}"),
                                        };
                                    }
                                    changed.extend(changes.contents.keys().cloned());
                                    return StepStatus::Success {
                                        gates: fresh.summary(),
                                        candidates: tried,
                                        repairs: 0,
                                    };
                                }
                                Ok(fresh) => {
                                    self.ui.dim(&format!(
                                        "  winner failed confirmation re-verify ({}); trying next",
                                        fresh.summary()
                                    ));
                                }
                                Err(e) => {
                                    self.ui.dim(&format!(
                                        "  winner failed confirmation re-verify ({}); trying next",
                                        truncate(&e.to_string(), 120)
                                    ));
                                }
                            }
                        }
                    }
                    (
                        "all candidates failed confirmation re-verify".to_string(),
                        None,
                    )
                }
                Collected::NonePassed {
                    failure_log,
                    best_attempt,
                } => (failure_log, best_attempt),
            };

            // --- 4a. Reflexion repair ---
            for round in 0..self.cfg.scaling.repair_rounds {
                self.ui.phase(
                    "repair",
                    &format!(
                        "reflexion round {}/{}…",
                        round + 1,
                        self.cfg.scaling.repair_rounds
                    ),
                );
                let user =
                    prompts::repair_user(task, step, &last_failure, best_attempt.as_deref(), &ctx);
                let temp = self.cfg.scaling.temperature_for(round);
                let cand = match repair_once(
                    self.provider,
                    &repairer,
                    self.cfg,
                    if micro {
                        prompts::micro_patch_system()
                    } else {
                        prompts::drafter_system()
                    },
                    user,
                    temp,
                    default_path.clone(),
                )
                .await
                {
                    Ok(Some(c)) => c,
                    _ => continue,
                };
                // Same deterministic filter applies to repairs.
                let changes = match self.run_prefilter(&cand.blocks, &contract) {
                    Prefilter::Pass(c) => c,
                    other => {
                        self.ui.candidate(0, false, &other.reason());
                        last_failure = other.reason();
                        continue;
                    }
                };
                for (from, to) in &changes.report.resolved_paths {
                    self.ui.dim(&format!("  path resolved: {from} → {to}"));
                }
                match self.verify_changes(step, &changes).await {
                    Ok(verdict) if verdict.passed => {
                        self.ui.candidate(0, true, &verdict.summary());
                        match apply_blocks(&self.root, &cand.blocks) {
                            Ok(report) => changed.extend(report.files_changed.into_keys()),
                            Err(e) => {
                                return StepStatus::Failed {
                                    reason: format!("applying repair: {e}"),
                                }
                            }
                        }
                        return StepStatus::Success {
                            gates: verdict.summary(),
                            candidates: 1,
                            repairs: round + 1,
                        };
                    }
                    Ok(verdict) => {
                        self.ui.candidate(0, false, &verdict.summary());
                        if let Some(log) = verdict.first_failure_log() {
                            last_failure = log.to_string();
                        }
                    }
                    Err(e) => last_failure = e.to_string(),
                }
            }

            // --- 4b. Recursive re-atomization ---
            if !self.ablation.no_decompose && depth < self.cfg.scaling.max_recursion && *budget > 0
            {
                self.ui
                    .phase("atomize", "step is hard; decomposing further…");
                let seed = if step.detail.trim().is_empty() {
                    &step.title
                } else {
                    &step.detail
                };
                let sub = make_plan(
                    self.provider,
                    self.cfg,
                    seed,
                    &context::repo_summary(&self.root),
                )
                .await
                .unwrap_or_default();
                if sub.steps.len() > 1 {
                    let mut all_ok = true;
                    for (i, ss) in sub.steps.iter().enumerate() {
                        if *budget == 0 {
                            all_ok = false;
                            break;
                        }
                        *budget -= 1;
                        self.ui
                            .dim(&format!("    sub-step {}: {}", i + 1, ss.title));
                        if let StepStatus::Failed { .. } = self
                            .process_step(task, ss, depth + 1, budget, changed)
                            .await
                        {
                            all_ok = false;
                            break;
                        }
                    }
                    if all_ok {
                        return StepStatus::Success {
                            gates: "via-decomposition".into(),
                            candidates: 0,
                            repairs: self.cfg.scaling.repair_rounds,
                        };
                    }
                }
            }

            StepStatus::Failed {
                reason: format!("no candidate passed: {}", truncate(&last_failure, 200)),
            }
        })
    }

    /// Run the deterministic filter funnel over every candidate, then verify the
    /// survivors. Returns passing (scored) or none-passed with the best log.
    async fn collect(
        &self,
        step: &Step,
        candidates: Vec<Candidate>,
        contract: &Contract,
    ) -> Collected {
        let total = candidates.len();
        if total == 0 {
            return Collected::NonePassed {
                failure_log: "model produced no parseable edit blocks".into(),
                best_attempt: None,
            };
        }
        // Stages 1 & 2: deterministic, no sandbox, no LLM. Done synchronously up
        // front so we know which candidates are even worth sandbox-verifying.
        let mut failure_log = String::from("all candidates failed verification");
        let mut best_attempt: Option<String> = None;
        let mut passers: Vec<(Candidate, crate::edits::Changes)> = Vec::new();
        for cand in candidates {
            match self.run_prefilter(&cand.blocks, contract) {
                Prefilter::Pass(c) => passers.push((cand, c)),
                other => {
                    self.ui.candidate(cand.index, false, &other.reason());
                    failure_log = other.reason();
                    best_attempt = Some(cand.raw.clone());
                }
            }
        }
        let syntax_ok = passers.len();
        if passers.is_empty() {
            self.ui
                .dim(&format!("  funnel: {total} generated → 0 passed filter"));
            return Collected::NonePassed {
                failure_log,
                best_attempt,
            };
        }

        // Stage 3: verify survivors CONCURRENTLY (bounded) with early-exit. This
        // is the key fix for one hanging/slow candidate serializing the rest: a
        // fast correct candidate returns immediately and the rest are cancelled
        // (kill_on_drop terminates their gate processes). General, not task-specific.
        let conc = self.cfg.scaling.concurrency.max(1).min(syntax_ok);
        let mut scored: Vec<Scored> = Vec::new();
        let mut stream =
            futures::stream::iter(passers.into_iter().map(|(cand, changes)| async move {
                let res = self.verify_changes(step, &changes).await;
                (cand, changes, res)
            }))
            .buffer_unordered(conc);

        while let Some((cand, changes, res)) = stream.next().await {
            match res {
                Ok(verdict) => {
                    let model_short = cand.model.rsplit('/').next().unwrap_or(&cand.model);
                    self.ui.candidate(
                        cand.index,
                        verdict.passed,
                        &format!(
                            "{} @t{:.2} [{}]",
                            verdict.summary(),
                            cand.temperature,
                            model_short
                        ),
                    );
                    if verdict.passed {
                        let touched = changes.report.touched_lines;
                        if self.cfg.scaling.early_exit {
                            self.ui.dim(&format!(
                                "  funnel: {total} generated → {syntax_ok} filtered → early-exit on first pass"
                            ));
                            return Collected::Passing {
                                scored: vec![Scored {
                                    candidate: cand,
                                    verdict,
                                    touched_lines: touched,
                                }],
                                tried: 1,
                            };
                        }
                        scored.push(Scored {
                            candidate: cand,
                            verdict,
                            touched_lines: touched,
                        });
                    } else {
                        if let Some(log) = verdict.first_failure_log() {
                            failure_log = log.to_string();
                        }
                        best_attempt = Some(cand.raw.clone());
                    }
                }
                Err(e) => {
                    self.ui.candidate(
                        cand.index,
                        false,
                        &format!("verify error: {}", truncate(&e.to_string(), 80)),
                    );
                    failure_log = e.to_string();
                    best_attempt = Some(cand.raw.clone());
                }
            }
        }
        self.ui.dim(&format!(
            "  funnel: {total} generated → {syntax_ok} filtered → {} verified",
            scored.len()
        ));
        let tried = scored.len();
        if scored.is_empty() {
            Collected::NonePassed {
                failure_log,
                best_attempt,
            }
        } else {
            Collected::Passing { scored, tried }
        }
    }

    /// Materialize precomputed changes into a fresh sandbox and run the gate.
    async fn verify_changes(
        &self,
        step: &Step,
        changes: &crate::edits::Changes,
    ) -> Result<Verdict> {
        let sandbox = Sandbox::create(&self.root, &step.title)?;
        for (rel, content) in &changes.contents {
            let abs = sandbox.path().join(rel);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&abs, content).map_err(|e| anyhow!("writing {rel} in sandbox: {e}"))?;
        }
        // A configured global `test` gate is authoritative; only fall back to the
        // planner's per-step check when no global test exists (weak planners can
        // emit bogus checks, which must not override a real test command).
        let acceptance = if self.cfg.verify.test.is_some() {
            None
        } else {
            step.check.as_deref()
        };
        Ok(verify(sandbox.path(), &self.cfg.verify, acceptance).await)
    }

    async fn final_review(
        &self,
        task: &str,
        changed: &std::collections::BTreeSet<String>,
    ) -> Option<String> {
        let model = self.cfg.models.judge_ref().ok()?;
        let summary = if changed.is_empty() {
            context::repo_summary(&self.root)
        } else {
            // Show the actual post-change contents of the files we touched.
            let mut s = String::new();
            for rel in changed {
                if let Ok(content) = std::fs::read_to_string(self.root.join(rel)) {
                    let body: String = content.lines().take(200).collect::<Vec<_>>().join("\n");
                    s.push_str(&format!("--- {rel} ---\n{body}\n\n"));
                }
            }
            s
        };
        let req = ChatRequest {
            model,
            messages: vec![
                Message::system(prompts::judge_system()),
                Message::user(prompts::final_critic_user(task, &summary)),
            ],
            temperature: 0.0,
            max_tokens: self.cfg.scaling.max_tokens,
        };
        let review = self.provider.complete(req).await.ok()?;
        let trimmed = review.trim();
        if trimmed.eq_ignore_ascii_case("LGTM") {
            self.ui.success("final critique: LGTM");
            None
        } else {
            self.ui.warn("final critique raised notes (see summary)");
            Some(trimmed.to_string())
        }
    }

    fn summary_md(&self, o: &RunOutcome) -> String {
        let mut s = format!(
            "# Damascus run summary\n\n- steps total: {}\n- succeeded: {}\n- failed: {}\n",
            o.steps_total, o.steps_succeeded, o.steps_failed
        );
        if let Some(r) = &o.final_review {
            s.push_str(&format!("\n## Final critique\n\n{r}\n"));
        }
        s
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}
