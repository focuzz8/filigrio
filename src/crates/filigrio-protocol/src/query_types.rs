//! The contract's query *value* types (ADR-0025/0027/0029) — the parameter
//! structures [`crate::contract::DataQuery`] is built out of, re-exported
//! through `contract.rs`. Not a second protocol layer: the wire vocabulary is
//! [`crate::contract`], this is the vocabulary's nouns.
//!
//! Traversal order and edge direction are **not** declared here. They are the
//! kernel's own [`filigrio_core::TraversalMode`] / [`filigrio_core::Direction`],
//! re-exported so a transport that only depends on the protocol can still name
//! them — a parallel wire enum would mean two definitions per concept and a
//! hand-written match at every boundary, which is exactly what a silently
//! transposed mapping needs to survive compilation.

pub use filigrio_core::{Direction, TraversalMode};
use serde::{Deserialize, Serialize};

/// NodeAddress supporting ADR-0027 addressing modes
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct NodeAddress {
    /// Unique node identifier (exact match, wins over label/src)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Node label (matches multiple nodes when id is absent)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Source file path to disambiguate homonyms (with label only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
}

impl NodeAddress {
    /// Create address by exact ID (highest priority match)
    pub fn by_id(id: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            label: None,
            src: None,
        }
    }

    /// Create address by label (may return ambiguous results)
    pub fn by_label(label: impl Into<String>) -> Self {
        Self {
            id: None,
            label: Some(label.into()),
            src: None,
        }
    }

    /// Create address by label narrowed by source file
    pub fn by_label_src(label: impl Into<String>, src: impl Into<String>) -> Self {
        Self {
            id: None,
            label: Some(label.into()),
            src: Some(src.into()),
        }
    }

    /// Check if this address is ambiguous (label without ID)
    pub fn is_ambiguous(&self) -> bool {
        self.id.is_none() && self.label.is_some()
    }

    /// Parse a single free-form address string into an id-or-label address,
    /// using the same heuristic every client (CLI, MCP bridge) already
    /// applied inline before this was centralized: an ADR-0028 id always
    /// contains `:` (`kind:src:name`), a bare label never does.
    pub fn parse(address: impl AsRef<str>) -> Self {
        let address = address.as_ref();
        if address.contains(':') {
            Self::by_id(address)
        } else {
            Self::by_label(address)
        }
    }

    /// Attach a `src` disambiguator — a no-op when this address already
    /// resolved to an id (id always wins over label/src, so src would be
    /// ignored anyway).
    pub fn with_src(mut self, src: Option<impl Into<String>>) -> Self {
        if self.label.is_some() {
            self.src = src.map(Into::into);
        }
        self
    }
}

/// Query parameters (ADR-0025/0029)
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct QueryParams {
    /// Query text (search terms)
    #[serde(default)]
    pub query: String,
    /// Traversal mode (bfs/dfs)
    #[serde(default)]
    pub mode: TraversalMode,
    /// Maximum search depth
    #[serde(default = "default_depth")]
    pub depth: u8,
    /// Result budget (max nodes to return)
    #[serde(default = "default_budget")]
    pub budget: usize,
    /// Token budget for output limiting
    #[serde(default = "default_token_budget")]
    pub token_budget: usize,
    /// Relation filters (e.g., "calls", "contains", "imports")
    #[serde(default)]
    pub relations: Vec<String>,
    /// Include unresolved edges in results
    #[serde(default)]
    pub include_unresolved: bool,
}

fn default_depth() -> u8 {
    2
}

fn default_budget() -> usize {
    32
}

fn default_token_budget() -> usize {
    4000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_address_serialization() {
        let addr = NodeAddress::by_id("node-123");
        let json = serde_json::to_string(&addr).unwrap();
        assert_eq!(json, r#"{"id":"node-123"}"#);

        let addr = NodeAddress::by_label("main");
        let json = serde_json::to_string(&addr).unwrap();
        assert_eq!(json, r#"{"label":"main"}"#);

        let addr = NodeAddress::by_label_src("main", "src/main.rs");
        let json = serde_json::to_string(&addr).unwrap();
        assert_eq!(json, r#"{"label":"main","src":"src/main.rs"}"#);
    }

    #[test]
    fn test_node_address_deserialization() {
        let json = r#"{"id":"node-456"}"#;
        let addr: NodeAddress = serde_json::from_str(json).unwrap();
        assert_eq!(addr, NodeAddress::by_id("node-456"));

        let json = r#"{"label":"test"}"#;
        let addr: NodeAddress = serde_json::from_str(json).unwrap();
        assert_eq!(addr, NodeAddress::by_label("test"));
    }

    #[test]
    fn test_query_params_defaults() {
        let params: QueryParams = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(params.depth, 2);
        assert_eq!(params.budget, 32);
        assert_eq!(params.token_budget, 4000);
        assert_eq!(params.mode, TraversalMode::Bfs);
    }

    #[test]
    fn test_query_params_serialization() {
        let params = QueryParams {
            query: "test search".to_string(),
            depth: 3,
            budget: 50,
            token_budget: 8000,
            mode: TraversalMode::Dfs,
            relations: vec!["calls".to_string()],
            include_unresolved: true,
        };
        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains(r#""query":"test search""#));
        assert!(json.contains(r#""depth":3"#));
        assert!(json.contains(r#""budget":50"#));
        assert!(json.contains(r#""token_budget":8000"#));
        assert!(json.contains(r#""mode":"dfs""#));
        assert!(json.contains(r#""include_unresolved":true"#));
    }

    #[test]
    fn test_traversal_mode_serialization() {
        assert_eq!(
            serde_json::to_string(&TraversalMode::Bfs).unwrap(),
            r#""bfs""#
        );
        assert_eq!(
            serde_json::to_string(&TraversalMode::Dfs).unwrap(),
            r#""dfs""#
        );
    }

    /// The wire spelling of the kernel's `Direction` is part of the contract:
    /// the enum moved into `filigrio-core`, the JSON must not.
    #[test]
    fn test_edge_direction_serialization() {
        assert_eq!(serde_json::to_string(&Direction::In).unwrap(), r#""in""#);
        assert_eq!(serde_json::to_string(&Direction::Out).unwrap(), r#""out""#);
        assert_eq!(
            serde_json::to_string(&Direction::Both).unwrap(),
            r#""both""#
        );
        assert_eq!(Direction::default(), Direction::Both);
        assert_eq!(TraversalMode::default(), TraversalMode::Bfs);
    }
}
