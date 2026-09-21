//! TDD spec for workspace-aware qualified path resolution (P0b)
//!
//! These tests verify that when resolving a qualified type reference, the resolver
//! checks if the head matches a known workspace crate and strips the workspace
//! qualifier if it does.
//!
//! Test coverage:
//! 1. Qualified types with workspace crate heads should resolve
//! 2. Qualified types with non-workspace heads should decline
//! 3. Multiple segments: crate::module::Type vs external::module::Type
//! 4. Edge cases: workspace crates with names that match std crates
//! 5. Integration with existing resolution logic

use filigrio_core::{
    ChangeSet, Confidence, Edge, EdgeTarget, Extraction, GraphDelta, GraphState, GraphStore,
    NodeId, Result, Source, TargetRef,
};
use filigrio_resolve::Engine;
use filigrio_store::MemoryStore;
use std::cell::RefCell;
use std::collections::BTreeMap;

// ---- toy Source + Extractor -------------------------------------------------
//
// Content is a tiny DSL so the tests own resolution end-to-end without pulling
// in tree-sitter:
//   `fn NAME`   → a `function` node `fn:<path>:NAME` + a resolved `contains`
//                 edge from the file node;
//   `call NAME` → a `calls` edge (enclosing fn → `Symbol(NAME)`), unresolved.
//
// These tests use qualified names like `filigrio_core::Error` to simulate
// real Rust workspace-qualified paths.

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
    #[allow(dead_code)]
    fn set(&self, path: &str, content: &str) {
        self.files.borrow_mut().insert(path.into(), content.into());
    }
    #[allow(dead_code)]
    fn reads(&self) -> Vec<String> {
        self.reads.borrow().clone()
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
    #[allow(dead_code)]
    fn extracts(&self) -> Vec<String> {
        self.extracts.borrow().clone()
    }
}

/// Basenames the toy extractor treats as project manifests (not code): they
/// exist in the tree only as project boundaries to be probed, exactly as
/// `package.json`/`Cargo.toml` are skipped by the real dispatch.
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
                // Function definition
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
                // Function call - use the qualified name as-is (this is what the real extractor does)
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

/// Fold a delta through the real store and return the resulting linked graph.
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

/// Find the single `calls` edge out of `src_id` (panics if not exactly one).
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
// TDD TESTS: Workspace-aware qualified path resolution (P0b)
// ---------------------------------------------------------------------------

#[test]
fn test_workspace_qualified_path_resolves() {
    // Source files simulating a workspace with two crates
    let files = &[
        // Workspace Manifest: filigrio-core
        (
            "crates/filigrio_core/Cargo.toml",
            "[package]\nname = \"filigrio-core\"\n",
        ),
        // Workspace Manifest: filigrio-daemon
        (
            "crates/filigrio_daemon/Cargo.toml",
            "[package]\nname = \"filigrio-daemon\"\n",
        ),
        // File A: defines Error in filigrio_core crate
        ("crates/filigrio_core/src/error.toy", "fn Error"),
        // File B: defines Command in filigrio_daemon crate
        ("crates/filigrio_daemon/src/cmd.toy", "fn Command"),
        // File C: calls filigrio_core::Error (workspace-qualified, should resolve)
        (
            "crates/filigrio_daemon/src/main.toy",
            "fn main\ncall filigrio_core::Error",
        ),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // Verify: filigrio_core::Error should resolve to Error (workspace-qualified path stripped)
    let e = call_edge(&state, "fn:crates/filigrio_daemon/src/main.toy:main");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:crates/filigrio_core/src/error.toy:Error"),
        "filigrio_core::Error should resolve to Error in the workspace, got {:?}",
        e.target
    );

    // Verify: confidence should be Inferred (cross-file resolution)
    assert_eq!(e.confidence, Confidence::Inferred, "cross-file → INFERRED");
}

#[test]
fn test_foreign_qualified_path_declines() {
    // Source files simulating a workspace with one crate and foreign dependencies
    let files = &[
        // Workspace Manifest: my-crate
        (
            "crates/my_crate/Cargo.toml",
            "[package]\nname = \"my-crate\"\n",
        ),
        // File A: defines Function in my_crate
        ("crates/my_crate/src/lib.toy", "fn Function"),
        // File B: calls turbojpeg::PixelFormat (foreign, should decline)
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
fn test_multiple_segments_workspace_path() {
    // Source files simulating a workspace with nested modules
    let files = &[
        // Workspace Manifest: filigrio-core
        (
            "crates/filigrio_core/Cargo.toml",
            "[package]\nname = \"filigrio-core\"\n",
        ),
        // File A: defines Type in filigrio_core crate
        ("crates/filigrio_core/src/types.toy", "fn Type"),
        // File B: calls filigrio_core::module::Type
        (
            "crates/filigrio_core/src/main.toy",
            "fn main\ncall filigrio_core::module::Type",
        ),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // Verify: filigrio_core::module::Type should resolve to Type (workspace head stripped)
    let e = call_edge(&state, "fn:crates/filigrio_core/src/main.toy:main");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:crates/filigrio_core/src/types.toy:Type"),
        "filigrio_core::module::Type should resolve to Type (workspace head stripped), got {:?}",
        e.target
    );
}

#[test]
fn test_std_qualified_path_not_confused() {
    // Source files simulating a workspace with a crate named "std" (edge case)
    let files = &[
        // Workspace Manifest: std (oops, crate name matches std library)
        ("crates/my_std/Cargo.toml", "[package]\nname = \"std\"\n"),
        // File A: defines Error in our custom "std" crate
        ("crates/my_std/src/error.toy", "fn Error"),
        // File B: calls std::io::Error (this should resolve to our Error, not the real std library)
        ("crates/my_std/src/main.toy", "fn main\ncall std::io::Error"),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // Verify: std::io::Error should resolve to our Error (because "std" is in workspace)
    // This is the correct workspace-aware behavior
    let e = call_edge(&state, "fn:crates/my_std/src/main.toy:main");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:crates/my_std/src/error.toy:Error"),
        "std::io::Error should resolve to Error when 'std' is a workspace crate, got {:?}",
        e.target
    );
}

#[test]
fn test_integration_with_project_scoping() {
    // Source files simulating a workspace with multiple projects
    let files = &[
        // Workspace Manifest: project-a
        (
            "crates/project_a/Cargo.toml",
            "[package]\nname = \"project-a\"\n",
        ),
        // Workspace Manifest: project-b
        (
            "crates/project_b/Cargo.toml",
            "[package]\nname = \"project-b\"\n",
        ),
        // File A: defines Shared in project_a
        ("crates/project_a/src/lib.toy", "fn Shared"),
        // File B: defines Local in project_b
        ("crates/project_b/src/lib.toy", "fn Local"),
        // File C: calls project_a::Shared (cross-project via workspace qualifier)
        (
            "crates/project_b/src/main.toy",
            "fn main\ncall project_a::Shared",
        ),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // Verify: project_a::Shared should resolve to Shared
    let e = call_edge(&state, "fn:crates/project_b/src/main.toy:main");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:crates/project_a/src/lib.toy:Shared"),
        "project_a::Shared should resolve to Shared (workspace-qualified cross-project), got {:?}",
        e.target
    );
}

#[test]
fn test_workspace_edge_case_crate_name_underscores() {
    // Source files simulating a workspace with underscores in crate name
    let files = &[
        // Workspace Manifest: filigrio_core_compat
        (
            "crates/filigrio_core_compat/Cargo.toml",
            "[package]\nname = \"filigrio_core_compat\"\n",
        ),
        // File A: defines Error
        ("crates/filigrio_core_compat/src/lib.toy", "fn Error"),
        // File B: calls filigrio_core_compat::Error
        (
            "crates/filigrio_core_compat/src/main.toy",
            "fn main\ncall filigrio_core_compat::Error",
        ),
    ];

    let (store, ..) = cold(files);
    let state = store.current().unwrap();

    // Verify: filigrio_core_compat::Error should resolve to Error
    let e = call_edge(&state, "fn:crates/filigrio_core_compat/src/main.toy:main");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:crates/filigrio_core_compat/src/lib.toy:Error"),
        "filigrio_core_compat::Error should resolve to Error (crate with underscores), got {:?}",
        e.target
    );
}
