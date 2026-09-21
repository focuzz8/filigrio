//! Incremental engine state (ADR-0012) and the query-side value types.
//!
//! `GraphState` is the stream operator's **keyed state** (ADR-0016);
//! `GraphDelta` is the patch `apply` emits. Since ADR-0042 Phase 1d P1 the delta
//! genuinely *is* a patch for the three facets a module-sharded store has to
//! route by — edges, `reverse`, and the dirty-file set; the rest replace
//! wholesale by decision (B2), not by omission. See [`GraphDelta`].

use crate::graph::Graph;
use crate::model::{Edge, Export, Node, NodeId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A VCS commit id (or equivalent). Updates are keyed by `(repo, revision)`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Revision(pub String);

/// Stable community id (ADR-0012, HLD §11.2).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct CommunityId(pub u64);

/// `{ added, modified, removed }` paths — the input to the engine. A cold build
/// is a ChangeSet where everything is "added" (ADR-0012).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
}

impl ChangeSet {
    /// The cold-build shape: the whole tree as all-added.
    pub fn all_added(paths: impl IntoIterator<Item = String>) -> Self {
        ChangeSet {
            added: paths.into_iter().collect(),
            modified: Vec::new(),
            removed: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }

    pub fn len(&self) -> usize {
        self.added.len() + self.modified.len() + self.removed.len()
    }

    /// Paths that must be (re)indexed: added ∪ modified.
    pub fn to_index(&self) -> impl Iterator<Item = &String> {
        self.added.iter().chain(self.modified.iter())
    }

    /// Every path named in the changeset: added ∪ modified ∪ removed. Prior
    /// contributions for all of these are cleared before re-indexing, so
    /// re-applying the same changeset is idempotent (HLD §11.4) — re-indexing
    /// an "added" path that already exists must not double its nodes/edges.
    pub fn touched(&self) -> impl Iterator<Item = &String> {
        self.added
            .iter()
            .chain(self.modified.iter())
            .chain(self.removed.iter())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub hash: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
}

/// path → content hash + last processed `Revision`; drives change detection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub entries: BTreeMap<String, ManifestEntry>,
}

impl Manifest {
    /// Highest revision recorded so far — the cursor a warm `poll` resumes from.
    pub fn latest_revision(&self) -> Option<Revision> {
        self.entries
            .values()
            .filter_map(|e| e.revision.clone())
            .max()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityMeta {
    pub id: CommunityId,
    pub label: String,
    pub size: usize,
    /// Intra-community edge weight as a fraction of the community's total incident
    /// weight — an honest *quality* signal (ADR-0024): `1000` = every edge stays
    /// inside, lower = more edges leak across the boundary. Stored as **permille
    /// `u16`** (not `f64`) so the partition keeps `Eq`/determinism and no float
    /// lands in serialized state; read it via [`CommunityMeta::cohesion`].
    /// `#[serde(default)]` so pre-0024 `state.json` loads (cohesion 0).
    #[serde(default)]
    pub cohesion_permille: u16,
}

impl CommunityMeta {
    /// Cohesion in `[0.0, 1.0]` — the reported fraction of this community's incident
    /// edge weight that is internal (ADR-0024).
    pub fn cohesion(&self) -> f64 {
        self.cohesion_permille as f64 / 1000.0
    }
}

/// node → community assignment plus stable community ids (ADR-0012, HLD §11.2).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Partition {
    pub node_community: BTreeMap<NodeId, CommunityId>,
    pub communities: BTreeMap<CommunityId, CommunityMeta>,
}

/// symbol/name → defining node. The "link" side of parse-then-link (HLD §11).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolTable {
    pub defs: BTreeMap<String, NodeId>,
}

/// One project = the directory of a package/module manifest (the dependency
/// boundary, ADR-0018). Keyed by `root` — the stable structural identity; the
/// package `name` lives only here, never copied onto files/symbols (ADR-0019).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Directory containing the manifest — the project id (`packages/ui`).
    pub root: String,
    /// Manifest basename that declared it (`package.json`, `Cargo.toml`, …).
    pub manifest: String,
    /// Package/module name = the specifier other projects import it by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Entry file relative to `root` (npm `main`, deno `exports`), if declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    /// Raw declared dependency names (workspace-internal *and* external); the
    /// project graph (`depends_on`) is these intersected with known package names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<String>,
}

/// The **repository structure** as first-class, incrementally-maintained state
/// (ADR-0019): the set of projects, keyed by root. `by_name` and `depends_on`
/// are *derived views* over this canonical set (computed, not stored, so they
/// cannot drift). Project membership of a path is a longest-prefix lookup —
/// derived, never stamped onto nodes, so a rename touches only this one place.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// project root dir → project. The canonical, persisted structure.
    pub projects: BTreeMap<String, Project>,
}

impl Workspace {
    /// The project a path belongs to: the nearest ancestor project root (longest
    /// matching prefix), or `None` for the root/no-manifest area.
    ///
    /// **Probes ancestors instead of scanning projects** (ADR-0042 Phase 1b).
    /// A root `r` contains `path` (the `path_under` rule) iff `r` is `""`, is `path`
    /// itself, or is an ancestor *directory* of `path` — so the matching roots are
    /// exactly the (at most `depth + 2`) ancestor prefixes of `path`, and the
    /// longest one is the first hit walking them from longest to shortest. That
    /// makes this `O(depth · log projects)` rather than `O(projects · |path|)`;
    /// on next.js (694 projects × 102k nodes) the scan form was **392 ms per
    /// apply**, ~10 % of the whole apply, for an answer that never depended on the
    /// project count. `projects` is keyed by `root` (the canonical structure —
    /// `maintain_workspace` is the only writer and inserts `root → Project{root}`),
    /// so a key lookup and a `p.root` comparison are the same test; the
    /// `equivalent_to_the_scan` property test pins that they agree.
    pub fn project_of(&self, path: &str) -> Option<&Project> {
        if let Some(p) = self.projects.get(path) {
            return Some(p);
        }
        let mut dir = path;
        while let Some(cut) = dir.rfind('/') {
            dir = &dir[..cut];
            if let Some(p) = self.projects.get(dir) {
                return Some(p);
            }
        }
        // A manifest at the repository root contains everything.
        self.projects.get("")
    }

    /// The project *root* a path belongs to (its scoping key for resolution).
    pub fn root_of(&self, path: &str) -> Option<&str> {
        self.project_of(path).map(|p| p.root.as_str())
    }

    /// Package name → its project. The `ModuleResolver`'s workspace-package map,
    /// derived from `projects` (only projects that declare a name).
    pub fn by_name(&self) -> BTreeMap<&str, &Project> {
        self.projects
            .values()
            .filter_map(|p| p.name.as_deref().map(|n| (n, p)))
            .collect()
    }

    /// The project graph: each project → the workspace-*internal* projects it
    /// depends on (declared deps that name another project here). Derived.
    pub fn depends_on(&self) -> BTreeMap<&str, BTreeSet<&str>> {
        let by_name = self.by_name();
        self.projects
            .values()
            .map(|p| {
                let deps = p
                    .deps
                    .iter()
                    .filter_map(|d| by_name.get(d.as_str()).map(|t| t.root.as_str()))
                    .filter(|r| *r != p.root) // no self-edge
                    .collect();
                (p.root.as_str(), deps)
            })
            .collect()
    }
}

/// Is `path` inside project `root` (or is the root itself)? An empty root (a
/// manifest at the repo root) contains everything.
///
/// The **containment rule** [`Workspace::project_of`] implements. It is no longer
/// evaluated per project on the hot path (that scan cost 392 ms per apply at
/// next.js scale — see `project_of`); it survives as the *reference* definition
/// the ancestor-probe form is property-tested against, which is why it is
/// `cfg(test)`. Change this and the probe must change with it — the test fails
/// loudly if they disagree.
#[cfg(test)]
fn path_under(path: &str, root: &str) -> bool {
    root.is_empty()
        || path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// One reference site: the `source` node that names a symbol via `relation`
/// (`calls`/`imports`). Carries enough to reconstruct the unresolved edge, so a
/// definition change can re-link exactly its dependents (HLD §11.0).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Reference {
    pub source: NodeId,
    pub relation: String,
    /// Receiver/type qualifier at the call site (`T::method` → `Some("T")`), used
    /// to disambiguate method-name homonyms during resolution. `None` for a bare
    /// name. Part of the reference identity, so `S::new` and `T::new` from the
    /// same source stay distinct references (HLD §11.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_hint: Option<String>,
    /// Raw import specifier for an `imports` reference (`./foo`, `@acme/ui`,
    /// `react`) — the module the name was imported from. Drives per-file import
    /// scope via the `ModuleResolver` (ADR-0018). `None` for non-import refs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specifier: Option<String>,
    /// The **pre-alias** name in the source module for an `imports` reference
    /// (`import { greet as g }` → `imported = "greet"`, name/key = `g`). Lets the
    /// export-graph walk look up the right name (ADR-0020). `None` = same as the
    /// bound name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported: Option<String>,
    /// The call site is a method call (`x.method()`) whose receiver type could
    /// **not** be inferred — an opaque receiver (method chain, builder return,
    /// trait object, external-crate type). Such a call must not bind by bare name
    /// across files: a wrong cross-file homonym is worse than an honest gap, so it
    /// stays unresolved unless a same-file def or stronger signal exists
    /// (ADR-0023). A bare free call and a type-hinted method call are not opaque.
    /// Part of the reference identity.
    #[serde(default, skip_serializing_if = "is_false")]
    pub recv_opaque: bool,
    /// Deferred receiver type: the method call's receiver is bound to the result
    /// of this **free/associated call** (`let x = compute(); x.m()` → `Some
    /// ("compute")`), so its type is that callee's declared return type — resolved
    /// **cross-file** by the linker (which alone sees every fn's `returns`). If the
    /// callee's return type can't be determined, the call declines like an opaque
    /// receiver (never a bare-name guess). Part of the reference identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recv_returns: Option<String>,
    /// The owner type of the deferred callee for an associated-call RHS
    /// (`let x = Foo::make()` → `Some("Foo")`), narrowing `recv_returns` to the
    /// right `make` before its return type is read. `None` for a free call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recv_returns_owner: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// symbol name → every site that references it; the **relink engine's** index —
/// lets a definition change find the exact affected edges without a full rescan
/// (HLD §11.0). Rebuilt incrementally: a changed file's own refs are dropped by
/// `source`, its new refs are added, and every reference is re-resolved.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReverseIndex {
    pub refs: BTreeMap<String, Vec<Reference>>,
}

/// file path → the file's export table (ADR-0020). A derived index maintained
/// incrementally like [`ReverseIndex`]: a changed file replaces its entry, and
/// the Engine rebuilds the per-module export graph from this each apply.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportIndex {
    pub by_file: BTreeMap<String, Vec<Export>>,
}

/// The candidate definitions a symbol name resolves against (ADR-0042 Phase 1.1).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolDefs {
    /// The defining node ids for this name (sorted, deduped — the resolver's
    /// candidate pool; `[0]` is the deterministic min).
    pub nodes: Vec<NodeId>,
    /// The modules that define this name — `module_of` of each candidate's file
    /// (sorted, deduped). Module = file today (identity grouping, ADR-0021).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<String>,
}

/// symbol name → its candidate definitions + defining modules — the **export /
/// symbol index** (ADR-0042 Phase 1.1). The `build_link_indices` candidate pool
/// grown from the ADR-0020 export tables and promoted to first-class,
/// persisted `GraphState`: the home module-scoped resolution and, in Phase 2,
/// module-sharded storage consult (`exports.json` sidecar). Maintained so that
/// after every apply it equals a from-scratch rebuild over the full node set.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolIndex {
    pub by_name: BTreeMap<String, SymbolDefs>,
}

/// The full persisted bundle — the stream operator's keyed state
/// (ADR-0005, ADR-0016).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphState {
    pub graph: Graph,
    pub partition: Partition,
    /// name → its single definition. **Never persisted** (ADR-0042 Phase 1c F3):
    /// computed from the node set by every apply and read by nothing from disk,
    /// so writing it bought only bytes. `skip` (not removal) keeps it a field —
    /// `state_facets` still digests it, so Global-vs-Scoped convergence still
    /// compares it. On load it is `Default`; the next apply restores it.
    #[serde(skip)]
    pub symbols: SymbolTable,
    pub reverse: ReverseIndex,
    pub manifest: Manifest,
    /// Repository structure (ADR-0019) — projects + their derived views. `default`
    /// so pre-0019 `state.json` loads (empty workspace = one root project).
    #[serde(default)]
    pub workspace: Workspace,
    /// Per-file export tables (ADR-0020), the raw material for the export graph.
    #[serde(default)]
    pub exports: ExportIndex,
    /// The export/symbol index (ADR-0042 Phase 1.1) — name → candidate defs +
    /// defining modules. **Never persisted** (ADR-0042 Phase 1c F3), for the same
    /// reason as [`GraphState::symbols`]: `build_symbol_index` recomputes it from
    /// the node set on every apply and nothing ever read the persisted copy, so
    /// the ~13 MB it added to each `state.json` write at next.js scale was pure
    /// cost. `skip` (not removal) keeps it a field, so the convergence comparator
    /// still digests it. On load it is `Default` — including from a pre-F3
    /// `state.json` that still contains it, which is ignored rather than adopted
    /// (nothing here sets `deny_unknown_fields`) — and the next apply restores it
    /// equal to a from-scratch rebuild (gate: `filigrio-resolve/tests/symbol_index.rs`).
    #[serde(skip)]
    pub symbol_index: SymbolIndex,
}
/// The patch one apply emits (**ADR-0042 Phase 1d P1**).
///
/// Three facets are real **patches** — they say what changed, not what the world
/// now is — because a `ShardedStore` (Phase 2) has to route a write to the
/// modules that actually changed, and cannot do that from a wholesale
/// replacement without serializing and diffing the entire graph (O(repo),
/// exactly what sharding exists to avoid):
///
/// * `edges_added` / `edges_removed` — the **minimal multiset diff** against
///   `prior.graph.edges`. Minimal, so it is a property of *the change* and not
///   of how the change was computed: it is byte-identical under
///   `LinkScope::Global` and `LinkScope::Scoped` (ADR-0042 B10), which is what
///   lets the store default and the scope default flip independently. Before
///   Phase 1d `edges_added` was the **whole** edge set (~351k edges at next.js
///   scale) and there was no removal list at all.
/// * `dirty_files` — the paths this apply touched (added ∪ modified ∪ removed,
///   *after* the F8 vanished-file fold), i.e. the shard-routing input.
/// * `reverse_dropped` / `reverse_added` — the `ReverseIndex` as drop-by-source
///   plus append, which is exactly the shape `rebuild_refs` already computes.
///   `reverse` is the largest facet after nodes/edges (~33 MB at next.js scale)
///   and, unlike `symbols`/`symbol_index`, is **not** reconstructible from the
///   node set (B2), so it must be persisted — and therefore patched.
///
/// Everything else replaces wholesale, deliberately and per **B2**: `partition`
/// is a global sidecar by decision (clustering stays hot, so it is rewritten
/// anyway — B3 accepts its ~11.6 MB as the dominant residual); `manifest`,
/// `exports` and `workspace` are global sidecars; `symbols` and `symbol_index`
/// are never persisted at all (Phase 1c F3) and ride here only as in-process
/// values.
///
/// **Measured, so nobody re-infers it** (ADR-0042 Phase 1b.1, 2026-07-26): the
/// note this replaced was read as "the derived indices are the fixed per-apply
/// cost" and two ADR phases were planned on that. They are not. On next.js
/// (102k nodes / 352k edges) *rebuilding* every derived index costs **137 ms of
/// a 3.8 s apply — 3.6 %**; clustering is another 5.4 %. The fixed cost lives
/// elsewhere (per-import module resolution, prior-edge cloning, the
/// reverse↔edges guard — see `docs/perf/benchmarks.md` §5d for the ranked
/// table). What the wholesale carry *did* cost is **write** amplification and
/// per-apply copying, which is what the patches above remove.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphDelta {
    pub nodes_added: Vec<Node>,
    pub nodes_removed: Vec<NodeId>,
    /// Edges this apply **adds** relative to `prior.graph.edges` — not the whole
    /// edge set (ADR-0042 Phase 1d P1). Order is the order the engine emitted
    /// them (structural, then linked, then the project overlay), so the merged
    /// edge vector keeps the exact order the wholesale carry produced.
    pub edges_added: Vec<Edge>,
    /// Edges this apply **removes** relative to `prior.graph.edges`, in prior
    /// order. A multiset: an edge present twice in `prior` and once in the
    /// result appears here once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges_removed: Vec<Edge>,
    /// The files this apply touched — added ∪ modified ∪ removed, **after** the
    /// F8 vanished-file fold, so a path that disappeared between detection and
    /// apply is here (as a removal) rather than silently absent. This is the
    /// Phase-2 shard-routing input; it is derived from the changeset, so it is
    /// scope-independent by construction.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub dirty_files: BTreeSet<String>,
    pub partition: Partition,
    pub symbols: SymbolTable,
    /// `reverse` patch, drop half: every node id whose reference sites this
    /// apply retires (the nodes of the touched files). A store drops each
    /// `Reference` whose `source` is in here, then removes any name left empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reverse_dropped: Vec<NodeId>,
    /// `reverse` patch, append half: `(symbol name, reference)` pairs this apply
    /// contributes — the freshly-extracted references plus anything
    /// `heal_diverged_refs` reconstructed. A store appends them and then sorts +
    /// dedups each **touched** name, which is exactly what `rebuild_refs` does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reverse_added: Vec<(String, Reference)>,
    pub manifest: Manifest,
    /// Repository structure this apply produced (ADR-0019) — replaces wholesale,
    /// like the other derived indices.
    #[serde(default)]
    pub workspace: Workspace,
    /// Per-file export tables this apply produced (ADR-0020).
    #[serde(default)]
    pub exports: ExportIndex,
    /// The export/symbol index this apply produced (ADR-0042 Phase 1.1) —
    /// replaces wholesale, like the other derived indices.
    #[serde(default)]
    pub symbol_index: SymbolIndex,
    /// Size of the affected set — telemetry for O(change) claims (HLD §11.1).
    /// A name-based **over-counting heuristic**, not an exact changed-edge
    /// count: added + removed nodes, plus every surviving referrer of every
    /// symbol label (un)defined in a touched file — even referrers whose
    /// resolution did not actually change. Never used for correctness.
    #[serde(default)]
    pub affected: usize,
    /// How many `added`/`modified` paths had **vanished** by the time the engine
    /// read them and were folded into the removals (ADR-0042 F8). A changeset is
    /// built by walking the tree and applied later; anything deleted inside that
    /// window converges (a cold build of the tree as it now stands has no such
    /// file) instead of aborting the batch. Reported rather than silent —
    /// "silently converged" must not become the new silent failure (ADR-0029).
    /// A path that *exists* but cannot be read is **not** counted here: that is
    /// still a hard error.
    #[serde(default)]
    pub vanished: usize,
}

// ---- query-side value types --------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStats {
    pub nodes: usize,
    pub edges: usize,
    pub communities: usize,
    pub by_confidence: BTreeMap<String, usize>,
}

/// One project in the monorepo map (ADR-0019): its identity, size, and the
/// workspace-internal projects it depends on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectNode {
    /// Project root dir — the stable id.
    pub root: String,
    /// Declared package/module name, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The manifest basename that declared it.
    pub manifest: String,
    /// Number of source files under this project.
    pub files: usize,
    /// Roots of the projects this one depends on (sorted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

/// The project dependency graph — the two-level graph's upper level (ADR-0019),
/// a queryable monorepo architecture map. `projects` are ordered by `root`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectGraph {
    pub projects: Vec<ProjectNode>,
}

/// Traversal order for a graph query — the one definition in the workspace:
/// the wire contract re-exports *this* type (`filigrio_protocol::TraversalMode`)
/// rather than declaring a parallel enum a transport has to hand-convert.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraversalMode {
    #[default]
    Bfs,
    Dfs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryOpts {
    pub depth: usize,
    pub budget: usize,
    pub mode: TraversalMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_filter: Option<Vec<String>>,
}

impl Default for QueryOpts {
    fn default() -> Self {
        QueryOpts {
            depth: 2,
            budget: 32,
            mode: TraversalMode::Bfs,
            context_filter: None,
        }
    }
}

/// A materialized traversal result (HLD §6). `communities` cites each returned
/// node's community label, so a consumer (an LLM) knows where in the map every
/// result lives without a second call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subgraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub communities: BTreeMap<NodeId, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-ADR-0042 implementation of [`Workspace::project_of`]: scan every
    /// project, keep the longest matching root. Kept **only** here, as the
    /// reference the fast ancestor-probe form is checked against.
    fn project_of_by_scan<'a>(ws: &'a Workspace, path: &str) -> Option<&'a Project> {
        ws.projects
            .values()
            .filter(|p| path_under(path, &p.root))
            .max_by_key(|p| p.root.len())
    }

    fn ws(roots: &[&str]) -> Workspace {
        Workspace {
            projects: roots
                .iter()
                .map(|r| {
                    (
                        r.to_string(),
                        Project {
                            root: r.to_string(),
                            manifest: "package.json".into(),
                            name: Some(format!("pkg-{r}")),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        }
    }

    /// The optimization's whole contract: for every workspace/path pairing the
    /// probe must return exactly what the scan returned. The cases below are the
    /// ones where a prefix-based shortcut can plausibly go wrong — a root that is
    /// a *string* prefix but not a *path* prefix (`app` vs `apps/…`), nested
    /// roots (longest must win), a repo-root manifest (`""` contains everything),
    /// the path being a root itself, and absolute/oddly-shaped paths.
    #[test]
    fn project_of_is_equivalent_to_the_scan() {
        let workspaces = [
            ws(&[]),
            ws(&[""]),
            ws(&["packages/ui"]),
            ws(&["", "packages/ui"]),
            ws(&["app", "apps", "apps/web", "apps/web/src"]),
            ws(&["a", "a/b", "a/b/c", "a/bb", "ab"]),
            ws(&["packages/ui", "packages/ui-kit"]),
            ws(&["src/x.ts"]), // a "root" that is itself a file path
        ];
        let paths = [
            "",
            "/",
            "x",
            "x.ts",
            "app/main.ts",
            "apps/web/src/index.ts",
            "apps/web/README.md",
            "appsx/web/index.ts",
            "a/b/c/d/e.ts",
            "a/bb/x.ts",
            "ab/x.ts",
            "packages/ui-kit/src/b.ts",
            "packages/ui/src/a.ts",
            "src/x.ts",
            "/abs/path/file.ts",
            "no/such/place.ts",
        ];
        for (i, w) in workspaces.iter().enumerate() {
            for p in paths {
                let want = project_of_by_scan(w, p).map(|x| x.root.as_str());
                let got = w.project_of(p).map(|x| x.root.as_str());
                assert_eq!(
                    want,
                    got,
                    "workspace #{i} ({:?}), path {p:?}",
                    w.projects.keys().collect::<Vec<_>>()
                );
                assert_eq!(want, w.root_of(p), "root_of disagrees: #{i}, {p:?}");
            }
        }
    }

    /// Nesting depth, not project count, is what the probe pays — the property
    /// that makes it O(1)-ish on a 694-project monorepo.
    #[test]
    fn deeply_nested_roots_pick_the_longest() {
        let w = ws(&["", "a", "a/b", "a/b/c", "a/b/c/d"]);
        assert_eq!(w.root_of("a/b/c/d/e/f.ts"), Some("a/b/c/d"));
        assert_eq!(w.root_of("a/b/x.ts"), Some("a/b"));
        assert_eq!(w.root_of("z.ts"), Some(""));
    }
}
