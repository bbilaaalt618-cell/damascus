//! Damascus library: a verify-gated, test-time-scaling coding harness that makes
//! modest / local LLMs produce frontier-quality, verified changes.
//!
//! The binary in `main.rs` is a thin CLI over these modules. Exposing them as a
//! library lets the whole Fold Loop be integration-tested with an in-process
//! mock provider (see `tests/`).

pub mod ast;
pub mod config;
pub mod context;
pub mod edits;
pub mod filter;
pub mod generate;
pub mod ledger;
pub mod orchestrator;
pub mod plan;
pub mod prompts;
pub mod provider;
pub mod qa;
pub mod sandbox;
pub mod select;
pub mod slice;
pub mod tree;
pub mod ui;
pub mod verify;

use std::path::Path;

/// Repo-relative path with forward slashes on every OS.
///
/// `Path::to_string_lossy` yields backslashes on Windows, which then leak
/// into prompts, contracts, and test expectations. Canonicalizing once —
/// everywhere a repo-relative path is stringified — keeps prompts, scope
/// checks, and path resolution (`edits::resolve_model_path`) consistent.
pub fn rel_forward(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
