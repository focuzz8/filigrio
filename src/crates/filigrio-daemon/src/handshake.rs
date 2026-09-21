//! Auto-start handshake with flock-based race guard (ADR-0032f §6).
//!
//! This module implements the daemon auto-start handshake sequence:
//! 1. Connect to the well-known socket
//! 2. On failure, take an exclusive lock (flock)
//! 3. Re-connect (a peer may have bound while we waited)
//! 4. Else become the daemon and bind
//!
//! This ensures a single-daemon guarantee without thundering-herd race conditions.

use crate::{Error, Result};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Lock file path for daemon auto-start synchronization.
const LOCK_FILE_SUFFIX: &str = ".lock";

/// Result of the auto-start handshake attempt.
#[derive(Debug, PartialEq)]
pub enum HandshakeResult {
    /// Successfully connected to existing daemon
    Connected,
    /// Became the daemon (acquired exclusive lock)
    BecameDaemon,
    /// Retry needed (race condition detected)
    Retry,
}

/// Whether something is listening on the daemon socket.
///
/// A successful connect, no more: it does not verify the listener is a
/// filigrio daemon, nor that it is responsive — a wedged daemon still reads
/// as running. That is the right check for all callers, which use it to
/// decide whether to spawn *another* daemon.
pub fn is_daemon_running(socket_path: &Path) -> bool {
    filigrio_protocol::SocketClient::new(socket_path).is_reachable()
}

/// Perform the auto-start handshake sequence.
///
/// This implements ADR-0032f §6 handshake:
/// 1. Try to connect to existing daemon
/// 2. If fails, attempt to acquire exclusive lock
/// 3. Re-connect to check if another daemon won the race
/// 4. If still no daemon, become the daemon
///
/// Returns the result of the handshake along with any lock file that was acquired.
pub fn perform_handshake(socket_path: &Path) -> Result<(HandshakeResult, Option<File>)> {
    // Step 1: Try to connect to existing daemon
    if is_daemon_running(socket_path) {
        return Ok((HandshakeResult::Connected, None));
    }

    // Step 2: No daemon running - attempt to acquire exclusive lock
    let lock_file_path = socket_path.with_extension(LOCK_FILE_SUFFIX.trim_start_matches('.'));
    let lock_file = acquire_lock(&lock_file_path)?;

    // Step 3: Re-connect to check if another daemon won the race
    if is_daemon_running(socket_path) {
        // Another daemon started while we were waiting for the lock
        drop(lock_file);
        return Ok((HandshakeResult::Retry, None));
    }

    // Step 4: We are the daemon - return the lock file for cleanup
    Ok((HandshakeResult::BecameDaemon, Some(lock_file)))
}

/// Acquire an exclusive lock on a file using flock.
///
/// Returns the locked file handle that should be held while the daemon is running
/// and dropped during cleanup.
fn acquire_lock(lock_path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);

    #[cfg(unix)]
    {
        options.mode(0o600); // rw-------
    }

    let file = options.open(lock_path).map_err(|e| {
        Error::Other(format!(
            "failed to open lock file {}: {}",
            lock_path.display(),
            e
        ))
    })?;

    // Try to acquire the exclusive lock (non-blocking). `fs2` is the same
    // `flock(LOCK_EX | LOCK_NB)` the client's `AutoStartHandshake` takes on its
    // own guard file (`filigrio-client-core/src/autostart.rs`) — one safe,
    // cross-platform wrapper for the pattern instead of a second hand-rolled
    // `unsafe` copy that only the daemon side knew about.
    file.try_lock_exclusive().map_err(|_| {
        Error::Other(format!(
            "failed to acquire lock on {}: another daemon may be starting",
            lock_path.display()
        ))
    })?;

    Ok(file)
}

/// Release the exclusive lock held by the daemon.
///
/// This should be called during daemon teardown to allow another process to start
/// a new daemon.
pub fn release_lock(lock_file: File, lock_path: &Path) -> Result<()> {
    // Release lock by closing the file descriptor
    drop(lock_file);

    // Remove the lock file (best-effort)
    let _ = std::fs::remove_file(lock_path);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_is_daemon_running_no_socket() {
        let temp = TempDir::new().unwrap();
        let socket_path = temp.path().join("test.sock");

        // Socket doesn't exist, daemon should not be running
        assert!(!is_daemon_running(&socket_path));
    }

    #[test]
    fn test_lock_acquire_release() {
        let temp = TempDir::new().unwrap();
        let lock_path = temp.path().join("test.lock");

        // Should be able to acquire lock
        let lock_file = acquire_lock(&lock_path).unwrap();

        // Should not be able to acquire another lock (non-blocking)
        let result = acquire_lock(&lock_path);
        assert!(result.is_err());

        // Release first lock
        release_lock(lock_file, &lock_path).unwrap();

        // Should now be able to acquire lock again
        let _lock_file = acquire_lock(&lock_path).unwrap();
    }

    #[test]
    fn test_handshake_become_daemon() {
        let temp = TempDir::new().unwrap();
        let socket_path = temp.path().join("test.sock");

        // No daemon running, should become daemon
        let (result, lock_file) = perform_handshake(&socket_path).unwrap();
        assert_eq!(result, HandshakeResult::BecameDaemon);
        assert!(lock_file.is_some());

        // Clean up
        if let Some(lock) = lock_file {
            let _ = release_lock(lock, &socket_path.with_extension("lock"));
        }
    }

    #[test]
    fn test_handshake_retry() {
        let temp = TempDir::new().unwrap();
        let socket_path = temp.path().join("test.sock");

        // First handshake - become daemon
        let (result, lock_file) = perform_handshake(&socket_path).unwrap();
        assert_eq!(result, HandshakeResult::BecameDaemon);

        // Simulate another daemon starting by creating a socket
        use std::os::unix::net::UnixListener;
        let _listener = UnixListener::bind(&socket_path).unwrap();

        // Drop the first lock (simulate losing the race)
        if let Some(lock) = lock_file {
            drop(lock);
        }

        // Second handshake - finds existing socket (UnixListener acts as daemon)
        let (result, lock_file) = perform_handshake(&socket_path).unwrap();
        assert_eq!(result, HandshakeResult::Connected);
        assert!(lock_file.is_none());

        // Clean up
        let _ = std::fs::remove_file(&socket_path);
    }
}
