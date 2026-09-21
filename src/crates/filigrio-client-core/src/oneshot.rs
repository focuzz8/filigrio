//! One-shot subprocess spawning (ADR-0032f §4)
//!
//! Handles spawning ephemeral daemon processes for CI/hermetic usage,
//! including socket lifecycle management and exit code propagation.
//!
//! **Readiness, not a connection**: the one-shot responder accepts **exactly
//! one** connection and answers exactly one request (`cmd_one_shot` in the
//! daemon binary), so the client gets one connection and it must be the one
//! carrying the request. Readiness is therefore waited for by watching for the
//! socket *file*, never by connecting — see [`wait_for_socket`].
//!
//! The wait strategy: start with 10ms delay, double each retry up to max 1s, total timeout 5s.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::process::Child;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Configuration for one-shot daemon spawning
#[derive(Clone, Debug)]
pub struct OneShotConfig {
    /// Path to the daemon executable
    pub daemon_exe: PathBuf,
    /// Working directory for daemon execution
    pub working_dir: Option<PathBuf>,
    /// Environment variables to set
    pub env_vars: Vec<(String, String)>,
    /// Timeout for the one-shot daemon *process* to complete. Must cover a
    /// whole synchronous command, not just a query: the one-shot path runs
    /// control commands (`filigrio project index --no-daemon`), and a cold
    /// index at monorepo scale is measured in minutes (ADR-0042 F6c). Kept in step with
    /// [`filigrio_protocol::client::COMMAND_TIMEOUT`], the client-side read
    /// bound — a process budget shorter than the read budget would kill the
    /// work the client is still patiently waiting for.
    pub execution_timeout: Duration,
    /// Whether to forward daemon stdout/stderr to parent process
    pub forward_io: bool,
}

impl Default for OneShotConfig {
    fn default() -> Self {
        Self {
            daemon_exe: crate::default_daemon_exe(),
            working_dir: None,
            env_vars: Vec::new(),
            execution_timeout: filigrio_protocol::client::COMMAND_TIMEOUT, // ~10 min (ADR-0042 F6c)
            forward_io: true,
        }
    }
}

/// Wait until the one-shot daemon's socket exists, then hand back a client
/// addressed at it. Exponential backoff (10ms → 1s, total 5s).
///
/// **This must not connect.** The one-shot responder binds, accepts *one*
/// connection, reads *one* request and exits (`cmd_one_shot`, ADR-0032f §4), so
/// the connection `DaemonClient::send` makes is the only one there is. A
/// connect-and-drop liveness probe — legitimate against the resident daemon,
/// which accepts forever (ADR-0032f §6) — spends that accept on an empty
/// connection: the responder reads EOF, exits, and the request that follows is
/// reset by peer.
///
/// Waiting on the *file* is a sound readiness signal because `bind(2)` is what
/// creates it: the inode existing means the daemon reached `UnixListener::bind`.
/// The residual window is between `bind` and `listen` inside that one call — a
/// poll landing there makes `send`'s connect fail loudly with `ECONNREFUSED`,
/// never hang or truncate.
async fn wait_for_socket(socket_path: &Path) -> Result<filigrio_protocol::DaemonClient> {
    let start_time = std::time::Instant::now();
    let max_total_duration = Duration::from_secs(5); // Total timeout (same as daemon handshake)
    let mut delay = Duration::from_millis(10); // Start with 10ms (faster than daemon's 50ms)
    let max_delay = Duration::from_secs(1); // Max delay per attempt

    while !socket_path.exists() {
        if start_time.elapsed() > max_total_duration {
            return Err(anyhow::anyhow!(
                "Cannot connect to one-shot daemon after {} seconds: socket not ready at {}",
                max_total_duration.as_secs(),
                socket_path.display()
            ).context("The daemon may have failed to start or exited immediately. Try with --verbose to see daemon logs."));
        }

        debug!(
            "Socket file doesn't exist yet, waiting {:?} before retry",
            delay
        );
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(max_delay);
    }

    debug!("One-shot daemon socket is bound; the request connection will be its one accept");
    Ok(filigrio_protocol::DaemonClient::new(socket_path))
}

/// One-shot daemon controller handles subprocess lifecycle
pub struct OneShotRunner {
    config: OneShotConfig,
    /// RAII guard, never read: the socket lives inside this `TempDir`, and its
    /// **drop** is the point — dropping it removes the directory the socket
    /// path names. Deleting the field would delete the socket out from under a
    /// running daemon, so it is `_`-prefixed rather than removed.
    _socket_dir: TempDir,
    socket_path: PathBuf,
    child: Arc<Mutex<Option<Child>>>,
}

impl OneShotRunner {
    /// Create new one-shot runner with random temporary socket path
    pub fn new(config: OneShotConfig) -> Result<Self> {
        let socket_dir = tempfile::tempdir().context("failed to create temp socket directory")?;
        let socket_path = socket_dir
            .path()
            .join(format!("filigrio-daemon-{}.sock", std::process::id()));

        info!("One-shot daemon socket path: {:?}", socket_path.display());

        Ok(Self {
            config,
            _socket_dir: socket_dir,
            socket_path,
            child: Arc::new(Mutex::new(None)),
        })
    }

    /// Get the socket path for this one-shot daemon
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// A client for this one-shot daemon, once its socket is bound.
    ///
    /// Waits (backoff 10ms → 1s, total 5s) and returns an *addressed* client,
    /// exactly like [`filigrio_protocol::DaemonClient::new`] — it opens no
    /// connection of its own, and must not: [`wait_for_socket`] says why.
    pub async fn client_when_ready(&self) -> Result<filigrio_protocol::DaemonClient> {
        wait_for_socket(&self.socket_path).await
    }

    /// Start the one-shot daemon subprocess
    pub async fn start(&self) -> Result<()> {
        let mut cmd = self.build_command()?;

        debug!("Spawning one-shot daemon: {:?}", cmd);

        let child = cmd.spawn().context("failed to spawn one-shot daemon")?;

        let pid = child.id();
        debug!("One-shot daemon spawned with PID: {:?}", pid);

        let mut lock = self.child.lock().await;
        *lock = Some(child);

        Ok(())
    }

    /// Wait for daemon completion and return exit code
    ///
    /// This forwards the daemon's exit status to the caller.
    /// Returns exit code if process exited, error if wait failed.
    pub async fn wait_for_completion(&self) -> Result<i32> {
        let mut lock = self.child.lock().await;

        if let Some(mut child) = lock.take() {
            let exit_status = child
                .wait()
                .await
                .context("failed to wait for one-shot daemon")?;

            let exit_code = exit_status
                .code()
                .unwrap_or(if exit_status.success() { 0 } else { 1 });

            info!("One-shot daemon exited with code: {}", exit_code);

            // Clean up socket
            self.cleanup_socket().await;

            Ok(exit_code)
        } else {
            Err(anyhow::anyhow!("No daemon process running"))
        }
    }

    /// Wait for completion with timeout
    pub async fn wait_for_completion_with_timeout(&self) -> Result<i32> {
        let timeout = tokio::time::sleep(self.config.execution_timeout);
        let wait_completion = self.wait_for_completion();

        tokio::select! {
            result = wait_completion => result,
            _ = timeout => {
                warn!("One-shot daemon execution timeout exceeded, killing process");
                self.kill().await?;
                Err(anyhow::anyhow!("One-shot daemon execution timeout"))
            }
        }
    }

    /// Kill the daemon process if still running
    pub async fn kill(&self) -> Result<()> {
        let mut lock = self.child.lock().await;

        if let Some(mut child) = lock.take() {
            debug!("Killing one-shot daemon PID: {:?}", child.id());

            child
                .kill()
                .await
                .context("failed to kill one-shot daemon")?;

            // Wait for process to actually exit
            let _ = child.wait().await;

            self.cleanup_socket().await;
        }

        Ok(())
    }

    /// Build the command to spawn the daemon subprocess
    fn build_command(&self) -> Result<tokio::process::Command> {
        let mut cmd = tokio::process::Command::new(&self.config.daemon_exe);

        // Set working directory if specified
        if let Some(ref cwd) = self.config.working_dir {
            cmd.current_dir(cwd);
        }

        // Set environment variables
        for (key, value) in &self.config.env_vars {
            cmd.env(key, value);
        }

        // Pass socket path to daemon with one-shot subcommand
        cmd.arg("--socket")
            .arg(&self.socket_path)
            .arg("one-shot")
            .arg("--verbose"); // Enable verbose logging for debugging

        // I/O handling: forward stderr for debugging, null stdin/stdout
        if self.config.forward_io {
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit());
        } else {
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped());
        }

        Ok(cmd)
    }

    /// Clean up the socket file
    async fn cleanup_socket(&self) {
        if self.socket_path.exists() {
            debug!("Cleaning up socket: {:?}", self.socket_path);
            if let Err(e) = tokio::fs::remove_file(&self.socket_path).await {
                warn!("Failed to clean up socket: {:?}", e);
            }
        }
    }
}

impl Drop for OneShotRunner {
    fn drop(&mut self) {
        debug!("OneShotRunner dropped, ensuring cleanup");
        // Socket is automatically cleaned up by TempDir
    }
}

/// Run a one-shot daemon to completion, returning exit code
///
/// This is a convenience function that handles the full lifecycle:
/// spawn → wait → cleanup → return exit code
pub async fn run_one_shot(config: OneShotConfig) -> Result<i32> {
    let runner = OneShotRunner::new(config)?;
    let _socket_path = runner.socket_path().to_path_buf();

    runner.start().await?;

    // Set up signal handling for graceful shutdown.
    //
    // Registration happens here, not inside the futures: `signal()` fails when
    // the handler cannot be installed at all (ADR-0041's "signal-setup"
    // category), which is a startup failure the caller must see as an error
    // rather than a panic from inside a `select!` arm.
    let mut sigterm = signal(SignalKind::terminate())
        .context("failed to register a SIGTERM handler for the one-shot daemon")?;
    let mut sigint = signal(SignalKind::interrupt())
        .context("failed to register a SIGINT handler for the one-shot daemon")?;

    let runner_arc = Arc::new(runner);
    let runner_clone = runner_arc.clone();

    let sigterm_fut = async {
        sigterm.recv().await;
        warn!("SIGTERM received, killing one-shot daemon");
        let _ = runner_clone.kill().await;
    };

    let sigint_fut = async {
        sigint.recv().await;
        warn!("SIGINT received, killing one-shot daemon");
        let _ = runner_clone.kill().await;
    };

    let run_fut = runner_arc.wait_for_completion_with_timeout();

    tokio::select! {
        result = run_fut => result,
        _ = sigterm_fut => Err(anyhow::anyhow!("One-shot daemon killed by SIGTERM")),
        _ = sigint_fut => Err(anyhow::anyhow!("One-shot daemon killed by SIGINT")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_one_shot_config_default() {
        let config = OneShotConfig::default();
        assert_eq!(config.execution_timeout, Duration::from_secs(600));
        assert!(config.forward_io);
    }

    #[test]
    fn test_one_shot_runner_creation() {
        let config = OneShotConfig::default();
        let runner = OneShotRunner::new(config).unwrap();

        // Socket path should be a temp file path (not necessarily ending with filigrio-daemon-)
        assert!(
            runner.socket_path().starts_with("/tmp/")
                || runner.socket_path().starts_with("/var/tmp/")
        );
        assert!(runner.socket_path().extension().unwrap_or_default() == "sock");
    }

    #[tokio::test]
    async fn test_one_shot_runner_lifecycle() {
        let config = OneShotConfig {
            daemon_exe: PathBuf::from("sleep"), // Use sleep as a fake daemon
            execution_timeout: Duration::from_secs(1),
            forward_io: false,
            ..Default::default()
        };

        let runner = OneShotRunner::new(config).unwrap();

        // Start the "daemon"
        let start_result = runner.start().await;
        assert!(start_result.is_ok());

        // Give it a moment to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Kill it
        let kill_result = runner.kill().await;
        assert!(kill_result.is_ok());
    }

    #[tokio::test]
    async fn test_socket_cleanup() {
        let config = OneShotConfig {
            daemon_exe: PathBuf::from("true"), // Use true as a fake daemon
            forward_io: false,
            ..Default::default()
        };

        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("test.sock");
        let socket_path_clone = socket_path.clone();

        // Create a dummy socket file
        tokio::fs::write(&socket_path, "dummy").await.unwrap();
        assert!(socket_path.exists());

        let runner = OneShotRunner {
            config,
            _socket_dir: socket_dir,
            socket_path: socket_path_clone,
            child: Arc::new(Mutex::new(None)),
        };

        runner.cleanup_socket().await;

        // Socket should be removed
        assert!(!socket_path.exists());
    }

    /// Defends the whole of `--no-daemon` (ADR-0032f §4): a readiness probe that
    /// connects spends the responder's **one** accept, and the request that
    /// follows is answered by nobody.
    ///
    /// The stand-in server is deliberately shaped like `cmd_one_shot` in the
    /// daemon binary — bind, accept once, read one framed request, write one
    /// framed response, stop. Against that server this test fails if
    /// [`OneShotRunner::client_when_ready`] ever opens a connection of its own:
    /// the accept is gone and `send` has nobody to talk to.
    ///
    /// Multi-threaded runtime on purpose: `DaemonClient::send` is blocking
    /// (`std::os::unix::net`), so it runs on `spawn_blocking` while the
    /// responder task makes progress on another thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn readiness_wait_leaves_the_single_accept_for_the_request() {
        use filigrio_protocol::{DaemonClientTrait, Request, Response};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("one-shot.sock");

        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let responder = tokio::spawn(async move {
            let (mut stream, _addr) = listener.accept().await.unwrap();
            drop(listener); // one accept, like the real responder

            let mut len = [0u8; 4];
            stream.read_exact(&mut len).await.unwrap();
            let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
            stream.read_exact(&mut body).await.unwrap();
            let request: Request = serde_json::from_slice(&body).unwrap();

            let reply = Response::QueryResult {
                data: serde_json::json!({ "nodes": 3 }),
            };
            stream
                .write_all(&filigrio_protocol::frame(&reply).unwrap())
                .await
                .unwrap();
            request
        });

        let runner = OneShotRunner {
            config: OneShotConfig::default(),
            _socket_dir: socket_dir,
            socket_path: socket_path.clone(),
            child: Arc::new(Mutex::new(None)),
        };

        let client = runner.client_when_ready().await.unwrap();
        let request = Request::data(filigrio_protocol::DataQuery::GraphStats {
            project: "p".to_string(),
        });
        let response = tokio::task::spawn_blocking(move || client.send(request))
            .await
            .unwrap()
            .expect("the request connection must be the accept the responder is waiting for");

        match response {
            Response::QueryResult { data } => assert_eq!(data["nodes"], 3),
            other => panic!("expected the responder's body back, got {other:?}"),
        }

        // And the responder saw the real request, not an empty probe connection.
        match responder.await.unwrap() {
            Request::Data(filigrio_protocol::DataQuery::GraphStats { project }) => {
                assert_eq!(project, "p")
            }
            other => panic!("responder received the wrong request: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_execution_timeout() {
        let config = OneShotConfig {
            daemon_exe: PathBuf::from("sleep"), // Sleep forever
            execution_timeout: Duration::from_millis(500),
            forward_io: false,
            ..Default::default()
        };

        let runner = OneShotRunner::new(config).unwrap();
        runner.start().await.unwrap();

        // Wait with timeout - should timeout and kill the process
        let result = runner.wait_for_completion_with_timeout().await;
        // Result should be successful (timeout handled gracefully)
        assert!(result.is_ok());

        // Process should be killed after timeout
        let lock = runner.child.lock().await;
        assert!(lock.is_none()); // Process was taken and killed
    }
}
