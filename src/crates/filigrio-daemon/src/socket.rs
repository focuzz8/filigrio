//! Async framing helpers over the §3 wire format (ADR-0032f §2).
//!
//! The transport itself lives in `filigrio-protocol` — `SocketServer` (the
//! blocking accept loop), `frame` (the one `[u32 BE len][json]` encoder), and
//! `MAX_FRAME`. This module holds only the **async** side of the same frame,
//! shared by the daemon's accept loop (`handle_conn`) and the one-shot binary,
//! so the async path can't drift from the blocking server's wire format.

use crate::{Error, Request, Result};
use filigrio_protocol::{frame, MAX_FRAME};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Read one framed [`Request`]: length prefix, bounds check against
/// [`MAX_FRAME`], then the body (`read_exact` loops until the whole frame is
/// in — safe across chunking).
pub async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Request> {
    let mut len_bytes = [0u8; 4];
    stream
        .read_exact(&mut len_bytes)
        .await
        .map_err(|e| Error::Socket(format!("read length failed: {e}")))?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > MAX_FRAME {
        return Err(Error::Socket(format!("request too large: {len} bytes")));
    }

    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .await
        .map_err(|e| Error::Socket(format!("read request failed: {e}")))?;
    serde_json::from_slice(&body)
        .map_err(|e| Error::Serialization(format!("deserialize failed: {e}")))
}

/// Write one framed message (via the shared `frame` encoder) and flush.
pub async fn write_frame<S, T>(stream: &mut S, msg: &T) -> Result<()>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    let out = frame(msg)?;
    stream
        .write_all(&out)
        .await
        .map_err(|e| Error::Socket(format!("write response failed: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| Error::Socket(format!("flush failed: {e}")))
}
