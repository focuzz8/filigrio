//! TDD spec for the MCP tool surface (Phase 3) — the 7 core tools rendered as
//! filigrio-`serve.py`-equivalent, sanitized, token-bounded text.
//!
//! Two styles: a **fake `GraphQuery`** that records the `QueryOpts` it is handed
//! (to assert arg parsing + hot-reload dispatch), and a **real `GraphView`** over
//! a hand-built graph (to assert the actual rendered text of each tool).

use filigrio_client_mcp::McpServer;
use filigrio_core::{
    CommunityId, Confidence, Edge, EdgeTarget, Graph, GraphQuery, GraphState, GraphStats, Node,
    NodeId, Partition, QueryOpts, Span, Subgraph, TargetRef, TraversalMode,
};
use filigrio_query::GraphView;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

// ---- a recording fake, for arg-parsing + dispatch --------------------------

#[derive(Default)]
struct FakeQuery {
    last_opts: RefCell<Option<QueryOpts>>,
    sub: Subgraph,
}

impl GraphQuery for FakeQuery {
    fn query(&self, _q: &str, opts: QueryOpts) -> filigrio_core::Result<Subgraph> {
        *self.last_opts.borrow_mut() = Some(opts);
        Ok(self.sub.clone())
    }
    fn nodes_by_label(&self, _l: &str) -> filigrio_core::Result<Vec<Node>> {
        Ok(Vec::new())
    }
    fn node_by_id(&self, _id: &str) -> filigrio_core::Result<Option<Node>> {
        Ok(None)
    }
    fn neighbors_by_id(
        &self,
        _id: &str,
        _r: &[String],
        _d: filigrio_core::Direction,
    ) -> filigrio_core::Result<Vec<(Edge, Node)>> {
        Ok(Vec::new())
    }
    fn unresolved_out_by_id(&self, _id: &str) -> filigrio_core::Result<Vec<Edge>> {
        Ok(Vec::new())
    }
    fn unresolved_in_by_label(&self, _l: &str) -> filigrio_core::Result<Vec<(Edge, Node)>> {
        Ok(Vec::new())
    }
    fn community(&self, _id: CommunityId) -> filigrio_core::Result<Vec<Node>> {
        Ok(Vec::new())
    }
    fn community_meta(
        &self,
        _id: CommunityId,
    ) -> filigrio_core::Result<Option<filigrio_core::CommunityMeta>> {
        Ok(None)
    }
    fn god_nodes(&self, _n: usize) -> filigrio_core::Result<Vec<(Node, usize)>> {
        Ok(Vec::new())
    }
    fn shortest_path(
        &self,
        _s: &str,
        _d: &str,
        _h: usize,
    ) -> filigrio_core::Result<Option<Vec<Node>>> {
        Ok(None)
    }
    fn stats(&self) -> filigrio_core::Result<GraphStats> {
        Ok(GraphStats::default())
    }
    fn project_graph(&self) -> filigrio_core::Result<filigrio_core::ProjectGraph> {
        Ok(filigrio_core::ProjectGraph::default())
    }
}

fn call<Q: GraphQuery>(server: &McpServer<Q>, tool: &str, args: Value) -> String {
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": tool, "arguments": args}
    });
    server.handle(&req)["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[test]
fn query_graph_parses_mode_and_relations() {
    let server = McpServer::new(FakeQuery::default());
    let _ = call(
        &server,
        "query_graph",
        json!({"q": "x", "mode": "dfs", "relations": ["calls", "imports"]}),
    );
    let opts = server.query_ref().last_opts.borrow().clone().unwrap();
    assert_eq!(opts.mode, TraversalMode::Dfs);
    assert_eq!(
        opts.context_filter,
        Some(vec!["calls".to_string(), "imports".to_string()])
    );
}

#[test]
fn project_path_routes_to_the_reloaded_view() {
    // Default view is empty; the reloader returns a view whose god_nodes has a
    // distinctive label. A call carrying project_path must hit the reloaded one.
    let reloader = Box::new(|path: &str| {
        if path == "/proj" {
            Ok(single_god_view("reloaded_hub"))
        } else {
            Err(format!("no project at {path}"))
        }
    });
    let server = McpServer::with_reloader(single_god_view("default_hub"), reloader);

    let default = call(&server, "god_nodes", json!({}));
    assert!(default.contains("default_hub"), "{default}");

    let reloaded = call(&server, "god_nodes", json!({"project_path": "/proj"}));
    assert!(reloaded.contains("reloaded_hub"), "{reloaded}");
}

// ---- real GraphView, for rendered-text fidelity ----------------------------

fn fnode(id: &str, label: &str, kind: &str, file: &str, loc: &str) -> Node {
    let mut n = Node::new(id, label, kind);
    n.source_file = Some(file.into());
    n.source_span = Span::parse(loc);
    n
}

fn calls(src: &str, dst: &str, conf: Confidence) -> Edge {
    Edge {
        source: NodeId::new(src),
        relation: "calls".into(),
        confidence: conf,
        target: EdgeTarget::Node(NodeId::new(dst)),
    }
}

/// parse ─calls→ read (both community 0), parse ─calls→ plan (community 1).
fn view() -> GraphView {
    let nodes = vec![
        fnode("n:parse", "parse", "function", "io.rs", "L1"),
        fnode("n:read", "read", "function", "io.rs", "L9"),
        fnode("n:plan", "plan", "function", "core.rs", "L4"),
    ];
    let edges = vec![
        calls("n:parse", "n:read", Confidence::Extracted),
        calls("n:parse", "n:plan", Confidence::Inferred),
    ];
    let mut node_community = BTreeMap::new();
    node_community.insert(NodeId::new("n:parse"), CommunityId(0));
    node_community.insert(NodeId::new("n:read"), CommunityId(0));
    node_community.insert(NodeId::new("n:plan"), CommunityId(1));
    GraphView::new(Arc::new(GraphState {
        graph: Graph { nodes, edges },
        partition: Partition {
            node_community,
            communities: BTreeMap::new(),
        },
        ..Default::default()
    }))
}

fn single_god_view(hub_label: &str) -> GraphView {
    // hub <-calls- caller, so hub has semantic degree ≥ 1 and tops god_nodes.
    let nodes = vec![
        fnode("n:hub", hub_label, "function", "h.rs", "L1"),
        fnode("n:caller", "caller", "function", "h.rs", "L2"),
    ];
    let edges = vec![calls("n:caller", "n:hub", Confidence::Extracted)];
    GraphView::new(Arc::new(GraphState {
        graph: Graph { nodes, edges },
        partition: Partition::default(),
        ..Default::default()
    }))
}

/// A view with a `Workspace` (2 projects, app → ui) + one file node each.
fn workspace_view() -> GraphView {
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
    let nodes = vec![
        fnode("file:app/x.ts", "app/x.ts", "file", "app/x.ts", "L1"),
        fnode("file:ui/y.ts", "ui/y.ts", "file", "ui/y.ts", "L1"),
    ];
    GraphView::new(Arc::new(GraphState {
        graph: Graph {
            nodes,
            edges: vec![],
        },
        workspace: Workspace { projects },
        ..Default::default()
    }))
}

#[test]
fn project_graph_renders_the_monorepo_map() {
    let server = McpServer::new(workspace_view());
    let text = call(&server, "project_graph", json!({}));
    assert!(text.starts_with("Projects: 2"), "header: {text}");
    assert!(
        text.contains("PROJECT @acme/app [root=app files=1] → @acme/ui"),
        "app depends on ui by name (external react excluded): {text}"
    );
    assert!(
        text.contains("PROJECT @acme/ui [root=ui files=1]"),
        "ui line: {text}"
    );
    assert!(
        !text.contains("react"),
        "external dep is not a project edge: {text}"
    );
}

#[test]
fn project_graph_empty_when_no_manifests() {
    let server = McpServer::new(view());
    let text = call(&server, "project_graph", json!({}));
    assert!(text.contains("No projects"), "{text}");
}

#[test]
fn query_graph_renders_node_and_edge_lines() {
    let server = McpServer::new(view());
    let text = call(&server, "query_graph", json!({"q": "parse", "depth": 2}));

    assert!(text.starts_with("Traversal: BFS depth=2"), "header: {text}");
    assert!(text.contains("3 nodes found"), "count: {text}");
    // NODE line format with src/loc/community + the copy-able id (ADR-0027 coherence)
    assert!(
        text.contains("NODE parse [src=io.rs loc=L1 community=parse id=n:parse]"),
        "node line: {text}"
    );
    assert!(
        text.contains("NODE plan [src=core.rs loc=L4 community=plan id=n:plan]"),
        "cross-community node: {text}"
    );
    // EDGE line with UPPERCASE confidence and both endpoints present
    assert!(
        text.contains("EDGE parse --calls [EXTRACTED]--> read"),
        "edge line: {text}"
    );
    assert!(
        text.contains("EDGE parse --calls [INFERRED]--> plan"),
        "inferred edge: {text}"
    );
}

#[test]
fn query_graph_truncates_to_token_budget() {
    let server = McpServer::new(view());
    // token_budget 1 → char_budget 3: the body must be cut and marked.
    let text = call(
        &server,
        "query_graph",
        json!({"q": "parse", "token_budget": 1}),
    );
    assert!(text.contains("truncated"), "must mark truncation: {text}");
    assert!(text.contains("token budget"), "names the budget: {text}");
    // header is never budgeted, so it survives in full
    assert!(text.starts_with("Traversal: BFS"), "header intact: {text}");
}

#[test]
fn get_node_reports_identity_and_community() {
    let server = McpServer::new(view());
    let text = call(&server, "get_node", json!({"label": "plan"}));
    // The header carries the shared handle: src/loc + the copy-able id, no separate
    // ID line (it's in the suffix now), identical across every tool.
    assert!(
        text.contains("Node: plan [src=core.rs loc=L4 id=n:plan]"),
        "{text}"
    );
    assert!(text.contains("Community: plan"), "{text}");

    let missing = call(&server, "get_node", json!({"label": "nope"}));
    assert!(missing.contains("No node matching 'nope'"), "{missing}");
}

/// Two distinct `from` methods on different types/files (the reflex `X::from` vs
/// `error::from` homonym the agent-eval hit), each with its own neighbor.
fn homonym_view() -> GraphView {
    let mut a = fnode("fn:a.rs:from", "from", "function", "a.rs", "L1");
    a.attrs.insert("impl".into(), "Alpha".into());
    let mut b = fnode("fn:b.rs:from", "from", "function", "b.rs", "L2");
    b.attrs.insert("impl".into(), "Beta".into());
    let nodes = vec![
        a,
        b,
        fnode("fn:a.rs:xa", "xa", "function", "a.rs", "L9"),
        fnode("fn:b.rs:xb", "xb", "function", "b.rs", "L9"),
    ];
    let edges = vec![
        calls("fn:a.rs:from", "fn:a.rs:xa", Confidence::Extracted),
        calls("fn:b.rs:from", "fn:b.rs:xb", Confidence::Extracted),
    ];
    GraphView::new(Arc::new(GraphState {
        graph: Graph { nodes, edges },
        partition: Partition::default(),
        ..Default::default()
    }))
}

#[test]
fn get_node_disambiguates_homonyms_and_addresses_by_src_or_id() {
    let server = McpServer::new(homonym_view());
    // Bare ambiguous label → a disambiguation list naming BOTH nodes (id + src),
    // not an arbitrary silent pick.
    let ambiguous = call(&server, "get_node", json!({"label": "from"}));
    assert!(
        ambiguous.contains("2 nodes"),
        "lists the count: {ambiguous}"
    );
    assert!(
        ambiguous.contains("fn:a.rs:from") && ambiguous.contains("fn:b.rs:from"),
        "lists both candidate ids: {ambiguous}"
    );
    assert!(
        ambiguous.contains("src="),
        "offers the src= lever: {ambiguous}"
    );

    // `src` picks exactly one.
    let by_src = call(&server, "get_node", json!({"label": "from", "src": "b.rs"}));
    assert!(by_src.contains("Node: from"), "{by_src}");
    assert!(
        by_src.contains("id=fn:b.rs:from"),
        "src picks Beta's from: {by_src}"
    );
    assert!(
        !by_src.contains("2 nodes"),
        "resolved, not a list: {by_src}"
    );

    // `id` is exact.
    let by_id = call(&server, "get_node", json!({"id": "fn:a.rs:from"}));
    assert!(by_id.contains("id=fn:a.rs:from"), "id is exact: {by_id}");
}

#[test]
fn get_neighbors_addresses_the_specific_homonym() {
    let server = McpServer::new(homonym_view());
    // Bare label is ambiguous → the same disambiguation list (never guess a node).
    let ambiguous = call(&server, "get_neighbors", json!({"label": "from"}));
    assert!(
        ambiguous.contains("2 nodes"),
        "ambiguous → list: {ambiguous}"
    );

    // `src` addresses the right one: Alpha::from calls xa, Beta::from calls xb.
    let a = call(
        &server,
        "get_neighbors",
        json!({"label": "from", "src": "a.rs"}),
    );
    assert!(
        a.contains("xa") && !a.contains("xb"),
        "a.rs from → xa only: {a}"
    );
    let b = call(&server, "get_neighbors", json!({"id": "fn:b.rs:from"}));
    assert!(
        b.contains("xb") && !b.contains("xa"),
        "b.rs from → xb only: {b}"
    );
}

#[test]
fn discovery_lines_show_impl_owner_for_a_readable_handle() {
    // The `impl` owner is shown consistently in god_nodes and get_node, so the
    // model has a readable name (X::from) alongside the copy-able src= address.
    let server = McpServer::new(homonym_view());
    let god = call(&server, "god_nodes", json!({"top_n": 2}));
    assert!(
        god.contains("from (impl Alpha)") || god.contains("from (impl Beta)"),
        "god_nodes shows the impl owner: {god}"
    );
    let node = call(&server, "get_node", json!({"id": "fn:a.rs:from"}));
    assert!(
        node.contains("(impl Alpha)"),
        "get_node shows the impl owner: {node}"
    );
}

/// `file --contains--> foo` (structural) and `caller --calls--> foo` (semantic):
/// foo's incoming edges mix the two.
fn contains_and_calls_view() -> GraphView {
    let nodes = vec![
        fnode("file:m.rs", "m.rs", "file", "m.rs", "L1"),
        fnode("fn:foo", "foo", "function", "m.rs", "L5"),
        fnode("fn:caller", "caller", "function", "m.rs", "L9"),
    ];
    let edges = vec![
        Edge {
            source: NodeId::new("file:m.rs"),
            relation: "contains".into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Node(NodeId::new("fn:foo")),
        },
        calls("fn:caller", "fn:foo", Confidence::Extracted),
    ];
    GraphView::new(Arc::new(GraphState {
        graph: Graph { nodes, edges },
        partition: Partition::default(),
        ..Default::default()
    }))
}

#[test]
fn get_neighbors_excludes_structural_edges_by_default() {
    // "What calls foo" (direction in, no relation) must show the caller, not the
    // file's structural `contains` edge — that noise confused the hub run (ADR-0027).
    let server = McpServer::new(contains_and_calls_view());
    let inn = call(
        &server,
        "get_neighbors",
        json!({"id": "fn:foo", "direction": "in"}),
    );
    // The caller line carries the arrow, the node's self-addressing handle, then
    // the edge relation (ADR-0027: a neighbor is addressable, like god_nodes).
    assert!(inn.contains("<-- caller"), "shows the caller: {inn}");
    assert!(
        inn.contains("src=m.rs loc=L9 id=fn:caller") && inn.contains("[calls]"),
        "caller line is self-addressing (src/loc/id) + relation-tagged: {inn}"
    );
    assert!(
        !inn.contains("contains"),
        "structural contains excluded by default: {inn}"
    );

    // But an explicit `relation` is always honored — structure on request.
    let structural = call(
        &server,
        "get_neighbors",
        json!({"id": "fn:foo", "direction": "in", "relation": "contains"}),
    );
    assert!(
        structural.contains("<-- m.rs") && structural.contains("[contains]"),
        "explicit relation=contains is honored: {structural}"
    );
}

#[test]
fn get_neighbors_supports_direction_in_out_both() {
    // `view()`: parse --calls--> read (and --> plan). So read's *incoming* neighbor
    // is parse (its caller) — the "what calls it" a hub question needs (ADR-0027).
    let server = McpServer::new(view());

    let incoming = call(
        &server,
        "get_neighbors",
        json!({"id": "n:read", "direction": "in"}),
    );
    assert!(
        incoming.contains("<-- parse"),
        "incoming shows the caller: {incoming}"
    );

    let outgoing = call(
        &server,
        "get_neighbors",
        json!({"id": "n:read", "direction": "out"}),
    );
    assert!(
        !outgoing.contains("parse"),
        "read has no outgoing edge: {outgoing}"
    );

    let out_parse = call(
        &server,
        "get_neighbors",
        json!({"id": "n:parse", "direction": "out"}),
    );
    assert!(
        out_parse.contains("--> read") && out_parse.contains("--> plan"),
        "parse's callees: {out_parse}"
    );

    // Default is `both`: an agent that just asks for a hub's neighbors sees its
    // callers without having to know to request them.
    let both = call(&server, "get_neighbors", json!({"id": "n:read"}));
    assert!(
        both.contains("<-- parse"),
        "default both includes callers: {both}"
    );
}

// ---- unresolved-edge status at get_neighbors (ADR-0029) ---------------------

/// An opaque, declined call edge: `src` calls the bare name `name` (recv opaque).
fn sym_call(src: &str, name: &str) -> Edge {
    let mut tref = TargetRef::new(name);
    tref.hints.insert("recv".into(), "opaque".into());
    Edge {
        source: NodeId::new(src),
        relation: "calls".into(),
        confidence: Confidence::Extracted,
        target: EdgeTarget::Symbol(tref),
    }
}

/// A hub `with_version` (no resolved neighbors) with two opaque by-name callers,
/// `run_pipeline` and `build`; `run_pipeline` also declines an outgoing call to an
/// unknown external `moondream2`. The invisible-edge seam ADR-0029 fixes.
fn unresolved_view() -> GraphView {
    let mut wv = fnode("n:wv", "with_version", "function", "cfg.rs", "L5");
    wv.attrs.insert("impl".into(), "ConfigBuilder".into());
    let nodes = vec![
        wv,
        fnode("n:rp", "run_pipeline", "function", "engine.rs", "L88-L94"),
        fnode("n:bd", "build", "function", "engine.rs", "L20"),
    ];
    let edges = vec![
        sym_call("n:rp", "with_version"),
        sym_call("n:bd", "with_version"),
        sym_call("n:rp", "moondream2"),
    ];
    GraphView::new(Arc::new(GraphState {
        graph: Graph { nodes, edges },
        partition: Partition::default(),
        ..Default::default()
    }))
}

#[test]
fn get_neighbors_reports_unresolved_counts_by_default() {
    // The seam: with_version has 0 *resolved* neighbors but 2 opaque callers. The
    // old surface returned an empty list — a lie ("nothing calls this"). Now the
    // count is always surfaced so the model sees the method is called ~2×.
    let server = McpServer::new(unresolved_view());
    let inn = call(
        &server,
        "get_neighbors",
        json!({"id": "n:wv", "direction": "in"}),
    );
    assert!(
        inn.contains("2") && inn.contains("by-name") && inn.contains("unresolved"),
        "in-count surfaced even with 0 resolved: {inn}"
    );
    assert!(
        inn.contains("include_unresolved"),
        "points at the opt-in expansion: {inn}"
    );
    // The count is NOT the list — bare caller labels are not spelled out yet.
    assert!(
        !inn.contains("<-- run_pipeline"),
        "counts-by-default does not list them: {inn}"
    );
}

#[test]
fn get_neighbors_lists_unresolved_on_opt_in() {
    let server = McpServer::new(unresolved_view());

    // Incoming: the caller IS addressable — show it with its id handle, marked
    // by-name (ADR-0029 open-q1 decision), so the model can read it to confirm.
    let inn = call(
        &server,
        "get_neighbors",
        json!({"id": "n:wv", "direction": "in", "include_unresolved": true}),
    );
    assert!(
        inn.contains("<-- run_pipeline") && inn.contains("id=n:rp"),
        "incoming caller is addressable: {inn}"
    );
    assert!(
        inn.contains("<-- build"),
        "both by-name callers listed: {inn}"
    );
    assert!(
        inn.contains("UNRESOLVED") && inn.contains("by-name"),
        "marked unresolved + by-name (may over-match homonyms): {inn}"
    );

    // Outgoing: the callee is a *bare name*, not an addressable node — no id= for it.
    let out = call(
        &server,
        "get_neighbors",
        json!({"id": "n:rp", "direction": "out", "include_unresolved": true}),
    );
    assert!(
        out.contains("--> with_version") && out.contains("--> moondream2"),
        "declined outgoing calls listed by callee name: {out}"
    );
    assert!(out.contains("UNRESOLVED"), "marked unresolved: {out}");
    // The bare-name callee lines carry no id= handle (there is no node to address).
    for line in out.lines().filter(|l| l.contains("-->")) {
        assert!(
            !line.contains("id="),
            "bare unresolved callee is not addressable: {line}"
        );
    }
}

#[test]
fn get_community_lists_members_under_derived_header() {
    let server = McpServer::new(view());
    let text = call(&server, "get_community", json!({"community_id": 0}));
    // community 0 = {parse, read}; derived label "parse"
    assert!(text.contains("Community 0 — parse"), "header: {text}");
    assert!(text.contains("(2 nodes)"), "size: {text}");
    // Members carry the same self-addressing handle as every other listing.
    assert!(
        text.contains("parse [src=io.rs loc=L1 id=n:parse]")
            && text.contains("read [src=io.rs loc=L9 id=n:read]"),
        "members: {text}"
    );

    let missing = call(&server, "get_community", json!({"community_id": 9}));
    assert!(missing.contains("Community 9 not found"), "{missing}");
}

#[test]
fn graph_stats_reports_confidence_percentages() {
    let server = McpServer::new(view());
    let text = call(&server, "graph_stats", json!({}));
    assert!(text.contains("Nodes: 3"), "{text}");
    assert!(text.contains("Edges: 2"), "{text}");
    // one EXTRACTED + one INFERRED of two edges → 50% each
    assert!(text.contains("EXTRACTED: 50%"), "{text}");
    assert!(text.contains("INFERRED: 50%"), "{text}");
}

#[test]
fn god_nodes_cite_source_location_and_degree() {
    // ADR-0025 eval finding: a bare hub label made the agent re-`get_node` to locate
    // it. god_nodes now carries src/loc like every other tool.
    let server = McpServer::new(view());
    let text = call(&server, "god_nodes", json!({"top_n": 1}));
    // parse has the highest semantic degree (2) in `view()`.
    assert!(
        text.contains("1. parse [src=io.rs loc=L1 id=n:parse] - 2 edges"),
        "god node cites src/loc + copy-able id + degree: {text}"
    );
}

#[test]
fn shortest_path_shows_relations() {
    let server = McpServer::new(view());
    // Endpoints addressed by `from`/`to` (a label here; id also accepted) — `src`
    // is reserved for a source *file*, not a path endpoint (ADR-0027 consistency).
    let text = call(
        &server,
        "shortest_path",
        json!({"from": "parse", "to": "plan"}),
    );
    assert!(text.starts_with("Shortest path (1 hops):"), "{text}");
    assert!(text.contains("parse --calls [INFERRED]--> plan"), "{text}");
}

#[test]
fn shortest_path_disambiguates_a_homonym_endpoint() {
    // A homonym endpoint gets the same treatment as get_node/get_neighbors: an
    // ambiguous label yields the candidate list (re-address by id), never a silent
    // first-match — and an exact id addresses the intended node (ADR-0027).
    let server = McpServer::new(homonym_view());
    let ambiguous = call(
        &server,
        "shortest_path",
        json!({"from": "from", "to": "xa"}),
    );
    assert!(
        ambiguous.contains("2 nodes") && ambiguous.contains("fn:a.rs:from"),
        "ambiguous endpoint → candidate list: {ambiguous}"
    );

    // Exact id resolves; a→xa is one hop.
    let exact = call(
        &server,
        "shortest_path",
        json!({"from": "fn:a.rs:from", "to": "fn:a.rs:xa"}),
    );
    assert!(exact.starts_with("Shortest path (1 hops):"), "{exact}");
    assert!(exact.contains("from --calls [EXTRACTED]--> xa"), "{exact}");
}

#[test]
fn tool_list_exposes_all_tools() {
    let names: Vec<String> = McpServer::<FakeQuery>::tool_list()
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for expected in [
        "query_graph",
        "get_node",
        "get_neighbors",
        "get_community",
        "god_nodes",
        "graph_stats",
        "shortest_path",
        "project_graph",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing {expected}: {names:?}"
        );
    }
}
