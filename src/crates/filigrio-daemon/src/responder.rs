//! The responder — the engine-service layer (ADR-0032f §3).
//!
//! The responder handles **read-only §3 contract queries** over an abstract
//! state source, making the exact same query logic usable by both lifecycles:
//! - The resident daemon (warm cache via `RegistryWarmStateSource`)
//! - The one-shot responder (cold store via `ColdStoreStateSource`)
//!
//! It is query-only *in its type surface*: there is no command entry point,
//! and since the ADR-0032f §2 type-level plane split it takes the data plane's
//! own [`DataQuery`] — the control plane (commands, plus the `Health`/`Progress`
//! daemon meta reads) cannot even be expressed here. Commands (writes) are
//! routed before the responder is ever reached — the resident daemon queues
//! them for its worker pool, the one-shot binary runs them synchronously via
//! `run_command_cold` — and the meta reads are answered by the daemon itself
//! (they read daemon-lifecycle state the responder deliberately doesn't hold).

use crate::{Response, Result as DaemonResult};
use filigrio_core::attrs;
use filigrio_core::relation::{filter, relation_matches_filter};
use filigrio_core::{Direction, Edge, EdgeTarget, GraphQuery, GraphState, Node, QueryOpts};
use filigrio_protocol::{DataQuery, NodeAddress, QueryParams};
use filigrio_query::GraphView;
use std::sync::Arc;

/// Wrap an already-built JSON payload as a `QueryResult`.
fn ok(data: serde_json::Value) -> Response {
    Response::QueryResult { data }
}

/// Serialize a value into a `QueryResult`, mapping a serialization failure to a
/// structured error naming `what` failed.
fn json_result<T: serde::Serialize>(value: &T, what: &str) -> Response {
    match serde_json::to_value(value) {
        Ok(data) => Response::QueryResult { data },
        Err(e) => Response::Error {
            message: format!("failed to serialize {what}: {e}"),
        },
    }
}

/// Abstract state source for the responder.
///
/// This trait abstracts over the two state sources:
/// - **Warm cache** (resident daemon): `Arc<Mutex<ProjectStateCache>>`
/// - **Cold store** (one-shot responder): `FsStore` directly
///
/// The responder uses this abstraction to be lifecycle-agnostic — it only cares about
/// getting state, not where it comes from.
pub trait StateSource: Send + Sync {
    /// Get project state for the given project ID.
    ///
    /// Returns `Arc<GraphState>` so hot hits are shared, not cloned.
    fn get_state(&self, project_id: &str) -> DaemonResult<Arc<GraphState>>;

    /// Check if a project exists in this state source.
    fn has_project(&self, project_id: &str) -> bool;

    /// The project ids this source can actually serve — used by the
    /// "not registered" error to name what **is** valid, not only what was
    /// wrong (the agent-eval trap: a caller that guesses a project name has no
    /// way to learn the accepted vocabulary and burns turns re-guessing).
    ///
    /// Defaults to "cannot enumerate": a source without a registry (the cold
    /// store, which resolves a project by looking for a directory) genuinely
    /// does not know the set, and an empty list there means "no hint
    /// available", not "no projects exist".
    fn known_projects(&self) -> Vec<String> {
        Vec::new()
    }

    /// The queryable index over a project's state — the thing every handler
    /// below actually needs (audit §L1).
    ///
    /// The default builds one per call, which is correct for any source and is
    /// exactly what all eight handlers used to do inline. A source that *owns*
    /// resident state overrides it: building a view is O(nodes+edges) —
    /// 24 ms at 20k nodes, ~100 ms at next.js scale — so rebuilding it per
    /// request made every wire read pay for an index before answering anything.
    /// The override is a cache; this default is the honest fallback for a source
    /// (like the one-shot cold store) that has nowhere to keep one.
    fn get_view(&self, project_id: &str) -> DaemonResult<Arc<GraphView>> {
        Ok(Arc::new(GraphView::new(self.get_state(project_id)?)))
    }
}

/// The responder — the engine-service layer (ADR-0032f §3).
///
/// The responder is the **single place** where requests are answered. It:
/// - Takes a `StateSource` for project state access
/// - Handles all queries synchronously (reads from state)
/// - Routes commands to the async queue (writes via priority)
///
/// Both the resident daemon and one-shot responder wrap this same responder instance
/// with their respective state sources, ensuring **identical handling** across lifecycles.
pub struct Responder<S> {
    /// Abstract state source (warm cache or cold store)
    state_source: S,
}

impl<S: StateSource> Responder<S> {
    /// Create a new responder with the given state source.
    pub fn new(state_source: S) -> Self {
        Self { state_source }
    }

    /// Handle a data-plane query by serving it synchronously from state.
    ///
    /// All queries are **read-only** and served immediately from the state source:
    /// - Project queries use `state_of` to get warm/cold state
    /// - Response is serialized JSON result or error
    ///
    /// (Daemon meta reads — `Health`/`Progress` — are `MetaQuery` control-plane
    /// ops now: they cannot reach the responder by construction, so the old
    /// runtime rejection arm for `Health` is gone rather than guarded.)
    pub fn handle_query(&self, query: DataQuery) -> Response {
        match query {
            DataQuery::Status {
                project: Some(project),
            } => self.handle_project_status(&project),
            DataQuery::Status { project: None } => Response::Error {
                message: "all-projects Status is not yet implemented".to_string(),
            },
            // Enhanced data plane queries (ADR-0032f Step 3)
            DataQuery::GetNode {
                project,
                node_address,
            } => self.handle_get_node(&project, &node_address),
            DataQuery::Neighbors {
                project,
                node,
                direction,
                relations,
                include_unresolved,
            } => {
                self.handle_get_neighbors(&project, node, direction, relations, include_unresolved)
            }
            DataQuery::Query { project, params } => self.handle_query_graph(&project, &params),
            DataQuery::GodNodes { project, limit } => self.handle_god_nodes(&project, limit),
            DataQuery::Path {
                project,
                from,
                to,
                max_hops,
            } => self.handle_path(&project, &from, &to, max_hops),
            DataQuery::GraphStats { project } => self.handle_graph_stats(&project),
            DataQuery::ProjectGraph { project } => self.handle_project_graph(&project),
            DataQuery::Community {
                project,
                community_id,
            } => self.handle_community(&project, community_id),
            DataQuery::ListCommunities { project, limit } => {
                self.handle_list_communities(&project, limit)
            }
            DataQuery::GraphReport { project, top } => self.handle_graph_report(&project, top),
        }
    }

    /// Handle project status query.
    fn handle_project_status(&self, project: &str) -> Response {
        if !self.state_source.has_project(project) {
            return self.unregistered_project_error(project);
        }

        match self.state_source.get_state(project) {
            Ok(state) => ok(serde_json::json!({
                "project": project,
                "file_count": state.manifest.entries.len(),
                "node_count": state.graph.nodes.len(),
                "edge_count": state.graph.edges.len(),
                "last_revision": state.manifest.latest_revision().map(|rev| rev.0),
            })),
            Err(e) => Response::Error {
                message: format!("failed to load state: {e}"),
            },
        }
    }

    /// Resolve an ADR-0027 `{id,label,src}` address to a concrete `Node`.
    ///
    /// This is the **one place** address resolution happens — `handle_get_node`,
    /// `handle_get_neighbors`, and `handle_path` all route through it, so a
    /// caller can name any of those queries' endpoints by label (+ optional
    /// `src` to disambiguate a homonym) instead of pre-resolving to an id
    /// client-side first. `src` genuinely narrows the candidate set here
    /// (it used to be accepted by the wire format and silently ignored).
    fn resolve_node_address(
        view: &GraphView,
        node_address: &NodeAddress,
    ) -> Result<Node, Response> {
        if let Some(id) = &node_address.id {
            // Exact ID lookup (preferred per ADR-0027)
            return match view.node_by_id(id) {
                Ok(Some(node)) => Ok(node),
                Ok(None) => Err(Response::Error {
                    message: format!("node not found by id: {id}"),
                }),
                Err(e) => Err(Response::Error {
                    message: format!("node lookup failed: {e}"),
                }),
            };
        }

        let Some(label) = &node_address.label else {
            return Err(Response::Error {
                message: "node address must specify either id or label".to_string(),
            });
        };

        // Label-based lookup with proper ADR-0027 disambiguation.
        // Never guess a homonym - return candidate list for resolution.
        let candidates = match view.nodes_by_label(label) {
            Ok(nodes) => nodes,
            Err(e) => {
                return Err(Response::Error {
                    message: format!("label lookup failed: {e}"),
                })
            }
        };

        // `src` narrows the candidate set — a homonym in a different file is
        // not a match, so it never appears in the disambiguation list either.
        let mut candidates: Vec<Node> = match &node_address.src {
            Some(src) => candidates
                .into_iter()
                .filter(|n| n.source_file.as_deref() == Some(src.as_str()))
                .collect(),
            None => candidates,
        };

        match candidates.len() {
            0 => Err(Response::Error {
                message: match &node_address.src {
                    Some(src) => format!("no node named '{label}' found in {src}"),
                    None => format!("no nodes found with label: {label}"),
                },
            }),
            1 => Ok(candidates.remove(0)),
            _ => Err(Response::Error {
                message: Self::format_ambiguous_label_response(label, &candidates),
            }),
        }
    }

    /// Handle GetNode query with ADR-0027 addressing (id > label+src).
    fn handle_get_node(&self, project: &str, node_address: &NodeAddress) -> Response {
        // Through the shared helper like every other data query: this one used
        // to call the source directly, so a wrong `project` reached the agent as
        // a bare storage error with none of the guidance the other eight give.
        let view = match self.project_view(project) {
            Ok(view) => view,
            Err(response) => return response,
        };

        let result = match Self::resolve_node_address(&view, node_address) {
            Ok(node) => node,
            Err(response) => return response,
        };

        json_result(&result, "node")
    }

    /// Handle Neighbors query with ADR-0027 addressing and direction.
    fn handle_get_neighbors(
        &self,
        project: &str,
        node: NodeAddress,
        direction: Direction,
        relations: Vec<String>,
        include_unresolved: bool,
    ) -> Response {
        let view = match self.project_view(project) {
            Ok(view) => view,
            Err(response) => return response,
        };

        // Resolve the ADR-0027 address to a concrete node in-process, so a
        // caller never needs a `GetNode` round trip before `Neighbors`.
        let query_node = match Self::resolve_node_address(&view, &node) {
            Ok(node) => node,
            Err(response) => return response,
        };
        let node_id = query_node.id.0;

        // `relations` is a filter **set**, OR'd, each entry matched hierarchically
        // by the port (ADR-0036 R2.1) — `["type"]` is the whole family,
        // `["type/param","type/return"]` is two members. This used to reject a
        // second entry outright while `query_graph`'s `context_filter` had always
        // OR'd them; the wire type was `Vec<String>` on both paths and only one
        // honored it.
        //
        // ADR-0044: an omitted/empty set is the **named** value `semantic` —
        // normalized here, in the shared helper `query_graph` also calls, rather
        // than post-filtered locally. It used to be post-filtered locally, and
        // `query_graph` locally decided the opposite (`context_filter: None`,
        // i.e. no filter at all), so the same empty array meant two things on
        // the two tools an agent uses most. `["any"]` now spells the behaviour
        // the empty array used to have here on `query_graph`.
        let normalized = filter::normalize(&relations);
        let relation_filter = normalized.as_ref();

        let result = match view.neighbors_by_id(&node_id, relation_filter, direction) {
            Ok(neighbors) => neighbors,
            Err(e) => {
                return Response::Error {
                    message: format!("neighbors lookup failed: {e}"),
                }
            }
        };

        // Label for unresolved-incoming lookups — already have it from resolution above.
        let query_node_label = Some(query_node.label.clone());

        // Determine which unresolved edges to compute based on direction (matching classic behavior)
        let want_out = matches!(direction, Direction::Out | Direction::Both);
        let want_in = matches!(direction, Direction::In | Direction::Both);

        // Collect unresolved edges (ADR-0029: **counts by default**, never a silent
        // "nothing here" when unresolved-only edges exist — `include_unresolved` only
        // gates whether the full list rides in the response, not whether we count).
        let mut unresolved_edges: Vec<(filigrio_core::Edge, filigrio_core::Node)> = Vec::new();

        // The unresolved half filters exactly as the resolved half does — the
        // *same normalized set* through the same hierarchical matcher, so the
        // empty case cannot be re-decided here either. Written once so the two
        // halves cannot answer the same filter differently.
        let keeps = |relation: &str| {
            relation_filter
                .iter()
                .any(|entry| relation_matches_filter(entry, relation))
        };

        // Get unresolved outgoing edges by ID (only if direction asks for it)
        if want_out {
            if let Ok(unresolved_out) = view.unresolved_out_by_id(&node_id) {
                for edge in unresolved_out {
                    if keeps(&edge.relation) {
                        // Create a stub node for unresolved edges
                        let stub_node = Self::create_stub_node_for_unresolved(&edge);
                        unresolved_edges.push((edge, stub_node));
                    }
                }
            }
        }

        // Get unresolved incoming edges by label when available (only if direction asks for it)
        if want_in {
            if let Some(ref label) = query_node_label {
                if let Ok(unresolved_in) = view.unresolved_in_by_label(label) {
                    for (edge, caller) in unresolved_in {
                        if keeps(&edge.relation) {
                            unresolved_edges.push((edge, caller));
                        }
                    }
                }
            }
        }

        ok(serde_json::json!({
            "node_id": node_id,
            "direction": format!("{:?}", direction),
            "resolved_neighbors": result.iter().map(|(edge, node)| {
                serde_json::json!({
                    "edge": edge,
                    "node": node
                })
            }).collect::<Vec<_>>(),
            "unresolved_neighbors": if include_unresolved {
                serde_json::to_value(&unresolved_edges).unwrap_or(serde_json::json!([]))
            } else {
                serde_json::json!([])
            },
            "unresolved_count": unresolved_edges.len()
        }))
    }

    /// Handle enhanced Query with ADR-0025/0029 parameters.
    fn handle_query_graph(&self, project: &str, params: &QueryParams) -> Response {
        let view = match self.project_view(project) {
            Ok(view) => view,
            Err(response) => return response,
        };

        let opts = QueryOpts {
            mode: params.mode,
            depth: params.depth as usize,
            budget: params.budget,
            // ADR-0044: the same normalization `get_neighbors` applies, from the
            // same helper. This path used to read an empty `relations` as
            // `None` — *no filter at all* — while `get_neighbors` read it as
            // "drop the structural scaffolding". Identical input, opposite
            // meanings. Reconciled onto `semantic`, which is what the tool
            // descriptions already promise; `["any"]` restores the old
            // no-filter traversal as a value the caller can name.
            context_filter: Some(filter::normalize(&params.relations).into_owned()),
        };

        match view.query(&params.query, opts) {
            Ok(subgraph) => ok(serde_json::json!({
                "query": params.query,
                "nodes": subgraph.nodes,
                "edges": subgraph.edges,
                "communities": subgraph.communities
            })),
            Err(e) => Response::Error {
                message: format!("graph query failed: {e}"),
            },
        }
    }

    /// Handle GodNodes query.
    fn handle_god_nodes(&self, project: &str, limit: usize) -> Response {
        let view = match self.project_view(project) {
            Ok(view) => view,
            Err(response) => return response,
        };

        match view.god_nodes(limit) {
            Ok(god_nodes) => ok(serde_json::json!({
                "project": project,
                "limit": limit,
                "god_nodes": god_nodes.iter().map(|(node, degree)| {
                    serde_json::json!({
                        "node": node,
                        "degree": degree
                    })
                }).collect::<Vec<_>>()
            })),
            Err(e) => Response::Error {
                message: format!("god nodes calculation failed: {e}"),
            },
        }
    }

    /// Handle Path query.
    fn handle_path(
        &self,
        project: &str,
        from: &NodeAddress,
        to: &NodeAddress,
        max_hops: u8,
    ) -> Response {
        let view = match self.project_view(project) {
            Ok(view) => view,
            Err(response) => return response,
        };

        // Resolve both endpoints the same way `get_node` does — `shortest_path`
        // itself is id-only (no label lookup), so an unresolved label used to
        // silently read as "no path found" instead of "no such node".
        let from_node = match Self::resolve_node_address(&view, from) {
            Ok(node) => node,
            Err(response) => return response,
        };
        let to_node = match Self::resolve_node_address(&view, to) {
            Ok(node) => node,
            Err(response) => return response,
        };
        let (from_id, to_id) = (from_node.id.0, to_node.id.0);

        match view.shortest_path(&from_id, &to_id, max_hops as usize) {
            Ok(Some(path)) => ok(serde_json::json!({
                "from": from_id,
                "to": to_id,
                "max_hops": max_hops,
                "path": path,
                "hop_count": path.len().saturating_sub(1)
            })),
            Ok(None) => ok(serde_json::json!({
                "from": from_id,
                "to": to_id,
                "max_hops": max_hops,
                "path": null,
                "hop_count": 0,
                "message": "No path found within max_hops"
            })),
            Err(e) => Response::Error {
                message: format!("path finding failed: {e}"),
            },
        }
    }

    /// Handle Community query — members of a community by id (ADR-0024).
    fn handle_community(&self, project: &str, community_id: u64) -> Response {
        let view = match self.project_view(project) {
            Ok(view) => view,
            Err(response) => return response,
        };
        let cid = filigrio_core::CommunityId(community_id);

        let members = match view.community(cid) {
            Ok(members) => members,
            Err(e) => {
                return Response::Error {
                    message: format!("community lookup failed: {e}"),
                }
            }
        };

        // Derived label lives on each member's `community` attr (stamped by
        // the view), same convention the legacy MCP `get_community` tool used.
        let name = members
            .first()
            .and_then(|n| n.attrs.get(attrs::COMMUNITY).cloned());
        let cohesion = view
            .community_meta(cid)
            .ok()
            .flatten()
            .map(|m| m.cohesion());

        ok(serde_json::json!({
            "community_id": community_id,
            "name": name,
            "cohesion": cohesion,
            "members": members,
        }))
    }

    /// Handle ListCommunities — the enumeration that makes a community id
    /// addressable at all (ADR-0024 surface).
    ///
    /// `community_summaries()` already computes exactly `(id, label, size,
    /// cohesion)`; its `members` roster is deliberately **not** shipped — that
    /// is `Community`'s job, one id at a time, and at 321 communities the
    /// rosters are the whole graph. Sorted by size descending (id ascending on
    /// a tie, so the order is total), capped at `limit`, and the untruncated
    /// `total` rides along so a capped list never reads as the whole set.
    fn handle_list_communities(&self, project: &str, limit: usize) -> Response {
        let view = match self.project_view(project) {
            Ok(view) => view,
            Err(response) => return response,
        };

        let mut summaries = view.community_summaries();
        summaries.sort_by(|a, b| b.size.cmp(&a.size).then(a.id.0.cmp(&b.id.0)));
        let total = summaries.len();

        ok(serde_json::json!({
            "project": project,
            "total": total,
            "limit": limit,
            "communities": summaries.iter().take(limit).map(|c| serde_json::json!({
                "id": c.id.0,
                "label": c.label,
                "size": c.size,
                "cohesion": c.cohesion(),
            })).collect::<Vec<_>>(),
        }))
    }

    /// Handle GraphReport — render `GRAPH_REPORT.md` daemon-side.
    ///
    /// The rendering lives here rather than in the CLI because the report needs
    /// community enumeration *and* every bridge; shipping those would put
    /// `CommunitySummary`/`Bridge` on the wire for a payload the only caller
    /// writes straight to a file. The daemon already links `filigrio-query`, so
    /// `render_markdown` runs beside the graph and the response is one string.
    fn handle_graph_report(&self, project: &str, top: usize) -> Response {
        let view = match self.project_view(project) {
            Ok(view) => view,
            Err(response) => return response,
        };

        match view.report(top) {
            Ok(report) => ok(serde_json::json!({
                "markdown": filigrio_query::render_markdown(&report),
            })),
            Err(e) => Response::Error {
                message: format!("graph report failed: {e}"),
            },
        }
    }

    /// Handle GraphStats query.
    fn handle_graph_stats(&self, project: &str) -> Response {
        let view = match self.project_view(project) {
            Ok(view) => view,
            Err(response) => return response,
        };

        match view.stats() {
            Ok(stats) => ok(serde_json::json!({
                "project": project,
                "nodes": stats.nodes,
                "edges": stats.edges,
                "communities": stats.communities,
                "by_confidence": stats.by_confidence,
                // The same snapshot the stats came from — a view is a pure
                // function of one state, so reading the manifest off it cannot
                // mix two revisions the way a second cache lookup could.
                "file_count": view.state().manifest.entries.len(),
                "last_revision": view.state().manifest.latest_revision().map(|rev| rev.0),
            })),
            Err(e) => Response::Error {
                message: format!("graph stats calculation failed: {e}"),
            },
        }
    }

    /// Handle ProjectGraph export query.
    fn handle_project_graph(&self, project: &str) -> Response {
        let view = match self.project_view(project) {
            Ok(view) => view,
            Err(response) => return response,
        };

        match view.project_graph() {
            Ok(project_graph) => json_result(&project_graph, "project graph"),
            Err(e) => Response::Error {
                message: format!("project graph export failed: {e}"),
            },
        }
    }

    /// Helper: get the project's query view, or the already-formatted error
    /// response — the not-registered / failed-to-load distinction the handlers'
    /// error text has always made, in **one** place instead of nine copies.
    ///
    /// Returning the `Response` (not a `DaemonError` each caller re-wraps) is
    /// what lets the not-registered case carry its own guidance without every
    /// call site prefixing it with a misleading "failed to load state:".
    ///
    /// (This replaced a `get_project_state` twin: every handler but
    /// `handle_project_status` needs a *view*, and that one reads counts
    /// straight off the state, so there is one helper here, not two.)
    fn project_view(&self, project: &str) -> Result<Arc<GraphView>, Response> {
        if !self.state_source.has_project(project) {
            return Err(self.unregistered_project_error(project));
        }

        self.state_source.get_view(project).map_err(|e| match e {
            // Registered but never indexed is the source's own sentence, and it
            // already names the fix — prefixing it with "failed to load state"
            // would file an instruction under a fault.
            crate::Error::ProjectNotIndexed(_) => Response::Error {
                message: e.to_string(),
            },
            e => Response::Error {
                message: format!("failed to load state for {}: {e}", project),
            },
        })
    }

    /// The "you named a project I don't have" response, for every data query.
    /// One message, formatted in one place
    /// ([`unregistered_project_message`](crate::project::unregistered_project_message))
    /// so the data plane, `Status` and the control-plane verbs cannot spell one
    /// condition three ways (ADR-0032b OQ4).
    fn unregistered_project_error(&self, project: &str) -> Response {
        Response::Error {
            message: crate::project::unregistered_project_message(
                project,
                &self.state_source.known_projects(),
            ),
        }
    }

    /// Format an ambiguous label response per ADR-0027: return candidate list
    /// instead of silently guessing. Enables caller to re-address by id/src.
    fn format_ambiguous_label_response(label: &str, candidates: &[Node]) -> String {
        let mut response = format!(
            "{} nodes named '{}' — use id or src to disambiguate:",
            candidates.len(),
            label
        );
        for node in candidates {
            let src = node.source_file.as_deref().unwrap_or("unknown");
            let node_type = node.kind.clone();
            response.push_str(&format!(
                "\n  - {} [type={}, src={}, id={}]",
                node.label, node_type, src, node.id.0
            ));
        }
        response
    }

    /// Create a stub node for an unresolved *outgoing* edge, so the wire's
    /// `(edge, node)` pair has a node slot to fill.
    ///
    /// **The `id` here is a placeholder, not an address.** There is no node —
    /// that is what "unresolved" means — so `unresolved-<name>` addresses
    /// nothing, and no renderer may present it in the `id=` slot real nodes use:
    /// a model taught to cite ids will cite it, and the citation is a
    /// fabrication (it happened — `unresolved-Array`, `unresolved-Self`,
    /// `unresolved-into_dyn` failed an agent-eval grounding check). `kind:
    /// "unresolved"` is the marker a consumer should key on; the bridge renders
    /// these rows with no `id=`/`src=`/`loc=` triple at all.
    ///
    /// (Incoming unresolved edges do NOT come through here — their "stub" is the
    /// real caller node, which *is* addressable and keeps its handle.)
    fn create_stub_node_for_unresolved(edge: &Edge) -> Node {
        let (stub_id, stub_label) = match &edge.target {
            EdgeTarget::Symbol(target_ref) => (
                format!("unresolved-{}", target_ref.name),
                format!("unresolved: {}", target_ref.name),
            ),
            EdgeTarget::Node(node_id) => (
                format!("unresolved-{}", node_id.0),
                format!("unresolved: {}", node_id.0),
            ),
        };

        Node {
            id: filigrio_core::NodeId(stub_id),
            label: stub_label,
            kind: "unresolved".to_string(),
            source_span: None,
            source_file: None,
            attrs: std::collections::BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error as DaemonError;
    // The taxonomy the ADR-0044 tests assert against — deliberately the real
    // predicate, not a hand-listed set of relation names.
    use filigrio_core::relation::is_structural;
    use filigrio_core::{
        ExportIndex, Graph, Manifest, Partition, ReverseIndex, SymbolTable, Workspace,
    };
    use std::collections::HashMap;

    // Mock state source for testing
    struct MockStateSource {
        projects: HashMap<String, Arc<GraphState>>,
    }

    impl MockStateSource {
        fn new() -> Self {
            Self {
                projects: HashMap::new(),
            }
        }

        fn add_project(&mut self, id: String, state: GraphState) {
            self.projects.insert(id, Arc::new(state));
        }
    }

    impl StateSource for MockStateSource {
        fn get_state(&self, project_id: &str) -> DaemonResult<Arc<GraphState>> {
            self.projects
                .get(project_id)
                .cloned()
                .ok_or_else(|| DaemonError::ProjectNotFound(project_id.to_string()))
        }

        fn has_project(&self, project_id: &str) -> bool {
            self.projects.contains_key(project_id)
        }

        fn known_projects(&self) -> Vec<String> {
            self.projects.keys().cloned().collect()
        }
    }

    /// A state with two nodes sharing a label, in different files — the homonym
    /// shape ADR-0027 addressing exists for.
    fn state_with_homonyms() -> GraphState {
        let mut graph = Graph::default();
        for (id, src) in [
            ("fn:src/a.rs:render", "src/a.rs"),
            ("fn:src/b.rs:render", "src/b.rs"),
        ] {
            graph.nodes.push(Node {
                id: filigrio_core::NodeId(id.to_string()),
                label: "render".to_string(),
                kind: "function".to_string(),
                source_span: None,
                source_file: Some(src.to_string()),
                attrs: std::collections::BTreeMap::new(),
            });
        }
        GraphState {
            graph,
            ..Default::default()
        }
    }

    /// ADR-0027: an ambiguous label is **refused with its candidates**, never
    /// silently resolved to one of them — and `src` narrows the same address to
    /// exactly one node.
    ///
    /// This replaces `test_responder_creation`, which constructed a
    /// `MockStateSource`, asserted the **mock's** map was empty, and never
    /// touched `Responder` at all (audit §K2e). The empty-source path it
    /// nominally covered is already pinned by `test_nonexistent_project_error`
    /// below.
    #[test]
    fn get_node_by_ambiguous_label_refuses_and_lists_the_candidates() {
        let mut source = MockStateSource::new();
        source.add_project("test".to_string(), state_with_homonyms());
        let responder = Responder::new(source);

        let ambiguous = responder.handle_query(DataQuery::GetNode {
            project: "test".to_string(),
            node_address: NodeAddress::by_label("render"),
        });
        let Response::Error { message } = ambiguous else {
            panic!("an ambiguous label must not resolve to a node: {ambiguous:?}");
        };
        assert!(
            message.contains("2 nodes named 'render'"),
            "the refusal must say how many candidates there are: {message}"
        );
        for expected in [
            "src/a.rs",
            "src/b.rs",
            "fn:src/a.rs:render",
            "fn:src/b.rs:render",
        ] {
            assert!(
                message.contains(expected),
                "the caller can only re-address if the refusal carries {expected}: {message}"
            );
        }

        // The same label plus `src` is unambiguous — and the *other* homonym is
        // not merely deprioritised, it is out of the candidate set entirely.
        let narrowed = responder.handle_query(DataQuery::GetNode {
            project: "test".to_string(),
            node_address: NodeAddress::by_label_src("render", "src/b.rs"),
        });
        let Response::QueryResult { data } = narrowed else {
            panic!("label+src addresses exactly one node: {narrowed:?}");
        };
        assert_eq!(data["id"], "fn:src/b.rs:render");
    }

    /// A label that matches nothing is an error naming the label, not an empty
    /// success — the caller must be able to tell "no such node" from "a node
    /// with no neighbours".
    #[test]
    fn get_node_by_unknown_label_is_an_error_not_an_empty_result() {
        let mut source = MockStateSource::new();
        source.add_project("test".to_string(), state_with_homonyms());
        let responder = Responder::new(source);

        let response = responder.handle_query(DataQuery::GetNode {
            project: "test".to_string(),
            node_address: NodeAddress::by_label("nonexistent"),
        });
        let Response::Error { message } = response else {
            panic!("expected an Error, got {response:?}");
        };
        assert!(message.contains("nonexistent"), "got: {message}");

        // A real label in the wrong file is the same "not found", and the
        // message says *where* it was looked for.
        let response = responder.handle_query(DataQuery::GetNode {
            project: "test".to_string(),
            node_address: NodeAddress::by_label_src("render", "src/c.rs"),
        });
        let Response::Error { message } = response else {
            panic!("expected an Error, got {response:?}");
        };
        assert!(
            message.contains("render") && message.contains("src/c.rs"),
            "got: {message}"
        );
    }

    #[test]
    fn test_project_status_query() {
        let mut source = MockStateSource::new();
        source.add_project(
            "test".to_string(),
            GraphState {
                graph: Graph::default(),
                partition: Partition::default(),
                symbols: SymbolTable::default(),
                reverse: ReverseIndex::default(),
                manifest: Manifest::default(),
                workspace: Workspace::default(),
                exports: ExportIndex::default(),
                ..Default::default()
            },
        );

        let responder = Responder::new(source);
        let query = DataQuery::Status {
            project: Some("test".to_string()),
        };

        let response = responder.handle_query(query);
        match response {
            Response::QueryResult { data } => {
                assert!(data["project"] == "test");
                assert!(data["node_count"].is_number());
            }
            _ => panic!("Expected QueryResult"),
        }
    }

    /// A partition of three communities of sizes 3/1/2 with distinct cohesion —
    /// enough to tell "sorted by size" from "sorted by id", which an equal-sized
    /// fixture cannot.
    fn state_with_communities() -> GraphState {
        use filigrio_core::{CommunityId, CommunityMeta, NodeId};

        let mut graph = Graph::default();
        let mut partition = Partition::default();
        // (community id, label, cohesion permille, member count)
        let plan = [
            (7u64, "small", 900u16, 1usize),
            (2, "big", 500, 3),
            (5, "mid", 750, 2),
        ];
        for (cid, label, cohesion_permille, count) in plan {
            let id = CommunityId(cid);
            for i in 0..count {
                let node_id = NodeId(format!("fn:c{cid}:n{i}"));
                graph.nodes.push(Node {
                    id: node_id.clone(),
                    label: format!("{label}_{i}"),
                    kind: "function".to_string(),
                    source_span: None,
                    source_file: Some(format!("src/c{cid}.rs")),
                    attrs: std::collections::BTreeMap::new(),
                });
                partition.node_community.insert(node_id, id);
            }
            partition.communities.insert(
                id,
                CommunityMeta {
                    id,
                    label: label.to_string(),
                    size: count,
                    cohesion_permille,
                },
            );
        }
        GraphState {
            graph,
            partition,
            ..Default::default()
        }
    }

    /// `list_communities` is what makes a community **id** obtainable at all:
    /// the `community=` attr on a node carries the derived *label*, so before
    /// this query `Community { community_id }` was reachable only by guessing
    /// integers. It must therefore carry the id per row — and be ordered by
    /// size, largest first, so a `limit` keeps the communities that matter.
    #[test]
    fn list_communities_ranks_by_size_and_carries_the_id() {
        let mut source = MockStateSource::new();
        source.add_project("test".to_string(), state_with_communities());
        let responder = Responder::new(source);

        let response = responder.handle_query(DataQuery::ListCommunities {
            project: "test".to_string(),
            limit: 10,
        });
        let Response::QueryResult { data } = response else {
            panic!("expected a QueryResult, got {response:?}");
        };

        let rows = data["communities"].as_array().expect("communities array");
        assert_eq!(
            data["total"], 3,
            "the untruncated count rides along: {data}"
        );
        let by_size: Vec<u64> = rows.iter().map(|r| r["id"].as_u64().unwrap()).collect();
        assert_eq!(
            by_size,
            vec![2, 5, 7],
            "largest first (3/2/1 members), not id order: {data}"
        );
        // The label is the view's **derived** one (highest-degree member,
        // tie-broken by label ascending) — the same string the `community=`
        // node attr carries, not `CommunityMeta.label`. That identity is the
        // reason a model can recognise a listed community from a node line, and
        // the reason the tag alone cannot be passed to `get_community`.
        assert_eq!(rows[0]["label"], "big_0");
        assert_eq!(rows[0]["size"], 3);
        assert_eq!(
            rows[0]["cohesion"].as_f64().unwrap(),
            0.5,
            "cohesion is the ADR-0024 permille read back as a fraction: {data}"
        );
        assert!(
            rows[0].get("members").is_none(),
            "rosters are `get_community`'s job — at 321 communities they are the \
             whole graph: {data}"
        );

        // A capped list must still report the real total, or a truncated answer
        // reads as the whole set (ADR-0029 honesty).
        let response = responder.handle_query(DataQuery::ListCommunities {
            project: "test".to_string(),
            limit: 1,
        });
        let Response::QueryResult { data } = response else {
            panic!("expected a QueryResult, got {response:?}");
        };
        assert_eq!(data["communities"].as_array().unwrap().len(), 1);
        assert_eq!(data["total"], 3);
    }

    /// The id `list_communities` hands out is the id `get_community` takes —
    /// the round trip that was impossible before, and the whole point of the
    /// pair.
    #[test]
    fn a_listed_community_id_is_addressable_by_get_community() {
        let mut source = MockStateSource::new();
        source.add_project("test".to_string(), state_with_communities());
        let responder = Responder::new(source);

        let Response::QueryResult { data } = responder.handle_query(DataQuery::ListCommunities {
            project: "test".to_string(),
            limit: 1,
        }) else {
            panic!("list must succeed");
        };
        let id = data["communities"][0]["id"].as_u64().expect("an id");

        let Response::QueryResult { data } = responder.handle_query(DataQuery::Community {
            project: "test".to_string(),
            community_id: id,
        }) else {
            panic!("the listed id must address a community");
        };
        assert_eq!(data["community_id"], id);
        assert_eq!(data["members"].as_array().unwrap().len(), 3);
    }

    /// `graph report` renders **daemon-side** and returns one markdown string —
    /// the wire carries no `CommunitySummary`/`Bridge`. Pins that the payload is
    /// the rendered document, not a JSON envelope a client would have to format.
    #[test]
    fn graph_report_returns_rendered_markdown() {
        let mut source = MockStateSource::new();
        source.add_project("test".to_string(), state_with_communities());
        let responder = Responder::new(source);

        let response = responder.handle_query(DataQuery::GraphReport {
            project: "test".to_string(),
            top: 5,
        });
        let Response::QueryResult { data } = response else {
            panic!("expected a QueryResult, got {response:?}");
        };

        let md = data["markdown"].as_str().expect("a markdown string");
        assert!(md.starts_with("# Graph Report"), "got: {md}");
        for section in [
            "## Projects",
            "## God nodes",
            "## Communities",
            "## Cross-community bridges",
        ] {
            assert!(md.contains(section), "missing {section} in: {md}");
        }
        assert!(
            md.contains("community 2") && md.contains("community 5") && md.contains("community 7"),
            "every community's roster is in the report — that is why it cannot be \
             composed from `Community` calls: {md}"
        );
        assert_eq!(
            data.as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["markdown"],
            "no new wire value types: the response is the document and nothing else"
        );
    }

    /// A type used from three positions by one function, which also calls an
    /// unbound external — enough to tell family matching, set-union matching and
    /// the ADR-0029 unresolved count apart from each other.
    fn state_with_type_references() -> GraphState {
        use filigrio_core::{Confidence, EdgeTarget, NodeId, TargetRef};

        let mut graph = Graph::default();
        for (id, label, kind) in [
            ("fn:src/a.rs:build", "build", "function"),
            ("ty:src/t.rs:Widget", "Widget", "struct"),
            ("fn:src/a.rs:helper", "helper", "function"),
        ] {
            graph.nodes.push(Node {
                id: NodeId(id.to_string()),
                label: label.to_string(),
                kind: kind.to_string(),
                source_span: None,
                source_file: Some("src/a.rs".to_string()),
                attrs: std::collections::BTreeMap::new(),
            });
        }
        for rel in ["type/param", "type/return", "type/field"] {
            graph.edges.push(Edge {
                source: NodeId("fn:src/a.rs:build".to_string()),
                relation: rel.to_string(),
                confidence: Confidence::Extracted,
                target: EdgeTarget::Node(NodeId("ty:src/t.rs:Widget".to_string())),
            });
        }
        graph.edges.push(Edge {
            source: NodeId("fn:src/a.rs:build".to_string()),
            relation: filigrio_core::relation::CALLS.to_string(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Node(NodeId("fn:src/a.rs:helper".to_string())),
        });
        // Declined call — the ADR-0029 unresolved half, which filters through
        // the same matcher as the resolved half.
        graph.edges.push(Edge {
            source: NodeId("fn:src/a.rs:build".to_string()),
            relation: filigrio_core::relation::CALLS.to_string(),
            confidence: Confidence::Inferred,
            target: EdgeTarget::Symbol(TargetRef::new("unbound")),
        });
        GraphState {
            graph,
            ..Default::default()
        }
    }

    /// `relations` on the neighbor query is a **filter set, OR'd**, matched by
    /// ADR-0036's hierarchical matcher.
    ///
    /// Two regressions in one, both on the flagship agent tool. A second entry
    /// was rejected outright — *"Multiple relations not supported"* — while
    /// `query_graph`'s `context_filter`, the same `Vec<String>` on the same wire,
    /// had always OR'd them. And the resolved path matched with `==`, so
    /// `["type"]`, the one spelling of "what uses this type", selected nothing at
    /// all; the MCP schema had to withhold the value rather than advertise a
    /// filter that silently returned empty.
    #[test]
    fn neighbor_relations_are_an_ord_set_matched_hierarchically() {
        let mut source = MockStateSource::new();
        source.add_project("test".to_string(), state_with_type_references());
        let responder = Responder::new(source);

        let ask = |relations: Vec<&str>| -> serde_json::Value {
            let response = responder.handle_query(DataQuery::Neighbors {
                project: "test".to_string(),
                node: NodeAddress::by_id("fn:src/a.rs:build"),
                direction: Direction::Out,
                relations: relations.iter().map(|r| r.to_string()).collect(),
                include_unresolved: false,
            });
            let Response::QueryResult { data } = response else {
                panic!("expected a QueryResult, got {response:?}");
            };
            data
        };
        let rels = |data: &serde_json::Value| -> Vec<String> {
            let mut out: Vec<String> = data["resolved_neighbors"]
                .as_array()
                .expect("resolved_neighbors")
                .iter()
                .map(|r| {
                    r["edge"]["relation"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                })
                .collect();
            out.sort();
            out
        };

        // The bare family spelling selects every member — the query that used to
        // come back empty.
        assert_eq!(
            rels(&ask(vec!["type"])),
            vec!["type/field", "type/param", "type/return"]
        );
        // A member narrows; two members union.
        assert_eq!(rels(&ask(vec!["type/param"])), vec!["type/param"]);
        assert_eq!(
            rels(&ask(vec!["type/param", "type/return"])),
            vec!["type/param", "type/return"],
            "a second entry is a union, not the error this used to return"
        );
        // A flat relation is untouched by any of it, and the family spelling
        // does not sweep it in.
        assert_eq!(rels(&ask(vec!["calls"])), vec!["calls"]);
        assert!(!rels(&ask(vec!["type"])).contains(&"calls".to_string()));

        // ADR-0029: the unresolved half filters through the *same* matcher, so a
        // filter that keeps `calls` keeps the declined call's count, and one that
        // doesn't, doesn't. The count rides even with include_unresolved:false.
        assert_eq!(ask(vec!["calls"])["unresolved_count"], 1);
        assert_eq!(ask(vec!["type"])["unresolved_count"], 0);
        assert_eq!(
            ask(vec![])["unresolved_count"],
            1,
            "no filter still counts the declined call — never a silent \"nothing here\""
        );
    }

    // ---- ADR-0044: `any` / `semantic`, and the empty case that meant two -----
    //
    // `get_neighbors` read an empty `relations` as "drop the structural
    // scaffolding"; `query_graph` read the *same empty array* as
    // `context_filter: None` — no filter at all. Identical input, opposite
    // meanings, on the two tools an agent uses most, with nothing in either
    // response saying which it had done. These pin the reconciliation.

    /// A file that **contains** a function and **imports** another file, plus a
    /// `calls` edge between functions: the minimum shape that can tell a
    /// structural edge from a semantic one on both tools. The file label is
    /// distinctive so `query_graph`'s trigram seeding lands on it.
    fn state_with_structure_and_meaning() -> GraphState {
        use filigrio_core::{Confidence, EdgeTarget, NodeId};

        let mut graph = Graph::default();
        for (id, label, kind, src) in [
            (
                "file:src/alphamod.rs",
                "alphamod.rs",
                "file",
                "src/alphamod.rs",
            ),
            (
                "file:src/betamod.rs",
                "betamod.rs",
                "file",
                "src/betamod.rs",
            ),
            (
                "fn:src/alphamod.rs:build",
                "build",
                "function",
                "src/alphamod.rs",
            ),
            (
                "fn:src/alphamod.rs:helper",
                "helper",
                "function",
                "src/alphamod.rs",
            ),
        ] {
            graph.nodes.push(Node {
                id: NodeId(id.to_string()),
                label: label.to_string(),
                kind: kind.to_string(),
                source_span: None,
                source_file: Some(src.to_string()),
                attrs: std::collections::BTreeMap::new(),
            });
        }
        let mut edge = |source: &str, relation: &str, target: &str| {
            graph.edges.push(Edge {
                source: NodeId(source.to_string()),
                relation: relation.to_string(),
                confidence: Confidence::Extracted,
                target: EdgeTarget::Node(NodeId(target.to_string())),
            });
        };
        // Structural scaffolding …
        edge(
            "file:src/alphamod.rs",
            filigrio_core::relation::CONTAINS,
            "fn:src/alphamod.rs:build",
        );
        edge(
            "file:src/alphamod.rs",
            filigrio_core::relation::IMPORTS,
            "file:src/betamod.rs",
        );
        // … and code meaning.
        edge(
            "fn:src/alphamod.rs:build",
            filigrio_core::relation::CALLS,
            "fn:src/alphamod.rs:helper",
        );
        GraphState {
            graph,
            ..Default::default()
        }
    }

    fn structural_responder() -> Responder<MockStateSource> {
        let mut source = MockStateSource::new();
        source.add_project("test".to_string(), state_with_structure_and_meaning());
        Responder::new(source)
    }

    /// The relations of `file:src/alphamod.rs`'s neighbors under one filter.
    fn neighbor_relations(
        responder: &Responder<MockStateSource>,
        relations: &[&str],
    ) -> Vec<String> {
        neighbor_relations_of(responder, "file:src/alphamod.rs", relations)
    }

    fn neighbor_relations_of(
        responder: &Responder<MockStateSource>,
        node_id: &str,
        relations: &[&str],
    ) -> Vec<String> {
        let response = responder.handle_query(DataQuery::Neighbors {
            project: "test".to_string(),
            node: NodeAddress::by_id(node_id),
            direction: Direction::Both,
            relations: relations.iter().map(|r| r.to_string()).collect(),
            include_unresolved: false,
        });
        let Response::QueryResult { data } = response else {
            panic!("expected a QueryResult, got {response:?}");
        };
        let mut out: Vec<String> = data["resolved_neighbors"]
            .as_array()
            .expect("resolved_neighbors")
            .iter()
            .map(|r| {
                r["edge"]["relation"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        out.sort();
        out
    }

    /// The relations `query_graph` traverses from the same node under one filter.
    fn query_relations(responder: &Responder<MockStateSource>, relations: &[&str]) -> Vec<String> {
        query_relations_for(responder, "alphamod", relations)
    }

    fn query_relations_for(
        responder: &Responder<MockStateSource>,
        q: &str,
        relations: &[&str],
    ) -> Vec<String> {
        let response = responder.handle_query(DataQuery::Query {
            project: "test".to_string(),
            params: QueryParams {
                query: q.to_string(),
                mode: filigrio_protocol::TraversalMode::Bfs,
                depth: 2,
                budget: 32,
                token_budget: 4000,
                relations: relations.iter().map(|r| r.to_string()).collect(),
                include_unresolved: false,
            },
        });
        let Response::QueryResult { data } = response else {
            panic!("expected a QueryResult, got {response:?}");
        };
        let mut out: Vec<String> = data["edges"]
            .as_array()
            .expect("edges")
            .iter()
            .map(|e| e["relation"].as_str().unwrap_or_default().to_string())
            .collect();
        out.sort();
        out
    }

    /// `any` is the widest filter — the structural scaffolding is there, and it
    /// is exactly what "no filter" used to return.
    #[test]
    fn any_returns_the_structural_edges() {
        let responder = structural_responder();
        assert_eq!(
            neighbor_relations(&responder, &["any"]),
            vec!["contains", "imports"],
            "`any` keeps the physical wiring"
        );
        assert_eq!(
            query_relations(&responder, &["any"]),
            vec!["calls", "contains", "imports"],
            "and the traversal walks through it to reach `build`'s callee"
        );
    }

    /// `semantic` drops it. Same node, same direction, one word different.
    #[test]
    fn semantic_drops_the_structural_edges() {
        let responder = structural_responder();
        assert!(
            neighbor_relations(&responder, &["semantic"]).is_empty(),
            "a file's only edges are scaffolding — `semantic` says so"
        );
        assert!(
            query_relations(&responder, &["semantic"]).is_empty(),
            "and the traversal does not walk them"
        );
        // Not vacuous: `semantic` is *filtering*, not returning nothing. Seeded
        // on the function instead of the file, the same filter keeps the
        // `calls` edge — the empties above are the scaffolding being dropped.
        assert_eq!(
            neighbor_relations_of(&responder, "fn:src/alphamod.rs:build", &["semantic"]),
            vec!["calls"],
            "the function's `contains` parent is dropped, its `calls` callee kept"
        );
        assert_eq!(
            query_relations_for(&responder, "build", &["semantic"]),
            vec!["calls"]
        );
    }

    /// An omitted/empty filter is **exactly** `["semantic"]` — the additive half
    /// of ADR-0044: naming the value is the intended spelling, but absence keeps
    /// working and now means one thing.
    #[test]
    fn an_empty_filter_equals_semantic() {
        let responder = structural_responder();
        assert_eq!(
            neighbor_relations(&responder, &[]),
            neighbor_relations(&responder, &["semantic"]),
        );
        assert_eq!(
            query_relations(&responder, &[]),
            query_relations(&responder, &["semantic"]),
        );
    }

    /// **The divergence itself.** `get_neighbors` and `query_graph` must give an
    /// empty `relations` the same meaning — they did not, and nothing in either
    /// response admitted it.
    ///
    /// Stated as an agreement between the two tools rather than as two separate
    /// expectations, because the defect was precisely that each was locally
    /// correct: assert them apart and both halves pass while the surface lies.
    #[test]
    fn both_tools_give_an_empty_filter_the_same_meaning() {
        let responder = structural_responder();

        // Whatever the empty case means, it means it on both tools: the set of
        // relations each is willing to traverse is the same.
        let kinds = |rows: Vec<String>| {
            let mut k: Vec<String> = rows;
            k.dedup();
            k
        };
        let neighbors_empty = kinds(neighbor_relations(&responder, &[]));
        let query_empty = kinds(query_relations(&responder, &[]));
        assert!(
            !neighbors_empty.iter().any(|r| is_structural(r)),
            "get_neighbors' empty case drops structural edges: {neighbors_empty:?}"
        );
        assert!(
            !query_empty.iter().any(|r| is_structural(r)),
            "…and so must query_graph's, which used to apply no filter at all: \
             {query_empty:?}"
        );

        // And the divergence is visible where it lived: `query_graph` with an
        // empty filter is NOT the same as `query_graph` with `any`. Before the
        // fix these two were identical (empty ⇒ `context_filter: None`), which
        // is the whole defect in one assertion.
        assert_ne!(
            query_relations(&responder, &[]),
            query_relations(&responder, &["any"]),
            "an empty `relations` must no longer mean `any` on query_graph"
        );
        // While on `get_neighbors` it never did — and still doesn't.
        assert_ne!(
            neighbor_relations(&responder, &[]),
            neighbor_relations(&responder, &["any"]),
        );
    }

    #[test]
    fn test_nonexistent_project_error() {
        let source = MockStateSource::new();
        let responder = Responder::new(source);
        let query = DataQuery::Status {
            project: Some("nonexistent".to_string()),
        };

        let response = responder.handle_query(query);
        match response {
            Response::Error { message } => {
                assert!(
                    message.contains("project not registered: 'nonexistent'"),
                    "{message}"
                );
            }
            _ => panic!("Expected Error for nonexistent project"),
        }
    }

    // ---- the unregistered-`project` trap -----------------------------------
    //
    // An agent-eval run spent 4 of its 10 steps re-guessing a `project` value
    // because the error only said what was *wrong*. These pin that one wrong
    // value now costs one step: the message names the accepted vocabulary.

    /// Every data query, not just one, must answer a wrong `project` with the
    /// guidance — `get_node` in particular used to bypass the shared helper.
    fn unregistered_message(query: DataQuery) -> String {
        let mut source = MockStateSource::new();
        source.add_project("hominid".to_string(), GraphState::default());
        source.add_project("next.js".to_string(), GraphState::default());
        match Responder::new(source).handle_query(query) {
            Response::Error { message } => message,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn unregistered_project_error_names_what_is_valid() {
        let message = unregistered_message(DataQuery::GraphStats {
            project: "apps/hominid-signalling-service".to_string(),
        });

        // What was wrong…
        assert!(
            message.contains("project not registered: 'apps/hominid-signalling-service'"),
            "{message}"
        );
        // …and, in the same breath, what is right: the registry's own ids, the
        // two accepted forms, and the omit-for-cwd escape.
        assert!(
            message.contains("hominid") && message.contains("next.js"),
            "lists the registered ids: {message}"
        );
        assert!(
            message.contains("absolute path") && message.contains("omit"),
            "names both accepted forms + the cwd default: {message}"
        );
        // The concept collision that produced the wrong value in the first place.
        assert!(
            message.contains("project_graph"),
            "warns that project_graph's rows are not values for `project`: {message}"
        );
    }

    #[test]
    fn every_data_query_gives_the_same_project_guidance() {
        let bad = "apps/hominid-signalling-service".to_string();
        for query in [
            DataQuery::Status {
                project: Some(bad.clone()),
            },
            DataQuery::GetNode {
                project: bad.clone(),
                node_address: NodeAddress {
                    id: Some("n:x".to_string()),
                    label: None,
                    src: None,
                },
            },
            DataQuery::Neighbors {
                project: bad.clone(),
                node: NodeAddress {
                    id: Some("n:x".to_string()),
                    label: None,
                    src: None,
                },
                direction: Direction::Both,
                relations: vec![],
                include_unresolved: false,
            },
            DataQuery::GodNodes {
                project: bad.clone(),
                limit: 5,
            },
            DataQuery::ProjectGraph {
                project: bad.clone(),
            },
        ] {
            let message = unregistered_message(query);
            assert!(
                message.contains("Registered projects: hominid, next.js")
                    && message.contains("omit"),
                "every query answers a bad project the same way: {message}"
            );
        }
    }

    /// A source that cannot enumerate (the cold store) still says what to do —
    /// it just can't name the set. The empty list must not print as an empty
    /// "Registered projects: ." that reads like "there are none".
    #[test]
    fn unregistered_project_error_without_a_registry_still_guides() {
        let source = MockStateSource::new();
        let message = match Responder::new(source).handle_query(DataQuery::GraphStats {
            project: "whatever".to_string(),
        }) {
            Response::Error { message } => message,
            other => panic!("expected an error, got {other:?}"),
        };
        assert!(!message.contains("Registered projects"), "{message}");
        assert!(message.contains("omit the `project` field"), "{message}");
    }
}
