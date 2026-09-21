//! The capability doc must advertise exactly the tools this bridge serves —
//! asked of the bridge itself, over the wire.
//!
//! ADR-0034 §4 promises one truth for "how to drive filigrio". The installed
//! `SKILL.md` / `AGENTS.md` list tools from `filigrio_install::capability::TOOLS`,
//! a documentation-side copy: the installer links no filigrio crate at all
//! (`filigrio-install/tests/dependency_hygiene.rs`), so it cannot import the
//! bridge's list. If the copy drifts, an agent is told to call a tool nobody
//! serves, gets an error, and the user reads that as "filigrio is broken".
//!
//! **This guard used to live in `filigrio-install` and worked by scraping this
//! crate's `src/main.rs` for `"name": "` inside `handle_tools_list`.** That
//! tested a source approximation: a refactor to a builder or a macro would have
//! silently weakened it to whatever still matched, and it passed with a clean
//! green while the real surface moved. So it lives here now and spawns the real
//! binary, speaks real JSON-RPC, and reads the real `tools/list` — the thing a
//! client actually receives, whatever shape the code that produces it takes.
//!
//! **It is here because `filigrio-install` is a `[dev-dependency]` of this
//! crate, never a `[dependency]`.** The arrow only points this way: a dev
//! dependency is not linked into the shipped bridge, so the installer's
//! "standalone, links nothing" invariant survives untouched. Reversing it — a
//! `filigrio-client-mcp` dependency in `filigrio-install` — would drag
//! `filigrio-core` into the engine-free CLI and break `dependency_hygiene.rs`.
//!
//! **If the bridge moves, move this guard with it — do not delete it.** The
//! capability doc's tool list has no other check, and deleting the test is the
//! cheapest-looking fix the next time it goes red. It is also always the wrong
//! one: red here means the doc and the bridge already disagree.
//!
//! Distinct from `protocol.rs`, which drives the in-process `McpServer` library
//! type. That server exposes eight tools; the binary serves nine
//! (`list_communities` is bridge-only). The doc describes the *binary*, so this
//! must ask the binary.

use std::io::Write;
use std::process::{Command, Stdio};

/// Tool names from a real `tools/list`, over stdio, against the real binary.
///
/// No daemon is needed: `initialize` and `tools/list` are answered from static
/// data, and the socket is never connected. It still gets a `TempDir` path
/// rather than a fixed `/tmp` name, matching `stdout_purity.rs` — nothing binds
/// it, but a shared literal is a collision waiting for two concurrent runs.
fn tools_advertised_on_the_wire() -> Vec<String> {
    let bin = env!("CARGO_BIN_EXE_filigrio-mcp");
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("filigrio-daemon.sock");

    let mut child = Command::new(bin)
        .args(["--socket", socket.to_str().expect("utf-8 temp path")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn filigrio-mcp");

    // A conformant client initializes before it lists; do the same, so this
    // exercises the sequence a real client sends rather than a shortcut.
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"capability-doc-drift","version":"0"}}}"#;
    let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(format!("{init}\n{list}\n").as_bytes())
        .expect("write requests");
    // `stdin` dropped here — the bridge sees EOF and exits, so this cannot hang.

    let output = child.wait_with_output().expect("wait for filigrio-mcp");
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    let response = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["id"] == serde_json::json!(2))
        .unwrap_or_else(|| {
            panic!(
                "the bridge answered no `tools/list` (id 2). stdout: {stdout:?}\nstderr: {stderr}"
            )
        });

    let tools = response["result"]["tools"].as_array().unwrap_or_else(|| {
        panic!("`tools/list` must return result.tools as an array, got: {response}")
    });

    tools
        .iter()
        .map(|t| {
            t["name"]
                .as_str()
                .unwrap_or_else(|| panic!("every tool carries a string `name`, got: {t}"))
                .to_string()
        })
        .collect()
}

#[test]
fn the_capability_doc_advertises_exactly_the_bridges_tools() {
    let mut advertised = tools_advertised_on_the_wire();
    // An empty list on both sides would agree vacuously. It is never correct:
    // a bridge that serves no tools is a bridge nobody can drive.
    assert!(
        !advertised.is_empty(),
        "the bridge advertised no tools at all — that is a broken bridge, not a \
         doc that has nothing to say"
    );

    let mut documented: Vec<String> = filigrio_install::capability::TOOLS
        .iter()
        .map(|t| t.name.to_string())
        .collect();

    advertised.sort();
    documented.sort();
    assert_eq!(
        documented, advertised,
        "filigrio_install::capability::TOOLS has drifted from the bridge's real \
         tools/list. Update the list (and, if a tool's meaning changed, \
         filigrio-install/assets/capability/body.md) in the same change as the bridge."
    );
}

/// Each documented tool carries a summary the doc can actually print.
///
/// The name match above is the load-bearing half, but an empty summary renders
/// a blank cell in `SKILL.md`'s tool table — a tool the agent can see and has no
/// reason to reach for. Cheap to hold here, next to the list it describes.
#[test]
fn every_documented_tool_has_a_summary_to_render() {
    for tool in filigrio_install::capability::TOOLS {
        assert!(
            !tool.summary.trim().is_empty(),
            "{} has no summary — it would render as an empty cell in the capability doc",
            tool.name
        );
    }
}

// Not asserted here: that every advertised name is also *dispatched* by
// `handle_tool_call` — the failure mode the old source-scraper structurally
// could not see. It needs a daemon. Every handler opens with `self.get_client()?`,
// and `get_client` **auto-starts** a daemon when nothing is listening (ADR-0032
// §1), spawning the `filigrio-daemon` sitting next to the test binary in
// `target/debug`. A `tools/call` probe would therefore start and index a real
// daemon as a side effect of running the suite. If the bridge ever grows a
// pre-dispatch validation step — or a `--no-autostart` — a "tool not allowed:
// {name}" probe becomes cheap and belongs right here.
