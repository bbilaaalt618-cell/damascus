//! Prompt templates. Kept terse and concrete because weaker models follow
//! short, example-driven instructions far better than long prose.

use crate::plan::Step;

pub const EDIT_FORMAT: &str = r#"Output ONLY search/replace edit blocks, no prose. For each file:

solution.py
<<<<<<< SEARCH
exact existing lines to find (leave EMPTY to create a new file)
=======
the replacement lines
>>>>>>> REPLACE

Rules:
- First line is the EXACT repo-relative path of the target file, copied
  verbatim from the context (e.g. `solution.py`, `src/main.rs`). NEVER invent
  paths and NEVER write placeholders like `path/to/...`, `<file>`, or
  `example.*` — such blocks are rejected.
- SEARCH text must match the current file EXACTLY (copy it verbatim).
- To create a new file, leave the SEARCH section empty and put full contents in REPLACE.
- Keep edits minimal and focused on the task. Do not reformat untouched code.
- You may emit multiple blocks across multiple files."#;

pub fn planner_system() -> String {
    "You are a senior engineer who breaks a coding task into the smallest possible \
     independently-verifiable steps. Small steps let a modest model succeed reliably."
        .to_string()
}

pub fn planner_user(task: &str, repo_summary: &str, has_test_cmd: bool) -> String {
    let check_hint = if has_test_cmd {
        "If a step needs a custom acceptance command, put it in `check`; otherwise leave it null and the project's test command is used."
    } else {
        "Put a shell `check` command (exit 0 == success) for each step when you can; null is allowed."
    };
    format!(
        r#"Task:
{task}

Repository overview:
{repo_summary}

Break this into ordered steps. Reply with ONLY a JSON array, no prose:

[
  {{"title": "short imperative", "detail": "what to change and why, concretely", "check": "shell command or null", "file": "exact repo-relative path or null", "symbol": "function_or_type_name or null"}}
]

Rules:
- Use the FEWEST steps possible. Each step must be independently verifiable.
- A single self-contained change (e.g. implementing one function) is ONE step, not many.
- Only split into multiple steps when the parts touch different files or are genuinely independent.
- Never split one function or one edit region across multiple steps — that causes conflicting edits.
- Aim for 1-5 steps; prefer 1 when the task is a single localized change.
- When a step edits a single function/type, set "file" to its path and "symbol" to
  its exact name. This lets the harness give the worker a tight, focused slice.

{check_hint}"#
    )
}

pub fn drafter_system() -> String {
    format!(
        "You are an expert programmer. You make precise, minimal code changes.\n\n{EDIT_FORMAT}"
    )
}

pub fn drafter_user(task: &str, step: &Step, file_context: &str) -> String {
    format!(
        r#"Overall task: {task}

Current step: {title}
Details: {detail}

Relevant files (current contents):
{file_context}

Produce the edit blocks that complete THIS step only."#,
        title = step.title,
        detail = step.detail,
    )
}

pub fn micro_patch_system() -> String {
    format!(
        "You are a fast, precise implementer working on ONE small, isolated unit of code.\n\
         You are given a focused slice: read-only dependency context plus a single target\n\
         definition you must edit. Change ONLY the target definition. Do not touch anything\n\
         else, do not add new files, do not change the target's signature unless explicitly\n\
         told to.\n\n{EDIT_FORMAT}"
    )
}

pub fn micro_patch_user(task: &str, step: &Step, slice: &str) -> String {
    format!(
        r#"Overall task: {task}

Your assignment: {title}
{detail}

Focused code slice (edit ONLY the target definition):
{slice}

Output the search/replace edit block(s) for the target definition only."#,
        title = step.title,
        detail = step.detail,
    )
}

pub fn repair_user(
    task: &str,
    step: &Step,
    failure_log: &str,
    prior_attempt: Option<&str>,
    file_context: &str,
) -> String {
    // Sequential refinement: show the model its own closest failing attempt so it
    // fixes that rather than starting from scratch each round.
    let prior = match prior_attempt {
        Some(a) if !a.trim().is_empty() => format!(
            "\nYour previous attempt (fix THIS rather than rewriting from scratch):\n-----\n{}\n-----\n",
            a.trim()
        ),
        _ => String::new(),
    };
    format!(
        r#"Overall task: {task}

Step: {title}
Details: {detail}

Your previous attempt did NOT pass verification. Failure output:
-----
{failure_log}
-----
{prior}
Relevant files (current contents):
{file_context}

Diagnose the specific cause from the failure output, then output corrected edit blocks only."#,
        title = step.title,
        detail = step.detail,
    )
}

pub fn judge_system() -> String {
    "You are a meticulous code reviewer choosing the best of several candidate \
     patches that all already pass automated checks. Prefer correctness, minimality, \
     and clarity."
        .to_string()
}

pub fn judge_user(task: &str, step: &Step, candidates: &[String]) -> String {
    let mut s = format!(
        "Task: {task}\nStep: {}\n\nAll candidates below PASSED the automated gates. \
         Pick the single best one.\n\n",
        step.title
    );
    for (i, c) in candidates.iter().enumerate() {
        s.push_str(&format!("=== Candidate {i} ===\n{c}\n\n"));
    }
    s.push_str("Reply with ONLY the integer index of the best candidate (e.g. `0`).");
    s
}

pub fn final_critic_user(task: &str, diff_summary: &str) -> String {
    format!(
        r#"Task: {task}

The following changes were made and pass all automated gates:
{diff_summary}

Are there any correctness bugs, missed requirements, or obvious problems that the
automated checks would NOT catch? Reply with a short bullet list, or exactly
`LGTM` if the work fully satisfies the task."#
    )
}
