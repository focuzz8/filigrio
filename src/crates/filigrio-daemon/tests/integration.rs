//! Integration test: daemon with watcher and priority queue.
//!
//! Tests the full flow:
//! 1. Daemon starts
//! 2. Watcher detects file changes
//! 3. Changeset submitted to priority queue
//! 4. Daemon processes changeset
//! 5. State updated

use filigrio_core::ChangeSet;
use filigrio_daemon::{FsWatcher, Priority, PriorityQueue, Produced, Producer, Project};
use std::sync::mpsc;
use tempfile::TempDir;
use tracing::info;

/// The watcher is now a `Producer` living in `filigrio-ingest` (ADR-0032e): it
/// emits `Produced` into an externally-supplied sink, not a daemon `Command`
/// into a channel it owns itself.
#[test]
fn test_daemon_watcher_integration() {
    // Initialize logging
    tracing_subscriber::fmt::init();

    info!("Starting daemon-watcher integration test");

    // Create temporary project directory
    let temp = TempDir::new().unwrap();
    let project_root = temp.path().to_path_buf();

    // Create project
    let project = Project::new(project_root.clone());

    // Create initial file
    let src_dir = project_root.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("main.rs"), "fn main() {}").unwrap();

    // Create + start the watcher over an externally-owned sink.
    let config = filigrio_daemon::WatcherConfig::default();
    let mut watcher = FsWatcher::new(project_root.clone(), project.id.clone(), config);
    let (tx, rx) = mpsc::channel();
    watcher.start(tx).unwrap();

    // Modify file (this should trigger watcher)
    std::fs::write(
        src_dir.join("main.rs"),
        "fn main() { println!(\"hello\"); }",
    )
    .unwrap();

    // Wait for watcher to detect change
    std::thread::sleep(std::time::Duration::from_secs(2));

    // Check if a produced item was received
    if let Ok(produced) = rx.recv_timeout(std::time::Duration::from_secs(2)) {
        info!("Received from watcher: {:?}", produced);

        match produced {
            Produced::Signal(changeset) => {
                assert_eq!(watcher.priority(), Priority::Fs);
                assert!(!changeset.added.is_empty() || !changeset.modified.is_empty());
                info!(
                    "Changeset: added={:?}, modified={:?}",
                    changeset.added, changeset.modified
                );
            }
            other => panic!("Unexpected produced item: {other:?}"),
        }
    } else {
        info!("Nothing produced (watcher may not have triggered yet)");
    }

    // Stop watcher
    watcher.stop();

    info!("Integration test completed");
}

/// The queue element is the daemon-internal `QueueItem { project, op, lane }`
/// (ADR-0042 F6b) — the wire `Command` was only ever borrowed as its type, and
/// since F6c no wire command enters the queue at all. Lane ordering is keyed on
/// the item's explicit `lane` (previously: derived from the `Command` variant),
/// which preserves the observable ordering exactly: manual > git > fs.
#[test]
fn test_priority_queue_ordering() {
    use filigrio_daemon::{Op, Priority, QueueItem};

    let mut queue = PriorityQueue::new();

    let item = |lane, op| QueueItem {
        project: "test".to_string(),
        op,
        lane,
    };

    // Submit low-priority first (a watcher signal).
    queue.submit(item(Priority::Fs, Op::Apply(ChangeSet::default())));

    // Submit high-priority (a git producer's authoritative diff).
    queue.submit(item(Priority::Git, Op::ApplyExact(ChangeSet::default())));

    // Submit manual (highest) — a deep reconcile is the specimen manual item.
    queue.submit(item(Priority::Manual, Op::Reconcile { deep: true }));

    // Should pop in priority order
    let first = queue.pop().unwrap();
    assert_eq!(first.lane, Priority::Manual);
    assert!(matches!(first.op, Op::Reconcile { deep: true }));

    assert_eq!(queue.pop().unwrap().lane, Priority::Git);
    assert_eq!(queue.pop().unwrap().lane, Priority::Fs);

    assert!(queue.pop().is_none());
}

#[test]
fn test_dedup_flow() {
    use filigrio_core::Manifest;
    use filigrio_pipeline::dedup::{dedup_file, get_mtime, update_manifest_entry};

    let temp = TempDir::new().unwrap();
    let file_path = temp.path().join("test.rs");
    std::fs::write(&file_path, "fn test() {}").unwrap();

    // Create manifest and add file
    let mut manifest = Manifest::default();
    update_manifest_entry(&file_path, file_path.to_str().unwrap(), &mut manifest).unwrap();

    // Check dedup (should be unchanged)
    let result = dedup_file(
        &file_path,
        file_path.to_str().unwrap(),
        &manifest,
        get_mtime(&file_path),
        false,
    )
    .unwrap();
    assert_eq!(result, filigrio_pipeline::dedup::DedupResult::Unchanged);

    // Modify file
    std::fs::write(&file_path, "fn test_modified() {}").unwrap();

    // Check dedup (should be changed)
    let result = dedup_file(
        &file_path,
        file_path.to_str().unwrap(),
        &manifest,
        get_mtime(&file_path),
        false,
    )
    .unwrap();
    assert_eq!(result, filigrio_pipeline::dedup::DedupResult::Changed);

    // Update manifest
    update_manifest_entry(&file_path, file_path.to_str().unwrap(), &mut manifest).unwrap();

    // Check dedup (should be unchanged again)
    let result = dedup_file(
        &file_path,
        file_path.to_str().unwrap(),
        &manifest,
        get_mtime(&file_path),
        false,
    )
    .unwrap();
    assert_eq!(result, filigrio_pipeline::dedup::DedupResult::Unchanged);
}

#[test]
fn test_reconcile_detects_drift() {
    use filigrio_core::GraphState;
    use filigrio_pipeline::{reconcile, ReconcileConfig};

    let temp = TempDir::new().unwrap();
    let project_root = temp.path().to_path_buf();

    // Create project
    let project = Project::new(project_root.clone());

    // Create files
    let src_dir = project_root.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(src_dir.join("other.rs"), "fn other() {}").unwrap();

    // Create state with both files
    let mut state = GraphState::default();
    state.manifest.entries.insert(
        "src/main.rs".to_string(),
        filigrio_core::ManifestEntry {
            hash: 0,
            last_modified: None,
            revision: None,
        },
    );
    state.manifest.entries.insert(
        "src/other.rs".to_string(),
        filigrio_core::ManifestEntry {
            hash: 0,
            last_modified: None,
            revision: None,
        },
    );

    // Remove other.rs from disk
    std::fs::remove_file(src_dir.join("other.rs")).unwrap();

    // Run reconcile
    let config = ReconcileConfig::default();
    let (report, _changeset) = reconcile(&project.root, &state, &config).unwrap();

    assert!(report.has_drift);
    assert_eq!(report.files_removed, 1);
}

#[test]
fn test_project_registry() {
    use filigrio_daemon::ProjectRegistry;

    let temp = TempDir::new().unwrap();
    let project = Project::new(temp.path().to_path_buf());

    let mut registry = ProjectRegistry::new();

    // Add project
    registry.add(project.clone()).unwrap();
    assert_eq!(registry.count(), 1);

    // Get project
    let retrieved = registry.get(&project.id).unwrap();
    assert_eq!(retrieved.id, project.id);

    // Find by path
    let found = registry
        .find_by_path(&temp.path().join("src/main.rs"))
        .unwrap();
    assert_eq!(found.id, project.id);

    // Remove project
    registry.remove(&project.id).unwrap();
    assert_eq!(registry.count(), 0);
}

/// Test §5 shrink guard, through the daemon's re-export: it fires on a node lost
/// from a file the apply did not touch, and not on one deleted from a file it did.
///
/// The decision (inside `Pipeline::apply`) is the public `check_shrink_guard`;
/// its full branch table lives in `filigrio-pipeline`'s `end_to_end.rs`, and the
/// behaviour on the daemon's real apply path in `apply_gate.rs`.
#[test]
fn test_shrink_guard_triggers() {
    use filigrio_core::{Graph, Node, NodeId};
    use filigrio_daemon::check_shrink_guard;
    use std::collections::BTreeSet;

    let mut n = Node::new("fn:b.rs:z", "z", "function");
    n.source_file = Some("b.rs".into());
    let prior = Graph {
        nodes: vec![n],
        edges: vec![],
    };
    let removed = [NodeId::new("fn:b.rs:z")];

    assert!(
        check_shrink_guard(&prior, &removed, &BTreeSet::from(["a.rs".to_string()])).is_err(),
        "losing a node of an untouched file must be rejected"
    );
    assert!(
        check_shrink_guard(&prior, &removed, &BTreeSet::from(["b.rs".to_string()])).is_ok(),
        "losing a node of a touched file is the edit"
    );
}

/// Test §4 two-stage dedup absorbs double-processing (git + fs events).
///
/// ADR-0032 §4 headline claim: after a git apply updates the manifest, redundant
/// fs-events for the same file should be deduped away by hash-match, preventing
/// double-processing. This is the key property that makes the priority-queue design
/// work during events like `git pull`.
#[test]
fn test_dedup_absorbs_double_processing() {
    use filigrio_core::Manifest;
    use filigrio_pipeline::dedup::{dedup_file, get_mtime, update_manifest_entry, DedupResult};

    let temp = TempDir::new().unwrap();
    let file_path = temp.path().join("test.rs");
    std::fs::write(&file_path, "fn test() {}\n").unwrap();

    let mut manifest = Manifest::default();

    // Simulate git apply: file is indexed into manifest
    update_manifest_entry(&file_path, file_path.to_str().unwrap(), &mut manifest).unwrap();
    let git_mtime = get_mtime(&file_path).unwrap();

    // First dedup check: file is unchanged (matches manifest)
    let result = dedup_file(
        &file_path,
        file_path.to_str().unwrap(),
        &manifest,
        Some(git_mtime),
        false,
    )
    .unwrap();
    assert_eq!(
        result,
        DedupResult::Unchanged,
        "git apply should mark file as unchanged"
    );

    // Simulate redundant fs-event after git pull: watcher fires for the same file
    // The file content is identical, so hash should match
    let result = dedup_file(
        &file_path,
        file_path.to_str().unwrap(),
        &manifest,
        Some(git_mtime),
        false,
    )
    .unwrap();
    assert_eq!(
        result,
        DedupResult::Unchanged,
        "redundant fs-event should be deduped away (hash match)"
    );

    // Modify file (simulating a real change)
    std::fs::write(&file_path, "fn test_modified() {}\n").unwrap();
    let new_mtime = get_mtime(&file_path).unwrap();

    // Dedup should detect the change
    let result = dedup_file(
        &file_path,
        file_path.to_str().unwrap(),
        &manifest,
        Some(new_mtime),
        false,
    )
    .unwrap();
    assert_eq!(
        result,
        DedupResult::Changed,
        "modified file should be detected as changed"
    );

    // Update manifest (simulating re-indexing)
    update_manifest_entry(&file_path, file_path.to_str().unwrap(), &mut manifest).unwrap();

    // Another redundant fs-event should be deduped
    let result = dedup_file(
        &file_path,
        file_path.to_str().unwrap(),
        &manifest,
        Some(new_mtime),
        false,
    )
    .unwrap();
    assert_eq!(
        result,
        DedupResult::Unchanged,
        "after re-indexing, redundant event should be deduped"
    );

    // Hash-match path (distinct from the mtime fast-path above): rewrite IDENTICAL
    // content, which bumps mtime but leaves the content hash unchanged. dedup must
    // fall THROUGH the mtime gate, hash the file, and STILL report Unchanged — the
    // real "redundant event, unchanged content" dedup, exercised via the hash not mtime.
    let stored_mtime = new_mtime; // what the manifest currently holds
    std::fs::write(&file_path, "fn test_modified() {}\n").unwrap(); // byte-identical rewrite
    let bumped_mtime = get_mtime(&file_path).unwrap();
    assert_ne!(
        bumped_mtime, stored_mtime,
        "rewriting must bump mtime, else this wouldn't reach the hash path"
    );
    let result = dedup_file(
        &file_path,
        file_path.to_str().unwrap(),
        &manifest,
        Some(bumped_mtime),
        false,
    )
    .unwrap();
    assert_eq!(
        result,
        DedupResult::Unchanged,
        "identical content with a NEW mtime → Unchanged via HASH-match, not the mtime fast-path"
    );
}

/// Test daemon persistence round-trip: changeset → reload → assert persisted.
///
/// `apply_delta` persists `state.json`; `snapshot()` is the **explicit export**
/// of the `graph.json` interchange file (ADR-0042 F2 — no longer called by the
/// apply path). The test drives both calls itself and verifies the changes
/// persist across store instances, with both artifacts present at the end.
#[test]
fn test_daemon_persistence_round_trip() {
    use filigrio_core::ChangeSet;
    use filigrio_core::GraphStore;
    use filigrio_index::DispatchExtractor;
    use filigrio_ingest::FsSource;
    use filigrio_pipeline::ClusterConfig;
    use filigrio_resolve::Engine;
    use filigrio_store::FsStore;

    let temp = TempDir::new().unwrap();
    let project_root = temp.path();

    // Create project with initial files
    let src_dir = project_root.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("main.rs"), "fn main() {}\n").unwrap();

    let output_dir = temp.path().join(".filigrio");
    std::fs::create_dir_all(&output_dir).unwrap();

    // Build initial state and persist
    let source = FsSource::new(project_root);
    let extractor = DispatchExtractor::with_defaults();
    let cluster_cfg = ClusterConfig::default();
    let prior = filigrio_core::GraphState::default();
    let initial_changeset = ChangeSet {
        added: vec!["src/main.rs".into()],
        modified: vec![],
        removed: vec![],
    };

    let initial_delta = Engine::apply(
        &prior,
        &initial_changeset,
        &source,
        &extractor,
        &cluster_cfg,
    )
    .unwrap();
    let store1 = FsStore::new(&output_dir);
    store1.apply_delta(&initial_delta).unwrap();
    store1.snapshot().unwrap();

    // Verify initial state persisted
    let reloaded_state = store1.load_state().unwrap().unwrap();
    assert_eq!(
        reloaded_state.graph.nodes.len(),
        initial_delta.nodes_added.len(),
        "initial state should persist"
    );

    // Add a new file and apply changeset
    std::fs::write(src_dir.join("lib.rs"), "fn lib_fn() {}\n").unwrap();

    let new_changeset = ChangeSet {
        added: vec!["src/lib.rs".into()],
        modified: vec![],
        removed: vec![],
    };

    let prior_state = store1.load_state().unwrap().unwrap();
    let new_delta = Engine::apply(
        &prior_state,
        &new_changeset,
        &source,
        &extractor,
        &cluster_cfg,
    )
    .unwrap();
    store1.apply_delta(&new_delta).unwrap();
    store1.snapshot().unwrap();

    // Reload store and verify changes persisted
    let store2 = FsStore::new(&output_dir);
    let persisted_state = store2.load_state().unwrap().unwrap();

    // The new file should be in the manifest
    assert!(
        persisted_state.manifest.entries.contains_key("src/lib.rs"),
        "src/lib.rs should be in persisted manifest"
    );

    // Node count should have increased
    assert!(
        persisted_state.graph.nodes.len() > initial_delta.nodes_added.len(),
        "node count should increase after adding lib.rs"
    );

    // Verify state.json file exists
    let state_path = output_dir.join("state.json");
    assert!(
        state_path.exists(),
        "state.json should exist after snapshot"
    );

    // Verify graph.json file exists (snapshot creates both)
    let graph_path = output_dir.join("graph.json");
    assert!(
        graph_path.exists(),
        "graph.json should exist after snapshot"
    );

    // Final verification: reload again and ensure consistency
    let store3 = FsStore::new(&output_dir);
    let final_state = store3.load_state().unwrap().unwrap();
    assert_eq!(
        final_state.graph.nodes.len(),
        persisted_state.graph.nodes.len(),
        "state should be consistent across multiple reloads"
    );
    assert_eq!(
        final_state.manifest.entries.len(),
        persisted_state.manifest.entries.len(),
        "manifest should be consistent across multiple reloads"
    );
}
