//! Checkpoint/Resume tests — ADR-0032 §6.
//!
//! Tests the daemon's ability to:
//! - Persist state to `state.json` checkpoints
//! - Resume from checkpoints after crash or clean shutdown
//! - Run appropriate reconcile (mtime vs deep) based on clean-shutdown marker
//! - Handle edge cases: corruption, clock skew, concurrent startups
//!
//! Mental model:
//!   Daemon Start -> Load state.json -> Read AND CONSUME clean-shutdown marker
//!                                    |
//!                    -------------------------------
//!                    |                               |
//!              Clean exists                   Clean absent
//!                    |                               |
//!                    v                               v
//!            mtime reconcile               Deep rehash + mtime scan
//!        (cheap, always)                (expensive, crash recovery)
//!                    |                               |
//!                    -------------------------------
//!                                    |
//!                                    v
//!                              Apply deltas
//!                                    |
//!                                    v
//!                        …and nothing else until
//!                    `shutdown()` writes the marker back
//!
//! The marker is written by a *completed graceful shutdown* and consumed at
//! startup — never the reverse. This file used to encode the reverse (marker
//! written at the end of `startup_reconcile`, removed in `shutdown`), which made
//! a crash indistinguishable from a clean exit in the unsafe direction; the
//! lifecycle is now pinned end-to-end, against a real killed process, in
//! `clean_shutdown_marker.rs`.

use filigrio_core::{ChangeSet, GraphState, GraphStore};
use filigrio_daemon::{Daemon, DaemonConfig, Project};
use filigrio_index::DispatchExtractor;
use filigrio_ingest::FsSource;
use filigrio_pipeline::ClusterConfig;
use filigrio_resolve::Engine;
use filigrio_store::FsStore;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

/// A clone of `project` with watch ON. ADR-0042 F6b made startup reconcile
/// (and startup watching) apply to `watch == true` projects only — an unwatched
/// registered project is cold by contract — so every fixture here that exercises
/// `startup_reconcile` must register the project the way `project watch on`
/// would persist it.
fn watched(project: &Project) -> Project {
    let mut p = project.clone();
    p.watch = true;
    p
}

/// Helper: Create a minimal project with state.json checkpoint.
fn create_project_with_checkpoint(root: &Path) -> (Project, GraphState) {
    let project = Project::new(root.to_path_buf());
    let output_dir = project.output_dir.clone();
    fs::create_dir_all(&output_dir).unwrap();

    // Create initial files
    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(src_dir.join("main.rs"), "fn main() {}\n").unwrap();

    // Build and checkpoint state
    let source = FsSource::new(root);
    let extractor = DispatchExtractor::with_defaults();
    let cluster_cfg = ClusterConfig::default();
    let prior = GraphState::default();
    let changeset = ChangeSet {
        added: vec!["src/main.rs".into()],
        modified: vec![],
        removed: vec![],
    };

    let delta = Engine::apply(&prior, &changeset, &source, &extractor, &cluster_cfg).unwrap();
    let store = FsStore::new(&output_dir);
    store.apply_delta(&delta).unwrap();
    store.snapshot().unwrap();

    let state = store.load_state().unwrap().unwrap();
    (project, state)
}

/// Helper: leave behind what a crashed predecessor leaves — nothing. A daemon
/// that is SIGKILLed runs no code, so it writes no marker; the absence *is* the
/// crash signal. (The real thing, with a real killed process, is in
/// `clean_shutdown_marker.rs`; these in-process tests take the state it leaves
/// as their starting point.)
fn simulate_crash(marker_path: &Path) {
    if marker_path.exists() {
        fs::remove_file(marker_path).unwrap();
    }
}

/// Helper: leave behind what a cleanly stopped predecessor leaves — the marker
/// its `shutdown()` wrote, byte-identical (`CLEAN_MARKER`), since a marker that
/// does not read back exactly is treated as torn and therefore as a crash.
fn simulate_clean_shutdown(marker_path: &Path) {
    fs::write(marker_path, filigrio_daemon::CLEAN_MARKER).unwrap();
}

/// Helper: Get file's mtime.
fn get_mtime(path: &Path) -> Option<u64> {
    path.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64) // nanos — match production dedup::get_mtime granularity
}

/// One hour, in the nanosecond units `get_mtime`/`dedup::get_mtime` speak.
const HOUR_NANOS: u64 = 3_600 * 1_000_000_000;

/// Helper: force `path`'s mtime to `nanos` since the epoch — the only way to
/// actually *simulate* clock skew rather than describe it in a comment.
/// `File::set_modified` requires a handle opened for writing.
fn set_mtime_nanos(path: &Path, nanos: u64) {
    let when = std::time::UNIX_EPOCH + Duration::from_nanos(nanos);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

/// Helper: run one clean-shutdown startup reconcile and return the resulting
/// state.
///
/// `create_project_with_checkpoint` builds its delta with `Engine::apply`, which
/// leaves every manifest entry as a `hash: 0, last_modified: None` stub — the
/// real values are stamped by `Pipeline::apply` (filigrio-pipeline/src/lib.rs).
/// A test that needs a manifest with a *real* recorded mtime has to go through
/// the daemon once first; `test_deep_rehash_updates_all_last_modified` relies on
/// the same hydration.
///
/// Leaves the marker **consumed** (startup consumes it), so a caller that wants
/// a particular lifecycle for its own run must set it up after calling this.
fn hydrate_manifest(project: &Project, marker_path: &Path) -> GraphState {
    simulate_clean_shutdown(marker_path);
    let config = DaemonConfig {
        shutdown_marker_path: marker_path.to_path_buf(),
        ..Default::default()
    };
    let mut daemon = Daemon::new(config);
    daemon.registry.lock().add(watched(project)).unwrap();
    daemon.startup_reconcile().unwrap();
    FsStore::new(&project.output_dir)
        .load_state()
        .unwrap()
        .unwrap()
}

/// Test 1: Clean shutdown marker exists -> only mtime reconcile.
///
/// Why: This is the happy path. After a clean shutdown, the daemon should
/// only run the cheap mtime reconcile, not the expensive deep rehash.
/// This validates that the marker correctly gates the reconcile depth.
#[test]
fn test_clean_shutdown_only_mtime_reconcile() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, _state) = create_project_with_checkpoint(root);

    // Write clean-shutdown marker
    let marker_path = &project.output_dir.join("clean-shutdown.marker");
    simulate_clean_shutdown(marker_path);

    // Create daemon with marker path
    let config = DaemonConfig {
        shutdown_marker_path: marker_path.clone(),
        ..Default::default()
    };
    let mut daemon = Daemon::new(config);
    daemon.registry.lock().add(watched(&project)).unwrap();

    // Run startup reconcile
    let result = daemon.startup_reconcile();

    assert!(
        result.is_ok(),
        "startup_reconcile should succeed with clean marker"
    );

    // …and the marker is *consumed*, not rewritten. Startup is not a clean
    // shutdown and may not claim to be one: from here until this daemon's own
    // `shutdown()` there must be no marker on disk, or a crash in between would
    // inherit the predecessor's clean bill of health.
    assert!(
        !marker_path.exists(),
        "the clean-shutdown marker must be consumed by the startup that reads it"
    );
}

/// Test 2: No clean shutdown marker -> deep rehash + mtime reconcile.
///
/// Why: This is the crash recovery path. After a crash, the daemon must
/// run deep rehash to ensure state integrity. This validates that the
/// absence of the marker triggers the expensive reconcile.
#[test]
fn test_crash_recovery_deep_rehash() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, _state) = create_project_with_checkpoint(root);

    // Simulate crash: remove marker
    let marker_path = &project.output_dir.join("clean-shutdown.marker");
    simulate_crash(marker_path);

    // Modify a file (simulate drift during crash)
    let src_dir = root.join("src");
    fs::write(
        src_dir.join("main.rs"),
        "fn main() { println!(\"hello\"); }\n",
    )
    .unwrap();

    // Create daemon
    let config = DaemonConfig {
        shutdown_marker_path: marker_path.clone(),
        ..Default::default()
    };
    let mut daemon = Daemon::new(config);
    daemon.registry.lock().add(watched(&project)).unwrap();

    // Run startup reconcile
    let result = daemon.startup_reconcile();

    assert!(
        result.is_ok(),
        "startup_reconcile should succeed after crash"
    );

    // Recovery does not make this daemon clean. Nothing writes the marker but a
    // completed `shutdown()`, so a daemon that crashes *again* mid-recovery
    // still leaves the absence its successor must read as a crash.
    assert!(
        !marker_path.exists(),
        "crash recovery must not fabricate a clean-shutdown marker"
    );
}

/// Test 3: The marker is *written* on shutdown — and only there.
///
/// Why: the marker is the daemon's signature on "I finished my teardown; the
/// store describes the tree; you may trust mtimes". Only the code that actually
/// finished the teardown can sign it. A daemon that dies before this point
/// leaves no signature, and its successor re-hashes — the fail-safe direction.
///
/// This test used to assert the exact opposite (removed on shutdown, written at
/// startup), and passed, because the implementation was inverted the same way.
#[test]
fn test_marker_written_on_shutdown() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, _state) = create_project_with_checkpoint(root);

    let marker_path = &project.output_dir.join("clean-shutdown.marker");
    simulate_crash(marker_path);

    // Create daemon
    let config = DaemonConfig {
        shutdown_marker_path: marker_path.clone(),
        ..DaemonConfig::default()
    };
    let daemon = Daemon::new(config);

    // Shutdown
    let result = daemon.shutdown();

    assert!(result.is_ok(), "shutdown should succeed");

    assert_eq!(
        fs::read(marker_path).expect("a graceful shutdown must leave its marker"),
        filigrio_daemon::CLEAN_MARKER,
        "the marker must be written whole — a partial one is read as a crash"
    );
}

/// Test 4: State.json persists across daemon restarts.
///
/// Why: This is the core checkpoint contract. The daemon must be able to
/// reload state from disk and continue where it left off. This validates
/// the write/read round-trip of state.json.
#[test]
fn test_state_persists_across_restarts() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, state) = create_project_with_checkpoint(root);

    // Verify initial state
    assert_eq!(state.manifest.entries.len(), 1);
    assert!(state.manifest.entries.contains_key("src/main.rs"));

    // Simulate daemon restart: load state from disk
    let store = FsStore::new(&project.output_dir);
    let loaded_state = store.load_state().unwrap().unwrap();

    // Verify state matches
    assert_eq!(
        loaded_state.manifest.entries.len(),
        state.manifest.entries.len(),
        "manifest entry count should match"
    );
    assert!(
        loaded_state.manifest.entries.contains_key("src/main.rs"),
        "src/main.rs should be in loaded manifest"
    );
    assert_eq!(
        loaded_state.graph.nodes.len(),
        state.graph.nodes.len(),
        "node count should match"
    );
}

/// Test 5: Corrupted state.json is handled gracefully.
///
/// Why: Real-world fragility: state.json can be corrupted by disk failure,
/// partial writes, or manual editing. The daemon should detect corruption
/// and either recover or fail with a clear error message.
#[test]
fn test_corrupted_state_json_handled_gracefully() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, _state) = create_project_with_checkpoint(root);

    // Corrupt state.json
    let state_path = project.output_dir.join("state.json");
    fs::write(&state_path, b"corrupted json {{{").unwrap();

    // Try to load state
    let store = FsStore::new(&project.output_dir);
    let result = store.load_state();

    assert!(
        result.is_err(),
        "loading corrupted state.json should fail with error"
    );
}

/// Test 6: A marker whose content is not the whole witness is treated as a
/// crash — and consumed anyway.
///
/// Why: the marker is written by `shutdown()`, which is exactly when a crash is
/// most plausible, and `fs::write` is create-then-write: dying between the two
/// leaves an empty file. If presence alone meant "clean", that torn write would
/// be read as a completed shutdown — the one direction that must never happen.
/// So the content is compared byte-for-byte.
///
/// The drift this replaces: the old body asserted the marker *content* was
/// `clean` after startup, which the inverted implementation satisfied by
/// overwriting the file at the end of `startup_reconcile`. It therefore passed
/// without ever checking that a corrupt marker produced a deep reconcile.
#[test]
fn test_invalid_marker_treated_as_crash() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, _state) = create_project_with_checkpoint(root);
    let marker_path = &project.output_dir.join("clean-shutdown.marker");

    // The mtime-preserved edit only a deep re-hash can see. If the corrupt
    // marker were trusted, the mtime path would skip it and the daemon would
    // serve a graph that never gained `beta`.
    let src = root.join("src/main.rs");
    let before = fs::metadata(&src).unwrap().modified().unwrap();
    hydrate_manifest(&project, marker_path);
    fs::write(&src, "fn main() {}\nfn beta() {}\n").unwrap();
    fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_modified(before)
        .unwrap();

    for corrupt in [b"".as_slice(), b"clea".as_slice(), b"corrupted".as_slice()] {
        fs::write(marker_path, corrupt).unwrap();

        let config = DaemonConfig {
            shutdown_marker_path: marker_path.clone(),
            ..Default::default()
        };
        let mut daemon = Daemon::new(config);
        daemon.registry.lock().add(watched(&project)).unwrap();

        assert!(
            daemon.startup_reconcile().is_ok(),
            "startup_reconcile should succeed with a corrupt marker ({corrupt:?})"
        );
        // Declined — and still consumed, so it cannot be re-read by the next start.
        assert!(
            !marker_path.exists(),
            "a marker the daemon declined to trust must not survive it ({corrupt:?})"
        );
    }

    let state = FsStore::new(&project.output_dir)
        .load_state()
        .unwrap()
        .unwrap();
    assert!(
        state.graph.nodes.iter().any(|n| n.label == "beta"),
        "a corrupt marker was trusted: the mtime path skipped a change only a re-hash can see"
    );
}

/// Test 7: Drift detected and healed during startup reconcile.
///
/// Why: This validates the reconcile logic. If files changed on disk
/// while daemon was stopped, startup reconcile should detect drift
/// and apply the changeset to heal the state.
#[test]
fn test_drift_detected_and_healed_on_startup() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, state) = create_project_with_checkpoint(root);

    // Record initial manifest
    let initial_mtime = state
        .manifest
        .entries
        .get("src/main.rs")
        .and_then(|e| e.last_modified);

    // Modify file (simulate drift)
    let src_dir = root.join("src");
    thread::sleep(Duration::from_millis(10)); // Ensure mtime changes
    fs::write(
        src_dir.join("main.rs"),
        "fn main() { println!(\"modified\"); }\n",
    )
    .unwrap();

    // Verify mtime changed
    let new_mtime = get_mtime(&src_dir.join("main.rs"));
    assert_ne!(
        initial_mtime, new_mtime,
        "mtime should change after modification"
    );

    // Run startup reconcile (should detect drift)
    let marker_path = &project.output_dir.join("clean-shutdown.marker");
    simulate_clean_shutdown(marker_path);

    let config = DaemonConfig {
        shutdown_marker_path: marker_path.clone(),
        ..Default::default()
    };
    let mut daemon = Daemon::new(config);
    daemon.registry.lock().add(watched(&project)).unwrap();

    let result = daemon.startup_reconcile();

    assert!(
        result.is_ok(),
        "startup_reconcile should detect and heal drift"
    );

    // Verify state was updated
    let store = FsStore::new(&project.output_dir);
    let updated_state = store.load_state().unwrap().unwrap();

    assert_eq!(
        updated_state.manifest.entries.len(),
        1,
        "manifest should still have one entry"
    );

    // Manifest should have updated last_modified
    let updated_entry = updated_state.manifest.entries.get("src/main.rs").unwrap();
    assert_eq!(
        updated_entry.last_modified, new_mtime,
        "manifest should have updated last_modified"
    );
}

/// Test 8: File deleted while daemon stopped -> drift detected and healed.
///
/// Why: This is a common real-world scenario: files are deleted outside
/// the daemon's control (e.g., manual cleanup, git operations). The daemon
/// must detect this and update the state accordingly.
#[test]
fn test_file_deleted_drift_detected_and_healed() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, _state) = create_project_with_checkpoint(root);

    // Delete file (simulate drift)
    let src_dir = root.join("src");
    fs::remove_file(src_dir.join("main.rs")).unwrap();

    // Run startup reconcile (should detect deletion)
    let marker_path = &project.output_dir.join("clean-shutdown.marker");
    simulate_clean_shutdown(marker_path);

    let config = DaemonConfig {
        shutdown_marker_path: marker_path.clone(),
        ..Default::default()
    };
    let mut daemon = Daemon::new(config);
    daemon.registry.lock().add(watched(&project)).unwrap();

    let result = daemon.startup_reconcile();

    assert!(
        result.is_ok(),
        "startup_reconcile should detect file deletion"
    );

    // Verify state was updated (file removed from manifest)
    let store = FsStore::new(&project.output_dir);
    let updated_state = store.load_state().unwrap().unwrap();

    assert!(
        !updated_state.manifest.entries.contains_key("src/main.rs"),
        "deleted file should be removed from manifest"
    );
}

/// Test 9: New file added while daemon stopped -> drift detected and healed.
///
/// Why: This is the counterpart to file deletion. Files added outside the
/// daemon's control must be detected and indexed during startup reconcile.
#[test]
fn test_new_file_added_drift_detected_and_healed() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, _state) = create_project_with_checkpoint(root);

    // Add new file (simulate drift)
    let src_dir = root.join("src");
    thread::sleep(Duration::from_millis(10)); // Ensure mtime changes
    fs::write(src_dir.join("lib.rs"), "fn lib_fn() {}\n").unwrap();

    // Run startup reconcile (should detect addition)
    let marker_path = &project.output_dir.join("clean-shutdown.marker");
    simulate_clean_shutdown(marker_path);

    let config = DaemonConfig {
        shutdown_marker_path: marker_path.clone(),
        ..Default::default()
    };
    let mut daemon = Daemon::new(config);
    daemon.registry.lock().add(watched(&project)).unwrap();

    let result = daemon.startup_reconcile();

    assert!(
        result.is_ok(),
        "startup_reconcile should detect file addition"
    );

    // Verify state was updated (new file in manifest)
    let store = FsStore::new(&project.output_dir);
    let updated_state = store.load_state().unwrap().unwrap();

    assert!(
        updated_state.manifest.entries.contains_key("src/lib.rs"),
        "new file should be added to manifest"
    );
}

/// Test 10: a backward clock step must not hide a real content change.
///
/// Why: real-world fragility — the system clock can be adjusted (NTP step,
/// manual change, a restored backup), which leaves a file whose mtime is
/// *older* than the one already recorded in the manifest. The mtime fast-path
/// (`dedup::dedup_file`) is therefore an **equality** test, not an
/// ordering one: unequal mtime → fall through to the hash, which is the
/// content authority. An "is the file newer than the manifest?" comparison
/// would silently classify this file as unchanged and rot the graph.
///
/// This test performs the skew (`set_mtime`) rather than describing it: until
/// 2026-07-28 the body simulated no skew at all and asserted only
/// `result.is_ok()` on an unmodified tree (audit §K1b).
#[test]
fn test_clock_skew_doesnt_break_mtime_reconcile() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, _state) = create_project_with_checkpoint(root);

    // Clean marker → the cheap mtime reconcile, which is the path under test.
    let marker_path = &project.output_dir.join("clean-shutdown.marker");
    let state = hydrate_manifest(&project, marker_path);

    let before = state
        .manifest
        .entries
        .get("src/main.rs")
        .expect("the checkpoint records src/main.rs")
        .clone();
    let recorded_mtime = before
        .last_modified
        .expect("the checkpoint records an mtime for src/main.rs");

    // A real content change...
    let main_rs = root.join("src/main.rs");
    fs::write(&main_rs, "fn main() { let changed = 1; }\n").unwrap();
    // ...and then the clock steps BACKWARD: the edited file's mtime is now an
    // hour older than the mtime the manifest recorded before the edit.
    set_mtime_nanos(&main_rs, recorded_mtime - HOUR_NANOS);
    assert!(
        get_mtime(&main_rs).unwrap() < recorded_mtime,
        "the fixture must actually skew the mtime backward, not just say so"
    );

    simulate_clean_shutdown(marker_path);

    let config = DaemonConfig {
        shutdown_marker_path: marker_path.clone(),
        ..Default::default()
    };
    let mut daemon = Daemon::new(config);
    daemon.registry.lock().add(watched(&project)).unwrap();

    let result = daemon.startup_reconcile();
    assert!(
        result.is_ok(),
        "startup_reconcile should succeed despite clock skew"
    );

    let store = FsStore::new(&project.output_dir);
    let updated_state = store.load_state().unwrap().unwrap();
    let after = updated_state
        .manifest
        .entries
        .get("src/main.rs")
        .expect("src/main.rs stays in the manifest");
    assert_ne!(
        after.hash, before.hash,
        "an mtime older than the manifest's must NOT be read as 'unchanged' — \
         the gate is equality, and the hash is the authority"
    );
}

/// Test 10b: the other half of the same property — a backward clock step on an
/// *unmodified* file must not fabricate drift either.
///
/// Why: the mtime gate rejects the fast-path on any inequality, so a skewed
/// file always reaches the hash check. The hash matches, so the file is
/// `Unchanged`, there is no drift, and nothing is applied. Without the hash
/// fallback every file in the tree would be re-indexed after any clock
/// adjustment — the false-positive twin of Test 10's false negative.
#[test]
fn test_clock_skew_alone_does_not_force_a_reindex() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, _state) = create_project_with_checkpoint(root);

    let marker_path = &project.output_dir.join("clean-shutdown.marker");
    let state = hydrate_manifest(&project, marker_path);

    let before = state
        .manifest
        .entries
        .get("src/main.rs")
        .expect("the checkpoint records src/main.rs")
        .clone();
    let recorded_mtime = before
        .last_modified
        .expect("the checkpoint records an mtime for src/main.rs");

    // Content untouched; only the clock moves.
    let main_rs = root.join("src/main.rs");
    set_mtime_nanos(&main_rs, recorded_mtime - HOUR_NANOS);
    assert!(get_mtime(&main_rs).unwrap() < recorded_mtime);

    simulate_clean_shutdown(marker_path);

    let config = DaemonConfig {
        shutdown_marker_path: marker_path.clone(),
        ..Default::default()
    };
    let mut daemon = Daemon::new(config);
    daemon.registry.lock().add(watched(&project)).unwrap();

    assert!(daemon.startup_reconcile().is_ok());

    let store = FsStore::new(&project.output_dir);
    let updated_state = store.load_state().unwrap().unwrap();
    let after = updated_state
        .manifest
        .entries
        .get("src/main.rs")
        .expect("src/main.rs stays in the manifest");
    assert_eq!(after.hash, before.hash, "content did not change");
    assert_eq!(
        after.last_modified, before.last_modified,
        "no drift → no apply → the manifest entry is untouched; a rewritten \
         mtime here would mean the skew alone triggered a re-index"
    );
}

/// Test 11: Two startups over one marker — the marker is claimed once.
///
/// Why: the handshake already admits one daemon per socket, so this is the
/// belt-and-braces case (a race in auto-start logic, a misconfigured second
/// instance pointed at the same marker). The marker is a single-use witness:
/// the first startup consumes it and takes the cheap path, the second finds
/// nothing and re-hashes. Erring toward deep is the safe half of the fork, and
/// neither run may leave a marker behind for a third.
#[test]
fn test_concurrent_startup_claims_the_marker_once() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, _state) = create_project_with_checkpoint(root);

    let marker_path = &project.output_dir.join("clean-shutdown.marker");
    simulate_clean_shutdown(marker_path);

    // Create two daemons (simulate concurrent startup)
    let config1 = DaemonConfig {
        shutdown_marker_path: marker_path.clone(),
        ..DaemonConfig::default()
    };
    let config2 = DaemonConfig {
        shutdown_marker_path: marker_path.clone(),
        ..DaemonConfig::default()
    };

    let mut daemon1 = Daemon::new(config1);
    let mut daemon2 = Daemon::new(config2);
    daemon1.registry.lock().add(watched(&project)).unwrap();
    daemon2.registry.lock().add(watched(&project)).unwrap();

    // Both daemons should be able to run startup reconcile
    let result1 = daemon1.startup_reconcile();
    let result2 = daemon2.startup_reconcile();

    assert!(result1.is_ok(), "first daemon startup should succeed");
    assert!(result2.is_ok(), "second daemon startup should succeed");

    assert!(
        !marker_path.exists(),
        "the marker is a single-use witness: neither startup may leave one behind"
    );
}

/// Test 12: Partial state.json write is detected.
///
/// Why: Real-world fragility: power loss or crash during state.json write
/// can leave a partial file. The daemon should detect this (e.g., via
/// atomic rename or checksum) and recover appropriately.
#[test]
fn test_partial_state_write_detected() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, _state) = create_project_with_checkpoint(root);

    // Simulate partial write: truncate state.json
    let state_path = project.output_dir.join("state.json");
    let content = fs::read_to_string(&state_path).unwrap();
    let partial = &content[..content.len() / 2]; // Truncate to half
    fs::write(&state_path, partial).unwrap();

    // Try to load state
    let store = FsStore::new(&project.output_dir);
    let result = store.load_state();

    // Should fail to load partial state
    assert!(
        result.is_err(),
        "loading partial state.json should fail with error"
    );
}

/// Test 13: A marker that cannot be written costs a deep reconcile, not a
/// failed shutdown.
///
/// Why: the marker lives beside the socket the daemon was started on, and that
/// directory can be gone by teardown time (a tmpfs cleaned under a long-lived
/// daemon, a removed run-dir). The daemon does not create it: the marker is an
/// optimisation hint, and the cost of not writing one is a deep reconcile —
/// strictly the safe direction — whereas failing the shutdown over it would turn
/// a cosmetic filesystem problem into a non-zero exit after a successful run.
/// What must never happen is a *phantom* marker.
///
/// This replaces `test_missing_marker_directory_created`, whose premise ("the
/// daemon creates the marker directory") was never true: the directory it
/// observed was created by the store during the reconcile, and the marker landed
/// in it only because the inverted implementation wrote one at startup.
#[test]
fn test_unwritable_marker_directory_costs_a_deep_reconcile_not_a_failed_shutdown() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, _state) = create_project_with_checkpoint(root);

    // A marker path whose parent directory does not exist.
    let marker_path = temp.path().join("gone").join("clean-shutdown.marker");
    assert!(!marker_path.parent().unwrap().exists());

    let config = DaemonConfig {
        shutdown_marker_path: marker_path.clone(),
        ..Default::default()
    };
    let daemon = Daemon::new(config.clone());

    assert!(
        daemon.shutdown().is_ok(),
        "an unwritable marker must not fail a shutdown that has already flushed"
    );
    assert!(
        !marker_path.exists(),
        "no marker means no phantom promise to the next start"
    );

    // …and the next start reads that absence as a crash.
    let mut successor = Daemon::new(config);
    successor.registry.lock().add(watched(&project)).unwrap();
    assert!(successor.startup_reconcile().is_ok());
}

/// Test 14: Permission errors on marker/state are reported clearly.
///
/// Why: Real-world fragility: permission issues can occur (read-only
/// filesystem, incorrect ownership). The daemon should fail with a
/// clear error message, not silently proceed or panic.
///
/// The failure is forced the way `filigrio-store`'s
/// `failed_write_preserves_prior_content` forces it: a `0o555` directory, and a
/// *detected* root run skips rather than false-passing. Until 2026-07-28 this
/// body was empty and `#[ignore]`d, so `--ignored` reported `ok` under a
/// reassuring name (audit §K1a).
#[cfg(unix)]
#[test]
fn test_permission_errors_degrade_toward_deep_never_toward_a_phantom_marker() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, _state) = create_project_with_checkpoint(root);

    // The marker gets its own directory so ONLY the marker write is denied —
    // the project's output dir stays writable, and the failure under test is
    // unambiguously the marker's.
    let marker_dir = temp.path().join("marker-home");
    fs::create_dir_all(&marker_dir).unwrap();
    let marker_path = marker_dir.join("clean-shutdown.marker");

    let config = DaemonConfig {
        shutdown_marker_path: marker_path.clone(),
        ..Default::default()
    };
    let daemon = Daemon::new(config.clone());

    fs::set_permissions(&marker_dir, fs::Permissions::from_mode(0o555)).unwrap();
    // Probe privilege INDEPENDENTLY of the call under test. Keying the skip off
    // the call's own success — the shape `atomic.rs` can use, because there the
    // write *is* the call under test — would also "skip" when the daemon
    // silently produced the wrong state, which is what this test exists to
    // catch.
    let probe = marker_dir.join(".privilege-probe");
    let privileged = fs::write(&probe, b"x").is_ok();
    let result = daemon.shutdown();
    fs::set_permissions(&marker_dir, fs::Permissions::from_mode(0o755)).unwrap();
    let _ = fs::remove_file(&probe);

    if privileged {
        // Running as root, which ignores directory permissions — the failure
        // cannot be forced here, so there is nothing to assert. Skip rather
        // than false-pass.
        eprintln!("skipped: running privileged, so directory permissions are not enforced");
        return;
    }

    // The shutdown itself succeeds: everything that mattered (the flush) is
    // already done, and the daemon is on its way out. What the denied write
    // costs is one deep reconcile on the next start — the safe direction, and
    // the reason this is an `error!` log rather than a returned `Err`. The
    // failure is *not* silent: it names the path and the consequence.
    assert!(
        result.is_ok(),
        "a denied marker write must not fail a shutdown that has already flushed: {result:?}"
    );
    assert!(
        !marker_path.exists(),
        "a denied marker write must not leave a marker behind — a phantom \
         marker would make the next start skip its crash-recovery deep reconcile"
    );

    // And the next start agrees: nothing to trust, so re-hash.
    let mut successor = Daemon::new(config);
    successor.registry.lock().add(watched(&project)).unwrap();
    assert!(successor.startup_reconcile().is_ok());
    assert!(
        !marker_path.exists(),
        "startup must not invent the marker the failed shutdown could not write"
    );
}

/// Test 15: Checkpoint includes unresolved edges (not stripped graph.json).
///
/// Why: ADR-0032 §6 explicitly states that state.json contains the
/// unresolved-edge set, unlike the stripped graph.json export. This test
/// validates that checkpoint preserves this critical information.
#[test]
fn test_checkpoint_includes_unresolved_edges() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    // Create project with unresolved reference
    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(src_dir.join("main.rs"), "fn main() { undefined_fn(); }\n").unwrap();

    let output_dir = root.join(".filigrio");
    fs::create_dir_all(&output_dir).unwrap();

    // Build and checkpoint
    let source = FsSource::new(root);
    let extractor = DispatchExtractor::with_defaults();
    let cluster_cfg = ClusterConfig::default();
    let prior = GraphState::default();
    let changeset = ChangeSet {
        added: vec!["src/main.rs".into()],
        modified: vec![],
        removed: vec![],
    };

    let delta = Engine::apply(&prior, &changeset, &source, &extractor, &cluster_cfg).unwrap();
    let store = FsStore::new(&output_dir);
    store.apply_delta(&delta).unwrap();
    store.snapshot().unwrap();

    // Load checkpoint
    let state = store.load_state().unwrap().unwrap();

    // Verify unresolved edges exist
    let has_unresolved = state
        .graph
        .edges
        .iter()
        .any(|e| matches!(&e.target, filigrio_core::EdgeTarget::Symbol(_)));

    assert!(has_unresolved, "checkpoint should include unresolved edges");
}

/// Test 16: Checkpoint preserves node IDs (ADR-0028 stability).
///
/// Why: ADR-0032 §7 requires that incremental and cold builds produce
/// identical node IDs. This validates that checkpoint preserves IDs
/// correctly across restarts.
#[test]
fn test_checkpoint_preserves_node_ids() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, state) = create_project_with_checkpoint(root);

    // Record node IDs from initial state
    let initial_ids: Vec<_> = state.graph.nodes.iter().map(|n| n.id.clone()).collect();

    // Reload state from checkpoint
    let store = FsStore::new(&project.output_dir);
    let reloaded_state = store.load_state().unwrap().unwrap();

    // Verify node IDs match
    let reloaded_ids: Vec<_> = reloaded_state
        .graph
        .nodes
        .iter()
        .map(|n| n.id.clone())
        .collect();

    assert_eq!(
        initial_ids, reloaded_ids,
        "node IDs should be preserved across checkpoint reload"
    );
}

/// Test 17: Deep rehash after crash updates all last_modified fields.
///
/// Why: After a crash, the deep rehash should update last_modified for
/// all files to current disk state. This ensures subsequent mtime
/// reconciles are accurate.
#[test]
fn test_deep_rehash_updates_all_last_modified() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let (project, _state) = create_project_with_checkpoint(root);

    // Simulate crash (remove marker)
    let marker_path = &project.output_dir.join("clean-shutdown.marker");
    simulate_crash(marker_path);

    // Run deep rehash via startup reconcile
    let config = DaemonConfig {
        shutdown_marker_path: marker_path.clone(),
        ..Default::default()
    };
    let mut daemon = Daemon::new(config);
    daemon.registry.lock().add(watched(&project)).unwrap();

    let result = daemon.startup_reconcile();
    assert!(result.is_ok(), "deep rehash should succeed");

    // Verify last_modified was updated
    let store = FsStore::new(&project.output_dir);
    let updated_state = store.load_state().unwrap().unwrap();

    let entry = updated_state.manifest.entries.get("src/main.rs").unwrap();
    let current_mtime = get_mtime(&root.join("src/main.rs"));

    assert_eq!(
        entry.last_modified, current_mtime,
        "last_modified should match current disk mtime after deep rehash"
    );
}
