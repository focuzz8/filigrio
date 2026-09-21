//! `TypeScriptExtractor` — a **real** tree-sitter extractor for TypeScript /
//! JavaScript (Phase 4). Handles `.ts`/`.tsx`/`.js` (the `.tsx` variant uses the
//! TSX grammar; JS parses under the TS grammar, a syntactic superset).
//!
//! Emits, per file:
//!   * a `file` node;
//!   * `class` nodes and `function` nodes — a `function`/`method`, plus **arrow
//!     functions bound to a `const`/`let`** (`const f = () => {}` → a `function`
//!     node named `f`, matching graphify). A `method` in a `class` is tagged
//!     `attrs["impl"] = <class>` and **contained by its class** (so a class owns
//!     its methods and they cluster together);
//!   * `contains` edges (file → top-level def, class → method);
//!   * `calls` edges (enclosing def → callee) as UNRESOLVED `Symbol` targets;
//!   * `imports` edges file → each imported name.
//!
//! **Receiver-type hints.** TypeScript is statically typed, so — like the Rust
//! extractor — a method call gets a `hints["type"]` when the receiver's type is
//! knowable: `this.method()` → the enclosing class; `Class.method()` → that
//! class; and `x.method()` where `x` came from `new Class(..)`, a `: Class`
//! annotation, or a typed parameter (a small local dataflow pass). `new Class()`
//! itself is NOT a call edge — it only types the binding (matching graphify).
//!
//! **Structure (ADR-0037 §3).** Shared plumbing lives in the generic [`Driver`]
//! (scope value = a bare type-name `String`); this backend supplies the split
//! capability traits + a thin TS-specific walk. The ADR-0036 heritage/field
//! features — `interface` nodes, `extends`/`implements`, `type/field`, and enum
//! variants — are implemented here on tree-sitter (Phase 0b, ADR-0037b). The **oxc
//! frontend swap** (replacing this tree-sitter walk for real binding resolution)
//! remains deferred.

use crate::capability::{
    FieldExtractor, Frontend, HeritageExtractor, ImportExtractor, NodeMapper, ParsedImport,
    ReceiverTyper, TypeNamer,
};
use crate::driver::Driver;
use filigrio_core::{Artifact, Export, Extraction, Extractor, NodeId, Result, TargetRef};
use tree_sitter::{Node as TsNode, Parser};

// Named only by the test module (via its `use super::*`).
#[cfg(test)]
use filigrio_core::{EdgeTarget, Node};

/// Type names that are never graph nodes and so must not be `type/field` targets:
/// the `type_identifier` primitive `bigint` (the `predefined_type` scalars are
/// dropped by node kind) plus builtin generic containers, whose *inner* type args
/// are what carry the real dependency (ADR-0037b follow-up).
fn is_ts_never_node(name: &str) -> bool {
    matches!(
        name,
        "bigint"
            | "Array"
            | "ReadonlyArray"
            | "Map"
            | "ReadonlyMap"
            | "Set"
            | "ReadonlySet"
            | "WeakMap"
            | "WeakSet"
            | "Promise"
            | "Record"
    )
}

/// The tree-sitter node kinds that spell "a class declaration".
///
/// The abstract form is a **separate grammar rule**, not a `class_declaration`
/// carrying an `abstract` modifier: tree-sitter-typescript spells
/// `export abstract class C {…}` as `abstract_class_declaration`. Every site
/// that recognises a class must therefore accept all three spellings, and the
/// cost of missing one is total — an unmatched kind falls through the walk, so
/// the class yields no node, no export entry, no heritage/field edges and no
/// type-parameter scope, and its methods lose their owner qualifier. Naming the
/// set once is what keeps the five recognition sites from drifting apart again.
fn is_class_kind(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration" | "abstract_class_declaration" | "class"
    )
}

pub struct TypeScriptExtractor;

impl TypeScriptExtractor {
    pub fn new() -> Self {
        TypeScriptExtractor
    }
}

impl Default for TypeScriptExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for TypeScriptExtractor {
    fn handles(&self, artifact: &Artifact) -> bool {
        matches!(
            artifact.language.as_deref(),
            Some("typescript") | Some("javascript")
        )
    }

    fn extract(&self, artifact: &Artifact, bytes: &[u8]) -> Result<Extraction> {
        let mut parser = Parser::new();
        // `.tsx`/`.jsx` need the JSX-aware grammar; everything else (incl. JS)
        // parses under the TypeScript grammar.
        let lang = if artifact.path.ends_with("x") {
            tree_sitter_typescript::LANGUAGE_TSX
        } else {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT
        };
        parser
            .set_language(&lang.into())
            .map_err(|e| filigrio_core::Error::Parse(format!("load ts grammar: {e}")))?;
        let tree = parser.parse(bytes, None).ok_or_else(|| {
            filigrio_core::Error::Parse(format!("parse failed: {}", artifact.path))
        })?;

        let mut ctx = Ctx::new(artifact, bytes);
        ctx.walk(tree.root_node(), None, None);
        Ok(ctx.d.finish())
    }
}

/// TypeScript backend: the shared [`Driver`] (scope value = a bare type name) plus
/// the capability-trait impls below and the TS-specific walk.
struct Ctx<'a> {
    d: Driver<'a, String>,
}

impl Frontend for Ctx<'_> {
    type Node<'tree> = TsNode<'tree>;
}

impl NodeMapper for Ctx<'_> {
    /// `class_declaration`/`abstract_class_declaration`/`class`→class; method /
    /// function / generator-function declarations→function. (Arrow-const functions
    /// are handled in the walk.)
    fn def_kind<'t>(&self, node: TsNode<'t>) -> Option<(&'static str, &'static str)> {
        Some(match node.kind() {
            k if is_class_kind(k) => ("type", "class"),
            "interface_declaration" => ("type", "interface"),
            "type_alias_declaration" => ("type", "type_alias"),
            "enum_declaration" => ("type", "enum"),
            "method_definition" | "function_declaration" | "generator_function_declaration" => {
                ("fn", "function")
            }
            _ => return None,
        })
    }
}

impl TypeNamer for Ctx<'_> {
    /// Base type name of a `type_annotation` / type node, unwrapping the leading
    /// `:`, generics (`Array<T>` → `Array`) and qualified names (`a.B` → `B`).
    fn type_name<'t>(&self, ty: TsNode<'t>) -> Option<String> {
        match ty.kind() {
            "type_annotation" => ty.named_child(0).and_then(|c| self.type_name(c)),
            "type_identifier" | "identifier" | "predefined_type" => self.d.text(ty),
            "generic_type" => ty
                .child_by_field_name("name")
                .and_then(|n| self.type_name(n)),
            "nested_type_identifier" => ty
                .child_by_field_name("name")
                .and_then(|n| self.d.text(n))
                .or_else(|| {
                    self.d
                        .text(ty)
                        .and_then(|t| t.rsplit('.').next().map(str::to_string))
                }),
            _ => None,
        }
    }
}

impl ReceiverTyper for Ctx<'_> {
    /// TS receivers resolve to a bare type name (no opaque/defer policy — untyped
    /// receivers fall back to a bare-name call in the walk).
    type Receiver = Option<String>;

    fn callee_ref<'t>(
        &self,
        func: TsNode<'t>,
        current_class: Option<&str>,
    ) -> Option<(String, Option<String>)> {
        match func.kind() {
            "identifier" => self.d.text(func).map(|n| (n, None)),
            // `obj.method()` — method name is the property; hint from the receiver.
            "member_expression" => {
                let method = func
                    .child_by_field_name("property")
                    .and_then(|p| self.d.text(p))?;
                let hint = self.receiver_type(func, current_class);
                Some((method, hint))
            }
            _ => None,
        }
    }

    /// The receiver type of a `member_expression`, when statically knowable:
    /// `this` → the enclosing class; an UpperCamel name → that class; a typed
    /// local → its type. Otherwise unknown (the call resolves by bare name).
    fn receiver_type<'t>(&self, member: TsNode<'t>, current_class: Option<&str>) -> Option<String> {
        let obj = member.child_by_field_name("object")?;
        match obj.kind() {
            "this" => current_class.map(str::to_string),
            "identifier" => {
                let name = self.d.text(obj)?;
                if name.chars().next().is_some_and(|c| c.is_uppercase()) {
                    Some(name)
                } else {
                    self.d.lookup_var(&name)
                }
            }
            _ => None,
        }
    }
}

impl ImportExtractor for Ctx<'_> {
    /// Decompose an `import_statement`: one entry per bound name, each carrying the
    /// statement's raw module `specifier` (`'@acme/ui'`, `'./shapes'`) and, when
    /// the local name differs from the source name, the pre-alias `imported` name
    /// (ADR-0018/0020). `import * as N` binds `N`; a bare `import 'x'` binds none.
    fn imports<'t>(&self, node: TsNode<'t>) -> Vec<ParsedImport> {
        let specifier = self.import_specifier(node);
        self.imported_names(node)
            .into_iter()
            .map(|(bound, imported)| {
                let alias = (bound != imported).then(|| bound.clone());
                ParsedImport::Named {
                    specifier: specifier.clone(),
                    imported,
                    alias,
                }
            })
            .collect()
    }
}

// ADR-0037b structural features implementation
impl HeritageExtractor for Ctx<'_> {
    /// Heritage relations for TypeScript (ADR-0037b):
    ///   * class decl (incl. `abstract`) → `class_heritage` → `implements_clause` → each `implements`
    ///   * class decl (incl. `abstract`) → `class_heritage` → `extends_clause` → each `extends`
    ///   * `interface_declaration` → `extends_type_clause` → each `extends`
    fn heritage<'t>(&self, node: TsNode<'t>) -> Vec<crate::capability::Heritage> {
        if !is_class_kind(node.kind()) && node.kind() != "interface_declaration" {
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                // class: `class_heritage` is a direct child wrapping the clauses
                // (it is NOT reached via a `heritage` field name).
                "class_heritage" => {
                    let mut inner = child.walk();
                    for clause in child.named_children(&mut inner) {
                        let rel = match clause.kind() {
                            "extends_clause" => filigrio_core::relation::EXTENDS,
                            "implements_clause" => filigrio_core::relation::IMPLEMENTS,
                            _ => continue,
                        };
                        for t in self.type_list(&clause) {
                            out.push(crate::capability::Heritage {
                                relation: rel,
                                target: t,
                            });
                        }
                    }
                }
                // interface: `extends_type_clause` is a direct child of the decl.
                "extends_type_clause" => {
                    for t in self.type_list(&child) {
                        out.push(crate::capability::Heritage {
                            relation: filigrio_core::relation::EXTENDS,
                            target: t,
                        });
                    }
                }
                _ => {}
            }
        }
        out
    }
}

impl FieldExtractor for Ctx<'_> {
    /// Fields for TypeScript (ADR-0037b):
    ///   * class decl (incl. `abstract`) → body → `public_field_definition` / `property_signature`
    fn fields<'t>(&self, node: TsNode<'t>) -> Vec<crate::capability::Field> {
        // Member container: class/interface bodies, or a `type X = { … }` alias
        // whose RHS is an object type (union / generic / name aliases have no fields).
        let container = match node.kind() {
            k if is_class_kind(k) || k == "interface_declaration" => {
                node.child_by_field_name("body")
            }
            "type_alias_declaration" => node
                .child_by_field_name("value")
                .filter(|v| v.kind() == "object_type"),
            _ => return Vec::new(),
        };
        let Some(body) = container else {
            return Vec::new();
        };

        let mut out = Vec::new();
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            let (name, ty) = match child.kind() {
                "public_field_definition" | "property_signature" => (
                    child
                        .child_by_field_name("name")
                        .and_then(|n| self.d.text(n)),
                    child.child_by_field_name("type"),
                ),
                _ => continue,
            };
            let (Some(name), Some(ty)) = (name, ty) else {
                continue;
            };
            // Collect every nameable, non-primitive target type: a generic/container
            // is unwrapped to its inner type arg(s) (`Array<Foo>`→Foo, `Foo[]`→Foo),
            // a *user* generic keeps both container and inner, unions emit each member;
            // scalar primitives and builtin containers are skipped (ADR-0037b follow-up).
            let mut targets = Vec::new();
            self.collect_field_targets(ty, &mut targets);
            for type_name in targets {
                out.push(crate::capability::Field {
                    name: name.clone(),
                    type_name,
                    visibility: None, // TypeScript fields carry no visibility modifier here
                });
            }
        }
        out
    }
}

impl<'a> Ctx<'a> {
    fn new(artifact: &Artifact, src: &'a [u8]) -> Self {
        Ctx {
            d: Driver::new(artifact, src, "typescript"),
        }
    }

    /// Helper: extract type names from a type list (extends/implements clauses)
    fn type_list(&self, node: &TsNode<'_>) -> Vec<String> {
        let mut out = Vec::new();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if let Some(name) = self.type_name(child) {
                out.push(name);
            }
        }
        out
    }

    /// `current_fn` = enclosing def; `current_class` = enclosing class (id, name).
    fn walk(
        &mut self,
        node: TsNode<'_>,
        current_fn: Option<(NodeId, String)>,
        current_class: Option<(NodeId, String)>,
    ) {
        match node.kind() {
            k if is_class_kind(k) => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| self.d.text(n))
                {
                    let (prefix, kind) = self.def_kind(node).unwrap_or(("type", "class"));
                    let file_id = self.d.file_id.clone();
                    let id = self.d.add_def(prefix, kind, &name, node, &file_id, None);

                    // `abstract class C` — the type is a declaration too, not just
                    // its members (ADR-0036 §5, Kythe `tag/abstract`). The kind
                    // stays `class`, exactly as an abstract method's stays
                    // `function`.
                    if node.kind() == "abstract_class_declaration" {
                        self.d.mark_abstract(&id);
                    }

                    // Register as local type for noise filtering exceptions (ADR-0036 R1.1)
                    let base_name = base_type_name(&name);
                    self.d.register_local_type(base_name);

                    // The class's own `<T, …>` scope: pushed for the body it
                    // encloses (methods see `T` without redeclaring it), popped on
                    // the way out so a sibling declaration does not inherit it.
                    let type_params = self.collect_type_parameters(node);
                    self.d.push_type_parameters(type_params);

                    // Emit structural edges for the class
                    self.emit_type_edges(&id, node);
                    self.walk_children(node, current_fn, Some((id, name)));
                    self.d.pop_type_parameters();
                    return;
                }
            }
            "interface_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| self.d.text(n))
                {
                    let (prefix, kind) = self.def_kind(node).unwrap_or(("type", "interface"));
                    let file_id = self.d.file_id.clone();
                    let id = self.d.add_def(prefix, kind, &name, node, &file_id, None);

                    // Register as local type for noise filtering exceptions (ADR-0036 R1.1)
                    let base_name = base_type_name(&name);
                    self.d.register_local_type(base_name);

                    // The interface's own `<T, …>` scope (see the class arm).
                    let type_params = self.collect_type_parameters(node);
                    self.d.push_type_parameters(type_params);

                    // An `interface` is the pure-abstract type: every member it
                    // declares is a declaration (ADR-0036 §5).
                    self.d.mark_abstract(&id);

                    // Emit structural edges for the interface
                    self.emit_type_edges(&id, node);
                    // The interface is passed down as the enclosing type, so its
                    // `method_signature` members are owner-qualified and contained
                    // by it — the same shape a class gives its methods. Before §5
                    // this was `None`, which was harmless only because nothing in
                    // an interface body became a node.
                    self.walk_children(node, current_fn, Some((id, name)));
                    self.d.pop_type_parameters();
                    return;
                }
            }
            "enum_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| self.d.text(n))
                {
                    let (prefix, kind) = self.def_kind(node).unwrap_or(("type", "enum"));
                    let file_id = self.d.file_id.clone();
                    let id = self.d.add_def(prefix, kind, &name, node, &file_id, None);
                    // Emit enum variants
                    self.emit_variants(node, &name, &id);
                    self.walk_children(node, current_fn, None);
                    return;
                }
            }
            "type_alias_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| self.d.text(n))
                {
                    let (prefix, kind) = self.def_kind(node).unwrap_or(("type", "type_alias"));
                    let file_id = self.d.file_id.clone();
                    let id = self.d.add_def(prefix, kind, &name, node, &file_id, None);
                    // `type Pair<T> = { a: T }` declares a scope too — without the
                    // frame its members would emit `type/field -> T`.
                    let type_params = self.collect_type_parameters(node);
                    self.d.push_type_parameters(type_params);
                    // Object-type aliases (`type X = { … }`) emit field_type edges;
                    // union / generic / name aliases just create the node.
                    self.emit_type_edges(&id, node);
                    self.walk_children(node, current_fn, None);
                    self.d.pop_type_parameters();
                    return;
                }
            }
            "method_definition" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| self.d.text(n))
                {
                    let (container, owner) = self.container_of(&current_class);
                    let (prefix, kind) = self.def_kind(node).unwrap_or(("fn", "function"));
                    let id =
                        self.d
                            .add_def(prefix, kind, &name, node, &container, owner.as_deref());
                    self.enter_fn(node, id, name, current_class);
                    return;
                }
            }
            // ADR-0036 §5 — the two spellings of a bodiless TS method.
            //
            // `abstract_method_signature` is `abstract go(): void` in an abstract
            // class; `method_signature` is `m(): void` in an `interface`. Both are
            // declarations and become `function` nodes marked `abstract`.
            //
            // `method_signature` also occurs in a **class** body, where it means
            // something else entirely — a TypeScript *overload* signature, whose
            // implementation with a body sits directly below it. That is not an
            // abstraction; minting a node for it would duplicate the method it
            // overloads. Hence the parent test rather than the kind alone.
            "abstract_method_signature" => {
                self.def_signature(node, current_class);
                return;
            }
            "method_signature" if node.parent().map(|p| p.kind()) == Some("interface_body") => {
                self.def_signature(node, current_class);
                return;
            }
            "function_declaration" | "generator_function_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| self.d.text(n))
                {
                    let (container, owner) = self.container_of(&current_class);
                    let (prefix, kind) = self.def_kind(node).unwrap_or(("fn", "function"));
                    let id =
                        self.d
                            .add_def(prefix, kind, &name, node, &container, owner.as_deref());
                    self.enter_fn(node, id, name, current_class);
                    return;
                }
            }
            "variable_declarator" => {
                self.walk_declarator(node, current_fn, current_class);
                return;
            }
            "call_expression" => {
                if let (Some((src_id, _)), Some(func)) =
                    (current_fn.as_ref(), node.child_by_field_name("function"))
                {
                    let class_name = current_class.as_ref().map(|(_, n)| n.as_str());
                    if let Some((callee, hint)) = self.callee_ref(func, class_name) {
                        let src_id = src_id.clone();
                        let mut tref = TargetRef::new(callee);
                        if let Some(ty) = hint {
                            tref.hints.insert("type".into(), ty);
                        }
                        self.d.emit_call(src_id, tref);
                    }
                }
            }
            "import_statement" => {
                // Each entry carries the module specifier (for the `ModuleResolver`,
                // ADR-0018) plus the pre-alias `imported` name (export-graph, 0020).
                for imp in self.imports(node) {
                    if let ParsedImport::Named {
                        specifier,
                        imported,
                        alias,
                    } = imp
                    {
                        let name = alias.clone().unwrap_or_else(|| imported.clone());
                        let mut tref = TargetRef::new(&name);
                        if let Some(spec) = &specifier {
                            tref.hints.insert("specifier".into(), spec.clone());
                        }
                        if alias.is_some() {
                            tref.hints.insert("imported".into(), imported);
                        }
                        self.d.emit_import(tref);
                    }
                }
            }
            "export_statement" => {
                self.handle_export(node);
            }
            _ => {}
        }
        self.walk_children(node, current_fn, current_class);
    }

    /// A **bodiless** method declaration as a node (ADR-0036 §5): an abstract
    /// class's `abstract go(): void` or an interface's `m(): void`.
    ///
    /// Identical to the `method_definition` path but for the `abstract` fact —
    /// the kind stays `function`, it is owner-qualified and contained by its
    /// class/interface, and [`Ctx::enter_fn`] gives it the same `type/param` /
    /// `type/return` edges a method with a body gets. Those signature edges are
    /// the point: a declaration with no incident type edges would be a node the
    /// ranking still cannot see.
    fn def_signature(&mut self, node: TsNode<'_>, current_class: Option<(NodeId, String)>) {
        let Some(name) = node
            .child_by_field_name("name")
            .and_then(|n| self.d.text(n))
        else {
            return;
        };
        let (container, owner) = self.container_of(&current_class);
        let id = self
            .d
            .add_def("fn", "function", &name, node, &container, owner.as_deref());
        self.d.mark_abstract(&id);
        self.enter_fn(node, id, name, current_class);
    }

    /// Emit ADR-0036 structural edges for a type (class/interface)
    fn emit_type_edges(&mut self, id: &NodeId, node: TsNode<'_>) {
        // Emit heritage edges (extends/implements)
        for h in self.heritage(node) {
            self.d.emit_heritage(id.clone(), h.relation, &h.target);
        }
        // Emit field_type edges
        for f in self.fields(node) {
            self.d
                .emit_field_type(id.clone(), &f.name, &f.type_name, f.visibility.as_deref());
        }
    }

    /// Collect the nameable, non-primitive target types of a field's type node,
    /// unwrapping generics/containers to their inner type argument(s) (ADR-0037b
    /// follow-up): `Array<Foo>`→Foo, `Foo[]`→Foo, `Map<string,W>`→W, `A | B`→A,B.
    /// A *user* generic keeps both its container name and inner args; builtin
    /// containers and scalar primitives (incl. the `type_identifier` `bigint`) are
    /// skipped — they are never graph nodes.
    fn collect_field_targets<'t>(&self, ty: TsNode<'t>, out: &mut Vec<String>) {
        self.collect_type_targets(ty, out);
    }

    /// Collect the nameable, non-primitive target types from a type annotation,
    /// unwrapping generics to their inner type parameter(s) (ADR-0036):
    /// `Array<Foo>`→Foo, `Foo[]`→Foo, `Promise<Widget>`→Widget, `A | B`→A,B.
    /// Builtin containers and scalar primitives are skipped.
    /// This is the core implementation reused for field, param, and return types.
    fn collect_type_targets<'t>(&self, ty: TsNode<'t>, out: &mut Vec<String>) {
        match ty.kind() {
            "type_annotation" => {
                if let Some(c) = ty.named_child(0) {
                    self.collect_type_targets(c, out);
                }
            }
            "predefined_type" => {} // number/string/boolean/symbol/void/… — never nodes
            "type_identifier" | "identifier" | "nested_type_identifier" => {
                if let Some(n) = self.type_name(ty) {
                    if !is_ts_never_node(&n) {
                        out.push(n);
                    }
                }
            }
            "generic_type" => {
                // Emit user-defined containers, but drop builtin containers (`Array<..>`, `Promise<..>`)
                if let Some(name) = ty
                    .child_by_field_name("name")
                    .and_then(|c| self.type_name(c))
                {
                    if !is_ts_never_node(&name) {
                        out.push(name);
                    }
                }
                // Then recurse into the type arguments for the inner types
                if let Some(args) = ty.child_by_field_name("type_arguments") {
                    let mut c = args.walk();
                    for a in args.named_children(&mut c) {
                        self.collect_type_targets(a, out);
                    }
                }
            }
            "array_type" => {
                if let Some(el) = ty.named_child(0) {
                    self.collect_type_targets(el, out);
                }
            }
            "union_type" | "intersection_type" => {
                let mut c = ty.walk();
                for m in ty.named_children(&mut c) {
                    self.collect_type_targets(m, out);
                }
            }
            _ => {}
        }
    }

    /// Emit `enum_variant` nodes + `has_variant` edges for an enum (ADR-0037b)
    fn emit_variants(&mut self, enum_node: TsNode<'_>, enum_name: &str, enum_id: &NodeId) {
        let Some(body) = enum_node.child_by_field_name("body") else {
            return;
        };
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            // Bare member `A` is a `property_identifier`; `B = 2` is an
            // `enum_assignment` whose `name` field holds the identifier.
            let vname = match child.kind() {
                "property_identifier" => self.d.text(child),
                "enum_assignment" => child
                    .child_by_field_name("name")
                    .and_then(|n| self.d.text(n)),
                _ => None,
            };
            if let Some(vname) = vname {
                self.d.add_variant(enum_id, enum_name, &vname, child);
            }
        }
    }

    /// The container id + owner name a def belongs to (its class, else the file).
    fn container_of(&self, current_class: &Option<(NodeId, String)>) -> (NodeId, Option<String>) {
        match current_class {
            Some((id, name)) => (id.clone(), Some(name.clone())),
            None => (self.d.file_id.clone(), None),
        }
    }

    /// Enter a function body: push a scope, bind parameters, recurse, pop.
    fn enter_fn(
        &mut self,
        node: TsNode<'_>,
        id: NodeId,
        name: String,
        current_class: Option<(NodeId, String)>,
    ) {
        self.d.scope_push();

        // The function's own `<T, …>` scope, entered BEFORE its parameters and
        // return type are read. Pushed, not assigned: a method of a generic class
        // declares nothing of its own and must still see the class's parameters.
        let type_params = self.collect_type_parameters(node);
        self.d.push_type_parameters(type_params);

        if let Some(params) = node.child_by_field_name("parameters") {
            self.collect_params(params, &id, &name);
        }

        // Emit return_type edges (ADR-0036)
        if let Some(return_type_node) = node.child_by_field_name("return_type") {
            let mut targets = Vec::new();
            self.collect_type_targets(return_type_node, &mut targets);
            for type_name in targets {
                let normalized_name = base_type_name(&type_name);
                // Prevent self-reference (function -> function)
                if base_type_name(&name) != normalized_name {
                    let _ = self.d.emit_return_type(id.clone(), normalized_name);
                }
            }
        }

        self.walk_children(node, Some((id, name)), current_class);
        self.d.pop_type_parameters();
        self.d.scope_pop();
    }

    /// Collect type parameters for noise filtering from a given node.
    /// Returns a HashSet of type parameter names (e.g., `T`, `TKey`, etc.).
    fn collect_type_parameters(&self, node: TsNode<'_>) -> std::collections::HashSet<String> {
        let mut params = std::collections::HashSet::new();
        if let Some(type_params) = node.child_by_field_name("type_parameters") {
            let mut cursor = type_params.walk();
            for child in type_params.children(&mut cursor) {
                if child.kind() == "type_parameter" {
                    if let Some(name) = child
                        .child_by_field_name("name")
                        .and_then(|n| self.d.text(n))
                    {
                        params.insert(name);
                    }
                }
            }
        }
        params
    }

    /// A `const`/`let` declarator: an arrow/function value is a **function def**
    /// named by the binding; anything else types the binding for receiver
    /// inference (`const c = new Circle()` / `const x: Circle = ..`).
    fn walk_declarator(
        &mut self,
        node: TsNode<'_>,
        current_fn: Option<(NodeId, String)>,
        current_class: Option<(NodeId, String)>,
    ) {
        let value = node.child_by_field_name("value");
        let fn_value = value.filter(|v| {
            matches!(
                v.kind(),
                "arrow_function" | "function_expression" | "function"
            )
        });
        if let Some(v) = fn_value {
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| self.d.text(n))
            {
                let (container, owner) = self.container_of(&current_class);
                let id =
                    self.d
                        .add_def("fn", "function", &name, node, &container, owner.as_deref());
                self.enter_fn(v, id, name, current_class);
                return;
            }
        }
        // Value binding: infer the type, walk the RHS, then record the binding.
        let binding = self.infer_declarator_type(node);
        self.walk_children(node, current_fn, current_class);
        if let Some((var, ty)) = binding {
            self.d.scope_insert(&var, ty);
        }
    }

    fn walk_children(
        &mut self,
        node: TsNode<'_>,
        current_fn: Option<(NodeId, String)>,
        current_class: Option<(NodeId, String)>,
    ) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child, current_fn.clone(), current_class.clone());
        }
    }

    // ---- receiver-type inference (scope stack) -----------------------------

    /// Bind each typed parameter (`w: Widget` → `w: Widget`) into the scope.
    /// Also emit param_type edges (ADR-0036).
    fn collect_params(&mut self, params: TsNode<'_>, fn_id: &NodeId, fn_name: &str) {
        let mut cursor = params.walk();
        for child in params.children(&mut cursor) {
            if !matches!(child.kind(), "required_parameter" | "optional_parameter") {
                continue;
            }
            let name = child
                .child_by_field_name("pattern")
                .filter(|p| p.kind() == "identifier")
                .and_then(|p| self.d.text(p));
            let ty = child
                .child_by_field_name("type")
                .and_then(|t| self.type_name(t));
            if let (Some(name), Some(ty)) = (name.clone(), ty) {
                self.d.scope_insert(&name, ty);
            }

            // Emit param_type edges (ADR-0036)
            if let Some(ty_node) = child.child_by_field_name("type") {
                let mut targets = Vec::new();
                self.collect_type_targets(ty_node, &mut targets);
                for type_name in targets {
                    let normalized_name = base_type_name(&type_name);
                    // Prevent self-reference (function -> function)
                    if base_type_name(fn_name) != normalized_name {
                        let _ = self.d.emit_param_type(fn_id.clone(), normalized_name);
                    }
                }
            }
        }
    }

    /// `(var, type)` for a declarator whose type is knowable: a `: Type`
    /// annotation, or a `new Type(..)` initializer.
    fn infer_declarator_type(&self, decl: TsNode<'_>) -> Option<(String, String)> {
        let name = decl
            .child_by_field_name("name")
            .filter(|n| n.kind() == "identifier")
            .and_then(|n| self.d.text(n))?;
        if let Some(ty) = decl
            .child_by_field_name("type")
            .and_then(|t| self.type_name(t))
        {
            return Some((name, ty));
        }
        let val = decl.child_by_field_name("value")?;
        if val.kind() == "new_expression" {
            let ty = val
                .child_by_field_name("constructor")
                .filter(|c| c.kind() == "identifier")
                .and_then(|c| self.d.text(c))?;
            return Some((name, ty));
        }
        None
    }

    // ---- imports & exports -------------------------------------------------

    /// Build the file's export table from an `export_statement` (ADR-0020):
    /// local exports (`export function/class/const`, `export { x }`), named
    /// re-exports (`export { x } from 'm'`), and wildcards (`export * from 'm'`).
    fn handle_export(&mut self, node: TsNode<'_>) {
        match self.import_specifier(node) {
            // Re-export: `export … from "specifier"`.
            Some(spec) => {
                if let Some(clause) = self.export_clause(node) {
                    let mut cur = clause.walk();
                    for esp in clause.named_children(&mut cur) {
                        if esp.kind() != "export_specifier" {
                            continue;
                        }
                        let imported = esp.child_by_field_name("name").and_then(|n| self.d.text(n));
                        let exported = esp
                            .child_by_field_name("alias")
                            .and_then(|n| self.d.text(n))
                            .or_else(|| imported.clone());
                        if let (Some(name), Some(imported)) = (exported, imported) {
                            self.d.exports.push(Export::ReExport {
                                name,
                                specifier: spec.clone(),
                                imported,
                            });
                        }
                    }
                } else if !self.has_child_kind(node, "namespace_export") {
                    // `export * from spec` (skip the rarer `export * as ns`).
                    self.d.exports.push(Export::Star { specifier: spec });
                }
            }
            // Local export: `export function/class/const …` or `export { a, b }`.
            None => {
                if let Some(clause) = self.export_clause(node) {
                    let mut cur = clause.walk();
                    for esp in clause.named_children(&mut cur) {
                        // `export { a }` → Local{a}; aliased `export { a as b }`
                        // (local rename) is deferred — surfaced via the fallback.
                        if esp.kind() == "export_specifier"
                            && esp.child_by_field_name("alias").is_none()
                        {
                            if let Some(name) =
                                esp.child_by_field_name("name").and_then(|n| self.d.text(n))
                            {
                                self.d.exports.push(Export::Local { name });
                            }
                        }
                    }
                } else {
                    for name in self.exported_decl_names(node) {
                        self.d.exports.push(Export::Local { name });
                    }
                }
            }
        }
    }

    fn export_clause<'n>(&self, node: TsNode<'n>) -> Option<TsNode<'n>> {
        node.named_children(&mut node.walk())
            .find(|c| c.kind() == "export_clause")
    }

    fn has_child_kind(&self, node: TsNode<'_>, kind: &str) -> bool {
        node.named_children(&mut node.walk())
            .any(|c| c.kind() == kind)
    }

    /// Names declared by an `export <decl>` (function/class/const), so each is a
    /// `Local` export whose terminal is the def node the walker also creates.
    fn exported_decl_names(&self, node: TsNode<'_>) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            match child.kind() {
                "function_declaration"
                | "generator_function_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration" => {
                    if let Some(n) = child
                        .child_by_field_name("name")
                        .and_then(|n| self.d.text(n))
                    {
                        out.push(n);
                    }
                }
                k if is_class_kind(k) => {
                    if let Some(n) = child
                        .child_by_field_name("name")
                        .and_then(|n| self.d.text(n))
                    {
                        out.push(n);
                    }
                }
                "lexical_declaration" | "variable_declaration" => {
                    let mut c2 = child.walk();
                    for d in child.named_children(&mut c2) {
                        if d.kind() == "variable_declarator" {
                            if let Some(n) = d
                                .child_by_field_name("name")
                                .filter(|n| n.kind() == "identifier")
                                .and_then(|n| self.d.text(n))
                            {
                                out.push(n);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// The raw module specifier of an `import` statement: the `source` string
    /// with its surrounding quotes stripped (`import .. from '@acme/ui'` →
    /// `@acme/ui`). `None` for a bare `import 'side-effect'` with no clause.
    fn import_specifier(&self, node: TsNode<'_>) -> Option<String> {
        let src = node.child_by_field_name("source")?;
        let raw = self.d.text(src)?;
        Some(
            raw.trim_matches(|c| c == '\'' || c == '"' || c == '`')
                .to_string(),
        )
    }

    /// The names an `import` binds, as `(bound, imported)`: `import { A, B as C }`
    /// → `[(A,A), (C,B)]`, `import D` → `[(D,D)]`, `import * as N` → `[(N,N)]`.
    /// `bound` is the local name (the call site); `imported` is the name in the
    /// source module (what the export-graph walk looks up — ADR-0020).
    fn imported_names(&self, node: TsNode<'_>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let Some(clause) = node
            .named_children(&mut node.walk())
            .find(|c| c.kind() == "import_clause")
        else {
            return out;
        };
        let mut cursor = clause.walk();
        for child in clause.named_children(&mut cursor) {
            match child.kind() {
                // default import: `import D from ..`
                "identifier" => {
                    if let Some(t) = self.d.text(child) {
                        out.push((t.clone(), t));
                    }
                }
                // `import * as N from ..`
                "namespace_import" => {
                    if let Some(t) = child
                        .named_children(&mut child.walk())
                        .find(|n| n.kind() == "identifier")
                        .and_then(|n| self.d.text(n))
                    {
                        out.push((t.clone(), t));
                    }
                }
                // `import { A, B as C } from ..`
                "named_imports" => {
                    let mut c2 = child.walk();
                    for spec in child.named_children(&mut c2) {
                        if spec.kind() == "import_specifier" {
                            let imported = spec
                                .child_by_field_name("name")
                                .and_then(|n| self.d.text(n));
                            let bound = spec
                                .child_by_field_name("alias")
                                .and_then(|n| self.d.text(n))
                                .or_else(|| imported.clone());
                            if let (Some(bound), Some(imported)) = (bound, imported) {
                                out.push((bound, imported));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }
}

/// The bare base identifier of a type name, stripping generic parameters and
/// path qualifiers: `MyType<T>` / `ns.Foo` / `Box<Foo>` → `MyType` / `Foo` / `Box`.
fn base_type_name(type_name: &str) -> &str {
    type_name
        .split('<')
        .next()
        .unwrap_or(type_name)
        .rsplit('.')
        .next()
        .unwrap_or(type_name)
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use filigrio_core::ArtifactKind;

    fn artifact(path: &str) -> Artifact {
        Artifact {
            path: path.into(),
            kind: ArtifactKind::Code,
            language: Some("typescript".into()),
        }
    }

    fn extract(src: &str) -> Extraction {
        TypeScriptExtractor::new()
            .extract(&artifact("src/demo.ts"), src.as_bytes())
            .unwrap()
    }

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

    fn node(ex: &Extraction, label: &str) -> Node {
        ex.nodes
            .iter()
            .find(|n| n.label == label)
            .unwrap_or_else(|| panic!("no node {label}"))
            .clone()
    }

    #[test]
    fn extracts_classes_methods_and_functions() {
        let src = r#"
export class Widget {
    n: number;
    go(): void {}
}
function helper(): number { return 7; }
"#;
        let ex = extract(src);
        assert_eq!(node(&ex, "Widget").kind, "class");
        assert_eq!(node(&ex, "go").kind, "function");
        assert_eq!(
            node(&ex, "go").attrs.get("impl").map(String::as_str),
            Some("Widget")
        );
        assert!(!node(&ex, "helper").attrs.contains_key("impl"));
    }

    #[test]
    fn arrow_const_becomes_a_function_node() {
        let src = "const build = (): number => { return 1; };";
        let ex = extract(src);
        assert_eq!(node(&ex, "build").kind, "function");
    }

    #[test]
    fn this_method_call_hints_enclosing_class() {
        let src = r#"
class S {
    run(): void { this.help(); }
    help(): void {}
}
"#;
        assert_eq!(hint_for(&extract(src), "help"), Some("S".into()));
    }

    #[test]
    fn new_binding_infers_receiver_type() {
        let src = r#"
class Widget { go(): void {} }
function build(): void {
    const w = new Widget();
    w.go();
}
"#;
        let ex = extract(src);
        assert_eq!(hint_for(&ex, "go"), Some("Widget".into()));
        // `new Widget()` itself is not a call edge.
        assert!(
            !ex.edges.iter().any(|e| e.relation == "calls"
                && matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "Widget")),
            "new-expression is not a call"
        );
    }

    #[test]
    fn annotated_parameter_infers_receiver_type() {
        let src = "function use(w: Widget): void { w.go(); }";
        assert_eq!(hint_for(&extract(src), "go"), Some("Widget".into()));
    }

    #[test]
    fn named_imports_bind_each_symbol() {
        let src = "import { Circle, Square as Sq } from './shapes';\nimport def from './d';\n";
        let ex = extract(src);
        let imported: Vec<&str> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "imports")
            .filter_map(|e| match &e.target {
                EdgeTarget::Symbol(r) => Some(r.name.as_str()),
                _ => None,
            })
            .collect();
        assert!(imported.contains(&"Circle"), "{imported:?}");
        assert!(imported.contains(&"Sq"), "alias bound: {imported:?}");
        assert!(imported.contains(&"def"), "default import: {imported:?}");
    }

    #[test]
    fn export_table_local_reexport_and_star() {
        let src = r#"
export function greet(): string { return "x"; }
export { helper as h } from './util';
export * from './shapes';
"#;
        let ex = extract(src);
        assert!(
            ex.exports.contains(&Export::Local {
                name: "greet".into()
            }),
            "local: {:?}",
            ex.exports
        );
        assert!(
            ex.exports.iter().any(|e| matches!(e,
                Export::ReExport { name, specifier, imported }
                if name == "h" && specifier == "./util" && imported == "helper")),
            "re-export: {:?}",
            ex.exports
        );
        assert!(
            ex.exports.contains(&Export::Star {
                specifier: "./shapes".into()
            }),
            "star: {:?}",
            ex.exports
        );
    }

    #[test]
    fn import_alias_carries_original_name() {
        let ex = extract("import { greet as g } from '@acme/ui';");
        let imp = ex
            .edges
            .iter()
            .find_map(|e| match &e.target {
                EdgeTarget::Symbol(r) if e.relation == "imports" && r.name == "g" => Some(r),
                _ => None,
            })
            .expect("bound name g");
        assert_eq!(imp.hints.get("imported").map(String::as_str), Some("greet"));
    }

    #[test]
    fn imports_carry_raw_specifier() {
        let ex = extract("import { greet } from '@acme/ui';\nimport { Circle } from './shapes';\n");
        let spec_of = |name: &str| -> Option<String> {
            ex.edges.iter().find_map(|e| match &e.target {
                EdgeTarget::Symbol(r) if e.relation == "imports" && r.name == name => {
                    r.hints.get("specifier").cloned()
                }
                _ => None,
            })
        };
        assert_eq!(spec_of("greet").as_deref(), Some("@acme/ui"));
        assert_eq!(spec_of("Circle").as_deref(), Some("./shapes"));
    }

    /// ADR-0037b — **a type-only import is an import.** `import type { X }`, the
    /// inline `import { type X, Y }` specifier, and the type-only default /
    /// namespace forms each bind their name and carry the module specifier,
    /// exactly like the value forms, so the ADR-0018/0020 resolver can bind them.
    ///
    /// The 0037b reproduction blamed this construct for the unresolved type-edge
    /// population. It is not the cause: extraction is byte-identical for the two
    /// forms (tree-sitter emits the same `import_statement → import_clause →
    /// named_imports → import_specifier` shape, `type` being an anonymous token;
    /// oxc exposes it as `import_kind`, which this backend deliberately ignores).
    /// This test pins that down so the equivalence cannot silently regress.
    #[test]
    fn type_only_imports_bind_like_value_imports() {
        let src = r#"
import type { Params } from '../../other/types';
import { Widget } from '../../other/types';
import { type Foo, Bar } from './m';
import type Def from './d';
import type * as NS from './n';
import type { Long as L } from './l';
"#;
        let ex = extract(src);
        let hint = |name: &str, key: &str| -> Option<String> {
            ex.edges.iter().find_map(|e| match &e.target {
                EdgeTarget::Symbol(r) if e.relation == "imports" && r.name == name => {
                    r.hints.get(key).cloned()
                }
                _ => None,
            })
        };
        let spec_of = |name: &str| hint(name, "specifier");
        // The reproduction: type-only named import, and the plain one beside it
        // (the control — it must not regress).
        assert_eq!(spec_of("Params").as_deref(), Some("../../other/types"));
        assert_eq!(spec_of("Widget").as_deref(), Some("../../other/types"));
        // Inline `type` modifier on one specifier of a mixed clause.
        assert_eq!(spec_of("Foo").as_deref(), Some("./m"));
        assert_eq!(spec_of("Bar").as_deref(), Some("./m"));
        // Type-only default / namespace forms share the value code path.
        assert_eq!(spec_of("Def").as_deref(), Some("./d"));
        assert_eq!(spec_of("NS").as_deref(), Some("./n"));
        // Aliasing still records the pre-alias name (ADR-0020's export lookup key).
        assert_eq!(spec_of("L").as_deref(), Some("./l"));
        assert_eq!(hint("L", "imported").as_deref(), Some("Long"));
    }

    /// The export-table twin of the above: `export type { X } from 'm'` is a
    /// re-export and `export type X = …` / `export interface X` are local exports,
    /// exactly like their value counterparts (ADR-0020).
    #[test]
    fn type_only_exports_populate_the_export_table() {
        let src = r#"
export type { Params } from './types';
export type Alias = string;
export interface Shape { a: string }
"#;
        let ex = extract(src);
        assert!(
            ex.exports.iter().any(|e| matches!(e,
                Export::ReExport { name, specifier, imported }
                if name == "Params" && specifier == "./types" && imported == "Params")),
            "type-only re-export: {:?}",
            ex.exports
        );
        assert!(
            ex.exports.contains(&Export::Local {
                name: "Alias".into()
            }),
            "exported type alias: {:?}",
            ex.exports
        );
        assert!(
            ex.exports.contains(&Export::Local {
                name: "Shape".into()
            }),
            "exported interface: {:?}",
            ex.exports
        );
    }

    #[test]
    fn bare_call_is_unresolved_symbol() {
        let src = "function build(): void { helper(); }";
        let ex = extract(src);
        let build = node(&ex, "build").id;
        assert!(ex.edges.iter().any(|e| e.source == build
            && e.relation == "calls"
            && matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "helper" && !r.hints.contains_key("type"))));
    }

    #[test]
    fn deterministic_across_runs() {
        let src = "class A { m(): void { this.n(); } n(): void {} }";
        assert_eq!(extract(src), extract(src));
    }

    // ---- ADR-0037b structural edges (TypeScript) -----------------------

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

    #[test]
    fn interface_creates_interface_node() {
        // `interface I { ... }` → an `interface` node (ADR-0037b).
        let src = "interface I { x: number; }";
        let ex = extract(src);
        let iface = ex.nodes.iter().find(|n| n.label == "I");
        assert!(iface.is_some(), "interface node exists");
        assert_eq!(iface.unwrap().kind, "interface");
    }

    #[test]
    fn class_implements_emits_implements_edge() {
        // `class C implements I` → an `implements` edge C → I (ADR-0037b).
        let src = r#"
interface I {}
class C implements I {}
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "implements", "C", "I"),
            "C implements I: {:?}",
            ex.edges
        );
    }

    #[test]
    fn class_extends_emits_extends_edge() {
        // `class C extends Base` → an `extends` edge C → Base (ADR-0037b).
        let src = r#"
class Base {}
class C extends Base {}
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "extends", "C", "Base"),
            "C extends Base: {:?}",
            ex.edges
        );
    }

    #[test]
    fn interface_extends_emits_extends_edge() {
        // `interface I extends Base` → an `extends` edge I → Base (ADR-0037b).
        let src = r#"
interface Base {}
interface I extends Base {}
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "extends", "I", "Base"),
            "I extends Base: {:?}",
            ex.edges
        );
    }

    #[test]
    fn class_field_emits_field_type_edge() {
        // `class C { x: T }` → a `type/field` edge C → T carrying the field
        // name `x` (ADR-0036 §1a: no field node). ADR-0037b.
        let src = r#"
class T {}
class C { x: T; count: number }
"#;
        let ex = extract(src);
        let c_id = &ex.nodes.iter().find(|n| n.label == "C").unwrap().id;
        let field = ex.edges.iter().find(|e| {
            e.relation == "type/field"
                && &e.source == c_id
                && matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "T")
        });
        let field = field.expect("field_type C -> T present");
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
    fn interface_property_emits_field_type_edge() {
        // `interface I { x: T }` → a `type/field` edge I → T (ADR-0037b).
        let src = r#"
class T {}
interface I { x: T }
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/field", "I", "T"),
            "I has property of type T: {:?}",
            ex.edges
        );
    }

    #[test]
    fn enum_creates_enum_node_and_variants() {
        // `enum E { A, B }` → an `enum` node + `enum_variant` nodes + `has_variant`
        // edges (ADR-0037b).
        let src = "enum E { A, B }";
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
    fn class_with_multiple_implements_emits_multiple_edges() {
        // `class C implements I, J` → two `implements` edges (ADR-0037b).
        let src = r#"
interface I {}
interface J {}
class C implements I, J {}
"#;
        let ex = extract(src);
        assert!(has_sym_edge(&ex, "implements", "C", "I"));
        assert!(has_sym_edge(&ex, "implements", "C", "J"));
    }

    #[test]
    fn class_with_extends_and_implements_emits_both() {
        // `class C extends Base implements I` → both `extends` and `implements`
        // edges (ADR-0037b).
        let src = r#"
class Base {}
interface I {}
class C extends Base implements I {}
"#;
        let ex = extract(src);
        assert!(has_sym_edge(&ex, "extends", "C", "Base"));
        assert!(has_sym_edge(&ex, "implements", "C", "I"));
    }

    #[test]
    fn enum_with_assigned_values_emits_variants() {
        // `enum E { A = 1, B = 2 }` — assigned members (`enum_assignment`) are
        // variants too, not just bare `property_identifier` members.
        let ex = extract("enum E { A = 1, B = 2 }");
        let variants: Vec<&Node> = ex
            .nodes
            .iter()
            .filter(|n| n.kind == "enum_variant")
            .collect();
        assert_eq!(
            variants.len(),
            2,
            "assigned members are variants: {:?}",
            variants
        );
        let ids: Vec<&str> = variants.iter().map(|n| n.id.0.as_str()).collect();
        assert!(
            ids.iter().any(|id| id.ends_with("E::A")) && ids.iter().any(|id| id.ends_with("E::B")),
            "owner-qualified variant ids: {ids:?}"
        );
    }

    #[test]
    fn interface_extends_multiple_emits_each() {
        // `interface I extends A, B` → an `extends` edge to each.
        let ex = extract("interface A {}\ninterface B {}\ninterface I extends A, B {}");
        assert!(has_sym_edge(&ex, "extends", "I", "A"), "{:?}", ex.edges);
        assert!(has_sym_edge(&ex, "extends", "I", "B"), "{:?}", ex.edges);
    }

    #[test]
    fn generic_and_qualified_type_targets_unwrap() {
        // `extends Base<number>` unwraps to `Base`; a qualified field type
        // `some.Foo` unwraps to `Foo` (nested_type_identifier).
        let ex = extract("class Base {}\nclass C extends Base<number> { x: some.Foo }");
        assert!(
            has_sym_edge(&ex, "extends", "C", "Base"),
            "generic supertype -> Base: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Foo"),
            "qualified field type -> Foo: {:?}",
            ex.edges
        );
    }

    #[test]
    fn primitive_field_type_is_skipped() {
        // `n: number` / `s: string` emit NO field_type edge (primitives aren't
        // nodes, consistent with Rust); a nameable type still does.
        let ex = extract("class Foo {}\nclass C { n: number; s: string; obj: Foo }");
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "number"),
            "no field_type to number"
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "string"),
            "no field_type to string"
        );
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Foo"),
            "nameable field type kept: {:?}",
            ex.edges
        );
    }

    // ---- ADR-0037b type_alias node support ----------------------------

    #[test]
    fn type_alias_creates_type_alias_node() {
        // `type X = Foo` → a `type_alias` node (modern TS leans on `type` over
        // `interface`; these were previously invisible).
        let ex = extract("type Foo = {};\ntype X = Foo;");
        let n = ex.nodes.iter().find(|n| n.label == "X");
        assert!(n.is_some(), "type alias node exists: {:?}", ex.nodes);
        assert_eq!(n.unwrap().kind, "type_alias");
    }

    #[test]
    fn object_type_alias_emits_field_type_edges() {
        // `type X = { a: Foo; b: Bar }` → field_type X→Foo and X→Bar (object-type
        // aliases are struct-like; fields are edges, ADR-0036 §1a).
        let ex = extract("class Foo {}\nclass Bar {}\ntype X = { a: Foo; b: Bar };");
        assert!(
            has_sym_edge(&ex, "type/field", "X", "Foo"),
            "{:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/field", "X", "Bar"),
            "{:?}",
            ex.edges
        );
    }

    #[test]
    fn object_type_alias_skips_primitive_fields() {
        // Primitive members of an object-type alias are skipped, like class fields.
        let ex = extract("class Foo {}\ntype X = { a: Foo; n: number };");
        assert!(has_sym_edge(&ex, "type/field", "X", "Foo"));
        assert!(
            !has_sym_edge(&ex, "type/field", "X", "number"),
            "no field_type to number"
        );
    }

    #[test]
    fn union_alias_has_node_but_no_field_type() {
        // `type Y = A | B` → the node exists, but a union RHS has no object fields
        // (union member references are the deferred `references` follow-up).
        let ex = extract("class A {}\nclass B {}\ntype Y = A | B;");
        let y = ex
            .nodes
            .iter()
            .find(|n| n.label == "Y" && n.kind == "type_alias");
        assert!(y.is_some(), "union alias still a node");
        let y_id = &y.unwrap().id;
        assert!(
            !ex.edges
                .iter()
                .any(|e| e.relation == "type/field" && &e.source == y_id),
            "no field_type from a union alias: {:?}",
            ex.edges
        );
    }

    // ---- generic-container unwrap + bigint backstop --------------------

    #[test]
    fn generic_field_unwraps_to_inner() {
        // `xs: Array<Foo>` → field_type to the INNER Foo, not the container Array.
        let ex = extract("class Foo {}\nclass C { xs: Array<Foo>; }");
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Foo"),
            "{:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "Array"),
            "builtin container skipped"
        );
    }

    #[test]
    fn array_shorthand_field_unwraps() {
        // `xs: Foo[]` → field_type to Foo.
        let ex = extract("class Foo {}\nclass C { xs: Foo[]; }");
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Foo"),
            "{:?}",
            ex.edges
        );
    }

    #[test]
    fn map_field_unwraps_value_skips_primitive_key_and_container() {
        // `m: Map<string, Widget>` → Widget only (string primitive + Map container skipped).
        let ex = extract("class Widget {}\nclass C { m: Map<string, Widget>; }");
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Widget"),
            "{:?}",
            ex.edges
        );
        assert!(!has_sym_edge(&ex, "type/field", "C", "string"));
        assert!(!has_sym_edge(&ex, "type/field", "C", "Map"));
    }

    #[test]
    fn union_field_emits_each_member() {
        // `u: A | B` → field_type to both A and B.
        let ex = extract("class A {}\nclass B {}\nclass C { u: A | B; }");
        assert!(has_sym_edge(&ex, "type/field", "C", "A"));
        assert!(has_sym_edge(&ex, "type/field", "C", "B"));
    }

    #[test]
    fn user_generic_field_keeps_container_and_inner() {
        // `w: MyWrap<Foo>` → both MyWrap (a user type) AND Foo.
        let ex = extract("class Foo {}\nclass MyWrap {}\nclass C { w: MyWrap<Foo>; }");
        assert!(
            has_sym_edge(&ex, "type/field", "C", "MyWrap"),
            "user container kept: {:?}",
            ex.edges
        );
        assert!(has_sym_edge(&ex, "type/field", "C", "Foo"), "inner kept");
    }

    #[test]
    fn bigint_field_is_skipped() {
        // `bigint` parses as a `type_identifier` (not `predefined_type`), so a
        // name-based backstop skips it like the other primitives.
        let ex = extract("class Foo {}\nclass C { n: bigint; f: Foo; }");
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "bigint"),
            "no field_type to bigint"
        );
        assert!(has_sym_edge(&ex, "type/field", "C", "Foo"));
    }

    // ---- P4: TypeScript type/param and type/return edge tests -------------

    #[test]
    fn function_param_emits_param_type_edge() {
        // A function with a typed parameter `x: Widget` should emit a param_type edge
        let src = r#"
class Widget {}
function process(x: Widget): void {}
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "process should have param_type edge to Widget: {:?}",
            ex.edges
        );
    }

    #[test]
    fn function_return_emits_return_type_edge() {
        // A function with return type `-> Widget` should emit a return_type edge
        let src = r#"
class Widget {}
function create(): Widget { return new Widget(); }
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/return", "create", "Widget"),
            "create should have return_type edge to Widget: {:?}",
            ex.edges
        );
    }

    #[test]
    fn method_param_emits_param_type_edge() {
        // A method with typed parameter should emit param_type edge
        let src = r#"
class Widget {}
class Container {
    process(item: Widget): void {}
}
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "Container.process should have param_type edge to Widget: {:?}",
            ex.edges
        );
    }

    #[test]
    fn method_return_emits_return_type_edge() {
        // A method with return type should emit return_type edge
        let src = r#"
class Widget {}
class Factory {
    create(): Widget { return new Widget(); }
}
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/return", "create", "Widget"),
            "Factory.create should have return_type edge to Widget: {:?}",
            ex.edges
        );
    }

    #[test]
    fn generic_param_type_emits_param_type_edge() {
        // A function with generic type parameter should emit param_type edge to inner type
        let src = r#"
class Widget {}
class Processor {
    process(widgets: Widget[]): void {}
}
"#;
        let ex = extract(src);
        // Generic types should unwrap to their inner type: Widget[] → Widget
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "process should have param_type edge to Widget (unwrapped from array): {:?}",
            ex.edges
        );
    }

    #[test]
    fn generic_return_type_emits_return_type_edge() {
        // A function with generic return type should emit return_type edge to inner type
        let src = r#"
class Widget {}
function createWidget(): Widget[] { return []; }
"#;
        let ex = extract(src);
        // Generic types should unwrap to their inner type: Widget[] → Widget
        assert!(
            has_sym_edge(&ex, "type/return", "createWidget", "Widget"),
            "createWidget should have return_type edge to Widget (unwrapped from array): {:?}",
            ex.edges
        );
    }

    #[test]
    fn multiple_params_emit_multiple_param_type_edges() {
        // A function with multiple parameters should emit param_type edges for each
        let src = r#"
class Widget {}
class Factory {}
function process(widget: Widget, factory: Factory): void {}
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "process should have param_type edge to Widget: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Factory"),
            "process should have param_type edge to Factory: {:?}",
            ex.edges
        );
    }

    #[test]
    fn arrow_function_param_emits_param_type_edge() {
        // An arrow function with typed parameter should emit param_type edge
        let src = r#"
class Widget {}
const process = (widget: Widget): void => {};
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "process arrow function should have param_type edge to Widget: {:?}",
            ex.edges
        );
    }

    #[test]
    fn arrow_function_return_emits_return_type_edge() {
        // An arrow function with return type should emit return_type edge
        let src = r#"
class Widget {}
const create = (): Widget => new Widget();
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/return", "create", "Widget"),
            "create arrow function should have return_type edge to Widget: {:?}",
            ex.edges
        );
    }

    #[test]
    fn interface_param_emits_param_type_edge() {
        // A function with interface parameter should emit param_type edge
        let src = r#"
interface IShape { area(): number; }
function measure(shape: IShape): number { return shape.area(); }
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "measure", "IShape"),
            "measure should have param_type edge to IShape: {:?}",
            ex.edges
        );
    }

    #[test]
    fn interface_return_emits_return_type_edge() {
        // A function with interface return type should emit return_type edge
        let src = r#"
interface IShape { area(): number; }
function getShape(): IShape { return null as any; }
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/return", "getShape", "IShape"),
            "getShape should have return_type edge to IShape: {:?}",
            ex.edges
        );
    }

    #[test]
    fn primitive_param_type_is_filtered() {
        // A function with primitive parameter should NOT emit param_type edge (noise filtering)
        let src = r#"
function process(x: number): void {}
"#;
        let ex = extract(src);
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "number"),
            "primitive parameters should not emit param_type edge (noise): {:?}",
            ex.edges
        );
    }

    #[test]
    fn primitive_return_type_is_filtered() {
        // A function with primitive return type should NOT emit return_type edge (noise filtering)
        let src = r#"
function getValue(): number { return 42; }
"#;
        let ex = extract(src);
        assert!(
            !has_sym_edge(&ex, "type/return", "getValue", "number"),
            "primitive return types should not emit return_type edge (noise): {:?}",
            ex.edges
        );
    }

    #[test]
    fn promise_generic_unwraps_to_inner_type_return() {
        // Promise<Widget> should emit return_type edge to Widget (unwrap Promise)
        let src = r#"
class Widget {}
async function getWidget(): Promise<Widget> { return new Widget(); }
"#;
        let ex = extract(src);
        // Promise is a builtin container, should unwrap to inner type
        assert!(
            !has_sym_edge(&ex, "type/return", "getWidget", "Promise"),
            "Promise should not emit return_type edge (builtin container): {:?}",
            ex.edges
        );
        // Inner type should be emitted
        assert!(
            has_sym_edge(&ex, "type/return", "getWidget", "Widget"),
            "Promise<Widget> should emit return_type edge to Widget: {:?}",
            ex.edges
        );
    }

    #[test]
    fn promise_generic_unwraps_to_inner_type_param() {
        // Promise<Widget> parameter should emit param_type edge to Widget (unwrap Promise)
        let src = r#"
class Widget {}
async function process(promise: Promise<Widget>): Promise<void> {}
"#;
        let ex = extract(src);
        // Promise is a builtin container, should unwrap to inner type
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "Promise"),
            "Promise should not emit param_type edge (builtin container): {:?}",
            ex.edges
        );
        // Inner type should be emitted
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "Promise<Widget> should emit param_type edge to Widget: {:?}",
            ex.edges
        );
    }

    #[test]
    fn union_type_emits_edges_for_all_members() {
        // Union types should emit edges for all non-primitive members
        let src = r#"
class Widget {}
class Factory {}
function process(x: Widget | Factory): void {}
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "Union type should emit param_type edge to Widget: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Factory"),
            "Union type should emit param_type edge to Factory: {:?}",
            ex.edges
        );
    }

    #[test]
    fn optional_param_emits_param_type_edge() {
        // Optional parameters should emit param_type edges
        let src = r#"
class Widget {}
function process(widget?: Widget): void {}
"#;
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "Optional parameter should emit param_type edge to Widget: {:?}",
            ex.edges
        );
    }

    #[test]
    fn qualified_type_emits_param_type_edge() {
        // Qualified types (pkg.Widget) should emit param_type edge to last segment
        let src = r#"
namespace shapes {
    export class Widget {}
}
function process(widget: shapes.Widget): void {}
"#;
        let ex = extract(src);
        // Qualified names should emit to last segment
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "Qualified parameter type should emit param_type edge to Widget: {:?}",
            ex.edges
        );
    }

    #[test]
    fn qualified_type_emits_return_type_edge() {
        // Qualified return types should emit return_type edge to last segment
        let src = r#"
namespace shapes {
    export class Widget {}
}
function create(): shapes.Widget { return new shapes.Widget(); }
"#;
        let ex = extract(src);
        // Qualified names should emit to last segment
        assert!(
            has_sym_edge(&ex, "type/return", "create", "Widget"),
            "Qualified return type should emit return_type edge to Widget: {:?}",
            ex.edges
        );
    }

    // ---- type-parameter scopes nest (ADR-0036 R1.1) -----------------------
    //
    // A generic construct's parameters are visible to everything it encloses and
    // to nothing outside it. Entering a scope must PUSH (a method that declares
    // no parameters of its own still sees its class's) and leaving must POP (a
    // sibling declaration must not inherit them). Assignment gets both wrong,
    // and each direction costs a different way: leaking IN emits an edge to a
    // name that can never bind (or worse, mis-binds to an unrelated real type),
    // leaking OUT suppresses a real reference.

    /// Every `type/*` edge whose target names `target`, as `source -> target`.
    fn type_edges_to(ex: &Extraction, target: &str) -> Vec<String> {
        ex.edges
            .iter()
            .filter(|e| e.relation.starts_with("type/"))
            .filter(|e| matches!(&e.target, EdgeTarget::Symbol(r) if r.name == target))
            .map(|e| format!("{} {} -> {target}", e.source.0, e.relation))
            .collect()
    }

    #[test]
    fn a_method_declaring_no_type_parameters_still_sees_its_class_scope() {
        // The nesting case: `plain` declares nothing, so an implementation that
        // *replaces* the scope on entering a function reduces it to the empty set
        // and `T` leaks into both signature positions.
        let src = r#"
class Widget {}
class Holder<T> {
    plain(x: T, w: Widget): T { return x }
}
"#;
        let ex = extract(src);
        assert!(
            type_edges_to(&ex, "T").is_empty(),
            "the class's `T` is in scope inside `plain`: {:?}",
            type_edges_to(&ex, "T")
        );
        assert!(
            has_sym_edge(&ex, "type/param", "plain", "Widget"),
            "a real parameter type is still emitted: {:?}",
            ex.edges
        );
    }

    #[test]
    fn a_method_with_its_own_type_parameters_keeps_its_class_scope_too() {
        // `nested<U>` declares `U`; the scope it sees must be the UNION `{T, U}`,
        // not just its own frame.
        let src = r#"
class Widget {}
class Holder<T> {
    nested<U>(x: T, y: U, w: Widget): T { return x }
}
"#;
        let ex = extract(src);
        for tp in ["T", "U"] {
            assert!(
                type_edges_to(&ex, tp).is_empty(),
                "`{tp}` is a declared type parameter here: {:?}",
                type_edges_to(&ex, tp)
            );
        }
        assert!(
            has_sym_edge(&ex, "type/param", "nested", "Widget"),
            "a real parameter type is still emitted: {:?}",
            ex.edges
        );
    }

    #[test]
    fn a_type_parameter_scope_does_not_outlive_its_declaration() {
        // The other direction, and the one that costs a real edge: a type
        // parameter may shadow a real type (next.js has `class EventQueue<Dispatch>`
        // alongside a real `Dispatch`). Inside the class `Dispatch` is the
        // parameter; the free function after it means the real interface.
        let src = r#"
interface Dispatch {}
class EventQueue<Dispatch> {
    send(d: Dispatch): void {}
}
function forward(d: Dispatch): void {}
"#;
        let ex = extract(src);
        assert!(
            !has_sym_edge(&ex, "type/param", "send", "Dispatch"),
            "inside the class, `Dispatch` is the type parameter: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/param", "forward", "Dispatch"),
            "after the class, `Dispatch` is the real interface again: {:?}",
            ex.edges
        );
    }

    #[test]
    fn an_interface_type_parameter_is_not_a_field_type() {
        let src = r#"
class Widget {}
interface Box<T> {
    item: T
    w: Widget
}
function take(b: Box<Widget>): void {}
"#;
        let ex = extract(src);
        assert!(
            type_edges_to(&ex, "T").is_empty(),
            "the interface's own parameter is not a field type: {:?}",
            type_edges_to(&ex, "T")
        );
        assert!(
            has_sym_edge(&ex, "type/field", "Box", "Widget"),
            "a real field type is still emitted: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/param", "take", "Box"),
            "the interface scope does not outlive the interface: {:?}",
            ex.edges
        );
    }

    #[test]
    fn a_type_alias_type_parameter_is_not_a_field_type() {
        // `type X<T> = { … }` declares a scope for its members just like a class.
        let src = r#"
class Widget {}
type Pair<T> = {
    item: T
    w: Widget
}
"#;
        let ex = extract(src);
        assert!(
            type_edges_to(&ex, "T").is_empty(),
            "the alias's own parameter is not a field type: {:?}",
            type_edges_to(&ex, "T")
        );
        assert!(
            has_sym_edge(&ex, "type/field", "Pair", "Widget"),
            "a real field type is still emitted: {:?}",
            ex.edges
        );
    }

    /// **`abstract class` is a class.**
    ///
    /// tree-sitter-typescript spells it `abstract_class_declaration`, a rule of
    /// its own rather than a modifier on `class_declaration`. The walk matched
    /// only the concrete spellings, and an unmatched kind is not a partial
    /// extraction — it is no extraction: no node, no export entry, no
    /// containment, no heritage, no fields, no type-parameter scope, and methods
    /// that fall out at file level without their owner. So this asserts all six
    /// at once; any one of them alone would still pass with the class dropped, as
    /// long as something else in the file happened to supply it.
    #[test]
    fn an_abstract_class_is_extracted_like_any_other_class() {
        let src = r#"
export class Base {}
export interface Marker {}
class Widget {}
export abstract class C<T> extends Base implements Marker {
    item: T
    gear: Widget
    abstract go(): void
    m(x: T): T { return x }
    use(w: Widget): Widget { return w }
}
"#;
        let ex = extract(src);

        // 1. the node exists, and is a `class`
        let c = ex
            .nodes
            .iter()
            .find(|n| n.label == "C")
            .unwrap_or_else(|| panic!("no node for `abstract class C`: {:?}", ex.nodes));
        assert_eq!(c.kind, "class");

        // 2. it is exported (ADR-0020: no entry ⇒ a consumer's import cannot bind)
        assert!(
            ex.exports
                .iter()
                .any(|e| matches!(e, Export::Local { name } if name == "C")),
            "`export abstract class C` is missing from the export table: {:?}",
            ex.exports
        );

        // 3. its methods carry the owner qualifier in the id and the `impl` attr
        //    (the *label* stays bare per ADR-0028, so a label-only check is blind
        //    to this — which is how the gap survived the parity suite).
        let m = ex
            .nodes
            .iter()
            .find(|n| n.label == "m")
            .expect("method node `m`");
        assert!(
            m.id.0.ends_with(":C::m"),
            "method id is not owner-qualified: {}",
            m.id.0
        );
        assert_eq!(m.attrs.get("impl").map(String::as_str), Some("C"));

        // 4. the class contains its methods
        assert!(
            ex.edges
                .iter()
                .any(|e| e.relation == filigrio_core::relation::CONTAINS
                    && e.source == c.id
                    && matches!(&e.target, EdgeTarget::Node(t) if t.0.ends_with(":C::m"))),
            "the abstract class does not contain its method: {:?}",
            ex.edges
        );

        // 5. heritage and fields are sourced from it
        assert!(
            has_sym_edge(&ex, filigrio_core::relation::EXTENDS, "C", "Base"),
            "no `extends Base`: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, filigrio_core::relation::IMPLEMENTS, "C", "Marker"),
            "no `implements Marker`: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Widget"),
            "no `type/field C -> Widget` for `gear: Widget`: {:?}",
            ex.edges
        );

        // 6. the class's `<T>` scope is pushed, so R1.1 suppresses every `T`
        //    position — the field, the parameter and the return — while the real
        //    types beside them survive.
        assert!(
            type_edges_to(&ex, "T").is_empty(),
            "the abstract class's own `<T>` leaked as a type reference: {:?}",
            type_edges_to(&ex, "T")
        );
        assert!(
            has_sym_edge(&ex, "type/param", "use", "Widget")
                && has_sym_edge(&ex, "type/return", "use", "Widget"),
            "real signature types are still emitted: {:?}",
            ex.edges
        );
    }

    #[test]
    fn a_generic_arrow_function_declares_a_scope() {
        let src = r#"
class Widget {}
const identity = <T,>(x: T, w: Widget): T => x;
"#;
        let ex = extract(src);
        assert!(
            type_edges_to(&ex, "T").is_empty(),
            "an arrow function's own `<T>` is in scope for its signature: {:?}",
            type_edges_to(&ex, "T")
        );
        assert!(
            has_sym_edge(&ex, "type/param", "identity", "Widget"),
            "a real parameter type is still emitted: {:?}",
            ex.edges
        );
    }

    // ---- ADR-0036 §5: a bodiless declaration is a node ---------------------

    const ABSTRACTION: &str = r#"
export class Widget {}
export class WidgetId {}

export interface Store {
  get(id: WidgetId): Widget
  readonly size: number
}

export abstract class Base {
  abstract render(w: Widget): WidgetId
  describe(w: Widget): WidgetId {
    return this.render(w)
  }
}
"#;

    /// An **interface** method signature and an **abstract class** method are
    /// both `function` nodes carrying `attrs["abstract"]`.
    ///
    /// The kind is the load-bearing assertion. A `method_signature` /
    /// `abstract_method` kind would sit outside `filigrio_resolve::is_linkable`,
    /// so the abstraction would be a node that no reference can ever bind to —
    /// P3 with a new name. ADR-0036 §5 follows Kythe (`tag/abstract`, a node
    /// fact) rather than SCIP (six kind variants for one concept).
    #[test]
    fn a_bodiless_method_is_an_abstract_function_node() {
        let ex = extract(ABSTRACTION);
        for (label, owner, id_suffix) in [
            ("get", "Store", ":Store::get"),
            ("render", "Base", ":Base::render"),
        ] {
            let n = ex
                .nodes
                .iter()
                .find(|n| n.label == label)
                .unwrap_or_else(|| {
                    panic!(
                        "no node for the bodiless `{label}`: {:?}",
                        ex.nodes
                            .iter()
                            .map(|n| (&n.id.0, &n.kind))
                            .collect::<Vec<_>>()
                    )
                });
            assert_eq!(n.kind, "function", "`{label}` keeps the `function` kind");
            assert_eq!(
                n.attrs.get("abstract").map(String::as_str),
                Some("true"),
                "`{label}` must carry the abstract fact"
            );
            assert_eq!(n.attrs.get("impl").map(String::as_str), Some(owner));
            assert!(
                n.id.0.ends_with(id_suffix),
                "`{label}` is not owner-qualified: {}",
                n.id.0
            );
        }
    }

    /// The declaring type **contains** its declarations — an interface owns its
    /// method signatures the way a class owns its methods.
    #[test]
    fn a_declaring_type_contains_its_declarations() {
        let ex = extract(ABSTRACTION);
        for (owner, member) in [("Store", ":Store::get"), ("Base", ":Base::render")] {
            let owner_id = ex
                .nodes
                .iter()
                .find(|n| n.label == owner)
                .map(|n| n.id.clone())
                .unwrap_or_else(|| panic!("no `{owner}` node"));
            assert!(
                ex.edges.iter().any(|e| {
                    e.relation == filigrio_core::relation::CONTAINS
                        && e.source == owner_id
                        && matches!(&e.target, EdgeTarget::Node(t) if t.0.ends_with(member))
                }),
                "`{owner}` does not contain `{member}`: {:?}",
                ex.edges
                    .iter()
                    .filter(|e| e.relation == filigrio_core::relation::CONTAINS)
                    .map(|e| format!("{} -> {:?}", e.source.0, e.target))
                    .collect::<Vec<_>>()
            );
        }
    }

    /// A declaration's signature is the **contract**, so it emits `type/param`
    /// and `type/return` like any other method (§5 + R2). Without them the
    /// abstraction is a node with no incident type edges — in the graph and
    /// still invisible to the ranking §5 exists to fix.
    #[test]
    fn a_bodiless_declaration_emits_its_signature_types() {
        let ex = extract(ABSTRACTION);
        for label in ["get", "render"] {
            assert!(
                has_sym_edge(&ex, "type/param", label, "WidgetId")
                    || has_sym_edge(&ex, "type/param", label, "Widget"),
                "`{label}` emits no `type/param`: {:?}",
                ex.edges
            );
        }
        assert!(
            has_sym_edge(&ex, "type/return", "get", "Widget"),
            "the interface method's return type is missing: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/return", "render", "WidgetId"),
            "the abstract method's return type is missing: {:?}",
            ex.edges
        );
    }

    /// The declaring **type** carries the fact too: an `interface` and an
    /// `abstract class` are abstract; a plain `class` is not.
    #[test]
    fn an_interface_and_an_abstract_class_are_abstract_types() {
        let ex = extract(ABSTRACTION);
        let ty = |l: &str| {
            ex.nodes
                .iter()
                .find(|n| n.label == l && n.kind != "function")
                .unwrap_or_else(|| panic!("no `{l}` type node"))
        };
        assert_eq!(
            ty("Store").attrs.get("abstract").map(String::as_str),
            Some("true"),
            "an interface is the pure-abstract type"
        );
        assert_eq!(
            ty("Base").attrs.get("abstract").map(String::as_str),
            Some("true"),
            "`abstract class Base` is abstract"
        );
        assert!(
            !ty("Widget").attrs.contains_key("abstract"),
            "a concrete class carries no `abstract` key (absent, never `false`)"
        );
    }

    /// A method **with a body** is never abstract — including the abstract
    /// class's own default method, which is the half that keeps the fact
    /// meaningful.
    #[test]
    fn a_method_with_a_body_is_not_abstract() {
        let ex = extract(ABSTRACTION);
        let marked: Vec<&str> = ex
            .nodes
            .iter()
            .filter(|n| n.kind == "function" && n.attrs.contains_key("abstract"))
            .map(|n| n.id.0.as_str())
            .collect();
        assert_eq!(
            marked,
            vec!["fn:src/demo.ts:Store::get", "fn:src/demo.ts:Base::render"],
            "only the two bodiless declarations are abstract"
        );
    }

    /// A **class-body** `method_signature` is a TypeScript *overload* signature,
    /// not an abstraction: the implementation with a body is right below it. It
    /// gets no node, so the method is not duplicated.
    ///
    /// tree-sitter spells the overload and the interface member with the *same*
    /// rule, so this is what the parent-kind test in the walk buys — and the one
    /// case where reading the kind alone would have been wrong.
    #[test]
    fn a_class_overload_signature_is_not_a_declaration_node() {
        let ex = extract(
            r#"
export class Api {
  send(x: string): string
  send(x: number): number
  send(x: any): any { return x }
}
"#,
        );
        let sends: Vec<&str> = ex
            .nodes
            .iter()
            .filter(|n| n.label == "send")
            .map(|n| n.id.0.as_str())
            .collect();
        assert_eq!(
            sends,
            vec!["fn:src/demo.ts:Api::send"],
            "an overload set is one method, not three nodes"
        );
        assert!(
            ex.nodes.iter().all(|n| !n.attrs.contains_key("abstract")),
            "an overload signature is not abstract: {:?}",
            ex.nodes
                .iter()
                .map(|n| (&n.id.0, &n.attrs))
                .collect::<Vec<_>>()
        );
    }
}
