//! End-to-end test of the Fold Loop with an in-process mock provider — no
//! network, no real models. Proves the orchestration: plan -> best-of-N ->
//! verify gate -> select -> apply, plus the reflexion repair path.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use damascus::config::Config;
use damascus::orchestrator::Orchestrator;
use damascus::provider::{ChatFuture, ChatProvider, ChatRequest};
use damascus::ui::Ui;

/// A scripted provider: classifies each request by content and returns canned
/// output. The "drafter" responses include search/replace edit blocks.
struct MockProvider {
    draft_calls: AtomicUsize,
    /// Replies for failing-then-passing scenarios keyed by call order.
    repair_succeeds_on: usize,
    log: Mutex<Vec<String>>,
}

impl MockProvider {
    fn new(repair_succeeds_on: usize) -> Self {
        MockProvider {
            draft_calls: AtomicUsize::new(0),
            repair_succeeds_on,
            log: Mutex::new(Vec::new()),
        }
    }
}

fn last_user(req: &ChatRequest) -> String {
    req.messages
        .last()
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

impl ChatProvider for MockProvider {
    fn complete<'a>(&'a self, req: ChatRequest) -> ChatFuture<'a> {
        let user = last_user(&req);
        let system = req
            .messages
            .first()
            .map(|m| m.content.clone())
            .unwrap_or_default();
        self.log.lock().unwrap().push(user.clone());

        // Planner: asked for a JSON array of steps.
        if user.contains("Reply with ONLY a JSON array") {
            // Acceptance check must run on every OS (`cmd /C` on Windows,
            // `sh -c` elsewhere — see `verify::shell_command`).
            let check = if cfg!(windows) {
                "findstr hello greet.txt"
            } else {
                "grep -q 'hello world' greet.txt"
            };
            let json = format!(
                r#"[{{"title":"add greeting function","detail":"create greet.txt","check":"{check}"}}]"#
            );
            return Box::pin(async move { Ok(json) });
        }

        // Judge tie-break: "Reply with ONLY the integer index".
        if user.contains("integer index") {
            return Box::pin(async move { Ok("0".to_string()) });
        }

        // Final critic.
        if user.contains("Reply with a short bullet list") {
            return Box::pin(async move { Ok("LGTM".to_string()) });
        }

        // Drafter / repairer: produce edit blocks. The first attempt creates a
        // file with the WRONG content (so the acceptance check `grep hello`
        // fails in tests that want a repair); the repair fixes it.
        if system.contains("search/replace") || user.contains("edit blocks") {
            let is_repair = user.contains("did NOT pass verification");
            let n = self.draft_calls.fetch_add(1, Ordering::SeqCst);
            let content = if is_repair || n >= self.repair_succeeds_on {
                "hello world"
            } else {
                "wrong content"
            };
            let block = format!("greet.txt\n<<<<<<< SEARCH\n=======\n{content}\n>>>>>>> REPLACE\n");
            return Box::pin(async move { Ok(block) });
        }

        Box::pin(async move { Ok(String::new()) })
    }
}

fn test_config(candidates: usize, repair_rounds: usize) -> Config {
    // No-op build gate on every OS.
    let build = if cfg!(windows) { "exit 0" } else { "true" };
    let toml = format!(
        r#"
[providers.mock]
base_url = "http://localhost/v1"

[models]
planner = "mock/m"
drafter = "mock/m"
judge = "mock/m"
repairer = "mock/m"

[scaling]
candidates = {candidates}
repair_rounds = {repair_rounds}
max_recursion = 1
max_steps = 10
temperature = 0.3
temperature_step = 0.2

[verify]
build = "{build}"
timeout_secs = 30
"#
    );
    toml::from_str(&toml).unwrap()
}

#[tokio::test]
async fn fold_loop_creates_verified_file() {
    let dir = tempfile::tempdir().unwrap();
    // First candidate already passes (repair_succeeds_on = 0 => always "hello world").
    let provider = MockProvider::new(0);
    let cfg = test_config(3, 2);
    let ui = Ui::new(false, true);
    let orch = Orchestrator::new(&provider, &cfg, dir.path().to_path_buf(), ui);

    let outcome = orch
        .run("create a greet.txt containing hello world")
        .await
        .unwrap();

    assert!(
        outcome.all_passed(),
        "expected all steps to pass: {outcome:?}"
    );
    let content = std::fs::read_to_string(dir.path().join("greet.txt")).unwrap();
    assert!(content.contains("hello world"));
    // Ledger written.
    assert!(dir.path().join(".damascus/latest").exists());
}

#[tokio::test]
async fn reflexion_repairs_a_failing_step() {
    let dir = tempfile::tempdir().unwrap();
    // No initial candidate passes the per-step acceptance check; the repair path
    // (which always emits "hello world") must rescue the step.
    let provider = MockProvider::new(usize::MAX);
    let cfg = test_config(2, 2);
    let ui = Ui::new(false, true);
    let orch = Orchestrator::new(&provider, &cfg, dir.path().to_path_buf(), ui);

    let outcome = orch.run("write hello world into greet.txt").await.unwrap();

    // Every initial candidate writes "wrong content" and fails the per-step
    // acceptance check (`grep -q 'hello world'`). The reflexion repair pass
    // emits the correct content and must rescue the step.
    assert_eq!(outcome.steps_total, 1);
    assert!(
        outcome.all_passed(),
        "repair should rescue the step: {outcome:?}"
    );
    let content = std::fs::read_to_string(dir.path().join("greet.txt")).unwrap();
    assert!(content.contains("hello world"));
}
