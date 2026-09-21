//! ADR-0032a R2 — the apply-time authority gate in `apply_core`.
//!
//! The watcher's producer-side filter (R1) is a *hint*; correctness must not
//! depend on any producer being well-behaved. These tests drive `apply_project`
//! (the public seam over `apply_core`) with changesets that name paths a
//! misbehaving/naive producer might submit, and prove the daemon itself refuses
//! to (a) index an out-of-scope file, and (b) do any work for a no-op change.

use filigrio_daemon::{
    Command, Daemon, DaemonConfig, DataQuery, Project, ProjectStatus, Request, Response,
};
use filigrio_protocol::CommandOutcome;
use std::time::Duration;
use tempfile::TempDir;

/// R2.5 — a wire `ProjectIndex` must actually apply a **mtime-preserved**
/// content drift (restored backup / clock reset): content differs but mtime
/// still matches the manifest. A wire index is always the DEEP reconcile
/// (ADR-0042 F6b: depth is Op-internal, and the shallow fast-path is the
/// watcher's alone), which re-hashes and detects it; before R2.5 that changeset
/// was routed back through the *shallow* signal gate, whose mtime fast-path saw
/// `mtime == manifest` and dropped exactly the drift deep was run to catch — so
/// the fix silently did nothing. Now reconcile output applies authoritatively
/// (no gate).
///
/// (The shallow-vs-deep *contrast* moved to `apply.rs`'s
/// `shallow_reconcile_trusts_mtime_deep_rehashes_and_applies` unit test, which
/// can still address both depths now that only `Op::Reconcile` carries them.)
#[test]
fn wire_index_applies_a_mtime_preserved_content_drift() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let src = root.join("src/lib.rs");
    std::fs::write(&src, "pub fn a() -> u32 { 1 }\n").unwrap();

    let (mut daemon, id) = daemon_with_project(root);

    // Index C0.
    daemon
        .apply_project(&id, &changeset(&["src/lib.rs"], &[], &[]))
        .unwrap();
    let base = daemon.project_status(&id).unwrap();
    assert!(base.node_count > 0, "precondition: C0 indexed");
    // The mtime the manifest recorded for this file (stamped from disk at apply).
    let mtime0 = std::fs::metadata(&src).unwrap().modified().unwrap();

    // Content drift (more code → more nodes) whose mtime is reset to match the
    // manifest — the case the mtime fast-path cannot see.
    std::fs::write(&src, "pub fn a() -> u32 { 1 }\npub fn b() -> u32 { 2 }\n").unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
    f.set_modified(mtime0).unwrap();
    drop(f);

    // The wire index re-hashes, detects it, and — authoritatively, no re-gate —
    // applies. Synchronously (ADR-0042 F6c): no drain call, and the response
    // carries the changed count.
    let resp = daemon.handle_request(Request::command(Command::ProjectIndex {
        project: id.clone(),
        clean: false,
    }));
    let Response::CommandCompleted {
        outcome: CommandOutcome::Indexed { changed, .. },
    } = resp
    else {
        panic!("expected a completed Indexed outcome, got {resp:?}");
    };
    assert!(
        changed > 0,
        "the index outcome must report the applied drift"
    );
    assert!(
        daemon.project_status(&id).unwrap().node_count > base.node_count,
        "the wire index must APPLY the mtime-preserved drift (gate used to drop it)"
    );
}

/// ADR-0042 F6c — a command executes **at ingress**: the graph is already
/// updated when the response arrives, and nothing is left queued for a drain.
/// This is the property that lets an agent index-then-query without a poll
/// (and the one the old `CommandAccepted` ack could not offer).
#[test]
fn a_wire_index_completes_before_the_response_and_queues_nothing() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();

    let (mut daemon, id) = daemon_with_project(root);
    assert_eq!(daemon.project_status(&id).unwrap().node_count, 0);

    let resp = daemon.handle_request(Request::command(Command::ProjectIndex {
        project: id.clone(),
        clean: false,
    }));
    assert!(
        matches!(resp, Response::CommandCompleted { .. }),
        "got {resp:?}"
    );

    // No drain, no sleep, no poll: the graph is already there.
    assert!(
        daemon.project_status(&id).unwrap().node_count > 0,
        "the index must be complete when the response returns"
    );
    assert_eq!(
        daemon.health().queue_depth,
        0,
        "a wire command must never enter the queue (it is the producer lane only)"
    );
}

/// ADR-0042 **F8** on the wire: a `Submit` naming a file that has been deleted
/// since the producer saw it must **complete**, converge the file away, and say
/// so — `changed: N, vanished: 1`.
///
/// Before F8 this exact request failed the command (the engine read the missing
/// path with `?`), which is how the race was found: F6c made the apply's `Err`
/// visible to the client instead of logging it behind an ack. The fix is
/// convergence, not suppression — hence the count on the outcome, so the client
/// is told what happened rather than being handed a clean-looking success.
#[test]
fn wire_submit_reports_the_vanished_count_instead_of_failing() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/keep.rs"), "pub fn keep() -> u32 { 1 }\n").unwrap();
    std::fs::write(root.join("src/gone.rs"), "pub fn gone() -> u32 { 2 }\n").unwrap();

    let (mut daemon, id) = daemon_with_project(root);
    daemon
        .apply_project(&id, &changeset(&["src/keep.rs", "src/gone.rs"], &[], &[]))
        .unwrap();
    let base = daemon.project_status(&id).unwrap();
    assert!(base.node_count > 0, "precondition: both files indexed");

    // The producer saw both files change; one is deleted before the apply reads it.
    std::fs::write(root.join("src/keep.rs"), "pub fn keep() -> u32 { 11 }\n").unwrap();
    std::fs::remove_file(root.join("src/gone.rs")).unwrap();

    let resp = daemon.handle_request(Request::command(Command::Submit {
        project: id.clone(),
        changeset: changeset(&[], &["src/keep.rs", "src/gone.rs"], &[]),
        priority: filigrio_daemon::Priority::Fs,
    }));
    let Response::CommandCompleted {
        outcome: CommandOutcome::Applied {
            changed, vanished, ..
        },
    } = resp
    else {
        panic!("a vanished file must not fail the command, got {resp:?}");
    };
    assert!(changed > 0, "the surviving file was still applied");
    assert_eq!(vanished, 1, "the outcome must report the vanished file");
    assert!(
        daemon.project_status(&id).unwrap().node_count < base.node_count,
        "the vanished file's nodes must be gone"
    );
}

fn daemon_with_project(root: &std::path::Path) -> (Daemon, String) {
    let project = Project::new(root.to_path_buf());
    std::fs::create_dir_all(&project.output_dir).unwrap();
    let id = project.id.clone();
    let daemon = Daemon::new(DaemonConfig::default());
    daemon.registry.lock().add(project).unwrap();
    (daemon, id)
}

fn changeset(added: &[&str], modified: &[&str], removed: &[&str]) -> filigrio_daemon::ChangeSet {
    filigrio_daemon::ChangeSet {
        added: added.iter().map(|s| s.to_string()).collect(),
        modified: modified.iter().map(|s| s.to_string()).collect(),
        removed: removed.iter().map(|s| s.to_string()).collect(),
    }
}

/// R2/T2 — the scope authority is independent of the producer. Even when a
/// changeset names a gitignored path *directly* (bypassing the watcher's R1
/// producer filter entirely), `apply_core` must drop it: zero nodes, zero
/// manifest entries. Before R2, `Engine::apply` read the file by path and indexed
/// it — the pollution ADR-0022 fixed, re-entering through the incremental door.
#[test]
fn apply_refuses_an_out_of_scope_path_named_by_the_producer() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("vendor")).unwrap();
    std::fs::write(root.join(".gitignore"), "vendor/\n").unwrap();
    // Real files on disk — the difference is purely scope (gitignore).
    std::fs::write(
        root.join("vendor/lib.rs"),
        "pub fn vendored() -> u32 { 1 }\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn hello() -> u32 { 42 }\n").unwrap();

    let (daemon, id) = daemon_with_project(root);

    // A producer names the gitignored file directly. The gate must drop it.
    daemon
        .apply_project(&id, &changeset(&["vendor/lib.rs"], &[], &[]))
        .unwrap();
    let after_ignored = daemon.project_status(&id).unwrap();
    assert_eq!(
        after_ignored.node_count, 0,
        "gitignored vendor/lib.rs must NOT be indexed (scope authority)"
    );
    assert_eq!(
        after_ignored.file_count, 0,
        "no manifest entry for out-of-scope file"
    );

    // The in-scope file, same call shape, DOES index — proving the gate is scope,
    // not a blanket refusal.
    daemon
        .apply_project(&id, &changeset(&["src/lib.rs"], &[], &[]))
        .unwrap();
    let after_src = daemon.project_status(&id).unwrap();
    assert!(after_src.node_count > 0, "in-scope src/lib.rs must index");
    assert_eq!(after_src.file_count, 1, "exactly the one in-scope file");
}

/// R2/T3 — a no-op change costs nothing. Re-submitting a file whose *content* is
/// unchanged (here rewritten byte-identical, which bumps mtime so the hash gate,
/// not just the mtime fast-path, is exercised) must be dropped by the dedup gate
/// and short-circuit before extract/relink/persist. Observable proof: `state.json`
/// is not rewritten (its mtime does not advance). Before R2 every event re-ran a
/// full `Engine::apply` + persist regardless.
#[test]
fn a_noop_change_does_not_rewrite_state() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let src = root.join("src/lib.rs");
    std::fs::write(&src, "pub fn hello() -> u32 { 42 }\n").unwrap();

    let (daemon, id) = daemon_with_project(root);

    // First apply: indexes the file and writes state.json.
    daemon
        .apply_project(&id, &changeset(&["src/lib.rs"], &[], &[]))
        .unwrap();
    let baseline = daemon.project_status(&id).unwrap();
    assert!(baseline.node_count > 0);

    let state_json = daemon
        .registry
        .lock()
        .get(&id)
        .unwrap()
        .output_dir
        .join("state.json");
    assert!(state_json.exists(), "first apply must write state.json");
    let mtime_before = std::fs::metadata(&state_json).unwrap().modified().unwrap();

    // Rewrite byte-identical content (bumps fs mtime, same hash) and re-submit as
    // `modified`. The dedup gate must find it Unchanged and short-circuit.
    std::thread::sleep(Duration::from_millis(15)); // ensure a later mtime *would* differ
    std::fs::write(&src, "pub fn hello() -> u32 { 42 }\n").unwrap();
    daemon
        .apply_project(&id, &changeset(&[], &["src/lib.rs"], &[]))
        .unwrap();

    let mtime_after = std::fs::metadata(&state_json).unwrap().modified().unwrap();
    assert_eq!(
        mtime_before, mtime_after,
        "a no-op change must NOT rewrite state.json (dedup short-circuit)"
    );

    let after = daemon.project_status(&id).unwrap();
    assert_eq!(
        after.node_count, baseline.node_count,
        "no-op apply must not change the graph"
    );
    assert_eq!(after.file_count, 1);
}

/// Status honesty across a restart — a per-project `Status` query must page the
/// project's `state.json` in from disk on a cold cache, exactly like every other
/// state read (`state_of`), and the result must decode as the typed wire
/// `ProjectStatus`. The old two-source merge fell back to a fabricated all-zeros
/// status whenever the warm cache missed — so right after a daemon restart every
/// built project reported `node_count: 0` until something else happened to page
/// its state in.
#[test]
fn status_query_pages_state_from_disk_after_restart() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();

    // First daemon lifetime: index the project (writes state.json).
    let (d1, id) = daemon_with_project(root);
    d1.apply_project(&id, &changeset(&["src/lib.rs"], &[], &[]))
        .unwrap();
    let built = d1.project_status(&id).unwrap().node_count;
    assert!(built > 0, "precondition: project indexed on disk");
    drop(d1);

    // "Restart": a fresh daemon with the same registration and a cold cache.
    let (mut d2, _) = daemon_with_project(root);
    let resp = d2.handle_request(Request::data(DataQuery::Status {
        project: Some(id.clone()),
    }));
    let Response::QueryResult { data } = resp else {
        panic!("expected QueryResult for a registered project, got {resp:?}");
    };
    let status: ProjectStatus =
        serde_json::from_value(data).expect("Status must decode as the typed ProjectStatus");
    assert_eq!(
        status.node_count, built,
        "a cold Status must page state.json in from disk, not fabricate zeros"
    );
    assert_eq!(status.file_count, 1);
}

/// Addressing parity — the sync command path must resolve a project by **path
/// or id**. Clients send the cwd *path*; before the two write paths were
/// unified, the inline `project build` handler (a verb ADR-0042 F5 later
/// collapsed into `ProjectIndex`) looked up by raw id only, so a path-addressed
/// build was silently skipped ("project not found") while the same request
/// succeeded against the live daemon — two paths, diverged behavior.
#[test]
fn sync_command_resolves_project_index_by_path() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();

    let (mut daemon, id) = daemon_with_project(root);

    // Address the index by PATH (what a client sends), not the registry id.
    daemon.handle_request(Request::command(Command::ProjectIndex {
        project: root.to_string_lossy().to_string(),
        clean: false,
    }));

    assert!(
        daemon.project_status(&id).unwrap().node_count > 0,
        "a path-addressed ProjectIndex must index (path→id resolution)"
    );
}

/// An unregistered project must come back as a typed **error response**, not a
/// daemon-side log line the client never sees (ADR-0042 F6c: no silent-failure
/// path).
#[test]
fn sync_command_for_an_unregistered_project_is_a_typed_error() {
    let scratch = TempDir::new().unwrap();
    let mut daemon = Daemon::new(DaemonConfig::default());

    let resp = daemon.handle_request(Request::command(Command::ProjectIndex {
        project: scratch.path().to_string_lossy().to_string(),
        clean: false,
    }));
    let Response::Error { message } = resp else {
        panic!("expected a typed error for an unregistered project, got {resp:?}");
    };
    assert!(
        message.contains("not registered"),
        "the error must say the project is not registered, got: {message}"
    );
}

/// ADR-0042 F5 + F6c + F7 — `ProjectIndex { clean: true }` (reserved
/// wipe-and-reindex) is rejected **before any work**, and the rejection now
/// reaches the CLIENT as a typed error (it used to be an `error!` log behind an
/// `CommandAccepted` ack — the exact silent-failure shape F6c deletes). Nothing
/// is written. A plain `clean: false` index on the same daemon then works,
/// proving the rejection is the flag, not the verb. The message must name
/// `--clean`, the flag the CLI offers since F7 — a rejection that names a flag
/// the user cannot type is its own kind of silent failure.
#[test]
fn resident_clean_reindex_errors_to_the_client_and_writes_nothing() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();

    let (mut daemon, id) = daemon_with_project(root);
    let out = daemon.registry.lock().get(&id).unwrap().output_dir.clone();

    let resp = daemon.handle_request(Request::command(Command::ProjectIndex {
        project: id.clone(),
        clean: true,
    }));
    let Response::Error { message } = resp else {
        panic!("a reserved --clean must be a typed error to the client, got {resp:?}");
    };
    assert!(
        message.contains("not implemented yet") && message.contains("--clean"),
        "the error must name the flag and say it isn't implemented: {message}"
    );
    assert!(
        !message.contains("--force"),
        "the error must not name the pre-F7 flag: {message}"
    );
    assert!(
        !out.join("state.json").exists(),
        "a rejected --clean must apply nothing (no job, no state.json)"
    );

    // Same daemon, same project, clean off → indexes normally.
    daemon.handle_request(Request::command(Command::ProjectIndex {
        project: id.clone(),
        clean: false,
    }));
    assert!(
        daemon.project_status(&id).unwrap().node_count > 0,
        "clean: false must keep today's behavior"
    );
}

/// ADR-0042 Phase 1c F2 — the sync command path honors the explicit export
/// verb: an index leaves no `graph.json` behind (the apply path no longer
/// snapshots), and `ProjectExport` then materializes it, reporting the written
/// path in its outcome (F6c).
#[test]
fn project_export_writes_graph_json_and_reports_its_path() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();

    let (mut daemon, id) = daemon_with_project(root);
    daemon.handle_request(Request::command(Command::ProjectIndex {
        project: id.clone(),
        clean: false,
    }));

    let out = daemon.registry.lock().get(&id).unwrap().output_dir.clone();
    assert!(
        out.join("state.json").exists(),
        "index persisted state.json"
    );
    assert!(
        !out.join("graph.json").exists(),
        "an apply must NOT produce graph.json (ADR-0042 F2)"
    );

    let resp = daemon.handle_request(Request::command(Command::ProjectExport {
        project: id.clone(),
    }));
    let Response::CommandCompleted {
        outcome: CommandOutcome::Exported { path, .. },
    } = resp
    else {
        panic!("expected an Exported outcome, got {resp:?}");
    };
    assert_eq!(path, out.join("graph.json").display().to_string());
    assert!(
        out.join("graph.json").exists(),
        "the explicit export verb materializes graph.json"
    );
}

/// R2 — a genuinely changed file still applies through the gate (the gate drops
/// only *unchanged* and *out-of-scope*, never real work). Guards against an
/// over-eager gate that would starve legitimate edits.
#[test]
fn a_real_change_still_applies_through_the_gate() {
    let scratch = TempDir::new().unwrap();
    let root = scratch.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let src = root.join("src/lib.rs");
    std::fs::write(&src, "pub fn hello() -> u32 { 1 }\n").unwrap();

    let (daemon, id) = daemon_with_project(root);
    daemon
        .apply_project(&id, &changeset(&["src/lib.rs"], &[], &[]))
        .unwrap();
    let before = daemon.project_status(&id).unwrap();

    // Real content change: add a second function → graph must grow.
    std::fs::write(
        &src,
        "pub fn hello() -> u32 { 1 }\npub fn world() -> u32 { 2 }\n",
    )
    .unwrap();
    daemon
        .apply_project(&id, &changeset(&[], &["src/lib.rs"], &[]))
        .unwrap();
    let after = daemon.project_status(&id).unwrap();
    assert!(
        after.node_count > before.node_count,
        "a real edit must apply (before={}, after={})",
        before.node_count,
        after.node_count
    );
}

/// Deleting a function from a file that **stays** must leave the graph, on both
/// lanes. Before the shrink guard asked *where* a removed node lived, it compared
/// node counts and allowed a drop only when a whole file was removed — so this,
/// the most ordinary edit there is, was rejected on every lane: the watcher
/// retried the apply forever while the graph served the deleted function, and a
/// wire `ProjectIndex` failed with `shrink guard: apply would drop N → N-1`.
/// Found live on next.js; the pipeline-level twin is `end_to_end.rs`'s
/// `deleting_a_function_from_a_kept_file_lands_with_the_guard_on`.
fn two_fns_then_one(root: &std::path::Path) -> (Daemon, String, usize) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    let src = root.join("src/lib.rs");
    std::fs::write(
        &src,
        "pub fn keep() -> u32 { 1 }\npub fn drop_me() -> u32 { 2 }\n",
    )
    .unwrap();
    let (daemon, id) = daemon_with_project(root);
    daemon
        .apply_project(&id, &changeset(&["src/lib.rs"], &[], &[]))
        .unwrap();
    let before = daemon.project_status(&id).unwrap().node_count;
    // A normal save: new content, fresh mtime — the file itself is kept.
    std::fs::write(&src, "pub fn keep() -> u32 { 1 }\n").unwrap();
    (daemon, id, before)
}

#[test]
fn a_function_deleted_from_a_kept_file_leaves_the_graph_on_the_producer_lane() {
    let scratch = TempDir::new().unwrap();
    let (daemon, id, before) = two_fns_then_one(scratch.path());

    daemon
        .apply_project(&id, &changeset(&[], &["src/lib.rs"], &[]))
        .expect("removing a function from a modified file is not an unexplained shrink");
    let after = daemon.project_status(&id).unwrap().node_count;
    assert!(
        after < before,
        "the deleted function must leave the graph ({before} → {after})"
    );
}

#[test]
fn a_function_deleted_from_a_kept_file_leaves_the_graph_on_a_wire_index() {
    let scratch = TempDir::new().unwrap();
    let (mut daemon, id, before) = two_fns_then_one(scratch.path());

    let resp = daemon.handle_request(Request::command(Command::ProjectIndex {
        project: id.clone(),
        clean: false,
    }));
    assert!(
        matches!(
            resp,
            Response::CommandCompleted {
                outcome: CommandOutcome::Indexed { changed, .. },
            } if changed > 0
        ),
        "the index must complete and report the edit, got {resp:?}"
    );
    let after = daemon.project_status(&id).unwrap().node_count;
    assert!(
        after < before,
        "the deleted function must leave the graph ({before} → {after})"
    );
}
