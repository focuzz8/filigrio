//! Split **capability traits** (ADR-0037 §3), Phase 0a. Rather than one
//! monolithic `LanguageBackend` supertrait, a backend composes small, focused
//! traits and implements only the capabilities it *has* — the optional ones
//! (`HeritageExtractor`, `FieldExtractor`) carry empty defaults, so a language
//! without heritage/fields simply inherits the no-op rather than stubbing
//! `return None` inside a god-trait.
//!
//! **Frontend-agnostic.** The parse-node type is named only through the
//! [`Frontend`] GAT (`type Node<'tree>`), never as a concrete `tree_sitter::Node`.
//! Every Phase-0a backend binds `Node = tree_sitter::Node`, but this is exactly
//! what lets a non-tree-sitter frontend (oxc for TS/JS, ADR-0037 §3 / Phase 0b)
//! implement the same capabilities over its own AST node without touching the
//! trait definitions.
//!
//! Phase 0a is a **behavior-preserving refactor**: these traits relocate the
//! extractors' existing per-language helpers (`base_type_name`, `callee_ref`,
//! receiver inference, `use`/`import` decomposition) behind a shared seam. The
//! ADR-0036 feature slots (`HeritageExtractor`/`FieldExtractor`) are defined but
//! empty here; they are back-filled in Phase 0b.

/// The frontend a backend parses with. A capability names its parse-node type
/// only through this associated GAT, so the capability traits are independent of
/// any particular parser (tree-sitter today; oxc for TS/JS in Phase 0b).
pub trait Frontend {
    /// The backend's parse-node type — `tree_sitter::Node<'tree>` for every
    /// Phase-0a backend. `Copy` so the walk can thread nodes by value.
    type Node<'tree>: Copy;
}

/// One decomposed entry of an import/`use` declaration (ADR-0037 §3
/// `ImportExtractor`): `{specifier, imported, alias}`, plus the glob case that
/// binds no single name. Uniform across languages; a backend that carries no
/// module specifier (Python today) leaves `specifier` `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsedImport {
    /// `use <specifier>::<imported> [as <alias>]` / `import { <imported> as <alias> }`.
    /// `specifier` is `None` for a single-segment `use foo` (or a specifier-less
    /// frontend). `alias` is `Some` only when the bound name differs from `imported`.
    Named {
        specifier: Option<String>,
        imported: String,
        alias: Option<String>,
    },
    /// `use <specifier>::*` — a glob re-export; no single bound name.
    Wildcard { specifier: String },
}

/// parse node → our `(id_prefix, node_kind)`, or `None` when the node is not a
/// definition. Required capability (ADR-0037 §3).
pub trait NodeMapper: Frontend {
    /// `("fn", "function")`, `("type", "struct")`, … for a definition node; `None`
    /// otherwise. The id prefix and the node `kind` label in one place.
    fn def_kind<'t>(&self, node: Self::Node<'t>) -> Option<(&'static str, &'static str)>;
}

/// type-ref node → bare target name (today's `base_type_name`): unwrap
/// references/generics/qualifiers to a single identifier. Required capability.
pub trait TypeNamer: Frontend {
    fn type_name<'t>(&self, node: Self::Node<'t>) -> Option<String>;
}

/// import node → the entries it binds (ADR-0037 §3). Required capability.
pub trait ImportExtractor: Frontend {
    fn imports<'t>(&self, node: Self::Node<'t>) -> Vec<ParsedImport>;
}

/// Receiver typing for a call site (ADR-0037 §3): the callee name + a receiver
/// type hint, and the inferred type of a method-call receiver. The associated
/// [`ReceiverTyper::Receiver`] carries each backend's **policy** — Rust binds an
/// enum expressing the ADR-0023 opaque-decline / ADR-0026 return-type-defer
/// choices; TS/Python bind a bare type name (`Option<String>`) with the
/// bare-name-fallback (TS) / drop-on-unknown (Python) policy applied in the walk.
pub trait ReceiverTyper: Frontend {
    /// The backend's inferred-receiver representation (see the trait docs).
    type Receiver;

    /// `(callee name, receiver-type hint)` for a call's `function` node. `owner`
    /// is the enclosing type (impl/class), used by backends that resolve the hint
    /// eagerly (TS/Python); Rust returns a syntactic qualifier and ignores it.
    fn callee_ref<'t>(
        &self,
        func: Self::Node<'t>,
        owner: Option<&str>,
    ) -> Option<(String, Option<String>)>;

    /// The inferred type of a method-call receiver expression.
    fn receiver_type<'t>(&self, recv_expr: Self::Node<'t>, owner: Option<&str>) -> Self::Receiver;
}

/// A heritage relation (`extends`/`implements`/`inherits`) a type declares
/// (ADR-0036). Produced by all three backends: Rust (ADR-0037a),
/// TypeScript (ADR-0037b), and Python (ADR-0037c).
pub struct Heritage {
    pub relation: &'static str,
    pub target: String,
}

/// A declared field `(name, type-ref, visibility)` for a `type/field` edge
/// (ADR-0036 §1a — a field is an edge, not a node).
pub struct Field {
    pub name: String,
    pub type_name: String,
    pub visibility: Option<String>,
}

/// type → heritage refs (ADR-0036). **Optional** — the empty default is the
/// ADR-0037 §3 contract for a future language without heritage, which then
/// provides nothing rather than stubbing. All three current backends override
/// it: Rust (ADR-0037a), TypeScript (ADR-0037b), Python (ADR-0037c).
pub trait HeritageExtractor: Frontend {
    fn heritage<'t>(&self, _node: Self::Node<'t>) -> Vec<Heritage> {
        Vec::new()
    }
}

/// type → declared fields (ADR-0036 §1a). **Optional** — the empty default is
/// the ADR-0037 §3 contract for a future language without declared fields. All
/// three current backends override it: Rust (ADR-0037a), TypeScript (ADR-0037b),
/// Python (ADR-0037c).
pub trait FieldExtractor: Frontend {
    fn fields<'t>(&self, _node: Self::Node<'t>) -> Vec<Field> {
        Vec::new()
    }

    fn tuple_fields<'t>(&self, _node: Self::Node<'t>) -> Vec<Self::Node<'t>> {
        Vec::new()
    }
}
