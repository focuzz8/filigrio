//! `graph.json` interchange adapter (ADR-0017).
//!
//! Bidirectional and **schema/semantic** compatible with Python graphify — NOT
//! byte-identical. This is the migration on-ramp (import real users' graphs) and
//! the differential oracle (diff our export against Python's). It is decoupled
//! from the native storage format: the engine's live state (partition, symbol
//! table, reverse index) is intentionally *not* represented here.
//!
//! Schema (graphify-description §4):
//! ```json
//! { "nodes": [{"id","label","source_file","source_location", + kind/community}],
//!   "edges": [{"source","target","relation","confidence"}] }
//! ```
//!
//! We export edges under `edges`; Python graphify uses the node-link key `links`.
//! [`import`] accepts either, so a real users' `graph.json` migrates in.

use filigrio_core::{
    CommunityId, Confidence, Edge, EdgeTarget, Error, Graph, GraphState, Node, NodeId, Partition,
    Result, Span,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// Reserved top-level node keys — everything else round-trips through `attrs`.
const CORE_KEYS: &[&str] = &[
    "id",
    "label",
    "kind",
    "source_file",
    "source_location",
    "community",
];

/// `GraphState` → graphify-schema `graph.json` (the export direction).
pub fn export(state: &GraphState) -> Value {
    let nodes: Vec<Value> = state
        .graph
        .nodes
        .iter()
        .map(|n| node_to_json(n, &state.partition))
        .collect();
    let edges: Vec<Value> = state.graph.edges.iter().map(edge_to_json).collect();
    json!({ "nodes": nodes, "edges": edges })
}

fn node_to_json(n: &Node, partition: &Partition) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!(n.id.0));
    m.insert("label".into(), json!(n.label));
    m.insert("kind".into(), json!(n.kind));
    if let Some(f) = &n.source_file {
        m.insert("source_file".into(), json!(f));
    }
    if n.source_span.is_some() {
        m.insert("source_location".into(), json!(n.loc()));
    }
    if let Some(c) = partition.node_community.get(&n.id) {
        m.insert("community".into(), json!(c.0));
    }
    for (k, v) in &n.attrs {
        m.entry(k.clone()).or_insert_with(|| json!(v));
    }
    Value::Object(m)
}

fn edge_to_json(e: &Edge) -> Value {
    // graph.json is post-resolution: resolved edges carry the target id;
    // still-unresolved Symbol edges carry the symbol name so nothing is lost.
    let target = match &e.target {
        EdgeTarget::Node(t) => t.0.clone(),
        EdgeTarget::Symbol(r) => r.name.clone(),
    };
    json!({
        "source": e.source.0,
        "target": target,
        "relation": e.relation,
        "confidence": serde_json::to_value(e.confidence).unwrap_or_else(|_| json!("EXTRACTED")),
    })
}

/// graphify-schema `graph.json` → `GraphState` (the import direction). Edges are
/// taken as resolved (`EdgeTarget::Node`), since a graphify `graph.json` is a
/// post-resolution snapshot.
pub fn import(bytes: &[u8]) -> Result<GraphState> {
    let v: Value = serde_json::from_slice(bytes)?;

    let node_values = v["nodes"]
        .as_array()
        .ok_or_else(|| Error::Parse("graph.json: missing `nodes` array".into()))?;
    let mut nodes = Vec::with_capacity(node_values.len());
    let mut node_community = BTreeMap::new();

    for nv in node_values {
        let id = nv["id"]
            .as_str()
            .ok_or_else(|| Error::Parse("graph.json: node without `id`".into()))?
            .to_string();
        let label = nv["label"].as_str().unwrap_or(&id).to_string();
        let kind = nv["kind"].as_str().unwrap_or("concept").to_string();
        let mut n = Node::new(id.clone(), label, kind);
        n.source_file = nv["source_file"].as_str().map(String::from);
        n.source_span = nv["source_location"].as_str().and_then(Span::parse);
        if let Some(c) = nv["community"].as_u64() {
            node_community.insert(NodeId::new(id.clone()), CommunityId(c));
        }
        if let Some(obj) = nv.as_object() {
            for (k, val) in obj {
                if CORE_KEYS.contains(&k.as_str()) {
                    continue;
                }
                if let Some(s) = val.as_str() {
                    n.attrs.insert(k.clone(), s.to_string());
                }
            }
        }
        nodes.push(n);
    }

    // Edges live under `edges` (our export) or `links` (Python graphify's
    // node-link convention) — accept either so real users' graphs import.
    let mut edges = Vec::new();
    if let Some(edge_values) = v
        .get("edges")
        .or_else(|| v.get("links"))
        .and_then(Value::as_array)
    {
        for ev in edge_values {
            let source = ev["source"]
                .as_str()
                .ok_or_else(|| Error::Parse("graph.json: edge without `source`".into()))?;
            let target = ev["target"]
                .as_str()
                .ok_or_else(|| Error::Parse("graph.json: edge without `target`".into()))?;
            let relation = ev["relation"].as_str().unwrap_or("relates").to_string();
            let confidence = ev
                .get("confidence")
                .cloned()
                .and_then(|c| serde_json::from_value::<Confidence>(c).ok())
                .unwrap_or(Confidence::Extracted);
            edges.push(Edge {
                source: NodeId::new(source),
                relation,
                confidence,
                target: EdgeTarget::Node(NodeId::new(target)),
            });
        }
    }

    Ok(GraphState {
        graph: Graph { nodes, edges },
        partition: Partition {
            node_community,
            communities: BTreeMap::new(),
        },
        ..Default::default()
    })
}
