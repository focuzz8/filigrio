//! The **read-path** ledger harness — `docs/perf/benchmarks.md` §6.
//!
//! The apply path has had eight ledger sections and a per-stage profile
//! (`filigrio-resolve/tests/apply_profile.rs`); the read path — the surface an
//! agent actually waits on — had none. This harness is the counterpart: it
//! times what one MCP tool call costs, split into the two halves that can
//! regress independently:
//!
//! * **view construction** (`GraphView::new`) — the petgraph index + the
//!   per-node community-label map, rebuilt per request before the daemon
//!   cached it;
//! * **the query itself** — `community`, `unresolved_in_by_label`,
//!   `neighbors_by_id`, … each backing a named tool.
//!
//! It is deliberately synthetic and shape-controlled: the point is the *growth
//! rate* (10× the nodes → how many × the time), which a real corpus cannot pin
//! down because its communities and homonym sets vary. A quadratic method shows
//! up here as ~100× across the two default scales and nowhere else.
//!
//! ```text
//! cargo test --release -p filigrio-query --test read_profile -- --ignored --nocapture
//! FILIGRIO_READ_N=2000,20000,100000 cargo test --release … # add a next.js-scale row
//! ```
//!
//! | env | default | meaning |
//! |---|---|---|
//! | `FILIGRIO_READ_N` | `2000,20000` | node counts to measure, comma-separated |
//! | `FILIGRIO_READ_REPS` | `100` | repetitions for the sub-millisecond queries |

use filigrio_core::profile;
use filigrio_core::{
    CommunityId, Confidence, Direction, Edge, EdgeTarget, Graph, GraphQuery, GraphState, Node,
    NodeId, Partition, QueryOpts, TargetRef, TraversalMode,
};
use filigrio_query::GraphView;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The name every unresolved edge in the fixture points at — the ADR-0029
/// honesty surface is hit hardest on exactly this shape (one popular homonym).
const HOMONYM: &str = "with_version";

/// A synthetic graph of `n` function nodes:
/// * ids and labels are realistically long (a bare `n0` id makes the linear
///   scans look ~4× cheaper than they are — the scan cost is string compares);
/// * 2 resolved `calls` edges per node (a ring plus a chord), so the petgraph
///   index has 2n edges;
/// * every node in **one** community, so `community(0)` returns all n — the
///   large-community case `get_community` hits on a monorepo;
/// * n unresolved `calls` edges all naming [`HOMONYM`], so
///   `unresolved_in_by_label` has n matches.
fn synthetic(n: usize) -> GraphState {
    let id = |i: usize| {
        format!(
            "fn:crates/filigrio-index/src/module_{}.rs::handler_{i}",
            i % 97
        )
    };
    let nodes: Vec<Node> = (0..n)
        .map(|i| {
            let mut node = Node::new(id(i), format!("handler_{i}"), "function");
            node.source_file = Some(format!("crates/filigrio-index/src/module_{}.rs", i % 97));
            node
        })
        .collect();

    let mut edges: Vec<Edge> = Vec::with_capacity(3 * n);
    for i in 0..n {
        for target in [(i + 1) % n, (i + 7) % n] {
            edges.push(Edge {
                source: NodeId::new(id(i)),
                relation: "calls".into(),
                confidence: Confidence::Extracted,
                target: EdgeTarget::Node(NodeId::new(id(target))),
            });
        }
        let mut tref = TargetRef::new(HOMONYM);
        tref.hints.insert("recv".into(), "opaque".into());
        edges.push(Edge {
            source: NodeId::new(id(i)),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Symbol(tref),
        });
    }

    let node_community: BTreeMap<NodeId, CommunityId> = nodes
        .iter()
        .map(|node| (node.id.clone(), CommunityId(0)))
        .collect();

    GraphState {
        graph: Graph { nodes, edges },
        partition: Partition {
            node_community,
            communities: BTreeMap::new(),
        },
        ..Default::default()
    }
}

/// Mean wall-clock of `reps` calls to `f`.
fn bench(reps: u32, mut f: impl FnMut()) -> Duration {
    let t = Instant::now();
    for _ in 0..reps {
        f();
    }
    t.elapsed() / reps
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

#[test]
#[ignore = "read-path ledger — run with `just read-ledger` or `cargo test --release -p filigrio-query --test read_profile -- --ignored --nocapture`"]
fn read_path_cost() {
    let scales: Vec<usize> = std::env::var("FILIGRIO_READ_N")
        .unwrap_or_else(|_| "2000,20000".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let reps: u32 = std::env::var("FILIGRIO_READ_REPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);

    println!("=== read-path cost (ledger §6) ===");
    println!(
        "{:<8} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "n nodes",
        "view::new",
        "community",
        "unresolved_in",
        "node_by_id",
        "neighbors",
        "god_nodes",
        "query"
    );

    for n in scales {
        let state = Arc::new(synthetic(n));
        // Warm up: page in the state, let the allocator settle.
        let view = GraphView::new(Arc::clone(&state));

        let build = bench(3, || {
            let v = GraphView::new(Arc::clone(&state));
            std::hint::black_box(&v);
            drop(v);
        });

        let probe_id = format!(
            "fn:crates/filigrio-index/src/module_{}.rs::handler_{}",
            (n / 2) % 97,
            n / 2
        );

        let community = bench(3, || {
            std::hint::black_box(view.community(CommunityId(0)).expect("community"));
        });
        let unresolved = bench(3, || {
            std::hint::black_box(view.unresolved_in_by_label(HOMONYM).expect("unresolved"));
        });
        let node_by_id = bench(reps, || {
            std::hint::black_box(view.node_by_id(&probe_id).expect("node_by_id"));
        });
        let neighbors = bench(reps, || {
            std::hint::black_box(
                view.neighbors_by_id(&probe_id, &[], Direction::Both)
                    .expect("neighbors"),
            );
        });
        let god_nodes = bench(3, || {
            std::hint::black_box(view.god_nodes(10).expect("god_nodes"));
        });
        let query = bench(3, || {
            std::hint::black_box(
                view.query(
                    "handler",
                    QueryOpts {
                        mode: TraversalMode::Bfs,
                        depth: 2,
                        budget: 64,
                        context_filter: None,
                    },
                )
                .expect("query"),
            );
        });

        println!(
            "{n:<8} {:>11.2}ms {:>11.2}ms {:>11.2}ms {:>11.3}ms {:>11.3}ms {:>11.2}ms {:>11.2}ms",
            ms(build),
            ms(community),
            ms(unresolved),
            ms(node_by_id),
            ms(neighbors),
            ms(god_nodes),
            ms(query),
        );

        // The instrumented split — the same `profile::capture` the apply path
        // uses (ADR-0042 Phase 1b.1), now pointed at the read path. `view_new`'s
        // two sub-stages are where a construction regression would show up;
        // `q.*` sub-stages are inside `query` and are *not* additional.
        let (_, stages) = profile::capture(|| {
            let v = GraphView::new(Arc::clone(&state));
            let _ = v.community(CommunityId(0));
            let _ = v.unresolved_in_by_label(HOMONYM);
            let _ = v.god_nodes(10);
            let _ = v.query(
                "handler",
                QueryOpts {
                    mode: TraversalMode::Bfs,
                    depth: 2,
                    budget: 64,
                    context_filter: None,
                },
            );
        });
        let mut rows: Vec<&profile::Stage> = stages.iter().collect();
        rows.sort_by_key(|s| std::cmp::Reverse(s.2));
        let split: Vec<String> = rows
            .iter()
            .map(|(name, depth, d)| {
                let indent = if *depth == 0 { "" } else { "·" };
                format!("{indent}{name} {:.2}ms", ms(*d))
            })
            .collect();
        println!("  stages: {}", split.join(" | "));

        // Sanity: the fixture really does exercise the shapes claimed above —
        // a silently-empty result would make every number here meaningless.
        assert_eq!(view.community(CommunityId(0)).expect("community").len(), n);
        assert_eq!(
            view.unresolved_in_by_label(HOMONYM)
                .expect("unresolved")
                .len(),
            n
        );
        assert!(view.node_by_id(&probe_id).expect("node_by_id").is_some());
    }
}
