//! TDD spec for P0c: disambiguate stripped qualifiers by crate name
//!
//! These tests verify that when a qualified workspace reference (e.g., `filigrio_core::Error`)
//! is stripped to an ambiguous base name (`Error`) due to multiple candidates:
//! - Use the stripped qualifier (`filigrio_core`) as disambiguating evidence
//! - Prefer candidates from the same crate when multiple candidates exist
//! - Transform honest declines into verified binds using workspace knowledge
//!
//! Test coverage:
//! 1. Single candidate with matching qualifier → resolve to that candidate
//! 2. Multiple candidates but only one in target crate → resolve to target crate's candidate
//! 3. Multiple candidates including one in target crate → prefer target crate's candidate
//! 4. Multiple candidates none in target crate → decline honestly (no match)
//! 5. Non-workspace qualified references → use existing logic (should already work)
//! 6. Edge cases: crate name normalization, nested errors, variant errors

use filigrio_core::{
    ChangeSet, Confidence, Edge, EdgeTarget, Extraction, GraphDelta, GraphState, GraphStore,
    NodeId, Result, Source, TargetRef,
};
use filigrio_resolve::Engine;
use filigrio_store::MemoryStore;
use std::cell::RefCell;
use std::collections::BTreeMap;

// ---- toy Source + Extractor -------------------------------------------------

struct ToySource {
    files: RefCell<BTreeMap<String, String>>,
    reads: RefCell<Vec<String>>,
}

impl ToySource {
    fn new(files: &[(&str, &str)]) -> Self {
        ToySource {
            files: RefCell::new(
                files
                    .iter()
                    .map(|(p, c)| (p.to_string(), c.to_string()))
                    .collect(),
            ),
            reads: RefCell::new(Vec::new()),
        }
    }
}

impl Source for ToySource {
    fn poll(&self, _since: Option<&filigrio_core::Revision>) -> Result<ChangeSet> {
        let mut paths: Vec<String> = self.files.borrow().keys().cloned().collect();
        paths.sort();
        Ok(ChangeSet::all_added(paths))
    }
    fn read(&self, path: &str) -> Result<Vec<u8>> {
        self.reads.borrow_mut().push(path.to_string());
        self.files
            .borrow()
            .get(path)
            .map(|s| s.clone().into_bytes())
            .ok_or_else(|| filigrio_core::Error::NotFound(path.into()))
    }
    fn exists(&self, path: &str) -> bool {
        self.files.borrow().contains_key(path)
    }
}

struct ToyExtractor {
    extracts: RefCell<Vec<String>>,
}

impl ToyExtractor {
    fn new() -> Self {
        ToyExtractor {
            extracts: RefCell::new(Vec::new()),
        }
    }
}

fn is_manifest_path(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    matches!(
        base,
        "Cargo.toml" | "package.json" | "deno.json" | "go.mod" | "pyproject.toml"
    )
}

impl filigrio_core::Extractor for ToyExtractor {
    fn handles(&self, artifact: &filigrio_core::Artifact) -> bool {
        !is_manifest_path(&artifact.path)
    }

    fn extract(&self, artifact: &filigrio_core::Artifact, bytes: &[u8]) -> Result<Extraction> {
        self.extracts.borrow_mut().push(artifact.path.clone());
        let path = &artifact.path;
        let text = String::from_utf8_lossy(bytes);

        let file_id = NodeId::new(format!("file:{path}"));
        let mut file_node = filigrio_core::Node::new(file_id.0.clone(), path.clone(), "file");
        file_node.source_file = Some(path.clone());
        let mut nodes = vec![file_node];
        let mut edges = Vec::new();
        let exports: Vec<filigrio_core::Export> = Vec::new();
        let mut current: Option<NodeId> = None;

        for line in text.lines() {
            let t = line.trim();
            if let Some(name) = t.strip_prefix("fn ") {
                let id = NodeId::new(format!("fn:{path}:{name}"));
                let mut n = filigrio_core::Node::new(id.0.clone(), name.trim(), "function");
                n.source_file = Some(path.clone());
                nodes.push(n);
                edges.push(Edge {
                    source: file_id.clone(),
                    relation: "contains".into(),
                    confidence: Confidence::Extracted,
                    target: EdgeTarget::Node(id.clone()),
                });
                current = Some(id);
            } else if let Some(name) = t.strip_prefix("call ") {
                if let Some(src) = &current {
                    let name = name.trim();
                    let tref = TargetRef::new(name);
                    edges.push(Edge {
                        source: src.clone(),
                        relation: "calls".into(),
                        confidence: Confidence::Extracted,
                        target: EdgeTarget::Symbol(tref),
                    });
                }
            }
        }
        Ok(Extraction {
            nodes,
            edges,
            exports,
        })
    }
}

// ---- harness ----------------------------------------------------------------

fn apply_to(
    store: &MemoryStore,
    prior: &GraphState,
    cs: &ChangeSet,
    src: &ToySource,
    ext: &ToyExtractor,
) -> GraphDelta {
    let delta = Engine::apply(
        prior,
        cs,
        src,
        ext,
        &filigrio_resolve::ClusterConfig::default(),
    )
    .expect("apply");
    store.apply_delta(&delta).expect("store");
    delta
}

fn cold(files: &[(&str, &str)]) -> (MemoryStore, ToySource, ToyExtractor) {
    let src = ToySource::new(files);
    let ext = ToyExtractor::new();
    let store = MemoryStore::new();
    let cs = src.poll(None).unwrap();
    apply_to(&store, &GraphState::default(), &cs, &src, &ext);
    (store, src, ext)
}

fn call_edge(s: &GraphState, src_id: &str) -> Edge {
    let calls: Vec<&Edge> = s
        .graph
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source.0 == src_id)
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "expected exactly one calls edge from {src_id}, got {calls:?}"
    );
    calls[0].clone()
}

// ---------------------------------------------------------------------------
// TDD TESTS: P0c - disambiguate stripped qualifiers by crate name
// ---------------------------------------------------------------------------

#[test]
fn test_single_candidate_from_target_crates_resolves() {
    // Single candidate with matching qualifier → resolve to that candidate
    let files = &[
        // Workspace manifests
        (
            "crates/filigrio_core/Cargo.toml",
            "[package]\nname = \"filigrio-core\"\n",
        ),
        (
            "crates/filigrio_daemon/Cargo.toml",
            "[package]\nname = \"filigrio-daemon\"\n",
        ),
        // Definitions: Error in both crates
        ("crates/filigrio_core/src/error.toy", "fn Error"),
        ("crates/filigrio_daemon/src/error.toy", "fn Error"),
        // Reference: filigrio_core::Error should resolve to filigrio_core's Error
        (
            "crates/filigrio_daemon/src/main.toy",
            "fn main\ncall filigrio_core::Error",
        ),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // Verify: filigrio_core::Error should resolve to Error in filigrio_core
    let e = call_edge(&state, "fn:crates/filigrio_daemon/src/main.toy:main");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:crates/filigrio_core/src/error.toy:Error"),
        "filigrio_core::Error should resolve to Error in filigrio_core crate, got {:?}",
        e.target
    );
}

#[test]
fn test_multiple_candidates_only_one_in_target_crates_resolves() {
    // Multiple candidates but only one in target crate → resolve to target crate's candidate
    let files = &[
        // Workspace manifests
        (
            "crates/filigrio_core/Cargo.toml",
            "[package]\nname = \"filigrio-core\"\n",
        ),
        (
            "crates/filigrio_daemon/Cargo.toml",
            "[package]\nname = \"filigrio-daemon\"\n",
        ),
        (
            "crates/filigrio_protocol/Cargo.toml",
            "[package]\nname = \"filigrio-protocol\"\n",
        ),
        // Definitions: Error in all three crates
        ("crates/filigrio_core/src/error.toy", "fn Error"),
        ("crates/filigrio_daemon/src/error.toy", "fn Error"),
        ("crates/filigrio_protocol/src/error.toy", "fn Error"),
        // Reference: filigrio_protocol::Error should resolve to filigrio_protocol's Error
        (
            "crates/filigrio_daemon/src/main.toy",
            "fn main\ncall filigrio_protocol::Error",
        ),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // Verify: filigrio_protocol::Error should resolve to Error in filigrio_protocol
    let e = call_edge(&state, "fn:crates/filigrio_daemon/src/main.toy:main");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:crates/filigrio_protocol/src/error.toy:Error"),
        "filigrio_protocol::Error should resolve to Error in filigrio_protocol crate, got {:?}",
        e.target
    );
}

#[test]
fn test_multiple_candidates_including_target_crates_prefers_target() {
    // Multiple candidates including one in target crate → prefer target crate's candidate
    let files = &[
        // Workspace manifests
        (
            "crates/filigrio_core/Cargo.toml",
            "[package]\nname = \"filigrio-core\"\n",
        ),
        (
            "crates/filigrio_daemon/Cargo.toml",
            "[package]\nname = \"filigrio-daemon\"\n",
        ),
        // Definitions: Error in both crates
        ("crates/filigrio_core/src/error.toy", "fn Error"),
        ("crates/filigrio_daemon/src/error.toy", "fn Error"),
        // Reference from filigrio_core itself should prefer its own Error
        (
            "crates/filigrio_core/src/main.toy",
            "fn main\ncall filigrio_core::Error",
        ),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // Verify: filigrio_core::Error in filigrio_core should resolve to its own Error
    let e = call_edge(&state, "fn:crates/filigrio_core/src/main.toy:main");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:crates/filigrio_core/src/error.toy:Error"),
        "filigrio_core::Error from filigrio_core should resolve to its own Error, got {:?}",
        e.target
    );
}

#[test]
fn test_multiple_candidates_none_in_target_crates_declines() {
    // Multiple candidates none in target crate → decline honestly (no match)
    let files = &[
        // Workspace manifests
        (
            "crates/filigrio_core/Cargo.toml",
            "[package]\nname = \"filigrio-core\"\n",
        ),
        (
            "crates/filigrio_daemon/Cargo.toml",
            "[package]\nname = \"filigrio-daemon\"\n",
        ),
        (
            "crates/external_crate/Cargo.toml",
            "[package]\nname = \"external-crate\"\n",
        ),
        // Definitions: Error in filigrio_core and filigrio_daemon only
        ("crates/filigrio_core/src/error.toy", "fn Error"),
        ("crates/filigrio_daemon/src/error.toy", "fn Error"),
        // Reference: external_crate::Error should stay unresolved (no Error in external_crate)
        (
            "crates/external_crate/src/main.toy",
            "fn main\ncall external_crate::Error",
        ),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // Verify: external_crate::Error should stay unresolved
    let e = call_edge(&state, "fn:crates/external_crate/src/main.toy:main");
    assert!(
        matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "external_crate::Error"),
        "external_crate::Error should stay unresolved (no Error in external_crate), got {:?}",
        e.target
    );
}

#[test]
fn test_non_workspace_qualified_references_use_existing_logic() {
    // Non-workspace qualified references → use existing logic (should already work)
    let files = &[
        (
            "crates/my_crate/Cargo.toml",
            "[package]\nname = \"my-crate\"\n",
        ),
        ("crates/my_crate/src/lib.toy", "fn Function"),
        (
            "crates/my_crate/src/main.toy",
            "fn main\ncall turbojpeg::PixelFormat",
        ),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // Verify: turbojpeg::PixelFormat should stay unresolved (foreign, not in workspace)
    let e = call_edge(&state, "fn:crates/my_crate/src/main.toy:main");
    assert!(
        matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "turbojpeg::PixelFormat"),
        "turbojpeg::PixelFormat should stay unresolved as foreign qualified path, got {:?}",
        e.target
    );
}

#[test]
fn test_crate_name_normalization_with_underscores() {
    // Edge case: crate name normalization (underscores vs dashes)
    let files = &[
        (
            "crates/filigrio_core_compat/Cargo.toml",
            "[package]\nname = \"filigrio_core_compat\"\n",
        ),
        ("crates/filigrio_core_compat/src/error.toy", "fn Error"),
        (
            "crates/filigrio_core_compat/src/main.toy",
            "fn main\ncall filigrio_core_compat::Error",
        ),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // Verify: filigrio_core_compat::Error should resolve correctly with underscores
    let e = call_edge(&state, "fn:crates/filigrio_core_compat/src/main.toy:main");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:crates/filigrio_core_compat/src/error.toy:Error"),
        "filigrio_core_compat::Error should resolve to Error with crate name normalization, got {:?}",
        e.target
    );
}

#[test]
fn test_nested_module_path_disambiguation() {
    // Edge case: nested module paths (crate::module::Type)
    let files = &[
        (
            "crates/filigrio_core/Cargo.toml",
            "[package]\nname = \"filigrio-core\"\n",
        ),
        (
            "crates/filigrio_daemon/Cargo.toml",
            "[package]\nname = \"filigrio-daemon\"\n",
        ),
        // Define Type in both crates
        ("crates/filigrio_core/src/types.toy", "fn Type"),
        ("crates/filigrio_daemon/src/types.toy", "fn Type"),
        // Reference with nested path from filigrio_daemon
        (
            "crates/filigrio_daemon/src/main.toy",
            "fn main\ncall filigrio_core::api::Type",
        ),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // Verify: filigrio_core::api::Type should resolve to Type in filigrio_core
    let e = call_edge(&state, "fn:crates/filigrio_daemon/src/main.toy:main");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:crates/filigrio_core/src/types.toy:Type"),
        "filigrio_core::api::Type should resolve to Type in filigrio_core crate (nested path), got {:?}",
        e.target
    );
}

#[test]
fn test_four_error_nodes_disambiguation() {
    // The motivating example: four nodes labelled `Error` (core, daemon, protocol, plus variant)
    let files = &[
        ("crates/filigrio_core/Cargo.toml", "[package]\nname = \"filigrio-core\"\n"),
        ("crates/filigrio_daemon/Cargo.toml", "[package]\nname = \"filigrio-daemon\"\n"),
        ("crates/filigrio_protocol/Cargo.toml", "[package]\nname = \"filigrio-protocol\"\n"),
        // Define Error in three crates
        ("crates/filigrio_core/src/error.toy", "fn Error"),
        ("crates/filigrio_daemon/src/error.toy", "fn Error"),
        ("crates/filigrio_protocol/src/error.toy", "fn Error"),
        // Define Response::Error variant (simulated as separate fn)
        ("crates/filigrio_protocol/src/response.toy", "fn Error"),
        // Reference each specifically
        (
            "crates/filigrio_daemon/src/main.toy",
            "fn main\ncall filigrio_core::Error\ncall filigrio_daemon::Error\ncall filigrio_protocol::Error",
        ),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // Collect all calls edges from main
    let calls: Vec<&Edge> = state
        .graph
        .edges
        .iter()
        .filter(|e| e.source.0 == "fn:crates/filigrio_daemon/src/main.toy:main")
        .collect();

    assert_eq!(
        calls.len(),
        3,
        "expected exactly 3 calls edges from main, got {}",
        calls.len()
    );

    // Verify each call resolves to the correct crate's Error
    let resolved_targets: Vec<&EdgeTarget> = calls.iter().map(|e| &e.target).collect();

    // filigrio_core::Error should resolve to filigrio_core's Error
    let core_resolved = resolved_targets.iter().any(|t| {
        matches!(t, EdgeTarget::Node(n) if n.0 == "fn:crates/filigrio_core/src/error.toy:Error")
    });
    assert!(
        core_resolved,
        "filigrio_core::Error should resolve to filigrio_core's Error"
    );

    // filigrio_daemon::Error should resolve to filigrio_daemon's Error
    let daemon_resolved = resolved_targets.iter().any(|t| {
        matches!(t, EdgeTarget::Node(n) if n.0 == "fn:crates/filigrio_daemon/src/error.toy:Error")
    });
    assert!(
        daemon_resolved,
        "filigrio_daemon::Error should resolve to filigrio_daemon's Error"
    );

    // filigrio_protocol::Error should resolve to filigrio_protocol's Error
    let protocol_resolved = resolved_targets.iter().any(
        |t| matches!(t, EdgeTarget::Node(n) if n.0.starts_with("fn:crates/filigrio_protocol")),
    );
    assert!(
        protocol_resolved,
        "filigrio_protocol::Error should resolve to filigrio_protocol's Error"
    );
}

#[test]
fn test_cross_crates_reference_with_same_name() {
    // Cross-crate reference when both crates have same-named types
    let files = &[
        ("crates/A/Cargo.toml", "[package]\nname = \"crate-a\"\n"),
        ("crates/B/Cargo.toml", "[package]\nname = \"crate-b\"\n"),
        // Both crates have CommonType
        ("crates/A/src/types.toy", "fn CommonType"),
        ("crates/B/src/types.toy", "fn CommonType"),
        // In crate A, reference crate_b::CommonType
        ("crates/A/src/main.toy", "fn main\ncall crate_b::CommonType"),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // Verify: crate_b::CommonType should resolve to crate B's CommonType
    let e = call_edge(&state, "fn:crates/A/src/main.toy:main");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:crates/B/src/types.toy:CommonType"),
        "crate_b::CommonType should resolve to CommonType in crate B, got {:?}",
        e.target
    );
}

#[test]
fn test_workspace_crates_with_std_names_not_confused() {
    // Edge case: workspace crates with names that match std library names
    let files = &[
        (
            "crates/std_compat/Cargo.toml",
            "[package]\nname = \"std\"\n",
        ),
        ("crates/std_compat/src/error.toy", "fn Error"),
        // Reference using the std name
        (
            "crates/std_compat/src/main.toy",
            "fn main\ncall std::io::Error",
        ),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // This should resolve to our Error because "std" is in workspace
    // even though std::io::Error would normally refer to the standard library
    let e = call_edge(&state, "fn:crates/std_compat/src/main.toy:main");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:crates/std_compat/src/error.toy:Error"),
        "std::io::Error should resolve to Error in workspace 'std' crate, got {:?}",
        e.target
    );
}
