# ctxproof

[![CI](https://github.com/Cukkoo12/ctxproof/actions/workflows/ci.yml/badge.svg)](https://github.com/Cukkoo12/ctxproof/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Cukkoo12/ctxproof)](https://github.com/Cukkoo12/ctxproof/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Context-rot, diff-hygiene, claim-verification, and MCP guardrail tool for AI coding agents.**

AI coding agents (`Claude Code`, `Codex`, `Cursor`, `Gemini CLI`, `Antigravity`, `OpenCode`) waste tokens and hallucinate when:
1. Repository docs (`AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `SKILL.md`, `README.md`, `docs/*.md`) rot — pointing to deleted files, shifted `#L` line links, obsolete Cargo/npm/Make commands, or contradicting instructions across tools.
2. Pre-existing formatting debt in large files blocks format checks, or debug calls and credentials slip into uncommitted diffs.
3. Final replies or PR bodies cite file paths, line ranges, AST symbols, or "all tests passed" claims that went stale after a final edit.

`ctxproof` is a single fast Rust binary solving all three via CLI, SARIF 2.1.0 CI reporting, and an MCP (`stdio` JSON-RPC 2.0) server.

---

## See It In Action

### 1. Catch hallucinated paths and AST symbols before replying

```sh
# 1) Record a cryptographic (blake3) receipt when checks pass
$ ctxproof verify -- cargo test --locked
[PASS] verify: `cargo test --locked` passed in 4850 ms (receipt recorded)

# 2) Agent cites a nonexistent path or a hallucinated method on a real type
$ ctxproof check-claims --text 'Updated `src/claims.rs` (`VerificationReceipt::bogus`) and `src/ghost.rs`.'
[FAIL] check-claims: checked claims text (2 errors, 0 warnings)
  - [error] claimed path `src/ghost.rs` does not exist in repository
  - [error] type or module `VerificationReceipt` exists in repo, but qualified symbol `VerificationReceipt::bogus` was not found
```

### 2. Read only the target AST slice instead of burning tokens on full files

**Before:** Agent reads the entire 305-line file to locate a single function.  
**After:** Ask `ctxproof` for a compact AST map (`repo_map` / `read_symbol`):

```sh
$ ctxproof map --filter verify --max-entries 8
src/lib.rs (9 lines):
  L9-L9 mod verify
src/verify.rs (305 lines):
  L13-L13 const RECEIPT_DIR
  L14-L14 const RECEIPT_FILE
  L18-L26 struct VerificationReceipt
  L29-L31 struct ReceiptStore
  L34-L72 fn compute_tree_fingerprint
  L76-L137 fn run_verified
  L139-L158 fn spawn_command
... truncated at 8 symbols

$ ctxproof map --symbol run_verified --source
// src/verify.rs (fn run_verified L76-L137)
  76: pub fn run_verified(root: &Path, command: &[String]) -> Result<(Report, VerificationReceipt)> {
  77:     anyhow::ensure!(!command.is_empty(), "command cannot be empty");
```

### 3. Detect & auto-fix drifted `#L` links in agent instruction files

```sh
$ ctxproof docs --fix
[PASS] docs: scanned 1 doc/instruction files (0 errors, 0 warnings)
```

---

## Installation

### Option 1: One-Line Installer (Recommended — no Rust needed)

**macOS (Apple Silicon) & Linux:**

```sh
curl -fsSL https://raw.githubusercontent.com/Cukkoo12/ctxproof/main/install.sh | sh
```

**Windows (PowerShell):**

```powershell
irm https://raw.githubusercontent.com/Cukkoo12/ctxproof/main/install.ps1 | iex
```

Both [install.sh](install.sh) and [install.ps1](install.ps1) verify the `.sha256` checksum from [**GitHub Releases**](https://github.com/Cukkoo12/ctxproof/releases/latest). Manual archives (`tar.gz` / `zip`) are also available on the releases page.

### Option 2: Install via Cargo

```sh
cargo install --git https://github.com/Cukkoo12/ctxproof --locked
```

Or from a local clone:

```sh
cargo install --path . --locked
```

---

## Two-Step Agent Integration (MCP + Rule)

### Step 1: Register the MCP server

**Claude Code:**

```sh
claude mcp add -s user ctxproof -- ctxproof mcp
```

**Codex CLI:**

```sh
codex mcp add ctxproof -- ctxproof mcp
```

**Cursor / Antigravity / Gemini CLI / Generic MCP (`mcp_config.json`):**

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

### Step 2: Tell your agent to use it (`CLAUDE.md` / `AGENTS.md`)

Coding agents keep using built-in `Read`/`Bash` unless instructed to prefer `ctxproof`. Add these 4 lines to your global or repo `CLAUDE.md` / `AGENTS.md` (also printed by `ctxproof init`):

```markdown
## Code inspection & verification (`ctxproof` MCP)
- Before reading large source files, use `ctxproof` `repo_map` + `read_symbol`.
- Run tests and builds through `run_verified` to record freshness receipts.
- Before replying, run `verify_diff` (and `verify_docs` / `check_claims` when citing files, symbols, or test results).
```

---

## Features (v0.4.1)

- **`ctxproof init`** — Writes `.ctxproof.toml`, adds `.ctxproof/` to `.gitignore` in git repos, and prints ready-to-paste MCP + `CLAUDE.md`/`AGENTS.md` setup instructions.
- **`ctxproof docs [--fix]`** — Scans `AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `RULES.md`, `CONTRIBUTING.md`, `README.md`, `docs/**/*.md`, `.cursorrules`, `.cursor/rules/**`, `.agents/**`, and `**/SKILL.md` (plus `extra_docs` in `.ctxproof.toml`) for:
  - Missing relative paths (including `res://` resource paths in Godot repos) and out-of-range `#Lstart-Lend` line links (`--fix` updates shifted `#L` links whose label uniquely matches an AST symbol in the target file)
  - Invalid `cargo -p` / `--features` / cargo aliases, `package.json` scripts, `Makefile` targets, and `justfile` recipes
  - Documented symbol declarations (`fn`, `func`, `struct`, `enum`, `trait`, `def`, `class`, `interface`, `record`, `namespace`, `signal`) verified with `tree-sitter` across **Rust, Python, TypeScript/TSX, JavaScript, Go, C, C++, C# (including Unity `.cs` projects, skipping `Library/`, `Temp/`, `Logs/`, `Obj/`), and GDScript (Godot `.gd` / `project.godot`, skipping `.godot/`, `.import/`, `.mono/`)**
  - Cross-file contradictions between AI instruction files and repo lockfiles (`npm` vs `pnpm` vs `yarn` vs `bun`)
- **`ctxproof diff`** — Audits staged + unstaged `git diff` and untracked files **strictly on edited lines**:
  - Diff-scoped `rustfmt --check` and Python `ruff format --diff` ignore untouched pre-existing formatting drift outside edited hunks (and report transparently if a formatter binary is unavailable)
  - Flags leftover debug statements (`dbg!`, `console.log`, `breakpoint()`, `pdb.set_trace()`, `TODO(agent)`) in added source files and credential/key patterns in any added lines
- **`ctxproof verify -- <cmd>`** & **`ctxproof status`** — Runs checks, distills noisy compiler/test output down to actionable failures, and records a `blake3` tree-fingerprint receipt under `.ctxproof/receipts.json`. Any edit invalidates matching receipts as **`STALE`**.
- **`ctxproof check-claims`** — Verifies draft PR descriptions or AI responses against live repo paths (`file://` and Godot `res://` links included), `#L` ranges, `tree-sitter` AST symbols (`Type::method`, `module::symbol`, `Type.Method`), and fresh passing test receipts.
- **`ctxproof map [--symbol <name> --source]`** — Prints a token-compact AST symbol map or reads only the numbered AST source slice for a requested symbol.
- **`ctxproof check`** — Runs `docs`, `diff`, and stale-receipt checks in one pass.
- **`ctxproof mcp`** — Exposes 7 tools over MCP `stdio` JSON-RPC 2.0: `verify_docs`, `verify_diff`, `run_verified`, `check_claims`, `repo_map`, `read_symbol`, and `receipt_status`.

---

## Quick CLI Reference & CI Usage

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

## License

[MIT](LICENSE)
