//! ADR-0032f §2 — the daemon funnel routes by *plane*.
//!
//! The type-level split (`DataQuery` / `ControlOp`) is only safe if the daemon
//! routes each plane to the same handlers the flat contract used:
//! - `Data` → the responder (or the daemon's own warm `Status` path);
//! - `Control` mutations → the synchronous command path (ADR-0042 F6c);
//! - `Control` meta reads (`Health`) → answered by the daemon itself.
//!
//! There was a third route until ADR-0042 F9: `Sliver` → widened to the
//! equivalent `Command`. Two tests here pinned that widening
//! (`sliver_register_routes_to_the_project_register_handler`,
//! `sliver_index_behaves_identically_to_project_index`); their whole subject
//! was the sliver-vs-control *equivalence*, which cannot drift once there is
//! only one route, so they are deleted rather than re-pointed. What they
//! asserted about the surviving control path is pinned elsewhere, unchanged:
//! register lands in the registry and starts no watcher —
//! `worker_pool.rs::guard_project_register_registers_without_building_or_watching`
//! (over the socket) and `registry_persist.rs`'s r6/r8; an index completes at
//! ingress and queues nothing —
//! `apply_gate.rs::a_wire_index_completes_before_the_response_and_queues_nothing`.

use filigrio_daemon::{
    Command, Daemon, DaemonConfig, DataQuery, MetaQuery, Project, Request, Response,
};
use tempfile::TempDir;

/// A daemon whose registry/marker/socket paths are isolated to `scratch` —
/// registration persists the registry, and that write must never land in the
/// real `~/.config` (see the registry-persist test isolation rule).
fn isolated_daemon(scratch: &TempDir) -> Daemon {
    let cfg = DaemonConfig {
        registry_path: scratch.path().join("registry.json"),
        shutdown_marker_path: scratch.path().join("daemon.clean"),
        socket_path: scratch.path().join("unused.sock"),
        ..Default::default()
    };
    Daemon::new(cfg)
}

/// A project dir with one indexable source file.
fn project_dir(scratch: &TempDir, name: &str) -> std::path::PathBuf {
    let root = scratch.path().join(name);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();
    root
}

/// Register `root` directly (bypassing the async command path), as the
/// transport tests do.
fn register(daemon: &mut Daemon, root: &std::path::Path) -> String {
    let project = Project::new(root.to_path_buf());
    std::fs::create_dir_all(&project.output_dir).unwrap();
    let id = project.id.clone();
    daemon.registry.lock().add(project).unwrap();
    id
}

/// ADR-0042 F6c (carried from the retired 0032g draft) — a pipeline/apply
/// **error** during a sync command reaches the client as a typed
/// `Response::Error`. Before F6c this failure was logged daemon-side behind a
/// `CommandAccepted` ack and dropped: the client saw success and exited 0.
/// (The panic half of the same contract is pinned by `daemon.rs`'s
/// `a_panicking_command_becomes_a_typed_error_response`, which drives the very
/// `catch_command_panic` wrapper the sync path uses.)
#[test]
fn an_apply_error_reaches_the_client_as_a_typed_error() {
    let scratch = TempDir::new().unwrap();
    let root = project_dir(&scratch, "broken");
    let mut daemon = isolated_daemon(&scratch);
    let project = Project::new(root.clone());
    let id = project.id.clone();
    daemon.registry.lock().add(project).unwrap();

    // Make the output dir un-creatable: a plain FILE where `.filigrio-out/`
    // must be. Every persist path then fails with a real I/O error.
    std::fs::write(root.join(".filigrio-out"), b"not a directory").unwrap();

    let resp = daemon.handle_request(Request::command(Command::ProjectIndex {
        project: id.clone(),
        clean: false,
    }));
    assert!(
        matches!(resp, Response::Error { .. }),
        "an apply failure must reach the client as a typed error, got {resp:?}"
    );
}

/// A data-plane query is delegated to the responder — the error for an unknown
/// project is the responder's, naming the project, not a routing failure.
#[test]
fn data_query_routes_to_the_responder() {
    let scratch = TempDir::new().unwrap();
    let mut daemon = isolated_daemon(&scratch);

    let resp = daemon.handle_request(Request::data(DataQuery::GetNode {
        project: "ghost".to_string(),
        node_address: Default::default(),
    }));
    let Response::Error { message } = resp else {
        panic!("expected the responder's structured error, got {resp:?}");
    };
    assert!(
        message.contains("ghost"),
        "the responder must name the unknown project, got: {message}"
    );
}

/// Health is a control-plane meta read answered by the daemon itself — real
/// registry state, `QueryResult` variant.
#[test]
fn health_meta_read_is_answered_by_the_daemon() {
    let scratch = TempDir::new().unwrap();
    let root = project_dir(&scratch, "p");
    let mut daemon = isolated_daemon(&scratch);
    register(&mut daemon, &root);

    let resp = daemon.handle_request(Request::meta(MetaQuery::Health));
    let Response::QueryResult { data } = resp else {
        panic!("Health must map to QueryResult, got {resp:?}");
    };
    assert_eq!(
        data["project_count"], 1,
        "Health must carry the daemon's real registry count"
    );
}
