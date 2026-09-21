//! The five ports (HLD §4). These traits are the entire external contract of
//! the system; every concrete crate is an adapter over one of them.

use crate::error::Result;
use crate::model::{Artifact, Edge, Extraction, Node};
use crate::state::{
    ChangeSet, CommunityId, CommunityMeta, GraphDelta, GraphState, GraphStats, ProjectGraph,
    QueryOpts, Revision, Subgraph,
};
use serde::{Deserialize, Serialize};

/// Where changes come from. Kappa: one path — `None` = cold full walk,
/// `Some(rev)` = a warm diff (ADR-0014). Paths are `String` in the skeleton;
/// the real adapter uses `Path`.
pub trait Source {
    fn poll(&self, since: Option<&Revision>) -> Result<ChangeSet>;
    fn read(&self, path: &str) -> Result<Vec<u8>>;
    /// Does `path` name a readable file in this source? A **metadata** question —
    /// the `ModuleResolver`'s source tier asks it up to ~19 times per import
    /// specifier (probe `x`, then `x.ts`, `x.js`, …, then `x/index.ts`, …) and
    /// wants only the answer, never the bytes.
    ///
    /// **Required, deliberately undefaulted.** `read(path).is_ok()` is a correct
    /// answer for any source and was how the resolver asked before this method
    /// existed — but it is O(file size) with an allocation, it inherits `read`'s
    /// failure modes (a permission or transient-IO error silently reads as "does
    /// not exist"), and on an instrumented source it counts a *probe* as a *read*.
    /// Defaulting to it would have made the pathological implementation the
    /// invisible one; a source that truly cannot do better should say so at the
    /// adapter, where it is visible. (ADR-0042 Phase 1b, item A: this expression
    /// was the single most expensive thing in the contract stage.)
    ///
    /// **Contract:** `exists(p)` iff `read(p)` would succeed. Answering `true`
    /// more widely (for a directory, say) makes the resolver hand back a path
    /// nothing can read.
    fn exists(&self, path: &str) -> bool;
    /// The real filesystem root, when this source is backed by one — lets the
    /// engine use the exact `oxc_resolver` tier (ADR-0018). A non-filesystem or
    /// mock source returns `None` and gets the source-only resolver.
    fn root(&self) -> Option<&std::path::Path> {
        None
    }
    /// Whether `rel` (a root-relative path) is inside the indexable source
    /// boundary — the ADR-0022 `.gitignore` + noise net (ADR-0032a R1/R2). The
    /// daemon's apply-time scope authority consults this so an out-of-scope file
    /// named by any producer never enters the graph. A non-filesystem or mock
    /// source has no boundary → everything is in scope.
    fn in_scope(&self, _rel: &str) -> bool {
        true
    }
}

/// Turns one artifact into unresolved nodes/edges. Pure, no cross-file state
/// (ADR-0003). Real adapters: tree-sitter per language, SCIP (ADR-0010).
pub trait Extractor {
    fn handles(&self, artifact: &Artifact) -> bool;
    fn extract(&self, artifact: &Artifact, bytes: &[u8]) -> Result<Extraction>;
}

/// Dumb persistence — not a query engine (ADR-0005). Incremental-first, so it
/// patches rather than only rewrites.
pub trait GraphStore {
    fn load_state(&self) -> Result<Option<GraphState>>;
    fn apply_delta(&self, delta: &GraphDelta) -> Result<()>;
    fn snapshot(&self) -> Result<()>;
}

/// Edge direction for a neighbor query: `Out` = callees (this node → others),
/// `In` = callers (others → this node — what a hub's high in-degree is made of),
/// `Both` = either. Discovery of "what calls X" needs `In`/`Both` (ADR-0027).
///
/// The one definition in the workspace: the wire contract re-exports *this*
/// type (`filigrio_protocol::Direction`) instead of declaring a parallel
/// `EdgeDirection` a transport has to hand-convert. It is `Serialize`/
/// `Deserialize` for that reason — the kernel itself never serializes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Out,
    In,
    #[default]
    Both,
}

/// The domain query contract; MCP/CLI/HTTP are transports over it (ADR-0006).
/// `shortest_path` and seed-scored budget packing land with Phase 2.
pub trait GraphQuery {
    fn query(&self, q: &str, opts: QueryOpts) -> Result<Subgraph>;
    /// **Every** node with this label — homonyms are distinct nodes, so a label is
    /// an ambiguous key (many `from`/`new`/`from_config`). Transports use this to
    /// disambiguate at the boundary rather than silently picking one (ADR-0027).
    fn nodes_by_label(&self, label: &str) -> Result<Vec<Node>>;
    /// The node with this exact id — the only globally-unique address (a label is
    /// not). `None` if absent.
    fn node_by_id(&self, id: &str) -> Result<Option<Node>>;
    /// Neighbors of the node with this exact **id** (not label), in the requested
    /// `direction` — the specific homonym resolved, not an arbitrary same-named one
    /// (ADR-0027). Each result's `Node` is the *other* endpoint; the `Edge` carries
    /// which way it points (its `source` is this node for an outgoing edge).
    ///
    /// `relations` is a **filter set, OR'd**, each entry matched by
    /// [`crate::relation::relation_matches_filter`] — so `["type"]` selects the
    /// whole `type/…` family, `["type/param", "type/return"]` selects two of its
    /// members, and the ADR-0044 filter words [`crate::relation::filter::ANY`] /
    /// [`crate::relation::filter::SEMANTIC`] select everything / everything
    /// non-structural. Same shape and same matcher as
    /// [`QueryOpts::context_filter`]: one relation-filter semantics across the
    /// read surface, not two.
    ///
    /// **Empty here means no filter** — this is the *port*, below the query
    /// vocabulary. A read surface normalizes the caller's empty set with
    /// [`crate::relation::filter::normalize`] (empty ⇒ `semantic`) *before*
    /// calling in, so the "what does absence mean" decision is taken in one
    /// place rather than re-decided by each transport (ADR-0044).
    fn neighbors_by_id(
        &self,
        id: &str,
        relations: &[String],
        direction: Direction,
    ) -> Result<Vec<(Edge, Node)>>;
    /// **Unresolved outgoing** edges of the node with this exact **id**: calls this
    /// node makes that declined to bind (ADR-0023 opaque receivers, unknown
    /// externals). Each is a full [`Edge`] whose `target` is still
    /// [`EdgeTarget::Symbol`] — the callee is a bare name, not an addressable node
    /// (that is what "unresolved" means). Exact: keyed by the node's own id. Empty
    /// if the node has no declined calls (ADR-0029).
    fn unresolved_out_by_id(&self, id: &str) -> Result<Vec<Edge>>;
    /// **Unresolved incoming-by-name** edges for a `label`: every unresolved
    /// `Symbol` edge whose callee `name` equals `label`. The returned [`Node`] is
    /// the **caller** (the edge's `source` — an addressable enclosing def), which
    /// the transport surfaces so the model can read it to confirm. This is a
    /// **by-name heuristic** — it may over-match homonyms — so callers must mark the
    /// result unresolved/by-name, never present it as a resolved caller (ADR-0029).
    fn unresolved_in_by_label(&self, label: &str) -> Result<Vec<(Edge, Node)>>;
    fn community(&self, id: CommunityId) -> Result<Vec<Node>>;
    /// Metadata for a community — its label, size, and cohesion (ADR-0024).
    /// `None` if the id names no community.
    fn community_meta(&self, id: CommunityId) -> Result<Option<CommunityMeta>>;
    fn god_nodes(&self, top_n: usize) -> Result<Vec<(Node, usize)>>;
    /// Shortest directed path `from_id`→`to_id` within `max_hops`, as the node
    /// sequence (inclusive of both ends). Endpoints are addressed by exact **id**
    /// (not label) — the same id-keying as [`GraphQuery::neighbors_by_id`], so a
    /// homonym endpoint resolves to the intended node, not an arbitrary same-named
    /// one (ADR-0027). `None` if either id is absent or unreachable in range.
    fn shortest_path(
        &self,
        from_id: &str,
        to_id: &str,
        max_hops: usize,
    ) -> Result<Option<Vec<Node>>>;
    fn stats(&self) -> Result<GraphStats>;
    /// The project dependency graph (ADR-0019) — the monorepo architecture map.
    /// Empty when the tree has no package/module manifests.
    fn project_graph(&self) -> Result<ProjectGraph>;
}

/// Resolves an import specifier to a concrete file in the source tree — the
/// sixth port (ADR-0018). `import X from "<specifier>"` in `importing_file` →
/// **which file?**. Structure-dependent (monorepo layout, workspace packages,
/// relative paths), so it is separated from the language-pure `Extractor`. Like
/// `Source`, it touches the filesystem (probing candidate files), so it is an
/// environment adapter.
///
/// Returns `None` for a specifier that resolves outside the indexed tree
/// (`react`, `jsr:…`, a URL, an un-followed `node_modules`) — those become
/// external boundary references, not indexed. Adapters: a source-only tier
/// (manifests + relative/workspace probing; `oxc_resolver` for JS/TS) and an
/// optional toolchain-backed tier (`cargo metadata`, `deno info`).
pub trait ModuleResolver {
    fn resolve(&self, importing_file: &str, specifier: &str) -> Option<String>;
}
