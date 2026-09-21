/// A producer's self-declared lane in the daemon's ordering (ADR-0032 §4).
///
/// This enum defines how daemon operations are sequenced, with Manual operations
/// receiving highest priority, followed by Git hooks, then filesystem events.
/// This closed ensures the daemon's `Produced → Op` match is total, so a new
/// variant forces every consumer to handle it.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
pub enum Priority {
    /// Manual operations (highest).
    #[default]
    Manual,
    /// Git hooks (high).
    Git,
    /// Filesystem events (low).
    Fs,
}
