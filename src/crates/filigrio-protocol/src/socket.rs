//! Socket transport for daemon communication (ADR-0032f §2).

use crate::contract::Request;
use crate::Error;
use crate::Result;
use serde::Serialize;
use serde_json;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use tracing::{debug, error, info};

/// Maximum framed message size (10 MB). A length prefix larger than this is
/// rejected before allocating, so a corrupt or hostile prefix can't OOM the daemon.
pub const MAX_FRAME: usize = 10 * 1024 * 1024;

/// Encode a message as a length-prefixed frame: `[u32 BE len][json]`.
///
/// This is the **one** wire-format encoder — the blocking socket server and the
/// async `Daemon::run` accept loop both call it, so the two transports cannot
/// drift on framing. Decoding is a bare `serde_json::from_slice` once the body
/// bytes are read (the read differs sync vs async; the format does not).
pub fn frame<T: Serialize>(msg: &T) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(msg)
        .map_err(|e| Error::Serialization(format!("serialize failed: {e}")))?;
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(&body);
    Ok(buf)
}

/// Socket server for handling client connections.
pub struct SocketServer {
    listener: UnixListener,
}

impl SocketServer {
    /// Bind to a socket path.
    pub fn bind(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        // Remove existing socket if present
        if path.exists() {
            std::fs::remove_file(path)
                .map_err(|e| Error::Socket(format!("failed to remove existing socket: {}", e)))?;
        }

        let listener =
            UnixListener::bind(path).map_err(|e| Error::Socket(format!("bind failed: {}", e)))?;

        info!("Socket server listening on {}", path.display());

        Ok(SocketServer { listener })
    }

    /// Accept incoming connections forever, one request per connection.
    pub fn run<F>(&self, handler: F) -> Result<()>
    where
        F: FnMut(Request) -> Result<crate::contract::Response>,
    {
        self.run_bounded(None, handler)
    }

    /// Accept connections, handling at most `max` of them before returning
    /// (`None` = forever). The bounded form exists so tests can drive a real
    /// socket round-trip and have the accept loop terminate deterministically
    /// instead of leaking a thread blocked on `accept()`. One request per
    /// connection either way (connection-per-request, matching `SocketClient`).
    pub fn run_bounded<F>(&self, max: Option<usize>, mut handler: F) -> Result<()>
    where
        F: FnMut(Request) -> Result<crate::contract::Response>,
    {
        let mut served = 0usize;
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    if let Err(e) = Self::handle_connection(stream, &mut handler) {
                        error!("Connection error: {}", e);
                    }
                }
                Err(e) => {
                    error!("Accept error: {}", e);
                }
            }
            served += 1;
            if let Some(m) = max {
                if served >= m {
                    break;
                }
            }
        }

        Ok(())
    }

    /// Handle a single client connection.
    fn handle_connection<F>(mut stream: UnixStream, handler: &mut F) -> Result<()>
    where
        F: FnMut(Request) -> Result<crate::contract::Response>,
    {
        // Set timeouts
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .map_err(|e| Error::Socket(format!("set_read_timeout failed: {}", e)))?;
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(30)))
            .map_err(|e| Error::Socket(format!("set_write_timeout failed: {}", e)))?;

        // Read request length
        let mut len_bytes = [0u8; 4];
        stream
            .read_exact(&mut len_bytes)
            .map_err(|e| Error::Socket(format!("read length failed: {}", e)))?;
        let len = u32::from_be_bytes(len_bytes) as usize;

        // Validate length
        if len > MAX_FRAME {
            return Err(Error::Socket(format!("request too large: {} bytes", len)));
        }

        // Read request
        let mut request_bytes = vec![0u8; len];
        stream
            .read_exact(&mut request_bytes)
            .map_err(|e| Error::Socket(format!("read request failed: {}", e)))?;

        // Deserialize request
        let request: Request = serde_json::from_slice(&request_bytes)
            .map_err(|e| Error::Serialization(format!("deserialize failed: {}", e)))?;

        debug!("Received request: {:?}", request);

        // Handle request; a handler error is itself a valid (framed) response, so a
        // failed request never desyncs the length-prefixed stream.
        let response = handler(request).unwrap_or_else(|e| crate::contract::Response::Error {
            message: e.to_string(),
        });

        // Serialize + send via the shared framing encoder.
        let response_bytes = frame(&response)?;
        stream
            .write_all(&response_bytes)
            .map_err(|e| Error::Socket(format!("write response failed: {}", e)))?;

        debug!("Sent response: {} bytes", response_bytes.len());

        Ok(())
    }
}

/// Connect to a daemon socket.
pub fn connect(path: impl AsRef<Path>) -> Result<UnixStream> {
    let path = path.as_ref();
    let stream =
        UnixStream::connect(path).map_err(|e| Error::Socket(format!("connect failed: {}", e)))?;

    // Set timeouts
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .map_err(|e| Error::Socket(format!("set_read_timeout failed: {}", e)))?;
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(30)))
        .map_err(|e| Error::Socket(format!("set_write_timeout failed: {}", e)))?;

    Ok(stream)
}

/// Where the daemon binds and every client connects by default:
/// `$XDG_RUNTIME_DIR/filigrio-daemon.sock`, else `/tmp/filigrio-daemon.sock`.
///
/// Lives beside the transport, which both sides already depend on (audit §F2).
/// It was in `filigrio-client-core` until 2026-07-28, which made the *server*
/// depend on the client library to learn where its own socket goes.
pub fn default_socket_path() -> PathBuf {
    socket_path_under(std::env::var("XDG_RUNTIME_DIR").ok())
}

/// The `XDG_RUNTIME_DIR` branch, lifted out of the env read so both arms are
/// testable without `set_var` — which is process-global and would race every
/// other test in the binary.
///
/// An **empty** `XDG_RUNTIME_DIR` is the unset case, not a request to bind at
/// `/filigrio-daemon.sock`: `PathBuf::from("").join(..)` yields a root-relative
/// path, so treating empty as set would put the socket somewhere unwritable.
fn socket_path_under(runtime_dir: Option<String>) -> PathBuf {
    match runtime_dir {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("filigrio-daemon.sock"),
        _ => PathBuf::from("/tmp/filigrio-daemon.sock"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The set arm: the socket lives *under* `XDG_RUNTIME_DIR`, not beside it.
    ///
    /// This replaces `test_default_socket_path`, which asserted
    /// `default_socket_path().ends_with("filigrio-daemon.sock")` — a restatement
    /// of the string literal in the function, true on either arm, and therefore
    /// blind to the only branch there is (audit §K2c).
    #[test]
    fn xdg_runtime_dir_is_where_the_socket_goes_when_it_is_set() {
        assert_eq!(
            socket_path_under(Some("/run/user/1000".to_string())),
            PathBuf::from("/run/user/1000/filigrio-daemon.sock")
        );
    }

    /// The unset arm — and the empty-string case, which is unset in disguise.
    #[test]
    fn socket_path_falls_back_to_tmp_when_xdg_is_unset_or_empty() {
        let fallback = PathBuf::from("/tmp/filigrio-daemon.sock");
        assert_eq!(socket_path_under(None), fallback);
        assert_eq!(
            socket_path_under(Some(String::new())),
            fallback,
            "an empty XDG_RUNTIME_DIR must not be honored — joining onto \"\" \
             yields the root-level /filigrio-daemon.sock"
        );
    }

    /// Binds in a `TempDir`, not at a fixed `/tmp` path (audit §K4): two
    /// concurrent runs of this suite — two developers on one box, CI with
    /// per-branch parallelism — collided on the shared name, and the
    /// `remove_file` bracketing meant the loser could delete the winner's live
    /// socket. The temp dir also makes the cleanup the destructor's job.
    #[test]
    fn test_socket_server_bind() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("filigrio-daemon.sock");

        let server = SocketServer::bind(&socket_path);
        assert!(server.is_ok(), "bind failed: {:?}", server.err());
        assert!(
            socket_path.exists(),
            "bind must leave a socket at the requested path"
        );
    }
}
