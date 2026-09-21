//! filigrio-protocol — the daemon contract and wire transport (ADR-0032f §2).
//!
//! This crate defines the single §3 contract used by all daemons and clients,
//! split into data and control planes, plus the socket framing layer.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────┐
//! │     wire contract (Request/Response) │
//! └─────────────────────────────────────┘
//!            │              │
//!    ┌───────┴──────┐  ┌────┴─────────┐
//!    │ Data Plane   │  │ Control Plane│
//!    │(queries)     │  │(commands)    │
//!    └──────────────┘  └──────────────┘
//!                          │
//!                 Socket framing (length-prefixed)
//! ```

#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

pub mod client;
pub mod contract;
pub mod query_types;
pub mod socket;

pub use client::{DaemonClient, DaemonClientTrait, InProcessClient, SocketClient};
pub use contract::{
    ChangeSet, CommandOutcome, DataQuery, HealthStatus, Priority, ProjectStatus, Request, Response,
};
#[cfg(feature = "control")]
pub use contract::{Command, ControlOp, MetaQuery};
pub use query_types::{Direction, NodeAddress, QueryParams, TraversalMode};
pub use socket::{connect, default_socket_path, frame, SocketServer, MAX_FRAME};

/// Error type for protocol operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("socket error: {0}")]
    Socket(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
