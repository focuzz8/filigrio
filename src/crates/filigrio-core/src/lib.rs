//! filigrio-core — the domain kernel.
//!
//! Defines the graph data model, the incremental engine state, and the five
//! port traits that are the *entire* external contract of the system
//! (HLD §4). **No I/O lives here.** Everything else is an adapter or a
//! pipeline stage over these types.

#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]
pub mod attrs;
mod error;
mod graph;
mod model;
mod ports;
pub mod priority;
pub mod profile;
pub mod relation;
mod state;

pub use error::{Error, Result};
pub use graph::Graph;
pub use model::{
    classify, is_indexable, is_manifest, manifest_basename, Artifact, ArtifactKind, Confidence,
    Edge, EdgeTarget, Export, Extraction, Node, NodeId, Span, TargetRef, MANIFEST_NAMES,
};
pub use ports::{Direction, Extractor, GraphQuery, GraphStore, ModuleResolver, Source};
pub use priority::Priority;
pub use state::{
    ChangeSet, CommunityId, CommunityMeta, ExportIndex, GraphDelta, GraphState, GraphStats,
    Manifest, ManifestEntry, Partition, Project, ProjectGraph, ProjectNode, QueryOpts, Reference,
    ReverseIndex, Revision, Subgraph, SymbolDefs, SymbolIndex, SymbolTable, TraversalMode,
    Workspace,
};
