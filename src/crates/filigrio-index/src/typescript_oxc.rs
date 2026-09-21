//! `TypeScriptOxcExtractor` — the **oxc**-backed TS/JS frontend (ADR-0040).
//!
//! Feature-gated (`ts-oxc`, off by default). This is the real oxc backend that
//! subsumes the `oxc_spike` feasibility probe: it parses with oxc 0.140
//! (`Allocator` + `Parser` + `SemanticBuilder::new().with_build_nodes(true)` — the
//! flag is MANDATORY or `semantic.nodes()` is silently empty), walks the flattened
//! `semantic.nodes()` matching `AstKind`, and emits into the **shared** [`Driver`]
//! (ADR-0040 §3) — the same accumulator the tree-sitter backends use.
//!
//! **Parity, not enhancement (ADR-0040 §4).** This step targets byte-for-byte
//! behavioural parity with the tree-sitter `TypeScriptExtractor`: the same node
//! labels/kinds, the same edges, and — crucially — the same *unresolved-`Symbol` +
//! receiver-hint* call model (bare-name callees; `this.m()`→enclosing class,
//! `Class.m()`→that class, and a small local dataflow for `x.m()` from `new C()` /
//! `: C` / a typed param). The oxc within-file **binding resolution** the spike
//! proved is deliberately NOT layered on here; that is a later step which will
//! update the specific call tests.
//!
//! Because the shared `Driver::add_def`/`add_variant` take a `tree_sitter::Node`
//! (for span + collision text), this file drives them through the `ts-oxc`-gated
//! [`Driver::add_def_raw`]/[`Driver::add_variant_raw`] shims, which take the
//! precomputed `(start_line, end_line, decl)` instead — identical id-minting,
//! `impl` attrs and `contains`/`has_variant` wiring, no tree-sitter node required.
#![cfg(feature = "ts-oxc")]

use crate::driver::Driver;
use filigrio_core::relation::{EXTENDS, IMPLEMENTS};
use filigrio_core::{Artifact, Export, Extraction, Extractor, NodeId, Result, TargetRef};
use std::collections::{HashMap, HashSet};

use oxc::allocator::Allocator;
use oxc::ast::ast::{
    BindingPattern, Class, ClassElement, Declaration, ExportAllDeclaration, ExportNamedDeclaration,
    Expression, Function, FunctionType, ImportDeclaration, ImportDeclarationSpecifier,
    MethodDefinition, MethodDefinitionType, ModuleExportName, ObjectProperty, PropertyKey,
    TSEnumDeclaration, TSEnumMemberName, TSInterfaceDeclaration, TSMethodSignature, TSSignature,
    TSType, TSTypeAliasDeclaration, TSTypeName, VariableDeclarator,
};
use oxc::ast::AstKind;
use oxc::parser::{Parser, ParserReturn};
use oxc::semantic::{Semantic, SemanticBuilder};
use oxc::span::{GetSpan, SourceType, Span};

// Named only by the test module (via its `use super::*`).
#[cfg(test)]
use filigrio_core::{EdgeTarget, Node};

/// Type names that are never graph nodes and so must not be `type/field` targets.
/// Duplicated locally from `typescript.rs` (ADR-0040 forbids refactoring that file):
/// the scalar `bigint` plus builtin generic containers, whose *inner* type args
/// carry the real dependency. In the oxc AST the scalar keywords (`number`,
/// `string`, `bigint`, …) parse as dedicated `TSType` keyword variants and are
/// dropped structurally; this name backstop covers the container generics.
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

pub struct TypeScriptOxcExtractor;

impl TypeScriptOxcExtractor {
    pub fn new() -> Self {
        TypeScriptOxcExtractor
    }
}

impl Default for TypeScriptOxcExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for TypeScriptOxcExtractor {
    fn handles(&self, artifact: &Artifact) -> bool {
        matches!(
            artifact.language.as_deref(),
            Some("typescript") | Some("javascript")
        )
    }

    fn extract(&self, artifact: &Artifact, bytes: &[u8]) -> Result<Extraction> {
        // oxc's `Parser` needs a `&str`, but a repo at scale contains the odd
        // non-UTF-8 file (test fixtures, binary-ish snapshots). A strict
        // `from_utf8` there aborts the WHOLE build; the tree-sitter frontend
        // parses raw bytes and never rejects a file for this. Convert lossily so
        // one bad byte can't sink the run. Every downstream span is computed
        // against `source_text` below, so U+FFFD substitution stays
        // self-consistent (byte offsets index into this same buffer).
        let source_text = String::from_utf8_lossy(bytes);

        // --- parse (arena stays local to this fn) -----------------------------
        let allocator = Allocator::default();
        let source_type = SourceType::from_path(&artifact.path).unwrap_or_default();
        let ParserReturn { program, .. } =
            Parser::new(&allocator, source_text.as_ref(), source_type).parse();

        // `with_build_nodes(true)` is REQUIRED — the default "Ancestry" mode leaves
        // `semantic.nodes()` empty (see oxc_spike). We do not consume the binding
        // model here (parity step); it comes online in the ADR-0040 §4 follow-up.
        let semantic = SemanticBuilder::new()
            .with_build_nodes(true)
            .build(&program)
            .semantic;

        let mut ctx = Ctx::new(artifact, source_text.as_bytes(), source_text.as_ref());
        ctx.run(&semantic);
        Ok(ctx.d.finish())
        // `allocator` (and everything borrowing it) drops here; `ctx.d.finish()`
        // returns an OWNED `Extraction`.
    }
}

/// The oxc TS walk state. Everything oxc-borrowed is resolved to owned data during
/// the walk; nothing arena-lifetimed escapes into `Ctx`.
struct Ctx<'a> {
    d: Driver<'a, String>,
    src: &'a str,
    /// Byte offset of the start of each line (1-based line = `partition_point`).
    line_starts: Vec<u32>,
    /// def-node span `(start,end)` → minted id, for the def kinds that can enclose
    /// a `calls` edge (function / method / arrow-or-fn-expr const). Anonymous
    /// callbacks are absent, so a call inside one attributes to the nearest *named*
    /// enclosing def — matching the tree-sitter `current_fn` threading.
    fn_def_ids: HashMap<(u32, u32), NodeId>,
    /// class-node span `(start,end)` → (minted id, class name), for method
    /// containment and `this`-receiver typing.
    class_ids: HashMap<(u32, u32), (NodeId, String)>,
    /// interface-node span `(start,end)` → (minted id, interface name). Kept
    /// apart from `class_ids` on purpose: an interface owns its **declarations**
    /// (ADR-0036 §5) but is never a `this` receiver, so folding the two maps
    /// together would put interfaces into receiver typing as a side effect.
    iface_ids: HashMap<(u32, u32), (NodeId, String)>,
    /// `(enclosing-fn span, var name)` → inferred receiver type. The flat analogue
    /// of the tree-sitter lexical scope stack (nested fns are searched outward).
    var_types: HashMap<(u32, u32, String), String>,
}

impl<'a> Ctx<'a> {
    fn new(artifact: &Artifact, src_bytes: &'a [u8], src: &'a str) -> Self {
        let mut line_starts = vec![0u32];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        Ctx {
            d: Driver::new(artifact, src_bytes, "typescript"),
            src,
            line_starts,
            fn_def_ids: HashMap::new(),
            class_ids: HashMap::new(),
            iface_ids: HashMap::new(),
            var_types: HashMap::new(),
        }
    }

    /// 1-based line number of a byte offset.
    fn line(&self, offset: u32) -> u32 {
        self.line_starts.partition_point(|&s| s <= offset) as u32
    }

    /// `(start_line, end_line, decl-text)` for a span — the args the raw Driver
    /// shims want. `decl` is the WHOLE-node source slice (collision-hash input).
    /// Used exactly for the kinds where tree-sitter's `decl_text` also hashes the
    /// whole node because the grammar exposes no `body` field there: type aliases
    /// (the RHS is the `value` field), enum members, bodyless (declare/overload)
    /// functions, and declarator-bound arrows. Kinds with a grammar `body` field
    /// (class/function/method/interface/enum) go through [`Ctx::loc_to_body`].
    fn loc(&self, span: Span) -> (u32, u32, String) {
        let decl = self
            .src
            .get(span.start as usize..span.end as usize)
            .unwrap_or("")
            .trim()
            .to_string();
        (self.line(span.start), self.line(span.end), decl)
    }

    /// Like [`Ctx::loc`] but slices `decl` as the signature UP TO the body start
    /// (`full.start .. body_start`) — ADR-0028 id stability: an overloaded /
    /// same-named def keeps a body-independent collision hash, so its id survives
    /// body edits. `start`/`end` lines still span the whole node.
    fn loc_to_body(&self, full: Span, body_start: u32) -> (u32, u32, String) {
        let decl = self
            .src
            .get(full.start as usize..body_start as usize)
            .unwrap_or("")
            .trim()
            .to_string();
        (self.line(full.start), self.line(full.end), decl)
    }

    // ---- the walk ----------------------------------------------------------

    fn run(&mut self, semantic: &Semantic<'a>) {
        // Pass 1: definitions, heritage/field/variant edges, imports, exports, and
        // the receiver-type environment. `semantic.nodes()` is pre-order, so a
        // parent def (class) is created before the children that reference it
        // (methods), and a binding is typed before a later sibling call reads it.
        for node in semantic.nodes().iter() {
            match node.kind() {
                AstKind::Class(class) => self.def_class(class, semantic, node.id()),
                AstKind::TSInterfaceDeclaration(iface) => {
                    self.def_interface(iface, semantic, node.id())
                }
                AstKind::TSTypeAliasDeclaration(alias) => {
                    self.def_type_alias(alias, semantic, node.id())
                }
                AstKind::TSEnumDeclaration(en) => self.def_enum(en),
                AstKind::Function(func) if func.r#type == FunctionType::FunctionDeclaration => {
                    self.def_function(func, semantic, node.id());
                }
                AstKind::MethodDefinition(m) => self.def_method(m, semantic, node.id()),
                // An interface's `m(): void` — a bodiless declaration, ADR-0036
                // §5. Handled here rather than inside `def_interface` so it gets
                // the same ancestor-driven type-parameter scope every other def
                // gets, and so its params reach `bind_param` by the normal route.
                AstKind::TSMethodSignature(sig) => {
                    self.def_method_signature(sig, semantic, node.id())
                }
                // Object-literal method shorthand `{ greet() {} }` — a FILE-contained
                // function (parity: tree-sitter emits one). ONLY when `method == true`;
                // an arrow / function-expression VALUE property (`{ fn: () => {} }`,
                // `{ fn: function(){} }`) has `method == false` and stays unemitted.
                AstKind::ObjectProperty(op) if op.method => self.def_object_method(op),
                AstKind::VariableDeclarator(vd) => self.walk_declarator(vd, semantic, node.id()),
                AstKind::FormalParameter(p) => self.bind_param(p, semantic, node.id()),
                AstKind::ImportDeclaration(imp) => self.walk_import(imp),
                AstKind::ExportNamedDeclaration(e) => self.walk_export_named(e),
                AstKind::ExportAllDeclaration(e) => self.walk_export_all(e),
                _ => {}
            }
        }

        // Pass 2: `calls` edges — needs the full def + receiver-type environment.
        for node in semantic.nodes().iter() {
            if let AstKind::CallExpression(call) = node.kind() {
                self.emit_call(&call.callee, semantic, node.id());
            }
        }
    }

    // ---- definitions -------------------------------------------------------

    fn def_class(&mut self, class: &Class<'a>, sem: &Semantic<'a>, node_id: oxc::semantic::NodeId) {
        let Some(bi) = &class.id else { return };
        let name = bi.name.to_string();
        // decl = `class Name … ` up to the body `{` (always present).
        let (s, e, decl) = self.loc_to_body(class.span, class.body.span.start);
        let file_id = self.d.file_id.clone();
        let id = self
            .d
            .add_def_raw("type", "class", &name, s, e, decl, &file_id, None);
        // `abstract class C` — the declaring type is a declaration too
        // (ADR-0036 §5). The kind stays `class`.
        if class.r#abstract {
            self.d.mark_abstract(&id);
        }

        // Register as local type for noise filtering exceptions (ADR-0036 R1.1)
        self.d.register_local_type(&name);

        // `class Holder<T>`: its own scope plus any enclosing one, for the field
        // types emitted below.
        self.push_scope_at(sem, node_id, class.type_parameters.as_deref());

        self.class_ids
            .insert((class.span.start, class.span.end), (id.clone(), name));

        // heritage: `extends Base`, `implements I, J`
        if let Some(sup) = &class.super_class {
            if let Some(t) = expr_type_name(sup) {
                self.d.emit_heritage(id.clone(), EXTENDS, &t);
            }
        }
        for imp in &class.implements {
            if let Some(t) = ts_type_name_ident(&imp.expression) {
                self.d.emit_heritage(id.clone(), IMPLEMENTS, &t);
            }
        }
        // fields (PropertyDefinition members)
        for el in &class.body.body {
            if let ClassElement::PropertyDefinition(p) = el {
                if let (Some(fname), Some(ann)) = (prop_key_name(&p.key), &p.type_annotation) {
                    self.emit_fields(&id, &fname, &ann.type_annotation);
                }
            }
        }
        self.d.pop_type_parameters();
    }

    fn def_interface(
        &mut self,
        iface: &TSInterfaceDeclaration<'a>,
        sem: &Semantic<'a>,
        node_id: oxc::semantic::NodeId,
    ) {
        let name = iface.id.name.to_string();
        // decl = `interface Name …` up to the body `{` (tree-sitter's
        // `interface_declaration` has a `body` field, so parity is to-body).
        let (s, e, decl) = self.loc_to_body(iface.span, iface.body.span.start);
        let file_id = self.d.file_id.clone();
        let id = self
            .d
            .add_def_raw("type", "interface", &name, s, e, decl, &file_id, None);
        // An `interface` is the pure-abstract type: every member is a
        // declaration (ADR-0036 §5).
        self.d.mark_abstract(&id);
        self.iface_ids.insert(
            (iface.span.start, iface.span.end),
            (id.clone(), name.clone()),
        );

        // Register as local type for noise filtering exceptions (ADR-0036 R1.1)
        self.d.register_local_type(&name);

        // `interface Box<T>`: its own scope plus any enclosing one.
        self.push_scope_at(sem, node_id, iface.type_parameters.as_deref());

        for h in &iface.extends {
            if let Some(t) = expr_type_name(&h.expression) {
                self.d.emit_heritage(id.clone(), EXTENDS, &t);
            }
        }
        for sig in &iface.body.body {
            if let TSSignature::TSPropertySignature(p) = sig {
                if let (Some(fname), Some(ann)) = (prop_key_name(&p.key), &p.type_annotation) {
                    self.emit_fields(&id, &fname, &ann.type_annotation);
                }
            }
        }
        self.d.pop_type_parameters();
    }

    fn def_type_alias(
        &mut self,
        alias: &TSTypeAliasDeclaration<'a>,
        sem: &Semantic<'a>,
        node_id: oxc::semantic::NodeId,
    ) {
        let name = alias.id.name.to_string();
        let (s, e, decl) = self.loc(alias.span);
        let file_id = self.d.file_id.clone();
        let id = self
            .d
            .add_def_raw("type", "type_alias", &name, s, e, decl, &file_id, None);

        // Register as local type for noise filtering exceptions (ADR-0036 R1.1)
        self.d.register_local_type(&name);

        // `type Pair<T> = { a: T }` declares a scope for its members.
        self.push_scope_at(sem, node_id, alias.type_parameters.as_deref());

        // Only an object-type RHS (`type X = { .. }`) is struct-like; union / name /
        // generic aliases just create the node.
        if let TSType::TSTypeLiteral(lit) = &alias.type_annotation {
            for sig in &lit.members {
                if let TSSignature::TSPropertySignature(p) = sig {
                    if let (Some(fname), Some(ann)) = (prop_key_name(&p.key), &p.type_annotation) {
                        self.emit_fields(&id, &fname, &ann.type_annotation);
                    }
                }
            }
        }
        self.d.pop_type_parameters();
    }

    fn def_enum(&mut self, en: &TSEnumDeclaration<'a>) {
        let name = en.id.name.to_string();
        // decl = `enum Name` up to the body `{` (tree-sitter's `enum_declaration`
        // has a `body` field, so parity is to-body).
        let (s, e, decl) = self.loc_to_body(en.span, en.body.span.start);
        let file_id = self.d.file_id.clone();
        let id = self
            .d
            .add_def_raw("type", "enum", &name, s, e, decl, &file_id, None);

        for m in &en.body.members {
            if let Some(vn) = enum_member_name(&m.id) {
                let (vs, ve, vdecl) = self.loc(m.span);
                self.d.add_variant_raw(&id, &name, &vn, vs, ve, vdecl);
            }
        }
    }

    fn def_function(
        &mut self,
        func: &Function<'a>,
        sem: &Semantic<'a>,
        node_id: oxc::semantic::NodeId,
    ) {
        let Some(bi) = &func.id else { return };
        let name = bi.name.to_string();
        // decl = signature up to the body `{`; a bodyless decl (`declare` /
        // overload signature) has no body span, so slice the whole node.
        let (s, e, decl) = match &func.body {
            Some(body) => self.loc_to_body(func.span, body.span.start),
            None => self.loc(func.span),
        };
        let (container, owner) = self.container_owner(sem, node_id);
        let id = self.d.add_def_raw(
            "fn",
            "function",
            &name,
            s,
            e,
            decl,
            &container,
            owner.as_deref(),
        );
        self.fn_def_ids
            .insert((func.span.start, func.span.end), id.clone());

        // `function f<T>()` — its own scope plus any enclosing one (a nested
        // function inside a generic one still sees the outer parameters).
        self.push_scope_at(sem, node_id, func.type_parameters.as_deref());

        // Emit return_type edge if return type annotation exists
        if let Some(return_ann) = &func.return_type {
            let mut targets = Vec::new();
            collect_type_targets(
                &return_ann.type_annotation,
                &mut targets,
                &self.d.local_types,
            );
            for target in targets {
                let normalized_name = base_type_name(&target);
                let _ = self.d.emit_return_type(id.clone(), normalized_name);
            }
        }
        self.d.pop_type_parameters();
    }

    fn def_method(
        &mut self,
        m: &MethodDefinition<'a>,
        sem: &Semantic<'a>,
        node_id: oxc::semantic::NodeId,
    ) {
        let Some(name) = prop_key_name(&m.key) else {
            return;
        };
        // decl = signature up to the method value's body `{`; an abstract / overload
        // method has no body, so slice the whole node.
        let (s, e, decl) = match &m.value.body {
            Some(body) => self.loc_to_body(m.span, body.span.start),
            None => self.loc(m.span),
        };
        // A method is contained by (and impl-tagged with) its enclosing class.
        let (container, owner) = self.container_owner(sem, node_id);
        let id = self.d.add_def_raw(
            "fn",
            "function",
            &name,
            s,
            e,
            decl,
            &container,
            owner.as_deref(),
        );
        // `abstract go(): void` — abstractness is read off the method's *type*,
        // not off "has no body": a class **overload** signature is bodiless too
        // and is not an abstraction (ADR-0036 §5). oxc mints a node for the
        // overload signature as well; that divergence from tree-sitter is
        // registered in `tests/ts_frontend_parity.rs`, not papered over here.
        if m.r#type == MethodDefinitionType::TSAbstractMethodDefinition {
            self.d.mark_abstract(&id);
        }
        self.fn_def_ids
            .insert((m.span.start, m.span.end), id.clone());

        // A method's own `<U>` PLUS its class's `<T>` — the enclosing class is an
        // ancestor of this node, so `push_scope_at` picks it up.
        self.push_scope_at(sem, node_id, m.value.type_parameters.as_deref());

        // Emit return_type edge if return type annotation exists
        if let Some(return_ann) = &m.value.return_type {
            let mut targets = Vec::new();
            collect_type_targets(
                &return_ann.type_annotation,
                &mut targets,
                &self.d.local_types,
            );
            for target in targets {
                let normalized_name = base_type_name(&target);
                let _ = self.d.emit_return_type(id.clone(), normalized_name);
            }
        }
        self.d.pop_type_parameters();
    }

    /// An **interface** method signature as a node (ADR-0036 §5).
    ///
    /// Kind `function`, marked `abstract`, owner-qualified by and contained by
    /// the interface — the same shape `def_method` gives a class method, because
    /// a method is a method whether or not it has a body. Its span joins
    /// `fn_def_ids`, which is what lets `bind_param` attribute the signature's
    /// `type/param` edges to it without a second parameter walk.
    ///
    /// Restricted to interface members: the same AST node also spells a member of
    /// an inline object type (`type X = { m(): void }`), whose declaring node is a
    /// type alias, and the tree-sitter frontend draws the line in the same place.
    fn def_method_signature(
        &mut self,
        sig: &TSMethodSignature<'a>,
        sem: &Semantic<'a>,
        node_id: oxc::semantic::NodeId,
    ) {
        let Some(name) = prop_key_name(&sig.key) else {
            return;
        };
        let Some((container, owner)) = self.enclosing_interface(sem, node_id) else {
            return;
        };
        let (s, e, decl) = self.loc(sig.span);
        let id = self.d.add_def_raw(
            "fn",
            "function",
            &name,
            s,
            e,
            decl,
            &container,
            Some(&owner),
        );
        self.d.mark_abstract(&id);
        self.fn_def_ids
            .insert((sig.span.start, sig.span.end), id.clone());

        self.push_scope_at(sem, node_id, sig.type_parameters.as_deref());
        if let Some(return_ann) = &sig.return_type {
            let mut targets = Vec::new();
            collect_type_targets(
                &return_ann.type_annotation,
                &mut targets,
                &self.d.local_types,
            );
            for target in targets {
                let normalized_name = base_type_name(&target);
                let _ = self.d.emit_return_type(id.clone(), normalized_name);
            }
        }
        self.d.pop_type_parameters();
    }

    /// An object-literal method shorthand `{ greet() {} }`: a FILE-contained
    /// `function` def named by the property key. Its span goes into `fn_def_ids`
    /// (like [`Ctx::def_method`]) so calls in its body attribute to it.
    fn def_object_method(&mut self, op: &ObjectProperty<'a>) {
        let Some(name) = prop_key_name(&op.key) else {
            return;
        };
        let (s, e, decl) = self.loc(op.span);
        let file_id = self.d.file_id.clone();
        let id = self
            .d
            .add_def_raw("fn", "function", &name, s, e, decl, &file_id, None);
        self.fn_def_ids.insert((op.span.start, op.span.end), id);
    }

    /// A `const`/`let` declarator: an arrow / function-expression value is a
    /// **function def** named by the binding; anything else types the binding for
    /// receiver inference (`const c = new Circle()` / `const x: Circle = ..`).
    fn walk_declarator(
        &mut self,
        vd: &VariableDeclarator<'a>,
        sem: &Semantic<'a>,
        node_id: oxc::semantic::NodeId,
    ) {
        let is_fn = matches!(
            &vd.init,
            Some(Expression::ArrowFunctionExpression(_)) | Some(Expression::FunctionExpression(_))
        );
        let Some(name) = binding_name(&vd.id) else {
            return;
        };
        if is_fn {
            let (s, e, decl) = self.loc(vd.span);
            let (container, owner) = self.container_owner(sem, node_id);
            let id = self.d.add_def_raw(
                "fn",
                "function",
                &name,
                s,
                e,
                decl,
                &container,
                owner.as_deref(),
            );
            self.fn_def_ids
                .insert((vd.span.start, vd.span.end), id.clone());

            // Emit return_type edge for arrow functions and function expressions
            match &vd.init {
                Some(Expression::ArrowFunctionExpression(arrow)) => {
                    // `const f = <T,>(…): T => …` — its own scope plus any enclosing
                    // one (an arrow declared inside a generic class or function).
                    self.push_scope_at(sem, node_id, arrow.type_parameters.as_deref());

                    if let Some(return_ann) = &arrow.return_type {
                        let mut targets = Vec::new();
                        collect_type_targets(
                            &return_ann.type_annotation,
                            &mut targets,
                            &self.d.local_types,
                        );
                        for target in targets {
                            let normalized_name = base_type_name(&target);
                            let _ = self.d.emit_return_type(id.clone(), normalized_name);
                        }
                    }
                    self.d.pop_type_parameters();
                }
                Some(Expression::FunctionExpression(func_expr)) => {
                    // `const f = function <T>(…): T {}` — same as the arrow arm.
                    self.push_scope_at(sem, node_id, func_expr.type_parameters.as_deref());

                    if let Some(return_ann) = &func_expr.return_type {
                        let mut targets = Vec::new();
                        collect_type_targets(
                            &return_ann.type_annotation,
                            &mut targets,
                            &self.d.local_types,
                        );
                        for target in targets {
                            let normalized_name = base_type_name(&target);
                            let _ = self.d.emit_return_type(id.clone(), normalized_name);
                        }
                    }
                    self.d.pop_type_parameters();
                }
                _ => {}
            }
            return;
        }
        // value binding — infer type, record into the enclosing fn's environment.
        if let Some(ty) = declarator_type(vd) {
            if let Some(key) = self.enclosing_fn_span(sem, node_id) {
                self.var_types.insert((key.0, key.1, name), ty);
            }
        }
    }

    /// Bind a typed parameter (`w: Widget`) into its function's environment.
    fn bind_param(
        &mut self,
        p: &oxc::ast::ast::FormalParameter<'a>,
        sem: &Semantic<'a>,
        node_id: oxc::semantic::NodeId,
    ) {
        let (Some(name), Some(ann)) = (binding_name(&p.pattern), &p.type_annotation) else {
            return;
        };

        // Store type for receiver inference (existing behavior)
        if let Some(ty) = ts_type_base_name(&ann.type_annotation) {
            if let Some(key) = self.enclosing_fn_span(sem, node_id) {
                self.var_types.insert((key.0, key.1, name), ty);
            }
        }

        // Emit param_type edge for all param types. The parameter sits INSIDE its
        // function, so the ancestor chain already carries every declaring scope
        // (the function's own `<T>` and any enclosing class's).
        if let Some(fn_id) = self.enclosing_fn_id(sem, node_id) {
            self.push_scope_at(sem, node_id, None);
            let mut targets = Vec::new();
            collect_type_targets(&ann.type_annotation, &mut targets, &self.d.local_types);
            for target in targets {
                let normalized_name = base_type_name(&target);
                let _ = self.d.emit_param_type(fn_id.clone(), normalized_name);
            }
            self.d.pop_type_parameters();
        }
    }

    // ---- calls -------------------------------------------------------------

    fn emit_call(
        &mut self,
        callee: &Expression<'a>,
        sem: &Semantic<'a>,
        node_id: oxc::semantic::NodeId,
    ) {
        // Source = the enclosing named def; a top-level call has none → no edge.
        let Some(src_id) = self.enclosing_fn_id(sem, node_id) else {
            return;
        };
        let (name, hint) = match callee {
            Expression::Identifier(idref) => (idref.name.to_string(), None),
            Expression::StaticMemberExpression(m) => {
                let method = m.property.name.to_string();
                let hint = self.receiver_hint(&m.object, sem, node_id);
                (method, hint)
            }
            _ => return,
        };
        let mut tref = TargetRef::new(name);
        if let Some(ty) = hint {
            tref.hints.insert("type".into(), ty);
        }
        self.d.emit_call(src_id, tref);
    }

    /// The receiver type of a `member.method()`, when statically knowable:
    /// `this` → the enclosing class; an UpperCamel name → that class; a typed local
    /// → its inferred type. Otherwise unknown (the call resolves by bare name).
    fn receiver_hint(
        &self,
        object: &Expression<'a>,
        sem: &Semantic<'a>,
        call_id: oxc::semantic::NodeId,
    ) -> Option<String> {
        match object {
            Expression::ThisExpression(_) => self.enclosing_class_name(sem, call_id),
            Expression::Identifier(idref) => {
                let name = idref.name.to_string();
                if name.chars().next().is_some_and(|c| c.is_uppercase()) {
                    Some(name)
                } else {
                    self.lookup_var(&name, sem, call_id)
                }
            }
            _ => None,
        }
    }

    // ---- imports & exports -------------------------------------------------

    fn walk_import(&mut self, imp: &ImportDeclaration<'a>) {
        let specifier = imp.source.value.as_str().to_string();
        // Bare `import 'side-effect'` (specifiers None) binds no name.
        let Some(specs) = &imp.specifiers else { return };
        for spec in specs {
            let (bound, imported) = match spec {
                ImportDeclarationSpecifier::ImportSpecifier(s) => {
                    (s.local.name.to_string(), module_export_name(&s.imported))
                }
                ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                    let b = s.local.name.to_string();
                    (b.clone(), b)
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                    let b = s.local.name.to_string();
                    (b.clone(), b)
                }
            };
            let alias = (bound != imported).then(|| bound.clone());
            let name = alias.clone().unwrap_or_else(|| imported.clone());
            let mut tref = TargetRef::new(&name);
            tref.hints.insert("specifier".into(), specifier.clone());
            if alias.is_some() {
                tref.hints.insert("imported".into(), imported);
            }
            self.d.emit_import(tref);
        }
    }

    fn walk_export_named(&mut self, e: &ExportNamedDeclaration<'a>) {
        if let Some(src) = &e.source {
            // Re-export: `export { imported as name } from "specifier"`.
            let specifier = src.value.as_str().to_string();
            for spec in &e.specifiers {
                let imported = module_export_name(&spec.local);
                let name = module_export_name(&spec.exported);
                self.d.exports.push(Export::ReExport {
                    name,
                    specifier: specifier.clone(),
                    imported,
                });
            }
        } else if let Some(decl) = &e.declaration {
            // Local export via a declaration: `export function/class/const …`.
            for name in decl_names(decl) {
                self.d.exports.push(Export::Local { name });
            }
        } else {
            // `export { a, b }` — un-aliased names only (aliased local rename is
            // deferred, matching the tree-sitter backend).
            for spec in &e.specifiers {
                let local = module_export_name(&spec.local);
                let exported = module_export_name(&spec.exported);
                if local == exported {
                    self.d.exports.push(Export::Local { name: exported });
                }
            }
        }
    }

    fn walk_export_all(&mut self, e: &ExportAllDeclaration<'a>) {
        // `export * from spec` (skip the rarer `export * as ns`).
        if e.exported.is_none() {
            self.d.exports.push(Export::Star {
                specifier: e.source.value.as_str().to_string(),
            });
        }
    }

    // ---- field-type unwrap (mirrors typescript.rs `collect_field_targets`) --

    fn emit_fields(&mut self, owner: &NodeId, fname: &str, ty: &TSType<'a>) {
        let mut targets = Vec::new();
        collect_field_targets(ty, &mut targets, &self.d.local_types);
        for t in targets {
            let _ = self.d.emit_field_type(owner.clone(), fname, &t, None);
        }
    }

    // ---- ancestry helpers (flat-walk context) ------------------------------

    // ---- type-parameter scopes (ADR-0036 R1.1) -----------------------------

    /// The type parameters visible at `id`: the union declared by every enclosing
    /// generic construct.
    ///
    /// The tree-sitter backends get this from a push/pop stack because they
    /// recurse — there is a scope *exit* to pop on. This walk is flat
    /// (`semantic.nodes()` in pre-order, no exit event), so the enclosing scope is
    /// re-derived from the ancestor chain instead. `ancestors` starts at the
    /// PARENT, so a node's own parameters are added by the caller.
    fn enclosing_type_params(
        &self,
        sem: &Semantic<'a>,
        id: oxc::semantic::NodeId,
    ) -> HashSet<String> {
        let mut out = HashSet::new();
        for anc in sem.nodes().ancestors(id) {
            let params = match anc.kind() {
                AstKind::Class(c) => &c.type_parameters,
                AstKind::TSInterfaceDeclaration(i) => &i.type_parameters,
                AstKind::TSTypeAliasDeclaration(t) => &t.type_parameters,
                // A method's parameters live on its `value` Function, which is an
                // ancestor of everything inside the method.
                AstKind::Function(f) => &f.type_parameters,
                AstKind::ArrowFunctionExpression(a) => &a.type_parameters,
                _ => continue,
            };
            if let Some(tp) = params {
                out.extend(tp.params.iter().map(|p| p.name.name.to_string()));
            }
        }
        out
    }

    /// Enter the scope of a def at `id` that itself declares `own` — the enclosing
    /// scopes plus its own. Every caller must pair this with
    /// [`Driver::pop_type_parameters`]; a def emits everything it needs
    /// synchronously, so the pair is always within one function here.
    fn push_scope_at(
        &mut self,
        sem: &Semantic<'a>,
        id: oxc::semantic::NodeId,
        own: Option<&oxc::ast::ast::TSTypeParameterDeclaration<'a>>,
    ) {
        let mut params = self.enclosing_type_params(sem, id);
        if let Some(tp) = own {
            params.extend(tp.params.iter().map(|p| p.name.name.to_string()));
        }
        self.d.push_type_parameters(params);
    }

    /// Nearest ancestor class `(id, name)`, else `None`.
    fn enclosing_class(
        &self,
        sem: &Semantic<'a>,
        id: oxc::semantic::NodeId,
    ) -> Option<(NodeId, String)> {
        for anc in sem.nodes().ancestors(id) {
            if let AstKind::Class(_) = anc.kind() {
                let sp = anc.span();
                if let Some(v) = self.class_ids.get(&(sp.start, sp.end)) {
                    return Some(v.clone());
                }
            }
        }
        None
    }

    /// Nearest ancestor `interface` `(id, name)`, else `None` — the declaring
    /// type of a `TSMethodSignature` (ADR-0036 §5). Deliberately separate from
    /// [`Ctx::enclosing_class`]: an interface owns declarations but is never a
    /// `this` receiver.
    fn enclosing_interface(
        &self,
        sem: &Semantic<'a>,
        id: oxc::semantic::NodeId,
    ) -> Option<(NodeId, String)> {
        for anc in sem.nodes().ancestors(id) {
            if let AstKind::TSInterfaceDeclaration(_) = anc.kind() {
                let sp = anc.span();
                if let Some(v) = self.iface_ids.get(&(sp.start, sp.end)) {
                    return Some(v.clone());
                }
            }
        }
        None
    }

    /// The `(container, owner)` a def belongs to: its class, else the file.
    fn container_owner(
        &self,
        sem: &Semantic<'a>,
        id: oxc::semantic::NodeId,
    ) -> (NodeId, Option<String>) {
        match self.enclosing_class(sem, id) {
            Some((cid, name)) => (cid, Some(name)),
            None => (self.d.file_id.clone(), None),
        }
    }

    fn enclosing_class_name(
        &self,
        sem: &Semantic<'a>,
        id: oxc::semantic::NodeId,
    ) -> Option<String> {
        self.enclosing_class(sem, id).map(|(_, n)| n)
    }

    /// Span key of the nearest enclosing def that can source a `calls` edge.
    fn enclosing_fn_span(
        &self,
        sem: &Semantic<'a>,
        id: oxc::semantic::NodeId,
    ) -> Option<(u32, u32)> {
        for anc in sem.nodes().ancestors(id) {
            let sp = anc.span();
            if self.fn_def_ids.contains_key(&(sp.start, sp.end)) {
                return Some((sp.start, sp.end));
            }
        }
        None
    }

    fn enclosing_fn_id(&self, sem: &Semantic<'a>, id: oxc::semantic::NodeId) -> Option<NodeId> {
        for anc in sem.nodes().ancestors(id) {
            let sp = anc.span();
            if let Some(nid) = self.fn_def_ids.get(&(sp.start, sp.end)) {
                return Some(nid.clone());
            }
        }
        None
    }

    /// Look up a receiver var's type by walking the enclosing-fn chain outward
    /// (inner scope shadows outer) — the flat analogue of the lexical scope stack.
    fn lookup_var(
        &self,
        name: &str,
        sem: &Semantic<'a>,
        id: oxc::semantic::NodeId,
    ) -> Option<String> {
        for anc in sem.nodes().ancestors(id) {
            let sp = anc.span();
            if self.fn_def_ids.contains_key(&(sp.start, sp.end)) {
                if let Some(ty) = self.var_types.get(&(sp.start, sp.end, name.to_string())) {
                    return Some(ty.clone());
                }
            }
        }
        None
    }
}

// ---- free helpers (owned-string extraction from typed oxc nodes) -----------

/// Normalize a type name by stripping generics and taking the last component:
/// `ns.Foo<Bar>` → `Foo`, `Map<string,W>` → `Map`
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

/// Base identifier of a heritage `Expression` (`extends Base`, `interface extends`),
/// unwrapping a qualified member (`ns.Base` → `Base`). Generics are separate AST
/// fields, so `Base<T>`'s `Expression` is just `Base`.
fn expr_type_name(e: &Expression) -> Option<String> {
    match e {
        Expression::Identifier(r) => Some(r.name.to_string()),
        Expression::StaticMemberExpression(m) => Some(m.property.name.to_string()),
        _ => None,
    }
}

/// Rightmost identifier of a `TSTypeName` (a class `implements` clause carries one):
/// `I` → `I`, `a.B` → `B`.
fn ts_type_name_ident(t: &TSTypeName) -> Option<String> {
    match t {
        TSTypeName::IdentifierReference(r) => Some(r.name.to_string()),
        TSTypeName::QualifiedName(q) => Some(q.right.name.to_string()),
        TSTypeName::ThisExpression(_) => None,
    }
}

/// Base name of a `TSType` reference (`Widget`, `a.B` → `B`); `None` for keywords,
/// arrays, unions, literals, etc. Used for receiver typing of `: Type` annotations.
fn ts_type_base_name(ty: &TSType) -> Option<String> {
    match ty {
        TSType::TSTypeReference(r) => ts_type_name_ident(&r.type_name),
        _ => None,
    }
}

/// Collect the nameable, non-primitive target types of a field's type, unwrapping
/// generics/containers to their inner arg(s): `Array<Foo>`→Foo, `Foo[]`→Foo,
/// `Map<K,V>`→V, `A|B`→A,B; a *user* generic keeps container + inner. Builtin
/// containers and scalar keywords are never nodes. Mirrors `typescript.rs`.
fn collect_field_targets(ty: &TSType, out: &mut Vec<String>, local_types: &HashSet<String>) {
    match ty {
        TSType::TSTypeReference(r) => {
            if let Some(n) = ts_type_name_ident(&r.type_name) {
                // Only filter out built-in containers if they are not defined locally
                // Local type definitions override the built-in denylist (ADR-0036 R1.1)
                if !is_ts_never_node(&n) || local_types.contains(&n) {
                    out.push(n);
                }
            }
            if let Some(args) = &r.type_arguments {
                for a in &args.params {
                    collect_field_targets(a, out, local_types);
                }
            }
        }
        TSType::TSArrayType(a) => collect_field_targets(&a.element_type, out, local_types),
        TSType::TSUnionType(u) => {
            for t in &u.types {
                collect_field_targets(t, out, local_types);
            }
        }
        TSType::TSIntersectionType(i) => {
            for t in &i.types {
                collect_field_targets(t, out, local_types);
            }
        }
        TSType::TSParenthesizedType(p) => {
            collect_field_targets(&p.type_annotation, out, local_types)
        }
        _ => {} // keyword scalars (number/string/bigint/…), literals, etc. — never nodes
    }
}

/// Collect the nameable, non-primitive target types from a type annotation,
/// unwrapping generics to their inner type parameter(s) (ADR-0036):
/// `Array<Foo>`→Foo, `Foo[]`→Foo, `Promise<Widget>`→Widget, `A | B`→A,B.
/// Builtin containers and scalar primitives are skipped.
/// This is the core implementation reused for field, param, and return types.
fn collect_type_targets(ty: &TSType, out: &mut Vec<String>, local_types: &HashSet<String>) {
    match ty {
        TSType::TSTypeReference(r) => {
            // Emit user-defined containers, but drop builtin containers (`Array<..>`, `Promise<..>`)
            if let Some(n) = ts_type_name_ident(&r.type_name) {
                if !is_ts_never_node(&n) || local_types.contains(&n) {
                    out.push(n);
                }
            }
            // Then recurse into the type arguments for the inner types
            if let Some(args) = &r.type_arguments {
                for a in &args.params {
                    collect_type_targets(a, out, local_types);
                }
            }
        }
        TSType::TSArrayType(a) => collect_type_targets(&a.element_type, out, local_types),
        TSType::TSUnionType(u) => {
            for t in &u.types {
                collect_type_targets(t, out, local_types);
            }
        }
        TSType::TSIntersectionType(i) => {
            for t in &i.types {
                collect_type_targets(t, out, local_types);
            }
        }
        TSType::TSParenthesizedType(p) => {
            collect_type_targets(&p.type_annotation, out, local_types)
        }
        TSType::TSIndexedAccessType(_) => {}
        TSType::TSTypeQuery(_) => {}
        TSType::TSInferType(_) => {}
        _ => {
            if let Some(n) = ts_type_base_name(ty) {
                if !is_ts_never_node(&n) || local_types.contains(&n) {
                    out.push(n);
                }
            }
        }
    }
}

/// `(var, type)` for a declarator whose type is knowable: a `: Type` annotation,
/// else a `new Type(..)` initializer. `None` otherwise.
fn declarator_type(vd: &VariableDeclarator) -> Option<String> {
    if let Some(ann) = &vd.type_annotation {
        if let Some(t) = ts_type_base_name(&ann.type_annotation) {
            return Some(t);
        }
    }
    if let Some(Expression::NewExpression(new_expr)) = &vd.init {
        if let Expression::Identifier(callee) = &new_expr.callee {
            return Some(callee.name.to_string());
        }
    }
    None
}

/// A plain field/method key name (`bar`), or `None` for private/computed keys.
fn prop_key_name(key: &PropertyKey) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(n) => Some(n.name.to_string()),
        _ => None,
    }
}

/// The bound name of a simple binding pattern; `None` for destructuring patterns.
fn binding_name(p: &BindingPattern) -> Option<String> {
    match p {
        BindingPattern::BindingIdentifier(bi) => Some(bi.name.to_string()),
        _ => None,
    }
}

fn enum_member_name(n: &TSEnumMemberName) -> Option<String> {
    match n {
        TSEnumMemberName::Identifier(i) => Some(i.name.to_string()),
        TSEnumMemberName::String(s) => Some(s.value.as_str().to_string()),
        _ => None,
    }
}

fn module_export_name(m: &ModuleExportName) -> String {
    match m {
        ModuleExportName::IdentifierName(n) => n.name.to_string(),
        ModuleExportName::IdentifierReference(r) => r.name.to_string(),
        ModuleExportName::StringLiteral(s) => s.value.as_str().to_string(),
    }
}

/// Names declared by an `export <decl>` (so each is a `Local` export).
fn decl_names(decl: &Declaration) -> Vec<String> {
    match decl {
        Declaration::FunctionDeclaration(f) => {
            f.id.as_ref()
                .map(|b| b.name.to_string())
                .into_iter()
                .collect()
        }
        Declaration::ClassDeclaration(c) => {
            c.id.as_ref()
                .map(|b| b.name.to_string())
                .into_iter()
                .collect()
        }
        Declaration::TSInterfaceDeclaration(i) => vec![i.id.name.to_string()],
        Declaration::TSTypeAliasDeclaration(t) => vec![t.id.name.to_string()],
        Declaration::TSEnumDeclaration(e) => vec![e.id.name.to_string()],
        Declaration::VariableDeclaration(v) => v
            .declarations
            .iter()
            .filter_map(|d| binding_name(&d.id))
            .collect(),
        _ => Vec::new(),
    }
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
        TypeScriptOxcExtractor::new()
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

    #[test]
    fn typescript_filters_standard_primitives() {
        // TypeScript denylist filtering should filter standard primitives
        let ex = extract("class Foo {}\nclass C { s: string; n: number; b: boolean; f: Foo; }");
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "string"),
            "string should be filtered"
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "number"),
            "number should be filtered"
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "boolean"),
            "boolean should be filtered"
        );
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Foo"),
            "user type should be kept"
        );
    }

    #[test]
    fn typescript_filters_collection_types() {
        // TypeScript should filter Array and Map but keep inner types
        let ex = extract("class Foo {}\nclass C { arr: Array<Foo>; map: Map<string, Foo>; }");
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "Array"),
            "Array should be filtered"
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "Map"),
            "Map should be filtered"
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "string"),
            "string should be filtered"
        );
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Foo"),
            "Foo should be kept"
        );
    }

    #[test]
    fn typescript_filters_utility_types() {
        // TypeScript should filter utility types like Promise, Record, etc.
        let ex = extract("class Foo {}\nclass C { p: Promise<Foo>; r: Record<string, Foo>; }");
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "Promise"),
            "Promise should be filtered"
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "Record"),
            "Record should be filtered"
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "string"),
            "string should be filtered"
        );
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Foo"),
            "Foo should be kept"
        );
    }

    #[test]
    fn typescript_filters_special_types() {
        // TypeScript should filter special types like any, unknown, void, etc.
        let ex = extract("class Foo {}\nclass C { a: any; u: unknown; v: void; n: null; ud: undefined; f: Foo; }");
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "any"),
            "any should be filtered"
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "unknown"),
            "unknown should be filtered"
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "void"),
            "void should be filtered"
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "null"),
            "null should be filtered"
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "undefined"),
            "undefined should be filtered"
        );
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Foo"),
            "user type should be kept"
        );
    }

    #[test]
    fn typescript_local_types_override_denylist() {
        // Local type definitions should override denylist patterns
        let ex = extract("class String {}\nclass C { s: String; }");
        assert!(
            has_sym_edge(&ex, "type/field", "C", "String"),
            "local String should NOT be filtered"
        );
    }

    #[test]
    fn typescript_local_array_override_denylist() {
        // Local type named Array should not be filtered
        let ex = extract("class Array {}\nclass C { arr: Array; }");
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Array"),
            "local Array should NOT be filtered"
        );
    }

    #[test]
    fn typescript_filters_generic_parameters() {
        // Generic parameters T, U, V, etc. should be filtered
        let ex = extract("class Foo {}\nfunction process<T>(x: T): T { return x; }");
        // FilterStats would track this, but we can't access them easily in tests
        // Just ensure no edges are emitted to generic parameters
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "T"),
            "generic parameter T should be filtered"
        );
        assert!(
            !has_sym_edge(&ex, "type/return", "process", "T"),
            "generic parameter T should be filtered"
        );
    }

    // ---- ADR-0040 §6 item A: object-method shorthand + decl-up-to-body -----

    #[test]
    fn object_method_shorthand_emits_function_node() {
        // `{ greet() {} }` is a method-shorthand → a FILE-contained `function`
        // node (parity with tree-sitter). Exactly ONE such node — the inner
        // FunctionExpression must NOT also be emitted by `def_function` (whose
        // `FunctionDeclaration` guard excludes function expressions).
        let src = "const o = { greet() { return 1 } };";
        let ex = extract(src);
        let greets: Vec<&Node> = ex.nodes.iter().filter(|n| n.label == "greet").collect();
        assert_eq!(
            greets.len(),
            1,
            "exactly one greet node (no double-emit): {greets:?}"
        );
        assert_eq!(greets[0].kind, "function");
        // FILE-contained: no `impl` owner attr.
        assert!(
            !greets[0].attrs.contains_key("impl"),
            "object method is file-contained: {:?}",
            greets[0].attrs
        );
    }

    #[test]
    fn object_value_arrow_property_is_not_a_function_node() {
        // `{ greet: () => 1 }` is an arrow VALUE property (method == false).
        // Tree-sitter emits no node for it → oxc must not either.
        let src = "const o = { greet: () => 1 };";
        let ex = extract(src);
        assert!(
            !ex.nodes.iter().any(|n| n.label == "greet"),
            "arrow value property is not a function node: {:?}",
            ex.nodes
        );
    }

    #[test]
    fn object_value_function_expression_property_is_not_a_function_node() {
        // `{ greet: function() {} }` is a function-expression VALUE property
        // (method == false). Tree-sitter emits no node for it → oxc must not.
        let src = "const o = { greet: function() { return 1 } };";
        let ex = extract(src);
        assert!(
            !ex.nodes.iter().any(|n| n.label == "greet"),
            "function-expression value property is not a function node: {:?}",
            ex.nodes
        );
    }

    #[test]
    fn object_method_call_attributes_to_the_method() {
        // A call inside a method-shorthand attributes to that method (its span is
        // inserted into `fn_def_ids`, like `def_method`).
        let src = "const o = { greet() { helper(); } };";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "calls", "greet", "helper"),
            "call inside object method attributes to greet: {:?}",
            ex.edges
        );
    }

    #[test]
    fn decl_hash_is_signature_up_to_body_not_full_body() {
        // ADR-0028: the collision-hash `decl` is the signature UP TO the body, so
        // an (overload-)colliding node's id is stable across body edits but shifts
        // on a signature edit. Two same-named `f` declarations collide, forcing
        // the decl into the finalized id where it is observable.
        let ids = |src: &str| -> Vec<String> {
            let ex = extract(src);
            let mut v: Vec<String> = ex
                .nodes
                .iter()
                .filter(|n| n.label == "f" && n.kind == "function")
                .map(|n| n.id.0.clone())
                .collect();
            v.sort();
            v
        };
        let base = "function f(a: number) { return 1 }\nfunction f(a: string) { return 2 }";
        let body_edit =
            "function f(a: number) { return 999 }\nfunction f(a: string) { return 888 }";
        let sig_edit = "function f(a: boolean) { return 1 }\nfunction f(a: string) { return 2 }";

        let base_ids = ids(base);
        assert_eq!(base_ids.len(), 2, "two colliding f nodes: {base_ids:?}");
        // Body-only edits leave the ids identical (decl sliced up to the body).
        assert_eq!(
            base_ids,
            ids(body_edit),
            "ids stable across body-only edits"
        );
        // A signature edit changes an id.
        assert_ne!(base_ids, ids(sig_edit), "signature edit changes id");
    }

    // ---- ADR-0040 collision-`decl` parity with tree-sitter ------------------
    //
    // The collision hash (ADR-0028) is computed over `decl`; the tree-sitter path
    // (`Driver::decl_text`) slices from the def start to the node's `body` FIELD,
    // falling back to the whole node when the grammar exposes no `body` field.
    // Grammar probe (tree-sitter-typescript 0.23.2 node-types.json):
    //   * `interface_declaration` fields: [body, name, type_parameters] → to-body
    //   * `enum_declaration`      fields: [body, name]                  → to-body
    //   * `type_alias_declaration` fields: [name, type_parameters, value] — NO
    //     `body` (the RHS is the `value` field) → whole-node fallback.
    // The oxc path must match per kind, byte for byte.

    fn ts_extract(src: &str) -> Extraction {
        crate::TypeScriptExtractor::new()
            .extract(&artifact("src/demo.ts"), src.as_bytes())
            .unwrap()
    }

    /// Sorted finalized ids of every node with this `label` + `kind`.
    fn ids_of(ex: &Extraction, label: &str, kind: &str) -> Vec<String> {
        let mut v: Vec<String> = ex
            .nodes
            .iter()
            .filter(|n| n.label == label && n.kind == kind)
            .map(|n| n.id.0.clone())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn colliding_interface_ids_match_tree_sitter() {
        // Twins with an identical signature (`interface Foo`) but different
        // bodies: tree-sitter's decl is signature-up-to-body, so the hashes tie
        // and the ids take the `#hash` / `#hash~1` ordinal shape — body-
        // INDEPENDENT. The oxc ids must be the same byte-for-byte set.
        let src = "interface Foo { a: number; }\ninterface Foo { b: string; }";
        let ts = ids_of(&ts_extract(src), "Foo", "interface");
        let oxc = ids_of(&extract(src), "Foo", "interface");
        assert_eq!(ts.len(), 2, "two colliding interfaces: {ts:?}");
        assert_eq!(ts, oxc, "interface collision ids match across frontends");
    }

    #[test]
    fn colliding_enum_ids_match_tree_sitter() {
        // Same-signature (`enum E`) twins with different bodies → tree-sitter
        // hashes tie (body-independent). oxc must match byte for byte.
        let src = "enum E { A = 1 }\nenum E { B = 2 }";
        let ts = ids_of(&ts_extract(src), "E", "enum");
        let oxc = ids_of(&extract(src), "E", "enum");
        assert_eq!(ts.len(), 2, "two colliding enums: {ts:?}");
        assert_eq!(ts, oxc, "enum collision ids match across frontends");
    }

    /// The `#<hash>[~k]` suffixes of a colliding id set.
    fn hash_suffixes(ids: &[String], base: &str) -> Vec<String> {
        ids.iter()
            .map(|id| {
                id.strip_prefix(base)
                    .unwrap_or_else(|| panic!("id {id} lacks base {base}"))
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn colliding_type_alias_ids_are_body_dependent_under_both_frontends() {
        // `type_alias_declaration` has NO `body` field (RHS = `value`), so the
        // tree-sitter decl falls back to the WHOLE node — the hash is body-
        // DEPENDENT: twins get two DISTINCT hashes, no tie ordinal. The oxc
        // whole-node `loc` decl is already parity for this kind; assert the
        // same distinct-hash shape under both frontends.
        let src = "type Foo = { a: number };\ntype Foo = { b: string };";
        let base = "type:src/demo.ts:Foo#";
        for (frontend, ex) in [("tree-sitter", ts_extract(src)), ("oxc", extract(src))] {
            let ids = ids_of(&ex, "Foo", "type_alias");
            assert_eq!(ids.len(), 2, "{frontend}: two colliding aliases: {ids:?}");
            let hashes = hash_suffixes(&ids, base);
            assert_ne!(
                hashes[0], hashes[1],
                "{frontend}: body-dependent distinct hashes: {ids:?}"
            );
            assert!(
                hashes.iter().all(|h| !h.contains('~')),
                "{frontend}: no tie ordinal for distinct bodies: {ids:?}"
            );
        }
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
    // Mirrors the tree-sitter suite in `typescript.rs`. The mechanism here is
    // different and so is the failure it guards: this walk is FLAT
    // (`semantic.nodes()` pre-order, no scope-exit event), so the scope is
    // re-derived from the ancestor chain per def. Getting it wrong leaks in BOTH
    // directions — a sibling's parameters clobbering the class's, and a class's
    // outliving it and suppressing a real reference in the next function.

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
        // `nested<U>` must see `{T, U}`. Deriving the scope from the method alone
        // loses `T`.
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
        // `class EventQueue<Dispatch>` shadows a real `Dispatch` (next.js does
        // exactly this). A scope that survives the class suppresses the real
        // reference in `forward` — a DROPPED edge, not a spurious one.
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
}
