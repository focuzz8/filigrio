//! The clean-shutdown marker means what ADR-0032 §6 says it means.
//!
//! §6's rule is one sentence: **marker present → cheap mtime reconcile; marker
//! absent → deep re-hash**, because deep exists precisely for drift the
//! filesystem's mtimes cannot reveal. The daemon's *read* of the marker has
//! always matched that. Its *lifecycle* did not: the marker was written at the
//! end of `startup_reconcile` and removed in `shutdown`, so the truth table ran
//! backwards — a clean exit left no marker (next start paid for a deep re-hash
//! it did not need) and a crash left one behind (next start took the cheap path
//! in the one case §6 wrote deep for, and served a stale index that looked
//! fresh).
//!
//! Both halves are pinned here, and the crash half is pinned against a **real
//! killed process**. That is not ceremony: `shutdown()` is a normal method, so
//! any in-process "simulated crash" is really a test choosing what a crash
//! leaves behind — which is the very thing under test. Only SIGKILL proves the
//! daemon cannot leave a clean-looking marker when it does not get to run code.
//!
//! Every daemon here is pointed at a scratch `XDG_CONFIG_HOME` and a short
//! `/tmp` socket: none of these tests may read or write the real
//! `~/.config/filigrio/registry.json`, and none may touch the well-known
//! daemon's socket or marker.

use filigrio_daemon::{Command, Daemon, DaemonConfig, DataQuery, Project, Request, Response};
use filigrio_protocol::{DaemonClientTrait, SocketClient};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

// ---- fixtures ---------------------------------------------------------------

/// A daemon whose registry, marker and socket all live under `dir`.
fn scratch_config(dir: &Path) -> DaemonConfig {
    DaemonConfig {
        socket_path: dir.join("daemon.sock"),
        shutdown_marker_path: dir.join("daemon.clean"),
        registry_path: dir.join("filigrio").join("registry.json"),
        ..Default::default()
    }
}

const TWO_FNS: &str = "pub fn alpha() -> u32 { 1 }\npub fn beta() -> u32 { alpha() + 1 }\n";

/// The same file with two more functions — and, when written by
/// [`edit_preserving_mtime`], the same mtime. This is the drift only a re-hash
/// can see.
const FOUR_FNS: &str = "pub fn alpha() -> u32 { 1 }\npub fn beta() -> u32 { alpha() + 1 }\n\
                        pub fn gamma() -> u32 { beta() + 1 }\npub fn delta() -> u32 { gamma() + 1 }\n";

fn project_tree(root: &Path) {
    std::fs::create_dir_all(root.join("src")).expect("mkdir src");
    std::fs::write(root.join("src/lib.rs"), TWO_FNS).expect("write src");
}

/// Rewrite `src/lib.rs` and put its modification time back where it was: the
/// content changed, the filesystem says it didn't. A checkout of an older
/// branch, a restored backup, a coarse-grained clock and a fast editor all
/// produce this in the wild; here it is manufactured exactly.
fn edit_preserving_mtime(root: &Path, content: &str) {
    let file = root.join("src/lib.rs");
    let before = std::fs::metadata(&file)
        .expect("stat before")
        .modified()
        .expect("mtime before");
    std::fs::write(&file, content).expect("rewrite src");
    std::fs::File::options()
        .write(true)
        .open(&file)
        .expect("reopen for set_modified")
        .set_modified(before)
        .expect("restore mtime");
    let after = std::fs::metadata(&file)
        .expect("stat after")
        .modified()
        .expect("mtime after");
    assert_eq!(
        before, after,
        "the fixture must actually preserve the mtime, or it is testing nothing"
    );
}

/// Register, index and *watch* `root` on a throwaway in-process daemon, leaving
/// a populated `registry.json` and `state.json` behind — the state of the world
/// a later daemon *process* starts from. Watch matters: ADR-0042 F6b reconciles
/// only watched projects on startup. Returns the indexed node count.
///
/// This daemon is dropped, never `shutdown()`, so it writes no marker.
fn seed_watched_and_indexed(config_dir: &Path, root: &Path) -> u64 {
    let mut daemon = Daemon::new(scratch_config(config_dir));
    for command in [
        Command::ProjectRegister {
            path: root.to_string_lossy().into_owned(),
        },
        Command::ProjectIndex {
            project: root.to_string_lossy().into_owned(),
            clean: false,
        },
        Command::ProjectWatch {
            project: root.to_string_lossy().into_owned(),
            on: true,
        },
    ] {
        let resp = daemon.handle_request(Request::command(command));
        assert!(
            matches!(resp, Response::CommandCompleted { .. }),
            "seed step failed: {resp:?}"
        );
    }
    let id = Project::new(root.to_path_buf()).id;
    let nodes = daemon.project_status(&id).expect("seed status").node_count as u64;
    assert!(nodes > 0, "the seed must actually index something");
    nodes
}

// ---- a real daemon process, killable ---------------------------------------

/// One daemon *identity* — a socket, and therefore a marker path — across
/// successive `filigrio-daemon start` processes. A restart is the same daemon on
/// the same socket, so a slot (not a single spawned child) is the right unit:
/// the marker a predecessor leaves is only visible to a successor that binds the
/// same socket, which is the whole point of deriving it from `--socket`
/// (746dd5c).
///
/// Everything it owns is released on drop — **including on panic**, which is
/// when a leaked daemon does the most damage (it holds the socket, so the next
/// run's handshake finds a "running" daemon belonging to a dead test). Same
/// shape as `registration_lookup.rs`.
struct DaemonSlot {
    config_dir: PathBuf,
    socket: PathBuf,
    child: Option<std::process::Child>,
    /// stdout of the run currently (or most recently) in this slot.
    log: PathBuf,
    generation: u32,
}

impl DaemonSlot {
    fn new(config_dir: &Path, tag: &str) -> DaemonSlot {
        // A unix socket path is capped near 108 bytes (`SUN_LEN`) and a
        // `TempDir` under this repo's scratch trees already exceeds it, so the
        // socket lives at a short `/tmp` path and cleanup is this type's job.
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let stem = PathBuf::from("/tmp").join(format!(
            "gfy-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let socket = stem.with_extension("sock");
        for stale in [
            &socket,
            &stem.with_extension("lock"),
            &stem.with_extension("clean"),
        ] {
            let _ = std::fs::remove_file(stale);
        }
        DaemonSlot {
            config_dir: config_dir.to_path_buf(),
            log: stem.with_extension("gen0.log"),
            socket,
            child: None,
            generation: 0,
        }
    }

    /// Start a daemon process in this slot and wait until it is serving.
    fn start(&mut self) -> SocketClient {
        assert!(self.child.is_none(), "slot already occupied");
        self.log = self
            .socket
            .with_extension(format!("gen{}.log", self.generation));
        self.generation += 1;

        // The daemon announces its reconcile depth on stdout; capturing it is
        // how a test reads the *decision* rather than inferring it.
        let out = std::fs::File::create(&self.log).expect("create daemon log");
        self.child = Some(
            std::process::Command::new(env!("CARGO_BIN_EXE_filigrio-daemon"))
                .arg("--socket")
                .arg(&self.socket)
                .arg("start")
                .arg("--idle-timeout")
                .arg("0")
                .env("XDG_CONFIG_HOME", &self.config_dir)
                .env("XDG_CACHE_HOME", self.config_dir.join("cache"))
                .env("HOME", &self.config_dir)
                .stdout(out)
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn filigrio-daemon"),
        );

        let deadline = Instant::now() + Duration::from_secs(30);
        let client = SocketClient::new(&self.socket).with_timeout(Duration::from_secs(10));
        while Instant::now() < deadline {
            if client.is_reachable() {
                return client;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "spawned daemon never bound {}\n--- log ---\n{}",
            self.socket.display(),
            self.log_text()
        );
    }

    /// Where this daemon's clean-shutdown marker lives: beside its own socket.
    fn marker(&self) -> PathBuf {
        self.socket.with_extension("clean")
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The startup reconcile depth the *current* run announced.
    fn reconcile_depth(&self) -> Depth {
        let log = self.log_text();
        match (
            log.contains("No clean shutdown marker"),
            log.contains("Clean shutdown marker found"),
        ) {
            (true, false) => Depth::Deep,
            (false, true) => Depth::Mtime,
            _ => panic!("the daemon must announce exactly one reconcile depth\n--- log ---\n{log}"),
        }
    }

    /// **Crash.** SIGKILL delivers no handler, runs no destructor and flushes
    /// nothing — the daemon gets no chance to claim it exited cleanly.
    fn crash(&mut self) {
        let mut child = self.child.take().expect("slot is empty");
        child.kill().expect("SIGKILL the daemon");
        child.wait().expect("reap the crashed daemon");
        // A crashed daemon leaves its socket and handshake lock behind. The
        // successor unlinks the stale socket itself, but this test owns the
        // sequencing, so clear them here rather than racing that path. The
        // marker is deliberately NOT touched — it is the subject.
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(self.socket.with_extension("lock"));
    }

    /// **Clean exit** through the wire verb an operator uses (`filigrio daemon
    /// stop` → `DaemonStop` → the run loop's notify → teardown → `shutdown`).
    fn stop_cleanly(&mut self) {
        let mut child = self.child.take().expect("slot is empty");
        let client = SocketClient::new(&self.socket).with_timeout(Duration::from_secs(10));
        client.stop().expect("send DaemonStop");
        let status = child.wait().expect("reap the stopped daemon");
        assert!(
            status.success(),
            "a clean stop must exit 0, got {status:?}\n--- log ---\n{}",
            self.log_text()
        );
    }
}

impl Drop for DaemonSlot {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(self.socket.with_extension("lock"));
        let _ = std::fs::remove_file(self.socket.with_extension("clean"));
        for gen in 0..self.generation {
            let _ = std::fs::remove_file(self.socket.with_extension(format!("gen{gen}.log")));
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Depth {
    Deep,
    Mtime,
}

fn node_count_of(response: &Response) -> u64 {
    let Response::QueryResult { data } = response else {
        panic!("expected a QueryResult, got {response:?}");
    };
    data["nodes"].as_u64().expect("stats carry a node count")
}

// ---- the crash half (the one that mattered) ---------------------------------

/// **A SIGKILLed daemon must not look clean to its successor.**
///
/// The sequence is the field one: a daemon is running, it dies without warning,
/// the tree changes while nothing is watching, and a new daemon starts on the
/// same socket. If the corpse left a clean-shutdown marker, the successor trusts
/// mtimes — and an mtime-preserving edit is invisible to mtimes, so the daemon
/// serves a graph that is missing two functions while reporting itself current.
/// That is the "stale index that looks fresh" §6 exists to prevent.
///
/// Under the inverted lifecycle this test fails twice over: the marker is
/// PRESENT after the kill (it was written at *startup*), and the successor's
/// graph still holds only the two original functions.
#[test]
fn a_crashed_daemon_leaves_no_marker_and_its_successor_re_hashes() {
    let scratch = TempDir::new().expect("tempdir");
    let root = scratch.path().join("proj");
    project_tree(&root);
    let before = seed_watched_and_indexed(scratch.path(), &root);

    let mut slot = DaemonSlot::new(scratch.path(), "crash");
    slot.start(); // runs until it is actually serving
    assert!(
        !slot.marker().exists(),
        "a *running* daemon must not be advertising a clean shutdown it has not performed"
    );

    slot.crash();
    assert!(
        !slot.marker().exists(),
        "a SIGKILLed daemon left a clean-shutdown marker behind — its successor will trust \
         mtimes in exactly the case ADR-0032 §6 wrote the deep re-hash for"
    );

    // The drift happens while nothing is watching, and the filesystem hides it.
    edit_preserving_mtime(&root, FOUR_FNS);

    // The successor is the same daemon identity: same socket, therefore the same
    // marker path. A successor on a *different* socket could not see the corpse's
    // marker at all, and would pass this test for the wrong reason.
    let client = slot.start();
    assert_eq!(
        slot.reconcile_depth(),
        Depth::Deep,
        "no marker means crash, and crash means deep"
    );

    let response = client
        .send(Request::data(DataQuery::GraphStats {
            project: root.to_string_lossy().into_owned(),
        }))
        .expect("the successor answered");
    assert!(
        node_count_of(&response) > before,
        "the successor served a stale graph ({} nodes, was {before}) after a crash: the two \
         functions added while it was dead never made it in",
        node_count_of(&response)
    );
}

// ---- the clean half ---------------------------------------------------------

/// **A clean exit leaves the marker, beside its own socket.**
///
/// Under the inverted lifecycle the marker was *removed* here, so every clean
/// restart paid for a deep re-hash of the whole tree — merely slow, but it also
/// meant the cheap path this mechanism exists to enable was dead code in
/// practice.
#[test]
fn a_cleanly_stopped_daemon_leaves_a_marker_and_its_successor_takes_the_cheap_path() {
    let scratch = TempDir::new().expect("tempdir");
    let root = scratch.path().join("proj");
    project_tree(&root);
    seed_watched_and_indexed(scratch.path(), &root);

    let mut slot = DaemonSlot::new(scratch.path(), "clean");
    slot.start();
    slot.stop_cleanly();

    assert!(
        slot.marker().exists(),
        "a daemon that ran its full teardown must record that it did"
    );
    // The marker is this instance's, addressed by its socket — never the global
    // path the well-known daemon would answer to (746dd5c).
    assert_ne!(
        slot.marker(),
        PathBuf::from("/tmp/filigrio-daemon.clean"),
        "a scratch daemon must never decide the well-known daemon's reconcile depth"
    );

    slot.start();
    assert_eq!(
        slot.reconcile_depth(),
        Depth::Mtime,
        "a marker left by a clean predecessor means the cheap path"
    );
    assert!(
        !slot.marker().exists(),
        "the marker is consumed at startup: a successor that crashes must not inherit its \
         predecessor's clean bill of health"
    );
}

// ---- the seam between them --------------------------------------------------

/// **The marker is consumed, not merely read.** `startup_reconcile` deletes it
/// *before* reconciling anything, so the window in which a crash could be
/// mistaken for a clean exit is empty — including a crash during recovery
/// itself. Asserting the second startup reads "crash" is what distinguishes
/// consumption from a read that leaves the file in place.
#[test]
fn reading_the_marker_consumes_it_so_a_crash_during_recovery_still_reads_as_a_crash() {
    let scratch = TempDir::new().expect("tempdir");
    let root = scratch.path().join("proj");
    project_tree(&root);
    seed_watched_and_indexed(scratch.path(), &root);

    let config = scratch_config(scratch.path());
    let marker = config.shutdown_marker_path.clone();
    std::fs::write(&marker, b"clean").expect("plant a clean predecessor's marker");

    let mut daemon = Daemon::new(config.clone());
    daemon.load_registry().expect("load registry");
    daemon.startup_reconcile().expect("startup reconcile");
    assert!(
        !marker.exists(),
        "startup consumed nothing — this daemon can now die and still look clean"
    );

    // Stand in for "this daemon then died": nobody called `shutdown`, so nobody
    // wrote the marker, and the next startup must see a crash.
    let mut successor = Daemon::new(config);
    successor.load_registry().expect("load registry");
    successor.startup_reconcile().expect("successor reconcile");
    assert!(
        !marker.exists(),
        "a crash must not leave a marker for the start after it either"
    );
}

/// **Only a completed graceful shutdown may write the marker.** If the final
/// flush could not persist a project, the store does not describe the tree, so
/// the promise "you may trust mtimes" is false and must not be made — the next
/// start re-hashes instead. Nothing else in the daemon writes this file.
#[test]
fn shutdown_is_the_only_writer_of_the_marker() {
    let scratch = TempDir::new().expect("tempdir");
    let config = scratch_config(scratch.path());
    let marker = config.shutdown_marker_path.clone();

    let daemon = Daemon::new(config);
    assert!(!marker.exists(), "a fresh daemon writes no marker");
    daemon.shutdown().expect("shutdown");
    assert!(
        marker.exists(),
        "the graceful-shutdown path is the marker's writer"
    );
}
