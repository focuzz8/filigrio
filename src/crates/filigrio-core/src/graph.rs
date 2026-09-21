//! The graph container. A plain adjacency-free `Vec` pair for the skeleton;
//! the real port swaps this for a `petgraph` newtype (HLD §9, ADR-0001)
//! behind the same accessors.

use crate::model::{Edge, EdgeTarget, Node, NodeId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl Graph {
    pub fn node_by_id(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.iter().find(|n| &n.id == id)
    }

    pub fn node_by_label(&self, label: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.label == label)
    }

    /// Out-degree of a node by its id (edges whose `source` is this node).
    pub fn out_degree(&self, id: &NodeId) -> usize {
        self.edges.iter().filter(|e| &e.source == id).count()
    }

    /// In-degree of a node by its id (resolved edges pointing at it).
    pub fn in_degree(&self, id: &NodeId) -> usize {
        self.edges
            .iter()
            .filter(|e| matches!(&e.target, EdgeTarget::Node(t) if t == id))
            .count()
    }

    pub fn degree(&self, id: &NodeId) -> usize {
        self.out_degree(id) + self.in_degree(id)
    }

    /// Resolved out-neighbours of a node (edge + target node).
    pub fn neighbors<'a>(&'a self, id: &NodeId) -> Vec<(&'a Edge, &'a Node)> {
        self.edges
            .iter()
            .filter(|e| &e.source == id)
            .filter_map(|e| match &e.target {
                EdgeTarget::Node(t) => self.node_by_id(t).map(|n| (e, n)),
                EdgeTarget::Symbol(_) => None,
            })
            .collect()
    }
}
