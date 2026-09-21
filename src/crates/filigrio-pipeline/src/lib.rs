//! filigrio-pipeline — the orchestrator (HLD §3/§5). Wires the ports together
//! around one entry point:
//!
//!   * [`Pipeline::build`] — one cold/warm `apply` pass: poll → index → resolve
//!     → store. This is the Kappa write loop (HLD §5.1/§5.3).
//!
//! Queued/asynchronous application of changesets is the freshness daemon's job
//! (ADR-0032, `filigrio-daemon`), which superseded the day-1 in-process
//! worker/`WorkQueue` layer (ADR-0015/0007, removed 2026-07-24).

#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

/// Per-file freshness comparison (mtime → hash vs the manifest). The dedup gate
/// the incremental reconcile and the daemon's apply gate both use (ADR-0032c).
pub mod dedup;

/// Incremental reconcile: walk ∩ boundary, dedup each file vs the manifest, and
/// synthesize removals (`manifest ∉ walk`). The single drift/reconcile strategy —
/// used by `build`, the daemon's index/startup reconcile, and (soon) `build --watch`.
pub mod reconcile;
pub use reconcile::{reconcile, ReconcileConfig, ReconcileReport};

use filigrio_core::{
    ChangeSet, Error, Extractor, Graph, GraphDelta, GraphState, GraphStore, NodeId, Result, Source,
};
use filigrio_resolve::Engine;
pub use filigrio_resolve::{
    ClusterConfig, ClusterStrategy, ClusterTiming, EdgeWeighting, LinkScope,
};
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

/// Summary of one `apply` pass — telemetry for the O(change) claim (HLD §11.1).
#[derive(Clone, Debug, Default)]
pub struct BuildReport {
    pub changed: usize,
    pub nodes_added: usize,
    pub nodes_removed: usize,
    /// Edges this apply **added** — since ADR-0042 Phase 1d P1 the delta is a
    /// patch, so this is the size of the change, not the size of the graph.
    pub edges_added: usize,
    /// Edges this apply **removed**. The pair is the honest report of a patch:
    /// `edges_added: 0, edges_removed: 0` means "resolution did not move".
    pub edges_removed: usize,
    /// Size of the affected set (`GraphDelta::affected`) — a name-based
    /// **over-counting telemetry heuristic** (every surviving referrer of every
    /// symbol label (un)defined in a touched file counts, even if its resolution
    /// did not change), not an exact changed-edge count.
    pub affected: usize,
    /// How many of the `changed` paths had **vanished** by the time the engine
    /// read them and were converged into removals instead of aborting the apply
    /// (ADR-0042 F8). Reported so the convergence stays visible (ADR-0029): a
    /// client sees `changed: 3, vanished: 1`. Exists-but-unreadable is *not*
    /// counted here — that is still a hard error.
    pub vanished: usize,
}

/// Owns the three write-path ports and drives a single `apply`.
pub struct Pipeline<'a> {
    pub source: &'a dyn Source,
    pub extractor: &'a dyn Extractor,
    pub store: &'a dyn GraphStore,
    /// Clustering strategy/knobs for this pipeline (ADR-0024). Defaults to
    /// `Simple`/`Uniform`/`1.0` — today's behavior — unless overridden.
    pub cluster_cfg: ClusterConfig,
    /// Enforce the shrink-guard (ADR-0032 §5) inside [`Pipeline::apply`]: reject an
    /// apply that drops nodes from files it did not touch. Off by default (the
    /// CLI `build` relies on the store's own `--force` guard); the **daemon** turns it
    /// on so it is the sole authority for a live apply. See [`check_shrink_guard`].
    shrink_guard: bool,
    /// The re-resolution strategy (ADR-0042 Phase 1.3). `Global` by default —
    /// every reference re-resolved each apply; the **daemon** selects `Scoped`
    /// (re-resolve only the change's impact set), see `with_link_scope`.
    link_scope: LinkScope,
    /// When clustering runs (`ClusterTiming`). `Inline` by default; the daemon's
    /// watcher lane selects `Deferred` and reclusters once a burst is over.
    cluster_timing: ClusterTiming,
}

impl<'a> Pipeline<'a> {
    pub fn new(
        source: &'a dyn Source,
        extractor: &'a dyn Extractor,
        store: &'a dyn GraphStore,
    ) -> Self {
        Pipeline {
            source,
            extractor,
            store,
            cluster_cfg: ClusterConfig::default(),
            shrink_guard: false,
            link_scope: LinkScope::Global,
            cluster_timing: ClusterTiming::Inline,
        }
    }

    /// Set the clustering configuration (ADR-0024). Builder-style so existing
    /// `Pipeline::new(...)` call sites are unaffected.
    pub fn with_cluster_config(mut self, cfg: ClusterConfig) -> Self {
        self.cluster_cfg = cfg;
        self
    }

    /// Enable the in-apply shrink-guard (the daemon's live-apply authority). See the
    /// `shrink_guard` field and [`check_shrink_guard`].
    pub fn with_shrink_guard(mut self, on: bool) -> Self {
        self.shrink_guard = on;
        self
    }

    /// Select the re-resolution strategy (ADR-0042 Phase 1.3). `Scoped` is
    /// proven equal to `Global` by the equivalence gates (`just shadow`,
    /// `just convergence`) and re-links only the change's impact set — on a
    /// one-file change at next.js scale, 1.73 s against 3.23 s per apply
    /// (`just profile`). Builder-style so existing call sites keep `Global`.
    pub fn with_link_scope(mut self, scope: LinkScope) -> Self {
        self.link_scope = scope;
        self
    }

    /// Select when clustering runs (`ClusterTiming`). `Deferred` takes the
    /// Louvain pass off the apply; the caller owes `Engine::recluster` after.
    pub fn with_cluster_timing(mut self, timing: ClusterTiming) -> Self {
        self.cluster_timing = timing;
        self
    }

    /// Cold or warm build. The store's prior state decides which: an empty
    /// store → `apply(∅, all-added)`; a populated one → warm update from its
    /// manifest cursor. Same code path (Kappa).
    ///
    /// Expressed as a shallow, drift-fixing [`reconcile_and_apply`](Self::reconcile_and_apply)
    /// (ADR-0032c): walk ∩ boundary, dedup each file vs the manifest → only
    /// *changed* files re-extract (O(change), not the whole tree); files that left
    /// the walk — deleted OR newly `.gitignore`d — are synthesized into `removed`.
    /// Cold build (empty manifest) → every file is `NotFound` → added, i.e. the
    /// full first pass. Requires a filesystem source (it hashes files); a future
    /// non-fs source would provide its own delta via `poll` + [`apply`](Self::apply).
    pub fn build(&self) -> Result<BuildReport> {
        let prior = self.store.load_state()?.unwrap_or_default();
        self.reconcile_and_apply(&prior, false).map(|(_, b)| b)
    }

    /// The **authoritative mutation** over an already-final changeset: extract the
    /// changed files, resolve, stamp real hashes, optionally shrink-guard, and persist
    /// (`apply_delta`; the `graph.json` interchange snapshot left the apply path —
    /// ADR-0042 F2/B4 — and is produced only by the explicit export verb). This is
    /// the single apply core — cold/warm `build` and the daemon's live applies
    /// all route through it, so they can never
    /// diverge. It re-detects nothing: gating (a raw signal) or reconciling (drift)
    /// happens upstream in [`apply_signal`] / [`reconcile_and_apply`].
    pub fn apply(&self, prior: &GraphState, changes: &ChangeSet) -> Result<BuildReport> {
        let mut delta = Engine::apply_with(
            prior,
            changes,
            self.source,
            self.extractor,
            &self.cluster_cfg,
            self.link_scope,
            self.cluster_timing,
        )?;
        // Stamp the real content hash + mtime into each changed file's manifest entry
        // (the Engine leaves them as `hash: 0` stubs) so the next build's incremental
        // reconcile can trust them and re-extract only what actually changed. Fs
        // sources only — a non-fs source has no file to hash and keeps the stub.
        if let Some(root) = self.source.root() {
            for rel in changes.added.iter().chain(&changes.modified) {
                if let Some(entry) = delta.manifest.entries.get_mut(rel) {
                    let full = root.join(rel);
                    entry.last_modified = dedup::get_mtime(&full);
                    if let Ok(h) = dedup::hash_file_u64(&full) {
                        entry.hash = h;
                    }
                }
            }
        }
        // Shrink-guard (ADR-0032 §5), when enabled: reject silent node loss before it
        // is persisted. Runs *before* `apply_delta`, so it is the authority — the daemon
        // pairs it with a `force`d store (the store's own guard stays out of the way).
        if self.shrink_guard {
            check_shrink_guard(&prior.graph, &delta.nodes_removed, &delta.dirty_files)?;
        }
        let report = report_of(changes, &delta);
        self.store.apply_delta(&delta)?;
        Ok(report)
    }

    /// Apply a **raw producer signal** (watcher/git): gate it (scope + dedup) to the
    /// *effective* changeset, then [`apply`](Self::apply). A signal is a hint (ADR-0032a
    /// R2) — scope and freshness are decided here, so no producer can inject an
    /// out-of-scope or no-op file. If the gate dissolves everything, nothing is applied
    /// and the report is empty (`changed == 0`).
    pub fn apply_signal(&self, prior: &GraphState, raw: &ChangeSet) -> Result<BuildReport> {
        let root = self
            .source
            .root()
            .ok_or_else(|| Error::Io("apply_signal requires a filesystem source".into()))?;
        let effective = gate_changeset(raw, prior, self.source, root);
        if effective.added.is_empty()
            && effective.modified.is_empty()
            && effective.removed.is_empty()
        {
            return Ok(BuildReport::default());
        }
        self.apply(prior, &effective)
    }

    /// Reconcile the project against disk and apply the drift **authoritatively**
    /// (ADR-0032a R2.5): the reconcile changeset is already scoped+deduped, so it is
    /// applied as-is — no signal gate — which is why a deep reconcile's mtime-preserved
    /// drift is not dropped. Returns the reconcile telemetry alongside the apply report
    /// (the caller logs it). No drift ⇒ an empty apply report — a reconcile on a
    /// clean tree IS the read-only check (ADR-0042 F6), so there is no separate
    /// `fix` knob anymore: apply iff drift, always.
    pub fn reconcile_and_apply(
        &self,
        prior: &GraphState,
        deep: bool,
    ) -> Result<(ReconcileReport, BuildReport)> {
        let root = self
            .source
            .root()
            .ok_or_else(|| Error::Io("reconcile requires a filesystem source".into()))?;
        let (report, changeset) = reconcile(root, prior, &ReconcileConfig { deep })
            .map_err(|e| Error::Io(format!("reconcile: {e}")))?;
        let build = if report.has_drift {
            self.apply(prior, &changeset)?
        } else {
            BuildReport::default()
        };
        Ok((report, build))
    }
}

/// Shrink guard (ADR-0032 §5): an apply may remove only nodes that belong to a
/// file it **touched**. Losing a node from a file the changeset never named is
/// almost always a reconcile bug, not intent — the "fail-closed" invariant
/// (Graphify's `_check_shrink`). `removed` is the delta's `nodes_removed`;
/// `touched` is its `dirty_files` (added ∪ modified ∪ removed, after the F8
/// vanished-file fold, so a vanished file licenses its own nodes' removal).
///
/// This used to compare node *counts* and allow a drop only when a whole file
/// was removed, which rejected the most ordinary edit there is: deleting a
/// function from a file that still exists. On a live daemon that edit could
/// never land — the graph kept serving the deleted function and the watcher
/// retried the rejected apply forever. Asking *where* each removed node lived
/// keeps the protection the guard exists for and drops the false positive.
///
/// Nodes with no `source_file` are exempt: today that is only the `project`
/// overlay, a projection of the workspace regenerated every apply, whose nodes
/// can legitimately go when a *different* manifest changes (a root
/// `package.json`'s workspace globs dropping a sub-project).
///
/// One pass over `prior` against a set of the removed ids — `Graph::node_by_id`
/// is a linear scan, and a next.js-scale graph holds ~125k nodes. Enforced
/// inside [`Pipeline::apply`] when the pipeline is built `with_shrink_guard(true)`.
pub fn check_shrink_guard(
    prior: &Graph,
    removed: &[NodeId],
    touched: &BTreeSet<String>,
) -> Result<()> {
    if removed.is_empty() {
        return Ok(());
    }
    let removed: HashSet<&NodeId> = removed.iter().collect();
    let mut stray = prior.nodes.iter().filter(|n| {
        removed.contains(&n.id) && n.source_file.as_ref().is_some_and(|f| !touched.contains(f))
    });
    let Some(first) = stray.next() else {
        return Ok(());
    };
    let count = 1 + stray.count();
    Err(Error::Other(format!(
        "shrink guard: apply would drop {count} node(s) from files it did not touch \
         (first: `{}` in `{}`)",
        first.id.0,
        first.source_file.as_deref().unwrap_or_default()
    )))
}

/// The apply-time authority gate (ADR-0032a R2). Turn a producer's raw changeset into
/// the *effective* one actually applied:
///
/// - **scope** — drop any `added`/`modified` path outside the source boundary
///   (`!source.in_scope`), so a gitignored/noise file named by *any* producer can never
///   enter the graph. This is the authority; the producer-side filter (R1) is only a
///   latency optimization on top of it.
/// - **dedup** — drop any `added`/`modified` whose content is unchanged
///   (`dedup_file == Unchanged`); its prior nodes stay in place — identical to a cold
///   build, so this is convergence-safe and never skips a needed relink.
/// - **removed** passes through: dropping nodes for a never-indexed file is a harmless
///   no-op downstream; scope-driven removals (a newly `.gitignore`d dir) arrive via
///   reconcile (R3), not here.
pub fn gate_changeset(
    cs: &ChangeSet,
    prior: &GraphState,
    source: &dyn Source,
    root: &Path,
) -> ChangeSet {
    let keep = |rel: &String| -> bool {
        if !source.in_scope(rel) {
            return false;
        }
        let abs = root.join(rel);
        // `Unchanged` ⇒ drop; `Changed`/`NotFound`/read-error ⇒ keep (a read error is
        // treated as changed, matching reconcile — re-extract rather than skip).
        !matches!(
            dedup::dedup_file(&abs, rel, &prior.manifest, dedup::get_mtime(&abs), false),
            Ok(dedup::DedupResult::Unchanged)
        )
    };
    ChangeSet {
        added: cs.added.iter().filter(|p| keep(p)).cloned().collect(),
        modified: cs.modified.iter().filter(|p| keep(p)).cloned().collect(),
        removed: cs.removed.clone(),
    }
}

fn report_of(changes: &ChangeSet, delta: &GraphDelta) -> BuildReport {
    BuildReport {
        changed: changes.len(),
        nodes_added: delta.nodes_added.len(),
        nodes_removed: delta.nodes_removed.len(),
        edges_added: delta.edges_added.len(),
        edges_removed: delta.edges_removed.len(),
        affected: delta.affected,
        vanished: delta.vanished,
    }
}
