//! MCP **protocol conformance** spec (ADR-0025). Distinct from `tools.rs` (which
//! pins rendered tool *text*): these assert the JSON-RPC/MCP envelope a standard
//! client relies on — real `inputSchema` JSON Schema on `tools/list`, a real
//! `protocolVersion` on `initialize`, and **no response** for notifications.
//!
//! Why it matters: a conformant client (Vercel AI SDK, `@modelcontextprotocol/sdk`)
//! reads each tool's `inputSchema` to tell the model its parameters. Without it the
//! model sees every tool as taking no arguments — it cannot call `query_graph(q=…)`.

use filigrio_client_mcp::McpServer;
use filigrio_core::{CommunityId, Edge, GraphQuery, GraphStats, Node, QueryOpts, Subgraph};
use serde_json::{json, Value};

/// Minimal `GraphQuery` — protocol tests never reach the graph, only the envelope.
#[derive(Default)]
struct NullQuery;

impl GraphQuery for NullQuery {
    fn query(&self, _q: &str, _opts: QueryOpts) -> filigrio_core::Result<Subgraph> {
        Ok(Subgraph::default())
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

fn server() -> McpServer<NullQuery> {
    McpServer::new(NullQuery)
}

fn tool<'a>(tools: &'a Value, name: &str) -> &'a Value {
    tools
        .as_array()
        .expect("tools is an array")
        .iter()
        .find(|t| t["name"] == json!(name))
        .unwrap_or_else(|| panic!("tool {name} not in list"))
}

#[test]
fn tools_list_exposes_json_schema_input_schema() {
    // `handle(tools/list)` is what a client actually calls — assert the envelope,
    // not the static helper.
    let resp = server().handle(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list"
    }));
    let tools = &resp["result"]["tools"];

    let qg = tool(tools, "query_graph");
    let schema = &qg["inputSchema"];
    assert_eq!(
        schema["type"],
        json!("object"),
        "inputSchema is an object schema: {qg}"
    );
    // The parameter the model must supply to call the tool at all.
    assert_eq!(
        schema["properties"]["q"]["type"],
        json!("string"),
        "query_graph.q must be a typed string property: {schema}"
    );
    assert!(
        schema["required"]
            .as_array()
            .map(|r| r.contains(&json!("q")))
            .unwrap_or(false),
        "q must be required: {schema}"
    );
    // The bespoke `input` field is gone (replaced, not duplicated).
    assert!(
        qg.get("input").is_none(),
        "legacy `input` field removed: {qg}"
    );
}

#[test]
fn every_tool_has_an_object_input_schema() {
    let resp = server().handle(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list"
    }));
    let tools = resp["result"]["tools"].as_array().unwrap().clone();
    assert_eq!(tools.len(), 8, "8 core tools");
    for t in &tools {
        let name = t["name"].as_str().unwrap();
        assert_eq!(
            t["inputSchema"]["type"],
            json!("object"),
            "{name} must carry an object inputSchema: {t}"
        );
        assert!(
            t["description"].is_string(),
            "{name} keeps a human description: {t}"
        );
    }
}

#[test]
fn query_graph_schema_types_optionals_correctly() {
    let resp = server().handle(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list"
    }));
    let props = &tool(&resp["result"]["tools"], "query_graph")["inputSchema"]["properties"];
    assert_eq!(props["depth"]["type"], json!("integer"), "depth is integer");
    assert_eq!(
        props["mode"]["enum"],
        json!(["bfs", "dfs"]),
        "mode is an enum"
    );
    assert_eq!(
        props["relations"]["type"],
        json!("array"),
        "relations is an array"
    );
    assert_eq!(
        props["relations"]["items"]["type"],
        json!("string"),
        "relations items are strings"
    );
    // Enumerated, not open: a grammar-constrained decoder can only emit values the
    // schema lists, so an open string is what lets a small model invent "call" and
    // silently get nothing back (ADR-0036 R2.1 / the `include_unresolved` regression).
    assert_eq!(
        props["relations"]["items"]["enum"],
        json!(filigrio_client_mcp::RELATION_FILTER_VOCABULARY),
        "query_graph relations enumerate the full relation vocabulary"
    );
}

/// The relation vocabulary a client may name, pinned at the wire.
///
/// Two properties, and the second is the load-bearing one:
///
/// 1. Every declared value is a real relation — the enum is built from
///    `filigrio_core::relation`'s constants, so a rename there is a compile error
///    here rather than a schema that quietly advertises a dead string.
/// 2. **Every declared value actually filters — including the bare family
///    spelling `type`.** It was withheld from `get_neighbors` while that path
///    matched with `==` (`GraphView::neighbors_at`) instead of ADR-0036's
///    `relation_matches_filter`, which made `relations=["type"]` match no edge and
///    return an empty list. Both surfaces now go through the one matcher, so both
///    advertise the one vocabulary — a schema value that returns nothing and a
///    working query the decoder may not spell are the same defect, and this test
///    is where either shows up.
#[test]
fn relation_enums_advertise_only_values_that_actually_filter() {
    let resp = server().handle(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list"
    }));
    let tools = &resp["result"]["tools"];

    let vocab = filigrio_client_mcp::RELATION_FILTER_VOCABULARY;
    for expected in [
        filigrio_core::relation::CALLS,
        filigrio_core::relation::PARAM_TYPE,
        filigrio_core::relation::RETURN_TYPE,
        filigrio_core::relation::FIELD_TYPE,
        filigrio_core::relation::BOUND_TYPE,
        filigrio_client_mcp::TYPE_FAMILY,
    ] {
        assert!(vocab.contains(&expected), "{expected} is in the vocabulary");
    }

    let neighbors = &tool(tools, "get_neighbors")["inputSchema"]["properties"]["relation"];
    let values = neighbors["enum"]
        .as_array()
        .expect("get_neighbors relation is an enum")
        .clone();
    assert!(
        values.contains(&json!(filigrio_core::relation::PARAM_TYPE)),
        "the family members are nameable: {values:?}"
    );
    assert!(
        values.contains(&json!(filigrio_client_mcp::TYPE_FAMILY)),
        "the bare family spelling filters here too — the neighbor path matches \
         hierarchically, so withholding it would hide a working query: {values:?}"
    );
    // One vocabulary across both surfaces, **plus** the ADR-0044 named questions
    // that only a neighbor query can answer — `callers` is `calls` + direction=in,
    // and a traversal has no direction to fold in.
    assert_eq!(
        values,
        filigrio_client_mcp::neighbor_relation_vocabulary()
            .into_iter()
            .map(|r| json!(r))
            .collect::<Vec<_>>(),
        "get_neighbors advertises the shared vocabulary verbatim"
    );
    for (name, _, _) in filigrio_client_mcp::NAMED_QUESTIONS {
        assert!(
            values.contains(&json!(name)),
            "`{name}` must be selectable, or it is prose again: {values:?}"
        );
    }
    for relation in vocab {
        assert!(
            values.contains(&json!(relation)),
            "the raw spelling stays valid — this is additive: {relation}"
        );
    }
}

#[test]
fn initialize_reports_a_real_protocol_version() {
    let resp = server().handle(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize"
    }));
    assert_eq!(
        resp["result"]["protocolVersion"],
        json!("2024-11-05"),
        "must advertise a real MCP protocol version, not a mock: {resp}"
    );
    // capabilities.tools stays present so the client enables tool use.
    assert!(
        resp["result"]["capabilities"]["tools"].is_object(),
        "tools capability present: {resp}"
    );
}

#[test]
fn notifications_get_no_response() {
    // A JSON-RPC notification has no `id`. Per spec the server MUST NOT answer it;
    // `handle` returns `None` so `serve` writes nothing.
    let out = server().handle_maybe(&json!({
        "jsonrpc": "2.0", "method": "notifications/initialized"
    }));
    assert!(
        out.is_none(),
        "notification must produce no response: {out:?}"
    );
}

#[test]
fn requests_with_id_still_get_a_response() {
    // The notification suppression must not swallow real (id-bearing) requests.
    let out = server().handle_maybe(&json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/list"
    }));
    let resp = out.expect("id-bearing request must get a response");
    assert_eq!(resp["id"], json!(7));
    assert!(resp["result"]["tools"].is_array());
}
