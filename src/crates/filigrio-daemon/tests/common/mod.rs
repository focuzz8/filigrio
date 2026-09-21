//! Shared test helpers for the daemon suites.
//!
//! `canonicalize` is the ADR-0032 §7 **identity comparator** — the one every
//! convergence-shaped claim in this crate is judged by. It lives here rather
//! than inside `convergence.rs` so that the F4 crash-window test (ADR-0042 B12:
//! "a crash loses at most the un-flushed window and restart reconciles to the
//! same state") is measured with exactly the same yardstick, instead of a
//! second, weaker comparator invented for the occasion.

#![allow(dead_code)]

use filigrio_core::{EdgeTarget, GraphState};

/// A stable, order-independent string identity for a graph: sorted nodes
/// (id/kind/label/source **and every attr**) and sorted edges with the resolution
/// state of each target (`N:` resolved to a node id, `S:` an honest-unresolved
/// symbol, with its hints).
///
/// `attrs` is part of the identity per ADR-0042 Phase 1a.1: ADR-0026's inferred
/// `returns` and the `impl` owner live there, so while they were excluded an
/// incremental path could corrupt them and still "converge". The unresolved
/// target's hints are included for the same reason — a silently-downgraded edge
/// (e.g. losing `recv=opaque`, ADR-0029) must be a difference.
pub fn canonicalize(state: &GraphState) -> String {
    let mut nodes: Vec<String> = state
        .graph
        .nodes
        .iter()
        .map(|n| {
            let attrs: Vec<String> = n
                .attrs
                .iter() // BTreeMap — already canonically ordered
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            format!(
                "{}\t{}\t{}\t{}\t{}",
                n.id.0,
                n.kind,
                n.label,
                n.source_file.as_deref().unwrap_or(""),
                attrs.join(",")
            )
        })
        .collect();
    nodes.sort();

    let mut edges: Vec<String> = state
        .graph
        .edges
        .iter()
        .map(|e| {
            let tgt = match &e.target {
                EdgeTarget::Node(id) => format!("N:{}", id.0),
                EdgeTarget::Symbol(s) => {
                    let hints: Vec<String> =
                        s.hints.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    format!("S:{}:{}", s.name, hints.join(","))
                }
            };
            format!("{}\t{}\t{}", e.source.0, e.relation, tgt)
        })
        .collect();
    edges.sort();

    format!(
        "NODES:\n{}\n\nEDGES:\n{}",
        nodes.join("\n"),
        edges.join("\n")
    )
}
