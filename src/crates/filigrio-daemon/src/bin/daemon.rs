//! filigrio-daemon — the freshness daemon binary (ADR-0032f §1).
//!
//! This is the long-running resident host that:
//! - Holds the engine, indices, and graph state (the only binary that does)
//! - Manages per-project watchers and LRU cache
//! - Serves the §3 contract over a Unix socket
//! - Supports auto-start, idle-shutdown, and lifecycle management

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use filigrio_daemon::{is_daemon_running, Daemon, DaemonConfig};
use filigrio_protocol::{default_socket_path, DaemonClientTrait};
use std::path::{Path, PathBuf};
use tracing::{info, warn};
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Parser)]
#[command(
    name = "filigrio-daemon",
    version,
    about = "filigrio daemon — the freshness daemon (ADR-0032)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Socket path for daemon communication.
    #[arg(long, global = true)]
    #[arg(default_value_os_t = default_socket_path())]
    socket: PathBuf,

    /// Verbose logging.
    #[arg(long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon (runs until interrupted or idle timeout).
    Start {
        /// Idle shutdown timeout in seconds (0 = never shut down).
        #[arg(long, default_value_t = 300)]
        idle_timeout: u64,

        /// Max number of full graphs held resident.
        #[arg(long, default_value_t = 8)]
        cache_capacity: usize,

        /// Max concurrent apply operations.
        #[arg(long)]
        #[arg(default_value_t = filigrio_daemon::default_worker_threads())]
        worker_threads: usize,

        /// Seconds a watched project may go without an apply before its
        /// resident state is written to the store (ADR-0042 F4/B12
        /// write-behind). Larger = fewer, bigger writes and a longer window in
        /// which `.filigrio-out` lags what the daemon serves.
        #[arg(long, default_value_t = 30)]
        flush_quiescence: u64,

        /// Hard cap in seconds on how long a project may hold unpersisted
        /// state, however busy it is (ADR-0042 F4/B12). This is the crash
        /// window: a kill loses at most this much re-indexable work.
        #[arg(long, default_value_t = 300)]
        flush_max_dirty_age: u64,
    },
    /// Stop the running daemon.
    Stop,
    /// Show daemon status.
    Status,
    /// Run as a one-shot responder (`--no-daemon` mode, ADR-0032f §4).
    ///
    /// This answers a single request without starting the long-running daemon,
    /// used for CI or when daemon mode is disabled.
    /// The daemon binds a socket and waits for a client to connect and send the request.
    OneShot,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    let filter = if cli.verbose {
        EnvFilter::from_env("FILIGRIO_LOG").add_directive("filigrio_daemon=debug".parse().unwrap())
    } else {
        EnvFilter::from_env("FILIGRIO_LOG").add_directive("filigrio_daemon=info".parse().unwrap())
    };

    fmt().with_env_filter(filter).with_target(false).init();

    match cli.command {
        Command::Start {
            idle_timeout,
            cache_capacity,
            worker_threads,
            flush_quiescence,
            flush_max_dirty_age,
        } => {
            cmd_start(
                cli.socket,
                idle_timeout,
                cache_capacity,
                worker_threads,
                filigrio_daemon::FlushConfig {
                    quiescence: std::time::Duration::from_secs(flush_quiescence),
                    max_dirty_age: std::time::Duration::from_secs(flush_max_dirty_age),
                },
            )
            .await
        }
        Command::Stop => cmd_stop(cli.socket).await,
        Command::Status => cmd_status(cli.socket).await,
        Command::OneShot => cmd_one_shot(cli.socket).await,
    }
}

async fn cmd_start(
    socket_path: PathBuf,
    idle_timeout_secs: u64,
    cache_capacity: usize,
    worker_threads: usize,
    flush: filigrio_daemon::FlushConfig,
) -> Result<()> {
    info!("Starting daemon on {}", socket_path.display());

    // Check if daemon is already running
    if is_daemon_running(&socket_path) {
        warn!("Daemon is already running on {}", socket_path.display());
        return Ok(());
    }

    let idle_timeout = if idle_timeout_secs > 0 {
        Some(std::time::Duration::from_secs(idle_timeout_secs))
    } else {
        None
    };

    let config = DaemonConfig {
        // The clean-shutdown marker belongs to *this* daemon instance, so it is
        // derived from the socket it was given — the same rule the handshake
        // lock already follows (`socket_path.with_extension("lock")`). Left at
        // the default it stayed the global `/tmp/filigrio-daemon.clean` however
        // `--socket` was set, so any second daemon (another user, a scratch
        // instance, a test) decided the *well-known* daemon's next startup
        // reconcile depth.
        shutdown_marker_path: socket_path.with_extension("clean"),
        socket_path: socket_path.clone(),
        idle_timeout,
        cache_capacity,
        worker_threads,
        flush,
        ..Default::default()
    };

    let mut daemon = Daemon::new(config);
    // The handle `DaemonStop` notifies (`request_graceful_shutdown`); the run
    // loop selects on it alongside SIGINT/SIGTERM.
    let shutdown = daemon.create_shutdown();

    info!("Daemon started, listening on {}", socket_path.display());
    if let Some(idle) = idle_timeout {
        info!("Idle timeout: {} seconds", idle.as_secs());
    } else {
        info!("No idle timeout (daemon runs until stopped)");
    }

    // The run loop's `Result` is the process's exit status, so this must bind
    // it, not `_`. Startup runs *inside* `run_until` — the bind (a socket path
    // over the 107-byte `sun_path` limit fails here), the registry load, the
    // reconcile — and a discarded `Result` reports every one of those as
    // "Daemon run loop completed" with exit 0, indistinguishable from a clean
    // shutdown (the ADR-0041 swallowed-`Result` class).
    //
    // Signals are the run loop's own business (`Daemon::shutdown_signal`, which
    // takes SIGINT *and* SIGTERM and breaks to teardown). This used to also
    // `select!` on `signal::ctrl_c()` out here — a second handler for the same
    // signal, racing the first: when the outer arm won, the `run_until` future
    // was **dropped** where it stood, so the pool drain, the shutdown flush and
    // the clean-shutdown marker never happened, and a deliberate Ctrl-C left the
    // same evidence as a crash. One handler, one teardown.
    daemon
        .run_until(shutdown)
        .await
        .context("daemon run loop failed")?;
    info!("Daemon run loop completed");

    Ok(())
}

async fn cmd_stop(socket_path: PathBuf) -> Result<()> {
    info!("Stopping daemon on {}", socket_path.display());

    if !is_daemon_running(&socket_path) {
        warn!("Daemon is not running on {}", socket_path.display());
        return Ok(());
    }

    // Send shutdown signal via socket
    use filigrio_protocol::DaemonClient;

    let client = DaemonClient::new(&socket_path);

    client.stop().context("Failed to send shutdown command")?;

    info!("Shutdown command sent successfully; daemon is shutting down");

    // Wait a moment for the daemon to clean up
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    if !is_daemon_running(&socket_path) {
        info!("Daemon stopped successfully");
    } else {
        warn!("Daemon shutdown command sent, but daemon may still be running");
    }

    Ok(())
}

async fn cmd_status(socket_path: PathBuf) -> Result<()> {
    info!("Checking daemon status on {}", socket_path.display());

    if !is_daemon_running(&socket_path) {
        println!("Daemon: stopped");
        return Ok(());
    }

    use filigrio_protocol::{DaemonClient, DaemonClientTrait};

    let client = DaemonClient::new(&socket_path);

    let health = client.health().context("Failed to get daemon health")?;

    println!("Daemon: running");
    println!("  Uptime: {} seconds", health.uptime_secs);
    println!("  Projects: {}", health.project_count);
    println!("  Queue depth: {}", health.queue_depth);
    println!("  Applies in flight: {}", health.applies_inflight);
    println!("  Applies deferred: {}", health.applies_deferred);
    println!("  Memory: {} MB", health.memory_bytes / (1024 * 1024));
    println!("  Last activity: {}", health.last_activity);

    Ok(())
}

async fn cmd_one_shot(socket_path: PathBuf) -> Result<()> {
    info!(
        "Running as one-shot responder on socket: {}",
        socket_path.display()
    );

    // Implement ADR-0032f §4 --no-daemon mode with socket communication
    use filigrio_daemon::socket::{read_request, write_frame};
    use filigrio_daemon::{ColdStoreStateSource, ControlOp, MetaQuery, Request, Responder};
    use tokio::net::UnixListener;

    // Create socket directory if needed
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("failed to create socket directory")?;
    }

    // Bind to the socket
    let listener = UnixListener::bind(&socket_path).context("failed to bind to socket")?;

    info!(
        "One-shot daemon listening on socket: {}",
        socket_path.display()
    );

    // Accept exactly one connection
    let (mut stream, _addr) = listener
        .accept()
        .await
        .context("failed to accept connection")?;

    info!("One-shot daemon accepted connection");

    // Read the framed request via the shared async framing helper.
    let request: Request = read_request(&mut stream)
        .await
        .context("failed to read request")?;

    info!("Handling one-shot request: {:?}", request);

    // Get the current directory as the base for cold store
    let base_dir = std::env::current_dir().context("failed to get current directory")?;

    // ADR-0032f §4/§5: a `--no-daemon` write runs synchronously, right here —
    // the responder is query-only by construction (it has no command entry
    // point), and there is no resident queue/worker pool in one-shot mode.
    // Daemon meta reads have no resident daemon to describe. Since ADR-0042 F9
    // there is no sliver envelope to widen: control commands are the one way
    // into the cold write path.
    let response = match request {
        Request::Control(ControlOp::Command(command)) => run_one_shot_command(&base_dir, command),
        Request::Control(ControlOp::Meta(meta)) => filigrio_protocol::Response::Error {
            message: match meta {
                MetaQuery::Health => "Health describes the resident daemon — there is none in one-shot mode; start the daemon, or drop --no-daemon".to_string(),
                MetaQuery::Progress { project } => format!(
                    "Progress describes the resident daemon's async queue — a one-shot write for '{project}' is synchronous and needs no progress handle"
                ),
            },
        },
        Request::Data(query) => {
            let state_source = ColdStoreStateSource::new(base_dir);
            Responder::new(state_source).handle_query(query)
        }
    };

    // Frame + send the response via the shared async framing helper.
    write_frame(&mut stream, &response)
        .await
        .context("failed to write response")?;

    info!("One-shot request completed successfully, shutting down");

    // Clean up socket
    drop(listener);
    let _ = tokio::fs::remove_file(&socket_path).await;

    Ok(())
}

/// The `--no-daemon` write path (ADR-0032f §4/§5): run the command to
/// completion right now, against a throwaway single-project cache/lock table
/// (there is no resident LRU cache or worker pool to hand it to), and report
/// the result synchronously — no queue, no `progress` handle to poll.
///
/// The project is `cwd`, addressed by whatever `project` string the request
/// carries (normally cwd itself, per §6) — a one-shot process has no registry
/// to resolve a project name against.
fn run_one_shot_command(
    base_dir: &Path,
    command: filigrio_daemon::Command,
) -> filigrio_protocol::Response {
    use filigrio_daemon::{run_command_cold, Project, ProjectLocks, ProjectStateCache};
    use filigrio_protocol::{Command, Response};
    use parking_lot::Mutex;
    use std::sync::Arc;

    let project_id = match &command {
        Command::Submit { project, .. }
        | Command::ProjectIndex { project, .. }
        | Command::ProjectExport { project, .. }
        // ADR-0042 F4: a one-shot writes through, so `flush` is an honest no-op
        // rather than an error — `run_command_cold` reports `wrote: false`.
        | Command::ProjectFlush { project, .. } => project.clone(),
        // Watch is a resident-daemon mode by definition (ADR-0042 F6b) — the
        // same clear rejection `run_command_cold` gives, surfaced before we
        // build a throwaway project for it.
        Command::ProjectWatch { .. } => {
            return Response::Error {
                message: "watch requires the resident daemon (`filigrio server serve`)".to_string(),
            };
        }
        Command::ProjectRegister { .. } | Command::ProjectRemove { .. } | Command::DaemonStop => {
            return Response::Error {
                message: "this command needs the resident daemon's registry/lifecycle — start the daemon, or drop --no-daemon".to_string(),
            };
        }
    };

    // Same id→output_dir rule the cold *read* path already uses (`ColdStoreStateSource::project_output_dir`)
    // — one-shot reads and writes of the same project must land in the same place.
    let output_dir = filigrio_daemon::ColdStoreStateSource::new(base_dir.to_path_buf())
        .project_output_dir(&project_id);
    let project = Project {
        id: project_id,
        root: base_dir.to_path_buf(),
        output_dir,
        watch: false,
    };

    let cache = Arc::new(Mutex::new(ProjectStateCache::new(1)));
    let locks = ProjectLocks::new();

    // The one-shot binary has no daemon config to carry a clustering choice yet;
    // it applies with the default, same as `DaemonConfig::default()`.
    let cluster_cfg = filigrio_pipeline::ClusterConfig::default();

    // ADR-0042 F6c: the response IS the outcome — same typed shape the
    // resident daemon returns, so a client can't tell the lifecycles apart.
    match run_command_cold(&project, command, cluster_cfg, &cache, &locks) {
        Ok(outcome) => Response::CommandCompleted { outcome },
        Err(e) => Response::Error {
            message: format!("one-shot command failed: {e}"),
        },
    }
}
