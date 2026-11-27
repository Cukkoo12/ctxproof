use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::{CheckKind, Diagnostic};

const CONFIG_FILE: &str = ".ctxproof.toml";

/// Optional repository configuration loaded from `.ctxproof.toml`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Relative paths or trailing-`*` prefix patterns ignored by doc/claim checks.
    pub ignore_paths: Vec<String>,
    /// Command strings or prefixes ignored by doc command validation.
    pub ignore_commands: Vec<String>,
    /// Additional markdown/instruction files to include in `ctxproof docs`.
    pub extra_docs: Vec<String>,
    /// Diagnostic `kind` slugs to suppress (e.g. `leftover-debug`, `conflicting-rule`).
    pub disable_checks: Vec<String>,
    /// Optional explicit Rust edition override for `rustfmt --edition`.
    pub rust_edition: Option<String>,
    /// Per-formatter toggles for `ctxproof diff`.
    pub formatters: FormatterConfig,
}

/// Formatter switches under `[formatters]` in `.ctxproof.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct FormatterConfig {
    pub rustfmt: bool,
    pub ruff: bool,
}

impl Default for FormatterConfig {
    fn default() -> Self {
        Self {
            rustfmt: true,
            ruff: true,
        }
    }
}

impl Config {
    /// Loads `<root>/.ctxproof.toml` if present, or returns defaults.
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(CONFIG_FILE);
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str::<Self>(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Writes a default `.ctxproof.toml` if one does not already exist (unless `force` is true).
    pub fn write_starter(root: &Path, force: bool) -> Result<bool> {
        let path = root.join(CONFIG_FILE);
        if path.exists() && !force {
            return Ok(false);
        }
        std::fs::write(&path, STARTER_TOML)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(true)
    }

    /// Ensures `.ctxproof/` is listed in `<root>/.gitignore` when inside a git repository.
    /// Returns `Ok(true)` if modified or created, `Ok(false)` if already present or non-git.
    pub fn ensure_gitignore_entry(root: &Path) -> Result<bool> {
        let gitignore = root.join(".gitignore");
        if !root.join(".git").exists() && !gitignore.is_file() {
            return Ok(false);
        }
        let existing = if gitignore.is_file() {
            std::fs::read_to_string(&gitignore)
                .with_context(|| format!("reading {}", gitignore.display()))?
        } else {
            String::new()
        };
        let already_ignored = existing.lines().any(|line| {
            matches!(
                line.trim(),
                ".ctxproof" | ".ctxproof/" | "/.ctxproof" | "/.ctxproof/"
            )
        });
        if already_ignored {
            return Ok(false);
        }

        let mut updated = existing;
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(".ctxproof/\n");
        std::fs::write(&gitignore, updated)
            .with_context(|| format!("writing {}", gitignore.display()))?;
        Ok(true)
    }

    /// True if `rel_path` matches any entry in `ignore_paths`.
    #[must_use]
    pub fn is_path_ignored(&self, rel_path: &str) -> bool {
        let norm = rel_path.replace('\\', "/");
        self.ignore_paths
            .iter()
            .any(|pat| glob_prefix_match(pat, &norm))
    }

    /// True if `cmd` matches any entry in `ignore_commands`.
    #[must_use]
    pub fn is_command_ignored(&self, cmd: &str) -> bool {
        let norm = cmd.trim();
        self.ignore_commands
            .iter()
            .any(|pat| glob_prefix_match(pat, norm))
    }

    /// Filters diagnostics according to `disable_checks` and `ignore_paths`.
    #[must_use]
    pub fn filter_diagnostics(&self, diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
        diagnostics
            .into_iter()
            .filter(|d| {
                let kind_slug = check_kind_slug(d.kind);
                if self
                    .disable_checks
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(kind_slug))
                {
                    return false;
                }
                if let Some(f) = &d.file
                    && self.is_path_ignored(&f.to_string_lossy())
                {
                    return false;
                }
                true
            })
            .collect()
    }
}

/// Returns the kebab-case slug matching `CheckKind` JSON serialization.
#[must_use]
pub fn check_kind_slug(kind: CheckKind) -> &'static str {
    match kind {
        CheckKind::MissingPath => "missing-path",
        CheckKind::BrokenLineRange => "broken-line-range",
        CheckKind::MissingSymbol => "missing-symbol",
        CheckKind::BrokenCommand => "broken-command",
        CheckKind::ConflictingRule => "conflicting-rule",
        CheckKind::DiffFormatting => "diff-formatting",
        CheckKind::LeftoverDebug => "leftover-debug",
        CheckKind::SecretLeak => "secret-leak",
        CheckKind::StaleReceipt => "stale-receipt",
        CheckKind::UnverifiedClaim => "unverified-claim",
        CheckKind::ProcessError => "process-error",
    }
}

fn glob_prefix_match(pattern: &str, value: &str) -> bool {
    let pat = pattern.trim().replace('\\', "/");
    let val = value.trim().replace('\\', "/");
    if pat.is_empty() {
        return false;
    }
    if let Some(prefix) = pat.strip_suffix("/**")
        && !prefix.contains(['*', '?'])
    {
        return val == prefix || val.starts_with(&format!("{prefix}/"));
    }
    glob_match(&pat, &val)
}

fn glob_match(pattern: &str, value: &str) -> bool {
    let p = pattern.as_bytes();
    let v = value.as_bytes();
    let (mut pi, mut vi) = (0usize, 0usize);
    let (mut star_pi, mut star_vi) = (None, 0usize);
    while vi < v.len() {
        if pi < p.len() && (p[pi] == v[vi] || p[pi] == b'?') {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_pi = Some(pi);
            star_vi = vi;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_vi += 1;
            vi = star_vi;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Starter `.ctxproof.toml` template generated by `ctxproof init`.
pub const STARTER_TOML: &str = r#"# ctxproof repository configuration

# Paths or glob patterns (`*`, `?`, `/**`) to ignore during doc and diff checks:
ignore_paths = []

# Documented commands to skip during validation:
ignore_commands = []

# Extra markdown/instruction files beyond AGENTS.md/CLAUDE.md/SKILL.md/README.md/docs/:
extra_docs = []

# Diagnostic kinds to suppress (e.g. "leftover-debug", "conflicting-rule", "missing-symbol"):
disable_checks = []

# Optional override for `rustfmt --edition`:
# rust_edition = "2024"

[formatters]
rustfmt = true
ruff = true
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_and_filters_config_patterns() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Config::write_starter(dir.path(), false).unwrap());
        assert!(!Config::write_starter(dir.path(), false).unwrap());

        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"
ignore_paths = ["examples/**", "fixtures/*", "*|.gd", "scenes/*/visual.tscn"]
ignore_commands = ["cargo xtask *"]
disable_checks = ["leftover-debug"]
"#,
        )
        .unwrap();

        let cfg = Config::load(dir.path()).unwrap();
        assert!(cfg.is_path_ignored("examples/demo/main.rs"));
        assert!(cfg.is_path_ignored("fixtures/a.py"));
        assert!(cfg.is_path_ignored("scenes/ui/map/map_screen.tscn|.gd"));
        assert!(cfg.is_path_ignored("scenes/player/visual.tscn"));
        assert!(!cfg.is_path_ignored("src/lib.rs"));
        assert!(cfg.is_command_ignored("cargo xtask bench"));

        let diags = vec![
            Diagnostic::new(
                crate::model::Severity::Warning,
                CheckKind::LeftoverDebug,
                None,
                None,
                "dbg",
            ),
            Diagnostic::new(
                crate::model::Severity::Error,
                CheckKind::MissingPath,
                Some("examples/a.rs".into()),
                None,
                "ignored path",
            ),
            Diagnostic::new(
                crate::model::Severity::Error,
                CheckKind::MissingPath,
                Some("src/missing.rs".into()),
                None,
                "kept",
            ),
        ];
        let filtered = cfg.filter_diagnostics(diags);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].message, "kept");
    }

    #[test]
    fn ensure_gitignore_entry_is_idempotent_and_skips_non_git() {
        let non_git = tempfile::tempdir().unwrap();
        assert!(!Config::ensure_gitignore_entry(non_git.path()).unwrap());
        assert!(!non_git.path().join(".gitignore").exists());

        let git_repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(git_repo.path().join(".git")).unwrap();
        std::fs::write(git_repo.path().join(".gitignore"), "target/").unwrap();
        assert!(Config::ensure_gitignore_entry(git_repo.path()).unwrap());
        assert!(!Config::ensure_gitignore_entry(git_repo.path()).unwrap());

        let text = std::fs::read_to_string(git_repo.path().join(".gitignore")).unwrap();
        assert_eq!(text, "target/\n.ctxproof/\n");
    }
}
