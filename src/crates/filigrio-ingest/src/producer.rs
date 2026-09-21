//! The change-producer taxonomy (ADR-0032e) — pull ([`Source`], ADR-0014) and push
//! ([`Producer`]) change sources, unified into one consumption-facing shape the
//! daemon drains generically.
//!
//! [`Produced`] carries the §8/R2.5 authority axis on the wire: a `Signal` is a
//! hint that must be gated (scope + dedup) before it mutates anything; an
//! `Authoritative` changeset is computed truth, applied as-is; a `Reconcile`
//! request says "re-derive the drift", deferring the actual walk/diff to the
//! consumer (which already holds the prior [`GraphState`](filigrio_core::GraphState)
//! a producer never sees). The enum is closed on purpose — the daemon's
//! `Produced → Op` match is then total, so a new variant forces every consumer to
//! handle it, and the authority rule can't be forgotten at a call site.

use filigrio_core::{ChangeSet, Priority, Result, Source};
use std::sync::mpsc::Sender;

/// What a producer hands the consumer, tagged with HOW it must be applied.
#[derive(Clone, Debug)]
pub enum Produced {
    /// A hint (e.g. inotify): gate (scope + dedup) before applying.
    Signal(ChangeSet),
    /// Computed truth (e.g. a git diff): apply as-is, never re-gated.
    Authoritative(ChangeSet),
    /// Re-derive the drift, then apply (e.g. a `.gitignore` edit re-scoping the
    /// walk). A reconcile always applies what it finds (ADR-0042 F6): the old
    /// `fix: false` read-only mode had zero producers and is gone — an index on
    /// a clean tree already IS the read-only check (`changed=0`, no writes).
    Reconcile { deep: bool },
}

/// The compiler-enforced total match the daemon's `op_of` relies on (ADR-0032e
/// §2): adding a `Produced` variant fails this match to compile until every
/// consumer is updated, which is the whole reason the enum is closed.
///
/// Deliberately **not** a `#[test]` — the check is the `match`, and a test body
/// with three empty arms asserts nothing at runtime (audit §K2f). As a plain
/// `fn` it is verified on every build, not only under `cargo test`.
#[allow(dead_code)]
fn _assert_produced_is_exhaustively_matchable(p: Produced) {
    match p {
        Produced::Signal(_) => {}
        Produced::Authoritative(_) => {}
        Produced::Reconcile { .. } => {}
    }
}

/// PUSH — the taxonomy base. Emits [`Produced`] to a sink over time and
/// self-declares its lane in the daemon's ordering (manual > git > fs). Lane is
/// intrinsic to what a producer *is*, so it lives on the producer, not in a
/// daemon-side per-producer table.
pub trait Producer: Send {
    /// This producer's lane in the daemon's priority ordering.
    fn priority(&self) -> Priority;
    /// Begin producing into `sink`. Idempotent; `stop` ends the stream.
    fn start(&mut self, sink: Sender<Produced>) -> Result<()>;
    /// Stop producing. Dropping the producer without calling `stop` is also
    /// valid — this exists for explicit lifecycle control (ADR-0032a §5).
    fn stop(&mut self);
}

/// What a [`TriggeredProducer`] emits when its trigger fires.
#[derive(Clone, Copy, Debug)]
pub enum ProducedKind {
    /// Poll the wrapped [`Source`] and emit the result as computed truth.
    Authoritative,
    /// Emit a bare reconcile request — the walk/diff itself happens downstream,
    /// against state ([`GraphState`](filigrio_core::GraphState)) this producer
    /// never holds. No `fix` field (ADR-0042 F6): a producer cannot request the
    /// removed read-only mode — the field's absence makes a stale
    /// `Reconcile { fix: false }` construction a compile error, and there is no
    /// serde impl on this type, so no serialized frame can smuggle one in either.
    Reconcile { deep: bool },
}

/// PULL, turned into a producer by a **trigger**. Any [`Source`] + a trigger
/// (startup fires once, a git hook fires via the socket, a timer on a cadence)
/// becomes a producer emitting [`Produced`] on each fire (ADR-0032e Observation 1).
/// The trigger itself is un-traited — a channel/closure that says *go*, carrying
/// no behavior of its own; `TriggeredProducer` is the trait-worthy polymorphic
/// part.
pub struct TriggeredProducer<S> {
    source: S,
    priority: Priority,
    kind: ProducedKind,
}

impl<S: Source> TriggeredProducer<S> {
    pub fn new(source: S, priority: Priority, kind: ProducedKind) -> Self {
        TriggeredProducer {
            source,
            priority,
            kind,
        }
    }

    /// Fire once, synchronously, returning the produced item without going
    /// through a sink or a background thread — the shape a manual "go now"
    /// trigger (startup, a synchronous index) needs.
    pub fn fire(&self) -> Result<Produced> {
        match self.kind {
            ProducedKind::Authoritative => Ok(Produced::Authoritative(self.source.poll(None)?)),
            ProducedKind::Reconcile { deep } => Ok(Produced::Reconcile { deep }),
        }
    }
}

impl<S: Source + Send> Producer for TriggeredProducer<S> {
    fn priority(&self) -> Priority {
        self.priority
    }

    /// MVP: a trigger that fires once at `start` time (the "startup fires once"
    /// shape — ADR-0032e §1). A cadence/socket-driven trigger that fires
    /// repeatedly is a later concern (0032b/0032c wiring); the type is already
    /// generic enough to grow a background loop without a signature change.
    fn start(&mut self, sink: Sender<Produced>) -> Result<()> {
        let produced = self.fire()?;
        let _ = sink.send(produced);
        Ok(())
    }

    fn stop(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use filigrio_core::Error;
    use std::path::Path;

    /// A trivial pull `Source` over an in-memory changeset, so these tests pin
    /// `TriggeredProducer`'s behavior without touching the filesystem.
    struct StubSource(ChangeSet);

    impl Source for StubSource {
        fn poll(&self, _since: Option<&filigrio_core::Revision>) -> Result<ChangeSet> {
            Ok(self.0.clone())
        }
        fn read(&self, _path: &str) -> Result<Vec<u8>> {
            Err(Error::Io("stub has no files".into()))
        }
        fn exists(&self, _path: &str) -> bool {
            false
        }
        fn root(&self) -> Option<&Path> {
            None
        }
    }

    /// Trigger fire → poll → emit `Authoritative` carrying the source's changeset.
    #[test]
    fn triggered_producer_fires_authoritative_from_source_poll() {
        let cs = ChangeSet {
            added: vec!["a.rs".into()],
            modified: vec![],
            removed: vec![],
        };
        let producer = TriggeredProducer::new(
            StubSource(cs.clone()),
            Priority::Git,
            ProducedKind::Authoritative,
        );
        let produced = producer.fire().unwrap();
        match produced {
            Produced::Authoritative(got) => assert_eq!(got.added, cs.added),
            other => panic!("expected Authoritative, got {other:?}"),
        }
    }

    /// The empty/no-drift case: an empty source changeset still surfaces as an
    /// (empty) `Authoritative` — the producer does not decide "nothing to do",
    /// that judgment belongs downstream at apply time.
    #[test]
    fn triggered_producer_empty_source_still_emits_authoritative() {
        let producer = TriggeredProducer::new(
            StubSource(ChangeSet::default()),
            Priority::Git,
            ProducedKind::Authoritative,
        );
        let produced = producer.fire().unwrap();
        match produced {
            Produced::Authoritative(cs) => {
                assert!(cs.added.is_empty() && cs.modified.is_empty() && cs.removed.is_empty())
            }
            other => panic!("expected Authoritative, got {other:?}"),
        }
    }

    /// A reconcile-mode fire is a bare signal — `deep` passes through unchanged,
    /// regardless of the (unused) wrapped source's content.
    #[test]
    fn triggered_producer_reconcile_mode_emits_bare_signal() {
        let producer = TriggeredProducer::new(
            StubSource(ChangeSet::default()),
            Priority::Fs,
            ProducedKind::Reconcile { deep: true },
        );
        let produced = producer.fire().unwrap();
        match produced {
            Produced::Reconcile { deep } => {
                assert!(deep);
            }
            other => panic!("expected Reconcile, got {other:?}"),
        }
    }

    /// `priority()` is self-declared by the producer, not assigned externally.
    #[test]
    fn priority_is_self_declared() {
        let producer = TriggeredProducer::new(
            StubSource(ChangeSet::default()),
            Priority::Git,
            ProducedKind::Authoritative,
        );
        assert_eq!(producer.priority(), Priority::Git);
    }
}
