//! The installer must not drag the engine into the CLI (ADR-0032f §1).
//!
//! The CLI is engine-free at *link* time, not just at runtime. Adding an
//! install surface is exactly the kind of change that quietly breaks that — a
//! `filigrio-pipeline` import to "just read the manifest" and the thin client
//! is a fat one. So the property is a test, not a review note.
//!
//! The check walks path dependencies through the workspace's own manifests, so
//! it is transitive and needs no `cargo` invocation.
//!
//! **What counts as "the engine".** `filigrio-core` is the shared domain model
//! and already reaches the CLI through `filigrio-protocol`'s wire types — it is
//! not the engine. The engine is the machinery that builds and holds a graph:
//! pipeline, index, store, query, resolve, ingest. Those are what only
//! `filigrio-daemon` may link.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const ENGINE_CRATES: &[&str] = &[
    "filigrio-pipeline",
    "filigrio-index",
    "filigrio-store",
    "filigrio-query",
    "filigrio-resolve",
    "filigrio-ingest",
    "filigrio-classic",
    "filigrio-daemon",
];

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/filigrio-install has a parent")
        .to_path_buf()
}

/// Workspace-internal dependencies of `krate`, read from its manifest's
/// `[dependencies]` / `[dev-dependencies]` are deliberately *excluded*: a dev
/// dependency is not linked into the shipped binary.
fn deps_of(krate: &str) -> BTreeSet<String> {
    let manifest = crates_dir().join(krate).join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("reading {}: {e}", manifest.display()));

    let mut in_deps = false;
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t == "[dependencies]";
            continue;
        }
        if !in_deps || t.starts_with('#') || t.is_empty() {
            continue;
        }
        if let Some(name) = t.split(['=', ' ']).next() {
            if name.starts_with("filigrio-") {
                out.insert(name.to_string());
            }
        }
    }
    out
}

fn transitive(root: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue = vec![root.to_string()];
    while let Some(k) = queue.pop() {
        for d in deps_of(&k) {
            if seen.insert(d.clone()) {
                queue.push(d);
            }
        }
    }
    seen
}

#[test]
fn the_installer_links_no_filigrio_crate_at_all() {
    let deps = deps_of("filigrio-install");
    assert!(
        deps.is_empty(),
        "filigrio-install must stay standalone — it writes files, it does not talk to \
         the daemon and does not model a graph. Found: {deps:?}"
    );
}

#[test]
fn the_cli_stays_engine_free_with_the_installer_attached() {
    let deps = transitive("filigrio-client-cli");
    assert!(
        deps.contains("filigrio-install"),
        "this test is vacuous unless the CLI actually depends on the installer; got {deps:?}"
    );
    for engine in ENGINE_CRATES {
        assert!(
            !deps.contains(*engine),
            "filigrio-client-cli reaches {engine} — the CLI must hold no engine \
             (ADR-0032f §1). Full set: {deps:?}"
        );
    }
}

#[test]
fn the_mcp_bridge_also_stays_engine_free() {
    let deps = transitive("filigrio-client-mcp");
    for engine in ENGINE_CRATES {
        assert!(
            !deps.contains(*engine),
            "filigrio-client-mcp reaches {engine}"
        );
    }
}
