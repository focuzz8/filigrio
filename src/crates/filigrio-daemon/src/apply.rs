//! The apply critical section — the daemon's **host wiring** around the freshness
//! engine (ADR-0032 §2 drain-level parallelism). It holds **no freshness logic**:
//! gating, mutation, reconcile, and the shrink-guard all live in `filigrio-pipeline`.
//! This module's whole job is to drive [`Pipeline`] with the daemon's two host
//! concerns — the **per-project lock** (serialize same-project applies, no lost
//! update) and the **resident state** ([`Flusher`]: supply `prior`, take back
//! `fresh`) — neither of which is the global `Daemon` mutex, so different
//! projects' applies run concurrently on the worker pool.
//!
//! Each apply is: `lock → state_of → Pipeline::apply_signal|reconcile_and_apply
//! → record_apply`. The `Daemon` methods, the `--no-daemon` inline path, and the
//! pooled [`ApplyJob`] all route through the same functions, so they can never
//! diverge.
//!
//! **ADR-0042 F4 changed what the middle step writes into.** The pipeline used
//! to be handed an [`FsStore`], so `Pipeline::apply` read `state.json`, merged,
//! and wrote it — and the daemon then *re-read* `state.json` to refresh its
//! cache. Three full traversals of a 210 MB file (at next.js scale) per apply,
//! for a state the daemon already held in memory. It is now handed a
//! [`DeferredStore`], which merges onto the resident `prior` and hands the
//! result straight back; the [`Flusher`] decides whether that result is written
//! now (a client is waiting) or later (the producer lane — B12 write-behind).
//! The pipeline is unchanged and still policy-free: cadence is a daemon concern.

use crate::flush::{Flusher, Persistence};
use crate::project::Project;
use crate::{Error, Result};
use filigrio_core::{ChangeSet, GraphState};
use filigrio_index::DispatchExtractor;
use filigrio_ingest::FsSource;
use filigrio_pipeline::{ClusterConfig, ClusterTiming, LinkScope, Pipeline};
use filigrio_store::DeferredStore;
use std::sync::Arc;
use tracing::{info, warn};

/// What an [`ApplyJob`] does when it runs (on the producer-lane worker pool, or
/// synchronously at command ingress — ADR-0042 F6c).
///
/// The split is the R2.5 separation of **detection** from **mutation**: a raw
/// producer signal must be gated (it's a hint); a detector's diff is already
/// authoritative and must NOT be re-gated. This is the daemon-side mirror of
/// [`filigrio_ingest::Produced`] (ADR-0032e) — [`op_of`] is the single total map
/// from one to the other. Reconcile **depth lives here, not on the wire**
/// (ADR-0042 F6b): the watcher's fast-path rides `Reconcile { deep: false }`
/// straight into the daemon-internal queue; a wire `ProjectIndex` converts to
/// `Reconcile { deep: true }` at ingress.
#[derive(Clone, Debug)]
pub enum Op {
    /// Apply a raw producer signal (watcher/git). Gated — scope + dedup — before
    /// the mutation (ADR-0032a R2).
    Apply(ChangeSet),
    /// Apply a pre-computed **authoritative** changeset as-is, never re-gated
    /// (ADR-0032e §2/R2.5) — the case a git diff needs (`Produced::Authoritative`).
    ApplyExact(ChangeSet),
    /// Reconcile the project against disk, then apply the resulting **authoritative**
    /// changeset. Detection (the walk) and mutation both run here — off the daemon
    /// lock, under the per-project lock — and the diff is NOT re-gated: it is already
    /// scoped+deduped, and re-running the signal gate would drop a deep reconcile's
    /// mtime-preserved drift, the exact case deep exists for (ADR-0032a R2.5).
    Reconcile { deep: bool },
    /// Run the clustering the project's watcher-lane applies deferred
    /// ([`ClusterTiming::Deferred`]) and swap the result into resident state.
    /// Queued by the run loop only once the project is quiet — no queued,
    /// deferred or running apply — so a burst of saves costs one recluster.
    Recluster,
}

/// The single total `Produced → Op` match (ADR-0032e §2) — the R2.5 authority
/// axis enforced in exactly one place. Every producer's output — the watcher, a
/// startup/watch-on reconcile trigger, or a future git-diff producer — passes
/// through here on its way to an [`ApplyJob`].
pub(crate) fn op_of(produced: filigrio_ingest::Produced) -> Op {
    match produced {
        filigrio_ingest::Produced::Signal(cs) => Op::Apply(cs),
        filigrio_ingest::Produced::Authoritative(cs) => Op::ApplyExact(cs),
        filigrio_ingest::Produced::Reconcile { deep } => Op::Reconcile { deep },
    }
}

/// What one apply actually did, in the two numbers a client is owed: how many
/// files were applied, and how many of them had **vanished** by the time the
/// engine read them and were converged into removals rather than aborting the
/// batch (ADR-0042 F8). Kept as a struct rather than a bare `usize` so the
/// vanished count cannot be dropped on the way to the outcome — "silently
/// converged" must not become the new silent failure (ADR-0029).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub changed: usize,
    pub vanished: usize,
}

impl From<&filigrio_pipeline::BuildReport> for Applied {
    fn from(r: &filigrio_pipeline::BuildReport) -> Self {
        Applied {
            changed: r.changed,
            vanished: r.vanished,
        }
    }
}

/// A self-contained unit of apply work, dispatchable to a worker thread.
///
/// Holds *clones* of everything the work touches — the `Project`, the shared cache
/// handle, and the shared per-project locks — so it is `Send + 'static` and carries
/// no borrow of the daemon.
pub(crate) struct ApplyJob {
    pub project: Project,
    pub op: Op,
    /// Clustering config for this apply (ADR-0024), from `DaemonConfig` — the
    /// daemon must not silently degrade a Full-clustered project to Simple.
    pub cluster_cfg: ClusterConfig,
    /// Which lane this job came from, hence when its result reaches disk
    /// (ADR-0042 F4/B12). A wire command is [`Persistence::Flush`]; the
    /// producer lane is [`Persistence::Defer`]. Carried on the job rather than
    /// inferred downstream so the lane is visible at the point it is chosen.
    pub persistence: Persistence,
    pub flusher: Arc<Flusher>,
}

impl ApplyJob {
    /// Run the job (blocking, CPU-bound). Each variant acquires the per-project
    /// lock so same-project work serializes even if both are dispatched.
    /// Returns [`Applied`] — the changed-file count (`0` = no effective change)
    /// plus the F8 vanished count — so a synchronous command can carry the
    /// outcome back to its client (ADR-0042 F6c).
    pub fn run(&self) -> Result<Applied> {
        match &self.op {
            Op::Apply(changeset) => apply_core(
                &self.project,
                changeset,
                self.cluster_cfg,
                &self.flusher,
                self.persistence,
            ),
            Op::ApplyExact(changeset) => apply_exact(
                &self.project,
                changeset,
                self.cluster_cfg,
                &self.flusher,
                self.persistence,
            ),
            Op::Reconcile { deep } => reconcile_and_apply(
                &self.project,
                *deep,
                self.cluster_cfg,
                &self.flusher,
                self.persistence,
            ),
            Op::Recluster => recluster(&self.project, self.cluster_cfg, &self.flusher),
        }
    }

    /// Whether this job, having applied `changed` files, leaves the project
    /// owing a recluster: a watcher-lane apply ran with clustering deferred.
    pub fn owes_recluster(&self, applied: &Applied) -> bool {
        applied.changed > 0
            && self.persistence == Persistence::Defer
            && !matches!(self.op, Op::Recluster)
    }
}

/// The host wiring shared by every apply variant: acquire the per-project lock
/// (serialize same-project applies, no lost update — never a global lock, so a
/// heavy monorepo apply cannot block a small repo's apply on another worker),
/// supply `prior` from the resident state, build the source/extractor/store/
/// pipeline triple, run `mutate`, and hand whatever the pipeline produced to the
/// [`Flusher`] on the caller's cadence (ADR-0042 F4/B12).
///
/// Two things this deliberately no longer does:
///
/// - **It does not re-read `state.json` afterwards.** The old code refreshed the
///   cache with `store.load_state()` — a full parse of the file the same apply
///   had just written. Under write-behind that would be worse than wasteful: it
///   would be *wrong*, because disk no longer holds the fresh state, so the
///   refresh would resurrect a stale one. [`DeferredStore::take`] returns the
///   merged state directly.
/// - **It does not infer "did anything change" from a report count.** `take()`
///   is `Some` exactly when the pipeline applied a delta, which is the same
///   question asked at the source instead of reconstructed from telemetry.
fn with_project_apply<T>(
    project: &Project,
    cluster_cfg: ClusterConfig,
    flusher: &Arc<Flusher>,
    persistence: Persistence,
    mutate: impl FnOnce(&Pipeline, &GraphState) -> Result<T>,
) -> Result<T> {
    let lock = flusher.lock_for(&project.id)?;
    let _guard = lock.lock();

    let prior = flusher.state_of(project)?;
    let source = FsSource::new(&project.root);
    let extractor = DispatchExtractor::with_defaults();
    // The shrink-guard authority is `Pipeline`'s (ADR-0032 §5) — as it already
    // was, since the daemon built its `FsStore` `with_force(true)` to keep the
    // store's own guard out of the way.
    let store = DeferredStore::new(prior.as_ref());
    let pipeline = Pipeline::new(&source, &extractor, &store)
        .with_cluster_config(cluster_cfg)
        .with_shrink_guard(true)
        .with_link_scope(link_scope())
        // The watcher lane defers clustering (the run loop reclusters once the
        // burst is over); a client is waiting on every other lane, and a
        // `project index` must answer with communities, so those cluster inline.
        .with_cluster_timing(match persistence {
            Persistence::Defer => ClusterTiming::Deferred,
            Persistence::Flush => ClusterTiming::Inline,
        });

    let result = mutate(&pipeline, prior.as_ref())?;
    if let Some(fresh) = store.take() {
        flusher.record_apply(project, fresh, persistence)?;
    }
    Ok(result)
}

/// The daemon's re-resolution strategy (ADR-0042 Phase 1.3, B10): **`Scoped`**
/// since 2026-09-10 — every daemon apply (watcher, git hook, `project index`,
/// startup reconcile, `--no-daemon`) re-links only the change's impact set.
/// Flipped after Validation 1 passed on both reference corpora: `just shadow`
/// (next.js 22,089 files and this workspace, 16 edits each, zero divergence) and
/// `just convergence` (40 real next.js commits and 30 of this repo's, Global ≡
/// Scoped at every step, both chains ≡ cold).
///
/// `FILIGRIO_LINK_SCOPE=global` in the daemon's environment is the rollback —
/// no rebuild, and the flag is inherited by an auto-started daemon. Read per
/// apply, so it costs one env lookup and needs no plumbing through the 14 call
/// sites that build an apply.
pub(crate) fn link_scope() -> LinkScope {
    link_scope_from(std::env::var("FILIGRIO_LINK_SCOPE").ok().as_deref())
}

/// [`link_scope`]'s mapping, lifted out of the env read so it is testable
/// without `set_var` (process-global, and it would race every other test).
fn link_scope_from(value: Option<&str>) -> LinkScope {
    match value {
        None => LinkScope::Scoped,
        Some(v) if v.eq_ignore_ascii_case("scoped") => LinkScope::Scoped,
        Some(v) if v.eq_ignore_ascii_case("global") => LinkScope::Global,
        Some(other) => {
            warn!("FILIGRIO_LINK_SCOPE={other:?} is not `global` or `scoped`; using scoped");
            LinkScope::Scoped
        }
    }
}

/// Recluster one project ([`Op::Recluster`]): the warm-started clustering its
/// deferred applies skipped, over the resident state, swapped in on the
/// producer-lane cadence (write-behind — nobody is waiting on communities).
/// Under the per-project lock like every apply, so it never interleaves with
/// one; a no-op when the partition comes out unchanged.
pub(crate) fn recluster(
    project: &Project,
    cluster_cfg: ClusterConfig,
    flusher: &Arc<Flusher>,
) -> Result<Applied> {
    let lock = flusher.lock_for(&project.id)?;
    let _guard = lock.lock();
    let prior = flusher.state_of(project)?;
    let partition = filigrio_resolve::Engine::recluster(prior.as_ref(), &cluster_cfg)
        .map_err(|e| Error::Other(format!("recluster failed for {}: {e}", project.id)))?;
    if partition == prior.partition {
        return Ok(Applied::default());
    }
    let communities = partition.communities.len();
    let mut fresh = prior.as_ref().clone();
    fresh.partition = partition;
    flusher.record_apply(project, fresh, Persistence::Defer)?;
    info!("Reclustered {}: {communities} communities", project.id);
    Ok(Applied::default())
}

/// Apply a **producer signal** to one project (watcher/git). Gates the raw
/// changeset (scope + dedup → effective), then mutates. The single signal-apply
/// path — `Daemon::apply_project`, the `--no-daemon` drain, and an `Op::Apply`
/// pool job all route through it.
pub(crate) fn apply_core(
    project: &Project,
    changeset: &ChangeSet,
    cluster_cfg: ClusterConfig,
    flusher: &Arc<Flusher>,
    persistence: Persistence,
) -> Result<Applied> {
    let report = with_project_apply(
        project,
        cluster_cfg,
        flusher,
        persistence,
        |pipeline, prior| {
            pipeline
                .apply_signal(prior, changeset)
                .map_err(|e| Error::Other(format!("apply failed for {}: {}", project.id, e)))
        },
    )?;
    if report.changed == 0 {
        info!(
            "apply to {}: no effective change after scope+dedup gate; skipped",
            project.id
        );
    } else {
        info!(
            "Applied to {}: changed={}, vanished={}, nodes_added={}, nodes_removed={}, edges_added={}, edges_removed={}, affected={}",
            project.id, report.changed, report.vanished, report.nodes_added, report.nodes_removed, report.edges_added, report.edges_removed, report.affected
        );
    }
    warn_vanished(project, report.vanished);
    Ok((&report).into())
}

/// Apply a **pre-computed authoritative changeset** (ADR-0032e `Produced::Authoritative`,
/// e.g. a future git diff) to one project — applied **as-is, no signal gate**
/// (R2.5: it is already computed truth, not a hint). Mirrors `apply_core`'s host
/// wiring via `with_project_apply`; the only difference is `Pipeline::apply`
/// directly instead of `Pipeline::apply_signal`'s gate.
pub(crate) fn apply_exact(
    project: &Project,
    changeset: &ChangeSet,
    cluster_cfg: ClusterConfig,
    flusher: &Arc<Flusher>,
    persistence: Persistence,
) -> Result<Applied> {
    let report = with_project_apply(
        project,
        cluster_cfg,
        flusher,
        persistence,
        |pipeline, prior| {
            pipeline
                .apply(prior, changeset)
                .map_err(|e| Error::Other(format!("apply-exact failed for {}: {}", project.id, e)))
        },
    )?;
    if report.changed == 0 {
        info!("apply-exact to {}: empty changeset; skipped", project.id);
    } else {
        info!(
            "Applied (exact) to {}: changed={}, vanished={}, nodes_added={}, nodes_removed={}, edges_added={}, edges_removed={}, affected={}",
            project.id, report.changed, report.vanished, report.nodes_added, report.nodes_removed, report.edges_added, report.edges_removed, report.affected
        );
    }
    warn_vanished(project, report.vanished);
    Ok((&report).into())
}

/// Reconcile a project against disk and apply the drift **authoritatively**
/// (ADR-0032a R2.5). Detection (the walk) and mutation run under one per-project
/// lock hold, off the daemon lock. The reconcile changeset is applied as-is — no
/// signal gate — so a deep reconcile's mtime-preserved drift is not dropped by the
/// shallow dedup. Used by `Op::Reconcile` (producer-lane pool jobs and the sync
/// command path), `run_command_cold`'s `ProjectIndex` (`--no-daemon`), and
/// `startup_reconcile`.
pub(crate) fn reconcile_and_apply(
    project: &Project,
    deep: bool,
    cluster_cfg: ClusterConfig,
    flusher: &Arc<Flusher>,
    persistence: Persistence,
) -> Result<Applied> {
    let (report, build) = with_project_apply(
        project,
        cluster_cfg,
        flusher,
        persistence,
        |pipeline, prior| {
            pipeline.reconcile_and_apply(prior, deep).map_err(|e| {
                Error::Reconcile(format!("reconcile failed for {}: {}", project.id, e))
            })
        },
    )?;
    info!(
        "Reconcile {}: checked={}, added={}, changed={}, removed={}, drift={}",
        project.id,
        report.files_checked,
        report.files_added,
        report.files_changed,
        report.files_removed,
        report.has_drift
    );
    warn_vanished(project, build.vanished);
    Ok((&build).into())
}

/// Log the F8 convergence. A vanished file is not an error, but it *is* a fact
/// the operator is owed even when the count never reaches a client (a watcher-lane
/// apply has no caller to answer) — ADR-0029 honesty.
fn warn_vanished(project: &Project, vanished: usize) {
    if vanished > 0 {
        warn!(
            "{}: {vanished} file(s) had vanished by read time; converged as removals (ADR-0042 F8)",
            project.id
        );
    }
}

/// The single "reserved flag" rejection both execution paths share (ADR-0042
/// F5): `--clean` on `project index` is wipe-and-reindex, which is not built —
/// it must fail fast with an explicit error, never be silently ignored
/// (ADR-0029 honesty applied to flags). Both execution paths return it to the
/// caller as a typed error and write nothing (ADR-0042 F6c: the resident path
/// used to only *log* it behind an ack).
///
/// The flag is `--clean`, not `--force` (ADR-0042 F7): `--force` already means
/// "override the safety check" on `project register`, so reusing it here would
/// overload one word with two semantics.
pub(crate) fn clean_reindex_unsupported() -> Error {
    Error::Other(
        "`--clean` (reindex from scratch: wipe and rebuild) is not implemented yet — \
         run `project index` without `--clean` for an incremental index"
            .to_string(),
    )
}

/// Execute a write `Command` synchronously with no resident host — the
/// `--no-daemon` one-shot write path (ADR-0032f §4/§5: "a `--no-daemon` write
/// is synchronous — the one-shot exits when done"). Routes through the exact
/// same `apply_core`/`reconcile_and_apply` the resident daemon's sync command
/// path calls; the [`Flusher`] is a throwaway, single-project instance the
/// one-shot binary's `cache`/`locks` back, not the resident LRU cache/lock
/// table. Returns the typed [`CommandOutcome`] (ADR-0042 F6c).
///
/// **Cadence: always [`Persistence::Flush`]** (ADR-0042 B12 "one-shot / CI:
/// flush at command completion"). The process is about to exit, so completion
/// *is* the flush point — write-behind here would mean writing nothing at all.
///
/// `ProjectRegister`/`ProjectRemove`/`DaemonStop` are rejected rather than silently
/// no-op'd: a one-shot process has no persistent registry to add/remove a
/// project from, and no separate resident process for `DaemonStop` to target.
/// `ProjectWatch` is rejected too: a watcher's whole point is a resident
/// process following events.
pub fn run_command_cold(
    project: &Project,
    command: crate::Command,
    cluster_cfg: ClusterConfig,
    cache: &Arc<parking_lot::Mutex<crate::cache::ProjectStateCache>>,
    locks: &crate::locks::ProjectLocks,
) -> Result<filigrio_protocol::CommandOutcome> {
    use crate::Command;
    use filigrio_protocol::CommandOutcome;
    let flusher = Arc::new(Flusher::new(
        cache.clone(),
        locks.clone(),
        crate::flush::FlushConfig::default(),
        Arc::new(crate::flush::SystemClock),
    ));
    match command {
        Command::Submit { changeset, .. } => {
            let applied = apply_core(
                project,
                &changeset,
                cluster_cfg,
                &flusher,
                Persistence::Flush,
            )?;
            Ok(CommandOutcome::Applied {
                project: project.id.clone(),
                changed: applied.changed,
                vanished: applied.vanished,
            })
        }
        // Mirrors the resident sync path's `ProjectIndex` arm — always the FULL
        // reconcile (depth is Op-internal, ADR-0042 F6b; the shallow fast-path
        // is the watcher's, and the watcher never reaches this cold path).
        // `clean` (wipe-and-reindex) is reserved and rejected before any work —
        // same helper as the resident path (ADR-0042 F5).
        Command::ProjectIndex { clean, .. } => {
            if clean {
                return Err(clean_reindex_unsupported());
            }
            let applied =
                reconcile_and_apply(project, true, cluster_cfg, &flusher, Persistence::Flush)?;
            Ok(CommandOutcome::Indexed {
                project: project.id.clone(),
                changed: applied.changed,
                vanished: applied.vanished,
            })
        }
        // ADR-0042 F4 — the explicit flush verb on the cold path. A one-shot
        // writes through, so there is never anything outstanding: report
        // `wrote: false` rather than rewriting the checkpoint for nothing.
        Command::ProjectFlush { .. } => Ok(CommandOutcome::Flushed {
            project: project.id.clone(),
            wrote: false,
        }),
        // The explicit `graph.json` export (ADR-0042 F2): the apply path no
        // longer snapshots, so this verb is the interchange file's only
        // producer. `snapshot()` reads `state.json`, so under write-behind the
        // project is flushed first (F4) — on this cold path that is always a
        // no-op, but routing both paths through the same rule is what keeps them
        // from diverging. Per-project lock: never read a half-applied store.
        Command::ProjectExport { .. } => {
            use filigrio_core::GraphStore;
            let lock = locks.lock_for(&project.id)?;
            let _guard = lock.lock();
            flusher.flush_locked(&project.id)?;
            filigrio_store::FsStore::new(&project.output_dir)
                .snapshot()
                .map_err(|e| Error::Other(format!("export failed for {}: {}", project.id, e)))?;
            Ok(CommandOutcome::Exported {
                project: project.id.clone(),
                path: project.output_dir.join("graph.json").display().to_string(),
            })
        }
        // Watch is a resident-daemon mode by definition (ADR-0042 F6b): a
        // one-shot process exits when done, so there is nothing to follow
        // events with. Clear error, never a silent no-op.
        Command::ProjectWatch { .. } => Err(Error::Other(
            "watch requires the resident daemon (`filigrio server serve`)".to_string(),
        )),
        Command::ProjectRegister { .. } | Command::ProjectRemove { .. } | Command::DaemonStop => {
            Err(Error::Other(
                "this command needs the resident daemon's registry/lifecycle — start the daemon, or drop --no-daemon".to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::ProjectStateCache;
    use crate::flush::{FlushConfig, SystemClock};
    use crate::locks::ProjectLocks;
    use filigrio_core::GraphStore;
    use filigrio_store::FsStore;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// The daemon links `Scoped` unless told otherwise, and `global` — any case —
    /// is the rollback. An unrecognised value keeps the default (and warns).
    #[test]
    fn link_scope_defaults_to_scoped_and_global_rolls_back() {
        assert_eq!(link_scope_from(None), LinkScope::Scoped);
        assert_eq!(link_scope_from(Some("scoped")), LinkScope::Scoped);
        assert_eq!(link_scope_from(Some("global")), LinkScope::Global);
        assert_eq!(link_scope_from(Some("GLOBAL")), LinkScope::Global);
        assert_eq!(link_scope_from(Some("globl")), LinkScope::Scoped);
    }

    /// A throwaway flusher over a fresh cache/lock table — what a single-project
    /// unit test needs in place of the daemon's resident one.
    fn flusher(capacity: usize) -> Arc<Flusher> {
        Arc::new(Flusher::new(
            Arc::new(Mutex::new(ProjectStateCache::new(capacity))),
            ProjectLocks::new(),
            FlushConfig::default(),
            Arc::new(SystemClock),
        ))
    }

    fn cs(added: &[&str], modified: &[&str]) -> ChangeSet {
        ChangeSet {
            added: added.iter().map(|s| s.to_string()).collect(),
            modified: modified.iter().map(|s| s.to_string()).collect(),
            removed: vec![],
        }
    }

    fn node_count(project: &Project) -> usize {
        FsStore::new(&project.output_dir)
            .load_state()
            .unwrap()
            .unwrap_or_default()
            .graph
            .nodes
            .len()
    }

    /// ADR-0032e / R2.5 — `apply_exact` (the daemon side of `op_of(Produced::Authoritative)`,
    /// what a git-diff producer will feed) applies a pre-computed changeset **as-is, without
    /// the signal gate**. Two things it must do, neither exercised elsewhere: (1) actually
    /// index — proving `op_of → Op::ApplyExact → apply_exact → pipeline.apply → persist` is
    /// live, not compile-only; (2) NOT dedup-drop an unchanged re-submit the way `apply_core`
    /// (gated) does — the R2.5 "authoritative is never re-gated" invariant. Guards against a
    /// refactor swapping `Pipeline::apply` for `apply_signal`, which would silently re-gate.
    #[test]
    fn apply_exact_applies_as_is_without_the_gate() {
        let scratch = TempDir::new().unwrap();
        let root = scratch.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();

        let project = Project::new(root.to_path_buf());
        std::fs::create_dir_all(&project.output_dir).unwrap();
        // `Persistence::Flush` (the client lane) keeps this test's observable —
        // `state.json`'s mtime — exactly what it was before ADR-0042 F4. What it
        // pins is the *gate*, not the cadence; the cadence has its own suite
        // (`tests/write_behind.rs`).
        let flusher = flusher(4);

        // (1) Authoritative index of a new file.
        apply_exact(
            &project,
            &cs(&["src/lib.rs"], &[]),
            ClusterConfig::default(),
            &flusher,
            Persistence::Flush,
        )
        .unwrap();
        assert!(
            node_count(&project) > 0,
            "authoritative apply must index the file"
        );

        // (2) Re-apply the SAME unchanged file. `apply_core` would dedup-drop it (no-op,
        // state.json untouched); `apply_exact` must re-run — state.json is rewritten as proof.
        let state_json = project.output_dir.join("state.json");
        let before = std::fs::metadata(&state_json).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        apply_exact(
            &project,
            &cs(&[], &["src/lib.rs"]),
            ClusterConfig::default(),
            &flusher,
            Persistence::Flush,
        )
        .unwrap();
        let after = std::fs::metadata(&state_json).unwrap().modified().unwrap();
        assert!(
            after > before,
            "apply_exact must NOT dedup-skip an unchanged re-submit (no gate)"
        );
    }

    /// ADR-0032f §4/§5 — the `--no-daemon` write path actually runs the write,
    /// synchronously, rather than the responder's blanket "commands must go
    /// through the daemon" error. `ProjectIndex` is the case `filigrio project
    /// index --no-daemon` relies on (the MCP bridge used to reach it through
    /// the control sliver; since ADR-0042 F9 it has no mutation surface at all,
    /// so the CLI is the only caller).
    #[test]
    fn run_command_cold_project_index_indexes_a_new_file() {
        let scratch = TempDir::new().unwrap();
        let root = scratch.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();

        let project = Project::new(root.to_path_buf());
        std::fs::create_dir_all(&project.output_dir).unwrap();
        let cache = Arc::new(Mutex::new(ProjectStateCache::new(4)));
        let locks = ProjectLocks::new();

        let outcome = run_command_cold(
            &project,
            crate::Command::ProjectIndex {
                project: project.id.clone(),
                clean: false,
            },
            ClusterConfig::default(),
            &cache,
            &locks,
        )
        .unwrap();

        assert!(
            node_count(&project) > 0,
            "one-shot ProjectIndex must actually index the tree"
        );
        // F6c: the outcome IS the result — the changed count must reflect the work.
        match outcome {
            filigrio_protocol::CommandOutcome::Indexed { changed, .. } => {
                assert!(
                    changed > 0,
                    "a cold index of a new file must report changed > 0"
                )
            }
            other => panic!("expected Indexed outcome, got {other:?}"),
        }
    }

    /// ADR-0042 F6c — watch is a resident-daemon mode; the cold one-shot must
    /// reject it with a clear error, never silently no-op.
    #[test]
    fn run_command_cold_rejects_project_watch() {
        let scratch = TempDir::new().unwrap();
        let project = Project::new(scratch.path().to_path_buf());
        let cache = Arc::new(Mutex::new(ProjectStateCache::new(1)));
        let locks = ProjectLocks::new();

        for on in [true, false] {
            let err = run_command_cold(
                &project,
                crate::Command::ProjectWatch {
                    project: project.id.clone(),
                    on,
                },
                ClusterConfig::default(),
                &cache,
                &locks,
            )
            .unwrap_err();
            assert_eq!(
                err.to_string(),
                "other: watch requires the resident daemon (`filigrio server serve`)",
                "watch (on={on}) must error clearly on the cold path"
            );
        }
    }

    /// ADR-0042 F6b — the R2.5 depth policy, pinned at the `Op` level now that
    /// `deep` is off the wire: a **shallow** reconcile trusts mtimes and misses
    /// an mtime-preserved content drift; a **deep** reconcile re-hashes,
    /// detects it, and applies it authoritatively (no re-gate). This is the
    /// internal half of the old wire-level `apply_gate` test.
    #[test]
    fn shallow_reconcile_trusts_mtime_deep_rehashes_and_applies() {
        let scratch = TempDir::new().unwrap();
        let root = scratch.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let src = root.join("src/lib.rs");
        std::fs::write(&src, "pub fn a() -> u32 { 1 }\n").unwrap();

        let project = Project::new(root.to_path_buf());
        std::fs::create_dir_all(&project.output_dir).unwrap();
        // Write-through, so `node_count` (which reads `state.json`) observes
        // each apply — the property under test is depth, not cadence.
        let flusher = flusher(4);

        // Index C0 (deep reconcile on an empty store = the cold build).
        reconcile_and_apply(
            &project,
            true,
            ClusterConfig::default(),
            &flusher,
            Persistence::Flush,
        )
        .unwrap();
        let base = node_count(&project);
        assert!(base > 0, "precondition: C0 indexed");
        let mtime0 = std::fs::metadata(&src).unwrap().modified().unwrap();

        // Content drift (more code → more nodes) whose mtime is reset to match
        // the manifest — the case the mtime fast-path cannot see.
        std::fs::write(&src, "pub fn a() -> u32 { 1 }\npub fn b() -> u32 { 2 }\n").unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
        f.set_modified(mtime0).unwrap();
        drop(f);

        // Shallow trusts mtime → misses the drift (confirms it IS mtime-invisible).
        let changed = reconcile_and_apply(
            &project,
            false,
            ClusterConfig::default(),
            &flusher,
            Persistence::Flush,
        )
        .unwrap();
        assert_eq!(
            changed.changed, 0,
            "shallow reconcile trusts mtime and must miss the drift"
        );
        assert_eq!(node_count(&project), base);

        // Deep re-hashes, detects, and applies — authoritatively, no re-gate.
        let changed = reconcile_and_apply(
            &project,
            true,
            ClusterConfig::default(),
            &flusher,
            Persistence::Flush,
        )
        .unwrap();
        assert!(
            changed.changed > 0,
            "deep reconcile must APPLY the mtime-preserved drift"
        );
        assert!(node_count(&project) > base);
    }

    /// ADR-0042 F5/F7 — `--clean` (reindex from scratch) is reserved but not
    /// implemented: the cold one-shot must fail fast with an explicit error
    /// naming the flag, and write **nothing** — a flag is never silently
    /// ignored (ADR-0029 honesty applied to flags). The error must name
    /// `--clean`, the flag the CLI actually offers since F7.
    #[test]
    fn run_command_cold_rejects_clean_reindex_and_writes_nothing() {
        let scratch = TempDir::new().unwrap();
        let root = scratch.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();

        let project = Project::new(root.to_path_buf());
        std::fs::create_dir_all(&project.output_dir).unwrap();
        let cache = Arc::new(Mutex::new(ProjectStateCache::new(4)));
        let locks = ProjectLocks::new();

        let err = run_command_cold(
            &project,
            crate::Command::ProjectIndex {
                project: project.id.clone(),
                clean: true,
            },
            ClusterConfig::default(),
            &cache,
            &locks,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("not implemented yet"),
            "the error must say the flag is not implemented yet: {msg}"
        );
        assert!(
            msg.contains("--clean"),
            "the error must name the flag: {msg}"
        );
        assert!(
            !msg.contains("--force"),
            "the error must not name the pre-F7 flag: {msg}"
        );
        assert!(
            !project.output_dir.join("state.json").exists(),
            "a rejected --clean must write nothing"
        );
    }

    /// ADR-0042 Phase 1c F2 — the apply path no longer snapshots, so `graph.json`
    /// only exists after the explicit export verb, and its bytes are exactly
    /// `graphjson::export` of the persisted state.
    #[test]
    fn run_command_cold_export_writes_graph_json_on_demand() {
        let scratch = TempDir::new().unwrap();
        let root = scratch.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();

        let project = Project::new(root.to_path_buf());
        std::fs::create_dir_all(&project.output_dir).unwrap();
        let cache = Arc::new(Mutex::new(ProjectStateCache::new(4)));
        let locks = ProjectLocks::new();

        run_command_cold(
            &project,
            crate::Command::ProjectIndex {
                project: project.id.clone(),
                clean: false,
            },
            ClusterConfig::default(),
            &cache,
            &locks,
        )
        .unwrap();
        let graph_json = project.output_dir.join("graph.json");
        assert!(
            !graph_json.exists(),
            "an apply must not produce graph.json (export is explicit, ADR-0042 F2)"
        );

        let outcome = run_command_cold(
            &project,
            crate::Command::ProjectExport {
                project: project.id.clone(),
            },
            ClusterConfig::default(),
            &cache,
            &locks,
        )
        .unwrap();
        // F6c: the export outcome names the written path.
        match &outcome {
            filigrio_protocol::CommandOutcome::Exported { path, .. } => {
                assert_eq!(path, &graph_json.display().to_string())
            }
            other => panic!("expected Exported outcome, got {other:?}"),
        }
        let bytes = std::fs::read(&graph_json).expect("export writes graph.json");
        let state = FsStore::new(&project.output_dir)
            .load_state()
            .unwrap()
            .unwrap_or_default();
        let expected =
            serde_json::to_vec_pretty(&filigrio_store::graphjson::export(&state)).unwrap();
        assert_eq!(
            bytes, expected,
            "export is exactly graphjson::export(state)"
        );
    }

    /// ADR-0042 F4 — the cold one-shot writes through (B12: "completion *is*
    /// the flush point"), so `project flush` there is an honest no-op that
    /// reports `wrote: false` — never an error, and never a pointless rewrite of
    /// a checkpoint that already matches.
    #[test]
    fn run_command_cold_flush_is_a_no_op_because_the_one_shot_writes_through() {
        let scratch = TempDir::new().unwrap();
        let root = scratch.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();

        let project = Project::new(root.to_path_buf());
        std::fs::create_dir_all(&project.output_dir).unwrap();
        let cache = Arc::new(Mutex::new(ProjectStateCache::new(4)));
        let locks = ProjectLocks::new();

        run_command_cold(
            &project,
            crate::Command::ProjectIndex {
                project: project.id.clone(),
                clean: false,
            },
            ClusterConfig::default(),
            &cache,
            &locks,
        )
        .unwrap();
        let state_json = project.output_dir.join("state.json");
        assert!(state_json.exists(), "the one-shot index persisted");
        let before = std::fs::metadata(&state_json).unwrap().modified().unwrap();

        let outcome = run_command_cold(
            &project,
            crate::Command::ProjectFlush {
                project: project.id.clone(),
            },
            ClusterConfig::default(),
            &cache,
            &locks,
        )
        .unwrap();
        match outcome {
            filigrio_protocol::CommandOutcome::Flushed { wrote, .. } => {
                assert!(!wrote, "a write-through one-shot has nothing outstanding")
            }
            other => panic!("expected Flushed outcome, got {other:?}"),
        }
        let after = std::fs::metadata(&state_json).unwrap().modified().unwrap();
        assert_eq!(
            after, before,
            "an idempotent flush must not rewrite the store"
        );
    }

    /// ADR-0042 F4 — the apply path no longer round-trips through disk. Proof by
    /// removal: after an apply, deleting `state.json` outright and applying again
    /// must still build on the *resident* prior — under the old code the second
    /// apply's `FsStore::apply_delta` would have re-read the (now missing) file
    /// and merged onto an empty state, losing the first apply's nodes.
    #[test]
    fn an_apply_merges_onto_resident_state_not_a_disk_re_read() {
        let scratch = TempDir::new().unwrap();
        let root = scratch.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "pub fn b() -> u32 { 2 }\n").unwrap();

        let project = Project::new(root.to_path_buf());
        std::fs::create_dir_all(&project.output_dir).unwrap();
        let flusher = flusher(4);

        apply_exact(
            &project,
            &cs(&["src/a.rs"], &[]),
            ClusterConfig::default(),
            &flusher,
            Persistence::Flush,
        )
        .unwrap();
        let after_a = node_count(&project);
        assert!(after_a > 0, "precondition: a.rs indexed");

        // Yank the checkpoint out from under the apply path.
        std::fs::remove_file(project.output_dir.join("state.json")).unwrap();

        apply_exact(
            &project,
            &cs(&["src/b.rs"], &[]),
            ClusterConfig::default(),
            &flusher,
            Persistence::Flush,
        )
        .unwrap();
        assert!(
            node_count(&project) > after_a,
            "the second apply must extend the resident prior, not a re-read of disk"
        );
    }

    /// Registry/lifecycle commands have nothing to act on in a registry-less
    /// one-shot process — must be a clear error, not a silent no-op or a panic.
    #[test]
    fn run_command_cold_rejects_registry_and_lifecycle_commands() {
        let scratch = TempDir::new().unwrap();
        let project = Project::new(scratch.path().to_path_buf());
        let cache = Arc::new(Mutex::new(ProjectStateCache::new(1)));
        let locks = ProjectLocks::new();

        for command in [
            crate::Command::DaemonStop,
            crate::Command::ProjectRegister {
                path: "/tmp/whatever".to_string(),
            },
            crate::Command::ProjectRemove {
                project: project.id.clone(),
            },
        ] {
            let err = run_command_cold(&project, command, ClusterConfig::default(), &cache, &locks)
                .unwrap_err();
            assert!(
                err.to_string().contains("resident daemon"),
                "expected a resident-daemon-required error, got: {err}"
            );
        }
    }
}
