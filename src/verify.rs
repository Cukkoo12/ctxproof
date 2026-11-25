use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};

use crate::model::{CheckKind, Diagnostic, Report, Severity};
use crate::symbols::normalize_rel;

const RECEIPT_DIR: &str = ".ctxproof";
const RECEIPT_FILE: &str = "receipts.json";

/// Stored verification proof tied to the repository's exact file-tree fingerprint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReceipt {
    pub command: Vec<String>,
    pub exit_code: i32,
    pub ok: bool,
    pub fingerprint: String,
    pub unix_seconds: u64,
    pub duration_ms: u64,
    pub compact_output: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ReceiptStore {
    receipts: BTreeMap<String, VerificationReceipt>,
}

/// Computes a deterministic `blake3` fingerprint over all non-ignored files in `root`.
pub fn compute_tree_fingerprint(root: &Path) -> Result<String> {
    let canon_root = std::fs::canonicalize(root)
        .with_context(|| format!("canonicalizing {}", root.display()))?;
    let mut paths = Vec::new();
    let walker = WalkBuilder::new(&canon_root)
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

    for item in walker {
        let Ok(entry) = item else { continue };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(&canon_root) {
            paths.push((normalize_rel(rel), entry.path().to_path_buf()));
        }
    }
    paths.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    for (rel, abs) in paths {
        let Ok(bytes) = std::fs::read(&abs) else {
            continue;
        };
        hasher.update(rel.as_bytes());
        hasher.update(&[0]);
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(blake3::hash(&bytes).as_bytes());
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Runs a verification command, distills noisy stdout/stderr for AI context,
/// and records a freshness receipt under `.ctxproof/receipts.json`.
pub fn run_verified(root: &Path, command: &[String]) -> Result<(Report, VerificationReceipt)> {
    anyhow::ensure!(!command.is_empty(), "command cannot be empty");

    let start = Instant::now();
    let output = spawn_command(root, command)
        .with_context(|| format!("executing verification command `{}`", command.join(" ")))?;
    let duration_ms = start.elapsed().as_millis() as u64;
    let exit_code = output.status.code().unwrap_or(-1);
    let ok = output.status.success();

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let compact_output = distill_command_output(&combined, ok, 40);
    let fingerprint = compute_tree_fingerprint(root)?;
    let unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let receipt = VerificationReceipt {
        command: command.to_vec(),
        exit_code,
        ok,
        fingerprint,
        unix_seconds,
        duration_ms,
        compact_output: compact_output.clone(),
    };
    save_receipt(root, &receipt)?;

    let mut diagnostics = Vec::new();
    if !ok {
        diagnostics.push(
            Diagnostic::new(
                Severity::Error,
                CheckKind::ProcessError,
                None,
                None,
                format!(
                    "command `{}` exited with code {exit_code}:\n{compact_output}",
                    command.join(" ")
                ),
            )
            .with_suggestion("fix the reported errors and re-run `ctxproof verify`"),
        );
    }
    let summary = if ok {
        format!(
            "`{}` passed in {duration_ms} ms (receipt recorded)",
            command.join(" ")
        )
    } else {
        format!("`{}` failed with exit code {exit_code}", command.join(" "))
    };
    Ok((
        Report::from_diagnostics("verify", root.to_path_buf(), summary, diagnostics),
        receipt,
    ))
}

fn spawn_command(root: &Path, command: &[String]) -> std::io::Result<std::process::Output> {
    if command.len() == 1 && command[0].contains(' ') {
        if cfg!(windows) {
            Command::new("cmd")
                .args(["/C", &command[0]])
                .current_dir(root)
                .output()
        } else {
            Command::new("sh")
                .args(["-c", &command[0]])
                .current_dir(root)
                .output()
        }
    } else {
        Command::new(&command[0])
            .args(&command[1..])
            .current_dir(root)
            .output()
    }
}

/// Checks whether stored receipts match the current repository fingerprint.
pub fn inspect_receipts(root: &Path) -> Result<Vec<(VerificationReceipt, bool)>> {
    let store = load_store(root)?;
    if store.receipts.is_empty() {
        return Ok(Vec::new());
    }
    let current = compute_tree_fingerprint(root)?;
    Ok(store
        .receipts
        .into_values()
        .map(|r| {
            let fresh = r.fingerprint == current;
            (r, fresh)
        })
        .collect())
}

/// Compresses noisy test/compiler logs into only errors, failures, panics, and summary lines.
#[must_use]
pub fn distill_command_output(raw: &str, succeeded: bool, max_lines: usize) -> String {
    let lines: Vec<&str> = raw
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return String::new();
    }

    let mut picked = Vec::new();
    for line in &lines {
        let lower = line.to_ascii_lowercase();
        let is_signal = if succeeded {
            lower.contains("test result:")
                || lower.starts_with("passed")
                || lower.starts_with("warning:")
                || lower.contains("files ok")
        } else {
            lower.contains("error")
                || lower.contains("failed")
                || lower.contains("panicked at")
                || lower.contains("assertion")
                || lower.contains("test result:")
                || line.trim_start().starts_with("-->")
                || line.trim_start().starts_with("left:")
                || line.trim_start().starts_with("right:")
        };
        if is_signal {
            picked.push(*line);
        }
    }

    if picked.is_empty() {
        let start = lines.len().saturating_sub(max_lines.min(12));
        return lines[start..].join("\n");
    }
    if picked.len() > max_lines {
        let omitted = picked.len() - max_lines;
        let mut head: Vec<String> = picked[..max_lines]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        head.push(format!("... ({omitted} more signal lines omitted)"));
        head.join("\n")
    } else {
        picked.join("\n")
    }
}

fn receipt_path(root: &Path) -> PathBuf {
    root.join(RECEIPT_DIR).join(RECEIPT_FILE)
}

fn load_store(root: &Path) -> Result<ReceiptStore> {
    let path = receipt_path(root);
    if !path.is_file() {
        return Ok(ReceiptStore::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading receipts from {}", path.display()))?;
    Ok(serde_json::from_str(&text).unwrap_or_default())
}

fn save_receipt(root: &Path, receipt: &VerificationReceipt) -> Result<()> {
    let dir = root.join(RECEIPT_DIR);
    std::fs::create_dir_all(&dir)?;
    let gi = dir.join(".gitignore");
    if !gi.exists() {
        let _ = std::fs::write(&gi, "*\n");
    }

    let mut store = load_store(root)?;
    store
        .receipts
        .insert(receipt.command.join(" "), receipt.clone());
    let json = serde_json::to_string_pretty(&store)?;
    std::fs::write(dir.join(RECEIPT_FILE), json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_becomes_stale_when_a_source_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("lib.rs");
        std::fs::write(&src, "pub fn a() -> i32 { 1 }\n").unwrap();

        let cmd = if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), "echo ok".into()]
        } else {
            vec!["sh".into(), "-c".into(), "echo ok".into()]
        };
        let (report, _) = run_verified(dir.path(), &cmd).unwrap();
        assert!(report.ok);

        let before = inspect_receipts(dir.path()).unwrap();
        assert_eq!(before.len(), 1);
        assert!(before[0].1, "receipt must be fresh before editing files");

        std::fs::write(&src, "pub fn a() -> i32 { 2 }\n").unwrap();
        let after = inspect_receipts(dir.path()).unwrap();
        assert!(!after[0].1, "receipt must become stale after file change");
    }

    #[test]
    fn distills_noisy_test_output_to_signal_lines() {
        let noisy = "\
test a ... ok
test b ... ok
thread 'c' panicked at src/lib.rs:42:5:
assertion `left == right` failed
  left: 1
 right: 2
test c ... FAILED
test result: FAILED. 2 passed; 1 failed; 0 ignored
";
        let distilled = distill_command_output(noisy, false, 20);
        assert!(!distilled.contains("test a ... ok"));
        assert!(distilled.contains("panicked at src/lib.rs:42:5"));
        assert!(distilled.contains("left: 1"));
        assert!(distilled.contains("test result: FAILED"));
    }
}
