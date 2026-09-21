//! Commands/queries contract (ADR-0032f §2) — split into planes **at the type
//! level**, not just in prose.
//!
//! - **Data plane** ([`DataQuery`]) — idempotent graph *reads*: `query`,
//!   `neighbors`, `god_nodes`, `path`, `get_node`, `graph_stats`,
//!   `project_graph`, `community`, plus per-project `status` (a read of *graph*
//!   state — file/node counts, last revision — served from a state source in
//!   both lifecycles, so it lives on the data plane even though ADR-0032f §2's
//!   shorthand lists "status" with the meta reads; the *daemon*-level status is
//!   [`MetaQuery::Health`]).
//! - **Control plane** ([`ControlOp`], behind the `control` feature) — state
//!   *changes* ([`Command`]: register/remove, index, export, submit,
//!   daemon stop) **plus** the meta reads about the daemon itself
//!   ([`MetaQuery`]: `health`, `progress`), which are read-only but reveal and
//!   concern daemon lifecycle, per ADR-0032f §2.
//!
//! There were **three** planes until ADR-0042 F9: a narrow "control sliver"
//! (`SliverOp` / `Request::Sliver`) carried register/index for the MCP bridge.
//! The bridge no longer exposes any mutation — it runs with the *user's*
//! filesystem permissions, not the agent's, so every mutation it offered was a
//! confused deputy, and MCP **roots** (the agent-scoped containment that would
//! make one safe) is not built. With the bridge read-only the sliver had no
//! producer left, and a zero-producer wire surface is exactly what the
//! no-speculative-wiring rule rejects, so it is gone: two planes, two request
//! kinds. A control-free client (`default-features = false`) can now express
//! **nothing but reads** — a stronger property than "only two mutations",
//! achieved without the extra enum.
//!
//! Commands are mutations that execute **synchronously at ingress** under the
//! per-project lock, and the response carries the typed outcome (ADR-0042 F6c);
//! data queries are read-only operations served from state. The daemon's
//! priority queue is purely the producer lane (watcher output) — no wire
//! command ever enters it.

pub use filigrio_core::{ChangeSet, Priority};
use serde::{Deserialize, Serialize};

// Re-export the contract's query value types (ADR-0025/27/29). `Direction`
// and `TraversalMode` are the kernel's own enums, re-exported through here so a
// protocol-only client can name them without depending on `filigrio-core`.
pub use crate::query_types::{Direction, NodeAddress, QueryParams, TraversalMode};

/// Commands are mutations, executed synchronously at ingress (control plane,
/// ADR-0042 F6c) — the response is the outcome, never an ack.
#[cfg(feature = "control")]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Command {
    /// Submit a changeset for a project (from 0032a/0032b).
    Submit {
        project: String,
        changeset: ChangeSet,
        #[serde(default)]
        priority: Priority,
    },
    /// Register a project in the registry. "Register" is the vocabulary's
    /// spelling for this operation across every mask (ADR-0042 F7) — it is what
    /// the CLI verb, the MCP tool, and the `ProjectRegistry` this mutates are
    /// all called; the old `ProjectAdd` was the fourth name for one thing.
    ProjectRegister { path: String },
    /// Remove a project from the registry.
    ProjectRemove { project: String },
    /// Index a project (incremental update; an index on an empty store IS the
    /// cold build — the manifest dedup gate makes them the same thing, which is
    /// why the old `ProjectBuild` verb was collapsed into this one, ADR-0042 F5;
    /// the old `Validate` verb followed in F6 — an index on a clean tree already
    /// IS the read-only check: `changed=0`, no delta, no writes).
    ///
    /// A wire index is always the **full** reconcile: reconcile depth is not a
    /// wire concept (ADR-0042 F6b) — the shallow mtime fast-path exists only on
    /// the daemon-internal `Op::Reconcile`, ridden by the watcher's producer
    /// lane. Executes synchronously; the response carries the outcome
    /// ([`CommandOutcome::Indexed`], ADR-0042 F6c).
    ProjectIndex {
        project: String,
        /// Reindex from scratch (wipe and rebuild). Reserved (ADR-0042 F5):
        /// until implemented, both execution paths fail fast with an explicit
        /// "not implemented yet" error — a flag is never silently ignored
        /// (ADR-0029 honesty).
        ///
        /// Named `clean` (CLI `--clean`), not `force` (ADR-0042 F7): `--force`
        /// already means "override the safety check" on `project register`, and
        /// one flag word cannot carry two semantics. `--clean` has the right
        /// precedent — clean build, `make clean`, `npm ci`.
        #[serde(default)]
        clean: bool,
    },
    /// Turn a project's watch mode on/off (ADR-0042 F6b). Watching is an
    /// explicit, persisted, per-project mode — never an ambient property of the
    /// resident daemon; a registered-but-unwatched project is cold by contract.
    /// `on: true` starts the watcher FIRST (events begin buffering), then runs a
    /// synchronous deep reconcile, then persists — the lost-event-free ordering.
    /// Control plane only — never reachable from the MCP bridge (agents don't
    /// manage watchers), which since ADR-0042 F9 is true of every mutation.
    ProjectWatch { project: String, on: bool },
    /// Export a project's `graph.json` interchange snapshot from its store
    /// (ADR-0042 F2/B4: the apply path no longer snapshots — this verb is the
    /// only producer of `graph.json`, for the perf-ledger jq recipes and
    /// oracle comparisons).
    ProjectExport { project: String },
    /// Persist a project's resident state to its store **now** (ADR-0042 F4 /
    /// B12). Under write-behind a producer-lane (watcher) apply updates resident
    /// state and marks the project dirty; the daemon persists it on quiescence,
    /// a max-dirty-age cap, shutdown, or dirty LRU eviction. This verb is the
    /// explicit trigger for the cases none of those cover — "make what you are
    /// serving readable from outside, now".
    ///
    /// Named **flush**, not *checkpoint*: ADR-0042 Phase 3 uses "checkpoint" for
    /// commit-keyed publication, which is a different operation. Idempotent: a
    /// clean project reports `wrote: false` and rewrites nothing.
    ///
    /// Control plane only — never reachable from the MCP bridge: an agent reads
    /// the daemon's answers, not `.filigrio-out`.
    ProjectFlush { project: String },
    /// Daemon lifecycle: stop.
    DaemonStop,
}

/// Meta reads about the **daemon**, not the graph (control plane, ADR-0032f §2:
/// "read-only but about the *daemon*"). Kept apart from [`Command`] so the
/// wire mutation vocabulary stays mutations-only.
#[cfg(feature = "control")]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum MetaQuery {
    /// Daemon health: uptime, memory, project count, queue depth.
    Health,
    /// Reserved stub: the future **streaming** hook for progress over a
    /// long-held command connection (ADR-0042 F6c). Commands execute
    /// synchronously and the response is the outcome, so nothing polls this
    /// today — it stays on the contract so the streaming UX can land without a
    /// wire break.
    Progress { project: String },
}

/// The full control plane: mutations + daemon meta reads (ADR-0032f §2).
///
/// Untagged: each inner enum carries its own `kind` tag, so the wire shape of
/// every existing command is unchanged (`{"type":"Command","kind":"Submit",…}`),
/// and the meta reads join the same envelope (`{"type":"Command","kind":"Health"}`).
#[cfg(feature = "control")]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ControlOp {
    /// A mutation, executed synchronously at ingress (ADR-0042 F6c).
    Command(Command),
    /// A daemon meta read, answered synchronously by the daemon itself.
    Meta(MetaQuery),
}

/// Data-plane queries: idempotent graph reads served from state.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum DataQuery {
    /// Per-project status: file/node counts, drift, last-applied revision — a
    /// read of *graph* state (data plane; the daemon-level counterpart is
    /// `MetaQuery::Health` on the control plane).
    Status { project: Option<String> },
    /// Query the graph with ADR-0025/0029 enhanced parameters.
    Query {
        project: String,
        #[serde(flatten)]
        params: crate::query_types::QueryParams,
    },
    /// Get neighbors with ADR-0027 addressing and enhanced filters.
    Neighbors {
        project: String,
        /// ADR-0027 {id,label,src} addressing — not a bare id, so homonyms can
        /// be disambiguated the same way `get_node` disambiguates them.
        #[serde(default)]
        node: NodeAddress,
        #[serde(default)]
        direction: Direction, // ADR-0029: typed edge direction
        #[serde(default)]
        relations: Vec<String>, // ADR-0025: relation filter
        #[serde(default)]
        include_unresolved: bool, // ADR-0029: include unresolved edges
    },
    /// Get god nodes (high-centrality nodes).
    GodNodes {
        project: String,
        #[serde(default)]
        limit: usize,
    },
    /// Find path between nodes.
    ///
    /// `from`/`to` are ADR-0027 addresses, not bare ids — a caller can name
    /// either endpoint by label (+ optional src) and let the daemon resolve
    /// it the same way `get_node` does, instead of pre-resolving client-side.
    Path {
        project: String,
        #[serde(default)]
        from: NodeAddress,
        #[serde(default)]
        to: NodeAddress,
        #[serde(default)]
        max_hops: u8,
    },
    /// Get specific node by ADR-0027 address.
    GetNode {
        project: String,
        #[serde(default)]
        node_address: NodeAddress,
    },
    /// Get graph statistics.
    GraphStats { project: String },
    /// Get project graph (the monorepo architecture map).
    ProjectGraph { project: String },
    /// Get the members of a community by id (ADR-0024 clustering surface —
    /// the legacy MCP server's `get_community` tool, restored to the
    /// enhanced protocol's data-plane superset).
    Community {
        project: String,
        #[serde(default)]
        community_id: u64,
    },
    /// Enumerate the communities — `(id, label, size, cohesion)` per community,
    /// largest first, capped at `limit`.
    ///
    /// This is what makes [`DataQuery::Community`] reachable: a community *id*
    /// is not emitted by any other read. The `community=` attr stamped on nodes
    /// carries the derived **label** (the highest-degree member's name), not the
    /// id, so an id could previously only be guessed. Rosters are deliberately
    /// not included — that is what `Community` is for, one id at a time.
    ///
    /// `limit` is capped because clustering routinely produces hundreds of
    /// communities (321 on this repo; ADR-0024's dogfood saw 155–655), so an
    /// unbounded dump is a token-budget hazard on the MCP surface. `0` means
    /// the caller's default, exactly as `GodNodes { limit }` treats it.
    ListCommunities {
        project: String,
        #[serde(default)]
        limit: usize,
    },
    /// Render the human-facing `GRAPH_REPORT.md` for a project (the classic
    /// `graph report`, ADR-0032f §F3 option 1).
    ///
    /// The **markdown is rendered daemon-side** and the response carries it as
    /// a single string: `GraphReport` is `stats + top-`top` god nodes + every
    /// community's roster and cohesion + every cross-community bridge + the
    /// project graph`, and shipping that as structured JSON would mean putting
    /// `CommunitySummary`/`Bridge` on the wire for a payload whose only
    /// consumer writes it straight to a file. The daemon already depends on
    /// `filigrio-query`, so `render_markdown` runs where the graph is.
    ///
    /// `top` is the number of god nodes to rank — the same meaning as classic's
    /// `graph report --top` (which defaults to 20; the CLI mask still does).
    GraphReport {
        project: String,
        #[serde(default)]
        top: usize,
    },
}

/// A request from a client, routed by plane.
///
/// Wire tags are preserved from the pre-split contract: the control plane keeps
/// the `"Command"` tag (its `Health`/`Progress` meta reads moved here from the
/// old `Query` envelope), the data plane keeps `"Query"`.
///
/// **Two kinds, not three** (ADR-0042 F9): the `"Sliver"` envelope is retired
/// along with the MCP bridge's mutation surface, and — following the F5/F6
/// pattern — a stale `{"type":"Sliver",…}` frame is a **dead frame**, rejected
/// at parse rather than reinterpreted as a command (pinned by
/// `tests/plane_split.rs`). Note what that buys: the boundary is no longer "the
/// bridge may express two mutations", it is "a `default-features = false`
/// build has no mutation vocabulary at all".
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    /// Control plane: mutation or daemon meta read.
    #[cfg(feature = "control")]
    #[serde(rename = "Command")]
    Control(ControlOp),
    /// Data plane: read-only graph query served from state.
    #[serde(rename = "Query")]
    Data(DataQuery),
}

impl Request {
    /// A data-plane request.
    pub fn data(query: DataQuery) -> Self {
        Request::Data(query)
    }

    /// A control-plane mutation.
    #[cfg(feature = "control")]
    pub fn command(cmd: Command) -> Self {
        Request::Control(ControlOp::Command(cmd))
    }

    /// A control-plane daemon meta read.
    #[cfg(feature = "control")]
    pub fn meta(meta: MetaQuery) -> Self {
        Request::Control(ControlOp::Meta(meta))
    }
}

/// A response from the daemon.
///
/// Commands execute synchronously and the response IS the outcome (ADR-0042
/// F6c) — there is no ack-then-poll: the old `CommandAccepted { job_id }` ack
/// (and the daemon's job-id minting behind it) is deleted, because an ack let a
/// failed apply be logged daemon-side and silently dropped while the client
/// exited 0.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Response {
    /// A command executed to completion; the typed outcome is the result.
    CommandCompleted { outcome: CommandOutcome },
    /// Query result.
    QueryResult { data: serde_json::Value },
    /// Error response.
    Error { message: String },
}

/// The typed outcome of a synchronously executed command (ADR-0042 F6c) — one
/// conventional shape per verb, carried by [`Response::CommandCompleted`].
/// Ungated (not behind the `control` feature) because it is reachable from
/// [`Response`], which is one type for every client: a `default-features =
/// false` build must still be able to *decode* a `CommandCompleted` frame, even
/// though since ADR-0042 F9 it can no longer provoke one (it has no way to send
/// a command). Decoding a response is not a privilege; sending a command is.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum CommandOutcome {
    /// `ProjectIndex`: the deep reconcile ran; `changed` is the number of
    /// changed files applied (`0` = clean tree, nothing written). `vanished`
    /// counts those that had been deleted by the time the engine read them and
    /// were converged into removals (ADR-0042 F8) — visible, never silent.
    Indexed {
        project: String,
        changed: usize,
        #[serde(default)]
        vanished: usize,
    },
    /// `Submit`: the (gated) changeset applied; `changed` files survived the
    /// scope+dedup gate (`0` = fully deduped, nothing written). `vanished` as
    /// for [`CommandOutcome::Indexed`].
    Applied {
        project: String,
        changed: usize,
        #[serde(default)]
        vanished: usize,
    },
    /// `ProjectExport`: `graph.json` written at `path`.
    Exported { project: String, path: String },
    /// `ProjectFlush`: `wrote` is whether the project actually had unpersisted
    /// resident state. `false` is the honest idempotent case — not a failure,
    /// and not a 210 MB rewrite for nothing (ADR-0042 F4).
    Flushed {
        project: String,
        #[serde(default)]
        wrote: bool,
    },
    /// `ProjectWatch`: the resulting watch state. `changed` carries the initial
    /// converge's applied-file count on `watch on` (`None` when no converge ran,
    /// i.e. `off` or an idempotent repeat); `vanished` is that converge's F8
    /// count, on the same `None` schedule; `note` says when the call was an
    /// idempotent no-op.
    Watch {
        project: String,
        watching: bool,
        #[serde(default)]
        changed: Option<usize>,
        #[serde(default)]
        vanished: Option<usize>,
        #[serde(default)]
        note: Option<String>,
    },
    /// `ProjectRegister`: confirmation.
    Registered { project: String, path: String },
    /// `ProjectRemove`: confirmation.
    Removed { project: String },
    /// `DaemonStop`: graceful shutdown initiated.
    Stopping,
}

/// Health status of the daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthStatus {
    /// Uptime in seconds.
    pub uptime_secs: u64,
    /// Number of registered projects.
    pub project_count: usize,
    /// Current **intake** queue depth. NOTE: this counts only commands not yet
    /// collected by the drain; once collected they move to the worker pool and are
    /// reflected in `applies_inflight` / `applies_deferred` instead (ADR-0032 §2).
    pub queue_depth: usize,
    /// Applies currently running on the worker pool. Together with
    /// `applies_deferred` this is the pool work `queue_depth` does NOT see — without
    /// it the daemon reads as idle while the pool churns.
    #[serde(default)]
    pub applies_inflight: usize,
    /// Apply jobs collected off the queue but not yet dispatched (pool at capacity,
    /// or the project already has an apply in flight).
    #[serde(default)]
    pub applies_deferred: usize,
    /// Memory usage in bytes (RSS).
    pub memory_bytes: u64,
    /// Last activity timestamp.
    pub last_activity: String,
}

/// Status of a project.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectStatus {
    /// Project name.
    pub project: String,
    /// Whether the index has drifted from the tree.
    pub has_drift: bool,
    /// Last applied revision.
    pub last_revision: Option<String>,
    /// Number of files indexed.
    pub file_count: usize,
    /// Number of nodes in the graph.
    pub node_count: usize,
    /// Whether a live filesystem watcher is following this project (ADR-0042
    /// F6b: watch is an explicit per-project mode; staleness must be visible).
    /// Defaults to `false` for producers that don't know (cold one-shot).
    #[serde(default)]
    pub watching: bool,
    /// Whether the daemon holds resident state **newer than the store**
    /// (ADR-0042 F4/B12). In-daemon queries are unaffected — they are served
    /// from that resident state — but an out-of-process reader of
    /// `.filigrio-out` is looking at something older. ADR-0029 honesty applied
    /// to persistence: write-behind is only acceptable if it is visible.
    #[serde(default)]
    pub dirty: bool,
    /// How long the project has been dirty, i.e. the upper bound on how stale
    /// the on-disk copy may be. `None` when clean.
    #[serde(default)]
    pub dirty_for_secs: Option<u64>,
    /// How long ago **this daemon lifetime** last persisted the project.
    /// `None` means it has not — the checkpoint on disk (if any) predates the
    /// process, which is deliberately not reported as "just written".
    #[serde(default)]
    pub last_persisted_secs_ago: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_serialization_flattened() {
        let query = DataQuery::Query {
            project: "test-project".to_string(),
            params: crate::query_types::QueryParams {
                query: "test search".to_string(),
                mode: TraversalMode::Bfs,
                depth: 3,
                budget: 50,
                token_budget: 4000,
                relations: vec![],
                include_unresolved: false,
            },
        };

        let json = serde_json::to_string(&query).unwrap();
        println!("Serialized Query: {}", json);

        // The flattened format should have query fields at the same level as project
        assert!(json.contains(r#""project":"test-project""#));
        assert!(json.contains(r#""query":"test search""#));
        assert!(json.contains(r#""mode":"bfs""#));
        assert!(json.contains(r#""depth":3"#));
        assert!(json.contains(r#""budget":50"#));

        // Should be able to deserialize back
        let deserialized: DataQuery = serde_json::from_str(&json).unwrap();
        match deserialized {
            DataQuery::Query { project, params } => {
                assert_eq!(project, "test-project");
                assert_eq!(params.query, "test search");
                assert_eq!(params.depth, 3);
                assert_eq!(params.budget, 50);
            }
            _ => panic!("Expected Query variant"),
        }
    }

    #[test]
    fn test_query_request_roundtrip() {
        let request = Request::Data(DataQuery::Query {
            project: "test-project".to_string(),
            params: crate::query_types::QueryParams {
                query: "test search".to_string(),
                mode: TraversalMode::Bfs,
                depth: 2,
                budget: 32,
                token_budget: 4000,
                relations: vec![],
                include_unresolved: false,
            },
        });

        let json = serde_json::to_string(&request).unwrap();
        println!("Serialized Request: {}", json);

        let deserialized: Request = serde_json::from_str(&json).unwrap();
        match deserialized {
            Request::Data(DataQuery::Query { project, params }) => {
                assert_eq!(project, "test-project");
                assert_eq!(params.query, "test search");
            }
            _ => panic!("Expected Query request"),
        }
    }

    #[test]
    fn test_python_format_query_request() {
        // Test the exact format that Python clients send
        let python_format = r#"{
            "type": "Query",
            "kind": "Query",
            "project": "test-project",
            "query": "test",
            "mode": "bfs", 
            "depth": 3,
            "budget": 50,
            "relations": []
        }"#;

        println!("Trying to deserialize Python format: {}", python_format);
        match serde_json::from_str::<Request>(python_format) {
            Ok(req) => {
                println!("✅ Python format deserialized successfully: {:?}", req);
                match req {
                    Request::Data(DataQuery::Query { project, params }) => {
                        assert_eq!(project, "test-project");
                        assert_eq!(params.query, "test");
                        assert_eq!(params.mode, TraversalMode::Bfs);
                        assert_eq!(params.depth, 3);
                        assert_eq!(params.budget, 50);
                    }
                    _ => panic!("Expected Query::Query variant"),
                }
            }
            Err(e) => {
                println!("❌ Python format deserialization failed: {}", e);
                panic!("Failed to deserialize Python format");
            }
        }
    }
}
