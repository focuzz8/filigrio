//! ADR-0042 Phase 1.0 + 1.3 — the **acceptance contract** for scoped
//! re-resolution: one convergence fixture per resolution tier that scoped linking
//! could plausibly get wrong, each asserting the §7 identity property
//! `apply(diffs) ≡ cold_build(final)` — under **both** [`LinkScope::Global`] (the
//! green baseline, 1.0) and [`LinkScope::Scoped`] (the equivalence gate, 1.3).
//!
//! Passing under both scopes proves, per tier, `Scoped ≡ Global ≡ cold`. If any
//! tier could not be made equivalent this file would fail loudly rather than
//! paper over it — the switch stays dormant until every row here is green.

mod common;
use common::*;
use filigrio_core::{EdgeTarget, GraphState};
use filigrio_resolve::LinkScope;

/// The core assertion each tier makes: the incremental sequence converges to the
/// cold build of its final tree under *both* scopes (so `Scoped ≡ Global ≡ cold`).
fn assert_tier(name: &str, initial: &[(&str, &str)], steps: &[Step]) {
    let cold = cold_final(initial, steps);
    let global = run_incremental(initial, steps, LinkScope::Global);
    let scoped = run_incremental(initial, steps, LinkScope::Scoped);
    // Resolution converges to the cold build under both scopes (partition excluded
    // — warm-start clustering, ADR-0024).
    assert_resolution_eq(
        &global,
        &cold,
        &format!("{name}: Global incremental ≢ cold"),
    );
    assert_resolution_eq(
        &scoped,
        &cold,
        &format!("{name}: Scoped incremental ≢ cold"),
    );
    // Scoped and Global are *fully* identical — partition included.
    assert_state_eq(&scoped, &global, &format!("{name}: Scoped ≢ Global"));
}

/// Find the single `calls` edge out of `src_id` in `s`.
fn call_target(s: &GraphState, src_id: &str) -> EdgeTarget {
    let mut it = s
        .graph
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source.0 == src_id);
    let e = it
        .next()
        .unwrap_or_else(|| panic!("no calls edge from {src_id}"));
    assert!(
        it.next().is_none(),
        "more than one calls edge from {src_id}"
    );
    e.target.clone()
}

// ---- tier (a): cross-module homonym, one module edits -----------------------

#[test]
fn tier_cross_module_homonym() {
    // `foo` defined in two modules; a third calls it (bare → AMBIGUOUS min-id).
    // Editing one definer module (adding an unrelated fn) must not change the
    // caller's edge — and the scoped path re-resolves it (its name is dirty) to
    // the same AMBIGUOUS pick.
    let initial = &[
        ("m1", "fn foo\nfn a"),
        ("m2", "fn foo"),
        ("caller", "fn c\ncall foo"),
    ];
    let steps = &[Step {
        write: vec![("m2", "fn foo\nfn extra")],
        remove: vec![],
        cs: modified(&["m2"]),
    }];
    assert_tier("cross_module_homonym", initial, steps);
    // Pin the actual resolution so the tier tests a real homonym, not a no-op.
    let s = run_incremental(initial, steps, LinkScope::Scoped);
    assert!(
        matches!(call_target(&s, "fn:caller:c"), EdgeTarget::Node(n) if n.0 == "fn:m1:foo"),
        "caller binds the deterministic (min-id) homonym"
    );
}

// ---- tier (b): opaque-receiver decline survives an unrelated neighbor edit ---

#[test]
fn tier_opaque_receiver_decline_survives_neighbor_edit() {
    // `caller.m` makes an opaque method call `iter`; a cross-file `iter` exists.
    // The call must stay DECLINED (unresolved Symbol, recv=opaque). An unrelated
    // edit to `other` leaves the caller site un-impacted, so scoped *carries* the
    // prior unresolved-with-hint edge verbatim — the carry must preserve it.
    let initial = &[
        ("caller", "fn m\nmcall iter"),
        ("lib", "fn iter"),
        ("other", "fn unrelated"),
    ];
    let steps = &[Step {
        write: vec![("other", "fn unrelated\nfn added")],
        remove: vec![],
        cs: modified(&["other"]),
    }];
    assert_tier("opaque_decline", initial, steps);
    let s = run_incremental(initial, steps, LinkScope::Scoped);
    let EdgeTarget::Symbol(r) = call_target(&s, "fn:caller:m") else {
        panic!("opaque call must stay an unresolved Symbol");
    };
    assert_eq!(r.name, "iter");
    assert_eq!(
        r.hints.get("recv").map(String::as_str),
        Some("opaque"),
        "the carried unresolved edge keeps its recv=opaque marker (ADR-0029)"
    );
}

// ---- tier (c): return-type-deferred inference across a module boundary -------

#[test]
fn tier_return_type_deferred_across_boundary() {
    // `let w = compute(); w.go()` with `compute() -> Widget` in another module and
    // `Widget::go` / `Other::go` in a third. Step 1 (unrelated `noise` edit) must
    // CARRY the resolved `Widget::go` edge; step 2 (adding a fn to `lib`, so the
    // callee `compute`'s name is dirty) must RE-RESOLVE via the recv_returns∈dirty
    // rule — both back to Widget::go.
    let initial = &[
        ("caller", "fn a\nrcall compute go"),
        ("lib", "fn compute -> Widget"),
        ("types", "method Widget go\nmethod Other go"),
        ("noise", "fn n"),
    ];
    let steps = &[
        Step {
            write: vec![("noise", "fn n\nfn n2")],
            remove: vec![],
            cs: modified(&["noise"]),
        },
        Step {
            write: vec![("lib", "fn compute -> Widget\nfn compute2")],
            remove: vec![],
            cs: modified(&["lib"]),
        },
    ];
    assert_tier("return_type_deferred", initial, steps);
    let s = run_incremental(initial, steps, LinkScope::Scoped);
    assert!(
        matches!(call_target(&s, "fn:caller:a"), EdgeTarget::Node(n) if n.0 == "fn:types:Widget::go"),
        "deferred receiver type resolves to Widget::go across the boundary"
    );
}

// ---- tier (d): import-alias rebinding ---------------------------------------

#[test]
fn tier_import_alias_rebinding() {
    // `import g ./b greet` then `call g` binds g→b.greet (EXTRACTED). Step 1
    // (unrelated `noise` edit) CARRIES the import-bound edge; step 2 re-points the
    // import to `./c` (new export table ⇒ full re-resolution) and must rebind
    // g→c.greet.
    let initial = &[
        ("a.rs", "import g ./b greet\nfn run\ncall g"),
        ("b.rs", "fn greet\nexport greet"),
        ("noise.rs", "fn n"),
    ];
    let steps = &[
        Step {
            write: vec![("noise.rs", "fn n\nfn n2")],
            remove: vec![],
            cs: modified(&["noise.rs"]),
        },
        Step {
            write: vec![
                ("a.rs", "import g ./c greet\nfn run\ncall g"),
                ("c.rs", "fn greet\nexport greet"),
            ],
            remove: vec![],
            cs: changeset(&["c.rs"], &["a.rs"], &[]),
        },
    ];
    assert_tier("import_alias_rebinding", initial, steps);
    let s = run_incremental(initial, steps, LinkScope::Scoped);
    assert!(
        matches!(call_target(&s, "fn:a.rs:run"), EdgeTarget::Node(n) if n.0 == "fn:c.rs:greet"),
        "the aliased import rebinds to c.greet after re-pointing"
    );
}

// ---- tier (e): pub use re-export chain, one module edits ---------------------

#[test]
fn tier_reexport_chain_terminal_edit() {
    // A 2-hop barrel: index → mid → greet. `app` imports greet from ./index and
    // calls it → binds to greet.rs. Editing the TERMINAL module greet.rs (adding a
    // private helper, export table unchanged) makes `greet` a dirty name, so the
    // importer's call re-resolves through the whole barrel chain — back to
    // greet.rs::greet. Convergence must hold under both scopes.
    let initial = &[
        ("index.rs", "reexport greet ./mid greet"),
        ("mid.rs", "reexport greet ./greet greet"),
        ("greet.rs", "fn greet\nexport greet"),
        ("app.rs", "import greet ./index\nfn boot\ncall greet"),
    ];
    let steps = &[Step {
        write: vec![("greet.rs", "fn greet\nexport greet\nfn helper")],
        remove: vec![],
        cs: modified(&["greet.rs"]),
    }];
    assert_tier("reexport_chain", initial, steps);
    let s = run_incremental(initial, steps, LinkScope::Scoped);
    assert!(
        matches!(call_target(&s, "fn:app.rs:boot"), EdgeTarget::Node(n) if n.0 == "fn:greet.rs:greet"),
        "the call resolves through the barrel chain to the real def"
    );
}

// ---- tier (f): manifest edits drive the workspace (ADR-0019/0020) -----------

#[test]
fn tier_manifest_edit_reshapes_the_workspace() {
    // ADR-0042 Phase 1a.4. Manifests were outside every corpus, so `workspace`
    // was empty in every run and its comparison was vacuous — module/project
    // resolution had **no** convergence coverage. Here a manifest is added and
    // then edited, so `Workspace.projects`, the `depends_on` project graph and
    // the derived project overlay all have to converge like anything else.
    let initial = &[
        ("app/Cargo.toml", "[package]\nname = \"app\"\n"),
        ("app/src/main.rs", "fn main\ncall greet"),
        ("lib/Cargo.toml", "[package]\nname = \"lib\"\n"),
        ("lib/src/lib.rs", "fn greet\nexport greet"),
    ];
    let steps = &[
        Step {
            // A new project appears (manifest + a file under it).
            write: vec![
                ("tool/Cargo.toml", "[package]\nname = \"tool\"\n"),
                ("tool/src/main.rs", "fn run"),
            ],
            remove: vec![],
            cs: added(&["tool/Cargo.toml", "tool/src/main.rs"]),
        },
        Step {
            // An existing manifest is EDITED: `app` now declares a dependency on
            // `lib`, which must materialize a `depends_on` edge without any code
            // file changing.
            write: vec![(
                "app/Cargo.toml",
                "[package]\nname = \"app\"\n[dependencies]\nlib = \"1\"\n",
            )],
            remove: vec![],
            cs: modified(&["app/Cargo.toml"]),
        },
    ];
    assert_tier("manifest_workspace", initial, steps);

    let s = run_incremental(initial, steps, LinkScope::Scoped);
    assert_eq!(
        s.workspace.projects.keys().collect::<Vec<_>>(),
        vec!["app", "lib", "tool"],
        "the added manifest became a project on the incremental path"
    );
    assert_eq!(
        s.workspace.projects["app"].deps,
        vec!["lib".to_string()],
        "the manifest EDIT propagated into the project's declared deps"
    );
    assert!(
        s.graph.edges.iter().any(|e| e.relation == "depends_on"
            && e.source.0.contains("app")
            && matches!(&e.target, EdgeTarget::Node(n) if n.0.contains("lib"))),
        "the manifest edit produced the project-graph `depends_on` edge"
    );
}

// ---- the existing relink / convergence contracts, under Scoped --------------
//
// The resolution.rs relink + diverged tests pin Global; these assert the *same*
// contracts hold identically under Scoped (ADR-0042 §1.3 gate list).

#[test]
fn scoped_relink_on_def_addition() {
    // caller calls b (unresolved), then a new module defines b → must relink.
    let initial = &[("caller", "fn a\ncall b")];
    let steps = &[Step {
        write: vec![("lib", "fn b")],
        remove: vec![],
        cs: added(&["lib"]),
    }];
    assert_tier("relink_add", initial, steps);
    let s = run_incremental(initial, steps, LinkScope::Scoped);
    assert!(
        matches!(call_target(&s, "fn:caller:a"), EdgeTarget::Node(n) if n.0 == "fn:lib:b"),
        "adding the def relinks the waiting dependent under Scoped"
    );
}

#[test]
fn scoped_relink_on_def_removal() {
    // caller.a → lib.b resolved; removing b's def must revert to unresolved.
    let initial = &[("caller", "fn a\ncall b"), ("lib", "fn b")];
    let steps = &[Step {
        write: vec![("lib", "fn c")],
        remove: vec![],
        cs: modified(&["lib"]),
    }];
    assert_tier("relink_remove", initial, steps);
    let s = run_incremental(initial, steps, LinkScope::Scoped);
    assert!(
        matches!(call_target(&s, "fn:caller:a"), EdgeTarget::Symbol(r) if r.name == "b"),
        "removing the def reverts the dependent under Scoped"
    );
}

#[test]
fn scoped_incremental_equals_cold_two_file() {
    // The canonical incremental≡cold: caller first (b unresolved), lib arrives.
    let initial = &[("caller", "fn a\ncall b")];
    let steps = &[Step {
        write: vec![("lib", "fn b")],
        remove: vec![],
        cs: added(&["lib"]),
    }];
    // assert_tier already checks both scopes vs cold; keep an explicit name here.
    assert_tier("incremental_equals_cold", initial, steps);
}

#[test]
fn scoped_multi_commit_add_modify_remove() {
    // The §7-style multi-commit sequence (add + modify + remove + boundary
    // relink), the daemon convergence test in miniature, under both scopes.
    let initial = &[
        ("main", "fn main\ncall alpha\ncall beta"),
        ("a", "fn alpha\ncall helper"),
        ("b", "fn helper"),
        ("old", "fn old"),
    ];
    let steps = &[
        Step {
            // add c (defines beta — main's beta() must relink) + modify b.
            write: vec![("c", "fn beta"), ("b", "fn helper\nfn helper2")],
            remove: vec![],
            cs: changeset(&["c"], &["b"], &[]),
        },
        Step {
            // remove old + modify a (adds a local fn).
            write: vec![("a", "fn alpha\ncall helper\nfn extra")],
            remove: vec!["old"],
            cs: changeset(&[], &["a"], &["old"]),
        },
    ];
    assert_tier("multi_commit", initial, steps);
    let s = run_incremental(initial, steps, LinkScope::Scoped);
    assert!(
        s.graph.edges.iter().any(|e| e.relation == "calls"
            && e.source.0 == "fn:main:main"
            && matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:c:beta")),
        "main's beta() relinks to c::beta on the incremental scoped path"
    );
    assert!(
        !s.graph
            .nodes
            .iter()
            .any(|n| n.source_file.as_deref() == Some("old")),
        "removed module is pruned"
    );
}
