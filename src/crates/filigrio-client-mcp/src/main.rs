//! filigrio-mcp (the binary of crate `filigrio-client-mcp`) — the MCP stdio
//! bridge to the daemon (ADR-0032f §1, §2).
//!
//! This is a **thin, engine-free** binary that:
//! - Speaks MCP JSON-RPC to agents over stdio
//! - Translates tool calls to §3 data-plane queries over the socket
//! - Exposes **read-only** operations: the data plane, and nothing else
//! - Owns the MCP session (roots, schemas)
//!
//! **The bridge has no mutation surface at all (ADR-0042 F9).** It runs with
//! the *user's* filesystem permissions, which are not the agent's, so any
//! mutation it exposed would be exercised with the bridge's authority — a
//! confused deputy. `register_project` in particular let an agent make a
//! project queryable through the graph that it may have had no right to read.
//! The proper containment is MCP **roots** (agent-scoped), which is not built;
//! until it is, the honest posture is no mutation surface at all. This is
//! strictly stronger than ADR-0032f's narrow-sliver answer: a bridge with no
//! command path cannot be confused into anything, rather than being confusable
//! only into two things. Registering and indexing are *operator* actions, done
//! through the CLI/daemon control plane; with watch mode a watched project
//! stays fresh without the agent asking.
//!
//! Unlike the old CLI's embedded server, this holds no graph state and links no
//! engine code — the only binary that should embed the engine is `filigrio-daemon`.

use anyhow::{Context, Result};
use filigrio_client_core::resolve_project_path;
use filigrio_core::{Edge, EdgeTarget, Node, Subgraph};
use filigrio_protocol::{
    default_socket_path, DaemonClient, DaemonClientTrait, DataQuery, Request, Response,
};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, EnvFilter};

// The **library's** `format`, not a second copy of it: `main.rs` used to
// declare its own `mod format;`, so `format.rs` was compiled into both of this
// crate's targets and `DEFAULT_TOKEN_BUDGET` was dead in one of them.
use filigrio_client_mcp::{
    format, named_question, neighbor_relation_vocabulary, RELATION_FILTER_VOCABULARY,
};
// The filter words come from core, not from a re-typed literal, for the same
// anti-drift reason the relation constants do (see `RELATION_FILTER_VOCABULARY`).
use filigrio_core::relation::filter::{ANY as FILTER_ANY, SEMANTIC as FILTER_SEMANTIC};

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();

    let mut verbose = false;
    let mut socket_path = default_socket_path();

    // Parse flags (very simple, not using clap to keep it minimal)
    for (i, arg) in args.iter().enumerate() {
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "--socket" if i + 1 < args.len() => {
                socket_path = PathBuf::from(&args[i + 1]);
            }
            _ => {}
        }
    }

    // Initialize logging
    let filter = if verbose {
        EnvFilter::from_env("FILIGRIO_LOG").add_directive("filigrio_mcp=debug".parse().unwrap())
    } else {
        EnvFilter::from_env("FILIGRIO_LOG").add_directive("filigrio_mcp=info".parse().unwrap())
    };

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    info!("filigrio-mcp starting");
    info!("Socket path: {}", socket_path.display());

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let reader = stdin.lock();
    let writer = std::io::BufWriter::new(stdout.lock());

    let bridge = McpBridge::new(socket_path);
    bridge.serve(reader, writer)?;

    Ok(())
}

struct McpBridge {
    socket_path: PathBuf,
}

impl McpBridge {
    fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    fn serve<R: BufRead, W: Write>(&self, reader: R, mut writer: W) -> Result<()> {
        info!("Listening for JSON-RPC requests on stdio");

        for line in reader.lines() {
            let line = line.context("Failed to read line from stdin")?;
            let trimmed = line.trim();

            if trimmed.is_empty() {
                continue;
            }

            let response = match serde_json::from_str::<Value>(trimmed) {
                Ok(req) => self.handle_request(req),
                Err(e) => {
                    error!("Failed to parse JSON-RPC: {}", e);
                    json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": {"code": -32700, "message": format!("parse error: {}", e)}
                    })
                }
            };

            if let Err(e) = writeln!(writer, "{}", serde_json::to_string(&response)?) {
                error!("Failed to write response: {}", e);
                return Err(e.into());
            }

            if let Err(e) = writer.flush() {
                error!("Failed to flush stdout: {}", e);
                return Err(e.into());
            }
        }

        info!("stdin closed, exiting");
        Ok(())
    }

    fn handle_request(&self, req: Value) -> Value {
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");

        if method == "tools/call" {
            // A tool-execution failure is a valid MCP result (`isError: true`),
            // not a JSON-RPC protocol error — the model needs to see the failure
            // as tool output, or it retries blind with no idea what went wrong.
            let params = req.get("params").cloned().unwrap_or(Value::Null);
            let value = match self.handle_tool_call(params) {
                Ok(value) => value,
                Err(err) => json!({
                    "content": [{"type": "text", "text": err.to_string()}],
                    "isError": true
                }),
            };
            return json!({"jsonrpc": "2.0", "id": id, "result": value});
        }

        let result = match method {
            "initialize" => self.handle_initialize(),
            "tools/list" => self.handle_tools_list(),
            other => {
                warn!("Unknown method: {}", other);
                Err(anyhow::anyhow!("unknown method: {}", other))
            }
        };

        match result {
            Ok(value) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
            Err(msg) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": msg.to_string()}
            }),
        }
    }

    fn handle_initialize(&self) -> Result<Value> {
        Ok(json!({
            "protocolVersion": "2024-11-05",
            "serverInfo": {"name": "filigrio-mcp", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"tools": {}}
        }))
    }

    fn handle_tools_list(&self) -> Result<Value> {
        // Data-plane tools only — the whole served surface is read-only
        // (ADR-0042 F9; see the module docs for the confused-deputy reasoning).
        // Descriptions are
        // written to actually teach a small model the tool's semantics (not just
        // name its arguments) — this is the single biggest lever for tool-calling
        // quality with a weak model, and every `project` field repeats the SAME
        // wording on purpose: leave it out, never guess "." or "" for "current
        // directory" (the daemon's registry has no entry named "."/"" and the
        // lookup fails outright — omitting the field is what actually means
        // "current directory").
        let project_prop = json!({
            "type": "string",
            "description": "A project REGISTERED WITH THE DAEMON: its id, or an absolute path inside its root. Omit this field entirely for the current directory — do NOT pass \".\" or \"\", and do NOT pass a name or root from `project_graph` (those are sub-projects *inside* one indexed repo, not projects the daemon knows); either way the call fails."
        });
        Ok(json!({
                    "tools": [
                        {
                            "name": "query_graph",
                            "description": "Seed-ranked traversal of the graph from search terms — the general-purpose \"find code related to X\" tool. Returns a token-bounded, community-cited list of nodes and edges (NODE .../EDGE ... lines), not raw file contents. Start here when you don't yet know an exact symbol name; once you have one, `get_node`/`get_neighbors` are more precise.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "project": project_prop,
                                    "q": {"type": "string", "description": "Search terms (symbol names, words from an error message, a topic) — not a full sentence."},
                                    "depth": {"type": "integer", "description": "Traversal hops from the seed nodes (default 2)."},
                                    "budget": {"type": "integer", "description": "Max nodes to return (default 32)."},
                                    // ADR-0044: the traversal has always filtered
                                    // edges; it just had no way to *say* which,
                                    // so the caller could neither pick nor see
                                    // the default. The *relation* vocabulary,
                                    // not `get_neighbors`' — a traversal has no
                                    // direction to fold in, so the named
                                    // questions would be values that cannot mean
                                    // anything here, which is the failure an
                                    // enum exists to prevent.
                                    "relations": {"type": "array",
                                                  "items": {"type": "string", "enum": RELATION_FILTER_VOCABULARY},
                                                  "description": "Edge kinds to traverse (OR across entries). `semantic` = code meaning only, the default; `any` = every kind, structural scaffolding (contains/imports) included."}
                                },
                                "required": ["q"]
                            }
                        },
                        {
                            "name": "get_node",
                            "description": "Fetch one specific node by exact address, once you already know its name. A `label` alone may be ambiguous — the SAME name can be defined in multiple files (a homonym), and each definition is a distinct node — so an ambiguous label returns the candidate list (with their `src`) instead of guessing; re-call with `src` or the exact `id` from that list to pick one.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "project": project_prop,
                                    "id": {"type": "string", "description": "Exact node id (the only unconditionally unique address) — e.g. one copied from a prior tool's [id=...] suffix."},
                                    "label": {"type": "string", "description": "Node name to look up. May be ambiguous — see the tool description."},
                                    "src": {"type": "string", "description": "Source file, to disambiguate a homonym `label` (pick the file from the ambiguous-label candidate list)."}
                                },
                                "required": []
                            }
                        },
                        {
                            "name": "get_neighbors",
                            "description": "Everything the graph knows about ONE node's edges. Address it by `id`, or by `label` (+ `src` to disambiguate a homonym, same as `get_node`). ASK BY NAMING THE QUESTION in `relations` — each name already carries its own direction, so you do NOT also need `direction`: `callers` (what calls it) · `callees` (what it calls) · `takes` (what has it as a parameter) · `produces` (what returns it) · `stores` (what has it as a field) · `implementors` · `subtypes` · `supertypes` · `bounded_by`. Entries are OR'd, so name as many as you want. Raw storage kinds still work if you prefer them (`calls`, `imports`, `contains`, `implements`, `extends`, `type`, `type/param`, `type/return`, `type/field`, `type/bound`) — those need `direction` to mean anything, and `direction` defaults to `both`. Two whole-graph values: `semantic` (code meaning only — the default) and `any` (every kind, structural scaffolding included). IMPORTANT: the response always reports *unresolved* edges (calls the graph couldn't bind — opaque receivers, external/builtin types) as a COUNT even when include_unresolved is false — a nonzero unresolved count with an empty resolved list means \"there IS something here, go look\", not \"dead code\". Pass `include_unresolved:true` to get the actual list instead of just the count. A response with NO rows always states the filter and direction it applied, so an empty answer is never silent.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "project": project_prop,
                                    "id": {"type": "string", "description": "Exact node id."},
                                    "label": {"type": "string", "description": "Node name. May be ambiguous — see the tool description."},
                                    "src": {"type": "string", "description": "Source file, to disambiguate a homonym `label`."},
                                    // Kept, and kept an enum — it has been one
                                    // since ADR-0032f, which is why "make
                                    // `direction` an enum" was never the fix
                                    // ADR-0044 held in reserve. It is now the
                                    // *fallback* spelling: the named questions in
                                    // `relations` carry direction themselves, in
                                    // the one field the model reliably fills.
                                    "direction": {"type": "string", "enum": ["in", "out", "both"], "description": "Only needed with a raw storage relation. in = callers/users, out = callees, both = default. A named question in `relations` (callers/callees/takes/produces/stores/…) sets the direction itself and wins over this field."},
                                    // Enumerated, not an open string: a grammar-constrained
                                    // decoder cannot emit a value the schema doesn't list, so
                                    // it can no longer invent "call"/"references" and get a
                                    // silently empty result. One shared vocabulary, no drift
                                    // (`filigrio_client_mcp::neighbor_relation_vocabulary`).
                                    "relations": {"type": "array",
                                                  "items": {"type": "string", "enum": neighbor_relation_vocabulary()},
                                                  "description": "What to ask for. Prefer a NAMED QUESTION — it carries its own direction: callers, callees, takes, produces, stores, implementors, subtypes, supertypes, bounded_by. Or a raw storage kind (`calls`, `type/param`, …), which needs `direction`. `semantic` = code meaning only, the default; `any` = every kind, structural included."},
                                    "include_unresolved": {"type": "boolean", "description": "List the unresolved by-name callers/callees, not just their count (see the tool description)."}
                                },
                                "required": []
                            }
                        },
                        {
                            "name": "god_nodes",
                            "description": "The top-N highest-degree (most-connected) nodes in the graph — the fastest way to find the codebase's central/hub symbols without knowing any names in advance.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "project": project_prop,
                                    "limit": {"type": "integer", "description": "How many nodes to return (default 10)."}
                                },
                                "required": []
                            }
                        },
                        {
                            "name": "list_communities",
                            "description": "The detected communities (clusters of tightly-related nodes), largest first: each row is `id`, its label (the cluster's most-connected member), its size, and its cohesion (how tightly bound it actually is — low cohesion means the cluster is a loose grab-bag). This is the ONLY tool that gives you a community `id`, so call it before `get_community`. Good for \"what are the major subsystems\" without knowing any names in advance.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "project": project_prop,
                                    "limit": {"type": "integer", "description": "How many communities to return, largest first (default 20). The response also reports the untruncated total."}
                                },
                                "required": []
                            }
                        },
                        {
                            "name": "get_community",
                            "description": "The full member list of ONE community, by its numeric id — get the id from `list_communities` (nothing else emits one: the `community=` tag on node lines carries the cluster's label, not its id, so it cannot be passed here). Use this once `list_communities` has shown you a cluster worth expanding.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "project": project_prop,
                                    "community_id": {"type": "integer", "description": "The community id, as returned by `list_communities`."}
                                },
                                "required": ["community_id"]
                            }
                        },
                        {
                            "name": "shortest_path",
                            "description": "Shortest directed call/reference path between two nodes. Address each end by `from`/`to` — an exact id, or a label (+ `from_src`/`to_src` to disambiguate a homonym label, same as `get_node`). An ambiguous label returns the candidate list instead of guessing. Returns null with no path if the two are unconnected within `max_hops`, or if a hop along the way is unresolved (the graph couldn't bind a call) — that's expected, not an error; try `get_neighbors` with `include_unresolved:true` on the last reachable node to see why it stops there.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "project": project_prop,
                                    "from": {"type": "string", "description": "Start node: an id, or a label."},
                                    "from_src": {"type": "string", "description": "Source file to disambiguate 'from' when it's an ambiguous label."},
                                    "to": {"type": "string", "description": "End node: an id, or a label."},
                                    "to_src": {"type": "string", "description": "Source file to disambiguate 'to' when it's an ambiguous label."},
                                    "max_hops": {"type": "integer", "description": "Maximum path length to search (default 8)."}
                                },
                                "required": ["from", "to"]
                            }
                        },
        {
                            "name": "graph_stats",
                            "description": "Node/edge/community counts and confidence mix for a project — a quick health/size check, not a way to enumerate the actual projects in a monorepo (use `project_graph` for that).",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "project": project_prop
                                },
                                "required": []
                            }
                        },
        {
                            "name": "project_graph",
                            "description": "The monorepo architecture map: every sub-project (with its root path and file count) and the depends_on edges between them. This is the tool for \"how many projects are there\" / \"which project is largest\" / \"what does X depend on\" questions — not `graph_stats` (which is per-project, not monorepo-wide) and not `query_graph` (which finds code symbols, not project-level structure). The names and roots it lists describe structure INSIDE the already-indexed repo; they are not values for any tool's `project` argument (that one names a daemon-registered repo — omit it to stay in the current one).",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "project": project_prop
                                },
                                "required": []
                            }
                        }
                    ]
                }))
    }

    fn handle_tool_call(&self, params: Value) -> Result<Value> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing tool name"))?;
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);

        info!("Tool call: {}", name);

        // Enforce the privilege boundary: the data plane, and nothing else
        // (ADR-0042 F9). Every arm below is a read; there is no `_ =>` escape
        // into a mutation, and this build cannot name one — `Command` is not
        // compiled into it (no `control` feature).
        match name {
            "graph_stats" => self.handle_graph_stats(args),
            "query_graph" => self.handle_query_graph(args),
            "get_node" => self.handle_get_node(args),
            "get_neighbors" => self.handle_get_neighbors(args),
            "god_nodes" => self.handle_god_nodes(args),
            "list_communities" => self.handle_list_communities(args),
            "get_community" => self.handle_get_community(args),
            "shortest_path" => self.handle_shortest_path(args),
            "project_graph" => self.handle_project_graph(args),
            other => {
                warn!("Attempted to call privileged tool: {}", other);
                Err(anyhow::anyhow!("tool not allowed: {}", other))
            }
        }
    }

    /// A client for the daemon, **auto-starting** it if nothing is listening
    /// (ADR-0032 §1, ssh-agent style — an agent's first tool call should not
    /// require the user to have run `filigrio daemon start`).
    ///
    /// Hangs off the real `is_reachable` probe, not off a constructor's error
    /// arm — a missing daemon must not surface as a raw "connect failed" from
    /// the first request.
    ///
    /// The handshake runs on its **own thread**: the bridge's request loop is
    /// synchronous but is driven from inside `#[tokio::main]`, and building a
    /// second runtime there panics with "Cannot start a runtime from within a
    /// runtime" — which the unreachable branch hid. Verified by running it.
    fn get_client(&self) -> Result<DaemonClient> {
        let client = DaemonClient::new(&self.socket_path);
        if client.is_reachable() {
            return Ok(client);
        }

        info!("Daemon not running, attempting auto-start");

        let daemon_exe = std::env::current_exe()
            .map(|exe| exe.with_file_name("filigrio-daemon"))
            .unwrap_or_else(|_| PathBuf::from("filigrio-daemon"));

        let config = filigrio_client_core::AutoStartConfig {
            daemon_exe,
            socket_path: self.socket_path.clone(),
            ..Default::default()
        };

        let ready = std::thread::spawn(move || -> Result<bool> {
            tokio::runtime::Runtime::new()
                .context("Failed to create tokio runtime for auto-start")?
                .block_on(
                    filigrio_client_core::AutoStartHandshake::new(config).ensure_daemon_ready(),
                )
                .context("Auto-start handshake failed")
        })
        .join()
        .map_err(|_| anyhow::anyhow!("auto-start thread panicked"))??;

        if !ready {
            return Err(anyhow::anyhow!("Failed to start daemon automatically"));
        }

        Ok(DaemonClient::new(&self.socket_path))
    }

    /// Resolve the `project` a tool call should target. The schema tells the
    /// model to omit `project` for "current directory" (see `handle_tools_list`),
    /// but a model — especially a small one — will still sometimes emit an
    /// explicit sentinel for "current directory" (`"."`, `""`, `"./"`) instead of
    /// omitting the field. The daemon's registry has no entry named `.`/``, so
    /// passing that straight through fails with "project not registered: ." —
    /// a confusing dead end the model then has to burn several tool calls
    /// working around (observed repeatedly in agent-eval transcripts). Treat
    /// those sentinels the same as an absent field instead.
    fn get_project(&self, args: &Value) -> String {
        let explicit = args.get("project").and_then(Value::as_str).map(str::trim);
        match explicit {
            Some(p) if !p.is_empty() && p != "." && p != "./" => p.to_string(),
            _ => resolve_project_path()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "default".to_string()),
        }
    }

    fn handle_graph_stats(&self, args: Value) -> Result<Value> {
        let client = self.get_client()?;
        let project = self.get_project(&args);

        let response = client.send(Request::data(DataQuery::GraphStats {
            project: project.clone(),
        }))?;

        match response {
            Response::QueryResult { data } => {
                let formatted = if let Some(obj) = data.as_object() {
                    let mut lines = vec![];
                    for (key, value) in obj {
                        let line = format!(
                            "{}: {}",
                            format::sanitize(key),
                            format::sanitize(&value.to_string())
                        );
                        lines.push(line);
                    }
                    lines.join("\n")
                } else {
                    format::sanitize(&data.to_string())
                };

                Ok(json!({
                    "content": [{"type": "text", "text": formatted}]
                }))
            }
            Response::Error { message } => Err(anyhow::anyhow!("{}", message)),
            _ => Err(anyhow::anyhow!("unexpected response")),
        }
    }

    fn handle_query_graph(&self, args: Value) -> Result<Value> {
        let client = self.get_client()?;
        let project = self.get_project(&args);

        let query_text = args
            .get("q")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing query"))?;

        let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(2) as u8;
        let budget = args.get("budget").and_then(Value::as_u64).unwrap_or(32) as usize;
        let token_budget = args
            .get("token_budget")
            .and_then(Value::as_u64)
            .unwrap_or(2000) as usize;

        // Passed through verbatim; the daemon normalizes an empty set to
        // `semantic` with the one shared helper (ADR-0044), so this transport
        // has no opinion about what absence means.
        let relations = relation_filter_arg(&args);

        let response = client.send(Request::data(DataQuery::Query {
            project: project.clone(),
            params: filigrio_protocol::QueryParams {
                query: query_text.to_string(),
                mode: filigrio_protocol::TraversalMode::default(),
                depth,
                budget,
                token_budget,
                relations,
                include_unresolved: false,
            },
        }))?;

        match response {
            Response::QueryResult { data } => {
                let formatted = format_subgraph_response(&data, token_budget)?;
                Ok(json!({
                    "content": [{"type": "text", "text": formatted}]
                }))
            }
            Response::Error { message } => Err(anyhow::anyhow!("{}", message)),
            _ => Err(anyhow::anyhow!("unexpected response")),
        }
    }

    fn handle_get_node(&self, args: Value) -> Result<Value> {
        let client = self.get_client()?;
        let project = self.get_project(&args);

        // Extract node address parameters (id, label, src) for ADR-0027 addressing
        let id = args.get("id").and_then(Value::as_str).map(String::from);
        let label = args.get("label").and_then(Value::as_str).map(String::from);
        let src = args.get("src").and_then(Value::as_str).map(String::from);

        if id.is_none() && label.is_none() {
            return Err(anyhow::anyhow!(
                "missing node address: provide either id, or label (with optional src)"
            ));
        }

        let node_address = filigrio_protocol::NodeAddress { id, label, src };

        let response = client.send(Request::data(DataQuery::GetNode {
            project: project.clone(),
            node_address,
        }))?;

        match response {
            Response::QueryResult { data } => {
                let formatted = format_node_response(&data)?;
                Ok(json!({
                    "content": [{"type": "text", "text": formatted}]
                }))
            }
            Response::Error { message } => {
                // Return disambiguation or error information to the client
                Ok(json!({
                    "content": [{"type": "text", "text": format!("Node lookup failed: {}", message)}]
                }))
            }
            _ => Err(anyhow::anyhow!("unexpected response")),
        }
    }

    fn handle_get_neighbors(&self, args: Value) -> Result<Value> {
        let client = self.get_client()?;
        // Every other data-plane tool honors an explicit `project` argument via
        // `get_project` — this one was hardcoded to cwd, silently ignoring the
        // `project` field its own schema advertises. Fixed for consistency.
        let project = self.get_project(&args);

        // Extract node address parameters (id, label, src) for ADR-0027
        // addressing. The daemon resolves the address itself — one round trip.
        let id = args.get("id").and_then(Value::as_str).map(String::from);
        let label = args.get("label").and_then(Value::as_str).map(String::from);
        let src = args.get("src").and_then(Value::as_str).map(String::from);

        if id.is_none() && label.is_none() {
            return Err(anyhow::anyhow!(
                "missing node address: provide either id, or label (with optional src)"
            ));
        }

        let node_address = filigrio_protocol::NodeAddress { id, label, src };

        let requested_direction = args
            .get("direction")
            .and_then(Value::as_str)
            .unwrap_or("both");

        // ADR-0044: a named question is resolved into (relation, direction) here,
        // and the question's direction WINS over an explicit `direction`. The
        // caller who wrote `callers` asked for incoming edges; honouring a
        // stale/mistaken `direction:"out"` beside it would answer a question
        // nobody asked. Named questions that disagree fall back to `both`, which
        // is the union they jointly denote and never drops a row.
        let (relations, direction) =
            resolve_questions(relation_filter_arg(&args), requested_direction);

        let edge_direction = match direction.as_str() {
            "in" | "incoming" => filigrio_protocol::Direction::In,
            "out" | "outgoing" => filigrio_protocol::Direction::Out,
            _ => filigrio_protocol::Direction::Both,
        };

        // Captured *before* the request moves `relations`: an empty result has to
        // be able to name the filter that produced it (see `no_match_note`).
        let asked = NeighborAsk {
            relations: relations.clone(),
            direction: direction.clone(),
        };

        let include_unresolved = args
            .get("include_unresolved")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let response = client.send(Request::data(DataQuery::Neighbors {
            project: project.clone(),
            node: node_address,
            direction: edge_direction,
            relations,
            include_unresolved,
        }))?;

        match response {
            Response::QueryResult { data } => {
                // The daemon echoes the resolved node's concrete id in `node_id`.
                let node_id = data
                    .get("node_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let formatted =
                    format_neighbors_response(&data, &node_id, include_unresolved, &asked)?;
                Ok(json!({
                    "content": [{"type": "text", "text": formatted}]
                }))
            }
            Response::Error { message } => {
                // Same convention as get_node: an ambiguous-address error is
                // useful tool output, not a hard failure.
                Ok(json!({
                    "content": [{"type": "text", "text": format!("Neighbors lookup failed: {}", message)}]
                }))
            }
            _ => Err(anyhow::anyhow!("unexpected response")),
        }
    }

    fn handle_god_nodes(&self, args: Value) -> Result<Value> {
        let client = self.get_client()?;
        let project = self.get_project(&args);

        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(10);

        let response = client.send(Request::data(DataQuery::GodNodes {
            project: project.clone(),
            limit,
        }))?;

        match response {
            Response::QueryResult { data } => {
                let formatted = format_god_nodes_response(&data)?;
                Ok(json!({
                    "content": [{"type": "text", "text": formatted}]
                }))
            }
            Response::Error { message } => Err(anyhow::anyhow!("{}", message)),
            _ => Err(anyhow::anyhow!("unexpected response")),
        }
    }

    fn handle_list_communities(&self, args: Value) -> Result<Value> {
        let client = self.get_client()?;
        let project = self.get_project(&args);

        // Mirrors `god_nodes`' default: a bounded top-N, because clustering
        // routinely yields hundreds of communities (321 on this repo).
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(20);

        let response = client.send(Request::data(DataQuery::ListCommunities {
            project: project.clone(),
            limit,
        }))?;

        match response {
            Response::QueryResult { data } => {
                let formatted = format_list_communities_response(&data)?;
                Ok(json!({
                    "content": [{"type": "text", "text": formatted}]
                }))
            }
            Response::Error { message } => Err(anyhow::anyhow!("{}", message)),
            _ => Err(anyhow::anyhow!("unexpected response")),
        }
    }

    fn handle_get_community(&self, args: Value) -> Result<Value> {
        let client = self.get_client()?;
        let project = self.get_project(&args);

        let community_id = args
            .get("community_id")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("missing community_id"))?;

        let response = client.send(Request::data(DataQuery::Community {
            project: project.clone(),
            community_id,
        }))?;

        match response {
            Response::QueryResult { data } => {
                let formatted = format_community_response(&data)?;
                Ok(json!({
                    "content": [{"type": "text", "text": formatted}]
                }))
            }
            Response::Error { message } => Err(anyhow::anyhow!("{}", message)),
            _ => Err(anyhow::anyhow!("unexpected response")),
        }
    }

    fn handle_shortest_path(&self, args: Value) -> Result<Value> {
        let client = self.get_client()?;
        let project = self.get_project(&args);

        let from = args
            .get("from")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing from node"))?;

        let to = args
            .get("to")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing to node"))?;

        // `from`/`to` are id-or-label (ADR-0028 ids always contain ':');
        // `from_src`/`to_src` disambiguate a homonym label the same way
        // get_node/get_neighbors' `src` does.
        let from_src = args
            .get("from_src")
            .and_then(Value::as_str)
            .map(String::from);
        let to_src = args.get("to_src").and_then(Value::as_str).map(String::from);

        let max_hops = args
            .get("max_hops")
            .and_then(Value::as_u64)
            .map(|v| v as u8)
            .unwrap_or(8);

        let response = client.send(Request::data(DataQuery::Path {
            project: project.clone(),
            from: filigrio_protocol::NodeAddress::parse(from).with_src(from_src),
            to: filigrio_protocol::NodeAddress::parse(to).with_src(to_src),
            max_hops,
        }))?;

        match response {
            Response::QueryResult { data } => {
                let formatted = format_path_response(&data)?;
                Ok(json!({
                    "content": [{"type": "text", "text": formatted}]
                }))
            }
            Response::Error { message } => Err(anyhow::anyhow!("{}", message)),
            _ => Err(anyhow::anyhow!("unexpected response")),
        }
    }

    fn handle_project_graph(&self, args: Value) -> Result<Value> {
        let client = self.get_client()?;
        let project = self.get_project(&args);

        let response = client.send(Request::data(DataQuery::ProjectGraph {
            project: project.clone(),
        }))?;

        match response {
            Response::QueryResult { data } => {
                let formatted = format_project_graph_response(&data)?;
                Ok(json!({
                    "content": [{"type": "text", "text": formatted}]
                }))
            }
            Response::Error { message } => Err(anyhow::anyhow!("{}", message)),
            _ => Err(anyhow::anyhow!("unexpected response")),
        }
    }
}

// ADR-0042 F9: `handle_register_project`/`handle_index_project` and the
// `describe_outcome` renderer they fed are gone with the tools. The bridge
// never receives a `Response::CommandCompleted` anymore — it sends no commands
// — so the `_ => "unexpected response"` arm on every read handler is now the
// only thing that could see one, which is exactly right: a command outcome
// arriving on this connection would mean something is very wrong.

/// The `relations` filter argument, read the same way on every tool that takes
/// one. Deliberately **not** normalized here: what an empty/absent set means is
/// the daemon's one decision (`filigrio_core::relation::filter::normalize`,
/// ADR-0044), and the divergence this closes was two call sites each deciding
/// it for themselves.
fn relation_filter_arg(args: &Value) -> Vec<String> {
    args.get("relations")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// The `(impl Owner) [src=… loc=… id=…]` self-addressing suffix (mirrors the
/// classic `filigrio-classic mcp serve` — `filigrio_client_mcp::id_suffix` — so both
/// front-ends read the same to a model, ADR-0027).
fn id_suffix(n: &Node) -> String {
    let owner = n
        .attrs
        .get("impl")
        .map(|o| format!(" (impl {})", format::sanitize(o)))
        .unwrap_or_default();
    format!(
        "{owner} [src={} loc={} id={}]",
        format::sanitize(n.source_file.as_deref().unwrap_or("")),
        format::sanitize(&n.loc()),
        format::sanitize(&n.id.0),
    )
}

/// `query_graph` / `get_neighbors` (relations-filtered) return a `Subgraph`
/// (`{nodes, edges, communities}`, `filigrio_core::Subgraph`'s own wire shape) —
/// deserialize into the real types and reuse the same `format::node_line`/
/// `edge_line` the classic server's `query_text` uses, so body output matches.
fn format_subgraph_response(data: &Value, token_budget: usize) -> Result<String> {
    let sub: Subgraph =
        serde_json::from_value(data.clone()).context("decoding subgraph response")?;

    let mut body = String::new();
    for n in &sub.nodes {
        let comm = sub.communities.get(&n.id).map(String::as_str).unwrap_or("");
        body.push_str(&format::node_line(n, comm));
        body.push('\n');
    }
    for e in &sub.edges {
        if let EdgeTarget::Node(t) = &e.target {
            let src = sub.nodes.iter().find(|n| n.id == e.source);
            let dst = sub.nodes.iter().find(|n| &n.id == t);
            if let (Some(s), Some(d)) = (src, dst) {
                body.push_str(&format::edge_line(
                    &s.label,
                    &e.relation,
                    e.confidence,
                    &d.label,
                ));
                body.push('\n');
            }
        }
    }

    let header = format!("Traversal: {} nodes found\n\n", sub.nodes.len());
    Ok(format!(
        "{header}{}",
        format::budget_cut(body, token_budget)
    ))
}

/// `ProjectGraph` returns monorepo sub-projects, not a node/edge subgraph —
/// `{"projects": [{"root","name","manifest","files","depends_on"}, ...]}`.
/// Wording mirrors the classic server's `project_graph_text`.
fn format_project_graph_response(data: &Value) -> Result<String> {
    let obj = data
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("expected an object"))?;
    let projects = obj
        .get("projects")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("missing projects"))?;

    if projects.is_empty() {
        return Ok("No projects — the tree has no package/module manifests.".to_string());
    }

    let label_of = |root: &str| -> String {
        projects
            .iter()
            .filter_map(Value::as_object)
            .find(|p| p.get("root").and_then(Value::as_str) == Some(root))
            .map(|p| {
                p.get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| root.to_string())
            })
            .unwrap_or_else(|| root.to_string())
    };

    // The header says whose projects these are, because the word collides: these
    // are ADR-0019 sub-projects *within* the indexed repo, while the `project`
    // tool argument names a daemon-**registered** repo. An agent-eval run read
    // a row here as a `project` value and spent 4 of its 10 steps failing on it.
    let mut body = format!(
        "Projects: {} (sub-projects inside this repo; project → depends_on). \
         These names/roots are not `project` argument values.\n",
        projects.len()
    );
    for project in projects {
        let Some(p) = project.as_object() else {
            continue;
        };
        let root = p.get("root").and_then(Value::as_str).unwrap_or("");
        let name = p.get("name").and_then(Value::as_str).unwrap_or(root);
        let files = p.get("files").and_then(Value::as_u64).unwrap_or(0);
        let deps: Vec<String> = p
            .get("depends_on")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|r| format::sanitize(&label_of(r)))
                    .collect()
            })
            .unwrap_or_default();
        let dep_str = if deps.is_empty() {
            String::new()
        } else {
            format!(" → {}", deps.join(", "))
        };

        body.push_str(&format!(
            "PROJECT {} [root={} files={}]{}\n",
            format::sanitize(name),
            format::sanitize(root),
            files,
            dep_str,
        ));
    }

    Ok(body)
}

/// `get_node` returns a raw `Node` (community lives in `attrs["community"]`,
/// stamped by `GraphView::stamp_community`, not a top-level field). Wording
/// mirrors the classic server's `node_text`.
fn format_node_response(data: &Value) -> Result<String> {
    let node: Node = serde_json::from_value(data.clone()).context("decoding node response")?;
    let community = node
        .attrs
        .get("community")
        .map(String::as_str)
        .unwrap_or("");
    Ok(format!(
        "Node: {}{}\n  Type: {}\n  Community: {}",
        format::sanitize(&node.label),
        id_suffix(&node),
        format::sanitize(&node.kind),
        format::sanitize(community),
    ))
}

/// `get_neighbors` returns `{node_id, direction, resolved_neighbors: [{edge,
/// node}], unresolved_neighbors: [[edge, node]], unresolved_count}` — not a
/// `Subgraph`. Wording mirrors the classic server's `neighbors_text` +
/// `append_unresolved` (ADR-0029 honesty: counts by default, full list on
/// `include_unresolved`).
/// Translate ADR-0044's **named questions** into the storage vocabulary.
///
/// Returns `(relations, direction)` where every named entry has been replaced by
/// the relation it denotes, and the direction is the one those names agree on.
/// Plain relations and the filter words (`semantic`/`any`) pass through untouched,
/// so this is additive: every argument shape that worked before still works.
///
/// Direction resolution, in order:
/// - no named question → the caller's `direction` verbatim (default `both`);
/// - one or more named questions that agree → their direction, overriding
///   `direction`, because the question *is* the direction;
/// - named questions that disagree (`["callers","callees"]`) → `both`, the union
///   they jointly denote. Picking one would silently drop half the answer.
fn resolve_questions(relations: Vec<String>, direction: &str) -> (Vec<String>, String) {
    let mut out = Vec::with_capacity(relations.len());
    let mut asked: Option<filigrio_client_mcp::Dir> = None;
    let mut conflict = false;

    for entry in relations {
        match named_question(&entry) {
            Some((relation, dir)) => {
                match asked {
                    Some(prev) if prev != dir => conflict = true,
                    _ => asked = Some(dir),
                }
                let relation = relation.to_string();
                if !out.contains(&relation) {
                    out.push(relation);
                }
            }
            None => {
                if !out.contains(&entry) {
                    out.push(entry);
                }
            }
        }
    }

    let resolved = match (asked, conflict) {
        (_, true) => "both".to_string(),
        (Some(filigrio_client_mcp::Dir::In), _) => "in".to_string(),
        (Some(filigrio_client_mcp::Dir::Out), _) => "out".to_string(),
        (None, _) => direction.to_string(),
    };
    (out, resolved)
}

/// What the caller actually asked for, kept so an **empty** answer can say which
/// filter produced it. See [`no_match_note`].
struct NeighborAsk {
    relations: Vec<String>,
    direction: String,
}

/// The line a zero-row neighbor response must carry.
///
/// A bare `Neighbors of <id>:` is indistinguishable from "this node has no
/// edges", "your relation filter kept none of them" and "the server did not
/// understand the filter word you named" — and an agent-eval run has already been
/// lost to the third: 33 of 33 `relations:["semantic"]` calls came back as that
/// one header line, the model concluded *"the graph does not contain any call
/// edges"*, and nothing in the response contradicted it. This is ADR-0029's rule
/// (a surface must not decide something the caller did not ask for, silently) and
/// ADR-0044's (anything that changes the answer must be nameable) applied to the
/// **response** rather than the request: an empty result states the filter it
/// applied and how to widen it.
fn no_match_note(ask: &NeighborAsk) -> String {
    let filter = if ask.relations.is_empty() {
        format!("{} (the default)", FILTER_SEMANTIC)
    } else {
        ask.relations.join(", ")
    };
    format!(
        "\n  no edges matched — direction={}, relations=[{}]. \
         Widen with relations:[\"{}\"], or a different direction; \
         an unrecognised relation name also lands here.",
        ask.direction,
        format::sanitize(&filter),
        FILTER_ANY,
    )
}

fn format_neighbors_response(
    data: &Value,
    node_id: &str,
    include_unresolved: bool,
    ask: &NeighborAsk,
) -> Result<String> {
    let obj = data
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("expected an object"))?;
    let resolved = obj
        .get("resolved_neighbors")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("missing resolved_neighbors"))?;

    let mut out = format!("Neighbors of {}:", format::sanitize(node_id));
    let header_len = out.len();
    for entry in resolved {
        let Some(entry_obj) = entry.as_object() else {
            continue;
        };
        let edge: Edge = match entry_obj.get("edge").cloned().map(serde_json::from_value) {
            Some(Ok(e)) => e,
            _ => continue,
        };
        let node: Node = match entry_obj.get("node").cloned().map(serde_json::from_value) {
            Some(Ok(n)) => n,
            _ => continue,
        };
        // `-->` = this node points at the neighbor (outgoing); `<--` = the
        // neighbor points at this node (incoming — "what calls it").
        let arrow = if edge.source.0 == node_id {
            "-->"
        } else {
            "<--"
        };
        out.push_str(&format!(
            "\n  {arrow} {}{} [{}] [{}]",
            format::sanitize(&node.label),
            id_suffix(&node),
            format::sanitize(&edge.relation),
            format::confidence(edge.confidence),
        ));
    }

    let unresolved_count = obj
        .get("unresolved_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if unresolved_count > 0 {
        if include_unresolved {
            if let Some(unresolved) = obj.get("unresolved_neighbors").and_then(Value::as_array) {
                for pair in unresolved {
                    let Some(arr) = pair.as_array() else { continue };
                    if arr.len() != 2 {
                        continue;
                    }
                    let edge: Option<Edge> = serde_json::from_value(arr[0].clone()).ok();
                    let stub: Option<Node> = serde_json::from_value(arr[1].clone()).ok();
                    let (Some(edge), Some(stub)) = (edge, stub) else {
                        continue;
                    };

                    // Determine direction: edge.source matches queried node = outgoing, incoming if not
                    let is_outgoing = edge.source.0 == node_id;

                    // Check for opaque receiver (ADR-0023 marker)
                    let is_opaque = matches!(&edge.target, EdgeTarget::Symbol(r) if r.hints.get("recv").map(String::as_str) == Some("opaque"));

                    // The addressability split — the whole point of the row.
                    //
                    // *Incoming*: the caller IS a real node (the daemon sends it
                    // verbatim), so it keeps the `[src= loc= id=]` handle and can be
                    // cited and re-queried.
                    //
                    // *Outgoing*: the callee is a bare `Symbol` name the graph
                    // declined to bind — there is **no node**. The daemon still has to
                    // put *something* in the wire's `Node` slot, so it sends a stub
                    // with a synthetic `unresolved-<name>` id; rendering that stub
                    // through `id_suffix` handed the model an `id=` that looks exactly
                    // like a real one, and a model taught "cite ids" duly cited
                    // `unresolved-Array` — a fabrication the grounding gate correctly
                    // failed. What has no address must LOOK unaddressable, which is
                    // ADR-0029's own honesty principle applied to its own rendering
                    // (and is what the in-process server has always done).
                    if is_outgoing {
                        let name = match &edge.target {
                            EdgeTarget::Symbol(r) => r.name.as_str(),
                            EdgeTarget::Node(_) => stub.label.as_str(),
                        };
                        let why = if is_opaque {
                            " (opaque receiver — read this node's body to confirm)"
                        } else {
                            ""
                        };
                        out.push_str(&format!(
                            "\n  --> {} [{}] [UNRESOLVED]{}",
                            format::sanitize(name),
                            format::sanitize(&edge.relation),
                            why,
                        ));
                    } else {
                        // Incoming unresolved: caller information is in the stub node,
                        // not edge.target (which points at the queried node itself).
                        out.push_str(&format!(
                            "\n  <-- {}{} [{}] [UNRESOLVED, by-name]",
                            format::sanitize(&stub.label),
                            id_suffix(&stub),
                            format::sanitize(&edge.relation),
                        ));
                    }
                }
            }
        } else {
            out.push_str(&format!(
                "\n  unresolved: {unresolved_count} — pass include_unresolved:true to list them",
            ));
        }
    }

    // Nothing was appended: the answer is empty, and it must say why.
    if out.len() == header_len {
        out.push_str(&no_match_note(ask));
    }

    Ok(out)
}

/// `god_nodes` returns `{project, limit, god_nodes: [{node, degree}]}`.
/// Wording mirrors the classic server's `god_text`.
fn format_god_nodes_response(data: &Value) -> Result<String> {
    let obj = data
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("expected an object"))?;
    let entries = obj
        .get("god_nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("missing god_nodes"))?;

    let mut out = String::from("God nodes (most connected):");
    for (i, entry) in entries.iter().enumerate() {
        let Some(entry_obj) = entry.as_object() else {
            continue;
        };
        let node: Node = match entry_obj.get("node").cloned().map(serde_json::from_value) {
            Some(Ok(n)) => n,
            _ => continue,
        };
        let degree = entry_obj.get("degree").and_then(Value::as_u64).unwrap_or(0);
        out.push_str(&format!(
            "\n  {}. {}{} - {} edges",
            i + 1,
            format::sanitize(&node.label),
            id_suffix(&node),
            degree
        ));
    }
    Ok(out)
}

/// `list_communities` returns `{project, total, limit, communities: [{id, label,
/// size, cohesion}]}` — no rosters (that is `get_community`, one id at a time).
/// The `id` is spelled out per row because it is the **only** place a model can
/// obtain one, and `get_community` is unreachable without it.
fn format_list_communities_response(data: &Value) -> Result<String> {
    let obj = data
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("expected an object"))?;
    let rows = obj
        .get("communities")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("missing communities"))?;

    if rows.is_empty() {
        return Ok("No communities — the graph has no clustering partition.".to_string());
    }

    let total = obj.get("total").and_then(Value::as_u64).unwrap_or(0);
    let shown = rows.len() as u64;
    // Say when the list is cut: a capped list that reads as the whole set is
    // exactly the silent-truncation lie ADR-0029 exists to avoid.
    let header = if shown < total {
        format!("Communities: {shown} of {total} (largest first):")
    } else {
        format!("Communities: {total} (largest first):")
    };

    let mut out = header;
    for row in rows {
        let Some(c) = row.as_object() else { continue };
        let id = c.get("id").and_then(Value::as_u64).unwrap_or(0);
        let label = c.get("label").and_then(Value::as_str).unwrap_or("");
        let size = c.get("size").and_then(Value::as_u64).unwrap_or(0);
        let cohesion = c.get("cohesion").and_then(Value::as_f64).unwrap_or(0.0);
        out.push_str(&format!(
            "\n  {} [id={id}] - {size} nodes, cohesion {cohesion:.2}",
            format::sanitize(label),
        ));
    }
    Ok(out)
}

/// `get_community` returns `{community_id, name, cohesion, members: Vec<Node>}`.
/// Wording mirrors the classic server's `community_text`.
fn format_community_response(data: &Value) -> Result<String> {
    let obj = data
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("expected an object"))?;
    let community_id = obj.get("community_id").and_then(Value::as_u64).unwrap_or(0);
    let members: Vec<Node> = obj
        .get("members")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .context("decoding community members")?
        .unwrap_or_default();

    if members.is_empty() {
        return Ok(format!("Community {community_id} not found."));
    }

    let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
    let base = format!("Community {community_id}");
    let header = if !name.is_empty() && name != base {
        format!("{base} — {}", format::sanitize(name))
    } else {
        base
    };
    let cohesion = obj
        .get("cohesion")
        .and_then(Value::as_f64)
        .map(|c| format!(", cohesion {c:.2}"))
        .unwrap_or_default();

    let mut out = format!("{header} ({} nodes{cohesion}):", members.len());
    for n in &members {
        out.push_str(&format!(
            "\n  {}{}",
            format::sanitize(&n.label),
            id_suffix(n)
        ));
    }
    Ok(out)
}

/// `shortest_path` returns `{from, to, max_hops, path: Vec<Node> | null,
/// hop_count, message?}` — no per-hop edge/relation data (that would need an
/// extra `neighbors` round trip per hop the daemon doesn't do), so unlike the
/// classic server's `path_text` this shows the node chain without relation
/// labels on each arrow.
fn format_path_response(data: &Value) -> Result<String> {
    let obj = data
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("expected an object"))?;

    if obj.get("path").map(Value::is_null).unwrap_or(true) {
        let message = obj
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("No path found.");
        return Ok(format::sanitize(message));
    }

    let path: Vec<Node> = obj
        .get("path")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .context("decoding path nodes")?
        .unwrap_or_default();
    let hop_count = obj.get("hop_count").and_then(Value::as_u64).unwrap_or(0);

    let labels: Vec<String> = path.iter().map(|n| format::sanitize(&n.label)).collect();
    Ok(format!(
        "Shortest path ({hop_count} hops):\n  {}",
        labels.join(" --> ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> McpBridge {
        McpBridge::new(PathBuf::from("/tmp/does-not-matter.sock"))
    }

    /// agent-eval transcripts repeatedly showed a small model passing `"."` or
    /// `""` for `project` (a natural reading of the schema's "defaults to
    /// current directory" wording) — the daemon's registry has no entry named
    /// `.`/``, so that used to fail with "project not registered: ." and burn
    /// several recovery tool calls. These sentinels must resolve exactly like
    /// an omitted field.
    #[test]
    fn get_project_treats_dot_and_empty_string_as_omitted() {
        let b = bridge();
        let cwd = resolve_project_path()
            .unwrap()
            .to_string_lossy()
            .to_string();

        assert_eq!(b.get_project(&json!({})), cwd);
        assert_eq!(b.get_project(&json!({"project": "."})), cwd);
        assert_eq!(b.get_project(&json!({"project": "./"})), cwd);
        assert_eq!(b.get_project(&json!({"project": ""})), cwd);
        assert_eq!(b.get_project(&json!({"project": "  "})), cwd);
    }

    /// A genuine project id/path must pass through untouched — only the
    /// "current directory" sentinels get normalized.
    #[test]
    fn get_project_passes_through_a_real_value() {
        let b = bridge();
        assert_eq!(
            b.get_project(&json!({"project": "reflex_monorepo"})),
            "reflex_monorepo"
        );
        assert_eq!(b.get_project(&json!({"project": "/abs/path"})), "/abs/path");
    }

    /// `include_unresolved` was dispatchable but missing from the advertised
    /// schema — a standard MCP client (and any grammar-constrained model
    /// relying on it) could never emit it. Locks the fix in.
    ///
    /// The served tool *list* is pinned separately, by
    /// `the_bridge_serves_only_read_only_tools` below.
    #[test]
    fn tools_list_advertises_include_unresolved() {
        let b = bridge();
        let tools = b.handle_tools_list().unwrap();

        let neighbors = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "get_neighbors")
            .unwrap();
        assert!(
            neighbors["inputSchema"]["properties"]["include_unresolved"].is_object(),
            "get_neighbors schema: {neighbors}"
        );
    }

    /// `relations` was an **open** `{"type": "string"}` array — nothing for a
    /// grammar-constrained decoder to constrain to, so a small model emits
    /// `"call"` / `"references"` and gets a silently empty result. Same class as
    /// the `include_unresolved` regression above, one field over.
    ///
    /// Two things are pinned. The enum exists, carrying the ADR-0036 `type/…`
    /// members by their *constants* (so the hierarchical rename cannot drift out
    /// of the wire surface). And the bare family spelling `type` is **present**:
    /// `GraphView::neighbors_at` matches through `relation_matches_filter`, so
    /// `relations:["type"]` returns the union of the family — verified
    /// end-to-end against a real index over this bridge. It was withheld for
    /// exactly as long as the matcher was `==`; a schema value that returns an
    /// empty list is the failure this enum exists to prevent, and so is a
    /// working query the model is forbidden to spell.
    #[test]
    fn tools_list_enumerates_the_relation_vocabulary() {
        let b = bridge();
        let tools = b.handle_tools_list().unwrap();
        let neighbors = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "get_neighbors")
            .unwrap()
            .clone();

        let items = &neighbors["inputSchema"]["properties"]["relations"]["items"];
        assert_eq!(items["type"], json!("string"));
        let values = items["enum"].as_array().expect("relations is an enum");

        for expected in [
            filigrio_core::relation::CALLS,
            filigrio_core::relation::IMPLEMENTS,
            filigrio_core::relation::PARAM_TYPE,
            filigrio_core::relation::RETURN_TYPE,
            filigrio_core::relation::FIELD_TYPE,
            filigrio_core::relation::BOUND_TYPE,
            // "what uses this type at all" — one call, not four unioned by hand,
            // and closed under a future family member (ADR-0036 R2.1).
            filigrio_client_mcp::TYPE_FAMILY,
        ] {
            assert!(
                values.contains(&json!(expected)),
                "{expected} must be nameable on the wire: {values:?}"
            );
        }
        // One shared vocabulary with the library server, so the two schema
        // declaration sites cannot drift apart.
        assert_eq!(
            values.clone(),
            neighbor_relation_vocabulary()
                .into_iter()
                .map(Value::from)
                .collect::<Vec<_>>(),
            "the bridge renders the shared vocabulary verbatim"
        );
    }

    /// **ADR-0044: no meaningful absences.** `any` and `semantic` are only
    /// worth anything if a grammar-constrained decoder can *select* them — it
    /// cannot reason its way to "if I omit this field, structural edges get
    /// dropped", which is exactly how 8 of 31 `get_neighbors` calls in one eval
    /// run silently received a filtered result.
    ///
    /// So: every tool that advertises `relations` advertises both words in its
    /// enum, and it advertises them from the shared vocabulary rather than a
    /// re-typed literal. `query_graph` is in that set now — its traversal has
    /// always filtered edges; it just had no way to say which, so the caller
    /// could neither pick the filter nor see the default.
    #[test]
    fn every_relations_enum_offers_any_and_semantic() {
        let b = bridge();
        let tools = b.handle_tools_list().unwrap();

        let mut advertised = Vec::new();
        for tool in tools["tools"].as_array().unwrap() {
            let Some(prop) = tool["inputSchema"]["properties"].get("relations") else {
                continue;
            };
            let name = tool["name"].as_str().unwrap().to_string();
            let values = prop["items"]["enum"]
                .as_array()
                .unwrap_or_else(|| panic!("{name}'s relations must be an enum: {prop}"));
            for word in filigrio_core::relation::filter::VOCABULARY {
                assert!(
                    values.contains(&json!(word)),
                    "{name} must let a decoder select `{word}`: {values:?}"
                );
            }
            // One vocabulary per surface, rendered from the shared slice rather
            // than a re-typed literal — so neither site can drift from
            // `filigrio_core::relation`. The two surfaces differ by exactly the
            // ADR-0044 named questions, which only `get_neighbors` can answer
            // (a traversal has no direction to fold in).
            let expected: Vec<Value> = if name == "get_neighbors" {
                neighbor_relation_vocabulary()
                    .into_iter()
                    .map(Value::from)
                    .collect()
            } else {
                RELATION_FILTER_VOCABULARY
                    .iter()
                    .copied()
                    .map(Value::from)
                    .collect()
            };
            assert_eq!(
                values.clone(),
                expected,
                "{name} renders the shared vocabulary verbatim"
            );
            // The description names the values rather than documenting what
            // absence does — the point of promoting them out of prose.
            let desc = prop["description"].as_str().unwrap_or_default();
            assert!(
                desc.contains("`any`") && desc.contains("`semantic`"),
                "{name}'s relations description must name the two values: {desc}"
            );
            advertised.push(name);
        }
        advertised.sort();
        assert_eq!(
            advertised,
            vec!["get_neighbors".to_string(), "query_graph".to_string()],
            "both filtering tools advertise the filter — the divergence was that \
             only one of them let the caller see it"
        );
    }

    /// The nine read-only tools the bridge is allowed to serve (ADR-0042 F9).
    /// Mirrored — deliberately, as a second copy — by
    /// `filigrio-protocol/tests/vocabulary.rs`'s `MCP_TOOLS`, which is the side
    /// of the wall where `Command` is in scope and can check that none of these
    /// is a mutation, and by `tools/agent_eval/src/mcp_test.ts`, which asserts
    /// the same list end-to-end over real JSON-RPC.
    const READ_ONLY_TOOLS: &[&str] = &[
        "get_community",
        "get_neighbors",
        "get_node",
        "god_nodes",
        "graph_stats",
        "list_communities",
        "project_graph",
        "query_graph",
        "shortest_path",
    ];

    /// ADR-0042 F9 — **the MCP surface is read-only; the mutation plane left
    /// the bridge.** This is the security invariant, checked on the side of the
    /// wall where the *real* served list is in scope.
    ///
    /// Why it is worth a test rather than a review habit: the bridge runs with
    /// the **user's** filesystem permissions, not the agent's. Every mutation
    /// it exposes is a confused deputy — `register_project` let an agent make a
    /// project queryable through the graph that it may have no right to read.
    /// MCP **roots** is the containment that would make an agent-scoped
    /// mutation safe, and it is not built; until it is, the honest posture is
    /// no mutation surface at all, and a bridge with no command path cannot be
    /// confused into anything.
    ///
    /// Three layers, weakest to strongest:
    /// 1. the served list is **exactly** the eight read-only tools;
    /// 2. no served name matches the F7 mutation shape `<verb>_project` (the
    ///    mechanical spelling of a `Command::Project<Verb>` mask) — so a future
    ///    tool that reintroduces one fails here even if someone also updates
    ///    the list above;
    /// 3. the removed names are not merely unadvertised but **undispatchable**
    ///    (a hidden-but-callable tool is the same confused deputy, minus the
    ///    documentation).
    ///
    /// Backstopping all three: this crate is built without the `control`
    /// feature (see `Cargo.toml`), so `Command` does not exist in this binary
    /// at all — the mutation vocabulary is unrepresentable, not just unused.
    #[test]
    fn the_bridge_serves_only_read_only_tools() {
        let b = bridge();
        let tools = b.handle_tools_list().unwrap();
        let mut names: Vec<String> = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        names.sort();

        assert_eq!(
            names, READ_ONLY_TOOLS,
            "F9: the bridge must serve exactly the read-only data-plane tools"
        );

        // `get_community` is reachable only if some tool emits a numeric id, and
        // `list_communities` is the only one that does — the `community=` node
        // attr carries the derived *label*. Pin the pairing so a future edit
        // cannot drop the enumeration and leave `get_community` guessable-only
        // again (a grammar-constrained model cannot put the label it saw into
        // an integer field).
        let by_name = |n: &str| -> Value {
            tools["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == n)
                .cloned()
                .unwrap()
        };
        assert!(
            by_name("get_community")["description"]
                .as_str()
                .unwrap()
                .contains("list_communities"),
            "get_community must say where its id comes from"
        );
        assert!(
            by_name("list_communities")["inputSchema"]["properties"]["limit"].is_object(),
            "list_communities must advertise `limit` — an unbounded dump of 321 \
             communities is a token-budget hazard, and an undeclared field is \
             unemittable under grammar-constrained decoding"
        );

        // The F7 rule run backwards: a mutation mask is spelled `<verb>_project`.
        // Nothing served may match it — that is the rule executed, not restated.
        let mutations: Vec<&String> = names
            .iter()
            .filter(|n| n.strip_suffix("_project").is_some())
            .collect();
        assert!(
            mutations.is_empty(),
            "F9: {mutations:?} is spelled like a `Command::Project<Verb>` mask — \
             the bridge must expose no mutation (confused-deputy: it holds the \
             user's permissions, not the agent's)"
        );

        // Removed means unreachable, not just unlisted: dispatch must refuse.
        for gone in ["register_project", "index_project"] {
            let err = b
                .handle_tool_call(json!({"name": gone, "arguments": {}}))
                .expect_err("a removed mutation tool must not dispatch");
            assert!(
                err.to_string().contains("not allowed"),
                "F9: `{gone}` must be refused at dispatch, got: {err}"
            );
        }
    }

    // ---- the two `project` words must not read as one ----------------------
    //
    // `project_graph` lists ADR-0019 sub-projects *inside* the indexed repo; the
    // `project` argument names a daemon-**registered** repo. An agent read the
    // former as the latter and burned 4 of 10 steps. Both surfaces now say so —
    // the schema before the mistake, the daemon's error after it.

    #[test]
    fn the_project_argument_says_it_means_a_registered_project() {
        let b = bridge();
        let tools = b.handle_tools_list().unwrap();
        for tool in tools["tools"].as_array().unwrap() {
            let Some(prop) = tool["inputSchema"]["properties"].get("project") else {
                continue;
            };
            let desc = prop["description"].as_str().unwrap();
            assert!(
                desc.contains("REGISTERED") && desc.contains("project_graph"),
                "{}'s `project` must name the accepted vocabulary and warn off \
                 project_graph's rows: {desc}",
                tool["name"]
            );
        }
    }

    #[test]
    fn project_graph_header_disambiguates_from_the_project_argument() {
        let text = format_project_graph_response(&json!({
            "projects": [{"root": "apps/signalling", "name": "signalling", "files": 13,
                          "depends_on": []}]
        }))
        .unwrap();
        assert!(text.starts_with("Projects: 1"), "{text}");
        assert!(
            text.contains("not `project` argument values"),
            "the row an agent misread must carry its own disclaimer: {text}"
        );
        assert!(
            text.contains("PROJECT signalling [root=apps/signalling files=13]"),
            "the map itself is unchanged: {text}"
        );
    }

    // ---- unresolved rows are not addressable (ADR-0029) --------------------

    fn node(id: &str, label: &str, src: &str, line: u32) -> Node {
        Node {
            id: filigrio_core::NodeId::new(id),
            label: label.to_string(),
            kind: "function".to_string(),
            source_span: Some(filigrio_core::Span::line(line)),
            source_file: Some(src.to_string()),
            attrs: Default::default(),
        }
    }

    /// The daemon's own `unresolved_neighbors` wire shape: `[[edge, node], …]`,
    /// where the node for an *outgoing* declined edge is the responder's stub
    /// (`create_stub_node_for_unresolved`) — synthetic id, no file, no span.
    fn stub(name: &str) -> Node {
        Node {
            id: filigrio_core::NodeId::new(format!("unresolved-{name}")),
            label: format!("unresolved: {name}"),
            kind: "unresolved".to_string(),
            source_span: None,
            source_file: None,
            attrs: Default::default(),
        }
    }

    fn sym_edge(source: &str, relation: &str, name: &str) -> Edge {
        Edge {
            source: filigrio_core::NodeId::new(source),
            relation: relation.to_string(),
            confidence: filigrio_core::Confidence::Extracted,
            target: EdgeTarget::Symbol(filigrio_core::TargetRef::new(name)),
        }
    }

    /// A neighbors payload for `n:me`: one resolved callee, one outgoing
    /// unresolved `type/param → Array`, one incoming by-name caller.
    fn neighbors_payload() -> Value {
        let me = "n:me";
        json!({
            "node_id": me,
            "direction": "Both",
            "resolved_neighbors": [{
                "edge": sym_edge(me, "calls", "render"),
                "node": node("n:render", "render", "ui.rs", 9),
            }],
            "unresolved_neighbors": [
                [sym_edge(me, "type/param", "Array"), stub("Array")],
                [sym_edge("n:caller", "calls", "me"), node("n:caller", "caller", "app.rs", 3)],
            ],
            "unresolved_count": 2,
        })
    }

    /// **The fabricated-citation defect.** The daemon must put *something* in the
    /// wire's `Node` slot for an edge it could not bind, so it sends a stub whose
    /// id is a synthetic `unresolved-Array`. Rendering that through the same
    /// `[src= loc= id=]` handle real nodes use handed the model an `id=` it could
    /// not tell from a real one — and a model taught "cite ids" cited
    /// `unresolved-Array`, `unresolved-Self`, `unresolved-into_dyn`, which the
    /// grounding gate correctly called fabrications. A row with no node must
    /// carry no address.
    #[test]
    fn an_unresolved_callee_row_exposes_no_citable_id() {
        let text = format_neighbors_response(&neighbors_payload(), "n:me", true, &both_semantic())
            .unwrap();

        let callee = text
            .lines()
            .find(|l| l.contains("Array"))
            .unwrap_or_else(|| panic!("declined callee must be listed: {text}"));
        assert!(
            callee.contains("[UNRESOLVED]") && callee.contains("type/param"),
            "still marked + relation-tagged: {callee}"
        );
        // None of the three addressing fields — not just `id=`. `src=`/`loc=`
        // are equally citable in the ADR-0030 evidence grammar (`name [src=…]`).
        for field in ["id=", "src=", "loc="] {
            assert!(
                !callee.contains(field),
                "a bare unresolved callee is not addressable, so it must not \
                 advertise `{field}`: {callee}"
            );
        }
        assert!(
            !text.contains("unresolved-Array"),
            "the synthetic wire id must never reach the model: {text}"
        );
    }

    /// The other half of the same rule: what *is* addressable keeps its handle.
    /// An incoming by-name caller is a real node (the daemon sends it verbatim),
    /// and so is every resolved neighbor — stripping those would trade one
    /// honesty bug for a navigability one.
    #[test]
    fn resolved_and_by_name_caller_rows_still_carry_their_id() {
        let text = format_neighbors_response(&neighbors_payload(), "n:me", true, &both_semantic())
            .unwrap();

        let resolved = text
            .lines()
            .find(|l| l.contains("render"))
            .unwrap_or_else(|| panic!("resolved neighbor listed: {text}"));
        assert!(
            resolved.contains("id=n:render") && resolved.contains("src=ui.rs"),
            "resolved rows stay citable: {resolved}"
        );

        let caller = text
            .lines()
            .find(|l| l.contains("<--"))
            .unwrap_or_else(|| panic!("by-name caller listed: {text}"));
        assert!(
            caller.contains("id=n:caller") && caller.contains("UNRESOLVED, by-name"),
            "an unresolved *caller* is a real node — addressable, and marked \
             by-name so the model knows it may over-match a homonym: {caller}"
        );
    }

    /// ADR-0029's load-bearing default is untouched by the rendering fix: with
    /// `include_unresolved:false` the edges are still **counted**, so an empty
    /// resolved list never reads as "nothing here".
    #[test]
    fn counts_by_default_survives_the_rendering_fix() {
        let text = format_neighbors_response(&neighbors_payload(), "n:me", false, &both_semantic())
            .unwrap();
        assert!(
            text.contains("unresolved: 2") && text.contains("include_unresolved"),
            "counts by default, and points at the opt-in: {text}"
        );
        assert!(!text.contains("Array"), "the count is not the list: {text}");
    }

    // ---- an empty answer is never silent -----------------------------------

    fn both_semantic() -> NeighborAsk {
        NeighborAsk {
            relations: vec![],
            direction: "both".to_string(),
        }
    }

    fn empty_payload() -> Value {
        json!({
            "node_id": "n:me",
            "direction": "In",
            "resolved_neighbors": [],
            "unresolved_neighbors": [],
            "unresolved_count": 0,
        })
    }

    /// **The regression that cost a whole eval run.** 33 of 33
    /// `relations:["semantic"]` calls came back as the bare header line, and the
    /// model concluded *"the graph does not contain any call edges"* — a false
    /// negative the response did nothing to prevent. A zero-row answer must name
    /// the filter and the direction that produced it.
    #[test]
    fn an_empty_answer_names_the_filter_that_produced_it() {
        let ask = NeighborAsk {
            relations: vec!["type/field".to_string()],
            direction: "in".to_string(),
        };
        let text = format_neighbors_response(&empty_payload(), "n:me", false, &ask).unwrap();
        assert!(
            text.contains("no edges matched"),
            "an empty result must say so in words: {text}"
        );
        assert!(
            text.contains("direction=in") && text.contains("type/field"),
            "and must name what it applied, so the caller can widen it: {text}"
        );
        assert!(
            text.contains("any"),
            "and point at the escape hatch: {text}"
        );
    }

    /// An omitted `relations` is `semantic` (ADR-0044) — the note has to say the
    /// *effective* filter, not "you passed nothing", or it explains nothing.
    #[test]
    fn an_empty_answer_names_the_default_filter_by_name() {
        let text =
            format_neighbors_response(&empty_payload(), "n:me", false, &both_semantic()).unwrap();
        assert!(
            text.contains("semantic (the default)"),
            "the default is a name, not an absence: {text}"
        );
    }

    /// The note is for the empty case only: a populated answer must not grow a
    /// line that reads as a caveat on real rows.
    #[test]
    fn a_populated_answer_carries_no_note() {
        let text = format_neighbors_response(&neighbors_payload(), "n:me", false, &both_semantic())
            .unwrap();
        assert!(!text.contains("no edges matched"), "{text}");
    }

    // ---- ADR-0044 named questions ------------------------------------------

    /// The whole point: the caller names the question, and the *direction comes
    /// with it* — no second key to add.
    #[test]
    fn a_named_question_carries_its_own_direction() {
        let (relations, direction) = resolve_questions(vec!["callers".to_string()], "both");
        assert_eq!(relations, vec!["calls".to_string()]);
        assert_eq!(direction, "in");

        let (relations, direction) = resolve_questions(vec!["callees".to_string()], "both");
        assert_eq!(relations, vec!["calls".to_string()]);
        assert_eq!(direction, "out");

        let (relations, direction) = resolve_questions(vec!["produces".to_string()], "both");
        assert_eq!(relations, vec!["type/return".to_string()]);
        assert_eq!(direction, "in");
    }

    /// The question wins over an explicit `direction`. A caller who wrote
    /// `callers` asked for incoming edges; honouring a contradicting
    /// `direction:"out"` beside it would answer a question nobody asked.
    #[test]
    fn the_question_outranks_an_explicit_direction() {
        let (_, direction) = resolve_questions(vec!["callers".to_string()], "out");
        assert_eq!(direction, "in");
    }

    /// Questions that disagree denote a union, and `both` is that union. Picking
    /// either one would silently drop half the answer.
    #[test]
    fn disagreeing_questions_widen_rather_than_drop() {
        let (relations, direction) =
            resolve_questions(vec!["callers".to_string(), "callees".to_string()], "in");
        assert_eq!(relations, vec!["calls".to_string()], "de-duplicated");
        assert_eq!(direction, "both");
    }

    /// Additive, not a replacement: every argument shape that worked before this
    /// change still means exactly what it meant.
    #[test]
    fn raw_relations_are_untouched() {
        let (relations, direction) =
            resolve_questions(vec!["type/param".to_string(), "semantic".to_string()], "in");
        assert_eq!(
            relations,
            vec!["type/param".to_string(), "semantic".to_string()]
        );
        assert_eq!(direction, "in", "no question named, so the caller's wins");

        let (relations, direction) = resolve_questions(vec![], "both");
        assert!(relations.is_empty());
        assert_eq!(direction, "both");
    }

    /// A question name that is not in the enum is not selectable by a
    /// constrained decoder — pinned so a rename is a test failure rather than a
    /// value the model can never emit.
    #[test]
    fn every_named_question_is_in_the_advertised_enum() {
        let vocab = neighbor_relation_vocabulary();
        for (name, _, _) in filigrio_client_mcp::NAMED_QUESTIONS {
            assert!(
                vocab.contains(name),
                "`{name}` resolves but is not advertised — the model cannot select it"
            );
        }
        assert_eq!(
            vocab.iter().position(|v| *v == "callers"),
            Some(0),
            "the named questions lead the enum: they are the intended spellings"
        );
    }

    /// `query_graph` is a traversal: it has no direction to fold in, so the
    /// question names must NOT appear on its enum. Advertising a value that
    /// cannot mean anything is the defect this whole vocabulary exists to avoid.
    #[test]
    fn query_graph_does_not_advertise_the_named_questions() {
        let tools = bridge().handle_tools_list().unwrap();
        let query = tools["tools"]
            .as_array()
            .and_then(|ts| ts.iter().find(|t| t["name"] == "query_graph"))
            .expect("query_graph is listed");
        let values = query["inputSchema"]["properties"]["relations"]["items"]["enum"]
            .as_array()
            .expect("relations is an enum");
        assert!(
            !values.iter().any(|v| v == "callers"),
            "a traversal cannot answer a directional question: {values:?}"
        );
        assert!(
            values.iter().any(|v| v == "semantic"),
            "but it keeps the relation vocabulary: {values:?}"
        );
    }
}
