//! Modularity-based clustering with community-id stability (Phase 2b,
//! HLD §11.1–11.2, ADR-0001) — now **switchable** (ADR-0024).
//!
//! The clusterer is a deterministic Louvain over the undirected projection of the
//! resolved graph, in three parts common to both strategies:
//!
//!   1. **Modularity optimization** — a local-moving pass (`local_move`) with a
//!      `resolution` knob. Communities follow call/containment structure, not file
//!      paths.
//!   2. **Warm start** — the initial assignment is seeded from the prior
//!      `Partition` (new nodes start as singletons), so a small change refines the
//!      existing clustering instead of recomputing it. Seeded from an empty prior
//!      it degenerates to the cold build.
//!   3. **Community-id stability** — final communities inherit a prior
//!      `CommunityId` by **maximum node overlap** (HLD §11.2); genuinely new
//!      communities get the smallest unused id. A one-file edit therefore does not
//!      renumber unrelated communities.
//!
//! **Two strategies (ADR-0024), both keeping warm-start + id stability:**
//!   * [`ClusterStrategy::Simple`] — one local-moving pass (single-level). Fast,
//!     the streaming default.
//!   * [`ClusterStrategy::Full`] — **multi-level** Louvain: iterate `local_move` +
//!     graph **aggregation** (communities become super-nodes) until the community
//!     count stops shrinking. Higher modularity on hierarchical graphs; still
//!     incrementally id-stable — one better than the Python oracle, which
//!     re-indexes by size.
//!
//! Two cross-cutting knobs feed **both** strategies (ADR-0024):
//!   * [`EdgeWeighting`] — `Uniform` (each collapsed pair weighs 1.0; the default,
//!     preserving prior behavior) or `Confidence` (weigh by the pair's strongest
//!     edge: `EXTRACTED 1.0 / INFERRED 0.5 / AMBIGUOUS 0.25` — the clustering half
//!     of ADR-0023, so a weak homonym bridge no longer couples two groups).
//!   * **Cohesion** — every community is scored `internal / (internal + boundary)`
//!     over the weighted graph and the value rides on `CommunityMeta` (permille).
//!     An honest quality signal, reported — never a split trigger.

use filigrio_core::{
    CommunityId, CommunityMeta, Confidence, Edge, EdgeTarget, Error, Node, NodeId, Partition,
    Result,
};
use std::collections::{BTreeMap, HashMap};

const EPS: f64 = 1e-12;

/// Which Louvain regime to run (ADR-0024).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ClusterStrategy {
    /// Single-level local-moving pass. Fast, incremental; the default.
    #[default]
    Simple,
    /// Multi-level Louvain (local-move + aggregation to convergence).
    Full,
}

/// How an edge contributes weight to the undirected clustering projection
/// (ADR-0024).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EdgeWeighting {
    /// Every collapsed endpoint pair weighs 1.0 (prior behavior; the default).
    #[default]
    Uniform,
    /// Weigh each pair by its strongest edge's confidence — down-weights
    /// `AMBIGUOUS` homonym bridges (the clustering half of ADR-0023).
    Confidence,
}

/// Clustering configuration (ADR-0024). `default()` = `Simple` / `Uniform` /
/// `1.0` — bit-for-bit the pre-0024 behavior.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClusterConfig {
    pub strategy: ClusterStrategy,
    pub weighting: EdgeWeighting,
    /// Modularity resolution: `>1.0` → more/smaller communities, `<1.0` → fewer.
    pub resolution: f64,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        ClusterConfig {
            strategy: ClusterStrategy::Simple,
            weighting: EdgeWeighting::Uniform,
            resolution: 1.0,
        }
    }
}

impl ClusterConfig {
    /// The default single-level, uniform-weight config.
    pub fn simple() -> Self {
        ClusterConfig::default()
    }

    /// Multi-level Louvain, uniform weight, resolution 1.0.
    pub fn full() -> Self {
        ClusterConfig {
            strategy: ClusterStrategy::Full,
            ..ClusterConfig::default()
        }
    }
}

/// Weight an edge contributes under a given scheme.
fn edge_weight(scheme: EdgeWeighting, c: Confidence) -> f64 {
    match scheme {
        EdgeWeighting::Uniform => 1.0,
        EdgeWeighting::Confidence => match c {
            Confidence::Extracted => 1.0,
            Confidence::Inferred => 0.5,
            Confidence::Ambiguous => 0.25,
        },
    }
}

/// Cluster `nodes` over the resolved `edges`, warm-started from `prior`, using the
/// **default** config (Simple / Uniform / 1.0). Kept for callers that don't tune
/// clustering; equivalent to [`cluster_with`] with [`ClusterConfig::default`].
pub fn cluster<N: std::borrow::Borrow<Node>>(
    nodes: &[N],
    edges: &[Edge],
    prior: &Partition,
) -> Result<Partition> {
    cluster_with(nodes, edges, prior, &ClusterConfig::default())
}

/// Cluster `nodes` over the resolved `edges`, warm-started from `prior`, under
/// `cfg` (ADR-0024). Generic over owned (`&[Node]`) and borrowed (`&[&Node]`)
/// node slices so the Engine's hot path avoids materializing a cloned node set.
pub fn cluster_with<N: std::borrow::Borrow<Node>>(
    nodes: &[N],
    edges: &[Edge],
    prior: &Partition,
    cfg: &ClusterConfig,
) -> Result<Partition> {
    let nodes: Vec<&Node> = nodes.iter().map(std::borrow::Borrow::borrow).collect();
    let n = nodes.len();
    if n == 0 {
        return Ok(Partition::default());
    }
    let index: HashMap<&NodeId, usize> =
        nodes.iter().enumerate().map(|(i, x)| (&x.id, i)).collect();

    // Undirected weighted adjacency over resolved edges. Direction is a real fact
    // for queries, but clustering is undirected — so collapse each unordered
    // endpoint pair to a single weight: the pair's **strongest** edge (so a
    // reciprocal `a↔b` or parallel edges don't double coupling, and — under
    // `Confidence` — an `EXTRACTED` call outweighs a coincident `AMBIGUOUS` one).
    // Self-loops are skipped.
    let mut pairw: BTreeMap<(usize, usize), f64> = BTreeMap::new();
    for e in edges {
        let EdgeTarget::Node(t) = &e.target else {
            continue;
        };
        let (Some(&u), Some(&v)) = (index.get(&e.source), index.get(t)) else {
            continue;
        };
        if u != v {
            let w = edge_weight(cfg.weighting, e.confidence);
            let slot = pairw.entry((u.min(v), u.max(v))).or_insert(0.0);
            *slot = slot.max(w);
        }
    }
    let mut adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut deg: Vec<f64> = vec![0.0; n];
    for (&(u, v), &w) in &pairw {
        adj[u].push((v, w));
        adj[v].push((u, w));
        deg[u] += w;
        deg[v] += w;
    }
    let two_m: f64 = deg.iter().sum();

    // Seed the initial community labels from the prior partition; nodes without a
    // prior community (and the no-edge case) start as singletons.
    let mut label: Vec<usize> = vec![0; n];
    let mut prior_label: HashMap<CommunityId, usize> = HashMap::new();
    let mut next_label = 0usize;
    for (i, node) in nodes.iter().enumerate() {
        match prior.node_community.get(&node.id) {
            Some(cid) => {
                let l = *prior_label.entry(*cid).or_insert_with(|| {
                    let l = next_label;
                    next_label += 1;
                    l
                });
                label[i] = l;
            }
            None => {
                label[i] = next_label;
                next_label += 1;
            }
        }
    }

    if two_m > 0.0 {
        match cfg.strategy {
            ClusterStrategy::Simple => local_move(&adj, &deg, two_m, cfg.resolution, &mut label)?,
            ClusterStrategy::Full => {
                label = louvain_full(&adj, &deg, two_m, cfg.resolution, label)?
            }
        }
    }

    // Group node indices by their final label (deterministic: groups ordered by
    // smallest member index).
    let mut by_label: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, &l) in label.iter().enumerate() {
        by_label.entry(l).or_default().push(i);
    }
    let mut groups: Vec<Vec<usize>> = by_label.into_values().collect();
    for g in &mut groups {
        g.sort_unstable();
    }
    groups.sort_by_key(|g| g[0]);

    // Cohesion per community (ADR-0024), over the same weighted projection.
    let cohesion = cohesion_permille(&pairw, &groups, n);

    stabilize(&nodes, &groups, prior, &cohesion)
}

/// A clustering-internal bookkeeping invariant didn't hold — a bug in this
/// module, not a data problem, so surfaced as a typed error rather than a
/// panic (ADR-0041 Class C): a future refactor of `local_move`/`stabilize`
/// that breaks the invariant now fails one `apply`, not the whole process.
fn poisoned_bookkeeping(what: &str, key: usize) -> Error {
    Error::Other(format!(
        "clustering invariant violated: {what} has no entry for {key}"
    ))
}

/// One Louvain local-moving pass to modularity convergence. Deterministic: nodes
/// are visited in index order, candidate communities in id order, and a node
/// moves only on a **strict** gain — so there is no tie-driven thrashing.
/// `resolution` scales the null-model penalty (ADR-0024); `1.0` = classic
/// modularity.
fn local_move(
    adj: &[Vec<(usize, f64)>],
    deg: &[f64],
    two_m: f64,
    resolution: f64,
    label: &mut [usize],
) -> Result<()> {
    let n = adj.len();
    // sum_tot[c] = total degree of nodes currently in community c.
    let mut sum_tot: HashMap<usize, f64> = HashMap::new();
    for i in 0..n {
        *sum_tot.entry(label[i]).or_default() += deg[i];
    }

    let mut improved = true;
    while improved {
        improved = false;
        for i in 0..n {
            let ci = label[i];
            *sum_tot
                .get_mut(&ci)
                .ok_or_else(|| poisoned_bookkeeping("sum_tot", ci))? -= deg[i];

            // Weight from i into each neighbouring community.
            let mut wt: BTreeMap<usize, f64> = BTreeMap::new();
            for &(j, w) in &adj[i] {
                *wt.entry(label[j]).or_default() += w;
            }
            wt.entry(ci).or_default();

            let gain = |c: usize| -> f64 {
                wt.get(&c).copied().unwrap_or(0.0)
                    - resolution * sum_tot.get(&c).copied().unwrap_or(0.0) * deg[i] / two_m
            };
            let base = gain(ci);
            let mut best = ci;
            let mut best_gain = base;
            for &c in wt.keys() {
                let g = gain(c);
                if g > best_gain + EPS {
                    best_gain = g;
                    best = c;
                }
            }

            *sum_tot.entry(best).or_default() += deg[i];
            if best != ci {
                label[i] = best;
                improved = true;
            }
        }
    }
    Ok(())
}

/// Multi-level Louvain (ADR-0024). Level 0 runs `local_move` warm-started from
/// `seed`; then repeatedly **aggregate** the graph (each community → one
/// super-node; inter-community weights sum; total degree preserved, so `two_m` is
/// invariant) and re-run `local_move` on the super-graph, until a level yields no
/// further merges. Returns the final community label per original node.
fn louvain_full(
    adj0: &[Vec<(usize, f64)>],
    deg0: &[f64],
    two_m: f64,
    resolution: f64,
    seed: Vec<usize>,
) -> Result<Vec<usize>> {
    let n = adj0.len();

    // Level 0: warm-started local-move over the original graph.
    let mut label = seed;
    local_move(adj0, deg0, two_m, resolution, &mut label)?;
    let (mut orig_comm, mut k) = densify(&label);

    // Higher levels: aggregate + local-move until no merge.
    loop {
        let (sadj, sdeg) = aggregate(adj0, deg0, &orig_comm, k);
        let mut slabel: Vec<usize> = (0..k).collect();
        local_move(&sadj, &sdeg, two_m, resolution, &mut slabel)?;
        let (sdense, k2) = densify(&slabel);
        if k2 == k {
            break; // no super-node moved → fixed point
        }
        for c in orig_comm.iter_mut() {
            *c = sdense[*c];
        }
        k = k2;
    }

    debug_assert_eq!(orig_comm.len(), n);
    Ok(orig_comm)
}

/// Build the aggregated super-graph over `k` communities from the *original*
/// weighted graph: `sdeg[c]` = total degree of community `c` (preserves `two_m`);
/// `sadj` carries summed inter-community weights (no self-loops — internal weight
/// is a constant that never changes a move). Deterministic (BTreeMap ordering).
fn aggregate(
    adj0: &[Vec<(usize, f64)>],
    deg0: &[f64],
    comm: &[usize],
    k: usize,
) -> (Vec<Vec<(usize, f64)>>, Vec<f64>) {
    let mut sdeg = vec![0.0; k];
    for (i, &d) in deg0.iter().enumerate() {
        sdeg[comm[i]] += d;
    }
    let mut inter: BTreeMap<(usize, usize), f64> = BTreeMap::new();
    for (u, nbrs) in adj0.iter().enumerate() {
        for &(v, w) in nbrs {
            if u < v {
                let (cu, cv) = (comm[u], comm[v]);
                if cu != cv {
                    *inter.entry((cu.min(cv), cu.max(cv))).or_insert(0.0) += w;
                }
            }
        }
    }
    let mut sadj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); k];
    for (&(a, b), &w) in &inter {
        sadj[a].push((b, w));
        sadj[b].push((a, w));
    }
    (sadj, sdeg)
}

/// Renumber arbitrary labels to a dense `0..k`, ordered by first occurrence (index
/// order) for determinism. Returns the dense labels and `k`.
fn densify(label: &[usize]) -> (Vec<usize>, usize) {
    let mut remap: HashMap<usize, usize> = HashMap::new();
    let mut next = 0usize;
    let dense: Vec<usize> = label
        .iter()
        .map(|&l| {
            *remap.entry(l).or_insert_with(|| {
                let d = next;
                next += 1;
                d
            })
        })
        .collect();
    (dense, next)
}

/// Cohesion per group (ADR-0024): `internal / (internal + boundary)` of edge
/// weight, as permille. A group with no boundary edges (incl. the no-edge
/// singleton `0/0`) scores `1000`. Aligned with `groups` order.
fn cohesion_permille(
    pairw: &BTreeMap<(usize, usize), f64>,
    groups: &[Vec<usize>],
    n: usize,
) -> Vec<u16> {
    let mut group_of = vec![0usize; n];
    for (g, members) in groups.iter().enumerate() {
        for &i in members {
            group_of[i] = g;
        }
    }
    let mut internal = vec![0.0; groups.len()];
    let mut boundary = vec![0.0; groups.len()];
    for (&(u, v), &w) in pairw {
        let (gu, gv) = (group_of[u], group_of[v]);
        if gu == gv {
            internal[gu] += w;
        } else {
            boundary[gu] += w;
            boundary[gv] += w;
        }
    }
    (0..groups.len())
        .map(|g| {
            let total = internal[g] + boundary[g];
            let coh = if total <= 0.0 {
                1.0
            } else {
                internal[g] / total
            };
            (coh * 1000.0).round().clamp(0.0, 1000.0) as u16
        })
        .collect()
}

/// Assign each final community a `CommunityId`, inheriting a prior id by maximum
/// node overlap (HLD §11.2). New communities take the smallest unused id.
/// `cohesion` is the per-group score (aligned with `groups`), stamped onto meta.
fn stabilize(
    nodes: &[&Node],
    groups: &[Vec<usize>],
    prior: &Partition,
    cohesion: &[u16],
) -> Result<Partition> {
    // prior community id for each node index (if any).
    let prior_of: Vec<Option<CommunityId>> = nodes
        .iter()
        .map(|node| prior.node_community.get(&node.id).copied())
        .collect();

    // For each group, tally overlap with each prior community id.
    let overlaps: Vec<BTreeMap<CommunityId, usize>> = groups
        .iter()
        .map(|g| {
            let mut m: BTreeMap<CommunityId, usize> = BTreeMap::new();
            for &i in g {
                if let Some(cid) = prior_of[i] {
                    *m.entry(cid).or_default() += 1;
                }
            }
            m
        })
        .collect();
    let best_overlap: Vec<usize> = overlaps
        .iter()
        .map(|m| m.values().copied().max().unwrap_or(0))
        .collect();

    // Claim prior ids best-match first (ties broken by smallest node index, via
    // the group order), so the strongest inheritor wins a contested id.
    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by(|&x, &y| {
        best_overlap[y]
            .cmp(&best_overlap[x])
            .then(groups[x][0].cmp(&groups[y][0]))
    });

    let mut assigned: Vec<Option<CommunityId>> = vec![None; groups.len()];
    let mut taken: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    for &g in &order {
        // best not-yet-taken prior id with positive overlap; tie → smallest id.
        let pick = overlaps[g]
            .iter()
            .filter(|(cid, _)| !taken.contains(&cid.0))
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
            .filter(|(_, &c)| c > 0)
            .map(|(cid, _)| *cid);
        if let Some(cid) = pick {
            assigned[g] = Some(cid);
            taken.insert(cid.0);
        }
    }

    // Fresh ids (smallest unused) for the rest, in group order for determinism.
    let mut used: std::collections::BTreeSet<u64> = taken.clone();
    let mut counter = 0u64;
    let mut fresh = || {
        while used.contains(&counter) {
            counter += 1;
        }
        used.insert(counter);
        CommunityId(counter)
    };
    for slot in assigned.iter_mut() {
        if slot.is_none() {
            *slot = Some(fresh());
        }
    }

    // Materialize the partition.
    let mut node_community = BTreeMap::new();
    let mut communities = BTreeMap::new();
    for (g, group) in groups.iter().enumerate() {
        let cid = assigned[g].ok_or_else(|| poisoned_bookkeeping("assigned", g))?;
        for &i in group {
            node_community.insert(nodes[i].id.clone(), cid);
        }
        // Inherited communities keep their prior label; new ones get a placeholder.
        let label = prior
            .communities
            .get(&cid)
            .map(|m| m.label.clone())
            .unwrap_or_else(|| format!("Community {}", cid.0));
        communities.insert(
            cid,
            CommunityMeta {
                id: cid,
                label,
                size: group.len(),
                cohesion_permille: cohesion[g],
            },
        );
    }

    Ok(Partition {
        node_community,
        communities,
    })
}

#[cfg(test)]
mod tests {
    //! ADR-0041 Class C: `local_move`'s `sum_tot` bookkeeping and `stabilize`'s
    //! `assigned` fill-in are both invariants enforced by construction — every
    //! label present in `label[]` is seeded into `sum_tot` before it is read
    //! (top of `local_move`), and `assigned`'s second pass unconditionally
    //! fills every remaining `None` slot before `stabilize` reads any of them.
    //! Neither is reachable in a "broken" state through any real call path, so
    //! there is no adversarial input that drives the new `Result` sites to
    //! their `Err` branch — these tests instead pin the *positive* path these
    //! invariants protect (single community, multiple communities, warm-start
    //! id inheritance, multi-level aggregation) directly against `cluster.rs`,
    //! which previously had zero unit coverage of its own (only exercised
    //! indirectly via `tests/clustering*.rs`).

    use super::*;

    fn n(id: &str) -> Node {
        Node::new(id, id, "function")
    }

    fn e(a: &str, b: &str) -> Edge {
        Edge {
            source: NodeId::new(a),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Node(NodeId::new(b)),
        }
    }

    #[test]
    fn empty_graph_is_ok_and_empty() {
        let p = cluster::<Node>(&[], &[], &Partition::default()).unwrap();
        assert!(p.node_community.is_empty());
        assert!(p.communities.is_empty());
    }

    #[test]
    fn single_triangle_is_one_community() {
        let nodes = vec![n("a"), n("b"), n("c")];
        let edges = vec![e("a", "b"), e("b", "c"), e("c", "a")];
        let p = cluster(&nodes, &edges, &Partition::default()).unwrap();
        assert_eq!(
            p.communities.len(),
            1,
            "sum_tot survived a full local_move pass"
        );
    }

    #[test]
    fn two_disjoint_triangles_are_two_communities_with_stable_ids() {
        // Exercises stabilize's `assigned` fill-in across >1 group, and the
        // overlap-based id inheritance on a warm re-cluster.
        let nodes = vec![n("a1"), n("a2"), n("a3"), n("b1"), n("b2"), n("b3")];
        let edges = vec![
            e("a1", "a2"),
            e("a2", "a3"),
            e("a3", "a1"),
            e("b1", "b2"),
            e("b2", "b3"),
            e("b3", "b1"),
        ];
        let p1 = cluster(&nodes, &edges, &Partition::default()).unwrap();
        assert_eq!(p1.communities.len(), 2);
        let cid_a = *p1.node_community.get(&NodeId::new("a1")).unwrap();
        let cid_b = *p1.node_community.get(&NodeId::new("b1")).unwrap();
        assert_ne!(cid_a, cid_b);

        // Warm re-cluster from p1 (every group now takes the "claim prior id"
        // branch of `assigned`, not just the "fresh id" branch above).
        let p2 = cluster(&nodes, &edges, &p1).unwrap();
        assert_eq!(p2.node_community.get(&NodeId::new("a1")), Some(&cid_a));
        assert_eq!(p2.node_community.get(&NodeId::new("b1")), Some(&cid_b));
    }

    #[test]
    fn full_strategy_survives_multiple_aggregation_levels() {
        // A ring of triangles: local_move runs to convergence at level 0, then
        // louvain_full re-runs it on the aggregated super-graph — the same
        // sum_tot/assigned bookkeeping exercised across several distinct
        // `label`/`assigned` vectors of shrinking size.
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for t in 0..6 {
            let ids = [format!("t{t}n0"), format!("t{t}n1"), format!("t{t}n2")];
            for id in &ids {
                nodes.push(n(id));
            }
            edges.push(e(&ids[0], &ids[1]));
            edges.push(e(&ids[1], &ids[2]));
            edges.push(e(&ids[2], &ids[0]));
            let nt = (t + 1) % 6;
            edges.push(e(&ids[0], &format!("t{nt}n0")));
        }
        let cfg = ClusterConfig {
            strategy: ClusterStrategy::Full,
            weighting: EdgeWeighting::Uniform,
            resolution: 1.0,
        };
        let p = cluster_with(&nodes, &edges, &Partition::default(), &cfg).unwrap();
        assert_eq!(
            p.node_community.len(),
            18,
            "every node assigned a community"
        );
        assert!(!p.communities.is_empty());
    }
}
