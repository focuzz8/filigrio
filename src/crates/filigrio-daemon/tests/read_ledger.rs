//! The **agent-visible** half of the read-path ledger (`docs/perf/benchmarks.md` §6).
//!
//! `filigrio-query/tests/read_profile.rs` times the pieces; this times what a
//! tool call actually costs — one `DataQuery` through the same
//! [`Responder`] + [`RegistryWarmStateSource`] the resident daemon serves from,
//! so the number includes project resolution, the cache hit, view acquisition
//! and the query. It is the only place the **view cache** is visible at all:
//! before it, every one of these calls rebuilt a petgraph index first.
//!
//! ```text
//! cargo test --release -p filigrio-daemon --test read_ledger -- --ignored --nocapture
//! FILIGRIO_READ_N=2000,20000,100000 cargo test --release … # add a next.js-scale row
//! ```

use filigrio_core::{
    CommunityId, Confidence, Edge, EdgeTarget, Graph, GraphState, Node, NodeId, Partition,
    TargetRef,
};
use filigrio_daemon::{
    DataQuery, FlushConfig, Flusher, Project, ProjectLocks, ProjectRegistry, ProjectStateCache,
    RegistryWarmStateSource, Responder, SystemClock,
};
use filigrio_protocol::NodeAddress;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const HOMONYM: &str = "with_version";

/// Same shape as the query-crate harness: n nodes, 2 resolved edges each, one
/// community holding everything, n unresolved edges naming one homonym.
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

fn bench(reps: u32, mut f: impl FnMut()) -> Duration {
    let t = Instant::now();
    for _ in 0..reps {
        f();
    }
    t.elapsed() / reps
}

#[test]
#[ignore = "read-path ledger — run with `just read-ledger`"]
fn tool_call_latency() {
    let scales: Vec<usize> = std::env::var("FILIGRIO_READ_N")
        .unwrap_or_else(|_| "2000,20000".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    println!("=== per-tool-call latency, warm daemon path (ledger §6) ===");
    println!(
        "{:<8} {:>12} {:>12} {:>12} {:>12}",
        "n nodes", "get_node", "get_neighbors", "get_community", "graph_stats"
    );

    for n in scales {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = Project::new(temp.path().to_path_buf());
        let id = project.id.clone();

        let cache = Arc::new(Mutex::new(ProjectStateCache::new(8)));
        cache.lock().load_into(&id, synthetic(n));
        let registry = Arc::new(Mutex::new(ProjectRegistry::new()));
        registry.lock().add(project).expect("register");
        let flusher = Arc::new(Flusher::new(
            Arc::clone(&cache),
            ProjectLocks::new(),
            FlushConfig::default(),
            Arc::new(SystemClock),
        ));
        let responder = Responder::new(RegistryWarmStateSource::new(flusher, registry));

        let probe_id = format!(
            "fn:crates/filigrio-index/src/module_{}.rs::handler_{}",
            (n / 2) % 97,
            n / 2
        );
        let address = || NodeAddress {
            id: Some(probe_id.clone()),
            label: None,
            src: None,
        };

        // Warm: the first call is the one that pays for building the view when
        // there is a cache, so it must not be inside the mean.
        let _ = responder.handle_query(DataQuery::GraphStats {
            project: id.clone(),
        });

        let get_node = bench(20, || {
            std::hint::black_box(responder.handle_query(DataQuery::GetNode {
                project: id.clone(),
                node_address: address(),
            }));
        });
        let neighbors = bench(5, || {
            std::hint::black_box(responder.handle_query(DataQuery::Neighbors {
                project: id.clone(),
                node: address(),
                direction: filigrio_protocol::Direction::Both,
                relations: Vec::new(),
                include_unresolved: false,
            }));
        });
        let community = bench(3, || {
            std::hint::black_box(responder.handle_query(DataQuery::Community {
                project: id.clone(),
                community_id: 0,
            }));
        });
        let stats = bench(20, || {
            std::hint::black_box(responder.handle_query(DataQuery::GraphStats {
                project: id.clone(),
            }));
        });

        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        println!(
            "{n:<8} {:>11.3}ms {:>11.3}ms {:>11.3}ms {:>11.3}ms",
            ms(get_node),
            ms(neighbors),
            ms(community),
            ms(stats)
        );
    }
}
