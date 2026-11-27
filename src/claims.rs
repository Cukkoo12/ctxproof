use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::Config;
use crate::docs::split_path_and_line_range;
use crate::model::{CheckKind, Diagnostic, Report, Severity};
use crate::symbols::{
    PathLookup, RepoIndex, clean_rel_str, expand_pipe_ext_shorthand, extract_decl_ident,
    strip_godot_res_prefix,
};
use crate::verify::inspect_receipts;

/// Verifies draft PR text, commit messages, or AI final answers against the live repo:
/// - Validates all relative & `file://` path links and `#L` line ranges
/// - Validates explicit symbol declarations (`fn foo`, `struct Bar`) and `Type::method`
///   references for types that exist in the repo
/// - Verifies that any cited passing commands have fresh, non-stale passing receipts
pub fn check_claims(index: &RepoIndex, claim_text: &str) -> Result<Report> {
    check_claims_with_config(index, &Config::default(), claim_text)
}

/// Variant of [`check_claims`] applying `.ctxproof.toml` ignore and disable rules.
pub fn check_claims_with_config(
    index: &RepoIndex,
    config: &Config,
    claim_text: &str,
) -> Result<Report> {
    let mut diagnostics = Vec::new();
    let mut seen = BTreeSet::new();

    for (idx, line) in claim_text.lines().enumerate() {
        let line_no = idx + 1;
        let linked_files =
            check_markdown_targets(index, line_no, line, &mut diagnostics, &mut seen);
        check_backtick_claims(
            index,
            line_no,
            line,
            &linked_files,
            &mut diagnostics,
            &mut seen,
        );
    }

    check_claimed_receipts(index, claim_text, &mut diagnostics)?;
    let diagnostics = config.filter_diagnostics(diagnostics);

    let errors = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();
    let summary = format!("checked claims text ({errors} errors, {warnings} warnings)");
    Ok(Report::from_diagnostics(
        "check-claims",
        index.root.clone(),
        summary,
        diagnostics,
    ))
}

fn check_markdown_targets(
    index: &RepoIndex,
    line_no: usize,
    line: &str,
    diagnostics: &mut Vec<Diagnostic>,
    seen: &mut BTreeSet<String>,
) -> Vec<String> {
    let mut indexed_files = Vec::new();
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
                let raw = line[i + 2..j].trim();
                let target = raw.split_whitespace().next().unwrap_or("");
                if let Some(rel) = to_repo_relative(index, target) {
                    let (base, _) = split_path_and_line_range(&rel);
                    if let Some(meta) = index.resolve_file(base) {
                        let norm = meta.path.to_string_lossy().replace('\\', "/");
                        if index.symbols_by_file.contains_key(&norm)
                            && !indexed_files.contains(&norm)
                        {
                            indexed_files.push(norm);
                        }
                    }
                    validate_repo_target(index, line_no, &rel, diagnostics, seen);
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    indexed_files
}

fn check_backtick_claims(
    index: &RepoIndex,
    line_no: usize,
    line: &str,
    linked_files: &[String],
    diagnostics: &mut Vec<Diagnostic>,
    seen: &mut BTreeSet<String>,
) {
    let mut line_files: Vec<String> = linked_files.to_vec();
    let mut backticks = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else { break };
        let token = after[..end].trim();
        if !token.is_empty() {
            backticks.push(token.to_owned());
        }
        rest = &after[end + 1..];
    }

    for token in &backticks {
        if let Some(rel) = to_repo_relative(index, token)
            && looks_like_file_token(&rel)
        {
            let (base, _) = split_path_and_line_range(&rel);
            if let Some(meta) = index.resolve_file(base) {
                let norm = meta.path.to_string_lossy().replace('\\', "/");
                if index.symbols_by_file.contains_key(&norm) && !line_files.contains(&norm) {
                    line_files.push(norm);
                }
            }
            validate_repo_target(index, line_no, &rel, diagnostics, seen);
        }
    }

    for token in &backticks {
        if let Some((_, ident)) = extract_decl_ident(token) {
            if index.find_symbol(ident, None).is_empty() && seen.insert(format!("sym:{token}")) {
                diagnostics.push(Diagnostic::new(
                    Severity::Error,
                    CheckKind::UnverifiedClaim,
                    None,
                    Some(line_no),
                    format!("claimed declaration `{token}` does not exist in indexed AST"),
                ));
            }
            continue;
        }

        // Check `Type::method` or `module::symbol` when the prefix is a repo symbol or module.
        if let Some((ty, _)) = token.split_once("::")
            && !matches!(ty, "std" | "core" | "alloc" | "crate" | "super" | "self")
            && !token.contains(' ')
            && !token.contains('/')
            && !token.ends_with("::*")
            && !index.find_symbol(ty, None).is_empty()
        {
            let clean_sym = token.split(['(', '<']).next().unwrap_or(token).trim();
            if index.find_qualified_symbol(clean_sym).is_empty()
                && seen.insert(format!("qual:{clean_sym}"))
            {
                diagnostics.push(Diagnostic::new(
                    Severity::Error,
                    CheckKind::UnverifiedClaim,
                    None,
                    Some(line_no),
                    format!(
                        "type or module `{ty}` exists in repo, but qualified symbol `{clean_sym}` was not found"
                    ),
                ));
            }
            continue;
        }

        // Check `Type.Method` (C#, Go, Python, TS/JS) when the receiver/type exists in this repo.
        if !token.contains(' ')
            && !token.contains('/')
            && !token.contains("::")
            && !looks_like_file_token(token)
            && let Some((ty, _)) = token.split_once('.')
            && is_plain_ident(ty)
            && !index.find_symbol(ty, None).is_empty()
        {
            let clean_sym = token.split(['(', '<']).next().unwrap_or(token).trim();
            if index.find_qualified_symbol(clean_sym).is_empty()
                && seen.insert(format!("dot:{clean_sym}"))
            {
                diagnostics.push(Diagnostic::new(
                    Severity::Error,
                    CheckKind::UnverifiedClaim,
                    None,
                    Some(line_no),
                    format!(
                        "type `{ty}` exists in repo, but qualified symbol `{clean_sym}` was not found"
                    ),
                ));
            }
            continue;
        }

        // Check `func()` when the same line cites exactly one source file in this repo.
        if line_files.len() == 1
            && let Some(fn_name) = token.strip_suffix("()")
            && is_plain_ident(fn_name)
        {
            let file_hint = &line_files[0];
            if index.symbols_by_file.contains_key(file_hint)
                && index.find_symbol(fn_name, Some(file_hint)).is_empty()
                && seen.insert(format!("file-sym:{file_hint}:{fn_name}"))
            {
                diagnostics.push(Diagnostic::new(
                    Severity::Error,
                    CheckKind::UnverifiedClaim,
                    Some(PathBuf::from(file_hint)),
                    Some(line_no),
                    format!("symbol `{fn_name}` was not found inside `{file_hint}`"),
                ));
            }
        }
    }
}

fn check_claimed_receipts(
    index: &RepoIndex,
    claim_text: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<()> {
    let receipts = inspect_receipts(&index.root)?;
    if receipts.is_empty() {
        return Ok(());
    }

    for (receipt, fresh) in receipts {
        let cmd_str = receipt.command.join(" ");
        if !claim_text.contains(&cmd_str) {
            continue;
        }
        if !receipt.ok {
            diagnostics.push(Diagnostic::new(
                Severity::Error,
                CheckKind::UnverifiedClaim,
                None,
                None,
                format!(
                    "claims text cites `{cmd_str}`, whose latest recorded run FAILED (exit {})",
                    receipt.exit_code
                ),
            ));
        } else if !fresh {
            diagnostics.push(
                Diagnostic::new(
                    Severity::Error,
                    CheckKind::StaleReceipt,
                    None,
                    None,
                    format!(
                        "claims text cites `{cmd_str}`, but repository files changed after that receipt was recorded"
                    ),
                )
                .with_suggestion(format!("re-run `ctxproof verify -- {cmd_str}`")),
            );
        }
    }
    Ok(())
}

fn to_repo_relative(index: &RepoIndex, target: &str) -> Option<String> {
    let trimmed = strip_godot_res_prefix(target.trim(), index.projects.godot);
    if trimmed.is_empty()
        || trimmed.starts_with('#')
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
    {
        return None;
    }
    if let Some(after_scheme) = trimmed.strip_prefix("file://") {
        let fs_path = after_scheme.strip_prefix('/').unwrap_or(after_scheme);
        let (base_part, line_suffix) = split_path_and_line_range(fs_path);
        let candidate = Path::new(base_part);
        let canon_cand = std::fs::canonicalize(candidate).ok();
        let rel_path = canon_cand
            .as_deref()
            .and_then(|c| c.strip_prefix(&index.root).ok())
            .or_else(|| candidate.strip_prefix(&index.root).ok())?;
        let norm = rel_path.to_string_lossy().replace('\\', "/");
        return Some(match line_suffix {
            Some((s, e)) => format!("{norm}#L{s}-L{e}"),
            None => norm,
        });
    }
    Some(trimmed.to_owned())
}

fn looks_like_file_token(token: &str) -> bool {
    if token.contains(' ') || token.contains("::") {
        return false;
    }
    let (base, line_range) = split_path_and_line_range(token);
    if line_range.is_some() {
        return base.contains('.') || base.contains('/');
    }
    base.contains('/') && !base.starts_with('-')
}

fn validate_repo_target(
    index: &RepoIndex,
    line_no: usize,
    rel: &str,
    diagnostics: &mut Vec<Diagnostic>,
    seen: &mut BTreeSet<String>,
) {
    let (path_part, line_range) = split_path_and_line_range(rel);
    if line_range.is_none()
        && let Some(expanded) = expand_pipe_ext_shorthand(path_part)
    {
        for single in expanded {
            validate_repo_target(index, line_no, &single, diagnostics, seen);
        }
        return;
    }
    if clean_rel_str(path_part).is_none() {
        return;
    }
    match index.path_exists(path_part) {
        PathLookup::Missing => {
            if seen.insert(format!("path:{path_part}")) {
                diagnostics.push(Diagnostic::new(
                    Severity::Error,
                    CheckKind::MissingPath,
                    None,
                    Some(line_no),
                    format!("claimed path `{path_part}` does not exist in repository"),
                ));
            }
        }
        PathLookup::Directory => {
            if line_range.is_some() && seen.insert(format!("range:{rel}")) {
                diagnostics.push(Diagnostic::new(
                    Severity::Error,
                    CheckKind::BrokenLineRange,
                    None,
                    Some(line_no),
                    format!("claimed line range targets directory `{path_part}`"),
                ));
            }
        }
        PathLookup::File {
            normalized,
            line_count,
        } => {
            if let Some((start, end)) = line_range
                && (start == 0 || end < start || end > line_count)
                && seen.insert(format!("range:{normalized}:{start}:{end}"))
            {
                diagnostics.push(
                    Diagnostic::new(
                        Severity::Error,
                        CheckKind::BrokenLineRange,
                        Some(PathBuf::from(&normalized)),
                        Some(line_no),
                        format!(
                            "claimed line range L{start}-L{end} exceeds `{normalized}` ({line_count} lines)"
                        ),
                    )
                    .with_suggestion(format!("valid range is 1..={line_count}")),
                );
            }
        }
    }
}

fn is_plain_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::run_verified;

    #[test]
    fn catches_hallucinated_paths_lines_symbols_and_stale_test_claims() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/engine.rs"),
            "pub struct Engine {}\nimpl Engine {\n    pub fn boot(&self) {}\n}\n",
        )
        .unwrap();

        let cmd = if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), "echo ok".into()]
        } else {
            vec!["sh".into(), "-c".into(), "echo ok".into()]
        };
        let cmd_str = cmd.join(" ");
        let _ = run_verified(dir.path(), &cmd).unwrap();
        // Edit file AFTER verification so the receipt becomes stale.
        std::fs::write(
            dir.path().join("src/engine.rs"),
            "pub struct Engine {}\nimpl Engine {\n    pub fn boot(&self) {}\n}\n// edited\n",
        )
        .unwrap();

        let index = RepoIndex::build(dir.path()).unwrap();
        let draft = format!(
            "Updated `Engine::missing_fn` and `phantom()` in `src/engine.rs`, [link](src/engine.rs#L1-L99), `src/ghost.rs`.\nRan `{cmd_str}` and passed."
        );
        let report = check_claims(&index, &draft).unwrap();
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
                .any(|d| d.kind == CheckKind::MissingPath)
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.kind == CheckKind::UnverifiedClaim
                    && d.message.contains("Engine::missing_fn"))
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.kind == CheckKind::UnverifiedClaim && d.message.contains("phantom"))
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.kind == CheckKind::StaleReceipt)
        );
    }

    #[test]
    fn resolves_rust_module_qualified_symbols_and_flags_mismatches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub mod logging;\n").unwrap();
        std::fs::write(
            dir.path().join("src/logging.rs"),
            "pub fn init() {}\npub fn init_stderr_only() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn sample_test() {}\n}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/other.rs"), "pub fn else_where() {}\n").unwrap();

        let index = RepoIndex::build(dir.path()).unwrap();
        let valid =
            "Updated `logging::init`, `logging::tests::sample_test`, and `logging::tests::*`.";
        let valid_report = check_claims(&index, valid).unwrap();
        assert!(valid_report.ok, "{}", valid_report.render_text());

        let bogus = "Called `logging::else_where`.";
        let bogus_report = check_claims(&index, bogus).unwrap();
        assert!(!bogus_report.ok);
    }

    #[test]
    fn validates_unity_csharp_dot_qualified_symbols_and_skips_library_cache() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Assets/Scripts")).unwrap();
        std::fs::create_dir_all(dir.path().join("ProjectSettings")).unwrap();
        std::fs::create_dir_all(dir.path().join("Library/PackageCache")).unwrap();
        std::fs::write(
            dir.path().join("Library/PackageCache/Generated.cs"),
            "public class CachedShouldNotIndex {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("Assets/Scripts/PlayerController.cs"),
            "namespace Game {\n    public class PlayerController {\n        public void Move() {}\n    }\n}\n",
        )
        .unwrap();

        let index = RepoIndex::build(dir.path()).unwrap();
        assert!(index.find_symbol("CachedShouldNotIndex", None).is_empty());

        let valid = check_claims(
            &index,
            "Updated `PlayerController.Move` in `Assets/Scripts/PlayerController.cs#L2-L4`.",
        )
        .unwrap();
        assert!(valid.ok, "{}", valid.render_text());

        let invalid = check_claims(&index, "Called `PlayerController.Fly`.").unwrap();
        assert!(!invalid.ok);
    }

    #[test]
    fn validates_godot_gdscript_symbols_res_paths_and_skips_cache() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("project.godot"), "[application]\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".godot/imported")).unwrap();
        std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
        std::fs::create_dir_all(dir.path().join("scenes")).unwrap();
        std::fs::write(
            dir.path().join(".godot/imported/Hidden.gd"),
            "class_name HiddenGodotCache\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("scripts/kapi.gd"),
            "class_name Kapi\nsignal toggled(state)\nfunc open_door() -> void:\n\tpass\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("scenes/kapi.tscn"), "[gd_scene format=3]\n").unwrap();

        let index = RepoIndex::build(dir.path()).unwrap();
        assert!(index.find_symbol("HiddenGodotCache", None).is_empty());

        let valid = check_claims(
            &index,
            "Updated `Kapi.open_door` and `signal toggled` in `res://scripts/kapi.gd#L1-L4` and [scene](res://scenes/kapi.tscn).",
        )
        .unwrap();
        assert!(valid.ok, "{}", valid.render_text());

        let invalid = check_claims(
            &index,
            "Updated `Kapi.fly` and `signal bogus_sig` in `res://scripts/missing.gd`.",
        )
        .unwrap();
        assert!(!invalid.ok);
        let rendered = invalid.render_text();
        assert!(rendered.contains("Kapi.fly"));
        assert!(rendered.contains("bogus_sig"));
        assert!(rendered.contains("scripts/missing.gd"));

        std::fs::write(dir.path().join("scenes/kapi.gd"), "extends Node2D\n").unwrap();
        let valid_pair = check_claims(
            &RepoIndex::build(dir.path()).unwrap(),
            "Updated `scenes/kapi.tscn|.gd` and `res://scenes/kapi.tscn|.gd`.",
        )
        .unwrap();
        assert!(valid_pair.ok, "{}", valid_pair.render_text());
    }
}
