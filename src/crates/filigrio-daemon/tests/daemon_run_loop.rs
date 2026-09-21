//! ADR-0032 §3 — `serve_socket` folded into `Daemon::run`'s tokio select loop.
//!
//! The round-trip tests (`socket_roundtrip.rs`) prove one request in / one
//! response out against a bounded server. This file proves the thing that makes
//! the daemon a daemon: the **live `run()` loop serves the socket AND drains the
//! priority queue concurrently** — a client can submit a command and watch the
//! same running loop apply it, all while the loop stays responsive to further
//! queries. It also proves the loop shuts down deterministically on request.

use filigrio_core::ChangeSet;
use filigrio_daemon::{Daemon, DaemonConfig, Project};
use filigrio_protocol::{DaemonClientTrait, SocketClient};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;

/// Run one blocking client call off the async runtime (SocketClient uses blocking
/// std sockets, so it must not run on a runtime worker thread).
async fn on_client<T, F>(path: &Path, f: F) -> T
where
    F: FnOnce(SocketClient) -> T + Send + 'static,
    T: Send + 'static,
{
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let client = SocketClient::new(&p).with_timeout(Duration::from_secs(5));
        f(client)
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_loop_serves_socket_and_drains_queue_concurrently() {
    let scratch = TempDir::new().unwrap();
    let socket_path = scratch.path().join("d.sock");

    // A registered project with a real source file, so an apply does real work.
    let root = scratch.path().join("proj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let project = Project::new(root.clone());
    std::fs::create_dir_all(&project.output_dir).unwrap();
    let project_id = project.id.clone();

    let cfg = DaemonConfig {
        socket_path: socket_path.clone(),
        shutdown_marker_path: scratch.path().join("daemon.clean"),
        registry_path: scratch.path().join("registry.json"), // isolate from real ~/.config
        idle_timeout: None,                                  // don't idle-exit mid-test
        ..Default::default()
    };

    let daemon = Daemon::new(cfg);
    daemon.registry.lock().add(project).unwrap();

    // Launch the real run loop with a test-controllable shutdown.
    let shutdown = Arc::new(Notify::new());
    let loop_handle = tokio::spawn(daemon.run_until(shutdown.clone()));

    // Wait for the loop to bind the socket (it binds only after startup reconcile).
    let mut bound = false;
    for _ in 0..300 {
        if socket_path.exists() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(bound, "the run loop never bound its socket");

    // 1) The loop serves queries while running.
    let health = on_client(&socket_path, |c| c.health())
        .await
        .expect("health while the loop runs");
    assert_eq!(health.project_count, 1);

    // 2) Submit a command over the socket → executed to completion by the same
    //    loop before the response comes back (ADR-0042 F6c), on a connection
    //    task that must NOT have held the daemon lock while applying.
    let pid = project_id.clone();
    let outcome = on_client(&socket_path, move |c| {
        c.submit(
            pid,
            ChangeSet {
                added: vec!["src/main.rs".into()],
                modified: vec![],
                removed: vec![],
            },
        )
    })
    .await
    .expect("submit while the loop runs");
    assert!(
        matches!(
            outcome,
            filigrio_protocol::CommandOutcome::Applied { changed: 1, .. }
        ),
        "the submit must report the work it did, got {outcome:?}"
    );

    // 3) The apply already ran — no drain to wait for, and nothing queued (the
    //    queue is the producer lane only). This is checked immediately, with no
    //    polling: if the command were still async this would fail.
    let status = on_client(&socket_path, move |c| c.project_status(project_id))
        .await
        .expect("status while the loop runs");
    assert!(
        status.node_count > 0,
        "the changeset must be applied by the time the submit response returns"
    );

    // 4) The loop is still serving, and the queue stayed empty throughout.
    let health = on_client(&socket_path, |c| c.health())
        .await
        .expect("health after the sync command");
    assert_eq!(
        health.queue_depth, 0,
        "a wire command must never enter the producer queue"
    );

    // 5) Deterministic shutdown: the loop stops promptly on request.
    shutdown.notify_one();
    let outcome = tokio::time::timeout(Duration::from_secs(5), loop_handle)
        .await
        .expect("run loop did not shut down within 5s")
        .expect("run loop task panicked");
    assert!(outcome.is_ok(), "run loop returned an error: {outcome:?}");

    // The socket is cleaned up on shutdown.
    assert!(
        !socket_path.exists(),
        "the socket file should be removed on clean shutdown"
    );
}
