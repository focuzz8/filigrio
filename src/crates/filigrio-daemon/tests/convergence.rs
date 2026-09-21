//! Convergence test — ADR-0032 §7, the acceptance gate for the whole freshness track:
//!
//!   canonical(apply_all(cold_build(c1), diffs(c1→c2→…→cN))) == canonical(cold_build(cN))
//!
//! This is the *identity-level* version (node ids + edge `(source, relation, target)`,
//! not just counts) over a multi-commit sequence exercising **add, modify, remove**, and —
//! critically — **boundary re-resolution**: at c1 `main` calls `beta()` which does not exist
//! (unresolved); c2 adds `beta` in a *new* file WITHOUT re-extracting `main`, so `main`'s
//! edge must relink to the new definition on the incremental path (the reverse-dep property
//! ADR-0032 §5 audited). If incremental ≡ cold, that relink demonstrably happened.
//!
//! Runs against the real pipeline (`FsSource` + `RustExtractor` + `MemoryStore` + `Pipeline`),
//! not the daemon orchestration — the convergence property lives in the engine; the daemon
//! only schedules changesets onto it.

use filigrio_core::{ChangeSet, EdgeTarget};
use filigrio_index::RustExtractor;
use filigrio_ingest::FsSource;
use filigrio_pipeline::Pipeline;
use filigrio_store::MemoryStore;
use std::fs;
use std::path::Path;

/// The ADR-0032 §7 identity comparator now lives in `tests/common/mod.rs`, so
/// the ADR-0042 F4 crash-window test (`write_behind.rs`) is judged by exactly
/// this yardstick rather than a second one invented for the occasion.
mod common;
use common::canonicalize;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// Lay down commit c1: `main` calls `alpha` (a.rs) and `beta` (NOT yet defined →
/// unresolved), `alpha` calls `helper` (b.rs), plus an `old.rs` to be removed later.
fn write_c1(root: &Path) {
    write(root, "Cargo.toml", "[package]\nname = \"app\"\n");
    write(
        root,
        "src/main.rs",
        "fn main() {\n    alpha();\n    beta();\n}\n",
    );
    write(root, "src/a.rs", "fn alpha() {\n    helper();\n}\n");
    write(root, "src/b.rs", "fn helper() {}\n");
    write(root, "src/old.rs", "fn old() {}\n");
}

#[test]
fn convergence_incremental_equals_cold_identity_level() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();

    // ---- incremental path: cold-build c1, then apply c1→c2 and c2→c3 deltas ----
    write_c1(root);
    let source = FsSource::new(root);
    let extractor = RustExtractor::new();
    let store = MemoryStore::new();
    let pipeline = Pipeline::new(&source, &extractor, &store);
    pipeline.build().expect("cold build c1");

    // c1 → c2: ADD c.rs (defines `beta`) + MODIFY b.rs. `main` is deliberately NOT in
    // the changeset — its unresolved `beta()` must relink to c.rs::beta anyway (reverse-dep).
    write(root, "src/c.rs", "fn beta() {}\n");
    write(root, "src/b.rs", "pub fn helper() {}\n");
    pipeline
        .apply(
            &store.current().unwrap(),
            &ChangeSet {
                added: vec!["src/c.rs".into()],
                modified: vec!["src/b.rs".into()],
                removed: vec![],
            },
        )
        .expect("apply c1→c2");

    // c2 → c3: REMOVE old.rs + MODIFY a.rs (adds a local fn).
    fs::remove_file(root.join("src/old.rs")).unwrap();
    write(
        root,
        "src/a.rs",
        "fn alpha() {\n    helper();\n}\nfn extra() {}\n",
    );
    pipeline
        .apply(
            &store.current().unwrap(),
            &ChangeSet {
                added: vec![],
                modified: vec!["src/a.rs".into()],
                removed: vec!["src/old.rs".into()],
            },
        )
        .expect("apply c2→c3");

    let incremental = canonicalize(&store.current().unwrap());

    // ---- cold path: build the final on-disk tree (c3) from scratch ----
    let cold_store = MemoryStore::new();
    Pipeline::new(&FsSource::new(root), &extractor, &cold_store)
        .build()
        .expect("cold build c3");
    let cold = canonicalize(&cold_store.current().unwrap());

    assert_eq!(
        incremental, cold,
        "incremental (c1 →c2 →c3) diverged from a cold build of c3.\n\
         --- INCREMENTAL ---\n{incremental}\n\n--- COLD ---\n{cold}"
    );

    // Guard the reverse-dep relink explicitly: `main`'s `beta()` call resolved to a
    // node (c.rs::beta), it is not left as an unresolved Symbol — proving c2's add
    // relinked an edge in the unchanged main.rs.
    assert!(
        store.current().unwrap().graph.edges.iter().any(|e| {
            e.relation == "calls"
                && e.source.0.contains("main.rs")
                && matches!(&e.target, EdgeTarget::Node(id) if id.0.contains("c.rs") && id.0.ends_with("beta"))
        }),
        "main's beta() call must relink to c.rs::beta on the incremental path"
    );

    // And old.rs is gone from the incremental graph (remove propagated, no dangle).
    assert!(
        !store
            .current()
            .unwrap()
            .graph
            .nodes
            .iter()
            .any(|n| n.source_file.as_deref() == Some("src/old.rs")),
        "removed old.rs must be pruned from the incremental graph"
    );
}

/// Test §7 rename variant: fileA → fileB rename must converge correctly.
///
/// ADR-0032 §7 explicitly requires renames (not just deletes). This test ensures
/// that when a file is renamed (old.rs → renamed.rs) and content changes, the
/// incremental path produces the same graph as a cold build.
#[test]
fn convergence_rename_variant() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();

    // Initial state: main.rs calls old()
    write(root, "Cargo.toml", "[package]\nname = \"app\"\n");
    write(root, "src/main.rs", "fn main() {\n    old();\n}\n");
    write(root, "src/old.rs", "fn old() {}\n");

    let source = FsSource::new(root);
    let extractor = RustExtractor::new();
    let store = MemoryStore::new();
    let pipeline = Pipeline::new(&source, &extractor, &store);
    pipeline.build().expect("cold build initial");

    // Rename old.rs → renamed.rs with content change
    fs::remove_file(root.join("src/old.rs")).unwrap();
    write(root, "src/renamed.rs", "fn renamed() {}\n");
    write(root, "src/main.rs", "fn main() {\n    renamed();\n}\n");

    // Apply changeset: remove old.rs, add renamed.rs, modify main.rs
    pipeline
        .apply(
            &store.current().unwrap(),
            &ChangeSet {
                added: vec!["src/renamed.rs".into()],
                modified: vec!["src/main.rs".into()],
                removed: vec!["src/old.rs".into()],
            },
        )
        .expect("apply rename changeset");

    let incremental = canonicalize(&store.current().unwrap());

    // Cold build of final state
    let cold_store = MemoryStore::new();
    Pipeline::new(&FsSource::new(root), &extractor, &cold_store)
        .build()
        .expect("cold build final");
    let cold = canonicalize(&cold_store.current().unwrap());

    assert_eq!(
        incremental, cold,
        "rename (old.rs → renamed.rs) diverged incremental from cold build.\n\
         --- INCREMENTAL ---\n{incremental}\n\n--- COLD ---\n{cold}"
    );

    // Verify old.rs is gone and renamed.rs exists
    assert!(
        !store
            .current()
            .unwrap()
            .graph
            .nodes
            .iter()
            .any(|n| n.source_file.as_deref() == Some("src/old.rs")),
        "old.rs must be removed after rename"
    );
    assert!(
        store
            .current()
            .unwrap()
            .graph
            .nodes
            .iter()
            .any(|n| n.source_file.as_deref() == Some("src/renamed.rs")),
        "renamed.rs must exist after rename"
    );
}
