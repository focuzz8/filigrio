//! Persistence cadence — the write-behind flusher (ADR-0042 Phase 1c F4, B12).
//!
//! > **B12 — Persistence is a cadence policy, not a side effect of apply.**
//! > Today every apply writes the store synchronously. That coupling dissolves;
//! > when the store is written is decided by mode.
//!
//! Before F4 the daemon's apply was `lock → prior → Pipeline::apply (which wrote
//! `state.json` inside `FsStore::apply_delta`) → **re-read `state.json`** to
//! refresh the cache`. Two full parses and a full write per apply, of a state the
//! daemon already held resident. F4 replaces the store on the apply path with
//! [`filigrio_store::DeferredStore`], which merges in memory and hands the result
//! back, and moves the write here, where it becomes a *schedule*:
//!
//! | lane | who is waiting | cadence |
//! |---|---|---|
//! | client ([`Persistence::Flush`]) | a wire command / a one-shot about to exit | write before responding |
//! | producer ([`Persistence::Defer`]) | nobody — a watcher event | quiescence, max-dirty-age cap, shutdown, dirty eviction |
//!
//! ## Durability contract (stated here because this is where it is decided)
//!
//! The graph is a **derived index**; the repository is the source of truth. A
//! crash therefore loses at most the un-flushed window, and a restart reconciles
//! that away — the cost is redoing work, never a wrong answer. What write-behind
//! must *never* produce is a corrupt or half-applied on-disk state: every flush
//! writes one whole, internally consistent `GraphState` through
//! [`FsStore::save_state`], which is temp+rename (ADR-0042 F1). There is no
//! partial-write window to widen.
//!
//! ## Honesty (ADR-0029 applied to persistence)
//!
//! In-daemon queries are unaffected — they are served from resident state. But
//! an **out-of-process** reader of `.filigrio-out` may now lag by up to the
//! quiescence window, so [`Flusher::dirty_for`] / [`Flusher::since_persisted`]
//! feed `ProjectStatus`, and staleness is visible rather than implicit.

use crate::cache::{Evicted, ProjectStateCache};
use crate::locks::ProjectLocks;
use crate::project::Project;
use crate::{Error, Result};
use filigrio_core::{GraphState, GraphStore};
use filigrio_store::FsStore;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// The daemon's clock, injectable so the B12 timers are testable without
/// sleeping. Everything that decides *when* to flush reads time through this.
pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now(&self) -> Instant;
}

/// Real time.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A clock a test advances by hand. `Instant` has no public constructor, so
/// "now" is a fixed base plus an offset the test controls — monotonic by
/// construction, and never tied to wall-clock progress.
#[derive(Debug)]
pub struct TestClock {
    base: Instant,
    offset: Mutex<Duration>,
}

impl TestClock {
    pub fn new() -> Self {
        TestClock {
            base: Instant::now(),
            offset: Mutex::new(Duration::ZERO),
        }
    }

    /// Move the clock forward. Time never goes backwards.
    pub fn advance(&self, by: Duration) {
        let mut o = self.offset.lock();
        *o += by;
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TestClock {
    fn now(&self) -> Instant {
        self.base + *self.offset.lock()
    }
}

/// The B12 write-behind cadence knobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlushConfig {
    /// Persist a project after this long with **no apply to it**. The steady
    /// state of watch-mode editing: a save-storm coalesces into one write.
    pub quiescence: Duration,
    /// Hard bound on how long a project may stay dirty, measured from when it
    /// *first* went dirty. Without it, a project edited more often than the
    /// quiescence window would never reach quiescence and so never be
    /// persisted — sustained churn must not be able to defer persistence
    /// forever, because the un-flushed window IS the crash window.
    pub max_dirty_age: Duration,
}

impl Default for FlushConfig {
    /// The B12 defaults: 30 s quiescence, 300 s cap.
    fn default() -> Self {
        FlushConfig {
            quiescence: Duration::from_secs(30),
            max_dirty_age: Duration::from_secs(300),
        }
    }
}

/// Which cadence an apply's result is persisted on (B12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Persistence {
    /// **Client lane** — a wire command or a one-shot: someone is waiting for
    /// an outcome, and CI reads the exit code as "it is on disk". Write now.
    Flush,
    /// **Producer lane** — the watcher: nobody is waiting. Update resident
    /// state, mark dirty, let the cadence decide.
    Defer,
}

/// When a project went dirty, and when it was last touched. Both are needed:
/// `last_apply` drives quiescence, `first_dirty` drives the cap.
#[derive(Clone, Copy, Debug)]
struct Dirty {
    first_dirty: Instant,
    last_apply: Instant,
}

/// Owns the resident state and decides when it reaches disk (B12).
///
/// It holds the LRU cache and the per-project lock table because both are part
/// of that decision: a flush must take the project's lock (so it serializes with
/// applies and can never read a half-applied state), and a *dirty eviction* is a
/// flush trigger the cache alone cannot service — the cache is deliberately
/// filesystem-free and hands the evicted state back for someone else to write.
/// That someone is this type; before F4 it was a `warn!` and a drop.
pub struct Flusher {
    cache: Arc<Mutex<ProjectStateCache>>,
    locks: ProjectLocks,
    clock: Arc<dyn Clock>,
    cfg: FlushConfig,
    /// Projects with resident state newer than their checkpoint.
    dirty: Mutex<HashMap<String, Dirty>>,
    /// Last time *this daemon lifetime* persisted each project. `None` for a
    /// project whose checkpoint predates the process — reported as such rather
    /// than fabricated.
    persisted: Mutex<HashMap<String, Instant>>,
    /// id → `.filigrio-out`, learned from every apply. A dirty eviction arrives
    /// as a bare id, so without this the write-back would have nowhere to go
    /// (the reason the old `flush_evicted` said "no write-back path").
    out_dirs: Mutex<HashMap<String, PathBuf>>,
    /// Store writes performed. Telemetry, and the counted quantity the F4
    /// burst-coalescing gate and the perf ledger assert on.
    writes: AtomicUsize,
    /// Set when the background driver should stop.
    stopping: Arc<AtomicBool>,
    driver: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Flusher {
    pub fn new(
        cache: Arc<Mutex<ProjectStateCache>>,
        locks: ProjectLocks,
        cfg: FlushConfig,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Flusher {
            cache,
            locks,
            clock,
            cfg,
            dirty: Mutex::new(HashMap::new()),
            persisted: Mutex::new(HashMap::new()),
            out_dirs: Mutex::new(HashMap::new()),
            writes: AtomicUsize::new(0),
            stopping: Arc::new(AtomicBool::new(false)),
            driver: Mutex::new(None),
        }
    }

    /// The shared LRU cache (the responder's warm state source reads it).
    pub fn cache(&self) -> &Arc<Mutex<ProjectStateCache>> {
        &self.cache
    }

    /// The per-project lock for `id` — the same table the apply path uses.
    pub fn lock_for(&self, id: &str) -> Result<Arc<Mutex<()>>> {
        self.locks.lock_for(id)
    }

    // ---- reads --------------------------------------------------------

    /// Does this project have resident state newer than its checkpoint?
    pub fn is_dirty(&self, id: &str) -> bool {
        self.dirty.lock().contains_key(id)
    }

    /// How long this project has been dirty, i.e. the upper bound on how stale
    /// an out-of-process reader of `.filigrio-out` may be. `None` = clean.
    pub fn dirty_for(&self, id: &str) -> Option<Duration> {
        let now = self.clock.now();
        self.dirty
            .lock()
            .get(id)
            .map(|d| now.saturating_duration_since(d.first_dirty))
    }

    /// How long ago this daemon last persisted the project. `None` means it has
    /// not persisted it at all in this lifetime — the on-disk checkpoint (if
    /// any) predates the process, which is a different statement from "just
    /// written" and must not be reported as zero.
    pub fn since_persisted(&self, id: &str) -> Option<Duration> {
        let now = self.clock.now();
        self.persisted
            .lock()
            .get(id)
            .map(|t| now.saturating_duration_since(*t))
    }

    /// Store writes performed by this flusher.
    pub fn writes_total(&self) -> usize {
        self.writes.load(Ordering::Relaxed)
    }

    // ---- the apply seam ------------------------------------------------

    /// Read a project's full state, preferring the resident LRU cache and paging
    /// in from the `state.json` checkpoint on a miss (ADR-0032 §2). The manifest
    /// stays hot even after the full graph is evicted.
    ///
    /// Safe under write-behind because **dirty ⇒ resident**: the only way a
    /// dirty project leaves the cache is eviction, and eviction writes it back
    /// first, so a cache miss can never read a checkpoint that is older than
    /// state the daemon still believes in.
    pub fn state_of(&self, project: &Project) -> Result<Arc<GraphState>> {
        self.remember(project);
        // A hot hit is an `Arc` clone — a refcount bump under the cache mutex, NOT an
        // O(nodes+edges) deep copy. That keeps the lock hold O(1) so concurrent applies
        // to different projects don't serialize on each other's prior-clone at scale.
        if let Some(state) = self.cache.lock().get_arc(&project.id) {
            return Ok(state);
        }
        let store = FsStore::new(&project.output_dir);
        let state = store.load_state()?.unwrap_or_default();
        let (arc, evicted) = {
            let mut c = self.cache.lock();
            let evicted = c.load_into(&project.id, state);
            // Re-fetch as the shared Arc the cache now owns (no second deep copy).
            (c.get_arc(&project.id), evicted)
        };
        self.flush_evicted(evicted)?;
        arc.ok_or_else(|| Error::Other(format!("state for {} vanished after load", project.id)))
    }

    /// Record the result of one apply on the given cadence (B12). Called with
    /// the project's lock already held by the apply path.
    ///
    /// `Flush` writes through and the cache entry is clean; `Defer` only updates
    /// the resident copy and starts (or extends) the dirty window.
    pub fn record_apply(
        &self,
        project: &Project,
        fresh: GraphState,
        persistence: Persistence,
    ) -> Result<()> {
        self.remember(project);
        match persistence {
            Persistence::Flush => {
                self.write_state(&project.id, &project.output_dir, &fresh)?;
                // `load_into` = clean: disk was just updated, so the cached copy
                // matches it.
                let evicted = self.cache.lock().load_into(&project.id, fresh);
                self.dirty.lock().remove(&project.id);
                self.flush_evicted(evicted)
            }
            Persistence::Defer => {
                let now = self.clock.now();
                // `put` = dirty: resident state now differs from the checkpoint.
                let evicted = self.cache.lock().put(&project.id, fresh);
                {
                    let mut dirty = self.dirty.lock();
                    let entry = dirty.entry(project.id.clone()).or_insert(Dirty {
                        first_dirty: now,
                        last_apply: now,
                    });
                    entry.last_apply = now;
                }
                self.flush_evicted(evicted)
            }
        }
    }

    // ---- the cadence ---------------------------------------------------

    /// Flush one project if it is dirty, taking its per-project lock so the
    /// write serializes with applies. `Ok(false)` = already clean (an honest
    /// no-op: a 210 MB rewrite for nothing is exactly what F4 exists to avoid).
    pub fn flush_project(&self, id: &str) -> Result<bool> {
        if !self.is_dirty(id) {
            return Ok(false);
        }
        let lock = self.locks.lock_for(id)?;
        let _guard = lock.lock();
        self.flush_locked(id)
    }

    /// Flush `id` with its per-project lock **already held** by the caller (the
    /// apply path, or the export verb which must not read a stale checkpoint).
    pub fn flush_locked(&self, id: &str) -> Result<bool> {
        if !self.is_dirty(id) {
            return Ok(false);
        }
        let Some(dir) = self.out_dirs.lock().get(id).cloned() else {
            return Err(Error::Storage(format!(
                "no output directory known for dirty project '{id}' — cannot flush"
            )));
        };
        // `peek_arc`: reading a project in order to persist it is not a use of
        // it, so it must not refresh LRU recency.
        let Some(state) = self.cache.lock().peek_arc(id) else {
            // dirty ⇒ resident is an invariant; if it is ever violated the
            // honest response is to say so, not to silently forget the flag.
            self.dirty.lock().remove(id);
            return Err(Error::Storage(format!(
                "project '{id}' was marked dirty but is not resident; its unpersisted state is gone"
            )));
        };
        self.write_state(id, &dir, &state)?;
        self.cache.lock().mark_clean(id);
        self.dirty.lock().remove(id);
        Ok(true)
    }

    /// Every project the B12 triggers say is due **now**: quiescent for longer
    /// than the window, or dirty for longer than the cap.
    pub fn due(&self) -> Vec<String> {
        let now = self.clock.now();
        let dirty = self.dirty.lock();
        dirty
            .iter()
            .filter(|(_, d)| {
                now.saturating_duration_since(d.last_apply) >= self.cfg.quiescence
                    || now.saturating_duration_since(d.first_dirty) >= self.cfg.max_dirty_age
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Flush everything currently due. Returns the failures (a project whose
    /// write failed stays dirty and will be retried) rather than swallowing
    /// them; the background driver logs them.
    pub fn flush_due(&self) -> Vec<(String, Error)> {
        self.flush_each(self.due())
    }

    /// Flush **every** dirty project — the graceful-shutdown trigger.
    pub fn flush_all(&self) -> Vec<(String, Error)> {
        let ids: Vec<String> = self.dirty.lock().keys().cloned().collect();
        self.flush_each(ids)
    }

    fn flush_each(&self, ids: Vec<String>) -> Vec<(String, Error)> {
        let mut failures = Vec::new();
        for id in ids {
            match self.flush_project(&id) {
                Ok(true) => info!("flushed {id}"),
                Ok(false) => {}
                Err(e) => failures.push((id, e)),
            }
        }
        failures
    }

    // ---- the background driver -----------------------------------------

    /// Start the cadence driver: a plain OS thread that asks [`Flusher::due`]
    /// every `poll` and flushes what it finds.
    ///
    /// A thread rather than a branch of the daemon's `select!` loop, for two
    /// reasons: a flush is blocking CPU+IO work (hundreds of ms at scale) that
    /// must not park a runtime worker or — critically — be reached through the
    /// **daemon** mutex, and the policy stays a plain synchronous function that
    /// a test can call directly with a fake clock. The poll interval is only
    /// granularity; the triggers themselves are read from the injected
    /// [`Clock`], which is why no test here sleeps.
    pub fn start_driver(self: &Arc<Self>, poll: Duration) {
        let mut slot = self.driver.lock();
        if slot.is_some() {
            return;
        }
        // A **weak** handle: a strong one would make the driver keep its own
        // flusher alive forever, so `Drop` (which is what stops an un-stopped
        // driver) could never run.
        let flusher = Arc::downgrade(self);
        let stopping = Arc::clone(&self.stopping);
        stopping.store(false, Ordering::SeqCst);
        *slot = Some(std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                std::thread::sleep(poll);
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                let Some(flusher) = flusher.upgrade() else {
                    break; // the daemon is gone
                };
                for (id, e) in flusher.flush_due() {
                    warn!("scheduled flush failed for {id}: {e}");
                }
            }
        }));
    }

    /// Stop the cadence driver and join it. Idempotent.
    pub fn stop_driver(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let handle = self.driver.lock().take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }

    // ---- internals -----------------------------------------------------

    /// Learn (or refresh) where a project's checkpoint lives, so an eviction —
    /// which arrives as a bare id — can be written back.
    fn remember(&self, project: &Project) {
        let mut dirs = self.out_dirs.lock();
        if dirs.get(&project.id) != Some(&project.output_dir) {
            dirs.insert(project.id.clone(), project.output_dir.clone());
        }
    }

    /// The one place a checkpoint is written. `FsStore::save_state` is
    /// temp+rename (ADR-0042 F1), so the file a reader sees is always a whole,
    /// internally consistent state — the write-behind window can lose recency,
    /// never integrity.
    fn write_state(&self, id: &str, dir: &std::path::Path, state: &GraphState) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        FsStore::new(dir).save_state(state)?;
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.persisted
            .lock()
            .insert(id.to_string(), self.clock.now());
        Ok(())
    }

    /// Write back state the LRU paged out (B12's fourth trigger). Before F4 this
    /// logged "no write-back path" and dropped the state, which was survivable
    /// only because nothing was ever dirty.
    ///
    /// It deliberately does **not** take the evicted project's lock: eviction
    /// happens while the caller already holds a *different* project's lock, and
    /// acquiring a second would make an A→B / B→A deadlock reachable. That is
    /// safe here because the write is a whole-state temp+rename, so a concurrent
    /// apply to the evicted project cannot tear it; the worst case is that the
    /// concurrent apply's own (newer) result lands afterwards — or, if this
    /// write lands last, that its result stays dirty in the cache and is flushed
    /// on the next trigger. Neither loses integrity.
    fn flush_evicted(&self, evicted: Vec<Evicted>) -> Result<()> {
        for e in evicted {
            self.dirty.lock().remove(&e.id);
            let Some(state) = e.state else {
                continue; // clean — already on disk, no rewrite
            };
            let Some(dir) = self.out_dirs.lock().get(&e.id).cloned() else {
                return Err(Error::Storage(format!(
                    "cache paged out dirty state for '{}' but its output directory is unknown; \
                     refusing to drop unpersisted work silently",
                    e.id
                )));
            };
            info!("cache paged out dirty state for '{}'; writing back", e.id);
            self.write_state(&e.id, &dir, &state)?;
        }
        Ok(())
    }
}

impl Drop for Flusher {
    fn drop(&mut self) {
        // Never leave the driver thread behind holding an `Arc` to a flusher
        // whose daemon is gone.
        self.stopping.store(true, Ordering::SeqCst);
    }
}
