//! TDD spec for the analysis / report surface (Phase 2b, slice 2).
//!
//! The clustering assigns nodes to communities but only a placeholder label
//! (`Community N`). The *human-facing map* needs three derived things, all
//! deterministic:
//!
//!   * **community labels** — each community named by its highest-degree member
//!     (its "god node"), tie-broken by label; this replaces `Community N`. The
//!     report does not derive these: it reads the map `GraphView` already built
//!     for the query surface, so the two can never disagree.
//!   * **cross-community bridges** — the semantic edges whose endpoints fall in
//!     *different* communities; these are the seams a reader cares about.
//!   * a rendered **`GRAPH_REPORT.md`** composing stats, god nodes, communities
//!     (with their real labels + members) and bridges.
//!
//! Fixture — two dense call-clusters joined by a single bridge:
//!
//!   community 0 (A):  a1 ─calls→ a2 ←calls─ a3        (a2 is the hub)
//!   community 1 (B):  b2 ─calls→ b1 ←calls─ b3        (b1 is the hub)
//!   bridge:           a3 ─calls→ b1                    (the only cross edge)
//!
//! Semantic degrees: b1=3 (in b2,b3,a3), a2=2, a3=2 (out a2 + bridge), a1=1,
//! b2=1, b3=1. So overall god node is b1; community A's label is `a2` (ties a3
//! at degree 2, broken by label), community B's is `b1`.

use filigrio_core::{
    CommunityId, CommunityMeta, Confidence, Edge, EdgeTarget, Graph, GraphState, Node, NodeId,
    Partition,
};
use filigrio_query::{render_markdown, GraphView};
use std::collections::BTreeMap;
use std::sync::Arc;

fn fnode(name: &str) -> Node {
    let mut n = Node::new(format!("fn:{name}"), name, "function");
    n.source_file = Some(format!("src/{}.rs", &name[..1]));
    n
}

fn calls(src: &str, dst: &str) -> Edge {
    Edge {
        source: NodeId::new(format!("fn:{src}")),
        relation: "calls".into(),
        confidence: Confidence::Inferred,
        target: EdgeTarget::Node(NodeId::new(format!("fn:{dst}"))),
    }
}

fn fixture() -> GraphState {
    let nodes = vec![
        fnode("a1"),
        fnode("a2"),
        fnode("a3"),
        fnode("b1"),
        fnode("b2"),
        fnode("b3"),
    ];
    let edges = vec![
        calls("a1", "a2"),
        calls("a3", "a2"),
        calls("b2", "b1"),
        calls("b3", "b1"),
        calls("a3", "b1"), // the bridge
    ];

    let mut node_community = BTreeMap::new();
    for n in ["a1", "a2", "a3"] {
        node_community.insert(NodeId::new(format!("fn:{n}")), CommunityId(0));
    }
    for n in ["b1", "b2", "b3"] {
        node_community.insert(NodeId::new(format!("fn:{n}")), CommunityId(1));
    }
    let mut communities = BTreeMap::new();
    communities.insert(
        CommunityId(0),
        CommunityMeta {
            id: CommunityId(0),
            label: "Community 0".into(),
            size: 3,
            ..Default::default()
        },
    );
    communities.insert(
        CommunityId(1),
        CommunityMeta {
            id: CommunityId(1),
            label: "Community 1".into(),
            size: 3,
            ..Default::default()
        },
    );

    GraphState {
        graph: Graph { nodes, edges },
        partition: Partition {
            node_community,
            communities,
        },
        ..Default::default()
    }
}

#[test]
fn community_labelled_by_top_semantic_node() {
    let v = GraphView::new(Arc::new(fixture()));
    let summaries = v.community_summaries();

    let by_id: BTreeMap<CommunityId, &_> = summaries.iter().map(|c| (c.id, c)).collect();
    // A's hub a2 (deg 2, beats a3 on the label tie); B's hub b1 (deg 3).
    assert_eq!(by_id[&CommunityId(0)].label, "a2");
    assert_eq!(by_id[&CommunityId(1)].label, "b1");
    // sizes + membership are reported
    assert_eq!(by_id[&CommunityId(0)].size, 3);
    let a_members: Vec<&str> = by_id[&CommunityId(0)]
        .members
        .iter()
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(a_members, vec!["a1", "a2", "a3"]);
}

/// The report's label and the query surface's label are the **same value**, and
/// unresolved out-edges count toward it.
///
/// These were two independent derivations of "highest-degree member, ties by
/// label" over two *different* degrees — the report's walked the petgraph
/// (resolved edges only), the query's walked the edge list (an unresolved
/// `Symbol` target still counts for its source). Every all-resolved fixture
/// hides that, which is how it survived; this one is built so the two answers
/// differ, and pins both halves of the fix:
///
///   * `report == query` — one implementation, not two that agree by luck;
///   * the surviving degree is the **edge-list** one. `worker`'s three calls all
///     leave the graph, so resolved-only would put it at degree 0 alongside
///     `alpha`, collapsing the label to the tie-break — "alphabetically first
///     member", which is not a name. Measured over three real corpora that
///     state doubles (this repo 33→54 of 338 communities, ironclaw 768→1567,
///     langchain 554→1614), so the collapse is the common case, not a corner.
#[test]
fn report_and_query_labels_agree_and_count_unresolved_edges() {
    let nodes = vec![fnode("alpha"), fnode("worker")];
    // Every one of `worker`'s callees is unbound (std, an external crate…).
    let edges: Vec<Edge> = ["println", "collect", "spawn"]
        .into_iter()
        .map(|callee| Edge {
            source: NodeId::new("fn:worker"),
            relation: "calls".into(),
            confidence: Confidence::Inferred,
            target: EdgeTarget::Symbol(filigrio_core::TargetRef::new(callee)),
        })
        .collect();

    let mut node_community = BTreeMap::new();
    for n in ["alpha", "worker"] {
        node_community.insert(NodeId::new(format!("fn:{n}")), CommunityId(0));
    }
    let mut communities = BTreeMap::new();
    communities.insert(
        CommunityId(0),
        CommunityMeta {
            id: CommunityId(0),
            label: "Community 0".into(),
            size: 2,
            ..Default::default()
        },
    );
    let state = GraphState {
        graph: Graph { nodes, edges },
        partition: Partition {
            node_community,
            communities,
        },
        ..Default::default()
    };

    let v = GraphView::new(Arc::new(state));
    let summaries = v.community_summaries();
    assert_eq!(summaries.len(), 1);
    assert_eq!(
        summaries[0].label, "worker",
        "the label must count `worker`'s unresolved calls; resolved-only degree \
         leaves both members at 0 and picks `alpha` off the tie-break alone"
    );
    for id in ["fn:alpha", "fn:worker"] {
        assert_eq!(
            v.community_of(&NodeId::new(id)).map(String::as_str),
            Some(summaries[0].label.as_str()),
            "{id}: GRAPH_REPORT.md and the MCP `community=` attr must be the \
             same derivation, not two that happen to agree"
        );
    }
}

#[test]
fn bridges_are_cross_community_edges_only() {
    let v = GraphView::new(Arc::new(fixture()));
    let bridges = v.bridges();
    // exactly one: a3 → b1, from community 0 to community 1.
    assert_eq!(
        bridges.len(),
        1,
        "only a3→b1 crosses communities: {bridges:?}"
    );
    let b = &bridges[0];
    assert_eq!(b.from, CommunityId(0));
    assert_eq!(b.to, CommunityId(1));
    assert_eq!(b.edge.source, NodeId::new("fn:a3"));
    assert!(matches!(&b.edge.target, EdgeTarget::Node(t) if t == &NodeId::new("fn:b1")));
}

/// A graph with a `Workspace`: two projects, `@acme/app` → `@acme/ui` (+ an
/// external `react` that must NOT become a project edge), one file each.
fn workspace_fixture() -> GraphState {
    use filigrio_core::{Project, Workspace};
    let mut projects = BTreeMap::new();
    projects.insert(
        "app".to_string(),
        Project {
            root: "app".into(),
            manifest: "package.json".into(),
            name: Some("@acme/app".into()),
            entry: None,
            deps: vec!["@acme/ui".into(), "react".into()],
        },
    );
    projects.insert(
        "ui".to_string(),
        Project {
            root: "ui".into(),
            manifest: "package.json".into(),
            name: Some("@acme/ui".into()),
            entry: None,
            deps: vec![],
        },
    );
    let mut app_file = Node::new("file:app/x.ts", "app/x.ts", "file");
    app_file.source_file = Some("app/x.ts".into());
    let mut ui_file = Node::new("file:ui/y.ts", "ui/y.ts", "file");
    ui_file.source_file = Some("ui/y.ts".into());
    GraphState {
        graph: Graph {
            nodes: vec![app_file, ui_file],
            edges: vec![],
        },
        workspace: Workspace { projects },
        ..Default::default()
    }
}

#[test]
fn report_has_projects_section() {
    let v = GraphView::new(Arc::new(workspace_fixture()));
    let md = render_markdown(&v.report(10).unwrap());
    assert!(md.contains("## Projects"), "projects section present: {md}");
    assert!(md.contains("@acme/app"), "project listed by name: {md}");
    assert!(
        md.contains("@acme/ui"),
        "workspace-internal dependency listed: {md}"
    );
    assert!(
        !md.contains("react"),
        "external dependency is not a project edge: {md}"
    );
    // The project with no internal deps renders a placeholder, not a blank cell.
    assert!(md.contains(" — |"), "empty-deps placeholder: {md}");
}

#[test]
fn report_projects_section_is_none_without_manifests() {
    // The original fixture has no `Workspace`; the section is present but empty.
    // (Its bridges are non-empty, so a `_none_` marker uniquely flags Projects.)
    let v = GraphView::new(Arc::new(fixture()));
    let md = render_markdown(&v.report(10).unwrap());
    assert!(md.contains("## Projects"), "section always present: {md}");
    assert!(
        md.contains("_none_"),
        "empty projects render `_none_`: {md}"
    );
}

#[test]
fn report_is_deterministic() {
    let v = GraphView::new(Arc::new(fixture()));
    assert_eq!(
        render_markdown(&v.report(10).unwrap()),
        render_markdown(&v.report(10).unwrap()),
        "the same graph must render byte-identically"
    );
}

#[test]
fn report_markdown_has_the_sections() {
    let v = GraphView::new(Arc::new(fixture()));
    let md = render_markdown(&v.report(10).unwrap());

    // headline counts
    assert!(md.contains("# Graph Report"), "{md}");
    assert!(md.contains("6 nodes"), "node count in summary: {md}");

    // god nodes — b1 is the overall top (semantic degree 3)
    assert!(md.contains("## God nodes"), "{md}");
    assert!(md.contains("b1"), "top god node listed: {md}");

    // communities carry their derived labels, not the `Community N` placeholder
    assert!(md.contains("## Communities"), "{md}");
    assert!(md.contains("a2"), "community A label: {md}");
    assert!(
        !md.contains("Community 0"),
        "placeholder must be replaced: {md}"
    );

    // confidence breakdown (all 5 edges are INFERRED here)
    assert!(md.contains("## Confidence"), "{md}");
    assert!(md.contains("Inferred"), "confidence tally: {md}");

    // the bridge section names the crossing, by community label
    assert!(md.contains("## Cross-community bridges"), "{md}");
    assert!(
        md.contains("a3") && md.contains("b1"),
        "bridge endpoints: {md}"
    );
}
