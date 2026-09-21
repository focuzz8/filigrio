//! filigrio-resolve — the barrier: `apply(prior, changeset)` (HLD §11, ADR-0004/0012).
//!
//! This is the **stateful stream operator** (ADR-0016): it reads prior
//! `GraphState`, indexes changed files, **links/relinks symbols across files**,
//! clusters, and emits a `GraphDelta`. With `prior = ∅` and a whole-tree
//! ChangeSet it degenerates to the cold build (Kappa).
//!
//! ## Cross-file resolution (Phase 2a — real)
//!
//! Extraction emits `calls`/`imports` as unresolved [`EdgeTarget::Symbol`] and
//! structural edges (`contains`) as resolved [`EdgeTarget::Node`]. Resolution
//! binds each symbol against a `SymbolTable` built over the whole node set and
//! tags honest provenance (ADR-0008):
//!
//! **Name deduction** (no receiver type — a bare `foo()`):
//!
//! | candidates for the name | result | confidence |
//! |---|---|---|
//! | 1, **same file** as the reference | `Node` | `EXTRACTED` (in-file direct ref) |
//! | 1, **different file** | `Node` | `INFERRED` (deduced across files) |
//! | ≥2 | `Node`, min-id (deterministic) | `AMBIGUOUS` (surfaced) |
//! | 0 | stays `Symbol` | `EXTRACTED` (extracted but unlinked; surfaced) |
//!
//! (This is the port's documented rule set, grounded in migration-plan §Phase
//! 1/2a and graphify-description §Confidence; the Python graphify oracle is the
//! differential check — ADR-0017.)
//!
//! **Receiver-type resolution.** A method call whose receiver type is known
//! carries a `type_hint` (`T::method` → `Some("T")`, incl. `Self`/`this` → the
//! enclosing type, and `x.method()` where `x`'s type was inferred). Candidates
//! are narrowed to methods whose `impl` owner matches — so `T::new` binds to T's
//! `new`, not the min-id homonym. The type is **explicit in the source**, so the
//! target is *certain*: a single match is `EXTRACTED` **even cross-file** (file
//! location doesn't downgrade a type-directed link — this is what INFERRED-vs-
//! EXTRACTED tracks: deduction vs certainty); ≥2 matches (same method on a
//! same-named type in several files) are `AMBIGUOUS`; and **0 matches** means the
//! call targets a type we don't define (e.g. `Vec::new`) — surfaced
//! **unresolved**, never bound to an unrelated same-named node. This splits the
//! merged-constructor god-node and cuts spurious `AMBIGUOUS` edges. Covers
//! associated calls `T::method()` and value/method calls `x.method()` /
//! `self.method()` (the extractor infers the receiver's type from local
//! dataflow).
//!
//! **Opaque-receiver method calls** (ADR-0023). When a method call's receiver
//! type could *not* be inferred (a method chain `a.b().c()`, a builder return, a
//! trait object, an external-crate type), the extractor marks the reference
//! `recv_opaque` instead of guessing a type. Such a call **does not bind by bare
//! name across files**: `v.iter()` does not call *your* `fn iter`, and a wrong
//! homonym bind manufactures fake god nodes (`iter`, `len`, `with_*`) and
//! pollutes clustering. It keeps the strong-locality tiers — an import, an
//! inferred type (absent by definition), or a **same-file** def still resolve —
//! but the cross-file bare-name tiers are declined to an honest unresolved
//! `Symbol`. A bare free call (`foo()`) is unaffected: a bare name genuinely may
//! denote a function in another file.
//!
//! **Return-type receiver inference** (ADR-0026, the recall counterpart). A
//! receiver bound to a free/associated call — `let x = compute(); x.m()` — carries
//! a deferred `recv_returns` (the callee) instead of a concrete type. The link
//! stage is the first point that sees every fn's declared return (`returns` attr),
//! so [`return_type_of`] resolves the callee cross-file, reads its return type, and
//! feeds it back as the receiver's `type_hint` — then the narrowing above binds
//! `m` type-directed. Unknown or ambiguous return → decline like an opaque
//! receiver. Single-hop: it never recurses into the callee's own receiver.
//!
//! ## Incremental relink (the point of Phase 2a)
//!
//! The [`ReverseIndex`] (`name → every (source, relation) that references it`)
//! is the relink engine. On a change we **re-extract only the changed files**;
//! then we rebuild the reference universe incrementally — drop references whose
//! `source` lives in a changed file, add the changed files' fresh references —
//! and **re-resolve every reference** against the new `SymbolTable`. Because a
//! reference carries its own `(source, relation)`, editing a *definition*
//! re-binds exactly its dependents (found via the index) without rescanning the
//! files that reference it. Re-resolution is O(references) cheap map lookups;
//! the expensive extraction stays O(change).
//!
//! ## Project-scoped resolution (Phase 4b — ADR-0018/0019)
//!
//! The candidate pool is not the whole repo: it is the **project** the reference
//! lives in. A project = the nearest ancestor **package/module manifest**
//! (`Cargo.toml`, `package.json`, `deno.json`, `go.mod`, `pyproject.toml`) — the
//! dependency boundary. The **Engine** (repo-aware, not the pure extractor —
//! ADR-0003) maintains a [`Workspace`](filigrio_core::Workspace) of projects as
//! first-class incremental state (ADR-0019): only *manifest* changes in a
//! changeset touch it, and a file's project is **derived** by a longest-prefix
//! lookup (`Workspace::root_of`) — never stamped onto nodes, so a rename touches
//! one place and a later-added manifest re-scopes its files for free. Resolution
//! narrows candidates to the source node's own project *before* the name/type
//! rules below run.
//!
//! Effect: duplicate names across packages (two `greet`s, forty `./utils`) no
//! longer collapse to one min-id homonym. A cross-project reference with no
//! same-project candidate stays an **honest unresolved `Symbol`** unless a
//! per-file **import scope** (built via the `ModuleResolver`, backed by the
//! `Workspace`) *licenses* the cross-project bind — an explicit import binds
//! `EXTRACTED` across the boundary. A single-package repo has one project, so
//! scoping is a no-op there (the oracle-diff fixtures are unchanged).
//!
//! Clustering (step 4) is **real** (Phase 2b): modularity-based, warm-started
//! from the prior partition with community-id stability — see [`cluster`].

#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]
mod cluster;
mod export_graph;
pub mod manifest;
mod modres;
mod module;
#[cfg(feature = "oxc")]
mod oxc;
pub use cluster::{cluster, cluster_with, ClusterConfig, ClusterStrategy, EdgeWeighting};
use filigrio_core::attrs;
use filigrio_core::profile::stage;
use modres::dir_of;
pub use modres::SourceModuleResolver;
pub use module::{module_of, ModuleId};
#[cfg(feature = "oxc")]
pub use oxc::OxcResolver;

use filigrio_core::relation::{CONTAINS, DEPENDS_ON, IMPORTS};
use filigrio_core::{
    ChangeSet, Confidence, Edge, EdgeTarget, Export, ExportIndex, Extractor, GraphDelta,
    GraphState, ModuleResolver, Node, NodeId, Partition, Reference, Result, ReverseIndex, Source,
    SymbolDefs, SymbolIndex, SymbolTable, TargetRef,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Node kinds that define a linkable symbol (files are not linkable).
///
/// **`interface` / `type_alias` (ADR-0037b, 2026-07-29).** These are the TS/TSX
/// `type:` node kinds the 0037b structural pass emits (`interface_declaration`,
/// `type_alias_declaration`). They were absent from this list, so an `interface`
/// or a `type X = …` could be *defined and exported* and still never appear in
/// `local_def` or the link candidates — every `imports`, `extends`, `implements`
/// and `type/field` reference to one was structurally unresolvable, whatever the
/// extractor emitted. That, not the `import type` syntax the ADR's reproduction
/// blamed, is why TS type edges bound at 11 % while `calls` bound at 53 %: the
/// reproduction confounded two variables (the *interface* `Params` arrived via
/// `import type`, the *class* `Widget` via a plain import). De-confounded, a
/// `import type { Class }` binds and a plain `import { Interface }` does not.
const LINKABLE: &[&str] = &[
    "function",
    "struct",
    "enum",
    "trait",
    "class",
    "interface",
    "type_alias",
];

/// Whether `n` defines a symbol other references can bind to.
///
/// Public so the `symbol_index` oracle shares **one** definition of the kind
/// filter. That oracle is a differential test — maintained index vs from-scratch
/// rebuild — and what it must independently re-derive is the *index construction*,
/// not the kind list. Cloning the list bought no independence and did drift: it
/// still read the pre-`interface`/`type_alias` set and nothing caught it, because
/// no fixture happens to define those kinds.
pub fn is_linkable(n: &Node) -> bool {
    LINKABLE.contains(&n.kind.as_str())
}

/// Which linking strategy `Engine::apply` uses for the re-resolution stage
/// (ADR-0042 Phase 1.3). **`Global` is the engine's and the pipeline's default**
/// and the byte-for-byte prior behavior: every reference is re-resolved each
/// apply. `Scoped` re-resolves only the *impact set* of a change and carries every
/// other prior edge verbatim; it is *proven equal* to `Global` by the equivalence
/// gates (`just shadow`, `just convergence`), and the **daemon** selects it
/// (ADR-0042 amendment 2026-09-10; `FILIGRIO_LINK_SCOPE=global` in the daemon's
/// environment rolls back). The switch is itself the rollback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkScope {
    /// Re-resolve every reference (the prior, always-correct behavior). Default.
    #[default]
    Global,
    /// Re-resolve only the change's impact set; carry unaffected edges verbatim.
    Scoped,
}

/// **When** an apply runs community detection — the *what* is [`ClusterConfig`].
///
/// `Inline` (the default everywhere) clusters inside the apply, so the delta's
/// partition is current. `Deferred` takes the Louvain pass — ~0.3 s at next.js
/// scale, O(graph) whatever the change — off the apply: the delta carries the
/// prior partition minus the nodes that are gone, new nodes stay unassigned, and
/// the caller owes an [`Engine::recluster`] once the burst of changes is over.
/// The daemon's watcher lane uses it so an edit is queryable ~0.3 s sooner;
/// communities catch up a moment later. Sound because clustering is warm-started
/// and history-dependent by design (ADR-0024) — the convergence gates already
/// exclude `partition` from incremental-vs-cold equality for exactly that reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClusterTiming {
    #[default]
    Inline,
    Deferred,
}

/// The partition a [`ClusterTiming::Deferred`] apply hands back: the prior
/// assignment minus the nodes that are **gone** — dropped and not re-extracted.
/// An edited file drops and re-adds its unchanged symbols under the same ids, and
/// those keep their community. Nodes new to this apply stay unassigned, and
/// community metadata (size, cohesion) is carried as-is, until the owed
/// [`Engine::recluster`]. O(change) beyond the one clone of the map.
fn carry_partition(
    prior: &Partition,
    dropped: &BTreeSet<&NodeId>,
    new_nodes: &[Node],
) -> Partition {
    let readded: HashSet<&NodeId> = new_nodes.iter().map(|n| &n.id).collect();
    let mut partition = prior.clone();
    for id in dropped {
        if !readded.contains(id) {
            partition.node_community.remove(*id);
        }
    }
    partition
}

/// The engine. Stateless itself — all state is the `prior` argument and the
/// emitted delta (the store is the state backend, ADR-0016).
pub struct Engine;

impl Engine {
    /// `apply(prior, Δ)`. With `prior = ∅` and a whole-tree ChangeSet this is
    /// the full cold build; with a small ChangeSet it is a warm, incrementally
    /// relinked update.
    ///
    /// Uses [`LinkScope::Global`] — every reference re-resolved. This is the
    /// default entry point and its behavior is fixed; [`Engine::apply_with_scope`]
    /// is the opt-in door to the scoped linker (ADR-0042 Phase 1).
    pub fn apply(
        prior: &GraphState,
        changes: &ChangeSet,
        source: &dyn Source,
        extractor: &dyn Extractor,
        cluster_cfg: &ClusterConfig,
    ) -> Result<GraphDelta> {
        Self::apply_with_scope(
            prior,
            changes,
            source,
            extractor,
            cluster_cfg,
            LinkScope::Global,
        )
    }

    /// `apply` under an explicit [`LinkScope`] (ADR-0042 Phase 1.3). `Global` is
    /// identical to [`Engine::apply`]; `Scoped` re-resolves only the change's
    /// impact set and carries every other prior edge verbatim — proven equal to
    /// `Global` by the equivalence suite.
    ///
    /// The body is bracketed by [`filigrio_core::profile::stage`] markers
    /// (ADR-0042 Phase 1b.1): wrap a call in
    /// [`filigrio_core::profile::capture`] to get the per-stage wall-clock split,
    /// which is how `docs/perf/benchmarks.md` §5d attributes the fixed per-apply
    /// cost. Recording is off by default and costs one thread-local check per
    /// stage, so this is the same apply either way.
    pub fn apply_with_scope(
        prior: &GraphState,
        changes: &ChangeSet,
        source: &dyn Source,
        extractor: &dyn Extractor,
        cluster_cfg: &ClusterConfig,
        scope: LinkScope,
    ) -> Result<GraphDelta> {
        Self::apply_with(
            prior,
            changes,
            source,
            extractor,
            cluster_cfg,
            scope,
            ClusterTiming::Inline,
        )
    }

    /// Run the clustering a [`ClusterTiming::Deferred`] apply skipped, over
    /// `state` exactly as the apply would have: the symbol graph only — the
    /// `project` overlay is left out, as the apply adds it after clustering, and
    /// `cluster_with` skips any edge with an endpoint outside the node set, so the
    /// overlay's edges drop out with its nodes — warm-started from
    /// `state.partition`.
    pub fn recluster(state: &GraphState, cluster_cfg: &ClusterConfig) -> Result<Partition> {
        let nodes: Vec<&Node> = state
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind != "project")
            .collect();
        cluster_with(&nodes, &state.graph.edges, &state.partition, cluster_cfg)
    }

    /// [`Engine::apply_with_scope`] with an explicit [`ClusterTiming`]. `Inline`
    /// is that function exactly; `Deferred` skips the Louvain pass and carries
    /// the prior partition (see [`ClusterTiming::Deferred`]) — the caller then
    /// owes a [`Engine::recluster`].
    pub fn apply_with(
        prior: &GraphState,
        changes: &ChangeSet,
        source: &dyn Source,
        extractor: &dyn Extractor,
        cluster_cfg: &ClusterConfig,
        scope: LinkScope,
        timing: ClusterTiming,
    ) -> Result<GraphDelta> {
        // ---- the front half (stages `fold_vanished` … `heal_diverged_refs`),
        //      shared verbatim with [`Engine::impact_report`] so the impact set
        //      the gate certifies is computed from this apply's own inputs. Both
        //      halves are destructured exhaustively on purpose: a new front-half
        //      output has to be acknowledged here and there (see `front_extract`).
        let FrontExtract {
            changes,
            vanished,
            dropped_nodes,
            surviving_nodes,
            prior_project_nodes,
            workspace,
            extracted:
                Extracted {
                    nodes: new_nodes,
                    structural: new_structural,
                    refs: new_refs,
                    exports: new_exports,
                },
        } = front_extract(prior, changes, source, extractor)?;
        let changes: &ChangeSet = &changes;
        let FrontLink {
            dropped_files,
            dropped_ids,
            prior_project_node_ids,
            all_nodes,
            idx,
            refs_by_name,
            exports_by_file,
            import_scope,
            healed_refs,
        } = front_link(
            prior,
            source,
            changes,
            &dropped_nodes,
            &surviving_nodes,
            &prior_project_nodes,
            &workspace,
            &new_nodes,
            &new_refs,
            &new_exports,
        );

        // ---- stage 4 · link: bind every reference through the import scope +
        //      project scope + name/type rules → a resolved edge. This is what
        //      relinks dependents when a definition changes (Phase 2a).
        let ctx = LinkCtx {
            file_of: &idx.file_of,
            impl_of: &idx.impl_of,
            returns_of: &idx.returns_of,
            project_of: &idx.project_of,
            import_scope: &import_scope,
            workspace: &workspace,
        };
        // Re-resolution (ADR-0042 Phase 1.3). `Global` re-resolves every reference
        // (the fixed default) and so has no impact set. `Scoped` re-resolves only
        // the change's impact set and carries every unaffected prior reference edge
        // verbatim — the two are proven-equal by the equivalence suite.
        let (resolved_edges, impact): (Vec<Edge>, Option<ImpactReport>) = match scope {
            LinkScope::Global => (
                stage("link", || resolve_all(&refs_by_name, &idx.candidates, &ctx)),
                None,
            ),
            LinkScope::Scoped => {
                let impact = stage("compute_impact", || {
                    compute_impact(
                        &dropped_nodes,
                        &new_nodes,
                        &refs_by_name,
                        &dropped_files,
                        &prior.exports,
                        &exports_by_file,
                        &prior.workspace,
                        &workspace,
                        &idx.file_of,
                    )
                });
                let resolved = stage("link", || {
                    resolve_impacted(&refs_by_name, &idx.candidates, &ctx, &impact)
                });
                (resolved, Some(impact))
            }
        };

        // 6. Carry the prior edges this apply is not re-emitting — structural ones
        //    and (Scoped only) the reference-site ones outside the impact set — then
        //    append the freshly-extracted structural edges and the linked ones.
        //    `retired` is the complement: the prior edges *not* carried, which is
        //    the left-hand side of the Phase 1d edge diff below.
        let (mut all_edges, retired): (Vec<Edge>, Vec<&Edge>) = stage("carry_edges", || {
            carry_prior_edges(
                prior,
                &refs_by_name,
                impact.as_ref(),
                &dropped_ids,
                &prior_project_node_ids,
            )
        });
        // Everything after this index is *emitted* by this apply rather than
        // carried — the right-hand side of the diff.
        let carried = all_edges.len();
        all_edges.extend(new_structural);
        all_edges.extend(resolved_edges);

        // 7. Cluster: modularity-based, warm-started from the prior partition,
        //    with community-id stability (Phase 2b, HLD §11.1–11.2). Clusters the
        //    *symbol* graph only — the project overlay (below) is added after, so
        //    it neither perturbs the community partition nor gets a community.
        let partition = match timing {
            ClusterTiming::Inline => stage("cluster", || {
                cluster_with(&all_nodes, &all_edges, &prior.partition, cluster_cfg)
            })?,
            ClusterTiming::Deferred => stage("cluster.carry", || {
                carry_partition(&prior.partition, &dropped_ids, &new_nodes)
            }),
        };

        // 7b. Project the `Workspace` into the visible graph (ADR-0019): one
        //     `project` node per project, `depends_on` edges (the monorepo
        //     architecture map), and `contains` edges project → its files (linking
        //     the two levels). A derived overlay — regenerated each apply, kept out
        //     of clustering/resolution, and carries no community.
        let (project_nodes, project_edges) = stage("project_overlay", || {
            project_overlay(&workspace, &all_nodes)
        });
        all_edges.extend(project_edges);

        // 8. Derived indices + delta.
        let symbols = stage("symbols", || SymbolTable {
            defs: idx
                .candidates
                .iter()
                .filter_map(|(name, ids)| ids.first().map(|id| (name.clone(), id.clone())))
                .collect(),
        });
        // The export/symbol index (ADR-0042 Phase 1.1): name → candidate defs +
        // defining modules, grown from the candidate pool `build_link_indices`
        // already computed over the full node set. Regenerated each apply, so it
        // equals a from-scratch rebuild by construction (gate: `symbol_index.rs`).
        let symbol_index = stage("symbol_index", || build_symbol_index(&idx));
        let reverse = ReverseIndex { refs: refs_by_name };

        // Removed = file-dropped nodes ∪ the prior project overlay (regenerated).
        let nodes_removed: Vec<NodeId> = dropped_ids
            .iter()
            .map(|id| (*id).clone())
            .chain(prior_project_nodes.iter().map(|n| n.id.clone()))
            .collect();
        let manifest = stage("manifest", || maintain_manifest(&prior.manifest, changes));
        let affected = stage("affected", || {
            compute_affected(
                &dropped_nodes,
                &new_nodes,
                &surviving_nodes,
                &reverse,
                nodes_removed.len(),
            )
        });

        // Nodes added = freshly-extracted symbols + the regenerated project overlay.
        let mut nodes_added = new_nodes;
        nodes_added.extend(project_nodes);

        // The **patch delta** (ADR-0042 Phase 1d P1): what changed, not what the
        // world now is. See [`diff_edges`] for why this is the *minimal* diff and
        // why it costs almost nothing under `Scoped`.
        let (edges_added, edges_removed) = stage("edge_diff", || {
            // `split_off` hands the emitted tail over **by value** — the additions
            // are moved out of it, never copied (the ADR's "the ~216 ms edge clone
            // becomes a move"). `all_edges` keeps the carried prefix, which is
            // already accounted for on both sides of the diff and is dropped here.
            let emitted = all_edges.split_off(carried);
            diff_edges(&retired, emitted)
        });
        // `reverse` as a drop+append patch: retire the touched files' reference
        // sites, contribute this apply's references (extracted + healed). The
        // store re-canonicalizes the touched names, which is what `rebuild_refs`
        // did to the whole index.
        let reverse_dropped: Vec<NodeId> = dropped_ids.iter().map(|id| (*id).clone()).collect();
        let mut reverse_added = new_refs;
        reverse_added.extend(healed_refs);

        Ok(GraphDelta {
            nodes_added,
            nodes_removed,
            edges_added,
            edges_removed,
            // The shard-routing input, straight off the F8-folded changeset: a
            // path that vanished between detection and apply is in here as a
            // removal rather than silently missing.
            dirty_files: changes.touched().cloned().collect(),
            partition,
            symbols,
            reverse_dropped,
            reverse_added,
            manifest,
            workspace,
            exports: ExportIndex {
                by_file: exports_by_file,
            },
            symbol_index,
            affected,
            vanished,
        })
    }

    /// Compute the change-impact set (ADR-0042 Phase 1.2) for `changes` against
    /// `prior` — the authority the scoped linker consults — without running the
    /// resolution or clustering stages.
    ///
    /// Runs **the same front half**, not a copy of it: [`front_extract`] and
    /// [`front_link`] are the identical calls [`Engine::apply_with_scope`] makes,
    /// so `compute_impact` here sees byte-for-byte the arguments it sees there.
    /// That is the property the impact-coverage gate (`impact.rs`) depends on — a
    /// parallel sequence could drift out of step with the apply and the gate would
    /// certify an impact set nothing computes. Only the *tail* (link, carry,
    /// cluster, delta) is skipped. Exposed for the gate.
    pub fn impact_report(
        prior: &GraphState,
        changes: &ChangeSet,
        source: &dyn Source,
        extractor: &dyn Extractor,
    ) -> Result<ImpactReport> {
        // Exhaustive destructuring, `_`-prefixed where this consumer has no use
        // for an output: a new front-half output is a compile error here too, so
        // whoever adds it must decide whether the impact set depends on it.
        let FrontExtract {
            changes,
            vanished: _vanished,
            dropped_nodes,
            surviving_nodes,
            prior_project_nodes,
            workspace,
            extracted:
                Extracted {
                    nodes: new_nodes,
                    structural: _new_structural,
                    refs: new_refs,
                    exports: new_exports,
                },
        } = front_extract(prior, changes, source, extractor)?;
        let changes: &ChangeSet = &changes;
        let FrontLink {
            dropped_files,
            dropped_ids: _dropped_ids,
            prior_project_node_ids: _prior_project_node_ids,
            all_nodes: _all_nodes,
            idx,
            refs_by_name,
            exports_by_file,
            import_scope: _import_scope,
            healed_refs: _healed_refs,
        } = front_link(
            prior,
            source,
            changes,
            &dropped_nodes,
            &surviving_nodes,
            &prior_project_nodes,
            &workspace,
            &new_nodes,
            &new_refs,
            &new_exports,
        );
        Ok(compute_impact(
            &dropped_nodes,
            &new_nodes,
            &refs_by_name,
            &dropped_files,
            &prior.exports,
            &exports_by_file,
            &prior.workspace,
            &workspace,
            &idx.file_of,
        ))
    }
}

// ---------------------------------------------------------------------------
// The front half — the one sequence both `apply_with_scope` and `impact_report`
// run.
//
// It is **one sequence in two functions**, and the split is forced by borrowing,
// not by design: [`front_link`]'s outputs (`all_nodes`, the [`LinkIndices`]) are
// *views into* [`front_extract`]'s outputs, and Rust cannot return a struct that
// borrows its own field. So the caller holds the owned half in a local and hands
// it to the borrowed half. Both halves return a struct that **both consumers
// destructure exhaustively, with no `..`**: adding an output to either one is a
// compile error at both call sites (an unused binding is a `-D warnings` error
// under the workspace gate), which is the ADR-0042 `state_facets` trick applied
// to control flow. Adding a *stage* inside these two functions needs no call-site
// change at all — that is the point: `impact_report` cannot fall behind the apply
// by omission, only by someone deliberately writing a stage outside them.
//
// The boundary is `fold_vanished … heal_diverged_refs` — everything up to and
// including the last stage that can still change `refs_by_name`, which is the
// input [`compute_impact`] reads. Ending it earlier (e.g. at `rebuild_refs`) is
// what let the old `impact_report` compute its verdict from *unhealed*
// references while the scoped apply computed its own from healed ones.
// ---------------------------------------------------------------------------

/// The owned half of the front sequence: stages `fold_vanished` → `split_nodes` →
/// `workspace` → `extract`. Everything here is owned outright, because the rest of
/// the apply borrows from it (and, at the very end, moves out of it).
struct FrontExtract<'a> {
    /// The F8-folded changeset — borrowed unless something vanished.
    changes: std::borrow::Cow<'a, ChangeSet>,
    /// How many changeset paths were gone by the time we looked (delta telemetry).
    vanished: usize,
    /// Prior nodes whose file this changeset touches; they drop.
    dropped_nodes: Vec<Node>,
    /// Prior symbol nodes that survive untouched (the project overlay excluded).
    surviving_nodes: Vec<Node>,
    /// The prior `project` overlay — always regenerated, never survives (ADR-0019).
    prior_project_nodes: Vec<Node>,
    /// The incrementally maintained `Workspace` (ADR-0019).
    workspace: filigrio_core::Workspace,
    /// This apply's raw per-file extraction.
    extracted: Extracted,
}

/// Run the owned half of the front sequence (see the module comment above).
fn front_extract<'a>(
    prior: &GraphState,
    changes: &'a ChangeSet,
    source: &dyn Source,
    extractor: &dyn Extractor,
) -> Result<FrontExtract<'a>> {
    // 0. **Reconcile the changeset with reality first** (ADR-0042 F8). A
    //    changeset is built by walking the tree and applied some time later;
    //    anything deleted inside that window is folded from `added`/`modified`
    //    into `removed` right here, *before* any stage runs. One seam, so
    //    every downstream stage (extract, workspace, manifest, the reverse
    //    index, the carry filter) sees one coherent changeset and none of them
    //    needs its own existence check.
    let (changes, vanished) = stage("fold_vanished", || fold_vanished(changes, source));

    // 1. Partition prior nodes by whether their file was touched. Every
    //    touched path (added ∪ modified ∪ removed) drops its prior nodes, so
    //    re-indexing an already-present file is idempotent (HLD §11.4).
    let (dropped_nodes, surviving_nodes, prior_project_nodes) = stage("split_nodes", || {
        let dropped_files: BTreeSet<&String> = changes.touched().collect();
        let in_dropped = |n: &Node| matches!(&n.source_file, Some(f) if dropped_files.contains(f));
        let (dropped_nodes, surviving_nodes): (Vec<Node>, Vec<Node>) =
            prior.graph.nodes.iter().cloned().partition(in_dropped);
        // `project` nodes are a *derived overlay* of the `Workspace` (ADR-0019) —
        // never survive; regenerated below. Split them out so they don't enter
        // clustering/resolution and stale ones are removed.
        let (prior_project_nodes, surviving_nodes): (Vec<Node>, Vec<Node>) = surviving_nodes
            .into_iter()
            .partition(|n| n.kind == "project");
        (dropped_nodes, surviving_nodes, prior_project_nodes)
    });

    // ---- stage 1 · physical (language-agnostic): maintain the `Workspace`
    //      incrementally (ADR-0019). Only *manifest* changes touch it; a
    //      file's project stays *derived* by prefix lookup, never stamped.
    //      `prior = ∅` ⇒ full discovery.
    let workspace = stage("workspace", || {
        maintain_workspace(&prior.workspace, &changes, source)
    })?;

    // ---- extract: raw per-file facts from the changed files (added ∪
    //      modified). Structural edges (`Node` targets, e.g. `contains`) are
    //      kept as-is; reference edges (`Symbol` targets, e.g. `calls`/
    //      `imports`) feed the relink engine; export tables (ADR-0020) feed
    //      the contract stage. Nodes carry no `project` attr — membership is
    //      derived from the `Workspace`, not stamped (ADR-0019). O(change).
    let extracted = stage("extract", || extract_changed(&changes, source, extractor))?;

    Ok(FrontExtract {
        changes,
        vanished,
        dropped_nodes,
        surviving_nodes,
        prior_project_nodes,
        workspace,
        extracted,
    })
}

/// The borrowed half of the front sequence: stages `all_nodes_view` →
/// `link_indices` → `rebuild_refs` → `maintain_exports` → `build_resolver` →
/// `import_scope` → `heal_diverged_refs`. Every view field borrows
/// [`FrontExtract`].
///
/// The four lifetimes are one per *provenance* (`'c`hangeset, `'d`ropped nodes,
/// `'p`rior overlay, `'n`ode set + workspace) rather than one unified `'a`,
/// because the apply's tail moves those owned values out at different points —
/// unifying them would make every such move wait for the last use of *any* view.
struct FrontLink<'c, 'd, 'p, 'n> {
    /// Every path this changeset touches (added ∪ modified ∪ removed).
    dropped_files: BTreeSet<&'c String>,
    /// The ids of the nodes that dropped with their file.
    dropped_ids: BTreeSet<&'d NodeId>,
    /// The ids of the prior project overlay (regenerated, so always retired).
    prior_project_node_ids: BTreeSet<&'p NodeId>,
    /// Survivors ⧺ freshly-extracted nodes, as a *reference* view — the hot path
    /// must not deep-clone every node a second time.
    all_nodes: Vec<&'n Node>,
    /// The five link lookups over `all_nodes` (and the `Workspace`).
    idx: LinkIndices<'n>,
    /// The reference universe, **healed** — the input `compute_impact` reads and
    /// the link stage resolves.
    refs_by_name: BTreeMap<String, Vec<Reference>>,
    /// The maintained per-file export index (ADR-0020).
    exports_by_file: BTreeMap<String, Vec<Export>>,
    /// `(importing file, bound name) → definition node` (ADR-0018/0020).
    import_scope: BTreeMap<(String, String), NodeId>,
    /// References reconstructed by the divergence guard (empty in the common case).
    healed_refs: Vec<(String, Reference)>,
}

/// Run the borrowed half of the front sequence (see the module comment above).
///
/// Takes the [`FrontExtract`] outputs as individual borrows rather than
/// `&FrontExtract`: a whole-struct borrow would still be live at the point the
/// apply moves `extracted.structural` into the edge list, and the alternative is
/// cloning it — which is exactly the edge clone ADR-0042 Phase 1d removed.
#[allow(clippy::too_many_arguments)]
fn front_link<'c, 'd, 'p, 'n>(
    prior: &GraphState,
    source: &dyn Source,
    changes: &'c ChangeSet,
    dropped_nodes: &'d [Node],
    surviving_nodes: &'n [Node],
    prior_project_nodes: &'p [Node],
    workspace: &'n filigrio_core::Workspace,
    new_nodes: &'n [Node],
    new_refs: &[(String, Reference)],
    new_exports: &BTreeMap<String, Vec<Export>>,
) -> FrontLink<'c, 'd, 'p, 'n> {
    let dropped_files: BTreeSet<&String> = changes.touched().collect();
    let dropped_ids: BTreeSet<&NodeId> = dropped_nodes.iter().map(|n| &n.id).collect();
    // The project overlay's prior node ids (needed by the divergence guard and by
    // the apply's carry filter).
    let prior_project_node_ids: BTreeSet<&NodeId> =
        prior_project_nodes.iter().map(|n| &n.id).collect();

    // 3. The link indices (five maps incl. the `SymbolTable`'s candidate
    //    pool), over the whole surviving + new node set. `all_nodes` is a
    //    *reference* view — the survivors were already cloned out of `prior`
    //    once; the hot path must not deep-clone every node a second time.
    let all_nodes: Vec<&Node> = stage("all_nodes_view", || {
        surviving_nodes.iter().chain(new_nodes.iter()).collect()
    });
    let idx = stage("link_indices", || build_link_indices(&all_nodes, workspace));

    // 4. Rebuild the reference universe incrementally (keep prior references
    //    whose source survived, add freshly-extracted ones, dedup) and the
    //    per-file export index the same way (ADR-0020).
    let mut refs_by_name = stage("rebuild_refs", || {
        rebuild_refs(&prior.reverse, &dropped_ids, new_refs)
    });
    let exports_by_file = stage("maintain_exports", || {
        maintain_exports(&prior.exports, &dropped_files, new_exports)
    });

    // ---- stage 2+3 · contracts: the `ModuleResolver` (backed by the
    //      `Workspace`) turns each `imports` specifier into a target module;
    //      the **export graph** walks re-exports (barrels) to the imported
    //      symbol's real definition. Result: the per-file import scope
    //      `(file, bound name) → definition node` — the strongest resolution
    //      signal, licensing a *correct* cross-project bind (ADR-0018/0020).
    let resolver = stage("build_resolver", || build_resolver(source, workspace));
    let import_scope = stage("import_scope", || {
        build_import_scope(
            &refs_by_name,
            &all_nodes,
            &exports_by_file,
            &idx.file_of,
            &*resolver,
        )
    });

    // Guard the `reverse` ↔ `graph.edges` consistency invariant: a diverged
    // persisted state (a reference-site edge whose `Reference` is missing
    // from `prior.reverse`) would silently lose that edge in the carry —
    // reconstruct the missing reference from the edge itself so it
    // re-resolves like any other and `reverse` converges. This is the last
    // stage that can change `refs_by_name`, which is why the shared front half
    // ends *here* and not at `rebuild_refs`: the impact set must be computed
    // from the same reference universe the linker resolves.
    let healed_refs = stage("heal_diverged_refs", || {
        heal_diverged_refs(
            &mut refs_by_name,
            prior,
            &dropped_ids,
            &prior_project_node_ids,
            &idx.file_of,
            &import_scope,
        )
    });

    FrontLink {
        dropped_files,
        dropped_ids,
        prior_project_node_ids,
        all_nodes,
        idx,
        refs_by_name,
        exports_by_file,
        import_scope,
        healed_refs,
    }
}

/// **The F8 seam** (ADR-0042 Phase 1c): reconcile a changeset with the source's
/// *current* reality before any stage consumes it.
///
/// A changeset is detected by walking the tree and applied some time later — at
/// next.js scale, seconds to tens of seconds later. A branch switch, a `cargo
/// clean`, a build deleting artifacts, or an editor temp file inside that window
/// leaves the changeset naming a path that is no longer there. Reading it with
/// `?` aborted the **whole** apply, which is strictly worse than the race it was
/// reporting: the batch's other files stay unindexed *and* the deleted file keeps
/// its nodes. So:
///
/// - **`exists() == false` ⇒ a removal.** A cold build of the tree as it now
///   stands has no such file, so folding it into `removed` is exactly what
///   `incremental ≡ cold` requires. If it was in the prior state its nodes/edges/
///   manifest entry drop like any removal's; if it was never indexed (created
///   *and* deleted inside the window) the removal is a no-op, which is also
///   correct — `maintain_manifest` removes an absent key and `touched()` drops
///   nodes that do not exist.
/// - **`exists() == true` ⇒ untouched.** A permission or IO fault on a file that
///   *is* there is a real error and stays one: it flows into `extract_changed`'s
///   `read(…)?` exactly as before. This is why [`Source::exists`] is a required
///   port method and not a `read(path).is_ok()` default — that default conflates
///   "cannot read" with "not there" and would silently converge away a
///   permission error.
///
/// Returns the effective changeset (borrowed — no clone — in the overwhelmingly
/// common case where nothing vanished) and the vanished count, which the delta
/// reports so the convergence is visible (ADR-0029).
fn fold_vanished<'a>(
    changes: &'a ChangeSet,
    source: &dyn Source,
) -> (std::borrow::Cow<'a, ChangeSet>, usize) {
    let gone = |p: &String| !source.exists(p);
    // Fast path: one metadata probe per changed path, no allocation. The extract
    // stage is about to read every one of these anyway.
    if !changes.to_index().any(gone) {
        return (std::borrow::Cow::Borrowed(changes), 0);
    }
    let mut added = Vec::with_capacity(changes.added.len());
    let mut modified = Vec::with_capacity(changes.modified.len());
    let mut removed = changes.removed.clone();
    let mut vanished = 0usize;
    for (src_list, dst_list) in [
        (&changes.added, &mut added),
        (&changes.modified, &mut modified),
    ] {
        for path in src_list {
            if gone(path) {
                vanished += 1;
                removed.push(path.clone());
            } else {
                dst_list.push(path.clone());
            }
        }
    }
    (
        std::borrow::Cow::Owned(ChangeSet {
            added,
            modified,
            removed,
        }),
        vanished,
    )
}

/// Raw per-file facts from the **extract** stage, before any linking: fresh
/// nodes, already-resolved structural edges (`Node` targets), unresolved
/// references (`Symbol` targets), and per-file export tables (ADR-0020).
struct Extracted {
    nodes: Vec<Node>,
    structural: Vec<Edge>,
    refs: Vec<(String, Reference)>,
    exports: BTreeMap<String, Vec<Export>>,
}

/// **Extract stage** — index the changed files (added ∪ modified) into raw
/// facts. Pure per-file work (O(change)); no cross-file linking here. Nodes
/// carry no `project` attr — membership is derived from the `Workspace`
/// (ADR-0019), never stamped.
fn extract_changed(
    changes: &ChangeSet,
    source: &dyn Source,
    extractor: &dyn Extractor,
) -> Result<Extracted> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut structural: Vec<Edge> = Vec::new();
    let mut refs: Vec<(String, Reference)> = Vec::new();
    let mut exports: BTreeMap<String, Vec<Export>> = BTreeMap::new();
    for path in changes.to_index() {
        let artifact = filigrio_core::classify(path);
        if !extractor.handles(&artifact) {
            continue;
        }
        let bytes = source.read(path)?;
        let extraction = extractor.extract(&artifact, &bytes)?;
        nodes.extend(extraction.nodes);
        if !extraction.exports.is_empty() {
            exports.insert(path.clone(), extraction.exports);
        }
        for edge in extraction.edges {
            match edge.target {
                EdgeTarget::Symbol(reff) => {
                    let type_hint = reff.hints.get("type").cloned();
                    let specifier = reff.hints.get("specifier").cloned();
                    let imported = reff.hints.get("imported").cloned();
                    let recv_opaque = reff
                        .hints
                        .get("recv")
                        .map(|r| r == "opaque")
                        .unwrap_or(false);
                    let recv_returns = reff.hints.get("recv_returns").cloned();
                    let recv_returns_owner = reff.hints.get("recv_returns_owner").cloned();
                    refs.push((
                        reff.name,
                        Reference {
                            source: edge.source,
                            relation: edge.relation,
                            type_hint,
                            specifier,
                            imported,
                            recv_opaque,
                            recv_returns,
                            recv_returns_owner,
                        },
                    ))
                }
                EdgeTarget::Node(_) => structural.push(edge),
            }
        }
    }
    Ok(Extracted {
        nodes,
        structural,
        refs,
        exports,
    })
}

/// The node-keyed lookup maps the link stage resolves through, built once per
/// apply over the surviving + new node set (borrowed, not cloned).
struct LinkIndices<'a> {
    /// node id → its source file (path).
    file_of: BTreeMap<&'a NodeId, Option<&'a String>>,
    /// node id → its `impl` owner type (methods carry it), so a type-qualified
    /// call `T::method` can be narrowed to T's method.
    impl_of: BTreeMap<&'a NodeId, Option<&'a String>>,
    /// node id → its declared return type (`returns` attr), so a variable bound
    /// to a call's result can be typed by its callee's return (ADR-0023 recall).
    returns_of: BTreeMap<&'a NodeId, Option<&'a String>>,
    /// node id → its project root: the file's nearest project root, **derived**
    /// from the `Workspace` (never stamped — ADR-0019). `None` = root/no-manifest.
    /// A manifest change re-derives this for free on the next apply.
    project_of: BTreeMap<&'a NodeId, Option<&'a str>>,
    /// symbol name → its defining node id(s) — the `SymbolTable`'s candidate
    /// pool. Multiple ids ⇒ ambiguity.
    candidates: BTreeMap<String, Vec<NodeId>>,
}

/// Build the [`LinkIndices`] for this apply (step 3 of [`Engine::apply`]).
fn build_link_indices<'a>(
    all_nodes: &[&'a Node],
    workspace: &'a filigrio_core::Workspace,
) -> LinkIndices<'a> {
    let file_of = stage("li.file_of", || {
        all_nodes
            .iter()
            .map(|n| (&n.id, n.source_file.as_ref()))
            .collect()
    });
    let impl_of = stage("li.impl_of", || {
        all_nodes
            .iter()
            .map(|n| (&n.id, n.attrs.get("impl")))
            .collect()
    });
    let returns_of = stage("li.returns_of", || {
        all_nodes
            .iter()
            .map(|n| (&n.id, n.attrs.get(attrs::RETURNS)))
            .collect()
    });
    let project_of = stage("li.project_of", || {
        all_nodes
            .iter()
            .map(|n| {
                let root = n.source_file.as_deref().and_then(|f| workspace.root_of(f));
                (&n.id, root)
            })
            .collect()
    });
    let candidates = stage("li.candidates", || {
        let mut candidates: BTreeMap<String, Vec<NodeId>> = BTreeMap::new();
        for n in all_nodes {
            if is_linkable(n) {
                candidates
                    .entry(n.label.clone())
                    .or_default()
                    .push(n.id.clone());
            }
        }
        for ids in candidates.values_mut() {
            ids.sort();
            ids.dedup();
        }
        candidates
    });
    LinkIndices {
        file_of,
        impl_of,
        returns_of,
        project_of,
        candidates,
    }
}

/// Build the export/symbol index (ADR-0042 Phase 1.1) from the per-apply
/// [`LinkIndices`]: symbol name → its candidate node ids (the pre-sorted,
/// deduped candidate pool) + the modules that define the name (`module_of` = the
/// file today, ADR-0021 identity grouping). A from-scratch derivation over the
/// whole node set, so the maintained index on `GraphState` equals a rebuild by
/// construction (the invariant the gate test pins). The candidate pool is already
/// recomputed each apply, so promoting it to persisted state costs nothing here;
/// truly-incremental maintenance is a Phase-2 refinement the sharded store earns.
fn build_symbol_index(idx: &LinkIndices<'_>) -> SymbolIndex {
    let mut by_name: BTreeMap<String, SymbolDefs> = BTreeMap::new();
    for (name, ids) in &idx.candidates {
        let mut modules: Vec<String> = ids
            .iter()
            .filter_map(|id| idx.file_of.get(id).copied().flatten())
            .map(|f| module_of(f))
            .collect();
        modules.sort();
        modules.dedup();
        by_name.insert(
            name.clone(),
            SymbolDefs {
                nodes: ids.clone(),
                modules,
            },
        );
    }
    SymbolIndex { by_name }
}

/// **Physical stage** — maintain the `Workspace` incrementally (ADR-0019): carry
/// prior projects, then apply only the *manifest* changes in this changeset — a
/// touched manifest drops its project, an added/modified one re-parses it. No
/// filesystem walk (manifests are already touched paths), no re-read of
/// unchanged projects. `prior = ∅` ⇒ full discovery.
fn maintain_workspace(
    prior: &filigrio_core::Workspace,
    changes: &ChangeSet,
    source: &dyn Source,
) -> Result<filigrio_core::Workspace> {
    let mut workspace = prior.clone();
    for path in changes.touched() {
        if filigrio_core::manifest_basename(path).is_some() {
            workspace.projects.remove(dir_of(path));
        }
    }
    for path in changes.to_index() {
        if let Some(base) = filigrio_core::manifest_basename(path) {
            let bytes = source.read(path)?;
            let m = manifest::parse(base, &bytes);
            let root = dir_of(path).to_string();
            workspace.projects.insert(
                root.clone(),
                filigrio_core::Project {
                    root,
                    manifest: base.to_string(),
                    name: m.name,
                    entry: m.entry,
                    deps: m.deps,
                },
            );
        }
    }
    Ok(workspace)
}

/// Rebuild the reference universe: keep prior references whose `source` survived
/// (not in a dropped file), add the freshly-extracted ones, dedup parallel edges.
///
/// This *is* the shape of the ADR-0042 Phase 1d `reverse` patch — drop by source,
/// append, re-canonicalize — which is why the delta can carry
/// `(reverse_dropped, reverse_added)` instead of the whole ~33 MB index and the
/// store can reproduce the result exactly (`filigrio_store::merge_reverse`).
/// `new_refs` is borrowed rather than consumed so the caller keeps it for the
/// delta; it is O(change), so the clone is not on any hot path.
fn rebuild_refs(
    prior: &ReverseIndex,
    dropped_ids: &BTreeSet<&NodeId>,
    new_refs: &[(String, Reference)],
) -> BTreeMap<String, Vec<Reference>> {
    let mut refs_by_name: BTreeMap<String, Vec<Reference>> = BTreeMap::new();
    for (name, list) in &prior.refs {
        for r in list {
            if !dropped_ids.contains(&r.source) {
                refs_by_name
                    .entry(name.clone())
                    .or_default()
                    .push(r.clone());
            }
        }
    }
    for (name, reference) in new_refs {
        refs_by_name
            .entry(name.clone())
            .or_default()
            .push(reference.clone());
    }
    for list in refs_by_name.values_mut() {
        list.sort();
        list.dedup();
    }
    refs_by_name
}

/// Guard the `reverse` ↔ `graph.edges` consistency invariant against a
/// **diverged persisted state**.
///
/// `apply` produces both together, so in normal operation every reference-site
/// edge in `graph.edges` has a matching `Reference` in `reverse`. If a reloaded
/// state lost such an entry (a hand-edited or partially-restored `state.json`),
/// step 6 silently loses the edge: it drops every prior edge whose
/// `(source, relation)` is a reference site — and any *other* surviving
/// reference from the same source/relation marks the pair a reference site —
/// then relies on the reference universe to re-emit it, which nothing does.
///
/// Detection is deliberately conservative, per `(source, relation)` group: only
/// when the prior edges **outnumber** the surviving references (a true
/// cardinality divergence) are the *unclaimed* edges — no reference accounts for
/// them by name (target label / symbol name) or through the import scope (an
/// aliased import binds under the alias, not the target's label) — healed,
/// capped at the deficit. Healing reconstructs a bare `Reference` from the edge
/// (source + relation + the reference name recovered from the target: a resolved
/// edge's target node label, an unresolved edge's symbol name), so the edge
/// re-resolves like any other reference and the emitted `reverse` converges on
/// this very apply. In a consistent state every group balances and this is a
/// no-op — incremental == cold is untouched.
///
/// **Two passes, cheap first** (ADR-0042 Phase 1b). Divergence is a *cardinality*
/// property, so the detection pass only counts: references per site, then
/// reference-site edges per site. It allocates nothing per site beyond two
/// integers, and in the consistent case — which is every apply that is not
/// repairing a damaged `state.json` — it returns there. Only the sites that
/// actually show a deficit pay for the name-level accounting (per-site name
/// lists, the import-scope claim set, the prior label map). The single-pass form
/// built all of that for **every** site on **every** apply: 656 ms of a 3.8 s
/// next.js apply (17 %) spent proving that nothing was wrong.
///
/// Returns the references it synthesized (empty in the common, consistent case)
/// so they can ride the delta's `reverse_added` list alongside the freshly
/// extracted ones — a repair the store must see, or the patched `reverse` would
/// converge in the engine and not on disk (ADR-0042 Phase 1d P1).
fn heal_diverged_refs(
    refs_by_name: &mut BTreeMap<String, Vec<Reference>>,
    prior: &GraphState,
    dropped_ids: &BTreeSet<&NodeId>,
    prior_project_node_ids: &BTreeSet<&NodeId>,
    file_of: &BTreeMap<&NodeId, Option<&String>>,
    import_scope: &BTreeMap<(String, String), NodeId>,
) -> Vec<(String, Reference)> {
    // ---- pass 1 · detect: per-site cardinality, no per-site allocation ----
    let diverged: BTreeSet<(&NodeId, &str)> = stage("hd.detect", || {
        // One map, counted up by references and back down by the prior edges that
        // claim them: a site left **negative** has more edges than references —
        // the divergence. (Two maps would hash every edge's site twice; the site
        // key is a whole node-id string, so hashing is the pass's real cost.)
        let mut balance: HashMap<(&NodeId, &str), i64> = HashMap::new();
        for list in refs_by_name.values() {
            for r in list {
                *balance.entry((&r.source, r.relation.as_str())).or_default() += 1;
            }
        }
        // Only edges at a *reference site* are at risk (step 6 carries the rest),
        // and only from a source that survives this apply. A modified file's nodes
        // are dropped and re-extracted under the same ids, so `dropped_ids` can
        // name a source that also has fresh references — the filter is not
        // redundant with the reference universe.
        for e in &prior.graph.edges {
            if dropped_ids.contains(&e.source) || prior_project_node_ids.contains(&e.source) {
                continue;
            }
            if let Some(c) = balance.get_mut(&(&e.source, e.relation.as_str())) {
                *c -= 1;
            }
        }
        balance
            .into_iter()
            .filter(|(_, balance)| *balance < 0)
            .map(|(site, _)| site)
            .collect()
    });
    if diverged.is_empty() {
        return Vec::new(); // consistent — the overwhelmingly common case
    }

    // ---- pass 2 · repair: name-level accounting, diverged sites only ----
    let healed: Vec<(String, Reference)> = {
        let label_of: BTreeMap<&NodeId, &str> = stage("hd.label_of", || {
            prior
                .graph
                .nodes
                .iter()
                .map(|n| (&n.id, n.label.as_str()))
                .collect()
        });

        // Surviving reference names per site, plus the defs each site's imports
        // claim through the import scope (alias-aware accounting).
        let (refs_of, import_claims) = stage("hd.refs_of", || {
            let mut refs_of: BTreeMap<(&NodeId, &str), Vec<&str>> = BTreeMap::new();
            let mut import_claims: BTreeSet<(&NodeId, &str, &NodeId)> = BTreeSet::new();
            for (name, list) in refs_by_name.iter() {
                for r in list {
                    let site = (&r.source, r.relation.as_str());
                    if !diverged.contains(&site) {
                        continue;
                    }
                    refs_of.entry(site).or_default().push(name);
                    if let Some(f) = file_of.get(&r.source).copied().flatten() {
                        if let Some(def) = import_scope.get(&(f.clone(), name.clone())) {
                            import_claims.insert((&r.source, r.relation.as_str(), def));
                        }
                    }
                }
            }
            (refs_of, import_claims)
        });

        // The prior reference-site edges step 6 will drop, grouped by site. A
        // `(source, relation)` with no surviving reference is *not* a reference
        // site — step 6 carries those edges, so they are not at risk here (pass 1
        // applied the same filter when counting).
        let edges_of: BTreeMap<(&NodeId, &str), Vec<&Edge>> = stage("hd.edges_of", || {
            let mut edges_of: BTreeMap<(&NodeId, &str), Vec<&Edge>> = BTreeMap::new();
            for e in &prior.graph.edges {
                let site = (&e.source, e.relation.as_str());
                if !diverged.contains(&site) {
                    continue;
                }
                edges_of.entry(site).or_default().push(e);
            }
            edges_of
        });

        let mut healed = Vec::new();
        for (site, edges) in &edges_of {
            let names = &refs_of[site];
            if edges.len() <= names.len() {
                continue; // balanced (or over-reffed) — consistent
            }
            let deficit = edges.len() - names.len();
            let name_set: BTreeSet<&str> = names.iter().copied().collect();
            // (reference name, recv_opaque) per unclaimed edge.
            let mut unclaimed: Vec<(&str, bool)> = Vec::new();
            for e in edges {
                match &e.target {
                    EdgeTarget::Node(t) => {
                        let claimed = label_of
                            .get(t)
                            .map(|l| name_set.contains(l))
                            // no prior node for the target → nothing to reconstruct
                            .unwrap_or(true)
                            || import_claims.contains(&(&e.source, e.relation.as_str(), t));
                        if !claimed {
                            if let Some(l) = label_of.get(t) {
                                unclaimed.push((l, false));
                            }
                        }
                    }
                    EdgeTarget::Symbol(r) => {
                        if !name_set.contains(r.name.as_str()) {
                            let opaque = r.hints.get("recv").is_some_and(|v| v == "opaque");
                            unclaimed.push((r.name.as_str(), opaque));
                        }
                    }
                }
            }
            unclaimed.sort_unstable();
            unclaimed.dedup();
            for (name, recv_opaque) in unclaimed.into_iter().take(deficit) {
                healed.push((
                    name.to_string(),
                    Reference {
                        source: site.0.clone(),
                        relation: site.1.to_string(),
                        type_hint: None,
                        specifier: None,
                        imported: None,
                        recv_opaque,
                        recv_returns: None,
                        recv_returns_owner: None,
                    },
                ));
            }
        }
        healed
    };
    for (name, r) in &healed {
        let list = refs_by_name.entry(name.clone()).or_default();
        list.push(r.clone());
        list.sort();
        list.dedup();
    }
    healed
}

/// Maintain the per-file export index incrementally (ADR-0020): carry surviving
/// files' export tables, replace the freshly-extracted ones.
///
/// `new_exports` is borrowed rather than consumed (like [`rebuild_refs`]'s
/// `new_refs`) so the front half can hand the same extraction to both of its
/// consumers; it covers only the changed files, so the clone is O(change) and off
/// every hot path — the same clone the surviving half already pays.
fn maintain_exports(
    prior: &ExportIndex,
    dropped_files: &BTreeSet<&String>,
    new_exports: &BTreeMap<String, Vec<Export>>,
) -> BTreeMap<String, Vec<Export>> {
    let mut exports_by_file: BTreeMap<String, Vec<Export>> = BTreeMap::new();
    for (f, exs) in &prior.by_file {
        if !dropped_files.contains(f) {
            exports_by_file.insert(f.clone(), exs.clone());
        }
    }
    exports_by_file.extend(new_exports.iter().map(|(f, exs)| (f.clone(), exs.clone())));
    exports_by_file
}

/// Maintain the file manifest incrementally: every indexed path gets a (skeleton)
/// entry, every removed path loses its entry.
fn maintain_manifest(
    prior: &filigrio_core::Manifest,
    changes: &ChangeSet,
) -> filigrio_core::Manifest {
    let mut manifest = prior.clone();
    for path in changes.to_index() {
        manifest.entries.insert(
            path.clone(),
            filigrio_core::ManifestEntry {
                hash: 0,
                last_modified: None,
                revision: None,
            },
        );
    }
    for path in &changes.removed {
        manifest.entries.remove(path);
    }
    manifest
}

/// The delta's `affected` telemetry (hand-off to 2b): added ∪ removed nodes, plus
/// the *surviving* references whose target symbol was (un)defined by this change —
/// the dependents this apply had to relink.
///
/// This is a **name-based over-counting heuristic**, not an exact changed-edge
/// count: every surviving referrer of every symbol *label* defined (or undefined)
/// in a touched file is counted, even if its resolution did not actually change
/// (e.g. a homonym in another project, or a reference that re-binds to the same
/// target). Telemetry for the O(change) claim — never used for correctness.
fn compute_affected(
    dropped_nodes: &[Node],
    new_nodes: &[Node],
    surviving_nodes: &[Node],
    reverse: &ReverseIndex,
    removed: usize,
) -> usize {
    let affected_names: BTreeSet<&String> = dropped_nodes
        .iter()
        .chain(new_nodes)
        .filter(|n| is_linkable(n))
        .map(|n| &n.label)
        .collect();
    let surviving_ids: BTreeSet<&NodeId> = surviving_nodes.iter().map(|n| &n.id).collect();
    let relinked: BTreeSet<&NodeId> = reverse
        .refs
        .iter()
        .filter(|(name, _)| affected_names.contains(name))
        .flat_map(|(_, list)| list.iter().map(|r| &r.source))
        .filter(|src| surviving_ids.contains(*src))
        .collect();
    new_nodes.len() + removed + relinked.len()
}

/// The change-impact set (ADR-0042 Phase 1.2) — which reference *sites* the
/// scoped linker must re-resolve, promoted from `compute_affected`'s
/// over-counting *telemetry* to resolution *authority*. Over-covering is
/// acceptable (costs time, never correctness); under-covering a reference whose
/// resolution could change is the one forbidden failure, so the definition is
/// deliberately conservative.
#[derive(Clone, Debug, Default)]
pub struct ImpactReport {
    /// Re-resolve *every* reference this apply. Set when a **workspace**
    /// (project-scoping) or **export-structure** change makes site-level reasoning
    /// unsound — no local set can bound how the import/export graph or project
    /// boundaries re-thread resolution, so the safe move is full re-resolution.
    /// When set, the scoped path is provably equivalent to `Global` for this apply.
    pub full: bool,
    /// The impacted reference sites, keyed `source → { relations }`.
    pub sites: BTreeMap<NodeId, BTreeSet<String>>,
    /// Labels defined or removed in touched files — the candidate-set delta. A
    /// reference to a dirty name may resolve differently and is always impacted.
    pub dirty_names: BTreeSet<String>,
}

impl ImpactReport {
    /// Is the reference site `(source, relation)` impacted (or is the whole apply
    /// full-impact)? Cheap — no allocation on the hot re-resolution path.
    pub fn site_impacted(&self, source: &NodeId, relation: &str) -> bool {
        self.full
            || self
                .sites
                .get(source)
                .is_some_and(|rels| rels.contains(relation))
    }

    /// Total impacted reference sites (0 when `full` — every site re-resolved).
    pub fn site_count(&self) -> usize {
        self.sites.values().map(BTreeSet::len).sum()
    }
}

/// Compute the [`ImpactReport`] for one apply (ADR-0042 Phase 1.2).
///
/// A reference site is impacted when **any** of:
///   * `full` fires — `workspace != prior` (project scoping shifted) or a touched
///     file's export table changed (the import/export graph shifted);
///   * the reference's source lives in a touched file (its references were
///     re-extracted, so every field may have changed);
///   * the reference's name is a *dirty name* (a candidate for it was
///     defined/removed in a touched file — its candidate set changed);
///   * the reference's deferred receiver callee (`recv_returns`) is a dirty name
///     (the callee's return type — hence the receiver type — may have changed,
///     ADR-0026).
///
/// The first two `full` triggers subsume every way a *non-local* signal
/// (barrels/re-exports, `module_of` grouping, manifest deps) can re-thread a
/// reference whose own name and source module are untouched — bought with the
/// over-approximation of re-resolving everything on those (rare) applies.
#[allow(clippy::too_many_arguments)]
fn compute_impact(
    dropped_nodes: &[Node],
    new_nodes: &[Node],
    refs_by_name: &BTreeMap<String, Vec<Reference>>,
    touched_files: &BTreeSet<&String>,
    prior_exports: &ExportIndex,
    exports_by_file: &BTreeMap<String, Vec<Export>>,
    prior_workspace: &filigrio_core::Workspace,
    workspace: &filigrio_core::Workspace,
    file_of: &BTreeMap<&NodeId, Option<&String>>,
) -> ImpactReport {
    // Full-impact triggers (see fn docs): project scoping or the export/import
    // graph changed — no site-level reasoning is sound, re-resolve everything.
    let workspace_changed = workspace != prior_workspace;
    let export_structure_changed = touched_files.iter().any(|f| {
        let empty: &[Export] = &[];
        prior_exports.by_file.get(*f).map_or(empty, Vec::as_slice)
            != exports_by_file.get(*f).map_or(empty, Vec::as_slice)
    });
    let full = workspace_changed || export_structure_changed;

    // Dirty names = labels defined or removed in touched files (candidate delta).
    let dirty_names: BTreeSet<String> = dropped_nodes
        .iter()
        .chain(new_nodes)
        .filter(|n| is_linkable(n))
        .map(|n| n.label.clone())
        .collect();

    let mut sites: BTreeMap<NodeId, BTreeSet<String>> = BTreeMap::new();
    if !full {
        for (name, list) in refs_by_name {
            // Workspace-aware dirty name check: a reference is impacted if its name
            // or its workspace-stripped version is dirty. This handles cases like
            // references to "filigrio_core::Direction" when "Direction" is dirty.
            let name_dirty = dirty_names.contains(name)
                || strip_workspace_qualifier(name, workspace)
                    .is_some_and(|(stripped, _)| dirty_names.contains(&stripped));
            for r in list {
                let source_touched = file_of
                    .get(&r.source)
                    .copied()
                    .flatten()
                    .is_some_and(|f| touched_files.contains(f));
                let recv_dirty = r
                    .recv_returns
                    .as_deref()
                    .is_some_and(|c| dirty_names.contains(c));
                if name_dirty || source_touched || recv_dirty {
                    sites
                        .entry(r.source.clone())
                        .or_default()
                        .insert(r.relation.clone());
                }
            }
        }
    }

    ImpactReport {
        full,
        sites,
        dirty_names,
    }
}

/// **Contract stage** (ADR-0018/0020) — build the per-file import scope. The
/// `local_def` map keys `(module, name) → def` via the `module_of` bridge seam
/// (identity today — ADR-0021), so Go/Rust grouping changes only `module_of`.
/// The export graph then walks re-exports (barrels) from each `imports`
/// specifier's target module to the imported symbol's real definition. Result:
/// `(importing file, bound name) → definition node`.
fn build_import_scope(
    refs_by_name: &BTreeMap<String, Vec<Reference>>,
    all_nodes: &[&Node],
    exports_by_file: &BTreeMap<String, Vec<Export>>,
    file_of: &BTreeMap<&NodeId, Option<&String>>,
    resolver: &dyn ModuleResolver,
) -> BTreeMap<(String, String), NodeId> {
    let local_def = stage("is.local_def", || {
        let mut local_def: BTreeMap<(ModuleId, String), NodeId> = BTreeMap::new();
        for n in all_nodes {
            if is_linkable(n) {
                if let Some(f) = &n.source_file {
                    local_def
                        .entry((module_of(f), n.label.clone()))
                        .or_insert_with(|| n.id.clone());
                }
            }
        }
        local_def
    });
    let export_graph = stage("is.export_graph", || {
        export_graph::ExportGraph::build(exports_by_file, &local_def, resolver)
    });
    stage("is.walk_imports", || {
        let mut import_scope: BTreeMap<(String, String), NodeId> = BTreeMap::new();
        for (name, list) in refs_by_name {
            for r in list {
                if r.relation != IMPORTS {
                    continue;
                }
                let (Some(spec), Some(importing)) = (
                    r.specifier.as_deref(),
                    file_of.get(&r.source).copied().flatten(),
                ) else {
                    continue;
                };
                // specifier → target module (via its entry file), then walk its
                // exports. `module_of` is the physical-file → module bridge seam.
                let Some(entry) = stage("is.resolve_spec", || resolver.resolve(importing, spec))
                else {
                    continue;
                };
                let imported = r.imported.as_deref().unwrap_or(name);
                if let Some(def) = stage("is.export_walk", || {
                    export_graph.resolve_import(&module_of(&entry), imported)
                }) {
                    import_scope.insert((importing.clone(), name.clone()), def);
                }
            }
        }
        import_scope
    })
}

/// **Link stage** — resolve every reference against its candidate definitions
/// (through the import/project/name-type rules in [`resolve`]) → the linked edge
/// set. This is what relinks dependents when a definition changes (Phase 2a).
/// The [`LinkScope::Global`] path; [`resolve_impacted`] is the scoped counterpart.
fn resolve_all(
    refs_by_name: &BTreeMap<String, Vec<Reference>>,
    candidates: &BTreeMap<String, Vec<NodeId>>,
    ctx: &LinkCtx<'_>,
) -> Vec<Edge> {
    let mut resolved_edges: Vec<Edge> = Vec::new();
    for (name, list) in refs_by_name {
        let cands = candidates.get(name);
        for r in list {
            // P0c: Track workspace qualifier for disambiguation
            let workspace_qualifier =
                strip_workspace_qualifier(name, ctx.workspace).map(|(_, crate_name)| crate_name);

            // Workspace-aware qualified path resolution (P0b + P0c):
            // If this is a workspace-qualified name with NO candidates, try resolving with the stripped version
            let result = if cands.is_none() {
                if let Some((stripped_name, _)) = strip_workspace_qualifier(name, ctx.workspace) {
                    // Only try workspace-aware resolution when the original name has no candidates
                    // Try resolving with the stripped name first (e.g., "Error" instead of "filigrio_core::Error")
                    let stripped_cands = candidates.get(&stripped_name);
                    let edge = resolve_ref(
                        &stripped_name,
                        r,
                        stripped_cands,
                        candidates,
                        true,
                        ctx,
                        workspace_qualifier.as_deref(),
                    );

                    // If we got a resolved node, use it; otherwise try the last segment
                    if let EdgeTarget::Node(_) = edge.target {
                        edge
                    } else {
                        // If the stripped name is multi-segment (e.g., "module::Type"), try resolving just the last segment
                        if stripped_name.contains("::") {
                            let last_segment =
                                stripped_name.split("::").last().unwrap_or(&stripped_name);
                            let last_segment_cands = candidates.get(last_segment);
                            let last_edge = resolve_ref(
                                last_segment,
                                r,
                                last_segment_cands,
                                candidates,
                                true,
                                ctx,
                                None, // No workspace qualifier for last segment fallback
                            );

                            if let EdgeTarget::Node(_) = last_edge.target {
                                last_edge
                            } else {
                                resolve_ref(name, r, cands, candidates, false, ctx, None)
                            }
                        } else {
                            resolve_ref(name, r, cands, candidates, false, ctx, None)
                        }
                    }
                } else {
                    // Not workspace-qualified, use normal resolution
                    resolve_ref(name, r, cands, candidates, false, ctx, None)
                }
            } else {
                // P0c: Original name has candidates - pass workspace qualifier for disambiguation
                resolve_ref(
                    name,
                    r,
                    cands,
                    candidates,
                    false,
                    ctx,
                    workspace_qualifier.as_deref(),
                )
            };

            resolved_edges.push(result);
        }
    }
    resolved_edges
}

/// Resolve a single reference to its linked edge — the one code path both
/// [`resolve_all`] (Global) and [`resolve_impacted`] (Scoped) share, so the two
/// strategies resolve *identically*; they differ only in *which* references they
/// re-resolve versus carry.
fn resolve_ref(
    name: &str,
    r: &Reference,
    cands: Option<&Vec<NodeId>>,
    candidates: &BTreeMap<String, Vec<NodeId>>,
    allow_cross_project: bool,
    ctx: &LinkCtx<'_>,
    workspace_qualifier: Option<&str>, // P0c: stripped workspace qualifier for disambiguation
) -> Edge {
    // Deferred receiver type (`let x = compute(); x.m()`): read the callee's
    // return type here — the link stage is the first point that sees every
    // fn's `returns`. A confident type narrows `m` type-directed; an unknown
    // or ambiguous return declines like an opaque receiver (never a guess).
    let (type_hint, recv_opaque) = match &r.recv_returns {
        Some(callee) => match return_type_of(
            callee,
            r.recv_returns_owner.as_deref(),
            &r.source,
            candidates,
            ctx,
        ) {
            Some(ty) => (Some(ty), false),
            None => (None, true),
        },
        None => (r.type_hint.clone(), r.recv_opaque),
    };
    let p0c_ctx = P0cResolveCtx {
        type_hint: type_hint.as_deref(),
        recv_opaque,
        allow_cross_project,
        ctx,
        workspace_qualifier,
    };
    let (mut target, confidence) = resolve_p0c(name, &r.source, cands, candidates, p0c_ctx);
    // ADR-0029: when an opaque receiver is *why* this call declined, carry the
    // marker onto the persisted `Symbol` so the query surface can honestly say
    // "unresolved by-name callers (opaque receivers)" and tell an opaque decline
    // from an unknown-external free call. `unresolved()` builds a bare target;
    // the reason lives here (the reference), so we stamp it here.
    if recv_opaque {
        if let EdgeTarget::Symbol(tref) = &mut target {
            tref.hints.insert("recv".into(), "opaque".into());
        }
    }
    Edge {
        source: r.source.clone(),
        relation: r.relation.clone(),
        confidence,
        target,
    }
}

/// Context for P0c resolution parameters
struct P0cResolveCtx<'a> {
    type_hint: Option<&'a str>,
    recv_opaque: bool,
    allow_cross_project: bool,
    ctx: &'a LinkCtx<'a>,
    workspace_qualifier: Option<&'a str>,
}

/// P0c-enhanced resolve function that includes workspace qualifier disambiguation.
///
/// This wraps the original `resolve` function with P0C disambiguation logic:
/// when a workspace-qualified reference results in ambiguous candidates,
/// use the stripped qualifier to prefer candidates from that specific crate.
fn resolve_p0c(
    name: &str,
    source: &NodeId,
    candidates: Option<&Vec<NodeId>>,
    _all_candidates: &BTreeMap<String, Vec<NodeId>>,
    p0c_ctx: P0cResolveCtx<'_>,
) -> (EdgeTarget, Confidence) {
    // If we have workspace qualifier information and there are multiple candidates,
    // try to disambiguate by crate
    if let (Some(crate_name), Some(cands)) = (p0c_ctx.workspace_qualifier, candidates) {
        if cands.len() > 1 {
            // We have ambiguity - try P0c disambiguation
            if let Some(disambiguated) = disambiguate_by_crate(cands, crate_name, p0c_ctx.ctx) {
                // Use the disambiguated candidates for resolution
                return resolve(
                    name,
                    source,
                    Some(&disambiguated),
                    p0c_ctx.type_hint,
                    p0c_ctx.recv_opaque,
                    p0c_ctx.allow_cross_project,
                    p0c_ctx.ctx,
                );
            } else {
                // P0c: If disambiguation returns None, no candidates from target crate.
                // This means the workspace-qualified reference doesn't exist in that crate.
                // Stay unresolved rather than picking some other crate's candidate.
                return unresolved(name);
            }
        }
    }

    // No workspace qualifier or disambiguation didn't help - use normal resolution
    resolve(
        name,
        source,
        candidates,
        p0c_ctx.type_hint,
        p0c_ctx.recv_opaque,
        p0c_ctx.allow_cross_project,
        p0c_ctx.ctx,
    )
}

/// **Scoped link stage** (ADR-0042 Phase 1.3) — re-resolve only the references
/// whose site is in the change's impact set (or *every* reference when the impact
/// set is `full`, e.g. a workspace/export-structure change). References at
/// unaffected sites are not re-resolved; their prior edges are carried by
/// [`carry_unimpacted_ref_edges`]. Because [`resolve_ref`] is shared with
/// [`resolve_all`], a re-resolved reference's edge is identical to what `Global`
/// would produce — the equivalence proof reduces to *impact-set coverage*.
fn resolve_impacted(
    refs_by_name: &BTreeMap<String, Vec<Reference>>,
    candidates: &BTreeMap<String, Vec<NodeId>>,
    ctx: &LinkCtx<'_>,
    impact: &ImpactReport,
) -> Vec<Edge> {
    let mut resolved_edges: Vec<Edge> = Vec::new();
    for (name, list) in refs_by_name {
        let cands = candidates.get(name);
        for r in list {
            if impact.site_impacted(&r.source, &r.relation) {
                // P0c: Track workspace qualifier for disambiguation
                let workspace_qualifier = strip_workspace_qualifier(name, ctx.workspace)
                    .map(|(_, crate_name)| crate_name);

                // Workspace-aware qualified path resolution (P0b + P0c):
                // If this is a workspace-qualified name with NO candidates, try resolving with the stripped version
                let result = if cands.is_none() {
                    if let Some((stripped_name, _)) = strip_workspace_qualifier(name, ctx.workspace)
                    {
                        // Only try workspace-aware resolution when the original name has no candidates
                        // Try resolving with the stripped name first (e.g., "Error" instead of "filigrio_core::Error")
                        let stripped_cands = candidates.get(&stripped_name);
                        let edge = resolve_ref(
                            &stripped_name,
                            r,
                            stripped_cands,
                            candidates,
                            true,
                            ctx,
                            workspace_qualifier.as_deref(),
                        );

                        // If we got a resolved node, use it; otherwise try the last segment
                        if let EdgeTarget::Node(_) = edge.target {
                            edge
                        } else {
                            // If the stripped name is multi-segment (e.g., "module::Type"), try resolving just the last segment
                            if stripped_name.contains("::") {
                                let last_segment =
                                    stripped_name.split("::").last().unwrap_or(&stripped_name);
                                let last_segment_cands = candidates.get(last_segment);
                                let last_edge = resolve_ref(
                                    last_segment,
                                    r,
                                    last_segment_cands,
                                    candidates,
                                    true,
                                    ctx,
                                    None, // No workspace qualifier for last segment fallback
                                );

                                if let EdgeTarget::Node(_) = last_edge.target {
                                    last_edge
                                } else {
                                    resolve_ref(name, r, cands, candidates, false, ctx, None)
                                }
                            } else {
                                resolve_ref(name, r, cands, candidates, false, ctx, None)
                            }
                        }
                    } else {
                        // Not workspace-qualified, use normal resolution
                        resolve_ref(name, r, cands, candidates, false, ctx, None)
                    }
                } else {
                    // P0c: Original name has candidates - pass workspace qualifier for disambiguation
                    resolve_ref(
                        name,
                        r,
                        cands,
                        candidates,
                        false,
                        ctx,
                        workspace_qualifier.as_deref(),
                    )
                };

                resolved_edges.push(result);
            }
        }
    }
    resolved_edges
}

/// **Prior-edge carry** (step 6) — the one pass that decides which of `prior`'s
/// edges survive into this apply. A prior edge is carried iff it is **healthy**
/// and this apply is **not about to re-emit it**:
///
/// * *healthy* = its source still exists and is not a regenerated project-overlay
///   node, and its target, if resolved, still exists. The project overlay (project
///   nodes + their `contains`/`depends_on` edges) is regenerated every apply from
///   the `Workspace`, so carrying its prior edges would duplicate it — a
///   convergence violation (incremental ≠ cold) the ADR-0032 §7 test catches.
/// * *not re-emitted*: a `(source, relation)` that is not a **reference site** is
///   never produced by the link stage, so it can only survive by being carried. A
///   reference site is re-emitted exactly when the link stage re-resolves it —
///   every site under [`LinkScope::Global`] (`impact = None`), and only the impact
///   set under `Scoped`.
///
/// **Why the `Scoped` carry equals `Global`** (ADR-0042 Phase 1.3): an unaffected
/// site's references have unchanged resolution inputs, so their current resolution
/// equals their prior resolution — which is exactly the prior edges kept here; and
/// no reference is added or removed at an unaffected site (that would touch the
/// site's source module and mark it impacted). The `heal_diverged_refs` guard runs
/// before this regardless of scope, keeping `reverse` and `graph.edges` consistent
/// so the carry is sound.
///
/// This was two passes until ADR-0042 Phase 1b item B — a structural carry and a
/// scoped reference carry — each building the whole `ref_sites` set (~300k
/// entries) and each walking all ~350k prior edges, into two `Vec<Edge>`s that
/// were then concatenated. One predicate, one pass; the result is the same **set**
/// of edges, which is all any consumer sees (clustering collapses each endpoint
/// pair to its strongest weight, and every comparator sorts).
///
/// **Returns the partition, not just the kept half** (ADR-0042 Phase 1d P1): the
/// second element is every prior edge this apply is *retiring*, borrowed from
/// `prior` (never cloned — under `Global` that is ~300 k edges). It is the left
/// operand of [`diff_edges`], and having it for free here is why the patch delta
/// costs a diff over the *retired* set rather than over the whole graph.
fn carry_prior_edges<'a>(
    prior: &'a GraphState,
    refs_by_name: &BTreeMap<String, Vec<Reference>>,
    impact: Option<&ImpactReport>,
    dropped_ids: &BTreeSet<&NodeId>,
    prior_project_node_ids: &BTreeSet<&NodeId>,
) -> (Vec<Edge>, Vec<&'a Edge>) {
    // Hashed, not ordered: this is a membership test per prior edge over ~300k
    // sites whose key is a whole node-id string, and nothing here needs the order.
    let ref_sites: HashSet<(&NodeId, &str)> = refs_by_name
        .values()
        .flatten()
        .map(|r| (&r.source, r.relation.as_str()))
        .collect();
    let mut carried: Vec<Edge> = Vec::new();
    let mut retired: Vec<&Edge> = Vec::new();
    for e in &prior.graph.edges {
        let keep = !prior_project_node_ids.contains(&e.source)
            && !dropped_ids.contains(&e.source)
            && match &e.target {
                EdgeTarget::Node(t) => !dropped_ids.contains(t),
                EdgeTarget::Symbol(_) => true,
            }
            && !(ref_sites.contains(&(&e.source, e.relation.as_str()))
                && impact.is_none_or(|i| i.site_impacted(&e.source, &e.relation)));
        if keep {
            carried.push(e.clone());
        } else {
            retired.push(e);
        }
    }
    (carried, retired)
}

/// The **minimal multiset diff** between the prior edge set and this apply's
/// result — the heart of the ADR-0042 Phase 1d patch delta.
///
/// It is computed over `retired` × `emitted` rather than `prior` × `result`, and
/// that is exact, not an approximation. Writing `result = carried ⊎ emitted` and
/// `prior = carried ⊎ retired` (the carry is a partition of `prior`, so the
/// `carried` term is literally the same multiset on both sides), the common term
/// cancels:
///
/// ```text
/// result \ prior = emitted \ retired      (the additions)
/// prior \ result = retired \ emitted      (the removals)
/// ```
///
/// Two consequences worth stating because both are load-bearing:
///
/// * **Cost tracks the scope.** Under `Scoped` almost every prior edge is
///   carried, so `retired` and `emitted` are both O(change) and this is nearly
///   free — the ADR's "falls out of the carry split". Under `Global` every
///   reference-site edge is retired and re-emitted, so this *is* the ADR's
///   one-pass set-diff over ~300 k edges, and the cancellation it performs is
///   the reason the resulting delta is small anyway.
/// * **The answer does not depend on the scope** (ADR-0042 B10). The minimal
///   diff is a property of `(prior, result)`, and the two scopes are proven to
///   produce the same `result`; the *split* between carried and re-emitted
///   differs, but everything re-emitted unchanged cancels here. That is what
///   lets the store default and the `LinkScope` default flip independently — and
///   it is asserted, not assumed: `delta_facets` compares `edges_added` and
///   `edges_removed` across scopes with no exclusions on every shadow and
///   `git_convergence` step.
///
/// Order is deterministic on both sides: additions follow the engine's emission
/// order (structural, linked, overlay), removals follow prior order.
///
/// `emitted` is taken **by value** so an addition is *moved* onto the delta
/// rather than copied — a cold build (nothing retired) hands its whole edge set
/// straight through without touching a single `Edge`, and an incremental apply
/// copies only the edges that genuinely appeared.
fn diff_edges(retired: &[&Edge], emitted: Vec<Edge>) -> (Vec<Edge>, Vec<Edge>) {
    if retired.is_empty() {
        return (emitted, Vec::new());
    }
    // How many copies of each retired edge are still unmatched.
    let mut unmatched: HashMap<&Edge, usize> = HashMap::with_capacity(retired.len());
    for e in retired {
        *unmatched.entry(*e).or_default() += 1;
    }
    let mut added: Vec<Edge> = Vec::new();
    for e in emitted {
        match unmatched.get_mut(&e) {
            // Re-emitted identically to an edge that was already there: not a
            // change at all, so it belongs in neither list.
            Some(n) if *n > 0 => *n -= 1,
            _ => added.push(e),
        }
    }
    // Whatever is still unmatched really is gone. Walk `retired` (not the map) so
    // the removal list is in prior order rather than hash order.
    let mut removed: Vec<Edge> = Vec::new();
    for e in retired {
        if let Some(n) = unmatched.get_mut(*e) {
            if *n > 0 {
                *n -= 1;
                removed.push((*e).clone());
            }
        }
    }
    (added, removed)
}

/// The concrete return type of `callee` (a free or, with `owner`, an associated
/// call) — the deferred receiver type for `let x = callee(); x.m()`. Project-scope
/// the callee's candidates like [`resolve`], narrow by `owner` when given, then
/// take the **single distinct** `returns` attr among them. Returns `None` (→ the
/// call declines) when the callee is unknown, has no declared return, or its
/// candidates disagree — precision over a guess. Single-hop: never recurses into
/// the callee's own receiver (keeps this O(candidates), not a chain walk).
fn return_type_of(
    callee: &str,
    owner: Option<&str>,
    source: &NodeId,
    candidates: &BTreeMap<String, Vec<NodeId>>,
    ctx: &LinkCtx<'_>,
) -> Option<String> {
    let ids = candidates.get(callee)?;
    let src_project = ctx.project_of.get(source).copied().flatten();
    let mut pool: Vec<&NodeId> = ids
        .iter()
        .filter(|id| ctx.project_of.get(id).copied().flatten() == src_project)
        .collect();
    // Associated call `Owner::callee` — narrow to that type's method first.
    if let Some(o) = owner {
        let want = norm_type(o);
        pool.retain(|id| {
            ctx.impl_of
                .get(id)
                .copied()
                .flatten()
                .is_some_and(|imp| norm_type(imp) == want)
        });
    }
    let mut rets: Vec<&str> = pool
        .iter()
        .filter_map(|id| {
            ctx.returns_of
                .get(id)
                .copied()
                .flatten()
                .map(String::as_str)
        })
        .collect();
    rets.sort_unstable();
    rets.dedup();
    match rets.as_slice() {
        [only] => Some(norm_type(only).to_string()),
        _ => None,
    }
}

/// Per-apply resolution context: the node-keyed lookup maps plus the per-file
/// import scope. Bundled so `resolve` stays a single-argument-group call.
struct LinkCtx<'a> {
    /// node id → its source file (path).
    file_of: &'a BTreeMap<&'a NodeId, Option<&'a String>>,
    /// node id → its `impl` owner type (methods only).
    impl_of: &'a BTreeMap<&'a NodeId, Option<&'a String>>,
    /// node id → its declared return type (`returns` attr; fns/methods only).
    returns_of: &'a BTreeMap<&'a NodeId, Option<&'a String>>,
    /// node id → its `project` root (nearest-manifest boundary; `None` = root),
    /// derived from the `Workspace` (ADR-0019).
    project_of: &'a BTreeMap<&'a NodeId, Option<&'a str>>,
    /// `(importing file, bound name) → the imported symbol's definition node`,
    /// resolved through the export graph (barrels/re-exports — ADR-0018/0020).
    import_scope: &'a BTreeMap<(String, String), NodeId>,
    /// The workspace (for workspace-aware qualified path resolution - P0b).
    workspace: &'a filigrio_core::Workspace,
}

/// Strip workspace qualifier from a qualified name if the head matches a workspace crate.
///
/// Returns Some((stripped_name, crate_name)) where:
/// - `stripped_name` is the name without the workspace qualifier (e.g., `Error` from `filigrio_core::Error`)
/// - `crate_name` is the normalized crate name that was stripped (e.g., `filigrio_core`)
///
/// Examples:
/// - `filigrio_core::Error` in workspace with `filigrio-core` crate → Some((`Error`, `filigrio_core`))
/// - `filigrio_core::module::Type` in workspace with `filigrio-core` crate → Some((`module::Types`, `filigrio_core`))
/// - `turbojpeg::PixelFormat` in workspace without `turbojpeg` crate → None
/// - `unqualified_name` → None
fn strip_workspace_qualifier(
    name: &str,
    workspace: &filigrio_core::Workspace,
) -> Option<(String, String)> {
    // Check if the name contains `::` (it's qualified)
    if !name.contains("::") {
        return None;
    }

    // Extract the head (first segment before `::`)
    let head = name.split("::").next()?;

    // Check if the head matches a workspace crate name (with dashes/underscores conversion)
    let crate_match = workspace.projects.values().find(|p| {
        if let Some(crate_name) = &p.name {
            // Normalize both names: convert underscores to dashes and vice versa
            // since crate names and package names can use either convention
            let normalized_head = head.replace('_', "-");
            let normalized_crate_name = crate_name.replace('_', "-");

            normalized_head == normalized_crate_name
        } else {
            false
        }
    });

    if let Some(_project) = crate_match {
        let crate_name = head.to_string();
        // Strip the workspace qualifier by taking everything after the first `::`
        let stripped_name = name
            .strip_prefix(&format!("{}::", head))
            .map(|s| s.to_string())?;
        Some((stripped_name, crate_name))
    } else {
        None
    }
}

/// Find the project root for a given normalized crate name.
///
/// This handles the conversion between crate names (with underscores/dashes)
/// and project roots in the workspace.
fn find_crate_project_root(
    crate_name: &str,
    workspace: &filigrio_core::Workspace,
) -> Option<String> {
    let normalized_target = crate_name.replace('_', "-");
    workspace
        .projects
        .values()
        .find(|p| {
            if let Some(name) = &p.name {
                let normalized_name = name.replace('_', "-");
                normalized_name == normalized_target
            } else {
                false
            }
        })
        .map(|p| p.root.clone())
}

/// P0c: Disambiguate candidates by crate name when we have workspace qualifier evidence.
///
/// When a workspace-qualified reference (e.g., `filigrio_core::Error`) is stripped to
/// an ambiguous base name (`Error`), use the stripped qualifier (`filigrio_core`) to
/// prefer candidates from that specific crate.
///
/// Returns:
/// - Filtered candidates if the qualifier provides disambiguating evidence
/// - None if the qualifier doesn't help (e.g., no candidates from target crate)
fn disambiguate_by_crate(
    candidates: &[NodeId],
    crate_name: &str,
    ctx: &LinkCtx<'_>,
) -> Option<Vec<NodeId>> {
    // Find the project root for the target crate
    let target_crate_root = find_crate_project_root(crate_name, ctx.workspace)?;

    // Filter candidates to only those from the target crate
    let crate_candidates: Vec<NodeId> = candidates
        .iter()
        .filter(|id| {
            ctx.project_of
                .get(id)
                .copied()
                .flatten()
                .map(|project_root| {
                    project_root.replace('\\', "/") == target_crate_root.replace('\\', "/")
                })
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    if crate_candidates.is_empty() {
        // No candidates from the target crate - the qualifier doesn't help
        None
    } else {
        // We have candidates from the target crate - use them for disambiguation
        Some(crate_candidates)
    }
}

/// Resolve one reference `name` (referenced by `source`) against its candidate
/// definitions (`ids`, pre-sorted so `[0]` is the deterministic min). See the
/// module table for the provenance rules.
///
/// **Receiver-type resolution** (the homonym fix): when the call carries a
/// receiver type (`T::method` / `x.method()` on a known-typed `x` →
/// `type_hint = Some("T")`), it names *exactly* which type's method it means.
/// The target is then **certain** — it resolves `EXTRACTED` on a single match
/// (even cross-file: the source explicitly said `T`, so nothing is guessed),
/// `AMBIGUOUS` if the same method exists on that same-named type in several
/// files, and **unresolved** if no candidate is a method on `T` (a call into a
/// type we don't define, e.g. `Vec::new` — never bound to an unrelated homonym).
///
/// **Name deduction** (no type hint): fall back to scope preference — a same-file
/// def shadows cross-file homonyms (`EXTRACTED`); otherwise the sole cross-file
/// candidate is a *deduced* link (`INFERRED`), and several are `AMBIGUOUS`.
///
/// **Preference order** (ADR-0018 §5): **import scope → same-file → same-project**.
/// An explicit `import name from "<specifier>"` resolved to a target file is the
/// strongest signal — it binds `EXTRACTED` to that file's def, *across a project
/// boundary if need be* (Increment 2). Absent an import, candidates are narrowed
/// to the source's own `project`; a reference with no same-project candidate
/// stays unresolved rather than binding across a boundary (Increment 1). A
/// single-project repo with no imports leaves both a no-op.
///
/// **Workspace-aware qualified path resolution (P0b):** If `name` is a qualified
/// path like `crate::path::Type`, check if `crate` is a workspace crate. If so,
/// strip the workspace qualifier and try to resolve the remaining `path::Type`.
/// When `allow_cross_project` is true, we skip the project scope filter to allow
/// workspace-qualified references to resolve across project boundaries.
fn resolve(
    name: &str,
    source: &NodeId,
    candidates: Option<&Vec<NodeId>>,
    type_hint: Option<&str>,
    recv_opaque: bool,
    allow_cross_project: bool,
    ctx: &LinkCtx<'_>,
) -> (EdgeTarget, Confidence) {
    // Import scope (tier 1): if the source's file imported `name`, the export
    // graph already resolved it (through any re-export barrel) to a definition
    // node — bind directly, certain (`EXTRACTED`), cross-project allowed. Checked
    // *first*, before the candidate guard: an aliased import (`import { x as y }`)
    // has no local def named `y`, yet must still resolve.
    if let Some(src_file) = ctx.file_of.get(source).copied().flatten() {
        if let Some(def) = ctx.import_scope.get(&(src_file.clone(), name.to_string())) {
            return (EdgeTarget::Node(def.clone()), Confidence::Extracted);
        }
    }

    let all_ids = match candidates {
        Some(ids) if !ids.is_empty() => ids,
        _ => {
            return unresolved(name);
        }
    };

    // Project scope: restrict to candidates in the source's own project. An empty
    // pool means the only definitions live in other projects — held unresolved
    // until an import licenses the cross-project bind (ADR-0018 §5).
    //
    // However, for workspace-aware qualified path resolution (P0b), we allow
    // cross-project resolution because the workspace qualifier itself is sufficient
    // evidence that the reference is intentional.
    let ids: Vec<NodeId> = if allow_cross_project {
        all_ids.to_vec()
    } else {
        let src_project = ctx.project_of.get(source).copied().flatten();
        let filtered: Vec<NodeId> = all_ids
            .iter()
            .filter(|id| {
                let candidate_project = ctx.project_of.get(id).copied().flatten();
                candidate_project == src_project
            })
            .cloned()
            .collect();
        filtered
    };

    if ids.is_empty() {
        return unresolved(name);
    }
    let ids = &ids;

    // Type-directed resolution: the receiver type is certain, so the target is
    // too — file location doesn't downgrade it.
    if let Some(ty) = type_hint {
        let want = norm_type(ty);
        let matched: Vec<&NodeId> = ids
            .iter()
            .filter(|id| {
                ctx.impl_of
                    .get(id)
                    .copied()
                    .flatten()
                    .is_some_and(|owner| norm_type(owner) == want)
            })
            .collect();
        return match matched.as_slice() {
            [] => unresolved(name),
            [only] => (EdgeTarget::Node((*only).clone()), Confidence::Extracted),
            [first, ..] => (EdgeTarget::Node((*first).clone()), Confidence::Ambiguous),
        };
    }

    // Name deduction: scope preference, then cross-file.
    let src_file = ctx.file_of.get(source).copied().flatten();
    let same_file: Vec<&NodeId> = ids
        .iter()
        .filter(|id| src_file.is_some() && ctx.file_of.get(*id).copied().flatten() == src_file)
        .collect();

    match (same_file.as_slice(), ids.as_slice()) {
        // exactly one same-file def → the in-file direct reference.
        ([only], _) => (EdgeTarget::Node((*only).clone()), Confidence::Extracted),
        // several same-file defs → genuinely ambiguous within the file.
        ([first, ..], _) => (EdgeTarget::Node((*first).clone()), Confidence::Ambiguous),
        // Opaque method receiver (`x.method()`, un-inferable type) with no same-file
        // def: decline the cross-file bare-name bind. `v.iter()` does not call *your*
        // `fn iter` — a wrong homonym fabricates fake god nodes and pollutes
        // clustering; an honest unresolved is better (ADR-0023). A bare free call is
        // unaffected (it genuinely may denote a fn in another file).
        ([], _) if recv_opaque => unresolved(name),
        // no same-file def, exactly one cross-file def → deduced link.
        ([], [only]) => (EdgeTarget::Node(only.clone()), Confidence::Inferred),
        // no same-file def, several cross-file defs → ambiguous.
        ([], [first, ..]) => (EdgeTarget::Node(first.clone()), Confidence::Ambiguous),
        ([], []) => unreachable!("ids is non-empty"),
    }
}

/// The base type name for hint/owner comparison: drop generics (`Foo<Bar>` →
/// `Foo`) and any path qualifier (`crate::a::S` → `S`), so a call `S::new`
/// matches an `impl crate::a::S` or `impl Foo<T>` written either way.
fn norm_type(s: &str) -> &str {
    s.split('<')
        .next()
        .unwrap_or(s)
        .rsplit("::")
        .next()
        .unwrap_or(s)
        .trim()
}

/// Pick the module-resolution backend for this apply. `oxc_resolver` is the
/// **JS/TS** tier (ADR-0018) — it knows nothing of Rust `crate::` paths — so when
/// it is selected (filesystem `Source` + `oxc` feature) it is composed behind a
/// [`RoutedResolver`] that routes `.rs` importers to the hand-rolled source tier's
/// Rust mod-path resolver instead (ADR-0021: same port, per-language adapter).
/// Absent oxc, the source tier already covers every language. Both satisfy the
/// same `ModuleResolver` port, so the import-scope step is identical.
fn build_resolver<'a>(
    source: &'a dyn Source,
    workspace: &filigrio_core::Workspace,
) -> Box<dyn ModuleResolver + 'a> {
    let source_tier = SourceModuleResolver::from_workspace(source, workspace);
    #[cfg(feature = "oxc")]
    {
        if let Some(root) = source.root() {
            return Box::new(RoutedResolver {
                js_ts: OxcResolver::new(root, workspace),
                source: source_tier,
            });
        }
    }
    Box::new(source_tier)
}

/// Language-routed resolution (ADR-0021: one `ModuleResolver` port, a
/// per-language adapter behind it). oxc is a JS/TS-*only* accelerator, so it is
/// the exception: JS/TS files route to it, and **every other** language — Rust
/// mod-paths today, Python/Go/C# as they land — is served by the hand-rolled
/// source tier, which itself branches by the importing file's language
/// (`resolve_rust`, and future `resolve_go`/`resolve_py`). A new language that
/// earns a dedicated high-fidelity backend adds one arm here; until then it
/// falls to the source tier for free.
#[cfg(feature = "oxc")]
struct RoutedResolver<'a> {
    js_ts: OxcResolver,
    source: SourceModuleResolver<'a>,
}

#[cfg(feature = "oxc")]
impl ModuleResolver for RoutedResolver<'_> {
    fn resolve(&self, importing_file: &str, specifier: &str) -> Option<String> {
        if is_js_ts(importing_file) {
            self.js_ts.resolve(importing_file, specifier)
        } else {
            self.source.resolve(importing_file, specifier)
        }
    }
}

/// JS/TS source extensions oxc handles — the only files routed to it; every
/// other language uses the source tier (ADR-0018/0021).
#[cfg(feature = "oxc")]
fn is_js_ts(file: &str) -> bool {
    const JS_TS_EXTS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];
    file.rsplit('.')
        .next()
        .is_some_and(|ext| JS_TS_EXTS.contains(&ext))
}

/// Project the `Workspace` into visible graph nodes/edges (ADR-0019): a `project`
/// node per project (label = package name, else root; no `source_file`, so it
/// carries no community), `depends_on` edges = the project dependency graph, and
/// `contains` edges project → each of its files. A pure projection of derived
/// state — regenerated every apply.
fn project_overlay(
    ws: &filigrio_core::Workspace,
    symbol_nodes: &[&Node],
) -> (Vec<Node>, Vec<Edge>) {
    let pid = |root: &str| NodeId::new(format!("project:{root}"));
    let mut nodes = Vec::new();
    for (root, p) in &ws.projects {
        let label = p.name.clone().unwrap_or_else(|| {
            if root.is_empty() {
                "<root>".to_string()
            } else {
                root.clone()
            }
        });
        let mut n = Node::new(pid(root).0, label, "project");
        n.attrs.insert("root".into(), root.clone());
        n.attrs.insert("manifest".into(), p.manifest.clone());
        if let Some(name) = &p.name {
            n.attrs.insert("name".into(), name.clone());
        }
        nodes.push(n);
    }
    let mut edges = Vec::new();
    // depends_on: the project dependency graph (deps ∩ known packages).
    for (root, deps) in ws.depends_on() {
        for dep in deps {
            edges.push(Edge {
                source: pid(root),
                relation: DEPENDS_ON.into(),
                confidence: Confidence::Extracted,
                target: EdgeTarget::Node(pid(dep)),
            });
        }
    }
    // contains: project → each file node under it (links the two levels).
    for n in symbol_nodes {
        if n.kind != "file" {
            continue;
        }
        if let Some(root) = n.source_file.as_deref().and_then(|f| ws.root_of(f)) {
            edges.push(Edge {
                source: pid(root),
                relation: CONTAINS.into(),
                confidence: Confidence::Extracted,
                target: EdgeTarget::Node(n.id.clone()),
            });
        }
    }
    (nodes, edges)
}

/// An unresolved reference stays a `Symbol` (surfaced, not dropped); the call
/// itself was extracted from source, so its provenance is `EXTRACTED`.
fn unresolved(name: &str) -> (EdgeTarget, Confidence) {
    (
        EdgeTarget::Symbol(TargetRef::new(name)),
        Confidence::Extracted,
    )
}
