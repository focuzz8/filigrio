//! ADR-0042 **Phase 1d P1 — the patch-delta contract**.
//!
//! Before Phase 1d a `GraphDelta` said what the world *now is*: `edges_added`
//! carried the entire resolved edge set (~351k edges at next.js scale), there
//! was no `edges_removed`, and nothing named the files the apply touched. A
//! `ShardedStore` handed such a delta cannot tell which shards changed without
//! serializing and diffing the whole graph — O(repo), which is exactly what
//! sharding exists to avoid.
//!
//! These specs pin the delta as a **patch**: it describes the change, and
//! nothing else. The equivalence half ("the same state still comes out") is
//! gated by the Phase 1a suite — `scoped_equivalence`, `shadow`,
//! `git_convergence` — which is deliberately left untouched; what is proven
//! *here* is that the patch is exact, minimal, and scope-independent.

mod common;

use common::{
    added, apply_scope, assert_resolution_eq, changeset, cold_final, cold_scope, edge_key,
    edge_keys, modified, Step, ToyExtractor, ToySource,
};
use filigrio_core::{Edge, EdgeTarget, GraphDelta, GraphState, Source};
use filigrio_resolve::LinkScope;
use filigrio_store::MemoryStore;
use std::collections::{BTreeMap, BTreeSet};

// ---- helpers ---------------------------------------------------------------

/// Edge multiset, keyed by the canonical [`edge_key`] the comparators use.
fn counts(edges: &[Edge]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for e in edges {
        *m.entry(edge_key(e)).or_default() += 1;
    }
    m
}

/// Cold build of `files`, plus the source/extractor/store to keep applying to.
fn start(files: &[(&'static str, &'static str)]) -> (MemoryStore, ToySource, ToyExtractor) {
    let src = ToySource::new(files);
    let ext = ToyExtractor::new();
    let store = MemoryStore::new();
    let cs = src.poll(None).expect("poll");
    apply_scope(
        &store,
        &GraphState::default(),
        &cs,
        &src,
        &ext,
        LinkScope::Global,
    );
    (store, src, ext)
}

/// Does `edges` contain an edge from `source` via `relation` to `target`
/// (`"N:<id>"` for a resolved target, `"S:<name>"` for an unresolved one)?
fn has(edges: &[Edge], source: &str, relation: &str, target: &str) -> bool {
    edges.iter().any(|e| {
        let t = match &e.target {
            EdgeTarget::Node(n) => format!("N:{}", n.0),
            EdgeTarget::Symbol(r) => format!("S:{}", r.name),
        };
        e.source.0 == source && e.relation == relation && t == target
    })
}

/// **The oracle**: what the pre-Phase-1d wholesale merge would have produced.
///
/// The old store did `state.graph.edges = delta.edges_added` (the whole set)
/// then pruned dangling endpoints. The whole set is, by the definition of a
/// minimal diff, `prior − edges_removed + edges_added`, so replaying *that*
/// through the same prune rule reproduces the old behaviour exactly — which is
/// what "behaviour-identical, just described differently" has to mean.
fn wholesale_edges(prior: &GraphState, delta: &GraphDelta, merged: &GraphState) -> Vec<String> {
    let mut budget = counts(&delta.edges_removed);
    let mut out: Vec<Edge> = Vec::new();
    for e in &prior.graph.edges {
        match budget.get_mut(&edge_key(e)) {
            Some(n) if *n > 0 => *n -= 1,
            _ => out.push(e.clone()),
        }
    }
    out.extend(delta.edges_added.iter().cloned());
    // The same dangling-endpoint prune the store applies, against the merged
    // node set (nodes are not patched, so `merged`'s node set is authoritative).
    let ids: BTreeSet<&str> = merged.graph.nodes.iter().map(|n| n.id.0.as_str()).collect();
    out.retain(|e| {
        ids.contains(e.source.0.as_str())
            && match &e.target {
                EdgeTarget::Node(t) => ids.contains(t.0.as_str()),
                EdgeTarget::Symbol(_) => true,
            }
    });
    edge_keys(&out)
}

// ---- the delta describes the change, not the world -------------------------

/// An apply that adds a file puts *its* edges in `edges_added` — and none of the
/// prior set, which is what the old delta shipped in full.
#[test]
fn an_added_file_contributes_only_its_own_edges() {
    let (store, src, ext) = start(&[("a", "fn a\ncall b"), ("b", "fn b")]);
    let prior = store.current().expect("state");
    assert_eq!(prior.graph.edges.len(), 3, "precondition: 3 prior edges");

    src.set("c", "fn c\ncall a");
    let delta = apply_scope(
        &store,
        &prior,
        &added(&["c"]),
        &src,
        &ext,
        LinkScope::Global,
    );

    let prior_keys: BTreeSet<String> = prior.graph.edges.iter().map(edge_key).collect();
    let carried: Vec<&Edge> = delta
        .edges_added
        .iter()
        .filter(|e| prior_keys.contains(&edge_key(e)))
        .collect();
    assert!(
        carried.is_empty(),
        "an unchanged prior edge must not be re-shipped as an addition: {carried:?}"
    );
    assert!(
        has(&delta.edges_added, "file:c", "contains", "N:fn:c:c"),
        "the new file's structural edge is an addition: {:?}",
        edge_keys(&delta.edges_added)
    );
    assert!(
        has(&delta.edges_added, "fn:c:c", "calls", "N:fn:a:a"),
        "the new file's resolved call is an addition"
    );
    assert!(
        delta.edges_removed.is_empty(),
        "adding a file removes nothing: {:?}",
        edge_keys(&delta.edges_removed)
    );
    // The whole point: the delta is smaller than the graph.
    assert!(
        delta.edges_added.len() < prior.graph.edges.len(),
        "the patch ({}) must be smaller than the prior edge set ({})",
        delta.edges_added.len(),
        prior.graph.edges.len()
    );
}

/// Removing a file puts its edges in `edges_removed` — including the *dependent*
/// resolved edge from the surviving caller, which downgrades to unresolved (so
/// the same apply is both a removal and an addition at that site).
#[test]
fn a_removed_file_puts_its_edges_in_edges_removed() {
    let (store, src, ext) = start(&[("a", "fn a\ncall b"), ("b", "fn b")]);
    let prior = store.current().expect("state");

    src.remove("b");
    let delta = apply_scope(
        &store,
        &prior,
        &changeset(&[], &[], &["b"]),
        &src,
        &ext,
        LinkScope::Global,
    );

    assert!(
        has(&delta.edges_removed, "file:b", "contains", "N:fn:b:b"),
        "the removed file's own edge is retired: {:?}",
        edge_keys(&delta.edges_removed)
    );
    assert!(
        has(&delta.edges_removed, "fn:a:a", "calls", "N:fn:b:b"),
        "the dependent's resolved edge is retired, not silently rewritten"
    );
    assert!(
        has(&delta.edges_added, "fn:a:a", "calls", "S:b"),
        "…and re-added unresolved: {:?}",
        edge_keys(&delta.edges_added)
    );
    assert_eq!(
        store.current().expect("state").graph.edges.len(),
        2,
        "the merged graph keeps file:a→fn:a and the now-unresolved call"
    );
}

/// Re-indexing unchanged content changes nothing, and the patch says so. This is
/// the sharpest statement of "the delta describes the change": under the old
/// wholesale delta this apply still shipped every edge in the graph.
#[test]
fn an_idempotent_reindex_produces_an_empty_edge_patch() {
    for scope in [LinkScope::Global, LinkScope::Scoped] {
        let (store, src, ext) = start(&[("a", "fn a\ncall b"), ("b", "fn b\nmcall hidden")]);
        let prior = store.current().expect("state");
        let delta = apply_scope(&store, &prior, &modified(&["a"]), &src, &ext, scope);
        assert!(
            delta.edges_added.is_empty() && delta.edges_removed.is_empty(),
            "{scope:?}: re-indexing identical content is not a change; got +{:?} -{:?}",
            edge_keys(&delta.edges_added),
            edge_keys(&delta.edges_removed)
        );
        assert_eq!(
            store.current().expect("state").graph.edges.len(),
            prior.graph.edges.len(),
            "{scope:?}: and the merged graph is unchanged"
        );
    }
}

// ---- the dirty-file set ----------------------------------------------------

/// `dirty_files` is the changeset's touched set — added ∪ modified ∪ removed.
#[test]
fn dirty_files_is_the_touched_set() {
    let (store, src, ext) = start(&[("a", "fn a"), ("b", "fn b"), ("keep", "fn k")]);
    let prior = store.current().expect("state");

    src.set("c", "fn c");
    src.set("a", "fn a\nfn a2");
    src.remove("b");
    let delta = apply_scope(
        &store,
        &prior,
        &changeset(&["c"], &["a"], &["b"]),
        &src,
        &ext,
        LinkScope::Global,
    );

    assert_eq!(
        delta.dirty_files,
        ["a", "b", "c"].iter().map(|s| s.to_string()).collect(),
        "added ∪ modified ∪ removed — and nothing else (`keep` was not touched)"
    );
}

/// **F8**: a path that vanished between detection and apply is folded into the
/// removals, so it must appear in `dirty_files` — the shard router has to hear
/// about the file whose nodes just went away. "Silently converged" must not
/// become the new silent failure (ADR-0029).
#[test]
fn a_vanished_path_is_still_dirty() {
    let (store, src, ext) = start(&[("a", "fn a"), ("doomed", "fn d")]);
    let prior = store.current().expect("state");

    // Announced as modified, but gone by the time the engine reads it.
    src.remove("doomed");
    let delta = apply_scope(
        &store,
        &prior,
        &modified(&["doomed"]),
        &src,
        &ext,
        LinkScope::Global,
    );

    assert_eq!(delta.vanished, 1, "precondition: the F8 fold fired");
    assert!(
        delta.dirty_files.contains("doomed"),
        "a vanished path is a removal, and removals are dirty: {:?}",
        delta.dirty_files
    );
    assert!(
        has(
            &delta.edges_removed,
            "file:doomed",
            "contains",
            "N:fn:doomed:d"
        ),
        "…and its edges are retired: {:?}",
        edge_keys(&delta.edges_removed)
    );
}

// ---- the `reverse` patch ---------------------------------------------------

/// `reverse` rides as drop-by-source + append. The drop half names exactly the
/// touched file's prior nodes; the append half carries only that file's fresh
/// references — not the whole ~33 MB index.
#[test]
fn the_reverse_patch_is_drop_by_source_plus_append() {
    let (store, src, ext) = start(&[("a", "fn a\ncall b"), ("b", "fn b\ncall a")]);
    let prior = store.current().expect("state");
    assert_eq!(
        prior.reverse.refs.len(),
        2,
        "precondition: both `a` and `b` are referenced"
    );

    src.set("a", "fn a\ncall zzz");
    let delta = apply_scope(
        &store,
        &prior,
        &modified(&["a"]),
        &src,
        &ext,
        LinkScope::Global,
    );

    let dropped: BTreeSet<&str> = delta.reverse_dropped.iter().map(|n| n.0.as_str()).collect();
    assert_eq!(
        dropped,
        ["file:a", "fn:a:a"].into_iter().collect(),
        "exactly the touched file's prior node ids retire their reference sites"
    );
    let names: BTreeSet<&str> = delta
        .reverse_added
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(
        names,
        ["zzz"].into_iter().collect(),
        "only the re-extracted file's references are contributed: {:?}",
        delta.reverse_added
    );

    // The merged index is what a from-scratch rebuild would have produced.
    let after = store.current().expect("state");
    assert!(
        !after.reverse.refs.contains_key("b"),
        "`a`'s retired reference to `b` leaves no empty name behind: {:?}",
        after.reverse.refs.keys().collect::<Vec<_>>()
    );
    assert!(
        after.reverse.refs.contains_key("zzz") && after.reverse.refs.contains_key("a"),
        "the new reference lands and `b`'s untouched reference to `a` survives"
    );
}

// ---- exactness, minimality, scope-independence ------------------------------

/// The property test: over a sequence of adds / edits / deletes / re-adds, every
/// step's patch must be
///
/// 1. **exact** — merging it reproduces the pre-Phase-1d wholesale result
///    (`wholesale_edges`, which replays `prior − removed + added` through the
///    same prune rule);
/// 2. **minimal** — no edge is both removed and added, and nothing is retired
///    that was not in `prior`;
/// 3. **scope-independent** — `Global` and `Scoped` emit the *same* patch, not
///    merely converge to the same state (ADR-0042 B10);
///
/// and the accumulated state must still equal a cold build of the final tree.
#[test]
fn the_patch_is_exact_minimal_and_scope_independent() {
    let initial: &[(&str, &str)] = &[
        ("a", "fn a\ncall b\ncall gone"),
        ("b", "fn b\nexport b\ncall a"),
        ("c", "fn c\nimport b ./b\nmcall opaque"),
    ];
    let steps = vec![
        // a new file that satisfies a previously unresolved name
        Step {
            write: vec![("gone", "fn gone")],
            remove: vec![],
            cs: added(&["gone"]),
        },
        // an in-place edit that adds and drops references at once
        Step {
            write: vec![("a", "fn a\ncall gone\ncall c")],
            remove: vec![],
            cs: modified(&["a"]),
        },
        // a deletion that strands two dependents
        Step {
            write: vec![],
            remove: vec!["gone"],
            cs: changeset(&[], &[], &["gone"]),
        },
        // an idempotent re-index (no content change at all)
        Step {
            write: vec![],
            remove: vec![],
            cs: modified(&["b"]),
        },
        // re-adding the deleted file — relinks the stranded dependents
        Step {
            write: vec![("gone", "fn gone\nexport gone")],
            remove: vec![],
            cs: added(&["gone"]),
        },
        // a rename, as the engine sees one: remove + add in a single changeset
        Step {
            write: vec![("d", "fn c\ncall a")],
            remove: vec!["c"],
            cs: changeset(&["d"], &[], &["c"]),
        },
    ];

    let src = ToySource::new(initial);
    let ext = ToyExtractor::new();
    let store = MemoryStore::new();
    let cs = src.poll(None).expect("poll");
    apply_scope(
        &store,
        &GraphState::default(),
        &cs,
        &src,
        &ext,
        LinkScope::Global,
    );

    for (i, step) in steps.iter().enumerate() {
        for (p, c) in &step.write {
            src.set(p, c);
        }
        for p in &step.remove {
            src.remove(p);
        }
        let prior = store.current().expect("state");

        // Same prior, same changeset, both scopes — the patch itself must match.
        let g = apply_scope(&store, &prior, &step.cs, &src, &ext, LinkScope::Global);
        let s = {
            let shadow = MemoryStore::new();
            apply_scope(&shadow, &prior, &step.cs, &src, &ext, LinkScope::Scoped)
        };
        assert_eq!(
            (edge_keys(&g.edges_added), edge_keys(&g.edges_removed)),
            (edge_keys(&s.edges_added), edge_keys(&s.edges_removed)),
            "step {i}: the minimal patch is a property of the change, not of the \
             link scope (ADR-0042 B10)"
        );
        assert_eq!(
            g.dirty_files, s.dirty_files,
            "step {i}: the dirty-file set is scope-independent"
        );

        // (2) minimality + containment.
        let add_keys: BTreeSet<String> = g.edges_added.iter().map(edge_key).collect();
        let rem_keys: BTreeSet<String> = g.edges_removed.iter().map(edge_key).collect();
        let both: Vec<&String> = add_keys.intersection(&rem_keys).collect();
        assert!(
            both.is_empty(),
            "step {i}: a minimal patch never removes and re-adds the same edge: {both:?}"
        );
        let prior_counts = counts(&prior.graph.edges);
        for (k, n) in counts(&g.edges_removed) {
            assert!(
                prior_counts.get(&k).copied().unwrap_or(0) >= n,
                "step {i}: cannot retire {k} — prior holds fewer copies than the patch removes"
            );
        }

        // (1) exactness against the pre-Phase-1d wholesale behaviour.
        let merged = store.current().expect("state");
        assert_eq!(
            edge_keys(&merged.graph.edges),
            wholesale_edges(&prior, &g, &merged),
            "step {i}: the patched merge must equal `prior − removed + added`, pruned"
        );
    }

    // …and the accumulated state still converges on a cold build.
    let owned: Vec<(String, String)> = {
        let mut m: BTreeMap<String, String> = initial
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect();
        for step in &steps {
            for (p, c) in &step.write {
                m.insert(p.to_string(), c.to_string());
            }
            for p in &step.remove {
                m.remove(*p);
            }
        }
        m.into_iter().collect()
    };
    let files: Vec<(&str, &str)> = owned
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    assert_resolution_eq(
        &store.current().expect("state"),
        &cold_scope(&files, LinkScope::Global),
        "incremental patch chain ≢ cold build",
    );
}

/// The Phase-1a tiers already prove `Scoped ≡ Global ≡ cold` for the *state*;
/// this pins that the same holds when the chain is driven purely through
/// patches, including a step whose only content is a removal (the case a
/// wholesale replacement got right by accident, because it never had to name
/// what disappeared).
#[test]
fn a_removal_only_chain_still_converges_on_a_cold_build() {
    let initial: &[(&str, &str)] = &[
        ("a", "fn a\ncall b\ncall c"),
        ("b", "fn b\ncall c"),
        ("c", "fn c"),
    ];
    let steps = vec![
        Step {
            write: vec![],
            remove: vec!["c"],
            cs: changeset(&[], &[], &["c"]),
        },
        Step {
            write: vec![],
            remove: vec!["b"],
            cs: changeset(&[], &[], &["b"]),
        },
    ];
    for scope in [LinkScope::Global, LinkScope::Scoped] {
        let got = common::run_incremental(initial, &steps, scope);
        assert_resolution_eq(
            &got,
            &cold_final(initial, &steps),
            &format!("{scope:?}: removal-only chain ≢ cold"),
        );
    }
}
