//! The objective verification gate — the heart of Damascus's "force quality"
//! claim. A candidate is accepted only if the configured commands succeed in its
//! sandbox. The model never gets to certify its own work.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::config::VerifyConfig;

/// The result of one shell gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResult {
    pub name: String,
    pub command: String,
    pub ran: bool,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Tail of combined stdout+stderr.
    pub output_tail: String,
}

/// Aggregate verdict over all gates plus a step-specific acceptance check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub passed: bool,
    pub gates: Vec<GateResult>,
    /// Heuristic count of error/warning-like lines across all gate output.
    pub diagnostics: usize,
}

impl Verdict {
    /// Compact human summary, e.g. `build:ok test:FAIL lint:ok`.
    /// Never empty: `nogates` signals that nothing actually ran, which is
    /// always worth surfacing instead of a silent blank.
    pub fn summary(&self) -> String {
        let s: Vec<String> = self
            .gates
            .iter()
            .filter(|g| g.ran)
            .map(|g| format!("{}:{}", g.name, if g.passed { "ok" } else { "FAIL" }))
            .collect();
        if s.is_empty() {
            "nogates".to_string()
        } else {
            s.join(" ")
        }
    }

    /// The output of the first failing gate, for repair feedback.
    pub fn first_failure_log(&self) -> Option<&str> {
        self.gates
            .iter()
            .find(|g| g.ran && !g.passed)
            .map(|g| g.output_tail.as_str())
    }
}

/// Run all gates against `dir`. `acceptance` overrides the `test` gate for a
/// specific step (the planner-provided per-step check).
pub async fn verify(dir: &Path, cfg: &VerifyConfig, acceptance: Option<&str>) -> Verdict {
    let mut gates = Vec::new();
    let dur = Duration::from_secs(cfg.timeout_secs);

    if let Some(cmd) = &cfg.build {
        gates.push(run_gate("build", cmd, dir, dur).await);
    }
    let test_cmd = acceptance.or(cfg.test.as_deref());
    if let Some(cmd) = test_cmd {
        let name = if acceptance.is_some() {
            "check"
        } else {
            "test"
        };
        gates.push(run_gate(name, cmd, dir, dur).await);
    }
    if let Some(cmd) = &cfg.lint {
        gates.push(run_gate("lint", cmd, dir, dur).await);
    }

    let passed = !gates.is_empty() && gates.iter().all(|g| !g.ran || g.passed);
    let diagnostics = gates
        .iter()
        .map(|g| count_diagnostics(&g.output_tail))
        .sum();
    Verdict {
        passed,
        gates,
        diagnostics,
    }
}

async fn run_gate(name: &str, command: &str, dir: &Path, dur: Duration) -> GateResult {
    let mut cmd = shell_command(command);
    cmd.current_dir(dir)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return GateResult {
                name: name.into(),
                command: command.into(),
                ran: false,
                passed: false,
                exit_code: None,
                timed_out: false,
                output_tail: format!("failed to spawn: {e}"),
            }
        }
    };

    let mut child = child;
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();

    let wait = async {
        let mut buf = String::new();
        if let Some(o) = out.as_mut() {
            let _ = o.read_to_string(&mut buf).await;
        }
        if let Some(e) = err.as_mut() {
            let mut ebuf = String::new();
            let _ = e.read_to_string(&mut ebuf).await;
            buf.push_str(&ebuf);
        }
        let status = child.wait().await;
        (status, buf)
    };

    match timeout(dur, wait).await {
        Ok((Ok(status), buf)) => GateResult {
            name: name.into(),
            command: command.into(),
            ran: true,
            passed: status.success(),
            exit_code: status.code(),
            timed_out: false,
            output_tail: tail(&buf, 4000),
        },
        Ok((Err(e), buf)) => GateResult {
            name: name.into(),
            command: command.into(),
            ran: true,
            passed: false,
            exit_code: None,
            timed_out: false,
            output_tail: format!("{}\n[wait error: {e}]", tail(&buf, 3500)),
        },
        Err(_) => {
            let _ = child.start_kill();
            GateResult {
                name: name.into(),
                command: command.into(),
                ran: true,
                passed: false,
                exit_code: None,
                timed_out: true,
                output_tail: format!("[timed out after {}s]", dur.as_secs()),
            }
        }
    }
}

/// Platform shell for gate commands: `cmd /C` on Windows, `sh -c` elsewhere.
/// Previously hard-coded to `sh`, which silently broke every gate on stock
/// Windows machines (no `sh` without Git/MSYS installed).
#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(command);
    cmd
}

/// Platform shell for gate commands: `cmd /C` on Windows, `sh -c` elsewhere.
#[cfg(not(windows))]
fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let start = s.len() - max;
    // align to a char boundary
    let mut start = start;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…\n{}", &s[start..])
}

fn count_diagnostics(s: &str) -> usize {
    s.lines()
        .filter(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("error") || l.contains("warning") || l.contains("failed")
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Cross-platform shell snippets (`cmd /C ...` on Windows, `sh -c ...`
    /// elsewhere — see `shell_command`), so the suite passes on every OS.
    #[cfg(windows)]
    fn ok_cmd() -> &'static str {
        "exit 0"
    }
    #[cfg(not(windows))]
    fn ok_cmd() -> &'static str {
        "true"
    }
    #[cfg(windows)]
    fn fail_cmd() -> &'static str {
        "echo boom 1>&2 & exit 1"
    }
    #[cfg(not(windows))]
    fn fail_cmd() -> &'static str {
        "echo boom >&2; false"
    }
    #[cfg(windows)]
    fn sleep_cmd() -> &'static str {
        // NOTE: `timeout.exe` refuses redirected stdin, so `ping` is used.
        "ping -n 6 127.0.0.1 >NUL"
    }
    #[cfg(not(windows))]
    fn sleep_cmd() -> &'static str {
        "sleep 5"
    }

    #[tokio::test]
    async fn passing_gate() {
        let dir = tempdir().unwrap();
        let cfg = VerifyConfig {
            build: Some(ok_cmd().into()),
            test: Some(ok_cmd().into()),
            lint: None,
            timeout_secs: 10,
        };
        let v = verify(dir.path(), &cfg, None).await;
        assert!(v.passed);
        assert_eq!(v.summary(), "build:ok test:ok");
    }

    #[tokio::test]
    async fn failing_gate_blocks() {
        let dir = tempdir().unwrap();
        let cfg = VerifyConfig {
            build: Some(ok_cmd().into()),
            test: Some(fail_cmd().into()),
            lint: None,
            timeout_secs: 10,
        };
        let v = verify(dir.path(), &cfg, None).await;
        assert!(!v.passed);
        assert!(v.first_failure_log().unwrap().contains("boom"));
    }

    #[tokio::test]
    async fn acceptance_overrides_test() {
        let dir = tempdir().unwrap();
        let cfg = VerifyConfig {
            build: None,
            test: Some(fail_cmd().into()),
            lint: None,
            timeout_secs: 10,
        };
        let v = verify(dir.path(), &cfg, Some(ok_cmd())).await;
        assert!(v.passed);
        assert_eq!(v.summary(), "check:ok");
    }

    #[tokio::test]
    async fn timeout_is_recorded() {
        let dir = tempdir().unwrap();
        let cfg = VerifyConfig {
            build: Some(sleep_cmd().into()),
            test: None,
            lint: None,
            timeout_secs: 1,
        };
        let v = verify(dir.path(), &cfg, None).await;
        assert!(!v.passed);
        assert!(v.gates[0].timed_out);
    }
}
