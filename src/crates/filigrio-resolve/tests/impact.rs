//! ADR-0042 Phase 1.2 — the change-impact set is **authority**, and it may
//! over-cover but must never under-cover.
//!
//! The property: for a change, every reference site whose resolved target set
//! differs between the pre-apply (prior) and post-apply (global) resolution is in
//! the impact set (or the impact set is `full`). Under-covering such a site is the
//! one forbidden failure — it would let the scoped linker carry a stale edge.
//! Plus a tightness witness: an unrelated edit must NOT drag in an unaffected
//! site (proof the set is a real reduction, not "always full").

mod common;
use common::*;
use filigrio_core::{ChangeSet, Edge, EdgeTarget, GraphState, NodeId};
use filigrio_resolve::{ClusterConfig, Engine, LinkScope};
use std::collections::{BTreeMap, BTreeSet};

/// Reference-site edges (`calls`/`imports`) as `(source, relation) → {target}`.
fn ref_sites(edges: &[Edge]) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut m: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for e in edges {
        if e.relation != "calls" && e.relation != "imports" {
            continue;
        }
        let tgt = match &e.target {
            EdgeTarget::Node(n) => format!("N:{}", n.0),
            EdgeTarget::Symbol(r) => format!("S:{}", r.name),
        };
        m.entry((e.source.0.clone(), e.relation.clone()))
            .or_default()
            .insert(tgt);
    }
    m
}

/// Assert the impact set covers every reference site whose resolution changed.
/// `prior` is the state before the change; `cs` the change; the source/extractor
/// carry the new file contents.
fn assert_covers(
    scenario: &str,
    prior: &GraphState,
    cs: &ChangeSet,
    src: &ToySource,
    ext: &ToyExtractor,
) {
    // Post-apply global resolution (the truth the scoped path must match).
    let delta = Engine::apply_with_scope(
        prior,
        cs,
        src,
        ext,
        &ClusterConfig::default(),
        LinkScope::Global,
    )
    .expect("global apply");
    let pre = ref_sites(&prior.graph.edges);
    let post = ref_sites(&delta.edges_added);

    let impact = Engine::impact_report(prior, cs, src, ext).expect("impact");

    // Every POST site whose target set differs from PRE must be impacted. (A site
    // that vanished — its source node removed — is a node deletion, not a
    // re-resolution, and is handled by node removal, not the impact set.)
    for (site, post_tgts) in &post {
        let pre_tgts = pre.get(site);
        if pre_tgts == Some(post_tgts) {
            continue; // resolution unchanged at this site
        }
        let (source, relation) = site;
        assert!(
            impact.site_impacted(&NodeId::new(source.clone()), relation),
            "{scenario}: site ({source}, {relation}) changed resolution \
             (pre={pre_tgts:?} post={post_tgts:?}) but is NOT in the impact set \
             (full={})",
            impact.full
        );
    }
}

/// Cold-build a fixture and return (state, live source, live extractor) so a
/// follow-up change can be applied against real file contents.
fn seed(files: &[(&str, &str)]) -> (GraphState, ToySource, ToyExtractor) {
    let src = ToySource::new(files);
    let ext = ToyExtractor::new();
    let store = filigrio_store::MemoryStore::new();
    let cs = ChangeSet::all_added(files.iter().map(|(p, _)| p.to_string()));
    apply_scope(
        &store,
        &GraphState::default(),
        &cs,
        &src,
        &ext,
        LinkScope::Global,
    );
    (store.current().unwrap(), src, ext)
}

#[test]
fn covers_def_addition_relink() {
    // caller calls b (unresolved); adding a def for b must relink caller — its
    // site's resolution changes, so it must be covered.
    let (prior, src, ext) = seed(&[("caller", "fn a\ncall b")]);
    src.set("lib", "fn b");
    assert_covers("def_addition", &prior, &added(&["lib"]), &src, &ext);
}

#[test]
fn covers_def_removal_relink() {
    let (prior, src, ext) = seed(&[("caller", "fn a\ncall b"), ("lib", "fn b")]);
    src.set("lib", "fn c");
    assert_covers("def_removal", &prior, &modified(&["lib"]), &src, &ext);
}

#[test]
fn covers_return_type_callee_edit() {
    // Editing the callee `compute` (adding a fn; compute's name becomes dirty)
    // must cover the caller's `go` site via the recv_returns∈dirty rule.
    let (prior, src, ext) = seed(&[
        ("caller", "fn a\nrcall compute go"),
        ("lib", "fn compute -> Widget"),
        ("types", "method Widget go\nmethod Other go"),
    ]);
    src.set("lib", "fn compute -> Widget\nfn compute2");
    assert_covers(
        "return_type_callee",
        &prior,
        &modified(&["lib"]),
        &src,
        &ext,
    );
}

#[test]
fn covers_barrel_terminal_edit() {
    let (prior, src, ext) = seed(&[
        ("index.rs", "reexport greet ./mid greet"),
        ("mid.rs", "reexport greet ./greet greet"),
        ("greet.rs", "fn greet\nexport greet"),
        ("app.rs", "import greet ./index\nfn boot\ncall greet"),
    ]);
    src.set("greet.rs", "fn greet\nexport greet\nfn helper");
    assert_covers(
        "barrel_terminal",
        &prior,
        &modified(&["greet.rs"]),
        &src,
        &ext,
    );
}

#[test]
fn covers_import_repoint_full() {
    let (prior, src, ext) = seed(&[
        ("a.rs", "import g ./b greet\nfn run\ncall g"),
        ("b.rs", "fn greet\nexport greet"),
    ]);
    src.set("a.rs", "import g ./c greet\nfn run\ncall g");
    src.set("c.rs", "fn greet\nexport greet");
    assert_covers(
        "import_repoint",
        &prior,
        &changeset(&["c.rs"], &["a.rs"], &[]),
        &src,
        &ext,
    );
}

// ---- tightness: an unrelated edit must not over-drag ------------------------

#[test]
fn unrelated_edit_leaves_far_sites_unimpacted() {
    // A return-type-resolved caller and an unrelated `noise` module. Editing noise
    // must NOT impact the caller's site, and must not go full — proof the impact
    // set is a genuine reduction.
    let (prior, src, ext) = seed(&[
        ("caller", "fn a\nrcall compute go"),
        ("lib", "fn compute -> Widget"),
        ("types", "method Widget go\nmethod Other go"),
        ("noise", "fn n"),
    ]);
    src.set("noise", "fn n\nfn n2");
    let impact = Engine::impact_report(&prior, &modified(&["noise"]), &src, &ext).expect("impact");
    assert!(
        !impact.full,
        "an unrelated body edit must not force full re-resolution"
    );
    assert!(
        !impact.site_impacted(&NodeId::new("fn:caller:a"), "calls"),
        "the caller's return-type site is far from the edit and must be un-impacted"
    );
    // And it still covers everything that changed (nothing did, here — but assert
    // the safety property holds regardless).
    assert_covers("unrelated_edit", &prior, &modified(&["noise"]), &src, &ext);
}
