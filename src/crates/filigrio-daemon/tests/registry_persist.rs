//! Registry persistence fragility suite (ADR-0032 §2: the registry survives a
//! daemon restart). The dangerous failure mode is *silent* loss — a first run or
//! a truncated write that quietly drops every registered project — so several of
//! these tests are about erroring loudly vs. starting empty.

use filigrio_daemon::{Command, Daemon, DaemonConfig, Project, ProjectRegistry, Request};
use std::path::PathBuf;
use tempfile::TempDir;

fn project_at(base: &std::path::Path, name: &str) -> Project {
    let root = base.join(name);
    std::fs::create_dir_all(&root).unwrap();
    Project::new(root)
}

/// R1 — first-ever run: an absent registry file yields an empty registry, NOT a
/// startup crash. A naive `File::open(...)?` would abort the daemon on ENOENT.
#[test]
fn r1_absent_file_is_empty_not_error() {
    let missing = PathBuf::from("/tmp/filigrio-does-not-exist-xyz/registry.json");
    let reg = ProjectRegistry::load_from(&missing).expect("absent file must not error");
    assert_eq!(reg.count(), 0);
}

/// R3 — round-trip fidelity: save then load reproduces the same projects,
/// including the derived `output_dir` — no field silently dropped by the serde shape.
#[test]
fn r3_round_trip_preserves_projects() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("registry.json");

    let mut reg = ProjectRegistry::new();
    let p1 = project_at(tmp.path(), "alpha");
    let p2 = project_at(tmp.path(), "beta");
    reg.add(p1.clone()).unwrap();
    reg.add(p2.clone()).unwrap();
    reg.save_to(&path).unwrap();

    let loaded = ProjectRegistry::load_from(&path).unwrap();
    assert_eq!(loaded.count(), 2);
    let g1 = loaded.get(&p1.id).expect("alpha must round-trip");
    assert_eq!(g1.root, p1.root);
    assert_eq!(g1.output_dir, p1.output_dir);
    assert!(loaded.get(&p2.id).is_some(), "beta must round-trip");
}

/// R2 — the parent directory (`~/.config/filigrio/`) may not exist yet; `save_to`
/// must create it rather than failing the very first persist.
#[test]
fn r2_save_creates_missing_parent_dirs() {
    let tmp = TempDir::new().unwrap();
    let path = tmp
        .path()
        .join("nested")
        .join("deeper")
        .join("registry.json");
    assert!(!path.parent().unwrap().exists());

    let mut reg = ProjectRegistry::new();
    reg.add(project_at(tmp.path(), "alpha")).unwrap();
    reg.save_to(&path).unwrap();

    assert!(path.exists(), "save must create missing parent dirs");
}

/// R4 — a corrupt registry file is a LOUD error, not a silent empty registry:
/// silently starting empty would erase every registered project without a trace.
#[test]
fn r4_corrupt_file_errors_loudly() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("registry.json");
    std::fs::write(&path, b"{ this is not valid json ]").unwrap();

    let result = ProjectRegistry::load_from(&path);
    assert!(
        result.is_err(),
        "a corrupt registry must error, never silently drop all projects"
    );
}

/// R5 — an overwrite fully replaces the prior file (atomic temp+rename), leaving
/// no partial state and no stray temp files. A crash mid-write must never corrupt
/// the previous good file.
#[test]
fn r5_overwrite_is_atomic_and_complete() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("registry.json");

    let mut first = ProjectRegistry::new();
    first.add(project_at(tmp.path(), "alpha")).unwrap();
    first.add(project_at(tmp.path(), "beta")).unwrap();
    first.save_to(&path).unwrap();

    let mut second = ProjectRegistry::new();
    second.add(project_at(tmp.path(), "gamma")).unwrap();
    second.save_to(&path).unwrap();

    let loaded = ProjectRegistry::load_from(&path).unwrap();
    assert_eq!(
        loaded.count(),
        1,
        "overwrite must fully replace, not merge/corrupt"
    );
    assert!(loaded.find_by_path(&tmp.path().join("gamma/x")).is_some());

    // No leftover temp file beside the final registry.
    let strays: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.contains("registry") && n != "registry.json"
        })
        .collect();
    assert!(
        strays.is_empty(),
        "atomic write left a stray temp file: {strays:?}"
    );
}

/// R7 — the duplicate-registration invariant survives a persist/load cycle:
/// loading a saved registry and re-adding a known project is still rejected.
#[test]
fn r7_dedup_invariant_survives_persistence() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("registry.json");
    let p = project_at(tmp.path(), "alpha");

    let mut reg = ProjectRegistry::new();
    reg.add(p.clone()).unwrap();
    reg.save_to(&path).unwrap();

    let mut loaded = ProjectRegistry::load_from(&path).unwrap();
    assert!(
        loaded.add(p).is_err(),
        "a project persisted then reloaded must still be a duplicate"
    );
}

/// R8 — `ProjectRegister` is handled **once**. `handle_request` registers the project
/// synchronously (so a follow-up query sees it immediately); it must NOT also
/// enter the queue — a drain would then run `handle_project_register` a second time,
/// fail on the duplicate-registration invariant, and log a spurious "project
/// add failed" error for every successful registration. Since ADR-0042 F6c the
/// queue is the producer lane only, so this holds for *every* command; the
/// assertion is now "the queue stayed empty".
#[test]
fn r8_project_register_is_not_double_executed() {
    let cfg_tmp = TempDir::new().unwrap();
    let proj_tmp = TempDir::new().unwrap();

    let cfg = DaemonConfig {
        registry_path: cfg_tmp.path().join("registry.json"),
        socket_path: cfg_tmp.path().join("d.sock"),
        shutdown_marker_path: cfg_tmp.path().join("d.clean"),
        ..DaemonConfig::default()
    };

    let mut daemon = Daemon::new(cfg);
    let proj_root = proj_tmp.path().join("myproj");
    std::fs::create_dir_all(&proj_root).unwrap();
    daemon.handle_request(Request::command(Command::ProjectRegister {
        path: proj_root.to_string_lossy().to_string(),
    }));

    assert_eq!(
        daemon.registry.lock().count(),
        1,
        "add must apply synchronously"
    );
    assert_eq!(
        daemon.health().queue_depth,
        0,
        "a synchronously-handled ProjectRegister must not be queued for a drain"
    );
    assert_eq!(daemon.registry.lock().count(), 1);
}

/// ADR-0042 F6b — the persisted `watch` flag round-trips through the registry
/// file, and a **pre-F6b registry** (no `watch` key at all) loads as unwatched
/// rather than failing to parse or defaulting to on: an upgrade must not
/// silently start inotify watches on every previously registered project.
#[test]
fn r9_watch_flag_round_trips_and_pre_f6b_files_load_unwatched() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("registry.json");

    let mut reg = ProjectRegistry::new();
    let watched = project_at(tmp.path(), "followed");
    let cold = project_at(tmp.path(), "cold");
    let (watched_id, cold_id) = (watched.id.clone(), cold.id.clone());
    reg.add(watched).unwrap();
    reg.add(cold).unwrap();
    reg.set_watch(&watched_id, true).unwrap();
    reg.save_to(&path).unwrap();

    let loaded = ProjectRegistry::load_from(&path).unwrap();
    assert!(
        loaded.get(&watched_id).unwrap().watch,
        "watch = true must survive the round-trip"
    );
    assert!(
        !loaded.get(&cold_id).unwrap().watch,
        "watch = false must survive the round-trip"
    );

    // A pre-F6b document: projects with no `watch` key at all.
    let legacy = tmp.path().join("legacy.json");
    std::fs::write(
        &legacy,
        serde_json::json!({
            "version": 1,
            "projects": [{
                "id": "legacy",
                "root": tmp.path().join("legacy"),
                "output_dir": tmp.path().join("legacy/.filigrio-out"),
            }]
        })
        .to_string(),
    )
    .unwrap();
    let loaded = ProjectRegistry::load_from(&legacy).expect("a pre-F6b registry must still load");
    assert!(
        !loaded.get("legacy").unwrap().watch,
        "a project with no persisted watch key must load as UNWATCHED (default off)"
    );
}

/// `set_watch` on an unregistered project must be a loud error, never a silent
/// no-op that leaves the user believing the daemon is following their repo.
#[test]
fn r10_set_watch_on_an_unknown_project_errors() {
    let mut reg = ProjectRegistry::new();
    let err = reg.set_watch("ghost", true).unwrap_err();
    assert!(
        err.contains("ghost"),
        "the error must name the project: {err}"
    );
}

/// R6 — **end-to-end restart.** `ProjectRegister` through the command path persists the
/// registry, and a fresh daemon pointed at the same file recovers the project.
/// This is the real bug the stubbed `save_registry` hides: today the add is lost
/// on restart.
#[test]
fn r6_project_register_survives_daemon_restart() {
    let cfg_tmp = TempDir::new().unwrap();
    let proj_tmp = TempDir::new().unwrap();
    let registry_path = cfg_tmp.path().join("registry.json");

    let cfg = DaemonConfig {
        registry_path: registry_path.clone(),
        socket_path: cfg_tmp.path().join("d.sock"),
        shutdown_marker_path: cfg_tmp.path().join("d.clean"),
        ..DaemonConfig::default()
    };

    // Add a project via the command path (executes synchronously, ADR-0042 F6c).
    let mut daemon = Daemon::new(cfg.clone());
    let proj_root = proj_tmp.path().join("myproj");
    std::fs::create_dir_all(&proj_root).unwrap();
    daemon.handle_request(Request::command(Command::ProjectRegister {
        path: proj_root.to_string_lossy().to_string(),
    }));

    assert!(
        registry_path.exists(),
        "ProjectRegister must persist the registry to disk"
    );

    // A fresh daemon (a "restart") must recover the registered project.
    let mut restarted = Daemon::new(cfg);
    restarted.load_registry().unwrap();
    assert_eq!(
        restarted.registry.lock().count(),
        1,
        "restart must recover the project registered before shutdown"
    );
}
