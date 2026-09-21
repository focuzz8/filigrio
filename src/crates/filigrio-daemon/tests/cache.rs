//! LRU state-cache fragility suite (ADR-0032 §2: "manifests hot, full state paged").
//!
//! The cache is pure and filesystem-free, so these tests model the on-disk
//! checkpoint as an in-memory `HashMap` ("the store") and flush evicted-dirty
//! state back into it exactly as the daemon would. Each test names the specific
//! fragility it pins — the *why*, not just the *what*.

use filigrio_core::{GraphQuery, GraphState, ManifestEntry};
use filigrio_daemon::cache::{Evicted, ProjectStateCache};
use filigrio_query::GraphView;
use std::collections::HashMap;
use std::sync::Arc;

/// A `GraphState` tagged with a recognizable marker in its manifest, so tests can
/// assert *which version* of a project's state survived an eviction/reload.
fn state_with(marker: u64) -> GraphState {
    let mut s = GraphState::default();
    s.manifest.entries.insert(
        "mark".to_string(),
        ManifestEntry {
            hash: marker,
            ..Default::default()
        },
    );
    s
}

fn marker_of(s: &GraphState) -> u64 {
    s.manifest.entries.get("mark").map(|e| e.hash).unwrap_or(0)
}

/// The daemon's write-back: a dirty evicted project must be persisted to the
/// backing store before its in-memory copy is gone.
fn flush(store: &mut HashMap<String, GraphState>, evicted: Vec<Evicted>) {
    for e in evicted {
        if let Some(state) = e.state {
            // `state` is the shared `Arc`; the store models the on-disk checkpoint
            // (owned `GraphState`), so deref-clone the inner state.
            store.insert(e.id, (*state).clone());
        }
    }
}

/// L1 — the capacity bound is never exceeded: paging is the whole point, and a
/// cache that lets every project stay resident defeats it (next.js ≈ 2.2 GB each).
#[test]
fn l1_capacity_bound_is_never_exceeded() {
    let mut cache = ProjectStateCache::new(2);
    cache.load_into("A", state_with(1));
    cache.load_into("B", state_with(2));
    cache.load_into("C", state_with(3));

    assert_eq!(cache.resident_len(), 2, "cache exceeded its capacity of 2");
    assert!(cache.is_resident("C") && cache.is_resident("B"));
    assert!(
        !cache.is_resident("A"),
        "A (LRU) should have been paged out"
    );
}

/// L2 — the LRU victim is the least-recently-*accessed*: `get` must refresh
/// recency, or a hot project gets wrongly evicted while a cold one lingers.
#[test]
fn l2_access_refreshes_recency_so_hot_project_survives() {
    let mut cache = ProjectStateCache::new(2);
    cache.load_into("A", state_with(1));
    cache.load_into("B", state_with(2));

    // Touch A → A is now most-recently-used; B becomes the LRU victim.
    assert!(cache.get("A").is_some());
    cache.load_into("C", state_with(3));

    assert!(cache.is_resident("A"), "A was accessed and must survive");
    assert!(cache.is_resident("C"));
    assert!(
        !cache.is_resident("B"),
        "B (now LRU) should be evicted, not A"
    );
}

/// L2b — re-inserting an already-resident id must not corrupt the order deque
/// (a classic LRU bug: duplicate order entries make the wrong victim get evicted
/// and the deque grow unbounded).
#[test]
fn l2b_reinsert_does_not_corrupt_lru_order() {
    let mut cache = ProjectStateCache::new(2);
    cache.load_into("A", state_with(1));
    cache.load_into("A", state_with(10)); // re-insert same id
    cache.load_into("A", state_with(11));
    cache.load_into("B", state_with(2));

    // Only A and B are resident; a third distinct project evicts the true LRU.
    assert_eq!(cache.resident_len(), 2);
    cache.load_into("C", state_with(3));
    assert_eq!(cache.resident_len(), 2, "capacity broke after re-inserts");
    // A was inserted before B and never re-touched after B → A is the victim.
    assert!(!cache.is_resident("A"));
    assert!(cache.is_resident("B") && cache.is_resident("C"));
}

/// L3 — **write-back on evict (the data-loss guard).** A *dirty* project (state
/// produced by an apply, not yet checkpointed) must be handed back for flush when
/// evicted; otherwise the incremental work is silently lost.
#[test]
fn l3_dirty_eviction_returns_state_for_writeback() {
    let mut cache = ProjectStateCache::new(1);
    cache.put("A", state_with(42)); // dirty (apply output)

    let evicted = cache.put("B", state_with(2)); // evicts A

    assert_eq!(evicted.len(), 1);
    assert_eq!(evicted[0].id, "A");
    let flushed = evicted[0]
        .state
        .as_ref()
        .expect("dirty eviction must return the state to flush (else data loss)");
    assert_eq!(
        marker_of(flushed),
        42,
        "the mutated state must survive eviction"
    );
}

/// L4 — a *clean* eviction must NOT return state to rewrite: the on-disk copy is
/// already current, so re-writing it is needless I/O (and risks clobbering a
/// newer external write).
#[test]
fn l4_clean_eviction_does_not_rewrite_store() {
    let mut cache = ProjectStateCache::new(1);
    cache.load_into("A", state_with(1)); // clean (loaded from disk)

    let evicted = cache.load_into("B", state_with(2)); // evicts A

    assert_eq!(evicted.len(), 1);
    assert_eq!(evicted[0].id, "A");
    assert!(
        evicted[0].state.is_none(),
        "a clean project must not be flushed on eviction"
    );
}

/// L5 — **manifests stay hot.** The full graph is paged out, but the manifest —
/// the dedup gate (§4) — remains resident so the pre-enqueue filter never has to
/// hit disk just to decide a file is unchanged.
#[test]
fn l5_manifest_stays_resident_after_full_graph_evicted() {
    let mut cache = ProjectStateCache::new(1);
    cache.put("A", state_with(7));
    cache.put("B", state_with(2)); // pages A's full graph out

    assert!(!cache.is_resident("A"), "A's full graph should be evicted");
    let manifest = cache
        .manifest("A")
        .expect("A's manifest must stay hot after the full graph is paged out");
    assert_eq!(
        manifest.entries.get("mark").unwrap().hash,
        7,
        "the resident manifest must reflect A's latest state"
    );
}

/// L6 — reload-after-evict returns the persisted (mutated) state: the daemon
/// flushes on evict, then a later access re-pages it in from the store.
#[test]
fn l6_reload_after_evict_returns_persisted_state() {
    let mut store: HashMap<String, GraphState> = HashMap::new();
    store.insert("A".into(), state_with(1)); // A already on disk
    let mut cache = ProjectStateCache::new(1);

    // Page A in, mutate it (apply → dirty), then evict via B and flush.
    let a_disk = store["A"].clone();
    flush(&mut store, cache.load_into("A", a_disk));
    flush(&mut store, cache.put("A", state_with(99))); // A mutated & dirty
    flush(&mut store, cache.put("B", state_with(2))); // evicts A → flushes 99

    assert!(!cache.is_resident("A"));
    // Re-page A in from the store: must be the mutation, not the stale 1.
    let a_reload = store["A"].clone();
    flush(&mut store, cache.load_into("A", a_reload));
    assert_eq!(marker_of(cache.peek("A").unwrap()), 99);
}

/// L7 — a hot hit is served without touching the store: `peek`/`get` return the
/// resident copy directly (the cache actually caches).
#[test]
fn l7_hot_hit_is_served_from_memory() {
    let mut cache = ProjectStateCache::new(2);
    cache.load_into("A", state_with(5));
    assert_eq!(
        marker_of(cache.peek("A").expect("A must be a hot hit")),
        5,
        "resident state must be served from memory, no reload"
    );
    // Re-loading the same id must not grow residency beyond one slot for A.
    cache.load_into("A", state_with(6));
    assert_eq!(cache.resident_len(), 1);
    assert_eq!(marker_of(cache.peek("A").unwrap()), 6);
}

/// L8 — **the resident state is shared, not deep-cloned.** `get_arc` hands out an
/// `Arc<GraphState>` that points at the *same* allocation as the cached entry, so
/// serving a hot project (in `apply::state_of`, under the shared cache mutex) is a
/// refcount bump, not an O(nodes+edges) memcpy. Without this, two concurrent
/// applies to different large projects serialize on each other's prior-clone and
/// the worker pool's cross-project parallelism collapses at scale.
#[test]
fn l8_get_arc_shares_storage_not_deep_clone() {
    let mut cache = ProjectStateCache::new(2);
    cache.load_into("A", state_with(1));

    let a1 = cache.get_arc("A").expect("A is resident");
    let a2 = cache.get_arc("A").expect("A is resident");
    assert!(
        Arc::ptr_eq(&a1, &a2),
        "get_arc must return the shared Arc, not a fresh deep clone"
    );
    // cache(1) + a1 + a2 = 3 strong refs to the one allocation.
    assert_eq!(Arc::strong_count(&a1), 3);
    assert_eq!(marker_of(&a1), 1);
}

// ---- the derived query view (audit §L1) -------------------------------------
//
// The view is a *derived index* over one state, cached so that a wire read does
// not rebuild a petgraph over the whole graph before answering (24 ms at 20k
// nodes, ~100 ms at next.js scale). That makes staleness the hazard: a view
// surviving one apply would answer from a graph the daemon no longer believes
// in, and it would do it silently. These pin that it cannot.

/// A state holding one named node, so a test can tell *which* graph a view
/// indexes by asking the view itself rather than trusting the cache.
fn state_with_node(marker: u64, node_id: &str) -> GraphState {
    let mut s = state_with(marker);
    s.graph
        .nodes
        .push(filigrio_core::Node::new(node_id, node_id, "function"));
    s
}

fn view_over(cache: &mut ProjectStateCache, id: &str) -> Arc<GraphView> {
    let state = cache.get_arc(id).expect("resident");
    let view = Arc::new(GraphView::new(Arc::clone(&state)));
    assert!(
        cache.install_view(id, &state, Arc::clone(&view)),
        "installing a view over the state just read must succeed"
    );
    view
}

/// L9 — **the cached view is shared, and it is the one that was installed.**
/// The whole point is that the second query does not rebuild the index.
#[test]
fn l9_cached_view_is_shared_not_rebuilt() {
    let mut cache = ProjectStateCache::new(2);
    cache.load_into("A", state_with_node(1, "fn:a"));
    assert!(
        cache.view("A").is_none(),
        "a state nobody has queried must not have paid for an index"
    );

    let installed = view_over(&mut cache, "A");
    let first = cache.view("A").expect("view is cached");
    let second = cache.view("A").expect("view is cached");
    assert!(Arc::ptr_eq(&first, &second) && Arc::ptr_eq(&first, &installed));
}

/// L10 — **an apply invalidates the view, structurally.** `put`/`load_into`
/// replace the whole entry, so the next reader gets no view rather than the
/// previous state's. This is the silent-wrong-answer case: before the cache, a
/// query rebuilt the index every time and could not be stale at all, so the
/// cache is only allowed to exist if this holds.
#[test]
fn l10_replacing_state_drops_its_view() {
    let mut cache = ProjectStateCache::new(2);
    cache.load_into("A", state_with_node(1, "fn:before"));
    let stale = view_over(&mut cache, "A");
    assert!(cache.view("A").is_some());

    // …an apply lands (producer lane: dirty `put`).
    cache.put("A", state_with_node(2, "fn:after"));
    assert!(
        cache.view("A").is_none(),
        "the pre-apply view survived the apply — queries would answer from the old graph"
    );
    // The flush lane (`load_into`, clean) must invalidate identically.
    let fresh = view_over(&mut cache, "A");
    cache.load_into("A", state_with_node(3, "fn:after"));
    assert!(cache.view("A").is_none());

    // The old views still describe the graphs they were built from — they are
    // not corrupt, they are simply *previous*, which is why they must not be
    // reachable from the cache.
    assert!(stale.node_by_id("fn:before").expect("query").is_some());
    assert!(fresh.node_by_id("fn:after").expect("query").is_some());
}

/// L11 — **a view built against a state that has since been replaced is
/// refused, not installed.** The view is built outside the cache mutex (a
/// ~100 ms build must not block every other project's apply), so an apply can
/// land in that window; publishing it then would reintroduce exactly the
/// staleness L10 forbids.
#[test]
fn l11_install_of_a_superseded_view_is_refused() {
    let mut cache = ProjectStateCache::new(2);
    cache.load_into("A", state_with_node(1, "fn:before"));
    let read = cache.get_arc("A").expect("resident");

    // The apply lands while this request is still building its view.
    cache.put("A", state_with_node(2, "fn:after"));

    let late = Arc::new(GraphView::new(Arc::clone(&read)));
    assert!(
        !cache.install_view("A", &read, late),
        "a view of a superseded state must not be published"
    );
    assert!(cache.view("A").is_none());
    // …and an unknown project is a refusal, not a panic.
    let orphan = Arc::new(GraphView::new(Arc::clone(&read)));
    assert!(!cache.install_view("nope", &read, orphan));
}

/// L12 — **eviction takes the view with it.** The LRU bound exists because a
/// resident next.js project is ≈2.2 GB; a view that outlived its eviction would
/// pin the whole `Arc<GraphState>` it holds and defeat the paging entirely.
#[test]
fn l12_eviction_drops_the_view_and_unpins_the_state() {
    let mut cache = ProjectStateCache::new(1);
    cache.load_into("A", state_with_node(1, "fn:a"));
    let view = view_over(&mut cache, "A");
    let state = Arc::clone(view.state());

    cache.load_into("B", state_with_node(2, "fn:b"));
    assert!(!cache.is_resident("A"), "A must have been paged out");
    assert!(cache.view("A").is_none());

    // Only this test's two handles remain; the cache holds nothing.
    drop(view);
    assert_eq!(
        Arc::strong_count(&state),
        1,
        "an evicted project's state is still pinned by a cached view"
    );
}
