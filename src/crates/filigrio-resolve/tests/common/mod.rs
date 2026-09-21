//! Shared test harness for the ADR-0042 Phase 1 (scoped re-resolution) suites.
//!
//! The toy `Source`/`Extractor` DSL is the same one `resolution.rs` uses (see
//! that file's header for the grammar: `fn`, `method`, `call`, `mcall`, `rcall`,
//! `import`, `export`, `reexport`, `exportstar`). Promoted here so the scoped-
//! equivalence, impact-coverage and symbol-index suites share one linker
//! observable and one apply harness parameterized by [`LinkScope`].

#![allow(dead_code)]

use filigrio_core::{
    ChangeSet, Confidence, Edge, EdgeTarget, Export, Extraction, Extractor, Graph, GraphDelta,
    GraphState, GraphStore, Node, NodeId, Result, Source, TargetRef,
};
use filigrio_resolve::{ClusterConfig, Engine, LinkScope};
use filigrio_store::MemoryStore;
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::hash::Hasher;
use std::io::Write as _;
use std::path::Path;

// ---- toy Source + Extractor (verbatim from resolution.rs) -------------------

pub struct ToySource {
    files: RefCell<BTreeMap<String, String>>,
    reads: RefCell<Vec<String>>,
    /// Paths that **exist** but whose `read` fails — the permission/IO case
    /// (ADR-0042 F8). The vanished/unreadable split must keep these hard errors.
    unreadable: RefCell<BTreeSet<String>>,
}

impl ToySource {
    pub fn new(files: &[(&str, &str)]) -> Self {
        ToySource {
            files: RefCell::new(
                files
                    .iter()
                    .map(|(p, c)| (p.to_string(), c.to_string()))
                    .collect(),
            ),
            reads: RefCell::new(Vec::new()),
            unreadable: RefCell::new(BTreeSet::new()),
        }
    }
    pub fn set(&self, path: &str, content: &str) {
        self.files.borrow_mut().insert(path.into(), content.into());
    }
    pub fn remove(&self, path: &str) {
        self.files.borrow_mut().remove(path);
    }
    /// Make `path` exist-but-unreadable (the `chmod 000` analogue): `exists`
    /// keeps answering `true`, `read` returns an IO error.
    pub fn poison(&self, path: &str) {
        self.unreadable.borrow_mut().insert(path.into());
    }
    pub fn reads(&self) -> Vec<String> {
        self.reads.borrow().clone()
    }
    pub fn clear_reads(&self) {
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
        if self.unreadable.borrow().contains(path) {
            return Err(filigrio_core::Error::Io(format!(
                "permission denied: {path}"
            )));
        }
        self.files
            .borrow()
            .get(path)
            .map(|s| s.clone().into_bytes())
            .ok_or_else(|| filigrio_core::Error::NotFound(path.into()))
    }
    /// Deliberately **not** recorded in `reads`: a resolver existence probe is not
    /// a file read, and `reads()` is the instrumentation the O(change) tests assert
    /// on ("only the changed files were re-read").
    ///
    /// A poisoned path still exists — that is the whole point of the split
    /// (ADR-0042 F8): absence is convergence, unreadability is an error.
    fn exists(&self, path: &str) -> bool {
        self.files.borrow().contains_key(path)
    }
}

pub struct ToyExtractor {
    extracts: RefCell<Vec<String>>,
}

impl ToyExtractor {
    pub fn new() -> Self {
        ToyExtractor {
            extracts: RefCell::new(Vec::new()),
        }
    }
    pub fn extracts(&self) -> Vec<String> {
        self.extracts.borrow().clone()
    }
    pub fn clear(&self) {
        self.extracts.borrow_mut().clear();
    }
}

impl Default for ToyExtractor {
    fn default() -> Self {
        Self::new()
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
        let mut file_node = Node::new(file_id.0.clone(), path.clone(), "file");
        file_node.source_file = Some(path.clone());
        let mut nodes = vec![file_node];
        let mut edges = Vec::new();
        let mut exports: Vec<Export> = Vec::new();
        let mut current: Option<NodeId> = None;

        for line in text.lines() {
            let t = line.trim();
            if let Some(name) = t.strip_prefix("export ") {
                exports.push(Export::Local {
                    name: name.trim().to_string(),
                });
            } else if let Some(rest) = t.strip_prefix("reexport ") {
                let mut it = rest.split_whitespace();
                if let (Some(name), Some(spec), Some(imported)) = (it.next(), it.next(), it.next())
                {
                    exports.push(Export::ReExport {
                        name: name.to_string(),
                        specifier: spec.to_string(),
                        imported: imported.to_string(),
                    });
                }
            } else if let Some(spec) = t.strip_prefix("exportstar ") {
                exports.push(Export::Star {
                    specifier: spec.trim().to_string(),
                });
            } else if let Some(rest) = t.strip_prefix("method ") {
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

// ---- real-corpus Source (a directory of files, held in memory) --------------
//
// Used by the corpus-driven suites (`shadow.rs`, `git_convergence.rs`) which run
// the *real* extractors over a checkout instead of the toy DSL above. Kept here
// so both share one definition (and one `delta_key`).

/// Every file of interest under some root, keyed by its path relative to that
/// root (always `/`-separated). Mutable so a harness can replay a changeset
/// against it incrementally.
pub struct DirSource {
    files: RefCell<BTreeMap<String, String>>,
}

impl DirSource {
    /// Load every `.rs` file (and every project manifest) under `root`.
    pub fn load(root: &Path) -> Self {
        Self::load_ext(root, &["rs"])
    }

    /// Load every file under `root` that [`path_in_corpus`] accepts (extension in
    /// `exts`, **or** a project manifest — ADR-0042 Phase 1a.4), pruning
    /// `node_modules` + common build-output dirs so a big-repo run (next.js)
    /// doesn't ingest gigabytes of dependencies.
    pub fn load_ext(root: &Path, exts: &[&str]) -> Self {
        Self::load_ext_counted(root, exts).0
    }

    /// [`DirSource::load_ext`] plus the number of in-corpus files the walk
    /// **silently skipped** because they could not be read as UTF-8 (ADR-0042
    /// Phase 1a.5: the replayed view and the fresh reload apply the same skip
    /// rule, so a drift check over them passes by construction — the only way to
    /// know the rule fired is to count it).
    pub fn load_ext_counted(root: &Path, exts: &[&str]) -> (Self, usize) {
        let mut files = BTreeMap::new();
        let mut skipped = 0usize;
        collect_ext_counted(root, root, exts, &mut files, &mut skipped);
        (
            DirSource {
                files: RefCell::new(files),
            },
            skipped,
        )
    }
    pub fn set(&self, rel: &str, body: &str) {
        self.files.borrow_mut().insert(rel.into(), body.into());
    }
    pub fn remove(&self, rel: &str) {
        self.files.borrow_mut().remove(rel);
    }
    pub fn get(&self, rel: &str) -> Option<String> {
        self.files.borrow().get(rel).cloned()
    }
    pub fn contains(&self, rel: &str) -> bool {
        self.files.borrow().contains_key(rel)
    }
    pub fn len(&self) -> usize {
        self.files.borrow().len()
    }
    pub fn is_empty(&self) -> bool {
        self.files.borrow().is_empty()
    }
    /// The path set, sorted — for diffing an incrementally-maintained view
    /// against a fresh load of the same tree.
    pub fn paths(&self) -> Vec<String> {
        self.files.borrow().keys().cloned().collect()
    }
}

impl Source for DirSource {
    fn poll(&self, _since: Option<&filigrio_core::Revision>) -> Result<ChangeSet> {
        let mut paths: Vec<String> = self.files.borrow().keys().cloned().collect();
        paths.sort();
        Ok(ChangeSet::all_added(paths))
    }
    fn read(&self, path: &str) -> Result<Vec<u8>> {
        self.files
            .borrow()
            .get(path)
            .map(|s| s.clone().into_bytes())
            .ok_or_else(|| filigrio_core::Error::NotFound(path.into()))
    }
    /// Membership, not a copy of the body — `read(..).is_ok()` clones every
    /// probed file's contents just to answer a boolean. Same predicate as `read`:
    /// the map holds exactly the readable files.
    fn exists(&self, path: &str) -> bool {
        self.files.borrow().contains_key(path)
    }
}

/// Directory names never walked: dependency trees, build output, VCS metadata.
/// A path is "of interest" iff no ancestor directory is in here or hidden — the
/// rule [`collect_ext`] walks and [`path_pruned`] re-derives for a git pathname.
pub const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "dist",
    ".next",
    "build",
    "coverage",
    "target",
    "out",
    ".git",
];

/// Recursively collect the in-corpus files under `dir`, pruning [`SKIP_DIRS`] and
/// hidden dirs so a big-repo walk stays bounded. Membership is decided by
/// [`path_in_corpus`] — the *same* predicate the git changeset filter applies, so
/// a diff and a directory load can never disagree on the file set.
pub fn collect_ext(base: &Path, dir: &Path, exts: &[&str], out: &mut BTreeMap<String, String>) {
    let mut skipped = 0;
    collect_ext_counted(base, dir, exts, out, &mut skipped);
}

/// [`collect_ext`], counting the in-corpus files skipped because they are not
/// readable UTF-8 (ADR-0042 Phase 1a.5 — instrument, don't infer).
pub fn collect_ext_counted(
    base: &Path,
    dir: &Path,
    exts: &[&str],
    out: &mut BTreeMap<String, String>,
    skipped: &mut usize,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                continue;
            }
            collect_ext_counted(base, &p, exts, out, skipped);
        } else {
            let Ok(rel_path) = p.strip_prefix(base) else {
                continue;
            };
            let rel = rel_path.to_string_lossy().replace('\\', "/");
            if !path_in_corpus(&rel, exts) {
                continue;
            }
            match std::fs::read_to_string(&p) {
                Ok(body) => {
                    out.insert(rel, body);
                }
                Err(_) => *skipped += 1,
            }
        }
    }
}

/// Would [`collect_ext`] have skipped `rel` (a `/`-separated repo-relative path)
/// because of a pruned or hidden ancestor directory? The changeset side of the
/// same rule the walker applies, so a git diff and a directory load agree on
/// exactly one file set.
pub fn path_pruned(rel: &str) -> bool {
    let mut comps: Vec<&str> = rel.split('/').collect();
    comps.pop(); // the basename is a file, not a directory
    comps
        .iter()
        .any(|c| c.starts_with('.') || SKIP_DIRS.contains(c))
}

/// Does `rel` belong in a corpus filtered to `exts`?
///
/// **Project manifests are always in-corpus** regardless of `exts` (ADR-0042
/// Phase 1a.4). They carry no nodes — no extractor `handles` them — but they are
/// the *only* input to `GraphState.workspace` (ADR-0019) and to the source-tier
/// `ModuleResolver` (ADR-0020). Excluding them left `workspace` degenerate and
/// its comparison vacuous. `filigrio_core::is_manifest` is the
/// authoritative rule (the same one `is_indexable`, the daemon watcher and
/// pipeline reconcile key on), so the harness cannot drift from production.
pub fn path_in_corpus(rel: &str, exts: &[&str]) -> bool {
    if path_pruned(rel) {
        return false;
    }
    filigrio_core::is_manifest(rel)
        || Path::new(rel)
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|e| exts.contains(&e))
}

/// Apply a changeset over arbitrary (dyn) source/extractor ports — the
/// real-corpus counterpart of [`apply_scope`].
pub fn apply_dyn(
    prior: &GraphState,
    cs: &ChangeSet,
    src: &dyn Source,
    ext: &dyn Extractor,
    scope: LinkScope,
) -> GraphDelta {
    Engine::apply_with_scope(prior, cs, src, ext, &ClusterConfig::default(), scope).expect("apply")
}

/// Cold build (`poll` = all-added) of `src` under `Global` — the reference build.
pub fn cold_ext(src: &dyn Source, ext: &dyn Extractor) -> GraphState {
    let store = MemoryStore::new();
    let cs = src.poll(None).unwrap();
    let d = apply_dyn(&GraphState::default(), &cs, src, ext, LinkScope::Global);
    store.apply_delta(&d).unwrap();
    store.current().unwrap()
}

// ---- facet digests: the whole state, nothing excluded (ADR-0042 Phase 1a) ----
//
// The big-repo harnesses cannot afford `normalize()`'s clone (a next.js
// `GraphState` is ~GBs and the comparison already holds two chains resident), and
// the previous answer — a hand-enumerated subset of facets — silently excluded
// `Node.attrs`, `partition`, `symbols` and `manifest`. The answer here is a
// *digest per facet* instead of a narrower comparison: streaming each field's
// canonical `Debug` encoding into a hasher costs no clone and no big allocation,
// and the field list is enforced exhaustive by destructuring (below), so nothing
// can be omitted by accident.

/// Hash sink that eats `Debug` output as it is produced. Every facet below is
/// built from `BTreeMap`/`Vec`, whose `Debug` output has deterministic order, so
/// `{:?}` *is* a canonical encoding of the value.
struct HashSink(DefaultHasher);

impl std::io::Write for HashSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Hasher::write(&mut self.0, buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Digest of `v`'s canonical `Debug` encoding, computed without materializing it.
pub fn hash_debug<T: Debug + ?Sized>(v: &T) -> u64 {
    let mut sink = HashSink(DefaultHasher::new());
    let _ = write!(sink, "{v:?}");
    sink.0.finish()
}

/// The **complete** identity of a node: `id/kind/label/source_file/span` plus
/// every attr. `attrs` is included deliberately (ADR-0042 Phase 1a.1) — ADR-0026's
/// inferred `returns` and the `impl` owner live there, so while it was excluded a
/// carry bug that corrupted an attr on a rarely-called function was invisible.
pub fn node_key(n: &Node) -> String {
    let mut s = format!(
        "{}\t{}\t{}\t{}\t{}",
        n.id.0,
        n.kind,
        n.label,
        n.source_file.as_deref().unwrap_or(""),
        n.loc(),
    );
    // `attrs` is a BTreeMap — iteration order is already canonical.
    for (k, v) in &n.attrs {
        s.push('\t');
        s.push_str(k);
        s.push('=');
        s.push_str(v);
    }
    s
}

/// `(source, relation, confidence, target)` as one string — the edge identity
/// used by every canonical comparison here. Unresolved (`S:`) targets carry their
/// hints, so a silently-downgraded edge is a difference.
pub fn edge_key(e: &Edge) -> String {
    let tgt = match &e.target {
        EdgeTarget::Node(n) => format!("N:{}", n.0),
        EdgeTarget::Symbol(r) => format!("S:{}:{:?}", r.name, {
            let mut h: Vec<_> = r.hints.iter().collect();
            h.sort();
            h
        }),
    };
    format!(
        "{}\t{}\t{:?}\t{}",
        e.source.0, e.relation, e.confidence, tgt
    )
}

/// Sorted [`node_key`]s — order-independent, so two states built in different
/// orders still compare equal.
pub fn node_keys(nodes: &[Node]) -> Vec<String> {
    let mut v: Vec<String> = nodes.iter().map(node_key).collect();
    v.sort();
    v
}

/// Sorted [`edge_key`]s.
pub fn edge_keys(edges: &[Edge]) -> Vec<String> {
    let mut v: Vec<String> = edges.iter().map(edge_key).collect();
    v.sort();
    v
}

/// One digest per `GraphState` field. **Exhaustive by construction**: the
/// destructure below fails to compile if a field is ever added to `GraphState`
/// without a facet here, which is what makes "the excluded set is empty" a
/// compiler-checked claim rather than a comment.
pub fn state_facets(s: &GraphState) -> BTreeMap<&'static str, u64> {
    let GraphState {
        graph,
        partition,
        symbols,
        reverse,
        manifest,
        workspace,
        exports,
        symbol_index,
    } = s;
    let Graph { nodes, edges } = graph;
    BTreeMap::from([
        ("nodes", hash_debug(&node_keys(nodes))),
        ("edges", hash_debug(&edge_keys(edges))),
        ("partition", hash_debug(partition)),
        ("symbols", hash_debug(symbols)),
        ("reverse", hash_debug(reverse)),
        ("manifest", hash_debug(manifest)),
        ("workspace", hash_debug(workspace)),
        ("exports", hash_debug(exports)),
        ("symbol_index", hash_debug(symbol_index)),
    ])
}

/// One digest per `GraphDelta` field — same exhaustiveness guarantee. Two deltas
/// with equal facets produce equal states under the store's merge.
///
/// `affected` is included: it is telemetry, but it is computed from
/// scope-independent inputs, so a difference there is a real behavioural
/// difference worth surfacing rather than an accepted one.
///
/// The ADR-0042 Phase 1d patch facets (`edges_removed`, `dirty_files`,
/// `reverse_dropped`, `reverse_added`) are digested here too, with **no
/// exclusions** — which is the gate on B10's claim that the patch is
/// scope-independent. `edges_added` no longer means "the whole edge set"; a
/// scope that computed a *larger-than-minimal* patch (e.g. by reading the
/// carried/re-emitted split off `Scoped` instead of diffing) would show up here
/// as a `Global ≠ Scoped` divergence on every shadow step.
pub fn delta_facets(d: &GraphDelta) -> BTreeMap<&'static str, u64> {
    let GraphDelta {
        nodes_added,
        nodes_removed,
        edges_added,
        edges_removed,
        dirty_files,
        partition,
        symbols,
        reverse_dropped,
        reverse_added,
        manifest,
        workspace,
        exports,
        symbol_index,
        affected,
        vanished,
    } = d;
    let mut removed: Vec<&str> = nodes_removed.iter().map(|n| n.0.as_str()).collect();
    removed.sort_unstable();
    let mut rev_dropped: Vec<&str> = reverse_dropped.iter().map(|n| n.0.as_str()).collect();
    rev_dropped.sort_unstable();
    // `(name, Reference)` pairs: sorted, so the digest compares the reference
    // *set* a delta contributes rather than the order it happened to emit it in.
    let mut rev_added: Vec<String> = reverse_added
        .iter()
        .map(|(n, r)| format!("{n}\t{r:?}"))
        .collect();
    rev_added.sort();
    BTreeMap::from([
        ("nodes_added", hash_debug(&node_keys(nodes_added))),
        ("nodes_removed", hash_debug(&removed)),
        ("edges_added", hash_debug(&edge_keys(edges_added))),
        ("edges_removed", hash_debug(&edge_keys(edges_removed))),
        ("dirty_files", hash_debug(dirty_files)),
        ("partition", hash_debug(partition)),
        ("symbols", hash_debug(symbols)),
        ("reverse_dropped", hash_debug(&rev_dropped)),
        ("reverse_added", hash_debug(&rev_added)),
        ("manifest", hash_debug(manifest)),
        ("workspace", hash_debug(workspace)),
        ("exports", hash_debug(exports)),
        ("symbol_index", hash_debug(symbol_index)),
        ("affected", hash_debug(affected)),
        ("vanished", hash_debug(vanished)),
    ])
}

/// Facet names whose digests differ, skipping `exclude`.
pub fn facet_mismatches(
    a: &BTreeMap<&'static str, u64>,
    b: &BTreeMap<&'static str, u64>,
    exclude: &[&str],
) -> Vec<&'static str> {
    a.iter()
        .filter(|(k, v)| !exclude.contains(*k) && b.get(*k) != Some(v))
        .map(|(k, _)| *k)
        .collect()
}

/// The first few differences between two sorted key lists, for a readable
/// failure (a raw `assert_eq!` on 500k strings is unreadable).
pub fn sample_diff(a: &[String], b: &[String], label: &str) -> Option<String> {
    if a == b {
        return None;
    }
    let sa: std::collections::BTreeSet<&String> = a.iter().collect();
    let sb: std::collections::BTreeSet<&String> = b.iter().collect();
    let only_a: Vec<&&String> = sa.difference(&sb).take(5).collect();
    let only_b: Vec<&&String> = sb.difference(&sa).take(5).collect();
    Some(format!(
        "{label}: {} vs {} entries; only-left({}) {:?}; only-right({}) {:?}",
        a.len(),
        b.len(),
        sa.difference(&sb).count(),
        only_a,
        sb.difference(&sa).count(),
        only_b,
    ))
}

/// Bounded key-level diff of two maps — the readable form of "this index differs"
/// (which keys appeared, vanished, or changed value).
fn map_diff<K: Ord + Debug, V: PartialEq + Debug>(
    a: &BTreeMap<K, V>,
    b: &BTreeMap<K, V>,
    label: &str,
) -> String {
    let only_a: Vec<&K> = a.keys().filter(|k| !b.contains_key(k)).collect();
    let only_b: Vec<&K> = b.keys().filter(|k| !a.contains_key(k)).collect();
    let changed: Vec<&K> = a
        .iter()
        .filter(|(k, v)| b.get(k).is_some_and(|o| o != *v))
        .map(|(k, _)| k)
        .collect();
    let sample = changed.first().map(|k| {
        let (l, r) = (&a[k], &b[k]);
        format!(" e.g. {k:?}: {} vs {}", trunc(l), trunc(r))
    });
    format!(
        "{label}: only-left({}) {:?}; only-right({}) {:?}; changed({}) {:?}{}",
        only_a.len(),
        only_a.iter().take(5).collect::<Vec<_>>(),
        only_b.len(),
        only_b.iter().take(5).collect::<Vec<_>>(),
        changed.len(),
        changed.iter().take(5).collect::<Vec<_>>(),
        sample.unwrap_or_default(),
    )
}

/// `{:?}` capped at 200 chars — a per-key value, never a whole index.
fn trunc<T: Debug>(v: &T) -> String {
    let s = format!("{v:?}");
    if s.len() > 200 {
        format!("{}…", &s[..200])
    } else {
        s
    }
}

/// Human-readable detail for one differing facet.
fn state_facet_detail(a: &GraphState, b: &GraphState, facet: &str) -> String {
    match facet {
        "nodes" => sample_diff(
            &node_keys(&a.graph.nodes),
            &node_keys(&b.graph.nodes),
            "nodes",
        )
        .unwrap_or_else(|| "nodes: digest collision?".into()),
        "edges" => sample_diff(
            &edge_keys(&a.graph.edges),
            &edge_keys(&b.graph.edges),
            "edges",
        )
        .unwrap_or_else(|| "edges: digest collision?".into()),
        "partition" => format!(
            "{} || {}",
            map_diff(
                &a.partition.node_community,
                &b.partition.node_community,
                "partition.node_community"
            ),
            map_diff(
                &a.partition.communities,
                &b.partition.communities,
                "partition.communities"
            )
        ),
        "symbols" => map_diff(&a.symbols.defs, &b.symbols.defs, "symbols.defs"),
        "reverse" => map_diff(&a.reverse.refs, &b.reverse.refs, "reverse.refs"),
        "manifest" => map_diff(&a.manifest.entries, &b.manifest.entries, "manifest.entries"),
        "workspace" => map_diff(
            &a.workspace.projects,
            &b.workspace.projects,
            "workspace.projects",
        ),
        "exports" => map_diff(&a.exports.by_file, &b.exports.by_file, "exports.by_file"),
        "symbol_index" => map_diff(
            &a.symbol_index.by_name,
            &b.symbol_index.by_name,
            "symbol_index.by_name",
        ),
        other => format!("{other}: differs"),
    }
}

/// Every way two states differ, one string per facet, skipping `exclude`.
/// Empty ⇒ the two states are identical on **every** `GraphState` field.
pub fn state_diff(a: &GraphState, b: &GraphState, exclude: &[&str]) -> Vec<String> {
    facet_mismatches(&state_facets(a), &state_facets(b), exclude)
        .into_iter()
        .map(|f| state_facet_detail(a, b, f))
        .collect()
}

// ---- changeset builders -----------------------------------------------------

pub fn added(paths: &[&str]) -> ChangeSet {
    ChangeSet::all_added(paths.iter().map(|s| s.to_string()))
}

pub fn modified(paths: &[&str]) -> ChangeSet {
    ChangeSet {
        modified: paths.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

pub fn changeset(added: &[&str], modified: &[&str], removed: &[&str]) -> ChangeSet {
    ChangeSet {
        added: added.iter().map(|s| s.to_string()).collect(),
        modified: modified.iter().map(|s| s.to_string()).collect(),
        removed: removed.iter().map(|s| s.to_string()).collect(),
    }
}

// ---- scope-parameterized apply ----------------------------------------------

pub fn apply_scope(
    store: &MemoryStore,
    prior: &GraphState,
    cs: &ChangeSet,
    src: &ToySource,
    ext: &ToyExtractor,
    scope: LinkScope,
) -> GraphDelta {
    let delta = Engine::apply_with_scope(prior, cs, src, ext, &ClusterConfig::default(), scope)
        .expect("apply");
    store.apply_delta(&delta).expect("store");
    delta
}

/// Cold build of `files` (poll = all-added) under `scope`.
pub fn cold_scope(files: &[(&str, &str)], scope: LinkScope) -> GraphState {
    let src = ToySource::new(files);
    let ext = ToyExtractor::new();
    let store = MemoryStore::new();
    let cs = src.poll(None).unwrap();
    apply_scope(&store, &GraphState::default(), &cs, &src, &ext, scope);
    store.current().unwrap()
}

/// One incremental commit: files to write/overwrite, files to remove, and the
/// changeset that announces it.
pub struct Step {
    pub write: Vec<(&'static str, &'static str)>,
    pub remove: Vec<&'static str>,
    pub cs: ChangeSet,
}

/// Run `initial` as a cold build, then apply each `Step` in order — all under
/// `scope` — and return the final state.
pub fn run_incremental(initial: &[(&str, &str)], steps: &[Step], scope: LinkScope) -> GraphState {
    let src = ToySource::new(initial);
    let ext = ToyExtractor::new();
    let store = MemoryStore::new();
    let cs = src.poll(None).unwrap();
    apply_scope(&store, &GraphState::default(), &cs, &src, &ext, scope);
    for step in steps {
        for (p, c) in &step.write {
            src.set(p, c);
        }
        for p in &step.remove {
            src.remove(p);
        }
        let prior = store.current().unwrap();
        apply_scope(&store, &prior, &step.cs, &src, &ext, scope);
    }
    store.current().unwrap()
}

/// The final on-disk file set after applying every `Step` to `initial`.
pub fn final_files(initial: &[(&str, &str)], steps: &[Step]) -> Vec<(String, String)> {
    let mut map: BTreeMap<String, String> = initial
        .iter()
        .map(|(p, c)| (p.to_string(), c.to_string()))
        .collect();
    for step in steps {
        for (p, c) in &step.write {
            map.insert(p.to_string(), c.to_string());
        }
        for p in &step.remove {
            map.remove(*p);
        }
    }
    map.into_iter().collect()
}

/// Cold build of the sequence's final tree (always `Global` — the reference).
pub fn cold_final(initial: &[(&str, &str)], steps: &[Step]) -> GraphState {
    let files = final_files(initial, steps);
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    cold_scope(&refs, LinkScope::Global)
}

// ---- canonical comparison ---------------------------------------------------

fn edge_sort_key(e: &Edge) -> (String, String, String, String) {
    let tgt = match &e.target {
        EdgeTarget::Node(n) => format!("N:{}", n.0),
        EdgeTarget::Symbol(r) => {
            let mut hints: Vec<String> = r.hints.iter().map(|(k, v)| format!("{k}={v}")).collect();
            hints.sort();
            format!("S:{}:{}", r.name, hints.join(","))
        }
    };
    (
        e.source.0.clone(),
        e.relation.clone(),
        format!("{:?}", e.confidence),
        tgt,
    )
}

/// A canonical (order-independent) copy of `state`: nodes sorted by id, edges
/// sorted by `(source, relation, confidence, target)`. Every other field is a
/// `BTreeMap`/`BTreeSet` (already canonical), so two normalized states compare
/// equal with `==` iff they are semantically identical — the strongest possible
/// equivalence check (covers graph, partition, reverse, symbol_index, …).
pub fn normalize(state: &GraphState) -> GraphState {
    let mut s = state.clone();
    s.graph.nodes.sort_by(|a, b| a.id.0.cmp(&b.id.0));
    s.graph.edges.sort_by_key(edge_sort_key);
    s
}

/// Assert two states agree on **resolution** — the node set, the resolved edge
/// set, the reverse index, the symbol index, exports and workspace — but *not*
/// the community partition. Clustering is warm-started (ADR-0024), so an
/// incremental partition legitimately differs from a cold build's; the existing
/// convergence suites (`incremental_equals_cold`, daemon `convergence.rs`)
/// likewise compare only nodes + edges. This is the `apply(diffs) ≡ cold`
/// contract for the resolution stage — exactly what Phase 1 preserves.
pub fn assert_resolution_eq(a: &GraphState, b: &GraphState, msg: &str) {
    let mut na = normalize(a);
    let mut nb = normalize(b);
    na.partition = Default::default();
    nb.partition = Default::default();
    if na == nb {
        return;
    }
    // Exact `==` above is the authority (these are small fixtures, so the clone is
    // affordable); `state_diff` only narrows the failure to a named facet.
    let d = state_diff(a, b, &["partition"]);
    assert!(
        !d.is_empty(),
        "{msg}: states differ but every facet digest matches — the comparator is \
         losing information (a key encoding is ambiguous, or a digest collided)"
    );
    panic!("{msg}: {}", d.join("\n  | "));
}

/// Assert two states are **fully** identical, partition included — the strongest
/// check, used for `Scoped ≡ Global` (which cluster over identical edge sets from
/// identical prior partitions, so their partitions must match too).
pub fn assert_state_eq(a: &GraphState, b: &GraphState, msg: &str) {
    let (na, nb) = (normalize(a), normalize(b));
    if na == nb {
        return;
    }
    let d = state_diff(a, b, &[]);
    assert!(
        !d.is_empty(),
        "{msg}: states differ but every facet digest matches — the comparator is \
         losing information (a key encoding is ambiguous, or a digest collided)"
    );
    panic!("{msg}: {}", d.join("\n  | "));
}
