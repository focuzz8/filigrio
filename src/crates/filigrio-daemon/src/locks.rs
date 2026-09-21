//! Per-project locks (ADR-0032 §2 concurrency).
//!
//! > Concurrency: serialize within a project, parallelize across. Different
//! > projects' graphs are independent → their applies run concurrently. Within
//! > one project, applies are serialized (incremental state mutates in order).
//!
//! The load-bearing correctness invariant is **per-project serialization**: an
//! apply is `load prior → apply delta → persist`. Two applies to the *same*
//! project that interleave both read the same `prior` and the second persist
//! clobbers the first — a lost update. Serializing per project closes that race;
//! keying the lock *per project* (not one global lock) lets independent projects
//! overlap so a heavy monorepo apply never blocks a small repo's apply.
//!
//! A sync `parking_lot::Mutex` (not `tokio::Mutex`) because the apply critical
//! section is synchronous CPU work (parse/resolve) with no `.await` inside it;
//! parking_lot also drops poisoning, so a panic under one project's lock can't
//! wedge the whole table.

use crate::Result;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// Hands out one stable lock per project id. Cloning shares the same underlying
/// table, so clones agree on lock identity (required for mutual exclusion).
#[derive(Clone, Default)]
pub struct ProjectLocks {
    locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl ProjectLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// The lock for `id`, created on first use. Calling twice for the same id
    /// MUST return handles to the *same* mutex — otherwise the "serialize within
    /// a project" guarantee is a no-op.
    pub fn lock_for(&self, id: &str) -> Result<Arc<Mutex<()>>> {
        let mut table = self.locks.lock();
        Ok(table
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone())
    }

    /// Number of distinct project locks currently tracked.
    pub fn tracked(&self) -> Result<usize> {
        Ok(self.locks.lock().len())
    }
}
