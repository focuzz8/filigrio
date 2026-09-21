//! filigrio-daemon — the freshness daemon (ADR-0032).
//!
//! A single long-running daemon that hosts the incremental changeset engine
//! for all projects. Implements the commands/queries contract — wire commands
//! execute synchronously at ingress and the response carries the outcome
//! (ADR-0042 F6c) — and manages the **producer lane**: a priority queue of
//! watcher output, drained onto the worker pool, with two-stage dedup
//! (mtime → hash → parse) at apply time.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    Freshness Daemon                         │
//! │  (One process, holds engines, indices, watchers per project) │
//! └─────────────────────────────────────────────────────────────┘
//!                               │
//!                     Commands/Queries Contract
//!                               │
//!         ┌─────────────────────┼─────────────────────┐
//!         │                     │                     │
//!    CLI Commands      MCP Bridge (stdio)     In-Process Path
//!    (daemon clients)  (thin proxy)         (--no-daemon)
//! ```

#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]
mod apply;
pub mod cache;
mod daemon;
pub mod flush;
mod handshake;
pub mod locks;
mod priority;
mod project;
mod responder;
mod scheduler;
pub mod socket;
mod state_source;
mod watchers;

pub use cache::{Evicted, ProjectStateCache};
// Persistence cadence (ADR-0042 F4/B12): write-behind in the producer lane,
// flush-before-response for clients. `TestClock` is exported deliberately — the
// B12 timers are only testable without sleeping if a suite can inject a clock.
pub use filigrio_protocol::{
    frame, ChangeSet, Command, ControlOp, DataQuery, HealthStatus, MetaQuery, Priority,
    ProjectStatus, Request, Response, SocketServer, MAX_FRAME,
};
pub use flush::{Clock, FlushConfig, Flusher, Persistence, SystemClock, TestClock};
// The shrink-guard now lives with the freshness engine (`filigrio-pipeline`);
// re-exported here so daemon consumers/tests keep the same path.
pub use daemon::{default_worker_threads, Daemon, DaemonConfig, CLEAN_MARKER};
pub use filigrio_pipeline::check_shrink_guard;
pub use locks::ProjectLocks;
// The queue element is daemon-internal (ADR-0042 F6b): `(project, Op, lane)`,
// never the wire `Command` — since F6c only producer (watcher) output enters it.
pub use apply::Op;
pub use priority::{PriorityQueue, QueueItem};
pub use project::{Project, ProjectRegistry, RegistryError};
// The filesystem watcher is a change **producer** living in `filigrio-ingest`
// (ADR-0032e) — re-exported here so existing call sites (tests, the CLI) keep
// working without reaching into the ingest crate directly.
pub use filigrio_ingest::{FsWatcher, Produced, Producer, WatcherConfig};
// Responder layer (ADR-0032f §3) — the engine-service abstraction
pub use responder::{Responder, StateSource};
// One-shot write execution (ADR-0032f §4/§5) — the `--no-daemon` synchronous
// command path, sharing the resident daemon's own apply functions.
pub use apply::run_command_cold;
pub use state_source::{ColdStoreStateSource, RegistryWarmStateSource};
// Auto-start handshake (ADR-0032f §6) — flock-based single-daemon race guard
pub use handshake::{is_daemon_running, perform_handshake, HandshakeResult};

/// Error type for daemon operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("daemon not running: {0}")]
    NotRunning(String),

    #[error("socket error: {0}")]
    Socket(String),

    #[error("project not registered: {0}")]
    ProjectNotFound(String),

    /// Registered, but nothing has ever been indexed for it. A distinct
    /// condition from `ProjectNotFound` because it has a distinct fix, and
    /// because `FsStore` reads a missing checkpoint as an empty graph — without
    /// this the caller gets a confident zero instead of an instruction.
    #[error("project '{0}' is registered but has no index yet — run `filigrio project index` from its root")]
    ProjectNotIndexed(String),

    #[error("project already registered: {0}")]
    ProjectAlreadyExists(String),

    #[error("reconcile error: {0}")]
    Reconcile(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("core error: {0}")]
    Core(#[from] filigrio_core::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("other: {0}")]
    Other(String),
}

// Convert protocol errors to daemon errors
impl From<filigrio_protocol::Error> for Error {
    fn from(err: filigrio_protocol::Error) -> Self {
        match err {
            filigrio_protocol::Error::Socket(msg) => Error::Socket(msg),
            filigrio_protocol::Error::Serialization(msg) => Error::Serialization(msg),
            filigrio_protocol::Error::Io(io) => Error::Io(io),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
