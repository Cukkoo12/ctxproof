use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::model::{CheckKind, Diagnostic, Report, Severity};

/// Parsed hunk edits and untracked additions from Git.
#[derive(Debug, Default, Clone)]
pub struct DiffSummary {
    pub changed_ranges: BTreeMap<String, Vec<RangeInclusive<usize>>>,
    pub added_lines: Vec<(String, usize, String)>,
    pub untracked_files: BTreeSet<String>,
}

/// Checks `git diff` + untracked files strictly on changed/new lines:
/// - Diff-scoped `rustfmt --check` (ignores untouched pre-existing formatting drift)
/// - Leftover debug calls (`dbg!`, `console.log`, `breakpoint()`, etc.)
/// - Secret/token patterns
pub fn check_diff(root: &Path) -> Result<Report> {
    let summary = collect_git_changes(root)?;
    let mut diagnostics = Vec::new();

    if summary.changed_ranges.is_empty() && summary.untracked_files.is_empty() {
        return Ok(Report::from_diagnostics(
            "diff",
            root.to_path_buf(),
            "working tree has no uncommitted diff or untracked files",
            diagnostics,
        ));
    }

    scan_added_lines(&summary, &mut diagnostics);
    let skipped_untouched = check_rustfmt_on_changed(root, &summary, &mut diagnostics);

    let errors = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();
    let touched_files = summary
        .changed_ranges
        .keys()
        .chain(summary.untracked_files.iter())
        .collect::<BTreeSet<_>>()
        .len();
    let msg = if skipped_untouched > 0 {
        format!(
            "checked {touched_files} changed files ({errors} errors, {warnings} warnings; ignored {skipped_untouched} untouched rustfmt hunks)"
        )
    } else {
        format!("checked {touched_files} changed files ({errors} errors, {warnings} warnings)")
    };

    Ok(Report::from_diagnostics(
        "diff",
        root.to_path_buf(),
        msg,
        diagnostics,
    ))
}

/// Collects modified ranges and added lines from `git diff` (staged + unstaged)
/// plus `git status --porcelain` untracked files.
pub fn collect_git_changes(root: &Path) -> Result<DiffSummary> {
    let mut out = DiffSummary::default();

    for args in [&["diff", "-U0"][..], &["diff", "--cached", "-U0"][..]] {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .with_context(|| format!("running git {:?}", args))?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git {:?} failed: {}", args, err.trim());
        }
        parse_unified_diff(&String::from_utf8_lossy(&output.stdout), &mut out);
    }

    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()
        .context("running git status --porcelain")?;
    if status.status.success() {
        for line in String::from_utf8_lossy(&status.stdout).lines() {
            if let Some(raw) = line.strip_prefix("?? ") {
                let norm = raw.trim().trim_matches('"').replace('\\', "/");
                if norm.is_empty() || norm.starts_with(".ctxproof/") {
                    continue;
                }
                let abs = root.join(&norm);
                if abs.is_file() {
                    out.untracked_files.insert(norm.clone());
                    if let Ok(text) = std::fs::read_to_string(&abs) {
                        for (idx, content) in text.lines().enumerate() {
                            out.added_lines
                                .push((norm.clone(), idx + 1, content.to_owned()));
                        }
                    }
                }
            }
        }
    }

    Ok(out)
}

/// Parses `git diff -U0` output into per-file changed line ranges and added lines.
pub fn parse_unified_diff(diff: &str, out: &mut DiffSummary) {
    let mut current_file: Option<String> = None;
    let mut next_new_line = 0usize;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            let norm = rest.trim().replace('\\', "/");
            current_file = Some(norm);
            continue;
        }
        if line.starts_with("+++ /dev/null") {
            current_file = None;
            continue;
        }
        if line.starts_with("@@ ")
            && let Some(file) = &current_file
            && let Some((start, count)) = parse_hunk_new_span(line)
        {
            next_new_line = start;
            if count > 0 {
                let end = start + count - 1;
                out.changed_ranges
                    .entry(file.clone())
                    .or_default()
                    .push(start..=end);
            }
            continue;
        }
        if let Some(file) = &current_file
            && let Some(added) = line.strip_prefix('+')
            && !line.starts_with("+++")
        {
            out.added_lines
                .push((file.clone(), next_new_line, added.to_owned()));
            next_new_line += 1;
        } else if line.starts_with(' ') {
            next_new_line += 1;
        }
    }
}

fn parse_hunk_new_span(header: &str) -> Option<(usize, usize)> {
    let plus = header.split_once(" +")?.1;
    let span = plus.split(" @@").next()?.trim();
    if let Some((start_s, count_s)) = span.split_once(',') {
        Some((start_s.parse().ok()?, count_s.parse().ok()?))
    } else {
        Some((span.parse().ok()?, 1))
    }
}

fn scan_added_lines(summary: &DiffSummary, diagnostics: &mut Vec<Diagnostic>) {
    for (file, line_no, text) in &summary.added_lines {
        let lower = text.to_ascii_lowercase();
        if let Some(secret_kind) = detect_secret_token(text) {
            diagnostics.push(
                Diagnostic::new(
                    Severity::Error,
                    CheckKind::SecretLeak,
                    Some(PathBuf::from(file)),
                    Some(*line_no),
                    format!("potential {secret_kind} found in added line"),
                )
                .with_suggestion(
                    "remove credentials and use environment variables or secret stores",
                ),
            );
        }

        if !is_source_code_file(file) {
            continue;
        }

        let trimmed = text.trim();
        let is_comment = trimmed.starts_with("//") || trimmed.starts_with('#');
        if !is_comment {
            if trimmed.contains("dbg!(")
                || trimmed.contains("console.log(")
                || trimmed.contains("breakpoint()")
                || trimmed.contains("pdb.set_trace()")
            {
                diagnostics.push(Diagnostic::new(
                    Severity::Warning,
                    CheckKind::LeftoverDebug,
                    Some(PathBuf::from(file)),
                    Some(*line_no),
                    "possible leftover debug statement in added code",
                ));
            }
        } else if lower.contains("todo(agent)") || lower.contains("fixme(ai)") {
            diagnostics.push(Diagnostic::new(
                Severity::Warning,
                CheckKind::LeftoverDebug,
                Some(PathBuf::from(file)),
                Some(*line_no),
                "leftover AI placeholder marker in comment",
            ));
        }
    }
}

fn is_source_code_file(rel: &str) -> bool {
    Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "rs" | "py"
                    | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "mjs"
                    | "cjs"
                    | "go"
                    | "c"
                    | "cpp"
                    | "cc"
                    | "h"
                    | "hpp"
                    | "java"
            )
        })
}

fn detect_secret_token(line: &str) -> Option<&'static str> {
    if line.contains("-----BEGIN ") && line.contains("PRIVATE KEY-----") {
        return Some("private key header");
    }
    for (prefix, min_len, label) in [
        ("ghp_", 20, "GitHub personal access token"),
        ("github_pat_", 24, "GitHub fine-grained token"),
        ("AKIA", 20, "AWS access key ID"),
        ("AIzaSy", 30, "Google API key"),
        ("sk-proj-", 24, "OpenAI project API key"),
        ("sk-ant-", 24, "Anthropic API key"),
    ] {
        if let Some(pos) = line.find(prefix) {
            let token: String = line[pos..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                .collect();
            if token.len() >= min_len {
                return Some(label);
            }
        }
    }
    None
}

fn check_rustfmt_on_changed(
    root: &Path,
    summary: &DiffSummary,
    diagnostics: &mut Vec<Diagnostic>,
) -> usize {
    let rust_files: Vec<String> = summary
        .changed_ranges
        .keys()
        .chain(summary.untracked_files.iter())
        .filter(|p| p.ends_with(".rs"))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if rust_files.is_empty() {
        return 0;
    }

    let edition = detect_rust_edition(root).unwrap_or_else(|| "2021".into());
    let mut cmd = Command::new("rustfmt");
    cmd.arg("--edition")
        .arg(&edition)
        .arg("--check")
        .current_dir(root);
    for rel in &rust_files {
        cmd.arg(rel);
    }
    let Ok(output) = cmd.output() else {
        return 0;
    };
    if output.status.success() {
        return 0;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let hunks = parse_rustfmt_diffs(&stdout, root);
    let mut untouched_skipped = 0usize;

    for (file_rel, start_line, end_line) in hunks {
        if summary.untracked_files.contains(&file_rel) {
            diagnostics.push(
                Diagnostic::new(
                    Severity::Error,
                    CheckKind::DiffFormatting,
                    Some(PathBuf::from(&file_rel)),
                    Some(start_line),
                    format!("rustfmt differs in new file at lines {start_line}-{end_line}"),
                )
                .with_suggestion(format!("run `rustfmt --edition {edition} {file_rel}`")),
            );
            continue;
        }
        let overlaps = summary.changed_ranges.get(&file_rel).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|r| *r.start() <= end_line && start_line <= *r.end())
        });
        if overlaps {
            diagnostics.push(
                Diagnostic::new(
                    Severity::Error,
                    CheckKind::DiffFormatting,
                    Some(PathBuf::from(&file_rel)),
                    Some(start_line),
                    format!(
                        "rustfmt differs inside changed hunk around lines {start_line}-{end_line}"
                    ),
                )
                .with_suggestion("format the edited lines cleanly without touching unrelated code"),
            );
        } else {
            untouched_skipped += 1;
        }
    }

    untouched_skipped
}

fn detect_rust_edition(root: &Path) -> Option<String> {
    let cargo = std::fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let doc = toml::from_str::<toml::Value>(&cargo).ok()?;
    for table in [
        doc.get("package"),
        doc.get("workspace").and_then(|w| w.get("package")),
    ] {
        if let Some(ed) = table
            .and_then(|t| t.get("edition"))
            .and_then(|v| v.as_str())
        {
            return Some(ed.to_owned());
        }
    }
    None
}

/// Parses `Diff in <abs_path>:<line>:` blocks emitted by `rustfmt --check`,
/// emitting only actual dirty or inserted line ranges and ignoring unchanged context lines.
pub fn parse_rustfmt_diffs(stdout: &str, root: &Path) -> Vec<(String, usize, usize)> {
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut results = Vec::new();
    let mut current_file: Option<String> = None;
    let mut next_orig = 0usize;
    let mut active_span: Option<(usize, usize)> = None;
    let mut saw_minus_in_block = false;

    for raw_line in stdout.lines() {
        let stripped = strip_ansi(raw_line);
        let line = stripped.as_str();
        if let Some(rest) = line.strip_prefix("Diff in ") {
            if let (Some(file), Some((s, e))) = (current_file.as_ref(), active_span.take()) {
                results.push((file.clone(), s, e));
            }
            current_file = None;
            saw_minus_in_block = false;
            let cleaned = rest.trim_end_matches(':');
            let cleaned = cleaned.strip_prefix(r"\\?\").unwrap_or(cleaned);
            if let Some((path_part, line_part)) = cleaned.rsplit_once(':')
                && let Ok(start) = line_part.parse::<usize>()
            {
                current_file = Some(make_relative_norm(path_part, root, &canon_root));
                next_orig = start;
            }
            continue;
        }
        let Some(file) = current_file.as_ref() else {
            continue;
        };
        if line.starts_with('-') {
            saw_minus_in_block = true;
            active_span = match active_span {
                Some((s, _)) => Some((s, next_orig)),
                None => Some((next_orig, next_orig)),
            };
            next_orig += 1;
        } else if line.starts_with('+') {
            if !saw_minus_in_block {
                let anchor = next_orig.saturating_sub(1).max(1);
                active_span = match active_span {
                    Some((s, _)) => Some((s, next_orig)),
                    None => Some((anchor, next_orig)),
                };
            }
        } else if line.starts_with(' ') {
            if let Some((s, e)) = active_span.take() {
                results.push((file.clone(), s, e));
            }
            saw_minus_in_block = false;
            next_orig += 1;
        } else {
            if let Some((s, e)) = active_span.take() {
                results.push((file.clone(), s, e));
            }
            saw_minus_in_block = false;
        }
    }
    if let (Some(file), Some((s, e))) = (current_file, active_span) {
        results.push((file, s, e));
    }
    results
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn make_relative_norm(raw_path: &str, root: &Path, canon_root: &Path) -> String {
    let p = Path::new(raw_path);
    let canon_p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    for base in [canon_root, root] {
        if let Ok(rel) = canon_p.strip_prefix(base) {
            return rel.to_string_lossy().replace('\\', "/");
        }
        if let Ok(rel) = p.strip_prefix(base) {
            return rel.to_string_lossy().replace('\\', "/");
        }
    }
    raw_path.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_rustfmt_hunks_to_only_changed_diff_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let file = src_dir.join("lib.rs");
        std::fs::write(&file, "").unwrap();

        let mut summary = DiffSummary::default();
        parse_unified_diff(
            "+++ b/src/lib.rs\n@@ -20,0 +20,3 @@\n+fn bad(  x: i32) {\n+    dbg!(x);\n+}\n",
            &mut summary,
        );
        assert_eq!(summary.changed_ranges["src/lib.rs"], vec![20..=22]);

        let rustfmt_out = format!(
            "Diff in {}:{start}:\n-fn untouched( x: i32) {{}}\n+fn untouched(x: i32) {{}}\n unchanged_line_6\n unchanged_line_7\nDiff in {}:{bad}:\n-fn bad(  x: i32) {{\n+fn bad(x: i32) {{\n",
            file.display(),
            file.display(),
            start = 5,
            bad = 20,
        );
        let hunks = parse_rustfmt_diffs(&rustfmt_out, dir.path());
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0], ("src/lib.rs".into(), 5, 5));
        assert_eq!(hunks[1], ("src/lib.rs".into(), 20, 20));

        // Secret scanner sanity check without literal prefix matching itself.
        let synthetic_tok = format!("{}{}", "ghp_", "abcdefghijklmnopqrst123456");
        assert!(detect_secret_token(&synthetic_tok).is_some());
    }

    #[test]
    fn check_diff_ignores_untouched_rustfmt_debt_with_real_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for args in [
            &["init"][..],
            &["config", "--local", "user.name", "Test"][..],
            &["config", "--local", "user.email", "t@example.com"][..],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(root)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"s\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn   untouched_bad_fmt(  ) -> i32 { 1 }\npub fn clean_fn() -> i32 { 2 }\n",
        )
        .unwrap();
        assert!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "init"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );

        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn   untouched_bad_fmt(  ) -> i32 { 1 }\npub fn clean_fn() -> i32 { 2 }\npub fn added_fn() -> i32 {\n    3\n}\n",
        )
        .unwrap();

        let report = check_diff(root).unwrap();
        assert!(report.ok, "{}", report.render_text());
        assert!(
            report.summary.contains("ignored 1 untouched rustfmt hunks"),
            "{}",
            report.summary
        );
    }
}
