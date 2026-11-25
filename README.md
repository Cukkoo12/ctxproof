# ctxproof

**Context-rot, diff-hygiene, claim-verification, and MCP guardrail tool for AI coding agents.**

AI coding agents (`Claude Code`, `Codex`, `Cursor`, `Gemini CLI`, `Antigravity`, `OpenCode`) waste tokens and hallucinate when:
1. Repository docs (`AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `SKILL.md`, `README.md`, `docs/*.md`) rot — pointing to deleted files, shifted `#L` line links, obsolete Cargo/npm/Make commands, or contradicting instructions across tools.
2. Pre-existing formatting debt in large files blocks format checks, or debug calls and credentials slip into uncommitted diffs.
3. Final replies or PR bodies cite file paths, line ranges, AST symbols, or "all tests passed" claims that went stale after a final edit.

`ctxproof` is a single fast Rust binary solving all three via CLI, SARIF 2.1.0 CI reporting, and an MCP (`stdio` JSON-RPC 2.0) server.

## Features (v0.3.0)

- **`ctxproof init`** — Writes a starter `.ctxproof.toml` configuration file and prints MCP client setup instructions.
- **`ctxproof docs [--fix]`** — Scans `AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `RULES.md`, `CONTRIBUTING.md`, `README.md`, `docs/**/*.md`, `.cursorrules`, `.cursor/rules/**`, `.agents/**`, and `**/SKILL.md` (plus `extra_docs` in `.ctxproof.toml`) for:
  - Missing relative paths and out-of-range `#Lstart-Lend` line links (`--fix` updates shifted `#L` links whose label uniquely matches an AST symbol in the target file)
  - Invalid `cargo -p` / `--features` / cargo aliases, `package.json` scripts, `Makefile` targets, and `justfile` recipes
  - Documented symbol declarations (`fn`, `func`, `struct`, `enum`, `trait`, `def`, `class`, `interface`, `record`, `namespace`) verified with `tree-sitter` across **Rust, Python, TypeScript/TSX, JavaScript, Go, C, C++, and C# (including Unity `.cs` projects under the Assets directory, skipping `Library/`, `Temp/`, `Logs/`, and `Obj/`)**
  - Cross-file contradictions between AI instruction files and repo lockfiles (`npm` vs `pnpm` vs `yarn` vs `bun`)
- **`ctxproof diff`** — Audits staged + unstaged `git diff` and untracked files **strictly on edited lines**:
  - Diff-scoped `rustfmt --check` and Python `ruff format --diff` ignore untouched pre-existing formatting drift outside edited hunks (and report transparently if a formatter binary is unavailable)
  - Flags leftover debug statements (`dbg!`, `console.log`, `breakpoint()`, `pdb.set_trace()`, `TODO(agent)`) in added source files and credential/key patterns in any added lines
- **`ctxproof verify -- <cmd>`** & **`ctxproof status`** — Runs checks, distills noisy compiler/test output down to actionable failures, and records a `blake3` tree-fingerprint receipt under `.ctxproof/receipts.json`. Any edit invalidates matching receipts as **`STALE`**.
- **`ctxproof check-claims`** — Verifies draft PR descriptions or AI responses against live repo paths (`file://` links included), `#L` ranges, `tree-sitter` AST symbols (`Type::method`, `module::symbol`, and `Type.Method`), and fresh passing test receipts.
- **`ctxproof map [--symbol <name> --source]`** — Prints a token-compact AST symbol map or reads only the numbered AST source slice for a requested symbol.
- **`ctxproof check`** — Runs `docs`, `diff`, and stale-receipt checks in one pass.
- **`ctxproof mcp`** — Exposes 7 tools over MCP `stdio` JSON-RPC 2.0: `verify_docs`, `verify_diff`, `run_verified`, `check_claims`, `repo_map`, `read_symbol`, and `receipt_status`.

## Installation & Quick Start

```sh
cargo install --path .
```

Run checks in any repository:

```sh
ctxproof init
ctxproof docs
ctxproof docs --fix
ctxproof diff
ctxproof verify -- cargo test --locked
ctxproof check-claims --file pr-body.md
ctxproof map --filter auth --max-entries 80
ctxproof map --symbol RepoIndex::build --source
ctxproof check
```

### Machine-Readable JSON & GitHub Code Scanning (SARIF 2.1.0)

Pass `--format json` or `--format sarif` to any reporting command (`docs`, `diff`, `check-claims`, `check`, `verify`). Exit code is `0` when clean and `2` when verification errors are found.

## Configuration (`.ctxproof.toml`)

```toml
ignore_paths = ["examples/**", "fixtures/*"]
ignore_commands = ["cargo xtask *"]
extra_docs = []
disable_checks = []

[formatters]
rustfmt = true
ruff = true
```

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
