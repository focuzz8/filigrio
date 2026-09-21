//! `RustExtractor` — a **real** tree-sitter extractor for Rust (Phase 1).
//!
//! Emits, per file:
//!   * a `file` node;
//!   * `function` / `struct` / `enum` / `trait` definition nodes (methods in
//!     `impl` blocks are functions tagged with an `impl` owner attr);
//!   * `contains` edges file → definition (already resolved);
//!   * `calls` edges (enclosing fn → callee) as UNRESOLVED `Symbol` targets;
//!   * `imports` edges file → used path as `Symbol` targets.
//!
//! Node ids are semantic and edit-stable (no line numbers): `kind:path:name`, with
//! methods owner-qualified (`fn:path:Owner::name`) and a `#hash` of the signature
//! appended only for a true same-owner overload (ADR-0028, minted in `idgen`).
//! Cross-file resolution is `filigrio-resolve`'s job (parse-then-link).
//!
//! **Receiver-type hints** (to disambiguate method homonyms downstream): a
//! `calls` edge carries `hints["type"]` when the call's receiver type is known —
//! an associated call `T::method()` (`Self` → enclosing impl), or a value call
//! `x.method()` / `self.method()` whose receiver type is inferred from a small
//! **local dataflow** pass (parameter types, `let` annotations, and constructor /
//! struct-literal RHS, tracked in a per-function scope stack).
//!
//! A binding to a **plain call** — `let x = compute()` / `Foo::make()` — cannot be
//! typed here (the callee's return type may live in another file), so instead of
//! marking the receiver opaque the edge carries deferred `hints["recv_returns"]`
//! (+ `recv_returns_owner` for the associated form); `filigrio-resolve` reads the
//! callee's stamped `returns` attr and narrows cross-file (ADR-0026). Every fn
//! node also carries `attrs["returns"]` (its declared `-> T`). When the type is
//! still not knowable (method chains, opaque receivers) `hints["recv"]="opaque"`
//! is emitted so resolution declines rather than guessing (ADR-0023) — better an
//! honest unresolved than a wrong bind.
//!
//! **Structure (ADR-0037 §3, Phase 0a).** Shared plumbing (id minting, node/edge
//! emission, the `scopes` stack, `Extraction` assembly) lives in the generic
//! [`Driver`]; this backend supplies the split capability traits ([`NodeMapper`],
//! [`TypeNamer`], [`ReceiverTyper`], [`ImportExtractor`], with empty
//! `Heritage`/`Field` slots for Phase 0b) plus the Rust-specific walk.

use crate::capability::{
    Field, FieldExtractor, Frontend, Heritage, HeritageExtractor, ImportExtractor, NodeMapper,
    ParsedImport, ReceiverTyper, TypeNamer,
};
use crate::driver::Driver;
use filigrio_core::relation::{CONTAINS, EXTENDS, IMPLEMENTS};
use filigrio_core::{
    Artifact, EdgeTarget, Export, Extraction, Extractor, NodeId, Result, TargetRef,
};
use std::collections::{HashMap, HashSet};
use tree_sitter::{Node as TsNode, Parser};

// Types named only by the test module
use filigrio_core::attrs;
#[cfg(test)]
use filigrio_core::{Node, Span};

pub struct RustExtractor;

impl RustExtractor {
    pub fn new() -> Self {
        RustExtractor
    }
}

impl Default for RustExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for RustExtractor {
    fn handles(&self, artifact: &Artifact) -> bool {
        artifact.language.as_deref() == Some("rust")
    }

    fn extract(&self, artifact: &Artifact, bytes: &[u8]) -> Result<Extraction> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .map_err(|e| filigrio_core::Error::Parse(format!("load rust grammar: {e}")))?;
        let tree = parser.parse(bytes, None).ok_or_else(|| {
            filigrio_core::Error::Parse(format!("parse failed: {}", artifact.path))
        })?;

        let mut ctx = Ctx::new(artifact, bytes);
        ctx.walk(tree.root_node(), None, None);
        // `implements` edges are sourced from the impl'd type's def node, which may
        // be declared after the `impl` block, so they are resolved post-walk once
        // every type node is registered (ADR-0037a).
        ctx.resolve_pending_impls();
        // Re-parent impl-block methods from the file to their type's node, now that
        // every type node is registered (ADR-0036 #4). Runs before `finish` so the
        // rewritten edges are subject to the same ADR-0028 collision remap.
        ctx.resolve_pending_method_containment();
        // ADR-0028 collision finalize + `Extraction` assembly live in the driver.
        Ok(ctx.d.finish())
    }
}

/// Rust backend: the shared [`Driver`] (scope value = [`VarType`]) plus the
/// capability-trait impls below and the Rust-specific walk.
struct Ctx<'a> {
    d: Driver<'a, VarType>,
    /// Locally-defined type name → its def-node id, so an `impl Trait for Type`
    /// can source its `implements` edge from `Type`'s node (ADR-0037a). Populated
    /// as type nodes are created; consumed in [`Ctx::resolve_pending_impls`].
    type_ids: HashMap<String, NodeId>,
    /// `(impl'd type name, trait target)` collected at each `impl_item`, resolved
    /// to `implements` edges after the walk (the type may be declared later).
    pending_impls: Vec<(String, String)>,
    /// `(impl'd base type name, method node id)` for every method emitted inside an
    /// `impl` block. Methods are first file-contained by `add_def` (like free fns);
    /// after the walk — once every type node is registered — the `contains` edge of
    /// each method whose type has a local node is re-sourced from that type node, so
    /// a method is contained by its `struct`/`enum`/`trait` rather than the file
    /// (matching TS/Python class-method containment). Deferred, not eager, because
    /// `impl Foo` may precede `struct Foo` (`type_ids` is only complete post-walk).
    pending_method_containers: Vec<(String, NodeId)>,
    /// Track impls that declined to emit bound_type edges, recording the self type
    /// and reason for declining (used for validation and debugging).
    declined_impls: Vec<(String, String)>,
    /// The enclosing `trait` `(node id, name)` while its body is walked — the
    /// owner/container a bodiless `function_signature_item` is attached to
    /// (ADR-0036 §5). `None` everywhere else, which is exactly what keeps an
    /// `extern` block's `fn …;` from being mistaken for a trait declaration.
    current_trait: Option<(NodeId, String)>,
}

/// The inferred type of a local binding. `Concrete` is a known type name (from an
/// annotation, a constructor-convention call, a struct literal, or a parameter).
/// `ReturnOf` is **deferred**: the type is whatever a free/associated call
/// returns — resolved cross-file in `filigrio-resolve`, which alone sees every
/// fn's declared return type (single-hop; method-chain receivers stay opaque).
#[derive(Clone)]
enum VarType {
    Concrete(String),
    ReturnOf {
        callee: String,
        owner: Option<String>,
    },
}

impl Frontend for Ctx<'_> {
    type Node<'tree> = TsNode<'tree>;
}

impl NodeMapper for Ctx<'_> {
    /// `function_item`→function; `struct_item`/`union_item`→struct; `enum_item`→
    /// enum; `trait_item`→trait. `impl_item` is owner context, not a node.
    fn def_kind<'t>(&self, node: TsNode<'t>) -> Option<(&'static str, &'static str)> {
        Some(match node.kind() {
            "function_item" => ("fn", "function"),
            "struct_item" | "union_item" => ("type", "struct"),
            "enum_item" => ("type", "enum"),
            "trait_item" => ("type", "trait"),
            _ => return None,
        })
    }
}

impl TypeNamer for Ctx<'_> {
    /// The base type identifier of a type node, unwrapping references and
    /// generics and dropping path qualifiers: `&mut Foo` / `Box<Foo>` /
    /// `a::Foo` → `Foo` / `Box` (generic base) / `Foo`.
    fn type_name<'t>(&self, ty: TsNode<'t>) -> Option<String> {
        match ty.kind() {
            "type_identifier" => self.d.text(ty),
            "reference_type" => ty
                .child_by_field_name("type")
                .and_then(|t| self.type_name(t)),
            "generic_type" => ty
                .child_by_field_name("type")
                .and_then(|t| self.type_name(t)),
            "scoped_type_identifier" => {
                let text = self.d.text(ty)?;

                // Local paths (crate::, self::, super::, Self::) → strip to last segment (definitionally local)
                // Everything else → keep qualified (foreign until proven otherwise)
                if text.starts_with("crate::")
                    || text.starts_with("self::")
                    || text.starts_with("super::")
                    || text.starts_with("Self::")
                {
                    // Local paths: strip to last segment
                    text.rsplit("::").next().map(str::to_string)
                } else {
                    // Everything else: keep qualified (std::, third-party crates, etc.)
                    Some(text)
                }
            }
            _ => None,
        }
    }
}

/// Check if a tree-sitter node kind represents a type node that should be processed
/// (as opposed to attribute nodes, comments, etc.)
fn is_type_node(kind: &str) -> bool {
    matches!(
        kind,
        "type_identifier"
            | "scoped_type_identifier"
            | "generic_type"
            | "reference_type"
            | "pointer_type"
            | "array_type"
            | "tuple_type"
            | "slice_type"
    )
}

impl<'a> Ctx<'a> {
    /// Recursively collect type references from a Rust type expression,
    /// matching Python's `_rust_collect_type_refs()` behavior.
    ///
    /// Walks type expressions and appends (name, role) tuples where:
    /// - role = "type" for top-level type identifiers
    /// - role = "generic_arg" for type arguments inside generics
    ///
    /// Mirrors the Python implementation that walks:
    /// - `type_identifier`, `scoped_type_identifier`: adds (name, role)
    /// - `generic_type`: adds base type (name, role), then walks `type_arguments` with role = "generic_arg"
    /// - `reference_type`, `pointer_type`, `array_type`, `tuple_type`, `slice_type`: recursively walks named children
    /// - `primitive_type`: skipped
    /// - Other named nodes: recursively walks named children
    fn collect_type_refs_recursive(
        &self,
        node: TsNode<'_>,
        generic: bool,
        out: &mut Vec<(String, String)>,
    ) {
        let kind = node.kind();

        match kind {
            "primitive_type" => {
                // Skip primitive types - no type reference emitted
            }
            "type_identifier" => {
                if let Some(text) = self.d.text(node) {
                    out.push((
                        text,
                        if generic { "generic_arg" } else { "type" }.to_string(),
                    ));
                }
            }
            "scoped_type_identifier" => {
                // Trait projections — `<T as Iterator>::Item` — carry a *second*
                // type reference the final segment alone loses: the trait. The
                // qualified form nests as
                // `scoped_type_identifier(bracketed_type(qualified_type(T, "as", Trait)), "::", Item)`,
                // so the trait is whatever sits after the `as`.
                //
                // That position is a full type, not a name: `<T as Iterator>::Item`
                // parses it as a `type_identifier`, but `<T as Into<Widget>>::Target`
                // parses it as a `generic_type` and `<T as a::B>::C` as a
                // `scoped_type_identifier`. Matching one node kind here silently
                // dropped every projection through a generic trait (and its type
                // arguments with it), so the node goes back through **this**
                // function, which already knows every type shape.
                let mut cursor = node.walk();

                for child in node.children(&mut cursor) {
                    if child.kind() == "bracketed_type" {
                        // Look for qualified_type with "as" inside bracketed_type
                        let mut bracketed_cursor = child.walk();
                        for bracketed_child in child.children(&mut bracketed_cursor) {
                            if bracketed_child.kind() == "qualified_type" {
                                let mut found_as = false;
                                for qualified_child in
                                    bracketed_child.children(&mut bracketed_child.walk())
                                {
                                    if found_as && qualified_child.is_named() {
                                        self.collect_type_refs_recursive(
                                            qualified_child,
                                            generic,
                                            out,
                                        );
                                        break;
                                    }
                                    if qualified_child.kind() == "as" {
                                        found_as = true;
                                    }
                                }
                                if found_as {
                                    break;
                                }
                            }
                        }
                        break;
                    }
                }

                // Extract the qualified path
                // Local paths (crate::, self::, super::, Self::) → strip to last segment (definitionally local)
                // Everything else → keep qualified (foreign until proven otherwise)
                // Exception: trait projections (contain <>) → extract last segment only
                if let Some(text) = self.d.text(node) {
                    let extracted_name = if text.contains('<') && text.contains('>') {
                        // Trait projection: extract final segment only
                        text.rsplit("::")
                            .next()
                            .map(|s| s.to_string())
                            .unwrap_or(text)
                    } else if text.starts_with("crate::")
                        || text.starts_with("self::")
                        || text.starts_with("super::")
                        || text.starts_with("Self::")
                    {
                        // Local paths: strip to last segment
                        text.rsplit("::")
                            .next()
                            .map(|s| s.to_string())
                            .unwrap_or(text)
                    } else {
                        // Everything else: keep qualified (std::, third-party crates, etc.)
                        text
                    };

                    out.push((
                        extracted_name,
                        if generic { "generic_arg" } else { "type" }.to_string(),
                    ));
                }
            }
            "generic_type" => {
                // Get the base type name
                let base_name = node
                    .child_by_field_name("type")
                    .and_then(|t| self.type_name(t))
                    .or_else(|| {
                        // Fallback: look for type_identifier or scoped_type_identifier among children
                        let mut cursor = node.walk();
                        for child in node.children(&mut cursor) {
                            if child.kind() == "type_identifier"
                                || child.kind() == "scoped_type_identifier"
                            {
                                return self.type_name(child);
                            }
                        }
                        None
                    });

                // Add base type reference
                if let Some(name) = base_name {
                    out.push((
                        name,
                        if generic { "generic_arg" } else { "type" }.to_string(),
                    ));
                }

                // Recursively walk type_arguments with generic = true
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "type_arguments" {
                        let mut arg_cursor = child.walk();
                        for arg in child.children(&mut arg_cursor) {
                            if arg.is_named() {
                                self.collect_type_refs_recursive(arg, true, out);
                            }
                        }
                    }
                }
            }
            "reference_type" | "pointer_type" | "array_type" | "tuple_type" | "slice_type" => {
                // Recursively walk named children
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.is_named() {
                        self.collect_type_refs_recursive(child, generic, out);
                    }
                }
            }
            _ => {
                // For any other named node, recursively walk named children
                if node.is_named() {
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        if child.is_named() {
                            self.collect_type_refs_recursive(child, generic, out);
                        }
                    }
                }
            }
        }
    }
}

impl ReceiverTyper for Ctx<'_> {
    /// Rust receivers carry the ADR-0023/0026 policy (concrete / deferred / opaque).
    type Receiver = Option<VarType>;

    /// Extract `(callee name, receiver-type hint)` from a `call_expression`'s
    /// `function` node. The hint is the type qualifier of an associated call
    /// `Type::method()` — the key to disambiguating constructor homonyms
    /// (`S::new` vs `T::new`). A path whose qualifier is a module (lower-case by
    /// Rust convention), or a plain/method call, yields no hint. `owner` is unused
    /// (Rust returns a *syntactic* qualifier; the walk resolves `Self`).
    fn callee_ref<'t>(
        &self,
        func: TsNode<'t>,
        _owner: Option<&str>,
    ) -> Option<(String, Option<String>)> {
        match func.kind() {
            "identifier" | "field_identifier" | "type_identifier" => {
                self.d.text(func).map(|n| (n, None))
            }
            // `path::name()` — capture the qualifier as a type hint only when it
            // looks like a type (UpperCamelCase), so `S::new` hints `S` but
            // `mem::swap` (a module path) does not.
            "scoped_identifier" => {
                let name = func
                    .child_by_field_name("name")
                    .and_then(|n| self.d.text(n))
                    .or_else(|| self.d.text(func))?;
                let hint = func
                    .child_by_field_name("path")
                    .and_then(|p| self.d.text(p))
                    .and_then(|p| p.rsplit("::").next().map(str::to_string))
                    .filter(|seg| seg.chars().next().is_some_and(|c| c.is_uppercase()));
                Some((name, hint))
            }
            // receiver.method() → method name; the receiver's type is unknown
            // without inference (Phase-4), so no hint.
            "field_expression" => func
                .child_by_field_name("field")
                .and_then(|n| self.d.text(n))
                .map(|n| (n, None)),
            // turbofish etc.
            "generic_function" => func
                .child_by_field_name("function")
                .and_then(|n| self.callee_ref(n, None)),
            _ => None,
        }
    }

    /// The inferred type of a `field_expression` receiver (`recv.method()`):
    /// `self` → the enclosing impl, a bound local → its recorded type (concrete or
    /// deferred), anything else (a chained/complex receiver) → unknown.
    fn receiver_type<'t>(
        &self,
        field_expr: TsNode<'t>,
        current_impl: Option<&str>,
    ) -> Option<VarType> {
        let recv = field_expr.child_by_field_name("value")?;
        match recv.kind() {
            "self" => current_impl.map(|s| VarType::Concrete(s.to_string())),
            "identifier" => {
                let name = self.d.text(recv)?;
                if name == "self" {
                    current_impl.map(|s| VarType::Concrete(s.to_string()))
                } else {
                    self.d.lookup_var(&name)
                }
            }
            _ => None,
        }
    }
}

impl ImportExtractor for Ctx<'_> {
    /// Parse a `use` declaration's `argument` into its entries. A module path is
    /// split into `(specifier, imported)` — the module mod-path (`crate::api`)
    /// and the symbol (`greet`) — so the resolver can bind it to the target
    /// module and the export graph can follow `pub use` re-exports (ADR-0020).
    /// Grouped `use a::{b, c}` and globs `use a::*` are handled; unknown shapes
    /// degrade to a best-effort last-segment import with no specifier.
    fn imports<'t>(&self, arg: TsNode<'t>) -> Vec<ParsedImport> {
        match arg.kind() {
            "identifier" | "type_identifier" | "scoped_identifier" => self
                .d
                .text(arg)
                .map(|full| vec![split_use(&full, None)])
                .unwrap_or_default(),
            "use_as_clause" => {
                let path = arg.child_by_field_name("path").and_then(|p| self.d.text(p));
                let alias = arg
                    .child_by_field_name("alias")
                    .and_then(|a| self.d.text(a));
                match path {
                    Some(full) => vec![split_use(&full, alias)],
                    None => Vec::new(),
                }
            }
            "use_wildcard" => self
                .d
                .text(arg)
                .map(|t| {
                    t.trim_end_matches('*')
                        .trim_end_matches("::")
                        .trim()
                        .to_string()
                })
                .filter(|spec| !spec.is_empty())
                .map(|specifier| vec![ParsedImport::Wildcard { specifier }])
                .unwrap_or_default(),
            // `crate::api::{greet, wave}` — a prefix over a list of entries.
            "scoped_use_list" => {
                let prefix = arg.child_by_field_name("path").and_then(|p| self.d.text(p));
                let mut out = Vec::new();
                if let Some(list) = arg.child_by_field_name("list") {
                    let mut cursor = list.walk();
                    for item in list.named_children(&mut cursor) {
                        for mut u in self.imports(item) {
                            prepend(&mut u, prefix.as_deref());
                            out.push(u);
                        }
                    }
                }
                out
            }
            "use_list" => {
                let mut out = Vec::new();
                let mut cursor = arg.walk();
                for item in arg.named_children(&mut cursor) {
                    out.extend(self.imports(item));
                }
                out
            }
            _ => self
                .last_path_segment(arg)
                .map(|imported| {
                    vec![ParsedImport::Named {
                        specifier: None,
                        imported,
                        alias: None,
                    }]
                })
                .unwrap_or_default(),
        }
    }
}

impl HeritageExtractor for Ctx<'_> {
    /// The heritage a type declares (ADR-0036):
    ///   * `trait_item` supertrait bounds (`trait A: B + C`) → `extends` per bound;
    ///   * `impl_item`'s `trait` field (`impl Foo for Bar`) → `implements Foo`.
    ///
    /// Lifetimes and higher-ranked bounds carry no nameable type, so `type_name`
    /// drops them. A blanket/`dyn` target is handled at the call site (the source
    /// type isn't a concrete local node), not here.
    fn heritage<'t>(&self, node: TsNode<'t>) -> Vec<Heritage> {
        match node.kind() {
            "trait_item" => match node.child_by_field_name("bounds") {
                Some(bounds) => {
                    let mut cursor = bounds.walk();
                    bounds
                        .named_children(&mut cursor)
                        .filter_map(|b| self.type_name(b))
                        .map(|target| Heritage {
                            relation: EXTENDS,
                            target,
                        })
                        .collect()
                }
                None => Vec::new(),
            },
            "impl_item" => node
                .child_by_field_name("trait")
                .and_then(|t| self.type_name(t))
                .map(|target| {
                    vec![Heritage {
                        relation: IMPLEMENTS,
                        target,
                    }]
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

impl FieldExtractor for Ctx<'_> {
    /// The named fields of a `struct_item`/`union_item` (`field_declaration_list`),
    /// each as `(name, base-type-name, visibility)` for a `type/field` edge
    /// (ADR-0036 §1a). Tuple structs (`ordered_field_declaration_list`) carry no
    /// field names and are skipped; a primitive-typed field yields no nameable
    /// target (`type_name` returns `None`) and is dropped.
    fn fields<'t>(&self, node: TsNode<'t>) -> Vec<Field> {
        let Some(body) = node.child_by_field_name("body") else {
            return Vec::new();
        };
        if body.kind() != "field_declaration_list" {
            return Vec::new();
        }
        let mut cursor = body.walk();
        body.named_children(&mut cursor)
            .filter(|c| c.kind() == "field_declaration")
            .filter_map(|fd| {
                let name = fd
                    .child_by_field_name("name")
                    .and_then(|n| self.d.text(n))?;
                let type_name = fd
                    .child_by_field_name("type")
                    .and_then(|t| self.type_name(t))?;
                let mut vc = fd.walk();
                let visibility = fd
                    .children(&mut vc)
                    .find(|c| c.kind() == "visibility_modifier")
                    .and_then(|v| self.d.text(v));
                Some(Field {
                    name,
                    type_name,
                    visibility,
                })
            })
            .collect()
    }

    fn tuple_fields<'t>(&self, node: Self::Node<'t>) -> Vec<Self::Node<'t>> {
        let Some(body) = node.child_by_field_name("body") else {
            return Vec::new();
        };
        if body.kind() != "ordered_field_declaration_list" {
            return Vec::new();
        }
        let mut cursor = body.walk();
        body.named_children(&mut cursor).collect()
    }
}

impl<'a> Ctx<'a> {
    fn new(artifact: &Artifact, src: &'a [u8]) -> Self {
        Ctx {
            d: Driver::new(artifact, src, "rust"),
            type_ids: HashMap::new(),
            pending_impls: Vec::new(),
            pending_method_containers: Vec::new(),
            declined_impls: Vec::new(),
            current_trait: None,
        }
    }

    /// Extract type parameter names from a `type_parameters` node.
    fn collect_type_parameters(&self, node: TsNode<'_>) -> HashSet<String> {
        let mut params = HashSet::new();
        let mut cursor = node.walk();

        // Walk through all children to find type parameter declarations
        for child in node.children(&mut cursor) {
            match child.kind() {
                "type_parameter" => {
                    // Get the name of the type parameter from its type_identifier child
                    if let Some(name_node) = child.child_by_field_name("name") {
                        if let Some(name) = self.d.text(name_node) {
                            params.insert(name);
                        }
                    }
                }
                // Recursively search in nested structures
                _ => {
                    if child.is_named() {
                        params.extend(self.collect_type_parameters(child));
                    }
                }
            }
        }
        params
    }

    #[allow(dead_code)]
    /// Get debugging statistics about declined impl bound_type edges.
    /// Returns (declined_count, impl_details) where impl_details is a Vec of (self_type, reason).
    fn get_declined_impls_stats(&self) -> (usize, Vec<(String, String)>) {
        (self.declined_impls.len(), self.declined_impls.clone())
    }

    /// Collect trait bound type names from a `type_parameters` node.
    /// Skips `?Sized` bounds (removed_trait_bound nodes) and lifetime bounds.
    /// Uses collect_type_refs_recursive to ensure generic arguments and function types are handled correctly.
    fn collect_trait_bounds(&self, node: TsNode<'_>) -> Vec<String> {
        let mut bounds = Vec::new();
        let mut cursor = node.walk();

        for child in node.children(&mut cursor) {
            match child.kind() {
                "type_parameter" => {
                    // Process trait_bounds children of type_parameter
                    let mut param_cursor = child.walk();
                    for param_child in child.children(&mut param_cursor) {
                        if param_child.kind() == "trait_bounds" {
                            // Process each child of trait_bounds using collect_type_refs_recursive
                            let mut bounds_cursor = param_child.walk();
                            for bounds_child in param_child.children(&mut bounds_cursor) {
                                // Skip removed_trait_bound nodes (?Sized bounds)
                                if bounds_child.kind() == "removed_trait_bound" {
                                    continue;
                                }
                                if bounds_child.is_named() {
                                    // Use the verified collect_type_refs_recursive function
                                    // This properly handles generic_type, function_type, associated_type, etc.
                                    let mut refs = Vec::new();
                                    self.collect_type_refs_recursive(
                                        bounds_child,
                                        false,
                                        &mut refs,
                                    );
                                    for (ref_name, _role) in refs {
                                        bounds.push(ref_name);
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {
                    if child.is_named() {
                        bounds.extend(self.collect_trait_bounds(child));
                    }
                }
            }
        }
        bounds
    }

    /// Collect trait bound type names from a `type_parameters` node with coverage tracking.
    /// Returns (bounds_vec, sites_processed) where sites_processed is the count of type_parameter sites visited.
    #[allow(dead_code)]
    fn collect_trait_bounds_with_coverage(&self, node: TsNode<'_>) -> (Vec<String>, usize) {
        let mut bounds = Vec::new();
        let mut sites_processed = 0;
        let mut cursor = node.walk();

        for child in node.children(&mut cursor) {
            match child.kind() {
                "type_parameter" => {
                    sites_processed += 1;
                    // Process trait_bounds children of type_parameter
                    let mut param_cursor = child.walk();
                    for param_child in child.children(&mut param_cursor) {
                        if param_child.kind() == "trait_bounds" {
                            // Process each child of trait_bounds using collect_type_refs_recursive
                            let mut bounds_cursor = param_child.walk();
                            for bounds_child in param_child.children(&mut bounds_cursor) {
                                // Skip removed_trait_bound nodes (?Sized bounds)
                                if bounds_child.kind() == "removed_trait_bound" {
                                    continue;
                                }
                                if bounds_child.is_named() {
                                    // Use the verified collect_type_refs_recursive function
                                    // This properly handles generic_type, function_type, associated_type, etc.
                                    let mut refs = Vec::new();
                                    self.collect_type_refs_recursive(
                                        bounds_child,
                                        false,
                                        &mut refs,
                                    );
                                    for (ref_name, _role) in refs {
                                        bounds.push(ref_name);
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {
                    if child.is_named() {
                        let (nested_bounds, nested_sites) =
                            self.collect_trait_bounds_with_coverage(child);
                        bounds.extend(nested_bounds);
                        sites_processed += nested_sites;
                    }
                }
            }
        }
        (bounds, sites_processed)
    }

    /// Collect trait bound type names from a `where_clause` node.
    /// Uses collect_type_refs_recursive to ensure generic arguments and function types are handled correctly.
    fn collect_where_clause_bounds(&self, node: TsNode<'_>) -> Vec<String> {
        let mut bounds = Vec::new();
        let mut cursor = node.walk();

        for child in node.children(&mut cursor) {
            if child.kind() == "where_predicate" {
                // Extract trait bounds from where predicates
                let mut pred_cursor = child.walk();
                for pred_child in child.children(&mut pred_cursor) {
                    if pred_child.kind() == "trait_bounds" {
                        // Process each child of trait_bounds using collect_type_refs_recursive
                        let mut bounds_cursor = pred_child.walk();
                        for bounds_child in pred_child.children(&mut bounds_cursor) {
                            if bounds_child.is_named() {
                                // Use the verified collect_type_refs_recursive function
                                let mut refs = Vec::new();
                                self.collect_type_refs_recursive(bounds_child, false, &mut refs);
                                for (ref_name, _role) in refs {
                                    bounds.push(ref_name);
                                }
                            }
                        }
                    }
                }
            }
        }
        bounds
    }

    /// Collect trait bound type names from a `where_clause` node with coverage tracking.
    /// Returns (bounds_vec, predicates_processed) where predicates_processed is the count of where_predicate sites visited.
    #[allow(dead_code)]
    fn collect_where_clause_bounds_with_coverage(&self, node: TsNode<'_>) -> (Vec<String>, usize) {
        let mut bounds = Vec::new();
        let mut predicates_processed = 0;
        let mut cursor = node.walk();

        for child in node.children(&mut cursor) {
            if child.kind() == "where_predicate" {
                predicates_processed += 1;
                // Extract trait bounds from where predicates
                let mut pred_cursor = child.walk();
                for pred_child in child.children(&mut pred_cursor) {
                    if pred_child.kind() == "trait_bounds" {
                        // Process each child of trait_bounds using collect_type_refs_recursive
                        let mut bounds_cursor = pred_child.walk();
                        for bounds_child in pred_child.children(&mut bounds_cursor) {
                            // Skip removed_trait_bound nodes (?Sized bounds)
                            if bounds_child.kind() == "removed_trait_bound" {
                                continue;
                            }
                            if bounds_child.is_named() {
                                // Use the verified collect_type_refs_recursive function
                                let mut refs = Vec::new();
                                self.collect_type_refs_recursive(bounds_child, false, &mut refs);
                                for (ref_name, _role) in refs {
                                    bounds.push(ref_name);
                                }
                            }
                        }
                    }
                }
            }
        }
        (bounds, predicates_processed)
    }

    /// Emit bound_type edges for a given source node from its type_parameters and where_clause.
    /// This must be called AFTER type parameters are pushed to the R1.1 stack.
    fn emit_bound_type_edges(&mut self, source_id: &NodeId, node: TsNode<'_>) {
        // Collect bounds from type_parameters
        if let Some(type_params) = node.child_by_field_name("type_parameters") {
            let bounds = self.collect_trait_bounds(type_params);
            for bound in bounds {
                let normalized_bound = base_type_name(&bound);
                let _ = self.d.emit_bound_type(source_id.clone(), normalized_bound);
            }
        }

        // Collect bounds from where_clause (search by kind, not field name)
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "where_clause" {
                let bounds = self.collect_where_clause_bounds(child);
                for bound in bounds {
                    let normalized_bound = base_type_name(&bound);
                    let _ = self.d.emit_bound_type(source_id.clone(), normalized_bound);
                }
                break; // Only one where_clause expected
            }
        }

        // Special handling for trait items: collect associated type bounds
        if node.kind() == "trait_item" || node.kind() == "trait" {
            if let Some(body) = node.child_by_field_name("body") {
                let assoc_type_bounds = self.collect_associated_type_bounds(body);
                for bound in assoc_type_bounds {
                    let normalized_bound = base_type_name(&bound);
                    let _ = self.d.emit_bound_type(source_id.clone(), normalized_bound);
                }
            }
        }
    }

    /// Collect trait bounds from associated types in trait bodies.
    /// Uses collect_type_refs_recursive to ensure generic arguments and function types are handled correctly.
    fn collect_associated_type_bounds(&self, node: TsNode<'_>) -> Vec<String> {
        let mut bounds = Vec::new();
        let mut cursor = node.walk();

        for child in node.children(&mut cursor) {
            if child.kind() == "associated_type" {
                // This is an associated type declaration like `type Item: Display;`
                // Collect trait bounds from the associated_type
                let mut item_cursor = child.walk();
                for item_child in child.children(&mut item_cursor) {
                    if item_child.kind() == "trait_bounds" {
                        // Use the verified collect_type_refs_recursive function
                        let mut refs = Vec::new();
                        self.collect_type_refs_recursive(item_child, false, &mut refs);
                        for (ref_name, _role) in refs {
                            let normalized = base_type_name(&ref_name);
                            if !bounds.iter().any(|s: &String| s.as_str() == normalized) {
                                bounds.push(normalized.to_string());
                            }
                        }
                    }
                }
            }
        }
        bounds
    }

    /// Bind each collected `impl Trait for Type` to an `implements` edge sourced
    /// from `Type`'s def node. A type with no local node — a blanket `impl<T> …
    /// for T` (target is a generic param) or `impl … for ForeignType` — is left
    /// honest-unresolved: no edge is anchored rather than a guessed one (ADR-0023).
    fn resolve_pending_impls(&mut self) {
        for (ty_name, target) in std::mem::take(&mut self.pending_impls) {
            if let Some(src) = self.type_ids.get(&ty_name).cloned() {
                self.d.emit_heritage(src, IMPLEMENTS, &target);
            }
        }
    }

    /// Re-source each impl-block method's `contains` edge from the file to its
    /// impl type's def node, so a `struct`/`enum`/`trait` contains its methods
    /// (ADR-0036 #4) — matching how TS/Python contain class methods. A method
    /// whose impl type has **no local node** (`impl SomeTrait for ForeignType`, a
    /// blanket `impl<T> … for T`, an impl over an undeclared type) is left
    /// file-contained: no type node to anchor to, so nothing is invented
    /// (ADR-0023). The method node itself and its `impl` attr are unchanged; only
    /// the `contains` edge's *source* moves.
    fn resolve_pending_method_containment(&mut self) {
        let file_id = self.d.file_id.clone();
        for (owner, method_id) in std::mem::take(&mut self.pending_method_containers) {
            let Some(type_id) = self.type_ids.get(&owner).cloned() else {
                continue; // foreign / blanket / undeclared type → keep file container
            };
            for e in self.d.edges.iter_mut() {
                if e.relation == CONTAINS
                    && e.source == file_id
                    && matches!(&e.target, EdgeTarget::Node(t) if *t == method_id)
                {
                    e.source = type_id;
                    break;
                }
            }
        }
    }

    /// A trait's **bodiless** method declaration (`fn get(&self) -> T;`) as a
    /// node (ADR-0036 §5).
    ///
    /// Kind `function`, like any other method — the declaration is what callers
    /// through the trait bind to, and a separate kind would only make it
    /// unlinkable. It is owner-qualified and contained by the *trait* (`Store::get`,
    /// `trait --contains--> get`) rather than by the file, matching how a class
    /// owns its methods in TS/Python and how §4 re-parents `impl` methods; that
    /// containment is what makes "traverse the abstraction to its operations"
    /// work, and what keeps two traits' same-named declarations apart.
    ///
    /// Its signature is a real contract, so `type/param` / `type/return` are
    /// emitted exactly as for a method with a body — that is the interface the
    /// caller programs against, and it is where §5 compounds with R2.
    fn def_signature_item(&mut self, node: TsNode<'_>, trait_id: &NodeId, trait_name: &str) {
        let Some(name) = node
            .child_by_field_name("name")
            .and_then(|n| self.d.text(n))
        else {
            return;
        };
        // The declaration's own `<T, …>`, on top of the trait's (already pushed).
        let type_params = node
            .child_by_field_name("type_parameters")
            .map(|tp| self.collect_type_parameters(tp))
            .unwrap_or_default();
        self.d.push_type_parameters(type_params);

        let id = self
            .d
            .add_def("fn", "function", &name, node, trait_id, Some(trait_name));
        self.d.mark_abstract(&id);

        // `-> Self` in a trait means the implementor, which is not knowable here;
        // `Self` resolves to the trait only in the sense that the trait is the
        // declaring type, which is what the rest of the walk already does for an
        // `impl` block's methods.
        if let Some(return_type_node) = node.child_by_field_name("return_type") {
            let mut refs = Vec::new();
            self.collect_type_refs_recursive(return_type_node, false, &mut refs);
            for (ref_name, _role) in refs {
                let resolved = if ref_name == "Self" {
                    trait_name.to_string()
                } else {
                    ref_name
                };
                let normalized = base_type_name(&resolved);
                if base_type_name(&name) != normalized {
                    let _ = self.d.emit_return_type(id.clone(), normalized);
                }
            }
        }
        self.emit_bound_type_edges(&id, node);

        self.d.scope_push();
        if let Some(params) = node.child_by_field_name("parameters") {
            self.collect_params(params, Some(trait_name), &id, &name);
        }
        self.d.scope_pop();
        self.d.pop_type_parameters();
    }

    /// Recursive descent. `current_fn` = the enclosing function (id, name) so
    /// calls can be attributed; `current_impl` = the enclosing `impl` type name.
    fn walk(
        &mut self,
        node: TsNode<'_>,
        current_fn: Option<(NodeId, String)>,
        current_impl: Option<String>,
    ) {
        match node.kind() {
            "function_item" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| self.d.text(n))
                {
                    // Collect type parameters for this function
                    let type_params = node
                        .child_by_field_name("type_parameters")
                        .map(|tp| self.collect_type_parameters(tp))
                        .unwrap_or_default();
                    self.d.push_type_parameters(type_params);

                    // A free `pub fn` is a module export; a `pub` method belongs to
                    // its type, not the module surface.
                    if current_impl.is_none() && is_pub(node) {
                        self.d.exports.push(Export::Local { name: name.clone() });
                    }
                    let (prefix, kind) = self.def_kind(node).unwrap_or(("fn", "function"));
                    let container = self.d.file_id.clone();
                    let id = self.d.add_def(
                        prefix,
                        kind,
                        &name,
                        node,
                        &container,
                        current_impl.as_deref(),
                    );
                    // A method (fn inside an `impl` block) is provisionally
                    // file-contained above; queue it to be re-parented to its impl
                    // type's node post-walk (ADR-0036 #4). Free fns (no impl) stay
                    // file-contained. Keyed by the *base* type name so it aligns
                    // with `type_ids` (`impl Widget<T>` → `Widget`).
                    if let Some(owner) = current_impl.as_deref() {
                        self.pending_method_containers
                            .push((base_type_name(owner).to_string(), id.clone()));
                    }
                    // Stamp the declared return type (`-> T`) so the resolver can
                    // infer the type of a variable bound to this fn's result. `Self`
                    // → the concrete impl type. No return type → no attr.
                    if let Some(mut rt) = node
                        .child_by_field_name("return_type")
                        .and_then(|t| self.type_name(t))
                    {
                        if rt == "Self" {
                            if let Some(owner) = current_impl.as_deref() {
                                rt = owner.to_string();
                            }
                        }
                        if let Some(n) = self.d.nodes.last_mut() {
                            n.attrs.insert(attrs::RETURNS.into(), rt);
                        }
                    }
                    // Emit return_type edges (ADR-0036)
                    if let Some(return_type_node) = node.child_by_field_name("return_type") {
                        // Collect all type references recursively
                        let mut refs = Vec::new();
                        self.collect_type_refs_recursive(return_type_node, false, &mut refs);

                        // Emit a return_type edge for each type found
                        for (ref_name, _role) in refs {
                            // Resolve Self to concrete type if needed
                            let resolved_name = if ref_name == "Self" {
                                current_impl
                                    .as_ref()
                                    .map(|s| s.to_string())
                                    .unwrap_or(ref_name.clone())
                            } else {
                                ref_name.clone()
                            };

                            let normalized_name = base_type_name(&resolved_name);
                            // Prevent self-reference (function -> function)
                            if base_type_name(&name) != normalized_name {
                                let _ = self.d.emit_return_type(id.clone(), normalized_name);
                            }
                        }
                    }

                    // Emit bound_type edges (ADR-0036) from type_parameters and where_clause
                    // Must be AFTER type_parameters are pushed to the R1.1 stack
                    self.emit_bound_type_edges(&id, node);

                    let fn_ctx = Some((id.clone(), name.clone()));
                    // A fresh scope for this function body's local type inference.
                    self.d.scope_push();
                    if let Some(params) = node.child_by_field_name("parameters") {
                        self.collect_params(params, current_impl.as_deref(), &id, &name);
                    }
                    self.walk_children(node, fn_ctx, current_impl);
                    self.d.scope_pop();
                    self.d.pop_type_parameters();
                    return;
                }
            }
            "function_signature_item" => {
                // `fn get(&self) -> T;` — a body-less declaration. A node only
                // inside a `trait` (ADR-0036 §5); the same grammar rule also
                // spells an `extern "C" { fn … ; }` FFI declaration, which is a
                // foreign symbol rather than an abstraction this code implements.
                if let Some((trait_id, trait_name)) = self.current_trait.clone() {
                    self.def_signature_item(node, &trait_id, &trait_name);
                    return;
                }
            }
            "let_declaration" => {
                // Infer the binding's type BEFORE the new binding is visible (so
                // `let x = x.foo()` reads the old `x`), then walk the RHS, then
                // record the binding for subsequent statements.
                let binding = self.infer_let_type(node, current_impl.as_deref());
                self.walk_children(node, current_fn, current_impl);
                if let Some((var, ty)) = binding {
                    self.d.scope_insert(&var, ty);
                }
                return;
            }
            "struct_item" | "union_item" | "enum_item" | "trait_item" => {
                // Collect type parameters for this type definition
                let type_params = node
                    .child_by_field_name("type_parameters")
                    .map(|tp| self.collect_type_parameters(tp))
                    .unwrap_or_default();
                self.d.push_type_parameters(type_params);
                self.add_named_type(node);

                // Get the node id for bound_type edge emission
                let type_id = if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| self.d.text(n))
                {
                    self.type_ids.get(&name).cloned()
                } else {
                    None
                };

                // Emit bound_type edges (ADR-0036) for type definitions
                if let Some(id) = type_id.clone() {
                    self.emit_bound_type_edges(&id, node);
                }

                // A `trait` is *the* abstract type in Rust — the thing code is
                // generic over and dyn-dispatches through (ADR-0036 §5). Its
                // bodiless members are marked in `def_signature_item` below; the
                // declaration itself is marked here, so `abstract` covers the
                // type and its members alike (Kythe's `tag/abstract`).
                let outer_trait = self.current_trait.take();
                if node.kind() == "trait_item" {
                    if let Some(id) = type_id.clone() {
                        self.d.mark_abstract(&id);
                        if let Some(name) = node
                            .child_by_field_name("name")
                            .and_then(|n| self.d.text(n))
                        {
                            self.current_trait = Some((id, name));
                        }
                    }
                }

                // The body is walked *inside* the scope. A `trait Store<T>` may
                // carry default method bodies, and `T` is in scope throughout
                // them: popping before the descent left the parameter invisible,
                // so `fn put(&self, item: T)` emitted a `type/param -> T` that
                // resolution then bound to whatever real type happened to be
                // called `T`. R1.1 suppression only works if the scope outlives
                // the members it covers.
                self.walk_children(node, current_fn, current_impl);
                self.current_trait = outer_trait;
                self.d.pop_type_parameters();
                return;
            }
            "impl_item" => {
                // Collect type parameters for this impl block
                let type_params = node
                    .child_by_field_name("type_parameters")
                    .map(|tp| self.collect_type_parameters(tp))
                    .unwrap_or_default();
                self.d.push_type_parameters(type_params);

                // `impl Trait for Type` → an `implements` edge Type → Trait
                // (ADR-0036). The source is the impl'd type's *base* name (so
                // `impl … for Vec<T>` sources from `Vec`); resolution to its def
                // node is deferred (the type may be declared after this block).
                if let Some(ty) = node
                    .child_by_field_name("type")
                    .and_then(|t| self.type_name(t))
                {
                    for h in self.heritage(node) {
                        self.pending_impls.push((ty.clone(), h.target));
                    }

                    // Emit bound_type edges (ADR-0036) for impl blocks
                    // Only if the self type has a local node (decline blanket impls)
                    let base_name = base_type_name(&ty);
                    if let Some(type_id) = self.type_ids.get(base_name).cloned() {
                        self.emit_bound_type_edges(&type_id, node);
                    } else {
                        // Track impls that decline due to missing local node (R5.1.2)
                        self.declined_impls
                            .push((base_name.to_string(), "no_local_node".to_string()));
                    }
                    // If type_id is None, this is a blanket impl or foreign type → decline
                }
                // Recurse into the impl with its type name as the owner so
                // methods get tagged; the impl itself is not a node.
                let owner = node
                    .child_by_field_name("type")
                    .and_then(|n| self.d.text(n))
                    .or_else(|| current_impl.clone());
                self.walk_children(node, current_fn, owner);
                self.d.pop_type_parameters();
                return;
            }
            "call_expression" => {
                if let (Some((src_id, _)), Some(func)) =
                    (current_fn.as_ref(), node.child_by_field_name("function"))
                {
                    if let Some((callee, syntactic_hint)) = self.callee_ref(func, None) {
                        // `Self::method()` → the enclosing impl's concrete type,
                        // not the useless literal `Self`.
                        let syntactic: Option<String> = match syntactic_hint.as_deref() {
                            Some("Self") => current_impl.clone(),
                            _ => syntactic_hint,
                        };
                        let src_id = src_id.clone();
                        let mut tref = TargetRef::new(callee);
                        if let Some(ty) = syntactic {
                            // Associated call `T::method` — the type is explicit.
                            tref.hints.insert("type".into(), ty);
                        } else if func.kind() == "field_expression" {
                            // `x.method()` / `self.method()` — infer the receiver's
                            // type from the local environment.
                            match self.receiver_type(func, current_impl.as_deref()) {
                                // Known concrete type → narrow directly.
                                Some(VarType::Concrete(ty)) => {
                                    tref.hints.insert("type".into(), ty);
                                }
                                // Bound to a free/associated call: defer to that
                                // callee's return type (resolved cross-file).
                                Some(VarType::ReturnOf { callee, owner }) => {
                                    tref.hints.insert("recv_returns".into(), callee);
                                    if let Some(owner) = owner {
                                        tref.hints.insert("recv_returns_owner".into(), owner);
                                    }
                                }
                                // Un-inferable receiver → opaque: resolution declines
                                // a cross-file bare-name homonym bind (ADR-0023).
                                None => {
                                    tref.hints.insert("recv".into(), "opaque".into());
                                }
                            }
                        }
                        // A bare free/associated call with no type carries no hint.
                        self.d.emit_call(src_id, tref);
                    }
                }
            }
            "use_declaration" => {
                if let Some(arg) = node.child_by_field_name("argument") {
                    let pub_ = is_pub(node);
                    for u in self.imports(arg) {
                        self.emit_use(u, pub_);
                    }
                }
            }
            _ => {}
        }
        self.walk_children(node, current_fn, current_impl);
    }

    fn walk_children(
        &mut self,
        node: TsNode<'_>,
        current_fn: Option<(NodeId, String)>,
        current_impl: Option<String>,
    ) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child, current_fn.clone(), current_impl.clone());
        }
    }

    /// Create a `struct`/`enum`/`trait` definition node (kind from [`NodeMapper`]),
    /// recording a `pub` item as a module export, then emit the ADR-0036 structural
    /// edges sourced from that fresh node: struct `type/field` edges, trait
    /// supertrait `extends` edges, and enum `enum_variant` nodes + `has_variant`.
    fn add_named_type(&mut self, node: TsNode<'_>) {
        if let Some(name) = node
            .child_by_field_name("name")
            .and_then(|n| self.d.text(n))
        {
            if is_pub(node) {
                self.d.exports.push(Export::Local { name: name.clone() });
            }
            let (prefix, kind) = self.def_kind(node).unwrap_or(("type", "struct"));
            let container = self.d.file_id.clone();
            let id = self.d.add_def(prefix, kind, &name, node, &container, None);
            self.type_ids.insert(name.clone(), id.clone());

            // Register as local type for noise filtering exceptions (ADR-0036 R1.1)
            let base_name = base_type_name(&name);
            self.d.register_local_type(base_name);
            match node.kind() {
                "struct_item" | "union_item" => {
                    for f in self.fields(node) {
                        let _ = self.d.emit_field_type(
                            id.clone(),
                            &f.name,
                            &f.type_name,
                            f.visibility.as_deref(),
                        );
                    }
                    for (index, type_node) in self.tuple_fields(node).into_iter().enumerate() {
                        let mut refs = Vec::new();
                        self.collect_type_refs_recursive(type_node, false, &mut refs);
                        for (ref_name, _role) in refs {
                            let normalized_name = base_type_name(&ref_name);
                            if base_type_name(&name) != normalized_name {
                                let _ = self.d.emit_field_type(
                                    id.clone(),
                                    &index.to_string(),
                                    normalized_name,
                                    None,
                                );
                            }
                        }
                    }
                }
                "trait_item" => {
                    for h in self.heritage(node) {
                        self.d.emit_heritage(id.clone(), h.relation, &h.target);
                    }

                    // Emit bound_type edges for associated types in trait body
                    // Source: trait node, not associated type (no associated type nodes yet)
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for child in body.named_children(&mut cursor) {
                            if child.kind() == "type_item" {
                                // Associated types can have bounds after the name (type Assoc: Bound)
                                let mut assoc_cursor = child.walk();
                                for assoc_child in child.children(&mut assoc_cursor) {
                                    if assoc_child.kind() == "trait_bound" {
                                        // Use the verified collect_type_refs_recursive function
                                        let mut refs = Vec::new();
                                        self.collect_type_refs_recursive(
                                            assoc_child,
                                            false,
                                            &mut refs,
                                        );
                                        for (ref_name, _role) in refs {
                                            let normalized_bound = base_type_name(&ref_name);
                                            let _ = self
                                                .d
                                                .emit_bound_type(id.clone(), normalized_bound);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                "enum_item" => self.emit_variants(node, &name, &id),
                _ => {}
            }
        }
    }

    /// Emit an `enum_variant` node (owner-qualified `Enum::Variant`) + a
    /// `has_variant` edge for each variant in an `enum_item`'s `enum_variant_list`.
    fn emit_variants(&mut self, enum_node: TsNode<'_>, enum_name: &str, enum_id: &NodeId) {
        let Some(body) = enum_node.child_by_field_name("body") else {
            return;
        };
        let mut cursor = body.walk();
        let variants: Vec<TsNode<'_>> = body
            .named_children(&mut cursor)
            .filter(|v| v.kind() == "enum_variant")
            .collect();
        for v in variants {
            if let Some(vname) = v.child_by_field_name("name").and_then(|n| self.d.text(n)) {
                let variant_id = self.d.add_variant(enum_id, enum_name, &vname, v);
                self.emit_variant_payload_types(v, &vname, enum_name, &variant_id);
            }
        }
    }

    /// Emit `type/field` edges from an enum variant to its payload types.
    /// Handles both tuple variants (`Variant(T1, T2)`) and struct variants (`Variant { field: Type }`).
    fn emit_variant_payload_types(
        &mut self,
        variant_node: TsNode<'_>,
        variant_name: &str,
        enum_name: &str,
        variant_id: &NodeId,
    ) {
        let mut cursor = variant_node.walk();

        for child in variant_node.children(&mut cursor) {
            match child.kind() {
                "ordered_field_declaration_list" => {
                    // Tuple variant: Variant(T1, T2)
                    self.emit_tuple_variant_payloads(child, variant_id, variant_name, enum_name);
                }
                "field_declaration_list" => {
                    // Struct variant: Variant { field: Type }
                    self.emit_struct_variant_payloads(child, variant_id, variant_name, enum_name);
                }
                _ => {
                    // Attribute nodes like #[from], #[error], etc. - skip them
                    // These are not payload types, they're metadata
                    continue;
                }
            }
        }
    }

    /// Emit `type/field` edges for tuple variant payloads: `Variant(T1, T2)`
    ///
    /// Member identity uses positional indices "0", "1", "2", etc. because:
    /// 1. Rust tuple fields are accessed as self.0, self.1, self.2
    /// 2. When R1.4 builds the carried channel, these indices will be correct
    /// 3. Makes R1.4 pure plumbing instead of plumbing + data backfill
    fn emit_tuple_variant_payloads(
        &mut self,
        fields_node: TsNode<'_>,
        variant_id: &NodeId,
        _variant_name: &str,
        _enum_name: &str,
    ) {
        let mut cursor = fields_node.walk();
        let mut field_index: usize = 0;

        for field in fields_node.named_children(&mut cursor) {
            // Skip attribute nodes like #[from], they are not type nodes
            if !is_type_node(field.kind()) {
                continue;
            }

            let mut refs = Vec::new();
            self.collect_type_refs_recursive(field, false, &mut refs);

            for (ref_name, _role) in refs {
                let normalized_name = base_type_name(&ref_name);
                // Use positional index as field name (Rust tuple fields are self.0, self.1, etc.)
                let field_name = field_index.to_string();
                let _ =
                    self.d
                        .emit_field_type(variant_id.clone(), &field_name, normalized_name, None);
            }
            field_index += 1;
        }
    }

    /// Emit `type/field` edges for struct variant payloads: `Variant { field: Type }`
    fn emit_struct_variant_payloads(
        &mut self,
        fields_node: TsNode<'_>,
        variant_id: &NodeId,
        _variant_name: &str,
        _enum_name: &str,
    ) {
        let mut cursor = fields_node.walk();

        for field in fields_node.named_children(&mut cursor) {
            if field.kind() != "field_declaration" {
                continue;
            }

            let name = field
                .child_by_field_name("name")
                .and_then(|n| self.d.text(n));
            let type_node = field.child_by_field_name("type");

            if let (Some(field_name), Some(type_child)) = (name, type_node) {
                let mut refs = Vec::new();
                self.collect_type_refs_recursive(type_child, false, &mut refs);

                for (ref_name, _role) in refs {
                    let normalized_name = base_type_name(&ref_name);
                    let visibility = field
                        .children(&mut field.walk())
                        .find(|c| c.kind() == "visibility_modifier")
                        .and_then(|v| self.d.text(v));
                    let _ = self.d.emit_field_type(
                        variant_id.clone(),
                        &field_name,
                        normalized_name,
                        visibility.as_deref(),
                    );
                }
            }
        }
    }

    // ---- receiver-type inference (the `x.method()` half of the homonym fix) --

    // ---- receiver-type inference (the `x.method()` half of the homonym fix) --

    // ---- receiver-type inference (the `x.method()` half of the homonym fix) --

    /// Record each typed parameter (`x: &Widget` → `x: Widget`) in the current
    /// scope. Also bind `self` for impl methods, and emit param_type edges (ADR-0036).
    fn collect_params(
        &mut self,
        params: TsNode<'_>,
        current_impl: Option<&str>,
        fn_id: &NodeId,
        fn_name: &str,
    ) {
        let mut cursor = params.walk();
        for child in params.children(&mut cursor) {
            match child.kind() {
                "self_parameter" => {
                    if let Some(owner) = current_impl {
                        self.d
                            .scope_insert("self", VarType::Concrete(owner.to_string()));
                    }
                }
                "parameter" => {
                    let name = child
                        .child_by_field_name("pattern")
                        .and_then(|p| self.pattern_name(p));
                    let ty = child
                        .child_by_field_name("type")
                        .and_then(|t| self.type_name(t));
                    if let (Some(name), Some(ty)) = (name, ty) {
                        self.d.scope_insert(&name, VarType::Concrete(ty));
                    }
                    // Emit param_type edges (ADR-0036)
                    if let Some(type_node) = child.child_by_field_name("type") {
                        let mut refs = Vec::new();
                        self.collect_type_refs_recursive(type_node, false, &mut refs);
                        for (ref_name, _role) in refs {
                            // Resolve Self to concrete type if needed
                            let resolved_name = if ref_name == "Self" {
                                current_impl
                                    .map(|s| s.to_string())
                                    .unwrap_or(ref_name.clone())
                            } else {
                                ref_name.clone()
                            };

                            let normalized_name = base_type_name(&resolved_name);
                            // Prevent self-reference (function -> function)
                            if base_type_name(fn_name) != normalized_name {
                                let _ = self.d.emit_param_type(fn_id.clone(), normalized_name);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// `(var, type)` for a `let` binding whose type is inferable: an explicit
    /// annotation (authoritative), else a constructor call / struct literal RHS
    /// (concrete), else a **free/associated call** RHS whose return type is
    /// resolved cross-file (`ReturnOf`). A method-chain RHS stays unknown.
    fn infer_let_type(
        &self,
        let_node: TsNode<'_>,
        current_impl: Option<&str>,
    ) -> Option<(String, VarType)> {
        let var = self.pattern_name(let_node.child_by_field_name("pattern")?)?;
        // Explicit annotation wins.
        if let Some(ty) = let_node
            .child_by_field_name("type")
            .and_then(|t| self.type_name(t))
        {
            return Some((var, VarType::Concrete(ty)));
        }
        let value = let_node.child_by_field_name("value")?;
        // A struct literal or constructor-convention call gives a concrete type.
        if let Some(ty) = self.value_type(value, current_impl) {
            return Some((var, VarType::Concrete(ty)));
        }
        // Else, if the RHS is a plain free/associated call, defer to its return
        // type (single-hop; the resolver reads the callee's `returns`).
        if let Some((callee, owner)) = self.deferred_return_of(value) {
            return Some((var, VarType::ReturnOf { callee, owner }));
        }
        None
    }

    /// The `(callee, owner)` a `let` RHS defers to, when it is a **free**
    /// (`foo()`) or **associated** (`Foo::make()`) call — the receiver's type is
    /// that callee's return type, resolved cross-file. A method-call RHS
    /// (`a.b()`, a chain) returns `None`: threading its type needs the receiver's
    /// own type and is out of scope (kept opaque). Constructor-convention calls
    /// are handled earlier by `value_type`, so they never reach here.
    fn deferred_return_of(&self, value: TsNode<'_>) -> Option<(String, Option<String>)> {
        if value.kind() != "call_expression" {
            return None;
        }
        let func = value.child_by_field_name("function")?;
        match func.kind() {
            "identifier" => self.d.text(func).map(|name| (name, None)),
            "scoped_identifier" | "generic_function" => {
                let (name, owner) = self.callee_ref(func, None)?;
                Some((name, owner))
            }
            // `a.b()` — method-call RHS (chain): out of scope, stays opaque.
            _ => None,
        }
    }

    /// The type an initializer expression evaluates to, when knowable: a struct
    /// literal `Type { .. }`, or a constructor-convention associated call
    /// (`Type::new/default/from/with_capacity`) that returns `Self`. Anything
    /// else (arbitrary call, builder, method chain) is left unknown — better no
    /// hint than a wrong one.
    fn value_type(&self, val: TsNode<'_>, current_impl: Option<&str>) -> Option<String> {
        const RETURNS_SELF: &[&str] = &["new", "default", "from", "with_capacity"];
        match val.kind() {
            "struct_expression" => {
                let name = val.child_by_field_name("name")?;
                let ty = self.type_name(name)?;
                if ty == "Self" {
                    current_impl.map(str::to_string)
                } else {
                    Some(ty)
                }
            }
            "call_expression" => {
                let (name, hint) = self.callee_ref(val.child_by_field_name("function")?, None)?;
                if !RETURNS_SELF.contains(&name.as_str()) {
                    return None;
                }
                match hint.as_deref() {
                    Some("Self") => current_impl.map(str::to_string),
                    _ => hint,
                }
            }
            _ => None,
        }
    }

    /// The name bound by a simple pattern (`x` or `mut x`); other patterns
    /// (tuples, structs) are not tracked.
    fn pattern_name(&self, pat: TsNode<'_>) -> Option<String> {
        match pat.kind() {
            "identifier" => self.d.text(pat),
            "mut_pattern" => {
                let mut cursor = pat.walk();
                let ident = pat
                    .named_children(&mut cursor)
                    .find(|n| n.kind() == "identifier");
                ident.and_then(|n| self.d.text(n))
            }
            _ => None,
        }
    }

    /// Emit the graph facts for one parsed `use` entry: a re-export table entry
    /// when the declaration is `pub` (a barrel — ADR-0020), and always an
    /// `imports` edge that brings the bound name into this file's scope, carrying
    /// the module `specifier` (and `imported`, when aliased) as resolver hints.
    fn emit_use(&mut self, u: ParsedImport, pub_: bool) {
        match u {
            ParsedImport::Named {
                specifier,
                imported,
                alias,
            } => {
                let name = alias.clone().unwrap_or_else(|| imported.clone());
                if pub_ {
                    self.d.exports.push(match &specifier {
                        // `pub use crate::api::greet` — a re-export to follow.
                        Some(spec) => Export::ReExport {
                            name: name.clone(),
                            specifier: spec.clone(),
                            imported: imported.clone(),
                        },
                        // `pub use greet` (no path) — re-export of a local name.
                        None => Export::Local { name: name.clone() },
                    });
                }
                let mut tref = TargetRef::new(&name);
                if let Some(spec) = &specifier {
                    tref.hints.insert("specifier".into(), spec.clone());
                }
                if alias.is_some() {
                    tref.hints.insert("imported".into(), imported);
                }
                self.d.emit_import(tref);
            }
            // `pub use crate::api::*` — a wildcard re-export; a glob has no single
            // bound name, so it emits no `imports` edge.
            ParsedImport::Wildcard { specifier } => {
                if pub_ {
                    self.d.exports.push(Export::Star { specifier });
                }
            }
        }
    }

    /// Last identifier segment of a `use` argument (best-effort for Phase 1).
    fn last_path_segment(&self, node: TsNode<'_>) -> Option<String> {
        match node.kind() {
            "identifier" | "type_identifier" => self.d.text(node),
            "scoped_identifier" => node
                .child_by_field_name("name")
                .and_then(|n| self.d.text(n))
                .or_else(|| self.d.text(node)),
            // use a::{b, c}; / use a::*; — fall back to the raw text's tail.
            _ => self
                .d
                .text(node)
                .and_then(|t| t.rsplit("::").next().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty()),
        }
    }
}

/// The bare base identifier of an impl type owner, so it keys into `type_ids`
/// (which is keyed by the declared type identifier): strip any generic args and
/// path qualifier — `Widget<T>` / `a::Foo` / `Box<Foo>` → `Widget` / `Foo` /
/// `Box`.
///
/// Preserves std library paths (std::*, core::*, alloc::*) as definitively foreign
/// while stripping other path qualifiers (ADR-0023 behavior).
pub fn base_type_name(owner: &str) -> &str {
    let without_generics = owner.split('<').next().unwrap_or(owner).trim();

    // Local paths (crate::, self::, super::, Self::) → strip to last segment (definitionally local)
    // Everything else → keep qualified (foreign until proven otherwise)
    if without_generics.starts_with("crate::")
        || without_generics.starts_with("self::")
        || without_generics.starts_with("super::")
        || without_generics.starts_with("Self::")
    {
        // Local paths: strip to last segment
        return without_generics
            .rsplit("::")
            .next()
            .unwrap_or(without_generics)
            .trim();
    }

    // Everything else: keep qualified (includes std::, core::, alloc::, third-party crates, etc.)
    without_generics
}

/// Does a Rust item node carry a `pub` (any `visibility_modifier`)?
fn is_pub(node: TsNode<'_>) -> bool {
    node.children(&mut node.walk())
        .any(|c| c.kind() == "visibility_modifier")
}

/// Prefix a parsed entry's module path with an enclosing list's path
/// (`crate::api` + `{greet}` → specifier `crate::api`; `crate::a` + `b::c`
/// → `crate::a::b`).
fn prepend(u: &mut ParsedImport, prefix: Option<&str>) {
    let Some(prefix) = prefix else { return };
    let join = |spec: &Option<String>| match spec {
        Some(s) => format!("{prefix}::{s}"),
        None => prefix.to_string(),
    };
    match u {
        ParsedImport::Named { specifier, .. } => *specifier = Some(join(specifier)),
        ParsedImport::Wildcard { specifier } => {
            *specifier = format!("{prefix}::{specifier}");
        }
    }
}

/// Split a `use` path `a::b::c` into `(specifier = "a::b", imported = "c")`;
/// a single segment `c` has no specifier. `alias` rides along for `x as y`.
fn split_use(full: &str, alias: Option<String>) -> ParsedImport {
    match full.trim().rsplit_once("::") {
        Some((spec, name)) => ParsedImport::Named {
            specifier: Some(spec.trim().to_string()),
            imported: name.trim().to_string(),
            alias,
        },
        None => ParsedImport::Named {
            specifier: None,
            imported: full.trim().to_string(),
            alias,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filigrio_core::{ArtifactKind, Edge};

    fn artifact() -> Artifact {
        Artifact {
            path: "src/demo.rs".into(),
            kind: ArtifactKind::Code,
            language: Some("rust".into()),
        }
    }

    fn extract(src: &str) -> Extraction {
        RustExtractor::new()
            .extract(&artifact(), src.as_bytes())
            .unwrap()
    }

    // Extended extraction helper that returns both Extraction and declined_impls statistics
    fn extract_with_stats(src: &str) -> (Extraction, (usize, Vec<(String, String)>)) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src.as_bytes(), None).unwrap();

        let mut ctx = Ctx::new(&artifact(), src.as_bytes());
        ctx.walk(tree.root_node(), None, None);
        ctx.resolve_pending_impls();
        ctx.resolve_pending_method_containment();
        let stats = ctx.get_declined_impls_stats();
        let extraction = ctx.d.finish();

        (extraction, stats)
    }

    #[test]
    fn extracts_functions_structs_and_calls() {
        let src = r#"
use std::collections::HashMap;

struct Widget { n: u32 }

fn helper() -> u32 { 42 }

fn build() -> Widget {
    let x = helper();
    Widget { n: x }
}
"#;
        let ex = extract(src);
        let labels: Vec<&str> = ex.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&"src/demo.rs"), "file node present");
        assert!(labels.contains(&"Widget"), "struct node present");
        assert!(
            labels.contains(&"helper") && labels.contains(&"build"),
            "fns present"
        );

        // struct kind is captured
        assert_eq!(
            ex.nodes.iter().find(|n| n.label == "Widget").unwrap().kind,
            "struct"
        );

        // build() calls helper() → unresolved Symbol edge from build
        let build_id = &ex.nodes.iter().find(|n| n.label == "build").unwrap().id;
        let calls_helper = ex.edges.iter().any(|e| {
            &e.source == build_id
                && e.relation == "calls"
                && matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "helper")
        });
        assert!(
            calls_helper,
            "build should have an unresolved call to helper"
        );

        // the `use` becomes an imports edge naming the last segment
        assert!(
            ex.edges.iter().any(|e| e.relation == "imports"
                && matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "HashMap")),
            "use std::collections::HashMap → imports HashMap"
        );
    }

    #[test]
    fn methods_get_impl_owner_and_stable_ids() {
        let src = r#"
struct S;
impl S {
    fn new() -> S { S }
    fn run(&self) { self.help(); }
}
struct T;
impl T {
    fn new() -> T { T }
}
"#;
        let ex = extract(src);
        // two `new` methods → ids disambiguated, not collapsed
        let news: Vec<&Node> = ex.nodes.iter().filter(|n| n.label == "new").collect();
        assert_eq!(news.len(), 2, "both new() methods kept");
        assert_ne!(news[0].id, news[1].id, "ids disambiguated with ~N");
        // impl owner recorded
        let owners: Vec<&String> = news.iter().filter_map(|n| n.attrs.get("impl")).collect();
        assert!(owners.contains(&&"S".to_string()) && owners.contains(&&"T".to_string()));
    }

    #[test]
    fn associated_call_captures_receiver_type_hint() {
        // `S::new()` and `T::new()` — the callee is `new` both times, but the
        // type qualifier (S / T) is captured as a hint so resolution can tell the
        // two constructors apart. A bare call and a module path carry no hint.
        let src = r#"
struct S;
struct T;
fn build() {
    let _ = S::new();
    let _ = T::new();
    let _ = helper();
    std::mem::swap();
}
"#;
        let ex = extract(src);
        let hint_of = |ty_name: &str| -> Option<String> {
            ex.edges.iter().find_map(|e| match &e.target {
                EdgeTarget::Symbol(r)
                    if r.name == "new"
                        && r.hints.get("type").map(String::as_str) == Some(ty_name) =>
                {
                    r.hints.get("type").cloned()
                }
                _ => None,
            })
        };
        assert_eq!(hint_of("S"), Some("S".into()), "S::new() hints type S");
        assert_eq!(hint_of("T"), Some("T".into()), "T::new() hints type T");

        // helper() — a bare call — has no type hint.
        let helper = ex
            .edges
            .iter()
            .find(|e| matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "helper"))
            .expect("helper call present");
        assert!(
            matches!(&helper.target, EdgeTarget::Symbol(r) if !r.hints.contains_key("type")),
            "a bare call carries no type hint"
        );

        // std::mem::swap() — module path — must NOT be hinted with `mem`.
        let swap = ex
            .edges
            .iter()
            .find(|e| matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "swap"));
        if let Some(e) = swap {
            assert!(
                matches!(&e.target, EdgeTarget::Symbol(r) if !r.hints.contains_key("type")),
                "a lower-case module path is not a type hint"
            );
        }
    }

    #[test]
    fn self_call_hint_binds_to_enclosing_impl_type() {
        // `Self::new()` inside `impl Widget` must hint the concrete type Widget,
        // not the literal `Self` (which resolves to nothing).
        let src = r#"
struct Widget;
impl Widget {
    fn make() -> Widget { Self::new() }
    fn new() -> Widget { Widget }
}
"#;
        let ex = extract(src);
        let hint = ex.edges.iter().find_map(|e| match &e.target {
            EdgeTarget::Symbol(r) if r.name == "new" => r.hints.get("type").cloned(),
            _ => None,
        });
        assert_eq!(
            hint,
            Some("Widget".into()),
            "Self::new() hints the enclosing impl type"
        );
    }

    /// The receiver-type hint on the first `calls` edge whose callee is `callee`.
    fn hint_for(ex: &Extraction, callee: &str) -> Option<String> {
        for e in &ex.edges {
            if let EdgeTarget::Symbol(r) = &e.target {
                if r.name == callee {
                    return r.hints.get("type").cloned();
                }
            }
        }
        None
    }

    /// The `recv` marker on the first `calls` edge whose callee is `callee`
    /// (`Some("opaque")` for an un-typeable method receiver, else `None`).
    fn recv_for(ex: &Extraction, callee: &str) -> Option<String> {
        hint_named(ex, callee, "recv")
    }

    /// An arbitrary hint on the first `calls` edge whose callee is `callee`.
    fn hint_named(ex: &Extraction, callee: &str, key: &str) -> Option<String> {
        for e in &ex.edges {
            if let EdgeTarget::Symbol(r) = &e.target {
                if r.name == callee {
                    return r.hints.get(key).cloned();
                }
            }
        }
        None
    }

    /// The `returns` attr of the node labelled `label` (a fn/method def).
    fn returns_of(ex: &Extraction, label: &str) -> Option<String> {
        ex.nodes
            .iter()
            .find(|n| n.label == label)
            .and_then(|n| n.attrs.get(attrs::RETURNS).cloned())
    }

    #[test]
    fn self_method_call_infers_enclosing_type() {
        // `self.help()` — the receiver `self` has the enclosing impl's type.
        let src = r#"
struct S;
impl S {
    fn run(&self) { self.help(); }
    fn help(&self) {}
}
"#;
        assert_eq!(hint_for(&extract(src), "help"), Some("S".into()));
    }

    #[test]
    fn let_binding_from_constructor_infers_receiver_type() {
        // `let w = Widget::new(); w.go();` — w's type is inferred from the
        // constructor, so `w.go()` hints Widget.
        let src = r#"
struct Widget;
impl Widget { fn new() -> Widget { Widget } fn go(&self) {} }
fn build() {
    let w = Widget::new();
    w.go();
}
"#;
        assert_eq!(hint_for(&extract(src), "go"), Some("Widget".into()));
    }

    #[test]
    fn let_type_annotation_infers_receiver_type() {
        // An explicit annotation is authoritative even when the RHS is opaque.
        let src = r#"
fn build() {
    let w: Widget = compute();
    w.go();
}
"#;
        assert_eq!(hint_for(&extract(src), "go"), Some("Widget".into()));
    }

    #[test]
    fn parameter_type_infers_receiver_type() {
        // `fn use_it(w: &Widget)` → a call `w.go()` inside hints Widget.
        let src = r#"
fn use_it(w: &Widget) { w.go(); }
"#;
        assert_eq!(hint_for(&extract(src), "go"), Some("Widget".into()));
    }

    #[test]
    fn struct_literal_binding_infers_receiver_type() {
        let src = r#"
struct Widget { n: u32 }
fn build() {
    let w = Widget { n: 1 };
    w.go();
}
"#;
        assert_eq!(hint_for(&extract(src), "go"), Some("Widget".into()));
    }

    #[test]
    fn uninferable_receiver_carries_no_hint() {
        // The RHS is neither a constructor/struct literal nor a plain call (here an
        // arithmetic expression) → nothing to infer or defer → no type hint. Better
        // an honest unresolved than a wrong guess.
        let src = r#"
fn build() {
    let w = a + b;
    w.go();
}
"#;
        assert_eq!(hint_for(&extract(src), "go"), None);
    }

    #[test]
    fn opaque_receiver_is_marked_recv_opaque() {
        // A receiver we cannot type at all — an *unbound* local (never `let`-bound,
        // not a parameter) — gets no `type` hint but a `recv=opaque` marker, so
        // resolution declines a cross-file homonym bind instead of guessing
        // (ADR-0023). (A `let w = compute()` receiver is *deferred*, not opaque.)
        let src = r#"
fn build() {
    w.go();
}
"#;
        let ex = extract(src);
        assert_eq!(hint_for(&ex, "go"), None, "still no type hint");
        assert_eq!(
            recv_for(&ex, "go"),
            Some("opaque".into()),
            "an un-typeable method receiver is marked opaque"
        );
    }

    #[test]
    fn typed_method_call_is_not_marked_opaque() {
        // A method call whose receiver type *is* inferred carries the type hint
        // and is NOT opaque — resolution should type-narrow, not decline.
        let src = r#"
struct Widget;
impl Widget { fn new() -> Widget { Widget } fn go(&self) {} }
fn build() {
    let w = Widget::new();
    w.go();
}
"#;
        let ex = extract(src);
        assert_eq!(hint_for(&ex, "go"), Some("Widget".into()));
        assert_eq!(recv_for(&ex, "go"), None, "a typed receiver is not opaque");
    }

    #[test]
    fn bare_call_is_not_marked_opaque() {
        // A bare free/associated call is not a method call — it genuinely denotes
        // a name that may live in another file, so it must NOT be marked opaque.
        let src = "fn a() { helper(); }\nfn helper() {}\n";
        assert_eq!(recv_for(&extract(src), "helper"), None);
    }

    #[test]
    fn fn_node_records_return_type() {
        // Each fn/method stamps its declared return type (`returns` attr) so the
        // resolver can infer the type of a variable bound to its result. A fn with
        // no return type carries no attr; `-> Self` records the enclosing impl.
        let src = r#"
struct Widget;
fn make() -> Widget { Widget }
fn nothing() {}
impl Widget { fn dup(&self) -> Self { Widget } }
"#;
        let ex = extract(src);
        assert_eq!(returns_of(&ex, "make"), Some("Widget".into()));
        assert_eq!(returns_of(&ex, "nothing"), None, "no return type → no attr");
        assert_eq!(
            returns_of(&ex, "dup"),
            Some("Widget".into()),
            "-> Self records the concrete impl type"
        );
    }

    #[test]
    fn method_ids_are_owner_qualified_labels_stay_bare() {
        // A method's id is `Owner::name` (semantic); its label stays the bare name.
        // Two structs' `new` become distinct ids with NO hash — the owner already
        // makes them unique (ADR-0028). A free fn keeps its plain id.
        let src = r#"
struct A;
struct B;
impl A { fn new() -> Self { A } }
impl B { fn new() -> Self { B } }
fn parse() {}
"#;
        let ex = extract(src);
        let new: Vec<&str> = ex
            .nodes
            .iter()
            .filter(|n| n.label == "new")
            .map(|n| n.id.0.as_str())
            .collect();
        assert_eq!(new.len(), 2);
        assert!(
            new.contains(&"fn:src/demo.rs:A::new") && new.contains(&"fn:src/demo.rs:B::new"),
            "owner-qualified, no hash: {new:?}"
        );
        let parse = ex.nodes.iter().find(|n| n.label == "parse").unwrap();
        assert_eq!(
            parse.id.0, "fn:src/demo.rs:parse",
            "free fn id is unqualified"
        );
        assert!(
            !ex.nodes.iter().any(|n| n.id.0.contains('#')),
            "no hash needed here"
        );
    }

    #[test]
    fn true_overload_same_owner_gets_a_signature_hash() {
        // Two `from` on the SAME owner W (two From impls) — same qualified name
        // `W::from`, different signatures → a real overload → hashed by signature.
        // Contains edges follow the rewrite; no `~n` counter survives (ADR-0028).
        let src = r#"
struct W;
impl From<u8> for W { fn from(v: u8) -> Self { W } }
impl From<u16> for W { fn from(v: u16) -> Self { W } }
"#;
        let ex = extract(src);
        let from_ids: Vec<&str> = ex
            .nodes
            .iter()
            .filter(|n| n.label == "from")
            .map(|n| n.id.0.as_str())
            .collect();
        assert_eq!(from_ids.len(), 2);
        assert!(
            from_ids
                .iter()
                .all(|id| id.starts_with("fn:src/demo.rs:W::from#")),
            "same-owner overload hashed by signature: {from_ids:?}"
        );
        assert_ne!(
            from_ids[0], from_ids[1],
            "distinct signatures → distinct hashes"
        );
        assert!(
            !ex.nodes.iter().any(|n| n.id.0.contains('~')),
            "no ordinal fallback"
        );

        let ids: std::collections::HashSet<&str> =
            ex.nodes.iter().map(|n| n.id.0.as_str()).collect();
        for e in &ex.edges {
            if let EdgeTarget::Node(t) = &e.target {
                assert!(ids.contains(t.0.as_str()), "dangling edge target {}", t.0);
            }
        }
    }

    #[test]
    fn fn_node_records_full_line_span() {
        // A node's `source_span` covers the definition start..end line (1-based,
        // inclusive) — not just the start — so a consumer can read exactly the
        // body. `\n` on line 1 puts `struct` on line 2, `fn` opens on line 3.
        let src = "\nstruct Widget;\nfn make() -> Widget {\n    Widget\n}\n";
        let ex = extract(src);
        let make = ex
            .nodes
            .iter()
            .find(|n| n.label == "make")
            .expect("make node");
        assert_eq!(
            make.source_span,
            Some(Span { start: 3, end: 5 }),
            "span covers the whole fn, not only its first line"
        );
        let widget = ex
            .nodes
            .iter()
            .find(|n| n.label == "Widget")
            .expect("Widget node");
        assert_eq!(
            widget.source_span,
            Some(Span::line(2)),
            "a one-line item is a single-line span"
        );
    }

    #[test]
    fn let_binding_from_free_call_defers_to_return_type() {
        // `let w = compute(); w.go();` — the RHS is a plain call, so w's type is
        // not yet knowable here (compute may live in another file). Instead of
        // marking the receiver opaque, defer: emit `recv_returns=compute` so the
        // resolver can look up compute's return type and narrow `go`.
        let src = r#"
fn build() {
    let w = compute();
    w.go();
}
"#;
        let ex = extract(src);
        assert_eq!(hint_named(&ex, "go", "type"), None, "no concrete type yet");
        assert_eq!(recv_for(&ex, "go"), None, "not opaque — deferred");
        assert_eq!(
            hint_named(&ex, "go", "recv_returns"),
            Some("compute".into()),
            "receiver type is deferred to compute's return type"
        );
    }

    #[test]
    fn associated_non_constructor_call_defers_with_owner() {
        // `let x = Foo::make(); x.go();` — `make` is not a constructor convention,
        // so the type is the return of `Foo::make`. Defer with the owner so the
        // resolver narrows the callee to Foo's `make` before reading its return.
        let src = r#"
fn build() {
    let x = Foo::make();
    x.go();
}
"#;
        let ex = extract(src);
        assert_eq!(hint_named(&ex, "go", "recv_returns"), Some("make".into()));
        assert_eq!(
            hint_named(&ex, "go", "recv_returns_owner"),
            Some("Foo".into())
        );
    }

    #[test]
    fn method_chain_receiver_stays_opaque_not_deferred() {
        // `let x = a.b(); x.go();` — the RHS is a *method* call (a chain). Threading
        // its return type is out of scope (needs receiver-type-of-the-receiver), so
        // it stays honestly opaque, never a bare-name guess (ADR-0023).
        let src = r#"
fn build() {
    let x = a.b();
    x.go();
}
"#;
        let ex = extract(src);
        assert_eq!(recv_for(&ex, "go"), Some("opaque".into()));
        assert_eq!(hint_named(&ex, "go", "recv_returns"), None);
    }

    #[test]
    fn deterministic_across_runs() {
        let src = "fn a() { b(); }\nfn b() {}\n";
        let first = extract(src);
        let second = extract(src);
        assert_eq!(first, second, "same input → identical extraction");
    }

    #[test]
    fn exports_pub_items_only() {
        // `pub` items are the module surface; private items and `pub` methods are
        // not module exports (ADR-0020 — Rust emits Local exports).
        let src = "pub fn greet() {}\nfn hidden() {}\npub struct Widget;\nstruct Secret;\nimpl Widget { pub fn m(&self) {} }\n";
        let ex = extract(src);
        use filigrio_core::Export;
        assert!(
            ex.exports.contains(&Export::Local {
                name: "greet".into()
            }),
            "{:?}",
            ex.exports
        );
        assert!(
            ex.exports.contains(&Export::Local {
                name: "Widget".into()
            }),
            "{:?}",
            ex.exports
        );
        assert!(
            !ex.exports.contains(&Export::Local {
                name: "hidden".into()
            }),
            "private fn"
        );
        assert!(
            !ex.exports.contains(&Export::Local {
                name: "Secret".into()
            }),
            "private struct"
        );
        assert!(
            !ex.exports.contains(&Export::Local { name: "m".into() }),
            "pub method is not a module export"
        );
    }

    /// The `imports` edge whose bound name is `name`, with its specifier hint.
    fn import_spec(ex: &Extraction, name: &str) -> Option<Option<String>> {
        ex.edges.iter().find_map(|e| match &e.target {
            EdgeTarget::Symbol(r) if e.relation == "imports" && r.name == name => {
                Some(r.hints.get("specifier").cloned())
            }
            _ => None,
        })
    }

    #[test]
    fn use_carries_module_path_specifier() {
        // `use crate::api::greet;` → imports `greet` with the module mod-path
        // `crate::api` as the specifier, so the resolver can bind it (ADR-0020).
        let src = "use crate::api::greet;\nfn boot() { greet(); }\n";
        let ex = extract(src);
        assert_eq!(
            import_spec(&ex, "greet"),
            Some(Some("crate::api".into())),
            "specifier is the path minus the imported symbol: {:?}",
            ex.edges
        );
    }

    #[test]
    fn use_single_segment_has_no_specifier() {
        // `use foo;` — a single segment names no module path.
        let ex = extract("use foo;\n");
        assert_eq!(import_spec(&ex, "foo"), Some(None));
    }

    #[test]
    fn use_alias_records_imported_name() {
        // `use crate::api::greet as hi;` → bound name `hi`, imported `greet`.
        let ex = extract("use crate::api::greet as hi;\n");
        let e = ex
            .edges
            .iter()
            .find_map(|e| match &e.target {
                EdgeTarget::Symbol(r) if e.relation == "imports" && r.name == "hi" => Some(r),
                _ => None,
            })
            .expect("aliased import bound as `hi`");
        assert_eq!(
            e.hints.get("specifier").map(String::as_str),
            Some("crate::api")
        );
        assert_eq!(e.hints.get("imported").map(String::as_str), Some("greet"));
    }

    #[test]
    fn pub_use_emits_reexport() {
        // `pub use crate::api::greet;` — a re-export (barrel) the export graph
        // follows to `greet`'s real definition. Closes the ADR-0020 Rust gap.
        use filigrio_core::Export;
        let ex = extract("pub use crate::api::greet;\n");
        assert!(
            ex.exports.contains(&Export::ReExport {
                name: "greet".into(),
                specifier: "crate::api".into(),
                imported: "greet".into(),
            }),
            "{:?}",
            ex.exports
        );
    }

    #[test]
    fn pub_use_alias_reexport() {
        // `pub use crate::api::greet as hello;` → re-export exposed as `hello`.
        use filigrio_core::Export;
        let ex = extract("pub use crate::api::greet as hello;\n");
        assert!(
            ex.exports.contains(&Export::ReExport {
                name: "hello".into(),
                specifier: "crate::api".into(),
                imported: "greet".into(),
            }),
            "{:?}",
            ex.exports
        );
    }

    #[test]
    fn pub_use_glob_emits_star() {
        // `pub use crate::api::*;` → a wildcard re-export.
        use filigrio_core::Export;
        let ex = extract("pub use crate::api::*;\n");
        assert!(
            ex.exports.contains(&Export::Star {
                specifier: "crate::api".into(),
            }),
            "{:?}",
            ex.exports
        );
    }

    // ---- ADR-0036 structural edges (Phase 0b unit 1) -----------------------

    /// The first edge with `relation` whose target Symbol name is `target`.
    fn has_sym_edge(ex: &Extraction, relation: &str, source_label: &str, target: &str) -> bool {
        let src_id = ex
            .nodes
            .iter()
            .find(|n| n.label == source_label)
            .map(|n| &n.id);
        ex.edges.iter().any(|e| {
            e.relation == relation
                && src_id.map(|s| s == &e.source).unwrap_or(false)
                && matches!(&e.target, EdgeTarget::Symbol(r) if r.name == target)
        })
    }

    /// The `source` id of the `contains` edge whose target is the node labelled
    /// `label` (the container that holds that definition).
    fn contains_source<'a>(ex: &'a Extraction, label: &str) -> Option<&'a NodeId> {
        let target_id = &ex.nodes.iter().find(|n| n.label == label)?.id;
        ex.edges.iter().find_map(|e| match &e.target {
            EdgeTarget::Node(t) if e.relation == "contains" && t == target_id => Some(&e.source),
            _ => None,
        })
    }

    fn file_id(ex: &Extraction) -> &NodeId {
        &ex.nodes.iter().find(|n| n.kind == "file").unwrap().id
    }

    #[test]
    fn impl_method_is_contained_by_its_type() {
        // ADR-0036 #4: a method in `impl S { .. }` is contained by S's node, not
        // the file (matching TS/Python class-method containment). The `impl` attr
        // is retained.
        let src = "struct S {}\nimpl S { fn m(&self) {} }\n";
        let ex = extract(src);
        let s_id = &ex.nodes.iter().find(|n| n.label == "S").unwrap().id;
        assert_eq!(
            contains_source(&ex, "m"),
            Some(s_id),
            "method m is contained by struct S, not the file: {:?}",
            ex.edges
        );
        let m = ex.nodes.iter().find(|n| n.label == "m").unwrap();
        assert_eq!(
            m.attrs.get("impl").map(String::as_str),
            Some("S"),
            "the impl owner attr is preserved"
        );
    }

    #[test]
    fn impl_before_type_still_type_contained() {
        // Ordering: `impl S` precedes `struct S`, so `type_ids` has no S when the
        // method is walked. Containment is deferred to post-walk, so the method is
        // still re-parented to S once its node is registered.
        let src = "impl S { fn m(&self) {} }\nstruct S {}\n";
        let ex = extract(src);
        let s_id = &ex.nodes.iter().find(|n| n.label == "S").unwrap().id;
        assert_eq!(
            contains_source(&ex, "m"),
            Some(s_id),
            "impl-before-type is still type-contained (deferred resolution)"
        );
    }

    #[test]
    fn foreign_type_impl_method_falls_back_to_file() {
        // `impl Foreign { .. }` where Foreign has no local node → no type node to
        // anchor to, so the method stays file-contained (ADR-0023: no invented
        // node). Same posture as a blanket/`dyn`/foreign-type impl.
        let src = "impl Foreign { fn m(&self) {} }\n";
        let ex = extract(src);
        assert_eq!(
            contains_source(&ex, "m"),
            Some(file_id(&ex)),
            "a method on a non-local type falls back to file containment"
        );
    }

    #[test]
    fn free_function_stays_file_contained() {
        // A free fn (not in an impl) is unchanged: still contained by the file.
        let src = "fn helper() {}\nstruct S {}\nimpl S { fn m(&self) {} }\n";
        let ex = extract(src);
        assert_eq!(
            contains_source(&ex, "helper"),
            Some(file_id(&ex)),
            "free functions remain file-contained"
        );
    }

    #[test]
    fn trait_method_is_contained_by_its_trait_impl_type() {
        // `impl Foo for Bar { fn m .. }` — the method is contained by the impl'd
        // *type* Bar (where the concrete node lives), not the trait Foo nor the
        // file.
        let src =
            "struct Bar {}\ntrait Foo { fn m(&self); }\nimpl Foo for Bar { fn m(&self) {} }\n";
        let ex = extract(src);
        let bar_id = &ex.nodes.iter().find(|n| n.label == "Bar").unwrap().id;
        // two `m` nodes exist (the trait decl's and Bar's impl); the impl one is
        // contained by Bar.
        let bar_contains_m = ex.edges.iter().any(|e| {
            e.relation == "contains"
                && &e.source == bar_id
                && matches!(&e.target, EdgeTarget::Node(t)
                    if ex.nodes.iter().any(|n| &n.id == t && n.label == "m"))
        });
        assert!(
            bar_contains_m,
            "Bar contains its impl method m: {:?}",
            ex.edges
        );
    }

    #[test]
    fn impl_trait_emits_implements_edge() {
        // `impl Foo for Bar` → an `implements` edge Bar → Foo (source is Bar's
        // def node; the trait is an unresolved Symbol). ADR-0037a.
        let src = r#"
struct Bar;
trait Foo {}
impl Foo for Bar {}
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "implements", "Bar", "Foo"),
            "Bar implements Foo: {:?}",
            ex.edges
        );
    }

    #[test]
    fn supertrait_emits_extends_edge() {
        // `trait A: B` → an `extends` edge A → B (from the trait node).
        let src = r#"
trait B {}
trait A: B {}
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "extends", "A", "B"),
            "trait A extends B: {:?}",
            ex.edges
        );
    }

    #[test]
    fn struct_field_emits_field_type_edge() {
        // `struct S { x: T }` → a `type/field` edge S → T carrying the field
        // name `x` (ADR-0036 §1a: no field node). A primitive-typed field has no
        // nameable target and emits nothing.
        let src = r#"
struct T;
struct S { x: T, count: u32 }
"#;
        let ex = extract(src);
        let s_id = &ex.nodes.iter().find(|n| n.label == "S").unwrap().id;
        let field = ex.edges.iter().find(|e| {
            e.relation == "type/field"
                && &e.source == s_id
                && matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "T")
        });
        let field = field.expect("field_type S -> T present");
        if let EdgeTarget::Symbol(r) = &field.target {
            assert_eq!(
                r.hints.get("name").map(String::as_str),
                Some("x"),
                "field name rides the edge"
            );
        }
        // no field node was created
        assert!(
            !ex.nodes.iter().any(|n| n.kind == "field"),
            "fields are edges, not nodes"
        );
    }

    #[test]
    fn enum_emits_variant_nodes_and_has_variant_edges() {
        // `enum E { A, B(u8) }` → two `enum_variant` nodes `E::A` / `E::B`
        // (owner-qualified ids, ADR-0028) + `has_variant` edges E → each.
        let src = r#"
enum E { A, B(u8) }
"#;
        let ex = extract(src);
        let enum_id = &ex
            .nodes
            .iter()
            .find(|n| n.label == "E" && n.kind == "enum")
            .unwrap()
            .id;
        let variants: Vec<&Node> = ex
            .nodes
            .iter()
            .filter(|n| n.kind == "enum_variant")
            .collect();
        assert_eq!(variants.len(), 2, "two enum_variant nodes: {:?}", variants);
        let ids: Vec<&str> = variants.iter().map(|n| n.id.0.as_str()).collect();
        assert!(
            ids.iter().any(|id| id.ends_with("E::A")) && ids.iter().any(|id| id.ends_with("E::B")),
            "owner-qualified variant ids: {ids:?}"
        );
        for v in &variants {
            assert!(
                ex.edges.iter().any(|e| e.relation == "has_variant"
                    && &e.source == enum_id
                    && matches!(&e.target, EdgeTarget::Node(t) if t == &v.id)),
                "has_variant E -> {}",
                v.label
            );
        }
    }

    #[test]
    fn pub_use_list_reexports_each() {
        // `pub use crate::api::{greet, wave};` → one re-export per name.
        use filigrio_core::Export;
        let ex = extract("pub use crate::api::{greet, wave};\n");
        for name in ["greet", "wave"] {
            assert!(
                ex.exports.contains(&Export::ReExport {
                    name: name.into(),
                    specifier: "crate::api".into(),
                    imported: name.into(),
                }),
                "missing {name}: {:?}",
                ex.exports
            );
        }
    }

    // ---- type collection tests (matching Python's _rust_collect_type_refs) --

    /// Helper to collect type refs from a type expression string. Returns (name, role) pairs.
    fn collect_type_refs(src: &str) -> Vec<(String, String)> {
        let artifact = Artifact {
            path: "test.rs".into(),
            kind: ArtifactKind::Code,
            language: Some("rust".into()),
        };
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src.as_bytes(), None).unwrap();
        let ctx = Ctx::new(&artifact, src.as_bytes());

        // Look for field declarations and extract their type nodes
        let mut cursor = tree.root_node().walk();
        for node in tree.root_node().children(&mut cursor) {
            if let Some(type_node) = find_field_type(node) {
                let mut refs = Vec::new();
                ctx.collect_type_refs_recursive(type_node, false, &mut refs);
                return refs;
            }
        }
        Vec::new()
    }

    /// Find the type of a field declaration in a struct
    fn find_field_type(node: TsNode<'_>) -> Option<TsNode<'_>> {
        if node.kind() == "field_declaration" {
            // Get the type field
            return node.child_by_field_name("type");
        } else if node.is_named() {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if let Some(found) = find_field_type(child) {
                    return Some(found);
                }
            }
        }
        None
    }

    #[test]
    fn simple_type_identifier_collected() {
        // String → [("String", "type")]
        let refs = collect_type_refs("struct S { x: String }");
        assert_eq!(refs, vec![("String".into(), "type".into())]);
    }

    #[test]
    fn scoped_type_identifier_collected() {
        // a::Foo → [("a::Foo", "type")] (qualified path preserved, not stripped)
        let refs = collect_type_refs("struct S { x: a::Foo }");
        assert_eq!(refs, vec![("a::Foo".into(), "type".into())]);
    }

    #[test]
    fn generic_type_with_single_argument() {
        // HashMap<String, u32> → [("HashMap", "type"), ("String", "generic_arg")]
        // Note: u32 is a primitive and is NOT collected (Python behavior: primitive_type returns early)
        let refs = collect_type_refs("struct S { x: HashMap<String, u32> }");
        assert_eq!(refs.len(), 2);
        assert!(refs.contains(&(String::from("HashMap"), String::from("type"))));
        assert!(refs.contains(&(String::from("String"), String::from("generic_arg"))));
    }

    #[test]
    fn nested_generic_type() {
        // HashMap<String, Vec<Widget>> → [("HashMap", "type"), ("String", "generic_arg"),
        //                                     ("Vec", "generic_arg"), ("Widget", "generic_arg")]
        let refs = collect_type_refs("struct S { x: HashMap<String, Vec<Widget>> }");
        assert_eq!(refs.len(), 4);
        assert!(refs.contains(&(String::from("HashMap"), String::from("type"))));
        assert!(refs.contains(&(String::from("String"), String::from("generic_arg"))));
        assert!(refs.contains(&(String::from("Vec"), String::from("generic_arg"))));
        assert!(refs.contains(&(String::from("Widget"), String::from("generic_arg"))));
    }

    #[test]
    fn reference_type_unwrapped() {
        // &mut Foo → [("Foo", "type")] (reference wrapper discarded)
        let refs = collect_type_refs("struct S { x: &mut Foo }");
        assert_eq!(refs, vec![("Foo".into(), "type".into())]);
    }

    #[test]
    fn reference_type_nested() {
        // &mut HashMap<String, Vec<Widget>> → [("HashMap", "type"), ("String", "generic_arg"),
        //                                            ("Vec", "generic_arg"), ("Widget", "generic_arg")]
        let refs = collect_type_refs("struct S { x: &mut HashMap<String, Vec<Widget>> }");
        assert_eq!(refs.len(), 4);
        assert!(refs.contains(&(String::from("HashMap"), String::from("type"))));
        assert!(refs.contains(&(String::from("String"), String::from("generic_arg"))));
        assert!(refs.contains(&(String::from("Vec"), String::from("generic_arg"))));
        assert!(refs.contains(&(String::from("Widget"), String::from("generic_arg"))));
    }

    #[test]
    fn array_type_unwrapped() {
        // [u8; 10] → [] (primitive u8 is NOT collected)
        let refs = collect_type_refs("struct S { x: [u8; 10] }");
        assert_eq!(refs, Vec::<(String, String)>::new());
    }

    #[test]
    fn array_type_with_nested() {
        // [Vec<String>; 5] → [("Vec", "type"), ("String", "generic_arg")]
        let refs = collect_type_refs("struct S { x: [Vec<String>; 5] }");
        assert_eq!(refs.len(), 2);
        assert!(refs.contains(&(String::from("Vec"), String::from("type"))));
        assert!(refs.contains(&(String::from("String"), String::from("generic_arg"))));
    }

    #[test]
    fn tuple_type_unwrapped() {
        // (String, i32) → [("String", "type")]
        // Note: i32 is a primitive and is NOT collected
        let refs = collect_type_refs("struct S { x: (String, i32) }");
        assert_eq!(refs.len(), 1);
        assert!(refs.contains(&(String::from("String"), String::from("type"))));
    }

    #[test]
    fn tuple_type_with_nested() {
        // (HashMap<String, Config>, Error) → [("HashMap", "type"), ("String", "generic_arg"),
        //                                           ("Config", "generic_arg"), ("Error", "type")]
        let refs = collect_type_refs("struct S { x: (HashMap<String, Config>, Error) }");
        assert_eq!(refs.len(), 4);
        assert!(refs.contains(&(String::from("HashMap"), String::from("type"))));
        assert!(refs.contains(&(String::from("String"), String::from("generic_arg"))));
        assert!(refs.contains(&(String::from("Config"), String::from("generic_arg"))));
        assert!(refs.contains(&(String::from("Error"), String::from("type"))));
    }

    #[test]
    fn pointer_type_unwrapped() {
        // *const T → [("T", "type")] (pointer wrapper discarded)
        let refs = collect_type_refs("struct S { x: *const T }");
        assert_eq!(refs, vec![("T".into(), "type".into())]);
    }

    #[test]
    fn slice_type_unwrapped() {
        // &[u8] → [] (primitive u8 is NOT collected)
        let refs = collect_type_refs("struct S { x: &[u8] }");
        assert_eq!(refs, Vec::<(String, String)>::new());
    }

    #[test]
    fn complex_nested_generic_expression() {
        // Result<Vec<HashMap<String, Config>>, Error> → [("Result", "type"), ("Vec", "generic_arg"),
        //                                                   ("HashMap", "generic_arg"), ("String", "generic_arg"),
        //                                                   ("Config", "generic_arg"), ("Error", "generic_arg")]
        let refs = collect_type_refs("struct S { x: Result<Vec<HashMap<String, Config>>, Error> }");
        assert_eq!(refs.len(), 6);
        assert!(refs.contains(&(String::from("Result"), String::from("type"))));
        assert!(refs.contains(&(String::from("Vec"), String::from("generic_arg"))));
        assert!(refs.contains(&(String::from("HashMap"), String::from("generic_arg"))));
        assert!(refs.contains(&(String::from("String"), String::from("generic_arg"))));
        assert!(refs.contains(&(String::from("Config"), String::from("generic_arg"))));
        assert!(refs.contains(&(String::from("Error"), String::from("generic_arg"))));
    }

    #[test]
    fn box_wrapped_type() {
        // Box<T> → [("Box", "type"), ("T", "generic_arg")]
        let refs = collect_type_refs("struct S { x: Box<T> }");
        assert_eq!(refs.len(), 2);
        assert!(refs.contains(&(String::from("Box"), String::from("type"))));
        assert!(refs.contains(&(String::from("T"), String::from("generic_arg"))));
    }

    #[test]
    fn primitive_type_ignored() {
        // u32 → [] (primitive types are ignored)
        let refs = collect_type_refs("struct S { x: u32 }");
        assert_eq!(refs, Vec::<(String, String)>::new());
    }

    #[test]
    fn complex_mixed_type_expression() {
        // &mut Vec<Result<HashMap<String, Config>, Error>> → [("Vec", "type"), ("Result", "generic_arg"),
        //                                                        ("HashMap", "generic_arg"), ("String", "generic_arg"),
        //                                                        ("Config", "generic_arg"), ("Error", "generic_arg")]
        let refs =
            collect_type_refs("struct S { x: &mut Vec<Result<HashMap<String, Config>, Error>> }");
        assert_eq!(refs.len(), 6);
        assert!(refs.contains(&(String::from("Vec"), String::from("type"))));
        assert!(refs.contains(&(String::from("Result"), String::from("generic_arg"))));
        assert!(refs.contains(&(String::from("HashMap"), String::from("generic_arg"))));
        assert!(refs.contains(&(String::from("String"), String::from("generic_arg"))));
        assert!(refs.contains(&(String::from("Config"), String::from("generic_arg"))));
        assert!(refs.contains(&(String::from("Error"), String::from("generic_arg"))));
    }

    // ---- Return type reference edges tests (matching Python's emit_param_return_refs) --

    /// Helper to find references edges from a specific node
    fn find_references_edges<'a>(ex: &'a Extraction, source_id: &str) -> Vec<&'a Edge> {
        ex.edges
            .iter()
            .filter(|e| e.relation == "type/return" && matches!(&e.source, s if s.0 == source_id))
            .collect()
    }

    fn find_param_type_edges<'a>(ex: &'a Extraction, source_id: &str) -> Vec<&'a Edge> {
        ex.edges
            .iter()
            .filter(|e| e.relation == "type/param" && matches!(&e.source, s if s.0 == source_id))
            .collect()
    }

    fn find_field_type_edges<'a>(ex: &'a Extraction, source_id: &str) -> Vec<&'a Edge> {
        ex.edges
            .iter()
            .filter(|e| e.relation == "type/field" && matches!(&e.source, s if s.0 == source_id))
            .collect()
    }

    /// Helper to get node ID by label and kind
    fn find_node_id(ex: &Extraction, label: &str, kind: &str) -> Option<String> {
        ex.nodes
            .iter()
            .find(|n| n.label == label && n.kind == kind)
            .map(|n| n.id.0.clone())
    }

    #[test]
    fn simple_return_type_creates_return_type_edge() {
        // fn make() -> Widget should create edge: make -> Widget with relation "type/return"
        let src = r#"
struct Widget;
fn make() -> Widget { Widget }
"#;
        let ex = extract(src);
        let make_id = find_node_id(&ex, "make", "function").expect("make function exists");
        let refs = find_references_edges(&ex, &make_id);

        assert!(
            !refs.is_empty(),
            "should have return_type edges for return type"
        );

        // Find the edge to Widget
        let widget_edge = refs
            .iter()
            .find(|e| matches!(&e.target, EdgeTarget::Symbol(t) if t.name == "Widget"));
        assert!(
            widget_edge.is_some(),
            "should have return_type edge to Widget"
        );
    }

    #[test]
    fn generic_return_type_creates_multiple_return_type_edges() {
        // fn returns_generic() -> HashMap<String, Config> should create edges:
        //   - function -> String (local type, not filtered)
        //   - function -> Config (local type, not filtered)
        // NOTE: HashMap is filtered out as standard library noise (ADR-0036 R1.1)
        let src = r#"
struct String;
struct Config;
fn returns_generic() -> HashMap<String, Config> { HashMap::new() }
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "returns_generic", "function").expect("function exists");
        let refs = find_references_edges(&ex, &func_id);

        assert_eq!(
            refs.len(),
            2,
            "should have 2 return_type edges (HashMap filtered)"
        );

        // Check each target type (HashMap filtered as noise)
        let target_names: Vec<String> = refs
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(t) = &e.target {
                    Some(t.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(
            !target_names.contains(&String::from("HashMap")),
            "HashMap should be filtered"
        );
        assert!(
            target_names.contains(&String::from("String")),
            "should have return_type edge to String (local type)"
        );
        assert!(
            target_names.contains(&String::from("Config")),
            "should have return_type edge to Config (local type)"
        );
    }

    #[test]
    fn nested_generic_return_type_creates_all_return_types() {
        // fn returns_nested() -> Result<Vec<String>, Error> should create edges to:
        //   - String (local, not filtered)
        //   - Error (local, not filtered)
        // NOTE: Result and Vec are filtered as standard library noise (ADR-0036 R1.1)
        let src = r#"
struct String;
struct Error;
fn returns_nested() -> Result<Vec<String>, Error> { Ok(vec![]) }
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "returns_nested", "function").expect("function exists");
        let refs = find_references_edges(&ex, &func_id);

        assert_eq!(
            refs.len(),
            2,
            "should have 2 return_type edges (Result and Vec filtered)"
        );

        let target_names: Vec<String> = refs
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(t) = &e.target {
                    Some(t.name.clone())
                } else {
                    None
                }
            })
            .collect();

        // Standard library types should be filtered
        assert!(
            !target_names.contains(&String::from("Result")),
            "Result should be filtered"
        );
        assert!(
            !target_names.contains(&String::from("Vec")),
            "Vec should be filtered"
        );

        // Local types should be preserved
        assert!(
            target_names.contains(&String::from("String")),
            "should have return_type edge to String (local)"
        );
        assert!(
            target_names.contains(&String::from("Error")),
            "should have return_type edge to Error (local)"
        );
    }

    #[test]
    fn return_type_with_reference_unwraps_correctly() {
        // fn returns_ref() -> &Widget should create edge to Widget (not to the reference)
        let src = r#"
struct Widget;
fn returns_ref() -> &Widget { &Widget }
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "returns_ref", "function").expect("function exists");
        let refs = find_references_edges(&ex, &func_id);

        assert!(!refs.is_empty(), "should have return_type edges");

        // Should reference Widget, not the reference type
        let widget_edge = refs
            .iter()
            .find(|e| matches!(&e.target, EdgeTarget::Symbol(t) if t.name == "Widget"));
        assert!(
            widget_edge.is_some(),
            "should have return_type edge to Widget"
        );
    }

    #[test]
    fn return_type_self_resolves_to_concrete_type() {
        // impl Widget { fn returns_self(&self) -> Self } should create edge to Widget
        let src = r#"
struct Widget;
impl Widget {
    fn returns_self(&self) -> Self { Widget }
}
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "returns_self", "function").expect("function exists");
        let refs = find_references_edges(&ex, &func_id);

        assert!(
            !refs.is_empty(),
            "should have return_type edges for Self return type"
        );

        // Self should resolve to Widget
        let widget_edge = refs
            .iter()
            .find(|e| matches!(&e.target, EdgeTarget::Symbol(t) if t.name == "Widget"));
        assert!(widget_edge.is_some(), "Self should resolve to Widget");
    }

    #[test]
    fn return_type_with_generic_self_resolves() {
        // impl<T> Container<T> { fn get(&self) -> T }
        // NOTE: Generic parameter T is filtered as noise (ADR-0036 R1.1)
        let src = r#"
struct Container<T>(T);
impl<T> Container<T> {
    fn get(&self) -> T { self.0 }
}
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "get", "function").expect("function exists");
        let refs = find_references_edges(&ex, &func_id);

        assert!(
            refs.is_empty(),
            "should have no return_type edges (generic parameter T filtered)"
        );

        // Generic parameter T should not be referenced
        let t_edge = refs
            .iter()
            .find(|e| matches!(&e.target, EdgeTarget::Symbol(t) if t.name == "T"));
        assert!(t_edge.is_none(), "generic parameter T should be filtered");
    }

    #[test]
    fn no_return_type_creates_no_return_type_edges() {
        // fn no_return() { } should not create any return type edges
        let src = r#"
fn no_return() { }
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "no_return", "function").expect("function exists");
        let refs = find_references_edges(&ex, &func_id);

        assert!(
            refs.is_empty(),
            "should not have return_type edges when no return type"
        );
    }

    #[test]
    fn primitive_return_type_creates_no_return_type_edges() {
        // fn returns_u32() -> u32 should not create return_type edges (primitives ignored)
        let src = r#"
fn returns_u32() -> u32 { 42 }
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "returns_u32", "function").expect("function exists");
        let refs = find_references_edges(&ex, &func_id);

        assert!(
            refs.is_empty(),
            "should not create return_type edges for primitive types"
        );
    }

    #[test]
    fn complex_nested_return_type_all_collected() {
        // fn returns_complex() -> Result<Vec<HashMap<String, Config>>, Error>
        // NOTE: Result, Vec, and HashMap are filtered as standard library noise (ADR-0036 R1.1)
        let src = r#"
struct String;
struct Config;
struct Error;
fn returns_complex() -> Result<Vec<HashMap<String, Config>>, Error> { Ok(vec![]) }
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "returns_complex", "function").expect("function exists");
        let refs = find_references_edges(&ex, &func_id);

        // After filtering: String, Config, Error = 3 total
        // Result, Vec, HashMap filtered as standard library noise
        assert_eq!(
            refs.len(),
            3,
            "should have 3 return_type edges after noise filtering (Result, Vec, HashMap filtered)"
        );

        let target_names: Vec<String> = refs
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(t) = &e.target {
                    Some(t.name.clone())
                } else {
                    None
                }
            })
            .collect();

        // Standard library types should be filtered
        assert!(
            !target_names.contains(&String::from("Result")),
            "Result should be filtered"
        );
        assert!(
            !target_names.contains(&String::from("Vec")),
            "Vec should be filtered"
        );
        assert!(
            !target_names.contains(&String::from("HashMap")),
            "HashMap should be filtered"
        );

        // Local types should be preserved
        assert!(
            target_names.contains(&String::from("String")),
            "String should be present (local)"
        );
        assert!(
            target_names.contains(&String::from("Config")),
            "Config should be present (local)"
        );
        assert!(
            target_names.contains(&String::from("Error")),
            "Error should be present (local)"
        );
    }

    #[test]
    fn simple_parameter_type_creates_param_type_edge() {
        // fn use_item(item: Widget) should create edge: use_item -> Widget with relation "type/param"
        let src = r#"
struct Widget;
fn use_item(item: Widget) { }
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "use_item", "function").expect("function exists");
        let refs = find_param_type_edges(&ex, &func_id);

        assert!(
            !refs.is_empty(),
            "should have param_type edges for parameter type"
        );

        // Find the edge to Widget
        let widget_edge = refs
            .iter()
            .find(|e| matches!(&e.target, EdgeTarget::Symbol(t) if t.name == "Widget"));
        assert!(
            widget_edge.is_some(),
            "should have param_type edge to Widget"
        );
    }

    #[test]
    fn generic_parameter_type_creates_multiple_param_type_edges() {
        // fn process(data: HashMap<String, Config>) should create edges:
        //   - function -> HashMap
        //   - function -> String
        //   - function -> Config
        // No distinction between base types and generic args
        let src = r#"
struct HashMap;
struct String;
struct Config;
fn process(data: HashMap<String, Config>) { }
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "process", "function").expect("function exists");
        let refs = find_param_type_edges(&ex, &func_id);

        assert_eq!(
            refs.len(),
            3,
            "should have 3 param_type edges for generic parameter type"
        );

        let target_names: Vec<String> = refs
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(t) = &e.target {
                    Some(t.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(target_names.contains(&String::from("HashMap")));
        assert!(target_names.contains(&String::from("String")));
        assert!(target_names.contains(&String::from("Config")));
    }

    #[test]
    fn multiple_parameters_create_param_type_edges() {
        // fn multi(a: Widget, b: Config, c: String) should create edges to all three types
        let src = r#"
struct Widget;
struct Config;
struct String;
fn multi(a: Widget, b: Config, c: String) { }
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "multi", "function").expect("function exists");
        let refs = find_param_type_edges(&ex, &func_id);

        assert_eq!(
            refs.len(),
            3,
            "should have 3 param_type edges for multiple parameters"
        );

        let target_names: Vec<String> = refs
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(t) = &e.target {
                    Some(t.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(target_names.contains(&String::from("Widget")));
        assert!(target_names.contains(&String::from("Config")));
        assert!(target_names.contains(&String::from("String")));
    }

    #[test]
    fn reference_parameter_creates_param_type_edges() {
        // fn use_ref(items: &Vec<String>) should create edges to Vec and String
        let src = r#"
struct Vec;
struct String;
fn use_ref(items: &Vec<String>) { }
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "use_ref", "function").expect("function exists");
        let refs = find_param_type_edges(&ex, &func_id);

        assert_eq!(
            refs.len(),
            2,
            "should have 2 param_type edges for reference parameter"
        );

        let target_names: Vec<String> = refs
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(t) = &e.target {
                    Some(t.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(target_names.contains(&String::from("Vec")));
        assert!(target_names.contains(&String::from("String")));
    }

    #[test]
    fn self_parameter_creates_no_param_type_edge() {
        // impl Widget { fn go(&self) { } }
        // Should NOT create edge: go -> Widget (redundant with contains relationship)
        // Self parameter param_type edges are no longer emitted per Issue #1
        let src = r#"
struct Widget;
impl Widget {
    fn go(&self) { }
}
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "go", "function").expect("method exists");
        let refs = find_param_type_edges(&ex, &func_id);

        assert!(
            refs.is_empty(),
            "should have no param_type edges for self parameter (redundant with contains)"
        );
    }

    #[test]
    fn mutable_self_parameter_creates_no_param_type_edge() {
        // impl Widget { fn mutate(&mut self) { } }
        // Should NOT create edge: mutate -> Widget (redundant with contains relationship)
        // Self parameter param_type edges are no longer emitted per Issue #1
        let src = r#"
struct Widget;
impl Widget {
    fn mutate(&mut self) { }
}
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "mutate", "function").expect("method exists");
        let refs = find_param_type_edges(&ex, &func_id);

        assert!(
            refs.is_empty(),
            "should have no param_type edges for &mut self parameter (redundant with contains)"
        );
    }

    #[test]
    fn mixed_simple_and_generic_parameters() {
        // fn mixed(a: Widget, b: Vec<String>, c: Config) should create edges to all types
        let src = r#"
struct Widget;
struct Vec;
struct String;
struct Config;
fn mixed(a: Widget, b: Vec<String>, c: Config) { }
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "mixed", "function").expect("function exists");
        let refs = find_param_type_edges(&ex, &func_id);

        // Widget, Vec, String, Config = 4 edges
        assert_eq!(
            refs.len(),
            4,
            "should have 4 param_type edges for mixed parameters"
        );

        let target_names: Vec<String> = refs
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(t) = &e.target {
                    Some(t.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(target_names.contains(&String::from("Widget")));
        assert!(target_names.contains(&String::from("Vec")));
        assert!(target_names.contains(&String::from("String")));
        assert!(target_names.contains(&String::from("Config")));
    }

    #[test]
    fn no_parameters_creates_no_references_edges() {
        // fn no_params() { } should not create any parameter type references edges
        let src = r#"
fn no_params() { }
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "no_params", "function").expect("function exists");
        let refs = find_param_type_edges(&ex, &func_id);

        assert!(
            refs.is_empty(),
            "should not have param_type edges when no parameters"
        );
    }

    #[test]
    fn pointer_parameter_creates_references_edges() {
        // fn use_ptr(data: *mut Vec<String>) should create edges to Vec and String
        let src = r#"
struct Vec;
struct String;
fn use_ptr(data: *mut Vec<String>) { }
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "use_ptr", "function").expect("function exists");
        let refs = find_param_type_edges(&ex, &func_id);

        assert_eq!(
            refs.len(),
            2,
            "should have 2 param_type edges for pointer parameter"
        );

        let target_names: Vec<String> = refs
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(t) = &e.target {
                    Some(t.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(target_names.contains(&String::from("Vec")));
        assert!(target_names.contains(&String::from("String")));
    }

    #[test]
    fn array_parameter_creates_references_edges() {
        // fn use_arr(arr: [String; 10]) should create edge to String
        let src = r#"
struct String;
fn use_arr(arr: [String; 10]) { }
"#;
        let ex = extract(src);
        let func_id = find_node_id(&ex, "use_arr", "function").expect("function exists");
        let refs = find_param_type_edges(&ex, &func_id);

        assert_eq!(
            refs.len(),
            1,
            "should have 1 param_type edge for array parameter"
        );

        let target_names: Vec<String> = refs
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(t) = &e.target {
                    Some(t.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(target_names.contains(&String::from("String")));
    }

    #[test]
    fn tuple_struct_creates_field_type_edges() {
        // struct Pair(String, Config) should create field_type edges to String and Config
        let src = r#"
struct String;
struct Config;
struct Pair(String, Config);
"#;
        let ex = extract(src);
        let pair_id = find_node_id(&ex, "Pair", "struct").expect("Pair struct exists");
        let refs = find_field_type_edges(&ex, &pair_id);

        assert_eq!(
            refs.len(),
            2,
            "should have 2 field_type edges for tuple struct"
        );

        let target_names: Vec<String> = refs
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(t) = &e.target {
                    Some(t.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(target_names.contains(&String::from("String")));
    }

    #[test]
    fn tuple_struct_with_generics_creates_field_type_edges() {
        // struct Wrapper(HashMap<String, Vec<Config>>) should create edges to all types
        let src = r#"
struct HashMap;
struct String;
struct Vec;
struct Config;
struct Wrapper(HashMap<String, Vec<Config>>);
"#;
        let ex = extract(src);
        let wrapper_id = find_node_id(&ex, "Wrapper", "struct").expect("Wrapper struct exists");
        let refs = find_field_type_edges(&ex, &wrapper_id);

        assert_eq!(
            refs.len(),
            4,
            "should have 4 field_type edges for tuple struct with generics"
        );

        let target_names: Vec<String> = refs
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(t) = &e.target {
                    Some(t.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(target_names.contains(&String::from("HashMap")));
        assert!(target_names.contains(&String::from("String")));
        assert!(target_names.contains(&String::from("Vec")));
        assert!(target_names.contains(&String::from("Config")));
    }

    #[test]
    fn tuple_struct_with_reference_to_itself_creates_no_self_reference() {
        // struct Recursive(Option<Box<Recursive>>) should NOT create field_type edge: Recursive -> Recursive
        let src = r#"
struct Option;
struct Box;
struct Recursive(Option<Box<Recursive>>);
"#;
        let ex = extract(src);
        let recursive_id =
            find_node_id(&ex, "Recursive", "struct").expect("Recursive struct exists");
        let refs = find_field_type_edges(&ex, &recursive_id);

        // Should only have field_type edges to Option and Box, not to Recursive itself
        let target_names: Vec<String> = refs
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(t) = &e.target {
                    Some(t.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(
            !target_names.contains(&String::from("Recursive")),
            "should not create self-reference field_type edge"
        );
        assert!(target_names.contains(&String::from("Option")));
        assert!(target_names.contains(&String::from("Box")));
    }

    #[test]
    fn cross_file_error_type_not_filtered_with_syntactic_approach() {
        // This test demonstrates the fix for the R1.1 generic parameter suppression bug.
        //
        // PROBLEM: With name-pattern matching, types named "Error" were incorrectly
        // filtered when referenced cross-file because "Error" is in the generic parameter
        // pattern list, even though it's a real type defined in lib.rs.
        //
        // SOLUTION: With syntactic type parameter collection, types are only filtered
        // if they are actual type parameters declared in the current context.

        let src = r#"
// Simulate daemon.rs that references Error from lib.rs
// Error is NOT a type parameter here, so it should NOT be filtered

struct Error; // This would normally be in lib.rs, defined elsewhere

fn handle_error(err: Error) {
    let _ = err;
}

fn returns_error() -> Error {
    Error
}

struct State; // Another pattern name that should NOT be filtered

fn process_state(s: State) -> State {
    s
}
"#;
        let ex = extract(src);

        // Verify that Error and State are NOT filtered (they're real types, not generic parameters)

        let handle_error_id = find_node_id(&ex, "handle_error", "function").unwrap();
        let handle_error_params = find_param_type_edges(&ex, &handle_error_id);
        assert!(
            !handle_error_params.is_empty(),
            "handle_error should have param_type edge to Error"
        );
        assert!(
            has_sym_edge(&ex, "type/param", "handle_error", "Error"),
            "Error type reference should NOT be filtered"
        );

        assert!(
            has_sym_edge(&ex, "type/return", "returns_error", "Error"),
            "Error type reference should NOT be filtered in return type"
        );

        assert!(
            has_sym_edge(&ex, "type/param", "process_state", "State"),
            "State type reference should NOT be filtered"
        );
        assert!(
            has_sym_edge(&ex, "type/return", "process_state", "State"),
            "State type reference should NOT be filtered in return type"
        );
        assert!(
            has_sym_edge(&ex, "type/param", "process_state", "State"),
            "State type reference should NOT be filtered"
        );
        assert!(
            has_sym_edge(&ex, "type/return", "process_state", "State"),
            "State type reference should NOT be filtered in return type"
        );
    }

    #[test]
    fn actual_generic_parameters_are_filtered_with_syntactic_approach() {
        // Verify that actual type parameters ARE still filtered correctly
        let src = r#"
struct Container<T>(T);

impl<T> Container<T> {
    fn get(&self) -> T {
        self.0
    }
    
    fn transform<U>(&self, input: U) -> Container<U> {
        Container(input)
    }
}
"#;
        let ex = extract(src);

        // T is a generic parameter (from impl<T>), so it should be filtered
        assert!(
            !has_sym_edge(&ex, "type/return", "get", "T"),
            "Generic parameter T should be filtered in return_type"
        );

        // U is a generic parameter (from fn transform<U>), so it should be filtered
        assert!(
            !has_sym_edge(&ex, "type/param", "transform", "U"),
            "Generic parameter U should be filtered in param_type"
        );
        assert!(
            !has_sym_edge(&ex, "type/return", "transform", "U"),
            "Generic parameter U should be filtered in return_type"
        );
    }

    #[test]
    fn mixed_real_types_and_generic_parameters() {
        // Test that real types are kept while generic parameters are filtered
        let src = r#"
struct Error;
struct Result<T> {
    value: T,
    error: Error,
}

impl<T> Result<T> {
    fn get_error(&self) -> Error {
        self.error
    }
    
    fn get_value(&self) -> T {
        self.value
    }
}
"#;
        let ex = extract(src);

        // Error should NOT be filtered (it's a real type, not a generic parameter)
        assert!(
            has_sym_edge(&ex, "type/return", "get_error", "Error"),
            "Real type Error should NOT be filtered"
        );

        // T should be filtered (it's a generic parameter from impl<T>)
        assert!(
            !has_sym_edge(&ex, "type/return", "get_value", "T"),
            "Generic parameter T should be filtered"
        );
    }

    #[test]
    fn cross_file_error_type_extraction_real_two_file_test() {
        // Real two-file extraction test: struct Error defined in lib.rs, referenced in daemon.rs
        // This validates cross-file type resolution pipeline and that Error edges are NOT filtered
        // as generic parameters

        let lib_rs = r#"
pub struct Error {
    message: String,
}

impl Error {
    pub fn new(msg: &str) -> Self {
        Error { message: msg.to_string() }
    }
}

pub fn validate_error(e: Error) -> Result<(), Error> {
    Err(e)
}

pub type Result<T> = std::result::Result<T, Error>;
"#;

        let daemon_rs = r#"
use crate::{Error, Result};

pub fn handle_error(err: Error) -> Error {
    err
}

pub fn process_result() -> Result<()> {
    Err(Error::new("failed"))
}

pub fn returns_error() -> Error {
    Error::new("test error")
}

pub fn takes_error(e: Error) {
    let _ = e;
}

pub fn returns_error_indirect() -> Error {
    let e = Error::new("indirect");
    e
}

pub fn validate(e: Error) -> Result<()> {
    crate::validate_error(e)
}
"#;

        // Extract both files using real extraction pipeline
        let lib_artifact = Artifact {
            path: "src/lib.rs".into(),
            kind: ArtifactKind::Code,
            language: Some("rust".into()),
        };

        let daemon_artifact = Artifact {
            path: "src/daemon.rs".into(),
            kind: ArtifactKind::Code,
            language: Some("rust".into()),
        };

        let lib_ex = RustExtractor::new()
            .extract(&lib_artifact, lib_rs.as_bytes())
            .unwrap();

        let daemon_ex = RustExtractor::new()
            .extract(&daemon_artifact, daemon_rs.as_bytes())
            .unwrap();

        // Count Error-related edges in daemon.rs
        // Based on the functions in daemon.rs:
        // handle_error(err: Error) -> Error (2 edges: param + return)
        // process_result() -> Result<()> - Error appears in Error::new call (not a type edge)
        // returns_error() -> Error (1 edge: return)
        // takes_error(e: Error) (1 edge: param)
        // returns_error_indirect() -> Error (1 edge: return)
        // validate(e: Error) -> Result<()> - Error appears as param (1 edge)
        // Total expected: 6 Error type edges (not counting Error::new constructor calls)

        let error_edges: Vec<&Edge> = daemon_ex
            .edges
            .iter()
            .filter(|e| {
                if let EdgeTarget::Symbol(sym) = &e.target {
                    sym.name == "Error"
                } else {
                    false
                }
            })
            .collect();

        // We expect at least 6 Error type edges, but there might be more from other contexts
        assert!(
            error_edges.len() >= 6,
            "Expected at least 6 Error type edges in daemon.rs, found {}",
            error_edges.len()
        );

        // Verify that Error is NOT filtered in various contexts
        assert!(
            has_sym_edge(&daemon_ex, "type/param", "handle_error", "Error"),
            "Error should NOT be filtered in function parameter"
        );
        assert!(
            has_sym_edge(&daemon_ex, "type/return", "handle_error", "Error"),
            "Error should NOT be filtered in function return type"
        );
        assert!(
            has_sym_edge(&daemon_ex, "type/return", "returns_error", "Error"),
            "Error should NOT be filtered in return type"
        );
        assert!(
            has_sym_edge(&daemon_ex, "type/param", "takes_error", "Error"),
            "Error should NOT be filtered in parameter"
        );
        assert!(
            has_sym_edge(&daemon_ex, "type/return", "returns_error_indirect", "Error"),
            "Error should NOT be filtered in indirect return"
        );
        assert!(
            has_sym_edge(&daemon_ex, "type/param", "validate", "Error"),
            "Error should NOT be filtered in validation parameter"
        );

        // Verify lib.rs also has Error edges (not filtered within definition file)
        let lib_error_edges: Vec<&Edge> = lib_ex
            .edges
            .iter()
            .filter(|e| {
                if let EdgeTarget::Symbol(sym) = &e.target {
                    sym.name == "Error"
                } else {
                    false
                }
            })
            .collect();

        assert!(
            !lib_error_edges.is_empty(),
            "lib.rs should have Error type references in impl block"
        );

        // Verify nodes are created correctly
        assert!(
            lib_ex
                .nodes
                .iter()
                .any(|n| n.label == "Error" && n.kind == "struct"),
            "lib.rs should define Error struct"
        );
        assert!(
            daemon_ex
                .nodes
                .iter()
                .any(|n| n.label == "handle_error" && n.kind == "function"),
            "daemon.rs should define handle_error function"
        );
    }

    #[test]
    fn type_parameter_stack_isolation_between_impl_blocks() {
        // Verify that type_parameter_stack push/pop balance prevents parameter leakage
        // across sibling items. Two adjacent impl blocks with different parameter names
        // should not interfere with each other.

        let src = r#"
// A real type named T (NOT a type parameter)
struct T {
    value: i32,
}

struct StructA;
struct StructB;

// impl block with parameter T
impl<T> StructA {
    fn process_t(&self, t: T) {
        let _ = t;
    }
}

// impl block with NO type parameters - should NOT inherit T from previous impl
impl StructB {
    // Here we use a real type named T, which should NOT be filtered
    // because the type parameter stack was properly popped after impl<T>
    fn field_t(&self) -> T {
        T { value: 42 }
    }
}
"#;

        let ex = extract(src);

        // Test validates stack isolation: The type_parameter_stack prevents parameter leakage
        // - impl<T> pushes T onto stack, sets Driver.type_parameters = {T}
        // - function process_t parameter T should be filtered (not emitted as edge)
        // - impl<T> completes, pops T from stack
        // - impl StructB has NO type parameters, so Driver.type_parameters = {}
        // - function field_t returns real type T, which should NOT be filtered
        //   because T is not in the current type_parameters set (stack was properly popped)

        // Verify T parameter is filtered in impl<T> StructA
        assert!(
            !has_sym_edge(&ex, "type/param", "process_t", "T"),
            "Type parameter T should be filtered in impl<T> StructA"
        );

        // Verify real type T is NOT filtered in impl StructB (proves stack isolation worked)
        // This is the discriminating case: if stack leaked, T would be filtered incorrectly
        assert!(
            has_sym_edge(&ex, "type/return", "field_t", "T"),
            "Real type T should NOT be filtered in impl StructB - proves stack was properly popped"
        );

        // Verify that nodes are created correctly
        assert!(
            ex.nodes
                .iter()
                .any(|n| n.label == "T" && n.kind == "struct"),
            "Real type T should be defined as a struct"
        );

        let struct_a = ex.nodes.iter().find(|n| n.label == "StructA");
        assert!(struct_a.is_some(), "StructA should be defined");

        let struct_b = ex.nodes.iter().find(|n| n.label == "StructB");
        assert!(struct_b.is_some(), "StructB should be defined");

        // Verify impl attributes are correct
        let process_t = ex.nodes.iter().find(|n| n.label == "process_t");
        assert!(
            process_t.is_some(),
            "process_t function should be defined in impl<T> StructA"
        );
        assert_eq!(
            process_t.unwrap().attrs.get("impl").map(String::as_str),
            Some("StructA"),
            "process_t should have impl attr set to StructA"
        );

        let field_t = ex.nodes.iter().find(|n| n.label == "field_t");
        assert!(
            field_t.is_some(),
            "field_t function should be defined in impl StructB"
        );
        assert_eq!(
            field_t.unwrap().attrs.get("impl").map(String::as_str),
            Some("StructB"),
            "field_t should have impl attr set to StructB"
        );

        // This proves stack isolation:
        // 1. impl<T> pushed T onto stack, then popped it
        // 2. impl StructB has NO type parameters (T is gone from stack)
        // 3. field_t returns real type T, which is NOT filtered because T is not in current type parameter set
    }

    /// **A trait's type parameters are in scope inside its default method bodies.**
    ///
    /// The sibling of `type_parameter_stack_isolation_between_impl_blocks`: that
    /// one proves the scope does not outlive the item, this one proves it *reaches*
    /// the item's members. The `struct_item | … | trait_item` arm used to pop
    /// before the walk descended, so `trait Store<T>`'s `T` was already gone by the
    /// time `fn put(&self, item: T)` was read — and because a `type/param -> T`
    /// then resolves like any other name, the edge did not merely leak, it bound to
    /// whatever real type was called `T`. The fixture declares exactly that type,
    /// so a regression produces a confidently wrong edge rather than an unresolved
    /// one.
    #[test]
    fn a_trait_type_parameter_is_in_scope_in_its_default_method_body() {
        let src = r#"
// A real type named T, so a mis-scoped `T` binds rather than dangling.
pub struct T {
    value: i32,
}

pub struct Widget;

pub trait Store<T> {
    fn put(&self, item: T, w: Widget) {
        let _ = (item, w);
    }
}

// The scope must not outlive the trait either.
pub fn after(t: T) -> T {
    t
}
"#;
        let ex = extract(src);

        assert!(
            !has_sym_edge(&ex, "type/param", "put", "T"),
            "the trait's own `<T>` is in scope in its default method body: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/param", "put", "Widget"),
            "a real parameter type beside it is still emitted: {:?}",
            ex.edges
        );
        // The discriminating half: pushing the scope must not swallow the real `T`
        // in a sibling declaration.
        assert!(
            has_sym_edge(&ex, "type/param", "after", "T")
                && has_sym_edge(&ex, "type/return", "after", "T"),
            "the trait scope leaked past the trait: {:?}",
            ex.edges
        );
    }

    // ---- Task 7: Enum Variant Payload Type References Tests --------------------

    #[test]
    fn tuple_variant_creates_field_type_edges() {
        // Tuple variant: ErrorKind::Io(std::io::Error) should create field_type edge
        let src = r#"
struct Error;
enum ErrorKind {
    Io(std::io::Error),
    Network(String),
}
"#;
        let ex = extract(src);

        // Find the Io variant node
        let io_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Io" && n.kind == "enum_variant");
        assert!(io_variant.is_some(), "Io variant should exist");

        let io_id = io_variant.unwrap().id.clone();

        // Io variant should have a field_type edge to std::io::Error
        let io_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == io_id)
            .collect();

        assert!(
            !io_edges.is_empty(),
            "Io variant should have field_type edges"
        );

        // Check the target is std::io::Error (std library paths stay qualified per ADR-0023)
        let has_error_edge = io_edges.iter().any(|e| {
            if let EdgeTarget::Symbol(sym) = &e.target {
                sym.name == "std::io::Error"
            } else {
                false
            }
        });
        assert!(
            has_error_edge,
            "Io variant should have field_type edge to std::io::Error"
        );
    }

    #[test]
    fn struct_variant_creates_field_type_edges() {
        // Struct variant: Message::Text { content: String } should create field_type edge
        let src = r#"
struct Message;
enum Message {
    Text { content: String },
    Data { payload: Vec<u8>, size: usize },
}
"#;
        let ex = extract(src);

        // Find the Text variant
        let text_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Text" && n.kind == "enum_variant");
        assert!(text_variant.is_some(), "Text variant should exist");
        let text_id = text_variant.unwrap().id.clone();

        // Text variant should have a field_type edge with name "content"
        let text_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == text_id)
            .collect();

        // String is filtered as noise, so no edges should be emitted
        assert!(
            text_edges.is_empty(),
            "Text variant should have no field_type edges (String filtered)"
        );

        // Data variant should have no edges (Vec and usize are filtered as noise)
        let data_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Data" && n.kind == "enum_variant");
        assert!(data_variant.is_some(), "Data variant should exist");
        let data_id = data_variant.unwrap().id.clone();

        let data_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == data_id)
            .collect();

        assert!(
            data_edges.is_empty(),
            "Data variant should have no field_type edges (Vec/usize filtered)"
        );
    }

    #[test]
    fn attribute_nodes_not_treated_as_types() {
        // Attribute nodes like #[from] should NOT be treated as types
        let src = r#"
use std::io;
struct IOError;
struct CustomError;
enum MyError {
    #[from]
    IOError(io::Error),
    #[error("custom error")]
    Custom(CustomError),
}
"#;
        let ex = extract(src);

        // Find the IOError variant
        let ioerror_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "IOError" && n.kind == "enum_variant");
        assert!(ioerror_variant.is_some(), "IOError variant should exist");
        let ioerror_id = ioerror_variant.unwrap().id.clone();

        // IOError variant should have a field_type edge to Error (from io::Error)
        let ioerror_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == ioerror_id)
            .collect();

        // io::Error normalizes to Error, and Error is NOT in the denylist, so it should be emitted
        assert!(
            !ioerror_edges.is_empty(),
            "IOError variant should have field_type edge to Error"
        );

        // Check that there's an edge to io::Error (kept qualified after fix)
        let has_error_edge = ioerror_edges.iter().any(|e| {
            if let EdgeTarget::Symbol(sym) = &e.target {
                sym.name == "io::Error"
            } else {
                false
            }
        });
        assert!(
            has_error_edge,
            "IOError variant should have field_type edge to io::Error"
        );

        // The key test: ensure [#from] attribute is NOT treated as a type name
        // The field name should be numeric (tuple field index), NOT "from"
        let has_from_as_field = ioerror_edges.iter().any(|e| {
            if let EdgeTarget::Symbol(sym) = &e.target {
                sym.hints.get("name").map(String::as_str) == Some("from")
            } else {
                false
            }
        });
        assert!(
            !has_from_as_field,
            "#[from] should NOT appear as field name"
        );
    }

    #[test]
    fn generic_enum_variant_payloads() {
        // Generic enum with type parameters: Result<T, E>
        let src = r#"
struct Config;
struct DatabaseError;
enum Result<T, E> {
    Ok(T),
    Err(E),
}
"#;
        let ex = extract(src);

        // Find variants
        let ok_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Ok" && n.kind == "enum_variant");
        let err_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Err" && n.kind == "enum_variant");

        assert!(ok_variant.is_some(), "Ok variant should exist");
        assert!(err_variant.is_some(), "Err variant should exist");

        let ok_id = ok_variant.unwrap().id.clone();
        let err_id = err_variant.unwrap().id.clone();

        // Both Ok and Err variants should have NO edges because T and E are type parameters (filtered)
        let ok_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == ok_id)
            .collect();

        let err_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == err_id)
            .collect();

        assert!(
            ok_edges.is_empty(),
            "Ok variant should have no field_type edges (T is type parameter)"
        );
        assert!(
            err_edges.is_empty(),
            "Err variant should have no field_type edges (E is type parameter)"
        );
    }

    #[test]
    fn mixed_enum_variant_payloads() {
        // Mixed enum with various payload types (some filtered, some not)
        let src = r#"
struct Widget;
struct ServiceError;
enum Complex {
    Empty,
    Single(Config),
    MultiField { name: String, service: ServiceError },
    Nested(Result<Widget, ServiceError>),
}
struct Config {
    enabled: bool,
}
"#;
        let ex = extract(src);

        // Empty variant should have no payload edges
        let empty_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Empty" && n.kind == "enum_variant");
        assert!(empty_variant.is_some(), "Empty variant should exist");
        let empty_id = empty_variant.unwrap().id.clone();

        let empty_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == empty_id)
            .collect();
        assert!(
            empty_edges.is_empty(),
            "Empty variant should have no field_type edges"
        );

        // Single variant should have edge to Config (not filtered - it's a local type)
        let single_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Single" && n.kind == "enum_variant");
        assert!(single_variant.is_some(), "Single variant should exist");
        let single_id = single_variant.unwrap().id.clone();

        let single_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == single_id)
            .collect();

        // Config is a local type, so it should NOT be filtered
        assert!(
            !single_edges.is_empty(),
            "Single variant should have field_type edge to Config"
        );

        let has_config_edge = single_edges.iter().any(|e| {
            if let EdgeTarget::Symbol(sym) = &e.target {
                sym.name == "Config"
            } else {
                false
            }
        });
        assert!(
            has_config_edge,
            "Single variant should have field_type edge to Config"
        );

        // MultiField variant should have edge to ServiceError (local type), but NOT String (filtered)
        let multi_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "MultiField" && n.kind == "enum_variant");
        assert!(multi_variant.is_some(), "MultiField variant should exist");
        let multi_id = multi_variant.unwrap().id.clone();

        let multi_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == multi_id)
            .collect();

        assert!(
            !multi_edges.is_empty(),
            "MultiField variant should have field_type edge to ServiceError"
        );

        let service_edges: Vec<_> = multi_edges
            .iter()
            .filter(|e| {
                if let EdgeTarget::Symbol(sym) = &e.target {
                    sym.name == "ServiceError"
                } else {
                    false
                }
            })
            .collect();
        assert!(
            !service_edges.is_empty(),
            "MultiField variant should have field_type edge to ServiceError"
        );

        // String should be filtered
        let string_edges: Vec<_> = multi_edges
            .iter()
            .filter(|e| {
                if let EdgeTarget::Symbol(sym) = &e.target {
                    sym.name == "String"
                } else {
                    false
                }
            })
            .collect();
        assert!(string_edges.is_empty(), "String should be filtered");

        // Nested variant should have edge to Widget and ServiceError (both local types)
        let nested_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Nested" && n.kind == "enum_variant");
        assert!(nested_variant.is_some(), "Nested variant should exist");
        let nested_id = nested_variant.unwrap().id.clone();

        let nested_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == nested_id)
            .collect();

        assert!(
            !nested_edges.is_empty(),
            "Nested variant should have field_type edges"
        );

        let has_widget = nested_edges.iter().any(|e| {
            if let EdgeTarget::Symbol(sym) = &e.target {
                sym.name == "Widget"
            } else {
                false
            }
        });
        assert!(
            has_widget,
            "Nested variant should have field_type edge to Widget"
        );

        let has_service_error = nested_edges.iter().any(|e| {
            if let EdgeTarget::Symbol(sym) = &e.target {
                sym.name == "ServiceError"
            } else {
                false
            }
        });
        assert!(
            has_service_error,
            "Nested variant should have field_type edge to ServiceError"
        );
    }

    #[test]
    fn thiserror_pattern_works_correctly() {
        // Test the real thiserror pattern used in production
        let src = r#"
use std::io;
struct IOError;
struct ValidationError;
#[derive(Debug)]
enum AppError {
    #[error("I/O error: {0}")]
    Io(io::Error),
    #[error("Validation failed: {0}")]
    Validation(ValidationError),
}
"#;
        let ex = extract(src);

        // Io variant should have edge to io::Error (kept qualified after fix)
        let io_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Io" && n.kind == "enum_variant");
        assert!(io_variant.is_some(), "Io variant should exist");
        let io_id = io_variant.unwrap().id.clone();

        let io_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == io_id)
            .collect();

        assert!(
            !io_edges.is_empty(),
            "Io variant should have field_type edge to io::Error"
        );

        // Verify it's an edge to io::Error
        let has_error_edge = io_edges.iter().any(|e| {
            if let EdgeTarget::Symbol(sym) = &e.target {
                sym.name == "io::Error"
            } else {
                false
            }
        });
        assert!(
            has_error_edge,
            "Io variant should have field_type edge to io::Error"
        );

        // Validation variant should have edge to ValidationError (local type)
        let validation_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Validation" && n.kind == "enum_variant");
        assert!(
            validation_variant.is_some(),
            "Validation variant should exist"
        );
        let validation_id = validation_variant.unwrap().id.clone();

        let validation_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == validation_id)
            .collect();

        assert!(
            !validation_edges.is_empty(),
            "Validation variant should have field_type edge to ValidationError"
        );

        let has_validation_error = validation_edges.iter().any(|e| {
            if let EdgeTarget::Symbol(sym) = &e.target {
                sym.name == "ValidationError"
            } else {
                false
            }
        });
        assert!(
            has_validation_error,
            "Validation variant should have field_type edge to ValidationError"
        );

        // Verify #[error] attributes are NOT treated as types
        let error_attr_as_field = validation_edges.iter().any(|e| {
            if let EdgeTarget::Symbol(sym) = &e.target {
                sym.name == "error"
            } else {
                false
            }
        });
        assert!(
            !error_attr_as_field,
            "#[error] should NOT appear as type name"
        );
    }

    #[test]
    fn enum_variant_sources_dont_affect_struct_sources() {
        // Verify that adding enum variant field_type edges doesn't affect struct field_type edge sources
        let src = r#"
struct MyStruct {
    field1: Config,
    field2: Widget,
}
struct Config;
struct Widget;
enum MyEnum {
    Variant1(Config),
    Variant2(Widget),
}
"#;
        let ex = extract(src);

        // struct should have field_type edges
        let struct_node = ex
            .nodes
            .iter()
            .find(|n| n.label == "MyStruct" && n.kind == "struct");
        assert!(struct_node.is_some(), "MyStruct should exist");
        let struct_id = struct_node.unwrap().id.clone();

        let struct_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == struct_id)
            .collect();

        assert!(
            !struct_edges.is_empty(),
            "MyStruct should have field_type edges"
        );

        // enum variant should also have field_type edges
        let variant1 = ex
            .nodes
            .iter()
            .find(|n| n.label == "Variant1" && n.kind == "enum_variant");
        assert!(variant1.is_some(), "Variant1 should exist");
        let variant1_id = variant1.unwrap().id.clone();

        let variant1_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == variant1_id)
            .collect();

        assert!(
            !variant1_edges.is_empty(),
            "Variant1 should have field_type edges"
        );

        // Variant2 should also have field_type edges
        let variant2 = ex
            .nodes
            .iter()
            .find(|n| n.label == "Variant2" && n.kind == "enum_variant");
        assert!(variant2.is_some(), "Variant2 should exist");
        let variant2_id = variant2.unwrap().id.clone();

        let variant2_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == variant2_id)
            .collect();

        assert!(
            !variant2_edges.is_empty(),
            "Variant2 should have field_type edges"
        );

        // Check that struct has both Config and Widget fields
        let struct_targets: Vec<_> = struct_edges
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(sym) = &e.target {
                    Some(sym.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(
            struct_targets.contains(&String::from("Config")),
            "struct should have Config field"
        );
        assert!(
            struct_targets.contains(&String::from("Widget")),
            "struct should have Widget field"
        );

        // Check that Variant1 has Config field
        let variant1_targets: Vec<_> = variant1_edges
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(sym) = &e.target {
                    Some(sym.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(
            variant1_targets.contains(&String::from("Config")),
            "Variant1 should have Config field"
        );

        // Check that Variant2 has Widget field
        let variant2_targets: Vec<_> = variant2_edges
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(sym) = &e.target {
                    Some(sym.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(
            variant2_targets.contains(&String::from("Widget")),
            "Variant2 should have Widget field"
        );
    }

    #[test]
    fn no_self_reference_edges_from_variants() {
        // Variants should not create field_type edges to themselves
        let src = r#"
struct Widget;
enum Widget {
    Variant1(u32),
    Variant2(String),
}
"#;
        let ex = extract(src);

        // Find the enum Widget (not the struct Widget)
        let enum_widget = ex
            .nodes
            .iter()
            .find(|n| n.label == "Widget" && n.kind == "enum");
        assert!(enum_widget.is_some(), "Widget enum should exist");

        let variants: Vec<_> = ex
            .nodes
            .iter()
            .filter(|n| n.kind == "enum_variant")
            .collect();

        assert!(!variants.is_empty(), "enum should have variants");

        // Check that no variant has a field_type edge to Widget
        for variant in &variants {
            let variant_edges: Vec<&Edge> = ex
                .edges
                .iter()
                .filter(|e| e.relation == "type/field" && e.source == variant.id)
                .collect();

            let has_widget_edge = variant_edges.iter().any(|e| {
                if let EdgeTarget::Symbol(sym) = &e.target {
                    sym.name == "Widget"
                } else {
                    false
                }
            });
            assert!(
                !has_widget_edge,
                "Variant should not have field_type edge to Widget (self-reference prevention)"
            );
        }
    }

    #[test]
    fn enum_variant_edge_validation_comprehensive() {
        // Comprehensive test for all aspects of enum variant field_type edges
        let src = r#"
struct Config;
struct DatabaseError;
struct NetworkError;
use std::path::PathBuf;

enum AppState {
    Idle,
 Loading(Config),
    Loaded { data: Vec<Config>, path: PathBuf },
    Error(DatabaseError),
    NetworkFailure(NetworkError),
}
"#;
        let ex = extract(src);

        // All variants should exist
        let idle = ex
            .nodes
            .iter()
            .find(|n| n.label == "Idle" && n.kind == "enum_variant");
        let loading = ex
            .nodes
            .iter()
            .find(|n| n.label == "Loading" && n.kind == "enum_variant");
        let loaded = ex
            .nodes
            .iter()
            .find(|n| n.label == "Loaded" && n.kind == "enum_variant");
        let error = ex
            .nodes
            .iter()
            .find(|n| n.label == "Error" && n.kind == "enum_variant");
        let network_failure = ex
            .nodes
            .iter()
            .find(|n| n.label == "NetworkFailure" && n.kind == "enum_variant");

        assert!(idle.is_some(), "Idle variant should exist");
        assert!(loading.is_some(), "Loading variant should exist");
        assert!(loaded.is_some(), "Loaded variant should exist");
        assert!(error.is_some(), "Error variant should exist");
        assert!(
            network_failure.is_some(),
            "NetworkFailure variant should exist"
        );

        // Idle should have no edges
        let idle_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == idle.unwrap().id)
            .collect();
        assert!(
            idle_edges.is_empty(),
            "Idle variant should have no field_type edges"
        );

        // Loading should have edge to Config (local type)
        let loading_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == loading.unwrap().id)
            .collect();

        assert!(
            !loading_edges.is_empty(),
            "Loading variant should have field_type edges"
        );
        let loading_targets: Vec<_> = loading_edges
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(sym) = &e.target {
                    Some(sym.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(
            loading_targets.contains(&String::from("Config")),
            "Loading should have Config field"
        );

        // Loaded should have edge to Config, but NOT to Vec or PathBuf (both filtered)
        let loaded_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == loaded.unwrap().id)
            .collect();

        let loaded_targets: Vec<_> = loaded_edges
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(sym) = &e.target {
                    Some(sym.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(
            loaded_targets.contains(&String::from("Config")),
            "Loaded should have Config field"
        );
        assert!(
            !loaded_targets.contains(&String::from("Vec")),
            "Vec should be filtered"
        );
        assert!(
            !loaded_targets.contains(&String::from("PathBuf")),
            "PathBuf should be filtered"
        );

        // Error should have edge to DatabaseError (local type)
        let error_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == error.unwrap().id)
            .collect();

        let error_targets: Vec<_> = error_edges
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(sym) = &e.target {
                    Some(sym.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(
            error_targets.contains(&String::from("DatabaseError")),
            "Error should have DatabaseError field"
        );

        // NetworkFailure should have edge to NetworkError (local type)
        let network_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == network_failure.unwrap().id)
            .collect();

        let network_targets: Vec<_> = network_edges
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(sym) = &e.target {
                    Some(sym.name.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(
            network_targets.contains(&String::from("NetworkError")),
            "NetworkFailure should have NetworkError field"
        );
    }

    #[test]
    fn tuple_variant_uses_positional_indices() {
        // Validate that tuple variants use positional indices "0", "1", etc. for field names
        // instead of placeholder "_", which fixes the member identity issue for R1.4
        let src = r#"
struct Command;
struct Error;
enum ControlOp {
    Command(Command),
    Error(Error),
}
"#;
        let ex = extract(src);

        // Find the Command variant
        let command_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Command" && n.kind == "enum_variant");
        assert!(command_variant.is_some(), "Command variant should exist");
        let command_id = command_variant.unwrap().id.clone();

        // Command variant should have field_type edge with field name "0"
        let command_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == command_id)
            .collect();

        assert_eq!(
            command_edges.len(),
            1,
            "Command variant should have exactly 1 field_type edge"
        );

        let command_edge = command_edges.first().unwrap();
        if let EdgeTarget::Symbol(sym) = &command_edge.target {
            assert_eq!(
                sym.hints.get("name").map(String::as_str),
                Some("0"),
                "Field name should be positional index '0', not '_'"
            );
            assert_eq!(sym.name, "Command", "Target should be Command type");
        } else {
            panic!("Edge target should be Symbol");
        }

        // Find the Error variant (should also use "0")
        let error_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Error" && n.kind == "enum_variant");
        assert!(error_variant.is_some(), "Error variant should exist");
        let error_id = error_variant.unwrap().id.clone();

        let error_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == error_id)
            .collect();

        assert_eq!(
            error_edges.len(),
            1,
            "Error variant should have exactly 1 field_type edge"
        );

        let error_edge = error_edges.first().unwrap();
        if let EdgeTarget::Symbol(sym) = &error_edge.target {
            assert_eq!(
                sym.hints.get("name").map(String::as_str),
                Some("0"),
                "Field name should be positional index '0', not '_'"
            );
            assert_eq!(sym.name, "Error", "Target should be Error type");
        } else {
            panic!("Edge target should be Symbol");
        }
    }

    #[test]
    fn newtype_wrapper_edges_emitted_correctly() {
        // Validate that newtype wrapper patterns like Command(Command), Exec(Exec)
        // emit edges correctly - the self-reference guard was incorrectly filtering these
        let src = r#"
struct Command;
struct Exec;
enum ControlOp {
    Command(Command),
    Exec(Exec),
}
"#;
        let ex = extract(src);

        // Find the Command variant - should have edge to Command (newtype wrapper)
        let command_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Command" && n.kind == "enum_variant");
        assert!(command_variant.is_some(), "Command variant should exist");
        let command_id = command_variant.unwrap().id.clone();

        let command_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == command_id)
            .collect();

        assert!(
            !command_edges.is_empty(),
            "Command(Command) newtype wrapper should emit field_type edge (was incorrectly filtered by self-reference guard)"
        );

        let command_edge = command_edges.first().unwrap();
        if let EdgeTarget::Symbol(sym) = &command_edge.target {
            assert_eq!(
                sym.name, "Command",
                "Command variant should edge to Command type (newtype wrapper pattern)"
            );
        } else {
            panic!("Edge target should be Symbol");
        }

        // Find the Exec variant - should also have edge to Exec
        let exec_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Exec" && n.kind == "enum_variant");
        assert!(exec_variant.is_some(), "Exec variant should exist");
        let exec_id = exec_variant.unwrap().id.clone();

        let exec_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == exec_id)
            .collect();

        assert!(
            !exec_edges.is_empty(),
            "Exec(Exec) newtype wrapper should emit field_type edge (was incorrectly filtered by self-reference guard)"
        );

        let exec_edge = exec_edges.first().unwrap();
        if let EdgeTarget::Symbol(sym) = &exec_edge.target {
            assert_eq!(
                sym.name, "Exec",
                "Exec variant should edge to Exec type (newtype wrapper pattern)"
            );
        } else {
            panic!("Edge target should be Symbol");
        }
    }

    #[test]
    fn tuple_variant_with_multiple_fields_uses_consistent_indices() {
        // Validate that tuple variants with multiple fields use consistent positional indices
        // even when attributes like #[from] are present
        let src = r#"
struct Request;
struct Response;
pub enum Message {
    #[from]
    Request(Request),
    Response(Response),
}
"#;
        let ex = extract(src);

        // Find the Request variant (has #[from] attribute - should still get correct index)
        let request_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Request" && n.kind == "enum_variant");
        assert!(request_variant.is_some(), "Request variant should exist");
        let request_id = request_variant.unwrap().id.clone();

        let request_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == request_id)
            .collect();

        assert_eq!(
            request_edges.len(),
            1,
            "Request variant should have exactly 1 field_type edge"
        );

        let request_edge = request_edges.first().unwrap();
        if let EdgeTarget::Symbol(sym) = &request_edge.target {
            assert_eq!(
                sym.hints.get("name").map(String::as_str),
                Some("0"),
                "Request variant field should be index '0' (attribute doesn't affect indexing)"
            );
            assert_eq!(sym.name, "Request", "Target should be Request type");
        } else {
            panic!("Edge target should be Symbol");
        }

        // Find the Response variant - should also use "0"
        let response_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Response" && n.kind == "enum_variant");
        assert!(response_variant.is_some(), "Response variant should exist");
        let response_id = response_variant.unwrap().id.clone();

        let response_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == response_id)
            .collect();

        assert_eq!(
            response_edges.len(),
            1,
            "Response variant should have exactly 1 field_type edge"
        );

        let response_edge = response_edges.first().unwrap();
        if let EdgeTarget::Symbol(sym) = &response_edge.target {
            assert_eq!(
                sym.hints.get("name").map(String::as_str),
                Some("0"),
                "Response variant field should be index '0' (attribute doesn't affect indexing)"
            );
            assert_eq!(sym.name, "Response", "Target should be Response type");
        } else {
            panic!("Edge target should be Symbol");
        }
    }

    // ---- Task 5: bound_type relation tests (ADR-0036 R5.1) ----

    #[test]
    fn bound_type_basic_function_bounds() {
        let src = r#"
use std::fmt::Debug;

fn process<T: Debug>(value: T) {
    println!("{:?}", value);
}
"#;
        let ex = extract(src);
        assert!(has_sym_edge(&ex, "type/bound", "process", "Debug"));
    }

    #[test]
    fn bound_type_where_clause_bounds() {
        let src = r#"
use std::marker::Send;

fn execute<T>(value: T) where T: Send {
    let _ = value;
}
"#;
        let ex = extract(src);
        assert!(has_sym_edge(&ex, "type/bound", "execute", "Send"));
    }

    #[test]
    fn bound_type_sized_exclusion() {
        let src = r#"
fn unsized<T: ?Sized>(value: &T) {
    let _ = value;
}
"#;
        let ex = extract(src);
        assert!(!has_sym_edge(&ex, "type/bound", "unsized", "Sized"));
    }

    #[test]
    fn bound_type_lifetime_bounds() {
        let src = r#"
fn with_lifetime<'a, T: 'a>(value: &'a T) {
    let _ = value;
}
"#;
        let ex = extract(src);
        let with_lifetime_id = find_node_id(&ex, "with_lifetime", "function").unwrap();
        let with_lifetime_node_id = NodeId::new(with_lifetime_id);
        let bound_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound" && e.source == with_lifetime_node_id)
            .collect();
        assert!(
            bound_edges.is_empty(),
            "Lifetime bounds should not produce bound_type edges"
        );
    }

    #[test]
    fn bound_type_function_type_correctness() {
        let src = r#"
use std::ops::FnOnce;

struct SocketClient;

fn execute<F: FnOnce(SocketClient)>(callback: F) {
    callback(SocketClient);
}
"#;
        let ex = extract(src);
        assert!(has_sym_edge(&ex, "type/bound", "execute", "FnOnce"));
        assert!(has_sym_edge(&ex, "type/bound", "execute", "SocketClient"));
    }

    #[test]
    fn bound_type_struct_bounds() {
        let src = r#"
use std::clone::Clone;

struct Container<T: Clone> {
    value: T,
}
"#;
        let ex = extract(src);
        assert!(has_sym_edge(&ex, "type/bound", "Container", "Clone"));
    }

    #[test]
    fn bound_type_enum_bounds() {
        let src = r#"
use std::iter::Iterator;

enum Stream<T: Iterator> {
    Empty,
    Values(T),
}
"#;
        let ex = extract(src);
        assert!(has_sym_edge(&ex, "type/bound", "Stream", "Iterator"));
    }

    #[test]
    fn bound_type_trait_bounds() {
        let src = r#"
use std::fmt::Display;

trait Printable<T: Display> {
    fn print(&self, value: T);
}
"#;
        let ex = extract(src);
        assert!(has_sym_edge(&ex, "type/bound", "Printable", "Display"));
    }

    #[test]
    fn bound_type_impl_blanket_decline() {
        let src = r#"
use std::fmt::Debug;

trait Marker {}

impl<T: Debug> Marker for T {
    fn debug_me(&self) {
        println!("{:?}", self);
    }
}
"#;
        let ex = extract(src);
        let bound_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound")
            .collect();
        assert!(
            bound_edges.is_empty(),
            "Blanket impl should not emit bound_type edges"
        );
    }

    #[test]
    fn bound_type_impl_decline_when_no_local_node() {
        // This test validates R5.1.2 requirement: impls decline bound_type edges
        // when the self type has no local node
        let src = r#"
use std::fmt::Debug;

struct LocalType;

trait Marker {}

// impl<T: Debug> Marker for T - blanket impl, T has no local node, should decline
impl<T: Debug> Marker for T {
    fn blanket_method(&self) {
        println!("{:?}", self);
    }
}
"#;
        let (ex, (declined_count, declined_impls)) = extract_with_stats(src);

        // Verify we have exactly 1 declined impl (the blanket impl)
        assert_eq!(
            declined_count, 1,
            "Should have exactly 1 declined impl (the blanket impl for T)"
        );

        // Verify the declined impl is for T and reason is "no_local_node"
        assert_eq!(declined_impls.len(), 1, "Should have 1 declined impl entry");

        let (declined_type, reason) = &declined_impls[0];
        assert_eq!(
            declined_type, "T",
            "The declined impl should be for blanket parameter T"
        );
        assert_eq!(
            reason, "no_local_node",
            "The decline reason should be 'no_local_node' per R5.1.2"
        );

        // Verify that blanket impl doesn't emit bound_type edges
        let bound_edges: Vec<Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound")
            .cloned()
            .collect();

        assert!(
            bound_edges.is_empty(),
            "Blanket impl should not emit bound_type edges (declined per R5.1.2)"
        );

        // Verify we have a LocalType struct defined
        let local_type_id = find_node_id(&ex, "LocalType", "struct").unwrap();
        assert!(
            !local_type_id.is_empty(),
            "LocalType should be defined as a struct"
        );
    }

    #[test]
    fn bound_type_impl_decline_with_mixed_local_foreign_types() {
        // Test that validates both local and foreign impl scenarios
        let src = r#"
use std::fmt::Debug;

struct LocalType;

// Local impl should NOT decline
impl<T: Debug> LocalType {
    fn local_method<U: Debug>(&self, value: U) {
        println!("{:?}", value);
    }
}
"#;
        let (ex, (declined_count, declined_impls)) = extract_with_stats(src);

        // Verify we have 0 declined impls (all types are local)
        assert_eq!(
            declined_count, 0,
            "Should have 0 declined impls when all self types have local nodes"
        );

        assert!(
            declined_impls.is_empty(),
            "Should have no declined impls when all self types are local"
        );

        // Verify we have bound_type edges from the method's type parameter
        let bound_edges: Vec<Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound")
            .cloned()
            .collect();

        assert!(
            !bound_edges.is_empty(),
            "Should have bound_type edges from impl method type parameters"
        );

        // Count bound_type edges - we expect exactly 2 (from the impl's T: Debug and method's U: Debug)
        // LocalType has a local node, so both should emit edges (unlike foreign types)
        assert_eq!(
            bound_edges.len(), 2,
            "Should have exactly 2 bound_type edges: one from impl T: Debug and one from method U: Debug"
        );

        // Verify the edge source is LocalType (the impl's self type)
        let local_type_id = find_node_id(&ex, "LocalType", "struct").unwrap();
        let local_type_node_id = NodeId::new(local_type_id);

        let local_type_bound_edges: Vec<Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound" && e.source == local_type_node_id)
            .cloned()
            .collect();

        // LocalType should have 1 bound_type edge:
        // From the impl's T: Debug (the method's U: Debug is attributed to the method node, not LocalType)
        assert_eq!(
            local_type_bound_edges.len(),
            1,
            "LocalType should have 1 bound_type edge from the impl's T: Debug"
        );

        // Verify the bound is Debug
        if let EdgeTarget::Symbol(sym) = &local_type_bound_edges[0].target {
            assert_eq!(sym.name, "Debug", "The bound should be Debug");
        } else {
            panic!("Bound edge target should be a Symbol");
        }
    }

    #[test]
    fn bound_type_parameter_ordering() {
        let src = r#"
use std::convert::Into;

fn convert<T: Into<U>, U>(value: T) -> U {
    value.into()
}
"#;
        let ex = extract(src);
        assert!(has_sym_edge(&ex, "type/bound", "convert", "Into"));
        let convert_id = find_node_id(&ex, "convert", "function").unwrap();
        let convert_node_id = NodeId::new(convert_id);
        let bound_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound" && e.source == convert_node_id)
            .collect();
        let bound_targets: Vec<String> = bound_edges
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(sym) = &e.target {
                    Some(sym.name.clone())
                } else {
                    None
                }
            })
            .collect();
        assert!(!bound_targets.contains(&"T".to_string()));
        assert!(!bound_targets.contains(&"U".to_string()));
    }

    #[test]
    fn bound_type_associated_type_bounds() {
        let src = r#"
use std::fmt::Display;

trait Container {
    type Item: Display;
}
"#;
        let ex = extract(src);
        assert!(has_sym_edge(&ex, "type/bound", "Container", "Display"));
    }

    // ============================================================================
    // Coverage Validation Tests for ADR-0036 R5.1 Task 5
    // ============================================================================

    // Test 1: Validate that we can distinguish type_parameter vs where_predicate coverage
    #[test]
    fn bound_type_coverage_site_kind_distribution() {
        let src = r#"
use std::fmt::{Debug, Display};

// This contributes to type_parameter sites
fn process<T: Debug>(value: T) {
    println!("{:?}", value);
}

// This contributes to where_predicate sites  
fn execute<T>(value: T) where T: Display {
    println!("{}", value);
}

// Struct with type parameter bounds
struct Container<T: Debug, U: Display> {
    first: T,
    second: U,
}
"#;
        let ex = extract(src);

        // Verify basic bound_type edges are emitted
        assert!(has_sym_edge(&ex, "type/bound", "process", "Debug"));
        assert!(has_sym_edge(&ex, "type/bound", "execute", "Display"));
        assert!(has_sym_edge(&ex, "type/bound", "Container", "Debug"));
        assert!(has_sym_edge(&ex, "type/bound", "Container", "Display"));

        // Count all bound_type edges
        let bound_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound")
            .collect();

        // Should have bounds from:
        // - process<T: Debug> (1 type_param site)
        // - execute where T: Display (1 where_pred site)
        // - Container<T: Debug, U: Display> (2 type_param sites)
        // Total: 4 type_param sites, 1 where_pred site = 4 bounds
        assert!(!bound_edges.is_empty(), "Should have bound_type edges");
    }

    // Test 2: Validate struct_item bound coverage specifically
    #[test]
    fn bound_type_struct_item_coverage() {
        let src = r#"
use std::fmt::Debug;
use std::clone::Clone;

struct WithBounds<T: Debug + Clone> {
    value: T,
}
"#;
        let ex = extract(src);

        // Verify we get both bounds from the struct
        assert!(has_sym_edge(&ex, "type/bound", "WithBounds", "Debug"));
        assert!(has_sym_edge(&ex, "type/bound", "WithBounds", "Clone"));

        // Count edges coming from struct_item specifically
        let with_bounds_id = find_node_id(&ex, "WithBounds", "struct").unwrap();
        let with_bounds_node_id = NodeId::new(with_bounds_id);

        let struct_bound_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound" && e.source == with_bounds_node_id)
            .collect();

        assert_eq!(
            struct_bound_edges.len(),
            2,
            "Struct item should emit 2 bound_type edges"
        );
    }

    // Test 4: Complex coverage validation with mixed sites
    #[test]
    fn bound_type_mixed_site_coverage_validation() {
        let src = r#"
use std::fmt::{Debug, Display};
use std::clone::Clone;

// Function with type_param bounds (type_param site)
fn func1<T: Debug + Clone>(value: T) -> T {
    value
}

// Function with where_clause bounds (where_pred site)
fn func2<T>(value: T) -> T 
where T: Debug + Display,
{
    value
}

// Function with both (both site types)
fn func3<T: Clone>(value: T) -> T 
where T: Debug,
{
    value
}

// Struct with bounds (type_param site)
struct Struct1<T: Debug, U: Clone> {
    t: T,
    u: U,
}

// Enum with bounds (type_param site)
enum Enum1<T: Debug> {
    Variant(T),
}
"#;
        let ex = extract(src);

        // Verify key bounds are emitted
        assert!(has_sym_edge(&ex, "type/bound", "func1", "Debug"));
        assert!(has_sym_edge(&ex, "type/bound", "func1", "Clone"));
        assert!(has_sym_edge(&ex, "type/bound", "func2", "Debug"));
        assert!(has_sym_edge(&ex, "type/bound", "func2", "Display"));
        assert!(has_sym_edge(&ex, "type/bound", "func3", "Clone"));
        assert!(has_sym_edge(&ex, "type/bound", "func3", "Debug"));
        assert!(has_sym_edge(&ex, "type/bound", "Struct1", "Debug"));
        assert!(has_sym_edge(&ex, "type/bound", "Struct1", "Clone"));
        assert!(has_sym_edge(&ex, "type/bound", "Enum1", "Debug"));

        // Count total bound_type edges for coverage validation
        let all_bound_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound")
            .collect();

        // Should have bounds from:
        // func1: Debug, Clone (2 bounds from type_param)
        // func2: Debug, Display (2 bounds from where_pred)
        // func3: Clone (from type_param), Debug (from where_pred) = 2 bounds
        // Struct1: Debug, Clone (2 bounds from type_param)
        // Enum1: Debug (1 bound from type_param)
        // Total: 9 bounds
        assert!(
            !all_bound_edges.is_empty(),
            "Should have multiple bound_type edges"
        );
    }

    #[test]
    fn tuple_variant_with_multiple_types_emits_correct_indices() {
        // Validate multi-field tuple variants emit edges with correct positional indices
        let src = r#"
struct TypeA;
struct TypeB;
struct TypeC;
enum Multi {
    Tuple(TypeA, TypeB, TypeC),
}
"#;
        let ex = extract(src);

        // Find the Tuple variant
        let tuple_variant = ex
            .nodes
            .iter()
            .find(|n| n.label == "Tuple" && n.kind == "enum_variant");
        assert!(tuple_variant.is_some(), "Tuple variant should exist");
        let tuple_id = tuple_variant.unwrap().id.clone();

        let tuple_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/field" && e.source == tuple_id)
            .collect();

        assert_eq!(
            tuple_edges.len(),
            3,
            "Tuple variant should have 3 field_type edges (one for each type)"
        );

        // Collect and sort by field name to validate indices
        let mut edges_by_index: Vec<(String, String)> = tuple_edges
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Symbol(sym) = &e.target {
                    sym.hints
                        .get("name")
                        .map(|name| (name.clone(), sym.name.clone()))
                } else {
                    None
                }
            })
            .collect();

        edges_by_index.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(
            edges_by_index,
            vec![
                ("0".to_string(), "TypeA".to_string()),
                ("1".to_string(), "TypeB".to_string()),
                ("2".to_string(), "TypeC".to_string()),
            ],
            "Multi-field tuple variant should use positional indices 0, 1, 2 (not '_' placeholders)"
        );
    }

    #[test]
    fn trait_projection_captures_trait_reference() {
        // <T as Iterator>::Item should capture both Iterator and Item
        // Previously only captured Item, losing the Iterator trait reference
        let src = r#"
use std::iter::Iterator;

fn process<U>(input: <U as Iterator>::Item) -> <U as Iterator>::Item {
    input
}
"#;
        let ex = extract(src);

        // Find the process function node
        let process_fn = ex
            .nodes
            .iter()
            .find(|n| n.label == "process" && n.kind == "function");
        assert!(process_fn.is_some(), "process function should exist");
        let process_id = process_fn.unwrap().id.clone();

        // Check that std::iter::Iterator is captured in the bound_type/param_type edges (kept qualified after fix)
        // The function should have references to both std::iter::Iterator and Item
        let iterator_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| {
                matches!(&e.target, EdgeTarget::Symbol(s) if s.name == "std::iter::Iterator" || s.name == "Iterator")
                    && e.source == process_id
            })
            .collect();

        let item_edges: Vec<&Edge> = ex
            .edges
            .iter()
            .filter(|e| {
                matches!(&e.target, EdgeTarget::Symbol(s) if s.name == "Item")
                    && e.source == process_id
            })
            .collect();

        // At minimum, Item should be captured (final segment)
        assert!(
            !item_edges.is_empty(),
            "Item (associated type) should be captured"
        );

        // Iterator trait should also be captured (the fix addresses this)
        assert!(
            !iterator_edges.is_empty(),
            "Iterator (trait) should be captured in trait projections"
        );
    }

    #[test]
    fn trait_projection_complex_type_expression() {
        // Test multi-layer trait projections and complex type expressions
        let src = r#"
use std::iter::Iterator;
use std::fmt::Display;

trait MyTrait {
    type Output: Display;
}

fn complex<T: MyTrait>(value: <T as MyTrait>::Output) -> Result<<T as MyTrait>::Output, String> {
    Ok(value)
}

struct Container<T> {
    inner: <T as Iterator>::Item,
}
"#;
        let ex = extract(src);

        // Find all trait references in the graph (std::iter::Iterator should be qualified after fix)
        let trait_refs: Vec<_> = ex
            .edges
            .iter()
            .filter(|e| {
                matches!(&e.target, EdgeTarget::Symbol(s) if
                    s.name == "MyTrait" || s.name == "Iterator" || s.name == "std::iter::Iterator")
            })
            .collect();

        // Find all associated type references
        let assoc_type_refs: Vec<_> = ex.edges.iter()
            .filter(|e| {
                matches!(&e.target, EdgeTarget::Symbol(s) if s.name == "Output" || s.name == "Item")
            })
            .collect();

        // Verify that both trait and associated type references are captured
        assert!(
            !trait_refs.is_empty(),
            "Trait references should be captured"
        );
        assert!(
            !assoc_type_refs.is_empty(),
            "Associated type references should be captured"
        );

        // Verify MyTrait is referenced at least in the function and struct bounds
        let mytrait_refs: Vec<_> = ex
            .edges
            .iter()
            .filter(|e| matches!(&e.target, EdgeTarget::Symbol(s) if s.name == "MyTrait"))
            .collect();

        assert!(
            !mytrait_refs.is_empty(),
            "MyTrait should be referenced in multiple places"
        );
    }

    /// The trait in `<T as Trait>::Assoc` is a **type**, not a name — so every
    /// type shape has to work there, not just the bare `type_identifier` the
    /// first cut matched.
    ///
    /// `<T as Into<Widget>>::Target` parses the trait as a `generic_type`, so a
    /// single-kind match skipped it entirely and emitted only `Target` — losing
    /// both the trait *and* its type arguments. `Deref`/`Into<X>`/`AsRef<X>`
    /// projections are common enough that "plain trait works, generic trait
    /// silently doesn't" is the worst possible split.
    #[test]
    fn trait_projection_captures_a_generic_trait_and_its_arguments() {
        let src = r#"
fn plain<T>(x: <T as Deref>::Target) {}
fn generic<T>(x: <T as Into<Widget>>::Target) {}
"#;
        let ex = extract(src);

        let refs_of = |fn_label: &str| -> Vec<String> {
            let id = ex
                .nodes
                .iter()
                .find(|n| n.label == fn_label && n.kind == "function")
                .expect("function node")
                .id
                .clone();
            ex.edges
                .iter()
                .filter(|e| e.source == id)
                .filter_map(|e| match &e.target {
                    EdgeTarget::Symbol(s) => Some(s.name.clone()),
                    _ => None,
                })
                .collect()
        };

        // The plain form is the case that already worked — keep it pinned.
        let plain = refs_of("plain");
        println!("plain refs: {:?}", plain);
        let has_deref = plain.iter().any(|s| s.contains("Deref"));
        let has_target = plain.contains(&"Target".to_string());
        assert!(
            has_deref && has_target,
            "plain trait projection keeps both trait and associated type: {plain:?}"
        );

        // The generic form: the trait is a `generic_type`, and its argument
        // rides along because the node goes through `collect_type_refs_recursive`.
        let generic = refs_of("generic");
        let has_into = generic.iter().any(|s| s.contains("Into"));
        let has_widget =
            generic.contains(&"Widget".to_string()) || generic.iter().any(|s| s.contains("Widget"));
        assert!(
            has_into,
            "a generic trait in a projection must still be captured: {generic:?}"
        );
        assert!(
            has_widget,
            "the generic trait's type argument rides along: {generic:?}"
        );
        assert!(
            generic.contains(&"Target".to_string()),
            "the associated type rides along: {generic:?}"
        );
        assert!(
            has_widget,
            "the generic trait's type argument rides along: {generic:?}"
        );
        assert!(
            generic.contains(&"Target".to_string()),
            "the associated type rides along: {generic:?}"
        );
        assert!(
            generic.contains(&"Widget".to_string()),
            "the generic trait's type argument rides along: {generic:?}"
        );
        assert!(
            generic.contains(&"Target".to_string()),
            "the associated type is still the final segment: {generic:?}"
        );
    }

    #[test]
    fn test_qualified_path_handling_local_paths_stripped() {
        let src = r#"
struct Widget;
fn test_local() -> crate::Widget {
    Widget
}
fn test_self_path() -> Self::MyType {
    unimplemented!()
}
impl Widget {
    fn method(&self) -> Self::Associated {
        Self::Associated
    }
}
"#;
        let ex = extract(src);

        let test_local_id =
            find_node_id(&ex, "test_local", "function").expect("test_local function exists");
        let refs = find_references_edges(&ex, &test_local_id);

        // crate::Widget should be stripped to just "Widget"
        assert!(
            !refs.is_empty(),
            "should have return_type edges for return type"
        );

        let widget_edge = refs
            .iter()
            .find(|e| matches!(&e.target, EdgeTarget::Symbol(t) if t.name == "Widget"));
        assert!(
            widget_edge.is_some(),
            "crate::Widget should be stripped to 'Widget': got {:?}",
            refs
        );
    }

    #[test]
    fn test_qualified_path_handling_foreign_paths_kept() {
        // Use std library paths (definitively foreign, not denylisted)
        let src = r#"
use std::ffi::CStr;

fn test_foreign_std() -> std::ffi::CStr {
    std::ffi::CStr::from_bytes_with_nul(b"test\0").unwrap()
}
fn test_mixed_local_foreign() -> (crate::Widget, std::ffi::CString) {
    unimplemented!()
}
"#;
        let ex = extract(src);

        let test_foreign_id = find_node_id(&ex, "test_foreign_std", "function")
            .expect("test_foreign_std function exists");
        let refs = find_references_edges(&ex, &test_foreign_id);

        // std library paths should be kept qualified
        assert!(
            !refs.is_empty(),
            "should have return_type edges for return type"
        );

        let cstr_edge = refs
            .iter()
            .find(|e| matches!(&e.target, EdgeTarget::Symbol(t) if t.name == "std::ffi::CStr"));
        assert!(
            cstr_edge.is_some(),
            "std::ffi::CStr should be kept qualified: got {:?}",
            refs
        );

        let test_mixed_id = find_node_id(&ex, "test_mixed_local_foreign", "function")
            .expect("test_mixed_local_foreign function exists");
        let mixed_refs = find_references_edges(&ex, &test_mixed_id);

        // Local paths should be stripped
        let widget_edge = mixed_refs
            .iter()
            .find(|e| matches!(&e.target, EdgeTarget::Symbol(t) if t.name == "Widget"));
        assert!(
            widget_edge.is_some(),
            "crate::Widget should be stripped to 'Widget': got {:?}",
            mixed_refs
        );

        // std library paths should be kept qualified
        let cstring_edge = mixed_refs
            .iter()
            .find(|e| matches!(&e.target, EdgeTarget::Symbol(t) if t.name == "std::ffi::CString"));
        assert!(
            cstring_edge.is_some(),
            "std::ffi::CString should be kept qualified: got {:?}",
            mixed_refs
        );
    }

    #[test]
    fn test_denylist_matches_on_last_segment_for_qualified_paths() {
        // This test verifies that denylist filtering works by matching last segment
        // std::sync::Mutex should be filtered because "Mutex" is in the denylist
        let src = r#"
fn test_denylist() -> std::sync::Mutex<u32> {
    std::sync::Mutex::new(42)
}
"#;
        let ex = extract(src);

        let test_denylist_id =
            find_node_id(&ex, "test_denylist", "function").expect("test_denylist function exists");
        let refs = find_references_edges(&ex, &test_denylist_id);

        // std::sync::Mutex should be filtered out because "Mutex" is in the denylist
        assert!(
            refs.iter().all(
                |e| !matches!(&e.target, EdgeTarget::Symbol(t) if t.name == "std::sync::Mutex")
            ),
            "std::sync::Mutex should be filtered (Mutex is denylisted): got {:?}",
            refs
        );
    }

    // ---- ADR-0036 §5: a bodiless declaration is a node ---------------------

    const TRAIT_WITH_SIGNATURES: &str = r#"
pub struct Widget;
pub struct WidgetId;

pub trait Store {
    /// A bodiless declaration — the contract callers program against.
    fn get(&self, id: WidgetId) -> Widget;

    /// A default method: it has a body, so it is NOT abstract, and the call it
    /// makes through the abstraction is the edge ADR-0036 §5 exists to enable.
    fn get_or_default(&self, id: WidgetId) -> Widget {
        self.get(id)
    }
}

pub struct MemStore;

impl Store for MemStore {
    fn get(&self, id: WidgetId) -> Widget {
        Widget
    }
}
"#;

    /// A trait's bodiless `fn get(&self) -> …;` is a **node**, kind `function`,
    /// carrying `attrs["abstract"]`.
    ///
    /// The kind is the assertion that matters: a separate `trait_method` kind
    /// would be outside `filigrio_resolve::is_linkable` and therefore unbindable,
    /// which is the P3 defect with a new name (ADR-0036 §5 — Kythe's node fact,
    /// not SCIP's six kind variants).
    #[test]
    fn a_trait_signature_item_is_an_abstract_function_node() {
        let ex = extract(TRAIT_WITH_SIGNATURES);

        let get = ex
            .nodes
            .iter()
            .find(|n| n.label == "get" && n.attrs.contains_key("abstract"))
            .unwrap_or_else(|| {
                panic!(
                    "no abstract `get` node: {:?}",
                    ex.nodes
                        .iter()
                        .map(|n| (&n.id.0, &n.kind))
                        .collect::<Vec<_>>()
                )
            });
        assert_eq!(get.kind, "function", "the kind stays `function`");
        assert_eq!(get.attrs.get("abstract").map(String::as_str), Some("true"));
        assert_eq!(
            get.attrs.get("impl").map(String::as_str),
            Some("Store"),
            "a declaration is owned by its trait"
        );
        assert_eq!(get.id.0, "fn:src/demo.rs:Store::get");
        assert_eq!(get.label, "get", "the label stays bare (ADR-0028)");
    }

    /// The declaration is contained by the **trait**, not the file — so
    /// "traverse the abstraction to its operations" works, exactly as §4 made it
    /// work for an `impl` block's methods.
    #[test]
    fn a_trait_contains_its_signature_items() {
        let ex = extract(TRAIT_WITH_SIGNATURES);
        let contained: Vec<&str> = ex
            .edges
            .iter()
            .filter(|e| e.relation == CONTAINS && e.source.0 == "type:src/demo.rs:Store")
            .filter_map(|e| match &e.target {
                EdgeTarget::Node(t) => Some(t.0.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            contained.contains(&"fn:src/demo.rs:Store::get"),
            "the trait must contain its declaration: {contained:?}"
        );
    }

    /// A declaration's **signature is a contract**, so it emits `type/param` and
    /// `type/return` like any other method (ADR-0036 §5 + R2). Without this the
    /// abstraction is a node with no incident type edges — present in the graph
    /// and still invisible to the ranking §5 exists to fix.
    #[test]
    fn a_trait_signature_item_emits_its_param_and_return_types() {
        let ex = extract(TRAIT_WITH_SIGNATURES);
        let from_decl = |rel: &str| -> Vec<String> {
            ex.edges
                .iter()
                .filter(|e| e.relation == rel && e.source.0 == "fn:src/demo.rs:Store::get")
                .map(|e| match &e.target {
                    EdgeTarget::Symbol(t) => t.name.clone(),
                    EdgeTarget::Node(t) => t.0.clone(),
                })
                .collect()
        };
        assert_eq!(from_decl("type/param"), vec!["WidgetId".to_string()]);
        assert_eq!(from_decl("type/return"), vec!["Widget".to_string()]);
    }

    /// The `trait` itself is abstract, and a `struct` is not — the fact covers
    /// the declaring type as well as its members (Kythe's `tag/abstract`).
    #[test]
    fn the_trait_node_is_abstract_and_a_struct_is_not() {
        let ex = extract(TRAIT_WITH_SIGNATURES);
        let by_label = |l: &str| {
            ex.nodes
                .iter()
                .find(|n| n.label == l && n.kind != "function")
        };
        assert_eq!(
            by_label("Store")
                .and_then(|n| n.attrs.get("abstract"))
                .map(String::as_str),
            Some("true"),
            "a trait is the abstract type"
        );
        assert!(
            !by_label("Widget").is_some_and(|n| n.attrs.contains_key("abstract")),
            "a struct is concrete — the key is absent, never `false`"
        );
    }

    /// A method **with a body** is not abstract, wherever it lives: the trait's
    /// own default method and the `impl`'s override both stay unmarked. This is
    /// the half that keeps `abstract` meaning something.
    #[test]
    fn a_method_with_a_body_is_never_abstract() {
        let ex = extract(TRAIT_WITH_SIGNATURES);
        let marked: Vec<&str> = ex
            .nodes
            .iter()
            .filter(|n| n.attrs.contains_key("abstract") && n.kind == "function")
            .map(|n| n.id.0.as_str())
            .collect();
        assert_eq!(
            marked,
            vec!["fn:src/demo.rs:Store::get"],
            "only the bodiless declaration is abstract"
        );
    }

    /// An `extern` block's `fn …;` uses the **same grammar rule** as a trait
    /// declaration (`function_signature_item`) and is **not** a node.
    ///
    /// It is a foreign symbol — a declaration of something defined outside this
    /// language entirely, not an abstraction this codebase implements — so
    /// minting a node for it would put an unimplementable `malloc` into the
    /// candidate pool for every call named `malloc`. Scoping §5 by the enclosing
    /// `trait` rather than by the node kind is what draws that line.
    #[test]
    fn an_extern_block_declaration_is_not_a_node() {
        let ex = extract(
            r#"
extern "C" {
    fn malloc(size: usize) -> *mut u8;
}
"#,
        );
        assert!(
            !ex.nodes.iter().any(|n| n.label == "malloc"),
            "an FFI declaration is not an abstraction: {:?}",
            ex.nodes.iter().map(|n| &n.id.0).collect::<Vec<_>>()
        );
    }
}
