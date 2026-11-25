use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use ctxproof::claims::check_claims;
use ctxproof::diff::check_diff;
use ctxproof::docs::check_docs;
use ctxproof::manifests::ProjectManifests;
use ctxproof::mcp::{render_repo_map, serve_stdio};
use ctxproof::model::{CheckKind, Diagnostic, Report, Severity};
use ctxproof::symbols::RepoIndex;
use ctxproof::verify::{inspect_receipts, run_verified};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Parser)]
#[command(
    name = "ctxproof",
    version,
    about = "Context-rot, diff-hygiene, claim-verification, and MCP guardrail tool for AI coding agents"
)]
struct Cli {
    /// Repository root path (defaults to current working directory).
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,

    /// Output format for reports.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Audit AGENTS.md, CLAUDE.md, GEMINI.md, SKILL.md, and README files for context rot.
    Docs,
    /// Audit git diff + untracked files strictly on edited lines.
    Diff,
    /// Run a check command, distill logs, and write a tree-fingerprint freshness receipt.
    Verify {
        /// Command and arguments to execute.
        #[arg(required = true, last = true)]
        cmd: Vec<String>,
    },
    /// Check draft PR text, commit notes, or an agent response against repo truth & receipts.
    CheckClaims {
        /// Path to a markdown/text file containing claims.
        #[arg(long, conflicts_with = "text", required_unless_present = "text")]
        file: Option<PathBuf>,
        /// Inline text containing claims.
        #[arg(long, conflicts_with = "file", required_unless_present = "file")]
        text: Option<String>,
    },
    /// Run docs, diff, and receipt freshness checks together.
    Check,
    /// Show recorded verification receipts and whether each is fresh or stale.
    Status,
    /// Print a token-compact AST symbol map for the repository.
    Map {
        /// Optional path or symbol substring filter.
        #[arg(long)]
        filter: Option<String>,
        /// Maximum number of symbol entries to print.
        #[arg(long, default_value_t = 120)]
        max_entries: usize,
    },
    /// Start an MCP JSON-RPC 2.0 server on stdio.
    Mcp,
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(err) => {
            eprintln!("ctxproof error: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<bool> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Docs => {
            let index = RepoIndex::build(&cli.root)?;
            let manifests = ProjectManifests::discover(&index);
            let report = check_docs(&index, &manifests);
            emit_report(&report, cli.format)
        }
        Commands::Diff => {
            let report = check_diff(&cli.root)?;
            emit_report(&report, cli.format)
        }
        Commands::Verify { cmd } => {
            let (report, receipt) = run_verified(&cli.root, &cmd)?;
            match cli.format {
                OutputFormat::Text => {
                    print!("{}", report.render_text());
                    if !receipt.compact_output.is_empty() && report.ok {
                        println!("{}", receipt.compact_output);
                    }
                }
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "report": report,
                            "receipt": receipt,
                        }))?
                    );
                }
            }
            Ok(report.ok)
        }
        Commands::CheckClaims { file, text } => {
            let body = match (file, text) {
                (Some(path), None) => std::fs::read_to_string(&path)
                    .with_context(|| format!("reading claims file {}", path.display()))?,
                (None, Some(t)) => t,
                _ => unreachable!(),
            };
            let index = RepoIndex::build(&cli.root)?;
            let report = check_claims(&index, &body)?;
            emit_report(&report, cli.format)
        }
        Commands::Check => {
            let index = RepoIndex::build(&cli.root)?;
            let manifests = ProjectManifests::discover(&index);
            let mut diags = Vec::new();
            let docs_report = check_docs(&index, &manifests);
            diags.extend(docs_report.diagnostics);

            if index.root.join(".git").exists() {
                let diff_report = check_diff(&index.root)?;
                diags.extend(diff_report.diagnostics);
            }

            let receipts = inspect_receipts(&index.root)?;
            let stale_count = receipts.iter().filter(|(_, fresh)| !*fresh).count();
            for (r, fresh) in receipts {
                if !fresh {
                    diags.push(Diagnostic::new(
                        Severity::Warning,
                        CheckKind::StaleReceipt,
                        None,
                        None,
                        format!(
                            "verification receipt for `{}` is stale",
                            r.command.join(" ")
                        ),
                    ));
                }
            }

            let errors = diags
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .count();
            let warnings = diags
                .iter()
                .filter(|d| d.severity == Severity::Warning)
                .count();
            let summary = format!(
                "combined check finished ({errors} errors, {warnings} warnings, {stale_count} stale receipts)"
            );
            let report = Report::from_diagnostics("check", index.root, summary, diags);
            emit_report(&report, cli.format)
        }
        Commands::Status => {
            let receipts = inspect_receipts(&cli.root)?;
            match cli.format {
                OutputFormat::Text => {
                    if receipts.is_empty() {
                        println!("No verification receipts recorded yet.");
                    } else {
                        for (r, fresh) in &receipts {
                            let state = if *fresh { "FRESH" } else { "STALE" };
                            let status = if r.ok { "PASS" } else { "FAIL" };
                            println!(
                                "[{state}][{status}] `{}` ({} ms)",
                                r.command.join(" "),
                                r.duration_ms
                            );
                        }
                    }
                }
                OutputFormat::Json => {
                    let items: Vec<_> = receipts
                        .into_iter()
                        .map(|(receipt, fresh)| {
                            serde_json::json!({ "fresh": fresh, "receipt": receipt })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&items)?);
                }
            }
            Ok(true)
        }
        Commands::Map {
            filter,
            max_entries,
        } => {
            let map = render_repo_map(&cli.root, filter.as_deref(), max_entries)?;
            print!("{map}");
            Ok(true)
        }
        Commands::Mcp => {
            serve_stdio(&cli.root)?;
            Ok(true)
        }
    }
}

fn emit_report(report: &Report, format: OutputFormat) -> Result<bool> {
    match format {
        OutputFormat::Text => print!("{}", report.render_text()),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(report)?),
    }
    Ok(report.ok)
}
