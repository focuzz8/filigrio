//! TDD spec for the real store (ADR-0005) and the graph.json interchange
//! adapter (ADR-0017). Written before the implementation.
//!
//! Two things under test:
//!   * `graphjson::{export, import}` — bidirectional, schema/semantic compatible
//!     with Python graphify (NOT byte-identical).
//!   * `FsStore` — native `GraphState` persistence + bulk `apply_delta` +
//!     shrink-guard + a `graph.json` snapshot.

use filigrio_core::{
    CommunityId, CommunityMeta, Confidence, Edge, EdgeTarget, Graph, GraphDelta, GraphState,
    GraphStore, ManifestEntry, Node, NodeId, Partition, Project, Reference, Span, SymbolDefs,
    SymbolIndex, SymbolTable,
};
use filigrio_store::{graphjson, FsStore};
use std::collections::BTreeMap;

fn node(id: &str, label: &str, kind: &str, file: &str, loc: Option<&str>) -> Node {
    let mut n = Node::new(id, label, kind);
    n.source_file = Some(file.into());
    n.source_span = loc.and_then(Span::parse);
    n
}

fn resolved(src: &str, rel: &str, dst: &str, c: Confidence) -> Edge {
    Edge {
        source: NodeId::new(src),
        relation: rel.into(),
        confidence: c,
        target: EdgeTarget::Node(NodeId::new(dst)),
    }
}

/// file ─contains→ run ─calls→ helper; `run` lives in community 3.
fn sample_state() -> GraphState {
    let nodes = vec![
        node("file:src/lib.rs", "src/lib.rs", "file", "src/lib.rs", None),
        node("fn:run", "run", "function", "src/lib.rs", Some("L10")),
        node("fn:helper", "helper", "function", "src/lib.rs", Some("L20")),
    ];
    let edges = vec![
        resolved(
            "file:src/lib.rs",
            "contains",
            "fn:run",
            Confidence::Extracted,
        ),
        resolved("fn:run", "calls", "fn:helper", Confidence::Inferred),
    ];
    let mut node_community = BTreeMap::new();
    node_community.insert(NodeId::new("fn:run"), CommunityId(3));
    let mut communities = BTreeMap::new();
    communities.insert(
        CommunityId(3),
        CommunityMeta {
            id: CommunityId(3),
            label: "core".into(),
            size: 1,
            ..Default::default()
        },
    );
    GraphState {
        graph: Graph { nodes, edges },
        partition: Partition {
            node_community,
            communities,
        },
        ..Default::default()
    }
}

const GRAPHIFY_JSON: &str = r#"{
  "nodes": [
    {"id": "a", "label": "Alpha", "kind": "function", "source_file": "a.py", "source_location": "L1", "community": 2},
    {"id": "b", "label": "Beta", "source_file": "b.py"}
  ],
  "edges": [
    {"source": "a", "target": "b", "relation": "calls", "confidence": "INFERRED"}
  ]
}"#;

#[test]
fn export_emits_graphify_schema() {
    let v = graphjson::export(&sample_state());

    let nodes = v["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 3);
    let run = nodes
        .iter()
        .find(|n| n["label"] == "run")
        .expect("run node");
    assert_eq!(run["id"], "fn:run");
    assert_eq!(run["source_file"], "src/lib.rs");
    assert_eq!(run["source_location"], "L10");
    assert_eq!(run["kind"], "function");
    assert_eq!(
        run["community"], 3,
        "community exported as a node attribute"
    );

    let edges = v["edges"].as_array().expect("edges array");
    let calls = edges
        .iter()
        .find(|e| e["relation"] == "calls")
        .expect("calls edge");
    assert_eq!(calls["source"], "fn:run");
    assert_eq!(calls["target"], "fn:helper");
    assert_eq!(calls["confidence"], "INFERRED", "confidence is UPPERCASE");
}

#[test]
fn import_reads_python_graphify_json() {
    let state = graphjson::import(GRAPHIFY_JSON.as_bytes()).unwrap();
    assert_eq!(state.graph.nodes.len(), 2);

    let a = state.graph.node_by_label("Alpha").expect("Alpha");
    assert_eq!(a.source_file.as_deref(), Some("a.py"));
    assert_eq!(a.kind, "function");
    // community attribute → partition
    assert_eq!(
        state.partition.node_community.get(&a.id),
        Some(&CommunityId(2))
    );

    assert_eq!(state.graph.edges.len(), 1);
    let e = &state.graph.edges[0];
    assert_eq!(e.relation, "calls");
    assert_eq!(e.confidence, Confidence::Inferred);
    assert!(matches!(&e.target, EdgeTarget::Node(t) if t.0 == "b"));
}

#[test]
fn import_accepts_python_links_key() {
    // Real Python graphify emits edges under `links` (node-link convention),
    // not `edges`. The migration on-ramp must read them (ADR-0017).
    let python = r#"{
      "nodes": [
        {"id": "src_a", "label": "a()", "source_file": "a.rs"},
        {"id": "src_b", "label": "b()", "source_file": "b.rs"}
      ],
      "links": [
        {"source": "src_a", "target": "src_b", "relation": "calls", "confidence": "INFERRED"}
      ]
    }"#;
    let state = graphjson::import(python.as_bytes()).unwrap();
    assert_eq!(state.graph.nodes.len(), 2);
    assert_eq!(
        state.graph.edges.len(),
        1,
        "edges under `links` are imported, not silently dropped"
    );
    let e = &state.graph.edges[0];
    assert_eq!(e.relation, "calls");
    assert!(matches!(&e.target, EdgeTarget::Node(t) if t.0 == "src_b"));
}

#[test]
fn roundtrip_is_semantically_stable() {
    // import → export → import must preserve node/edge sets.
    let once = graphjson::import(GRAPHIFY_JSON.as_bytes()).unwrap();
    let exported = graphjson::export(&once);
    let twice = graphjson::import(exported.to_string().as_bytes()).unwrap();

    let labels = |s: &GraphState| {
        let mut v: Vec<String> = s.graph.nodes.iter().map(|n| n.label.clone()).collect();
        v.sort();
        v
    };
    assert_eq!(labels(&once), labels(&twice));

    let edges = |s: &GraphState| {
        let mut v: Vec<String> = s
            .graph
            .edges
            .iter()
            .map(|e| {
                let t = match &e.target {
                    EdgeTarget::Node(n) => n.0.clone(),
                    EdgeTarget::Symbol(r) => r.name.clone(),
                };
                format!("{}-{}->{}", e.source.0, e.relation, t)
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(edges(&once), edges(&twice));
}

// ---- FsStore ----------------------------------------------------------

fn tmp(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("filigrio-store-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn add_delta(nodes: Vec<Node>) -> GraphDelta {
    GraphDelta {
        nodes_added: nodes,
        ..Default::default()
    }
}

#[test]
fn bulk_apply_then_load_then_snapshot() {
    let dir = tmp("bulk");
    let store = FsStore::new(&dir);

    let delta = add_delta(vec![
        node("n1", "one", "function", "a.rs", None),
        node("n2", "two", "function", "a.rs", None),
        node("n3", "three", "function", "b.rs", None),
    ]);
    store.apply_delta(&delta).unwrap();

    let loaded = store.load_state().unwrap().unwrap();
    assert_eq!(
        loaded.graph.nodes.len(),
        3,
        "bulk delta persisted in one shot"
    );

    store.snapshot().unwrap();
    let snap = dir.join("graph.json");
    assert!(snap.exists(), "snapshot writes graph.json");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&snap).unwrap()).unwrap();
    assert_eq!(
        v["nodes"].as_array().unwrap().len(),
        3,
        "snapshot is filigrio schema"
    );
}

#[test]
fn apply_leaves_no_temp_residue_and_survives_stale_temp() {
    // ADR-0042 Phase 1c F1: FsStore writes are temp+rename. Fault injection
    // mid-write isn't practical here, so pin the observable invariants.
    let dir = tmp("atomic");
    let store = FsStore::new(&dir);
    store
        .apply_delta(&add_delta(vec![node(
            "n1", "one", "function", "a.rs", None,
        )]))
        .unwrap();
    store.snapshot().unwrap();

    let residue: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp."))
        .collect();
    assert!(
        residue.is_empty(),
        "no *.tmp.* residue after apply+snapshot: {residue:?}"
    );

    // A crashed previous writer's stale temp must not break apply or load.
    let stale = dir.join(format!("state.json.tmp.{}", std::process::id()));
    std::fs::write(&stale, b"{ torn garba").unwrap();
    store
        .apply_delta(&add_delta(vec![node(
            "n2", "two", "function", "b.rs", None,
        )]))
        .unwrap();
    let loaded = store.load_state().unwrap().unwrap();
    assert_eq!(loaded.graph.nodes.len(), 2);
    assert!(
        !stale.exists(),
        "the stale temp is consumed, not left around"
    );
}

#[test]
fn shrink_guard_blocks_unless_forced() {
    let dir = tmp("shrink");
    let store = FsStore::new(&dir);
    store
        .apply_delta(&add_delta(vec![
            node("n1", "one", "function", "a.rs", None),
            node("n2", "two", "function", "a.rs", None),
            node("n3", "three", "function", "b.rs", None),
        ]))
        .unwrap();

    // A delta that removes two nodes shrinks 3 → 1.
    let shrink = GraphDelta {
        nodes_removed: vec![NodeId::new("n2"), NodeId::new("n3")],
        ..Default::default()
    };
    assert!(
        store.apply_delta(&shrink).is_err(),
        "shrink must be refused by default (filigrio #479)"
    );
    // state untouched after the refusal
    assert_eq!(store.load_state().unwrap().unwrap().graph.nodes.len(), 3);

    // ...but a forced store allows it.
    FsStore::new(&dir)
        .with_force(true)
        .apply_delta(&shrink)
        .unwrap();
    assert_eq!(store.load_state().unwrap().unwrap().graph.nodes.len(), 1);
}

// ---- ADR-0042 Phase 1c F3: the write-only facets are not written -----------
//
// `symbols` and `symbol_index` are rebuilt from scratch by every apply and read
// by nothing from the persisted copy (B2), so they are `#[serde(skip)]`: still
// fields of `GraphState` — the convergence comparator digests all nine facets —
// but absent from `state.json`. The store's contract is therefore **round-trip
// identity over the persisted facets**, plus reconstructibility for the skipped
// ones, which `filigrio-resolve/tests/symbol_index.rs` gates.

/// `sample_state()` with every remaining facet non-empty too, so "the other
/// facets are still written" cannot pass vacuously.
fn every_facet_populated() -> GraphState {
    let mut s = sample_state();
    s.symbols.defs.insert("run".into(), NodeId::new("fn:run"));
    s.symbol_index.by_name.insert(
        "run".into(),
        SymbolDefs {
            nodes: vec![NodeId::new("fn:run")],
            modules: vec!["src/lib.rs".into()],
        },
    );
    s.reverse.refs.insert(
        "helper".into(),
        vec![Reference {
            source: NodeId::new("fn:run"),
            relation: "calls".into(),
            type_hint: None,
            specifier: None,
            imported: None,
            recv_opaque: false,
            recv_returns: None,
            recv_returns_owner: None,
        }],
    );
    s.manifest.entries.insert(
        "src/lib.rs".into(),
        ManifestEntry {
            hash: 7,
            last_modified: None,
            revision: None,
        },
    );
    s.workspace.projects.insert(
        "".into(),
        Project {
            root: "".into(),
            manifest: "Cargo.toml".into(),
            ..Default::default()
        },
    );
    s.exports.by_file.insert("src/lib.rs".into(), Vec::new());
    s
}

#[test]
fn state_json_omits_the_write_only_facets() {
    let dir = tmp("f3-omit");
    let store = FsStore::new(&dir);
    store.save_state(&every_facet_populated()).unwrap();

    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
    let obj = v.as_object().expect("state.json is a JSON object");

    assert!(
        !obj.contains_key("symbols"),
        "`symbols` is rebuilt every apply — it must not be written: {:?}",
        obj.keys().collect::<Vec<_>>()
    );
    assert!(
        !obj.contains_key("symbol_index"),
        "`symbol_index` is rebuilt every apply — it must not be written: {:?}",
        obj.keys().collect::<Vec<_>>()
    );

    // ...and nothing else went missing with them.
    for k in [
        "graph",
        "partition",
        "reverse",
        "manifest",
        "workspace",
        "exports",
    ] {
        assert!(obj.contains_key(k), "persisted facet `{k}` vanished too");
    }
    assert!(!v["graph"]["nodes"].as_array().unwrap().is_empty());
    assert!(!v["graph"]["edges"].as_array().unwrap().is_empty());
    assert!(!v["partition"]["node_community"]
        .as_object()
        .unwrap()
        .is_empty());
    assert!(!v["reverse"]["refs"].as_object().unwrap().is_empty());
    assert!(!v["manifest"]["entries"].as_object().unwrap().is_empty());
    assert!(!v["workspace"]["projects"].as_object().unwrap().is_empty());
    assert!(!v["exports"]["by_file"].as_object().unwrap().is_empty());
}

/// A pre-F3 `state.json` — written when both facets *were* persisted. Nothing in
/// the state types carries `deny_unknown_fields`, so it still loads and the two
/// now-unknown fields are ignored.
const PRE_F3_STATE_JSON: &str = r#"{
  "graph": {
    "nodes": [{"id": "fn:run", "label": "run", "kind": "function", "source_file": "src/lib.rs"}],
    "edges": []
  },
  "partition": {"node_community": {"fn:run": 3}, "communities": {}},
  "symbols": {"defs": {"run": "fn:run"}},
  "reverse": {"refs": {"helper": [{"source": "fn:run", "relation": "calls"}]}},
  "manifest": {"entries": {"src/lib.rs": {"hash": 7}}},
  "workspace": {"projects": {"": {"root": "", "manifest": "Cargo.toml"}}},
  "exports": {"by_file": {"src/lib.rs": []}},
  "symbol_index": {"by_name": {"run": {"nodes": ["fn:run"], "modules": ["src/lib.rs"]}}}
}"#;

#[test]
fn pre_f3_state_json_still_loads_with_the_skipped_fields_ignored() {
    let dir = tmp("f3-migrate");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("state.json"), PRE_F3_STATE_JSON).unwrap();

    let s = FsStore::new(&dir)
        .load_state()
        .expect("an old state.json must still load")
        .unwrap();

    // Persisted facets are unaffected by the migration.
    assert_eq!(s.graph.nodes.len(), 1);
    assert_eq!(s.partition.node_community.len(), 1);
    assert_eq!(s.reverse.refs.len(), 1);
    assert_eq!(s.manifest.entries.len(), 1);
    assert_eq!(s.workspace.projects.len(), 1);
    assert_eq!(s.exports.by_file.len(), 1);

    // The two skipped ones are ignored on the way in and arrive as `Default` —
    // the next apply rebuilds them (gate: filigrio-resolve/tests/symbol_index.rs).
    assert_eq!(
        s.symbols,
        SymbolTable::default(),
        "a persisted `symbols` must be ignored, not adopted"
    );
    assert_eq!(
        s.symbol_index,
        SymbolIndex::default(),
        "a persisted `symbol_index` must be ignored, not adopted"
    );
}

#[test]
fn serialized_size_is_independent_of_the_write_only_facets() {
    let small = every_facet_populated();
    let mut big = small.clone();
    for i in 0..5_000 {
        let id = NodeId::new(format!("fn:{i}"));
        big.symbols.defs.insert(format!("sym{i}"), id.clone());
        big.symbol_index.by_name.insert(
            format!("sym{i}"),
            SymbolDefs {
                nodes: vec![id],
                modules: vec!["src/lib.rs".into()],
            },
        );
    }
    let small_len = serde_json::to_vec_pretty(&small).unwrap().len();
    let big_len = serde_json::to_vec_pretty(&big).unwrap().len();
    assert_eq!(
        small_len, big_len,
        "state.json size must not scale with the write-only facets \
         (this is the ~15.7 MB/apply at next.js scale that F3 removes)"
    );
}

// ---- DeferredStore (ADR-0042 Phase 1c F4 / B12) -------------------------
//
// The write-behind seam. `Pipeline::apply` writes through a `GraphStore`; under
// B12 *when* that reaches disk is a daemon cadence decision, so the daemon hands
// the pipeline a store that merges into memory and hands the merged state back.
// The contract these tests pin is an **equivalence**: for the same prior and the
// same delta, `DeferredStore` must produce exactly what `FsStore` would have
// persisted — otherwise swapping it into the apply path changes the graph, not
// just the write schedule.

/// Reference implementation of the merge the daemon used to get: seed an
/// `FsStore` with `prior`, `apply_delta`, read back.
fn fsstore_merge(tag: &str, prior: &GraphState, delta: &GraphDelta) -> GraphState {
    let dir = tmp(tag);
    let store = FsStore::new(&dir).with_force(true);
    store.save_state(prior).unwrap();
    store.apply_delta(delta).unwrap();
    store.load_state().unwrap().unwrap()
}

/// A delta that touches every facet, so "carried from prior" vs "replaced from
/// delta" cannot pass vacuously for any field of `GraphState` — including the
/// three the ADR-0042 Phase 1d patch delta no longer replaces (edges and
/// `reverse`), which are exercised in **both** directions: an edge added and an
/// edge removed, a reference source dropped and a reference appended.
fn full_delta() -> GraphDelta {
    let src = every_facet_populated();
    GraphDelta {
        nodes_added: vec![node(
            "fn:added",
            "added",
            "function",
            "src/new.rs",
            Some("L1"),
        )],
        nodes_removed: vec![NodeId::new("fn:helper")],
        edges_added: vec![resolved(
            "fn:run",
            "calls",
            "fn:added",
            Confidence::Extracted,
        )],
        edges_removed: vec![resolved(
            "fn:run",
            "calls",
            "fn:helper",
            Confidence::Inferred,
        )],
        dirty_files: ["src/lib.rs".to_string(), "src/new.rs".to_string()]
            .into_iter()
            .collect(),
        reverse_dropped: vec![NodeId::new("fn:helper")],
        reverse_added: vec![(
            "added".into(),
            Reference {
                source: NodeId::new("fn:run"),
                relation: "calls".into(),
                type_hint: None,
                specifier: None,
                imported: None,
                recv_opaque: false,
                recv_returns: None,
                recv_returns_owner: None,
            },
        )],
        partition: {
            let mut p = Partition::default();
            p.node_community
                .insert(NodeId::new("fn:run"), CommunityId(9));
            p.communities.insert(
                CommunityId(9),
                CommunityMeta {
                    id: CommunityId(9),
                    label: "nine".into(),
                    size: 1,
                    cohesion_permille: 1000,
                },
            );
            p
        },
        symbols: src.symbols.clone(),
        manifest: src.manifest.clone(),
        workspace: src.workspace.clone(),
        exports: src.exports.clone(),
        symbol_index: src.symbol_index.clone(),
        ..Default::default()
    }
}

/// F4 — the swap is behaviour-preserving. The state `DeferredStore` hands back
/// must be byte-for-byte what `FsStore` would have written for the same
/// (prior, delta). This is also the drift guard for `merge_from`'s optimization
/// (only `graph.nodes` is carried from `prior`; everything else is replaced
/// from the delta): if `GraphState` grows a field the merge does not overwrite,
/// this comparison fails instead of silently dropping it.
#[test]
fn deferred_store_result_equals_what_fsstore_would_have_persisted() {
    let prior = every_facet_populated();
    let delta = full_delta();

    let deferred = filigrio_store::DeferredStore::new(&prior);
    deferred.apply_delta(&delta).unwrap();
    let got = deferred
        .take()
        .expect("an applied delta must yield a state");

    let expected = fsstore_merge("deferred-parity", &prior, &delta);
    assert_eq!(
        serde_json::to_value(&got).unwrap(),
        serde_json::to_value(&expected).unwrap(),
        "DeferredStore must produce exactly the state FsStore would have persisted"
    );
    // …including the skipped-on-disk facets, which the JSON comparison above
    // cannot see (F3). They are replaced wholesale from the delta.
    assert_eq!(got.symbols, delta.symbols);
    assert_eq!(got.symbol_index, delta.symbol_index);
}

/// F4 — `take()` is the "did this apply mutate anything" signal the daemon uses
/// instead of re-reading the store: `None` until a delta is applied.
#[test]
fn deferred_store_take_is_none_until_a_delta_is_applied() {
    let prior = every_facet_populated();
    let store = filigrio_store::DeferredStore::new(&prior);
    assert!(
        store.take().is_none(),
        "no apply ⇒ nothing to persist ⇒ the daemon must not touch the cache"
    );
    store.apply_delta(&GraphDelta::default()).unwrap();
    assert!(store.take().is_some(), "an apply ⇒ a state to persist");
}

/// F4 — `load_state` serves the **resident** prior, never disk. This is the
/// half of the win that is not about write-behind at all: the old path read
/// `state.json` inside `FsStore::apply_delta` and again afterwards to refresh
/// the cache, two full parses of a state the daemon already held.
#[test]
fn deferred_store_load_state_serves_the_resident_prior() {
    let prior = every_facet_populated();
    let store = filigrio_store::DeferredStore::new(&prior);
    let loaded = store
        .load_state()
        .unwrap()
        .expect("prior is always available");
    assert_eq!(loaded.graph.nodes.len(), prior.graph.nodes.len());

    // After an apply it serves the merged state, so a second apply composes.
    store.apply_delta(&full_delta()).unwrap();
    let after = store.load_state().unwrap().unwrap();
    assert!(after.graph.nodes.iter().any(|n| n.id.0 == "fn:added"));
}

/// F4 — a deferred store has no directory, so it cannot honestly produce the
/// `graph.json` interchange artifact. It must say so rather than return `Ok(())`
/// and leave the caller believing a file was written (ADR-0029 honesty). The
/// export verb flushes first and snapshots through `FsStore`.
#[test]
fn deferred_store_refuses_to_snapshot() {
    let prior = every_facet_populated();
    let store = filigrio_store::DeferredStore::new(&prior);
    let err = store
        .snapshot()
        .expect_err("a memory-only store must not claim to have written graph.json");
    assert!(
        err.to_string().contains("export"),
        "the error must point at the explicit export path: {err}"
    );
}

// ---- ADR-0042 Phase 1d P1: the merge applies patches -----------------------
//
// Edges and `reverse` are no longer replaced wholesale, so the merge has to get
// remove-then-append right rather than just overwriting. The engine-side
// contract (the patch is exact, minimal and scope-independent) is gated by
// `filigrio-resolve/tests/patch_delta.rs`; what these pin is the *store* half.

/// A hand-built delta is now a patch: `edges_added` appends onto the prior edge
/// set instead of replacing it, and `edges_removed` retires named edges.
#[test]
fn merge_patches_edges_instead_of_replacing_them() {
    let dir = tmp("edge-patch");
    let store = FsStore::new(&dir).with_force(true);
    store.save_state(&sample_state()).unwrap();

    store
        .apply_delta(&GraphDelta {
            nodes_added: vec![node("fn:extra", "extra", "function", "src/lib.rs", None)],
            edges_added: vec![resolved(
                "fn:run",
                "calls",
                "fn:extra",
                Confidence::Extracted,
            )],
            edges_removed: vec![resolved(
                "fn:run",
                "calls",
                "fn:helper",
                Confidence::Inferred,
            )],
            ..Default::default()
        })
        .unwrap();

    let after = store.load_state().unwrap().unwrap();
    let keys: Vec<String> = after
        .graph
        .edges
        .iter()
        .map(|e| {
            let t = match &e.target {
                EdgeTarget::Node(n) => n.0.clone(),
                EdgeTarget::Symbol(r) => r.name.clone(),
            };
            format!("{}-{}->{t}", e.source.0, e.relation)
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            "file:src/lib.rs-contains->fn:run".to_string(),
            "fn:run-calls->fn:extra".to_string(),
        ],
        "the untouched prior edge survives in place, the named one is gone, the \
         new one is appended — a wholesale replacement would have dropped the first"
    );
}

/// Removal is a **multiset** operation: listing an edge once retires one
/// occurrence, not every duplicate. (Two references with different names can
/// resolve to the same target from the same source, so parallel edges are real.)
#[test]
fn edge_removal_is_by_multiplicity_not_by_value() {
    let dir = tmp("edge-multiset");
    let store = FsStore::new(&dir).with_force(true);
    let mut prior = sample_state();
    let dup = resolved("fn:run", "calls", "fn:helper", Confidence::Inferred);
    prior.graph.edges.push(dup.clone());
    store.save_state(&prior).unwrap();

    store
        .apply_delta(&GraphDelta {
            edges_removed: vec![dup],
            ..Default::default()
        })
        .unwrap();

    let after = store.load_state().unwrap().unwrap();
    assert_eq!(
        after
            .graph
            .edges
            .iter()
            .filter(|e| e.relation == "calls")
            .count(),
        1,
        "one listed removal retires exactly one of the two identical edges"
    );
}

/// The `reverse` patch: drop by source, append, re-canonicalize — and a name
/// whose references all retire disappears, exactly as a from-scratch
/// `rebuild_refs` would never have created it.
#[test]
fn merge_patches_the_reverse_index() {
    fn reference(source: &str, relation: &str) -> Reference {
        Reference {
            source: NodeId::new(source),
            relation: relation.into(),
            type_hint: None,
            specifier: None,
            imported: None,
            recv_opaque: false,
            recv_returns: None,
            recv_returns_owner: None,
        }
    }

    let dir = tmp("reverse-patch");
    let store = FsStore::new(&dir).with_force(true);
    let mut prior = every_facet_populated();
    // A second name, referenced only from the node that is about to retire.
    prior
        .reverse
        .refs
        .insert("doomed".into(), vec![reference("fn:run", "calls")]);
    prior
        .reverse
        .refs
        .get_mut("helper")
        .unwrap()
        .push(reference("fn:other", "calls"));
    store.save_state(&prior).unwrap();

    store
        .apply_delta(&GraphDelta {
            reverse_dropped: vec![NodeId::new("fn:run")],
            // Re-contributed verbatim: an idempotent re-index must not duplicate.
            reverse_added: vec![
                ("helper".into(), reference("fn:other", "calls")),
                ("fresh".into(), reference("fn:other", "calls")),
            ],
            ..Default::default()
        })
        .unwrap();

    let after = store.load_state().unwrap().unwrap();
    assert!(
        !after.reverse.refs.contains_key("doomed"),
        "a name left with no references is removed, not kept empty: {:?}",
        after.reverse.refs.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        after.reverse.refs.get("helper").map(Vec::len),
        Some(1),
        "`fn:run`'s reference dropped; `fn:other`'s survived and did not double \
         when re-contributed (sort+dedup, exactly like rebuild_refs)"
    );
    assert!(
        after.reverse.refs.contains_key("fresh"),
        "the appended reference lands under its own name"
    );
}
