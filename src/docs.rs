use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::manifests::ProjectManifests;
use crate::model::{CheckKind, Diagnostic, Report, Severity};
use crate::symbols::{Lang, PathLookup, RepoIndex, SymbolEntry, clean_rel_str};

const DOC_EXACT_NAMES: &[&str] = &[
    "AGENTS.md",
    "CLAUDE.md",
    "GEMINI.md",
    "RULES.md",
    "CONTRIBUTING.md",
    "README.md",
    ".cursorrules",
    "copilot-instructions.md",
    "SKILL.md",
];

const STANDARD_CARGO_SUBCOMMANDS: &[&str] = &[
    "add",
    "bench",
    "build",
    "check",
    "clean",
    "clippy",
    "doc",
    "fetch",
    "fix",
    "fmt",
    "generate-lockfile",
    "help",
    "info",
    "init",
    "install",
    "locate-project",
    "login",
    "logout",
    "metadata",
    "new",
    "owner",
    "package",
    "pkgid",
    "publish",
    "read-manifest",
    "remove",
    "report",
    "run",
    "rustc",
    "rustdoc",
    "search",
    "test",
    "tree",
    "uninstall",
    "update",
    "vendor",
    "verify-project",
    "version",
    "yank",
];

const CODE_DOC_EXTENSIONS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "c", "h", "cpp", "cc", "hpp", "cs",
    "csproj", "sln", "asmdef", "unity", "prefab", "shader", "hlsl", "cginc", "toml", "json", "yml",
    "yaml", "md", "sh", "ps1", "lock",
];

/// Runs context-rot checks across AI instruction files, skills, and README docs.
#[must_use]
pub fn check_docs(index: &RepoIndex, manifests: &ProjectManifests) -> Report {
    check_docs_with_config(index, manifests, &Config::default(), false)
}

/// Runs context-rot checks with `.ctxproof.toml` config and optional `--fix` updates
/// for shifted symbol line links (`[sym](file.rs#L10-L20)`).
#[must_use]
pub fn check_docs_with_config(
    index: &RepoIndex,
    manifests: &ProjectManifests,
    config: &Config,
    fix: bool,
) -> Report {
    let mut doc_files = discover_doc_files(index);
    for extra in &config.extra_docs {
        if let Some(clean) = clean_rel_str(extra)
            && !doc_files.contains(&clean)
            && matches!(index.path_exists(&clean), PathLookup::File { .. })
        {
            doc_files.push(clean);
        }
    }
    doc_files.retain(|f| !config.is_path_ignored(f));
    let mut diagnostics = Vec::new();
    let mut seen = BTreeSet::new();
    let mut js_mentions: BTreeMap<String, Vec<(PathBuf, usize)>> = BTreeMap::new();

    for doc_rel in &doc_files {
        let abs = index.root.join(doc_rel);
        let Ok(content) = std::fs::read_to_string(&abs) else {
            continue;
        };
        let doc_path = PathBuf::from(doc_rel);
        let doc_parent = doc_path.parent().unwrap_or_else(|| Path::new(""));
        let is_instruction_file = is_ai_instruction_file(doc_rel);
        let mut line_rewrites: Vec<(String, String)> = Vec::new();

        let parsed = scan_markdown(&content);
        for link in parsed.links {
            if let Some((old_target, new_target, fix_msg)) = fix
                .then(|| try_fix_symbol_link(index, doc_parent, &link))
                .flatten()
            {
                line_rewrites.push((old_target, new_target));
                diagnostics.push(Diagnostic::new(
                    Severity::Info,
                    CheckKind::BrokenLineRange,
                    Some(doc_path.clone()),
                    Some(link.line),
                    fix_msg,
                ));
                continue;
            }
            validate_path_ref(
                index,
                config,
                (&doc_path, doc_parent),
                link.line,
                &link.target,
                &mut diagnostics,
                &mut seen,
            );
        }
        if !line_rewrites.is_empty() {
            let mut updated = content.clone();
            for (old, new) in line_rewrites {
                updated = updated.replacen(&format!("({old})"), &format!("({new})"), 1);
            }
            let _ = std::fs::write(&abs, updated);
        }
        for span in parsed.backticks {
            if looks_like_command(&span.text) {
                if config.is_command_ignored(&span.text) {
                    continue;
                }
                validate_command_tokens(
                    manifests,
                    &doc_path,
                    span.line,
                    &span.text,
                    &mut diagnostics,
                    &mut seen,
                    is_instruction_file.then_some(&mut js_mentions),
                );
                continue;
            }
            if looks_like_symbol_decl(&span.text) {
                validate_symbol_decl(index, &doc_path, span.line, &span.text, &mut diagnostics);
                continue;
            }
            if looks_like_repo_path(&span.text) {
                validate_path_ref(
                    index,
                    config,
                    (&doc_path, doc_parent),
                    span.line,
                    &span.text,
                    &mut diagnostics,
                    &mut seen,
                );
            }
        }
        for cmd in parsed.shell_commands {
            if config.is_command_ignored(&cmd.text) {
                continue;
            }
            validate_command_tokens(
                manifests,
                &doc_path,
                cmd.line,
                &cmd.text,
                &mut diagnostics,
                &mut seen,
                is_instruction_file.then_some(&mut js_mentions),
            );
        }
    }

    check_js_manager_conflicts(manifests, &js_mentions, &mut diagnostics);
    let diagnostics = config.filter_diagnostics(diagnostics);

    let errors = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();
    let summary = format!(
        "scanned {} doc/instruction files ({errors} errors, {warnings} warnings)",
        doc_files.len()
    );
    Report::from_diagnostics("docs", index.root.clone(), summary, diagnostics)
}

/// Discovers AI instruction files, skills, and project documentation.
#[must_use]
pub fn discover_doc_files(index: &RepoIndex) -> Vec<String> {
    index
        .files
        .keys()
        .filter(|rel| {
            let file_name = Path::new(rel)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            DOC_EXACT_NAMES.contains(&file_name)
                || is_ai_instruction_file(rel)
                || (rel.starts_with("docs/") && rel.ends_with(".md"))
        })
        .cloned()
        .collect()
}

fn is_ai_instruction_file(rel: &str) -> bool {
    let file_name = Path::new(rel)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    matches!(
        file_name,
        "AGENTS.md"
            | "CLAUDE.md"
            | "GEMINI.md"
            | "RULES.md"
            | ".cursorrules"
            | "copilot-instructions.md"
            | "SKILL.md"
    ) || rel.starts_with(".cursor/rules/")
        || rel.starts_with(".agents/")
        || rel.starts_with(".claude/")
}

#[derive(Debug)]
struct LineText {
    line: usize,
    text: String,
}

#[derive(Debug)]
struct MarkdownLink {
    line: usize,
    label: String,
    target: String,
}

#[derive(Debug, Default)]
struct ParsedMarkdown {
    links: Vec<MarkdownLink>,
    backticks: Vec<LineText>,
    shell_commands: Vec<LineText>,
}

fn scan_markdown(text: &str) -> ParsedMarkdown {
    let mut out = ParsedMarkdown::default();
    let mut in_fence = false;
    let mut fence_shell = false;
    let mut fence_len = 0usize;

    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw_line.trim_start();
        let backtick_run = trimmed.bytes().take_while(|&b| b == b'`').count();
        if backtick_run >= 3 {
            if !in_fence {
                in_fence = true;
                fence_len = backtick_run;
                let lang = trimmed[backtick_run..]
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                fence_shell = matches!(
                    lang.as_str(),
                    "sh" | "bash" | "zsh" | "shell" | "pwsh" | "powershell" | "console"
                );
            } else if backtick_run >= fence_len && trimmed[backtick_run..].trim().is_empty() {
                in_fence = false;
                fence_shell = false;
            }
            continue;
        }

        if in_fence {
            if fence_shell {
                let cmd = trimmed.trim_start_matches("$ ").trim_start_matches("> ");
                if !cmd.is_empty() && !cmd.starts_with('#') {
                    out.shell_commands.push(LineText {
                        line: line_no,
                        text: cmd.to_owned(),
                    });
                }
            }
            continue;
        }

        extract_links_in_line(raw_line, line_no, &mut out.links);
        extract_backticks_in_line(raw_line, line_no, &mut out.backticks);
    }

    out
}

fn extract_links_in_line(line: &str, line_no: usize, out: &mut Vec<MarkdownLink>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b']' && i + 1 < bytes.len() && bytes[i + 1] == b'(' {
            let mut j = i + 2;
            let mut depth = 1usize;
            while j < bytes.len() && depth > 0 {
                match bytes[j] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                if depth > 0 {
                    j += 1;
                }
            }
            if depth == 0 {
                let target = line[i + 2..j].trim();
                let target = target.split_whitespace().next().unwrap_or("");
                if !target.is_empty()
                    && !target.starts_with('#')
                    && !target.contains("://")
                    && !target.starts_with("mailto:")
                {
                    let label = line[..i]
                        .rfind('[')
                        .map(|open| line[open + 1..i].trim().to_owned())
                        .unwrap_or_default();
                    out.push(MarkdownLink {
                        line: line_no,
                        label,
                        target: target.trim_matches(|c| c == '<' || c == '>').to_owned(),
                    });
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
}

fn extract_backticks_in_line(line: &str, line_no: usize, out: &mut Vec<LineText>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let start = i + 1;
            if let Some(rel_end) = line[start..].find('`') {
                let inner = line[start..start + rel_end].trim();
                if !inner.is_empty() {
                    out.push(LineText {
                        line: line_no,
                        text: inner.to_owned(),
                    });
                }
                i = start + rel_end + 1;
                continue;
            }
        }
        i += 1;
    }
}

/// Splits `path/file.rs#L10-L20` or `path/file.rs:10-20` into `(path, Option<(start, end)>)`.
#[must_use]
pub fn split_path_and_line_range(raw: &str) -> (&str, Option<(usize, usize)>) {
    if let Some((path, frag)) = raw.split_once('#') {
        if let Some(range) = parse_line_fragment(frag) {
            return (path, Some(range));
        }
        return (path, None);
    }
    if let Some((path, suffix)) = raw.rsplit_once(':')
        && !path.is_empty()
        && let Some(range) = parse_line_fragment(suffix)
    {
        return (path, Some(range));
    }
    (raw, None)
}

fn parse_line_fragment(frag: &str) -> Option<(usize, usize)> {
    let clean = frag.trim();
    if let Some((a, b)) = clean.split_once('-') {
        let start = a.trim_start_matches('L').parse::<usize>().ok()?;
        let end = b.trim_start_matches('L').parse::<usize>().ok()?;
        Some((start, end))
    } else if let Some(num) = clean.strip_prefix('L') {
        let line = num.parse::<usize>().ok()?;
        Some((line, line))
    } else if clean.bytes().all(|b| b.is_ascii_digit()) && !clean.is_empty() {
        let line = clean.parse::<usize>().ok()?;
        Some((line, line))
    } else {
        None
    }
}

fn has_placeholder_tokens(s: &str) -> bool {
    s.contains('<')
        || s.contains('>')
        || s.contains('{')
        || s.contains('}')
        || s.contains('*')
        || s.contains('?')
        || s.contains('$')
        || s.contains('%')
        || s.starts_with("path/to/")
        || s.starts_with("example/")
        || s.contains("...")
}

fn looks_like_repo_path(s: &str) -> bool {
    if s.len() < 3
        || s.len() > 140
        || s.contains(' ')
        || s.contains("://")
        || has_placeholder_tokens(s)
        || s.starts_with('-')
    {
        return false;
    }
    let (base, line_range) = split_path_and_line_range(s);
    if base.starts_with('/') || base.contains("::") {
        return false;
    }
    if line_range.is_some() {
        return base.contains('.') || base.contains('/') || base.contains('\\');
    }
    let forward = base.replace('\\', "/");
    if forward.contains('/') {
        let first = forward.split('/').next().unwrap_or("");
        let looks_like_mime = matches!(
            first,
            "application" | "text" | "image" | "audio" | "video" | "multipart"
        ) && !forward.contains('.');
        let is_runtime_dir = matches!(
            first,
            "target"
                | "node_modules"
                | ".ctxproof"
                | ".git"
                | "dist"
                | "build"
                | ".venv"
                | "__pycache__"
                | ".cache"
                | "out"
                | "tmp"
                | "Library"
                | "Temp"
                | "Logs"
                | "Obj"
                | "obj"
                | "bin"
        );
        if looks_like_mime || is_runtime_dir {
            return false;
        }
        let has_known_prefix = matches!(
            first,
            "src"
                | "crates"
                | "apps"
                | "tests"
                | "docs"
                | "scripts"
                | "Assets"
                | ".github"
                | ".cursor"
                | ".agents"
        );
        let has_ext = Path::new(&forward)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| CODE_DOC_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
        return has_known_prefix || has_ext;
    }
    false
}

fn try_fix_symbol_link(
    index: &RepoIndex,
    doc_parent: &Path,
    link: &MarkdownLink,
) -> Option<(String, String, String)> {
    let (raw_path, Some((old_start, old_end))) = split_path_and_line_range(&link.target) else {
        return None;
    };
    let clean_label = link.label.trim().trim_matches('`');
    let sym_query = clean_label
        .split_once(' ')
        .and_then(|(kw, rest)| {
            matches!(
                kw,
                "fn" | "struct" | "enum" | "trait" | "def" | "class" | "func" | "type"
            )
            .then_some(rest.trim())
        })
        .unwrap_or(clean_label)
        .trim_end_matches("()");
    if sym_query.is_empty() || sym_query.contains(' ') {
        return None;
    }

    let rel_from_doc = (!doc_parent.as_os_str().is_empty())
        .then(|| clean_rel_str(&format!("{}/{raw_path}", doc_parent.display())))
        .flatten();
    let target_file = rel_from_doc
        .as_deref()
        .and_then(|r| index.resolve_file(r))
        .or_else(|| index.resolve_file(raw_path))?;
    let target_norm = target_file.path.to_string_lossy().replace('\\', "/");

    let matches: Vec<&SymbolEntry> = index.find_symbol(sym_query, Some(&target_norm));
    if matches.len() != 1 {
        return None;
    }
    let sym = matches[0];
    if sym.start_line == old_start && sym.end_line == old_end {
        return None;
    }
    let new_target = if old_start == old_end && sym.start_line == sym.end_line {
        format!("{raw_path}#L{}", sym.start_line)
    } else {
        format!("{raw_path}#L{}-L{}", sym.start_line, sym.end_line)
    };
    if new_target == link.target {
        return None;
    }
    let msg = format!(
        "fixed shifted line range `{}` -> `{}` for symbol `{sym_query}`",
        link.target, new_target
    );
    Some((link.target.clone(), new_target, msg))
}

fn validate_path_ref(
    index: &RepoIndex,
    config: &Config,
    doc: (&Path, &Path),
    line_no: usize,
    raw_target: &str,
    diagnostics: &mut Vec<Diagnostic>,
    seen: &mut BTreeSet<(PathBuf, usize, String)>,
) {
    let (doc_path, doc_parent) = doc;
    if has_placeholder_tokens(raw_target) {
        return;
    }
    let (raw_path, line_range) = split_path_and_line_range(raw_target);
    let raw_trimmed = raw_path.trim_end_matches('/');
    if raw_trimmed.is_empty() || config.is_path_ignored(raw_trimmed) {
        return;
    }

    // Try resolving relative to the doc's folder first, then from the repo root.
    let rel_from_doc = if doc_parent.as_os_str().is_empty() {
        None
    } else {
        clean_rel_str(&format!("{}/{raw_trimmed}", doc_parent.display()))
    };
    let lookup_doc = rel_from_doc
        .as_deref()
        .map(|r| index.path_exists(r))
        .unwrap_or(PathLookup::Missing);
    let lookup = match lookup_doc {
        PathLookup::Missing => index.path_exists(raw_trimmed),
        found => found,
    };

    let key = (doc_path.to_path_buf(), line_no, raw_target.to_owned());
    match lookup {
        PathLookup::Missing => {
            if seen.insert(key) {
                diagnostics.push(
                    Diagnostic::new(
                        Severity::Error,
                        CheckKind::MissingPath,
                        Some(doc_path.to_path_buf()),
                        Some(line_no),
                        format!("referenced path `{raw_trimmed}` does not exist in repo"),
                    )
                    .with_suggestion("update or remove the stale path reference"),
                );
            }
        }
        PathLookup::Directory => {
            if line_range.is_some() && seen.insert(key) {
                diagnostics.push(Diagnostic::new(
                    Severity::Error,
                    CheckKind::BrokenLineRange,
                    Some(doc_path.to_path_buf()),
                    Some(line_no),
                    format!("line range given for directory `{raw_trimmed}`"),
                ));
            }
        }
        PathLookup::File {
            normalized,
            line_count,
        } => {
            if let Some((start, end)) = line_range
                && (start == 0 || end < start || end > line_count)
                && seen.insert(key)
            {
                diagnostics.push(
                    Diagnostic::new(
                        Severity::Error,
                        CheckKind::BrokenLineRange,
                        Some(doc_path.to_path_buf()),
                        Some(line_no),
                        format!(
                            "line range L{start}-L{end} is out of bounds for `{normalized}` ({line_count} lines)"
                        ),
                    )
                    .with_suggestion(format!("valid lines for `{normalized}` are 1..={line_count}")),
                );
            }
        }
    }
}

fn looks_like_symbol_decl(s: &str) -> bool {
    let trimmed = s.trim();
    for prefix in [
        "fn ",
        "func ",
        "struct ",
        "enum ",
        "trait ",
        "def ",
        "class ",
        "interface ",
        "record ",
        "namespace ",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let ident = rest.split(['(', '<', '{', ':', ' ']).next().unwrap_or("");
            return !ident.is_empty()
                && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        }
    }
    false
}

fn validate_symbol_decl(
    index: &RepoIndex,
    doc_path: &Path,
    line_no: usize,
    raw: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let trimmed = raw.trim();
    let (kw, rest) = trimmed.split_once(' ').unwrap_or(("", ""));
    let ident = rest
        .split(['(', '<', '{', ':', ' '])
        .next()
        .unwrap_or("")
        .trim();
    if ident.is_empty() || has_placeholder_tokens(ident) {
        return;
    }
    let has_lang = match kw {
        "fn" | "trait" => index.files.values().any(|f| f.language == Some(Lang::Rust)),
        "func" => index.files.values().any(|f| f.language == Some(Lang::Go)),
        "record" => index
            .files
            .values()
            .any(|f| f.language == Some(Lang::CSharp)),
        "namespace" => index
            .files
            .values()
            .any(|f| matches!(f.language, Some(Lang::Cpp | Lang::CSharp))),
        "struct" => index.files.values().any(|f| {
            matches!(
                f.language,
                Some(Lang::Rust | Lang::C | Lang::Cpp | Lang::CSharp)
            )
        }),
        "enum" => index.files.values().any(|f| {
            matches!(
                f.language,
                Some(
                    Lang::Rust | Lang::C | Lang::Cpp | Lang::CSharp | Lang::TypeScript | Lang::Tsx
                )
            )
        }),
        "def" => index
            .files
            .values()
            .any(|f| f.language == Some(Lang::Python)),
        "interface" => index.files.values().any(|f| {
            matches!(
                f.language,
                Some(Lang::TypeScript | Lang::Tsx | Lang::CSharp)
            )
        }),
        "class" => index.files.values().any(|f| {
            matches!(
                f.language,
                Some(
                    Lang::Python
                        | Lang::TypeScript
                        | Lang::Tsx
                        | Lang::JavaScript
                        | Lang::Cpp
                        | Lang::CSharp
                )
            )
        }),
        _ => false,
    };
    if !has_lang {
        return;
    }
    if index.find_symbol(ident, None).is_empty() {
        diagnostics.push(Diagnostic::new(
            Severity::Warning,
            CheckKind::MissingSymbol,
            Some(doc_path.to_path_buf()),
            Some(line_no),
            format!("documented symbol `{trimmed}` was not found in indexed AST symbols"),
        ));
    }
}

fn looks_like_command(s: &str) -> bool {
    let first = s.split_whitespace().next().unwrap_or("");
    matches!(
        first,
        "cargo" | "npm" | "pnpm" | "yarn" | "bun" | "make" | "just"
    ) && s.split_whitespace().count() >= 2
}

fn validate_command_tokens(
    manifests: &ProjectManifests,
    doc_path: &Path,
    line_no: usize,
    cmd: &str,
    diagnostics: &mut Vec<Diagnostic>,
    seen: &mut BTreeSet<(PathBuf, usize, String)>,
    js_mentions: Option<&mut BTreeMap<String, Vec<(PathBuf, usize)>>>,
) {
    if has_placeholder_tokens(cmd) {
        return;
    }
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    match parts[0] {
        "cargo" if manifests.has_cargo_workspace => {
            validate_cargo_cmd(manifests, doc_path, line_no, &parts, diagnostics, seen);
        }
        pm @ ("npm" | "pnpm" | "yarn" | "bun") if manifests.has_package_json => {
            if let Some(map) = js_mentions {
                map.entry((*pm).to_owned())
                    .or_default()
                    .push((doc_path.to_path_buf(), line_no));
            }
            validate_js_cmd(manifests, doc_path, line_no, &parts, diagnostics, seen);
        }
        "make" if !manifests.make_targets.is_empty() => {
            let target = parts[1];
            if !target.starts_with('-')
                && !manifests.make_targets.contains(target)
                && seen.insert((doc_path.to_path_buf(), line_no, cmd.to_owned()))
            {
                diagnostics.push(Diagnostic::new(
                    Severity::Error,
                    CheckKind::BrokenCommand,
                    Some(doc_path.to_path_buf()),
                    Some(line_no),
                    format!("Makefile target `{target}` in `{cmd}` does not exist"),
                ));
            }
        }
        "just" if !manifests.just_recipes.is_empty() => {
            let recipe = parts[1];
            if !recipe.starts_with('-')
                && !manifests.just_recipes.contains(recipe)
                && seen.insert((doc_path.to_path_buf(), line_no, cmd.to_owned()))
            {
                diagnostics.push(Diagnostic::new(
                    Severity::Error,
                    CheckKind::BrokenCommand,
                    Some(doc_path.to_path_buf()),
                    Some(line_no),
                    format!("justfile recipe `{recipe}` in `{cmd}` does not exist"),
                ));
            }
        }
        _ => {}
    }
}

fn validate_cargo_cmd(
    manifests: &ProjectManifests,
    doc_path: &Path,
    line_no: usize,
    parts: &[&str],
    diagnostics: &mut Vec<Diagnostic>,
    seen: &mut BTreeSet<(PathBuf, usize, String)>,
) {
    let sub = parts[1].trim_start_matches('+');
    let sub = if sub == parts[1] {
        sub
    } else {
        parts.get(2).copied().unwrap_or("")
    };
    if !sub.is_empty()
        && !sub.starts_with('-')
        && !STANDARD_CARGO_SUBCOMMANDS.contains(&sub)
        && !manifests.cargo_aliases.contains(sub)
        && !manifests.cargo_binaries.contains(&format!("cargo-{sub}"))
        && !manifests.cargo_packages.contains(sub)
        && seen.insert((doc_path.to_path_buf(), line_no, parts.join(" ")))
    {
        diagnostics.push(Diagnostic::new(
            Severity::Error,
            CheckKind::BrokenCommand,
            Some(doc_path.to_path_buf()),
            Some(line_no),
            format!("cargo subcommand/alias `{sub}` was not found in Cargo config or workspace"),
        ));
    }

    let mut i = 1;
    while i < parts.len() {
        match parts[i] {
            "-p" | "--package" if i + 1 < parts.len() => {
                let pkg = parts[i + 1];
                if !manifests.cargo_packages.is_empty()
                    && !manifests.cargo_packages.contains(pkg)
                    && seen.insert((doc_path.to_path_buf(), line_no, format!("pkg:{pkg}")))
                {
                    diagnostics.push(Diagnostic::new(
                        Severity::Error,
                        CheckKind::BrokenCommand,
                        Some(doc_path.to_path_buf()),
                        Some(line_no),
                        format!("documented package `{pkg}` does not exist in Cargo workspace"),
                    ));
                }
                i += 2;
                continue;
            }
            "--features" | "-F" if i + 1 < parts.len() => {
                for feat in parts[i + 1].split([',', ' ']).filter(|s| !s.is_empty()) {
                    if !feat.contains('/')
                        && !manifests.cargo_features.is_empty()
                        && !manifests.cargo_features.contains(feat)
                        && seen.insert((doc_path.to_path_buf(), line_no, format!("feat:{feat}")))
                    {
                        diagnostics.push(Diagnostic::new(
                            Severity::Warning,
                            CheckKind::BrokenCommand,
                            Some(doc_path.to_path_buf()),
                            Some(line_no),
                            format!("feature `{feat}` was not found in workspace Cargo.toml files"),
                        ));
                    }
                }
                i += 2;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
}

fn validate_js_cmd(
    manifests: &ProjectManifests,
    doc_path: &Path,
    line_no: usize,
    parts: &[&str],
    diagnostics: &mut Vec<Diagnostic>,
    seen: &mut BTreeSet<(PathBuf, usize, String)>,
) {
    let script = match parts {
        [_, "run", s, ..] if !s.starts_with('-') => Some(*s),
        _ => None,
    };
    if let Some(script_name) = script
        && !manifests.npm_scripts.contains(script_name)
        && seen.insert((
            doc_path.to_path_buf(),
            line_no,
            format!("script:{script_name}"),
        ))
    {
        diagnostics.push(Diagnostic::new(
            Severity::Error,
            CheckKind::BrokenCommand,
            Some(doc_path.to_path_buf()),
            Some(line_no),
            format!("package.json script `{script_name}` does not exist"),
        ));
    }
}

fn check_js_manager_conflicts(
    manifests: &ProjectManifests,
    js_mentions: &BTreeMap<String, Vec<(PathBuf, usize)>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if js_mentions.len() > 1 {
        let summary: Vec<String> = js_mentions
            .iter()
            .filter_map(|(pm, locs)| {
                locs.first()
                    .map(|(f, l)| format!("{pm} ({}:{l})", f.display()))
            })
            .collect();
        diagnostics.push(
            Diagnostic::new(
                Severity::Warning,
                CheckKind::ConflictingRule,
                None,
                None,
                format!(
                    "AI instruction files prescribe conflicting JS package managers: {}",
                    summary.join(" vs ")
                ),
            )
            .with_suggestion("standardize on a single package manager matching the lockfile"),
        );
    }
    if !manifests.detected_js_managers.is_empty() {
        for (pm, locs) in js_mentions {
            if !manifests.detected_js_managers.contains(pm)
                && let Some((file, line)) = locs.first()
            {
                let lock_pms: Vec<_> = manifests.detected_js_managers.iter().cloned().collect();
                diagnostics.push(
                    Diagnostic::new(
                        Severity::Warning,
                        CheckKind::ConflictingRule,
                        Some(file.clone()),
                        Some(*line),
                        format!(
                            "instruction recommends `{pm}`, but repo lockfile/packageManager indicates {}",
                            lock_pms.join("/")
                        ),
                    ),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catches_broken_paths_line_ranges_commands_and_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn real_fn() {}\n").unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\"scripts\": {\"test\": \"vitest\"}}",
        )
        .unwrap();
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();

        std::fs::write(
            dir.path().join("AGENTS.md"),
            "See [lib](src/lib.rs#L1-L99) and `src/deleted.rs`.\nRun `cargo test -p ghost` and `pnpm run test`.\nDecl `fn removed_fn`.\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "Run `npm run missing`.\n").unwrap();

        let index = RepoIndex::build(dir.path()).unwrap();
        let manifests = ProjectManifests::discover(&index);
        let report = check_docs(&index, &manifests);

        assert!(!report.ok);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.kind == CheckKind::BrokenLineRange)
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.kind == CheckKind::MissingPath && d.message.contains("src/deleted.rs"))
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.kind == CheckKind::BrokenCommand && d.message.contains("ghost"))
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.kind == CheckKind::BrokenCommand && d.message.contains("missing"))
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.kind == CheckKind::MissingSymbol && d.message.contains("removed_fn"))
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.kind == CheckKind::ConflictingRule)
        );
    }

    #[test]
    fn fixes_shifted_symbol_line_links() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "// header\n// moved down\npub fn real_fn() {\n    let _ = 1;\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "Entry point: [`real_fn`](src/lib.rs#L1-L2).\n",
        )
        .unwrap();

        let index = RepoIndex::build(dir.path()).unwrap();
        let manifests = ProjectManifests::discover(&index);
        let fixed = check_docs_with_config(&index, &manifests, &Config::default(), true);
        assert!(fixed.ok, "{}", fixed.render_text());

        let updated = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert!(updated.contains("src/lib.rs#L3-L5"), "{updated}");
    }
}
