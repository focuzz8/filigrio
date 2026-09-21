//! ADR-0042 Phase 1.1 — the export/symbol index on `GraphState`.
//!
//! Gate: after **any** apply, the maintained `symbol_index` equals a from-scratch
//! rebuild over the full node set (name → sorted candidate ids + defining
//! modules). A differential property test over cold and incremental applies.

mod common;
use common::*;
use filigrio_core::{GraphState, GraphStore, NodeId, Source, SymbolDefs, SymbolIndex};
use filigrio_resolve::{is_linkable, LinkScope};
use filigrio_store::FsStore;
use std::collections::BTreeMap;

/// From-scratch rebuild of the export/symbol index over a node set — the oracle
/// the maintained index must match.
fn rebuild(state: &GraphState) -> SymbolIndex {
    let mut candidates: BTreeMap<String, Vec<NodeId>> = BTreeMap::new();
    let mut mods: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for n in &state.graph.nodes {
        if is_linkable(n) {
            candidates
                .entry(n.label.clone())
                .or_default()
                .push(n.id.clone());
            if let Some(f) = &n.source_file {
                mods.entry(n.label.clone()).or_default().push(f.clone());
            }
        }
    }
    let mut by_name = BTreeMap::new();
    for (name, mut ids) in candidates {
        ids.sort();
        ids.dedup();
        let mut modules = mods.remove(&name).unwrap_or_default();
        modules.sort();
        modules.dedup();
        by_name.insert(
            name,
            SymbolDefs {
                nodes: ids,
                modules,
            },
        );
    }
    SymbolIndex { by_name }
}

fn assert_index_matches(state: &GraphState, msg: &str) {
    assert_eq!(state.symbol_index, rebuild(state), "{msg}");
}

#[test]
fn cold_build_index_equals_rebuild() {
    let s = cold_scope(
        &[
            ("a", "fn foo\nfn bar"),
            ("b", "fn foo\nmethod S new"),
            ("c", "fn baz\ncall foo"),
        ],
        LinkScope::Global,
    );
    assert_index_matches(&s, "cold: symbol_index ≠ rebuild");
    // Spot-check the shape: `foo` has two candidate defs across two modules.
    let foo = s.symbol_index.by_name.get("foo").expect("foo indexed");
    assert_eq!(foo.nodes.len(), 2, "foo has two candidates");
    assert_eq!(foo.modules, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn index_maintained_across_incremental_applies() {
    // Every step (add, modify, remove) must leave symbol_index == rebuild, under
    // both scopes.
    for scope in [LinkScope::Global, LinkScope::Scoped] {
        let initial = &[("a", "fn foo\ncall bar"), ("b", "fn bar")];
        let steps = &[
            Step {
                write: vec![("c", "fn foo\nfn qux")],
                remove: vec![],
                cs: added(&["c"]),
            },
            Step {
                write: vec![("b", "fn renamed")],
                remove: vec![],
                cs: modified(&["b"]),
            },
            Step {
                write: vec![],
                remove: vec!["c"],
                cs: changeset(&[], &[], &["c"]),
            },
        ];
        let s = run_incremental(initial, steps, scope);
        assert_index_matches(
            &s,
            &format!("{scope:?}: symbol_index ≠ rebuild after sequence"),
        );
    }
}

/// ADR-0042 Phase 1c F3 — the index is **not persisted**: a save/load cycle
/// drops it, and the next apply restores it equal to a from-scratch rebuild.
///
/// This is the reconstructibility half of the store contract. `load ∘ save ≡
/// identity` holds over the *persisted* facets only; for the write-only ones the
/// promise is this test — the same `rebuild` oracle the rest of the file uses,
/// run across a real `FsStore` round trip rather than an in-memory one.
#[test]
fn save_load_drops_the_index_and_the_next_apply_restores_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FsStore::new(dir.path());
    let src = ToySource::new(&[("a", "fn foo\ncall bar"), ("b", "fn bar")]);
    let ext = ToyExtractor::new();

    // Cold build, persisted through the real store.
    let cs = src.poll(None).expect("poll");
    let cold = apply_dyn(&GraphState::default(), &cs, &src, &ext, LinkScope::Global);
    assert!(
        !cold.symbol_index.by_name.is_empty(),
        "precondition: the apply produced a non-empty index to lose"
    );
    store.apply_delta(&cold).expect("apply");

    // Load: the persisted facets survive; the write-only ones come back Default.
    let loaded = store.load_state().expect("load").expect("state");
    assert!(!loaded.graph.nodes.is_empty(), "nodes are persisted");
    assert!(
        loaded.symbol_index.by_name.is_empty(),
        "symbol_index must not be read back from disk"
    );
    assert!(
        loaded.symbols.defs.is_empty(),
        "symbols must not be read back from disk"
    );

    // One apply over that index-less prior restores it in full.
    src.set("c", "fn qux\ncall foo");
    let delta = apply_dyn(&loaded, &added(&["c"]), &src, &ext, LinkScope::Global);
    store.apply_delta(&delta).expect("apply");

    // The resident state after the apply = what the store persisted + the index
    // the apply produced (which is what `merge` assigns in memory).
    let mut resident = store.load_state().expect("load").expect("state");
    resident.symbol_index = delta.symbol_index.clone();
    assert!(
        !resident.symbol_index.by_name.is_empty(),
        "the apply after a reload rebuilt the index"
    );
    assert_index_matches(
        &resident,
        "after reload + one apply: symbol_index ≠ from-scratch rebuild",
    );
}
