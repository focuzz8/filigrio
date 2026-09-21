//! "Is this project registered?" — one authority, one answer, one spelling.
//!
//! The **registry** is the authority on registration (ADR-0032 §2). Nothing
//! else may answer that question: a project's residency in the LRU cache is a
//! performance fact about this daemon *lifetime*, and a daemon that has just
//! started has an empty cache by construction. When existence was read off the
//! cache, an auto-spawned daemon (ADR-0032f §6: dead socket → spawn → first
//! request) declined every graph query for a project it had loaded from
//! `registry.json` seconds earlier — and the decline named that same project in
//! its own "Registered projects:" list.
//!
//! The suites here pin both halves:
//! - **Existence** is the registry's, across both lifecycles — a resident
//!   daemon and a freshly spawned daemon *process*.
//! - **The wording** is one string. `Status` used to say `project not found`
//!   and `Submit` `project not registered` for the identical condition
//!   (ADR-0032b OQ4 recorded the wart), so a hook diagnostic quoting the daemon
//!   quoted whichever it happened to hit.
//!
//! Every daemon here is pointed at a scratch registry: none of these tests may
//! read or write the real `~/.config/filigrio/registry.json`.

use filigrio_daemon::{Command, Daemon, DaemonConfig, DataQuery, Project, Request, Response};
use filigrio_protocol::{DaemonClientTrait, SocketClient};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// A daemon whose registry, shutdown marker and socket all live under `dir` —
/// never the real `~/.config/filigrio/registry.json`.
fn scratch_config(dir: &Path) -> DaemonConfig {
    DaemonConfig {
        socket_path: dir.join("daemon.sock"),
        shutdown_marker_path: dir.join("daemon.clean"),
        registry_path: registry_path(dir),
        ..Default::default()
    }
}

/// The registry file the daemon binary resolves from `XDG_CONFIG_HOME=<dir>`
/// (`default_registry_path`), so the in-process daemon that seeds it and the
/// spawned process that reads it agree without either hard-coding the other.
fn registry_path(dir: &Path) -> PathBuf {
    dir.join("filigrio").join("registry.json")
}

/// A one-file project on disk. Small on purpose: these tests are about
/// resolution, not extraction.
fn project_tree(root: &Path) {
    std::fs::create_dir_all(root.join("src")).expect("mkdir src");
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha() -> u32 { 1 }\npub fn beta() -> u32 { alpha() + 1 }\n",
    )
    .expect("write src");
}

/// Register + index `root` on a throwaway daemon, leaving a populated
/// `registry.json` and `state.json` behind — the state of the world a *later*
/// daemon starts from. Returns the node count that later daemon must report.
fn seed_registered_and_indexed(config_dir: &Path, root: &Path) -> usize {
    let mut daemon = Daemon::new(scratch_config(config_dir));
    let resp = daemon.handle_request(Request::command(Command::ProjectRegister {
        path: root.to_string_lossy().into_owned(),
    }));
    assert!(
        matches!(resp, Response::CommandCompleted { .. }),
        "seed register failed: {resp:?}"
    );
    let resp = daemon.handle_request(Request::command(Command::ProjectIndex {
        project: root.to_string_lossy().into_owned(),
        clean: false,
    }));
    assert!(
        matches!(resp, Response::CommandCompleted { .. }),
        "seed index failed: {resp:?}"
    );

    let id = Project::new(root.to_path_buf()).id;
    let nodes = daemon.project_status(&id).expect("seed status").node_count;
    assert!(nodes > 0, "the seed must actually index something");
    nodes
}

fn node_count_of(response: &Response) -> u64 {
    let Response::QueryResult { data } = response else {
        panic!("expected a QueryResult, got {response:?}");
    };
    data["nodes"].as_u64().expect("stats carry a node count")
}

/// **Registration is the registry's fact.** A daemon that has never applied
/// this project — cold LRU cache, registry loaded from disk — must answer graph
/// queries for it, not decline it as unregistered.
#[test]
fn a_registered_project_answers_graph_queries_on_a_cold_daemon() {
    let scratch = TempDir::new().expect("tempdir");
    let root = scratch.path().join("proj");
    project_tree(&root);
    let indexed = seed_registered_and_indexed(scratch.path(), &root) as u64;

    // The restart: a new daemon over the same registry file, nothing resident.
    let mut cold = Daemon::new(scratch_config(scratch.path()));
    cold.load_registry().expect("load registry");

    let response = cold.handle_request(Request::data(DataQuery::GraphStats {
        project: root.to_string_lossy().into_owned(),
    }));
    assert_eq!(
        node_count_of(&response),
        indexed,
        "a cold daemon must page state.json in for a registered project, not decline it: {response:?}"
    );
}

/// The two authorities must agree. `Status` resolves against the registry and
/// `GraphStats` used to resolve against the cache, so on the same daemon, in
/// the same breath, one said the project existed and the other said it was not
/// registered. ADR-0032b's OQ4 hook diagnostic reads `Status`; every agent-facing
/// graph query reads the other — a disagreement here makes one of them a liar.
#[test]
fn status_and_graph_queries_agree_about_registration() {
    let scratch = TempDir::new().expect("tempdir");
    let root = scratch.path().join("proj");
    project_tree(&root);
    seed_registered_and_indexed(scratch.path(), &root);

    let mut cold = Daemon::new(scratch_config(scratch.path()));
    cold.load_registry().expect("load registry");
    let addressed = root.to_string_lossy().into_owned();

    // Registered: both answer. **Order is load-bearing** — `Status` pages the
    // checkpoint into the cache, so asking it first would warm the very cache
    // whose emptiness was the bug, and the graph query would pass for the wrong
    // reason. (That is also why the defect read as a resident-vs-auto-spawned
    // divergence in the field: any earlier `status` hid it.)
    let stats = cold.handle_request(Request::data(DataQuery::GraphStats {
        project: addressed.clone(),
    }));
    let status = cold.handle_request(Request::data(DataQuery::Status {
        project: Some(addressed.clone()),
    }));
    assert!(
        matches!(status, Response::QueryResult { .. }),
        "Status declined a registered project: {status:?}"
    );
    assert!(
        matches!(stats, Response::QueryResult { .. }),
        "a graph query declined a registered project Status accepted: {stats:?}"
    );

    // Unregistered: both decline.
    let elsewhere = scratch.path().join("not-a-project");
    let status = cold.handle_request(Request::data(DataQuery::Status {
        project: Some(elsewhere.to_string_lossy().into_owned()),
    }));
    let stats = cold.handle_request(Request::data(DataQuery::GraphStats {
        project: elsewhere.to_string_lossy().into_owned(),
    }));
    assert!(
        matches!(status, Response::Error { .. }),
        "Status accepted an unregistered project: {status:?}"
    );
    assert!(
        matches!(stats, Response::Error { .. }),
        "a graph query accepted an unregistered project: {stats:?}"
    );
}

/// Registered but never indexed is its own answer. Reporting it as
/// "not registered" sends the caller to `project register` (which then fails as
/// a duplicate), and answering with a silent zero-node graph is the ADR-0029
/// lie in the other direction — so it names `project index`.
#[test]
fn a_registered_but_unindexed_project_is_told_to_index_not_to_register() {
    let scratch = TempDir::new().expect("tempdir");
    let root = scratch.path().join("proj");
    project_tree(&root);

    let mut daemon = Daemon::new(scratch_config(scratch.path()));
    daemon.handle_request(Request::command(Command::ProjectRegister {
        path: root.to_string_lossy().into_owned(),
    }));

    let response = daemon.handle_request(Request::data(DataQuery::GraphStats {
        project: root.to_string_lossy().into_owned(),
    }));
    let Response::Error { message } = response else {
        panic!("an unindexed project must not answer with a fabricated empty graph: {response:?}");
    };
    assert!(
        message.contains("project index"),
        "the error must name the verb that fixes it: {message}"
    );
    assert!(
        !message.contains("not registered"),
        "the project IS registered — saying otherwise sends the caller to the wrong verb: {message}"
    );
}

/// One condition, one spelling. `Status` said `project not found`, `Submit` said
/// `project not registered` (ADR-0032b OQ4), and the data plane said a third,
/// longer thing — all for "the registry has nothing for this". These strings are
/// read by agents and quoted by the hook diagnostic, so they are compared
/// verbatim rather than by keyword.
#[test]
fn every_path_spells_an_unregistered_project_the_same_way() {
    let scratch = TempDir::new().expect("tempdir");
    let registered = scratch.path().join("registered");
    project_tree(&registered);
    seed_registered_and_indexed(scratch.path(), &registered);

    let mut daemon = Daemon::new(scratch_config(scratch.path()));
    daemon.load_registry().expect("load registry");
    let missing = scratch
        .path()
        .join("nowhere")
        .to_string_lossy()
        .into_owned();

    let messages = [
        (
            "Status",
            daemon.handle_request(Request::data(DataQuery::Status {
                project: Some(missing.clone()),
            })),
        ),
        (
            "GraphStats",
            daemon.handle_request(Request::data(DataQuery::GraphStats {
                project: missing.clone(),
            })),
        ),
        (
            "Submit",
            daemon.handle_request(Request::command(Command::Submit {
                project: missing.clone(),
                changeset: filigrio_daemon::ChangeSet::default(),
                priority: filigrio_daemon::Priority::Fs,
            })),
        ),
        (
            "ProjectIndex",
            daemon.handle_request(Request::command(Command::ProjectIndex {
                project: missing.clone(),
                clean: false,
            })),
        ),
        (
            "ProjectWatch",
            daemon.handle_request(Request::command(Command::ProjectWatch {
                project: missing.clone(),
                on: true,
            })),
        ),
        (
            "ProjectExport",
            daemon.handle_request(Request::command(Command::ProjectExport {
                project: missing.clone(),
            })),
        ),
        (
            "ProjectFlush",
            daemon.handle_request(Request::command(Command::ProjectFlush {
                project: missing.clone(),
            })),
        ),
    ]
    .map(|(verb, response)| match response {
        Response::Error { message } => (verb, message),
        other => panic!("{verb} must decline an unregistered project: {other:?}"),
    });

    let (first_verb, first) = &messages[0];
    for (verb, message) in &messages[1..] {
        assert_eq!(
            message, first,
            "{verb} spells the unregistered condition differently from {first_verb}"
        );
    }

    // …and the one spelling is actionable: it names the argument, what IS
    // registered, and the verb that fixes it (the `integration status`
    // diagnostic's advice, from the daemon's own mouth).
    assert!(
        first.contains(&missing),
        "the message must quote the argument: {first}"
    );
    assert!(
        first.contains("not registered"),
        "the message must name the condition: {first}"
    );
    assert!(
        first.contains("filigrio project register"),
        "the message must name the verb that fixes it: {first}"
    );
    assert!(
        first.contains(&Project::new(registered).id),
        "the message must list the registered project ids: {first}"
    );
}

// ---- the auto-spawn lifecycle (ADR-0032f §6) --------------------------------

/// A spawned `filigrio-daemon start` and the socket it binds, both released on
/// drop — **including on panic**, which is when a leaked daemon does the most
/// damage (it holds the socket, so the next run's handshake finds a "running"
/// daemon that belongs to a dead test).
struct SpawnedDaemon {
    child: std::process::Child,
    socket: PathBuf,
}

impl SpawnedDaemon {
    /// Start the daemon exactly as `AutoStartHandshake::spawn_daemon` does
    /// (same argv), pointed at a scratch `XDG_CONFIG_HOME` so it resolves the
    /// seeded registry and never the developer's own.
    fn start(config_dir: &Path, tag: &str) -> SpawnedDaemon {
        // A unix socket path is capped near 108 bytes (`SUN_LEN`) and a
        // `TempDir` under this repo's scratch trees already exceeds it, so the
        // socket lives at a short `/tmp` path and cleanup is this type's job.
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let socket = PathBuf::from("/tmp").join(format!(
            "gfy-{tag}-{}-{}.sock",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&socket);

        let child = std::process::Command::new(env!("CARGO_BIN_EXE_filigrio-daemon"))
            .arg("--socket")
            .arg(&socket)
            .arg("start")
            .arg("--idle-timeout")
            .arg("0")
            .env("XDG_CONFIG_HOME", config_dir)
            .env("XDG_CACHE_HOME", config_dir.join("cache"))
            .env("HOME", config_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn filigrio-daemon");

        SpawnedDaemon { child, socket }
    }

    /// A client for the daemon once it is listening.
    fn client(&self) -> SocketClient {
        let deadline = Instant::now() + Duration::from_secs(20);
        let client = SocketClient::new(&self.socket).with_timeout(Duration::from_secs(10));
        while Instant::now() < deadline {
            if client.is_reachable() {
                return client;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("spawned daemon never bound {}", self.socket.display());
    }
}

impl Drop for SpawnedDaemon {
    fn drop(&mut self) {
        // SIGKILL: the point of this guard is the panicking path, where a
        // graceful stop would have to succeed to run at all. It skips teardown,
        // so the lifecycle files are removed here by name.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(self.socket.with_extension("lock"));
        let _ = std::fs::remove_file(self.socket.with_extension("clean"));
    }
}

/// **The lifecycle that broke.** A daemon *process* started fresh against an
/// existing registry — what the MCP bridge's auto-start does when it finds a
/// dead socket (ADR-0032f §6) — must answer for a registered project on its
/// very first request. Not after a warming `Status`, not after an index: first
/// request.
///
/// In-process coverage above cannot substitute for this one: the registry
/// arrives here by `XDG_CONFIG_HOME` → `default_registry_path` → `load_registry`
/// in another process, which is exactly the chain the "different registry path"
/// hypothesis lived in.
#[test]
fn an_auto_spawned_daemon_answers_for_a_registered_project_on_its_first_request() {
    let scratch = TempDir::new().expect("tempdir");
    let root = scratch.path().join("proj");
    project_tree(&root);
    let indexed = seed_registered_and_indexed(scratch.path(), &root) as u64;

    let daemon = SpawnedDaemon::start(scratch.path(), "autospawn");
    let client = daemon.client();

    // First request: a graph query, cold cache, addressed by path as a client
    // addresses it.
    let response = client
        .send(Request::data(DataQuery::GraphStats {
            project: root.to_string_lossy().into_owned(),
        }))
        .expect("the daemon answered the request");
    assert_eq!(
        node_count_of(&response),
        indexed,
        "an auto-spawned daemon declined a project its own registry holds: {response:?}"
    );

    // A daemon's lifecycle files belong to the socket it was started on, not to
    // a global path: this instance must not be able to decide the *well-known*
    // daemon's next startup reconcile depth (nor could this test isolate itself
    // if it could). The handshake lock is the socket-derived file a *running*
    // daemon holds; the clean-shutdown marker is the one it writes on the way
    // out, and is asserted where that lifecycle is under test
    // (`clean_shutdown_marker.rs`).
    assert!(
        daemon.socket.with_extension("lock").exists(),
        "the handshake lock must live beside this daemon's socket"
    );
}
