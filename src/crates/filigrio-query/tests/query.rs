//! TDD spec for the petgraph-backed query index. These are written before the
//! implementation: they define the behaviour `GraphView` must satisfy over a
//! hand-built graph.
//!
//! Fixture (resolved edges only):
//!   file ─contains→ a, b, c
//!   a ─calls→ b ─calls→ c
//!
//! God-node degree counts *semantic* edges only (calls/uses…), excluding the
//! structural `contains`/`imports` edges — otherwise a file is always the top
//! "god node", which is meaningless (graphify: god nodes are core abstractions).
//! So semantic degrees are: b=2 (in a, out c), a=1, c=1, file=0.

use filigrio_core::{
    CommunityId, Confidence, Edge, EdgeTarget, Graph, GraphQuery, GraphState, Node, NodeId,
    Partition, QueryOpts, TargetRef, TraversalMode,
};
use filigrio_query::GraphView;
use std::collections::BTreeMap;
use std::sync::Arc;

fn fnode(name: &str) -> Node {
    let mut n = Node::new(format!("fn:{name}"), name, "function");
    n.source_file = Some("src/lib.rs".into());
    n
}

fn edge(src: &str, rel: &str, dst: &str) -> Edge {
    Edge {
        source: NodeId::new(format!("fn:{src}")),
        relation: rel.into(),
        confidence: Confidence::Extracted,
        target: EdgeTarget::Node(NodeId::new(format!("fn:{dst}"))),
    }
}

fn fixture() -> GraphState {
    let file = {
        let mut f = Node::new("file:src/lib.rs", "src/lib.rs", "file");
        f.source_file = Some("src/lib.rs".into());
        f
    };
    let nodes = vec![file, fnode("a"), fnode("b"), fnode("c")];
    let edges = vec![
        Edge {
            source: NodeId::new("file:src/lib.rs"),
            relation: "contains".into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Node(NodeId::new("fn:a")),
        },
        Edge {
            source: NodeId::new("file:src/lib.rs"),
            relation: "contains".into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Node(NodeId::new("fn:b")),
        },
        Edge {
            source: NodeId::new("file:src/lib.rs"),
            relation: "contains".into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Node(NodeId::new("fn:c")),
        },
        edge("a", "calls", "b"),
        edge("b", "calls", "c"),
    ];

    let mut node_community = BTreeMap::new();
    for n in &nodes {
        node_community.insert(n.id.clone(), CommunityId(0));
    }
    GraphState {
        graph: Graph { nodes, edges },
        partition: Partition {
            node_community,
            communities: BTreeMap::new(),
        },
        ..Default::default()
    }
}

/// A relation-filter set, spelled the way a transport hands one over.
fn filter(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|s| s.to_string()).collect()
}

#[test]
fn neighbors_filtered_by_relation() {
    let v = GraphView::new(Arc::new(fixture()));
    use filigrio_core::Direction;
    // Addressed by exact node id (ADR-0027): a homonym label would be ambiguous.
    let calls = v
        .neighbors_by_id("fn:a", &filter(&["calls"]), Direction::Out)
        .unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1.label, "b");

    // a has no outgoing `contains` edges
    assert!(v
        .neighbors_by_id("fn:a", &filter(&["contains"]), Direction::Out)
        .unwrap()
        .is_empty());
    // unfiltered outgoing = the file's 3 `contains` edges
    assert_eq!(
        v.neighbors_by_id("file:src/lib.rs", &[], Direction::Out)
            .unwrap()
            .len(),
        3
    );
}

/// The neighbor filter is a **set**, OR'd, matched by
/// `filigrio_core::relation::relation_matches_filter` — the same matcher
/// `query_graph`'s `context_filter` uses, not a second `==` implementation.
///
/// This is the regression that made ADR-0036's whole point unusable on the
/// flagship agent tool: `relations=["type"]` is *the* "what uses this type"
/// query, and under `==` it matched nothing at all — so the MCP schema had to
/// withhold the family spelling rather than advertise a value that silently
/// returned an empty list. Both properties are pinned here, at the port, where
/// the bug lived.
#[test]
fn a_neighbor_filter_is_a_set_and_matches_the_family_hierarchically() {
    use filigrio_core::Direction;
    // `a` references three types by position, plus its ordinary call to `b`.
    let mut state = fixture();
    state
        .graph
        .nodes
        .push(Node::new("ty:Widget", "Widget", "struct"));
    for rel in ["type/param", "type/return", "type/field"] {
        state.graph.edges.push(Edge {
            source: NodeId::new("fn:a"),
            relation: rel.into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Node(NodeId::new("ty:Widget")),
        });
    }
    let v = GraphView::new(Arc::new(state));

    let names = |rels: &[&str], dir| -> Vec<String> {
        let mut out: Vec<String> = v
            .neighbors_by_id("fn:a", &filter(rels), dir)
            .unwrap()
            .into_iter()
            .map(|(e, _)| e.relation)
            .collect();
        out.sort();
        out
    };

    // The bare family spelling selects every member — including, by construction,
    // members added after this test was written (ADR-0036 R2.1).
    assert_eq!(
        names(&["type"], Direction::Out),
        vec!["type/field", "type/param", "type/return"],
        "`type` selects the whole family, not nothing"
    );
    // A member narrows to itself, never to a sibling.
    assert_eq!(names(&["type/param"], Direction::Out), vec!["type/param"]);
    // Two entries are a union, not an error.
    assert_eq!(
        names(&["type/param", "type/return"], Direction::Out),
        vec!["type/param", "type/return"]
    );
    // A non-family entry is unaffected by any of it.
    assert_eq!(names(&["calls"], Direction::Out), vec!["calls"]);
    // Mixed sets OR across the two kinds of entry.
    assert_eq!(
        names(&["calls", "type/field"], Direction::Out),
        vec!["calls", "type/field"]
    );

    // "What uses this type" — the same filter read from the other end, which is
    // the direction an agent actually asks it in.
    let users = v
        .neighbors_by_id("ty:Widget", &filter(&["type"]), Direction::In)
        .unwrap();
    assert_eq!(users.len(), 3, "every position that references Widget");
    assert!(users.iter().all(|(_, n)| n.label == "a"));
}

#[test]
fn god_nodes_ranked_by_semantic_degree() {
    let v = GraphView::new(Arc::new(fixture()));
    let gods = v.god_nodes(3).unwrap();
    let ranked: Vec<(&str, usize)> = gods.iter().map(|(n, d)| (n.label.as_str(), *d)).collect();
    // b (2) leads; a/c tie at 1 broken by label; the file (structural only) is 0
    assert_eq!(ranked[0], ("b", 2));
    assert_eq!(ranked[1], ("a", 1));
    assert!(
        !ranked.iter().any(|(l, _)| *l == "src/lib.rs"),
        "a file must never be a god node"
    );
}

#[test]
fn shortest_path_respects_hops() {
    let v = GraphView::new(Arc::new(fixture()));
    // Endpoints are addressed by id (`fn:<name>`), not label (ADR-0027).
    let path = v
        .shortest_path("fn:a", "fn:c", 5)
        .unwrap()
        .expect("a→b→c exists");
    let labels: Vec<&str> = path.iter().map(|n| n.label.as_str()).collect();
    assert_eq!(labels, vec!["a", "b", "c"]);

    // 2 hops needed; capping at 1 finds nothing
    assert!(v.shortest_path("fn:a", "fn:c", 1).unwrap().is_none());
    // no path backwards (directed)
    assert!(v.shortest_path("fn:c", "fn:a", 5).unwrap().is_none());
}

#[test]
fn query_bfs_bounded_by_budget() {
    let v = GraphView::new(Arc::new(fixture()));
    let full = v
        .query(
            "a",
            QueryOpts {
                depth: 5,
                budget: 10,
                mode: TraversalMode::Bfs,
                context_filter: None,
            },
        )
        .unwrap();
    // from seed a: a, b, c reachable
    let labels: Vec<&str> = full.nodes.iter().map(|n| n.label.as_str()).collect();
    assert!(labels.contains(&"a") && labels.contains(&"b") && labels.contains(&"c"));

    // budget of 1 visits only the seed
    let tiny = v
        .query(
            "a",
            QueryOpts {
                depth: 5,
                budget: 1,
                mode: TraversalMode::Bfs,
                context_filter: None,
            },
        )
        .unwrap();
    assert_eq!(tiny.nodes.len(), 1);
}

// ---- unresolved-edge status (ADR-0029) --------------------------------------
//
// The petgraph index holds *resolved* edges only; the unresolved `Symbol` edges
// live in `state.graph.edges` and were invisible to the query surface. These
// specs define the two honest reads that surface them.

/// An unresolved call edge: `src` calls the bare name `name`, receiver opaque.
fn sym_call(src: &str, name: &str) -> Edge {
    let mut tref = TargetRef::new(name);
    tref.hints.insert("recv".into(), "opaque".into());
    Edge {
        source: NodeId::new(format!("fn:{src}")),
        relation: "calls".into(),
        confidence: Confidence::Extracted,
        target: EdgeTarget::Symbol(tref),
    }
}

/// `fixture()` + a hub `with_version` and two opaque callers of it, plus one
/// declined outgoing call from `a` to an external `moondream2`.
fn fixture_with_unresolved() -> GraphState {
    let mut st = fixture();
    st.graph.nodes.push(fnode("with_version"));
    // two by-name callers of `with_version` (opaque receivers) …
    st.graph.edges.push(sym_call("a", "with_version"));
    st.graph.edges.push(sym_call("b", "with_version"));
    // … and one declined outgoing call from `a` to an unknown external.
    st.graph.edges.push(sym_call("a", "moondream2"));
    st
}

#[test]
fn unresolved_out_by_id_lists_this_nodes_declined_calls() {
    let v = GraphView::new(Arc::new(fixture_with_unresolved()));
    let out = v.unresolved_out_by_id("fn:a").unwrap();
    let names: Vec<&str> = out
        .iter()
        .filter_map(|e| match &e.target {
            EdgeTarget::Symbol(r) => Some(r.name.as_str()),
            _ => None,
        })
        .collect();
    // `a` declined two calls (`with_version`, `moondream2`); `b` declined one.
    assert!(
        names.contains(&"with_version") && names.contains(&"moondream2"),
        "a's declined outgoing calls, got {names:?}"
    );
    assert_eq!(out.len(), 2, "exactly a's two unresolved out-edges");
    // resolved edges are never returned here
    assert!(out
        .iter()
        .all(|e| matches!(e.target, EdgeTarget::Symbol(_))));
    // and a node with no declined calls reports none
    assert!(v.unresolved_out_by_id("fn:c").unwrap().is_empty());
}

#[test]
fn unresolved_in_by_label_finds_by_name_callers_with_addressable_source() {
    let v = GraphView::new(Arc::new(fixture_with_unresolved()));
    let callers = v.unresolved_in_by_label("with_version").unwrap();
    let mut ids: Vec<&str> = callers.iter().map(|(_, n)| n.id.0.as_str()).collect();
    ids.sort();
    // the two opaque callers, each surfaced as its *addressable* enclosing node
    assert_eq!(ids, vec!["fn:a", "fn:b"], "by-name callers of with_version");
    // the returned Node is the caller (edge source), not the (unresolvable) callee
    for (e, n) in &callers {
        assert_eq!(&e.source, &n.id, "returned Node is the caller/source");
    }
    // a name nobody calls → empty (not a lie)
    assert!(v.unresolved_in_by_label("nonexistent").unwrap().is_empty());
}
