//! ADR-0042 Phase 1c **F4 / B12** — persistence is a cadence policy, not a side
//! effect of apply.
//!
//! Before F4 every apply wrote `state.json` synchronously (~653 ms / 210 MB at
//! next.js scale, ledger §5g) and then **re-read it** to refresh the resident
//! cache. F4 makes *when* we persist a decision of the lane the apply came from:
//!
//! - a **client** apply (a wire `ProjectIndex`/`Submit`/`ProjectWatch on`, or a
//!   one-shot) flushes before the response is produced — CI needs the exit code
//!   to mean "it is on disk";
//! - a **producer-lane** apply (the watcher) updates resident state and marks
//!   the project dirty; the flusher persists on quiescence, a max-dirty-age cap,
//!   graceful shutdown, or dirty LRU eviction.
//!
//! Durability contract these tests encode: the graph is a **derived index** and
//! the repo is the source of truth, so an un-flushed window can be lost — a
//! restart reconciles it away. What must never happen is a *corrupt* or
//! *half-applied* on-disk state, or an out-of-process reader being lied to
//! about how fresh `.filigrio-out` is.
//!
//! Timers are driven by an injected [`TestClock`] — no test sleeps.

mod common;

use common::canonicalize;
use filigrio_core::GraphStore;
use filigrio_daemon::{
    ChangeSet, Command, Daemon, DaemonConfig, FlushConfig, Project, Request, Response, TestClock,
};
use filigrio_protocol::CommandOutcome;
use filigrio_store::FsStore;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

// ---- harness ------------------------------------------------------------

/// A daemon whose flush cadence is driven by a fake clock: quiescence 30 s and
/// max-dirty-age 300 s (the B12 defaults), but advanced explicitly by the test.
fn daemon_with_clock(root: &Path) -> (Daemon, String, Arc<TestClock>) {
    let project = Project::new(root.to_path_buf());
    std::fs::create_dir_all(&project.output_dir).unwrap();
    let id = project.id.clone();
    let clock = Arc::new(TestClock::new());
    let daemon = Daemon::new(DaemonConfig {
        clock: clock.clone(),
        // `shutdown()` writes the clean-shutdown marker (ADR-0032 §6), and the
        // default path is the global `/tmp/filigrio-daemon.clean` — a test that
        // ran a graceful shutdown would leave a marker in the *shared*
        // namespace, telling whatever daemon reads it that a clean shutdown it
        // knows nothing about has happened. Scratch it, like every other
        // lifecycle file here.
        shutdown_marker_path: root.join("daemon.clean"),
        ..DaemonConfig::default()
    });
    daemon.registry.lock().add(project).unwrap();
    (daemon, id, clock)
}

fn write_src(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn cs(added: &[&str], modified: &[&str]) -> ChangeSet {
    ChangeSet {
        added: added.iter().map(|s| s.to_string()).collect(),
        modified: modified.iter().map(|s| s.to_string()).collect(),
        removed: vec![],
    }
}

fn out_dir(daemon: &Daemon, id: &str) -> std::path::PathBuf {
    daemon.registry.lock().get(id).unwrap().output_dir.clone()
}

/// The state actually on disk (what an out-of-process reader of `.filigrio-out`
/// sees), or `None` if nothing has been persisted yet.
fn on_disk(daemon: &Daemon, id: &str) -> Option<filigrio_core::GraphState> {
    let dir = out_dir(daemon, id);
    if !dir.join("state.json").exists() {
        return None;
    }
    FsStore::new(&dir).load_state().unwrap()
}

/// The state the daemon is serving (resident), via its own cache.
fn resident(daemon: &Daemon, id: &str) -> filigrio_core::GraphState {
    let project = daemon.registry.lock().get(id).unwrap().clone();
    (*daemon.flusher().state_of(&project).unwrap()).clone()
}

// ---- 1. a producer-lane apply does not write, it marks dirty ------------

/// F4/B12 — the watcher's lane is **write-behind**: the apply updates resident
/// state and marks the project dirty, and `state.json` is not touched. Before
/// F4 this same edit cost a full 210 MB store rewrite (at next.js scale) per
/// keystroke-batch.
#[test]
fn a_producer_lane_apply_marks_dirty_and_writes_nothing() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    write_src(root, "src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    let (daemon, id, _clock) = daemon_with_clock(root);

    daemon
        .apply_producer(&id, &cs(&["src/lib.rs"], &[]))
        .unwrap();

    assert!(
        on_disk(&daemon, &id).is_none(),
        "a producer-lane apply must NOT write state.json (write-behind, B12)"
    );
    assert!(
        daemon.flusher().is_dirty(&id),
        "the project must be marked dirty so the flusher knows to persist it"
    );
    assert!(
        !resident(&daemon, &id).graph.nodes.is_empty(),
        "…but the resident state IS updated — in-daemon queries are unaffected"
    );
    assert_eq!(
        daemon.flusher().writes_total(),
        0,
        "no store write may have happened yet"
    );
}

// ---- 2. quiescence ------------------------------------------------------

/// F4/B12 — after N seconds with no further apply to a project (default 30 s),
/// the flusher persists it, and the on-disk state then equals the resident one.
#[test]
fn quiescence_flushes_and_disk_then_equals_resident() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    write_src(root, "src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    let (daemon, id, clock) = daemon_with_clock(root);

    daemon
        .apply_producer(&id, &cs(&["src/lib.rs"], &[]))
        .unwrap();

    // Not yet quiescent: 29 s of the 30 s window.
    clock.advance(Duration::from_secs(29));
    daemon.flusher().flush_due();
    assert!(
        on_disk(&daemon, &id).is_none(),
        "the quiescence window must not fire early"
    );

    clock.advance(Duration::from_secs(2));
    daemon.flusher().flush_due();

    let disk = on_disk(&daemon, &id).expect("quiescence must persist the project");
    assert_eq!(
        canonicalize(&disk),
        canonicalize(&resident(&daemon, &id)),
        "after a flush, disk must equal what the daemon serves"
    );
    assert!(
        !daemon.flusher().is_dirty(&id),
        "a flushed project is clean"
    );
    assert_eq!(daemon.flusher().writes_total(), 1);
}

// ---- 3. max-dirty-age cap ----------------------------------------------

/// F4/B12 — sustained churn never reaches quiescence, so the cap (default
/// 300 s since the project first went dirty) is what bounds the crash window.
/// Without it a project edited every 20 s would stay unpersisted forever.
#[test]
fn max_dirty_age_forces_a_flush_under_continuous_applies() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    write_src(root, "src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    let (daemon, id, clock) = daemon_with_clock(root);

    daemon
        .apply_producer(&id, &cs(&["src/lib.rs"], &[]))
        .unwrap();

    // Churn: an apply every 20 s — the 30 s quiescence window never elapses.
    // The cap is 300 s from when the project *first* went dirty (t = 0), so the
    // poll at t = 300 is the first one that may fire, and it must.
    let mut fired_at = None;
    for i in 0..30u64 {
        clock.advance(Duration::from_secs(20));
        write_src(
            root,
            "src/lib.rs",
            &format!("pub fn a() -> u32 {{ {i} }}\npub fn b{i}() {{}}\n"),
        );
        daemon
            .apply_producer(&id, &cs(&[], &["src/lib.rs"]))
            .unwrap();
        daemon.flusher().flush_due();
        if daemon.flusher().writes_total() > 0 {
            fired_at = Some((i + 1) * 20);
            break;
        }
    }

    assert_eq!(
        fired_at,
        Some(300),
        "the cap must fire at exactly max_dirty_age after the project went dirty — \
         never (quiescence starvation) and never early"
    );
    assert!(on_disk(&daemon, &id).is_some());
    assert_eq!(
        daemon.flusher().writes_total(),
        1,
        "exactly one write: the cap, not per-apply write-through"
    );
}

// ---- 4. graceful shutdown ----------------------------------------------

/// F4/B12 — graceful shutdown flushes every dirty project. This is what keeps
/// the ordinary stop/start cycle free of re-indexing work.
#[test]
fn graceful_shutdown_flushes_dirty_projects() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    write_src(root, "src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    let (daemon, id, _clock) = daemon_with_clock(root);

    daemon
        .apply_producer(&id, &cs(&["src/lib.rs"], &[]))
        .unwrap();
    assert!(on_disk(&daemon, &id).is_none(), "precondition: unpersisted");

    daemon.shutdown().unwrap();

    let disk = on_disk(&daemon, &id).expect("graceful shutdown must persist dirty state");
    assert!(!disk.graph.nodes.is_empty());
    assert!(!daemon.flusher().is_dirty(&id));
}

// ---- 5. dirty LRU eviction ---------------------------------------------

/// F4/B12 — the LRU's dirty/evicted path stops being a `warn!`-and-drop. Before
/// F4 a project paged out with unpersisted state lost that work silently (the
/// path was unreachable only because everything was write-through; write-behind
/// makes it live).
#[test]
fn dirty_eviction_persists_before_dropping() {
    let scratch = TempDir::new().unwrap();
    let a_root = scratch.path().join("a");
    let b_root = scratch.path().join("b");
    write_src(&a_root, "src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    write_src(&b_root, "src/lib.rs", "pub fn b() -> u32 { 2 }\n");

    // Capacity 1: applying to `b` must page `a` out.
    let clock = Arc::new(TestClock::new());
    let daemon = Daemon::new(DaemonConfig {
        cache_capacity: 1,
        clock: clock.clone(),
        ..DaemonConfig::default()
    });
    for root in [&a_root, &b_root] {
        let p = Project::new(root.clone());
        std::fs::create_dir_all(&p.output_dir).unwrap();
        daemon.registry.lock().add(p).unwrap();
    }

    daemon
        .apply_producer("a", &cs(&["src/lib.rs"], &[]))
        .unwrap();
    assert!(
        on_disk(&daemon, "a").is_none(),
        "precondition: `a` is dirty, unwritten"
    );

    daemon
        .apply_producer("b", &cs(&["src/lib.rs"], &[]))
        .unwrap();

    let a_disk = on_disk(&daemon, "a")
        .expect("a dirty project evicted from the cache must be written back, not dropped");
    assert!(
        !a_disk.graph.nodes.is_empty(),
        "the evicted project's work must survive the eviction"
    );
    assert!(
        !daemon.flusher().is_dirty("a"),
        "an evicted project is no longer dirty"
    );
}

// ---- 6. a client command flushes before responding ----------------------

/// F4/B12 — a wire command is someone waiting for an outcome: CI reads the exit
/// code as "it is on disk". The assertion is on **disk**, not on the outcome —
/// an outcome that reports success for unpersisted state is the exact failure
/// this pins.
#[test]
fn a_wire_index_has_flushed_before_its_response() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    write_src(root, "src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    let (mut daemon, id, _clock) = daemon_with_clock(root);

    let resp = daemon.handle_request(Request::command(Command::ProjectIndex {
        project: id.clone(),
        clean: false,
    }));
    assert!(
        matches!(
            resp,
            Response::CommandCompleted {
                outcome: CommandOutcome::Indexed { changed, .. }
            } if changed > 0
        ),
        "precondition: the index applied something, got {resp:?}"
    );

    let disk = on_disk(&daemon, &id).expect("a client command must flush before responding");
    assert_eq!(
        canonicalize(&disk),
        canonicalize(&resident(&daemon, &id)),
        "the response must not claim success for unpersisted state"
    );
    assert!(!daemon.flusher().is_dirty(&id));
}

/// F4/B12 — the guarantee is about the *project*, not just this apply: a client
/// command run on a project made dirty by the watcher must leave nothing
/// unpersisted, even when the command itself applies no change (`changed == 0`).
#[test]
fn a_wire_index_also_flushes_dirt_left_by_the_producer_lane() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    write_src(root, "src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    let (mut daemon, id, _clock) = daemon_with_clock(root);

    // Producer lane brings the whole tree in; nothing on disk yet.
    daemon
        .apply_producer(&id, &cs(&["src/lib.rs"], &[]))
        .unwrap();
    assert!(daemon.flusher().is_dirty(&id), "precondition: dirty");

    // A wire index over a now-clean tree: no drift, so nothing to apply.
    let resp = daemon.handle_request(Request::command(Command::ProjectIndex {
        project: id.clone(),
        clean: false,
    }));
    assert!(
        matches!(resp, Response::CommandCompleted { .. }),
        "got {resp:?}"
    );

    assert!(
        !daemon.flusher().is_dirty(&id),
        "after a client command returns Ok the project must not be dirty"
    );
    assert!(on_disk(&daemon, &id).is_some());
}

// ---- 7. the `project flush` verb ---------------------------------------

/// F4/B12 — `flush` is the explicit verb (deliberately NOT "checkpoint", which
/// Phase 3 uses for commit-keyed publication). It persists, reports whether it
/// wrote, and is an honest no-op when the project is already clean.
#[test]
fn project_flush_persists_and_is_idempotent_when_clean() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    write_src(root, "src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    let (mut daemon, id, _clock) = daemon_with_clock(root);

    daemon
        .apply_producer(&id, &cs(&["src/lib.rs"], &[]))
        .unwrap();
    assert!(on_disk(&daemon, &id).is_none(), "precondition: unpersisted");

    let resp = daemon.handle_request(Request::command(Command::ProjectFlush {
        project: id.clone(),
    }));
    let Response::CommandCompleted {
        outcome: CommandOutcome::Flushed { wrote, .. },
    } = resp
    else {
        panic!("expected a Flushed outcome, got {resp:?}");
    };
    assert!(wrote, "a dirty project's flush must report that it wrote");
    let disk = on_disk(&daemon, &id).expect("flush must persist");
    assert_eq!(canonicalize(&disk), canonicalize(&resident(&daemon, &id)));

    // Idempotent: a second flush writes nothing and says so.
    let resp = daemon.handle_request(Request::command(Command::ProjectFlush {
        project: id.clone(),
    }));
    let Response::CommandCompleted {
        outcome: CommandOutcome::Flushed { wrote, .. },
    } = resp
    else {
        panic!("expected a Flushed outcome, got {resp:?}");
    };
    assert!(!wrote, "a clean project's flush must be an honest no-op");
    assert_eq!(
        daemon.flusher().writes_total(),
        1,
        "an idempotent flush must not rewrite 210 MB for nothing"
    );
}

// ---- 8. status honesty --------------------------------------------------

/// F4/B12 (ADR-0029 applied to persistence) — out-of-process readers of
/// `.filigrio-out` may now lag by up to N seconds. `status` must say so:
/// dirty + how long it has been dirty, and how long ago it was last persisted.
#[test]
fn status_reports_dirty_since_and_last_persisted() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    write_src(root, "src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    let (daemon, id, clock) = daemon_with_clock(root);

    let clean = daemon.project_status(&id).unwrap();
    assert!(!clean.dirty, "an untouched project is not dirty");
    assert_eq!(clean.dirty_for_secs, None);

    daemon
        .apply_producer(&id, &cs(&["src/lib.rs"], &[]))
        .unwrap();
    clock.advance(Duration::from_secs(12));

    let dirty = daemon.project_status(&id).unwrap();
    assert!(dirty.dirty, "an unpersisted apply must be visible as dirty");
    assert_eq!(
        dirty.dirty_for_secs,
        Some(12),
        "status must say how stale the on-disk copy may be"
    );
    assert_eq!(
        dirty.last_persisted_secs_ago, None,
        "nothing has been persisted by this daemon yet"
    );

    daemon.flusher().flush_project(&id).unwrap();
    clock.advance(Duration::from_secs(5));

    let flushed = daemon.project_status(&id).unwrap();
    assert!(!flushed.dirty);
    assert_eq!(flushed.dirty_for_secs, None);
    assert_eq!(flushed.last_persisted_secs_ago, Some(5));
}

// ---- 9. the crash window ------------------------------------------------

/// F4/B12 durability contract — a crash loses at most the un-flushed window and
/// **restart reconciles to the same state**, never to a wrong one. Simulated by
/// dropping the daemon (no shutdown, so no flush) after a producer-lane apply,
/// then starting a fresh daemon and running the converging verb.
///
/// Compared with the ADR-0032 §7 identity comparator (`common::canonicalize`),
/// not a count — a graph that converges on node counts but not on edges is
/// exactly the failure this contract has to exclude.
#[test]
fn a_crash_between_apply_and_flush_reconciles_to_the_same_state() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    write_src(root, "src/main.rs", "fn main() {\n    alpha();\n}\n");
    write_src(root, "src/a.rs", "fn alpha() {\n    helper();\n}\n");
    write_src(root, "src/b.rs", "fn helper() {}\n");

    // Lifetime 1: converge, flush (a client command), then edit under the
    // producer lane and **crash** before the flusher runs.
    let expected = {
        let (mut daemon, id, _clock) = daemon_with_clock(root);
        daemon.handle_request(Request::command(Command::ProjectIndex {
            project: id.clone(),
            clean: false,
        }));
        write_src(root, "src/b.rs", "pub fn helper() {}\npub fn extra() {}\n");
        daemon.apply_producer(&id, &cs(&[], &["src/b.rs"])).unwrap();
        assert!(
            daemon.flusher().is_dirty(&id),
            "precondition: unpersisted work"
        );
        let expected = canonicalize(&resident(&daemon, &id));
        // Crash: the process disappears. No shutdown, no flush.
        drop(daemon);
        expected
    };

    // Lifetime 2: a fresh daemon loads the (older) checkpoint and converges.
    let (mut daemon, id, _clock) = daemon_with_clock(root);
    let stale = on_disk(&daemon, &id).expect("the pre-crash checkpoint survives");
    assert_ne!(
        canonicalize(&stale),
        expected,
        "precondition: the crash really did lose the un-flushed window"
    );

    daemon.handle_request(Request::command(Command::ProjectIndex {
        project: id.clone(),
        clean: false,
    }));

    assert_eq!(
        canonicalize(&on_disk(&daemon, &id).unwrap()),
        expected,
        "restart + reconcile must converge to exactly the pre-crash state"
    );
}

// ---- 10. burst coalescing ----------------------------------------------

/// F4/B12 — the point of write-behind, counted rather than timed: M applies
/// inside one quiescence window collapse to **one** store write. This is the
/// §5g ledger claim in test form.
#[test]
fn a_burst_of_applies_inside_one_window_writes_the_store_once() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    write_src(root, "src/lib.rs", "pub fn a() -> u32 { 0 }\n");
    let (daemon, id, clock) = daemon_with_clock(root);

    const BURST: usize = 12;
    for i in 0..BURST {
        write_src(
            root,
            "src/lib.rs",
            &format!("pub fn a() -> u32 {{ {i} }}\npub fn b{i}() {{}}\n"),
        );
        let which = if i == 0 {
            cs(&["src/lib.rs"], &[])
        } else {
            cs(&[], &["src/lib.rs"])
        };
        daemon.apply_producer(&id, &which).unwrap();
        // The flusher is polling all along — it must find nothing due.
        clock.advance(Duration::from_secs(2));
        daemon.flusher().flush_due();
    }
    assert_eq!(
        daemon.flusher().writes_total(),
        0,
        "{BURST} applies inside the quiescence window must not have written yet"
    );

    clock.advance(Duration::from_secs(31));
    daemon.flusher().flush_due();

    assert_eq!(
        daemon.flusher().writes_total(),
        1,
        "the whole burst must collapse to exactly one store write (was {BURST})"
    );
    assert_eq!(
        canonicalize(&on_disk(&daemon, &id).unwrap()),
        canonicalize(&resident(&daemon, &id)),
        "coalescing must not cost correctness — the one write is the final state"
    );
}

// ---- knobs --------------------------------------------------------------

/// The B12 defaults, pinned so a silent change is a test failure rather than a
/// wider crash window in production.
#[test]
fn flush_cadence_defaults_are_30s_quiescence_and_300s_cap() {
    let cfg = FlushConfig::default();
    assert_eq!(cfg.quiescence, Duration::from_secs(30));
    assert_eq!(cfg.max_dirty_age, Duration::from_secs(300));
    assert_eq!(DaemonConfig::default().flush.quiescence, cfg.quiescence);
    assert_eq!(
        DaemonConfig::default().flush.max_dirty_age,
        cfg.max_dirty_age
    );
}

/// The cadence is configurable (B12: "default N = 30, config"): a daemon
/// configured with a shorter window flushes on that window, not on the default.
#[test]
fn a_configured_quiescence_window_is_what_fires() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    write_src(root, "src/lib.rs", "pub fn a() -> u32 { 1 }\n");

    let project = Project::new(root.to_path_buf());
    std::fs::create_dir_all(&project.output_dir).unwrap();
    let id = project.id.clone();
    let clock = Arc::new(TestClock::new());
    let daemon = Daemon::new(DaemonConfig {
        clock: clock.clone(),
        flush: FlushConfig {
            quiescence: Duration::from_secs(5),
            max_dirty_age: Duration::from_secs(60),
        },
        ..DaemonConfig::default()
    });
    daemon.registry.lock().add(project).unwrap();

    daemon
        .apply_producer(&id, &cs(&["src/lib.rs"], &[]))
        .unwrap();
    clock.advance(Duration::from_secs(6));
    daemon.flusher().flush_due();

    assert!(
        on_disk(&daemon, &id).is_some(),
        "the configured 5 s window must be what fires, not the 30 s default"
    );
}

/// F2 + F4 compose: the export verb reads `state.json`, so under write-behind it
/// must flush first — otherwise `graph.json` would be an export of a *stale*
/// checkpoint while the daemon serves something newer.
#[test]
fn export_flushes_first_so_graph_json_is_not_a_stale_snapshot() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    write_src(root, "src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    let (mut daemon, id, _clock) = daemon_with_clock(root);

    daemon
        .apply_producer(&id, &cs(&["src/lib.rs"], &[]))
        .unwrap();
    assert!(on_disk(&daemon, &id).is_none(), "precondition: unpersisted");

    let resp = daemon.handle_request(Request::command(Command::ProjectExport {
        project: id.clone(),
    }));
    assert!(
        matches!(
            resp,
            Response::CommandCompleted {
                outcome: CommandOutcome::Exported { .. }
            }
        ),
        "got {resp:?}"
    );

    let dir = out_dir(&daemon, &id);
    let exported: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("graph.json")).unwrap()).unwrap();
    assert!(
        !exported["nodes"].as_array().unwrap().is_empty(),
        "export must reflect the resident state, not an empty/stale checkpoint"
    );
    assert!(!daemon.flusher().is_dirty(&id));
}
