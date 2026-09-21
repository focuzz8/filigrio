//! Graph data model: nodes, edges, the parse-then-link `EdgeTarget`, and the
//! per-file `Extraction` contract (HLD §4, ADR-0003/0008).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Stable, deterministic node identity (HLD §11.4). In the real port this is
/// derived from source path + symbol so unchanged files yield identical ids.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl NodeId {
    pub fn new(s: impl Into<String>) -> Self {
        NodeId(s.into())
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Provenance as a first-class enum, never a float (ADR-0008).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Confidence {
    /// Explicit in source.
    Extracted,
    /// Deduced across files.
    Inferred,
    /// Uncertain; surfaced for review.
    Ambiguous,
}

/// A source line span, 1-based and inclusive. Stored as numbers so consumers
/// (e.g. a `read_file` range) can arithmetic on it; the `L42` / `L42-L58` string
/// is presentation, produced only at render boundaries via [`Span::render`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    /// A single-line span.
    pub fn line(line: u32) -> Self {
        Span {
            start: line,
            end: line,
        }
    }

    /// `L42` when single-line, else `L42-L58`.
    pub fn render(&self) -> String {
        if self.end > self.start {
            format!("L{}-L{}", self.start, self.end)
        } else {
            format!("L{}", self.start)
        }
    }

    /// Parse `L42` or `L42-L58` back to a span (the graph.json import direction).
    pub fn parse(s: &str) -> Option<Span> {
        let s = s.strip_prefix('L')?;
        match s.split_once("-L") {
            Some((a, b)) => Some(Span {
                start: a.parse().ok()?,
                end: b.parse().ok()?,
            }),
            None => {
                let n = s.parse().ok()?;
                Some(Span { start: n, end: n })
            }
        }
    }
}

/// A graph vertex: a definition (function, class, file, concept).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub label: String,
    pub kind: String, // function | class | file | concept | ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_span: Option<Span>, // {start,end}; renders as "L42" / "L42-L58"
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: BTreeMap<String, String>,
}

impl Node {
    pub fn new(id: impl Into<String>, label: impl Into<String>, kind: impl Into<String>) -> Self {
        Node {
            id: NodeId::new(id),
            label: label.into(),
            kind: kind.into(),
            source_file: None,
            source_span: None,
            attrs: BTreeMap::new(),
        }
    }

    /// The `L..` location string for display (`""` when unknown).
    pub fn loc(&self) -> String {
        self.source_span.map(|s| s.render()).unwrap_or_default()
    }
}

/// Parse-then-link: extraction emits `Symbol(..)`; resolution binds it to
/// `Node(..)` (HLD §11.0).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeTarget {
    /// Unresolved — the symbol this edge is trying to reach.
    Symbol(TargetRef),
    /// Resolved (linked) to a concrete node.
    Node(NodeId),
}

/// What "relink" matches on; keyed by the `ReverseIndex`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TargetRef {
    pub name: String, // callee / imported symbol
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hints: BTreeMap<String, String>, // import alias, receiver, scope
}

impl TargetRef {
    pub fn new(name: impl Into<String>) -> Self {
        TargetRef {
            name: name.into(),
            hints: BTreeMap::new(),
        }
    }
}

/// A directed relationship. `target` starts unresolved, bound at the link stage.
///
/// `Hash` is derived (with `EdgeTarget`/`TargetRef`/`Confidence`) so an edge can
/// be a hash-map key: the ADR-0042 Phase 1d patch delta computes its
/// `edges_added`/`edges_removed` as a **multiset** diff, which needs edge
/// identity to be hashable, not just comparable.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    pub source: NodeId,
    pub relation: String, // calls | imports | uses | ...
    pub confidence: Confidence,
    pub target: EdgeTarget,
}

/// One entry in a file's **export table** (ADR-0020). Exports are a per-file
/// interface *declaration* — a table, not references — so they ride alongside
/// nodes/edges rather than as graph edges. `export *` has no symbol target,
/// which is one reason edges are the wrong shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Export {
    /// A name defined *and* exported by this file (`export function greet()`).
    Local { name: String },
    /// A re-export: `export { imported as name } from "specifier"`. `name` is what
    /// this module exposes; `imported` is the name in the source module.
    ReExport {
        name: String,
        specifier: String,
        imported: String,
    },
    /// `export * from "specifier"` — re-export every name from the source module.
    Star { specifier: String },
}

/// The normalized per-file output every `Extractor` produces (ADR-0003/0020):
/// `nodes` + `edges` (the code graph) plus the file's `exports` table.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extraction {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// The file's export interface (ADR-0020). Default-empty; a language that
    /// doesn't emit exports yet degrades to the file-level resolution fallback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<Export>,
}

/// A classified unit of work for the index stage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub path: String,
    pub kind: ArtifactKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    Code,
    Doc,
    Other,
}

/// Route a path to an [`Artifact`] (code vs doc vs other) from its extension — the
/// **single** path-classification vocabulary the whole system shares. It's pure
/// (no I/O) and yields core types, so it lives here in the kernel rather than in
/// the Source adapter: the extractor dispatches on it, the resolver filters on it,
/// and freshness's `is_indexable` derives from it. Identical for cold and warm
/// walks (HLD §5.1).
pub fn classify(path: &str) -> Artifact {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    let (kind, language) = match ext.as_str() {
        "rs" => (ArtifactKind::Code, Some("rust")),
        "py" => (ArtifactKind::Code, Some("python")),
        "js" | "mjs" | "cjs" => (ArtifactKind::Code, Some("javascript")),
        "ts" | "tsx" => (ArtifactKind::Code, Some("typescript")),
        "go" => (ArtifactKind::Code, Some("go")),
        "java" => (ArtifactKind::Code, Some("java")),
        "c" | "h" => (ArtifactKind::Code, Some("c")),
        "cpp" | "cc" | "hpp" => (ArtifactKind::Code, Some("cpp")),
        "md" | "rst" | "txt" => (ArtifactKind::Doc, None),
        _ => (ArtifactKind::Other, None),
    };
    Artifact {
        path: path.to_string(),
        kind,
        language: language.map(str::to_string),
    }
}

/// Manifest basenames that mark a **project boundary** (a dependency unit) and
/// name a package (ADR-0018). `tsconfig.json` is deliberately absent — a
/// within-package alias detail, not a boundary.
pub const MANIFEST_NAMES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "deno.json",
    "deno.jsonc",
    "go.mod",
    "pyproject.toml",
];

/// The manifest basename of `path` if it names a project manifest
/// (`.../Cargo.toml` → `Some("Cargo.toml")`), else `None`.
pub fn manifest_basename(path: &str) -> Option<&'static str> {
    let base = path.rsplit('/').next().unwrap_or(path);
    MANIFEST_NAMES.iter().copied().find(|m| *m == base)
}

/// Whether `path` names a project manifest — a graph-affecting non-code file.
pub fn is_manifest(path: &str) -> bool {
    manifest_basename(path).is_some()
}

/// Whether `path` is worth putting through the incremental engine — a file whose
/// *changes affect the graph*: **code** (→ nodes) or a **project manifest** (→ the
/// workspace/module structure, ADR-0019). The producer/reconcile pre-filter — a
/// *performance* gate, not the authority (the extractor's `handles` and
/// `maintain_workspace` are the real gates downstream). It stops non-graph edits
/// (README, images, lockfiles) from triggering no-op re-applies, while — unlike a
/// bare code check — keeping `Cargo.toml`/`package.json` so a manifest edit
/// re-indexes.
///
/// Intentionally **path-shape-agnostic** — keys on basename + extension only —
/// because callers pass both absolute (ingest watcher) and relative (pipeline
/// reconcile) paths.
pub fn is_indexable(path: &str) -> bool {
    classify(path).kind == ArtifactKind::Code || is_manifest(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_renders_single_line_and_range() {
        assert_eq!(Span::line(42).render(), "L42");
        assert_eq!(Span { start: 29, end: 41 }.render(), "L29-L41");
    }

    #[test]
    fn span_parse_roundtrips_render() {
        for s in [Span::line(42), Span { start: 29, end: 41 }] {
            assert_eq!(Span::parse(&s.render()), Some(s));
        }
        assert_eq!(Span::parse("nonsense"), None);
    }

    #[test]
    fn node_loc_renders_span_or_empty() {
        let mut n = Node::new("id", "f", "function");
        assert_eq!(n.loc(), "");
        n.source_span = Some(Span { start: 3, end: 9 });
        assert_eq!(n.loc(), "L3-L9");
    }

    #[test]
    fn is_indexable_covers_code_and_manifests_only() {
        assert!(is_indexable("src/main.rs"), "code");
        assert!(is_indexable("crate/Cargo.toml"), "manifest");
        assert!(!is_indexable("README.md"), "doc is not indexable");
        assert!(!is_indexable("Cargo.lock"), "lockfile is not a manifest");
    }
}
