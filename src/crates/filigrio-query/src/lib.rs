//! filigrio-query — the `GraphQuery` port (HLD §4/§6, ADR-0006).
//!
//! `GraphView` owns a loaded `GraphState` and builds a **petgraph `DiGraph`
//! index** over its resolved edges at construction (HLD §5.2 — "query builds
//! indices"). Traversal, degree, and shortest-path run against that index; the
//! core node/edge lists remain the source of truth (and stay `graph.json`
//! friendly). Seed selection ranks labels by IDF-weighted character-trigram
//! overlap (HLD §6, see `retrieval.rs`). Transports call the trait, never this type.
//!
//! # Cost, and who pays it
//!
//! Construction is O(nodes + edges) and every method here is a *read* of a
//! `GraphState` that never changes — so a view is a pure function of a state and
//! is meant to be **built once per state and shared**, not once per request
//! (the daemon's LRU holds one alongside each resident state; audit §L1).
//! Both halves are instrumented with [`filigrio_core::profile`] — recording is
//! off unless a caller wraps the call in `profile::capture`, so an unprofiled
//! query is the same query. `docs/perf/benchmarks.md` §6 is the ledger those
//! stages feed, and `tests/read_profile.rs` is the harness that produces it.

#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]
use filigrio_core::attrs;
use filigrio_core::profile::stage;
use filigrio_core::relation::{is_structural, relation_matches_filter};
use filigrio_core::{
    CommunityId, Direction, Edge, EdgeTarget, GraphQuery, GraphState, GraphStats, Node, NodeId,
    QueryOpts, Result, Subgraph, TraversalMode,
};
use petgraph::algo::astar;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use petgraph::Direction::{Incoming, Outgoing};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

mod analysis;
mod report;
mod retrieval;
pub use analysis::{Bridge, CommunitySummary, GraphReport};
pub use report::render_markdown;

pub struct GraphView {
    state: Arc<GraphState>,
    /// node weight = index into `state.graph.nodes`; edge weight = index into
    /// `state.graph.edges`. Only *resolved* (`EdgeTarget::Node`) edges are added.
    g: DiGraph<usize, usize>,
    idx_of: HashMap<NodeId, NodeIndex>,
    /// Derived label **per community**, not per node. The label is a pure
    /// function of the community, and `state.partition.node_community` already
    /// maps node → community, so a node's label is two lookups
    /// ([`GraphView::community_of`]) rather than a stored clone. Keyed by
    /// `CommunityId` in a `BTreeMap` so iteration is id-ordered.
    ///
    /// **The only place a community label is computed.** Both label surfaces
    /// read this map: the query side through [`GraphView::community_of`]
    /// (the `community=` attr, `Subgraph::communities`, `get_community`) and
    /// the report side through [`GraphView::community_summaries`]. They used to
    /// each derive their own, over *different* degree definitions, and so could
    /// name the same community differently — see
    /// [`GraphView::build_community_labels`] for the rule and why it is the one
    /// that survived.
    ///
    /// This is deliberately *not* a `HashMap<NodeId, String>`. That shape held
    /// one cloned `String` per node — 100 000 clones and inserts for the ~10²–10³
    /// distinct labels an actual partition has (ADR-0024 saw 155–655 communities)
    /// — and was 74 % of construction time and ~40 % of the view's heap
    /// (audit §A2, ledger §6). Nothing is mutated on the shared `GraphState`
    /// either way; the community context MCP responses carry is stamped at read
    /// time by [`GraphView::stamp_community`].
    community_labels: BTreeMap<CommunityId, String>,
}

impl GraphView {
    pub fn new(state: Arc<GraphState>) -> Self {
        stage("view_new", || {
            let (g, idx_of) = stage("vn.index", || {
                let mut g = DiGraph::new();
                let mut idx_of = HashMap::new();
                for (i, n) in state.graph.nodes.iter().enumerate() {
                    idx_of.insert(n.id.clone(), g.add_node(i));
                }
                for (ei, e) in state.graph.edges.iter().enumerate() {
                    if let EdgeTarget::Node(target) = &e.target {
                        if let (Some(&s), Some(&d)) = (idx_of.get(&e.source), idx_of.get(target)) {
                            g.add_edge(s, d, ei);
                        }
                    }
                }
                (g, idx_of)
            });

            // Build community labels without mutating shared state
            let community_labels = stage("vn.labels", || Self::build_community_labels(&state));

            GraphView {
                state,
                g,
                idx_of,
                community_labels,
            }
        })
    }

    /// The state this view indexes. The view is a pure function of it, so a
    /// caller holding the view never needs a second cache lookup to read the
    /// manifest or the workspace off the same snapshot.
    pub fn state(&self) -> &Arc<GraphState> {
        &self.state
    }

    /// Derive each community's label — its highest-degree member, ties broken by
    /// label ascending — without mutating shared state. **The single
    /// implementation of that rule**; [`GraphView::community_summaries`] reads
    /// the map this produces rather than re-deriving it.
    ///
    /// # Which degree, and why it is *not* [`GraphView::semantic_degree`]
    ///
    /// Degree here is an **edge-list walk**: every non-structural edge gives +1
    /// to its source and +1 to its target *when the target resolved*. So an
    /// **unresolved** out-edge (`EdgeTarget::Symbol` — a call whose callee we
    /// could not bind, ADR-0029) still counts toward its caller.
    /// [`GraphView::semantic_degree`] walks the petgraph, which holds resolved
    /// edges only, so it does not.
    ///
    /// That difference is deliberate, and it is the anomaly rather than the
    /// convention — [`GraphQuery::god_nodes`], the user-facing "most connected
    /// symbols" tool, *does* use `semantic_degree`. Labels diverge because
    /// unresolved edges are 36–63 % of every corpus measured (ADR-0042 P2), and
    /// a community whose members mostly call outward into std/external code has
    /// **every member at resolved degree 0**. The label then collapses to the
    /// tie-break alone — "alphabetically first member" — which is not a name.
    /// Measured over three real corpora, resolved-only degree roughly doubles
    /// the communities in that state (this repo 33→54 of 338; ironclaw
    /// 768→1567 of 10 077; langchain 554→1614 of 3975) and produces labels like
    /// `libs/core/langchain_core/output_parsers/pydantic.py`, `Other` and
    /// `default` where the walk names `render_text_description_and_args`,
    /// `SendError` and `OneShotConfig`.
    ///
    /// A *label* wants "the member that does the most", including work that
    /// leaves the graph. A *god node* ranking wants measured connectivity
    /// inside the graph. Two questions, two degrees — on purpose. Do not unify
    /// them by pointing this at `semantic_degree`; that trade was measured and
    /// declined.
    fn build_community_labels(state: &Arc<GraphState>) -> BTreeMap<CommunityId, String> {
        if state.partition.node_community.is_empty() {
            return BTreeMap::new();
        }

        // Degree per node. Keys borrow the ids out of the state rather than
        // cloning one per edge endpoint — the map dies at the end of this
        // function, so it never needs to own them.
        let mut node_edges: HashMap<&NodeId, usize> = HashMap::new();
        for e in &state.graph.edges {
            if filigrio_core::relation::is_structural(&e.relation) {
                continue;
            }

            *node_edges.entry(&e.source).or_insert(0) += 1;

            if let EdgeTarget::Node(target) = &e.target {
                *node_edges.entry(target).or_insert(0) += 1;
            }
        }

        // One pass, keeping the running best member per community: highest
        // degree, ties broken by label ascending. Equivalent to
        // collecting each community's members and sorting on that key — the same
        // rule, without the per-node `Vec` of cloned `(NodeId, String)` (of which
        // the id was never read) or the sort over it.
        let mut best: BTreeMap<CommunityId, (usize, &str)> = BTreeMap::new();
        for n in &state.graph.nodes {
            if let Some(&cid) = state.partition.node_community.get(&n.id) {
                let candidate = (
                    node_edges.get(&n.id).copied().unwrap_or(0),
                    n.label.as_str(),
                );
                match best.entry(cid) {
                    std::collections::btree_map::Entry::Vacant(slot) => {
                        slot.insert(candidate);
                    }
                    std::collections::btree_map::Entry::Occupied(mut slot) => {
                        let (deg, label) = *slot.get();
                        if candidate.0 > deg || (candidate.0 == deg && candidate.1 < label) {
                            slot.insert(candidate);
                        }
                    }
                }
            }
        }

        // The only clone left: one label per community.
        best.into_iter()
            .map(|(cid, (_, label))| (cid, label.to_string()))
            .collect()
    }

    /// Get the community label for a node, if available.
    ///
    /// Two lookups — node → [`CommunityId`] through the partition the state
    /// already carries, then community → label — in place of the per-node clone
    /// the view used to store. A node the partition does not place has **no**
    /// label (not a default one), exactly as the per-node map had no entry for it.
    pub fn community_of(&self, node_id: &NodeId) -> Option<&String> {
        let cid = self.state.partition.node_community.get(node_id)?;
        self.community_labels.get(cid)
    }

    /// Stamp a node's community label into `attrs["community"]`, if it has one.
    ///
    /// The MCP boundary reads community from there (`format_node_response`), so
    /// this attr is observable output, not a scratch field.
    fn stamp_community(&self, node: &mut Node) {
        if let Some(community) = self.community_of(&node.id) {
            node.attrs
                .insert(attrs::COMMUNITY.into(), community.clone());
        }
    }

    /// The `Subgraph::communities` sidecar for a set of returned nodes: each
    /// node's community label, keyed by node id.
    ///
    /// Scoped to the nodes handed back, which is what that field documents
    /// itself as ("each *returned* node's community label"). It used to be the
    /// whole graph's node→label map — a full copy per query, of which every
    /// consumer reads only the rows matching `Subgraph::nodes`.
    fn communities_of(&self, nodes: &[Node]) -> BTreeMap<NodeId, String> {
        nodes
            .iter()
            .filter_map(|n| self.community_of(&n.id).map(|l| (n.id.clone(), l.clone())))
            .collect()
    }

    /// Get a node with community annotation applied (for MCP boundary).
    /// Returns a clone of the node with community label added to attrs if available.
    pub fn node_with_community(&self, ix: NodeIndex) -> Node {
        let mut node = self.node_at(ix).clone();
        self.stamp_community(&mut node);
        node
    }

    fn index_of_id(&self, id: &str) -> Option<NodeIndex> {
        self.idx_of.get(&NodeId::new(id)).copied()
    }

    /// Resolve a node id through the index this view already built.
    ///
    /// [`filigrio_core::Graph::node_by_id`] is `nodes.iter().find(..)` — O(N) —
    /// and three query methods used to call it, two of them **once per result
    /// row** (audit §A1: 10× the nodes → 92× the time for [`Self::community`],
    /// while [`Self::neighbors_by_id`], which goes through `idx_of`, is flat).
    /// The petgraph node weight *is* the index into `state.graph.nodes`, so
    /// `idx_of` answers the same question in O(1).
    ///
    /// **Equivalent, not merely similar:** node ids are unique in any merged
    /// state (`filigrio_store::merge` dedups additions by id), so "the first
    /// match a scan finds" and "the indexed match" are the same node.
    fn node_by_id_indexed(&self, id: &NodeId) -> Option<&Node> {
        self.idx_of.get(id).map(|&ix| self.node_at(ix))
    }

    /// Neighbors of a node index in the requested direction, filtered by a set of
    /// relation entries (**OR'd**; empty = no filter). `Out` yields callees (other
    /// end = edge target), `In` yields callers (other end = edge source), `Both`
    /// yields both.
    ///
    /// Matching is [`relation_matches_filter`], not `==`: an entry selects a
    /// relation exactly *or* as a `/`-separated family prefix, so `["type"]`
    /// selects every `type/…` member including ones added later (ADR-0036 R2.1).
    /// This call site used to hand-roll `==`, which made the family spelling match
    /// nothing at all and forced the MCP surface to withhold it.
    fn neighbors_at(
        &self,
        ix: NodeIndex,
        relations: &[String],
        direction: Direction,
    ) -> Vec<(Edge, Node)> {
        let matches = |ei: usize| {
            relations.is_empty()
                || relations
                    .iter()
                    .any(|entry| relation_matches_filter(entry, &self.edge_at(ei).relation))
        };
        let mut out = Vec::new();
        if matches!(direction, Direction::Out | Direction::Both) {
            for er in self.g.edges_directed(ix, Outgoing) {
                if matches(*er.weight()) {
                    out.push((
                        self.edge_at(*er.weight()).clone(),
                        self.node_with_community(er.target()),
                    ));
                }
            }
        }
        if matches!(direction, Direction::In | Direction::Both) {
            for er in self.g.edges_directed(ix, Incoming) {
                if matches(*er.weight()) {
                    out.push((
                        self.edge_at(*er.weight()).clone(),
                        self.node_with_community(er.source()),
                    ));
                }
            }
        }
        out
    }

    /// Degree counting *semantic* edges only (calls/uses…), both directions —
    /// the "importance" signal behind [`GraphQuery::god_nodes`]. Structural
    /// `contains`/`imports` are excluded so files never dominate.
    ///
    /// This walks the petgraph, so it counts **resolved edges only**: a call
    /// whose callee never bound to a node contributes nothing. That is the right
    /// answer for "most connected symbol" — it is a claim about measured
    /// connectivity — and the wrong one for naming a community, which is why
    /// [`GraphView::build_community_labels`] uses a different degree. See its
    /// doc for the measurement behind that split.
    fn semantic_degree(&self, ix: NodeIndex) -> usize {
        self.g
            .edges_directed(ix, Outgoing)
            .chain(self.g.edges_directed(ix, Incoming))
            .filter(|er| !is_structural(&self.edge_at(*er.weight()).relation))
            .count()
    }

    fn node_at(&self, ix: NodeIndex) -> &Node {
        &self.state.graph.nodes[self.g[ix]]
    }

    fn edge_at(&self, ei: usize) -> &Edge {
        &self.state.graph.edges[ei]
    }
}

impl GraphQuery for GraphView {
    fn query(&self, q: &str, opts: QueryOpts) -> Result<Subgraph> {
        stage("query", || {
            // Seeds are ranked by IDF+trigram relevance (HLD §6); packing them in
            // priority order means a tight budget keeps the most relevant region.
            let seeds: Vec<NodeIndex> = stage("q.seeds", || {
                self.seed_scores(q)
                    .into_iter()
                    .filter_map(|(n, _)| self.idx_of.get(&n.id).copied())
                    .collect()
            });

            let mut visited: Vec<NodeIndex> = Vec::new();
            let mut queue: VecDeque<(NodeIndex, usize)> =
                seeds.into_iter().map(|s| (s, 0)).collect();
            let mut edges: Vec<Edge> = Vec::new();

            while let Some((ix, depth)) = pop(&mut queue, opts.mode) {
                if visited.contains(&ix) || visited.len() >= opts.budget {
                    continue;
                }
                visited.push(ix);
                if depth >= opts.depth {
                    continue;
                }
                for er in self.g.edges_directed(ix, Outgoing) {
                    let edge = self.edge_at(*er.weight());
                    if let Some(filter) = &opts.context_filter {
                        // Entries match exactly *or* as a family prefix, so
                        // `relations=["type"]` selects the whole `type/…` family
                        // including members added later (ADR-0036 R2.1).
                        if !filter
                            .iter()
                            .any(|r| relation_matches_filter(r, &edge.relation))
                        {
                            continue;
                        }
                    }
                    edges.push(edge.clone());
                    queue.push_back((er.target(), depth + 1));
                }
            }

            let nodes: Vec<Node> = visited
                .iter()
                .map(|&ix| self.node_with_community(ix))
                .collect();
            let communities = stage("q.labels_map", || self.communities_of(&nodes));
            Ok(Subgraph {
                nodes,
                edges,
                communities,
            })
        })
    }

    fn nodes_by_label(&self, label: &str) -> Result<Vec<Node>> {
        stage("nodes_by_label", || {
            let nodes: Vec<Node> = self
                .state
                .graph
                .nodes
                .iter()
                .filter(|n| n.label == label)
                .map(|n| {
                    let mut node = n.clone();
                    self.stamp_community(&mut node);
                    node
                })
                .collect();
            Ok(nodes)
        })
    }

    fn node_by_id(&self, id: &str) -> Result<Option<Node>> {
        stage("node_by_id", || {
            let node_id = NodeId::new(id);
            let node = self.node_by_id_indexed(&node_id).cloned();

            if let Some(mut n) = node {
                self.stamp_community(&mut n);
                Ok(Some(n))
            } else {
                Ok(None)
            }
        })
    }

    fn neighbors_by_id(
        &self,
        id: &str,
        relations: &[String],
        direction: Direction,
    ) -> Result<Vec<(Edge, Node)>> {
        stage("neighbors", || {
            let Some(&ix) = self.idx_of.get(&NodeId::new(id)) else {
                return Ok(Vec::new());
            };
            Ok(self.neighbors_at(ix, relations, direction))
        })
    }

    fn unresolved_out_by_id(&self, id: &str) -> Result<Vec<Edge>> {
        stage("unresolved_out", || {
            // A parallel read over the raw edge list — the petgraph holds resolved
            // edges only, so unresolved `Symbol` targets never entered it (ADR-0029).
            let sid = NodeId::new(id);
            Ok(self
                .state
                .graph
                .edges
                .iter()
                .filter(|e| e.source == sid && matches!(e.target, EdgeTarget::Symbol(_)))
                .cloned()
                .collect())
        })
    }

    fn unresolved_in_by_label(&self, label: &str) -> Result<Vec<(Edge, Node)>> {
        stage("unresolved_in", || {
            // Reverse-by-name: every unresolved edge whose callee name == `label`. The
            // Node handed back is the *caller* (edge source), an addressable node — the
            // by-name match itself is the heuristic the transport must mark (ADR-0029).
            Ok(self
                .state
                .graph
                .edges
                .iter()
                .filter_map(|e| match &e.target {
                    // `node_by_id_indexed`, not a scan: this runs once per matching
                    // edge, and the labels it is called on are precisely the popular
                    // homonyms where `matches` is largest (§A1).
                    EdgeTarget::Symbol(r) if r.name == label => self
                        .node_by_id_indexed(&e.source)
                        .map(|caller| (e.clone(), caller.clone())),
                    _ => None,
                })
                .collect())
        })
    }

    fn community(&self, id: CommunityId) -> Result<Vec<Node>> {
        stage("community", || {
            let nodes: Vec<Node> = self
                .state
                .partition
                .node_community
                .iter()
                .filter(|(_, c)| **c == id)
                .filter_map(|(nid, _)| {
                    // Once per community member — the scan this replaced is what made
                    // `get_community` quadratic on a large community (§A1).
                    let mut node = self.node_by_id_indexed(nid).cloned()?;
                    self.stamp_community(&mut node);
                    Some(node)
                })
                .collect();
            Ok(nodes)
        })
    }

    fn community_meta(&self, id: CommunityId) -> Result<Option<filigrio_core::CommunityMeta>> {
        Ok(self.state.partition.communities.get(&id).cloned())
    }

    fn god_nodes(&self, top_n: usize) -> Result<Vec<(Node, usize)>> {
        stage("god_nodes", || {
            let mut scored: Vec<(Node, usize)> = self
                .g
                .node_indices()
                .map(|ix| (self.node_at(ix).clone(), self.semantic_degree(ix)))
                .collect();
            // Deterministic: degree desc, then label asc (HLD §8).
            scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.label.cmp(&b.0.label)));
            scored.truncate(top_n);
            Ok(scored)
        })
    }

    fn shortest_path(
        &self,
        from_id: &str,
        to_id: &str,
        max_hops: usize,
    ) -> Result<Option<Vec<Node>>> {
        stage("shortest_path", || {
            let (Some(s), Some(d)) = (self.index_of_id(from_id), self.index_of_id(to_id)) else {
                return Ok(None);
            };
            // Unit edge cost → cost == hop count; A* with a zero heuristic == BFS.
            match astar(&self.g, s, |n| n == d, |_| 1usize, |_| 0usize) {
                Some((cost, path)) if cost <= max_hops => Ok(Some(
                    path.into_iter()
                        .map(|ix| self.node_at(ix).clone())
                        .collect(),
                )),
                _ => Ok(None),
            }
        })
    }

    fn stats(&self) -> Result<GraphStats> {
        stage("stats", || {
            let mut by_confidence: BTreeMap<String, usize> = BTreeMap::new();
            for e in &self.state.graph.edges {
                *by_confidence
                    .entry(format!("{:?}", e.confidence))
                    .or_default() += 1;
            }
            Ok(GraphStats {
                nodes: self.state.graph.nodes.len(),
                edges: self.state.graph.edges.len(),
                communities: self.state.partition.communities.len(),
                by_confidence,
            })
        })
    }

    fn project_graph(&self) -> Result<filigrio_core::ProjectGraph> {
        stage("project_graph", || {
            let ws = &self.state.workspace;
            let deps = ws.depends_on();
            // File count per project = `file` nodes whose nearest project is this root.
            let mut files: BTreeMap<&str, usize> = BTreeMap::new();
            for n in &self.state.graph.nodes {
                if n.kind == "file" {
                    if let Some(root) = n.source_file.as_deref().and_then(|f| ws.root_of(f)) {
                        *files.entry(root).or_default() += 1;
                    }
                }
            }
            let projects = ws
                .projects
                .values()
                .map(|p| filigrio_core::ProjectNode {
                    root: p.root.clone(),
                    name: p.name.clone(),
                    manifest: p.manifest.clone(),
                    files: files.get(p.root.as_str()).copied().unwrap_or(0),
                    depends_on: deps
                        .get(p.root.as_str())
                        .map(|s| s.iter().map(|r| r.to_string()).collect())
                        .unwrap_or_default(),
                })
                .collect();
            Ok(filigrio_core::ProjectGraph { projects })
        })
    }
}

fn pop(
    queue: &mut VecDeque<(NodeIndex, usize)>,
    mode: TraversalMode,
) -> Option<(NodeIndex, usize)> {
    match mode {
        TraversalMode::Bfs => queue.pop_front(),
        TraversalMode::Dfs => queue.pop_back(),
    }
}
