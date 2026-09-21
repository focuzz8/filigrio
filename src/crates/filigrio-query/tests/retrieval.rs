//! TDD spec for retrieval (Phase 2b, slice 3) — the LLM-facing core.
//!
//! Retrieval must do three things the substring stub could not:
//!   1. **rank seeds by IDF + trigram** — fuzzy (typo-tolerant) matching, with
//!      discriminative (rare) trigrams weighted above common ones;
//!   2. **pack to a budget in seed-priority order** — under a tight budget the
//!      most relevant region survives, not whatever was inserted first;
//!   3. **cite community context** — every returned node carries its community
//!      label, so an agent knows where in the map each result lives.

use filigrio_core::{
    CommunityId, CommunityMeta, Confidence, Edge, EdgeTarget, Graph, GraphQuery, GraphState, Node,
    NodeId, Partition, QueryOpts, TraversalMode,
};
use filigrio_query::GraphView;
use std::collections::BTreeMap;
use std::sync::Arc;

fn node(id: &str, label: &str, kind: &str) -> Node {
    Node::new(id, label, kind)
}

fn calls(src: &str, dst: &str) -> Edge {
    Edge {
        source: NodeId::new(src),
        relation: "calls".into(),
        confidence: Confidence::Inferred,
        target: EdgeTarget::Node(NodeId::new(dst)),
    }
}

/// A graph from bare node labels, one community per given group.
fn graph_of(nodes: Vec<Node>, edges: Vec<Edge>, communities: &[(&str, &[&str])]) -> GraphState {
    let mut node_community = BTreeMap::new();
    let mut metas = BTreeMap::new();
    for (i, (label, members)) in communities.iter().enumerate() {
        let cid = CommunityId(i as u64);
        for m in *members {
            node_community.insert(NodeId::new(*m), cid);
        }
        metas.insert(
            cid,
            CommunityMeta {
                id: cid,
                label: (*label).into(),
                size: members.len(),
                ..Default::default()
            },
        );
    }
    GraphState {
        graph: Graph { nodes, edges },
        partition: Partition {
            node_community,
            communities: metas,
        },
        ..Default::default()
    }
}

#[test]
fn seeds_fuzzy_match_via_trigrams() {
    // A typo ("clustr") is not a substring of any label, but shares trigrams
    // (clu/lus/ust) with the cluster* nodes — so trigram scoring still finds them.
    let nodes = vec![
        node("n:cluster", "cluster", "function"),
        node("n:clustering", "clustering", "function"),
        node("n:banana", "banana", "function"),
    ];
    let v = GraphView::new(Arc::new(graph_of(nodes, vec![], &[])));

    let scored = v.seed_scores("clustr");
    let labels: Vec<&str> = scored.iter().map(|(n, _)| n.label.as_str()).collect();

    assert!(
        labels.contains(&"cluster"),
        "fuzzy hit expected: {labels:?}"
    );
    assert!(
        labels.contains(&"clustering"),
        "fuzzy hit expected: {labels:?}"
    );
    assert!(
        !labels.contains(&"banana"),
        "no shared trigrams: {labels:?}"
    );
    // exact-length match ranks first (equal trigram idf, tie broken by label)
    assert_eq!(labels[0], "cluster");
    // every returned seed has a positive score
    assert!(scored.iter().all(|(_, s)| *s > 0.0));
}

#[test]
fn seed_scoring_weights_rare_trigrams_above_common() {
    // "aaa" is a common trigram (5 nodes); "qzq" is rare (1 node). A query that
    // shares exactly one of each with two candidate nodes must rank the node
    // sharing the *rare* trigram higher — that is IDF, not raw overlap count.
    let nodes = vec![
        node("n:aaa", "aaa", "function"),
        node("n:aaab", "aaab", "function"),
        node("n:aaac", "aaac", "function"),
        node("n:aaad", "aaad", "function"),
        node("n:aaae", "aaae", "function"),
        node("n:qzq", "qzq", "function"),
    ];
    let v = GraphView::new(Arc::new(graph_of(nodes, vec![], &[])));

    let scored = v.seed_scores("aaaqzq");
    let by_label: BTreeMap<&str, f64> =
        scored.iter().map(|(n, s)| (n.label.as_str(), *s)).collect();

    assert!(
        by_label["qzq"] > by_label["aaa"],
        "rare trigram must outweigh common: qzq={} aaa={}",
        by_label["qzq"],
        by_label["aaa"]
    );
    assert_eq!(scored[0].0.label, "qzq", "rare-trigram node ranks first");
}

#[test]
fn budget_packs_highest_ranked_seed_first() {
    // Two matching seeds in *insertion* order [low, high]; the low-scoring one is
    // inserted first. With budget 1, priority packing must keep the high-scoring
    // seed — proving order comes from the score, not insertion.
    let nodes = vec![
        node("n:register", "register", "function"), // weak match ("ste"/"ter")
        node("n:cluster", "cluster", "function"),   // strong match (all trigrams)
    ];
    let v = GraphView::new(Arc::new(graph_of(nodes, vec![], &[])));

    let sub = v
        .query(
            "cluster",
            QueryOpts {
                depth: 0,
                budget: 1,
                mode: TraversalMode::Bfs,
                context_filter: None,
            },
        )
        .unwrap();
    let labels: Vec<&str> = sub.nodes.iter().map(|n| n.label.as_str()).collect();
    assert_eq!(labels, vec!["cluster"], "budget 1 keeps the top seed only");
}

#[test]
fn results_carry_community_labels() {
    // seed `parse` (calls `read` intra-community and `plan` across). Results are
    // tagged with the *derived* community label — the same top-god-node label
    // the report uses (not the `Community N` placeholder), so query and map agree.
    // Community 0 = {parse, read}: parse has degree 2 (out read + out plan) so it
    // is the top node → label "parse". Community 1 = {plan} → label "plan".
    let nodes = vec![
        node("n:parse", "parse", "function"),
        node("n:read", "read", "function"),
        node("n:plan", "plan", "function"),
    ];
    let edges = vec![calls("n:parse", "n:read"), calls("n:parse", "n:plan")];
    let state = graph_of(
        nodes,
        edges,
        &[
            ("Community 0", &["n:parse", "n:read"]),
            ("Community 1", &["n:plan"]),
        ],
    );
    let v = GraphView::new(Arc::new(state));

    let sub = v
        .query(
            "parse",
            QueryOpts {
                depth: 2,
                budget: 10,
                mode: TraversalMode::Bfs,
                context_filter: None,
            },
        )
        .unwrap();

    let lbl = |id: &str| sub.communities.get(&NodeId::new(id)).map(String::as_str);
    assert_eq!(lbl("n:parse"), Some("parse"));
    assert_eq!(lbl("n:read"), Some("parse"));
    assert_eq!(lbl("n:plan"), Some("plan"));
}

/// The label-selection rule, pinned: **highest degree, ties broken by label
/// ascending** — and structural edges do not count toward it.
///
/// This is the rule the view derives once per *community*; every node in the
/// community reads it back through `community_of`, and `community_summaries`
/// reads the same map. The tie-break is the half a "pick the max"
/// implementation gets wrong silently, so it is asserted on a fixture built to
/// be a tie. Which *degree* feeds the rule is the other half, pinned separately
/// in `tests/report.rs` — it is **not**
/// `GraphView::semantic_degree` (see that function's doc for why).
#[test]
fn community_label_is_top_degree_ties_broken_by_label() {
    // `zeta` and `alpha` both have degree 1; `omega` has 0 but is the target of
    // a *structural* `contains`, which must not lift it.
    let nodes = vec![
        node("n:zeta", "zeta", "function"),
        node("n:alpha", "alpha", "function"),
        node("n:omega", "omega", "function"),
    ];
    let edges = vec![
        calls("n:zeta", "n:alpha"),
        Edge {
            source: NodeId::new("n:omega"),
            relation: "contains".into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Node(NodeId::new("n:zeta")),
        },
    ];
    let state = graph_of(
        nodes,
        edges,
        &[("Community 0", &["n:zeta", "n:alpha", "n:omega"])],
    );
    let v = GraphView::new(Arc::new(state));

    for id in ["n:zeta", "n:alpha", "n:omega"] {
        assert_eq!(
            v.community_of(&NodeId::new(id)).map(String::as_str),
            Some("alpha"),
            "{id}: degree 1 ties between zeta and alpha → alphabetically first wins, \
             and omega's structural edge must not count"
        );
    }
}

/// A node the partition does not place has **no** label — not a default, not a
/// placeholder. The per-node map simply had no entry for it; the per-community
/// lookup must be just as absent.
#[test]
fn unpartitioned_nodes_have_no_community_label() {
    let nodes = vec![
        node("n:in", "in", "function"),
        node("n:out", "out", "function"),
    ];
    let state = graph_of(nodes, vec![calls("n:in", "n:out")], &[("C0", &["n:in"])]);
    let v = GraphView::new(Arc::new(state));

    assert_eq!(
        v.community_of(&NodeId::new("n:in")).map(String::as_str),
        Some("in")
    );
    assert_eq!(v.community_of(&NodeId::new("n:out")), None);

    // …and the absence carries through to the attr stamp and the sidecar.
    let n = v.node_by_id("n:out").unwrap().expect("node");
    assert!(
        !n.attrs.contains_key("community"),
        "no community → no stamped attr, not an empty one: {:?}",
        n.attrs
    );
    let sub = v
        .query(
            "out",
            QueryOpts {
                depth: 1,
                budget: 10,
                mode: TraversalMode::Bfs,
                context_filter: None,
            },
        )
        .unwrap();
    assert!(sub.nodes.iter().any(|n| n.id == NodeId::new("n:out")));
    assert_eq!(sub.communities.get(&NodeId::new("n:out")), None);
}
