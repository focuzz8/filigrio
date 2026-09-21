//! ADR-0032a — the filesystem-watcher **lifecycle** inside the daemon, as
//! revised by ADR-0042 F6b (watching is an explicit, persisted, per-project
//! mode — never an ambient property of the resident daemon).
//!
//! The unit tests in `watchers.rs` pin the individual mechanics (route, stop,
//! idempotent start) against injected channels. These integration tests prove
//! the wiring the unit tests can't reach:
//!
//! 1. `watch_on_starts_a_watcher_watch_off_and_remove_stop_it` — the command
//!    contract drives the lifecycle. Fully deterministic — no filesystem
//!    events, no sleeps.
//! 2. `register_alone_does_not_watch` — the F6b default: a registered project
//!    is cold by contract.
//! 3. `editing_a_file_on_the_live_daemon_reindexes` — the whole point of
//!    ADR-0032a: a real edit on a *watched* running daemon flows notify → queue
//!    → apply and the graph grows. This one uses real `notify` events, so it
//!    polls-until-observed with a ceiling (the same shape the `daemon_run_loop`
//!    test uses).

use filigrio_daemon::{Command, Daemon, DaemonConfig, DataQuery, Project, Request, Response};
use filigrio_protocol::{CommandOutcome, DaemonClientTrait, SocketClient};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;

/// A daemon config with every on-disk path isolated under `scratch`.
fn isolated_config(scratch: &TempDir) -> DaemonConfig {
    DaemonConfig {
        registry_path: scratch.path().join("registry.json"),
        shutdown_marker_path: scratch.path().join("daemon.clean"),
        socket_path: scratch.path().join("d.sock"),
        idle_timeout: None,
        watcher_debounce: Duration::from_millis(50),
        ..Default::default()
    }
}

/// A project registered with `watch: true` (what `watch on` persists), so the
/// live-daemon tests below get a watcher at startup (ADR-0042 F6b).
fn watched_project(root: &std::path::Path) -> Project {
    let mut project = Project::new(root.to_path_buf());
    project.watch = true;
    project
}

/// The command contract owns the watcher lifecycle (ADR-0042 F6b): `watch on`
/// begins watching, `watch off` stops, and `ProjectRemove` stops too —
/// otherwise a removed project keeps a stale watcher feeding the queue. Driven
/// entirely through the public command path, which now executes synchronously
/// (F6c), so it needs no drain call and no filesystem events at all.
#[test]
fn watch_on_starts_a_watcher_watch_off_and_remove_stop_it() {
    let scratch = TempDir::new().unwrap();
    let mut daemon = Daemon::new(isolated_config(&scratch));

    // A real project directory so the watcher can actually bind.
    let root = scratch.path().join("myproj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();
    let project_id = Project::new(root.clone()).id;

    daemon.handle_request(Request::command(Command::ProjectRegister {
        path: root.to_string_lossy().into_owned(),
    }));

    // `watch on` → watching, AND the initial converge ran (the "watch rechecks
    // the index before watching" contract): the outcome carries its changed count.
    let resp = daemon.handle_request(Request::command(Command::ProjectWatch {
        project: project_id.clone(),
        on: true,
    }));
    let Response::CommandCompleted {
        outcome: CommandOutcome::Watch {
            watching, changed, ..
        },
    } = resp
    else {
        panic!("expected a Watch outcome, got {resp:?}");
    };
    assert!(watching, "watch on must report watching");
    assert_eq!(
        changed,
        Some(1),
        "watch on must run the initial deep converge and report it"
    );
    assert!(
        daemon.is_watching(&project_id),
        "watch on must start a watcher for the project"
    );
    assert!(
        daemon.project_status(&project_id).unwrap().node_count > 0,
        "watch on must leave the index converged, not merely following"
    );

    // `watch off` → stopped.
    let resp = daemon.handle_request(Request::command(Command::ProjectWatch {
        project: project_id.clone(),
        on: false,
    }));
    assert!(matches!(
        resp,
        Response::CommandCompleted {
            outcome: CommandOutcome::Watch {
                watching: false,
                ..
            }
        }
    ));
    assert!(
        !daemon.is_watching(&project_id),
        "watch off must stop the project's watcher"
    );

    // Watch back on, then ProjectRemove → unregistered AND unwatched.
    daemon.handle_request(Request::command(Command::ProjectWatch {
        project: project_id.clone(),
        on: true,
    }));
    assert!(daemon.is_watching(&project_id));
    daemon.handle_request(Request::command(Command::ProjectRemove {
        project: project_id.clone(),
    }));
    assert!(
        !daemon.is_watching(&project_id),
        "ProjectRemove must stop the project's watcher"
    );
    assert_eq!(daemon.watcher_count(), 0);
}

/// ADR-0042 F6b — both polarities are **idempotent**, and say so: a second
/// `watch on` does not restart the watcher or re-run the converge, and a
/// `watch off` on an unwatched project is a clean no-op. Neither may be an
/// error (a user re-running the verb must not have to care about prior state).
#[test]
fn watch_is_idempotent_in_both_polarities() {
    let scratch = TempDir::new().unwrap();
    let mut daemon = Daemon::new(isolated_config(&scratch));
    let root = scratch.path().join("myproj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();
    let project_id = Project::new(root.clone()).id;
    daemon.handle_request(Request::command(Command::ProjectRegister {
        path: root.to_string_lossy().into_owned(),
    }));

    // `watch off` before ever watching: success, and it says nothing was running.
    let resp = daemon.handle_request(Request::command(Command::ProjectWatch {
        project: project_id.clone(),
        on: false,
    }));
    let Response::CommandCompleted {
        outcome:
            CommandOutcome::Watch {
                watching,
                changed,
                note,
                ..
            },
    } = resp
    else {
        panic!("watch off on an unwatched project must succeed, got {resp:?}");
    };
    assert!(!watching);
    assert_eq!(changed, None, "watch off never converges");
    assert!(
        note.unwrap_or_default().contains("was not watching"),
        "an idempotent watch off must say so"
    );

    daemon.handle_request(Request::command(Command::ProjectWatch {
        project: project_id.clone(),
        on: true,
    }));

    // Second `watch on`: success, watching, and explicitly NO second converge.
    let resp = daemon.handle_request(Request::command(Command::ProjectWatch {
        project: project_id.clone(),
        on: true,
    }));
    let Response::CommandCompleted {
        outcome:
            CommandOutcome::Watch {
                watching,
                changed,
                note,
                ..
            },
    } = resp
    else {
        panic!("a repeated watch on must succeed, got {resp:?}");
    };
    assert!(watching);
    assert_eq!(changed, None, "an idempotent watch on must not re-converge");
    assert!(
        note.unwrap_or_default().contains("already watching"),
        "an idempotent watch on must say so"
    );
    assert_eq!(
        daemon.watcher_count(),
        1,
        "no duplicate watcher may be started"
    );
}

/// ADR-0042 F6b — registration is exactly registration: no watcher, no index.
/// (Auto-watching every registered project is the fd/inotify-exhaustion
/// problem F6b removes, and it made "registered" mean two different things.)
/// `Status` must surface the state so staleness is visible.
#[test]
fn register_alone_does_not_watch() {
    let scratch = TempDir::new().unwrap();
    let mut daemon = Daemon::new(isolated_config(&scratch));
    let root = scratch.path().join("myproj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let project_id = Project::new(root.clone()).id;

    daemon.handle_request(Request::command(Command::ProjectRegister {
        path: root.to_string_lossy().into_owned(),
    }));

    assert!(
        daemon.registry.lock().get(&project_id).is_some(),
        "register must register"
    );
    assert!(
        !daemon.is_watching(&project_id),
        "register must NOT start a watcher (watch is explicit, default off)"
    );
    assert!(
        !daemon.registry.lock().get(&project_id).unwrap().watch,
        "a newly registered project persists watch = false"
    );
    assert!(
        !daemon.project_status(&project_id).unwrap().watching,
        "Status must report the project as unwatched"
    );
}

/// ADR-0042 F6b — `watch on` **persists**, so a restart re-watches exactly the
/// projects the user asked for and leaves the rest cold. This is the whole
/// point of putting the mode in the registry rather than in daemon memory.
#[test]
fn watch_state_persists_across_a_restart_and_drives_startup() {
    let scratch = TempDir::new().unwrap();
    let cfg = isolated_config(&scratch);

    let watched_root = scratch.path().join("followed");
    let cold_root = scratch.path().join("cold");
    for root in [&watched_root, &cold_root] {
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();
    }
    let watched_id = Project::new(watched_root.clone()).id;
    let cold_id = Project::new(cold_root.clone()).id;

    let mut daemon = Daemon::new(cfg.clone());
    for root in [&watched_root, &cold_root] {
        daemon.handle_request(Request::command(Command::ProjectRegister {
            path: root.to_string_lossy().into_owned(),
        }));
    }
    daemon.handle_request(Request::command(Command::ProjectWatch {
        project: watched_id.clone(),
        on: true,
    }));
    drop(daemon);

    // "Restart": a fresh daemon loading the same registry file.
    let mut restarted = Daemon::new(cfg);
    restarted.load_registry().unwrap();
    assert!(
        restarted.registry.lock().get(&watched_id).unwrap().watch,
        "watch on must survive the restart"
    );
    assert!(
        !restarted.registry.lock().get(&cold_id).unwrap().watch,
        "an unwatched project must stay unwatched"
    );

    restarted.start_all_watchers();
    assert!(
        restarted.is_watching(&watched_id),
        "startup must watch the watch=true project"
    );
    assert!(
        !restarted.is_watching(&cold_id),
        "startup must leave the watch=false project cold (ADR-0042 F6b)"
    );
    assert_eq!(restarted.watcher_count(), 1);
}

/// ADR-0042 F6b — startup reconcile follows the same rule as startup watching:
/// only `watch == true` projects converge at boot. An unwatched registered
/// project is **cold by contract** — indexed on demand, never behind the user's
/// back (the "read verbs never auto-reconcile" rule of amendment §D).
#[test]
fn startup_reconcile_only_converges_watched_projects() {
    let scratch = TempDir::new().unwrap();
    let cfg = isolated_config(&scratch);

    let watched_root = scratch.path().join("followed");
    let cold_root = scratch.path().join("cold");
    for root in [&watched_root, &cold_root] {
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();
    }

    let mut daemon = Daemon::new(cfg);
    let watched = watched_project(&watched_root);
    let cold = Project::new(cold_root.clone()); // watch: false
    let watched_id = watched.id.clone();
    let cold_id = cold.id.clone();
    std::fs::create_dir_all(&watched.output_dir).unwrap();
    std::fs::create_dir_all(&cold.output_dir).unwrap();
    daemon.registry.lock().add(watched).unwrap();
    daemon.registry.lock().add(cold).unwrap();

    daemon.startup_reconcile().unwrap();

    assert!(
        daemon.project_status(&watched_id).unwrap().node_count > 0,
        "startup must converge a watched project"
    );
    assert_eq!(
        daemon.project_status(&cold_id).unwrap().node_count,
        0,
        "startup must NOT converge an unwatched project (cold by contract)"
    );
}

/// Run one blocking client call off the async runtime.
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

/// The ADR's reason to exist: editing a file on a *live* daemon must re-index it.
/// Before this wiring the daemon never referenced `FsWatcher`, so a save did
/// nothing. Here the loop starts a watcher at startup; writing a new source file
/// into the tree must flow notify → priority queue → apply and grow the graph.
///
/// Real `notify` events are asynchronous and best-effort (ADR-0032a §4), so this
/// polls until the node count grows, bounded by a ceiling — it never sleeps as a
/// stand-in for synchronization.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editing_a_file_on_the_live_daemon_reindexes() {
    let scratch = TempDir::new().unwrap();
    let cfg = isolated_config(&scratch);
    let socket_path = cfg.socket_path.clone();

    // A registered project that starts with NO code file, so the baseline graph
    // is empty and any growth is unambiguously from the live edit.
    let root = scratch.path().join("proj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    // ADR-0042 F6b: the daemon watches only `watch == true` projects at
    // startup, so this live-edit fixture registers with watch on — the same
    // state `project watch on` persists.
    let project = watched_project(&root);
    std::fs::create_dir_all(&project.output_dir).unwrap();
    let project_id = project.id.clone();

    let daemon = Daemon::new(cfg);
    daemon.registry.lock().add(project).unwrap();

    let shutdown = Arc::new(Notify::new());
    let loop_handle = tokio::spawn(daemon.run_until(shutdown.clone()));

    // Wait for the loop to bind its socket (bound only after startup + watcher start).
    let mut bound = false;
    for _ in 0..300 {
        if socket_path.exists() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(bound, "the run loop never bound its socket");

    // Baseline: empty graph.
    let base = on_client(&socket_path, {
        let pid = project_id.clone();
        move |c| c.project_status(pid)
    })
    .await
    .expect("baseline status");
    assert_eq!(base.node_count, 0, "precondition: graph starts empty");

    // The live edit: write a new source file with a real symbol.
    std::fs::write(root.join("src/lib.rs"), "pub fn hello() -> u32 { 42 }\n").unwrap();

    // Poll until the watcher's event has flowed all the way to graph nodes.
    let mut grew = false;
    for _ in 0..200 {
        let status = on_client(&socket_path, {
            let pid = project_id.clone();
            move |c| c.project_status(pid)
        })
        .await
        .expect("status while running");
        if status.node_count > 0 {
            grew = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        grew,
        "editing a file on the live daemon must re-index it (notify → queue → apply)"
    );

    // The watcher lane defers clustering (ClusterTiming::Deferred): the apply
    // above left the new symbols with no community, and only the run loop's
    // recluster — dispatched once the project is quiet — assigns them. So
    // communities appearing here is that wiring, observed from the wire.
    let mut clustered = false;
    for _ in 0..200 {
        let communities = on_client(&socket_path, {
            let pid = project_id.clone();
            move |c| match c.send(Request::data(DataQuery::GraphStats { project: pid })) {
                Ok(Response::QueryResult { data }) => data["communities"].as_u64(),
                _ => None,
            }
        })
        .await;
        if communities.is_some_and(|n| n > 0) {
            clustered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        clustered,
        "a deferred watcher-lane apply must be followed by a recluster that assigns communities"
    );

    // Deterministic shutdown.
    shutdown.notify_one();
    let outcome = tokio::time::timeout(Duration::from_secs(5), loop_handle)
        .await
        .expect("run loop did not shut down within 5s")
        .expect("run loop task panicked");
    assert!(outcome.is_ok(), "run loop returned an error: {outcome:?}");
}

/// R3/T4 — editing `.gitignore` on a live daemon re-scopes the index. A file that
/// was indexed becomes gitignored (still on disk, no FS event of its own) and must
/// drop out of the graph. This proves the whole R3 chain: the watcher sees the
/// `.gitignore` change → submits a re-scoping `Reconcile` (wire: `ProjectIndex
/// { deep: false }`, ADR-0042 F6) → reconcile re-derives scope and emits
/// the removal → apply shrinks the graph. Real `notify`, so poll-until-observed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editing_gitignore_on_the_live_daemon_rescopes_the_index() {
    let scratch = TempDir::new().unwrap();
    let cfg = isolated_config(&scratch);
    let socket_path = cfg.socket_path.clone();

    let root = scratch.path().join("proj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("vendor")).unwrap();
    // ADR-0042 F6b: the daemon watches only `watch == true` projects at
    // startup, so this live-edit fixture registers with watch on — the same
    // state `project watch on` persists.
    let project = watched_project(&root);
    std::fs::create_dir_all(&project.output_dir).unwrap();
    let project_id = project.id.clone();

    let daemon = Daemon::new(cfg);
    daemon.registry.lock().add(project).unwrap();

    let shutdown = Arc::new(Notify::new());
    let loop_handle = tokio::spawn(daemon.run_until(shutdown.clone()));

    let mut bound = false;
    for _ in 0..300 {
        if socket_path.exists() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(bound, "the run loop never bound its socket");

    let status = |sock: PathBuf, pid: String| async move {
        on_client(&sock, move |c| c.project_status(pid))
            .await
            .expect("status")
    };

    // No .gitignore yet → both files are in scope; the watcher indexes both.
    std::fs::write(root.join("vendor/x.rs"), "pub fn v() -> u32 { 1 }\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn hello() -> u32 { 42 }\n").unwrap();
    let mut indexed_both = false;
    for _ in 0..200 {
        if status(socket_path.clone(), project_id.clone())
            .await
            .file_count
            >= 2
        {
            indexed_both = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(indexed_both, "both in-scope files must index first");

    // Now ignore vendor/ — the file stays on disk but leaves scope. The watcher
    // must treat the .gitignore edit as a re-scoping trigger (a shallow
    // ProjectIndex, ADR-0042 F6), not drop it.
    std::fs::write(root.join(".gitignore"), "vendor/\n").unwrap();
    let mut rescoped = false;
    for _ in 0..200 {
        if status(socket_path.clone(), project_id.clone())
            .await
            .file_count
            == 1
        {
            rescoped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        rescoped,
        "editing .gitignore must re-scope: vendor/x.rs should drop out of the index"
    );

    shutdown.notify_one();
    let outcome = tokio::time::timeout(Duration::from_secs(5), loop_handle)
        .await
        .expect("run loop did not shut down within 5s")
        .expect("run loop task panicked");
    assert!(outcome.is_ok(), "run loop returned an error: {outcome:?}");
}

/// R1/T1 — a save inside a `.gitignore`d directory must NOT be indexed. The
/// watcher now filters candidates through the shared ADR-0022 boundary
/// (`SourceBoundary::matches`), so an edit under an ignored dir never becomes a
/// `Submit`. We prove it by writing an ignored file AND an in-scope file, then
/// waiting for the in-scope one to land: `file_count` (manifest entries) must be
/// exactly 1 — the ignored file was dropped at the producer, not indexed.
///
/// `file_count` keys off the manifest so this is unambiguous; using two writes
/// (one of which *does* flow through) also proves the pipeline was live, so a
/// green result can't be the ignored event merely not having arrived yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gitignored_edit_on_the_live_daemon_is_not_indexed() {
    let scratch = TempDir::new().unwrap();
    let cfg = isolated_config(&scratch);
    let socket_path = cfg.socket_path.clone();

    let root = scratch.path().join("proj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("vendor")).unwrap();
    // `.gitignore` puts vendor/ out of scope — the same boundary the cold walk uses.
    std::fs::write(root.join(".gitignore"), "vendor/\n").unwrap();
    // ADR-0042 F6b: the daemon watches only `watch == true` projects at
    // startup, so this live-edit fixture registers with watch on — the same
    // state `project watch on` persists.
    let project = watched_project(&root);
    std::fs::create_dir_all(&project.output_dir).unwrap();
    let project_id = project.id.clone();

    let daemon = Daemon::new(cfg);
    daemon.registry.lock().add(project).unwrap();

    let shutdown = Arc::new(Notify::new());
    let loop_handle = tokio::spawn(daemon.run_until(shutdown.clone()));

    let mut bound = false;
    for _ in 0..300 {
        if socket_path.exists() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(bound, "the run loop never bound its socket");

    // Write the IGNORED file first, then the in-scope file. If the watcher wrongly
    // submitted the ignored one, it would be indexed too and file_count would reach 2.
    std::fs::write(
        root.join("vendor/lib.rs"),
        "pub fn vendored() -> u32 { 1 }\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn hello() -> u32 { 42 }\n").unwrap();

    // Wait for the in-scope file to land (proves the pipeline is live).
    let mut landed = false;
    for _ in 0..200 {
        let status = on_client(&socket_path, {
            let pid = project_id.clone();
            move |c| c.project_status(pid)
        })
        .await
        .expect("status while running");
        if status.node_count > 0 {
            // The in-scope file is indexed; the ignored file must NOT be.
            assert_eq!(
                status.file_count, 1,
                "only src/lib.rs may be indexed; vendor/lib.rs is gitignored"
            );
            landed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        landed,
        "the in-scope edit was never indexed (pipeline not live?)"
    );

    shutdown.notify_one();
    let outcome = tokio::time::timeout(Duration::from_secs(5), loop_handle)
        .await
        .expect("run loop did not shut down within 5s")
        .expect("run loop task panicked");
    assert!(outcome.is_ok(), "run loop returned an error: {outcome:?}");
}
