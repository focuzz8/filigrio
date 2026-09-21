//! Per-project state cache with LRU paging (ADR-0032 §2).
//!
//! > Memory: manifests hot, full state paged. Holding every project's full graph
//! > resident is infeasible (next.js alone ≈ 2.2 GB RSS). Only the **manifest**
//! > (the dedup gate) needs to be resident; the full `state.json` is paged in
//! > per-apply and **LRU-evicted**. The on-disk checkpoint is the backing store —
//! > memory is a hot cache, not the system of record.
//!
//! This unit is **pure and filesystem-free**: it never touches disk. On eviction
//! of a *dirty* project it hands the evicted `GraphState` back to the caller, who
//! owns the store and performs the checkpoint write. That keeps the LRU policy
//! deterministic and unit-testable without tempfiles, and keeps the "who writes
//! disk" seam in one place (the daemon).
//!
//! Invariants (each pinned by a test in `tests/cache.rs`):
//! - **manifests never evicted** — a paged-out project keeps a resident manifest
//!   (the dedup gate stays hot). [`ProjectStateCache::manifest`]
//! - **write-back on evict** — a dirty project's state is returned for flush before
//!   its full graph is dropped, so incremental work is never lost.
//! - **LRU victim** — the least-recently-*accessed* project is evicted first;
//!   `access`/`put` refresh recency and must not corrupt the order bookkeeping.
//! - **a cached view never outlives its state** — the derived query index is a
//!   field of the same entry as the `Arc<GraphState>`, so replacing the state
//!   *is* dropping the view (audit §L1; [`ProjectStateCache::view`]).

use filigrio_core::{GraphState, Manifest};
use filigrio_query::GraphView;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// A project paged out of the resident cache. If `state` is `Some`, the project
/// was **dirty** and the caller MUST checkpoint it to the store before it is gone
/// from memory; `None` means it was clean (already matches disk) and can be
/// dropped with no write.
#[derive(Debug)]
pub struct Evicted {
    pub id: String,
    pub state: Option<Arc<GraphState>>,
}

/// One resident project: its state, and the queryable index derived from
/// **that** state.
///
/// The two live in one struct on purpose (audit §L1). A `GraphView` is a pure
/// function of a `GraphState`, so it is cacheable — but a view that outlived the
/// state it was built from would answer queries from a graph the daemon no
/// longer believes in, silently. Making it a field of the entry means every
/// existing way state is replaced (`load_into`, `put`) or removed (eviction)
/// already invalidates the view, with no second bookkeeping step to forget:
/// `insert` overwrites the whole `Resident`, and the old view is dropped with it.
struct Resident {
    state: Arc<GraphState>,
    /// Built on first query against this state, then shared. `None` = this state
    /// has not been queried yet (a state applied and never read never pays for
    /// an index it does not need).
    view: Option<Arc<GraphView>>,
}

/// Hand-written because `GraphView` is an index, not a value: printing it would
/// dump the whole graph. What a cache dump needs is the *shape* of the entry.
impl std::fmt::Debug for Resident {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resident")
            .field("nodes", &self.state.graph.nodes.len())
            .field("edges", &self.state.graph.edges.len())
            .field("view_built", &self.view.is_some())
            .finish()
    }
}

/// LRU cache of full `GraphState` per project, with manifests held resident.
///
/// Resident state is held behind an `Arc` so that serving a hot project is a
/// refcount bump, not an O(nodes+edges) deep copy — critical because the daemon
/// clones the prior state *under the shared cache mutex* on every apply, and a
/// deep copy of a next.js-scale graph there would serialize all projects' applies
/// (ADR-0032 §2 follow-up #1).
#[derive(Debug)]
pub struct ProjectStateCache {
    /// Max number of full graphs resident at once. Full graphs beyond this are
    /// paged out (LRU). A memory-pressure watermark (ADR default 80% RSS) maps
    /// onto this count knob in the daemon.
    capacity: usize,
    /// Resident full state (plus its derived query view), keyed by project id.
    /// `Arc` so a hot hit is shared, not deep-cloned.
    graphs: HashMap<String, Resident>,
    /// Manifests — held resident even when the full graph is paged out (the
    /// dedup gate stays hot). Never evicted.
    manifests: HashMap<String, Manifest>,
    /// Access order for LRU. Front = least-recently-used, back = most-recent.
    /// Each resident graph id appears exactly once.
    order: VecDeque<String>,
    /// Projects whose resident state has been mutated since the last checkpoint
    /// and must be flushed on eviction.
    dirty: HashSet<String>,
}

impl ProjectStateCache {
    /// New cache holding at most `capacity` full graphs resident. `capacity` is
    /// clamped to ≥1 so there is always room for the project being paged in.
    pub fn new(capacity: usize) -> Self {
        ProjectStateCache {
            capacity: capacity.max(1),
            graphs: HashMap::new(),
            manifests: HashMap::new(),
            order: VecDeque::new(),
            dirty: HashSet::new(),
        }
    }

    /// Is this project's full graph currently resident?
    pub fn is_resident(&self, id: &str) -> bool {
        self.graphs.contains_key(id)
    }

    /// Number of full graphs resident.
    pub fn resident_len(&self) -> usize {
        self.graphs.len()
    }

    /// Borrow a resident full graph (no recency change). `None` if paged out.
    pub fn peek(&self, id: &str) -> Option<&GraphState> {
        self.graphs.get(id).map(|r| r.state.as_ref())
    }

    /// Borrow a resident full graph and mark it most-recently-used.
    pub fn get(&mut self, id: &str) -> Option<&GraphState> {
        if self.graphs.contains_key(id) {
            self.touch(id);
            self.graphs.get(id).map(|r| r.state.as_ref())
        } else {
            None
        }
    }

    /// Get a resident full graph as a **shared** `Arc` (refcount bump, not a deep
    /// copy) and mark it most-recently-used. This is the hot-path accessor
    /// `apply::state_of` uses so cloning the prior state under the cache mutex is
    /// O(1), not O(nodes+edges). `None` if paged out.
    pub fn get_arc(&mut self, id: &str) -> Option<Arc<GraphState>> {
        if self.graphs.contains_key(id) {
            self.touch(id);
            self.graphs.get(id).map(|r| Arc::clone(&r.state))
        } else {
            None
        }
    }

    /// A resident full graph as a **shared** `Arc`, **without** touching LRU
    /// recency — the accessor a checkpoint write uses (ADR-0042 F4). Persisting
    /// a project is not a *use* of it: letting a background flush refresh
    /// recency would let the write-behind cadence, rather than actual demand,
    /// decide who gets paged out. `None` if paged out.
    pub fn peek_arc(&self, id: &str) -> Option<Arc<GraphState>> {
        self.graphs.get(id).map(|r| Arc::clone(&r.state))
    }

    // ---- the derived query index (audit §L1) ---------------------------

    /// The cached [`GraphView`] for a resident project, marking it
    /// most-recently-used. `None` means either "paged out" or "not built yet" —
    /// both answered by [`Self::install_view`] + a fresh build, and neither is a
    /// case where serving a *stale* view is possible.
    pub fn view(&mut self, id: &str) -> Option<Arc<GraphView>> {
        let view = self.graphs.get(id)?.view.clone()?;
        self.touch(id);
        Some(view)
    }

    /// Publish a view **only if** the entry still holds the exact state it was
    /// built from (`Arc::ptr_eq`), so a view is never attached to a state it does
    /// not describe.
    ///
    /// The pointer check is what lets the caller build the view *outside* the
    /// cache mutex: at next.js scale construction is ~100 ms, and holding the
    /// one global cache lock for that would serialize every other project's
    /// applies behind one project's first query. If an apply lands in that
    /// window the freshly-built view is simply not installed (the next query
    /// builds one for the new state); the caller still answers this request from
    /// the snapshot it read, which is exactly what it did before any caching.
    ///
    /// Returns whether the view was installed — the observable the cache suite
    /// asserts on, since "not installed" must be a decision, not a silent drop.
    pub fn install_view(&mut self, id: &str, of: &Arc<GraphState>, view: Arc<GraphView>) -> bool {
        let Some(entry) = self.graphs.get_mut(id) else {
            return false;
        };
        if !Arc::ptr_eq(&entry.state, of) {
            return false;
        }
        entry.view = Some(view);
        true
    }

    /// The resident manifest for a project (stays hot even when paged out).
    pub fn manifest(&self, id: &str) -> Option<&Manifest> {
        self.manifests.get(id)
    }

    /// Access all resident manifests (for state source/project resolution).
    pub fn manifests(&self) -> &HashMap<String, Manifest> {
        &self.manifests
    }

    /// Insert freshly-loaded state (a cache miss just resolved from the store).
    /// Not dirty (it matches disk). Returns any project(s) evicted to make room.
    pub fn load_into(&mut self, id: &str, state: GraphState) -> Vec<Evicted> {
        self.insert(id, state, false)
    }

    /// Insert state produced by an apply. Marked **dirty** (differs from disk
    /// until checkpointed). Returns any project(s) evicted to make room.
    pub fn put(&mut self, id: &str, state: GraphState) -> Vec<Evicted> {
        self.insert(id, state, true)
    }

    /// Mark a resident project's state clean (its store checkpoint is current).
    pub fn mark_clean(&mut self, id: &str) {
        self.dirty.remove(id);
    }

    /// Is this project currently dirty (needs write-back on evict)?
    pub fn is_dirty(&self, id: &str) -> bool {
        self.dirty.contains(id)
    }

    // ---- internals ----

    /// Insert (or replace) a project's full state + refresh its manifest, then
    /// evict down to capacity. `dirty` records whether this state has unpersisted
    /// mutations. The manifest is always retained (hot); only the full graph pages.
    fn insert(&mut self, id: &str, state: GraphState, dirty: bool) -> Vec<Evicted> {
        // Manifest stays resident (the dedup gate, ADR-0032 §2/§4). Cloned out
        // before the state is shared into the Arc so it survives the full graph's
        // eviction (manifests are never paged out).
        self.manifests
            .insert(id.to_string(), state.manifest.clone());

        // A whole-entry replacement: the previous state's derived view is
        // dropped here, which is the *only* invalidation this cache needs.
        self.graphs.insert(
            id.to_string(),
            Resident {
                state: Arc::new(state),
                view: None,
            },
        );
        if dirty {
            self.dirty.insert(id.to_string());
        } else {
            self.dirty.remove(id);
        }
        // Most-recently-used: move (or add) this id to the back of the order.
        self.touch(id);

        self.evict_to_capacity()
    }

    /// Move `id` to the most-recently-used position, keeping exactly one entry
    /// per resident id (the invariant that stops the order deque from growing
    /// unbounded and picking the wrong victim on re-insert — fragility L2b).
    fn touch(&mut self, id: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == id) {
            self.order.remove(pos);
        }
        self.order.push_back(id.to_string());
    }

    /// Evict least-recently-used full graphs until within capacity. A dirty
    /// victim's state is returned for the caller to checkpoint (write-back);
    /// a clean victim returns `None` (already on disk — no rewrite). Manifests
    /// are never evicted.
    fn evict_to_capacity(&mut self) -> Vec<Evicted> {
        let mut evicted = Vec::new();
        while self.graphs.len() > self.capacity {
            // Front of the order is the LRU; skip any stale ids defensively.
            let victim = loop {
                match self.order.pop_front() {
                    Some(id) if self.graphs.contains_key(&id) => break id,
                    Some(_) => continue,
                    None => return evicted, // order/graphs desynced — nothing to evict
                }
            };
            // The victim's view goes with it — `Evicted` carries state only,
            // because a checkpoint write has no use for a query index.
            let state = self.graphs.remove(&victim).map(|r| r.state);
            let was_dirty = self.dirty.remove(&victim);
            evicted.push(Evicted {
                id: victim,
                state: if was_dirty { state } else { None },
            });
        }
        evicted
    }
}
