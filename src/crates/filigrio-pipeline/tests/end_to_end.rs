//! Walking-skeleton proof: the whole pipeline wires together end to end over
//! mock adapters. Ingest (real fs walk) → index (mock) → resolve (mock apply)
//! → store (mock) → query (mock). No parsing, no real storage — just wiring.

use filigrio_core::{Direction, Edge, GraphQuery, Node, Source};
use filigrio_index::{MockExtractor, RustExtractor};
use filigrio_ingest::FsSource;
use filigrio_pipeline::Pipeline;
use filigrio_query::GraphView;
use filigrio_store::MemoryStore;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

/// Neighbors of the (uniquely-labelled, in these fixtures) node `label` — resolve
/// the label to its id, then address by id (the port is id-keyed; ADR-0027).
fn neighbors_of(view: &GraphView, label: &str, rel: Option<&str>) -> Vec<(Edge, Node)> {
    let id = view
        .nodes_by_label(label)
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no node labelled {label}"))
        .id
        .0;
    // `rel` is this helper's single-relation convenience; the port takes the
    // filter *set*, of which absent = empty.
    let filter: Vec<String> = rel.map(str::to_string).into_iter().collect();
    view.neighbors_by_id(&id, &filter, Direction::Out).unwrap()
}

/// Create a tiny two-file "repo" in a unique temp dir under the target dir.
fn fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("filigrio-skeleton-{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("src/a.rs"),
        "fn alpha() {\n    beta();\n}\nfn helper() {}\n",
    )
    .unwrap();
    fs::write(dir.join("src/b.rs"), "fn beta() {\n    helper();\n}\n").unwrap();
    // noise that must be ignored by the source walk
    fs::create_dir_all(dir.join("target")).unwrap();
    fs::write(dir.join("target/junk.rs"), "fn should_be_ignored() {}").unwrap();
    dir
}

#[test]
fn ingest_walks_and_ignores() {
    let dir = fixture("walk");
    let source = FsSource::new(&dir);
    let changes = source.poll(None).unwrap();
    // two real files, target/ skipped.
    assert_eq!(
        changes.added.len(),
        2,
        "should find exactly the 2 src files"
    );
    assert!(changes.added.iter().all(|p| !p.contains("target")));
    assert!(changes.added.iter().any(|p| p.ends_with("a.rs")));
}

#[test]
fn full_pipeline_builds_and_queries() {
    let dir = fixture("full");
    let source = FsSource::new(&dir);
    let extractor = MockExtractor::new();
    let store = MemoryStore::new();

    let pipeline = Pipeline::new(&source, &extractor, &store);
    let report = pipeline.build().unwrap();
    assert_eq!(report.changed, 2);
    assert!(report.nodes_added >= 2, "at least the two file nodes");

    let view = GraphView::new(Arc::new(store.current().unwrap()));
    let stats = view.stats().unwrap();
    // 2 files + alpha/helper/beta = 5 nodes. Real (Phase-2b) clustering is
    // structural, not directory-based: modularity splits the two files into
    // two communities even though they call across (the old mock gave 1).
    assert_eq!(stats.nodes, 5, "2 files + 3 functions");
    assert_eq!(
        stats.communities, 2,
        "modularity clusters by structure, not by dir"
    );

    // The cross-file call alpha -> beta must have LINKED (Symbol -> Node).
    let ns = neighbors_of(&view, "alpha", Some("calls"));
    assert!(
        ns.iter().any(|(_, n)| n.label == "beta"),
        "alpha's call to beta should resolve across files"
    );
}

#[test]
fn rust_call_binds_through_pub_use_reexport() {
    // End-to-end (real RustExtractor + real resolver, ADR-0020 gap closed):
    // `consumer` imports `greet` from a `prelude` barrel that `pub use`s it from
    // `api`, where it is defined. The call `boot() -> greet()` must bind to
    // `api.rs`'s `greet`, following the mod-path re-export across three files.
    let dir = std::env::temp_dir().join("filigrio-rust-reexport");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    // A Cargo.toml makes this a crate/project (so `crate::` resolves — ADR-0018).
    fs::write(dir.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
    fs::write(dir.join("src/lib.rs"), "pub mod api;\npub mod prelude;\n").unwrap();
    fs::write(dir.join("src/api.rs"), "pub fn greet() {}\n").unwrap();
    // The barrel: re-export greet by its mod-path.
    fs::write(dir.join("src/prelude.rs"), "pub use crate::api::greet;\n").unwrap();
    // The consumer: import through the barrel, then call.
    fs::write(
        dir.join("src/consumer.rs"),
        "use crate::prelude::greet;\nfn boot() { greet(); }\n",
    )
    .unwrap();

    let source = FsSource::new(&dir);
    let store = MemoryStore::new();
    Pipeline::new(&source, &RustExtractor::new(), &store)
        .build()
        .unwrap();

    let view = GraphView::new(Arc::new(store.current().unwrap()));
    let ns = neighbors_of(&view, "boot", Some("calls"));
    let greet = ns
        .iter()
        .find(|(_, n)| n.label == "greet")
        .expect("boot's call to greet must resolve, not stay unresolved");
    assert_eq!(
        greet.1.source_file.as_deref(),
        Some("src/api.rs"),
        "greet binds to its real definition in api.rs (through the prelude barrel), not the re-export site",
    );
    assert_eq!(greet.0.confidence, filigrio_core::Confidence::Extracted);
}

#[test]
fn rust_use_resolves_vs_module_not_file() {
    // The "negative zone": two modules define the SAME name `greet`. Bare
    // name-deduction (resolve-vs-file/project) is genuinely AMBIGUOUS here — it
    // cannot tell which `greet` a call means. The explicit `use crate::api::greet`
    // resolves vs the MODULE (export table), binding `boot()`'s call to *api*'s
    // greet specifically — the disambiguation only module-scoped resolution can do.
    let dir = std::env::temp_dir().join("filigrio-rust-module-scope");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        "pub mod api;\npub mod other;\npub mod handlers;\n",
    )
    .unwrap();
    fs::write(dir.join("src/api.rs"), "pub fn greet() -> u32 { 1 }\n").unwrap();
    // A homonym in a different module — the thing name-deduction can't tell apart.
    fs::write(dir.join("src/other.rs"), "pub fn greet() -> u32 { 2 }\n").unwrap();
    fs::write(
        dir.join("src/handlers.rs"),
        "use crate::api::greet;\nfn boot() -> u32 { greet() }\n",
    )
    .unwrap();

    let source = FsSource::new(&dir);
    let store = MemoryStore::new();
    Pipeline::new(&source, &RustExtractor::new(), &store)
        .build()
        .unwrap();

    let view = GraphView::new(Arc::new(store.current().unwrap()));
    let ns = neighbors_of(&view, "boot", Some("calls"));
    let greet = ns
        .iter()
        .find(|(_, n)| n.label == "greet")
        .expect("boot's call to greet must resolve");
    assert_eq!(
        greet.1.source_file.as_deref(),
        Some("src/api.rs"),
        "the `use crate::api::greet` binds to api's greet, NOT other's homonym",
    );
    // Certain, because the import names exactly which module — not the AMBIGUOUS
    // a bare `greet()` with two candidate defs would have produced.
    assert_eq!(greet.0.confidence, filigrio_core::Confidence::Extracted);
}

/// Does the store's current graph hold a node from `path`?
fn has_nodes_from(store: &MemoryStore, path: &str) -> bool {
    store
        .current()
        .unwrap()
        .graph
        .nodes
        .iter()
        .any(|n| n.source_file.as_deref() == Some(path))
}

#[test]
fn deleted_file_is_pruned_on_rebuild() {
    // Snapshot reconcile (ADR-0022): a file that leaves the walk (here: deleted)
    // is pruned on the next build, even though `poll` only ever reports adds.
    let dir = std::env::temp_dir().join("filigrio-prune-delete");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/a.rs"), "fn a() {}\n").unwrap();
    fs::write(dir.join("src/b.rs"), "fn b() {}\n").unwrap();
    let source = FsSource::new(&dir);
    let store = MemoryStore::new();
    Pipeline::new(&source, &RustExtractor::new(), &store)
        .build()
        .unwrap();
    assert!(has_nodes_from(&store, "src/b.rs"));

    fs::remove_file(dir.join("src/b.rs")).unwrap();
    Pipeline::new(&source, &RustExtractor::new(), &store)
        .build()
        .unwrap();
    assert!(!has_nodes_from(&store, "src/b.rs"), "deleted file pruned");
    assert!(has_nodes_from(&store, "src/a.rs"), "a.rs survives");
}

#[test]
fn newly_ignored_file_is_pruned_on_rebuild() {
    // The streaming concern: editing `.gitignore` to ignore an already-indexed
    // file prunes it — the file's own bytes never changed, only the rules did.
    let dir = std::env::temp_dir().join("filigrio-prune-ignore");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/keep.rs"), "fn a() {}\n").unwrap();
    fs::write(dir.join("src/gen.rs"), "fn g() {}\n").unwrap();
    let source = FsSource::new(&dir);
    let store = MemoryStore::new();
    Pipeline::new(&source, &RustExtractor::new(), &store)
        .build()
        .unwrap();
    assert!(has_nodes_from(&store, "src/gen.rs"));

    fs::write(dir.join(".gitignore"), "gen.rs\n").unwrap();
    Pipeline::new(&source, &RustExtractor::new(), &store)
        .build()
        .unwrap();
    assert!(
        !has_nodes_from(&store, "src/gen.rs"),
        "newly-ignored file pruned"
    );
    assert!(has_nodes_from(&store, "src/keep.rs"));
}

#[test]
fn newly_unignored_file_is_indexed_on_rebuild() {
    // The reverse direction (added, not pruned): removing a `.gitignore` pattern
    // un-ignores a file, which must now be indexed — again with no change to the
    // file itself.
    let dir = std::env::temp_dir().join("filigrio-unignore");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/keep.rs"), "fn a() {}\n").unwrap();
    fs::write(dir.join("src/gen.rs"), "fn g() {}\n").unwrap();
    fs::write(dir.join(".gitignore"), "gen.rs\n").unwrap();
    let source = FsSource::new(&dir);
    let store = MemoryStore::new();
    Pipeline::new(&source, &RustExtractor::new(), &store)
        .build()
        .unwrap();
    assert!(!has_nodes_from(&store, "src/gen.rs"), "starts ignored");

    fs::write(dir.join(".gitignore"), "\n").unwrap();
    Pipeline::new(&source, &RustExtractor::new(), &store)
        .build()
        .unwrap();
    assert!(
        has_nodes_from(&store, "src/gen.rs"),
        "newly-un-ignored file indexed"
    );
}

#[test]
fn constructor_calls_follow_the_type_not_the_same_file_homonym() {
    // Ground-truth-derived from reflex_monorepo (the big-repo oracle diff). A file
    // defines its OWN `new`, but a method constructs OTHER types: `Vec::new()`
    // (external) and `TelemetryState::new()` (another file). Correct Rust
    // semantics: the calls follow the *type* — `TelemetryState::new` → helpers.rs,
    // `Vec::new` → unresolved (we don't index `Vec`). Neither may bind to THIS
    // file's `new`.
    //
    // The Python oracle over-binds BOTH to the same-file `new` via a
    // same-file-constructor heuristic (how it reaches 0 unresolved calls) — which
    // is wrong here (`default()` in bytetrack never calls `ByteTracker::new`; it
    // fills a struct literal with `Vec::new()`). This test PINS our correct,
    // type-directed behavior so that heuristic is never adopted as a "fix".
    let dir = std::env::temp_dir().join("filigrio-ctor-homonym");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("src/helpers.rs"),
        "pub struct TelemetryState;\nimpl TelemetryState { pub fn new() -> Self { TelemetryState } }\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/backend.rs"),
        "pub struct Backend;\n\
         impl Backend {\n\
         \x20   pub fn new() -> Self { Backend }\n\
         \x20   pub fn open(&self) {\n\
         \x20       let _t = TelemetryState::new();\n\
         \x20       let _v: Vec<u8> = Vec::new();\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();

    let source = FsSource::new(&dir);
    let store = MemoryStore::new();
    Pipeline::new(&source, &RustExtractor::new(), &store)
        .build()
        .unwrap();

    let state = store.current().unwrap();
    let open_new_targets: Vec<&filigrio_core::EdgeTarget> = state
        .graph
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source.0.ends_with("backend.rs:Backend::open"))
        .map(|e| &e.target)
        .collect();

    // The `TelemetryState::new()` call binds cross-file to helpers.rs's `new`.
    assert!(
        open_new_targets.iter().any(|t| matches!(
            t,
            filigrio_core::EdgeTarget::Node(n) if n.0 == "fn:src/helpers.rs:TelemetryState::new"
        )),
        "TelemetryState::new() follows the type to helpers.rs::new: {open_new_targets:?}"
    );
    // It must NOT bind to the same-file `Backend::new` (the oracle's mistake).
    assert!(
        !open_new_targets.iter().any(|t| matches!(
            t,
            filigrio_core::EdgeTarget::Node(n) if n.0 == "fn:src/backend.rs:Backend::new"
        )),
        "no constructor call binds to the same-file homonym Backend::new: {open_new_targets:?}"
    );
    // `Vec::new()` — a type we don't define — stays honestly unresolved, not bound
    // to any local `new`.
    assert!(
        open_new_targets.iter().any(|t| matches!(
            t,
            filigrio_core::EdgeTarget::Symbol(s) if s.name == "new"
        )),
        "Vec::new() stays an unresolved Symbol: {open_new_targets:?}"
    );
}

#[test]
fn polled_changeset_applies_as_an_explicit_changeset() {
    // Formerly the ADR-0015 worker/queue test; the daemon (ADR-0032) superseded
    // that layer. What it actually asserted survives: a polled changeset, applied
    // as an explicit changeset (not via `build`), produces the full graph.
    let dir = fixture("worker");
    let source = FsSource::new(&dir);
    let extractor = MockExtractor::new();
    let store = MemoryStore::new();

    let changes = source.poll(None).unwrap();
    let pipeline = Pipeline::new(&source, &extractor, &store);
    pipeline.apply(&store.current().unwrap(), &changes).unwrap();

    let view = GraphView::new(Arc::new(store.current().unwrap()));
    assert_eq!(view.stats().unwrap().nodes, 5);
}

#[test]
fn idempotent_reapply() {
    // Applying the same cold build twice must not double the graph
    // (idempotency, HLD §11.4).
    let dir = fixture("idem");
    let source = FsSource::new(&dir);
    let extractor = MockExtractor::new();
    let store = MemoryStore::new();
    let pipeline = Pipeline::new(&source, &extractor, &store);

    pipeline.build().unwrap();
    let first = store.current().unwrap().graph;
    pipeline.build().unwrap();
    let second = store.current().unwrap().graph;
    assert_eq!(
        first.nodes.len(),
        second.nodes.len(),
        "nodes must be idempotent"
    );
    assert_eq!(
        first.edges.len(),
        second.edges.len(),
        "edges must be idempotent"
    );
}

#[test]
fn incremental_modify_is_bounded() {
    // Editing one file re-applies as a `modified` changeset and converges to
    // the same graph a cold build would produce (Kappa equivalence, HLD §11.1).
    use filigrio_core::ChangeSet;

    let dir = fixture("incr");
    let source = FsSource::new(&dir);
    let extractor = MockExtractor::new();
    let store = MemoryStore::new();
    let pipeline = Pipeline::new(&source, &extractor, &store);

    pipeline.build().unwrap();
    let before = store.current().unwrap().graph.nodes.len();

    // Add a new function to b.rs, then re-apply just that file as `modified`.
    fs::write(
        dir.join("src/b.rs"),
        "fn beta() {\n    helper();\n}\nfn gamma() {}\n",
    )
    .unwrap();
    let changes = ChangeSet {
        added: vec![],
        modified: vec!["src/b.rs".to_string()],
        removed: vec![],
    };
    let report = pipeline.apply(&store.current().unwrap(), &changes).unwrap();
    assert_eq!(report.changed, 1, "only one file in the changeset");

    let after = store.current().unwrap().graph.nodes.len();
    assert_eq!(after, before + 1, "exactly one new function node (gamma)");

    // Convergence: a cold build of the final tree yields the same node count.
    let cold_store = MemoryStore::new();
    Pipeline::new(&source, &extractor, &cold_store)
        .build()
        .unwrap();
    assert_eq!(
        store.current().unwrap().graph.nodes.len(),
        cold_store.current().unwrap().graph.nodes.len(),
        "incremental result converges to a cold build"
    );
}

#[test]
fn cluster_config_threads_through_pipeline_and_emits_cohesion() {
    // ADR-0024: a non-default clustering config (Full strategy + Confidence
    // weighting) must thread Pipeline → Engine::apply → cluster, and the resulting
    // partition must carry the per-community cohesion score.
    use filigrio_pipeline::ClusterConfig;

    let dir = fixture("clustercfg");
    let source = FsSource::new(&dir);
    let extractor = MockExtractor::new();
    let store = MemoryStore::new();

    let report = Pipeline::new(&source, &extractor, &store)
        .with_cluster_config(ClusterConfig::full())
        .build()
        .expect("full-strategy build");
    assert_eq!(report.changed, 2);

    let state = store.current().unwrap();
    assert!(
        !state.partition.communities.is_empty(),
        "Full strategy still produces communities"
    );
    // Cohesion is emitted for every community (ADR-0024) — disjoint files score
    // fully cohesive; the point is the field is populated end-to-end.
    for meta in state.partition.communities.values() {
        assert!(
            meta.cohesion_permille > 0,
            "community {} carries a cohesion score",
            meta.id.0
        );
        assert!(meta.cohesion() > 0.0 && meta.cohesion() <= 1.0);
    }
}

#[test]
fn apply_writes_state_but_not_graph_json() {
    // ADR-0042 Phase 1c F2/B4: `graph.json` has zero production readers, so the
    // apply path must NOT pay a full interchange rewrite per apply. Producing it
    // is an explicit export (`GraphStore::snapshot`), which must match what
    // `graphjson::export` yields from the current state.
    use filigrio_core::GraphStore;
    use filigrio_store::{graphjson, FsStore};

    let dir = fixture("nosnap");
    let out = std::env::temp_dir().join("filigrio-nosnap-store");
    let _ = fs::remove_dir_all(&out);
    fs::create_dir_all(&out).unwrap();

    let source = FsSource::new(&dir);
    let extractor = MockExtractor::new();
    let store = FsStore::new(&out);
    Pipeline::new(&source, &extractor, &store).build().unwrap();

    assert!(
        out.join("state.json").exists(),
        "the native checkpoint is still written per apply"
    );
    assert!(
        !out.join("graph.json").exists(),
        "apply must NOT produce graph.json — export is explicit (ADR-0042 F2)"
    );

    // The explicit export produces it, byte-equal to `graphjson::export` of the
    // state the apply just persisted.
    store.snapshot().unwrap();
    let bytes = fs::read(out.join("graph.json")).unwrap();
    let state = store.load_state().unwrap().unwrap();
    let expected = serde_json::to_vec_pretty(&graphjson::export(&state)).unwrap();
    assert_eq!(
        bytes, expected,
        "export writes exactly graphjson::export(state)"
    );
}

// ---- the freshness logic relocated from the daemon (ADR-0032 extraction) ----

/// The shrink-guard invariant, owned by the pipeline: an apply may remove only
/// nodes that belong to a file it touched. A node lost from an untouched file is
/// the unexplained shrink; a node lost from an edited file is the edit.
#[test]
fn check_shrink_guard_rejects_only_nodes_lost_from_untouched_files() {
    use filigrio_core::{Graph, NodeId};
    use filigrio_pipeline::check_shrink_guard;
    use std::collections::BTreeSet;

    let in_file = |id: &str, file: &str| {
        let mut n = Node::new(id, id, "function");
        n.source_file = Some(file.into());
        n
    };
    let prior = Graph {
        nodes: vec![
            in_file("fn:a.rs:x", "a.rs"),
            in_file("fn:a.rs:y", "a.rs"),
            in_file("fn:b.rs:z", "b.rs"),
            // The workspace overlay: no source file, regenerated every apply.
            Node::new("project:web", "web", "project"),
        ],
        edges: vec![],
    };
    let ids = |v: &[&str]| v.iter().map(|s| NodeId::new(*s)).collect::<Vec<_>>();
    let files = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();

    assert!(
        check_shrink_guard(&prior, &[], &files(&[])).is_ok(),
        "nothing removed"
    );
    assert!(
        check_shrink_guard(&prior, &ids(&["fn:a.rs:y"]), &files(&["a.rs"])).is_ok(),
        "a function deleted from an edited file is the edit, not a shrink"
    );
    assert!(
        check_shrink_guard(&prior, &ids(&["fn:a.rs:x", "fn:a.rs:y"]), &files(&["a.rs"])).is_ok(),
        "a removed file (touched) takes all its nodes with it"
    );
    assert!(
        check_shrink_guard(&prior, &ids(&["project:web"]), &files(&["a.rs"])).is_ok(),
        "a file-less overlay node is exempt"
    );

    let err = check_shrink_guard(&prior, &ids(&["fn:a.rs:y", "fn:b.rs:z"]), &files(&["a.rs"]))
        .expect_err("b.rs was not touched, so losing its node is the unexplained shrink");
    let msg = err.to_string();
    assert!(
        msg.contains("1 node(s)") && msg.contains("`fn:b.rs:z` in `b.rs`"),
        "the error must count only the stray node and name it: {msg}"
    );
}

/// `ClusterTiming::Deferred` takes Louvain off the apply without changing where
/// the partition ends up: between the apply and the recluster, the edited file's
/// surviving symbols keep their communities and the new symbol has none; after
/// `Engine::recluster`, the partition equals the one an inline apply computes
/// from the same prior. Two stores built identically, one edit applied each way.
#[test]
fn deferred_clustering_then_recluster_equals_inline() {
    use filigrio_pipeline::ClusterTiming;
    use filigrio_resolve::Engine;

    let dir = fixture("defer-cluster");
    let source = FsSource::new(&dir);
    let extractor = MockExtractor::new();
    let (inline_store, deferred_store) = (MemoryStore::new(), MemoryStore::new());
    let inline = Pipeline::new(&source, &extractor, &inline_store);
    let deferred = Pipeline::new(&source, &extractor, &deferred_store);
    // Both stores start from an inline cold build — the daemon never defers a
    // first index (only watcher-lane applies defer), and a deferred cold build
    // would have no partition at all — and only then does `deferred` switch.
    inline.build().unwrap();
    deferred.build().unwrap();
    let deferred = deferred.with_cluster_timing(ClusterTiming::Deferred);
    assert_eq!(
        inline_store.current().unwrap(),
        deferred_store.current().unwrap(),
        "precondition: identical starting states"
    );

    // One edit: src/a.rs keeps alpha and helper and gains `delta`, which calls helper.
    fs::write(
        dir.join("src/a.rs"),
        "fn alpha() {\n    beta();\n}\nfn helper() {}\nfn delta() {\n    helper();\n}\n",
    )
    .unwrap();
    let prior = inline_store.current().unwrap();
    inline.reconcile_and_apply(&prior, false).unwrap();
    deferred.reconcile_and_apply(&prior, false).unwrap();

    let after = deferred_store.current().unwrap();
    let community = |label: &str| {
        let id = &after
            .graph
            .nodes
            .iter()
            .find(|n| n.label == label)
            .unwrap()
            .id;
        after.partition.node_community.get(id).copied()
    };
    assert!(
        community("alpha").is_some(),
        "a re-extracted, unchanged symbol keeps its community"
    );
    assert!(
        community("delta").is_none(),
        "a new symbol waits for the recluster"
    );

    let reclustered = Engine::recluster(&after, &Default::default()).unwrap();
    assert_eq!(
        reclustered,
        inline_store.current().unwrap().partition,
        "deferred + recluster must land exactly where the inline apply does"
    );
}

/// The live-daemon regression, one layer down: with the guard **on** (the
/// shipping configuration), deleting a function from a file that stays must
/// land. Found on next.js — the count-based guard rejected it on every lane.
#[test]
fn deleting_a_function_from_a_kept_file_lands_with_the_guard_on() {
    let dir = fixture("guard-delete");
    let source = FsSource::new(&dir);
    let extractor = MockExtractor::new();
    let store = MemoryStore::new();
    let pipeline = Pipeline::new(&source, &extractor, &store).with_shrink_guard(true);
    pipeline.build().unwrap();
    let has = |label: &str| {
        store
            .current()
            .unwrap()
            .graph
            .nodes
            .iter()
            .any(|n| n.label == label)
    };
    assert!(has("helper"), "precondition: src/a.rs defines helper");

    // Drop `helper` from src/a.rs; the file itself stays.
    fs::write(dir.join("src/a.rs"), "fn alpha() {\n    beta();\n}\n").unwrap();
    let prior = store.current().unwrap();
    let (rep, build) = pipeline
        .reconcile_and_apply(&prior, false)
        .expect("a function deleted from a kept file is not an unexplained shrink");
    assert!(
        rep.has_drift && build.nodes_removed > 0,
        "the deletion was applied"
    );
    assert!(!has("helper"), "helper must be gone from the graph");
    assert!(has("alpha") && has("beta"), "untouched nodes stay");
}

/// `apply_signal` gates a raw producer signal: an UNCHANGED indexed file (dedup)
/// and an OUT-OF-SCOPE file (boundary) both dissolve, so nothing is applied.
#[test]
fn apply_signal_gates_unchanged_and_out_of_scope() {
    let dir = fixture("gate");
    let source = FsSource::new(&dir);
    let extractor = MockExtractor::new();
    let store = MemoryStore::new();
    let pipeline = Pipeline::new(&source, &extractor, &store).with_shrink_guard(true);
    pipeline.build().unwrap(); // cold build stamps the manifest with real hashes

    let prior = store.current().unwrap();
    let raw = filigrio_core::ChangeSet {
        added: vec![],
        // src/a.rs is unchanged on disk; target/junk.rs is out of the source boundary.
        modified: vec!["src/a.rs".to_string(), "target/junk.rs".to_string()],
        removed: vec![],
    };
    let report = pipeline.apply_signal(&prior, &raw).unwrap();
    assert_eq!(
        report.changed, 0,
        "unchanged + out-of-scope are both gated out"
    );
}

/// `reconcile_and_apply` detects an on-disk edit with no producer signal and applies
/// the drift authoritatively.
#[test]
fn reconcile_and_apply_applies_disk_drift() {
    let dir = fixture("recon");
    let source = FsSource::new(&dir);
    let extractor = MockExtractor::new();
    let store = MemoryStore::new();
    let pipeline = Pipeline::new(&source, &extractor, &store).with_shrink_guard(true);
    pipeline.build().unwrap();

    // Edit a file on disk (grow it) — no changeset. Reconcile must find + apply it.
    fs::write(
        dir.join("src/a.rs"),
        "fn alpha() {\n    beta();\n}\nfn helper() {}\nfn gamma() {}\n",
    )
    .unwrap();
    let prior = store.current().unwrap();
    let (rep, build) = pipeline.reconcile_and_apply(&prior, false).unwrap();
    assert!(rep.has_drift, "the disk edit is drift");
    assert!(build.changed > 0, "the drift was applied");
}
