//! The freshness daemon (ADR-0032 §1).
//!
//! A long-running host draining a priority command queue onto a worker pool.

use crate::{
    cache::ProjectStateCache,
    flush::{Flusher, Persistence},
    handshake::{perform_handshake, release_lock, HandshakeResult},
    locks::ProjectLocks,
    priority::{PriorityQueue, QueueItem},
    project::{Project, ProjectRegistry},
    responder::Responder,
    socket::{read_request, write_frame},
    state_source::RegistryWarmStateSource,
    watchers::WatcherSet,
    Command, ControlOp, DataQuery, Error, HealthStatus, MetaQuery, ProjectStatus, Request,
    Response, Result,
};
use filigrio_ingest::Produced;
use filigrio_protocol::CommandOutcome;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UnixListener as TokioUnixListener;
use tokio::signal;
use tokio::sync::Notify;
use tracing::{error, info, warn};

/// Daemon configuration.
#[derive(Clone, Debug)]
pub struct DaemonConfig {
    /// Socket path for client communication.
    pub socket_path: PathBuf,
    /// Idle shutdown timeout (None = never shut down).
    pub idle_timeout: Option<Duration>,
    /// Clean shutdown marker path.
    pub shutdown_marker_path: PathBuf,
    /// Where the project registry is persisted (ADR-0032 §2 registry-persist).
    pub registry_path: PathBuf,
    /// Max number of full graphs held resident before LRU paging (ADR-0032 §2).
    /// A memory-pressure watermark maps onto this count knob.
    pub cache_capacity: usize,
    /// Max number of applies running concurrently on the worker pool (ADR-0032
    /// §2: "parallelize across projects"). Bounds the drain-level fan-out.
    pub worker_threads: usize,
    /// Debounce window for each project's filesystem watcher (ADR-0032a §1): an
    /// editor save-storm settles to one event per path before it becomes a
    /// candidate changeset. Threaded into every `FsWatcher` the daemon starts.
    pub watcher_debounce: Duration,
    /// Write-behind persistence cadence (ADR-0042 F4/B12): how long a project
    /// may hold unpersisted resident state. Applies to the **producer lane**
    /// only — a client command always flushes before responding.
    pub flush: crate::flush::FlushConfig,
    /// How often the background flush driver asks which projects are due. Pure
    /// granularity: the triggers themselves are read from `clock`, so this is
    /// not a second timeout to reason about.
    pub flush_poll: Duration,
    /// The clock every B12 timer reads. Injectable so the cadence is testable
    /// without sleeping (`TestClock`); production is `SystemClock`.
    pub clock: Arc<dyn crate::flush::Clock>,
    /// Clustering strategy/knobs (ADR-0024) for every apply this daemon runs.
    /// Threaded into each [`Pipeline`](filigrio_pipeline::Pipeline) the apply
    /// path builds — a project cold-built with a non-default config must not
    /// silently degrade to `Simple` clustering on incremental daemon applies.
    pub cluster_cfg: filigrio_pipeline::ClusterConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("/tmp/filigrio-daemon.sock"),
            idle_timeout: Some(Duration::from_secs(300)), // 5 minutes
            shutdown_marker_path: PathBuf::from("/tmp/filigrio-daemon.clean"),
            registry_path: default_registry_path(),
            cache_capacity: 8,
            worker_threads: default_worker_threads(),
            // 250 ms, down from 1 s (2026-09-10): the debounce is pure latency on
            // every save, and 1 s of it was a third of save → visible on next.js.
            // A burst still coalesces — the debouncer waits for 250 ms of quiet —
            // and a quicker trigger costs at most an extra apply the gate absorbs.
            watcher_debounce: Duration::from_millis(250),
            flush: crate::flush::FlushConfig::default(),
            flush_poll: Duration::from_secs(1),
            clock: Arc::new(crate::flush::SystemClock),
            cluster_cfg: filigrio_pipeline::ClusterConfig::default(),
        }
    }
}

/// The entire content of a clean-shutdown marker (ADR-0032 §6). It is a
/// *witness*, not a record: what matters is that the daemon got far enough
/// through `shutdown` to write it in full. Read back byte-for-byte, so a
/// half-written file is not mistaken for the promise.
pub const CLEAN_MARKER: &[u8] = b"clean";

/// Default worker-pool size: available parallelism minus one (leave a core for
/// the run loop + socket handlers), floored at 1, defaulting to 4 if the count
/// is unavailable.
pub fn default_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(1).max(1))
        .unwrap_or(4)
}

/// Default registry location: `$XDG_CONFIG_HOME/filigrio/registry.json`, falling
/// back to `$HOME/.config/...`, then `/tmp/...` if neither is set (headless/CI).
pub fn default_registry_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("filigrio").join("registry.json");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home)
                .join(".config")
                .join("filigrio")
                .join("registry.json");
        }
    }
    PathBuf::from("/tmp/filigrio/registry.json")
}

/// The freshness daemon.
pub struct Daemon {
    config: DaemonConfig,
    pub registry: Arc<Mutex<ProjectRegistry>>,
    queue: PriorityQueue,
    /// Handshake lock file (if we became the daemon)
    handshake_lock: Option<std::fs::File>,
    start_time: Instant,
    /// Per-project lock set (serialize within project, parallelize across).
    /// The [`Flusher`] holds the same table — a flush takes the project's lock.
    locks: ProjectLocks,
    /// Persistence cadence (ADR-0042 F4/B12): owns the resident-state write-back
    /// — which projects are dirty, when they must reach disk, and the write.
    flusher: Arc<Flusher>,
    /// Filesystem watcher set (starts/stops per project).
    watchers: WatcherSet,
    /// The responder — the engine-service layer (ADR-0032f §3).
    /// Handles all queries over the warm cache, ensuring one responder across lifecycles.
    responder: Responder<RegistryWarmStateSource>,
    /// Last activity timestamp.
    last_activity: Arc<Mutex<Instant>>,
    /// Worker-pool telemetry (inflight/deferred), published by the run loop and
    /// read by `health` — pool work `queue_depth` can't see (ADR-0032 §2).
    pool_stats: Arc<crate::scheduler::PoolStats>,
    /// Shutdown signal for graceful termination.
    shutdown: Option<Arc<Notify>>,
}

impl Daemon {
    /// Create a new shutdown signal for this daemon.
    pub fn create_shutdown(&mut self) -> Arc<Notify> {
        let shutdown = Arc::new(Notify::new());
        self.shutdown = Some(shutdown.clone());
        shutdown
    }

    /// Get the shutdown signal for this daemon.
    pub fn get_shutdown(&self) -> Option<&Arc<Notify>> {
        self.shutdown.as_ref()
    }

    /// Request graceful shutdown.
    pub fn request_graceful_shutdown(&self) {
        if let Some(ref notify) = self.shutdown {
            notify.notify_one();
        }
    }

    /// Create a new daemon.
    pub fn new(config: DaemonConfig) -> Self {
        let capacity = config.cache_capacity.max(1);
        let watcher_debounce = config.watcher_debounce;
        let state_cache = Arc::new(Mutex::new(ProjectStateCache::new(capacity)));

        // Registry is shared between the daemon and the responder's state source.
        let registry = Arc::new(Mutex::new(ProjectRegistry::new()));

        let locks = ProjectLocks::new();
        let flusher = Arc::new(Flusher::new(
            state_cache.clone(),
            locks.clone(),
            config.flush,
            config.clock.clone(),
        ));

        // Responder over the registry (which projects exist) + the flusher
        // (their state, resident or paged in) — ADR-0032f §3.
        let state_source = RegistryWarmStateSource::new(flusher.clone(), registry.clone());
        let responder = Responder::new(state_source);

        Daemon {
            config,
            registry,
            queue: PriorityQueue::new(),
            watchers: WatcherSet::new(watcher_debounce),
            locks,
            flusher,
            responder,
            handshake_lock: None, // Will be set during auto-start handshake
            start_time: Instant::now(),
            last_activity: Arc::new(Mutex::new(Instant::now())),
            pool_stats: Arc::new(crate::scheduler::PoolStats::default()),
            shutdown: None,
        }
    }

    /// The one "you named a project I don't have" message (ADR-0032b OQ4),
    /// carrying this daemon's registry as the accepted vocabulary. Every verb
    /// that can decline for want of a registration — and `Status`, which used to
    /// say `project not found` for the identical condition — formats it here.
    fn unregistered(&self, project: &str) -> String {
        let known: Vec<String> = self
            .registry
            .lock()
            .all()
            .into_iter()
            .map(|p| p.id.clone())
            .collect();
        crate::project::unregistered_project_message(project, &known)
    }

    /// Resolve a project path/ID to the actual project ID using registry lookup.
    fn resolve_project_id(&self, input: &str) -> Option<String> {
        use std::path::Path;
        // First, try direct lookup (input might already be a project ID)
        if self.registry.lock().get(input).is_some() {
            return Some(input.to_string());
        }

        // If not found directly, treat input as a path and look up by path
        let path = Path::new(input);
        if let Some(project) = self.registry.lock().find_by_path(path) {
            return Some(project.id.clone());
        }

        None
    }

    /// Start the daemon and run until a shutdown signal (SIGTERM/SIGINT).
    ///
    /// This is the one loop that makes the daemon a daemon: it serves the §3
    /// contract socket **and** drains the priority queue **concurrently**. Before
    /// this fold, the daemon could do one or the other (`serve_socket` blocked, or
    /// `run` drained) — never both, so a live daemon was impossible.
    pub async fn run(self) -> Result<()> {
        self.run_inner(None).await
    }

    /// Like [`run`](Self::run) but also stops on an explicit `shutdown`
    /// notification, so a supervisor — or a test — can stop the loop
    /// deterministically without delivering a signal.
    pub async fn run_until(self, shutdown: Arc<Notify>) -> Result<()> {
        self.run_inner(Some(shutdown)).await
    }

    async fn run_inner(self, shutdown: Option<Arc<Notify>>) -> Result<()> {
        info!("Starting filigrio daemon - entering run_inner loop");
        info!("Daemon configuration: {:?}", self.config);
        info!(
            "Link scope: {:?} (FILIGRIO_LINK_SCOPE=global|scoped overrides)",
            crate::apply::link_scope()
        );

        let socket_path = self.config.socket_path.clone();
        let idle_timeout = self.config.idle_timeout;

        // The daemon state is shared between the queue-drain (this task) and the
        // per-connection handler tasks (ADR-0032 §2/§3). The lock is held only for
        // fast synchronous work — draining the queue, or one `handle_request` — and
        // NEVER across socket I/O, so a slow client cannot stall the daemon or the
        // other clients.
        let shared = Arc::new(Mutex::new(self));

        // ① Startup: recover state, start watchers, open the contract socket.
        let listener = Self::startup(&shared, &socket_path)?;

        // A stateful interval, NOT a per-iteration `sleep`. `select!` builds a fresh
        // future for each branch every loop, so a `sleep(100ms)` branch is *reset*
        // whenever another branch (accept) wins — under steady client traffic the
        // drain would then never fire (starvation). `interval.tick()` accumulates
        // against wall-clock, so the drain runs ~10×/s regardless of accept churn.
        let mut drain_tick = tokio::time::interval(Duration::from_millis(100));

        // The worker pool (ADR-0032 §2 drain-level parallelism) — since ADR-0042
        // F6c this is purely the PRODUCER lane: the drain collects each queued
        // watcher/producer item into a self-contained `ApplyJob` and hands it to
        // the scheduler, which runs applies on `spawn_blocking` tasks: different
        // projects concurrently (bounded by `worker_threads`), same project
        // serialized. Sync wire commands run on their own connection tasks; the
        // per-project lock coordinates the two schedulers. Producer-lane outcomes
        // are logged, not delivered — a watcher event has no client waiting.
        let (worker_threads, pool_stats) = {
            let d = shared.lock();
            (d.config.worker_threads, d.pool_stats.clone())
        };
        let mut scheduler = crate::scheduler::ApplyScheduler::new(worker_threads);
        let mut deferred: Vec<crate::apply::ApplyJob> = Vec::new();
        // Projects whose watcher-lane applies deferred clustering (ClusterTiming::
        // Deferred). Each gets ONE recluster, dispatched only once the project is
        // quiet — nothing queued, deferred or running for it — so a burst of saves
        // costs one Louvain pass after the burst, not one per save.
        let mut owes_recluster: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        loop {
            // Dispatch any backlog now, with exclusive access to the scheduler
            // (outside `select!`, so the drain and join branches never alias it).
            if !deferred.is_empty() {
                deferred = scheduler.dispatch(std::mem::take(&mut deferred));
            }
            if !owes_recluster.is_empty() {
                let jobs: Vec<crate::apply::ApplyJob> = {
                    let d = shared.lock();
                    let quiet: Vec<String> = owes_recluster
                        .iter()
                        .filter(|id| {
                            !d.queue.has_project(id)
                                && !scheduler.is_running(id)
                                && !deferred.iter().any(|j| &j.project.id == *id)
                        })
                        .cloned()
                        .collect();
                    quiet
                        .into_iter()
                        .filter_map(|id| {
                            owes_recluster.remove(&id);
                            d.make_job(&id, crate::apply::Op::Recluster, Persistence::Defer)
                        })
                        .collect()
                };
                if !jobs.is_empty() {
                    deferred.extend(scheduler.dispatch(jobs));
                }
            }

            // Publish pool telemetry (inflight + deferred) for `Health`. This is
            // the work `queue_depth` can't see — commands leave the intake queue as
            // soon as they're collected into the pool.
            pool_stats.record(scheduler.inflight_count(), deferred.len());

            // Idle shutdown: truly idle only when the queue is drained, no jobs are
            // deferred, AND no applies are still running on the pool.
            if let Some(timeout) = idle_timeout {
                let (queue_empty, idle) = {
                    let d = shared.lock();
                    let queue_empty = d.queue.is_empty();
                    let idle = d.last_activity.lock().elapsed();
                    (queue_empty, idle)
                };
                if idle > timeout
                    && queue_empty
                    && deferred.is_empty()
                    && scheduler.is_idle()
                    && owes_recluster.is_empty()
                {
                    info!("Idle timeout reached, shutting down");
                    break;
                }
            }

            tokio::select! {
                // Drain tick: collect the producer lane (ADR-0042 F6c: wire
                // commands execute at ingress — only watcher/producer items are
                // ever queued). Each item becomes an `ApplyJob` appended to
                // `deferred`, dispatched at the loop top. Split into brief
                // critical sections to avoid blocking connection handlers.
                _ = drain_tick.tick() => {
                    // First brief lock: route watcher events only (quick operation)
                    {
                        let mut d = shared
                            .lock();
                        d.route_watcher_events();
                    }

                    // Second brief lock: collect apply jobs from the producer lane
                    {
                        let mut d = shared
                            .lock();
                        d.collect_apply_jobs(&mut deferred);
                    }
                }
                // An apply finished on the pool → clear its slot; the freed
                // capacity lets the loop-top dispatch admit backlog. Guarded so an
                // idle (empty) scheduler doesn't spin on an immediately-ready None.
                completed = scheduler.join_next(), if !scheduler.is_idle() => {
                    match completed {
                        Some((id, Ok(owes))) => {
                            info!("apply complete [{id}]");
                            if owes {
                                owes_recluster.insert(id);
                            }
                        }
                        Some((id, Err(e))) => error!("apply failed [{id}] (skipped): {e}"),
                        None => {}
                    }
                }
                // Accept a client connection and hand it to its own task, so
                // multiple clients — and slow ones — are handled concurrently.
                accepted = listener.accept() => {
                    info!("Client connection accepted");
                    match accepted {
                        Ok((stream, _addr)) => {
                            let d = shared.clone();
                            tokio::spawn(async move {
                                info!("Processing client connection in async task");
                                if let Err(e) = handle_conn(stream, d).await {
                                    warn!("connection handler error: {e}");
                                } else {
                                    info!("Client connection processed successfully");
                                }
                            });
                        }
                        Err(e) => warn!("accept error: {e}"),
                    }
                }
                signal_result = Self::shutdown_signal() => {
                    signal_result?;
                    info!("Shutdown signal received");
                    break;
                }
                _ = wait_for_notify(&shutdown) => {
                    info!("Shutdown requested");
                    break;
                }
            }
        }

        // Drain in-flight applies before teardown so we never abandon a half-written
        // checkpoint. Deferred (never-dispatched) jobs are dropped — startup reconcile
        // (§6) heals any resulting drift on restart.
        owes_recluster.extend(scheduler.drain().await);
        if !deferred.is_empty() {
            warn!(
                "{} apply job(s) undispatched at shutdown; dropped (startup reconcile heals)",
                deferred.len()
            );
        }
        // Owed reclusters run before teardown's final flush: a restart with a clean
        // marker does no apply that would recluster, so skipping them here would
        // leave the newest nodes community-less until the next edit.
        for id in owes_recluster {
            let job = shared
                .lock()
                .make_job(&id, crate::apply::Op::Recluster, Persistence::Defer);
            if let Some(job) = job {
                match tokio::task::spawn_blocking(move || job.run()).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => error!("recluster failed at shutdown [{id}]: {e}"),
                    Err(e) => error!("recluster task failed at shutdown [{id}]: {e}"),
                }
            }
        }

        // ③ Teardown: release the socket + watchers, write the clean-shutdown marker.
        Self::teardown(&shared, &socket_path)
    }

    /// ① Startup phase (ADR-0032 §6): recover state and open the door. Loads the
    /// registry, reconciles each project against disk, starts one watcher per
    /// project (§5), and binds the contract socket (a stale socket from a crashed
    /// daemon is unlinked first; the ADR-0032d §6 flock race-guard is a later
    /// concern). Returns the bound listener the serve loop accepts on.
    fn startup(shared: &Arc<Mutex<Daemon>>, socket_path: &Path) -> Result<TokioUnixListener> {
        // Perform ADR-0032f §6 auto-start handshake with flock race guard
        let (handshake_result, lock_file) = perform_handshake(socket_path)?;

        match handshake_result {
            HandshakeResult::Connected => {
                // A daemon is already running - this should not happen for the daemon binary
                return Err(Error::Other(format!(
                    "Daemon is already running on {}. Use 'filigrio daemon status' to check.",
                    socket_path.display()
                )));
            }
            HandshakeResult::Retry => {
                // Race condition detected - retry should happen at a higher level
                return Err(Error::Other(
                    "Auto-start handshake detected a race condition. Please try again.".to_string(),
                ));
            }
            HandshakeResult::BecameDaemon => {
                // We successfully became the daemon - store the lock file
                {
                    let mut d = shared.lock();
                    d.handshake_lock = lock_file;
                    d.load_registry()?;
                    d.startup_reconcile()?;
                    // Best-effort per §4: a watcher that fails to bind is logged, not fatal.
                    d.start_all_watchers();
                    // ADR-0042 F4/B12 — the write-behind cadence driver. Started
                    // after the startup reconcile (which writes through) so its
                    // first poll sees a settled registry.
                    d.flusher.start_driver(d.config.flush_poll);
                }
            }
        }

        // Clean up stale socket if it exists (should not happen due to handshake, but safe to check)
        if socket_path.exists() {
            let _ = std::fs::remove_file(socket_path);
        }

        let listener = TokioUnixListener::bind(socket_path)
            .map_err(|e| Error::Socket(format!("bind failed: {e}")))?;
        info!(
            "Daemon contract socket listening on {}",
            socket_path.display()
        );
        Ok(listener)
    }

    /// ③ Teardown phase: release resources after the serve loop exits. Best-effort
    /// socket cleanup, release every watcher's inotify handle + debounce thread
    /// (§5), and write the clean-shutdown marker (`shutdown`). Also releases the
    /// handshake lock to allow another daemon to start (ADR-0032f §6). In-flight
    /// connection tasks are short-lived (connection-per-request); a graceful drain
    /// of them on SIGTERM is a lifecycle follow-up (ADR-0032 §OQ2).
    fn teardown(shared: &Arc<Mutex<Daemon>>, socket_path: &Path) -> Result<()> {
        let mut d = shared.lock();

        // Release the handshake lock first to allow another daemon to start
        if let Some(lock_file) = d.handshake_lock.take() {
            let lock_path = socket_path.with_extension("lock");
            if let Err(e) = release_lock(lock_file, &lock_path) {
                warn!("Failed to release handshake lock: {}", e);
            }
        }

        // Clean up socket
        let _ = std::fs::remove_file(socket_path);

        // Stop watchers and shutdown
        d.stop_all_watchers();
        d.shutdown()
    }

    // ---- ADR-0032 §3: the commands/queries funnel + socket transport ----

    /// The single transport-agnostic request funnel (ADR-0032 §3), routed by
    /// plane (ADR-0032f §2, now a *type-level* split):
    ///
    /// - **Control** — a mutation, executed **synchronously** (ADR-0042 F6c:
    ///   the response carries the typed outcome — no ack, no job id, no
    ///   silent-failure path), or a daemon meta read (`Health`/`Progress`,
    ///   answered here — meta is about the daemon, which is exactly the state
    ///   the responder deliberately doesn't hold).
    /// - **Data** — read-only graph queries, served synchronously from state.
    ///
    /// Two planes, not three: the narrow MCP "sliver" that used to widen into a
    /// `Command` here is gone with ADR-0042 F9 (the bridge has no mutation
    /// surface, so nothing produced one). Every mutation the daemon executes
    /// now arrives on the control plane — one route in, not two.
    ///
    /// Structured as **plan → execute** so the transports can release the
    /// daemon lock before the (possibly minutes-long) apply work runs:
    /// [`plan_request`](Self::plan_request) does the brief resolution under
    /// whatever lock the caller holds, [`execute_planned`](Self::execute_planned)
    /// runs the work under only the per-project lock. This method is the
    /// in-place composition of the two for callers that own the daemon
    /// directly (tests, the one-shot responder) — same code, no divergence.
    pub fn handle_request(&mut self, request: Request) -> Response {
        let planned = self.plan_request(request);
        Self::execute_planned(planned)
    }

    /// Transport entry (ADR-0042 F6c): plan under a **brief** daemon-lock
    /// section, then execute with the daemon lock released — a heavy sync
    /// command never stalls queries or other clients; within-project
    /// serialization is the per-project lock's job.
    pub fn handle_request_shared(daemon: &Arc<Mutex<Daemon>>, request: Request) -> Response {
        let planned = {
            let mut d = daemon.lock();
            d.plan_request(request)
        };
        Self::execute_planned(planned)
    }

    /// Phase 1: route + resolve under the caller's lock. Everything cheap
    /// (queries, meta reads, registry mutations, watch off) completes here;
    /// apply-shaped work (index, submit, export, watch-on's initial converge)
    /// is returned as a [`Planned::Exec`] for phase 2.
    fn plan_request(&mut self, request: Request) -> Planned {
        match request {
            Request::Control(op) => match op {
                ControlOp::Command(cmd) => self.plan_command(cmd),
                ControlOp::Meta(meta) => Planned::Ready(self.handle_meta(meta)),
            },
            Request::Data(query) => Planned::Ready(self.handle_data(query)),
        }
    }

    /// Plan one mutation (ADR-0042 F6c). Registry/lifecycle verbs are handled
    /// inline (they mutate daemon state and must stay serialized under the
    /// daemon lock); apply-shaped verbs resolve to a self-contained
    /// [`Exec`] that phase 2 runs under only the per-project lock.
    fn plan_command(&mut self, cmd: Command) -> Planned {
        *self.last_activity.lock() = Instant::now();
        match cmd {
            Command::ProjectRegister { path } => match self.handle_project_register(&path) {
                Ok(project) => Planned::done(CommandOutcome::Registered {
                    project: project.id,
                    path,
                }),
                Err(e) => Planned::error(format!("project register failed: {e}")),
            },
            Command::ProjectRemove { project } => match self.handle_project_remove(&project) {
                Ok(()) => Planned::done(CommandOutcome::Removed { project }),
                Err(e) => Planned::error(format!("project remove failed: {e}")),
            },
            Command::DaemonStop => {
                info!("Received DaemonStop command, initiating graceful shutdown");
                self.request_graceful_shutdown();
                Planned::done(CommandOutcome::Stopping)
            }
            Command::ProjectIndex { project, clean } => {
                // ADR-0042 F5: `clean` (wipe-and-reindex) is reserved, not
                // implemented — reject HERE, before a job exists, so the flag
                // is never silently ignored (ADR-0029 honesty for flags). The
                // rejection now reaches the client as a typed error instead of
                // a daemon-side log line.
                if clean {
                    return Planned::error(crate::apply::clean_reindex_unsupported().to_string());
                }
                // A wire index is always the full reconcile (F6b: depth is
                // Op-internal; the shallow fast-path belongs to the watcher).
                match self.make_job(
                    &project,
                    crate::apply::Op::Reconcile { deep: true },
                    Persistence::Flush,
                ) {
                    Some(job) => Planned::Exec(Exec::Apply {
                        job,
                        verb: ApplyVerb::Index,
                    }),
                    None => Planned::error(self.unregistered(&project)),
                }
            }
            Command::Submit {
                project,
                changeset,
                priority: _,
            } => match self.make_job(
                &project,
                crate::apply::Op::Apply(changeset),
                Persistence::Flush,
            ) {
                Some(job) => Planned::Exec(Exec::Apply {
                    job,
                    verb: ApplyVerb::Submit,
                }),
                None => Planned::error(self.unregistered(&project)),
            },
            Command::ProjectExport { project } => {
                let Some(id) = self.resolve_project_id(&project) else {
                    return Planned::error(self.unregistered(&project));
                };
                let Some(p) = self.registry.lock().get(&id).cloned() else {
                    return Planned::error(self.unregistered(&project));
                };
                Planned::Exec(Exec::Export {
                    project: p,
                    flusher: self.flusher.clone(),
                })
            }
            // ADR-0042 F4 — the explicit flush verb (B12). Named *flush*, not
            // *checkpoint*: Phase 3 uses "checkpoint" for commit-keyed
            // publication, which is a different thing.
            Command::ProjectFlush { project } => {
                let Some(id) = self.resolve_project_id(&project) else {
                    return Planned::error(self.unregistered(&project));
                };
                Planned::Exec(Exec::Flush {
                    project: id,
                    flusher: self.flusher.clone(),
                })
            }
            Command::ProjectWatch { project, on } => self.plan_watch(&project, on),
        }
    }

    /// Plan `project watch on|off` (ADR-0042 F6b).
    ///
    /// `on`: idempotent if already watching; otherwise **start the watcher
    /// FIRST** (events begin buffering — the lost-event-free ordering: an edit
    /// made during the initial converge still produces an event, there is no
    /// gap), then hand back the synchronous deep reconcile as an `Exec`, which
    /// persists `watch = true` and responds with the converged+watching
    /// outcome. `off`: stop the watcher, persist `watch = false`, respond
    /// (idempotent if not watching).
    fn plan_watch(&mut self, project: &str, on: bool) -> Planned {
        let Some(id) = self.resolve_project_id(project) else {
            return Planned::error(self.unregistered(project));
        };
        let Some(p) = self.registry.lock().get(&id).cloned() else {
            return Planned::error(self.unregistered(project));
        };
        if on {
            if self.watchers.is_watching(&id) {
                return Planned::done(CommandOutcome::Watch {
                    project: id,
                    watching: true,
                    changed: None,
                    vanished: None,
                    note: Some("already watching — no reconcile run".to_string()),
                });
            }
            // Watcher first: from this point every FS event buffers in the
            // producer channel, so nothing written during the converge is lost.
            if let Err(e) = self.watchers.start(&p) {
                return Planned::error(format!("failed to start watcher for {id}: {e}"));
            }
            let Some(job) = self.make_job(
                &id,
                crate::apply::Op::Reconcile { deep: true },
                Persistence::Flush,
            ) else {
                return Planned::error(self.unregistered(project));
            };
            Planned::Exec(Exec::Apply {
                job,
                verb: ApplyVerb::WatchOn {
                    registry: self.registry.clone(),
                    registry_path: self.config.registry_path.clone(),
                },
            })
        } else {
            let was_watching = self.watchers.is_watching(&id);
            self.watchers.stop(&id);
            if let Err(e) = self.registry.lock().set_watch(&id, false) {
                return Planned::error(format!("failed to persist watch=off for {id}: {e}"));
            }
            if let Err(e) = self.save_registry() {
                return Planned::error(format!("failed to persist watch=off for {id}: {e}"));
            }
            Planned::done(CommandOutcome::Watch {
                project: id,
                watching: false,
                changed: None,
                vanished: None,
                note: (!was_watching).then(|| "was not watching — nothing to stop".to_string()),
            })
        }
    }

    /// Phase 2: run planned work with the daemon lock released. The apply/
    /// export executes under only the **per-project lock** (same-project work
    /// serializes with producer-lane pool jobs; different projects run
    /// concurrently). Wrapped in `catch_unwind` — the scheduler's pattern
    /// applied to the sync path — so a panicking apply becomes a typed error
    /// response, never a dropped connection (ADR-0042 F6c, carried from the
    /// retired 0032g draft).
    fn execute_planned(planned: Planned) -> Response {
        let exec = match planned {
            Planned::Ready(response) => return response,
            Planned::Exec(exec) => exec,
        };
        let project_id = exec.project_id().to_string();
        match catch_command_panic(&project_id, move || exec.run()) {
            Ok(outcome) => Response::CommandCompleted { outcome },
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        }
    }

    /// Route a control-plane meta read (ADR-0032f §2). These are read-only but
    /// about the *daemon* — uptime, queue, apply progress — so they're answered
    /// here, not by the responder. With `Health` a `MetaQuery` (not a data
    /// query), the responder's old "Health must be handled by the daemon"
    /// rejection arm is structurally impossible to reach — it no longer exists.
    fn handle_meta(&self, meta: MetaQuery) -> Response {
        match meta {
            MetaQuery::Health => match serde_json::to_value(self.health()) {
                Ok(data) => Response::QueryResult { data },
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            },
            MetaQuery::Progress { project } => Response::Error {
                message: format!("Progress is not yet implemented (project: {project})"),
            },
        }
    }

    /// Route a data-plane query: delegate to the responder (ADR-0032f §3).
    ///
    /// One deliberate interception remains: per-project `Status` is answered by
    /// `project_status` here because it reports on the *daemon's* handling of a
    /// project — whether a watcher follows it, and how far its checkpoint lags
    /// resident state (ADR-0042 F4/B12) — which is state the responder does not
    /// hold. Registration and state-paging are no longer part of that reason:
    /// both lifecycles now resolve against the registry and page the checkpoint
    /// in on a cold cache.
    fn handle_data(&self, query: DataQuery) -> Response {
        match query {
            // A registered-but-unbuilt project reads as an empty status rather
            // than an error: zero files and zero nodes is the true answer to
            // "what state is this project in", where it would be a fabrication
            // as an answer to a graph query.
            DataQuery::Status {
                project: Some(project),
            } => {
                let Some(id) = self.resolve_project_id(&project) else {
                    return Response::Error {
                        message: self.unregistered(&project),
                    };
                };
                match self.project_status(&id) {
                    Ok(status) => match serde_json::to_value(status) {
                        Ok(data) => Response::QueryResult { data },
                        Err(e) => Response::Error {
                            message: e.to_string(),
                        },
                    },
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                }
            }
            DataQuery::Status { project: None } => Response::Error {
                message: "all-projects Status is not yet implemented".to_string(),
            },
            // All other queries are fully delegated to the responder (ADR-0032f §3)
            _ => self.responder.handle_query(query),
        }
    }

    /// Serve the §3 contract over a unix-domain socket on the **blocking**
    /// transport (`filigrio-protocol`'s `SocketServer` — the same wire format
    /// as the live daemon's async accept loop, one implementation). Each
    /// connection carries one request, planned under a brief daemon lock and
    /// executed with it released (`handle_request_shared`, ADR-0042 F6c). `max`
    /// bounds the number of connections served (`None` = forever; `Some(n)`
    /// lets the transport tests terminate deterministically without the run
    /// loop's drain in play).
    pub fn serve_socket(
        daemon: Arc<Mutex<Daemon>>,
        path: impl AsRef<Path>,
        max: Option<usize>,
    ) -> Result<()> {
        let server = filigrio_protocol::SocketServer::bind(path).map_err(Error::from)?;
        server
            .run_bounded(max, move |req| {
                Ok(Daemon::handle_request_shared(&daemon, req))
            })
            .map_err(Error::from)
    }

    /// Load the project registry from disk (ADR-0032 §2 registry-persist).
    /// Called on startup so the daemon recovers every project registered before
    /// the last shutdown. An absent file is a clean first run; a corrupt file is
    /// a loud error (never silently drop the registry).
    pub fn load_registry(&mut self) -> Result<()> {
        let persisted = ProjectRegistry::load_from(&self.config.registry_path)
            .map_err(|e| Error::Storage(format!("load registry: {e}")))?;
        // Merge, don't clobber: projects registered in-memory before startup (a
        // caller that pre-seeded the registry) must survive alongside the
        // persisted set. Same-id entries already present win (they're live).
        let mut added = 0;
        for project in persisted.all() {
            if self.registry.lock().get(&project.id).is_none() {
                let _ = self.registry.lock().add(project.clone());
                added += 1;
            }
        }
        info!(
            "Loaded {} project(s) from {}",
            added,
            self.config.registry_path.display()
        );
        Ok(())
    }

    /// Run startup reconcile for every **watched** project (ADR-0042 F6b:
    /// `serve` startup reconciles + watches only `watch == true` projects — an
    /// unwatched registered project is cold by contract, indexed on demand).
    pub fn startup_reconcile(&mut self) -> Result<()> {
        // Clean-shutdown marker present → cheap mtime reconcile; absent (crash) →
        // deep re-hash (ADR-0032 §6). Deep exists precisely for mtime-preserved
        // drift, so it MUST apply authoritatively — see `reconcile_and_apply`.
        //
        // Reading the marker **consumes** it, and it is consumed *here* — before
        // a single project is reconciled. That ordering is the crash-safety of
        // this whole mechanism: from this instant until `shutdown` writes it
        // again, no marker exists on disk, so any exit that does not run
        // `shutdown` — SIGKILL, OOM, power loss, a panic mid-reconcile — leaves
        // the absence that means "crash" and the next start re-hashes. Deferring
        // the removal until after the reconcile loop would leave a daemon that
        // died *during* recovery looking clean to its successor, which is the
        // one case that must never read cheap.
        //
        // Only the daemon that won the handshake reaches here (`Daemon::startup`),
        // so a second process refused on this socket cannot consume the running
        // daemon's marker.
        let marker = self.config.shutdown_marker_path.clone();
        // Only the daemon's own complete marker counts as a promise. A file that
        // exists but does not read back as `CLEAN_MARKER` is a *torn* one — and
        // the window for that is real now that the write happens at shutdown,
        // the moment a crash is most plausible: `fs::write` is create-then-write,
        // so dying between the two leaves an empty file. Presence alone would
        // read that as "clean", the unsafe direction; the content check makes an
        // interrupted promise no promise at all.
        let clean = match std::fs::read(&marker) {
            Ok(body) => body == CLEAN_MARKER,
            Err(_) => false, // absent, or unreadable — either way, not a promise
        };
        let deep = !clean;
        if deep {
            info!("No clean shutdown marker; running deep reconcile on startup");
        } else {
            info!("Clean shutdown marker found; running mtime reconcile on startup");
        }
        // Consume it either way: a marker we declined to trust must not survive
        // to be re-read by the start after this one.
        if marker.exists() {
            if let Err(e) = std::fs::remove_file(&marker) {
                // Non-fatal, but loud: a marker we could not consume would make
                // the *next* start read clean whatever happens to this one — the
                // exact unsafe direction. Refusing to start over it would be
                // worse (the daemon is otherwise healthy), so this is an error
                // log, not a returned `Err`.
                error!(
                    "could not consume clean-shutdown marker {}: {e} — a crash of this daemon may \
                     be mistaken for a clean exit and skip the deep reconcile",
                    marker.display()
                );
            }
        }

        let projects: Vec<Project> = self
            .registry
            .lock()
            .all()
            .into_iter()
            .filter(|p| p.watch)
            .cloned()
            .collect();
        for project in &projects {
            // Startup reconcile is a *triggered-pull producer* firing once (ADR-0032e
            // §1/§4): the trigger fires immediately, `op_of` maps its `Reconcile`
            // signal onto the same `Op` the live queue would use, and the job runs
            // through the one apply path (`ApplyJob::run`) — not a bespoke bypass.
            // Authoritative (ADR-0032a R2.5): NOT the signal gate, so a deep
            // crash-recovery drift is actually applied rather than dropped by the
            // shallow dedup.
            if let Err(e) = self.run_reconcile_now(project, deep) {
                error!("Startup reconcile failed for {}: {}", project.id, e);
            }
        }

        Ok(())
    }

    /// Drain the producer lane for the run loop's worker-pool path: every
    /// queued [`QueueItem`] — since ADR-0042 F6c the queue holds ONLY producer
    /// (watcher) output, wire commands execute at ingress — becomes a
    /// self-contained [`ApplyJob`](crate::apply::ApplyJob) appended to `out`
    /// for the scheduler to run off the daemon lock. Returns immediately after
    /// collecting.
    ///
    /// A single item's failure to resolve is **logged and skipped** — one stale
    /// producer item (e.g. a project removed mid-flight) must never crash the
    /// daemon or stall the other projects' work.
    pub(crate) fn collect_apply_jobs(&mut self, out: &mut Vec<crate::apply::ApplyJob>) {
        while let Some(item) = self.queue.pop() {
            // Liveness timestamp; the parking_lot lock never poisons.
            *self.last_activity.lock() = Instant::now();
            // The producer lane is **write-behind** (ADR-0042 B12): nobody is
            // waiting on a watcher event, so its result updates resident state
            // and the flusher decides when it reaches disk.
            match self.make_job(&item.project, item.op, Persistence::Defer) {
                Some(job) => out.push(job),
                None => error!(
                    "queued producer item for unregistered project '{}'; skipped",
                    item.project
                ),
            }
        }
    }

    /// A clone of the per-project lock handle (shares the same lock table).
    /// Exposed so a worker-pool drain — or a test — can coordinate on the same
    /// per-project locks the apply path uses.
    pub fn locks(&self) -> ProjectLocks {
        self.locks.clone()
    }

    /// Apply a changeset to one project through its per-project lock (ADR-0032 §2).
    ///
    /// This is the seam a concurrent drain calls: it acquires the project's lock
    /// so two applies to the *same* project serialize (no lost update), while
    /// applies to *different* projects hold different locks and run concurrently.
    /// The lock is held only around this project's `apply` — never a global lock —
    /// so a heavy monorepo apply cannot block a small repo's apply.
    pub fn apply_project(
        &self,
        project_id: &str,
        changeset: &filigrio_core::ChangeSet,
    ) -> Result<()> {
        self.apply_in_lane(project_id, changeset, Persistence::Flush)
    }

    /// Apply a changeset on the **producer lane** (ADR-0042 F4/B12): the
    /// watcher's cadence. Identical work to [`apply_project`](Self::apply_project)
    /// — same gate, same lock, same engine — but the result is *write-behind*:
    /// resident state updates and the project is marked dirty, and the flusher
    /// persists it on quiescence / the max-dirty-age cap / shutdown / dirty
    /// eviction. Nobody is waiting on a filesystem event, so paying a full store
    /// write per keystroke-batch buys nothing.
    pub fn apply_producer(
        &self,
        project_id: &str,
        changeset: &filigrio_core::ChangeSet,
    ) -> Result<()> {
        self.apply_in_lane(project_id, changeset, Persistence::Defer)
    }

    fn apply_in_lane(
        &self,
        project_id: &str,
        changeset: &filigrio_core::ChangeSet,
        persistence: Persistence,
    ) -> Result<()> {
        let project = self
            .registry
            .lock()
            .get(project_id)
            .ok_or_else(|| Error::ProjectNotFound(project_id.to_string()))?
            .clone();
        // `apply_core` acquires the per-project lock itself (single source of truth).
        crate::apply::apply_core(
            &project,
            changeset,
            self.config.cluster_cfg,
            &self.flusher,
            persistence,
        )
        .map(|_changed| ())
    }

    /// The persistence cadence (ADR-0042 F4/B12) — dirty projects, the flush
    /// triggers, and the write itself.
    pub fn flusher(&self) -> Arc<Flusher> {
        self.flusher.clone()
    }

    /// Build a self-contained [`ApplyJob`](crate::apply::ApplyJob) for an
    /// [`Op`](crate::apply::Op) — runnable off the daemon lock, on the
    /// producer-lane worker pool or the sync command path alike. `None` if
    /// `project_id` is unregistered.
    fn make_job(
        &self,
        project_id: &str,
        op: crate::apply::Op,
        persistence: Persistence,
    ) -> Option<crate::apply::ApplyJob> {
        // Clients send the cwd *path*; the registry is keyed by *id*. Resolve either
        // (path or id) so Submit / ProjectIndex both match.
        let id = self.resolve_project_id(project_id)?;
        let project = self.registry.lock().get(&id)?.clone();
        Some(crate::apply::ApplyJob {
            project,
            op,
            cluster_cfg: self.config.cluster_cfg,
            persistence,
            flusher: self.flusher.clone(),
        })
    }

    // ---- ADR-0032a: filesystem-watcher lifecycle (delegated to `WatcherSet`) ----

    /// Start a watcher for every project with `watch == true` (startup path).
    /// ADR-0042 F6b: watching is an explicit persisted per-project mode —
    /// auto-watching every registered project meant recursive inotify watches
    /// on all of them (fd/watch-limit exhaustion on a many-project daemon); an
    /// unwatched registered project is cold by contract.
    ///
    /// Public alongside [`startup_reconcile`](Self::startup_reconcile): the two
    /// together *are* the startup contract, so a test must be able to drive
    /// them without standing up the whole run loop.
    pub fn start_all_watchers(&mut self) {
        let projects: Vec<Project> = self
            .registry
            .lock()
            .all()
            .into_iter()
            .filter(|p| p.watch)
            .cloned()
            .collect();
        self.watchers.start_all(&projects);
    }

    /// Stop every watcher (shutdown path).
    pub(crate) fn stop_all_watchers(&mut self) {
        self.watchers.stop_all();
    }

    /// Drain every producer's output into the priority queue (ADR-0032a §1). The
    /// `WatcherSet` yields `(project, Produced, Priority)` triples generically —
    /// it never interprets `Produced` — and the single total `op_of` mapping
    /// (ADR-0032e §2) turns each into a daemon-internal [`QueueItem`] directly:
    /// the watcher's re-scoping trigger enters as `Op::Reconcile { deep: false }`,
    /// an edit as `Op::Apply`, and a future git producer's `Authoritative` as
    /// `Op::ApplyExact` — no wire `Command` detour (ADR-0042 F6b: only producer
    /// output ever enters this queue). This route happens on every drain tick,
    /// before the pool collect, so a filesystem event submitted during the
    /// interval becomes an ApplyJob in the same tick that produced it
    /// (ADR-0032a §1's "one-pass" contract, otherwise there'd be one tick of lag).
    pub(crate) fn route_watcher_events(&mut self) {
        for (project, produced, priority) in self.watchers.drain() {
            self.queue.submit(QueueItem {
                project,
                op: crate::apply::op_of(produced),
                lane: priority,
            });
        }
    }

    /// Whether a live watcher exists for `project_id`.
    pub fn is_watching(&self, project_id: &str) -> bool {
        self.watchers.is_watching(project_id)
    }

    /// Number of live watchers.
    pub fn watcher_count(&self) -> usize {
        self.watchers.count()
    }

    /// Run a reconcile synchronously for `project` (ADR-0032e §1/§4): the shape
    /// `startup_reconcile` (which runs before the run loop's queue/worker pool
    /// exist) needs. Goes through `op_of` — same as every other
    /// path — rather than calling `apply::reconcile_and_apply` directly, so this
    /// is not a second, bespoke bypass of the `Produced`/`Op` mapping. A
    /// `Reconcile` request is a bare signal (its own walk/diff happens
    /// downstream, against `prior` state this call site doesn't hold) — there is
    /// no `Source` to poll here, unlike `TriggeredProducer`'s `Authoritative`
    /// mode, so building one would hold an `FsSource` that's never used. The
    /// other two routes to the same work — a wire `ProjectIndex` (executed at
    /// ingress, ADR-0042 F6c) and a watcher event (queued onto the producer
    /// lane) — both build the same `Op::Reconcile` job and run it through
    /// `ApplyJob::run`, so the three can never diverge.
    fn run_reconcile_now(&self, project: &Project, deep: bool) -> Result<()> {
        let job = crate::apply::ApplyJob {
            project: project.clone(),
            op: crate::apply::op_of(Produced::Reconcile { deep }),
            cluster_cfg: self.config.cluster_cfg,
            // Startup convergence writes through (ADR-0042 F4): the checkpoint
            // it heals is the thing a restart will read next time.
            persistence: Persistence::Flush,
            flusher: self.flusher.clone(),
        };
        job.run()?;
        // Parity with the pool path's "apply complete [id]" (scheduler.join_next),
        // which the synchronous startup/index path otherwise never emits.
        info!("apply complete [{}]", project.id);
        Ok(())
    }

    /// Handle a `ProjectRegister` command: register + persist, **without** starting a
    /// watcher (ADR-0042 F6b: watch is an explicit mode, default OFF — the full
    /// verb set is `register` → `index` → (`export` | `watch on` | …); nothing
    /// hidden happens on register). Returns the registered project so the
    /// caller can build the typed outcome.
    fn handle_project_register(&mut self, path: &str) -> Result<Project> {
        let root = PathBuf::from(path);
        let project = Project::new(root);

        self.registry
            .lock()
            .add(project.clone())
            .map_err(Error::Other)?;

        info!(
            "Added project: {} (watch off — cold by contract)",
            project.id
        );

        self.save_registry()?;

        Ok(project)
    }

    /// Handle a ProjectRemove command.
    fn handle_project_remove(&mut self, project_id: &str) -> Result<()> {
        self.registry
            .lock()
            .remove(project_id)
            .map_err(Error::Other)?;

        // Stop watching before the project is forgotten (ADR-0032a §5), so a
        // stale watcher can't keep submitting for an unregistered project.
        self.watchers.stop(project_id);

        info!("Removed project: {}", project_id);

        // Save registry
        self.save_registry()?;

        Ok(())
    }

    /// Save the project registry to disk atomically (ADR-0032 §2 registry-persist).
    /// Called after every registry mutation so a restart recovers the change.
    fn save_registry(&self) -> Result<()> {
        self.registry
            .lock()
            .save_to(&self.config.registry_path)
            .map_err(|e| Error::Storage(format!("save registry: {e}")))
    }

    /// Graceful shutdown — and the **only** writer of the clean-shutdown marker
    /// (ADR-0032 §6).
    ///
    /// The marker is a promise about the state this daemon leaves behind: "I
    /// flushed, so `state.json`'s manifest describes the tree as of now, and the
    /// next start may trust file mtimes instead of re-hashing." Nothing but a
    /// completed graceful shutdown may make that promise, which is why it is
    /// written *here*, at the end, and consumed at startup. A process that is
    /// SIGKILLed runs no code at all and therefore writes nothing — the absence
    /// is the crash signal, and "we did not get to say we were clean" is exactly
    /// what the reader should conclude.
    pub fn shutdown(&self) -> Result<()> {
        info!("Shutting down daemon");

        // ADR-0042 F4/B12 — graceful shutdown is a flush trigger. Stop the
        // cadence driver first so it cannot race this final pass, then persist
        // every project still holding unpersisted resident state. A failure here
        // is logged, not fatal: the durability contract is that a lost window
        // costs re-work on the next reconcile, never a wrong answer, and
        // refusing to shut down would be the worse trade.
        self.flusher.stop_driver();
        let flush_failures = self.flusher.flush_all();
        for (id, e) in &flush_failures {
            error!("shutdown flush failed for {id} (startup reconcile will heal it): {e}");
        }

        // …and "the next reconcile heals it" is only true if the next reconcile
        // is the deep one. A project whose final flush failed has resident state
        // the store never received, so the marker's promise does not hold: skip
        // it and let the successor re-hash.
        if !flush_failures.is_empty() {
            warn!(
                "{} project(s) failed their shutdown flush; withholding the clean-shutdown marker \
                 so the next start runs a deep reconcile",
                flush_failures.len()
            );
            return Ok(());
        }

        if let Err(e) = std::fs::write(&self.config.shutdown_marker_path, CLEAN_MARKER) {
            // Same trade as the flush above, in the safe direction: without the
            // marker the next start is merely slower (deep), never wrong, so a
            // shutdown that has already flushed everything does not fail over it.
            error!(
                "failed to write clean-shutdown marker {}: {e} — the next start will deep-reconcile",
                self.config.shutdown_marker_path.display()
            );
        }

        Ok(())
    }

    /// Get health status.
    pub fn health(&self) -> HealthStatus {
        let elapsed = self.last_activity.lock().elapsed();
        let (applies_inflight, applies_deferred) = self.pool_stats.snapshot();
        HealthStatus {
            uptime_secs: self.start_time.elapsed().as_secs(),
            project_count: self.registry.lock().count(),
            queue_depth: self.queue.len(),
            applies_inflight,
            applies_deferred,
            memory_bytes: Self::get_memory_usage(),
            last_activity: format!("{:?}", elapsed),
        }
    }

    /// Get status for a specific project.
    pub fn project_status(&self, project_id: &str) -> Result<ProjectStatus> {
        let project = self
            .registry
            .lock()
            .get(project_id)
            .ok_or_else(|| Error::ProjectNotFound(project_id.to_string()))?
            .clone();

        // Served from the resident cache when hot (paged in from disk on a miss).
        let state = self.flusher.state_of(&project)?;

        Ok(ProjectStatus {
            project: project_id.to_string(),
            has_drift: false, // TODO: Run quick drift check
            last_revision: state.manifest.latest_revision().map(|r| r.0.clone()),
            file_count: state.manifest.entries.len(),
            node_count: state.graph.nodes.len(),
            // ADR-0042 F6b: staleness must be visible — status says whether a
            // live watcher is following this project.
            watching: self.watchers.is_watching(&project.id),
            // ADR-0042 F4/B12 honesty: under write-behind an out-of-process
            // reader of `.filigrio-out` can lag the daemon. These three say by
            // how much, instead of leaving the client to assume it cannot.
            dirty: self.flusher.is_dirty(&project.id),
            dirty_for_secs: self.flusher.dirty_for(&project.id).map(|d| d.as_secs()),
            last_persisted_secs_ago: self
                .flusher
                .since_persisted(&project.id)
                .map(|d| d.as_secs()),
        })
    }

    /// Get current memory usage (RSS) in bytes.
    fn get_memory_usage() -> u64 {
        // Platform-specific memory detection
        #[cfg(unix)]
        {
            use std::mem;

            unsafe {
                let mut info: libc::rusage = mem::zeroed();
                libc::getrusage(libc::RUSAGE_SELF, &mut info);
                info.ru_maxrss as u64 * 1024 // Convert KB to bytes
            }
        }

        #[cfg(not(unix))]
        {
            0 // Not implemented for non-Unix
        }
    }

    /// Create a shutdown signal future.
    async fn shutdown_signal() -> Result<()> {
        #[cfg(unix)]
        {
            let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
                .map_err(|e| Error::Other(format!("failed to setup SIGTERM handler: {e}")))?;
            let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())
                .map_err(|e| Error::Other(format!("failed to setup SIGINT handler: {e}")))?;

            tokio::select! {
                _ = sigterm.recv() => {}
                _ = sigint.recv() => {}
            }
        }

        #[cfg(windows)]
        {
            // Windows doesn't have SIGTERM/SIGINT in the same way
            // For now, just wait forever
            std::future::pending::<()>().await;
        }

        Ok(())
    }
}

/// Handle one client connection on its own task (ADR-0032 §3).
///
/// Reads a framed request, **plans** it under a brief daemon lock, then
/// executes with the lock released (ADR-0042 F6c) on `spawn_blocking` — a sync
/// command is blocking CPU work that can run minutes, and must park neither a
/// runtime worker nor the daemon lock; concurrent clients serialize only on the
/// microsecond-scale planning (and, same-project, on the per-project lock).
/// A waiting connection is just a parked task. Framing goes through the shared
/// `socket` helpers so this async path can't drift from the blocking server's
/// wire format.
async fn handle_conn(mut stream: tokio::net::UnixStream, daemon: Arc<Mutex<Daemon>>) -> Result<()> {
    let request = read_request(&mut stream).await?;

    info!("handle_conn: About to handle request: {:?}", request);
    let response =
        tokio::task::spawn_blocking(move || Daemon::handle_request_shared(&daemon, request))
            .await
            // A JoinError here means the blocking task itself died (the command
            // path catch_unwinds its execution, so this is planning-phase only) —
            // still answer with a typed error, never a dropped connection.
            .unwrap_or_else(|e| Response::Error {
                message: format!("command task failed: {e}"),
            });
    info!("handle_conn: Got response: {:?}", response);

    write_frame(&mut stream, &response).await
}

/// A request after phase-1 planning (ADR-0042 F6c): either fully answered under
/// the daemon lock, or apply-shaped work to run with it released.
enum Planned {
    /// Fully handled under the lock (queries, meta reads, registry mutations,
    /// watch-off, errors).
    Ready(Response),
    /// Self-contained work for phase 2 — runs under only the per-project lock.
    Exec(Exec),
}

impl Planned {
    fn done(outcome: CommandOutcome) -> Planned {
        Planned::Ready(Response::CommandCompleted { outcome })
    }

    fn error(message: String) -> Planned {
        Planned::Ready(Response::Error { message })
    }
}

/// The off-daemon-lock half of a synchronous command (ADR-0042 F6c). Holds
/// clones of everything it touches (the same discipline as the pool's
/// [`ApplyJob`](crate::apply::ApplyJob)), so it borrows nothing from the daemon.
enum Exec {
    /// Apply-shaped work (index / submit / watch-on's initial converge),
    /// running through the exact same `ApplyJob::run` the producer-lane pool
    /// uses — one apply path, two schedulers, the per-project lock coordinates.
    Apply {
        job: crate::apply::ApplyJob,
        verb: ApplyVerb,
    },
    /// The explicit `graph.json` export (ADR-0042 F2). Takes the per-project
    /// lock so it serializes with applies and never reads a half-applied store.
    Export {
        project: Project,
        flusher: Arc<Flusher>,
    },
    /// The explicit `project flush` verb (ADR-0042 F4/B12): persist this
    /// project's resident state now. Takes the per-project lock inside.
    Flush {
        project: String,
        flusher: Arc<Flusher>,
    },
}

/// Which verb an [`Exec::Apply`] answers for — decides the typed outcome shape
/// and any post-apply persistence.
enum ApplyVerb {
    /// `ProjectIndex` → [`CommandOutcome::Indexed`].
    Index,
    /// `Submit` → [`CommandOutcome::Applied`].
    Submit,
    /// `ProjectWatch { on: true }`'s initial deep converge: on success,
    /// persist `watch = true` (registry handle + path travel with the exec so
    /// no daemon lock is needed) → [`CommandOutcome::Watch`].
    WatchOn {
        registry: Arc<Mutex<ProjectRegistry>>,
        registry_path: PathBuf,
    },
}

impl Exec {
    fn project_id(&self) -> &str {
        match self {
            Exec::Apply { job, .. } => &job.project.id,
            Exec::Export { project, .. } => &project.id,
            Exec::Flush { project, .. } => project,
        }
    }

    /// Run to completion (blocking; under the per-project lock inside).
    fn run(self) -> Result<CommandOutcome> {
        match self {
            Exec::Apply { job, verb } => {
                let applied = job.run()?;
                let (changed, vanished) = (applied.changed, applied.vanished);
                let project = job.project.id.clone();
                info!("apply complete [{project}]");
                // ADR-0042 F4/B12 — a client command must not report success for
                // anything unpersisted. The apply itself ran `Persistence::Flush`,
                // so this is normally a no-op; it is here for the case the apply
                // changed nothing (`changed == 0`) but the *project* was left
                // dirty by earlier producer-lane work. The guarantee is about the
                // project, not about this one apply.
                job.flusher.flush_project(&project)?;
                match verb {
                    ApplyVerb::Index => Ok(CommandOutcome::Indexed {
                        project,
                        changed,
                        vanished,
                    }),
                    ApplyVerb::Submit => Ok(CommandOutcome::Applied {
                        project,
                        changed,
                        vanished,
                    }),
                    ApplyVerb::WatchOn {
                        registry,
                        registry_path,
                    } => {
                        registry
                            .lock()
                            .set_watch(&project, true)
                            .map_err(Error::Other)?;
                        registry
                            .lock()
                            .save_to(&registry_path)
                            .map_err(|e| Error::Storage(format!("save registry: {e}")))?;
                        Ok(CommandOutcome::Watch {
                            project,
                            watching: true,
                            changed: Some(changed),
                            vanished: Some(vanished),
                            note: None,
                        })
                    }
                }
            }
            Exec::Export { project, flusher } => {
                let lock = flusher.lock_for(&project.id)?;
                let _guard = lock.lock();
                use filigrio_core::GraphStore;
                // `snapshot()` reads `state.json`, so under write-behind an
                // un-flushed project would export a *stale* graph.json while the
                // daemon serves something newer (ADR-0042 F2 + F4). Flush first,
                // under the lock we already hold.
                flusher.flush_locked(&project.id)?;
                filigrio_store::FsStore::new(&project.output_dir).snapshot()?;
                info!(
                    "Exported graph.json for {} → {}",
                    project.id,
                    project.output_dir.display()
                );
                Ok(CommandOutcome::Exported {
                    path: project.output_dir.join("graph.json").display().to_string(),
                    project: project.id,
                })
            }
            Exec::Flush { project, flusher } => {
                let wrote = flusher.flush_project(&project)?;
                if wrote {
                    info!("flushed {project} on request");
                }
                Ok(CommandOutcome::Flushed { project, wrote })
            }
        }
    }
}

/// Run a synchronous command's work, converting a **panic** into an ordinary
/// `Err` (ADR-0042 F6c, carried from the retired 0032g draft). This is the
/// scheduler's `catch_unwind` discipline applied to the sync path: without it a
/// panicking apply would unwind through the connection task and the client
/// would see a *dropped connection* — indistinguishable from a crashed daemon,
/// and never a non-zero exit with a reason.
///
/// A caught panic says the state may be partially applied and names the
/// converging verb, because that is the honest recovery instruction.
fn catch_command_panic<T>(project_id: &str, work: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_else(|_| {
        Err(Error::Other(format!(
            "command panicked while executing for '{project_id}' — state may be partially applied; run `project index` to converge"
        )))
    })
}

/// Resolve when `shutdown` is notified; if there is no notifier, never resolve
/// (so this `select!` branch is inert on the plain [`Daemon::run`] path).
async fn wait_for_notify(shutdown: &Option<Arc<Notify>>) {
    match shutdown {
        Some(n) => n.notified().await,
        None => std::future::pending::<()>().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_creation() {
        let config = DaemonConfig::default();
        let daemon = Daemon::new(config);

        assert_eq!(daemon.registry.lock().count(), 0);
        assert!(daemon.queue.is_empty());
    }

    /// ADR-0042 F6c — a **panicking** command becomes a typed error, never an
    /// unwind through the connection task (which the client sees as a dropped
    /// connection: indistinguishable from a crashed daemon, and never a
    /// non-zero exit with a reason). Drives the exact wrapper the sync command
    /// path uses, so this cannot be a parallel copy that drifts.
    #[test]
    fn a_panicking_command_becomes_a_typed_error_response() {
        // Keep the default hook's backtrace noise out of the test output while
        // still exercising a real unwind.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let caught: Result<()> =
            catch_command_panic("myproj", || panic!("extractor exploded mid-apply"));
        std::panic::set_hook(previous);

        let err = caught.expect_err("a panic must not surface as success");
        let msg = err.to_string();
        assert!(
            msg.contains("panicked"),
            "the error must say it panicked: {msg}"
        );
        assert!(
            msg.contains("myproj"),
            "the error must name the project: {msg}"
        );
        assert!(
            msg.contains("project index"),
            "the error must name the converging verb: {msg}"
        );

        // The wrapper is transparent for the ordinary paths.
        let ok = catch_command_panic("myproj", || Ok(7)).unwrap();
        assert_eq!(ok, 7);
        let err = catch_command_panic("myproj", || {
            Err::<(), _>(Error::Other("plain failure".into()))
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "other: plain failure");
    }
}
