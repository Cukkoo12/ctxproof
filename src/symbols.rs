use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use serde::Serialize;
use tree_sitter::{Language, Node, Parser};

/// Programming languages indexed with `tree-sitter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
}

impl Lang {
    /// Detects supported source language from a file extension.
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Some(Self::Rust),
            "py" => Some(Self::Python),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "js" | "mjs" | "cjs" | "jsx" => Some(Self::JavaScript),
            _ => None,
        }
    }

    fn ts_language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        }
    }
}

/// Extracted code symbol entry from an AST.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolEntry {
    pub name: String,
    pub qualified: String,
    pub kind: &'static str,
    pub file: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
}

/// File metadata tracked for every non-ignored repository file.
#[derive(Debug, Clone)]
pub struct FileMeta {
    pub path: PathBuf,
    pub line_count: usize,
    pub language: Option<Lang>,
}

/// Repository file + AST symbol index used across doc, diff, and claim verification.
#[derive(Debug, Clone)]
pub struct RepoIndex {
    pub root: PathBuf,
    pub files: BTreeMap<String, FileMeta>,
    pub dirs: BTreeSet<String>,
    pub symbols_by_name: BTreeMap<String, Vec<SymbolEntry>>,
    pub symbols_by_file: BTreeMap<String, Vec<SymbolEntry>>,
    lowercase_files: BTreeMap<String, String>,
    lowercase_dirs: BTreeMap<String, String>,
}

impl RepoIndex {
    /// Walks `root` (respecting `.gitignore`, while preserving hidden rule folders
    /// like `.github/`, `.cursor/`, `.agents/`) and indexes source symbols.
    pub fn build(root: &Path) -> Result<Self> {
        let root = std::fs::canonicalize(root)
            .with_context(|| format!("canonicalizing repository root {}", root.display()))?;

        let mut files = BTreeMap::new();
        let mut dirs = BTreeSet::new();
        let mut lowercase_files = BTreeMap::new();
        let mut lowercase_dirs = BTreeMap::new();
        let mut symbols_by_name: BTreeMap<String, Vec<SymbolEntry>> = BTreeMap::new();
        let mut symbols_by_file: BTreeMap<String, Vec<SymbolEntry>> = BTreeMap::new();

        let walker = WalkBuilder::new(&root)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    ".git" | ".ctxproof" | "target" | "node_modules"
                )
            })
            .build();

        let mut parser = Parser::new();
        for item in walker {
            let Ok(entry) = item else { continue };
            let abs = entry.path();
            let Ok(rel) = abs.strip_prefix(&root) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                continue;
            }
            let norm = normalize_rel(rel);
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                lowercase_dirs.insert(norm.to_ascii_lowercase(), norm.clone());
                dirs.insert(norm);
                continue;
            }
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }

            // Record parent directories even if walker filtered intermediate entries.
            record_parent_dirs(&norm, &mut dirs, &mut lowercase_dirs);

            let language = Lang::from_path(abs);
            let bytes = match std::fs::read(abs) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let line_count = count_lines(&bytes);
            if let Some(lang) = language
                && let Ok(text) = std::str::from_utf8(&bytes)
            {
                let entries = extract_symbols(&mut parser, lang, Path::new(&norm), text);
                for sym in &entries {
                    symbols_by_name
                        .entry(sym.name.clone())
                        .or_default()
                        .push(sym.clone());
                    if sym.qualified != sym.name {
                        symbols_by_name
                            .entry(sym.qualified.clone())
                            .or_default()
                            .push(sym.clone());
                    }
                }
                if !entries.is_empty() {
                    symbols_by_file.insert(norm.clone(), entries);
                }
            }

            lowercase_files.insert(norm.to_ascii_lowercase(), norm.clone());
            files.insert(
                norm.clone(),
                FileMeta {
                    path: PathBuf::from(&norm),
                    line_count,
                    language,
                },
            );
        }

        Ok(Self {
            root,
            files,
            dirs,
            symbols_by_name,
            symbols_by_file,
            lowercase_files,
            lowercase_dirs,
        })
    }

    /// Resolves a relative repository path (including `.` / `..` segments and
    /// case-insensitive fallback on Windows), returning metadata if present.
    #[must_use]
    pub fn resolve_file(&self, rel: &str) -> Option<&FileMeta> {
        let cleaned = clean_rel_str(rel)?;
        if let Some(meta) = self.files.get(&cleaned) {
            return Some(meta);
        }
        if cfg!(windows)
            && let Some(actual) = self.lowercase_files.get(&cleaned.to_ascii_lowercase())
        {
            return self.files.get(actual);
        }
        // Handle files inside gitignored folders (e.g. Cargo.lock if ignored, or dotpaths).
        let candidate = self.root.join(&cleaned);
        if candidate.is_file() {
            // Only fall back when file exists under root on disk and wasn't indexed
            // (e.g. if listed in .gitignore while referenced in docs). We avoid storing
            // on `&self`, so callers needing line counts for gitignored files handle via path.
            return None;
        }
        None
    }

    /// Checks whether a relative path exists as either an indexed file, directory,
    /// or non-ignored path on disk under `self.root` (stays strictly inside `root`).
    #[must_use]
    pub fn path_exists(&self, rel: &str) -> PathLookup {
        let Some(cleaned) = clean_rel_str(rel) else {
            return PathLookup::Missing;
        };
        if let Some(meta) = self.resolve_file(&cleaned) {
            return PathLookup::File {
                normalized: meta.path.to_string_lossy().replace('\\', "/"),
                line_count: meta.line_count,
            };
        }
        if self.dirs.contains(&cleaned)
            || (cfg!(windows)
                && self
                    .lowercase_dirs
                    .contains_key(&cleaned.to_ascii_lowercase()))
        {
            return PathLookup::Directory;
        }
        // Fallback for paths ignored by .gitignore or newly created after index build.
        let disk = self.root.join(&cleaned);
        if let Ok(canon) = std::fs::canonicalize(&disk)
            && canon.starts_with(&self.root)
        {
            if canon.is_file() {
                let line_count = std::fs::read(&canon).map(|b| count_lines(&b)).unwrap_or(0);
                return PathLookup::File {
                    normalized: cleaned,
                    line_count,
                };
            }
            if canon.is_dir() {
                return PathLookup::Directory;
            }
        }
        PathLookup::Missing
    }

    /// Looks up a symbol anywhere in the repo, or scoped to one normalized file path.
    #[must_use]
    pub fn find_symbol(&self, query: &str, file_hint: Option<&str>) -> Vec<&SymbolEntry> {
        if let Some(hint) = file_hint.and_then(clean_rel_str) {
            let resolved_key = self
                .resolve_file(&hint)
                .map(|m| m.path.to_string_lossy().replace('\\', "/"))
                .unwrap_or(hint);
            return self
                .symbols_by_file
                .get(&resolved_key)
                .into_iter()
                .flatten()
                .filter(|s| s.name == query || s.qualified == query)
                .collect();
        }
        self.symbols_by_name
            .get(query)
            .map(|list| list.iter().collect())
            .unwrap_or_default()
    }

    /// Looks up a Rust/module-qualified symbol (`Type::method` or `mod::sub::sym`),
    /// resolving file module paths (`src/a/b.rs` -> `a::b`) alongside AST scopes.
    #[must_use]
    pub fn find_qualified_symbol(&self, query: &str) -> Vec<&SymbolEntry> {
        if let Some(direct) = self.symbols_by_name.get(query) {
            return direct.iter().collect();
        }
        let Some((_, leaf)) = query.rsplit_once("::") else {
            return Vec::new();
        };
        let Some(candidates) = self.symbols_by_name.get(leaf) else {
            return Vec::new();
        };
        let suffix = format!("::{query}");
        candidates
            .iter()
            .filter(|sym| {
                let mod_path = rust_file_module_path(&sym.file);
                let full_qual = if mod_path.is_empty() {
                    sym.qualified.clone()
                } else {
                    format!("{mod_path}::{}", sym.qualified)
                };
                full_qual == query || full_qual.ends_with(&suffix)
            })
            .collect()
    }
}

fn rust_file_module_path(file: &Path) -> String {
    let norm = normalize_rel(file);
    let after_src = norm
        .split_once("/src/")
        .map(|(_, rest)| rest)
        .or_else(|| norm.strip_prefix("src/"))
        .unwrap_or(&norm);
    let without_ext = after_src.strip_suffix(".rs").unwrap_or(after_src);
    let trimmed = without_ext
        .strip_suffix("/mod")
        .or_else(|| without_ext.strip_suffix("/lib"))
        .or_else(|| without_ext.strip_suffix("/main"))
        .unwrap_or(without_ext);
    if matches!(trimmed, "lib" | "main" | "") {
        String::new()
    } else {
        trimmed.replace('/', "::")
    }
}

/// Outcome of resolving a relative doc/claim path reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathLookup {
    File {
        normalized: String,
        line_count: usize,
    },
    Directory,
    Missing,
}

/// Normalizes a relative `Path` to forward-slash notation.
#[must_use]
pub fn normalize_rel(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Cleans `./`, `../`, and backslashes, refusing paths that escape the repo root.
#[must_use]
pub fn clean_rel_str(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim()
        .trim_matches(|c| matches!(c, '`' | '"' | '\'' | '<' | '>'));
    if trimmed.is_empty()
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with('#')
    {
        return None;
    }
    let forward = trimmed.replace('\\', "/");
    let path = Path::new(&forward);
    if path.is_absolute() {
        return None;
    }
    let mut stack: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(seg) => stack.push(seg.to_string_lossy().into_owned()),
            Component::ParentDir => {
                stack.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if stack.is_empty() {
        None
    } else {
        Some(stack.join("/"))
    }
}

fn record_parent_dirs(
    norm: &str,
    dirs: &mut BTreeSet<String>,
    lowercase: &mut BTreeMap<String, String>,
) {
    let mut path = Path::new(norm);
    while let Some(parent) = path.parent() {
        let s = normalize_rel(parent);
        if s.is_empty() {
            break;
        }
        lowercase.insert(s.to_ascii_lowercase(), s.clone());
        if !dirs.insert(s) {
            break;
        }
        path = parent;
    }
}

/// Counts lines accurately (returns `0` for empty file, otherwise number of lines).
#[must_use]
pub fn count_lines(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    if bytes.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    }
}

/// Extracts named definitions from a source file using `tree-sitter`.
#[must_use]
pub fn extract_symbols(
    parser: &mut Parser,
    lang: Lang,
    rel_file: &Path,
    source: &str,
) -> Vec<SymbolEntry> {
    if parser.set_language(&lang.ts_language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut scope: Vec<String> = Vec::new();
    walk_node(
        tree.root_node(),
        lang,
        rel_file,
        source.as_bytes(),
        &mut scope,
        &mut out,
    );
    out
}

fn walk_node(
    node: Node<'_>,
    lang: Lang,
    file: &Path,
    src: &[u8],
    scope: &mut Vec<String>,
    out: &mut Vec<SymbolEntry>,
) {
    let kind = node.kind();
    if let Some((sym_kind, name_node, pushes_scope)) = classify_node(node, lang, src)
        && let Ok(name_text) = name_node.utf8_text(src)
    {
        let name = name_text.trim().to_owned();
        if !name.is_empty() {
            let sep = match lang {
                Lang::Rust => "::",
                Lang::Python | Lang::TypeScript | Lang::Tsx | Lang::JavaScript => ".",
            };
            let qualified = if scope.is_empty() {
                name.clone()
            } else {
                format!("{}{sep}{name}", scope.join(sep))
            };
            out.push(SymbolEntry {
                name: name.clone(),
                qualified,
                kind: sym_kind,
                file: file.to_path_buf(),
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
            });
            if pushes_scope {
                scope.push(name);
                walk_children(node, lang, file, src, scope, out);
                scope.pop();
                return;
            }
        }
    }

    // Rust `impl` blocks push their target type onto the scope stack so methods resolve as `Type::method`.
    if lang == Lang::Rust
        && kind == "impl_item"
        && let Some(ty) = node.child_by_field_name("type")
        && let Ok(ty_text) = ty.utf8_text(src)
    {
        let base_ty = ty_text
            .split('<')
            .next()
            .unwrap_or(ty_text)
            .split("::")
            .last()
            .unwrap_or(ty_text)
            .trim()
            .to_owned();
        if !base_ty.is_empty() {
            scope.push(base_ty);
            walk_children(node, lang, file, src, scope, out);
            scope.pop();
            return;
        }
    }

    walk_children(node, lang, file, src, scope, out);
}

fn walk_children(
    node: Node<'_>,
    lang: Lang,
    file: &Path,
    src: &[u8],
    scope: &mut Vec<String>,
    out: &mut Vec<SymbolEntry>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_node(child, lang, file, src, scope, out);
    }
}

fn classify_node<'a>(
    node: Node<'a>,
    lang: Lang,
    _src: &[u8],
) -> Option<(&'static str, Node<'a>, bool)> {
    let kind = node.kind();
    match lang {
        Lang::Rust => match kind {
            "function_item" | "function_signature_item" => {
                Some(("fn", node.child_by_field_name("name")?, false))
            }
            "struct_item" => Some(("struct", node.child_by_field_name("name")?, true)),
            "enum_item" => Some(("enum", node.child_by_field_name("name")?, true)),
            "trait_item" => Some(("trait", node.child_by_field_name("name")?, true)),
            "mod_item" => Some(("mod", node.child_by_field_name("name")?, true)),
            "type_item" => Some(("type", node.child_by_field_name("name")?, false)),
            "const_item" => Some(("const", node.child_by_field_name("name")?, false)),
            "static_item" => Some(("static", node.child_by_field_name("name")?, false)),
            "macro_definition" => Some(("macro", node.child_by_field_name("name")?, false)),
            _ => None,
        },
        Lang::Python => match kind {
            "function_definition" => Some(("fn", node.child_by_field_name("name")?, false)),
            "class_definition" => Some(("class", node.child_by_field_name("name")?, true)),
            _ => None,
        },
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => match kind {
            "function_declaration" | "generator_function_declaration" => {
                Some(("fn", node.child_by_field_name("name")?, false))
            }
            "class_declaration" => Some(("class", node.child_by_field_name("name")?, true)),
            "method_definition" => Some(("method", node.child_by_field_name("name")?, false)),
            "interface_declaration" => Some(("interface", node.child_by_field_name("name")?, true)),
            "type_alias_declaration" => Some(("type", node.child_by_field_name("name")?, false)),
            "enum_declaration" => Some(("enum", node.child_by_field_name("name")?, true)),
            "variable_declarator" => {
                let is_top_level = node
                    .parent()
                    .and_then(|p| p.parent())
                    .is_some_and(|gp| matches!(gp.kind(), "program" | "export_statement"));
                if is_top_level {
                    Some(("var", node.child_by_field_name("name")?, false))
                } else {
                    None
                }
            }
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_python_and_ts_symbols_with_scopes() {
        let mut parser = Parser::new();

        let rs = extract_symbols(
            &mut parser,
            Lang::Rust,
            Path::new("src/lib.rs"),
            r#"
pub struct Engine { x: i32 }
impl Engine {
    pub fn start(&self) {}
}
pub fn helper() {}
"#,
        );
        assert!(
            rs.iter()
                .any(|s| s.name == "Engine" && s.kind == "struct" && s.start_line == 2)
        );
        assert!(
            rs.iter()
                .any(|s| s.qualified == "Engine::start" && s.name == "start")
        );
        assert!(rs.iter().any(|s| s.name == "helper" && s.start_line == 6));

        let py = extract_symbols(
            &mut parser,
            Lang::Python,
            Path::new("app.py"),
            "class Runner:\n    def run(self):\n        pass\n",
        );
        assert!(py.iter().any(|s| s.name == "Runner" && s.kind == "class"));
        assert!(py.iter().any(|s| s.qualified == "Runner.run"));

        let ts = extract_symbols(
            &mut parser,
            Lang::TypeScript,
            Path::new("index.ts"),
            "export interface Config { ok: boolean }\nexport function boot(): void {}\nexport const VERSION = 1;\n",
        );
        assert!(
            ts.iter()
                .any(|s| s.name == "Config" && s.kind == "interface")
        );
        assert!(ts.iter().any(|s| s.name == "boot" && s.kind == "fn"));
        assert!(ts.iter().any(|s| s.name == "VERSION" && s.kind == "var"));
    }

    #[test]
    fn rejects_parent_escape_paths() {
        assert_eq!(clean_rel_str("../outside.rs"), None);
        assert_eq!(clean_rel_str("./src\\lib.rs"), Some("src/lib.rs".into()));
        assert_eq!(clean_rel_str("a/b/../c.rs"), Some("a/c.rs".into()));
    }
}
