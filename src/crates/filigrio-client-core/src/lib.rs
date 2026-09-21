//! filigrio-client-core — shared client logic for all daemons (ADR-0032f §1).
//!
//! This crate contains shared functionality used by the `filigrio` CLI
//! (`filigrio-client-cli`) and the `filigrio-mcp` bridge
//! (`filigrio-client-mcp`), including:
//! - Connection establishment (connect, auto-start handshake)
//! - cwd→project resolution
//! - One-shot subprocess spawning
//! - Response decoding utilities
//!
//! Both clients are engine-free and share this core logic.

// ADR-0041's gate, held rather than narrated: `deny`, not `warn`. The two
// remaining violations (signal registration in `oneshot::run_one_shot`) are
// fixed, so nothing here is grandfathered in.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod autostart;
pub mod oneshot;

use std::path::PathBuf;

pub use autostart::{AutoStartConfig, AutoStartHandshake, FlockGuard};
pub use oneshot::{run_one_shot, OneShotConfig, OneShotRunner};

// `default_socket_path` moved to `filigrio_protocol::socket` on 2026-07-28
// (audit §F2) — the default belongs beside the transport, so the daemon can
// learn where its own socket goes without depending on the client library.
// Not re-exported here: one name, one home.

/// Default executable for spawning the daemon — a **bare program name**, so the
/// OS resolves it against `PATH` at `exec` time. That is deliberate, not a
/// stub: a `which`-style lookup here would only duplicate the same `PATH`
/// search earlier, and would have to invent an answer (or fail) for the case
/// the spawn already reports honestly — the daemon not being installed. A
/// caller that wants a specific binary sets `daemon_exe` on its
/// [`AutoStartConfig`]/[`OneShotConfig`] instead.
pub fn default_daemon_exe() -> PathBuf {
    PathBuf::from("filigrio-daemon")
}

/// Resolve the current working directory to a project path (ADR-0032f §6).
///
/// This intentionally sends the **raw** cwd, not a pre-resolved project id —
/// registry matching (exact id, else nearest ancestor `root` via
/// `ProjectRegistry::find_by_path`) already happens server-side, once, in
/// `RegistryWarmStateSource`'s resolver. Re-matching against the registry
/// here too would be a redundant round trip that can only agree with the
/// server's answer or go stale against it, never improve on it.
///
/// What's still genuinely unresolved: a client run from an unregistered
/// project directory (no daemon round trip to ask). Finding *that* root by
/// walking up for a VCS/manifest marker is ADR-0032f §6's "unindexed folder
/// honesty" staging step (8), not this function's job — until then, an
/// unregistered cwd is sent as-is and the server reports it unmatched.
pub fn resolve_project_path() -> std::io::Result<PathBuf> {
    std::env::current_dir()
}

/// Error type for client core operations.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("socket error: {0}")]
    Socket(String),

    #[error("project resolution error: {0}")]
    ProjectResolution(String),
}

pub type Result<T> = std::result::Result<T, ClientError>;

// This file has no unit tests, deliberately (audit §K2d): the only behaviour in
// it is `default_daemon_exe`'s literal, and `resolve_project_path` *is*
// `std::env::current_dir()` with no added logic. What the raw cwd resolves
// against is pinned server-side — `filigrio-daemon/src/project.rs`'s
// `test_find_by_path` and `filigrio-daemon/tests/integration.rs`. The
// `socket_path_under` arm tests moved with their function to
// `filigrio-protocol/src/socket.rs` (audit §F2).
