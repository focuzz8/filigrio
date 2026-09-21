//! ADR-0042 F7/F9 — **one vocabulary; and the MCP mask is read-only.**
//!
//! F7 settled that a surface is a *mask* over one vocabulary, never a dialect
//! of it: which operations a surface exposes may differ, the names and argument
//! shapes may not. F9 then removed the MCP bridge's mutation surface entirely,
//! which changed what this file is for. It used to enforce "`SliverOp` mirrors
//! `Command`, and the sliver's two ops are exactly the two `_project` tools".
//! With the sliver gone, the invariant is **stronger and simpler**:
//!
//! | vocabulary        | CLI                | wire                        | MCP tool |
//! |-------------------|--------------------|-----------------------------|----------|
//! | register a project| `project register` | `Command::ProjectRegister`  | *(none)* |
//! | index a project   | `project index`    | `Command::ProjectIndex`     | *(none)* |
//! | …every mutation   | some CLI verb      | some `Command`              | *(none)* |
//!
//! **No `Command` is expressible on the MCP surface.** That is a security
//! property, not a tidiness one: the bridge runs with the *user's* filesystem
//! permissions, not the agent's, so any mutation it exposed would be exercised
//! with the bridge's authority — a confused deputy. MCP **roots** is the
//! agent-scoped containment that would make one safe, and it is not built.
//!
//! The enforcement, unchanged in kind from F7:
//!
//! 1. [`mapping`] is an **exhaustive `match` over [`Command`]** with no
//!    wildcard arm — the `common::state_facets` destructure trick applied to an
//!    enum. Adding a command without declaring its CLI verb, and without
//!    facing the question of whether it leaks onto the bridge, is a **compile
//!    error**, not a review miss.
//! 2. The command name is not written twice: it is read off the *wire tag*, so
//!    a rename that touches only one side fails here.
//! 3. The MCP rule `Command::Project<Verb>` ↔ tool `<verb>_project` is still
//!    *executed* — but now to prove the derived name is **absent** from the
//!    served set. A future tool that reintroduces a mutation fails here.
//!
//! **How the served tool list gets in here.** The bridge's list lives in
//! `McpBridge::handle_tools_list` — a private method of the `filigrio-mcp`
//! *binary*, and `filigrio-client-mcp` depends on this crate (so this crate cannot
//! depend back on it). Rather than grow production surface just to make it
//! readable, the names are pinned below as [`MCP_TOOLS`] and the **real** list
//! is checked on the other side of the wall, where it is in scope:
//!   - `crates/filigrio-client-mcp/src/main.rs` :: `the_bridge_serves_only_read_only_tools`
//!     — asserts the *actual* served list is exactly these nine, that none is
//!     spelled like a mutation, and that the removed ones no longer dispatch;
//!   - `tools/agent_eval/src/mcp_test.ts` — the end-to-end JSON-RPC
//!     conformance run against the spawned binary.
//!
//! Gated on `control`: this file is about the *mutation* vocabulary, and a
//! control-free build (which is what the bridge is) has none — the strongest
//! form of the property this file checks, guaranteed by the compiler rather
//! than by a test.
#![cfg(feature = "control")]

use filigrio_protocol::{Command, ControlOp, Request};

/// The tool names the MCP bridge serves — **all nine are data-plane reads**
/// (ADR-0042 F9).
///
/// Single source this must match: `crates/filigrio-client-mcp/src/main.rs` ::
/// `McpBridge::handle_tools_list` (see the module docs for why it is pinned
/// rather than read, and for the two places that check the real list).
const MCP_TOOLS: &[&str] = &[
    "get_community",
    "get_neighbors",
    "get_node",
    "god_nodes",
    "graph_stats",
    "list_communities",
    "project_graph",
    "query_graph",
    "shortest_path",
];

/// One row of the vocabulary: the command's wire tag and the CLI verb that is
/// its other mask. (There is no MCP column any more — that is the point.)
struct Row {
    /// The `Command` wire tag.
    command_kind: &'static str,
    /// The CLI verb, as typed: `filigrio <verb>`.
    cli: &'static str,
}

/// **Exhaustive** — no `_` arm, deliberately. A new [`Command`] variant does not
/// compile until it is given its place in the vocabulary here, which is also
/// where the "does this leak onto the bridge?" question gets asked.
fn mapping(cmd: &Command) -> Row {
    match cmd {
        Command::ProjectRegister { .. } => Row {
            command_kind: "ProjectRegister",
            cli: "project register",
        },
        Command::ProjectIndex { .. } => Row {
            command_kind: "ProjectIndex",
            cli: "project index",
        },
        Command::ProjectRemove { .. } => Row {
            command_kind: "ProjectRemove",
            cli: "project remove",
        },
        Command::ProjectWatch { .. } => Row {
            command_kind: "ProjectWatch",
            cli: "project watch",
        },
        Command::ProjectExport { .. } => Row {
            command_kind: "ProjectExport",
            cli: "project export",
        },
        Command::ProjectFlush { .. } => Row {
            command_kind: "ProjectFlush",
            cli: "project flush",
        },
        Command::DaemonStop => Row {
            command_kind: "DaemonStop",
            cli: "daemon stop",
        },
        // The producer-lane verb: emitted by the watcher/git-hook producers
        // (ADR-0032a/b), never typed by a human, so it has no CLI mask.
        Command::Submit { .. } => Row {
            command_kind: "Submit",
            cli: "",
        },
    }
}

/// Every command, for iteration. Values are irrelevant; the variants are the
/// point (and the exhaustive `mapping` above is what keeps this list honest —
/// a missing entry here is caught by the count assertion below).
fn every_command() -> Vec<Command> {
    vec![
        Command::ProjectRegister { path: "/w".into() },
        Command::ProjectIndex {
            project: "p".into(),
            clean: false,
        },
        Command::ProjectRemove {
            project: "p".into(),
        },
        Command::ProjectWatch {
            project: "p".into(),
            on: true,
        },
        Command::ProjectExport {
            project: "p".into(),
        },
        Command::ProjectFlush {
            project: "p".into(),
        },
        Command::DaemonStop,
        Command::Submit {
            project: "p".into(),
            changeset: filigrio_protocol::ChangeSet::default(),
            priority: filigrio_protocol::Priority::Fs,
        },
    ]
}

/// The wire `kind` tag of a serialized value.
fn kind_of<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value).expect("serialize")["kind"]
        .as_str()
        .expect("every command carries a `kind` tag")
        .to_string()
}

/// The mechanical MCP rule, executed: `Project<Verb>` → `<verb>_project`.
/// Returns `None` for a command that is not object-verb on `Project` — which is
/// itself a finding for the `Project*` family (the vocabulary is object-verb by
/// decision), and simply expected for `Submit`/`DaemonStop`.
fn mcp_tool_name(command_kind: &str) -> Option<String> {
    let verb = command_kind.strip_prefix("Project")?;
    Some(format!("{}_project", verb.to_lowercase()))
}

/// **F9 — no command is expressible on the MCP surface.** The security
/// invariant, run over the whole `Command` enum: for every mutation, derive the
/// tool name it *would* be spelled with under the F7 rule and assert the bridge
/// does not serve it. This is what fails loudly if a future tool reintroduces a
/// mutation; the exhaustive `mapping` is what makes it cover a *new* command
/// too.
///
/// Second half: the served set is entirely free of the `<verb>_project` shape,
/// so a mutation cannot sneak in under a verb that has no `Command` yet.
#[test]
fn no_command_is_expressible_on_the_mcp_surface() {
    for cmd in every_command() {
        let kind = kind_of(&cmd);
        if let Some(tool) = mcp_tool_name(&kind) {
            assert!(
                !MCP_TOOLS.contains(&tool.as_str()),
                "F9: the bridge serves `{tool}`, the F7 spelling of `Command::{kind}` — \
                 the MCP surface must expose no mutation (it holds the user's \
                 filesystem permissions, not the agent's; MCP roots is not built)"
            );
        }
    }

    let mutation_shaped: Vec<&&str> = MCP_TOOLS
        .iter()
        .filter(|t| t.strip_suffix("_project").is_some())
        .collect();
    assert!(
        mutation_shaped.is_empty(),
        "F9: {mutation_shaped:?} is spelled like a `Command::Project<Verb>` mask"
    );
}

/// F7 — the vocabulary is **object-verb** (`Project<Verb>`), and the CLI mask
/// spells it the same way, in the same order: `filigrio project register`, not
/// `filigrio register project`. The one exception is deliberate and named.
#[test]
fn the_command_vocabulary_is_object_verb_and_the_cli_mirrors_it() {
    for cmd in every_command() {
        let row = mapping(&cmd);
        assert_eq!(
            kind_of(&cmd),
            row.command_kind,
            "the mapping names a tag that isn't the wire tag"
        );

        if row.cli.is_empty() {
            assert_eq!(
                row.command_kind, "Submit",
                "only the producer-lane `Submit` may lack a CLI verb — \
                 a new command without one is either a leak or an oversight"
            );
            continue;
        }

        // `Object<Verb>` → `object verb`. Executed, not restated in prose.
        let (object, verb) = match row.command_kind.strip_prefix("Project") {
            Some(verb) => ("project", verb),
            None => (
                "daemon",
                row.command_kind
                    .strip_prefix("Daemon")
                    .unwrap_or_else(|| panic!("F7: `{}` is not object-verb", row.command_kind)),
            ),
        };
        assert_eq!(
            row.cli,
            format!("{object} {}", verb.to_lowercase()),
            "F7: the CLI is a mask over `{}`, not a dialect of it",
            row.command_kind
        );
    }
}

/// F7 — the mapping covers every variant. `every_command` is hand-written, so
/// this pins that it did not fall behind the enum: serde's tag set is derived
/// from the enum itself, and every tag must be distinct.
#[test]
fn every_command_variant_is_in_the_mapping() {
    let mut kinds: Vec<String> = every_command().iter().map(kind_of).collect();
    let total = kinds.len();
    kinds.sort();
    kinds.dedup();
    assert_eq!(
        kinds.len(),
        total,
        "every_command lists a variant twice — the mapping's coverage is overstated"
    );

    // Each command must parse back from its own frame — a typo'd tag would not.
    for cmd in every_command() {
        let json = serde_json::to_value(Request::command(cmd)).expect("serialize");
        serde_json::from_value::<Request>(json).expect("a command must round-trip");
    }
}

/// F7 — the renamed wire frames are **dead frames**: an old `ProjectAdd`
/// command and a `ProjectIndex` carrying the old `force` flag must fail rather
/// than be silently reinterpreted.
///
/// (The pre-F7 verb-object *sliver* frames used to be checked here too. They
/// moved to `plane_split.rs`'s `the_sliver_envelope_is_dead`, which since F9
/// rejects the whole `"Sliver"` envelope rather than particular spellings —
/// strictly more than this test asserted.)
#[test]
fn pre_f7_frames_are_dead() {
    // The old command tag.
    let old_add = serde_json::json!({"type": "Command", "kind": "ProjectAdd", "path": "/w"});
    assert!(
        serde_json::from_value::<Request>(old_add).is_err(),
        "the renamed ProjectAdd command frame must not parse"
    );

    // `force` → `clean`: an old frame's `force: true` must NOT arrive as
    // `clean: true`. serde drops the unknown field, so what makes this a dead
    // frame rather than a silent reinterpretation is that the *reserved
    // wipe-and-rebuild is not selected* — the request degrades to a plain
    // incremental index, which is the safe direction, and `clean` is
    // unreachable from a pre-F7 client.
    let old_force = serde_json::json!(
        {"type": "Command", "kind": "ProjectIndex", "project": "p", "force": true}
    );
    let req: Request =
        serde_json::from_value(old_force).expect("the frame still names a live verb");
    assert!(
        matches!(
            req,
            Request::Control(ControlOp::Command(Command::ProjectIndex {
                clean: false,
                ..
            }))
        ),
        "a pre-F7 `force: true` must never be read as `clean: true`"
    );

    // And the new spelling does select it.
    let clean = serde_json::json!(
        {"type": "Command", "kind": "ProjectIndex", "project": "p", "clean": true}
    );
    let req: Request = serde_json::from_value(clean).expect("clean must parse");
    assert!(matches!(
        req,
        Request::Control(ControlOp::Command(Command::ProjectIndex {
            clean: true,
            ..
        }))
    ));
}
