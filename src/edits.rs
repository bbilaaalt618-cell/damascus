//! Deterministic edit application.
//!
//! Weak models produce unreliable unified diffs, so Damascus uses aider-style
//! search/replace blocks instead. Parsing and application are 100% deterministic
//! Rust — the probabilistic part (the model) only proposes; this module decides
//! whether the proposal is even applicable before any verifier runs.
//!
//! Block grammar (the path is the line immediately above the SEARCH marker,
//! ignoring a code-fence line):
//!
//! ```text
//! src/lib.rs
//! <<<<<<< SEARCH
//! old code (empty => create a new file)
//! =======
//! new code
//! >>>>>>> REPLACE
//! ```

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Result};

const SEARCH: &str = "<<<<<<< SEARCH";
const DIVIDER: &str = "=======";
const REPLACE: &str = ">>>>>>> REPLACE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditBlock {
    pub path: String,
    pub search: String,
    pub replace: String,
}

/// Parse zero or more edit blocks from arbitrary model output. Surrounding prose
/// and ``` fences are tolerated.
pub fn parse_blocks(text: &str) -> Result<Vec<EditBlock>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim_end() == SEARCH {
            let path = find_path_above(&lines, i).ok_or_else(|| {
                anyhow!("SEARCH block at line {} has no file path above it", i + 1)
            })?;
            // collect search body until divider
            let mut j = i + 1;
            let mut search = String::new();
            while j < lines.len() && lines[j].trim_end() != DIVIDER {
                search.push_str(lines[j]);
                search.push('\n');
                j += 1;
            }
            if j >= lines.len() {
                bail!("unterminated SEARCH block (missing `{DIVIDER}`)");
            }
            j += 1; // skip divider
            let mut replace = String::new();
            while j < lines.len() && lines[j].trim_end() != REPLACE {
                replace.push_str(lines[j]);
                replace.push('\n');
                j += 1;
            }
            if j >= lines.len() {
                bail!("unterminated block (missing `{REPLACE}`)");
            }
            blocks.push(EditBlock {
                path,
                search: strip_trailing_newline(&search),
                replace: strip_trailing_newline(&replace),
            });
            i = j + 1;
        } else {
            i += 1;
        }
    }
    Ok(blocks)
}

/// Like [`parse_blocks`], but if no search/replace blocks are present and a
/// `default_path` (single target file) is known, fall back to treating the
/// dominant fenced code block (or the whole response) as a full-file CREATE.
/// Strong code models often ignore the edit format and just emit the code.
pub fn parse_blocks_fallback(text: &str, default_path: Option<&str>) -> Vec<EditBlock> {
    if let Ok(blocks) = parse_blocks(text) {
        if !blocks.is_empty() {
            return blocks;
        }
    }
    if let Some(path) = default_path {
        if let Some(code) = extract_dominant_code(text) {
            return vec![EditBlock {
                path: path.to_string(),
                search: String::new(),
                replace: code,
            }];
        }
    }
    Vec::new()
}

/// Extract the largest fenced code block; if none, return the whole trimmed text
/// when it looks like code rather than prose.
fn extract_dominant_code(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut best: Option<String> = None;
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("```") {
            let mut j = i + 1;
            let mut body = String::new();
            while j < lines.len() && !lines[j].trim_start().starts_with("```") {
                body.push_str(lines[j]);
                body.push('\n');
                j += 1;
            }
            if best.as_ref().map(|b| body.len() > b.len()).unwrap_or(true) {
                best = Some(body);
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    if let Some(b) = best {
        let t = strip_trailing_newline(&b);
        if !t.trim().is_empty() {
            return Some(t);
        }
    }
    // No fence: accept the whole text only if it has no obvious prose and contains code-ish lines.
    let t = text.trim();
    if !t.is_empty()
        && (t.contains("def ")
            || t.contains("import ")
            || t.contains("class ")
            || t.contains("fn ")
            || t.contains("function ")
            || t.contains("#include"))
    {
        return Some(t.to_string());
    }
    None
}

fn strip_trailing_newline(s: &str) -> String {
    s.strip_suffix('\n').unwrap_or(s).to_string()
}

/// The file path is the nearest non-empty line above the SEARCH marker, skipping
/// an opening code fence.
fn find_path_above(lines: &[&str], search_idx: usize) -> Option<String> {
    let mut k = search_idx;
    while k > 0 {
        k -= 1;
        let t = lines[k].trim();
        if t.is_empty() || t.starts_with("```") {
            continue;
        }
        // Strip common decorations like backticks or trailing colon.
        let cleaned = t.trim_matches('`').trim_end_matches(':').trim();
        if cleaned.is_empty() {
            continue;
        }
        return Some(cleaned.to_string());
    }
    None
}

/// Reject paths that escape the project root (`..`, absolute paths).
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        bail!("refusing absolute path `{rel}`");
    }
    for c in rel_path.components() {
        if matches!(c, Component::ParentDir) {
            bail!("refusing path with `..`: `{rel}`");
        }
    }
    Ok(root.join(rel_path))
}

/// Directory names never indexed for path resolution (mirrors sandbox skips).
const RESOLVE_SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".damascus",
    ".venv",
    "venv",
    "dist",
    "build",
    ".next",
    ".cargo",
    "__pycache__",
];

/// Filenames that are virtually always model-invented placeholders, never real
/// repo files a step would target.
const PLACEHOLDER_FILENAMES: &[&str] = &[
    "file.ext",
    "file.txt",
    "filename.ext",
    "example.py",
    "example.rs",
    "example.js",
    "example.ts",
    "foo.py",
    "foo.rs",
    "foo.js",
    "bar.py",
    "bar.rs",
    "your_file.py",
    "yourfile.py",
    "myfile.py",
];

/// True when `p` (already normalized) looks like an invented example path
/// rather than a real repo file. Weak models copy prompt examples verbatim
/// (`path/to/...`), so this is checked before anything touches disk.
fn looks_like_placeholder(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    if l.contains("path/to")
        || l.contains("path\\to")
        || l.contains('<')
        || l.contains('>')
        || l.contains("...")
        || l.contains('*')
        || l.contains('?')
    {
        return true;
    }
    // A leading foo/bar/baz segment is example-speak, not a repo layout.
    if let Some(first) = l.split('/').next() {
        if matches!(first, "foo" | "bar" | "baz") {
            return true;
        }
    }
    if let Some(name) = l.rsplit('/').next() {
        if PLACEHOLDER_FILENAMES.contains(&name) {
            return true;
        }
    }
    false
}

/// Normalize a model-emitted path: trim decorations, unify separators,
/// drop `./` prefixes and duplicate slashes. Pure string surgery, no I/O.
fn normalize_model_path(raw: &str) -> String {
    let mut p = raw.trim().to_string();
    p = p
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string();
    p = p.trim_end_matches(':').trim().to_string();
    p = p.replace('\\', "/");
    while p.starts_with("./") {
        p = p[2..].to_string();
    }
    while p.contains("//") {
        p = p.replace("//", "/");
    }
    p
}

/// List repo files (relative, `/`-separated), skipping heavy/derived dirs.
/// Bounded so pathological trees can't stall the loop.
fn repo_files(root: &Path) -> Vec<String> {
    const CAP: usize = 5000;
    let mut out = Vec::new();
    let mut walker = walkdir::WalkDir::new(root).into_iter();
    // walkdir is a hard dependency (see sandbox.rs); filter manually here.
    while let Some(entry) = walker.next() {
        if out.len() >= CAP {
            break;
        }
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if path != root && RESOLVE_SKIP_DIRS.contains(&name) {
                    walker.skip_current_dir();
                }
            }
            continue;
        }
        if entry.file_type().is_file() {
            // Forward slashes on every OS (see `crate::rel_forward`).
            out.push(crate::rel_forward(root, path));
        }
    }
    out
}

/// Resolve a model-emitted path to the canonical repo-relative path.
///
/// - Normalizes separators/decorations (`src\\main.rs`, `./x`, backticks).
/// - Rejects placeholder paths (`path/to/...`, `<...>`, globs) with an
///   actionable error that feeds the repair loop.
/// - For modifications (non-empty SEARCH) the target must exist: exact hit
///   wins, otherwise a unique suffix match (`path/to/solution.py` →
///   `solution.py`) is adopted; ambiguous/missing targets error with hints.
/// - For creations (empty SEARCH) the normalized path is kept as-is so new
///   files still work.
pub fn resolve_model_path(root: &Path, raw: &str, is_create: bool) -> Result<String> {
    let norm = normalize_model_path(raw);
    if norm.is_empty() {
        bail!("empty file path in edit block; give the exact repo-relative path");
    }
    if norm.ends_with('/') {
        bail!(
            "`{raw}` is a directory, not a file; give the exact repo-relative file path"
        );
    }
    if looks_like_placeholder(&norm) {
        bail!(
            "path `{raw}` looks like a placeholder, not a real repo file. \
             Use the EXACT repo-relative path shown in your context (e.g. `solution.py`), \
             never `path/to/...`, `<...>`, or example filenames"
        );
    }
    let rel = Path::new(&norm);
    if rel.is_absolute() {
        bail!("refusing absolute path `{raw}`; use a repo-relative path");
    }
    for c in rel.components() {
        if matches!(c, Component::ParentDir) {
            bail!("refusing path with `..`: `{raw}`; stay inside the repo");
        }
    }
    let abs = root.join(rel);
    if abs.is_dir() {
        bail!("`{raw}` is a directory; give the exact repo-relative file path");
    }
    if abs.is_file() {
        return Ok(norm);
    }
    if is_create {
        // Genuinely new file: keep the normalized path so creation works.
        return Ok(norm);
    }
    // Modification of a path that doesn't exist: try suffix resolution.
    let files = repo_files(root);
    let suffix = format!("/{norm}");
    let mut hits: Vec<&String> = files
        .iter()
        .filter(|f| *f == &norm || f.ends_with(suffix.as_str()))
        .collect();
    // Bare filename? Match by file name as a last resort.
    if hits.is_empty() && !norm.contains('/') {
        hits = files
            .iter()
            .filter(|f| {
                Path::new(f)
                    .file_name()
                    .map(|n| n.to_string_lossy() == norm)
                    .unwrap_or(false)
            })
            .collect();
    }
    match hits.len() {
        1 => Ok(hits[0].clone()),
        0 => {
            let hint = suggest_similar(&files, &norm);
            bail!(
                "file `{raw}` does not exist in the repo; use an EXACT repo-relative path.{hint}"
            )
        }
        _ => {
            let list: Vec<&str> = hits.iter().take(5).map(|s| s.as_str()).collect();
            bail!(
                "path `{raw}` is ambiguous; multiple repo files match. \
                 Use the exact one: {}",
                list.join(", ")
            )
        }
    }
}

/// Small hint for missing-file errors: same file name elsewhere, else a few
/// top-level files so the model sees real paths.
fn suggest_similar(files: &[String], norm: &str) -> String {
    let want_name = Path::new(norm)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    let same_name: Vec<&str> = files
        .iter()
        .filter(|f| {
            want_name.as_ref().is_some_and(|w| {
                Path::new(f)
                    .file_name()
                    .map(|n| n.to_string_lossy() == *w)
                    .unwrap_or(false)
            })
        })
        .take(5)
        .map(|s| s.as_str())
        .collect();
    if !same_name.is_empty() {
        return format!(" Did you mean one of: {}?", same_name.join(", "));
    }
    let top: Vec<&str> = files
        .iter()
        .filter(|f| !f.contains('/'))
        .take(8)
        .map(|s| s.as_str())
        .collect();
    if !top.is_empty() {
        return format!(" Repo top-level files include: {}.", top.join(", "));
    }
    String::new()
}

/// Outcome of applying an edit set, used by selection to prefer smaller diffs.
#[derive(Debug, Default, Clone)]
pub struct ApplyReport {
    pub files_changed: BTreeMap<String, ChangeKind>,
    /// Total lines emitted in replace bodies (a cheap diff-size proxy).
    pub touched_lines: usize,
    /// Model-emitted path -> canonical repo path, when resolution rewrote one.
    /// Surfaced in the UI so path fixes are visible, not silent.
    pub resolved_paths: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Created,
    Modified,
}

/// The in-memory result of applying an edit set: final content per file plus a
/// change report. Nothing is written to disk.
#[derive(Debug, Default, Clone)]
pub struct Changes {
    /// Relative path -> final file content (with a trailing newline).
    pub contents: BTreeMap<String, String>,
    pub report: ApplyReport,
}

/// Compute the result of applying every block, reading current files from `root`
/// but writing nothing. Multiple blocks targeting the same file are applied
/// sequentially. Returns an error if any block cannot be applied unambiguously —
/// that failure is *signal* for the repair loop, not noise.
pub fn compute_changes(root: &Path, blocks: &[EditBlock]) -> Result<Changes> {
    if blocks.is_empty() {
        bail!("no edit blocks found in model output");
    }
    // Resolve every model-emitted path to its canonical repo-relative form
    // FIRST, so scope checks, sandbox writes, ledger records, and the final
    // apply all see the same real path (never `path/to/...` junk).
    let mut resolved: Vec<EditBlock> = Vec::with_capacity(blocks.len());
    let mut remapped: Vec<(String, String)> = Vec::new();
    for b in blocks {
        let canon = resolve_model_path(root, &b.path, b.search.trim().is_empty())?;
        if canon != b.path {
            remapped.push((b.path.clone(), canon.clone()));
        }
        resolved.push(EditBlock {
            path: canon,
            search: b.search.clone(),
            replace: b.replace.clone(),
        });
    }

    let mut contents: BTreeMap<String, String> = BTreeMap::new();
    let mut existed: BTreeMap<String, bool> = BTreeMap::new();
    let mut report = ApplyReport {
        resolved_paths: remapped,
        ..Default::default()
    };

    for b in &resolved {
        let abs = safe_join(root, &b.path)?;
        let current = if let Some(c) = contents.get(&b.path) {
            c.clone()
        } else {
            let was = abs.exists();
            existed.insert(b.path.clone(), was);
            if was {
                std::fs::read_to_string(&abs).map_err(|e| anyhow!("reading {}: {e}", b.path))?
            } else {
                String::new()
            }
        };

        let new_content = if b.search.trim().is_empty() {
            b.replace.clone()
        } else {
            replace_once(&current, &b.search, &b.replace)
                .ok_or_else(|| anyhow!("SEARCH text not found (or ambiguous) in `{}`", b.path))?
        };

        contents.insert(b.path.clone(), ensure_final_newline(&new_content));
        report.touched_lines += b.replace.lines().count().max(1);
        let kind = if *existed.get(&b.path).unwrap_or(&true) {
            ChangeKind::Modified
        } else {
            ChangeKind::Created
        };
        report.files_changed.insert(b.path.clone(), kind);
    }
    Ok(Changes { contents, report })
}

/// Apply every block to the tree rooted at `root`, writing the results to disk.
pub fn apply_blocks(root: &Path, blocks: &[EditBlock]) -> Result<ApplyReport> {
    let changes = compute_changes(root, blocks)?;
    for (rel, content) in &changes.contents {
        let abs = safe_join(root, rel)?;
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&abs, content).map_err(|e| anyhow!("writing {rel}: {e}"))?;
    }
    Ok(changes.report)
}

fn ensure_final_newline(s: &str) -> String {
    if s.is_empty() || s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

/// Replace the first occurrence of `search` in `haystack`. Tries an exact match
/// first, then a whitespace-tolerant match (trailing whitespace per line and a
/// uniform indent shift), which absorbs the small formatting drift typical of
/// weaker models.
fn replace_once(haystack: &str, search: &str, replace: &str) -> Option<String> {
    if let Some(pos) = haystack.find(search) {
        let mut out = String::with_capacity(haystack.len() - search.len() + replace.len());
        out.push_str(&haystack[..pos]);
        out.push_str(replace);
        out.push_str(&haystack[pos + search.len()..]);
        return Some(out);
    }
    flexible_replace(haystack, search, replace)
}

fn needle_too_long(hay: &[&str], needle: &[&str]) -> bool {
    needle.len() > hay.len()
}

fn flexible_replace(haystack: &str, search: &str, replace: &str) -> Option<String> {
    let hay_lines: Vec<&str> = haystack.lines().collect();
    let search_lines: Vec<&str> = search.lines().collect();
    if search_lines.is_empty() || needle_too_long(&hay_lines, &search_lines) {
        return None;
    }
    let norm = |s: &str| s.trim_end().to_string();
    let needle: Vec<String> = search_lines.iter().map(|l| norm(l)).collect();

    let mut start = None;
    'outer: for i in 0..=hay_lines.len().saturating_sub(needle.len()) {
        for (k, want) in needle.iter().enumerate() {
            if norm(hay_lines[i + k]) != *want {
                continue 'outer;
            }
        }
        start = Some(i);
        break;
    }
    let start = start?;
    let end = start + needle.len();

    let mut out_lines: Vec<String> = Vec::with_capacity(hay_lines.len());
    out_lines.extend(hay_lines[..start].iter().map(|s| s.to_string()));
    out_lines.extend(replace.lines().map(|s| s.to_string()));
    out_lines.extend(hay_lines[end..].iter().map(|s| s.to_string()));
    Some(out_lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_single_block() {
        let text = "\
Here is the change:
src/lib.rs
<<<<<<< SEARCH
fn old() {}
=======
fn new() {}
>>>>>>> REPLACE
done.";
        let blocks = parse_blocks(text).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].path, "src/lib.rs");
        assert_eq!(blocks[0].search, "fn old() {}");
        assert_eq!(blocks[0].replace, "fn new() {}");
    }

    #[test]
    fn parses_block_inside_fence() {
        let text = "```rust\nsrc/a.rs\n<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE\n```";
        let blocks = parse_blocks(text).unwrap();
        assert_eq!(blocks[0].path, "src/a.rs");
    }

    #[test]
    fn applies_modification() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "alpha\nbeta\n").unwrap();
        let blocks = vec![EditBlock {
            path: "f.txt".into(),
            search: "beta".into(),
            replace: "gamma".into(),
        }];
        let rep = apply_blocks(dir.path(), &blocks).unwrap();
        assert_eq!(rep.files_changed.get("f.txt"), Some(&ChangeKind::Modified));
        let out = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert_eq!(out, "alpha\ngamma\n");
    }

    #[test]
    fn creates_new_file_with_empty_search() {
        let dir = tempdir().unwrap();
        let blocks = vec![EditBlock {
            path: "new/mod.rs".into(),
            search: "".into(),
            replace: "pub fn x() {}".into(),
        }];
        let rep = apply_blocks(dir.path(), &blocks).unwrap();
        assert_eq!(
            rep.files_changed.get("new/mod.rs"),
            Some(&ChangeKind::Created)
        );
        let out = std::fs::read_to_string(dir.path().join("new/mod.rs")).unwrap();
        assert_eq!(out, "pub fn x() {}\n");
    }

    #[test]
    fn flexible_match_tolerates_trailing_whitespace() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.rs"), "let x = 1;   \nlet y = 2;\n").unwrap();
        let blocks = vec![EditBlock {
            path: "f.rs".into(),
            search: "let x = 1;".into(),
            replace: "let x = 42;".into(),
        }];
        apply_blocks(dir.path(), &blocks).unwrap();
        let out = std::fs::read_to_string(dir.path().join("f.rs")).unwrap();
        assert!(out.contains("let x = 42;"));
    }

    #[test]
    fn rejects_path_traversal() {
        let dir = tempdir().unwrap();
        let blocks = vec![EditBlock {
            path: "../escape.txt".into(),
            search: "".into(),
            replace: "x".into(),
        }];
        assert!(apply_blocks(dir.path(), &blocks).is_err());
    }

    #[test]
    fn missing_search_is_error() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello\n").unwrap();
        let blocks = vec![EditBlock {
            path: "f.txt".into(),
            search: "nonexistent".into(),
            replace: "x".into(),
        }];
        assert!(apply_blocks(dir.path(), &blocks).is_err());
    }
    #[test]
    fn search_longer_than_file_is_error_not_panic() {
        // Regression: flexible_replace used to index out of bounds when the
        // SEARCH spanned more lines than the target file.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "only one line\n").unwrap();
        let blocks = vec![EditBlock {
            path: "f.txt".into(),
            search: "line one\nline two\nline three".into(),
            replace: "x".into(),
        }];
        assert!(apply_blocks(dir.path(), &blocks).is_err());
    }
    #[test]
    fn fallback_uses_code_fence_as_full_file() {
        let text =
            "Here is the solution:\n```python\nimport sys\nprint(sys.stdin.read())\n```\nDone.";
        let blocks = parse_blocks_fallback(text, Some("solution.py"));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].path, "solution.py");
        assert_eq!(blocks[0].search, "");
        assert!(blocks[0].replace.contains("import sys"));
        assert!(!blocks[0].replace.contains("Here is"));
    }

    #[test]
    fn fallback_prefers_real_blocks() {
        let text = "src/a.rs\n<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE";
        let blocks = parse_blocks_fallback(text, Some("solution.py"));
        assert_eq!(blocks[0].path, "src/a.rs");
    }

    #[test]
    fn fallback_none_without_default_path() {
        let text = "```\nsome code\n```";
        assert!(parse_blocks_fallback(text, None).is_empty());
    }

    #[test]
    fn placeholder_path_rejected_with_actionable_error() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("solution.py"), "x = 1\n").unwrap();
        for bad in [
            "path/to/solution.py",
            "path\\to\\solution.py",
            "<solution.py>",
            "example.py",
            "file.ext",
        ] {
            let blocks = vec![EditBlock {
                path: bad.into(),
                search: "x = 1".into(),
                replace: "x = 2".into(),
            }];
            let err = compute_changes(dir.path(), &blocks).unwrap_err().to_string();
            assert!(
                err.contains("placeholder") || err.contains("EXACT"),
                "path `{bad}` gave weak error: {err}"
            );
        }
        // ...and nothing was written to disk.
        assert!(!dir.path().join("path").exists());
    }

    #[test]
    fn backslash_and_dot_slash_paths_normalized() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn a() {}\n").unwrap();
        for raw in ["src\\main.rs", "./src/main.rs", "src//main.rs"] {
            let canon = resolve_model_path(dir.path(), raw, false).unwrap();
            assert_eq!(canon, "src/main.rs", "raw: {raw}");
        }
    }

    #[test]
    fn missing_file_errors_with_hint() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("solution.py"), "x = 1\n").unwrap();
        let blocks = vec![EditBlock {
            path: "totally_missing.py".into(),
            search: "x".into(),
            replace: "y".into(),
        }];
        let err = compute_changes(dir.path(), &blocks).unwrap_err().to_string();
        assert!(err.contains("does not exist"), "weak error: {err}");
    }

    #[test]
    fn directory_path_rejected() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkg")).unwrap();
        let blocks = vec![EditBlock {
            path: "pkg/".into(),
            search: "x".into(),
            replace: "y".into(),
        }];
        assert!(compute_changes(dir.path(), &blocks).is_err());
    }

    #[test]
    fn ambiguous_suffix_errors_with_candidates() {
        let dir = tempdir().unwrap();
        // Neither candidate matches exactly; the raw trailing path is
        // intentionally not a suffix of either, forcing ambiguity via
        // bare-name match is impossible here, so craft: two files share the
        // name and the model path matches both by suffix.
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::create_dir_all(dir.path().join("b")).unwrap();
        std::fs::write(dir.path().join("a/solution.py"), "x = 1\n").unwrap();
        std::fs::write(dir.path().join("b/solution.py"), "x = 1\n").unwrap();
        let blocks = vec![EditBlock {
            path: "solution.py".into(),
            search: "x = 1".into(),
            replace: "x = 2".into(),
        }];
        let err = compute_changes(dir.path(), &blocks).unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "weak error: {err}");
    }

    #[test]
    fn create_new_nested_file_still_works() {
        let dir = tempdir().unwrap();
        let blocks = vec![EditBlock {
            path: "pkg/new_mod.py".into(),
            search: "".into(),
            replace: "X = 1".into(),
        }];
        let rep = apply_blocks(dir.path(), &blocks).unwrap();
        assert_eq!(
            rep.files_changed.get("pkg/new_mod.py"),
            Some(&ChangeKind::Created)
        );
    }
}
