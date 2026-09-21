//! filigrio-store — the `GraphStore` port (HLD §4, ADR-0005).
//!
//! Persistence is real; the *native* format (here serde JSON of `GraphState`)
//! is deliberately simple and swappable — the port lets it become embedded KV
//! (`redb`) for the portable flavour or object-store/columnar for enterprise
//! without touching the engine. `graph.json` is a separate **interchange**
//! format (import + export), not the storage backend — see [`graphjson`] and
//! ADR-0017.
//!
//! Adapters:
//!   * `MemoryStore` — in-process state behind a `Mutex` (Send + Sync, so it can
//!     back the worker/queue). Used by `demo` and the pipeline tests.
//!   * `FsStore` — a directory holding native `state.json` + a `graph.json`
//!     snapshot; bulk `apply_delta` (HLD §11.4) and a **shrink-guard** (§479).

#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]
pub mod atomic;
pub mod graphjson;

use filigrio_core::{Edge, EdgeTarget, Error, GraphDelta, GraphState, GraphStore, NodeId, Result};
use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

/// Apply a delta to an owned state in place. Shared by every adapter so the
/// merge semantics are defined once.
///
/// **Three facets are patches, six replace wholesale** (ADR-0042 Phase 1d P1 /
/// B2). Edges and `reverse` are applied as remove-then-append against `state`,
/// so `state` **must be the very state the delta was computed against** — the
/// engine's `prior`. Every call site satisfies that structurally (the pipeline
/// reads `prior` from the store it then writes; `DeferredStore` is documented to
/// merge onto the resident state the daemon holds under the project lock), and
/// it is the same requirement any incremental store has. Before Phase 1d the
/// edge facet was a wholesale replacement, which made this merge accidentally
/// self-healing against a mismatched base; it is not any more, which is the
/// price of not shipping the whole graph in every delta.
fn merge(state: &mut GraphState, delta: &GraphDelta) {
    // 1. Remove dropped nodes and any edge touching them.
    let removed: BTreeSet<&NodeId> = delta.nodes_removed.iter().collect();
    if !removed.is_empty() {
        state.graph.nodes.retain(|n| !removed.contains(&n.id));
    }
    // 2. Add new nodes (dedup by id — idempotent apply, HLD §11.4).
    let existing: BTreeSet<NodeId> = state.graph.nodes.iter().map(|n| n.id.clone()).collect();
    for n in &delta.nodes_added {
        if !existing.contains(&n.id) {
            state.graph.nodes.push(n.clone());
        }
    }
    // 3. Edges: drop the retired ones (in place, keeping prior order), append the
    //    new ones. Multiset semantics — an edge listed once in `edges_removed`
    //    removes one occurrence, not every duplicate.
    if !delta.edges_removed.is_empty() {
        let mut budget: HashMap<&Edge, usize> = HashMap::new();
        for e in &delta.edges_removed {
            *budget.entry(e).or_default() += 1;
        }
        state.graph.edges.retain(|e| match budget.get_mut(e) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        });
    }
    state.graph.edges.extend(delta.edges_added.iter().cloned());
    prune_dangling(state);
    // 4. `reverse` is a patch too: drop every reference whose source retired,
    //    append this apply's, then re-canonicalize only the names that moved.
    merge_reverse(state, delta);
    // 5. The remaining derived indices replace wholesale — by decision, not by
    //    omission (ADR-0042 B2): `partition` is a global sidecar clustering
    //    rewrites anyway, `manifest`/`exports`/`workspace` are global sidecars,
    //    and `symbols`/`symbol_index` are never persisted at all (F3).
    state.partition = delta.partition.clone();
    state.symbols = delta.symbols.clone();
    state.manifest = delta.manifest.clone();
    state.workspace = delta.workspace.clone();
    state.exports = delta.exports.clone();
    state.symbol_index = delta.symbol_index.clone();
}

/// The `reverse` half of [`merge`]: drop by source, append, re-canonicalize.
///
/// Equivalent by construction to the engine's `rebuild_refs` + `heal_diverged_refs`
/// (which keep prior references whose source survived, push the new ones, then
/// `sort` + `dedup` each list): the same three operations, done against the
/// resident index instead of a fresh map. Only names the patch actually touched
/// are re-sorted — every other list was already canonical and stays untouched,
/// which is the whole saving over rebuilding the ~33 MB index each apply.
fn merge_reverse(state: &mut GraphState, delta: &GraphDelta) {
    let dropped: BTreeSet<&NodeId> = delta.reverse_dropped.iter().collect();
    let mut touched: BTreeSet<String> = BTreeSet::new();
    // A name whose references all retire disappears, exactly as a from-scratch
    // rebuild would never have created it. The `is_empty` sweep runs even when
    // nothing was dropped so a state that arrived holding an empty list (a
    // hand-edited `state.json`) converges rather than keeping a phantom name.
    state.reverse.refs.retain(|name, list| {
        if !dropped.is_empty() {
            let before = list.len();
            list.retain(|r| !dropped.contains(&r.source));
            if list.len() != before {
                touched.insert(name.clone());
            }
        }
        !list.is_empty()
    });
    for (name, r) in &delta.reverse_added {
        state
            .reverse
            .refs
            .entry(name.clone())
            .or_default()
            .push(r.clone());
        touched.insert(name.clone());
    }
    for name in &touched {
        if let Some(list) = state.reverse.refs.get_mut(name) {
            list.sort();
            list.dedup();
        }
    }
}

/// Drop edges whose endpoints no longer exist (health invariant, HLD §5 / Phase 5).
fn prune_dangling(state: &mut GraphState) {
    let ids: BTreeSet<&NodeId> = state.graph.nodes.iter().map(|n| &n.id).collect();
    state.graph.edges.retain(|e: &Edge| {
        ids.contains(&e.source)
            && match &e.target {
                EdgeTarget::Node(t) => ids.contains(t),
                EdgeTarget::Symbol(_) => true, // unresolved edges are allowed to dangle
            }
    });
}

/// Merge `delta` onto `prior` **without cloning the facets the delta replaces**
/// (ADR-0042 Phase 1c F4, revised by Phase 1d P1).
///
/// `merge` carries forward exactly the facets the delta *patches* rather than
/// replaces — `graph.nodes`, `graph.edges` and `reverse` — so those three have
/// to be seeded from `prior`. The six wholesale facets
/// (`partition`/`symbols`/`manifest`/`workspace`/`exports`/`symbol_index`) are
/// overwritten a few lines later, so copying them here would be pure waste;
/// `Default` is cheaper and provably equivalent.
///
/// Phase 1d moved `graph.edges` and `reverse` from the second group into the
/// first: they used to be replaced wholesale from the delta (which is why F4
/// seeded only the node vector), and are now patched in place. Note the copy
/// *count* did not grow — the same ~350 k edges that were cloned out of the
/// delta are now cloned out of the prior — and the composed path below avoids
/// even that.
///
/// "Same result" is not asserted here, it is **tested**: `tests/store.rs`
/// compares this against a full `FsStore` round-trip over a state with every
/// facet populated, so a new `GraphState` field that `merge` does not overwrite
/// fails the suite instead of silently vanishing on every daemon apply.
fn merge_from(prior: &GraphState, delta: &GraphDelta) -> GraphState {
    let mut next = GraphState {
        graph: prior.graph.clone(),
        reverse: prior.reverse.clone(),
        ..Default::default()
    };
    merge(&mut next, delta);
    next
}

/// A `GraphStore` that merges **into memory** and hands the result back — the
/// write-behind seam for ADR-0042 B12 / Phase 1c F4.
///
/// `Pipeline::apply` persists through the `GraphStore` port. Under B12, *when*
/// an apply reaches disk is a daemon cadence decision, not a property of the
/// apply — so the daemon drives the pipeline with one of these and then decides
/// whether to `FsStore::save_state` immediately (a client is waiting) or to mark
/// the project dirty and let the flusher write it later (the producer lane).
/// The pipeline stays policy-free: it still just calls `apply_delta`.
///
/// It also removes two full `state.json` parses per daemon apply that had
/// nothing to do with persistence cadence: `FsStore::apply_delta` re-read from
/// disk the state the daemon already held resident, and the daemon then re-read
/// it *again* to refresh its cache. Here the prior is the resident state by
/// construction and the merged state is returned directly.
///
/// **Contract difference from `FsStore`, stated deliberately:** the merge base
/// is the caller-supplied `prior` (the resident state), not whatever is on disk.
/// Within the daemon these were already required to be equal — the per-project
/// lock makes the apply a read-modify-write of the state the daemon just read —
/// and under write-behind the resident copy is by definition the newer of the
/// two, so basing the merge on disk would be the bug.
pub struct DeferredStore<'a> {
    prior: &'a GraphState,
    /// `Some` once a delta has been applied — which is also the daemon's signal
    /// that this apply actually mutated anything, replacing the old "re-read the
    /// store and hope the report's `changed` count agreed" heuristic.
    applied: Mutex<Option<GraphState>>,
}

impl<'a> DeferredStore<'a> {
    /// A deferred store over the resident `prior` state.
    pub fn new(prior: &'a GraphState) -> Self {
        DeferredStore {
            prior,
            applied: Mutex::new(None),
        }
    }

    /// The merged state, if any delta was applied. `None` means the apply was a
    /// no-op (gated away, or a reconcile that found no drift) — nothing to
    /// persist and nothing to write back into the cache.
    pub fn take(&self) -> Option<GraphState> {
        self.applied.lock().take()
    }
}

impl GraphStore for DeferredStore<'_> {
    /// The resident prior, or the merged state once an apply has run (so a
    /// second `apply_delta` composes onto the first). Never touches disk.
    fn load_state(&self) -> Result<Option<GraphState>> {
        Ok(Some(match self.applied.lock().as_ref() {
            Some(state) => state.clone(),
            None => self.prior.clone(),
        }))
    }

    /// Merge onto the resident state. A second apply composes onto the first
    /// **in place** — it already owns that state, so there is nothing to copy
    /// (the seeding clone in [`merge_from`] is only needed for the borrowed
    /// `prior`).
    fn apply_delta(&self, delta: &GraphDelta) -> Result<()> {
        let mut applied = self.applied.lock();
        match applied.as_mut() {
            Some(current) => merge(current, delta),
            None => *applied = Some(merge_from(self.prior, delta)),
        }
        Ok(())
    }

    /// A deferred store has no directory, so it cannot produce the `graph.json`
    /// interchange artifact. Saying so is the honest answer (ADR-0029): an
    /// `Ok(())` here would leave the caller believing a file exists. The export
    /// verb flushes the project and snapshots through `FsStore` (ADR-0042 F2).
    fn snapshot(&self) -> Result<()> {
        Err(Error::Storage(
            "DeferredStore holds state in memory and has no directory to write \
             graph.json into — flush the project and use the explicit export verb"
                .to_string(),
        ))
    }
}

/// In-memory store. Default for `demo` and tests.
pub struct MemoryStore {
    state: Mutex<GraphState>,
}

impl MemoryStore {
    pub fn new() -> Self {
        MemoryStore {
            state: Mutex::new(GraphState::default()),
        }
    }

    /// Clone the current state (for handing to a `GraphQuery` view).
    pub fn current(&self) -> Result<GraphState> {
        Ok(self.state.lock().clone())
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphStore for MemoryStore {
    fn load_state(&self) -> Result<Option<GraphState>> {
        Ok(Some(self.current()?))
    }

    fn apply_delta(&self, delta: &GraphDelta) -> Result<()> {
        let mut state = self.state.lock();
        merge(&mut state, delta);
        Ok(())
    }

    fn snapshot(&self) -> Result<()> {
        Ok(()) // no external artifact in the memory store
    }
}

/// A directory-backed store: native `state.json` (full `GraphState`) plus a
/// `graph.json` interchange snapshot. Works across CLI invocations.
pub struct FsStore {
    dir: PathBuf,
    /// Bypass the shrink-guard (the `--force` escape hatch, graphify #479).
    force: bool,
}

impl FsStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        FsStore {
            dir: dir.into(),
            force: false,
        }
    }

    /// Allow `apply_delta` to shrink the graph (skip the shrink-guard).
    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    /// Whether a native checkpoint exists — one `stat`, no read.
    ///
    /// [`load_state`](GraphStore::load_state) deliberately cannot answer this:
    /// it maps a missing `state.json` onto an empty [`GraphState`] so an
    /// incremental apply can start from nothing, which makes "never indexed"
    /// and "indexed to an empty graph" the same value. A caller that must tell
    /// those apart (the daemon distinguishing *registered but unindexed* from
    /// *registered and resident*) asks here.
    pub fn has_checkpoint(&self) -> bool {
        self.state_path().exists()
    }

    /// Persist a full `GraphState` as the native checkpoint (`state.json`),
    /// bypassing the incremental merge. This is the write-back a daemon uses when
    /// an LRU cache eviction pages a project's already-validated in-memory state
    /// out (ADR-0032 §2); the shrink-guard does not apply — the state is the
    /// authority being checkpointed, not a candidate delta.
    pub fn save_state(&self, state: &GraphState) -> Result<()> {
        let native = serde_json::to_vec_pretty(state)?;
        self.write(self.state_path(), &native)
    }

    fn read(&self) -> Result<GraphState> {
        match std::fs::read(self.state_path()) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(GraphState::default()),
            Err(e) => Err(Error::Storage(format!(
                "{}: {e}",
                self.state_path().display()
            ))),
        }
    }

    fn write(&self, path: PathBuf, bytes: &[u8]) -> Result<()> {
        // Temp+rename (ADR-0042 Phase 1c F1): a crash mid-write must never
        // leave a torn state.json / graph.json — the prior file survives.
        atomic::atomic_write(&path, bytes)
            .map_err(|e| Error::Storage(format!("{}: {e}", path.display())))
    }
}

impl GraphStore for FsStore {
    fn load_state(&self) -> Result<Option<GraphState>> {
        Ok(Some(self.read()?))
    }

    fn apply_delta(&self, delta: &GraphDelta) -> Result<()> {
        let mut state = self.read()?;
        let before = state.graph.nodes.len();
        merge(&mut state, delta);
        let after = state.graph.nodes.len();
        // Shrink-guard: refuse to overwrite a larger graph with a smaller one
        // unless forced (protects against a failed build wiping the graph).
        if after < before && !self.force {
            return Err(Error::Storage(format!(
                "shrink-guard: would drop {before} → {after} nodes; use --force to allow (filigrio #479)"
            )));
        }
        let native = serde_json::to_vec_pretty(&state)?;
        self.write(self.state_path(), &native)
    }

    fn snapshot(&self) -> Result<()> {
        // Materialize the graphify-compatible interchange file (ADR-0017).
        let state = self.read()?;
        let json = serde_json::to_vec_pretty(&graphjson::export(&state))?;
        self.write(self.dir.join("graph.json"), &json)
    }
}

#[cfg(test)]
mod tests {
    //! `parking_lot::Mutex` doesn't poison (ADR-0032f §... the parking_lot switch):
    //! a panic under the lock releases it on unwind rather than wedging every later
    //! acquirer. This regression pins the *new* contract — one bad thread must not
    //! take the store down with it — superseding ADR-0041's fail-on-poison test.

    use super::*;
    use std::sync::Arc;

    #[test]
    fn panic_under_lock_leaves_store_usable() {
        let store = Arc::new(MemoryStore::new());
        let panicker = Arc::clone(&store);
        let _ = std::thread::spawn(move || {
            let _guard = panicker.state.lock();
            panic!("thread panics while holding the lock");
        })
        .join();

        // parking_lot released the lock on unwind — no poisoning, so the store is
        // still fully operational rather than returning Err forever.
        assert!(
            store.current().is_ok(),
            "current() must still succeed after a panic under the lock"
        );
        assert!(
            store.apply_delta(&GraphDelta::default()).is_ok(),
            "apply_delta() must still succeed after a panic under the lock"
        );
    }
}
