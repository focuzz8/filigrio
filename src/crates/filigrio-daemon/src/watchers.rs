//! The set of live change producers, one per project (ADR-0032a §5, generalized
//! to the `Producer` taxonomy by ADR-0032e).
//!
//! Extracted from the daemon orchestrator so the producer lifecycle — start, stop,
//! and the routing drain — is one cohesive unit the daemon *delegates* to, rather
//! than lifecycle code woven through the run loop and command handlers. `WatcherSet`
//! owns the producers and their sinks; the daemon owns the priority queue the
//! drained items flow into (this module never touches the queue, and never
//! interprets `Produced` — that total match, `op_of`, lives with the daemon's `Op`).
//!
//! Which projects have a producer is now an explicit, persisted per-project
//! decision (`project watch on`, ADR-0042 F6b) — the daemon starts and stops
//! them; this set just holds whatever it is told to hold.

use crate::project::Project;
use crate::Result;
use filigrio_core::Priority;
use filigrio_ingest::{FsWatcher, Produced, Producer, WatcherConfig};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;
use tracing::warn;

/// One live producer plus the receiving end of its `Produced` channel. The set
/// holds the producer to keep its background machinery (e.g. a watcher's
/// debouncer thread) alive and to `stop()` it on remove/shutdown, and drains
/// `rx` each tick.
///
/// The receiver is wrapped in a `Mutex` purely to keep the owner `Sync`: a bare
/// `mpsc::Receiver` is `!Sync`, and the daemon is shared as `Arc<Mutex<Daemon>>`
/// across threads by the apply path. The lock is uncontended — only the
/// single-threaded drain tick ever touches it.
struct ProducerHandle {
    producer: Box<dyn Producer + Send + Sync>,
    rx: Mutex<Receiver<Produced>>,
}

/// Live change producers, one per project. Owns the lifecycle and the routing
/// drain; the daemon owns the queue the drained items are submitted into.
pub(crate) struct WatcherSet {
    producers: HashMap<String, ProducerHandle>,
    debounce: Duration,
}

impl WatcherSet {
    pub fn new(debounce: Duration) -> Self {
        Self {
            producers: HashMap::new(),
            debounce,
        }
    }

    /// Start one producer (today: an `FsWatcher`) for `project` (ADR-0032a §5).
    /// **Idempotent** — a project already watched is left untouched; a second
    /// `notify` watch would leak the first handle and double every event. A bind
    /// failure is returned so the caller can log it; best-effort per §4
    /// (startup/manual reconcile heals the gap).
    pub fn start(&mut self, project: &Project) -> Result<()> {
        if self.producers.contains_key(&project.id) {
            return Ok(());
        }
        let config = WatcherConfig {
            debounce: self.debounce,
        };
        let mut watcher = FsWatcher::new(project.root.clone(), project.id.clone(), config);
        let (tx, rx) = mpsc::channel();
        watcher.start(tx)?;
        self.producers.insert(
            project.id.clone(),
            ProducerHandle {
                producer: Box::new(watcher),
                rx: Mutex::new(rx),
            },
        );
        Ok(())
    }

    /// Stop and drop the producer for `project_id`, if any (ADR-0032a §5). Dropping
    /// the handle releases its resources (e.g. a watcher's `notify`/inotify handles
    /// and debounce thread) and removes the channel from the routing set so a
    /// removed project can no longer submit into the queue.
    pub fn stop(&mut self, project_id: &str) {
        if let Some(mut handle) = self.producers.remove(project_id) {
            handle.producer.stop();
        }
    }

    /// Start a producer for each project (startup path). A single project's failure
    /// is logged and skipped — one un-watchable repo must not keep the daemon from
    /// watching the others.
    pub fn start_all(&mut self, projects: &[Project]) {
        for project in projects {
            if let Err(e) = self.start(project) {
                warn!("failed to start watcher for {} (skipped): {e}", project.id);
            }
        }
    }

    /// Stop every producer (shutdown path).
    pub fn stop_all(&mut self) {
        for (_id, mut handle) in self.producers.drain() {
            handle.producer.stop();
        }
    }

    /// Drain every producer's channel, returning each `Produced` item paired with
    /// the owning project id and the producer's self-declared lane (ADR-0032a §1;
    /// ADR-0032e §1/§2 — the lane comes off the producer, not a daemon-side table).
    /// Non-blocking `try_recv` so a quiet producer never stalls the tick; the caller
    /// (the daemon) maps each item through `op_of` into a daemon-internal
    /// `QueueItem` and submits it into the priority queue (ADR-0042 F6b: the
    /// queue element is `(project, Op, lane)`, never the wire `Command`).
    /// The channel mutex is a `parking_lot::Mutex` (no poisoning),
    /// so a panic under it can't wedge the drain (liveness path, no correctness
    /// invariant — the module's "one bad thing never crashes the daemon"
    /// discipline); a disconnected channel simply yields nothing.
    pub fn drain(&self) -> Vec<(String, Produced, Priority)> {
        let mut items = Vec::new();
        for (id, handle) in &self.producers {
            let rx = handle.rx.lock();
            while let Ok(produced) = rx.try_recv() {
                items.push((id.clone(), produced, handle.producer.priority()));
            }
        }
        items
    }

    /// Whether a live producer exists for `project_id`.
    pub fn is_watching(&self, project_id: &str) -> bool {
        self.producers.contains_key(project_id)
    }

    /// Number of live producers.
    pub fn count(&self) -> usize {
        self.producers.len()
    }

    /// Insert a pre-built producer + channel directly, so a test can hold the
    /// sender and drive the routing drain without real `notify` events (or any
    /// other producer's real background machinery).
    #[cfg(test)]
    pub fn insert_for_test(
        &mut self,
        project_id: String,
        producer: Box<dyn Producer + Send + Sync>,
        rx: Receiver<Produced>,
    ) {
        self.producers.insert(
            project_id,
            ProducerHandle {
                producer,
                rx: Mutex::new(rx),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filigrio_core::ChangeSet;
    use std::sync::mpsc::Sender;
    use tempfile::TempDir;

    /// A minimal test-only producer: no background machinery, just declares its
    /// lane. `insert_for_test` pairs it with a channel the test drives directly.
    struct StubProducer(Priority);
    impl Producer for StubProducer {
        fn priority(&self) -> Priority {
            self.0
        }
        fn start(&mut self, _sink: Sender<Produced>) -> filigrio_core::Result<()> {
            Ok(())
        }
        fn stop(&mut self) {}
    }

    /// F1/F4 — start records exactly one watcher and is idempotent. Editing a file
    /// does nothing unless a watcher was started; a second start for the same
    /// project must not leak a duplicate `notify` watch (double events).
    #[test]
    fn start_records_one_and_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let project = Project::new(temp.path().to_path_buf());
        let mut set = WatcherSet::new(Duration::from_millis(50));

        set.start(&project).unwrap();
        assert!(
            set.is_watching(&project.id),
            "start must record the watcher"
        );
        assert_eq!(set.count(), 1);

        set.start(&project).unwrap();
        assert_eq!(set.count(), 1, "a second start must not add a duplicate");
    }

    /// F1/F5 — startup watches every project and shutdown releases every watcher.
    #[test]
    fn start_all_then_stop_all() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let pa = Project::new(a.path().to_path_buf());
        let pb = Project::new(b.path().to_path_buf());
        let mut set = WatcherSet::new(Duration::from_millis(50));

        set.start_all(&[pa.clone(), pb.clone()]);
        assert_eq!(set.count(), 2);
        assert!(set.is_watching(&pa.id) && set.is_watching(&pb.id));

        set.stop_all();
        assert_eq!(set.count(), 0, "shutdown must stop every watcher");
    }

    /// F2 (load-bearing) — a producer's output is surfaced by the drain, paired
    /// with its project id and its self-declared priority. This is the signal
    /// path that makes a live edit reach the queue; if it breaks, edits are
    /// silently dropped.
    #[test]
    fn drain_yields_producer_output_with_project_and_priority() {
        let (tx, rx) = mpsc::channel();
        let mut set = WatcherSet::new(Duration::from_millis(50));
        set.insert_for_test("proj".to_string(), Box::new(StubProducer(Priority::Fs)), rx);

        tx.send(Produced::Signal(ChangeSet::default())).unwrap();
        let drained = set.drain();
        assert_eq!(drained.len(), 1, "the producer's output must be drained");
        assert_eq!(drained[0].0, "proj");
        assert_eq!(drained[0].2, Priority::Fs);
        assert!(matches!(drained[0].1, Produced::Signal(_)));
    }

    /// F3 — after `stop`, the producer can no longer route: its channel is gone, so
    /// a removed project can't keep feeding the queue.
    #[test]
    fn stop_removes_the_routing_source() {
        let (tx, rx) = mpsc::channel();
        let mut set = WatcherSet::new(Duration::from_millis(50));
        set.insert_for_test("proj".to_string(), Box::new(StubProducer(Priority::Fs)), rx);

        set.stop("proj");
        assert!(
            !set.is_watching("proj"),
            "stop must drop the producer handle"
        );

        // The receiver was dropped with the handle, so this send fails and nothing routes.
        let _ = tx.send(Produced::Signal(ChangeSet::default()));
        assert!(set.drain().is_empty(), "no item may route after stop");
    }
}
