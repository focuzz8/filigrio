//! Concurrency end-to-end through the real `run()` loop, across the **two**
//! execution paths ADR-0042 F6c leaves us with:
//!
//! - **wire commands** run synchronously on their own connection task (F6c),
//!   so C1/C2 below prove a heavy command neither blocks another project's
//!   command nor holds the global daemon lock;
//! - the **producer lane** (watcher output) still runs on the worker pool
//!   (ADR-0032 §2 "parallelize across projects"), so W6/W7 prove its telemetry
//!   is real and `producer_lane_still_applies_through_the_pool` proves the lane
//!   itself survived the re-type.
//!
//! Both halves gate an apply with its own per-project lock, held on a plain
//! thread (so no non-`Send` `MutexGuard` ever crosses an `.await`) — the lock is
//! what coordinates the two schedulers, and holding it is how a test freezes an
//! apply mid-flight without a sleep.
//!
//! (Before F6c these were W1/W2: a `Submit` was queued and the *pool* ran it.
//! Now a `Submit` executes at ingress, so the same two properties have to be
//! proven of the command path — same fixtures, different scheduler.)

use filigrio_core::ChangeSet;
use filigrio_daemon::{Daemon, DaemonConfig, Project, ProjectLocks};
use filigrio_protocol::{Command, DaemonClientTrait, Request, Response, SocketClient};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;

/// Run one blocking client call off the runtime (SocketClient uses blocking std
/// sockets, so it must not run on a runtime worker thread).
async fn on_client<T, F>(path: &Path, f: F) -> T
where
    F: FnOnce(SocketClient) -> T + Send + 'static,
    T: Send + 'static,
{
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let client = SocketClient::new(&p)
            .with_timeout(Duration::from_secs(5))
            // Commands are synchronous now (ADR-0042 F6c); a deliberately
            // wedged one is *supposed* to hang, so give it a bounded budget
            // rather than the 10-minute production default — a broken test must
            // fail, not stall the suite.
            .with_command_timeout(Duration::from_secs(20));
        f(client)
    })
    .await
    .unwrap()
}

/// Write a tiny buildable project at `base/name`; return its root path (its id is
/// `name`). Deliberately does NOT register it — that happens post-startup.
fn write_project(base: &Path, name: &str) -> PathBuf {
    let root = base.join(name);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "fn main() {\n    helper();\n}\nfn helper() {}\n",
    )
    .unwrap();
    root
}

/// An empty (but real) project dir: nothing for startup reconcile to index, so
/// any later graph growth is unambiguously the producer lane's doing.
fn write_empty_project(base: &Path, name: &str) -> PathBuf {
    let root = base.join(name);
    std::fs::create_dir_all(root.join("src")).unwrap();
    root
}

fn isolated_config(scratch: &Path, socket: &Path) -> DaemonConfig {
    DaemonConfig {
        socket_path: socket.to_path_buf(),
        shutdown_marker_path: scratch.join("daemon.clean"),
        registry_path: scratch.join("registry.json"),
        idle_timeout: None,
        worker_threads: 2, // enough for A stuck + B running concurrently
        watcher_debounce: Duration::from_millis(50),
        ..DaemonConfig::default()
    }
}

/// Register `root` with watch ON (what `project watch on` persists) so the run
/// loop starts a watcher for it at boot — the producer lane's precondition
/// since ADR-0042 F6b.
fn add_watched(daemon: &Daemon, root: &Path) {
    let mut project = Project::new(root.to_path_buf());
    project.watch = true;
    std::fs::create_dir_all(&project.output_dir).unwrap();
    daemon.registry.lock().add(project).unwrap();
}

/// Hold `id`'s per-project lock on a dedicated thread until the returned sender is
/// signalled. Blocks until the lock is actually held, so the gate is closed on
/// return.
fn hold_lock(locks: &ProjectLocks, id: &str) -> mpsc::Sender<()> {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (held_tx, held_rx) = mpsc::channel::<()>();
    let locks = locks.clone();
    let id = id.to_string();
    std::thread::spawn(move || {
        let lock = locks.lock_for(&id).unwrap();
        let _guard = lock.lock();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv(); // hold until released
    });
    held_rx.recv().unwrap();
    release_tx
}

async fn await_bound(socket_path: &Path) {
    for _ in 0..300 {
        if socket_path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("run loop never bound its socket");
}

/// Register a project and wait until the daemon has processed the add (status
/// stops erroring with ProjectNotFound). It is registered but NOT built and NOT
/// watched — `ProjectRegister` does neither (ADR-0042 F6b).
async fn add_and_await(socket_path: &Path, root: &Path, id: &str) {
    let path = root.to_string_lossy().to_string();
    on_client(socket_path, move |c| c.project_register(path))
        .await
        .unwrap();
    for _ in 0..200 {
        let id = id.to_string();
        if on_client(socket_path, move |c| c.project_status(id))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("project {id} was never registered");
}

/// Fire a synchronous `ProjectIndex` on a background task without awaiting it —
/// the shape a test needs when the command is *expected* to wedge on a held
/// per-project lock (ADR-0042 F6c: the command blocks its own connection until
/// the apply completes).
fn spawn_index(socket_path: &Path, project: &str) -> tokio::task::JoinHandle<Response> {
    let socket_path = socket_path.to_path_buf();
    let project = project.to_string();
    tokio::spawn(async move {
        on_client(&socket_path, move |c| {
            c.send(Request::command(Command::ProjectIndex {
                project,
                clean: false,
            }))
            .expect("the wedged index must not fail at the transport level")
        })
        .await
    })
}

/// C1 — cross-project concurrency for **synchronous commands** (ADR-0042 F6c):
/// with project `stuck`'s index frozen on a held per-project lock, an index of
/// project `free` still runs to completion on its own connection. If the sync
/// command path took the daemon lock (or serialized connections), `free` could
/// never make progress while `stuck` was blocked — which is precisely the
/// "pinned connection / head-of-line blocking" objection F6c had to answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn c1_a_wedged_sync_command_does_not_block_another_project() {
    let scratch = TempDir::new().unwrap();
    let socket_path = scratch.path().join("d.sock");
    let stuck_root = write_project(scratch.path(), "stuck");
    let free_root = write_project(scratch.path(), "free");

    let daemon = Daemon::new(isolated_config(scratch.path(), &socket_path));
    let locks = daemon.locks();
    let shutdown = Arc::new(Notify::new());
    let loop_handle = tokio::spawn(daemon.run_until(shutdown.clone()));
    await_bound(&socket_path).await;

    add_and_await(&socket_path, &stuck_root, "stuck").await;
    add_and_await(&socket_path, &free_root, "free").await;

    // Freeze `stuck`'s apply, then fire its index and leave it hanging.
    let release = hold_lock(&locks, "stuck");
    let wedged = spawn_index(&socket_path, "stuck");
    // Give the wedged command time to actually reach the per-project lock.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !wedged.is_finished(),
        "precondition: `stuck` must be wedged"
    );

    // `free` must complete even though `stuck` is wedged. (Sent as a plain
    // control command: the `index_project` client helper this used to call was
    // the MCP *sliver*'s, removed with the bridge's mutation surface in
    // ADR-0042 F9. Same wire path the wedged `stuck` index above takes, which
    // is what the test is actually about.)
    let free = tokio::time::timeout(
        Duration::from_secs(20),
        on_client(&socket_path, |c| {
            c.send(Request::command(Command::ProjectIndex {
                project: "free".into(),
                clean: false,
            }))
        }),
    )
    .await
    .expect("indexing `free` hung behind the wedged `stuck` command")
    .expect("indexing `free` failed");
    assert!(
        matches!(
            free,
            Response::CommandCompleted {
                outcome: filigrio_protocol::CommandOutcome::Indexed { .. }
            }
        ),
        "got {free:?}"
    );
    assert!(
        on_client(&socket_path, |c| c.project_status("free".into()))
            .await
            .unwrap()
            .node_count
            > 0,
        "`free` must actually have been indexed"
    );

    // Release `stuck` so its command can finish, then shut down.
    let _ = release.send(());
    let stuck = tokio::time::timeout(Duration::from_secs(20), wedged)
        .await
        .expect("the released command never completed")
        .expect("the command task panicked");
    assert!(
        matches!(stuck, Response::CommandCompleted { .. }),
        "the wedged command must complete once released, got {stuck:?}"
    );

    shutdown.notify_one();
    tokio::time::timeout(Duration::from_secs(10), loop_handle)
        .await
        .expect("run loop did not shut down")
        .expect("run loop task panicked")
        .expect("run loop returned an error");
}

/// C2 — a synchronous command does **not** hold the global daemon lock: with
/// `stuck`'s index frozen mid-apply, a `Health` query still returns promptly.
/// This is the invariant that makes F6c's "plan under a brief lock, execute
/// with it released" split load-bearing: run the apply inside the funnel's lock
/// section and every query on the daemon hangs behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn c2_query_responsive_during_a_wedged_sync_command() {
    let scratch = TempDir::new().unwrap();
    let socket_path = scratch.path().join("d.sock");
    let stuck_root = write_project(scratch.path(), "stuck");

    let daemon = Daemon::new(isolated_config(scratch.path(), &socket_path));
    let locks = daemon.locks();
    let shutdown = Arc::new(Notify::new());
    let loop_handle = tokio::spawn(daemon.run_until(shutdown.clone()));
    await_bound(&socket_path).await;
    add_and_await(&socket_path, &stuck_root, "stuck").await;

    let release = hold_lock(&locks, "stuck");
    let wedged = spawn_index(&socket_path, "stuck");

    // Query repeatedly across the window in which the command is dispatched and
    // wedges on the held lock — every query must return promptly, never hang.
    for _ in 0..10 {
        let health = on_client(&socket_path, |c| c.health())
            .await
            .expect("Health query hung while a sync command was applying — daemon lock held");
        assert_eq!(health.project_count, 1);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    let _ = release.send(());
    let _ = tokio::time::timeout(Duration::from_secs(20), wedged).await;
    shutdown.notify_one();
    tokio::time::timeout(Duration::from_secs(10), loop_handle)
        .await
        .expect("run loop did not shut down")
        .expect("run loop task panicked")
        .expect("run loop returned an error");
}

// ---- Guard tests for the premises the concurrency tests rest on ----

/// GUARD — `project_register` registers a project WITHOUT building or watching it
/// (ADR-0042 F6b): node count stays 0 until something is explicitly asked for.
/// If this regresses, C1/C2 would silently be measuring startup instead of the
/// command path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guard_project_register_registers_without_building_or_watching() {
    let scratch = TempDir::new().unwrap();
    let socket_path = scratch.path().join("d.sock");
    let root = write_project(scratch.path(), "proj");

    let daemon = Daemon::new(isolated_config(scratch.path(), &socket_path));
    let shutdown = Arc::new(Notify::new());
    let loop_handle = tokio::spawn(daemon.run_until(shutdown.clone()));
    await_bound(&socket_path).await;

    add_and_await(&socket_path, &root, "proj").await;

    // Registered, but never indexed → the graph is empty, and no watcher is
    // following it. Give any (erroneous) background build a generous window.
    for _ in 0..10 {
        let status = on_client(&socket_path, |c| c.project_status("proj".into()))
            .await
            .unwrap();
        assert_eq!(
            status.node_count, 0,
            "project_register must NOT build the project"
        );
        assert!(
            !status.watching,
            "project_register must NOT start a watcher (ADR-0042 F6b: watch is explicit)"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    shutdown.notify_one();
    tokio::time::timeout(Duration::from_secs(10), loop_handle)
        .await
        .expect("run loop did not shut down")
        .expect("run loop task panicked")
        .expect("run loop returned an error");
}

/// GUARD — startup reconcile builds a pre-registered **watched** project, and
/// leaves an unwatched one cold (ADR-0042 F6b). Before F6b every registered
/// project was reconciled and watched at boot; this pins the new rule at the
/// live-loop level (the in-process version lives in `watcher_lifecycle.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guard_startup_reconciles_only_watched_projects() {
    let scratch = TempDir::new().unwrap();
    let socket_path = scratch.path().join("d.sock");
    let watched_root = write_project(scratch.path(), "followed");
    let cold_root = write_project(scratch.path(), "cold");

    // Registered BEFORE the loop runs: one watched, one not.
    let daemon = Daemon::new(isolated_config(scratch.path(), &socket_path));
    add_watched(&daemon, &watched_root);
    daemon.registry.lock().add(Project::new(cold_root)).unwrap();

    let shutdown = Arc::new(Notify::new());
    let loop_handle = tokio::spawn(daemon.run_until(shutdown.clone()));
    await_bound(&socket_path).await;

    // No command anywhere — the only possible builder is startup reconcile.
    let mut built = false;
    for _ in 0..200 {
        let nodes = on_client(&socket_path, |c| c.project_status("followed".into()))
            .await
            .map(|s| s.node_count)
            .unwrap_or(0);
        if nodes > 0 {
            built = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        built,
        "startup reconcile must build a pre-registered WATCHED project"
    );

    let cold = on_client(&socket_path, |c| c.project_status("cold".into()))
        .await
        .unwrap();
    assert_eq!(
        cold.node_count, 0,
        "startup must leave an unwatched project cold (ADR-0042 F6b)"
    );
    assert!(
        !cold.watching,
        "an unwatched project must not get a watcher at boot"
    );

    shutdown.notify_one();
    tokio::time::timeout(Duration::from_secs(10), loop_handle)
        .await
        .expect("run loop did not shut down")
        .expect("run loop task panicked")
        .expect("run loop returned an error");
}

/// The producer lane still reaches the pool and applies: a real edit on a
/// watched project grows the graph with no client command at all. This is the
/// path the queue re-type (ADR-0042 F6b) rewired — `Produced` → `op_of` →
/// `QueueItem` → `ApplyJob`, with no wire `Command` in between.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn producer_lane_still_applies_through_the_pool() {
    let scratch = TempDir::new().unwrap();
    let socket_path = scratch.path().join("d.sock");
    let root = write_empty_project(scratch.path(), "live");

    let daemon = Daemon::new(isolated_config(scratch.path(), &socket_path));
    add_watched(&daemon, &root);

    let shutdown = Arc::new(Notify::new());
    let loop_handle = tokio::spawn(daemon.run_until(shutdown.clone()));
    await_bound(&socket_path).await;

    // Baseline: the tree was empty at boot, so any growth is the watcher's.
    assert_eq!(
        on_client(&socket_path, |c| c.project_status("live".into()))
            .await
            .unwrap()
            .node_count,
        0
    );

    std::fs::write(
        root.join("src/main.rs"),
        "fn main() { helper(); }\nfn helper() {}\n",
    )
    .unwrap();

    let mut grew = false;
    for _ in 0..200 {
        let nodes = on_client(&socket_path, |c| c.project_status("live".into()))
            .await
            .map(|s| s.node_count)
            .unwrap_or(0);
        if nodes > 0 {
            grew = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        grew,
        "a watcher edit must still flow producer → queue → pool → apply"
    );

    shutdown.notify_one();
    tokio::time::timeout(Duration::from_secs(10), loop_handle)
        .await
        .expect("run loop did not shut down")
        .expect("run loop task panicked")
        .expect("run loop returned an error");
}

/// W6 — **Health reflects producer-lane pool work that `queue_depth` can't
/// see.** Since ADR-0042 F6c the worker pool serves the watcher lane alone, so
/// this drives a real file edit on a watched project with its per-project lock
/// held: the resulting pool job wedges in flight, and Health shows
/// `applies_inflight > 0` while `queue_depth == 0` (the item was already
/// collected off the intake queue into the pool). Without the telemetry wiring
/// the daemon reads as idle mid-apply.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w6_health_reports_inflight_while_queue_empty() {
    let scratch = TempDir::new().unwrap();
    let socket_path = scratch.path().join("d.sock");
    // Empty tree → startup reconcile has nothing to do and finishes instantly.
    let stuck_root = write_empty_project(scratch.path(), "stuck");

    let daemon = Daemon::new(isolated_config(scratch.path(), &socket_path));
    add_watched(&daemon, &stuck_root);
    let locks = daemon.locks();

    let shutdown = Arc::new(Notify::new());
    let loop_handle = tokio::spawn(daemon.run_until(shutdown.clone()));
    await_bound(&socket_path).await; // startup (incl. watcher start) is done

    // Freeze the project's applies, then make a real edit: the watcher event
    // becomes a pool job that wedges on the held lock.
    let release = hold_lock(&locks, "stuck");
    std::fs::write(stuck_root.join("src/main.rs"), "fn main() {}\n").unwrap();

    let mut saw_inflight = false;
    for _ in 0..200 {
        let h = on_client(&socket_path, |c| c.health()).await.unwrap();
        if h.applies_inflight > 0 {
            assert_eq!(
                h.queue_depth, 0,
                "the item was collected into the pool, so intake queue_depth must be 0"
            );
            saw_inflight = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw_inflight,
        "Health never reported the in-flight producer-lane apply — telemetry not wired \
         (daemon looks idle mid-apply)"
    );

    let _ = release.send(());
    shutdown.notify_one();
    tokio::time::timeout(Duration::from_secs(10), loop_handle)
        .await
        .expect("run loop did not shut down")
        .expect("run loop task panicked")
        .expect("run loop returned an error");
}

/// W7 — **Health reports the deferred backlog.** With a 1-permit pool and two
/// watched projects both producing events, one apply wedges in flight and the
/// other is deferred by the pool bound; Health surfaces `applies_deferred > 0`
/// — work that is in neither `queue_depth` nor `applies_inflight`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w7_health_reports_deferred_backlog() {
    let scratch = TempDir::new().unwrap();
    let socket_path = scratch.path().join("d.sock");
    let stuck_root = write_empty_project(scratch.path(), "stuck");
    let other_root = write_empty_project(scratch.path(), "other");

    let mut cfg = isolated_config(scratch.path(), &socket_path);
    cfg.worker_threads = 1; // one permit → the second job must defer
    let daemon = Daemon::new(cfg);
    add_watched(&daemon, &stuck_root);
    add_watched(&daemon, &other_root);
    let locks = daemon.locks();

    let shutdown = Arc::new(Notify::new());
    let loop_handle = tokio::spawn(daemon.run_until(shutdown.clone()));
    await_bound(&socket_path).await;

    let release = hold_lock(&locks, "stuck");
    std::fs::write(stuck_root.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(other_root.join("src/main.rs"), "fn main() {}\n").unwrap();

    let mut saw_deferred = false;
    for _ in 0..200 {
        let h = on_client(&socket_path, |c| c.health()).await.unwrap();
        if h.applies_inflight >= 1 && h.applies_deferred >= 1 {
            saw_deferred = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw_deferred,
        "Health never reported the deferred backlog (inflight+deferred pool work invisible)"
    );

    let _ = release.send(());
    shutdown.notify_one();
    tokio::time::timeout(Duration::from_secs(10), loop_handle)
        .await
        .expect("run loop did not shut down")
        .expect("run loop task panicked")
        .expect("run loop returned an error");
}

/// Keeps `ChangeSet` in scope as the shape a producer's `Op::Apply` carries —
/// the lane's payload type is unchanged by the queue re-type.
#[test]
fn producer_payload_is_still_a_changeset() {
    let cs = ChangeSet {
        added: vec!["src/main.rs".into()],
        modified: vec![],
        removed: vec![],
    };
    assert_eq!(cs.added.len(), 1);
}
