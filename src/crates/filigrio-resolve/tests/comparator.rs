//! ADR-0042 Phase 1a — **negative controls for the convergence gate itself.**
//!
//! Every other suite here asserts "the two states are equal". That is only worth
//! something if the comparator can actually *see* an inequality — and the Phase 1
//! comparator could not: `Node.attrs`, `partition`, `symbols` and `manifest` were
//! silently outside it, so a corruption in any of them read as convergence.
//!
//! This file mutates each `GraphState` field in turn and asserts the comparator
//! names it. It is the evidence behind the claim "the excluded set is empty" —
//! together with the compiler-enforced destructure in `common::state_facets`,
//! which makes it impossible to *add* a field without giving it a facet.

mod common;
use common::*;
use filigrio_core::{GraphState, ManifestEntry, NodeId, Project};

/// A small but non-degenerate state: several modules, an `impl` owner and an
/// inferred `returns` (the two ADR-0026 attrs), plus real resolution.
fn sample() -> GraphState {
    cold_scope(
        &[
            ("caller", "fn a\nrcall compute go\ncall helper"),
            ("lib", "fn compute -> Widget\nfn helper\nexport helper"),
            ("types", "method Widget go\nmethod Other go"),
        ],
        filigrio_resolve::LinkScope::Global,
    )
}

/// The comparator must report *nothing* when nothing changed — otherwise every
/// assertion below would pass vacuously.
#[test]
fn identical_states_have_no_diff() {
    let a = sample();
    let b = a.clone();
    assert!(
        state_diff(&a, &b, &[]).is_empty(),
        "{:?}",
        state_diff(&a, &b, &[])
    );
}

/// ADR-0042 Phase 1a.1 — the headline gap: an attr change must be a difference.
#[test]
fn node_attrs_are_part_of_the_node_identity() {
    let a = sample();
    let mut b = a.clone();
    // Corrupt exactly what ADR-0026 infers, on one function, leaving everything
    // else — id, kind, label, source_file, every edge — identical. This is the
    // shape of bug the Phase 1 gate was blind to.
    let n = b
        .graph
        .nodes
        .iter_mut()
        .find(|n| n.attrs.contains_key("returns"))
        .expect("the fixture has a `returns` attr to corrupt");
    n.attrs.insert("returns".into(), "Corrupted".into());

    let d = state_diff(&a, &b, &[]);
    assert!(
        !d.is_empty(),
        "a corrupted `returns` attr must be a difference"
    );
    assert!(
        d[0].starts_with("nodes"),
        "reported as a node difference: {d:?}"
    );
}

/// The `impl` owner (the other attr resolution depends on) likewise.
#[test]
fn impl_owner_attr_is_part_of_the_node_identity() {
    let a = sample();
    let mut b = a.clone();
    let n = b
        .graph
        .nodes
        .iter_mut()
        .find(|n| n.attrs.contains_key("impl"))
        .expect("the fixture has an `impl` attr");
    n.attrs.insert("impl".into(), "Wrong".into());
    assert!(!state_diff(&a, &b, &[]).is_empty());
}

/// Every remaining `GraphState` field, one mutation each. A field that fails
/// here is a hole in the gate.
#[test]
fn every_state_field_is_observed() {
    let base = sample();
    /// (facet name, a mutation that must show up as exactly that facet).
    type Case = (&'static str, fn(&mut GraphState));
    let cases: Vec<Case> = vec![
        ("edges", |s| {
            s.graph.edges.pop();
        }),
        ("partition", |s| {
            let cid = s
                .partition
                .node_community
                .values()
                .next()
                .cloned()
                .expect("a clustered fixture");
            s.partition
                .node_community
                .insert(NodeId::new("phantom"), cid);
        }),
        ("symbols", |s| {
            s.symbols
                .defs
                .insert("phantom".into(), NodeId::new("phantom"));
        }),
        ("reverse", |s| {
            s.reverse.refs.insert("phantom".into(), Vec::new());
        }),
        ("manifest", |s| {
            s.manifest.entries.insert(
                "phantom".into(),
                ManifestEntry {
                    hash: 7,
                    last_modified: None,
                    revision: None,
                },
            );
        }),
        ("workspace", |s| {
            s.workspace.projects.insert(
                "phantom".into(),
                Project {
                    root: "phantom".into(),
                    manifest: "Cargo.toml".into(),
                    ..Default::default()
                },
            );
        }),
        ("exports", |s| {
            s.exports.by_file.insert("phantom".into(), Vec::new());
        }),
        ("symbol_index", |s| {
            s.symbol_index
                .by_name
                .insert("phantom".into(), Default::default());
        }),
    ];

    for (field, mutate) in cases {
        let mut b = base.clone();
        mutate(&mut b);
        let d = state_diff(&base, &b, &[]);
        assert!(
            !d.is_empty(),
            "mutating `{field}` produced no diff — that field is outside the gate"
        );
        assert!(
            d.iter().any(|s| s.starts_with(field)),
            "mutating `{field}` was reported as {d:?}, not as `{field}`"
        );
    }
}

/// The **one** documented exclusion, and its exact scope: `partition` is skipped
/// for chain-vs-cold (warm-start clustering, ADR-0024) and *only* there. This
/// pins the exclusion so it cannot silently widen.
#[test]
fn partition_is_the_only_excludable_facet() {
    let a = sample();
    let mut b = a.clone();
    let cid = a.partition.node_community.values().next().cloned().unwrap();
    b.partition
        .node_community
        .insert(NodeId::new("phantom"), cid);

    assert!(
        !state_diff(&a, &b, &[]).is_empty(),
        "scope-vs-scope (Phase 1a.2): partition counts"
    );
    assert!(
        state_diff(&a, &b, &["partition"]).is_empty(),
        "chain-vs-cold: partition, and nothing else, is waived"
    );
}

/// `delta_facets` covers every `GraphDelta` field too — the per-step half of the
/// gate. Checked on the field the step comparison newly gained (`partition`) and
/// on a node-attr corruption.
#[test]
fn delta_facets_cover_attrs_and_partition() {
    let src = ToySource::new(&[
        ("lib", "fn compute -> Widget"),
        ("caller", "fn a\ncall compute"),
    ]);
    let ext = ToyExtractor::new();
    let store = filigrio_store::MemoryStore::new();
    let cs = added(&["lib", "caller"]);
    let base = apply_scope(
        &store,
        &GraphState::default(),
        &cs,
        &src,
        &ext,
        filigrio_resolve::LinkScope::Global,
    );

    let mut corrupted = base.clone();
    let n = corrupted
        .nodes_added
        .iter_mut()
        .find(|n| n.attrs.contains_key("returns"))
        .expect("a `returns` attr in the delta");
    n.attrs.insert("returns".into(), "Corrupted".into());
    assert_eq!(
        facet_mismatches(&delta_facets(&base), &delta_facets(&corrupted), &[]),
        vec!["nodes_added"],
    );

    let mut repartitioned = base.clone();
    let cid = base
        .partition
        .node_community
        .values()
        .next()
        .cloned()
        .unwrap();
    repartitioned
        .partition
        .node_community
        .insert(NodeId::new("phantom"), cid);
    assert_eq!(
        facet_mismatches(&delta_facets(&base), &delta_facets(&repartitioned), &[]),
        vec!["partition"],
    );
}
