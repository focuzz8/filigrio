//! Priority queue implementation (ADR-0032 §4).
//!
//! Three priority lanes: manual > git-changeset > fs-event, FIFO within a lane
//! (a `VecDeque` per lane).
//!
//! The element is the daemon-internal [`QueueItem`] — `(project, Op, lane)` —
//! not the wire `Command` (ADR-0042 F6b): since F6c, wire commands never enter
//! the queue at all (they execute synchronously at ingress), so the queue is
//! purely the **producer lane** (watcher events, internal `Op` items). The lane
//! is explicit on the item, carried from the producer's self-declared priority;
//! FIFO ordering within a lane is unchanged from the `Command`-typed queue.
//! There is still no queue-level dedup — redundant items are absorbed
//! downstream by the two-stage mtime/hash gate at apply time (ADR-0032 §4),
//! exactly as before.

use crate::apply::Op;
use crate::Priority;
use std::collections::VecDeque;

/// A daemon-internal unit of queued work: which project, what to do, and the
/// lane it rides (the producer's self-declared priority, ADR-0032e).
#[derive(Clone, Debug)]
pub struct QueueItem {
    /// Registry project id (producers are keyed by id, not path).
    pub project: String,
    /// What to do — the R2.5 authority axis rides here (ADR-0032e §2), and so
    /// does reconcile depth (ADR-0042 F6b: depth is Op-internal, not wire).
    pub op: Op,
    /// Which lane this item rides (manual > git > fs).
    pub lane: Priority,
}

/// A priority queue with three lanes.
#[derive(Debug, Default)]
pub struct PriorityQueue {
    /// Manual lane (highest priority).
    manual: VecDeque<QueueItem>,
    /// Git changeset lane (high priority).
    git: VecDeque<QueueItem>,
    /// Filesystem event lane (low priority).
    fs: VecDeque<QueueItem>,
}

impl PriorityQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Submit an item into its self-declared lane.
    pub fn submit(&mut self, item: QueueItem) {
        match item.lane {
            Priority::Manual => self.manual.push_back(item),
            Priority::Git => self.git.push_back(item),
            Priority::Fs => self.fs.push_back(item),
        }
    }

    /// Whether any lane holds an item for `project`.
    pub fn has_project(&self, project: &str) -> bool {
        self.manual
            .iter()
            .chain(&self.git)
            .chain(&self.fs)
            .any(|item| item.project == project)
    }

    /// Pop the highest-priority item.
    /// Returns None if all lanes are empty.
    pub fn pop(&mut self) -> Option<QueueItem> {
        if !self.manual.is_empty() {
            self.manual.pop_front()
        } else if !self.git.is_empty() {
            self.git.pop_front()
        } else {
            self.fs.pop_front()
        }
    }

    /// Get the total number of queued items.
    pub fn len(&self) -> usize {
        self.manual.len() + self.git.len() + self.fs.len()
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the depth of each priority lane.
    pub fn lane_depths(&self) -> (usize, usize, usize) {
        (self.manual.len(), self.git.len(), self.fs.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filigrio_core::ChangeSet;

    fn item(project: &str, op: Op, lane: Priority) -> QueueItem {
        QueueItem {
            project: project.to_string(),
            op,
            lane,
        }
    }

    /// Lane ordering is keyed on the item's explicit `lane` (the producer's
    /// self-declared priority — before the re-type it was derived from the
    /// `Command` variant): manual > git > fs, regardless of submit order.
    #[test]
    fn test_priority_ordering() {
        let mut queue = PriorityQueue::new();

        // Submit low-priority first (the watcher's signal lane).
        queue.submit(item("test", Op::Apply(ChangeSet::default()), Priority::Fs));

        // Submit high-priority (a git producer's lane).
        queue.submit(item(
            "test",
            Op::ApplyExact(ChangeSet::default()),
            Priority::Git,
        ));

        // Submit manual (highest) — a deep reconcile is the specimen
        // manual-lane item.
        queue.submit(item("test", Op::Reconcile { deep: true }, Priority::Manual));

        // Should pop in priority order.
        let it = queue.pop().unwrap();
        assert_eq!(it.lane, Priority::Manual);
        assert!(matches!(it.op, Op::Reconcile { deep: true }));

        let it = queue.pop().unwrap();
        assert_eq!(it.lane, Priority::Git);

        let it = queue.pop().unwrap();
        assert_eq!(it.lane, Priority::Fs);

        assert!(queue.pop().is_none());
    }

    /// FIFO within a lane, keyed on nothing but arrival order (no queue-level
    /// dedup — that's the apply-time mtime/hash gate's job, unchanged).
    #[test]
    fn test_fifo_within_lane() {
        let mut queue = PriorityQueue::new();

        queue.submit(item("test1", Op::Apply(ChangeSet::default()), Priority::Fs));
        queue.submit(item("test2", Op::Apply(ChangeSet::default()), Priority::Fs));

        assert_eq!(queue.pop().unwrap().project, "test1");
        assert_eq!(queue.pop().unwrap().project, "test2");
    }

    /// The watcher's shallow re-scoping reconcile — the one production item
    /// besides `Apply` — carries its depth ON the op (ADR-0042 F6b), not on any
    /// wire field.
    #[test]
    fn watcher_reconcile_rides_the_fs_lane_with_op_internal_depth() {
        let mut queue = PriorityQueue::new();
        queue.submit(item("proj", Op::Reconcile { deep: false }, Priority::Fs));

        let it = queue.pop().unwrap();
        assert_eq!(it.project, "proj");
        assert_eq!(it.lane, Priority::Fs);
        assert!(matches!(it.op, Op::Reconcile { deep: false }));
    }
}
