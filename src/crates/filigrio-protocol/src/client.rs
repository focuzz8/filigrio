//! Client implementations for the daemon contract (ADR-0032f).
//!
//! Convenience methods are plane-scoped: control-plane helpers (`submit`,
//! `project_register`, `stop`, `health`, …) exist only under the `control`
//! feature; the data-plane `project_status` is always available.
//! A control-free client (`default-features = false`) is therefore read-only by
//! construction — the bridge has no mutation surface at all (ADR-0042 F9).

#[cfg(feature = "control")]
use crate::contract::{Command, CommandOutcome, HealthStatus, MetaQuery};
use crate::contract::{DataQuery, ProjectStatus, Request, Response};
use crate::Error;
use crate::Result;
use serde_json;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

/// Decode a completed command's typed outcome (ADR-0042 F6c: the response IS
/// the outcome — there is no ack/job-id to decode anymore).
///
/// Plane-scoped like its callers: only the control-plane helpers decode a
/// command outcome, so a control-free build does not compile this at all.
#[cfg(feature = "control")]
fn outcome(response: Response) -> Result<CommandOutcome> {
    match response {
        Response::CommandCompleted { outcome } => Ok(outcome),
        Response::Error { message } => Err(Error::Socket(message)),
        _ => Err(Error::Socket("unexpected response".to_string())),
    }
}

/// Decode a `QueryResult` payload into `T`.
fn query_result<T: serde::de::DeserializeOwned>(response: Response) -> Result<T> {
    match response {
        Response::QueryResult { data } => {
            serde_json::from_value(data).map_err(|e| Error::Serialization(e.to_string()))
        }
        Response::Error { message } => Err(Error::Socket(message)),
        _ => Err(Error::Socket("unexpected response".to_string())),
    }
}

/// Trait for daemon clients.
pub trait DaemonClientTrait {
    /// Send a request and receive a response.
    fn send(&self, request: Request) -> Result<Response>;

    // ---- data plane (always available) ----

    /// Get project status.
    fn project_status(&self, project: String) -> Result<ProjectStatus> {
        query_result(self.send(Request::data(DataQuery::Status {
            project: Some(project),
        }))?)
    }

    // ---- control plane (compiled out without the `control` feature) ----

    /// Submit a changeset (applied synchronously through the signal gate;
    /// the outcome carries the effective change count).
    #[cfg(feature = "control")]
    fn submit(
        &self,
        project: String,
        changeset: filigrio_core::ChangeSet,
    ) -> Result<CommandOutcome> {
        outcome(self.send(Request::command(Command::Submit {
            project,
            changeset,
            priority: crate::contract::Priority::Fs,
        }))?)
    }

    /// Register a project with the daemon (ADR-0032 §2 registry). This is the
    /// *only* way to register since ADR-0042 F9 — registration is an operator
    /// action on the control plane, never something an agent asks the bridge
    /// for.
    #[cfg(feature = "control")]
    fn project_register(&self, path: String) -> Result<()> {
        outcome(self.send(Request::command(Command::ProjectRegister { path }))?).map(|_| ())
    }

    // No `project_index` helper: the CLI and the daemon suites both send
    // `Command::ProjectIndex` directly (it carries `clean`, which a one-line
    // helper would have to hide). The removed sliver `index_project` helper is
    // not replaced here — a wrapper with one caller is wiring nothing produces.

    /// Deregister a project from the daemon (ADR-0032 §2 registry).
    #[cfg(feature = "control")]
    fn project_remove(&self, project: String) -> Result<()> {
        outcome(self.send(Request::command(Command::ProjectRemove { project }))?).map(|_| ())
    }

    /// Get daemon health (a control-plane meta read, ADR-0032f §2).
    #[cfg(feature = "control")]
    fn health(&self) -> Result<HealthStatus> {
        query_result(self.send(Request::meta(MetaQuery::Health))?)
    }

    /// Stop the daemon gracefully.
    #[cfg(feature = "control")]
    fn stop(&self) -> Result<()> {
        outcome(self.send(Request::command(Command::DaemonStop))?).map(|_| ())
    }
}

/// In-process client for protocol validation and testing.
///
/// This client provides protocol surface validation without requiring a daemon
/// process. It's designed for:
/// - Protocol contract validation (ensuring all commands/queries parse correctly)
/// - Integration testing (verifying protocol compatibility)
/// - Development environments (for testing without daemon setup)
///
/// **Architectural Note**: Per ADR-0032f, the actual "in-process" execution
/// semantics are provided by the daemon binary itself via the one-shot subcommand.
/// This client serves as a lightweight protocol test harness rather than a
/// full in-process GraphState implementation.
///
/// For true in-process execution with proper state handling, use:
/// ```bash
/// filigrio-daemon --socket <path> one-shot --request-type Query --payload '...'
/// ```
pub struct InProcessClient {
    /// Enable detailed protocol validation responses
    detailed_mode: bool,
}

impl InProcessClient {
    /// Create a new in-process client for basic protocol validation.
    pub fn new() -> Self {
        Self {
            detailed_mode: false,
        }
    }

    /// Create an in-process client with detailed validation responses.
    pub fn with_detailed_mode() -> Self {
        Self {
            detailed_mode: true,
        }
    }

    /// Validate that a protocol request is well-formed and responds appropriately.
    ///
    /// This supports protocol surface validation by:
    /// - Accepting all defined Query and Command variants
    /// - Returning protocol-compliant responses (success or appropriate errors)
    /// - Providing meaningful validation feedback for protocol development
    fn validate_request(&self, request: Request) -> Result<Response> {
        match request {
            #[cfg(feature = "control")]
            Request::Control(op) => self.validate_control(op),
            Request::Data(query) => self.validate_data(query),
        }
    }

    #[cfg(feature = "control")]
    fn validate_control(&self, op: crate::contract::ControlOp) -> Result<Response> {
        use crate::contract::ControlOp;
        match op {
            // Health meta reads are special - provide a valid response for protocol validation
            ControlOp::Meta(MetaQuery::Health) => Ok(Response::QueryResult {
                data: serde_json::to_value(HealthStatus {
                    uptime_secs: 0,
                    project_count: 0,
                    queue_depth: 0,
                    applies_inflight: 0,
                    applies_deferred: 0,
                    memory_bytes: 0,
                    last_activity: "0s".to_string(),
                })
                .map_err(|e| Error::Serialization(e.to_string()))?,
            }),
            ControlOp::Meta(MetaQuery::Progress { project }) => {
                if self.detailed_mode {
                    Ok(Response::QueryResult {
                        data: serde_json::json!({
                            "project": project,
                            "progress": 0,
                            "validation": "InProcessClient: Progress meta read syntax validated successfully"
                        }),
                    })
                } else {
                    Err(Error::Socket(
                        "InProcessClient: Progress not implemented - use daemon or one-shot mode"
                            .to_string(),
                    ))
                }
            }
            ControlOp::Command(cmd) => self.validate_command(cmd),
        }
    }

    #[cfg(feature = "control")]
    fn validate_command(&self, cmd: Command) -> Result<Response> {
        // Command validation — accept every defined command; detailed mode
        // fabricates the synchronous typed outcome the daemon would return
        // (ADR-0042 F6c), so the wire shape can be exercised without a daemon.
        let done = |outcome: CommandOutcome| Ok(Response::CommandCompleted { outcome });
        match cmd {
            Command::ProjectRegister { path } => {
                if self.detailed_mode {
                    done(CommandOutcome::Registered {
                        project: "validation".to_string(),
                        path,
                    })
                } else {
                    Err(Error::Socket("InProcessClient: ProjectRegister command not implemented - use daemon or one-shot mode for project and path operations".to_string()))
                }
            },
            Command::ProjectRemove { project } => {
                if self.detailed_mode {
                    done(CommandOutcome::Removed { project })
                } else {
                    Err(Error::Socket("InProcessClient: ProjectRemove command not implemented - use daemon or one-shot mode".to_string()))
                }
            },
            Command::Submit { project, changeset: _, priority: _ } => {
                if self.detailed_mode {
                    done(CommandOutcome::Applied { project, changed: 0, vanished: 0 })
                } else {
                    Err(Error::Socket("InProcessClient: Submit command not implemented - use daemon or one-shot mode".to_string()))
                }
            },
            Command::ProjectIndex { project, clean: _ } => {
                if self.detailed_mode {
                    done(CommandOutcome::Indexed { project, changed: 0, vanished: 0 })
                } else {
                    Err(Error::Socket("InProcessClient: ProjectIndex command not implemented - use daemon or one-shot mode".to_string()))
                }
            },
            Command::ProjectWatch { project, on } => {
                if self.detailed_mode {
                    done(CommandOutcome::Watch {
                        project,
                        watching: on,
                        changed: None,
                        vanished: None,
                        note: Some("validation only — no watcher started".to_string()),
                    })
                } else {
                    Err(Error::Socket("InProcessClient: ProjectWatch requires the resident daemon".to_string()))
                }
            },
            Command::ProjectExport { project } => {
                if self.detailed_mode {
                    done(CommandOutcome::Exported {
                        project,
                        path: "validation".to_string(),
                    })
                } else {
                    Err(Error::Socket("InProcessClient: ProjectExport command not implemented - use daemon or one-shot mode".to_string()))
                }
            },
            Command::ProjectFlush { project } => {
                if self.detailed_mode {
                    // Validation-only: nothing resident, so nothing outstanding.
                    done(CommandOutcome::Flushed { project, wrote: false })
                } else {
                    Err(Error::Socket("InProcessClient: ProjectFlush command not implemented - use daemon or one-shot mode".to_string()))
                }
            },
            Command::DaemonStop => {
                Err(Error::Socket("InProcessClient: Daemon stop command requires actual daemon process. Use 'filigrio daemon stop'.".to_string()))
            },
        }
    }

    fn validate_data(&self, query: DataQuery) -> Result<Response> {
        match query {
            // Query validation - accept all defined queries
            DataQuery::Status { project } => {
                if self.detailed_mode {
                    Ok(Response::QueryResult {
                        data: serde_json::json!({
                            "project": project.unwrap_or_else(|| "validation".to_string()),
                            "state": "validation",
                            "message": "InProcessClient: Use daemon or one-shot mode for actual status queries"
                        }),
                    })
                } else {
                    Err(Error::Socket("InProcessClient: Status query not implemented - use daemon or one-shot mode".to_string()))
                }
            }
            DataQuery::Query { project, params } => {
                if self.detailed_mode {
                    Ok(Response::QueryResult {
                        data: serde_json::json!({
                            "query": params.query,
                            "project": project,
                            "mode": format!("{:?}", params.mode),
                            "result": [],
                            "validation": "InProcessClient: Query syntax validated successfully"
                        }),
                    })
                } else {
                    Err(Error::Socket(
                        "InProcessClient: Query not implemented - use daemon or one-shot mode"
                            .to_string(),
                    ))
                }
            }
            DataQuery::GodNodes { project, limit } => {
                if self.detailed_mode {
                    Ok(Response::QueryResult {
                        data: serde_json::json!({
                            "project": project,
                            "limit": limit,
                            "nodes": [],
                            "validation": "InProcessClient: God nodes query syntax validated successfully"
                        }),
                    })
                } else {
                    Err(Error::Socket("InProcessClient: God nodes query not implemented - use daemon or one-shot mode".to_string()))
                }
            }
            DataQuery::Path {
                project,
                from,
                to,
                max_hops,
            } => {
                if self.detailed_mode {
                    Ok(Response::QueryResult {
                        data: serde_json::json!({
                            "project": project,
                            "from": from,
                            "to": to,
                            "max_hops": max_hops,
                            "path": [],
                            "validation": "InProcessClient: Path query syntax validated successfully"
                        }),
                    })
                } else {
                    Err(Error::Socket(
                        "InProcessClient: Path query not implemented - use daemon or one-shot mode"
                            .to_string(),
                    ))
                }
            }
            DataQuery::GetNode {
                project,
                node_address,
            } => {
                if self.detailed_mode {
                    Ok(Response::QueryResult {
                        data: serde_json::json!({
                            "project": project,
                            "node_address": node_address,
                            "node": null,
                            "validation": "InProcessClient: GetNode query syntax validated successfully"
                        }),
                    })
                } else {
                    Err(Error::Socket("InProcessClient: GetNode query not implemented - use daemon or one-shot mode".to_string()))
                }
            }
            DataQuery::Neighbors {
                project,
                node,
                direction,
                relations,
                include_unresolved,
            } => {
                if self.detailed_mode {
                    Ok(Response::QueryResult {
                        data: serde_json::json!({
                            "project": project,
                            "node": node,
                            "direction": format!("{:?}", direction),
                            "relations": relations,
                            "include_unresolved": include_unresolved,
                            "neighbors": [],
                            "validation": "InProcessClient: Neighbors query syntax validated successfully"
                        }),
                    })
                } else {
                    Err(Error::Socket("InProcessClient: Neighbors query not implemented - use daemon or one-shot mode".to_string()))
                }
            }
            DataQuery::GraphStats { project } => {
                if self.detailed_mode {
                    Ok(Response::QueryResult {
                        data: serde_json::json!({
                            "project": project,
                            "nodes": 0,
                            "edges": 0,
                            "validation": "InProcessClient: GraphStats query syntax validated successfully"
                        }),
                    })
                } else {
                    Err(Error::Socket("InProcessClient: GraphStats query not implemented - use daemon or one-shot mode".to_string()))
                }
            }
            DataQuery::ProjectGraph { project } => {
                if self.detailed_mode {
                    Ok(Response::QueryResult {
                        data: serde_json::json!({
                            "project": project,
                            "graph": [],
                            "validation": "InProcessClient: ProjectGraph query syntax validated successfully"
                        }),
                    })
                } else {
                    Err(Error::Socket("InProcessClient: ProjectGraph query not implemented - use daemon or one-shot mode".to_string()))
                }
            }
            DataQuery::Community {
                project,
                community_id,
            } => {
                if self.detailed_mode {
                    Ok(Response::QueryResult {
                        data: serde_json::json!({
                            "project": project,
                            "community_id": community_id,
                            "members": [],
                            "validation": "InProcessClient: Community query syntax validated successfully"
                        }),
                    })
                } else {
                    Err(Error::Socket("InProcessClient: Community query not implemented - use daemon or one-shot mode".to_string()))
                }
            }
            DataQuery::ListCommunities { project, limit } => {
                if self.detailed_mode {
                    Ok(Response::QueryResult {
                        data: serde_json::json!({
                            "project": project,
                            "limit": limit,
                            "communities": [],
                            "validation": "InProcessClient: ListCommunities query syntax validated successfully"
                        }),
                    })
                } else {
                    Err(Error::Socket("InProcessClient: ListCommunities query not implemented - use daemon or one-shot mode".to_string()))
                }
            }
            DataQuery::GraphReport { project, top } => {
                if self.detailed_mode {
                    Ok(Response::QueryResult {
                        data: serde_json::json!({
                            "project": project,
                            "top": top,
                            "markdown": "",
                            "validation": "InProcessClient: GraphReport query syntax validated successfully"
                        }),
                    })
                } else {
                    Err(Error::Socket("InProcessClient: GraphReport query not implemented - use daemon or one-shot mode".to_string()))
                }
            }
        }
    }
}

impl Default for InProcessClient {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonClientTrait for InProcessClient {
    fn send(&self, request: Request) -> Result<Response> {
        self.validate_request(request)
    }
}

/// Default read timeout for **command** requests: commands execute
/// synchronously daemon-side (ADR-0042 F6c) and a cold deep index can run
/// minutes at monorepo scale (measured worst-case ≈ minutes at next.js scale),
/// so the client must not kill the connection at query-snappy timeouts.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

/// Socket-based client (for daemon mode).
///
/// Two read-timeout tiers: `timeout` for queries (snappy, default 30s) and
/// `command_timeout` for mutations (default [`COMMAND_TIMEOUT`] — the daemon
/// runs the work before replying, F6c).
pub struct SocketClient {
    socket_path: std::path::PathBuf,
    timeout: Duration,
    command_timeout: Duration,
}

impl SocketClient {
    /// A client **addressed at** `socket_path`. No I/O happens here: the
    /// connection is per-request, in [`send`](DaemonClientTrait::send), so there
    /// is nothing that can fail and nothing to report — this was `connect()
    /// -> Result<Self>` until 2026-07-28, an asserted-infallible `Result`
    /// (ADR-0041) whose name promised a connection it never made. Callers that
    /// need to know whether anything is listening ask [`is_reachable`](Self::is_reachable).
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
            timeout: Duration::from_secs(30),
            command_timeout: COMMAND_TIMEOUT,
        }
    }

    /// Whether a daemon is **listening** on this client's socket, right now.
    ///
    /// The real liveness probe the old `connect() -> Result<Self>` was mistaken
    /// for: it opens a connection and drops it. Callers that used to branch on
    /// `connect`'s (impossible) `Err` — "no daemon running", "State: stopped",
    /// auto-start — branch on this instead. Racy by nature, like every such
    /// probe; a `true` here can still be followed by a failing `send`, and the
    /// send's error remains the authority.
    pub fn is_reachable(&self) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::net::UnixStream::connect(&self.socket_path).is_ok()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Set the **query** request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the **command** request timeout (sync commands can run minutes).
    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    /// The read timeout for one request: control-plane **mutations** wait out
    /// the synchronous execution (ADR-0042 F6c — an index can run minutes);
    /// reads, including the control plane's own meta reads, stay snappy.
    fn timeout_for(&self, request: &Request) -> Duration {
        match request {
            #[cfg(feature = "control")]
            Request::Control(crate::contract::ControlOp::Command(_)) => self.command_timeout,
            _ => self.timeout,
        }
    }
}

impl DaemonClientTrait for SocketClient {
    fn send(&self, request: Request) -> Result<Response> {
        #[cfg(unix)]
        {
            use std::os::unix::net::UnixStream;

            let mut stream = UnixStream::connect(&self.socket_path)
                .map_err(|e| Error::Socket(format!("connect failed: {}", e)))?;

            let timeout = self.timeout_for(&request);
            stream
                .set_read_timeout(Some(timeout))
                .map_err(|e| Error::Socket(format!("set timeout failed: {}", e)))?;
            stream
                .set_write_timeout(Some(timeout))
                .map_err(|e| Error::Socket(format!("set timeout failed: {}", e)))?;

            // Serialize request
            let request_bytes =
                serde_json::to_vec(&request).map_err(|e| Error::Serialization(e.to_string()))?;

            // Send length prefix
            let len = request_bytes.len() as u32;
            stream
                .write_all(&len.to_be_bytes())
                .map_err(|e| Error::Socket(format!("write failed: {}", e)))?;

            // Send request
            stream
                .write_all(&request_bytes)
                .map_err(|e| Error::Socket(format!("write failed: {}", e)))?;

            // Read response length
            let mut len_bytes = [0u8; 4];
            stream
                .read_exact(&mut len_bytes)
                .map_err(|e| Error::Socket(format!("read failed: {}", e)))?;
            let len = u32::from_be_bytes(len_bytes) as usize;

            // Read response
            let mut response_bytes = vec![0u8; len];
            stream
                .read_exact(&mut response_bytes)
                .map_err(|e| Error::Socket(format!("read failed: {}", e)))?;

            // Deserialize response
            serde_json::from_slice(&response_bytes).map_err(|e| Error::Serialization(e.to_string()))
        }

        #[cfg(not(unix))]
        {
            Err(Error::Socket(
                "not implemented for this platform".to_string(),
            ))
        }
    }
}

/// Type alias for the default daemon client.
pub type DaemonClient = SocketClient;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_in_process_client_health() {
        let client = InProcessClient::new();
        let health = client.health().unwrap();

        assert_eq!(health.project_count, 0);
        assert_eq!(health.queue_depth, 0);
    }

    /// Constructing a client is pure addressing; **reachability** is the
    /// question that needs a real answer, and the two must not be conflated
    /// (audit §K2/§K4). Binds in a `TempDir`, never a hardcoded `/tmp` path.
    #[cfg(unix)]
    #[test]
    fn is_reachable_answers_from_the_socket_not_from_the_constructor() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");

        // Nothing bound yet: addressable, not reachable.
        assert!(!SocketClient::new(&path).is_reachable());

        let listener = UnixListener::bind(&path).unwrap();
        assert!(
            SocketClient::new(&path).is_reachable(),
            "a bound socket is reachable"
        );

        drop(listener);
        std::fs::remove_file(&path).unwrap();
        assert!(
            !SocketClient::new(&path).is_reachable(),
            "a torn-down socket is not reachable"
        );
    }
}
