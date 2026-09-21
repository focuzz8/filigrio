//! The apply worker pool (ADR-0032 §2 drain-level parallelism).
//!
//! The run loop hands [`ApplyJob`](crate::apply::ApplyJob)s here; the scheduler
//! runs them on `spawn_blocking` tasks (applies are synchronous CPU work) under
//! two bounds:
//!
//! 1. **Pool bound** — at most `permits` applies run at once, so a churning
//!    monorepo can't exhaust the machine.
//! 2. **One-per-project** — never two in-flight applies for the *same* project.
//!    The per-project lock already makes same-project applies *correct*; this
//!    bound stops a same-project flood from parking a whole pool of threads all
//!    blocked on that one lock (which would starve other projects).
//!
//! Jobs that don't fit either bound are returned as *deferred* for the caller to
//! retry when a slot frees (a completion clears the project's in-flight mark).

use crate::apply::ApplyJob;
use crate::Result;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::task::JoinSet;
use tracing::error;

/// Shared, lock-free snapshot of the pool's pending work, so `Daemon::health`
/// (which runs on `&Daemon`, off the run loop) can report what the worker pool is
/// doing. Held behind an `Arc`: the run loop writes it each iteration, `health`
/// reads it. Without this, `queue_depth` reads 0 while the pool churns and the
/// daemon looks idle when it isn't (ADR-0032 §2 telemetry).
#[derive(Debug, Default)]
pub(crate) struct PoolStats {
    inflight: AtomicUsize,
    deferred: AtomicUsize,
}

impl PoolStats {
    /// Publish the current pool state (called by the run loop each iteration).
    pub fn record(&self, inflight: usize, deferred: usize) {
        self.inflight.store(inflight, Ordering::Relaxed);
        self.deferred.store(deferred, Ordering::Relaxed);
    }

    /// `(inflight, deferred)` for a `Health` response.
    pub fn snapshot(&self) -> (usize, usize) {
        (
            self.inflight.load(Ordering::Relaxed),
            self.deferred.load(Ordering::Relaxed),
        )
    }
}

/// Bounded, one-per-project dispatcher for apply jobs.
pub(crate) struct ApplyScheduler {
    permits: usize,
    inflight: HashSet<String>,
    /// Each task yields its project id and, on success, whether it left the
    /// project owing a recluster ([`ApplyJob::owes_recluster`]).
    tasks: JoinSet<(String, Result<bool>)>,
}

impl ApplyScheduler {
    pub fn new(permits: usize) -> Self {
        ApplyScheduler {
            permits: permits.max(1),
            inflight: HashSet::new(),
            tasks: JoinSet::new(),
        }
    }

    /// Number of applies currently running (published to `Health` telemetry via
    /// [`PoolStats`], and asserted by the scheduler tests).
    pub fn inflight_count(&self) -> usize {
        self.inflight.len()
    }

    /// No applies in flight.
    pub fn is_idle(&self) -> bool {
        self.inflight.is_empty()
    }

    /// Whether `project` has a job running right now.
    pub fn is_running(&self, project: &str) -> bool {
        self.inflight.contains(project)
    }

    /// Dispatch as many jobs as the pool bound and the one-per-project rule allow;
    /// return the rest (deferred).
    pub fn dispatch(&mut self, jobs: Vec<ApplyJob>) -> Vec<ApplyJob> {
        let mut deferred = Vec::new();
        for job in jobs {
            let id = job.project.id.clone();
            // Defer if the pool is full or this project already has an apply
            // running (one-per-project). Deferred jobs are retried by the caller
            // when a completion frees a slot / clears the project.
            if self.inflight.len() >= self.permits || self.inflight.contains(&id) {
                deferred.push(job);
                continue;
            }
            self.inflight.insert(id.clone());
            // Applies are synchronous CPU work → `spawn_blocking` (not an async
            // task, which would park a runtime worker). `catch_unwind` keeps a
            // panicking apply from aborting the JoinSet task and leaking the
            // in-flight slot — it becomes an `Err` carrying the project id.
            self.tasks.spawn_blocking(move || {
                // Producer-lane outcomes are logged, not delivered — there is no
                // client on this lane (a watcher event has no caller waiting);
                // the changed count is dropped here on purpose (ADR-0042 F6c).
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    job.run().map(|applied| job.owes_recluster(&applied))
                }))
                .unwrap_or_else(|_| Err(crate::Error::Other(format!("apply panicked for {id}"))));
                (id, res)
            });
        }
        deferred
    }

    /// Await the next completed apply, clearing its in-flight slot. Resolves to
    /// `None` when nothing is in flight (callers should guard with `is_idle`).
    pub async fn join_next(&mut self) -> Option<(String, Result<bool>)> {
        match self.tasks.join_next().await {
            Some(Ok((id, res))) => {
                self.inflight.remove(&id);
                Some((id, res))
            }
            // A JoinError only occurs on task cancellation (we never abort here)
            // — `catch_unwind` above converts panics into `Err` results instead.
            Some(Err(e)) => {
                error!("apply task join error: {e}");
                None
            }
            None => None,
        }
    }

    /// Await every in-flight apply (shutdown drain), returning the projects the
    /// drained jobs left owing a recluster.
    pub async fn drain(&mut self) -> Vec<String> {
        let mut owing = Vec::new();
        while let Some((id, res)) = self.join_next().await {
            match res {
                Ok(true) => owing.push(id),
                Ok(false) => {}
                Err(e) => error!("apply failed during shutdown drain [{id}]: {e}"),
            }
        }
        owing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::ApplyJob;
    use crate::cache::ProjectStateCache;
    use crate::flush::{FlushConfig, Flusher, Persistence, SystemClock};
    use crate::locks::ProjectLocks;
    use crate::project::Project;
    use filigrio_core::ChangeSet;
    use parking_lot::Mutex;
    use std::path::Path;
    use std::sync::Arc;

    fn cache() -> Arc<Mutex<ProjectStateCache>> {
        Arc::new(Mutex::new(ProjectStateCache::new(16)))
    }

    /// The pool's jobs are producer-lane jobs, so they run the ADR-0042 F4
    /// write-behind cadence; these tests observe *scheduling*, not persistence.
    fn flusher(cache: &Arc<Mutex<ProjectStateCache>>, locks: &ProjectLocks) -> Arc<Flusher> {
        Arc::new(Flusher::new(
            cache.clone(),
            locks.clone(),
            FlushConfig::default(),
            Arc::new(SystemClock),
        ))
    }

    /// A real apply job against a tiny on-disk project (id = `name`). Held-lock
    /// gating lets a test freeze the job mid-flight to observe scheduler state.
    fn job(
        base: &Path,
        name: &str,
        locks: &ProjectLocks,
        cache: &Arc<Mutex<ProjectStateCache>>,
    ) -> ApplyJob {
        let root = base.join(name);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        ApplyJob {
            project: Project::new(root),
            op: crate::apply::Op::Apply(ChangeSet {
                added: vec!["src/main.rs".into()],
                modified: vec![],
                removed: vec![],
            }),
            cluster_cfg: filigrio_pipeline::ClusterConfig::default(),
            persistence: Persistence::Defer,
            flusher: flusher(cache, locks),
        }
    }

    /// W4 — the pool bound is respected: with 2 permits and 4 different-project
    /// jobs (all frozen on held locks), exactly 2 dispatch and 2 defer. Releasing
    /// the locks lets the dispatched pair finish, and the deferred pair then runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w4_dispatch_bounds_to_permits() {
        let tmp = tempfile::TempDir::new().unwrap();
        let locks = ProjectLocks::new();
        let cache = cache();
        let names = ["p0", "p1", "p2", "p3"];

        // Freeze every project's apply by pre-holding its per-project lock.
        let guards: Vec<_> = names.iter().map(|n| locks.lock_for(n).unwrap()).collect();
        let held: Vec<_> = guards.iter().map(|g| g.lock()).collect();

        let jobs: Vec<_> = names
            .iter()
            .map(|n| job(tmp.path(), n, &locks, &cache))
            .collect();

        let mut sched = ApplyScheduler::new(2);
        let deferred = sched.dispatch(jobs);

        assert_eq!(sched.inflight_count(), 2, "pool bound of 2 not respected");
        assert_eq!(deferred.len(), 2, "the 2 over-bound jobs must be deferred");

        // Release → the 2 dispatched applies finish; the 2 deferred then run.
        drop(held);
        assert_eq!(sched.join_next().await.map(|(_, r)| r.is_ok()), Some(true));
        assert_eq!(sched.join_next().await.map(|(_, r)| r.is_ok()), Some(true));
        assert!(sched.is_idle());

        let deferred = sched.dispatch(deferred);
        assert!(
            deferred.is_empty(),
            "freed slots must accept the deferred jobs"
        );
        assert_eq!(sched.inflight_count(), 2);
        sched.drain().await;
        assert!(sched.is_idle());
    }

    /// W3 — one apply per project: 3 jobs for the SAME project, ample permits, but
    /// only one dispatches (the other two defer). The per-project lock makes
    /// same-project applies correct; this bound stops them parking pool threads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w3_at_most_one_inflight_per_project() {
        let tmp = tempfile::TempDir::new().unwrap();
        let locks = ProjectLocks::new();
        let cache = cache();

        let lock = locks.lock_for("solo").unwrap();
        let held = lock.lock();

        let jobs: Vec<_> = (0..3)
            .map(|_| job(tmp.path(), "solo", &locks, &cache))
            .collect();
        let mut sched = ApplyScheduler::new(4);
        let deferred = sched.dispatch(jobs);

        assert_eq!(
            sched.inflight_count(),
            1,
            "same project must not run concurrently"
        );
        assert_eq!(
            deferred.len(),
            2,
            "extra same-project jobs must defer, not drop"
        );

        drop(held);
        sched.drain().await;
        assert!(sched.is_idle());
    }

    /// W5 — a completion clears the project's in-flight slot, so its backlog can
    /// dispatch next. (No held lock → the apply runs to completion immediately.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w5_completion_clears_inflight() {
        let tmp = tempfile::TempDir::new().unwrap();
        let locks = ProjectLocks::new();
        let cache = cache();

        let mut sched = ApplyScheduler::new(2);
        let deferred = sched.dispatch(vec![job(tmp.path(), "p", &locks, &cache)]);
        assert!(deferred.is_empty());
        assert_eq!(sched.inflight_count(), 1);

        let done = sched.join_next().await.expect("the apply should complete");
        assert!(done.1.is_ok(), "apply failed: {:?}", done.1);
        assert_eq!(
            sched.inflight_count(),
            0,
            "completion must clear the in-flight slot"
        );
        assert!(
            sched.join_next().await.is_none(),
            "idle scheduler yields None"
        );
    }
}
