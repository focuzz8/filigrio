//! One typed error threaded through every port (`thiserror`). Binaries and glue
//! code convert these into `anyhow::Error` at their boundary.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// I/O at a `Source`/`GraphStore` boundary.
    #[error("io: {0}")]
    Io(String),
    /// An `Extractor` failed to parse an artifact.
    #[error("parse: {0}")]
    Parse(String),
    /// A `GraphStore` failed to load/persist state.
    #[error("storage: {0}")]
    Storage(String),
    /// A lookup missed (unknown node/community/path).
    #[error("not found: {0}")]
    NotFound(String),
    /// A transport (MCP/HTTP) framing/dispatch error.
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("{0}")]
    Other(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Parse(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
