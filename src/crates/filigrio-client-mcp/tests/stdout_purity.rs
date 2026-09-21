//! Regression test: the MCP stdio bridge must never write anything but JSON-RPC
//! to stdout. A `tracing_subscriber::fmt()` writer left on its stdout default once
//! interleaved log lines into the protocol stream, breaking every request a real
//! MCP client (Vercel AI SDK, `@modelcontextprotocol/sdk`) sent — logs must go to
//! stderr only.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn stdout_carries_only_json_rpc_lines() {
    let bin = env!("CARGO_BIN_EXE_filigrio-mcp");
    // A `TempDir` path, not a fixed `/tmp` name (audit §K4): nothing binds this
    // socket — `initialize` is answered without a daemon — but a shared literal
    // is still a collision waiting for two concurrent runs of the suite.
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("filigrio-daemon.sock");
    let mut child = Command::new(bin)
        .args([
            "--verbose",
            "--socket",
            socket.to_str().expect("utf-8 temp path"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn filigrio-mcp");

    let request = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{request}\n").as_bytes())
        .expect("write request");

    let output = child.wait_with_output().expect("wait for child");
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");

    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one JSON-RPC line on stdout, got: {lines:?}"
    );
    let parsed: serde_json::Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("stdout line is not valid JSON ({e}): {:?}", lines[0]));
    assert_eq!(parsed["id"], 1);
    assert!(
        parsed.get("result").is_some(),
        "expected a result field: {parsed}"
    );
}
