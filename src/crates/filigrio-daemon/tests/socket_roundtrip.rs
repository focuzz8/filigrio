//! ADR-0032 §3 — end-to-end socket round-trip.
//!
//! These tests drive the *public* surface exactly as a real client would: a
//! `SocketClient` sends a contract `Request` over a unix-domain socket to a
//! `Daemon` served by `Daemon::serve_socket`, and we assert on the `Response`
//! that comes back. Nothing here reaches into private internals except reading
//! daemon state *after* the server thread has finished, to check side effects.
//!
//! Every test is anchored to a concrete fragility of the transport, called out
//! in its doc comment — the point is not "does a message move" but "does the
//! move survive the things that actually break length-prefixed socket IPC."

use filigrio_core::ChangeSet;
use filigrio_daemon::{Daemon, DaemonConfig, Priority, Project, Request, Response};
use filigrio_protocol::{CommandOutcome, DaemonClientTrait, DataQuery, MetaQuery, SocketClient};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tempfile::TempDir;

/// Build a daemon (wrapped for sharing) with `n` projects pre-registered, plus
/// a scratch temp dir to host the socket. Projects are registered directly
/// rather than through `ProjectRegister` so these tests exercise the transport
/// alone, with no command semantics in the way.
fn daemon_with_projects(n: usize) -> (Arc<Mutex<Daemon>>, TempDir, Vec<String>) {
    let scratch = TempDir::new().unwrap();
    // Keep this daemon's marker files off the shared /tmp defaults so parallel
    // tests never collide on them.
    let cfg = DaemonConfig {
        shutdown_marker_path: scratch.path().join("daemon.clean"),
        socket_path: scratch.path().join("unused.sock"),
        ..Default::default()
    };

    let daemon = Daemon::new(cfg);
    let mut ids = Vec::new();
    for i in 0..n {
        let root = scratch.path().join(format!("proj{i}"));
        std::fs::create_dir_all(&root).unwrap();
        let project = Project::new(root);
        ids.push(project.id.clone());
        daemon.registry.lock().add(project).unwrap();
    }
    (Arc::new(Mutex::new(daemon)), scratch, ids)
}

/// As [`daemon_with_projects`], but each project holds one real source file and
/// an output dir — needed now that a `Submit` actually *applies* before
/// replying (ADR-0042 F6c) rather than parking in a queue.
fn daemon_with_projects_with_source(n: usize) -> (Arc<Mutex<Daemon>>, TempDir, Vec<String>) {
    let scratch = TempDir::new().unwrap();
    let cfg = DaemonConfig {
        shutdown_marker_path: scratch.path().join("daemon.clean"),
        socket_path: scratch.path().join("unused.sock"),
        ..DaemonConfig::default()
    };

    let daemon = Daemon::new(cfg);
    let mut ids = Vec::new();
    for i in 0..n {
        let root = scratch.path().join(format!("proj{i}"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();
        let project = Project::new(root);
        std::fs::create_dir_all(&project.output_dir).unwrap();
        ids.push(project.id.clone());
        daemon.registry.lock().add(project).unwrap();
    }
    (Arc::new(Mutex::new(daemon)), scratch, ids)
}

/// Spawn the socket server on a background thread, bounded to `max` connections
/// so the thread terminates deterministically (no leaked `accept()`-blocked
/// thread). Returns the socket path and the join handle.
fn spawn_server(daemon: Arc<Mutex<Daemon>>, dir: &Path, max: usize) -> (PathBuf, JoinHandle<()>) {
    let socket_path = dir.join("d.sock");
    let sp = socket_path.clone();
    let handle = thread::spawn(move || {
        Daemon::serve_socket(daemon, sp, Some(max)).expect("serve_socket failed");
    });
    wait_for_socket(&socket_path);
    (socket_path, handle)
}

/// The server binds *inside* the spawned thread, so a client that connects
/// immediately can race the `bind`. Poll for the socket file (created by
/// `bind`) before handing the path to a client.
fn wait_for_socket(path: &Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("socket never appeared at {}", path.display());
}

fn client(path: &Path) -> SocketClient {
    SocketClient::new(path).with_timeout(Duration::from_secs(5))
}

/// FRAGILITY: the whole round-trip — bind, length-prefix framing in both
/// directions, serde encode/decode, and the query→`QueryResult` mapping. If any
/// of those disagree, a query cannot return real state. This is the baseline
/// that proves the transport is genuinely wired (not the daemon dead code it
/// replaced).
#[test]
fn health_query_round_trips_real_state() {
    let (daemon, dir, _ids) = daemon_with_projects(2);
    let (path, handle) = spawn_server(daemon, dir.path(), 1);

    let health = client(&path).health().expect("health round-trip failed");
    assert_eq!(
        health.project_count, 2,
        "the health response must carry the daemon's real registry count, \
         proving state — not a stub — crossed the socket"
    );

    handle.join().unwrap();
}

/// FRAGILITY: the response must not lie. A `Submit` now executes to completion
/// before replying (ADR-0042 F6c), so the outcome must describe **work that
/// already happened** — and nothing may be left queued for a drain that this
/// test never runs. Pins the side effect (the graph, and an empty queue), not
/// merely that a reply came back.
#[test]
fn submit_command_executes_before_replying() {
    let (daemon, dir, ids) = daemon_with_projects_with_source(1);
    let (path, handle) = spawn_server(daemon.clone(), dir.path(), 1);

    let outcome = client(&path)
        .submit(
            ids[0].clone(),
            ChangeSet {
                added: vec!["src/lib.rs".into()],
                modified: vec![],
                removed: vec![],
            },
        )
        .expect("submit round-trip failed");
    handle.join().unwrap();

    let CommandOutcome::Applied { changed, .. } = outcome else {
        panic!("a Submit must return an Applied outcome, got {outcome:?}");
    };
    assert_eq!(
        changed, 1,
        "the outcome must report the file it actually applied"
    );

    let d = daemon.lock();
    assert_eq!(
        d.health().queue_depth,
        0,
        "a wire command must never enter the queue (producer lane only, ADR-0042 F6c)"
    );
    assert!(
        d.project_status(&ids[0]).unwrap().node_count > 0,
        "the apply must be complete by the time the response arrives"
    );
}

/// FRAGILITY: an outcome that reports work it didn't do. Two identical submits
/// must report *different* change counts — the first applies the file, the
/// second is absorbed by the dedup gate — so the count is real per-call
/// information, not a constant stamped onto every reply (which is exactly what
/// the old `job-N` ack degenerated into once nobody could poll it).
#[test]
fn repeated_commands_report_their_own_real_outcomes() {
    let (daemon, dir, ids) = daemon_with_projects_with_source(1);
    let (path, handle) = spawn_server(daemon, dir.path(), 2);

    let cs = || ChangeSet {
        added: vec!["src/lib.rs".into()],
        modified: vec![],
        removed: vec![],
    };
    let first = client(&path).submit(ids[0].clone(), cs()).unwrap();
    let second = client(&path).submit(ids[0].clone(), cs()).unwrap();
    handle.join().unwrap();

    let (CommandOutcome::Applied { changed: c1, .. }, CommandOutcome::Applied { changed: c2, .. }) =
        (&first, &second)
    else {
        panic!("expected two Applied outcomes, got {first:?} / {second:?}");
    };
    assert_eq!(*c1, 1, "the first submit indexes the file");
    assert_eq!(
        *c2, 0,
        "the second is deduped away — the outcome must say so, not repeat the first"
    );
}

/// FRAGILITY: the error path must round-trip as a structured `Response::Error`
/// carrying a diagnostic — not a panic, not a dropped connection, and not a
/// generic "not implemented" that hides *which* thing failed. A query for an
/// unregistered project is the canonical honest-failure case.
#[test]
fn unknown_project_status_returns_structured_error() {
    let (daemon, dir, _ids) = daemon_with_projects(1);
    let (path, handle) = spawn_server(daemon, dir.path(), 1);

    let resp = client(&path)
        .send(Request::data(DataQuery::Status {
            project: Some("does-not-exist".to_string()),
        }))
        .expect("the transport itself must succeed even when the query fails");
    handle.join().unwrap();

    match resp {
        Response::Error { message } => assert!(
            message.contains("does-not-exist"),
            "the error must name the missing project, got: {message}"
        ),
        other => panic!("expected Response::Error for an unknown project, got {other:?}"),
    }
}

/// FRAGILITY: framing desync after an error. A length-prefixed protocol is
/// fragile precisely at the error boundary — if an error response writes the
/// wrong length (or nothing), the *next* request on the accept loop reads a
/// misaligned stream and the daemon wedges. We send a failing request, then a
/// normal one, and require the second to succeed on a fresh connection.
#[test]
fn accept_loop_survives_an_error_response() {
    let (daemon, dir, _ids) = daemon_with_projects(3);
    let (path, handle) = spawn_server(daemon, dir.path(), 2);

    // Request 1: guaranteed error (unknown project).
    let err = client(&path)
        .send(Request::data(DataQuery::Status {
            project: Some("ghost".to_string()),
        }))
        .unwrap();
    assert!(matches!(err, Response::Error { .. }));

    // Request 2 (new connection): must still work — the loop and framing survived.
    let health = client(&path)
        .health()
        .expect("health after an error failed");
    assert_eq!(health.project_count, 3);

    handle.join().unwrap();
}

/// FRAGILITY: `read_exact` across a buffer boundary. A payload larger than the
/// socket buffer (~64KB) is delivered in multiple chunks; naive one-shot reads
/// would truncate it. A `Submit` carrying thousands of paths forces the
/// multi-read framing path, and we assert the command still decodes and runs
/// intact.
///
/// The 5000 paths are deliberately **gitignored**, so the scope gate drops them
/// before anything is read from disk and the outcome truthfully reports
/// `changed: 0`. (Naming 5000 *in-scope but nonexistent* files instead would
/// now report `vanished: 5000` under ADR-0042 F8 — it used to fail the command
/// with an I/O error, which is how the vanished-file race was found — but
/// either way that would make this a test of the F8 fold, not of framing.)
#[test]
fn large_changeset_survives_multi_read_framing() {
    let (daemon, dir, ids) = daemon_with_projects_with_source(1);
    std::fs::write(dir.path().join("proj0/.gitignore"), "vendor/\n").unwrap();
    let (path, handle) = spawn_server(daemon.clone(), dir.path(), 1);

    // ~5000 paths → well over 64KB once serialized, forcing chunked reads.
    let big = ChangeSet {
        added: (0..5000)
            .map(|i| format!("vendor/module_{i}/file_{i}.rs"))
            .collect(),
        modified: vec![],
        removed: vec![],
    };
    let payload_est: usize = big.added.iter().map(|s| s.len() + 4).sum();
    assert!(
        payload_est > 64 * 1024,
        "payload must exceed a socket buffer to be a real test"
    );

    let outcome = client(&path)
        .submit(ids[0].clone(), big)
        .expect("large submit round-trip failed");
    handle.join().unwrap();

    assert!(
        matches!(outcome, CommandOutcome::Applied { changed: 0, .. }),
        "a large changeset must arrive intact and report its real (gated) outcome, got {outcome:?}"
    );
    assert_eq!(
        daemon.lock().health().queue_depth,
        0,
        "the wire command executed at ingress; nothing may be left queued"
    );
}

/// FRAGILITY: connection-per-request. `SocketClient` opens a fresh connection
/// per `send`, and the server handles one request per connection then loops back
/// to `accept()`. If the accept loop doesn't actually continue, only the first
/// request works. Three back-to-back sends must all succeed.
#[test]
fn sequential_requests_each_get_a_fresh_connection() {
    let (daemon, dir, _ids) = daemon_with_projects(1);
    let (path, handle) = spawn_server(daemon, dir.path(), 3);

    for _ in 0..3 {
        let health = client(&path).health().expect("a sequential health failed");
        assert_eq!(health.project_count, 1);
    }

    handle.join().unwrap();
}

/// FRAGILITY: response-variant mapping. A command must map to
/// `CommandCompleted` and a (meta) read to `QueryResult` — swapping them makes
/// a correct-looking daemon return replies the typed client rejects as
/// "unexpected response". We assert the raw variant for one of each, below the
/// convenience helpers.
#[test]
fn command_and_query_map_to_the_right_response_variants() {
    let (daemon, dir, ids) = daemon_with_projects_with_source(1);
    let (path, handle) = spawn_server(daemon, dir.path(), 2);

    let cmd_resp = client(&path)
        .send(Request::command(filigrio_daemon::Command::Submit {
            project: ids[0].clone(),
            changeset: ChangeSet::default(),
            priority: Priority::Fs,
        }))
        .unwrap();
    assert!(
        matches!(cmd_resp, Response::CommandCompleted { .. }),
        "a Command must return CommandCompleted, got {cmd_resp:?}"
    );

    let qry_resp = client(&path)
        .send(Request::meta(MetaQuery::Health))
        .unwrap();
    assert!(
        matches!(qry_resp, Response::QueryResult { .. }),
        "a Query must return QueryResult, got {qry_resp:?}"
    );

    handle.join().unwrap();
}
