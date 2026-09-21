//! TDD spec for Phase 2a — **real incremental cross-file resolution**
//! (migration-plan §Phase 2a, HLD §11.0). Written before the implementation.
//!
//! What is under test is the *linked graph* `Engine::apply` produces and its
//! incremental behaviour — black-box, via the emitted `GraphDelta` folded
//! through the real `MemoryStore`:
//!
//!   * **Provenance rules** (the port's documented scheme, grounded in
//!     migration-plan §Phase-1/2a and graphify-description §Confidence):
//!       - same-file single def   → `Node`, `EXTRACTED` (in-file direct ref)
//!       - cross-file single def  → `Node`, `INFERRED`  (call-graph 2nd pass)
//!       - ≥2 candidate defs      → `Node` (deterministic pick), `AMBIGUOUS`
//!       - no def                 → stays `Symbol`, surfaced (not dropped)
//!   * **Parallel-edge dedup** — a symbol called twice yields one edge.
//!   * **ReverseIndex** maintained: name → every reference site (source, rel).
//!   * **Incremental == cold**: editing files then `apply` yields the *same
//!     linked graph* as a cold `apply(∅, all)`; extraction is O(change).
//!   * **Relink via ReverseIndex**: changing a definition re-binds exactly its
//!     dependents — with **no rescan** of the files that reference it.

use filigrio_core::{
    ChangeSet, Confidence, Edge, EdgeTarget, Extraction, GraphDelta, GraphState, GraphStore, Node,
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
// Both counters record activity so tests can assert "no rescan".

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
    fn set(&self, path: &str, content: &str) {
        self.files.borrow_mut().insert(path.into(), content.into());
    }
    fn reads(&self) -> Vec<String> {
        self.reads.borrow().clone()
    }
    fn clear_reads(&self) {
        self.reads.borrow_mut().clear();
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
    /// Not recorded in `reads` — an existence probe is not a read (see the
    /// `ToySource` in `tests/common`).
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
    fn extracts(&self) -> Vec<String> {
        self.extracts.borrow().clone()
    }
    fn clear(&self) {
        self.extracts.borrow_mut().clear();
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
        let mut file_node = Node::new(file_id.0.clone(), path.clone(), "file");
        file_node.source_file = Some(path.clone());
        let mut nodes = vec![file_node];
        let mut edges = Vec::new();
        let mut exports: Vec<filigrio_core::Export> = Vec::new();
        let mut current: Option<NodeId> = None;

        for line in text.lines() {
            let t = line.trim();
            if let Some(name) = t.strip_prefix("export ") {
                // `export NAME` → a local export (name defined + exported here).
                exports.push(filigrio_core::Export::Local {
                    name: name.trim().to_string(),
                });
            } else if let Some(rest) = t.strip_prefix("reexport ") {
                // `reexport NAME SPEC IMPORTED` → export { IMPORTED as NAME } from SPEC.
                let mut it = rest.split_whitespace();
                if let (Some(name), Some(spec), Some(imported)) = (it.next(), it.next(), it.next())
                {
                    exports.push(filigrio_core::Export::ReExport {
                        name: name.to_string(),
                        specifier: spec.to_string(),
                        imported: imported.to_string(),
                    });
                }
            } else if let Some(spec) = t.strip_prefix("exportstar ") {
                // `exportstar SPEC` → export * from SPEC.
                exports.push(filigrio_core::Export::Star {
                    specifier: spec.trim().to_string(),
                });
            } else if let Some(rest) = t.strip_prefix("method ") {
                // `method TYPE NAME` (optionally `-> RET`) → a method node labelled
                // NAME with an `impl` owner attr, id `fn:<path>:TYPE::NAME` (models
                // same-name methods on different types living in one or more files).
                let (rest, ret) = match rest.split_once("->") {
                    Some((r, ty)) => (r.trim(), Some(ty.trim().to_string())),
                    None => (rest.trim(), None),
                };
                let mut it = rest.split_whitespace();
                if let (Some(owner), Some(name)) = (it.next(), it.next()) {
                    let id = NodeId::new(format!("fn:{path}:{owner}::{name}"));
                    let mut n = Node::new(id.0.clone(), name, "function");
                    n.source_file = Some(path.clone());
                    n.attrs.insert("impl".into(), owner.to_string());
                    if let Some(ty) = ret {
                        n.attrs.insert("returns".into(), ty);
                    }
                    nodes.push(n);
                    edges.push(Edge {
                        source: file_id.clone(),
                        relation: "contains".into(),
                        confidence: Confidence::Extracted,
                        target: EdgeTarget::Node(id.clone()),
                    });
                    current = Some(id);
                }
            } else if let Some(name) = t.strip_prefix("fn ") {
                // `fn NAME` or `fn NAME -> TYPE` (the latter stamps a `returns`
                // attr, so a variable bound to NAME()'s result gets type TYPE).
                let (name, ret) = match name.split_once("->") {
                    Some((n, ty)) => (n.trim(), Some(ty.trim().to_string())),
                    None => (name.trim(), None),
                };
                let id = NodeId::new(format!("fn:{path}:{name}"));
                let mut n = Node::new(id.0.clone(), name, "function");
                n.source_file = Some(path.clone());
                if let Some(ty) = ret {
                    n.attrs.insert("returns".into(), ty);
                }
                nodes.push(n);
                edges.push(Edge {
                    source: file_id.clone(),
                    relation: "contains".into(),
                    confidence: Confidence::Extracted,
                    target: EdgeTarget::Node(id.clone()),
                });
                current = Some(id);
            } else if let Some(rest) = t.strip_prefix("import ") {
                // `import BOUND SPECIFIER [IMPORTED]` → an imports edge (file →
                // BOUND) carrying the module specifier and, for an alias, the
                // pre-alias source name (else BOUND).
                let mut it = rest.split_whitespace();
                if let (Some(bound), Some(spec)) = (it.next(), it.next()) {
                    let mut tref = TargetRef::new(bound);
                    tref.hints.insert("specifier".into(), spec.to_string());
                    if let Some(imported) = it.next() {
                        tref.hints.insert("imported".into(), imported.to_string());
                    }
                    edges.push(Edge {
                        source: file_id.clone(),
                        relation: "imports".into(),
                        confidence: Confidence::Extracted,
                        target: EdgeTarget::Symbol(tref),
                    });
                }
            } else if let Some(rest) = t.strip_prefix("rcall ") {
                // `rcall CALLEE METHOD` (or `rcall OWNER::CALLEE METHOD`) → a call
                // to METHOD whose receiver is bound to the result of CALLEE (a
                // free/associated call). Deferred: the resolver reads CALLEE's
                // return type and narrows METHOD to that type's method.
                if let Some(src) = &current {
                    let mut it = rest.split_whitespace();
                    if let (Some(callee), Some(method)) = (it.next(), it.next()) {
                        let mut r = TargetRef::new(method);
                        match callee.split_once("::") {
                            Some((owner, c)) => {
                                r.hints.insert("recv_returns".into(), c.to_string());
                                r.hints
                                    .insert("recv_returns_owner".into(), owner.to_string());
                            }
                            None => {
                                r.hints.insert("recv_returns".into(), callee.to_string());
                            }
                        }
                        edges.push(Edge {
                            source: src.clone(),
                            relation: "calls".into(),
                            confidence: Confidence::Extracted,
                            target: EdgeTarget::Symbol(r),
                        });
                    }
                }
            } else if let Some(name) = t.strip_prefix("mcall ") {
                // `mcall NAME` → an *opaque-receiver* method call: a `calls` edge
                // whose ref is marked `recv=opaque` (the extractor's marker for
                // `x.method()` with an un-inferable receiver type).
                if let Some(src) = &current {
                    let mut r = TargetRef::new(name.trim());
                    r.hints.insert("recv".into(), "opaque".into());
                    edges.push(Edge {
                        source: src.clone(),
                        relation: "calls".into(),
                        confidence: Confidence::Extracted,
                        target: EdgeTarget::Symbol(r),
                    });
                }
            } else if let Some(name) = t.strip_prefix("call ") {
                if let Some(src) = &current {
                    // `call TYPE::NAME` carries a receiver-type hint; `call NAME`
                    // does not (bare name, as before).
                    let name = name.trim();
                    let tref = match name.split_once("::") {
                        Some((ty, m)) => {
                            let mut r = TargetRef::new(m);
                            r.hints.insert("type".into(), ty.to_string());
                            r
                        }
                        None => TargetRef::new(name),
                    };
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

/// The state `prior` becomes once `delta` is merged onto it.
///
/// Since ADR-0042 Phase 1d P1 a delta is a **patch**, so "what did this apply
/// produce" is a question about `merge(prior, delta)`, not about reading
/// `delta.edges_added` as if it were the whole edge set (which it was, and is
/// not any more). `DeferredStore` is the merge that takes an explicit prior, so
/// a test can name the base it means — here a deliberately *corrupted* prior
/// that the store the fixture built does not hold.
fn merged(prior: &GraphState, delta: &GraphDelta) -> GraphState {
    let store = filigrio_store::DeferredStore::new(prior);
    store.apply_delta(delta).expect("merge");
    store.take().expect("an applied delta yields a state")
}

fn cold(files: &[(&str, &str)]) -> (MemoryStore, ToySource, ToyExtractor) {
    let src = ToySource::new(files);
    let ext = ToyExtractor::new();
    let store = MemoryStore::new();
    let cs = src.poll(None).unwrap();
    apply_to(&store, &GraphState::default(), &cs, &src, &ext);
    (store, src, ext)
}

fn added(paths: &[&str]) -> ChangeSet {
    ChangeSet::all_added(paths.iter().map(|s| s.to_string()))
}

fn modified(paths: &[&str]) -> ChangeSet {
    ChangeSet {
        modified: paths.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

/// One canonical string per edge — the observable the specs compare on.
fn edge_key(e: &Edge) -> String {
    let tgt = match &e.target {
        EdgeTarget::Node(n) => format!("Node({})", n.0),
        EdgeTarget::Symbol(r) => format!("Symbol({})", r.name),
    };
    format!(
        "{} -{} {:?}-> {}",
        e.source.0, e.relation, e.confidence, tgt
    )
}

fn edge_keys(s: &GraphState) -> Vec<String> {
    let mut v: Vec<String> = s.graph.edges.iter().map(edge_key).collect();
    v.sort();
    v
}

fn node_ids(s: &GraphState) -> Vec<String> {
    let mut v: Vec<String> = s.graph.nodes.iter().map(|n| n.id.0.clone()).collect();
    v.sort();
    v
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

// ---- provenance -------------------------------------------------------------

#[test]
fn same_file_call_resolves_extracted() {
    // a and b in the same file → in-file direct reference → EXTRACTED.
    let (store, ..) = cold(&[("m", "fn a\ncall b\nfn b")]);
    let e = call_edge(&store.current().unwrap(), "fn:m:a");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:m:b"),
        "same-file call links to the def node, got {:?}",
        e.target
    );
    assert_eq!(e.confidence, Confidence::Extracted, "same-file → EXTRACTED");
}

#[test]
fn cross_file_call_resolves_inferred() {
    // caller.a → lib.b across files → deduced link → INFERRED.
    let (store, ..) = cold(&[("caller", "fn a\ncall b"), ("lib", "fn b")]);
    let e = call_edge(&store.current().unwrap(), "fn:caller:a");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:lib:b"),
        "cross-file call links to the def in the other file, got {:?}",
        e.target
    );
    assert_eq!(e.confidence, Confidence::Inferred, "cross-file → INFERRED");
}

#[test]
fn same_file_def_shadows_cross_file_homonym() {
    // `local` is defined in BOTH caller and other. A call to `local` from
    // caller must bind to caller's *own* definition (scope preference) as
    // EXTRACTED — not AMBIGUOUS — matching the graphify oracle (a local def
    // shadows an unrelated cross-file homonym).
    let (store, ..) = cold(&[
        ("caller", "fn a\ncall local\nfn local"),
        ("other", "fn local"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:caller:a");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:caller:local"),
        "same-file definition wins over the cross-file homonym, got {:?}",
        e.target
    );
    assert_eq!(
        e.confidence,
        Confidence::Extracted,
        "same-file scope → EXTRACTED, not AMBIGUOUS"
    );
}

#[test]
fn multiple_defs_resolve_ambiguous() {
    // `b` defined in two files → uncertain → AMBIGUOUS, bound deterministically.
    let (store, ..) = cold(&[
        ("caller", "fn a\ncall b"),
        ("lib1", "fn b"),
        ("lib2", "fn b"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:caller:a");
    assert_eq!(
        e.confidence,
        Confidence::Ambiguous,
        "≥2 candidates → AMBIGUOUS"
    );
    // deterministic pick = the lexicographically-smallest candidate id.
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:lib1:b"),
        "ambiguous edge binds to the deterministic (min-id) candidate, got {:?}",
        e.target
    );
}

// ---- opaque-receiver method calls (ADR-0023) --------------------------------

#[test]
fn opaque_receiver_declines_cross_file_but_bare_call_does_not() {
    // Identical cross-file homonym `iter` in `lib`, referenced two ways from the
    // same file. A *bare* call binds it (INFERRED — a bare name may denote a fn in
    // another file). An *opaque method* call to the same name DECLINES to an honest
    // unresolved Symbol — a wrong homonym bind is worse than a gap. Proves the
    // decline is driven by the `recv=opaque` marker, not the candidate set.
    let (store, ..) = cold(&[
        ("caller", "fn bare\ncall iter\nfn meth\nmcall iter"),
        ("lib", "fn iter"),
    ]);
    let s = store.current().unwrap();

    let bare = call_edge(&s, "fn:caller:bare");
    assert!(
        matches!(&bare.target, EdgeTarget::Node(n) if n.0 == "fn:lib:iter"),
        "bare call still binds the cross-file def, got {:?}",
        bare.target
    );
    assert_eq!(bare.confidence, Confidence::Inferred);

    let meth = call_edge(&s, "fn:caller:meth");
    assert!(
        matches!(&meth.target, EdgeTarget::Symbol(r) if r.name == "iter"),
        "opaque method call declines the cross-file homonym → unresolved, got {:?}",
        meth.target
    );
}

#[test]
fn declined_opaque_call_preserves_recv_opaque_marker(/* ADR-0029 */) {
    // The declined method call stays an unresolved Symbol — and the *reason* it
    // declined (an opaque receiver, ADR-0023) must ride onto the persisted target,
    // not be dropped. The query surface (ADR-0029) reads `hints["recv"] == "opaque"`
    // to annotate "unresolved by-name callers (opaque receivers)" truthfully and to
    // tell an opaque decline from an unknown-external free call. A bare free call —
    // which has no receiver — carries no such marker.
    let (store, ..) = cold(&[
        ("caller", "fn bare\ncall iter\nfn meth\nmcall iter"),
        ("lib", "fn iter"),
    ]);
    let s = store.current().unwrap();

    let meth = call_edge(&s, "fn:caller:meth");
    let EdgeTarget::Symbol(r) = &meth.target else {
        panic!(
            "opaque method call should decline to a Symbol, got {:?}",
            meth.target
        );
    };
    assert_eq!(
        r.hints.get("recv").map(String::as_str),
        Some("opaque"),
        "declined opaque call must preserve recv=opaque on the Symbol, got {:?}",
        r.hints
    );
}

#[test]
fn opaque_receiver_declines_ambiguous_cross_file() {
    // Two cross-file `iter` defs: a bare call would be AMBIGUOUS; an opaque method
    // call refuses to manufacture the edge at all.
    let (store, ..) = cold(&[
        ("caller", "fn a\nmcall iter"),
        ("lib1", "fn iter"),
        ("lib2", "fn iter"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:caller:a");
    assert!(
        matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "iter"),
        "opaque method call with only cross-file homonyms stays unresolved, got {:?}",
        e.target
    );
}

#[test]
fn opaque_receiver_still_binds_same_file() {
    // Decision 2 keeps the same-file bind: a local def is a real locality signal
    // even for an opaque receiver, and recall matters. Only the *cross-file*
    // bare-name bind is declined.
    let (store, ..) = cold(&[("caller", "fn a\nmcall helper\nfn helper")]);
    let e = call_edge(&store.current().unwrap(), "fn:caller:a");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:caller:helper"),
        "same-file def still resolves for an opaque receiver, got {:?}",
        e.target
    );
    assert_eq!(e.confidence, Confidence::Extracted, "same-file → EXTRACTED");
}

// ---- return-type inference (recall counterpart to ADR-0023) -----------------

#[test]
fn return_type_infers_cross_file_receiver() {
    // `let w = compute(); w.go()` with `compute() -> Widget` in another file. The
    // resolver reads compute's return type and narrows `go` to Widget::go — a
    // type-directed link (EXTRACTED), not a homonym guess. The rival `Other::go`
    // must not win.
    let (store, ..) = cold(&[
        ("caller", "fn a\nrcall compute go"),
        ("lib", "fn compute -> Widget"),
        ("types", "method Widget go\nmethod Other go"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:caller:a");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:types:Widget::go"),
        "receiver type inferred from compute()'s return → Widget::go, got {:?}",
        e.target
    );
    assert_eq!(
        e.confidence,
        Confidence::Extracted,
        "type-directed link is certain"
    );
}

#[test]
fn return_type_narrows_by_associated_owner() {
    // `let x = Foo::make(); x.go()` — two `make`s: Foo::make -> A, Bar::make -> B.
    // The owner Foo selects Foo::make, whose return A narrows `go` to A::go (not
    // the rival B::go). Proves the owner disambiguates the *callee* before its
    // return type is read.
    let (store, ..) = cold(&[
        ("caller", "fn a\nrcall Foo::make go"),
        ("mk", "method Foo make -> A\nmethod Bar make -> B"),
        ("ret", "method A go\nmethod B go"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:caller:a");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:ret:A::go"),
        "owner Foo → Foo::make → return A → A::go, got {:?}",
        e.target
    );
    assert_eq!(e.confidence, Confidence::Extracted);
}

#[test]
fn return_type_unknown_declines_like_opaque() {
    // `compute` has no declared return type → the receiver type stays unknown →
    // the call declines to unresolved (never a bare-name homonym bind).
    let (store, ..) = cold(&[
        ("caller", "fn a\nrcall compute go"),
        ("lib", "fn compute"),
        ("types", "method Widget go"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:caller:a");
    assert!(
        matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "go"),
        "unknown return type → honest unresolved, got {:?}",
        e.target
    );
}

#[test]
fn return_type_ambiguous_callee_declines() {
    // Two `compute`s returning different types → the receiver type is not certain
    // → decline rather than guess.
    let (store, ..) = cold(&[
        ("caller", "fn a\nrcall compute go"),
        ("lib1", "fn compute -> Widget"),
        ("lib2", "fn compute -> Gadget"),
        ("types", "method Widget go\nmethod Gadget go"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:caller:a");
    assert!(
        matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "go"),
        "ambiguous return type → honest unresolved, got {:?}",
        e.target
    );
}

#[test]
fn unresolved_call_stays_symbol() {
    // no def for `missing` → edge is surfaced as an unresolved Symbol, kept.
    let (store, ..) = cold(&[("m", "fn a\ncall missing")]);
    let e = call_edge(&store.current().unwrap(), "fn:m:a");
    assert!(
        matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "missing"),
        "unresolved call stays a Symbol (surfaced, not dropped), got {:?}",
        e.target
    );
}

#[test]
fn parallel_calls_deduped() {
    // a calls b twice → one resolved edge, not two (graphify parallel-edge dedup).
    let (store, ..) = cold(&[("m", "fn a\ncall b\ncall b\nfn b")]);
    let n_calls = store
        .current()
        .unwrap()
        .graph
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source.0 == "fn:m:a")
        .count();
    assert_eq!(n_calls, 1, "duplicate parallel calls collapse to one edge");
}

#[test]
fn reverse_index_tracks_every_reference() {
    let (store, ..) = cold(&[("caller", "fn a\ncall b"), ("lib", "fn b")]);
    let state = store.current().unwrap();
    let refs = state.reverse.refs.get("b").expect("reverse index has 'b'");
    assert!(
        refs.iter()
            .any(|r| r.source.0 == "fn:caller:a" && r.relation == "calls"),
        "reverse index records the (source, relation) of every reference: {refs:?}"
    );
}

// ---- receiver-type disambiguation (the homonym gap) -------------------------

/// Return every `calls` edge out of `src_id` (unordered).
fn call_edges(s: &GraphState, src_id: &str) -> Vec<Edge> {
    s.graph
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source.0 == src_id)
        .cloned()
        .collect()
}

#[test]
fn type_hint_disambiguates_cross_file_homonym() {
    // `new` is defined on BOTH S and T, in different files. A *type-qualified*
    // call `T::new` from a third file must bind to T's `new` — INFERRED, not the
    // AMBIGUOUS min-id pick a bare `new` would get.
    let (store, ..) = cold(&[
        ("s", "method S new"),
        ("t", "method T new"),
        ("caller", "fn build\ncall T::new"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:caller:build");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:t:T::new"),
        "type-qualified call binds to the matching type's method, got {:?}",
        e.target
    );
    assert_eq!(
        e.confidence,
        Confidence::Extracted,
        "type-directed resolution is certain → EXTRACTED even cross-file, not AMBIGUOUS"
    );
}

#[test]
fn type_hint_beats_same_file_ambiguity() {
    // Both S::new and T::new live in the SAME file as the caller. Bare-name
    // resolution would see two same-file `new`s → AMBIGUOUS; the type hint picks
    // S → EXTRACTED.
    let (store, ..) = cold(&[("m", "method S new\nmethod T new\nfn build\ncall S::new")]);
    let e = call_edge(&store.current().unwrap(), "fn:m:build");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:m:S::new"),
        "same-file type-qualified call picks the matching method, got {:?}",
        e.target
    );
    assert_eq!(e.confidence, Confidence::Extracted, "same-file → EXTRACTED");
}

#[test]
fn distinct_typed_calls_are_not_deduped() {
    // One function calls both S::new and T::new. These must stay TWO edges
    // (to the two methods), not collapse to one ambiguous `new` edge.
    let (store, ..) = cold(&[
        ("s", "method S new"),
        ("t", "method T new"),
        ("caller", "fn build\ncall S::new\ncall T::new"),
    ]);
    let mut targets: Vec<String> = call_edges(&store.current().unwrap(), "fn:caller:build")
        .iter()
        .filter_map(|e| match &e.target {
            EdgeTarget::Node(n) => Some(n.0.clone()),
            _ => None,
        })
        .collect();
    targets.sort();
    assert_eq!(
        targets,
        vec!["fn:s:S::new".to_string(), "fn:t:T::new".to_string()],
        "each typed call resolves to its own method"
    );
}

#[test]
fn typed_call_to_unknown_type_stays_unresolved() {
    // `Z::new` names a type we don't define (only S has a `new`). Binding it to
    // S's `new` would be wrong — the qualifier is explicit that it is NOT S. So
    // the call is surfaced unresolved (like an external `Vec::new`), not linked
    // to an unrelated homonym.
    let (store, ..) = cold(&[("s", "method S new"), ("caller", "fn build\ncall Z::new")]);
    let e = call_edge(&store.current().unwrap(), "fn:caller:build");
    assert!(
        matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "new"),
        "a type-qualified call to an unknown type stays unresolved, got {:?}",
        e.target
    );
}

// ---- incremental == cold ----------------------------------------------------

#[test]
fn incremental_equals_cold_for_linked_graph() {
    let files = [("caller", "fn a\ncall b"), ("lib", "fn b")];

    // Cold: everything at once.
    let (cold_store, ..) = cold(&files);

    // Incremental: caller first (b unresolved), then lib arrives → must relink.
    let src = ToySource::new(&files);
    let ext = ToyExtractor::new();
    let store = MemoryStore::new();
    apply_to(
        &store,
        &GraphState::default(),
        &added(&["caller"]),
        &src,
        &ext,
    );
    let prior = store.current().unwrap();
    apply_to(&store, &prior, &added(&["lib"]), &src, &ext);

    assert_eq!(
        node_ids(&store.current().unwrap()),
        node_ids(&cold_store.current().unwrap()),
        "same node set"
    );
    assert_eq!(
        edge_keys(&store.current().unwrap()),
        edge_keys(&cold_store.current().unwrap()),
        "incremental linked graph (targets + confidence) equals the cold build"
    );
}

// ---- relink via ReverseIndex, no rescan -------------------------------------

#[test]
fn relink_on_def_removal_reverts_without_rescan() {
    // Cold: caller.a resolves to lib.b (INFERRED).
    let (store, src, ext) = cold(&[("caller", "fn a\ncall b"), ("lib", "fn b")]);
    let e = call_edge(&store.current().unwrap(), "fn:caller:a");
    assert!(
        matches!(&e.target, EdgeTarget::Node(_)),
        "precondition: resolved"
    );

    // Edit lib so it no longer defines b. Only lib is re-indexed.
    src.set("lib", "fn c");
    ext.clear();
    src.clear_reads();
    let prior = store.current().unwrap();
    apply_to(&store, &prior, &modified(&["lib"]), &src, &ext);

    // The dependent edge is re-linked to unresolved — WITHOUT touching caller.
    assert_eq!(
        ext.extracts(),
        vec!["lib".to_string()],
        "only the changed file is extracted"
    );
    assert!(
        !src.reads().contains(&"caller".to_string()),
        "caller is not re-read — relink comes from the ReverseIndex, not a rescan"
    );
    let e = call_edge(&store.current().unwrap(), "fn:caller:a");
    assert!(
        matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "b"),
        "removing the def reverts its dependent to an unresolved Symbol, got {:?}",
        e.target
    );
}

#[test]
fn relink_on_def_addition_links_without_rescan() {
    // Cold: caller.a calls b, but nothing defines b yet → unresolved.
    let (store, src, ext) = cold(&[("caller", "fn a\ncall b")]);
    let e = call_edge(&store.current().unwrap(), "fn:caller:a");
    assert!(
        matches!(&e.target, EdgeTarget::Symbol(_)),
        "precondition: unresolved"
    );

    // A new file defines b. Only lib is indexed; caller is not rescanned.
    src.set("lib", "fn b");
    ext.clear();
    src.clear_reads();
    let prior = store.current().unwrap();
    apply_to(&store, &prior, &added(&["lib"]), &src, &ext);

    assert_eq!(
        ext.extracts(),
        vec!["lib".to_string()],
        "only the new file is extracted"
    );
    assert!(
        !src.reads().contains(&"caller".to_string()),
        "caller is not re-read — the new def relinks it via the ReverseIndex"
    );
    let e = call_edge(&store.current().unwrap(), "fn:caller:a");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:lib:b"),
        "adding the def links the waiting dependent, got {:?}",
        e.target
    );
    assert_eq!(
        e.confidence,
        Confidence::Inferred,
        "cross-file link → INFERRED"
    );
}

// ---- project-scoped resolution (Phase 4b Increment 1, ADR-0018) -------------
//
// The project (nearest ancestor package/module manifest) is the unit of
// resolution. The Engine stamps each node with its `project` and scopes the
// candidate pool to the source's own project — so duplicate names across
// packages no longer cross-bind. Cross-project references await import scope
// (Increment 2); until then they surface as honest unresolved `Symbol`s rather
// than mis-binding to a same-named node in another project.

/// The project root a file resolves to via the `Workspace` (ADR-0019 — project
/// membership is derived here, not stamped on nodes).
fn project_root(s: &GraphState, file: &str) -> Option<String> {
    s.workspace.root_of(file).map(str::to_string)
}

#[test]
fn workspace_maps_file_to_nearest_manifest() {
    // `ui/Cargo.toml` makes `ui/` a project; the file under it derives to it.
    let (store, ..) = cold(&[("ui/Cargo.toml", ""), ("ui/widget.rs", "fn render")]);
    assert_eq!(
        project_root(&store.current().unwrap(), "ui/widget.rs").as_deref(),
        Some("ui"),
        "a file's project = the dir of its nearest ancestor manifest"
    );
}

#[test]
fn nearest_manifest_wins_for_nested_projects() {
    // A root manifest AND a nested one: the nested file belongs to the *nearest*
    // (inner) project, not the root.
    let (store, ..) = cold(&[
        ("Cargo.toml", ""),
        ("crates/inner/Cargo.toml", ""),
        ("crates/inner/x.rs", "fn foo"),
    ]);
    let s = store.current().unwrap();
    assert_eq!(
        project_root(&s, "crates/inner/x.rs").as_deref(),
        Some("crates/inner"),
        "nearest ancestor manifest wins over an outer one"
    );
    // and the workspace knows about both projects.
    assert!(s.workspace.projects.contains_key("crates/inner"));
    assert!(s.workspace.projects.contains_key(""));
}

#[test]
fn same_name_defs_in_different_projects_do_not_cross_bind() {
    // The monorepo bug in miniature: `greet` is defined in TWO packages; a caller
    // in a THIRD package references it with no same-project def. The flat resolver
    // bound this AMBIGUOUS to the min-id (wrong package). Project scoping (with no
    // import yet) makes it an honest unresolved Symbol — never a wrong bind.
    let (store, ..) = cold(&[
        ("ui/Cargo.toml", ""),
        ("ui/greet.rs", "fn greet"),
        ("admin/Cargo.toml", ""),
        ("admin/greet.rs", "fn greet"),
        ("app/Cargo.toml", ""),
        ("app/boot.rs", "fn boot\ncall greet"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:app/boot.rs:boot");
    assert!(
        matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "greet"),
        "cross-package homonym stays an honest unresolved Symbol, got {:?}",
        e.target
    );
}

#[test]
fn cross_project_unique_ref_waits_for_import_scope() {
    // Even a UNIQUE name defined only in another project stays unresolved until
    // an import justifies the cross-project link (Increment 2). Increment 1 never
    // reaches into the whole-repo pool — same-project only (ADR-0018 §5).
    let (store, ..) = cold(&[
        ("lib/Cargo.toml", ""),
        ("lib/util.rs", "fn helper"),
        ("app/Cargo.toml", ""),
        ("app/main.rs", "fn run\ncall helper"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:app/main.rs:run");
    assert!(
        matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "helper"),
        "cross-project ref without an import stays unresolved, got {:?}",
        e.target
    );
}

#[test]
fn same_project_cross_file_still_resolves() {
    // Guard against over-restriction: WITHIN one project, cross-file resolution
    // is unchanged (INFERRED). Only *cross-project* refs are held back.
    let (store, ..) = cold(&[
        ("proj/Cargo.toml", ""),
        ("proj/a.rs", "fn a\ncall b"),
        ("proj/b.rs", "fn b"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:proj/a.rs:a");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:proj/b.rs:b"),
        "same-project cross-file call resolves, got {:?}",
        e.target
    );
    assert_eq!(
        e.confidence,
        Confidence::Inferred,
        "same-project cross-file link → INFERRED (unchanged)"
    );
}

#[test]
fn no_manifest_tree_is_a_single_project() {
    // Regression guard: with no manifests anywhere, every file is the one root
    // project, so scoping is a no-op and cross-file resolution behaves as before.
    let (store, ..) = cold(&[("caller", "fn a\ncall b"), ("lib", "fn b")]);
    let e = call_edge(&store.current().unwrap(), "fn:caller:a");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:lib:b"),
        "no-manifest tree resolves cross-file as one project, got {:?}",
        e.target
    );
    assert_eq!(e.confidence, Confidence::Inferred);
}

// ---- import scope (Phase 4b Increment 2, ADR-0018) --------------------------
//
// A resolved `import name from "<specifier>"` is the strongest signal: it binds
// `name` across a project boundary to the *specific* package it names — the tier
// that flips a cross-project reference from honest-unresolved (Increment 1) to
// correctly-resolved. Specifiers resolve via the manifest-fed `ModuleResolver`.

#[test]
fn import_scope_binds_cross_project_call_to_correct_package() {
    // The monorepo case: `greet` is defined in BOTH @acme/ui and @acme/admin.
    // @acme/app imports it from @acme/ui and calls it — it must bind to *ui*, not
    // the admin homonym (and not stay unresolved as in Increment 1).
    let (store, ..) = cold(&[
        (
            "ui/package.json",
            r#"{"name":"@acme/ui","main":"greet.rs"}"#,
        ),
        ("ui/greet.rs", "fn greet"),
        (
            "admin/package.json",
            r#"{"name":"@acme/admin","main":"greet.rs"}"#,
        ),
        ("admin/greet.rs", "fn greet"),
        (
            "app/package.json",
            r#"{"name":"@acme/app","main":"boot.rs"}"#,
        ),
        ("app/boot.rs", "import greet @acme/ui\nfn boot\ncall greet"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:app/boot.rs:boot");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:ui/greet.rs:greet"),
        "import from @acme/ui binds to ui's greet, not admin's, got {:?}",
        e.target
    );
    assert_eq!(
        e.confidence,
        Confidence::Extracted,
        "an explicit resolved import is certain → EXTRACTED"
    );
}

#[test]
fn relative_import_binds_extracted() {
    // A relative specifier resolves to the sibling file; the call it licenses
    // binds EXTRACTED (an explicit import), stronger than the bare-name INFERRED
    // it would otherwise get.
    let (store, ..) = cold(&[
        ("a.rs", "import helper ./b\nfn run\ncall helper"),
        ("b.rs", "fn helper"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:a.rs:run");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:b.rs:helper"),
        "relative import resolves to the sibling def, got {:?}",
        e.target
    );
    assert_eq!(e.confidence, Confidence::Extracted);
}

#[test]
fn external_import_does_not_leak_cross_project() {
    // `thing` is imported from an EXTERNAL specifier (`react`) the resolver can't
    // follow — even though a `thing` exists in another project, the unresolved
    // specifier must not license that cross-project bind. Stays unresolved.
    let (store, ..) = cold(&[
        (
            "lib/package.json",
            r#"{"name":"@acme/lib","main":"thing.rs"}"#,
        ),
        ("lib/thing.rs", "fn thing"),
        (
            "app/package.json",
            r#"{"name":"@acme/app","main":"boot.rs"}"#,
        ),
        ("app/boot.rs", "import thing react\nfn boot\ncall thing"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:app/boot.rs:boot");
    assert!(
        matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "thing"),
        "an unresolvable external import does not bind cross-project, got {:?}",
        e.target
    );
}

// ---- Workspace as incremental state (Phase 4b Increment 3, ADR-0019) --------

#[test]
fn late_added_manifest_rescopes_without_reindex() {
    // The stale-`project` gap ADR-0019 closes. Project membership is DERIVED from
    // the Workspace, so a manifest that arrives *after* its files re-scopes them
    // with no re-index — and the result matches a cold build.
    let files = [
        ("pkg/Cargo.toml", ""),
        ("pkg/a.rs", "fn a\ncall b"),
        ("other/Cargo.toml", ""),
        ("other/b.rs", "fn b"),
    ];

    // Cold, manifests present: a and b are different projects, no import → unresolved.
    let (cold_store, ..) = cold(&files);
    let ce = call_edge(&cold_store.current().unwrap(), "fn:pkg/a.rs:a");
    assert!(
        matches!(&ce.target, EdgeTarget::Symbol(r) if r.name == "b"),
        "precondition (cold): cross-project call is unresolved, got {:?}",
        ce.target
    );

    // Incremental: index the CODE first, no manifests → one root project → a→b
    // resolves cross-file (INFERRED).
    let src = ToySource::new(&files);
    let ext = ToyExtractor::new();
    let store = MemoryStore::new();
    apply_to(
        &store,
        &GraphState::default(),
        &added(&["pkg/a.rs", "other/b.rs"]),
        &src,
        &ext,
    );
    let e = call_edge(&store.current().unwrap(), "fn:pkg/a.rs:a");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:other/b.rs:b"),
        "precondition (no manifests): resolves as one project, got {:?}",
        e.target
    );

    // Now the manifests arrive — ONLY the manifests are in the changeset.
    ext.clear();
    src.clear_reads();
    let prior = store.current().unwrap();
    apply_to(
        &store,
        &prior,
        &added(&["pkg/Cargo.toml", "other/Cargo.toml"]),
        &src,
        &ext,
    );
    assert!(
        ext.extracts().is_empty(),
        "code files are NOT re-indexed when a manifest arrives: {:?}",
        ext.extracts()
    );
    let e = call_edge(&store.current().unwrap(), "fn:pkg/a.rs:a");
    assert!(
        matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "b"),
        "late manifest re-scopes derived membership → unresolved, got {:?}",
        e.target
    );
    assert_eq!(
        edge_keys(&store.current().unwrap()),
        edge_keys(&cold_store.current().unwrap()),
        "incremental-with-late-manifest linked graph == cold"
    );
}

#[test]
fn workspace_derives_depends_on_from_manifests() {
    // The project graph: `depends_on` = declared deps ∩ known packages. An
    // external dep (`react`) is not a project edge.
    let (store, ..) = cold(&[
        ("ui/package.json", r#"{"name":"@acme/ui"}"#),
        ("ui/x.rs", "fn a"),
        (
            "app/package.json",
            r#"{"name":"@acme/app","dependencies":{"@acme/ui":"1.0.0","react":"18"}}"#,
        ),
        ("app/y.rs", "fn b"),
    ]);
    let ws = store.current().unwrap().workspace;
    let deps = ws.depends_on();
    let app_deps: Vec<&str> = deps
        .get("app")
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();
    assert_eq!(
        app_deps,
        vec!["ui"],
        "app depends on the ui project only (external `react` excluded)"
    );
    assert!(
        deps.get("ui").is_none_or(|d| d.is_empty()),
        "ui has no workspace-internal deps"
    );
}

#[test]
fn workspace_projects_into_visible_graph() {
    // The project graph is visible: a `project` node per project, `depends_on`
    // edges (app→ui), and `contains` edges project → its files.
    let (store, ..) = cold(&[
        ("ui/package.json", r#"{"name":"@acme/ui"}"#),
        ("ui/x.rs", "fn a"),
        (
            "app/package.json",
            r#"{"name":"@acme/app","dependencies":{"@acme/ui":"1.0.0"}}"#,
        ),
        ("app/y.rs", "fn b"),
    ]);
    let s = store.current().unwrap();

    // `project` nodes exist, labelled by package name, and carry no community.
    let proj: Vec<&Node> = s
        .graph
        .nodes
        .iter()
        .filter(|n| n.kind == "project")
        .collect();
    let labels: Vec<&str> = {
        let mut v: Vec<&str> = proj.iter().map(|n| n.label.as_str()).collect();
        v.sort();
        v
    };
    assert_eq!(labels, vec!["@acme/app", "@acme/ui"]);
    for p in &proj {
        assert!(
            !s.partition.node_community.contains_key(&p.id),
            "project nodes are an overlay — no community: {}",
            p.id.0
        );
    }

    // depends_on: app → ui.
    assert!(
        s.graph.edges.iter().any(|e| e.relation == "depends_on"
            && e.source.0 == "project:app"
            && matches!(&e.target, EdgeTarget::Node(t) if t.0 == "project:ui")),
        "depends_on edge app→ui present"
    );
    // contains: project → its file node (links the two levels).
    assert!(
        s.graph.edges.iter().any(|e| e.relation == "contains"
            && e.source.0 == "project:ui"
            && matches!(&e.target, EdgeTarget::Node(t) if t.0 == "file:ui/x.rs")),
        "project→file contains edge present"
    );
}

#[test]
fn project_overlay_is_regenerated_not_duplicated() {
    // The overlay is derived: re-applying the same build must not double project
    // nodes/edges (idempotent), and a manifest edit updates them in place.
    let files = [
        ("ui/package.json", r#"{"name":"@acme/ui"}"#),
        ("ui/x.rs", "fn a"),
    ];
    let src = ToySource::new(&files);
    let ext = ToyExtractor::new();
    let store = MemoryStore::new();
    apply_to(
        &store,
        &GraphState::default(),
        &src.poll(None).unwrap(),
        &src,
        &ext,
    );
    let n1 = store
        .current()
        .unwrap()
        .graph
        .nodes
        .iter()
        .filter(|n| n.kind == "project")
        .count();
    // Re-apply the whole tree: idempotent — still exactly one project node.
    let prior = store.current().unwrap();
    apply_to(&store, &prior, &src.poll(None).unwrap(), &src, &ext);
    let n2 = store
        .current()
        .unwrap()
        .graph
        .nodes
        .iter()
        .filter(|n| n.kind == "project")
        .count();
    assert_eq!(n1, 1, "one project node");
    assert_eq!(n1, n2, "overlay regenerated, not duplicated");
}

// ---- symbol-level resolution: follow re-exports (Phase 4b, ADR-0020) --------

#[test]
fn import_follows_barrel_reexport_to_real_def() {
    // `@acme/ui`'s entry is a BARREL: it re-exports `greet` from `./greet`, where
    // greet is actually defined. `@acme/app` imports greet from `@acme/ui` and
    // calls it — it must bind to ui's `greet.rs`, not stay unresolved (the entry
    // file has no greet *definition*).
    let (store, ..) = cold(&[
        (
            "ui/package.json",
            r#"{"name":"@acme/ui","main":"index.rs"}"#,
        ),
        ("ui/index.rs", "reexport greet ./greet greet"),
        ("ui/greet.rs", "fn greet\nexport greet"),
        (
            "app/package.json",
            r#"{"name":"@acme/app","main":"boot.rs"}"#,
        ),
        ("app/boot.rs", "import greet @acme/ui\nfn boot\ncall greet"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:app/boot.rs:boot");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:ui/greet.rs:greet"),
        "call binds through the barrel to the real def, got {:?}",
        e.target
    );
    assert_eq!(e.confidence, Confidence::Extracted);
}

#[test]
fn import_follows_wildcard_reexport() {
    // `export * from './greet'` — a wildcard barrel. The imported name is found by
    // searching the star source.
    let (store, ..) = cold(&[
        (
            "ui/package.json",
            r#"{"name":"@acme/ui","main":"index.rs"}"#,
        ),
        ("ui/index.rs", "exportstar ./greet"),
        ("ui/greet.rs", "fn greet\nexport greet"),
        (
            "app/package.json",
            r#"{"name":"@acme/app","main":"boot.rs"}"#,
        ),
        ("app/boot.rs", "import greet @acme/ui\nfn boot\ncall greet"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:app/boot.rs:boot");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:ui/greet.rs:greet"),
        "wildcard re-export is followed, got {:?}",
        e.target
    );
}

#[test]
fn import_alias_looks_up_the_original_name() {
    // `import { greet as g } from '@acme/ui'` then `g()` — the export lookup uses
    // the pre-alias name `greet`, the binding is under the local `g`.
    let (store, ..) = cold(&[
        (
            "ui/package.json",
            r#"{"name":"@acme/ui","main":"index.rs"}"#,
        ),
        ("ui/index.rs", "reexport greet ./greet greet"),
        ("ui/greet.rs", "fn greet\nexport greet"),
        (
            "app/package.json",
            r#"{"name":"@acme/app","main":"boot.rs"}"#,
        ),
        ("app/boot.rs", "import g @acme/ui greet\nfn boot\ncall g"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:app/boot.rs:boot");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:ui/greet.rs:greet"),
        "aliased import resolves via the original name, got {:?}",
        e.target
    );
}

#[test]
fn import_follows_multi_hop_reexport_chain() {
    // A 2-hop barrel: index re-exports from `./mid`, which re-exports from
    // `./greet`, where greet is defined. The walk chases the whole chain.
    let (store, ..) = cold(&[
        (
            "ui/package.json",
            r#"{"name":"@acme/ui","main":"index.rs"}"#,
        ),
        ("ui/index.rs", "reexport greet ./mid greet"),
        ("ui/mid.rs", "reexport greet ./greet greet"),
        ("ui/greet.rs", "fn greet\nexport greet"),
        (
            "app/package.json",
            r#"{"name":"@acme/app","main":"boot.rs"}"#,
        ),
        ("app/boot.rs", "import greet @acme/ui\nfn boot\ncall greet"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:app/boot.rs:boot");
    assert!(
        matches!(&e.target, EdgeTarget::Node(n) if n.0 == "fn:ui/greet.rs:greet"),
        "multi-hop re-export chain is followed to the real def, got {:?}",
        e.target
    );
}

#[test]
fn reexport_cycle_does_not_hang() {
    // Two barrels re-export a name from each other (`a.rs` ⇄ `b.rs`) and nothing
    // terminates — resolution must not loop; the call stays unresolved.
    let (store, ..) = cold(&[
        ("p/package.json", r#"{"name":"@acme/p","main":"a.rs"}"#),
        ("p/a.rs", "reexport loop ./b loop"),
        ("p/b.rs", "reexport loop ./a loop"),
        (
            "app/package.json",
            r#"{"name":"@acme/app","main":"boot.rs"}"#,
        ),
        ("app/boot.rs", "import loop @acme/p\nfn boot\ncall loop"),
    ]);
    let e = call_edge(&store.current().unwrap(), "fn:app/boot.rs:boot");
    assert!(
        matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "loop"),
        "a re-export cycle terminates as unresolved, got {:?}",
        e.target
    );
}

// ---- diverged persisted state (reverse ↔ edges consistency guard) -----------

#[test]
fn diverged_reverse_index_does_not_lose_edges() {
    // `apply` produces `reverse` and `graph.edges` together, so a reference-site
    // edge always has a matching `Reference`. A DIVERGED persisted state (e.g. a
    // store reload whose `reverse` lost an entry while `graph.edges` kept the
    // resolved edge) must not silently lose the edge: step 6 drops every prior
    // edge whose `(source, relation)` is a reference site — and caller.a's other
    // surviving `calls` reference (`c`) makes `(fn:caller:a, calls)` a reference
    // site — so without healing the `b` edge is dropped and, with no reference
    // left to re-resolve, never re-emitted.
    let (store, src, ext) = cold(&[("caller", "fn a\ncall b\ncall c"), ("lib", "fn b\nfn c")]);
    let state = store.current().unwrap();
    let resolved: Vec<&Edge> = state
        .graph
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source.0 == "fn:caller:a")
        .collect();
    assert_eq!(resolved.len(), 2, "precondition: both calls resolved");

    // Surgically corrupt the persisted state: drop caller's `b` reference from
    // `reverse`, leaving the resolved edge in `graph.edges`.
    let mut prior = state.clone();
    let removed = prior.reverse.refs.remove("b");
    assert!(removed.is_some(), "precondition: `b` had a reference entry");

    // Apply a changeset touching only an UNRELATED third file.
    src.set("other", "fn unrelated");
    let delta = Engine::apply(
        &prior,
        &added(&["other"]),
        &src,
        &ext,
        &filigrio_resolve::ClusterConfig::default(),
    )
    .expect("apply");

    // The caller→lib.b edge must survive the merge onto the corrupted prior.
    let after = merged(&prior, &delta);
    let b_edges: Vec<&Edge> = after
        .graph
        .edges
        .iter()
        .filter(|e| {
            e.relation == "calls"
                && e.source.0 == "fn:caller:a"
                && match &e.target {
                    EdgeTarget::Node(n) => n.0 == "fn:lib:b",
                    EdgeTarget::Symbol(r) => r.name == "b",
                }
        })
        .collect();
    assert_eq!(
        b_edges.len(),
        1,
        "a diverged reverse index must not silently lose the caller→b edge; got {:?}",
        after
            .graph
            .edges
            .iter()
            .filter(|e| e.source.0 == "fn:caller:a")
            .collect::<Vec<_>>()
    );
    assert!(
        matches!(&b_edges[0].target, EdgeTarget::Node(n) if n.0 == "fn:lib:b"),
        "the healed reference re-resolves to the def, got {:?}",
        b_edges[0].target
    );
    // The edge is *carried*, not re-added: the healed reference re-resolves to
    // exactly what was already there, so a minimal patch says nothing about it.
    assert!(
        !delta
            .edges_removed
            .iter()
            .any(|e| e.source.0 == "fn:caller:a" && e.relation == "calls"),
        "the patch must not retire the caller's edges it then fails to re-add"
    );
    // And the divergence is healed, not just papered over: the reference
    // reappears in the merged reverse index, so the next apply is consistent.
    assert!(
        after
            .reverse
            .refs
            .get("b")
            .is_some_and(|list| list.iter().any(|r| r.source.0 == "fn:caller:a")),
        "the reconstructed reference lands in the merged reverse index"
    );
}

#[test]
fn divergence_is_healed_even_when_the_changeset_drops_nodes() {
    // ADR-0042 Phase 1b: the guard now *detects* before it repairs — pass 1
    // counts references and prior edges per `(source, relation)` and returns
    // immediately when everything balances. This pins the interaction the
    // counting pass has to get right: a changeset that **drops and re-extracts**
    // nodes at the same time as a divergence exists elsewhere.
    //
    // A modified file's nodes are dropped and re-added under the SAME ids, so a
    // source can be in `dropped_ids` and still own fresh references. Pass 1 must
    // skip that site's prior edges (they are re-emitted by re-resolution, not
    // lost) while still finding the deficit at the untouched `caller` site.
    let (store, src, ext) = cold(&[
        ("caller", "fn a\ncall b\ncall c"),
        ("touched", "fn t\ncall b"),
        ("lib", "fn b\nfn c"),
    ]);
    let state = store.current().unwrap();
    let mut prior = state.clone();
    // Corrupt only `caller`'s reference to `b`, keeping its resolved edge.
    let refs = prior.reverse.refs.get_mut("b").expect("`b` has references");
    refs.retain(|r| r.source.0 != "fn:caller:a");
    assert!(
        refs.iter().any(|r| r.source.0 == "fn:touched:t"),
        "precondition: the other referrer of `b` survives the corruption"
    );

    // Modify `touched` in the same apply, so its nodes are dropped and
    // re-extracted under identical ids.
    src.set("touched", "fn t\ncall b\ncall c");
    let delta = Engine::apply(
        &prior,
        &modified(&["touched"]),
        &src,
        &ext,
        &filigrio_resolve::ClusterConfig::default(),
    )
    .expect("apply");
    // Merged onto the *corrupted* prior — the state under test, not the pristine
    // one the fixture's store still holds.
    let after = merged(&prior, &delta);

    let healed: Vec<&Edge> = after
        .graph
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source.0 == "fn:caller:a")
        .collect();
    assert_eq!(
        healed.len(),
        2,
        "caller's b and c edges both survive a concurrent drop-and-re-extract; got {healed:?}"
    );
    assert!(
        after
            .reverse
            .refs
            .get("b")
            .is_some_and(|l| l.iter().any(|r| r.source.0 == "fn:caller:a")),
        "the divergence is healed in the merged reverse index, not just papered over"
    );
    // And the re-extracted file's own references are intact (not mistaken for a
    // divergence and double-counted).
    assert_eq!(
        call_edges(&after, "fn:touched:t").len(),
        2,
        "the re-extracted source keeps exactly its two fresh call edges"
    );
}

#[test]
fn affected_set_counts_the_change() {
    // `affected` is the name-based over-counting telemetry heuristic:
    //   new nodes + removed nodes + surviving referrers of every symbol label
    //   (un)defined by this change.
    // Here: adding lib yields 2 new nodes (file:lib + fn:lib:b), removes 0, and
    // the one surviving referrer of the (newly defined) label `b` is
    // fn:caller:a → affected = 2 + 0 + 1 = 3, exactly.
    let (store, src, ext) = cold(&[("caller", "fn a\ncall b")]);
    src.set("lib", "fn b");
    let prior = store.current().unwrap();
    let delta = apply_to(&store, &prior, &added(&["lib"]), &src, &ext);
    assert_eq!(
        delta.affected, 3,
        "affected = new nodes (2) + removed (0) + relinked referrers of `b` (1)"
    );
}
