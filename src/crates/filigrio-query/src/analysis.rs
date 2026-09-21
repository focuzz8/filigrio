//! Derived analysis over a built graph (Phase 2b, slice 2) — the raw material
//! for the human-facing `GRAPH_REPORT.md` and, later, for labelling retrieval
//! results. Everything here is a **pure, deterministic** function of the loaded
//! `GraphState`; nothing is persisted (clustering stays structural, labels are
//! derived on demand).

use crate::GraphView;
use filigrio_core::{CommunityId, Edge, EdgeTarget, GraphStats, Node};
use std::collections::BTreeMap;

/// One community, named by its most-central member (its "god node") rather than
/// the `Community N` placeholder the clusterer emits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommunitySummary {
    pub id: CommunityId,
    /// Label of the community's highest-degree member, tie-broken by node label
    /// ascending — read straight off the view's `community_labels`, so the
    /// report and the query surface can never name a community differently.
    /// [`crate::GraphView::build_community_labels`] defines the rule and the
    /// degree it uses.
    pub label: String,
    pub size: usize,
    /// Members, sorted by label — the readable roster.
    pub members: Vec<Node>,
    /// Cohesion permille (ADR-0024) — carried from the partition's `CommunityMeta`
    /// so the report can flag loosely-bound clusters. `u16` keeps this type `Eq`.
    pub cohesion_permille: u16,
}

impl CommunitySummary {
    /// Cohesion in `[0.0, 1.0]` (ADR-0024).
    pub fn cohesion(&self) -> f64 {
        self.cohesion_permille as f64 / 1000.0
    }
}

/// A semantic edge whose endpoints live in *different* communities — a seam
/// between clusters, and the thing a reader most wants surfaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bridge {
    pub edge: Edge,
    pub from: CommunityId,
    pub to: CommunityId,
}

/// The full report payload — composed by [`crate::render_markdown`].
#[derive(Clone, Debug)]
pub struct GraphReport {
    pub stats: GraphStats,
    /// Top-N nodes by semantic degree (degree carried alongside).
    pub god_nodes: Vec<(Node, usize)>,
    pub communities: Vec<CommunitySummary>,
    pub bridges: Vec<Bridge>,
    /// The project dependency graph — the monorepo architecture map (ADR-0019).
    pub projects: filigrio_core::ProjectGraph,
}

impl GraphView {
    /// Community id of a node by its id, if the partition assigns one.
    fn community_id_of(&self, id: &filigrio_core::NodeId) -> Option<CommunityId> {
        self.state.partition.node_community.get(id).copied()
    }

    /// Every community with its derived label + sorted membership. Communities
    /// are ordered by id.
    ///
    /// The label is **read**, not recomputed: the view already derived one per
    /// community into `community_labels` at construction, and that is the map
    /// `community_of` answers the query surface from. This used to derive its
    /// own from [`crate::GraphView::semantic_degree`] — the same tie-break but a
    /// different degree — so `GRAPH_REPORT.md` and the MCP `community=` attr
    /// could disagree about the same community's name on any graph with
    /// unresolved edges (i.e. every real one). One computation, two consumers.
    pub fn community_summaries(&self) -> Vec<CommunitySummary> {
        // Group node ids by community (ordered by community id via BTreeMap).
        let mut members: BTreeMap<CommunityId, Vec<&Node>> = BTreeMap::new();
        for n in &self.state.graph.nodes {
            if let Some(cid) = self.community_id_of(&n.id) {
                members.entry(cid).or_default().push(n);
            }
        }

        members
            .into_iter()
            .map(|(id, mut ms)| {
                ms.sort_by(|a, b| a.label.cmp(&b.label));
                // Every community reached here has at least one member placed by
                // the partition, which is exactly the condition under which
                // `build_community_labels` records an entry — so the fallback is
                // unreachable, and kept only so this stays total.
                let label = self.community_labels.get(&id).cloned().unwrap_or_default();
                let cohesion_permille = self
                    .state
                    .partition
                    .communities
                    .get(&id)
                    .map(|m| m.cohesion_permille)
                    .unwrap_or(0);
                CommunitySummary {
                    id,
                    label,
                    size: ms.len(),
                    members: ms.into_iter().cloned().collect(),
                    cohesion_permille,
                }
            })
            .collect()
    }

    /// The semantic edges whose endpoints fall in different communities, sorted
    /// deterministically by `(from, to, source, target)`. Structural
    /// `contains`/`imports` edges are excluded — a bridge is real coupling.
    pub fn bridges(&self) -> Vec<Bridge> {
        let mut out: Vec<Bridge> = Vec::new();
        for e in &self.state.graph.edges {
            let EdgeTarget::Node(t) = &e.target else {
                continue;
            };
            if filigrio_core::relation::is_structural(&e.relation) {
                continue;
            }
            let (Some(from), Some(to)) = (self.community_id_of(&e.source), self.community_id_of(t))
            else {
                continue;
            };
            if from != to {
                out.push(Bridge {
                    edge: e.clone(),
                    from,
                    to,
                });
            }
        }
        out.sort_by(|a, b| {
            (a.from, a.to, &a.edge.source, target_id(&a.edge)).cmp(&(
                b.from,
                b.to,
                &b.edge.source,
                target_id(&b.edge),
            ))
        });
        out
    }

    /// Assemble the full report: stats, top-`top_gods` god nodes, community
    /// summaries and cross-community bridges.
    pub fn report(&self, top_gods: usize) -> filigrio_core::Result<GraphReport> {
        use filigrio_core::GraphQuery;
        Ok(GraphReport {
            stats: self.stats()?,
            god_nodes: self.god_nodes(top_gods)?,
            communities: self.community_summaries(),
            bridges: self.bridges(),
            projects: self.project_graph()?,
        })
    }
}

/// The resolved target id of an edge (bridges only ever hold resolved edges).
fn target_id(e: &Edge) -> &filigrio_core::NodeId {
    match &e.target {
        EdgeTarget::Node(t) => t,
        // Unreachable for bridges; give a stable ordering key regardless.
        EdgeTarget::Symbol(_) => &e.source,
    }
}
