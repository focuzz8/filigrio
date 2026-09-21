//! Per-project lock fragility suite (ADR-0032 §2: "serialize within a project,
//! parallelize across").
//!
//! The mechanic exists to close one race: two applies to the *same* project both
//! read the same `prior` state and the second checkpoint clobbers the first (a
//! lost update). These tests pin the three properties that make the lock actually
//! work — mutual exclusion, stable identity, and cross-project independence.

use filigrio_daemon::ProjectLocks;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{mpsc, Arc, Barrier};
use std::thread;
use std::time::Duration;

/// C3 — **lock identity is stable.** `lock_for(id)` twice must return handles to
/// the *same* mutex; if each call minted a fresh mutex, mutual exclusion would be
/// a silent no-op. Distinct ids must get distinct mutexes.
#[test]
fn c3_lock_identity_is_stable_per_project() {
    let locks = ProjectLocks::new();
    let a1 = locks.lock_for("proj").unwrap();
    let a2 = locks.lock_for("proj").unwrap();
    assert!(
        Arc::ptr_eq(&a1, &a2),
        "lock_for(same id) returned different mutexes → no mutual exclusion"
    );

    let b = locks.lock_for("other").unwrap();
    assert!(
        !Arc::ptr_eq(&a1, &b),
        "distinct projects must have distinct locks"
    );
    assert_eq!(locks.tracked().unwrap(), 2);
}

/// C1 — **mutual exclusion within a project (the lost-update guard).** Many
/// threads hammer the same project's lock; an in-flight counter must never show
/// two threads inside the critical section at once. Deterministic green (a real
/// shared mutex holds `max == 1`); the broken case — a fresh mutex per call —
/// lets the sections overlap and blows the counter past 1 with high reliability.
#[test]
fn c1_same_project_critical_sections_never_overlap() {
    let locks = ProjectLocks::new();
    let inflight = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let l = locks.clone();
        let inf = inflight.clone();
        let mx = max_seen.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..2_000 {
                let m = l.lock_for("proj").unwrap();
                let g = m.lock();
                let cur = inf.fetch_add(1, SeqCst) + 1;
                mx.fetch_max(cur, SeqCst);
                // Widen the critical section so an unsynchronized run overlaps.
                for _ in 0..40 {
                    std::hint::spin_loop();
                }
                inf.fetch_sub(1, SeqCst);
                drop(g);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        max_seen.load(SeqCst),
        1,
        "two applies to the same project overlapped → lost-update race"
    );
}

/// C2 — **cross-project parallelism.** Two projects must hold their locks at the
/// same instant. A `Barrier(2)` inside both critical sections only clears if both
/// threads are inside simultaneously — a global lock would deadlock here, which
/// the watchdog timeout turns into a clean failure instead of a hang.
#[test]
fn c2_different_projects_run_concurrently() {
    let locks = ProjectLocks::new();
    let barrier = Arc::new(Barrier::new(2));
    let (tx, rx) = mpsc::channel();

    for id in ["a", "b"] {
        let l = locks.clone();
        let bar = barrier.clone();
        let tx = tx.clone();
        thread::spawn(move || {
            let m = l.lock_for(id).unwrap();
            let g = m.lock();
            // Both threads must be here at once for the barrier to release.
            bar.wait();
            drop(g);
            tx.send(id).unwrap();
        });
    }
    drop(tx);

    for _ in 0..2 {
        rx.recv_timeout(Duration::from_secs(5))
            .expect("cross-project applies serialized (deadlock) — locks are not per-project");
    }
}
