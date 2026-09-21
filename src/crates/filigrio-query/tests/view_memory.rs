//! What a cached [`GraphView`] costs in memory — the other half of the §L1
//! decision, and the reason it is a measurement and not an assumption.
//!
//! The daemon's LRU is bounded because a resident next.js project is ≈2.2 GB
//! (ADR-0032 §2). Caching a view alongside each resident state raises that
//! ceiling by whatever a view weighs, so "it's just an index, it's small" is
//! exactly the kind of claim this project has been wrong about before
//! (ADR-0042 Phase 1b: the ~2.1 s attributed to derived indices by reading the
//! code turned out to be 3.6%). This harness weighs it.
//!
//! Method: a counting global allocator over `System`. Live-bytes are sampled
//! before and after each construction, so the number is the heap the object
//! actually holds — not `size_of`, which sees three pointers and a `usize`.
//! It is a **separate test binary from `read_profile.rs` on purpose**: the
//! counter adds an atomic to every allocation, which would tax exactly the
//! allocation-heavy path that harness times.
//!
//! ```text
//! cargo test --release -p filigrio-query --test view_memory -- --ignored --nocapture
//! ```

use filigrio_core::{
    CommunityId, Confidence, Edge, EdgeTarget, Graph, GraphState, Node, NodeId, Partition,
};
use filigrio_query::GraphView;
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;

static LIVE: AtomicIsize = AtomicIsize::new(0);

/// `System`, plus a running total of live bytes. Relaxed ordering: the harness
/// is single-threaded at every sampling point, and the counter is a measurement,
/// not a synchronization mechanism.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_add(
                new_size as isize - layout.size() as isize,
                Ordering::Relaxed,
            );
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> isize {
    LIVE.load(Ordering::Relaxed)
}

/// Heap bytes held by whatever `f` returns (live-bytes delta across the call).
fn weigh<T>(f: impl FnOnce() -> T) -> (T, isize) {
    let before = live();
    let value = f();
    (value, live() - before)
}

fn mb(bytes: isize) -> f64 {
    bytes as f64 / 1e6
}

/// The same shape as `read_profile.rs`: realistic id/label lengths (the view's
/// dominant cost is cloned id strings, so short ids would flatter it), 2 resolved
/// edges per node, and every node in one community.
///
/// One community for *n* nodes is the friendliest case for a per-community label
/// map and the harshest for the per-node map it replaced — deliberately, since
/// that is the shape the two are being compared on. It is not a flattering
/// corner: a partition with more communities adds one small entry each, so the
/// map stays O(communities), which ADR-0024's dogfood put at 155–655.
fn synthetic(n: usize, cluster: bool) -> GraphState {
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
    let mut edges: Vec<Edge> = Vec::with_capacity(2 * n);
    for i in 0..n {
        for target in [(i + 1) % n, (i + 7) % n] {
            edges.push(Edge {
                source: NodeId::new(id(i)),
                relation: "calls".into(),
                confidence: Confidence::Extracted,
                target: EdgeTarget::Node(NodeId::new(id(target))),
            });
        }
    }
    let node_community: BTreeMap<NodeId, CommunityId> = if cluster {
        nodes
            .iter()
            .map(|node| (node.id.clone(), CommunityId(0)))
            .collect()
    } else {
        BTreeMap::new()
    };
    GraphState {
        graph: Graph { nodes, edges },
        partition: Partition {
            node_community,
            communities: BTreeMap::new(),
        },
        ..Default::default()
    }
}

#[test]
#[ignore = "read-path ledger — run with `just read-ledger`"]
fn view_footprint_against_state() {
    let scales: Vec<usize> = std::env::var("FILIGRIO_READ_N")
        .unwrap_or_else(|_| "2000,20000,100000".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    println!("=== GraphView memory footprint (ledger §6) ===");
    println!(
        "{:<8} {:>12} {:>12} {:>12} {:>10} {:>12}",
        "n nodes", "state MB", "view MB", "of which", "view/state", "B/node"
    );
    println!(
        "{:<8} {:>12} {:>12} {:>12} {:>10} {:>12}",
        "", "", "", "labels MB", "", ""
    );

    for n in scales {
        // Clustered: the production shape — the view carries a per-node label map.
        let (state, state_bytes) = weigh(|| Arc::new(synthetic(n, true)));
        let (view, view_bytes) = weigh(|| GraphView::new(Arc::clone(&state)));

        // Unclustered: `build_community_labels` returns empty, so the delta
        // between the two views is the label map's whole share (§A2). It was the
        // per-node map (~40 % of the view); it is now one entry per *community*,
        // so this column reads ~0 — that near-zero is the collapse, measured.
        let bare = Arc::new(synthetic(n, false));
        let (bare_view, bare_bytes) = weigh(|| GraphView::new(Arc::clone(&bare)));

        println!(
            "{n:<8} {:>11.1}MB {:>11.1}MB {:>11.1}MB {:>9.1}% {:>11.0}",
            mb(state_bytes),
            mb(view_bytes),
            mb(view_bytes - bare_bytes),
            100.0 * view_bytes as f64 / state_bytes as f64,
            view_bytes as f64 / n as f64,
        );

        // Keep both alive across the sampling above (a dropped view would have
        // deallocated inside the window and reported ~0).
        std::hint::black_box((&view, &bare_view));
        drop(view);
        drop(bare_view);
    }

    println!(
        "note: synthetic ids ~50 chars. The view's cost is dominated by cloned NodeId\n\
         strings (idx_of + the label map), so it scales with id length, not just node count."
    );
}
