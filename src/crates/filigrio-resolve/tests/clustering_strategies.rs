//! TDD spec for ADR-0024 — **switchable clustering**: single-level `Simple` vs
//! multi-level `Full` Louvain, plus the two cross-cutting knobs that feed *both*
//! strategies — confidence-weighted edges and per-community cohesion scoring.
//!
//! The pre-0024 behavior (Simple / Uniform / resolution 1.0) is covered by
//! `clustering.rs`; this file exercises everything the switch adds. Fixtures were
//! calibrated so each assertion demonstrates one mechanism unambiguously.

use filigrio_core::{CommunityId, Confidence, Edge, EdgeTarget, Node, NodeId, Partition};
use filigrio_resolve::{cluster_with, ClusterConfig, ClusterStrategy, EdgeWeighting};

fn node(id: &str) -> Node {
    let mut n = Node::new(id, id, "function");
    n.source_file = Some("f.rs".into());
    n
}

fn edge_c(a: &str, b: &str, c: Confidence) -> Edge {
    Edge {
        source: NodeId::new(a),
        relation: "calls".into(),
        confidence: c,
        target: EdgeTarget::Node(NodeId::new(b)),
    }
}

/// Fully-connected `ids` with the given confidence.
fn clique_c(ids: &[&str], c: Confidence) -> Vec<Edge> {
    let mut e = Vec::new();
    for i in 0..ids.len() {
        for j in (i + 1)..ids.len() {
            e.push(edge_c(ids[i], ids[j], c));
        }
    }
    e
}

fn cfg(strategy: ClusterStrategy, weighting: EdgeWeighting, resolution: f64) -> ClusterConfig {
    ClusterConfig {
        strategy,
        weighting,
        resolution,
    }
}

fn n_comms(p: &Partition) -> usize {
    p.communities.len()
}

fn comm_of(p: &Partition, id: &str) -> CommunityId {
    *p.node_community
        .get(&NodeId::new(id))
        .expect("node has a community")
}

/// A ring of `ntri` triangles (each a 3-clique), consecutive triangles joined by a
/// single edge. Single-level Louvain finds every triangle; multi-level aggregation
/// coarsens adjacent ones — a clean `Full < Simple` demonstrator.
fn triangle_ring(ntri: usize) -> (Vec<Node>, Vec<Edge>) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for t in 0..ntri {
        let ids = [format!("t{t}n0"), format!("t{t}n1"), format!("t{t}n2")];
        for id in &ids {
            nodes.push(node(id));
        }
        let refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        edges.extend(clique_c(&refs, Confidence::Extracted));
        let nt = (t + 1) % ntri;
        edges.push(edge_c(
            &format!("t{t}n0"),
            &format!("t{nt}n0"),
            Confidence::Extracted,
        ));
    }
    (nodes, edges)
}

// ---- multi-level: Full merges where Simple stops -----------------------------

#[test]
fn full_merges_hierarchy_that_simple_leaves_split() {
    let (nodes, edges) = triangle_ring(10);
    let simple = cluster_with(
        &nodes,
        &edges,
        &Partition::default(),
        &cfg(ClusterStrategy::Simple, EdgeWeighting::Uniform, 1.0),
    )
    .unwrap();
    let full = cluster_with(
        &nodes,
        &edges,
        &Partition::default(),
        &cfg(ClusterStrategy::Full, EdgeWeighting::Uniform, 1.0),
    )
    .unwrap();
    // Simple sees each triangle; aggregation coarsens them.
    assert_eq!(n_comms(&simple), 10, "single-level finds each triangle");
    assert!(
        n_comms(&full) < n_comms(&simple),
        "multi-level aggregation merges adjacent triangles: full={} simple={}",
        n_comms(&full),
        n_comms(&simple),
    );
}

#[test]
fn full_never_yields_more_communities_than_simple() {
    // Aggregation only ever merges — it cannot split what Simple already found.
    for ntri in [4usize, 6, 8, 10] {
        let (nodes, edges) = triangle_ring(ntri);
        let simple = cluster_with(
            &nodes,
            &edges,
            &Partition::default(),
            &ClusterConfig::simple(),
        )
        .unwrap();
        let full = cluster_with(
            &nodes,
            &edges,
            &Partition::default(),
            &ClusterConfig::full(),
        )
        .unwrap();
        assert!(
            n_comms(&full) <= n_comms(&simple),
            "ntri={ntri}: full={} simple={}",
            n_comms(&full),
            n_comms(&simple)
        );
    }
}

#[test]
fn full_is_deterministic() {
    let (nodes, edges) = triangle_ring(10);
    let a = cluster_with(
        &nodes,
        &edges,
        &Partition::default(),
        &ClusterConfig::full(),
    )
    .unwrap();
    let b = cluster_with(
        &nodes,
        &edges,
        &Partition::default(),
        &ClusterConfig::full(),
    )
    .unwrap();
    assert_eq!(a, b, "same input + Full → identical partition");
}

#[test]
fn full_keeps_community_ids_stable_on_unrelated_add() {
    // Full must inherit the same warm-start + max-overlap id stability as Simple.
    let base_nodes: Vec<Node> = ["a1", "a2", "a3", "b1", "b2", "b3"]
        .iter()
        .map(|s| node(s))
        .collect();
    let mut base_edges = clique_c(&["a1", "a2", "a3"], Confidence::Extracted);
    base_edges.extend(clique_c(&["b1", "b2", "b3"], Confidence::Extracted));

    let p1 = cluster_with(
        &base_nodes,
        &base_edges,
        &Partition::default(),
        &ClusterConfig::full(),
    )
    .unwrap();
    let (id_a, id_b) = (comm_of(&p1, "a1"), comm_of(&p1, "b1"));

    let mut nodes = base_nodes.clone();
    nodes.extend(["x1", "x2", "x3"].iter().map(|s| node(s)));
    let mut edges = base_edges.clone();
    edges.extend(clique_c(&["x1", "x2", "x3"], Confidence::Extracted));

    let p2 = cluster_with(&nodes, &edges, &p1, &ClusterConfig::full()).unwrap();
    assert_eq!(
        comm_of(&p2, "a1"),
        id_a,
        "community A keeps its id under Full"
    );
    assert_eq!(
        comm_of(&p2, "b1"),
        id_b,
        "community B keeps its id under Full"
    );
    let id_x = comm_of(&p2, "x1");
    assert!(
        id_x != id_a && id_x != id_b,
        "new community gets a fresh id"
    );
}

// ---- confidence weighting: a weak homonym bridge must not couple (ADR-0023) --

#[test]
fn confidence_weighting_stops_ambiguous_bridges_from_merging() {
    // Two triangles (intra EXTRACTED) joined by 8 AMBIGUOUS cross edges. Under
    // Uniform, the bridges weigh as much as real calls and the groups merge; under
    // Confidence, an AMBIGUOUS bridge weighs 0.25 and the groups stay apart — the
    // clustering half of ADR-0023.
    let a = ["a1", "a2", "a3"];
    let b = ["b1", "b2", "b3"];
    let bridges: [(usize, usize); 8] = [
        (0, 0),
        (1, 1),
        (2, 2),
        (0, 1),
        (1, 2),
        (2, 0),
        (0, 2),
        (1, 0),
    ];
    let nodes: Vec<Node> = a.iter().chain(b.iter()).map(|s| node(s)).collect();
    let mut edges = clique_c(&a, Confidence::Extracted);
    edges.extend(clique_c(&b, Confidence::Extracted));
    for &(i, j) in &bridges {
        edges.push(edge_c(a[i], b[j], Confidence::Ambiguous));
    }

    let uniform = cluster_with(
        &nodes,
        &edges,
        &Partition::default(),
        &cfg(ClusterStrategy::Simple, EdgeWeighting::Uniform, 1.0),
    )
    .unwrap();
    let confidence = cluster_with(
        &nodes,
        &edges,
        &Partition::default(),
        &cfg(ClusterStrategy::Simple, EdgeWeighting::Confidence, 1.0),
    )
    .unwrap();
    assert_eq!(
        n_comms(&uniform),
        1,
        "uniform weighting merges on the bridges"
    );
    assert_eq!(
        n_comms(&confidence),
        2,
        "confidence weighting keeps the two groups apart"
    );
    assert_ne!(comm_of(&confidence, "a1"), comm_of(&confidence, "b1"));
}

// ---- resolution knob ---------------------------------------------------------

#[test]
fn higher_resolution_yields_more_communities() {
    // A single 6-clique is one community at resolution 1.0; cranking the resolution
    // penalty fragments it — the granularity dial works, on both strategies.
    let six = ["s1", "s2", "s3", "s4", "s5", "s6"];
    let nodes: Vec<Node> = six.iter().map(|s| node(s)).collect();
    let edges = clique_c(&six, Confidence::Extracted);

    let low = cluster_with(
        &nodes,
        &edges,
        &Partition::default(),
        &cfg(ClusterStrategy::Simple, EdgeWeighting::Uniform, 1.0),
    )
    .unwrap();
    let high = cluster_with(
        &nodes,
        &edges,
        &Partition::default(),
        &cfg(ClusterStrategy::Simple, EdgeWeighting::Uniform, 2.0),
    )
    .unwrap();
    assert_eq!(n_comms(&low), 1, "one clique at resolution 1.0");
    assert!(
        n_comms(&high) > n_comms(&low),
        "higher resolution fragments: high={} low={}",
        n_comms(&high),
        n_comms(&low)
    );
}

// ---- cohesion scoring --------------------------------------------------------

#[test]
fn cohesion_is_full_for_disjoint_communities() {
    // Two disconnected triangles: every edge stays inside its community → cohesion
    // 1.0 for both.
    let nodes: Vec<Node> = ["a1", "a2", "a3", "b1", "b2", "b3"]
        .iter()
        .map(|s| node(s))
        .collect();
    let mut edges = clique_c(&["a1", "a2", "a3"], Confidence::Extracted);
    edges.extend(clique_c(&["b1", "b2", "b3"], Confidence::Extracted));

    let p = cluster_with(
        &nodes,
        &edges,
        &Partition::default(),
        &ClusterConfig::simple(),
    )
    .unwrap();
    assert_eq!(n_comms(&p), 2);
    for meta in p.communities.values() {
        assert_eq!(
            meta.cohesion_permille, 1000,
            "disjoint community is fully cohesive"
        );
        assert_eq!(meta.cohesion(), 1.0);
    }
}

#[test]
fn cohesion_drops_with_a_boundary_edge() {
    // Two triangles joined by one bridge, clustered into 2: each community has 3
    // internal edges + 1 boundary edge → cohesion 3/4 = 0.75.
    let nodes: Vec<Node> = ["a1", "a2", "a3", "b1", "b2", "b3"]
        .iter()
        .map(|s| node(s))
        .collect();
    let mut edges = clique_c(&["a1", "a2", "a3"], Confidence::Extracted);
    edges.extend(clique_c(&["b1", "b2", "b3"], Confidence::Extracted));
    edges.push(edge_c("a1", "b1", Confidence::Extracted)); // the bridge

    let p = cluster_with(
        &nodes,
        &edges,
        &Partition::default(),
        &ClusterConfig::simple(),
    )
    .unwrap();
    assert_eq!(n_comms(&p), 2, "one bridge does not merge the triangles");
    for meta in p.communities.values() {
        assert_eq!(
            meta.cohesion_permille, 750,
            "3 internal / (3 internal + 1 boundary)"
        );
        assert!((meta.cohesion() - 0.75).abs() < 1e-9);
    }
}
