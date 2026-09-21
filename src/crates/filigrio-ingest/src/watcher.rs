//! Filesystem watcher — a native-push [`Producer`] (ADR-0032a; relocated from
//! `filigrio-daemon` by ADR-0032e). Turns a live working tree into a stream of
//! [`Produced`] items: `Signal` for ordinary edits (a hint the consumer must
//! gate), `Reconcile` for a `.gitignore` edit (a re-scoping trigger — R3).
//!
//! Knows nothing of `Command`/`Project`/host protocol; it is addressed by a bare
//! `root` + `id` (for tracing) and speaks only the `Producer`/`Produced`
//! taxonomy, so the daemon consumes it exactly like any other producer.

use crate::producer::{Produced, Producer};
use crate::SourceBoundary;
use filigrio_core::{ChangeSet, Error, Priority, Result};
use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{
    new_debouncer, DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache,
};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Filesystem watcher configuration.
#[derive(Clone, Debug)]
pub struct WatcherConfig {
    /// Debounce duration (default: 250 ms — the daemon's own default; see
    /// `DaemonConfig::watcher_debounce`).
    pub debounce: Duration,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(250),
        }
    }
}

/// Filesystem watcher that produces `Signal`/`Reconcile` items (ADR-0032e).
pub struct FsWatcher {
    root: PathBuf,
    /// For tracing only — this producer carries no host `Project`/`Command` concept.
    id: String,
    config: WatcherConfig,
    // `RecommendedCache`, not a concrete cache: `new_debouncer` returns the
    // platform's recommended one, and that alias resolves to `NoCache` on
    // Linux/Android but `FileIdMap` elsewhere (macOS, Windows).
    _debouncer: Option<Debouncer<RecommendedWatcher, RecommendedCache>>,
}

impl FsWatcher {
    /// Create a new filesystem watcher over `root`. `id` is a caller-supplied
    /// label (e.g. a project id) used only for tracing.
    pub fn new(root: PathBuf, id: String, config: WatcherConfig) -> Self {
        FsWatcher {
            root,
            id,
            config,
            _debouncer: None,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }
}

/// A **read** is not a change. notify's inotify backend subscribes to `OPEN`, so
/// every open of a watched file — an editor, `git status`, ripgrep, and our own
/// reconcile walk reading each `.gitignore` or the dedup gate hashing a file —
/// arrives as an `Access` event. Counted as a change, a read of a `.gitignore`
/// requested a full-tree reconcile whose walk read every `.gitignore` again: a
/// self-sustaining loop that ran a reconcile every ~0.4 s on next.js even when
/// idle, and absorbed each real edit into one (~6–7 s, not a per-file apply).
///
/// Only the definite reads are dropped. `Close(Write)` — inotify's
/// `IN_CLOSE_WRITE`, "written and closed" — and any `Access` a backend cannot
/// classify still count, per the rule that an unrecognised kind re-indexes.
fn is_read(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Access(
            AccessKind::Read
                | AccessKind::Open(_)
                | AccessKind::Close(AccessMode::Read | AccessMode::Execute)
        )
    )
}

/// Classify one debounced batch into a content [`ChangeSet`] plus the R3
/// "an ignore file changed" flag.
///
/// This is the whole of the watcher's classification policy, and it lives here
/// rather than inside `start`'s `move`/`'static` closure so that it can be
/// called directly: the four rules below — R3 `.gitignore` detection, the
/// `is_indexable` pre-filter, the boundary scope pre-filter, and the
/// `EventKind` → added/modified/removed mapping — had no unit test at all while
/// they were unreachable from outside the debouncer (audit §G2/§K3).
///
/// `boundary` is supplied by the caller rather than built here, which is what
/// lets `start` keep one across batches (audit §G3); the only I/O this function
/// does of its own is the `is_dir` stat.
fn classify_events(
    events: &[DebouncedEvent],
    root: &Path,
    boundary: &SourceBoundary,
) -> (ChangeSet, bool) {
    let mut changeset = ChangeSet::default();
    // An ignore-file edit re-scopes the walk with no FS event of its own
    // for the affected files (ADR-0032a R3) → trigger a reconcile, not a
    // content Signal. Tracked separately because it survives none of the
    // code/scope filters below (`.gitignore` is neither code nor in-scope).
    let mut ignore_changed = false;

    for event in events {
        let kind = event.event.kind;
        if is_read(kind) {
            continue;
        }
        for path in &event.event.paths {
            // R3: a `.gitignore` change (create/modify/remove all re-scope)
            // is a reconcile trigger — detect it before the filters drop it.
            if path.file_name().is_some_and(|n| n == ".gitignore") {
                ignore_changed = true;
                continue;
            }
            // A removed file is gone from disk; only skip dirs for live kinds.
            if !matches!(kind, EventKind::Remove(_)) && path.is_dir() {
                continue;
            }
            // Producer-side pre-filter (cheap, perf not authority): submit
            // only files whose changes affect the graph — code (→ nodes) or
            // a project manifest (→ workspace, ADR-0019). `is_indexable`, not
            // bare `is_code`, so a `Cargo.toml` edit re-indexes live instead
            // of being dropped. Scope is the boundary's job below.
            if !filigrio_core::is_indexable(&path.to_string_lossy()) {
                continue;
            }
            let Some(rel) = path.strip_prefix(root).ok().and_then(|r| r.to_str()) else {
                continue;
            };
            // Scope pre-filter (ADR-0032a R1): drop out-of-boundary paths
            // before they ever reach the consumer. R2's apply-time gate is
            // the authority; this is the cheap latency optimization.
            if !boundary.matches(rel) {
                continue;
            }
            match kind {
                EventKind::Create(_) => changeset.added.push(rel.to_string()),
                EventKind::Remove(_) => changeset.removed.push(rel.to_string()),
                _ => changeset.modified.push(rel.to_string()),
            }
        }
    }

    (changeset, ignore_changed)
}

impl Producer for FsWatcher {
    fn priority(&self) -> Priority {
        Priority::Fs
    }

    /// Start watching `root`, emitting into `sink`.
    fn start(&mut self, sink: Sender<Produced>) -> Result<()> {
        info!("Starting watcher for: {}", self.id);

        let root = self.root.clone();

        // ONE boundary for the watcher's lifetime, not one per debounced batch:
        // `FsSource` holds exactly one for the same reason — "its `.gitignore`
        // matcher cache is reused across every `walk()`/`in_scope()` call rather
        // than rebuilt per call" (lib.rs) — and the watcher is the
        // higher-frequency caller of the two (audit §G3, measured ~26 µs/batch).
        // Owned outright rather than `Arc`ed: the closure is the only holder.
        // The boundary's own doc-comment bounds its cache lifetime so it "can't
        // go stale"; the rebuild below is what keeps that true here.
        let mut boundary = SourceBoundary::new(root.clone());

        let mut debouncer: Debouncer<RecommendedWatcher, RecommendedCache> = new_debouncer(
            self.config.debounce,
            None, // No tick callback
            move |result: DebounceEventResult| {
                let events = match result {
                    Ok(events) => events,
                    Err(errors) => {
                        warn!("Watcher errors: {:?}", errors);
                        return;
                    }
                };

                // A `DebouncedEvent` wraps a `notify::Event` in `.event` (with `.kind`
                // and `.paths`). The watcher is a *signal* producer (ADR-0032a §1/§2):
                // classify by kind; the consumer re-reads + dedups from disk anyway.
                // The SAME source boundary the cold walk uses (ADR-0032a R1), so
                // the watcher never submits a gitignored/noise path — no more
                // hardcoded extension+substring filter that drifted from ADR-0022.
                let (mut changeset, ignore_changed) = classify_events(&events, &root, &boundary);

                if ignore_changed {
                    // R3: this batch re-scoped the tree, so the cached matcher is
                    // stale as of *this* batch — rebuild and re-classify rather
                    // than carry the pre-edit verdicts. Per-batch construction
                    // used to give this for free; doing it only here keeps that
                    // exact behaviour while paying for it only when an ignore
                    // file actually moved. Without the re-classify, a file that
                    // a `.gitignore` edit brings back INTO scope would keep
                    // being dropped by the pre-filter until the watcher restarted.
                    boundary = SourceBoundary::new(root.clone());
                    changeset = classify_events(&events, &root, &boundary).0;
                }

                // R3: fire the re-scoping reconcile FIRST (and independently of the
                // content changeset — a pure `.gitignore` edit has an empty changeset
                // and would otherwise be lost to the early return below). Reconcile is
                // authoritative for scope (umbrella §8): it re-derives walk∩boundary vs
                // the manifest and emits the precise add/remove set.
                if ignore_changed && sink.send(Produced::Reconcile { deep: false }).is_err() {
                    warn!("Failed to send re-scoping Reconcile from watcher");
                }

                if changeset.added.is_empty()
                    && changeset.modified.is_empty()
                    && changeset.removed.is_empty()
                {
                    return;
                }
                debug!(
                    "Watcher changeset: +{} ~{} -{}",
                    changeset.added.len(),
                    changeset.modified.len(),
                    changeset.removed.len()
                );
                if sink.send(Produced::Signal(changeset)).is_err() {
                    warn!("Failed to send Signal from watcher");
                }
            },
        )
        .map_err(|e| Error::Io(format!("failed to create debouncer: {}", e)))?;

        debouncer
            .watch(&self.root, RecursiveMode::Recursive)
            .map_err(|e| Error::Io(format!("failed to watch directory: {}", e)))?;

        self._debouncer = Some(debouncer);

        info!("Watcher started for: {}", self.id);
        Ok(())
    }

    fn stop(&mut self) {
        self._debouncer = None;
        info!("Watcher stopped for: {}", self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind, RemoveKind};
    use notify::Event;
    use std::fs;
    use std::time::Instant;
    use tempfile::TempDir;

    #[test]
    fn test_watcher_creation() {
        let temp = TempDir::new().unwrap();
        let config = WatcherConfig::default();

        let watcher = FsWatcher::new(temp.path().to_path_buf(), "myproj".into(), config);

        assert_eq!(watcher.id(), "myproj");
        assert_eq!(watcher.root(), temp.path());
    }

    // ---- classify_events (audit §G2/§K3) -----------------------------------
    //
    // These are the first tests of the watcher's classification policy. Until
    // the extraction above, the whole of it lived in a `move`/`'static` closure
    // inside `start`, reachable only by driving a real debouncer — so
    // `test_watcher_creation`'s two getters were the crate's ONLY watcher
    // coverage, and `daemon/tests/watcher_lifecycle.rs` covers start/stop, not
    // classification.

    /// One debounced event with `kind` over `paths` (absolute, as notify
    /// delivers them). A rename arrives as one event with two paths, which is
    /// why this takes a slice.
    fn ev(kind: EventKind, paths: &[PathBuf]) -> DebouncedEvent {
        let mut event = Event::new(kind);
        for p in paths {
            event = event.add_path(p.clone());
        }
        DebouncedEvent::new(event, Instant::now())
    }

    const CREATE: EventKind = EventKind::Create(CreateKind::File);
    const MODIFY: EventKind = EventKind::Modify(ModifyKind::Any);
    const REMOVE: EventKind = EventKind::Remove(RemoveKind::File);

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    /// The `EventKind` → add/modify/remove mapping, including the multi-path
    /// event shape a rename produces.
    #[test]
    fn classify_maps_event_kinds_to_added_modified_removed() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write(root, "src/a.rs", "fn a() {}");
        write(root, "src/b.rs", "fn b() {}");
        let boundary = SourceBoundary::new(root);

        // `src/gone.rs` is deliberately NOT on disk: a removal names a path
        // that no longer exists, and must still be classified.
        let events = vec![
            ev(CREATE, &[root.join("src/a.rs")]),
            ev(MODIFY, &[root.join("src/b.rs")]),
            ev(REMOVE, &[root.join("src/gone.rs")]),
            // A rename event carries both endpoints in one event.
            ev(MODIFY, &[root.join("src/a.rs"), root.join("src/b.rs")]),
        ];

        let (cs, ignore_changed) = classify_events(&events, root, &boundary);
        assert!(!ignore_changed);
        assert_eq!(cs.added, ["src/a.rs"]);
        assert_eq!(cs.modified, ["src/b.rs", "src/a.rs", "src/b.rs"]);
        assert_eq!(cs.removed, ["src/gone.rs"]);
    }

    /// Everything that is not `Create`/`Remove` — or a definite read, below — is
    /// a modification: `Any`, `Other`, an `Access` the backend cannot classify,
    /// and `Close(Write)` (inotify's `IN_CLOSE_WRITE`) included. A kind the
    /// watcher does not recognise must re-index the file, never silently drop it.
    #[test]
    fn classify_treats_any_unrecognised_kind_as_a_modification() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write(root, "src/a.rs", "fn a() {}");
        let boundary = SourceBoundary::new(root);

        for kind in [
            EventKind::Any,
            EventKind::Other,
            EventKind::Access(AccessKind::Any),
            EventKind::Access(AccessKind::Other),
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
        ] {
            let (cs, _) = classify_events(&[ev(kind, &[root.join("src/a.rs")])], root, &boundary);
            assert_eq!(cs.modified, ["src/a.rs"], "kind {kind:?}");
            assert!(
                cs.added.is_empty() && cs.removed.is_empty(),
                "kind {kind:?}"
            );
        }
    }

    /// R3: an ignore-file edit is a **re-scoping** trigger, not content. It sets
    /// the flag (which `start` turns into `Produced::Reconcile`) and never lands
    /// in the changeset — `.gitignore` is neither code nor in-scope, so every
    /// filter below would have dropped it silently.
    #[test]
    fn classify_flags_a_gitignore_edit_and_keeps_it_out_of_the_changeset() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write(root, ".gitignore", "vendor/\n");
        write(root, "src/nested/.gitignore", "*.gen.rs\n");
        let boundary = SourceBoundary::new(root);

        // Create, modify and remove all re-scope.
        for kind in [CREATE, MODIFY, REMOVE] {
            for rel in [".gitignore", "src/nested/.gitignore"] {
                let (cs, ignore_changed) =
                    classify_events(&[ev(kind, &[root.join(rel)])], root, &boundary);
                assert!(ignore_changed, "{rel} under {kind:?} must re-scope");
                assert!(
                    cs.added.is_empty() && cs.modified.is_empty() && cs.removed.is_empty(),
                    "{rel} must not appear as content: {cs:?}"
                );
            }
        }

        // A batch mixing an ignore edit with real content yields both.
        let (cs, ignore_changed) = classify_events(
            &[
                ev(MODIFY, &[root.join(".gitignore")]),
                ev(MODIFY, &[root.join("src/nested/keep.rs")]),
            ],
            root,
            &boundary,
        );
        assert!(ignore_changed);
        assert_eq!(cs.modified, ["src/nested/keep.rs"]);
    }

    /// A **read** is not a change — of a code file or of a `.gitignore`. notify's
    /// inotify backend reports every `open`, and the reconcile walk reads each
    /// `.gitignore` in the tree: when a read of one counted as an ignore-file
    /// edit, each reconcile triggered the next (every ~0.4 s on next.js, idle or
    /// not) and swallowed every real edit into a full-tree walk. Writing an ignore
    /// file still re-scopes, whichever event kind reports it.
    #[test]
    fn classify_ignores_reads_but_not_writes() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write(root, "src/a.rs", "fn a() {}");
        write(root, ".gitignore", "vendor/\n");
        let boundary = SourceBoundary::new(root);

        for kind in [
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            EventKind::Access(AccessKind::Open(AccessMode::Write)),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Access(AccessKind::Close(AccessMode::Execute)),
        ] {
            let (cs, ignore_changed) = classify_events(
                &[ev(kind, &[root.join("src/a.rs"), root.join(".gitignore")])],
                root,
                &boundary,
            );
            assert!(
                !ignore_changed,
                "reading .gitignore must not re-scope ({kind:?})"
            );
            assert!(
                cs.added.is_empty() && cs.modified.is_empty() && cs.removed.is_empty(),
                "a read is not a change ({kind:?}): {cs:?}"
            );
        }

        let closed_after_write = EventKind::Access(AccessKind::Close(AccessMode::Write));
        let (_, ignore_changed) = classify_events(
            &[ev(closed_after_write, &[root.join(".gitignore")])],
            root,
            &boundary,
        );
        assert!(
            ignore_changed,
            "a .gitignore closed after writing still re-scopes"
        );
    }

    /// The `is_indexable` pre-filter: code and project manifests get through,
    /// docs and lockfiles do not. `Cargo.toml` is the case a bare "is it code?"
    /// check got wrong — a manifest edit changes the workspace (ADR-0019).
    #[test]
    fn classify_drops_paths_that_are_not_indexable() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let boundary = SourceBoundary::new(root);

        let events: Vec<DebouncedEvent> = [
            "src/main.rs",
            "Cargo.toml",
            "web/package.json",
            "README.md",
            "Cargo.lock",
            "docs/diagram.png",
        ]
        .iter()
        .map(|rel| ev(MODIFY, &[root.join(rel)]))
        .collect();

        let (cs, _) = classify_events(&events, root, &boundary);
        assert_eq!(
            cs.modified,
            ["src/main.rs", "Cargo.toml", "web/package.json"],
            "code + manifests only"
        );
    }

    /// The boundary pre-filter (ADR-0032a R1): a `.gitignore`d path and a
    /// built-in noise dir are dropped before the consumer ever sees them.
    #[test]
    fn classify_drops_paths_outside_the_source_boundary() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write(root, ".gitignore", "vendor/\n");
        let boundary = SourceBoundary::new(root);

        let events: Vec<DebouncedEvent> = [
            "src/keep.rs",
            "vendor/ignored.rs",
            "target/debug/build.rs",
            "node_modules/pkg/index.js",
            ".filigrio/state.json",
        ]
        .iter()
        .map(|rel| ev(MODIFY, &[root.join(rel)]))
        .collect();

        let (cs, _) = classify_events(&events, root, &boundary);
        assert_eq!(
            cs.modified,
            ["src/keep.rs"],
            "gitignored + builtin-noise paths must not reach the consumer"
        );
    }

    /// A path outside `root` cannot be made root-relative, so it is dropped
    /// rather than submitted under some other project's name.
    #[test]
    fn classify_drops_paths_outside_the_watched_root() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("watched");
        let elsewhere = temp.path().join("elsewhere");
        write(&root, "src/keep.rs", "fn a() {}");
        write(&elsewhere, "src/other.rs", "fn b() {}");
        let boundary = SourceBoundary::new(&root);

        let (cs, _) = classify_events(
            &[
                ev(MODIFY, &[root.join("src/keep.rs")]),
                ev(MODIFY, &[elsewhere.join("src/other.rs")]),
            ],
            &root,
            &boundary,
        );
        assert_eq!(cs.modified, ["src/keep.rs"]);
    }

    /// Directories are skipped for live kinds but NOT for removals: a removed
    /// path is already gone from disk, so `is_dir()` answers `false` for it and
    /// the check would be meaningless — worse, applying it would depend on
    /// whether something else had recreated the path in the meantime.
    #[test]
    fn classify_skips_directories_for_live_kinds_but_not_for_removals() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        // A directory whose *name* passes `is_indexable` — the only case where
        // the `is_dir` check changes the answer.
        fs::create_dir_all(root.join("src/looks_like.rs")).unwrap();
        let boundary = SourceBoundary::new(root);
        let dir = root.join("src/looks_like.rs");

        let (cs, _) = classify_events(&[ev(CREATE, std::slice::from_ref(&dir))], root, &boundary);
        assert!(
            cs.added.is_empty(),
            "a live event on a directory is not a file change: {cs:?}"
        );
        let (cs, _) = classify_events(&[ev(MODIFY, std::slice::from_ref(&dir))], root, &boundary);
        assert!(cs.modified.is_empty(), "{cs:?}");

        let (cs, _) = classify_events(&[ev(REMOVE, &[dir])], root, &boundary);
        assert_eq!(
            cs.removed,
            ["src/looks_like.rs"],
            "a removal is classified without consulting the disk"
        );
    }

    /// The boundary is now built once and reused across batches (audit §G3), so
    /// pin what that reuse must not change: the same boundary gives the same
    /// verdicts on batch two as on batch one.
    #[test]
    fn classify_is_stable_across_batches_on_one_reused_boundary() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write(root, ".gitignore", "vendor/\n");
        write(root, "src/keep.rs", "fn a() {}");
        let boundary = SourceBoundary::new(root);

        let events = vec![
            ev(MODIFY, &[root.join("src/keep.rs")]),
            ev(MODIFY, &[root.join("vendor/ignored.rs")]),
        ];
        let first = classify_events(&events, root, &boundary);
        let second = classify_events(&events, root, &boundary);
        assert_eq!(first.0.modified, second.0.modified);
        assert_eq!(first.1, second.1);
        assert_eq!(first.0.modified, ["src/keep.rs"]);
    }

    /// A freshly built boundary sees a `.gitignore` edit that a cached one
    /// cannot — which is exactly why `start` rebuilds on the R3 flag instead of
    /// keeping the boundary for the watcher's whole life. Without the rebuild,
    /// a file brought back INTO scope stays invisible to the pre-filter.
    #[test]
    fn a_rebuilt_boundary_sees_a_rescoping_that_the_cached_one_does_not() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write(root, ".gitignore", "vendor/\n");
        write(root, "vendor/x.rs", "fn v() {}");
        let cached = SourceBoundary::new(root);
        let events = vec![ev(MODIFY, &[root.join("vendor/x.rs")])];

        assert!(
            classify_events(&events, root, &cached)
                .0
                .modified
                .is_empty(),
            "vendor/ is ignored at this point"
        );

        // The `.gitignore` stops ignoring vendor/ — the R3 re-scope.
        write(root, ".gitignore", "\n");
        assert!(
            classify_events(&events, root, &cached)
                .0
                .modified
                .is_empty(),
            "a cached boundary cannot see the edit — this is the staleness the \
             rebuild in `start` exists to prevent"
        );
        let rebuilt = SourceBoundary::new(root);
        assert_eq!(
            classify_events(&events, root, &rebuilt).0.modified,
            ["vendor/x.rs"]
        );
    }
}
