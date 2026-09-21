//! The view cache, seen from the wire (audit §L1).
//!
//! `tests/cache.rs` L9–L12 pin the cache *entry* invariants in isolation. This
//! suite pins the property those invariants exist to guarantee, along the real
//! path: **an apply is visible to the very next query**, through
//! `Flusher::record_apply` → `ProjectStateCache` → `RegistryWarmStateSource` →
//! `Responder`, on both persistence lanes.
//!
//! Why this is the test that matters: before the cache, a query rebuilt the
//! petgraph index from resident state every time and *could not* be stale. The
//! cache trades that away for latency (24 ms/query at 20k nodes, ~100 ms at
//! next.js scale — `docs/perf/benchmarks.md` §6), and a stale view does not
//! fail loudly: it answers, confidently, from a graph the daemon has replaced.

use filigrio_core::{GraphState, Node};
use filigrio_daemon::{
    DataQuery, FlushConfig, Flusher, Persistence, Project, ProjectLocks, ProjectRegistry,
    ProjectStateCache, RegistryWarmStateSource, Responder, Response, SystemClock,
};
use filigrio_protocol::NodeAddress;
use parking_lot::Mutex;
use std::sync::Arc;
use tempfile::TempDir;

/// A state holding exactly one function node, so "which graph answered" is
/// readable off any response.
fn state_with_node(id: &str) -> GraphState {
    let mut s = GraphState::default();
    s.graph.nodes.push(Node::new(id, id, "function"));
    s
}

fn address(id: &str) -> NodeAddress {
    NodeAddress {
        id: Some(id.to_string()),
        label: None,
        src: None,
    }
}

struct Harness {
    _temp: TempDir,
    project: Project,
    cache: Arc<Mutex<ProjectStateCache>>,
    flusher: Arc<Flusher>,
    responder: Responder<RegistryWarmStateSource>,
}

impl Harness {
    fn new(initial: GraphState) -> Self {
        let temp = TempDir::new().expect("tempdir");
        let project = Project::new(temp.path().to_path_buf());
        let cache = Arc::new(Mutex::new(ProjectStateCache::new(4)));
        cache.lock().load_into(&project.id, initial);
        let registry = Arc::new(Mutex::new(ProjectRegistry::new()));
        registry.lock().add(project.clone()).expect("register");
        let flusher = Arc::new(Flusher::new(
            Arc::clone(&cache),
            ProjectLocks::new(),
            FlushConfig::default(),
            Arc::new(SystemClock),
        ));
        let responder = Responder::new(RegistryWarmStateSource::new(
            Arc::clone(&flusher),
            Arc::clone(&registry),
        ));
        Harness {
            _temp: temp,
            project,
            cache,
            flusher,
            responder,
        }
    }

    /// `true` if the graph currently answering queries contains `node_id`.
    fn sees(&self, node_id: &str) -> bool {
        let response = self.responder.handle_query(DataQuery::GetNode {
            project: self.project.id.clone(),
            node_address: address(node_id),
        });
        match response {
            Response::QueryResult { data } => data["id"] == node_id,
            Response::Error { .. } => false,
            other => panic!("unexpected response: {other:?}"),
        }
    }
}

/// The producer lane (`Defer`): a watcher-driven apply updates resident state
/// only. The next query must see the new graph and must not see the old one.
#[test]
fn a_deferred_apply_is_visible_to_the_next_query() {
    let h = Harness::new(state_with_node("fn:before"));

    assert!(h.sees("fn:before"), "the pre-apply graph must answer first");
    assert!(!h.sees("fn:after"));
    // That query built and cached a view — the state this test now replaces.
    assert!(h.cache.lock().view(&h.project.id).is_some());

    h.flusher
        .record_apply(&h.project, state_with_node("fn:after"), Persistence::Defer)
        .expect("apply");

    assert!(h.sees("fn:after"), "the apply is invisible to queries");
    assert!(
        !h.sees("fn:before"),
        "a query answered from the pre-apply graph — the cached view is stale"
    );
}

/// The client lane (`Flush`): the same property must hold when the apply also
/// writes the checkpoint, since that path inserts state a *different* way
/// (`load_into`, clean) than the producer lane (`put`, dirty).
#[test]
fn a_flushed_apply_is_visible_to_the_next_query() {
    let h = Harness::new(state_with_node("fn:before"));
    assert!(h.sees("fn:before"));

    h.flusher
        .record_apply(&h.project, state_with_node("fn:after"), Persistence::Flush)
        .expect("apply");

    assert!(h.sees("fn:after"));
    assert!(!h.sees("fn:before"));
}

/// Repeated reads share one view: the second query must not rebuild the index.
/// (The saving this whole change exists for — the correctness tests above are
/// what license it.)
#[test]
fn repeated_queries_reuse_one_view() {
    let h = Harness::new(state_with_node("fn:a"));
    assert!(
        h.cache.lock().view(&h.project.id).is_none(),
        "no query yet — nothing should have paid to build an index"
    );

    assert!(h.sees("fn:a"));
    let first = h.cache.lock().view(&h.project.id).expect("view cached");
    assert!(h.sees("fn:a"));
    let second = h.cache.lock().view(&h.project.id).expect("view cached");

    assert!(
        Arc::ptr_eq(&first, &second),
        "the second query rebuilt the view instead of reusing the cached one"
    );
    // …and it indexes the resident state, not a copy of it.
    assert!(Arc::ptr_eq(
        first.state(),
        &h.cache.lock().get_arc(&h.project.id).expect("resident")
    ));
}
