use std::fmt::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Severity of a diagnostic emitted by `ctxproof`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// Machine-readable diagnostic category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckKind {
    MissingPath,
    BrokenLineRange,
    MissingSymbol,
    BrokenCommand,
    ConflictingRule,
    DiffFormatting,
    LeftoverDebug,
    SecretLeak,
    StaleReceipt,
    UnverifiedClaim,
    ProcessError,
}

/// One concrete issue or notice discovered during verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub kind: CheckKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

impl Diagnostic {
    /// Constructs a diagnostic with no optional suggestion attached.
    #[must_use]
    pub fn new(
        severity: Severity,
        kind: CheckKind,
        file: Option<PathBuf>,
        line: Option<usize>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            kind,
            file,
            line,
            message: message.into(),
            suggestion: None,
        }
    }

    /// Attaches a short remediation hint to the diagnostic.
    #[must_use]
    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }
}

/// Aggregated result returned by CLI subcommands and MCP tools.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub tool: String,
    pub root: PathBuf,
    pub ok: bool,
    pub summary: String,
    pub diagnostics: Vec<Diagnostic>,
}

impl Report {
    /// Builds a new report and computes `ok` from error-severity diagnostics.
    #[must_use]
    pub fn from_diagnostics(
        tool: impl Into<String>,
        root: PathBuf,
        summary: impl Into<String>,
        diagnostics: Vec<Diagnostic>,
    ) -> Self {
        let ok = !diagnostics.iter().any(|d| d.severity == Severity::Error);
        Self {
            tool: tool.into(),
            root,
            ok,
            summary: summary.into(),
            diagnostics,
        }
    }

    /// Renders a compact human- and agent-readable plain-text report.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let status = if self.ok { "PASS" } else { "FAIL" };
        let _ = writeln!(out, "[{status}] {}: {}", self.tool, self.summary);
        for diag in &self.diagnostics {
            let sev = match diag.severity {
                Severity::Error => "error",
                Severity::Warning => "warn",
                Severity::Info => "info",
            };
            let location = match (&diag.file, diag.line) {
                (Some(f), Some(l)) => format!("{}:{} ", f.display(), l),
                (Some(f), None) => format!("{} ", f.display()),
                (None, _) => String::new(),
            };
            let _ = writeln!(out, "  - [{sev}] {location}{}", diag.message);
            if let Some(hint) = &diag.suggestion {
                let _ = writeln!(out, "      hint: {hint}");
            }
        }
        out
    }

    /// Renders diagnostics as SARIF 2.1.0 JSON for GitHub Code Scanning / CI integrations.
    #[must_use]
    pub fn render_sarif(&self) -> serde_json::Value {
        let results: Vec<serde_json::Value> = self
            .diagnostics
            .iter()
            .map(|d| {
                let level = match d.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                    Severity::Info => "note",
                };
                let rule_id = crate::config::check_kind_slug(d.kind);
                let mut msg = d.message.clone();
                if let Some(hint) = &d.suggestion {
                    msg.push_str(&format!(" (hint: {hint})"));
                }
                let locations: Vec<serde_json::Value> = d
                    .file
                    .as_ref()
                    .map(|f| {
                        let uri = f.to_string_lossy().replace('\\', "/");
                        let mut phys = serde_json::json!({
                            "artifactLocation": { "uri": uri }
                        });
                        if let Some(line) = d.line {
                            phys["region"] = serde_json::json!({ "startLine": line });
                        }
                        vec![serde_json::json!({ "physicalLocation": phys })]
                    })
                    .unwrap_or_default();

                let mut entry = serde_json::json!({
                    "ruleId": rule_id,
                    "level": level,
                    "message": { "text": msg },
                });
                if !locations.is_empty() {
                    entry["locations"] = serde_json::Value::Array(locations);
                }
                entry
            })
            .collect();

        serde_json::json!({
            "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
            "version": "2.1.0",
            "runs": [{
                "tool": {
                    "driver": {
                        "name": "ctxproof",
                        "version": env!("CARGO_PKG_VERSION"),
                        "informationUri": "https://github.com/Cukkoo12/ctxproof"
                    }
                },
                "results": results
            }]
        })
    }
}
