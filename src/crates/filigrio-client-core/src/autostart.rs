//! Auto-start handshake implementation (ADR-0032f §6)
//!
//! Implements the race-guarded daemon startup pattern:
//! connect → flock → spawn → re-connect → bind
//!
//! This ensures only one daemon process exists while allowing
//! multiple concurrent clients to safely auto-start it.

use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Configuration for auto-start behavior
#[derive(Clone, Debug)]
pub struct AutoStartConfig {
    /// Path to the flock file for race guards
    pub flock_path: PathBuf,
    /// Path to the daemon executable
    pub daemon_exe: PathBuf,
    /// Socket path to check for daemon readiness
    pub socket_path: PathBuf,
    /// Health check interval
    pub health_check_interval: Duration,
    /// Total timeout for daemon startup
    pub startup_timeout: Duration,
}

impl Default for AutoStartConfig {
    fn default() -> Self {
        Self {
            flock_path: default_flock_path(),
            daemon_exe: crate::default_daemon_exe(),
            socket_path: filigrio_protocol::default_socket_path(),
            health_check_interval: Duration::from_millis(100),
            startup_timeout: Duration::from_secs(5),
        }
    }
}

/// Auto-start handshake controller
pub struct AutoStartHandshake {
    config: AutoStartConfig,
}

impl Default for AutoStartHandshake {
    fn default() -> Self {
        Self::new(AutoStartConfig::default())
    }
}

impl AutoStartHandshake {
    pub fn new(config: AutoStartConfig) -> Self {
        Self { config }
    }

    /// Perform full auto-start handshake: try connect, acquire lock if needed,
    /// spawn daemon, wait for readiness, release lock.
    ///
    /// Returns true if daemon is ready, false if failed after retries.
    pub async fn ensure_daemon_ready(&self) -> Result<bool> {
        // Phase 1: Try existing socket first
        if self.check_socket_ready().await {
            debug!("Daemon already running, skipping auto-start");
            return Ok(true);
        }

        // Phase 2: Acquire flock to guard startup race
        let _lock_guard = self
            .acquire_flock()
            .await
            .context("failed to acquire startup lock")?;

        // Double-check: another client might have started daemon while we waited
        if self.check_socket_ready().await {
            debug!("Daemon started by another client, proceeding");
            return Ok(true);
        }

        // Phase 3: Spawn daemon subprocess
        info!("Auto-starting daemon: {:?}", self.config.daemon_exe);
        self.spawn_daemon()
            .await
            .context("failed to spawn daemon")?;

        // Phase 4: Wait for daemon to become ready
        let ready = self
            .wait_for_daemon_ready()
            .await
            .context("daemon readiness check failed")?;

        if ready {
            info!("Daemon auto-start successful");
        } else {
            warn!("Daemon failed to become ready within timeout");
        }

        Ok(ready)
    }

    /// Phase 1: Check if socket exists and is connectable
    async fn check_socket_ready(&self) -> bool {
        if !self.config.socket_path.exists() {
            debug!("Socket path does not exist: {:?}", self.config.socket_path);
            return false;
        }

        tokio::net::UnixStream::connect(&self.config.socket_path)
            .await
            .map(|stream| {
                debug!("Socket connectable, daemon appears ready");
                drop(stream);
                true
            })
            .unwrap_or_else(|e| {
                debug!("Socket exists but not connectable: {}", e);
                false
            })
    }

    /// Phase 2: Acquire flock on lock file to guard startup race condition
    async fn acquire_flock(&self) -> Result<FlockGuard> {
        debug!("Acquiring flock on: {:?}", self.config.flock_path);

        // Create lock file if it doesn't exist
        if !self.config.flock_path.exists() {
            if let Some(parent) = self.config.flock_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::File::create(&self.config.flock_path).await?;
        }

        let flock_path = self.config.flock_path.clone();

        // Open file and acquire exclusive lock
        tokio::task::spawn_blocking(move || {
            let file = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_CREAT | libc::O_WRONLY)
                .open(&flock_path)
                .context("failed to open lock file")?;

            // Use blocking lock to wait for other processes to release
            file.lock_exclusive().context("failed to acquire lock")?;

            Ok::<_, anyhow::Error>(FlockGuard { _file: file })
        })
        .await?
    }

    /// Phase 3: Spawn daemon as subprocess
    async fn spawn_daemon(&self) -> Result<()> {
        // `start` is not optional: `filigrio-daemon` is a subcommand CLI, and
        // without it clap exits 2 with a usage error before the daemon ever
        // binds — so the readiness wait below could only ever time out. That
        // went unnoticed because no caller could reach this function at all
        // (the `connect` that gated it never failed).
        let mut cmd = tokio::process::Command::new(&self.config.daemon_exe);
        cmd.arg("--socket")
            .arg(&self.config.socket_path)
            .arg("start")
            .arg("--idle-timeout")
            .arg("0") // Never shut down for auto-started daemon
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());

        debug!("Spawning daemon: {:?}", self.config.daemon_exe);

        let child = cmd.spawn().context("failed to spawn daemon subprocess")?;

        debug!("Daemon spawned with PID: {:?}", child.id());

        // Detach so daemon continues running after handshake completes
        drop(child);

        Ok(())
    }

    /// Phase 4: Wait for daemon to become ready (socket connectable)
    async fn wait_for_daemon_ready(&self) -> Result<bool> {
        let start = std::time::Instant::now();
        let mut attempts = 0;

        while start.elapsed() < self.config.startup_timeout {
            attempts += 1;
            tokio::time::sleep(self.config.health_check_interval).await;

            if self.check_socket_ready().await {
                info!(
                    "Daemon became ready after {} attempts ({:?})",
                    attempts,
                    start.elapsed()
                );
                return Ok(true);
            }
        }

        warn!(
            "Daemon failed to become ready after {} attempts ({:?})",
            attempts,
            start.elapsed()
        );
        Ok(false)
    }
}

/// Guard that releases flock when dropped
pub struct FlockGuard {
    _file: File,
}

impl Drop for FlockGuard {
    fn drop(&mut self) {
        debug!("Releasing flock");
        // Use best-effort unlock; panics here should not bubble up
        if let Err(e) = self._file.unlock() {
            warn!("Failed to release flock: {:?}", e);
        }
    }
}

// Default paths follow XDG spec with fallbacks

pub fn default_flock_path() -> PathBuf {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        if !runtime.is_empty() {
            return PathBuf::from(runtime).join("filigrio.lock");
        }
    }
    PathBuf::from("/tmp/filigrio.lock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_default_paths() {
        let flock = default_flock_path();
        assert!(flock.ends_with("filigrio.lock"));

        let socket = filigrio_protocol::default_socket_path();
        assert!(socket.ends_with("filigrio-daemon.sock"));
    }

    #[test]
    fn test_auto_start_config_default() {
        let config = AutoStartConfig::default();
        assert_eq!(config.health_check_interval, Duration::from_millis(100));
        assert_eq!(config.startup_timeout, Duration::from_secs(5));
    }

    #[tokio::test]
    async fn test_socket_ready_check() {
        let temp = TempDir::new().unwrap();
        let socket_path = temp.path().join("test.sock");
        let handshake = AutoStartHandshake::new(AutoStartConfig {
            flock_path: temp.path().join("lock"),
            daemon_exe: PathBuf::from("fake-daemon"),
            socket_path: socket_path.clone(),
            ..Default::default()
        });

        // Socket doesn't exist
        assert!(!handshake.check_socket_ready().await);

        // Create socket (not really a Unix socket, just a file)
        tokio::fs::write(&socket_path, "dummy").await.unwrap();

        // File exists but not connectable
        assert!(!handshake.check_socket_ready().await);
    }

    #[tokio::test]
    async fn test_flock_guard_cleanup() {
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path().to_path_buf();

        // Create lock file
        let lock_path = temp_path.join("test.lock");
        File::create(&lock_path).unwrap();

        {
            let file = OpenOptions::new().write(true).open(&lock_path).unwrap();
            file.lock_exclusive().unwrap();
            let _lock = FlockGuard { _file: file };
            // Lock is held here
        }

        // Lock should be released (can acquire again)
        tokio::task::spawn_blocking(move || {
            let file = OpenOptions::new().write(true).open(&lock_path).unwrap();
            file.try_lock_exclusive()
                .expect("Can acquire lock after guard dropped");
        })
        .await
        .unwrap();
    }
}
