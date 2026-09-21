//! ADR-0042 **Phase 1c / F8 — vanished-file races converge, they don't abort.**
//!
//! A changeset is built by walking the tree and applied some time later; at
//! next.js scale that window is seconds to tens of seconds. A branch switch, a
//! `cargo clean`, or an editor temp file inside it deletes a path the changeset
//! still names. Before F8 the engine read every `to_index()` path with `?`, so
//! one missing file aborted the **whole** apply — every other valid file in the
//! batch included, leaving the graph both stale *and* still holding a node for
//! the deleted file.
//!
//! The contract these tests pin:
//!
//! | at read time | verdict |
//! |---|---|
//! | `exists() == false` | fold into **removals** — a cold build of the tree as it now stands has no such file, so this is what `incremental ≡ cold` requires |
//! | `exists() == false`, never in the prior manifest | **no-op** (created *and* deleted inside the window) |
//! | `exists() == true`, `read()` fails | **hard error** — permission/IO is not absence |
//!
//! The vanished count is reported (`GraphDelta::vanished`), never silent
//! (ADR-0029 honesty: "silently converged" must not become the new silent
//! failure).

mod common;

use common::{added, changeset, cold_scope, modified, ToyExtractor, ToySource};
use filigrio_core::{ChangeSet, GraphState, GraphStore, Source};
use filigrio_resolve::{ClusterConfig, Engine, LinkScope};
use filigrio_store::MemoryStore;

/// `Engine::apply_with_scope` without the harness's `expect` — these tests are
/// about whether the apply succeeds at all.
fn try_apply(
    prior: &GraphState,
    cs: &ChangeSet,
    src: &ToySource,
    ext: &ToyExtractor,
) -> filigrio_core::Result<filigrio_core::GraphDelta> {
    Engine::apply_with_scope(
        prior,
        cs,
        src,
        ext,
        &ClusterConfig::default(),
        LinkScope::Global,
    )
}

/// Cold-build `files` into a fresh store and return `(store, source, extractor)`.
fn seeded(files: &[(&str, &str)]) -> (MemoryStore, ToySource, ToyExtractor) {
    let src = ToySource::new(files);
    let ext = ToyExtractor::new();
    let store = MemoryStore::new();
    let cs = added(&files.iter().map(|(p, _)| *p).collect::<Vec<_>>());
    let delta = try_apply(&GraphState::default(), &cs, &src, &ext).expect("cold build");
    store.apply_delta(&delta).expect("store");
    (store, src, ext)
}

fn files_with_nodes(state: &GraphState) -> Vec<String> {
    let mut v: Vec<String> = state
        .graph
        .nodes
        .iter()
        .filter_map(|n| n.source_file.clone())
        .collect();
    v.sort();
    v.dedup();
    v
}

// ---- 1. the batch survives ---------------------------------------------------

/// The headline: A, B, C are scheduled; B is deleted before the apply reads it.
/// The apply must succeed and index A and C — not abort the batch.
#[test]
fn vanished_file_does_not_abort_the_batch() {
    let src = ToySource::new(&[("a.rs", "fn a\n"), ("b.rs", "fn b\n"), ("c.rs", "fn c\n")]);
    let ext = ToyExtractor::new();
    let cs = added(&["a.rs", "b.rs", "c.rs"]);

    // The race: b.rs disappears between changeset construction and the apply.
    src.remove("b.rs");

    let delta = try_apply(&GraphState::default(), &cs, &src, &ext)
        .expect("a vanished file must converge, not abort the whole batch");

    let labels: Vec<&str> = delta.nodes_added.iter().map(|n| n.label.as_str()).collect();
    assert!(
        labels.contains(&"a"),
        "a.rs must still be indexed: {labels:?}"
    );
    assert!(
        labels.contains(&"c"),
        "c.rs must still be indexed: {labels:?}"
    );
    assert!(
        !delta
            .nodes_added
            .iter()
            .any(|n| n.source_file.as_deref() == Some("b.rs")),
        "the vanished file must contribute nothing: {labels:?}"
    );
    assert!(
        !delta.manifest.entries.contains_key("b.rs"),
        "a vanished file must not get a manifest entry"
    );
    assert_eq!(delta.vanished, 1, "the vanished count must be reported");
}

// ---- 2. incremental ≡ cold ---------------------------------------------------

/// The convergence claim: after the batch above, the state equals a cold build
/// of the tree **as it now stands** (a.rs + c.rs).
#[test]
fn vanished_apply_equals_cold_build_of_the_current_tree() {
    let src = ToySource::new(&[
        ("a.rs", "fn a\ncall c\n"),
        ("b.rs", "fn b\n"),
        ("c.rs", "fn c\n"),
    ]);
    let ext = ToyExtractor::new();
    let store = MemoryStore::new();
    let cs = added(&["a.rs", "b.rs", "c.rs"]);
    src.remove("b.rs");

    let delta = try_apply(&GraphState::default(), &cs, &src, &ext).expect("apply");
    store.apply_delta(&delta).expect("store");

    let cold = cold_scope(
        &[("a.rs", "fn a\ncall c\n"), ("c.rs", "fn c\n")],
        LinkScope::Global,
    );
    common::assert_state_eq(
        &store.current().unwrap(),
        &cold,
        "apply-with-vanished ≢ cold build of the current tree",
    );
}

// ---- 3. prior state is cleaned ----------------------------------------------

/// A file that *was* indexed and then vanishes mid-apply must have its nodes,
/// edges and manifest entry dropped — exactly as a normal removal would — not
/// left behind as a stale node.
#[test]
fn previously_indexed_vanished_file_is_cleaned_up() {
    let (store, src, ext) = seeded(&[
        ("a.rs", "fn a\ncall b\n"),
        ("b.rs", "fn b\n"),
        ("c.rs", "fn c\n"),
    ]);
    let before = store.current().unwrap();
    assert!(
        files_with_nodes(&before).contains(&"b.rs".to_string()),
        "precondition: b.rs was indexed"
    );
    assert!(before.manifest.entries.contains_key("b.rs"));

    // The changeset says "b.rs changed"; by the time the engine reads it, it is gone.
    src.remove("b.rs");
    let delta = try_apply(&before, &modified(&["b.rs"]), &src, &ext).expect("apply must converge");
    store.apply_delta(&delta).expect("store");
    let after = store.current().unwrap();

    assert_eq!(delta.vanished, 1);
    assert!(
        !files_with_nodes(&after).contains(&"b.rs".to_string()),
        "the vanished file's nodes must be dropped: {:?}",
        files_with_nodes(&after)
    );
    assert!(
        !after.manifest.entries.contains_key("b.rs"),
        "the vanished file's manifest entry must be dropped"
    );
    assert!(
        !after
            .graph
            .edges
            .iter()
            .any(|e| e.source.0.contains("b.rs")),
        "no edge may still be sourced from the vanished file"
    );
    common::assert_resolution_eq(
        &after,
        &cold_scope(
            &[("a.rs", "fn a\ncall b\n"), ("c.rs", "fn c\n")],
            LinkScope::Global,
        ),
        "cleanup-after-vanish ≢ cold build of the current tree",
    );
}

// ---- 4. the ADR's open sub-case: created *and* deleted inside the window -----

/// A path in `added` that was **never** in the prior manifest and does not exist
/// at read time (an editor temp file, a build artifact) must be a pure no-op:
/// no error, no spurious removal, state otherwise unchanged.
#[test]
fn created_and_deleted_inside_the_window_is_a_noop() {
    let (store, src, ext) = seeded(&[("a.rs", "fn a\n"), ("c.rs", "fn c\n")]);
    let before = store.current().unwrap();

    // `.tmp.rs` was created and deleted inside the walk→apply window: the
    // changeset names it, the manifest never knew it, and it is gone now.
    assert!(!Source::exists(&src, ".tmp.rs"));
    let delta = try_apply(&before, &added(&[".tmp.rs"]), &src, &ext)
        .expect("a never-indexed vanished path must be a no-op, not an error");
    store.apply_delta(&delta).expect("store");
    let after = store.current().unwrap();

    assert_eq!(delta.vanished, 1, "it is still reported as vanished");
    assert!(
        delta.nodes_added.iter().all(|n| n.kind == "project"),
        "no nodes may be added for a path that never existed"
    );
    assert!(
        !delta
            .nodes_removed
            .iter()
            .any(|id| id.0.contains(".tmp.rs")),
        "no spurious removal: {:?}",
        delta.nodes_removed
    );
    assert!(!after.manifest.entries.contains_key(".tmp.rs"));
    common::assert_state_eq(&after, &before, "a no-op apply changed the state");
}

// ---- 5. exists-but-unreadable stays a hard error ------------------------------

/// The split's other half. `exists() == true` + a failing `read()` is a
/// permission/IO fault, **not** absence: it must still abort the apply. If this
/// ever passes, the F8 fold has started swallowing real read errors.
#[test]
fn exists_but_unreadable_is_still_a_hard_error() {
    let (store, src, ext) = seeded(&[("a.rs", "fn a\n"), ("b.rs", "fn b\n")]);
    let before = store.current().unwrap();

    src.poison("b.rs"); // chmod 000: still there, cannot be read
    assert!(
        Source::exists(&src, "b.rs"),
        "precondition: it still exists"
    );

    let err = try_apply(&before, &modified(&["b.rs"]), &src, &ext)
        .expect_err("an unreadable-but-present file must not be silently converged away");
    assert!(
        format!("{err}").contains("permission denied"),
        "the underlying read error must propagate verbatim, got: {err}"
    );
    assert_eq!(
        store.current().unwrap().graph.nodes.len(),
        before.graph.nodes.len(),
        "nothing was written"
    );
}

/// The same file, now genuinely gone: the *only* difference is `exists()`, which
/// is what makes this the port-method split rather than `read().is_ok()`.
#[test]
fn absence_and_unreadability_differ_only_by_exists() {
    let make = || seeded(&[("a.rs", "fn a\n"), ("b.rs", "fn b\n")]);

    let (store, src, ext) = make();
    let prior = store.current().unwrap();
    src.poison("b.rs");
    assert!(try_apply(&prior, &modified(&["b.rs"]), &src, &ext).is_err());

    let (store, src, ext) = make();
    let prior = store.current().unwrap();
    src.remove("b.rs");
    assert!(try_apply(&prior, &modified(&["b.rs"]), &src, &ext).is_ok());
}

// ---- 6. the manifest/workspace pass ------------------------------------------

/// The second `?`-read (the ADR's `:672`): a project manifest deleted mid-apply
/// must not abort either, and must drop its project from the `Workspace` exactly
/// as a normal manifest removal does.
#[test]
fn vanished_project_manifest_converges_as_a_removal() {
    let (store, src, ext) = seeded(&[
        ("pkg/package.json", r#"{"name":"@acme/pkg"}"#),
        ("pkg/a.rs", "fn a\n"),
        ("app/a.rs", "fn app\n"),
    ]);
    let before = store.current().unwrap();
    assert!(
        before.workspace.projects.contains_key("pkg"),
        "precondition: pkg is a project: {:?}",
        before.workspace.projects.keys().collect::<Vec<_>>()
    );

    src.remove("pkg/package.json");
    let delta = try_apply(&before, &modified(&["pkg/package.json"]), &src, &ext)
        .expect("a vanished manifest must converge, not abort");
    store.apply_delta(&delta).expect("store");
    let after = store.current().unwrap();

    assert_eq!(delta.vanished, 1);
    assert!(
        !after.workspace.projects.contains_key("pkg"),
        "the vanished manifest's project must be dropped: {:?}",
        after.workspace.projects.keys().collect::<Vec<_>>()
    );
    common::assert_resolution_eq(
        &after,
        &cold_scope(
            &[("pkg/a.rs", "fn a\n"), ("app/a.rs", "fn app\n")],
            LinkScope::Global,
        ),
        "vanished-manifest apply ≢ cold build of the current tree",
    );
}

// ---- 7. mixed batches --------------------------------------------------------

/// Vanished paths mix with explicit removals without double-counting or losing
/// either, under both link scopes (`Scoped` must fold identically to `Global` —
/// the fold happens before the scope split).
#[test]
fn vanished_and_explicit_removals_coexist_under_both_scopes() {
    for scope in [LinkScope::Global, LinkScope::Scoped] {
        let src = ToySource::new(&[
            ("a.rs", "fn a\n"),
            ("b.rs", "fn b\n"),
            ("c.rs", "fn c\n"),
            ("d.rs", "fn d\n"),
        ]);
        let ext = ToyExtractor::new();
        let store = MemoryStore::new();
        let cold_cs = added(&["a.rs", "b.rs", "c.rs", "d.rs"]);
        let delta = Engine::apply_with_scope(
            &GraphState::default(),
            &cold_cs,
            &src,
            &ext,
            &ClusterConfig::default(),
            scope,
        )
        .expect("cold");
        store.apply_delta(&delta).expect("store");

        // d.rs is an announced removal; b.rs vanishes unannounced.
        src.remove("d.rs");
        src.remove("b.rs");
        let prior = store.current().unwrap();
        let cs = changeset(&[], &["a.rs", "b.rs"], &["d.rs"]);
        let delta =
            Engine::apply_with_scope(&prior, &cs, &src, &ext, &ClusterConfig::default(), scope)
                .expect("mixed batch must converge");
        store.apply_delta(&delta).expect("store");

        assert_eq!(delta.vanished, 1, "{scope:?}: only b.rs vanished");
        common::assert_resolution_eq(
            &store.current().unwrap(),
            &cold_scope(&[("a.rs", "fn a\n"), ("c.rs", "fn c\n")], LinkScope::Global),
            &format!("{scope:?}: mixed vanished+removed ≢ cold"),
        );
    }
}
