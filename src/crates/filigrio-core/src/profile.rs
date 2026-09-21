//! Opt-in per-stage timing — ADR-0042 **Phase 1b.1** ("profile first").
//!
//! The ~2.1 s of fixed per-apply cost that Phase 1's scoped linker does *not*
//! remove was attributed, in the ADR and in `docs/perf/benchmarks.md` §5c, by
//! *reading the code* — derived-index rebuilds and full-scale clustering. That
//! is an inference, and Phase 1b is not allowed to optimize against an
//! inference. This module is the measurement.
//!
//! **Why it lives in the kernel.** It began as `filigrio-resolve`'s private
//! timer for `Engine::apply_with_scope`; the read path (`filigrio-query`) needs
//! the same one, and a query crate must not depend on the write engine to get a
//! stopwatch. It is a generic caller-gated timer with no domain content and no
//! dependency beyond `std` — the kernel every crate already depends on is its
//! only home that does not invent a layering edge (audit §F).
//!
//! Design constraints it satisfies:
//!
//! * **Zero behavioural effect.** Recording is off unless a caller wraps the
//!   apply in [`capture`]; when off, each instrumented stage costs one
//!   thread-local `is_some` check. No env var, no cargo feature, no global
//!   state: a profiled apply is byte-for-byte the same apply.
//! * **Thread-local, so it composes.** The daemon applies projects in parallel;
//!   a captured apply only ever sees its own thread's stages.
//! * **Nested-stage safe.** Stages are recorded in completion order with their
//!   own depth, so an outer stage that contains inner ones still reports its own
//!   total (the reporter sums only depth-0 rows).

use std::cell::RefCell;
use std::time::{Duration, Instant};

/// One timed stage: `(name, depth, elapsed)`. `depth` is the nesting level at
/// entry, so a reporter can sum the top level without double-counting.
pub type Stage = (&'static str, usize, Duration);

thread_local! {
    /// `Some` only inside [`capture`]; the recording buffer plus the current
    /// nesting depth.
    static SINK: RefCell<Option<(Vec<Stage>, usize)>> = const { RefCell::new(None) };
}

/// Run `f` with stage recording enabled on this thread, returning its value and
/// the stages it recorded (in completion order). Re-entrant calls are *not*
/// supported and are not needed: the outer capture wins and the inner one is a
/// plain call (its stages land in the outer buffer).
pub fn capture<T>(f: impl FnOnce() -> T) -> (T, Vec<Stage>) {
    /// Restores the enclosing sink **on unwind too** — a panicking apply (the
    /// convergence harnesses panic on divergence) must not leave this thread
    /// recording into a dead buffer.
    struct Restore(Option<(Vec<Stage>, usize)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SINK.with(|s| *s.borrow_mut() = self.0.take());
        }
    }
    let _outer = Restore(SINK.with(|s| s.borrow_mut().replace((Vec::new(), 0))));
    let value = f();
    let mine = SINK.with(|s| s.borrow_mut().take());
    (value, mine.map(|(v, _)| v).unwrap_or_default())
}

/// Time `f` as the stage `name` when a [`capture`] is active on this thread;
/// otherwise call it directly. The `Instant::now()` pair is only taken while
/// capturing, so an un-profiled apply pays a single thread-local read.
pub fn stage<T>(name: &'static str, f: impl FnOnce() -> T) -> T {
    let depth = match SINK.with(|s| {
        s.borrow_mut().as_mut().map(|(_, d)| {
            *d += 1;
            *d - 1
        })
    }) {
        Some(d) => d,
        None => return f(),
    };
    let t = Instant::now();
    let value = f();
    let elapsed = t.elapsed();
    SINK.with(|s| {
        if let Some((buf, d)) = s.borrow_mut().as_mut() {
            *d -= 1;
            buf.push((name, depth, elapsed));
        }
    });
    value
}

/// Total elapsed across the top-level (depth-0) stages of a capture — the part
/// of an apply the instrumentation accounts for. Compare against the wall-clock
/// of the whole apply to see how much is *not* attributed to any stage.
pub fn accounted(stages: &[Stage]) -> Duration {
    stages
        .iter()
        .filter(|(_, d, _)| *d == 0)
        .map(|(_, _, e)| *e)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_nothing_when_not_capturing() {
        let v = stage("a", || 7);
        assert_eq!(v, 7);
        // Nothing to assert but the absence of a panic/leak: the sink is None,
        // so the very next capture must start empty.
        let (_, stages) = capture(|| stage("b", || ()));
        assert_eq!(
            stages.len(),
            1,
            "a prior un-captured stage leaked: {stages:?}"
        );
    }

    #[test]
    fn records_nesting_depth_and_order() {
        let (v, stages) = capture(|| {
            stage("outer", || {
                stage("inner1", || ());
                stage("inner2", || ());
                42
            })
        });
        assert_eq!(v, 42);
        let names: Vec<_> = stages.iter().map(|(n, d, _)| (*n, *d)).collect();
        // Completion order: inners finish before the outer that contains them.
        assert_eq!(names, vec![("inner1", 1), ("inner2", 1), ("outer", 0)]);
        // `accounted` counts the outer once, not the outer plus its inners.
        assert_eq!(accounted(&stages), stages[2].2);
    }

    #[test]
    fn a_panicking_capture_does_not_wedge_the_thread() {
        let r = std::panic::catch_unwind(|| capture(|| stage("boom", || panic!("apply failed"))));
        assert!(r.is_err());
        // The sink was restored on unwind, so the next capture is clean and the
        // next un-captured apply records nothing.
        stage("not recorded", || ());
        let (_, stages) = capture(|| stage("after", || ()));
        assert_eq!(
            stages.iter().map(|(n, ..)| *n).collect::<Vec<_>>(),
            vec!["after"],
            "a panicking capture leaked recorder state"
        );
    }

    #[test]
    fn capture_is_scoped_to_the_capture() {
        let (_, first) = capture(|| stage("x", || ()));
        assert_eq!(first.len(), 1);
        let (_, second) = capture(|| ());
        assert!(second.is_empty(), "sink leaked across captures: {second:?}");
    }
}
