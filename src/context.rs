//! Builds the small, focused context a weak model needs to stay on task: a repo
//! file listing plus the contents of files the step actually references.

use std::path::Path;

use walkdir::WalkDir;

const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".damascus",
    ".venv",
    "venv",
    "dist",
    "build",
    ".next",
    "__pycache__",
    ".cargo",
];
const MAX_LISTED: usize = 300;
const MAX_FILE_LINES: usize = 400;

/// A compact listing of source files in the repo (relative paths).
pub fn repo_summary(root: &Path) -> String {
    let mut files = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_skipped(e.path()))
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            // Forward slashes on every OS (see `crate::rel_forward`).
            files.push(crate::rel_forward(root, entry.path()));
        }
        if files.len() >= MAX_LISTED {
            files.push("… (truncated)".to_string());
            break;
        }
    }
    files.sort();
    if files.is_empty() {
        "(empty repository)".to_string()
    } else {
        files.join("\n")
    }
}

/// Gather the contents of files the step text references, plus the listing as a
/// fallback. Each file is truncated to keep the prompt small.
pub fn file_context(root: &Path, step_text: &str) -> String {
    let listing = repo_summary(root);
    let mut sections = Vec::new();
    for path in referenced_paths(root, step_text) {
        let abs = root.join(&path);
        if let Ok(content) = std::fs::read_to_string(&abs) {
            sections.push(format!(
                "--- {path} ---\n{}",
                truncate_lines(&content, MAX_FILE_LINES)
            ));
        }
    }
    if sections.is_empty() {
        format!("Project files:\n{listing}")
    } else {
        format!("Project files:\n{listing}\n\n{}", sections.join("\n\n"))
    }
}

/// Extract path-like tokens from text that correspond to existing files.
fn referenced_paths(root: &Path, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for raw in text.split(|c: char| c.is_whitespace() || "()[]{}<>,;:\"'`".contains(c)) {
        let tok = raw.trim_matches(|c: char| c == '.' || c == ',');
        if tok.len() < 3 || !tok.contains('.') {
            continue;
        }
        if (tok.contains('/') || looks_like_filename(tok))
            && root.join(tok).is_file()
            && !found.contains(&tok.to_string())
        {
            found.push(tok.to_string());
        }
    }
    found.truncate(8);
    found
}

fn looks_like_filename(tok: &str) -> bool {
    matches!(
        tok.rsplit('.').next(),
        Some(
            "rs" | "py"
                | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "go"
                | "java"
                | "c"
                | "h"
                | "cpp"
                | "hpp"
                | "rb"
                | "toml"
                | "json"
                | "yaml"
                | "yml"
                | "md"
                | "txt"
                | "sh"
                | "html"
                | "css"
        )
    )
}

fn truncate_lines(s: &str, max: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max {
        s.to_string()
    } else {
        format!(
            "{}\n… ({} more lines)",
            lines[..max].join("\n"),
            lines.len() - max
        )
    }
}

fn is_skipped(path: &Path) -> bool {
    path.is_dir()
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| SKIP_DIRS.contains(&n))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn summary_lists_files_and_skips_heavy() {
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src/main.rs"), "x").unwrap();
        std::fs::create_dir_all(d.path().join("target")).unwrap();
        std::fs::write(d.path().join("target/junk"), "y").unwrap();
        let s = repo_summary(d.path());
        assert!(s.contains("src/main.rs"));
        assert!(!s.contains("target/junk"));
    }

    #[test]
    fn file_context_includes_referenced_file() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("lib.rs"), "fn a() {}").unwrap();
        let ctx = file_context(d.path(), "edit lib.rs to add b");
        assert!(ctx.contains("--- lib.rs ---"));
        assert!(ctx.contains("fn a() {}"));
    }

    #[test]
    fn ignores_nonexistent_paths() {
        let d = tempdir().unwrap();
        let ctx = file_context(d.path(), "edit ghost.rs");
        assert!(!ctx.contains("--- ghost.rs ---"));
    }
}
