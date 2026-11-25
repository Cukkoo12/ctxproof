use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ctxproof")
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn end_to_end_docs_diff_verify_claims_and_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    git(root, &["init"]);
    git(root, &["config", "--local", "user.name", "Test"]);
    git(
        root,
        &["config", "--local", "user.email", "test@example.com"],
    );

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    // Deliberately keep line 1 badly formatted as pre-existing untouched debt.
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn   untouched_bad_fmt(  ) -> i32 { 1 }\npub fn clean_fn() -> i32 { 2 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("AGENTS.md"),
        "See [lib](src/lib.rs#L1-L2) and `src/missing.rs`.\nRun `cargo test -p sample`.\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "init"]);

    // 1. `docs` catches missing path and exits with code 2.
    let bad_docs = Command::new(bin())
        .args([
            "--root",
            &root.to_string_lossy(),
            "--format",
            "json",
            "docs",
        ])
        .output()
        .unwrap();
    assert_eq!(bad_docs.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&bad_docs.stdout).unwrap();
    assert_eq!(json["ok"], false);

    // Fix AGENTS.md cleanly; `docs` now exits 0.
    std::fs::write(
        root.join("AGENTS.md"),
        "See [lib](src/lib.rs#L1-L2).\nRun `cargo test -p sample`.\n",
    )
    .unwrap();
    let ok_docs = Command::new(bin())
        .args(["--root", &root.to_string_lossy(), "docs"])
        .output()
        .unwrap();
    assert!(
        ok_docs.status.success(),
        "{}",
        String::from_utf8_lossy(&ok_docs.stdout)
    );

    // 2. Clean formatted edit at bottom of src/lib.rs ignores pre-existing line 1 formatting debt.
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn   untouched_bad_fmt(  ) -> i32 { 1 }\npub fn clean_fn() -> i32 { 2 }\npub fn added_fn() -> i32 {\n    3\n}\n",
    )
    .unwrap();
    let ok_diff = Command::new(bin())
        .args(["--root", &root.to_string_lossy(), "diff"])
        .output()
        .unwrap();
    assert!(
        ok_diff.status.success(),
        "{}",
        String::from_utf8_lossy(&ok_diff.stdout)
    );
    let diff_stdout = String::from_utf8_lossy(&ok_diff.stdout);
    assert!(
        diff_stdout.contains("ignored 1 untouched rustfmt hunks"),
        "unexpected diff stdout: {diff_stdout}"
    );

    // 3. Record verification receipt, verify freshness, invalidate on edit, then re-verify.
    let verify_cmd = if cfg!(windows) {
        vec![
            "--root",
            root.to_str().unwrap(),
            "verify",
            "--",
            "cmd",
            "/C",
            "echo ok",
        ]
    } else {
        vec![
            "--root",
            root.to_str().unwrap(),
            "verify",
            "--",
            "sh",
            "-c",
            "echo ok",
        ]
    };
    let v1 = Command::new(bin()).args(&verify_cmd).output().unwrap();
    assert!(
        v1.status.success(),
        "{}",
        String::from_utf8_lossy(&v1.stderr)
    );

    let cited = if cfg!(windows) {
        "Added `fn added_fn` in `src/lib.rs#L3-L5` and ran `cmd /C echo ok`."
    } else {
        "Added `fn added_fn` in `src/lib.rs#L3-L5` and ran `sh -c echo ok`."
    };
    let fresh_claims = Command::new(bin())
        .args([
            "--root",
            &root.to_string_lossy(),
            "check-claims",
            "--text",
            cited,
        ])
        .output()
        .unwrap();
    assert!(
        fresh_claims.status.success(),
        "{}",
        String::from_utf8_lossy(&fresh_claims.stdout)
    );

    // Edit file -> receipt becomes stale and check-claims exits with code 2.
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn   untouched_bad_fmt(  ) -> i32 { 1 }\npub fn clean_fn() -> i32 { 2 }\npub fn added_fn() -> i32 {\n    4\n}\n",
    )
    .unwrap();
    let stale_claims = Command::new(bin())
        .args([
            "--root",
            &root.to_string_lossy(),
            "check-claims",
            "--text",
            cited,
        ])
        .output()
        .unwrap();
    assert_eq!(stale_claims.status.code(), Some(2));

    // 4. Spawn real MCP stdio server and verify tools/list + repo_map.
    let mut mcp = Command::new(bin())
        .args(["--root", &root.to_string_lossy(), "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = mcp.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{}}}}"
        )
        .unwrap();
        writeln!(
            stdin,
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{{\"name\":\"repo_map\",\"arguments\":{{\"filter\":\"added_fn\"}}}}}}"
        )
        .unwrap();
    }
    let mcp_out = mcp.wait_with_output().unwrap();
    assert!(mcp_out.status.success());
    let stdout = String::from_utf8(mcp_out.stdout).unwrap();
    assert!(stdout.contains("added_fn"));
}
