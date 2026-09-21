//! filigrio-client-mcp — the MCP stdio adapter over `GraphQuery` (HLD §3, ADR-0006/0013).
//! The crate was `filigrio-mcp` before 2026-07-28; the binary still is.
//!
//! The **transport framing** is a line-delimited JSON-RPC 2.0 subset of MCP; the
//! **tool set is the 7 core graphify tools** (`query_graph`, `get_node`,
//! `get_neighbors`, `get_community`, `god_nodes`, `graph_stats`,
//! `shortest_path`). Output text mirrors Python graphify's `serve.py` so an agent
//! sees equivalent, cited, **token-bounded** text. Serialization, **token
//! budgeting** and label **sanitization** live here (ADR-0006/0013) — transport
//! concerns, not query-core ones.
//!
//! Generic over `GraphQuery`, so the same adapter serves an in-memory view, a
//! DB-backed one, or a test fake. An optional **reloader** gives per-call graph
//! hot-reload: a tool call carrying `project_path` is answered against a freshly
//! loaded view for that path (the CLI wires this to a store directory).

#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

/// Token-bounded, sanitized text rendering. **Public because this crate's other
/// target needs it**: the `filigrio-mcp` binary renders the same lines from the
/// daemon's wire types, and it used to do so by declaring its own `mod format;`
/// — compiling a second copy of this file in which `DEFAULT_TOKEN_BUDGET` was
/// dead (papered over with `#[allow(dead_code)]`). One module, one copy, and
/// the two renderers cannot drift.
pub mod format;

use filigrio_core::attrs;
use filigrio_core::relation::filter::{ANY as FILTER_ANY, SEMANTIC as FILTER_SEMANTIC};
use filigrio_core::relation::{
    filter, BOUND_TYPE, CALLS, CONTAINS, DEPENDS_ON, EXTENDS, FIELD_TYPE, HAS_VARIANT, IMPLEMENTS,
    IMPORTS, INHERITS, PARAM_TYPE, RETURN_TYPE,
};
use filigrio_core::{CommunityId, EdgeTarget, GraphQuery, Node, QueryOpts, TraversalMode};
use format::{budget_cut, confidence, edge_line, node_line, sanitize, DEFAULT_TOKEN_BUDGET};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// The bare family spelling — `TYPE_FAMILY_PREFIX` without its trailing `/`.
/// A filter entry `"type"` selects the whole `type/…` family *wherever the
/// matcher is [`filigrio_core::relation::relation_matches_filter`]* (ADR-0036 R2.1).
pub const TYPE_FAMILY: &str = "type";

/// **The one home for the relation-filter vocabulary a tool schema advertises.**
///
/// Both declaration sites (this crate's library `tool_list` and the `filigrio-mcp`
/// bridge binary) render their `relations` enum from this slice, so the two cannot
/// drift — and neither can drift from `filigrio_core::relation`, since every entry
/// is that module's constant, not a re-typed string literal.
///
/// Why an **enum** and not an open `{"type": "string"}`: a grammar-constrained
/// decoder can only emit tokens the schema permits. Given an open string a small
/// model invents `"call"` / `"function_call"` / `"references"` and gets a silently
/// empty result; given an enum it cannot. This project has already paid for that
/// lesson once, with `include_unresolved` missing from the schema entirely.
///
/// Ordering is the taxonomy, not alphabetical: the two ADR-0044 **filter words**
/// first (they are the intended spellings of "everything" and "the default"),
/// then the flat relations, then the hierarchical `type/…` family behind its
/// bare family spelling.
///
/// The filter words are `filigrio_core::relation::filter`'s, not this crate's,
/// for the same anti-drift reason as the relation constants — and they are not
/// relations: no edge carries one (see that module's docs).
pub const RELATION_FILTER_VOCABULARY: &[&str] = &[
    FILTER_ANY,
    FILTER_SEMANTIC,
    CALLS,
    IMPORTS,
    CONTAINS,
    DEPENDS_ON,
    IMPLEMENTS,
    EXTENDS,
    INHERITS,
    HAS_VARIANT,
    TYPE_FAMILY,
    PARAM_TYPE,
    RETURN_TYPE,
    FIELD_TYPE,
    BOUND_TYPE,
];

/// **ADR-0044's named questions** — the query vocabulary, resolved here into the
/// storage vocabulary `(relation, direction)`.
///
/// Why this is not cosmetic, and why the evidence is stronger than ADR-0044's own:
/// across the three 2026-07-29 agent-eval runs the model passed `direction` on
/// **0 of 106** `get_neighbors` calls and `include_unresolved` on **5 of 106**,
/// while filling `relations` on 85 of 106 — and 51 of the 374 calls in those runs
/// were byte-identical repeats of the previous call whose *reasoning* said "let's
/// add direction:in" or "let's add include_unresolved:true". The model is not
/// declining to compose; it cannot reliably **add a key** to an argument object it
/// has already emitted. It can, demonstrably, *select a different value in a field
/// it is already filling*. Folding direction into the `relations` values routes the
/// question through the one field that works.
///
/// ADR-0044 sites this resolution in `filigrio-protocol`/the daemon responder so
/// the CLI inherits it. It lives here instead, for now, because the bridge **is**
/// the agent-facing query surface and this is one table in one place; promoting it
/// to the contract is a strictly mechanical move and does not change these names.
pub const NAMED_QUESTIONS: &[(&str, &str, Dir)] = &[
    ("callers", CALLS, Dir::In),
    ("callees", CALLS, Dir::Out),
    ("implementors", IMPLEMENTS, Dir::In),
    ("subtypes", EXTENDS, Dir::In),
    ("supertypes", EXTENDS, Dir::Out),
    ("takes", PARAM_TYPE, Dir::In),
    ("produces", RETURN_TYPE, Dir::In),
    ("stores", FIELD_TYPE, Dir::In),
    ("bounded_by", BOUND_TYPE, Dir::In),
];

/// The direction half of a named question. A local three-valued enum rather than
/// `filigrio_protocol::Direction`, so the library target (which does not depend on
/// the protocol crate) and the bridge share one table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
}

/// `(relation, direction)` for a named question, or `None` if `entry` is a plain
/// relation/filter word. Lookup is by exact match: a question name is a value the
/// caller *selects*, never a prefix to be guessed at.
pub fn named_question(entry: &str) -> Option<(&'static str, Dir)> {
    NAMED_QUESTIONS
        .iter()
        .find(|(name, _, _)| *name == entry)
        .map(|(_, relation, dir)| (*relation, *dir))
}

/// The vocabulary a **neighbor** filter may name.
///
/// **One vocabulary, both surfaces.** It briefly was the slice *minus* the bare
/// family spelling [`TYPE_FAMILY`], because `GraphView::neighbors_at` matched a
/// filter with `==` rather than
/// [`filigrio_core::relation::relation_matches_filter`] — so `relations=["type"]`
/// selected no edge and returned an empty list, and advertising a value that
/// silently returns nothing is exactly the failure an enum exists to prevent. The
/// withholding was always stated as temporary ("until the matcher honors the
/// prefix"); the matcher now does, on both the resolved and unresolved halves, so
/// this collapses to the slice itself as promised. Kept as a function, not
/// re-pointed at the constant, so the two schema sites keep one call site to
/// change if a surface ever *does* need a narrower vocabulary.
pub fn neighbor_relation_vocabulary() -> Vec<&'static str> {
    // The named questions come **first**: they are the intended spellings, and a
    // constrained decoder reads the enum in order.
    NAMED_QUESTIONS
        .iter()
        .map(|(name, _, _)| *name)
        .chain(RELATION_FILTER_VOCABULARY.iter().copied())
        .collect()
}

/// Builds a fresh view for a `project_path` (per-call hot-reload). `Err` text is
/// surfaced to the caller as a tool error.
type Reloader<Q> = Box<dyn Fn(&str) -> std::result::Result<Q, String>>;

pub struct McpServer<Q: GraphQuery> {
    query: Q,
    reload: Option<Reloader<Q>>,
}

impl<Q: GraphQuery> McpServer<Q> {
    pub fn new(query: Q) -> Self {
        McpServer {
            query,
            reload: None,
        }
    }

    /// As [`McpServer::new`], plus a reloader: any tool call carrying a
    /// `project_path` argument is answered against `reload(project_path)`.
    pub fn with_reloader(query: Q, reload: Reloader<Q>) -> Self {
        McpServer {
            query,
            reload: Some(reload),
        }
    }

    /// Borrow the default query engine (transports/tests inspect it).
    pub fn query_ref(&self) -> &Q {
        &self.query
    }

    /// The 8 core tools this adapter exposes, each with a **JSON Schema
    /// `inputSchema`** (MCP-conformant, ADR-0025) so a standard client can tell the
    /// model each tool's parameters. `query_graph`/`project_graph` also accept an
    /// optional `project_path` (hot-reload) when the server has a reloader.
    pub fn tool_list() -> Value {
        // Small local helpers keep the schemas readable and uniform.
        let obj = |props: Value, required: Value| {
            json!({
                "type": "object", "properties": props, "required": required
            })
        };
        let string = || json!({"type": "string"});
        let integer = || json!({"type": "integer"});

        json!([
            {"name": "graph_stats",
             "description": "Node/edge/community counts + confidence mix.",
             "inputSchema": obj(json!({}), json!([]))},
            {"name": "query_graph",
             "description": "IDF+trigram seed-ranked traversal; community-cited, token-bounded text.",
             "inputSchema": obj(json!({
                 "q": {"type": "string", "description": "Search terms to seed the traversal."},
                 "depth": integer(),
                 "budget": integer(),
                 "mode": {"type": "string", "enum": ["bfs", "dfs"]},
                 // Enumerated, not an open string (see `RELATION_FILTER_VOCABULARY`).
                 // This filter matches hierarchically, so `["type"]` selects the
                 // whole `type/…` family — hence the full vocabulary here.
                 "relations": {"type": "array",
                               "items": {"type": "string", "enum": RELATION_FILTER_VOCABULARY},
                               "description": "Edge kinds to keep (OR across entries). `semantic` = code meaning only, the default; `any` = every kind, structural scaffolding included. `type` selects the whole type-reference family; `type/param` etc. narrow it."},
                 "token_budget": integer(),
                 "project_path": string()
             }), json!(["q"]))},
            {"name": "get_node",
             "description": "Fetch one node. A label may be ambiguous (homonyms are distinct nodes); pass `src` (the file from a listing) or the exact `id` to pick one — an ambiguous label returns the candidate list.",
             "inputSchema": obj(json!({
                 "label": string(),
                 "src": {"type": "string", "description": "Source file to disambiguate a homonym label."},
                 "id": {"type": "string", "description": "Exact node id (the only unique address)."}
             }), json!([]))},
            {"name": "get_neighbors",
             "description": "Adjacent nodes of a node. Address the specific node by `id`, or by `label` (+ `src` to disambiguate a homonym). `direction`: in = callers/users (what calls or uses it), out = callees, both (default). `relation` picks the edge kind: `semantic` (the default) = code meaning only, `any` = every kind including structure, `\"contains\"`/`\"imports\"` for structure alone, or `type` for every type reference, narrowed by member: with direction=in, `type` = what USES this type, `type/param` = what TAKES it, `type/return` = what PRODUCES it, `type/field` = what STORES it. A status line always reports *unresolved* edges we couldn't bind (opaque receivers, externals) as counts — an empty resolved list with a nonzero count means \"look\", not \"dead\". Pass `include_unresolved:true` to list them (incoming callers are addressable; outgoing callees are bare names — read the source to confirm).",
             "inputSchema": obj(
                 json!({
                     "label": string(),
                     // Matched hierarchically (ADR-0036 R2.1), so the bare family
                     // spelling `type` is a real, non-empty filter here.
                     "relation": {"type": "string", "enum": neighbor_relation_vocabulary(),
                                  "description": "Keep only edges of this kind. `semantic` = code meaning only (the default); `any` = every kind, structural included. `type` keeps the whole type-reference family; a `type/…` member narrows it."},
                     "src": string(), "id": string(),
                     "direction": {"type": "string", "enum": ["in", "out", "both"]},
                     "include_unresolved": {"type": "boolean", "description": "List the unresolved by-name callers / declined calls, not just their count."}
                 }),
                 json!([]))},
            {"name": "get_community",
             "description": "Members of a community by id.",
             "inputSchema": obj(
                 json!({"community_id": integer()}), json!(["community_id"]))},
            {"name": "god_nodes",
             "description": "Top-N most-connected nodes.",
             "inputSchema": obj(json!({"top_n": integer()}), json!([]))},
            {"name": "shortest_path",
             "description": "Shortest directed path between two nodes. Address each end by `from`/`to` — a node id, or a label (an ambiguous label returns the candidate list so you can re-address by id). `src` here would be a *file*; these are endpoints, hence `from`/`to`.",
             "inputSchema": obj(
                 json!({
                     "from": {"type": "string", "description": "Start node: an id, or a label."},
                     "to": {"type": "string", "description": "End node: an id, or a label."},
                     "max_hops": integer()
                 }),
                 json!(["from", "to"]))},
            {"name": "project_graph",
             "description": "The monorepo architecture map: projects + depends_on edges.",
             "inputSchema": obj(
                 json!({"token_budget": integer(), "project_path": string()}), json!([]))}
        ])
    }

    /// Handle one JSON-RPC request object, returning the response object. A
    /// **notification** (a request with no `id`) yields `None` — the server must not
    /// answer it (JSON-RPC/MCP, ADR-0025). Errors map to JSON-RPC error responses,
    /// never panics.
    pub fn handle_maybe(&self, req: &Value) -> Option<Value> {
        // No `id` member ⇒ notification ⇒ no response.
        let id = req.get("id")?.clone();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": {"name": "filigrio", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"tools": {}}
            })),
            "tools/list" => Ok(json!({"tools": Self::tool_list()})),
            "tools/call" => self.call_tool(&params),
            other => Err(format!("unknown method: {other}")),
        };

        Some(match result {
            Ok(value) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
            Err(msg) => json!({"jsonrpc": "2.0", "id": id,
                               "error": {"code": -32601, "message": msg}}),
        })
    }

    /// Convenience wrapper over [`handle_maybe`] for id-bearing requests (tests and
    /// in-process callers): a notification collapses to `Null` rather than `None`.
    pub fn handle(&self, req: &Value) -> Value {
        self.handle_maybe(req).unwrap_or(Value::Null)
    }

    fn call_tool(&self, params: &Value) -> std::result::Result<Value, String> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);

        // Per-call hot-reload: a `project_path` + a reloader → answer against a
        // freshly loaded view; otherwise the default view.
        let text = match (arg_str(&args, "project_path"), &self.reload) {
            (Some(path), Some(reload)) => {
                let view = reload(&path)?;
                run_tool(&view, name, &args)?
            }
            _ => run_tool(&self.query, name, &args)?,
        };
        Ok(json!({"content": [{"type": "text", "text": text}]}))
    }

    /// Read JSON-RPC requests line by line, write responses. One object per line.
    pub fn serve<R: BufRead, W: Write>(&self, reader: R, mut writer: W) -> std::io::Result<()> {
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let response = match serde_json::from_str::<Value>(trimmed) {
                // A notification (no `id`) yields `None` — write nothing (ADR-0025).
                Ok(req) => match self.handle_maybe(&req) {
                    Some(resp) => resp,
                    None => continue,
                },
                Err(e) => json!({"jsonrpc": "2.0", "id": Value::Null,
                                 "error": {"code": -32700, "message": format!("parse error: {e}")}}),
            };
            writeln!(writer, "{}", serde_json::to_string(&response)?)?;
            writer.flush()?;
        }
        Ok(())
    }
}

// ---- tool bodies (free fns over any `GraphQuery`; format mirrors serve.py) ----

fn run_tool<Q: GraphQuery>(
    view: &Q,
    name: &str,
    args: &Value,
) -> std::result::Result<String, String> {
    match name {
        "graph_stats" => stats_text(view),
        "query_graph" => query_text(view, args),
        "get_node" => node_text(view, args),
        "get_neighbors" => neighbors_text(view, args),
        "get_community" => community_text(view, args),
        "god_nodes" => god_text(view, args),
        "shortest_path" => path_text(view, args),
        "project_graph" => project_graph_text(view, args),
        other => Err(format!("unknown tool: {other}")),
    }
}

fn stats_text<Q: GraphQuery>(view: &Q) -> std::result::Result<String, String> {
    let s = view.stats().map_err(|e| e.to_string())?;
    let total: usize = s.by_confidence.values().sum();
    let pct = |k: &str| -> usize {
        if total == 0 {
            0
        } else {
            ((*s.by_confidence.get(k).unwrap_or(&0) as f64) / total as f64 * 100.0).round() as usize
        }
    };
    Ok(format!(
        "Nodes: {}\nEdges: {}\nCommunities: {}\nEXTRACTED: {}%\nINFERRED: {}%\nAMBIGUOUS: {}%",
        s.nodes,
        s.edges,
        s.communities,
        pct("Extracted"),
        pct("Inferred"),
        pct("Ambiguous"),
    ))
}

fn query_text<Q: GraphQuery>(view: &Q, args: &Value) -> std::result::Result<String, String> {
    let q = arg_str(args, "q").unwrap_or_default();
    let mut opts = QueryOpts::default();
    if let Some(d) = arg_usize(args, "depth") {
        opts.depth = d;
    }
    if let Some(b) = arg_usize(args, "budget") {
        opts.budget = b;
    }
    if let Some(m) = arg_str(args, "mode") {
        if m.eq_ignore_ascii_case("dfs") {
            opts.mode = TraversalMode::Dfs;
        }
    }
    // ADR-0044: an omitted/empty `relations` is the named value `semantic`, via
    // the one normalization every read surface shares — this used to be the
    // *other* reading of empty (no filter at all), which is now spelled `any`.
    let requested: Vec<String> = args
        .get("relations")
        .and_then(Value::as_array)
        .map(|rels| {
            rels.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    opts.context_filter = Some(filter::normalize(&requested).into_owned());
    let token_budget = arg_usize(args, "token_budget").unwrap_or(DEFAULT_TOKEN_BUDGET);

    // Capture header inputs before `opts` moves into the query.
    let mode = if opts.mode == TraversalMode::Dfs {
        "DFS"
    } else {
        "BFS"
    };
    let depth = opts.depth;
    let ctx = opts
        .context_filter
        .as_ref()
        .map(|f| format!(" | Context: {}", f.join(", ")))
        .unwrap_or_default();

    let sub = view.query(&q, opts).map_err(|e| e.to_string())?;

    // Body: nodes first (already priority-ordered), then edges whose BOTH
    // endpoints are present — matching serve.py's `_subgraph_to_text`.
    let mut body = String::new();
    for n in &sub.nodes {
        let comm = sub.communities.get(&n.id).map(String::as_str).unwrap_or("");
        body.push_str(&node_line(n, comm));
        body.push('\n');
    }
    for e in &sub.edges {
        if let EdgeTarget::Node(t) = &e.target {
            let src = sub.nodes.iter().find(|n| n.id == e.source);
            let dst = sub.nodes.iter().find(|n| &n.id == t);
            if let (Some(s), Some(d)) = (src, dst) {
                body.push_str(&edge_line(&s.label, &e.relation, e.confidence, &d.label));
                body.push('\n');
            }
        }
    }

    // Header is always shown; only the body is budgeted (as in serve.py).
    let header = format!(
        "Traversal: {} depth={}{} | {} nodes found\n\n",
        mode,
        depth,
        ctx,
        sub.nodes.len(),
    );
    Ok(format!("{header}{}", budget_cut(body, token_budget)))
}

/// Resolving a node reference from `{id?, label?, src?}`: an exact `id` wins;
/// else `label` (optionally narrowed by `src`) — which may still be ambiguous
/// because homonyms are distinct nodes (ADR-0027).
enum Resolved {
    One(Node),
    None,
    /// Several nodes share the label — surfaced as a candidate list so the caller
    /// can re-address by `id`/`src` rather than the transport guessing.
    Many(Vec<Node>),
}

/// Resolve a single string that is *either* an exact id *or* a label — the
/// addressing for `shortest_path`'s `from`/`to` endpoints, where there's no room
/// for a separate `src` disambiguator per end. An id wins; else the label, which
/// may still be the ambiguous set (ADR-0027: never guess a homonym).
fn resolve_endpoint<Q: GraphQuery>(view: &Q, value: &str) -> std::result::Result<Resolved, String> {
    if let Some(n) = view.node_by_id(value).map_err(|e| e.to_string())? {
        return Ok(Resolved::One(n));
    }
    let mut cands = view.nodes_by_label(value).map_err(|e| e.to_string())?;
    Ok(match cands.len() {
        0 => Resolved::None,
        1 => match cands.pop() {
            Some(n) => Resolved::One(n),
            None => {
                return Err("resolve_endpoint: candidate list empty after length check".to_string())
            }
        },
        _ => Resolved::Many(cands),
    })
}

/// Resolve `{id?, label?, src?}` to a single node, no node, or the ambiguous set.
fn resolve_target<Q: GraphQuery>(view: &Q, args: &Value) -> std::result::Result<Resolved, String> {
    if let Some(id) = arg_str(args, "id") {
        return Ok(match view.node_by_id(&id).map_err(|e| e.to_string())? {
            Some(n) => Resolved::One(n),
            None => Resolved::None,
        });
    }
    let label = arg_str(args, "label").unwrap_or_default();
    let mut cands = view.nodes_by_label(&label).map_err(|e| e.to_string())?;
    if let Some(src) = arg_str(args, "src") {
        cands.retain(|n| n.source_file.as_deref() == Some(src.as_str()));
    }
    Ok(match cands.len() {
        0 => Resolved::None,
        1 => match cands.pop() {
            Some(n) => Resolved::One(n),
            None => {
                return Err("resolve_target: candidate list empty after length check".to_string())
            }
        },
        _ => Resolved::Many(cands),
    })
}

/// The disambiguation list an ambiguous label yields — every candidate's id + src
/// so the model can re-address by `id=`/`src=` (ADR-0027: never guess a homonym).
fn disambiguation(label: &str, cands: &[Node]) -> String {
    let mut out = format!(
        "{} nodes named '{}' — pass id= (or src=) to pick one:",
        cands.len(),
        sanitize(label)
    );
    for n in cands {
        // `label (impl X) [src=… loc=… id=…]` — the id rides in the suffix now, so
        // the model copies the whole `id=` token rather than the leading bare id.
        out.push_str(&format!("\n  {}{}", sanitize(&n.label), id_suffix(n)));
    }
    out
}

/// The `(impl Owner) [src=… loc=… id=…]` suffix that makes a node self-identifying.
/// The `id=` is the copy-able unique address — every discovery line carries it so a
/// model never has to *construct* an id from the label/impl (which it gets wrong,
/// ADR-0027): it copies `id=` verbatim into `get_node`/`get_neighbors({id})`.
fn id_suffix(n: &Node) -> String {
    let owner = n
        .attrs
        .get("impl")
        .map(|o| format!(" (impl {})", sanitize(o)))
        .unwrap_or_default();
    format!(
        "{owner} [src={} loc={} id={}]",
        sanitize(n.source_file.as_deref().unwrap_or("")),
        sanitize(&n.loc()),
        sanitize(&n.id.0),
    )
}

fn node_text<Q: GraphQuery>(view: &Q, args: &Value) -> std::result::Result<String, String> {
    let label = arg_str(args, "label").unwrap_or_default();
    match resolve_target(view, args)? {
        Resolved::One(n) => {
            let community = n
                .attrs
                .get(attrs::COMMUNITY)
                .map(String::as_str)
                .unwrap_or("");
            // Same `(impl X) [src=… loc=… id=…]` handle as god_nodes/disambiguation so
            // the citation + copy-able id read identically across every tool
            // (ADR-0027); impl owner and id ride in the suffix, no separate lines.
            Ok(format!(
                "Node: {}{}\n  Type: {}\n  Community: {}",
                sanitize(&n.label),
                id_suffix(&n),
                sanitize(&n.kind),
                sanitize(community),
            ))
        }
        Resolved::None => Ok(format!("No node matching '{label}' found.")),
        Resolved::Many(cands) => Ok(disambiguation(&label, &cands)),
    }
}

fn neighbors_text<Q: GraphQuery>(view: &Q, args: &Value) -> std::result::Result<String, String> {
    let label = arg_str(args, "label").unwrap_or_default();
    // ADR-0044: this surface takes ONE relation, so a named question resolves to
    // exactly one `(relation, direction)` pair and there is no conflict case. The
    // question's direction wins over `direction`, same rule as the bridge — the
    // schema advertises these names, so failing to resolve them here would be the
    // "advertise a value that silently returns nothing" defect the enum exists to
    // prevent.
    let asked = arg_str(args, "relation");
    let (relation, direction) = match asked.as_deref().and_then(named_question) {
        Some((r, Dir::In)) => (Some(r.to_string()), filigrio_core::Direction::In),
        Some((r, Dir::Out)) => (Some(r.to_string()), filigrio_core::Direction::Out),
        None => (asked, parse_direction(args)),
    };
    let node = match resolve_target(view, args)? {
        Resolved::One(n) => n,
        Resolved::None => return Ok(format!("No node matching '{label}' found.")),
        Resolved::Many(cands) => return Ok(disambiguation(&label, &cands)),
    };
    // Address neighbors by the *specific* node's id, not the ambiguous label.
    // This surface takes one relation, the port takes a filter set — and an
    // absent `relation` normalizes to the named `semantic` (ADR-0044), the same
    // helper `query_text` uses, so the structural scaffolding drops here without
    // a local post-filter that could drift from the other surface.
    let requested: Vec<String> = relation.clone().into_iter().collect();
    let rel_filter = filter::normalize(&requested);
    let ns = view
        .neighbors_by_id(&node.id.0, rel_filter.as_ref(), direction)
        .map_err(|e| e.to_string())?;
    let mut out = format!(
        "Neighbors of {}{}:",
        sanitize(&node.label),
        id_suffix(&node)
    );
    let header_len = out.len();
    for (e, n) in ns {
        // `-->` = this node calls the neighbor (outgoing); `<--` = the neighbor
        // calls this node (incoming — "what calls it"). Read off the edge.
        let arrow = if e.source.0 == node.id.0 {
            "-->"
        } else {
            "<--"
        };
        // Carry the same `(impl X) [src=… loc=…]` self-addressing handle as
        // god_nodes/get_node (ADR-0027): a neighbor is the most likely next hop, and
        // a bare label can't be re-addressed if it's a homonym.
        out.push_str(&format!(
            "\n  {arrow} {}{} [{}] [{}]",
            sanitize(&n.label),
            id_suffix(&n),
            sanitize(&e.relation),
            confidence(e.confidence),
        ));
    }
    append_unresolved(&mut out, view, &node, rel_filter.as_ref(), direction, args)?;
    // An empty answer states the filter that produced it, rather than being
    // indistinguishable from "this node has no edges" — see `no_match_note`'s
    // twin in the bridge for the run this cost.
    if out.len() == header_len {
        out.push_str(&format!(
            "\n  no edges matched — direction={direction:?}, relations=[{}]. \
             Widen with relation:\"{FILTER_ANY}\", or a different direction; \
             an unrecognised relation name also lands here.",
            sanitize(&rel_filter.join(", ")),
        ));
    }
    Ok(out)
}

/// Whether the (already normalized) filter set keeps this unresolved edge. The
/// unresolved half filters through the **same** normalized set and the **same**
/// hierarchical matcher the resolved half uses (ADR-0029 + ADR-0036 R2.1 +
/// ADR-0044), so `type` selects the family on both halves or neither, and the
/// empty case is decided once, upstream, rather than twice here.
fn keeps(relations: &[String], edge_rel: &str) -> bool {
    relations
        .iter()
        .any(|entry| filigrio_core::relation::relation_matches_filter(entry, edge_rel))
}

/// `true` when the declined call's receiver was opaque (ADR-0023 marker, preserved
/// onto the persisted `Symbol` by ADR-0029) — lets the render say so truthfully.
fn is_opaque(e: &filigrio_core::Edge) -> bool {
    matches!(&e.target, filigrio_core::EdgeTarget::Symbol(r) if r.hints.get("recv").map(String::as_str) == Some("opaque"))
}

/// The ADR-0029 honesty extension: `neighbors` now means "resolved, **plus** a
/// truthful signal about the unresolved edges we couldn't bind." Counts by default
/// (so an empty resolved list never reads as "nothing here"); `include_unresolved`
/// lists them, marked, so the model can read the source to confirm the specific call.
fn append_unresolved<Q: GraphQuery>(
    out: &mut String,
    view: &Q,
    node: &Node,
    relations: &[String],
    direction: filigrio_core::Direction,
    args: &Value,
) -> std::result::Result<(), String> {
    use filigrio_core::Direction;
    let want_out = matches!(direction, Direction::Out | Direction::Both);
    let want_in = matches!(direction, Direction::In | Direction::Both);

    // Outgoing: this node's own declined calls — exact (keyed by id).
    let outs: Vec<_> = if want_out {
        view.unresolved_out_by_id(&node.id.0)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|e| keeps(relations, &e.relation))
            .collect()
    } else {
        Vec::new()
    };
    // Incoming: unresolved edges whose callee name == this label — a *by-name*
    // heuristic (may over-match homonyms), so it is marked as such, never as resolved.
    let ins: Vec<_> = if want_in {
        view.unresolved_in_by_label(&node.label)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|(e, _)| keeps(relations, &e.relation))
            .collect()
    } else {
        Vec::new()
    };

    if outs.is_empty() && ins.is_empty() {
        return Ok(());
    }

    if arg_bool(args, "include_unresolved") {
        // Incoming: the caller IS an addressable node → show its id handle so the
        // model can read it to confirm (ADR-0029 open-q1). Marked `by-name`.
        for (e, caller) in &ins {
            out.push_str(&format!(
                "\n  <-- {}{} [{}] [UNRESOLVED, by-name]",
                sanitize(&caller.label),
                id_suffix(caller),
                sanitize(&e.relation),
            ));
        }
        // Outgoing: the callee is a bare `Symbol` name, *not* an addressable node —
        // no id= handle (that absence is the honest part).
        for e in &outs {
            let name = match &e.target {
                filigrio_core::EdgeTarget::Symbol(r) => r.name.as_str(),
                _ => continue,
            };
            let why = if is_opaque(e) {
                " (opaque receiver — read this node's body to confirm)"
            } else {
                ""
            };
            out.push_str(&format!(
                "\n  --> {} [{}] [UNRESOLVED]{}",
                sanitize(name),
                sanitize(&e.relation),
                why,
            ));
        }
    } else {
        // Counts by default: never silently omit. One status line so an empty
        // resolved list doesn't read as "nothing calls this" (the builder_family lie).
        let mut parts = Vec::new();
        if !ins.is_empty() {
            let opaque = if ins.iter().any(|(e, _)| is_opaque(e)) {
                " (opaque receivers)"
            } else {
                ""
            };
            parts.push(format!("{} by-name caller(s){}", ins.len(), opaque));
        }
        if !outs.is_empty() {
            parts.push(format!("{} declined call(s)", outs.len()));
        }
        out.push_str(&format!(
            "\n  unresolved: {} — pass include_unresolved:true to list them",
            parts.join(", "),
        ));
    }
    Ok(())
}

/// `direction` arg → `Direction` (default `Both`: an agent asking for a hub's
/// neighbors sees its callers without knowing to request `in`; ADR-0027).
fn parse_direction(args: &Value) -> filigrio_core::Direction {
    use filigrio_core::Direction;
    match arg_str(args, "direction").as_deref() {
        Some("out") => Direction::Out,
        Some("in") => Direction::In,
        _ => Direction::Both,
    }
}

fn community_text<Q: GraphQuery>(view: &Q, args: &Value) -> std::result::Result<String, String> {
    let cid = arg_usize(args, "community_id").unwrap_or(0) as u64;
    let members = view
        .community(CommunityId(cid))
        .map_err(|e| e.to_string())?;
    if members.is_empty() {
        return Ok(format!("Community {cid} not found."));
    }
    // Derived label lives on each node's `community` attr (stamped by the view).
    let name = members[0]
        .attrs
        .get("community")
        .cloned()
        .unwrap_or_default();
    let base = format!("Community {cid}");
    let header = if !name.is_empty() && name != base {
        format!("{base} — {}", sanitize(&name))
    } else {
        base
    };
    // Cohesion (ADR-0024): an honest quality signal, so an LLM reader can tell a
    // tight cluster from a loosely-bound one.
    let cohesion = view
        .community_meta(CommunityId(cid))
        .ok()
        .flatten()
        .map(|m| format!(", cohesion {:.2}", m.cohesion()))
        .unwrap_or_default();
    let mut out = format!("{header} ({} nodes{cohesion}):", members.len());
    for n in &members {
        // Members carry the same `(impl X) [src=… loc=… id=…]` handle as every other
        // listing (ADR-0027 coherence): a community is a discovery surface too, so a
        // member you want to inspect is addressable without a re-lookup.
        out.push_str(&format!("\n  {}{}", sanitize(&n.label), id_suffix(n)));
    }
    Ok(out)
}

fn god_text<Q: GraphQuery>(view: &Q, args: &Value) -> std::result::Result<String, String> {
    let top_n = arg_usize(args, "top_n").unwrap_or(10);
    let gs = view.god_nodes(top_n).map_err(|e| e.to_string())?;
    let mut out = String::from("God nodes (most connected):");
    for (i, (n, deg)) in gs.iter().enumerate() {
        // Cite the impl owner + src/loc (ADR-0025/0027 eval findings): a bare label
        // both forced a re-lookup to locate the hub *and* was ambiguous across
        // homonyms — the `(impl X) [src=…]` suffix makes each hub self-addressing.
        out.push_str(&format!(
            "\n  {}. {}{} - {} edges",
            i + 1,
            sanitize(&n.label),
            id_suffix(n),
            deg
        ));
    }
    Ok(out)
}

/// Display name of a project: its package name, else its root (`<root>` at the
/// repo root).
fn project_label(p: &filigrio_core::ProjectNode) -> String {
    p.name.clone().unwrap_or_else(|| {
        if p.root.is_empty() {
            "<root>".to_string()
        } else {
            p.root.clone()
        }
    })
}

fn project_graph_text<Q: GraphQuery>(
    view: &Q,
    args: &Value,
) -> std::result::Result<String, String> {
    let pg = view.project_graph().map_err(|e| e.to_string())?;
    if pg.projects.is_empty() {
        return Ok("No projects — the tree has no package/module manifests.".to_string());
    }
    // root → display label, for rendering `depends_on` targets by name.
    let label_of = |root: &str| -> String {
        pg.projects
            .iter()
            .find(|p| p.root == root)
            .map(project_label)
            .unwrap_or_else(|| root.to_string())
    };
    let header = format!("Projects: {} (project → depends_on)\n", pg.projects.len());
    let mut body = String::new();
    for p in &pg.projects {
        let deps: Vec<String> = p
            .depends_on
            .iter()
            .map(|r| sanitize(&label_of(r)))
            .collect();
        let dep_str = if deps.is_empty() {
            String::new()
        } else {
            format!(" → {}", deps.join(", "))
        };
        body.push_str(&format!(
            "PROJECT {} [root={} files={}]{}\n",
            sanitize(&project_label(p)),
            sanitize(&p.root),
            p.files,
            dep_str,
        ));
    }
    let token_budget = arg_usize(args, "token_budget").unwrap_or(DEFAULT_TOKEN_BUDGET);
    Ok(format!("{header}{}", budget_cut(body, token_budget)))
}

fn path_text<Q: GraphQuery>(view: &Q, args: &Value) -> std::result::Result<String, String> {
    let from = arg_str(args, "from").unwrap_or_default();
    let to = arg_str(args, "to").unwrap_or_default();
    let max_hops = arg_usize(args, "max_hops").unwrap_or(8);
    // Address each endpoint by id-or-label with the same homonym discipline as the
    // other tools (ADR-0027): an ambiguous end yields the candidate list, never a
    // silent first-match. Then run the path over the resolved *ids*.
    let from_node = match resolve_endpoint(view, &from)? {
        Resolved::One(n) => n,
        Resolved::None => return Ok(format!("No node matching '{from}' found.")),
        Resolved::Many(cands) => return Ok(disambiguation(&from, &cands)),
    };
    let to_node = match resolve_endpoint(view, &to)? {
        Resolved::One(n) => n,
        Resolved::None => return Ok(format!("No node matching '{to}' found.")),
        Resolved::Many(cands) => return Ok(disambiguation(&to, &cands)),
    };
    let path = match view
        .shortest_path(&from_node.id.0, &to_node.id.0, max_hops)
        .map_err(|e| e.to_string())?
    {
        Some(p) => p,
        None => {
            return Ok(format!(
                "No path found between '{}' and '{}'.",
                sanitize(&from_node.label),
                sanitize(&to_node.label)
            ))
        }
    };
    let hops = path.len().saturating_sub(1);
    // Rebuild each hop's relation from the directed neighbourhood (the path is
    // directed, so v is always a successor of u).
    let mut segments: Vec<String> = Vec::new();
    if let Some(first) = path.first() {
        segments.push(sanitize(&first.label));
    }
    for w in path.windows(2) {
        let (u, v) = (&w[0], &w[1]);
        // Address `u` by its exact id — a homonym label could fetch a different
        // node's neighbours and drop the hop relation to `?` (ADR-0027). The path is
        // directed, so `v` is an *outgoing* successor of `u`.
        let (rel, conf) = view
            .neighbors_by_id(&u.id.0, &[], filigrio_core::Direction::Out)
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|(_, nb)| nb.id == v.id)
            .map(|(e, _)| (e.relation, format!(" [{}]", confidence(e.confidence))))
            .unwrap_or_else(|| (String::from("?"), String::new()));
        segments.push(format!(
            "--{}{}--> {}",
            sanitize(&rel),
            conf,
            sanitize(&v.label)
        ));
    }
    Ok(format!(
        "Shortest path ({hops} hops):\n  {}",
        segments.join(" ")
    ))
}

// ---- arg helpers -------------------------------------------------------------

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn arg_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|v| v as usize)
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

// Re-exported so integration tests can build a `Node` list without duplicating
// the community-attr convention.
#[doc(hidden)]
pub fn community_attr(mut n: Node, label: &str) -> Node {
    n.attrs.insert(attrs::COMMUNITY.into(), label.into());
    n
}
