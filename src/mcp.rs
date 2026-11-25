use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::claims::check_claims_with_config;
use crate::config::Config;
use crate::diff::check_diff;
use crate::docs::check_docs_with_config;
use crate::manifests::ProjectManifests;
use crate::model::Report;
use crate::symbols::{RepoIndex, read_symbol_source};
use crate::verify::{inspect_receipts, run_verified};

const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[serde(default)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i64,
    message: String,
}

/// Runs the MCP JSON-RPC 2.0 server over stdio until EOF.
pub fn serve_stdio(default_root: &Path) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    serve_loop(default_root, &mut reader, &mut writer)
}

/// Processes line-delimited JSON-RPC 2.0 MCP requests from `reader` and writes responses to `writer`.
pub fn serve_loop<R: BufRead, W: Write>(
    default_root: &Path,
    reader: &mut R,
    writer: &mut W,
) -> Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: RpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let err_resp = RpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(RpcError {
                        code: -32700,
                        message: format!("invalid JSON: {e}"),
                    }),
                };
                write_frame(writer, &err_resp)?;
                continue;
            }
        };

        // Notifications omit `id` and must receive no reply.
        let Some(id) = req.id else {
            continue;
        };
        let _ = req.jsonrpc;
        let resp = handle_request(default_root, id, &req.method, &req.params);
        write_frame(writer, &resp)?;
    }
    Ok(())
}

fn write_frame<W: Write>(writer: &mut W, resp: &RpcResponse) -> Result<()> {
    serde_json::to_writer(&mut *writer, resp)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn handle_request(default_root: &Path, id: Value, method: &str, params: &Value) -> RpcResponse {
    match method {
        "initialize" => RpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "ctxproof",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })),
            error: None,
        },
        "ping" => RpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({})),
            error: None,
        },
        "tools/list" => RpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({ "tools": tool_schemas() })),
            error: None,
        },
        "tools/call" => call_tool(default_root, id, params),
        other => RpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code: -32601,
                message: format!("unknown method `{other}`"),
            }),
        },
    }
}

fn tool_schemas() -> Value {
    json!([
        {
            "name": "verify_docs",
            "description": "Audit AGENTS.md, CLAUDE.md, GEMINI.md, SKILL.md, README.md, and docs/*.md for context rot, respecting .ctxproof.toml. Optionally fix shifted symbol line links.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Optional repository root path (defaults to server cwd)." },
                    "fix": { "type": "boolean", "description": "Update stale #L line links matching a unique AST symbol in the target file." }
                }
            }
        },
        {
            "name": "verify_diff",
            "description": "Audit uncommitted git diff and untracked files strictly on changed lines (diff-scoped rustfmt & ruff format, debug prints, secret tokens).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Optional repository root path." }
                }
            }
        },
        {
            "name": "run_verified",
            "description": "Execute a test/lint command, distill noisy logs into compact diagnostics, and record a file-tree freshness receipt.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Optional repository root path." },
                    "command": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Command tokens, e.g. [\"cargo\", \"test\", \"--locked\"]."
                    }
                },
                "required": ["command"]
            }
        },
        {
            "name": "check_claims",
            "description": "Verify a draft PR body or final agent answer against live repo paths, line ranges, AST symbols, and fresh passing test receipts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Optional repository root path." },
                    "text": { "type": "string", "description": "Draft response, PR description, or commit text to audit." }
                },
                "required": ["text"]
            }
        },
        {
            "name": "repo_map",
            "description": "Return a compact AST symbol map (functions, structs, classes, line ranges across Rust/Python/TS/JS/Go/C/C++/C#) to save agent tokens.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Optional repository root path." },
                    "filter": { "type": "string", "description": "Optional path or symbol substring filter." },
                    "max_entries": { "type": "integer", "description": "Maximum symbols returned (default 120)." }
                }
            }
        },
        {
            "name": "read_symbol",
            "description": "Read only the exact AST line slice(s) for a named or qualified symbol instead of reading whole source files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Optional repository root path." },
                    "symbol": { "type": "string", "description": "Symbol or qualified name, e.g. `Engine::start` or `extract_symbols`." },
                    "file": { "type": "string", "description": "Optional relative file path filter." }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "receipt_status",
            "description": "Inspect recorded verification receipts under .ctxproof/receipts.json and check whether each is still FRESH or STALE.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Optional repository root path." }
                }
            }
        }
    ])
}

fn call_tool(default_root: &Path, id: Value, params: &Value) -> RpcResponse {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let resolved_root = match resolve_mcp_root(default_root, &args) {
        Ok(r) => r,
        Err(e) => {
            return RpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(json!({
                    "content": [{ "type": "text", "text": format!("error: {e}") }],
                    "isError": true
                })),
                error: None,
            };
        }
    };
    let root = resolved_root.as_path();

    let result = match name {
        "verify_docs" => {
            let fix = args.get("fix").and_then(|v| v.as_bool()).unwrap_or(false);
            run_docs(root, fix).map(format_report_payload)
        }
        "verify_diff" => check_diff(root).map(format_report_payload),
        "run_verified" => {
            let cmd: Vec<String> = args
                .get("command")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(ToOwned::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            if cmd.is_empty() {
                Err(anyhow::anyhow!("`command` array must be non-empty"))
            } else {
                run_verified(root, &cmd).map(|(report, receipt)| {
                    let text = format!("{}\n---\n{}", report.render_text(), receipt.compact_output);
                    json!({
                        "content": [{ "type": "text", "text": text }],
                        "structuredContent": { "report": report, "receipt": receipt },
                        "isError": !report.ok
                    })
                })
            }
        }
        "check_claims" => {
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            Config::load(root).and_then(|cfg| {
                RepoIndex::build(root)
                    .and_then(|idx| check_claims_with_config(&idx, &cfg, text))
                    .map(format_report_payload)
            })
        }
        "repo_map" => {
            let filter = args.get("filter").and_then(|v| v.as_str());
            let max_entries = args
                .get("max_entries")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(120);
            render_repo_map(root, filter, max_entries).map(|map_text| {
                json!({
                    "content": [{ "type": "text", "text": map_text }],
                    "isError": false
                })
            })
        }
        "read_symbol" => {
            let sym = args.get("symbol").and_then(|v| v.as_str()).unwrap_or("");
            let file_hint = args.get("file").and_then(|v| v.as_str());
            if sym.trim().is_empty() {
                Err(anyhow::anyhow!("`symbol` must be non-empty"))
            } else {
                render_symbol_slices(root, sym, file_hint, 5).map(|text| {
                    json!({
                        "content": [{ "type": "text", "text": text }],
                        "isError": false
                    })
                })
            }
        }
        "receipt_status" => inspect_receipts(root).map(|items| {
            let mut out = String::new();
            if items.is_empty() {
                out.push_str("No verification receipts recorded yet.\n");
            } else {
                for (r, fresh) in &items {
                    let state = if *fresh { "FRESH" } else { "STALE" };
                    let status = if r.ok { "PASS" } else { "FAIL" };
                    let _ = writeln!(
                        out,
                        "[{state}][{status}] `{}` ({} ms)",
                        r.command.join(" "),
                        r.duration_ms
                    );
                }
            }
            let structured: Vec<_> = items
                .into_iter()
                .map(|(receipt, fresh)| json!({ "fresh": fresh, "receipt": receipt }))
                .collect();
            json!({
                "content": [{ "type": "text", "text": out }],
                "structuredContent": { "receipts": structured },
                "isError": false
            })
        }),
        unknown => Err(anyhow::anyhow!("unknown tool `{unknown}`")),
    };

    match result {
        Ok(payload) => RpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(payload),
            error: None,
        },
        Err(e) => RpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({
                "content": [{ "type": "text", "text": format!("error: {e}") }],
                "isError": true
            })),
            error: None,
        },
    }
}

fn resolve_mcp_root(default_root: &Path, args: &Value) -> Result<PathBuf> {
    if let Some(explicit) = args
        .get("root")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Ok(PathBuf::from(explicit));
    }
    let canon = std::fs::canonicalize(default_root).unwrap_or_else(|_| default_root.to_path_buf());
    let is_fs_root = canon.parent().is_none();
    let is_home = ["USERPROFILE", "HOME"]
        .into_iter()
        .filter_map(std::env::var_os)
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .any(|home| home == canon);
    let has_marker = [
        ".git",
        ".ctxproof.toml",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "AGENTS.md",
        "CLAUDE.md",
    ]
    .iter()
    .any(|m| canon.join(m).exists());
    if (is_fs_root || is_home) && !has_marker {
        anyhow::bail!(
            "MCP server cwd ({}) is a home/root directory rather than a repository; pass explicit `root` in arguments",
            canon.display()
        );
    }
    Ok(canon)
}

fn run_docs(root: &Path, fix: bool) -> Result<Report> {
    let config = Config::load(root)?;
    let index = RepoIndex::build(root)?;
    let manifests = ProjectManifests::discover(&index);
    Ok(check_docs_with_config(&index, &manifests, &config, fix))
}

fn format_report_payload(report: Report) -> Value {
    let text = report.render_text();
    let is_error = !report.ok;
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": report,
        "isError": is_error
    })
}

/// Renders a compact token-efficient AST map for `root`.
pub fn render_repo_map(root: &Path, filter: Option<&str>, max_entries: usize) -> Result<String> {
    let index = RepoIndex::build(root)?;
    let mut out = String::new();
    let mut count = 0usize;
    let needle = filter.map(str::to_ascii_lowercase);

    for (file, syms) in &index.symbols_by_file {
        let matching: Vec<_> = syms
            .iter()
            .filter(|s| {
                needle.as_ref().is_none_or(|n| {
                    file.to_ascii_lowercase().contains(n)
                        || s.qualified.to_ascii_lowercase().contains(n)
                })
            })
            .collect();
        if matching.is_empty() {
            continue;
        }
        let line_count = index.files.get(file).map_or(0, |f| f.line_count);
        let _ = writeln!(out, "{file} ({line_count} lines):");
        for sym in matching {
            if count >= max_entries {
                let _ = writeln!(out, "... truncated at {max_entries} symbols");
                return Ok(out);
            }
            let _ = writeln!(
                out,
                "  L{}-L{} {} {}",
                sym.start_line, sym.end_line, sym.kind, sym.qualified
            );
            count += 1;
        }
    }
    if count == 0 {
        let _ = writeln!(out, "(no matching AST symbols)");
    }
    Ok(out)
}

/// Renders exact numbered source slices for matching AST symbols.
pub fn render_symbol_slices(
    root: &Path,
    symbol: &str,
    file_hint: Option<&str>,
    max_matches: usize,
) -> Result<String> {
    let index = RepoIndex::build(root)?;
    let matches = if let Some(hint) = file_hint {
        index.find_symbol(symbol, Some(hint))
    } else if symbol.contains("::") || symbol.contains('.') {
        let q = index.find_qualified_symbol(symbol);
        if q.is_empty() {
            index.find_symbol(symbol, None)
        } else {
            q
        }
    } else {
        index.find_symbol(symbol, None)
    };
    if matches.is_empty() {
        return Ok(format!("(symbol `{symbol}` not found in indexed AST)\n"));
    }

    let mut out = String::new();
    for entry in matches.iter().take(max_matches) {
        let slice = read_symbol_source(&index.root, entry)?;
        out.push_str(&slice);
        out.push('\n');
    }
    if matches.len() > max_matches {
        let _ = writeln!(
            out,
            "... ({} more matches omitted)",
            matches.len() - max_matches
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn mcp_stdio_round_trip_handles_initialize_list_and_tools() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn ping() {}\n").unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "See `src/lib.rs#L1-L1`.\n").unwrap();

        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"verify_docs\",\"arguments\":{}}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"check_claims\",\"arguments\":{\"text\":\"Checked `fn missing`\"}}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"read_symbol\",\"arguments\":{\"symbol\":\"ping\"}}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"tools/call\",\"params\":{\"name\":\"receipt_status\",\"arguments\":{}}}\n"
        );
        let mut reader = Cursor::new(input.as_bytes());
        let mut out = Vec::new();
        serve_loop(dir.path(), &mut reader, &mut out).unwrap();

        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 6); // notification produced no response
        assert_eq!(lines[0]["result"]["serverInfo"]["name"], "ctxproof");
        assert_eq!(lines[1]["result"]["tools"].as_array().unwrap().len(), 7);
        assert_eq!(lines[2]["result"]["isError"], false);
        assert_eq!(lines[3]["result"]["isError"], true);
        assert!(
            lines[4]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("pub fn ping()")
        );
        assert_eq!(lines[5]["result"]["isError"], false);
    }

    #[test]
    fn refuses_implicit_filesystem_root_cwd() {
        let fs_root = if cfg!(windows) {
            Path::new("C:\\")
        } else {
            Path::new("/")
        };
        let err = resolve_mcp_root(fs_root, &json!({})).unwrap_err();
        assert!(err.to_string().contains("home/root directory"));

        let explicit = resolve_mcp_root(fs_root, &json!({ "root": "d:\\Ai\\ctxproof" })).unwrap();
        assert_eq!(explicit, PathBuf::from("d:\\Ai\\ctxproof"));
    }
}
