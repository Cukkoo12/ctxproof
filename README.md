# ctxproof

**Context-rot, diff-hygiene, claim-verification, and MCP guardrail tool for AI coding agents.**

AI coding agents (`Claude Code`, `Codex`, `Cursor`, `Gemini CLI`, `Antigravity`, `OpenCode`) waste context tokens and hallucinate when:
1. Repository docs (`AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `SKILL.md`, `README.md`) rot — pointing to deleted files, out-of-range `#L` line links, obsolete Cargo/npm/Make commands, or contradictory rules across tools.
2. Pre-existing formatting debt in large files blocks formatter checks, or leftover `dbg!`/debug calls and secrets slip into uncommitted diffs.
3. Final replies or PR bodies cite file paths, line ranges, AST symbols, or "all tests passed" claims that went stale after a final edit.

`ctxproof` is a single fast Rust binary that solves all three via CLI and an MCP (`stdio` JSON-RPC 2.0) server.

## Features

- **`ctxproof docs`** — Scans `AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `RULES.md`, `CONTRIBUTING.md`, `README.md`, `docs/**/*.md`, `.cursorrules`, `.cursor/rules/**`, `.agents/**`, and `**/SKILL.md` for:
  - Missing relative paths and out-of-range `#Lstart-Lend` line links
  - Invalid `cargo -p` / `--features` / cargo aliases, `package.json` scripts, `Makefile` targets, and `justfile` recipes
  - Documented symbol declarations (`fn`, `struct`, `enum`, `trait`, `def`, `class`, `interface`) verified with `tree-sitter` across Rust, Python, TypeScript/TSX, and JavaScript
  - Cross-file contradictions between AI instruction files and repo lockfiles (`npm` vs `pnpm` vs `yarn` vs `bun`)
- **`ctxproof diff`** — Audits staged + unstaged `git diff` and untracked files **strictly on edited lines**:
  - Diff-scoped `rustfmt --check` ignores pre-existing formatting drift outside your edited hunks
  - Flags leftover debug statements (`dbg!`, `console.log`, `breakpoint()`, `pdb.set_trace()`, `TODO(agent)`) and leaked token/key patterns
- **`ctxproof verify -- <cmd>`** & **`ctxproof status`** — Runs checks, distills verbose compiler/test output down to actionable failures, and records a `blake3` tree-fingerprint receipt under `.ctxproof/receipts.json`. Any edit invalidates matching receipts as **`STALE`**.
- **`ctxproof check-claims`** — Verifies draft PR descriptions or AI responses against live repo paths, `#L` ranges, `tree-sitter` AST symbols (including Rust `Type::method` and `module::symbol` paths), and fresh passing test receipts.
- **`ctxproof map`** — Prints a token-compact AST symbol map (`file`, line count, `Lstart-Lend kind qualified_name`).
- **`ctxproof mcp`** — Exposes `verify_docs`, `verify_diff`, `run_verified`, `check_claims`, and `repo_map` over MCP `stdio` JSON-RPC 2.0.

## Installation & Quick Start

```sh
cargo install --path .
```

Run checks in any repository:

```sh
ctxproof docs
ctxproof diff
ctxproof verify -- cargo test --locked
ctxproof check-claims --file pr-body.md
ctxproof map --filter auth --max-entries 80
ctxproof check
```

Add `--format json` to any reporting command for machine-readable output. Exit code is `0` when clean and `2` when verification errors are found.

## MCP Integration

Add `ctxproof` to any MCP-compatible client (`Claude Code`, `Codex`, `Cursor`, `Antigravity`):

```json
{
  "mcpServers": {
    "ctxproof": {
      "command": "ctxproof",
      "args": ["mcp"]
    }
  }
}
```

## License

MIT
