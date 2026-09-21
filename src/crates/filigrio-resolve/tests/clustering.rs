//! TDD spec for Phase 2b — **real, incrementally-maintained clustering**
//! (migration-plan §Phase 2b, HLD §11.1–11.2). Written before the implementation.
//!
//! The mock (`cluster_by_dir`) bucketed nodes by top-level directory. Real
//! clustering is **modularity-based** (a Louvain local-move over the resolved
//! graph), so communities follow *structure*, not paths — and it is
//! **incrementally stable**: re-clustering after a change keeps unchanged
//! communities' ids (max-overlap remap), and warm-starting from the prior
//! partition reproduces the cold grouping.

use filigrio_core::{CommunityId, Confidence, Edge, EdgeTarget, Node, NodeId, Partition};
use filigrio_resolve::cluster;
use std::collections::{BTreeMap, BTreeSet};

fn node(id: &str, file: &str) -> Node {
    let mut n = Node::new(id, id, "function");
    n.source_file = Some(file.into());
    n
}

fn edge(a: &str, b: &str) -> Edge {
    Edge {
        source: NodeId::new(a),
        relation: "calls".into(),
        confidence: Confidence::Inferred,
        target: EdgeTarget::Node(NodeId::new(b)),
    }
}

/// A fully-connected group (every pair linked) of `ids`, all in `file`.
fn clique(ids: &[&str], file: &str) -> (Vec<Node>, Vec<Edge>) {
    let nodes = ids.iter().map(|i| node(i, file)).collect();
    let mut edges = Vec::new();
    for i in 0..ids.len() {
        for j in (i + 1)..ids.len() {
            edges.push(edge(ids[i], ids[j]));
        }
    }
    (nodes, edges)
}

fn concat(a: (Vec<Node>, Vec<Edge>), b: (Vec<Node>, Vec<Edge>)) -> (Vec<Node>, Vec<Edge>) {
    let (mut n, mut e) = a;
    n.extend(b.0);
    e.extend(b.1);
    (n, e)
}

/// The partition as a set of node-id groups (ignores the community *ids*).
fn groups(p: &Partition) -> BTreeSet<BTreeSet<String>> {
    let mut m: BTreeMap<CommunityId, BTreeSet<String>> = BTreeMap::new();
    for (id, c) in &p.node_community {
        m.entry(*c).or_default().insert(id.0.clone());
    }
    m.into_values().collect()
}

fn comm_of(p: &Partition, id: &str) -> CommunityId {
    *p.node_community
        .get(&NodeId::new(id))
        .expect("node has a community")
}

// ---- structure, not directory ----------------------------------------------

#[test]
fn disjoint_cliques_form_separate_communities() {
    let (nodes, edges) = concat(
        clique(&["a1", "a2", "a3"], "a.rs"),
        clique(&["b1", "b2", "b3"], "b.rs"),
    );
    let p = cluster(&nodes, &edges, &Partition::default()).unwrap();
    assert_eq!(
        groups(&p),
        BTreeSet::from([
            BTreeSet::from(["a1".into(), "a2".into(), "a3".into()]),
            BTreeSet::from(["b1".into(), "b2".into(), "b3".into()]),
        ]),
        "two disconnected cliques → two communities"
    );
}

#[test]
fn single_connected_clique_is_one_community() {
    let (nodes, edges) = clique(&["a1", "a2", "a3"], "a.rs");
    let p = cluster(&nodes, &edges, &Partition::default()).unwrap();
    assert_eq!(
        p.communities.len(),
        1,
        "one connected group → one community"
    );
}

#[test]
fn clustering_follows_structure_not_directory() {
    // All six nodes live in the SAME file (one directory) but form two
    // disconnected cliques. `cluster_by_dir` would give 1 community; a
    // structural clusterer must give 2.
    let (nodes, edges) = concat(
        clique(&["x1", "x2", "x3"], "mono.rs"),
        clique(&["y1", "y2", "y3"], "mono.rs"),
    );
    let p = cluster(&nodes, &edges, &Partition::default()).unwrap();
    assert_eq!(
        p.communities.len(),
        2,
        "structure splits one file into two communities"
    );
    assert_ne!(comm_of(&p, "x1"), comm_of(&p, "y1"));
}

#[test]
fn weakly_linked_dense_groups_split() {
    // Two dense 4-cliques joined by a single bridge edge → modularity keeps
    // them apart (a naive connected-components view would merge them).
    let (mut nodes, mut edges) = concat(
        clique(&["p1", "p2", "p3", "p4"], "p.rs"),
        clique(&["q1", "q2", "q3", "q4"], "q.rs"),
    );
    edges.push(edge("p1", "q1")); // the single weak link
    let _ = &mut nodes;
    let p = cluster(&nodes, &edges, &Partition::default()).unwrap();
    assert_eq!(
        p.communities.len(),
        2,
        "a single weak link does not merge two dense groups"
    );
    assert_ne!(comm_of(&p, "p2"), comm_of(&p, "q2"));
}

#[test]
fn clustering_is_deterministic() {
    let (nodes, edges) = concat(
        clique(&["a1", "a2", "a3"], "a.rs"),
        clique(&["b1", "b2", "b3"], "b.rs"),
    );
    let p1 = cluster(&nodes, &edges, &Partition::default()).unwrap();
    let p2 = cluster(&nodes, &edges, &Partition::default()).unwrap();
    assert_eq!(p1, p2, "same input → identical partition");
}

// ---- incremental: id stability + warm-start ---------------------------------

#[test]
fn community_ids_stable_when_unrelated_group_added() {
    let (base_nodes, base_edges) = concat(
        clique(&["a1", "a2", "a3"], "a.rs"),
        clique(&["b1", "b2", "b3"], "b.rs"),
    );
    let p1 = cluster(&base_nodes, &base_edges, &Partition::default()).unwrap();
    let (id_a, id_b) = (comm_of(&p1, "a1"), comm_of(&p1, "b1"));

    // Add an unrelated clique X and re-cluster, warm-started from p1.
    let (nodes, edges) = concat(
        (base_nodes, base_edges),
        clique(&["x1", "x2", "x3"], "x.rs"),
    );
    let p2 = cluster(&nodes, &edges, &p1).unwrap();

    assert_eq!(comm_of(&p2, "a1"), id_a, "community A keeps its id");
    assert_eq!(comm_of(&p2, "b1"), id_b, "community B keeps its id");
    let id_x = comm_of(&p2, "x1");
    assert!(
        id_x != id_a && id_x != id_b,
        "the new community gets a fresh id, not a reused one"
    );
    // and the A/B groupings themselves are untouched
    assert_eq!(comm_of(&p2, "a2"), id_a);
    assert_eq!(comm_of(&p2, "b3"), id_b);
}

#[test]
fn warm_start_reproduces_cold_grouping() {
    let base = concat(
        clique(&["a1", "a2", "a3"], "a.rs"),
        clique(&["b1", "b2", "b3"], "b.rs"),
    );
    let full = concat(base.clone(), clique(&["x1", "x2", "x3"], "x.rs"));

    // Cold: cluster the whole thing from scratch.
    let cold = cluster(&full.0, &full.1, &Partition::default()).unwrap();
    // Warm: cluster base, then re-cluster the whole thing seeded from it.
    let prior = cluster(&base.0, &base.1, &Partition::default()).unwrap();
    let warm = cluster(&full.0, &full.1, &prior).unwrap();

    assert_eq!(
        groups(&cold),
        groups(&warm),
        "warm-started clustering yields the same grouping as a cold re-cluster"
    );
}

#[test]
fn seeded_from_empty_equals_cold() {
    // Warm-starting with an empty prior must degenerate to a cold build.
    let (nodes, edges) = concat(
        clique(&["a1", "a2", "a3"], "a.rs"),
        clique(&["b1", "b2", "b3"], "b.rs"),
    );
    let a = cluster(&nodes, &edges, &Partition::default()).unwrap();
    let b = cluster(&nodes, &edges, &Partition::default()).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.communities.len(), 2);
}
