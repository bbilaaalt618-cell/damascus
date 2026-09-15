//! Sub-file AST slicing — the context-isolation strategy.
//!
//! OSS models lose accuracy fast as context grows. So instead of handing over a
//! whole file (or repo), Damascus gives the model exactly the target definition
//! plus the *signatures* of the types/functions it depends on, capped to a few
//! thousand tokens. Concentration over volume.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::ast::{self, Lang, Symbol, SymbolKind};

/// ~4 chars per token; default snippet budget ≈ 3.5K tokens.
pub const DEFAULT_MAX_CHARS: usize = 14_000;
/// A referenced type smaller than this is inlined in full (fields matter).
const INLINE_TYPE_MAX_LINES: usize = 20;
const MAX_DEPS: usize = 14;

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

/// One parsed source file held in the index.
struct IndexedFile {
    lang: Lang,
    source: String,
    symbols: Vec<Symbol>,
}

/// A repo-wide symbol index built once per run.
pub struct RepoIndex {
    root: PathBuf,
    files: BTreeMap<String, IndexedFile>,
    /// symbol name -> (rel_path, symbol_idx) for fast dependency lookup.
    by_name: BTreeMap<String, Vec<(String, usize)>>,
}

impl RepoIndex {
    /// Parse every supported source file under `root`.
    pub fn build(root: &Path) -> Self {
        let mut files = BTreeMap::new();
        let mut by_name: BTreeMap<String, Vec<(String, usize)>> = BTreeMap::new();
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_entry(|e| !is_skipped(e.path()))
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let Some(lang) = Lang::from_path(entry.path()) else {
                continue;
            };
            let Ok(source) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            if source.len() > 400_000 {
                continue; // skip pathologically large files
            }
            // Forward slashes on every OS (see `crate::rel_forward`).
            let rel = crate::rel_forward(root, entry.path());
            let syms = ast::symbols(lang, &source);
            for (i, s) in syms.iter().enumerate() {
                by_name
                    .entry(s.name.clone())
                    .or_default()
                    .push((rel.clone(), i));
            }
            files.insert(
                rel,
                IndexedFile {
                    lang,
                    source,
                    symbols: syms,
                },
            );
        }
        RepoIndex {
            root: root.to_path_buf(),
            files,
            by_name,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Raw source of an indexed file, if present.
    pub fn file_source(&self, rel_path: &str) -> Option<&str> {
        self.files.get(rel_path).map(|f| f.source.as_str())
    }

    /// Whether a path is a supported, parsed source file in the index.
    pub fn has_file(&self, rel_path: &str) -> bool {
        self.files.contains_key(rel_path)
    }

    /// All symbol names in a given file (for planner targeting hints).
    pub fn symbols_in(&self, rel_path: &str) -> Vec<&Symbol> {
        self.files
            .get(rel_path)
            .map(|f| f.symbols.iter().collect())
            .unwrap_or_default()
    }

    /// All indexed file paths (sorted).
    pub fn file_paths(&self) -> Vec<&str> {
        self.files.keys().map(|s| s.as_str()).collect()
    }

    fn symbol_text<'a>(&'a self, file: &'a IndexedFile, s: &Symbol) -> &'a str {
        file.source.get(s.start_byte..s.end_byte).unwrap_or("")
    }

    /// Build a compact slice for the named symbol in `rel_path`. Returns `None`
    /// when the file is unsupported or the symbol is absent (caller falls back).
    pub fn slice_symbol(&self, rel_path: &str, name: &str, max_chars: usize) -> Option<Slice> {
        let file = self.files.get(rel_path)?;
        let target = ast::find_symbol(&file.symbols, name)?.clone();
        let target_text = self.symbol_text(file, &target).to_string();

        let deps = self.collect_deps(rel_path, &target, &target_text);

        let mut snippet = String::new();
        snippet.push_str(&format!(
            "// file: {rel_path}  (lang: {})\n",
            file.lang.name()
        ));
        if !deps.is_empty() {
            snippet.push_str("// ---- dependencies (read-only context) ----\n");
            for d in &deps {
                snippet.push_str(&d.render());
                snippet.push('\n');
            }
        }
        snippet.push_str(&format!(
            "// ---- target: {} (edit ONLY this definition) ----\n",
            target.name
        ));
        snippet.push_str(&target_text);
        snippet.push('\n');

        if snippet.len() > max_chars {
            snippet.truncate(nearest_char_boundary(&snippet, max_chars));
            snippet.push_str("\n// … (context truncated)\n");
        }

        Some(Slice {
            lang: file.lang,
            rel_path: rel_path.to_string(),
            target,
            target_text,
            snippet,
        })
    }

    /// Find definitions referenced (by identifier) inside the target body.
    fn collect_deps(&self, target_path: &str, target: &Symbol, target_text: &str) -> Vec<Dep> {
        let idents = identifiers(target_text);
        let mut deps = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for ident in idents {
            if ident == target.name || !seen.insert(ident.clone()) {
                continue;
            }
            let Some(locs) = self.by_name.get(&ident) else {
                continue;
            };
            // Prefer a definition in another file or a different symbol.
            for (path, idx) in locs {
                let file = &self.files[path];
                let sym = &file.symbols[*idx];
                if path == target_path && sym.start_byte == target.start_byte {
                    continue;
                }
                let text = self.symbol_text(file, sym);
                deps.push(Dep::new(sym, text));
                break;
            }
            if deps.len() >= MAX_DEPS {
                break;
            }
        }
        deps
    }
}

/// A compact, model-ready slice of the codebase.
pub struct Slice {
    pub lang: Lang,
    pub rel_path: String,
    pub target: Symbol,
    pub target_text: String,
    pub snippet: String,
}

struct Dep {
    signature: String,
    full: String,
    inline_full: bool,
}

impl Dep {
    fn new(sym: &Symbol, text: &str) -> Self {
        let inline_full = sym.kind == SymbolKind::Type && sym.line_count() <= INLINE_TYPE_MAX_LINES;
        Dep {
            signature: sym.signature.clone(),
            full: text.to_string(),
            inline_full,
        }
    }
    fn render(&self) -> String {
        if self.inline_full {
            self.full.clone()
        } else {
            format!(
                "{} {{ … }}",
                self.signature.trim_end_matches([' ', '{']).trim_end()
            )
        }
    }
}

/// Extract identifier-like words (`[A-Za-z_][A-Za-z0-9_]*`) from code text.
fn identifiers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
        } else if !cur.is_empty() {
            push_ident(&mut out, &mut cur);
        }
    }
    if !cur.is_empty() {
        push_ident(&mut out, &mut cur);
    }
    out
}

fn push_ident(out: &mut Vec<String>, cur: &mut String) {
    let word = std::mem::take(cur);
    if word
        .chars()
        .next()
        .map(|c| c.is_alphabetic() || c == '_')
        .unwrap_or(false)
    {
        out.push(word);
    }
}

fn nearest_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
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

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn slices_target_and_pulls_dep_type() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "src/geo.rs",
            "pub struct Point { pub x: i32, pub y: i32 }\n",
        );
        write(
            dir.path(),
            "src/lib.rs",
            "use crate::geo::Point;\n\npub fn norm(p: Point) -> i32 {\n    p.x * p.x + p.y * p.y\n}\n",
        );
        let idx = RepoIndex::build(dir.path());
        let slice = idx
            .slice_symbol("src/lib.rs", "norm", DEFAULT_MAX_CHARS)
            .unwrap();
        assert_eq!(slice.target.name, "norm");
        assert!(slice.snippet.contains("fn norm(p: Point) -> i32"));
        // Dependency Point is inlined as context.
        assert!(slice.snippet.contains("struct Point"));
        // Target marker present.
        assert!(slice.snippet.contains("edit ONLY this definition"));
    }

    #[test]
    fn missing_symbol_returns_none() {
        let dir = tempdir().unwrap();
        write(dir.path(), "a.rs", "fn x() {}\n");
        let idx = RepoIndex::build(dir.path());
        assert!(idx
            .slice_symbol("a.rs", "ghost", DEFAULT_MAX_CHARS)
            .is_none());
    }

    #[test]
    fn identifiers_extracts_words() {
        let ids = identifiers("let p = Point::new(1, 2);");
        assert!(ids.contains(&"Point".to_string()));
        assert!(ids.contains(&"new".to_string()));
        assert!(!ids.contains(&"1".to_string()));
    }
}
