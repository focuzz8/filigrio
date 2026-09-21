//! Apply-path concurrency wiring (ADR-0032 §2). The `ProjectLocks` primitive is
//! unit-tested in `project_locks.rs`; here we prove the *apply seam* actually
//! goes through it: `apply_project` serializes on the same project and stays
//! independent across projects. These drive the real engine on a tiny project.

use filigrio_core::ChangeSet;
use filigrio_daemon::{Daemon, DaemonConfig, Project};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Write a minimal buildable Rust project rooted at `base/name`, returning the
/// project (its id is `name`).
fn tiny_project(base: &std::path::Path, name: &str) -> Project {
    let root = base.join(name);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "fn main() {\n    helper();\n}\nfn helper() {}\n",
    )
    .unwrap();
    Project::new(root)
}

fn add_changeset() -> ChangeSet {
    ChangeSet {
        added: vec!["src/main.rs".into()],
        modified: vec![],
        removed: vec![],
    }
}

fn test_config(dir: &std::path::Path) -> DaemonConfig {
    DaemonConfig {
        registry_path: dir.join("registry.json"),
        socket_path: dir.join("d.sock"),
        shutdown_marker_path: dir.join("d.clean"),
        ..DaemonConfig::default()
    }
}

/// C4 — **serialize within a project.** While the target project's lock is held
/// externally, an `apply_project` for that same project must block; it proceeds
/// only once the lock is released. This is the lost-update guard wired into the
/// apply path.
#[test]
fn c4_apply_blocks_while_same_project_lock_held() {
    let cfg_tmp = tempfile::TempDir::new().unwrap();
    let proj_tmp = tempfile::TempDir::new().unwrap();
    let daemon = Daemon::new(test_config(cfg_tmp.path()));
    let project = tiny_project(proj_tmp.path(), "target");
    daemon.registry.lock().add(project).unwrap();
    let daemon = Arc::new(daemon);

    // Hold the target project's lock.
    let lock = daemon.locks().lock_for("target").unwrap();
    let guard = lock.lock();

    let (tx, rx) = mpsc::channel();
    let d2 = daemon.clone();
    let worker = thread::spawn(move || {
        d2.apply_project("target", &add_changeset()).unwrap();
        tx.send(()).unwrap();
    });

    // The apply must NOT complete while we hold the project's lock.
    assert!(
        rx.recv_timeout(Duration::from_millis(400)).is_err(),
        "apply ran while the project lock was held → not serialized within the project"
    );

    // Release → the apply proceeds.
    drop(guard);
    rx.recv_timeout(Duration::from_secs(10))
        .expect("apply did not proceed after the project lock was released");
    worker.join().unwrap();
}

/// C5 — **parallelize across projects.** Holding project A's lock must not block
/// an apply to a *different* project B. A global lock would fail this; per-project
/// locks let B proceed immediately.
#[test]
fn c5_apply_to_other_project_is_not_blocked() {
    let cfg_tmp = tempfile::TempDir::new().unwrap();
    let proj_tmp = tempfile::TempDir::new().unwrap();
    let daemon = Daemon::new(test_config(cfg_tmp.path()));
    daemon
        .registry
        .lock()
        .add(tiny_project(proj_tmp.path(), "proj_a"))
        .unwrap();
    daemon
        .registry
        .lock()
        .add(tiny_project(proj_tmp.path(), "proj_b"))
        .unwrap();
    let daemon = Arc::new(daemon);

    // Hold A's lock the whole time.
    let lock_a = daemon.locks().lock_for("proj_a").unwrap();
    let _guard_a = lock_a.lock();

    let (tx, rx) = mpsc::channel();
    let d2 = daemon.clone();
    let worker = thread::spawn(move || {
        d2.apply_project("proj_b", &add_changeset()).unwrap();
        tx.send(()).unwrap();
    });

    rx.recv_timeout(Duration::from_secs(10))
        .expect("apply to project B blocked behind project A's lock → not per-project");
    worker.join().unwrap();
}
