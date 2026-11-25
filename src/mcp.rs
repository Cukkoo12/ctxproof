use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::claims::check_claims;
use crate::diff::check_diff;
use crate::docs::check_docs;
use crate::manifests::ProjectManifests;
use crate::model::Report;
use crate::symbols::RepoIndex;
use crate::verify::run_verified;

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
            "description": "Audit AGENTS.md, CLAUDE.md, GEMINI.md, SKILL.md, and README.md for missing file paths, out-of-range line links, invalid commands, missing AST symbols, and rule conflicts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Optional repository root path (defaults to server cwd)." }
                }
            }
        },
        {
            "name": "verify_diff",
            "description": "Audit uncommitted git diff and untracked files strictly on changed lines (diff-only rustfmt, debug prints, secret tokens).",
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
            "description": "Return a compact AST symbol map (functions, structs, classes, line ranges) filtered by optional substring to save agent tokens.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Optional repository root path." },
                    "filter": { "type": "string", "description": "Optional path or symbol substring filter." },
                    "max_entries": { "type": "integer", "description": "Maximum symbols returned (default 120)." }
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
    let root = args
        .get("root")
        .and_then(|v| v.as_str())
        .map(Path::new)
        .unwrap_or(default_root);

    let result = match name {
        "verify_docs" => run_docs(root).map(format_report_payload),
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
            RepoIndex::build(root)
                .and_then(|idx| check_claims(&idx, text))
                .map(format_report_payload)
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

fn run_docs(root: &Path) -> Result<Report> {
    let index = RepoIndex::build(root)?;
    let manifests = ProjectManifests::discover(&index);
    Ok(check_docs(&index, &manifests))
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
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"check_claims\",\"arguments\":{\"text\":\"Checked `fn missing`\"}}}\n"
        );
        let mut reader = Cursor::new(input.as_bytes());
        let mut out = Vec::new();
        serve_loop(dir.path(), &mut reader, &mut out).unwrap();

        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 4); // notification produced no response
        assert_eq!(lines[0]["result"]["serverInfo"]["name"], "ctxproof");
        assert_eq!(lines[1]["result"]["tools"].as_array().unwrap().len(), 5);
        assert_eq!(lines[2]["result"]["isError"], false);
        assert_eq!(lines[3]["result"]["isError"], true);
    }
}
