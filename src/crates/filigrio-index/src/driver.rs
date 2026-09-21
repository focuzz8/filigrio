//! The shared extractor **driver** (ADR-0037 §3, Phase 0a). Owns the plumbing
//! every tree-sitter backend used to duplicate: the `file` node, id minting
//! (ADR-0028, via [`crate::idgen`]), node / `contains` / call / import edge
//! emission, the lexical `scopes` stack for receiver-type inference, and the
//! final [`Extraction`] assembly. A per-language `Ctx` embeds one `Driver` plus
//! its own capability-trait impls and a thin language-specific walk.
//!
//! Generic over the scope value type `V`: Rust binds a `VarType` carrying the
//! ADR-0023/0026 opaque/defer policy, TS/Python a bare type-name `String`, so a
//! single scope stack serves every backend without forcing a common receiver
//! model.

use crate::idgen::{self, IdMeta};
use crate::type_filter::{should_filter_type_with_context, FilterStats};
use filigrio_core::attrs;
use filigrio_core::attrs::{ABSTRACT, TRUE as ABSTRACT_TRUE};
use filigrio_core::relation::{
    BOUND_TYPE, CALLS, CONTAINS, FIELD_TYPE, HAS_VARIANT, IMPORTS, PARAM_TYPE, RETURN_TYPE,
};
use filigrio_core::{
    Artifact, Confidence, Edge, EdgeTarget, Export, Extraction, Node, NodeId, Span, TargetRef,
};
use std::collections::{HashMap, HashSet};
use tree_sitter::Node as TsNode;

/// Shared accumulator threaded through a backend's walk. `V` is the per-language
/// scope value (see the module docs).
pub(crate) struct Driver<'a, V> {
    pub path: String,
    pub src: &'a [u8],
    pub file_id: NodeId,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Module export table (ADR-0020), assembled by the language walk.
    pub exports: Vec<Export>,
    /// per-file id disambiguation: base id → next occurrence index (provisional
    /// `~n`; collisions are re-minted to `#hash` in [`Driver::finish`]).
    seen: HashMap<String, u32>,
    /// provisional id → collision metadata for the id finalize pass.
    id_meta: HashMap<String, IdMeta>,
    /// Lexical variable→type environment for receiver-type inference: a stack of
    /// per-function scopes (`var → inferred type`).
    pub scopes: Vec<HashMap<String, V>>,
    /// The language being extracted (used for type filtering)
    language: String,
    /// Statistics for noise filtering (ADR-0036 R1.1)
    pub filter_stats: FilterStats,
    /// Locally defined type names (for exception from denylist filtering)
    pub local_types: HashSet<String>,
    /// Type-parameter scopes, innermost last (ADR-0036 R1.1). One frame per
    /// enclosing generic construct — `impl`/class/interface/type-alias/function.
    /// A generic construct *nests*: a method of `class Holder<T>` sees `T` even
    /// though it declares nothing itself, so entering a scope must PUSH, never
    /// replace, and leaving must pop.
    type_parameter_stack: Vec<HashSet<String>>,
    /// Union of every active frame — the set the R1.1 filter reads. Cached on
    /// each push/pop so the filter stays a single hash lookup.
    type_parameters: HashSet<String>,
}

impl<'a, V: Clone> Driver<'a, V> {
    /// Start a fresh extraction: seed the `file` node (with its `language` attr).
    pub fn new(artifact: &Artifact, src: &'a [u8], language: &str) -> Self {
        let file_id = NodeId::new(format!("file:{}", artifact.path));
        let mut file_node = Node::new(file_id.0.clone(), &artifact.path, "file");
        file_node.source_file = Some(artifact.path.clone());
        file_node
            .attrs
            .insert(attrs::LANGUAGE.into(), language.into());
        Driver {
            path: artifact.path.clone(),
            src,
            file_id,
            nodes: vec![file_node],
            edges: Vec::new(),
            exports: Vec::new(),
            seen: HashMap::new(),
            id_meta: HashMap::new(),
            scopes: Vec::new(),
            language: language.to_string(),
            filter_stats: FilterStats::new(),
            local_types: HashSet::new(),
            type_parameter_stack: Vec::new(),
            type_parameters: HashSet::new(),
        }
    }

    // ---- type-parameter scopes (ADR-0036 R1.1) -----------------------------

    /// Enter a generic construct's scope with the parameters it declares (empty
    /// is fine and still pushes — the frame is what makes the matching pop safe).
    pub fn push_type_parameters(&mut self, params: HashSet<String>) {
        self.type_parameters.extend(params.iter().cloned());
        self.type_parameter_stack.push(params);
    }

    /// Leave the innermost generic scope, restoring the enclosing one.
    pub fn pop_type_parameters(&mut self) {
        if self.type_parameter_stack.pop().is_some() {
            // A name can be declared by several active frames (`impl<T>` + `fn<T>`),
            // so the union is recomputed rather than difference-removed.
            self.type_parameters = self
                .type_parameter_stack
                .iter()
                .flat_map(|s| s.iter().cloned())
                .collect();
        }
    }

    /// Add a name to the innermost active scope (Python's `T = TypeVar(..)`,
    /// which declares a parameter by assignment rather than in a bracket list).
    pub fn insert_type_parameter(&mut self, name: &str) {
        // Keep the invariant `type_parameters == union(type_parameter_stack)`:
        // an insert with no active frame would be dropped by the next pop.
        if self.type_parameter_stack.is_empty() {
            self.type_parameter_stack.push(HashSet::new());
        }
        if let Some(frame) = self.type_parameter_stack.last_mut() {
            frame.insert(name.to_string());
        }
        self.type_parameters.insert(name.to_string());
    }

    /// The type parameters visible here: the union of every active scope.
    pub fn type_parameters(&self) -> &HashSet<String> {
        &self.type_parameters
    }

    /// UTF-8 source text of a node.
    pub fn text(&self, node: TsNode<'_>) -> Option<String> {
        node.utf8_text(self.src).ok().map(str::to_string)
    }

    /// The declaration slice a collision hash disambiguates by: source from the
    /// def start to the body (the signature), stable across body edits. Falls back
    /// to the whole node when there is no `body` field.
    pub fn decl_text(&self, node: TsNode<'_>) -> String {
        let end = node
            .child_by_field_name("body")
            .map(|b| b.start_byte())
            .unwrap_or_else(|| node.end_byte());
        std::str::from_utf8(&self.src[node.start_byte()..end])
            .unwrap_or("")
            .trim()
            .to_string()
    }

    /// Mint an edit-stable, per-file-unique provisional id for a definition.
    pub fn mint_id(&mut self, prefix: &str, name: &str) -> NodeId {
        let base = format!("{prefix}:{}:{}", self.path, name);
        let n = self.seen.entry(base.clone()).or_insert(0);
        let id = if *n == 0 {
            base.clone()
        } else {
            format!("{base}~{n}")
        };
        *n += 1;
        NodeId::new(id)
    }

    /// Register a locally-defined type for noise filtering exceptions.
    /// Types defined in the current file should not be filtered even if they match denylist patterns.
    pub fn register_local_type(&mut self, type_name: &str) {
        let base_name = type_name.split('<').next().unwrap_or(type_name);
        let clean_name = base_name.rsplit("::").next().unwrap_or(base_name).trim();
        self.local_types.insert(clean_name.to_string());
    }

    /// Create a definition node + a resolved `contains` edge from `container` (the
    /// file for a top-level def; the class for a method). `owner` (the impl/class
    /// type) owner-qualifies the id name (`Owner::name`, ADR-0028) and is stamped
    /// as the `impl` attr; the node *label* stays the bare name. Records the
    /// collision metadata consumed by [`Driver::finish`].
    pub fn add_def(
        &mut self,
        id_prefix: &str,
        kind: &str,
        name: &str,
        node: TsNode<'_>,
        container: &NodeId,
        owner: Option<&str>,
    ) -> NodeId {
        let qualified = idgen::qualify(name, owner);
        let id = self.mint_id(id_prefix, &qualified);
        self.id_meta.insert(
            id.0.clone(),
            IdMeta {
                base: format!("{id_prefix}:{}:{}", self.path, qualified),
                decl: self.decl_text(node),
            },
        );
        let mut n = Node::new(id.0.clone(), name, kind);
        n.source_file = Some(self.path.clone());
        n.source_span = Some(Span {
            start: node.start_position().row as u32 + 1,
            end: node.end_position().row as u32 + 1,
        });
        if let Some(owner) = owner {
            n.attrs.insert(attrs::IMPL.into(), owner.to_string());
        }
        self.edges.push(Edge {
            source: container.clone(),
            relation: CONTAINS.into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Node(id.clone()),
        });
        self.nodes.push(n);
        id
    }

    /// Stamp `attrs["abstract"] = "true"` on an already-added node (ADR-0036 §5).
    ///
    /// The **one** writer of the fact, shared by all five backends, because the
    /// thing being asserted is cross-language: a bodiless method declaration, or
    /// the abstract type that declares one. It is a *node fact*, not a node kind
    /// — see [`filigrio_core::attrs::ABSTRACT`] for why (Kythe's `tag/abstract`
    /// vs SCIP's six kind variants), and note the direct consequence: the kind
    /// stays `function`, so a declaration is a link candidate like any other and
    /// a call through the abstraction binds to the abstraction.
    ///
    /// Searches from the end — every caller stamps a node it has just added, so
    /// this is O(1) in practice.
    pub fn mark_abstract(&mut self, id: &NodeId) {
        if let Some(n) = self.nodes.iter_mut().rev().find(|n| n.id == *id) {
            n.attrs.insert(ABSTRACT.into(), ABSTRACT_TRUE.into());
        }
    }

    /// Emit a `calls` edge (enclosing def → callee) as an unresolved `Symbol`.
    pub fn emit_call(&mut self, source: NodeId, tref: TargetRef) {
        self.edges.push(Edge {
            source,
            relation: CALLS.into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Symbol(tref),
        });
    }

    /// Emit an `imports` edge (file → bound name) as an unresolved `Symbol`.
    pub fn emit_import(&mut self, tref: TargetRef) {
        self.edges.push(Edge {
            source: self.file_id.clone(),
            relation: IMPORTS.into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Symbol(tref),
        });
    }

    // ---- ADR-0036 structural edges (Phase 0b) ------------------------------

    /// Emit a **heritage** edge (`implements` / `extends` / `inherits`) from a
    /// type's def node to the type it names, as an unresolved `Symbol` (bound by
    /// the same relation-agnostic resolver as calls; a foreign target stays
    /// honest-unresolved, ADR-0023). A syntactic declaration is `Extracted`.
    pub fn emit_heritage(&mut self, source: NodeId, relation: &str, target: &str) {
        self.edges.push(Edge {
            source,
            relation: relation.to_string(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Symbol(TargetRef::new(target)),
        });
    }

    /// Emit a `type/field` edge: the owning type → the field's type (unresolved
    /// `Symbol`), carrying the field `name` (and `vis`, when known) as edge attrs.
    /// **No field node** is created (ADR-0036 §1a).
    ///
    /// Applies R1.1 noise filtering: returns whether the edge was emitted
    /// (false if the type was filtered as noise).
    pub fn emit_field_type(
        &mut self,
        source: NodeId,
        name: &str,
        ty: &str,
        vis: Option<&str>,
    ) -> bool {
        self.filter_stats.record_considered();

        // Apply R1.1 noise filtering with local types and type parameters
        if should_filter_type_with_context(
            &self.language,
            ty,
            &self.local_types,
            &self.type_parameters,
        ) {
            self.filter_stats.record_filtered();
            return false;
        }

        let mut tref = TargetRef::new(ty);
        tref.hints.insert("name".into(), name.to_string());
        if let Some(v) = vis {
            tref.hints.insert("vis".into(), v.to_string());
        }
        self.edges.push(Edge {
            source,
            relation: FIELD_TYPE.into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Symbol(tref),
        });
        self.filter_stats.record_emitted();
        true
    }

    /// Emit a `type/param` edge: a function/method's parameter → its declared type
    /// (unresolved `Symbol`). The relation itself encodes the position (ADR-0036).
    ///
    /// Applies R1.1 noise filtering: returns whether the edge was emitted
    /// (false if the type was filtered as noise).
    pub fn emit_param_type(&mut self, source: NodeId, ty: &str) -> bool {
        self.filter_stats.record_considered();

        // Apply R1.1 noise filtering with local types and type parameters
        if should_filter_type_with_context(
            &self.language,
            ty,
            &self.local_types,
            &self.type_parameters,
        ) {
            self.filter_stats.record_filtered();
            return false;
        }

        self.edges.push(Edge {
            source,
            relation: PARAM_TYPE.into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Symbol(TargetRef::new(ty)),
        });
        self.filter_stats.record_emitted();
        true
    }

    /// Emit a `type/return` edge: a function/method's return position → its
    /// declared type (unresolved `Symbol`). The relation itself encodes the
    /// position (ADR-0036).
    ///
    /// Applies R1.1 noise filtering: returns whether the edge was emitted
    /// (false if the type was filtered as noise).
    pub fn emit_return_type(&mut self, source: NodeId, ty: &str) -> bool {
        self.filter_stats.record_considered();

        // Apply R1.1 noise filtering with local types and type parameters
        if should_filter_type_with_context(
            &self.language,
            ty,
            &self.local_types,
            &self.type_parameters,
        ) {
            self.filter_stats.record_filtered();
            return false;
        }

        self.edges.push(Edge {
            source,
            relation: RETURN_TYPE.into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Symbol(TargetRef::new(ty)),
        });
        self.filter_stats.record_emitted();
        true
    }

    /// Emit a `type/bound` edge: a type → a trait bound appearing in its type
    /// parameters or where clause (unresolved `Symbol`). The relation itself
    /// encodes the position (ADR-0036).
    ///
    /// Applies R1.1 noise filtering: returns whether the edge was emitted
    /// (false if the type was filtered as noise).
    pub fn emit_bound_type(&mut self, source: NodeId, ty: &str) -> bool {
        self.filter_stats.record_considered();

        // Apply R1.1 noise filtering with local types and type parameters
        if should_filter_type_with_context(
            &self.language,
            ty,
            &self.local_types,
            &self.type_parameters,
        ) {
            self.filter_stats.record_filtered();
            return false;
        }

        self.edges.push(Edge {
            source,
            relation: BOUND_TYPE.into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Symbol(TargetRef::new(ty)),
        });
        self.filter_stats.record_emitted();
        true
    }

    /// Add an `enum_variant` node (owner-qualified `Enum::Variant`, ADR-0028) and a
    /// resolved `has_variant` edge from the enum's def node to it. The variant is
    /// contained by the enum (via `has_variant`), not the file. Returns the new id.
    pub fn add_variant(
        &mut self,
        enum_id: &NodeId,
        enum_name: &str,
        variant: &str,
        node: TsNode<'_>,
    ) -> NodeId {
        let qualified = idgen::qualify(variant, Some(enum_name));
        let id = self.mint_id("variant", &qualified);
        self.id_meta.insert(
            id.0.clone(),
            IdMeta {
                base: format!("variant:{}:{}", self.path, qualified),
                decl: self.decl_text(node),
            },
        );
        let mut n = Node::new(id.0.clone(), variant, "enum_variant");
        n.source_file = Some(self.path.clone());
        n.source_span = Some(Span {
            start: node.start_position().row as u32 + 1,
            end: node.end_position().row as u32 + 1,
        });
        n.attrs.insert(attrs::IMPL.into(), enum_name.to_string());
        self.edges.push(Edge {
            source: enum_id.clone(),
            relation: HAS_VARIANT.into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Node(id.clone()),
        });
        self.nodes.push(n);
        id
    }

    // ---- scope stack (receiver-type inference) -----------------------------

    pub fn scope_push(&mut self) {
        self.scopes.push(HashMap::new());
    }

    pub fn scope_pop(&mut self) {
        self.scopes.pop();
    }

    pub fn scope_insert(&mut self, var: &str, ty: V) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(var.to_string(), ty);
        }
    }

    /// Type of `var` from the nearest enclosing scope that binds it.
    pub fn lookup_var(&self, var: &str) -> Option<V> {
        self.scopes.iter().rev().find_map(|s| s.get(var).cloned())
    }

    /// Finalize ids (ADR-0028 collision hashing) and assemble the `Extraction`.
    pub fn finish(self) -> Extraction {
        let Driver {
            mut nodes,
            mut edges,
            exports,
            id_meta,
            ..
        } = self;
        idgen::finalize_collisions(&mut nodes, &mut edges, &id_meta);
        Extraction {
            nodes,
            edges,
            exports,
        }
    }
}

/// Node-source-agnostic def/variant emission for the oxc frontend (ADR-0040).
///
/// The tree-sitter-facing [`Driver::add_def`] / [`Driver::add_variant`] take a
/// `tree_sitter::Node` (for span + collision-`decl` text). The oxc backend has no
/// tree-sitter node, so these mirror them exactly but take the already-computed
/// `(start_line, end_line, decl)` instead — reusing the same id-minting, collision
/// metadata, `impl` attr, and `contains`/`has_variant` wiring so oxc-extracted
/// graphs are byte-for-byte shaped like the tree-sitter ones.
///
/// Gated on `ts-oxc`: without the feature this impl block does not exist, so the
/// default build of `driver.rs` is unchanged.
#[cfg(feature = "ts-oxc")]
impl<'a, V: Clone> Driver<'a, V> {
    #[allow(clippy::too_many_arguments)]
    pub fn add_def_raw(
        &mut self,
        id_prefix: &str,
        kind: &str,
        name: &str,
        start_line: u32,
        end_line: u32,
        decl: String,
        container: &NodeId,
        owner: Option<&str>,
    ) -> NodeId {
        let qualified = idgen::qualify(name, owner);
        let id = self.mint_id(id_prefix, &qualified);
        self.id_meta.insert(
            id.0.clone(),
            IdMeta {
                base: format!("{id_prefix}:{}:{}", self.path, qualified),
                decl,
            },
        );
        let mut n = Node::new(id.0.clone(), name, kind);
        n.source_file = Some(self.path.clone());
        n.source_span = Some(Span {
            start: start_line,
            end: end_line,
        });
        if let Some(owner) = owner {
            n.attrs.insert(attrs::IMPL.into(), owner.to_string());
        }
        self.edges.push(Edge {
            source: container.clone(),
            relation: CONTAINS.into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Node(id.clone()),
        });
        self.nodes.push(n);
        id
    }

    pub fn add_variant_raw(
        &mut self,
        enum_id: &NodeId,
        enum_name: &str,
        variant: &str,
        start_line: u32,
        end_line: u32,
        decl: String,
    ) -> NodeId {
        let qualified = idgen::qualify(variant, Some(enum_name));
        let id = self.mint_id("variant", &qualified);
        self.id_meta.insert(
            id.0.clone(),
            IdMeta {
                base: format!("variant:{}:{}", self.path, qualified),
                decl,
            },
        );
        let mut n = Node::new(id.0.clone(), variant, "enum_variant");
        n.source_file = Some(self.path.clone());
        n.source_span = Some(Span {
            start: start_line,
            end: end_line,
        });
        n.attrs.insert(attrs::IMPL.into(), enum_name.to_string());
        self.edges.push(Edge {
            source: enum_id.clone(),
            relation: HAS_VARIANT.into(),
            confidence: Confidence::Extracted,
            target: EdgeTarget::Node(id.clone()),
        });
        self.nodes.push(n);
        id
    }
}
