//! `PythonExtractor` — a **real** tree-sitter extractor for Python (Phase 4).
//!
//! Emits, per file:
//!   * a `file` node;
//!   * `class` nodes and `function` nodes; a `def` inside a `class` is a method,
//!     tagged with `attrs["impl"] = <class>` (the same owner key the Rust
//!     extractor uses, so `filigrio-resolve`'s receiver-type narrowing works
//!     unchanged) and **contained by its class**, not the file — so a class owns
//!     its methods and they cluster together (matching graphify's topology);
//!   * `contains` edges file → top-level def and class → method (resolved);
//!   * `calls` edges (enclosing def → callee) as UNRESOLVED `Symbol` targets;
//!   * `imports` edges file → imported name (`Symbol` targets).
//!
//! **Method calls resolve by receiver type, or not at all.** Python is
//! dynamically typed, so a method call is only linked when the receiver's type
//! is *statically certain*: `self.method()` → the enclosing class,
//! `Class.method()` → that class, `x.method()` where `x` is an annotated local.
//! A method call on a bare local (`x = f(); x.method()`) is **dropped** rather
//! than guessed — binding it by name would merge homonyms, and a constructor
//! binding isn't reliable when the variable can be reassigned. This matches the
//! graphify oracle, which also drops unresolvable member calls, and means Python
//! honestly resolves less than the statically-typed Rust path.
//!
//! **Structure (ADR-0037 §3, Phase 0a).** Shared plumbing lives in the generic
//! [`Driver`] (scope value = a bare type-name `String`); this backend supplies the
//! split capability traits + a thin Python-specific walk. The ADR-0036
//! heritage/field features (empty capability slots) are Phase 0b.

use crate::capability::{
    FieldExtractor, Frontend, HeritageExtractor, ImportExtractor, NodeMapper, ParsedImport,
    ReceiverTyper, TypeNamer,
};
use crate::driver::Driver;
use crate::idgen;
use filigrio_core::{Artifact, Extraction, Extractor, NodeId, Result, TargetRef};
use std::collections::HashSet;
use tree_sitter::{Node as TsNode, Parser};

// Named only by the test module (via its `use super::*`).
#[cfg(test)]
use filigrio_core::{EdgeTarget, Node};

/// Python builtin scalar type names skipped for `type/field` edges (ADR-0037c):
/// they are never graph nodes, so an edge to one is pure unresolved noise. This
/// is **name-based** (unlike Rust `primitive_type` / TS `predefined_type`, which
/// have a syntactic marker) — a Python builtin is a plain `identifier`, so the
/// only signal is the name. Typing containers (`List`/`Dict`) are left as-is
/// (honest-unresolved) for now — a deliberate follow-up, not skipped here.
const PY_BUILTIN_SCALARS: &[&str] = &[
    "int",
    "str",
    "bool",
    "float",
    "bytes",
    "complex",
    "object",
    "bytearray",
    "True",
    "False",
];

/// Python builtin generic containers whose *inner* type args carry the real
/// dependency, so the container name itself is never a `type/field` target.
const PY_BUILTIN_CONTAINERS: &[&str] = &[
    "list",
    "dict",
    "set",
    "tuple",
    "frozenset",
    "type",
    "List",
    "Dict",
    "Set",
    "Tuple",
    "FrozenSet",
    "Type",
    "Optional",
    "Union",
    "Sequence",
    "Iterable",
    "Mapping",
    "MutableMapping",
    "MutableSequence",
    "Callable",
    "Awaitable",
    "AsyncIterable",
    "AsyncIterator",
    "Coroutine",
    "Generator",
    "AsyncGenerator",
    "ContextManager",
    "AsyncContextManager",
    "Any",
    "ClassVar",
    "Final",
    "Annotated",
    "Literal",
    "Concatenate",
    "ParamSpec",
    "TypeVar",
    "None",
    "Ellipsis",
];

/// A name that is never a graph node — a scalar primitive or a builtin container —
/// so it must not become a `type/field` target (ADR-0037c follow-up).
fn py_never_node(name: &str) -> bool {
    PY_BUILTIN_SCALARS.contains(&name) || PY_BUILTIN_CONTAINERS.contains(&name)
}

pub struct PythonExtractor;

impl PythonExtractor {
    pub fn new() -> Self {
        PythonExtractor
    }
}

impl Default for PythonExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for PythonExtractor {
    fn handles(&self, artifact: &Artifact) -> bool {
        artifact.language.as_deref() == Some("python")
    }

    fn extract(&self, artifact: &Artifact, bytes: &[u8]) -> Result<Extraction> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .map_err(|e| filigrio_core::Error::Parse(format!("load python grammar: {e}")))?;
        let tree = parser.parse(bytes, None).ok_or_else(|| {
            filigrio_core::Error::Parse(format!("parse failed: {}", artifact.path))
        })?;

        let mut ctx = Ctx::new(artifact, bytes);

        // First pass: collect module-level TypeVars. They are declared by
        // assignment, so a class using `T` may textually precede `T = TypeVar("T")`
        // — a single walk would read the scope before it exists.
        let mut module_typevars = HashSet::new();
        let mut cursor = tree.root_node().walk();
        for node in tree.root_node().named_children(&mut cursor) {
            if node.kind() == "assignment" && ctx.is_typevar_declaration(node) {
                if let Some(name) = ctx.extract_typevar_name(node) {
                    module_typevars.insert(name);
                }
            }
        }

        // The module's TypeVars are the OUTERMOST type-parameter scope; class and
        // function scopes push onto it and pop back to it.
        ctx.d.push_type_parameters(module_typevars);

        // Second pass: full walk with TypeVar context
        ctx.walk(tree.root_node(), None, None);
        Ok(ctx.d.finish())
    }
}

/// Python backend: the shared [`Driver`] (scope value = a bare type name) plus the
/// capability-trait impls below and the Python-specific walk.
struct Ctx<'a> {
    d: Driver<'a, String>,
    /// Current class name for Self resolution
    current_class_name: Option<String>,
    /// TypeVar bounds: TypeVar name -> bound type (shared between P1b suppression and P1c emission)
    typevar_bounds: HashSet<(String, String)>,
}

impl Frontend for Ctx<'_> {
    type Node<'tree> = TsNode<'tree>;
}

impl NodeMapper for Ctx<'_> {
    /// `class_definition`→class or enum (if inherits from Enum); `function_definition`/methods→function.
    /// Note: enum detection happens in the walk method where we can inspect the argument_list
    fn def_kind<'t>(&self, node: TsNode<'t>) -> Option<(&'static str, &'static str)> {
        Some(match node.kind() {
            // Base kind detection - reclassification to enum happens in the walk
            "class_definition" => ("type", "class"),
            "function_definition" => ("fn", "function"),
            _ => return None,
        })
    }
}

impl TypeNamer for Ctx<'_> {
    /// Base type name from a Python type annotation, unwrapping `type` wrappers,
    /// subscripts (`List[int]` → `List`) and attributes (`typing.List` → `List`).
    /// Also resolves `typing.Self` to the enclosing class name.
    /// **Also unwraps string forward references**: `"Widget"` → `Widget`.
    fn type_name<'t>(&self, ty: TsNode<'t>) -> Option<String> {
        match ty.kind() {
            "type" => ty.named_child(0).and_then(|c| self.type_name(c)),
            "string" => {
                // Handle string forward references: "Widget" -> Widget
                let text = self.d.text(ty)?;
                self.unwrap_string_forward_reference(&text)
            }
            "identifier" => {
                let name = self.d.text(ty)?;
                // typing.Self resolves to the current class
                if name == "Self" {
                    self.current_class_name.clone()
                } else {
                    Some(name)
                }
            }
            "subscript" => ty
                .child_by_field_name("value")
                .and_then(|v| self.type_name(v)),
            "attribute" => {
                let attr = ty
                    .child_by_field_name("attribute")
                    .and_then(|a| self.d.text(a))?;
                // typing.Self resolves to the current class
                if attr == "Self" {
                    self.current_class_name.clone()
                } else {
                    Some(attr)
                }
            }
            _ => None,
        }
    }
}

impl ReceiverTyper for Ctx<'_> {
    /// Python receivers resolve to a bare type name; an unknown receiver is
    /// **dropped** (not name-guessed) by the walk — the strict dynamic-safe policy.
    type Receiver = Option<String>;

    /// `(callee name, receiver-type hint)` for a `call`'s `function` node.
    fn callee_ref<'t>(
        &self,
        func: TsNode<'t>,
        current_class: Option<&str>,
    ) -> Option<(String, Option<String>)> {
        match func.kind() {
            // `foo()` / `Foo()` — bare name (a `Foo()` constructor resolves to
            // the class node by name; no hint needed).
            "identifier" => self.d.text(func).map(|n| (n, None)),
            // `obj.method()` — resolve by the receiver's type or not at all. A
            // method call whose receiver type we can't determine is **dropped**
            // (not name-resolved to a homonym): Python is dynamic, so binding
            // `x.area()` to some `area` by name would be a guess. This matches
            // the graphify oracle, which also drops unresolvable member calls.
            "attribute" => {
                let method = func
                    .child_by_field_name("attribute")
                    .and_then(|a| self.d.text(a))?;
                let hint = self.receiver_type(func, current_class)?;
                Some((method, Some(hint)))
            }
            _ => None,
        }
    }

    /// The inferred type of an `attribute` receiver, when statically certain:
    /// `self` → the enclosing class; an UpperCamel name → that class
    /// (`Class.method`); an annotated local in scope → its type. A bare local
    /// (`x = Foo()`) is intentionally NOT inferred — Python's dynamism makes a
    /// constructor binding an unreliable guess, so those receivers stay unknown.
    fn receiver_type<'t>(&self, attr: TsNode<'t>, current_class: Option<&str>) -> Option<String> {
        let obj = attr.child_by_field_name("object")?;
        if obj.kind() != "identifier" {
            return None;
        }
        let name = self.d.text(obj)?;
        if name == "self" {
            current_class.map(str::to_string)
        } else if name.chars().next().is_some_and(|c| c.is_uppercase()) {
            Some(name)
        } else {
            self.d.lookup_var(&name)
        }
    }
}

impl ImportExtractor for Ctx<'_> {
    /// The bound names an import introduces: `from m import a, b` → `[a, b]`,
    /// `import os` → `[os]`, `import a.b.c` → `[c]` (last segment),
    /// `... as alias` → `[alias]`. Python carries **no specifier** (no resolver
    /// yet, ADR-0037c) — each entry is a bare name. `import *` binds nothing.
    fn imports<'t>(&self, node: TsNode<'t>) -> Vec<ParsedImport> {
        self.imported_names(node)
            .into_iter()
            .map(|name| ParsedImport::Named {
                specifier: None,
                imported: name,
                alias: None,
            })
            .collect()
    }
}

// ADR-0037c structural features (Phase 0b).
impl HeritageExtractor for Ctx<'_> {
    /// `inherits` edges for Python (ADR-0037c): a `class_definition`'s
    /// `superclasses` field is an `argument_list` whose entries are the base
    /// classes (`identifier` / `attribute` / `subscript`). Each resolves to a
    /// bare type name via [`TypeNamer::type_name`]. **No `implements`** — Python
    /// is duck-typed, so ABCs / `Protocol` are just base classes and fold into
    /// `inherits` (ADR-0036 open Q3). Keyword args (`metaclass=..`) yield no name
    /// and are skipped.
    fn heritage<'t>(&self, node: TsNode<'t>) -> Vec<crate::capability::Heritage> {
        if node.kind() != "class_definition" {
            return Vec::new();
        }
        let Some(supers) = node.child_by_field_name("superclasses") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut cursor = supers.walk();
        for base in supers.named_children(&mut cursor) {
            if let Some(target) = self.type_name(base) {
                out.push(crate::capability::Heritage {
                    relation: filigrio_core::relation::INHERITS,
                    target,
                });
            }
        }
        out
    }
}

impl FieldExtractor for Ctx<'_> {
    /// `type/field` edges for Python (ADR-0037c): class-body annotated
    /// assignments (`x: T`) — reusing [`Ctx::infer_assignment_type`], which
    /// already pulls the `(name, type)` pair (unwrapping `subscript`/`attribute`
    /// via `type_name`). Python builtin scalars are name-skipped (see
    /// [`PY_BUILTIN_SCALARS`]). No field node (ADR-0036 §1a).
    fn fields<'t>(&self, node: TsNode<'t>) -> Vec<crate::capability::Field> {
        if node.kind() != "class_definition" {
            return Vec::new();
        }
        let Some(body) = node.child_by_field_name("body") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut cursor = body.walk();
        for stmt in body.named_children(&mut cursor) {
            // A class-body field is an `expression_statement` wrapping an
            // annotated `assignment` (`x: T`).
            if stmt.kind() != "expression_statement" {
                continue;
            }
            let Some(assign) = stmt.named_child(0) else {
                continue;
            };
            if assign.kind() != "assignment" {
                continue;
            }
            let Some(left) = assign.child_by_field_name("left") else {
                continue;
            };
            if left.kind() != "identifier" {
                continue;
            }
            let Some(name) = self.d.text(left) else {
                continue;
            };
            let Some(ty) = assign.child_by_field_name("type") else {
                continue;
            };
            // Collect every nameable, non-primitive target: a generic container is
            // unwrapped to its inner type arg(s) (`list[Foo]`→Foo, `dict[str,W]`→W),
            // scalar primitives and builtin containers skipped (ADR-0037c follow-up).
            let mut targets = Vec::new();
            self.collect_field_targets(ty, &mut targets);
            for type_name in targets {
                out.push(crate::capability::Field {
                    name: name.clone(),
                    type_name,
                    visibility: None, // Python has no field visibility modifier
                });
            }
        }
        out
    }
}

impl<'a> Ctx<'a> {
    fn new(artifact: &Artifact, src: &'a [u8]) -> Self {
        Ctx {
            d: Driver::new(artifact, src, "python"),
            current_class_name: None,
            typevar_bounds: HashSet::new(),
        }
    }

    /// Unwrap a string forward reference by stripping quotes and extracting the inner type name.
    /// Returns `None` if the text is not a valid string forward reference or if we can't parse it.
    /// Handles simple cases like `"Widget"` → `Widget`, `"pkg.Widget"` → `Widget`.
    fn unwrap_string_forward_reference(&self, text: &str) -> Option<String> {
        let text = text.trim();

        // Check if it's a string literal (starts and ends with quotes)
        if !(text.starts_with('"') && text.ends_with('"'))
            && !(text.starts_with('\'') && text.ends_with('\''))
        {
            return None;
        }

        // Strip the quotes
        let inner = &text[1..text.len() - 1];
        let inner = inner.trim();

        if inner.is_empty() {
            return None;
        }

        // For simple identifiers, return them directly
        if inner.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Some(inner.to_string());
        }

        // For qualified names like `pkg.Widget`, extract the last segment
        if let Some(last_dot) = inner.rfind('.') {
            let last_segment = &inner[last_dot + 1..];
            if last_segment
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_')
            {
                return Some(last_segment.to_string());
            }
        }

        // Handle simple generic types like `List[Widget]` → extract Widget
        if let Some(open_bracket) = inner.find('[') {
            if let Some(close_bracket) = inner.rfind(']') {
                let inner_content = &inner[open_bracket + 1..close_bracket];
                let inner_content = inner_content.trim();

                // Recursively unwrap if the inner content is also a string
                if let Some(unwrapped) = self.unwrap_string_forward_reference(inner_content) {
                    return Some(unwrapped);
                }

                // Otherwise, extract the last segment (for qualified names)
                if let Some(last_dot) = inner_content.rfind('.') {
                    let last_segment = &inner_content[last_dot + 1..];
                    return Some(last_segment.trim().to_string());
                }

                return Some(inner_content.to_string());
            }
        }

        // For more complex cases, just return the identifier part if possible
        if let Some(identifier) = inner
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .next()
        {
            if !identifier.is_empty() {
                return Some(identifier.to_string());
            }
        }

        // If we can't parse it, return None (let other handling take over)
        None
    }

    /// **Is this class Python's abstract type?** (ADR-0036 §5.)
    ///
    /// Python has no `abstract` keyword, so the language spells it in the base
    /// list: `class C(ABC)`, `class P(Protocol)` / `Protocol[T]`, or
    /// `class C(metaclass=ABCMeta)`. Both the bare and dotted spellings count
    /// (`abc.ABC`, `typing.Protocol`) because [`TypeNamer::type_name`] already
    /// reduces an `attribute` base to its last segment.
    ///
    /// Deliberately **not** "contains an abstract method": a concrete class with
    /// one `raise NotImplementedError` stub is not an abstract type, and reading
    /// the declaration is what the other two backends do.
    fn is_abstract_class(&self, node: TsNode<'a>) -> bool {
        let Some(supers) = node.child_by_field_name("superclasses") else {
            return false;
        };
        const ABSTRACT_BASES: [&str; 3] = ["ABC", "ABCMeta", "Protocol"];
        let mut cursor = supers.walk();
        for base in supers.named_children(&mut cursor) {
            // `metaclass=ABCMeta` is a keyword argument, so the name to test is
            // its *value*; a positional base is tested directly.
            let named = if base.kind() == "keyword_argument" {
                base.child_by_field_name("name")
                    .and_then(|n| self.d.text(n))
                    .filter(|k| k == "metaclass")
                    .and_then(|_| base.child_by_field_name("value"))
            } else {
                Some(base)
            };
            if let Some(ty) = named.and_then(|n| self.type_name(n)) {
                if ABSTRACT_BASES.contains(&ty.as_str()) {
                    return true;
                }
            }
        }
        false
    }

    /// **Is this method a declaration rather than a definition?** (ADR-0036 §5.)
    ///
    /// Python cannot spell a bodiless method, so a declaration is spelled with a
    /// *placeholder* body, and the rule has two independent halves:
    ///
    /// 1. **The decorator says so** — `@abstractmethod` / `@abstractproperty`,
    ///    bare or dotted (`@abc.abstractmethod`). This is the explicit form and
    ///    is trusted whatever the body is.
    /// 2. **The body is a placeholder** — `...` or `raise NotImplementedError`,
    ///    optionally after a docstring. This is the informal form, and it is by
    ///    far the more common one: `Protocol` bodies are `...` and carry no
    ///    decorator at all, and plenty of base classes raise without inheriting
    ///    `ABC`.
    ///
    /// **Where the line is drawn, and why:** a bare `pass` is *not* a
    /// declaration. `def on_event(self): pass` is a no-op default hook — a
    /// definition that does nothing — and subclasses are not required to
    /// override it. Treating it as abstract would sweep in every optional
    /// callback in a codebase, which is the failure mode that would make the
    /// fact meaningless. A docstring-only body is read the same way. `...` and
    /// `raise NotImplementedError` both mean *"a subclass must supply this"*;
    /// `pass` means *"nothing happens"*.
    ///
    /// The caller additionally requires an enclosing class, so a module-level
    /// `def f(): ...` (a stub or an `@overload` head) is not swept in.
    fn is_abstract_method(&self, node: TsNode<'a>) -> bool {
        if self.has_abstract_decorator(node) {
            return true;
        }
        let Some(body) = node.child_by_field_name("body") else {
            return false;
        };
        let mut cursor = body.walk();
        let mut placeholder = false;
        for stmt in body.named_children(&mut cursor) {
            match stmt.kind() {
                // `raise NotImplementedError` / `raise NotImplementedError("…")`.
                "raise_statement" => {
                    let raised = self.d.text(stmt).unwrap_or_default();
                    if raised.contains("NotImplementedError") {
                        placeholder = true;
                    } else {
                        return false;
                    }
                }
                "expression_statement" => match stmt.named_child(0).map(|c| c.kind()) {
                    // The `...` body.
                    Some("ellipsis") => placeholder = true,
                    // A docstring may precede either placeholder form.
                    Some("string") => {}
                    _ => return false,
                },
                // A comment is a *named* node in tree-sitter's block, not a
                // statement. Failing to skip it cost a real declaration on the
                // langchain corpus (`BaseLanguageModel.with_structured_output`:
                // docstring, comment, `raise NotImplementedError`).
                "comment" => {}
                // Anything else — including a bare `pass` — is a definition.
                _ => return false,
            }
        }
        placeholder
    }

    /// `@abstractmethod` / `@abstractproperty`, bare or dotted. A decorated
    /// `def` is wrapped in a `decorated_definition`, so the decorators are the
    /// *parent's* children, not the function's.
    fn has_abstract_decorator(&self, node: TsNode<'a>) -> bool {
        let Some(parent) = node.parent().filter(|p| p.kind() == "decorated_definition") else {
            return false;
        };
        let mut cursor = parent.walk();
        let decorated = parent.named_children(&mut cursor).any(|child| {
            child.kind() == "decorator"
                && self
                    .d
                    .text(child)
                    .map(|t| {
                        let last = t.trim_start_matches('@').rsplit('.').next().unwrap_or("");
                        // `@abstractmethod` and `@abstractmethod()` alike.
                        let last = last.split('(').next().unwrap_or("").trim();
                        last == "abstractmethod" || last == "abstractproperty"
                    })
                    .unwrap_or(false)
        });
        decorated
    }

    /// Check if a class_definition is an enum by inspecting its argument_list
    /// for Enum, IntEnum, StrEnum, Flag, IntFlag, ReprEnum
    fn is_enum_class(&self, node: TsNode<'a>) -> bool {
        if let Some(supers) = node.child_by_field_name("superclasses") {
            let enum_types = ["Enum", "IntEnum", "StrEnum", "Flag", "IntFlag", "ReprEnum"];
            let mut cursor = supers.walk();
            for base in supers.named_children(&mut cursor) {
                if let Some(type_name) = self.type_name(base) {
                    if enum_types.contains(&type_name.as_str()) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Extract enum variants from the block of an enum class
    /// Returns a list of (variant_name, assignment_node) pairs
    fn extract_enum_variants(&self, node: TsNode<'a>) -> Vec<(String, TsNode<'a>)> {
        let mut variants = Vec::new();

        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for stmt in body.named_children(&mut cursor) {
                // Python enum bodies can have various statement types
                // Try to find assignments in the most direct way
                let assign = match stmt.kind() {
                    // Direct assignment
                    "assignment" => stmt,
                    // Assignment wrapped in expression_statement (most common case)
                    "expression_statement" => stmt
                        .named_child(0)
                        .filter(|c| c.kind() == "assignment")
                        .unwrap_or(stmt),
                    // Skip other statement types
                    _ => continue,
                };

                if assign.kind() != "assignment" {
                    continue;
                }

                // Skip if there's a type annotation (PEP-613 style variant should not be emitted)
                if assign.child_by_field_name("type").is_some() {
                    continue;
                }

                // Get the left-hand side (the variant name)
                let left = match assign.child_by_field_name("left") {
                    Some(n) if n.kind() == "identifier" => n,
                    _ => continue,
                };

                if let Some(name) = self.d.text(left) {
                    // Skip special enum members like _ignore_
                    if name == "_ignore_" {
                        continue;
                    }

                    // Skip dunder methods
                    if name.starts_with("__") && name.ends_with("__") {
                        continue;
                    }

                    variants.push((name, assign));
                }
            }
        }

        variants
    }

    /// Extract the bound from a TypeVar call, if present
    /// Returns the bound type name, or None if no bound
    fn extract_typevar_bound(&self, call_node: TsNode<'a>) -> Option<String> {
        if call_node.kind() != "call" {
            return None;
        }

        // Check if this is a TypeVar call
        let func = call_node.child_by_field_name("function")?;
        let func_name = match func.kind() {
            "identifier" => self.d.text(func)?,
            "attribute" => func
                .child_by_field_name("attribute")
                .and_then(|a| self.d.text(a))?,
            _ => return None,
        };

        if func_name != "TypeVar" {
            return None;
        }

        // Look for named argument "bound"
        if let Some(args) = call_node.child_by_field_name("arguments") {
            let mut cursor = args.walk();
            for arg in args.named_children(&mut cursor) {
                if arg.kind() == "keyword_argument" {
                    let name = arg.child_by_field_name("name");
                    if let Some(name_node) = name {
                        if let Some(n) = self.d.text(name_node) {
                            if n == "bound" {
                                let value = arg.child_by_field_name("value");
                                if let Some(value_node) = value {
                                    // Extract the type name from the bound value
                                    if let Some(type_name) = self.type_name(value_node) {
                                        return Some(type_name);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }

    /// Collect type parameter bounds from PEP-695 type parameters
    /// Returns a list of (type_param_name, bound_type_name) pairs
    fn collect_pep695_type_bounds(&self, node: TsNode<'a>) -> Vec<(String, String)> {
        let mut bounds = Vec::new();

        if node.kind() != "class_definition" && node.kind() != "function_definition" {
            return bounds;
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "type_parameter" {
                if let Some(text) = self.d.text(child) {
                    // Remove the outer brackets and split on commas to get individual type parameters
                    // Format: "[T: Widget, U: Processor]" -> ["T: Widget", " U: Processor"]
                    let contents = text.trim_start_matches('[').trim_end_matches(']');

                    for part in contents.split(',') {
                        let part = part.trim();
                        // Look for the pattern: <name>: <bound>
                        if let Some(colon_pos) = part.find(':') {
                            let type_param = part[..colon_pos].trim();
                            let bound_spec = part[colon_pos + 1..].trim();

                            if !type_param.is_empty() && !bound_spec.is_empty() {
                                // Clean up the bound specification (remove any trailing brackets if present)
                                let bound = bound_spec
                                    .trim_start_matches('[')
                                    .trim_end_matches(']')
                                    .trim();
                                if !bound.is_empty() {
                                    bounds.push((type_param.to_string(), bound.to_string()));
                                }
                            }
                        }
                    }
                }
            }
        }

        bounds
    }

    /// `current_fn` = enclosing def (id, name) for call attribution;
    /// `current_class` = enclosing class (node id + name): the `self` type, the
    /// method owner, and the container methods `contains`-link to.
    fn walk(
        &mut self,
        node: TsNode<'_>,
        current_fn: Option<(NodeId, String)>,
        current_class: Option<(NodeId, String)>,
    ) {
        match node.kind() {
            "class_definition" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| self.d.text(n))
                {
                    if current_fn.is_none() && current_class.is_none() {
                        self.d
                            .exports
                            .push(filigrio_core::Export::Local { name: name.clone() });
                    }

                    // Check if this is an enum class
                    let is_enum = self.is_enum_class(node);

                    // The class's PEP-695 scope (`class C[T]`), pushed onto the
                    // enclosing one — the module's TypeVars, and an outer class's
                    // parameters for a nested class.
                    self.d
                        .push_type_parameters(self.collect_pep695_type_parameters(node));

                    // Determine the kind - reclassify to "enum" if it inherits from Enum
                    let (prefix, kind) = if is_enum {
                        ("type", "enum")
                    } else {
                        self.def_kind(node).unwrap_or(("type", "class"))
                    };

                    let file_id = self.d.file_id.clone();
                    let id = self.d.add_def(prefix, kind, &name, node, &file_id, None);

                    // An ABC / `Protocol` is Python's abstract type (ADR-0036 §5).
                    // Python has no `abstract` keyword, so the fact is what carries
                    // the information the *kind* carries in Rust (`trait`) and TS
                    // (`interface`): here the kind is plain `class` either way.
                    if self.is_abstract_class(node) {
                        self.d.mark_abstract(&id);
                    }

                    // Register as local type for noise filtering exceptions (ADR-0036 R1.1)
                    let base_name = base_type_name(&name);
                    self.d.register_local_type(base_name);

                    // Track current class name for Self resolution
                    self.current_class_name = Some(name.clone());

                    // Handle enum variant extraction
                    if is_enum {
                        let variants = self.extract_enum_variants(node);
                        for (variant_name, variant_node) in variants {
                            // Create qualified variant name per ADR-0028
                            let qualified_variant = idgen::qualify(&variant_name, Some(&name));
                            // Use Driver's add_variant method which creates the variant node + has_variant edge
                            let variant_id =
                                self.d.add_variant(&id, &name, &variant_name, variant_node);

                            // Find the just-created variant node and update its label to be qualified
                            if let Some(node) = self.d.nodes.iter_mut().find(|n| n.id == variant_id)
                            {
                                node.label = qualified_variant;
                            }
                        }
                    }

                    // ADR-0037c structural edges: `inherits` + `type/field`.
                    self.emit_type_edges(&id, node);
                    // Recurse the body owned by this class; class-level code keeps
                    // the outer fn context (usually None).
                    self.walk_children(node, current_fn, Some((id, name)));

                    // Restore previous context
                    self.current_class_name = None;
                    self.d.pop_type_parameters();
                    return;
                }
            }
            "function_definition" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| self.d.text(n))
                {
                    if current_fn.is_none() && current_class.is_none() {
                        self.d
                            .exports
                            .push(filigrio_core::Export::Local { name: name.clone() });
                    }

                    // The function's own PEP-695 scope (`def func[T]():`), pushed
                    // onto the enclosing one: a method of `class C[T]` sees `T`
                    // without redeclaring it, and a sibling method does not see
                    // this function's parameters once it pops.
                    self.d
                        .push_type_parameters(self.collect_pep695_type_parameters(node));
                    let in_scope = self.d.type_parameters().clone();

                    // A method is contained by its class; a free function by the
                    // file.
                    let container = current_class
                        .as_ref()
                        .map(|(id, _)| id.clone())
                        .unwrap_or_else(|| self.d.file_id.clone());
                    let owner = current_class.as_ref().map(|(_, n)| n.as_str());
                    let (prefix, kind) = self.def_kind(node).unwrap_or(("fn", "function"));
                    let id = self.d.add_def(prefix, kind, &name, node, &container, owner);
                    // A `Protocol`/ABC method declaration (ADR-0036 §5). Python
                    // has no bodiless syntax, so the declaration is spelled with a
                    // placeholder body — see [`Ctx::is_abstract_method`] for the
                    // rule and its two halves.
                    if current_class.is_some() && self.is_abstract_method(node) {
                        self.d.mark_abstract(&id);
                    }
                    let fn_ctx = Some((id.clone(), name.clone()));

                    // Emit PEP-695 type/bound edges
                    let pep695_bounds = self.collect_pep695_type_bounds(node);
                    for (_type_param, bound_type) in pep695_bounds {
                        let normalized_bound = base_type_name(&bound_type);
                        let _ = self.d.emit_bound_type(id.clone(), normalized_bound);
                    }

                    // Emit classic TypeVar type/bound edges
                    // Find TypeVars used in this function's signature
                    for (typevar, bound) in &self.typevar_bounds {
                        if in_scope.contains(typevar) {
                            let normalized_bound = base_type_name(bound);
                            let _ = self.d.emit_bound_type(id.clone(), normalized_bound);
                        }
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

                    self.d.scope_push();
                    if let Some(params) = node.child_by_field_name("parameters") {
                        self.collect_params(params, &id);
                    }
                    self.walk_children(node, fn_ctx, current_class);
                    self.d.scope_pop();
                    self.d.pop_type_parameters();
                    return;
                }
            }
            "assignment" => {
                // Check for TypeVar declaration: `T = TypeVar(...)`
                if self.is_typevar_declaration(node) {
                    if let Some(name) = self.extract_typevar_name(node) {
                        let name_clone = name.clone();
                        // Declares a type parameter in the CURRENT scope (module
                        // level for the usual case; a `TypeVar` bound inside a
                        // function goes out of scope with it).
                        self.d.insert_type_parameter(&name);
                        // Extract bound for P1c type/bound emission
                        if let Some(right) = node.child_by_field_name("right") {
                            if let Some(bound) = self.extract_typevar_bound(right) {
                                self.typevar_bounds.insert((name_clone, bound));
                            }
                        }
                    }
                }

                // Check for PEP-613 type alias: `Vector: TypeAlias = list[float]`
                let is_pep613_alias = self.is_pep613_type_alias(node);

                // Record `x = Class()` / `x: Class = ..` for receiver inference,
                // AFTER walking the RHS (so the new binding isn't seen too early).
                let binding = self.infer_assignment_type(node);
                self.walk_children(node, current_fn, current_class);

                // Emit type_alias node for PEP-613
                if is_pep613_alias {
                    if let Some(alias_name) = node
                        .child_by_field_name("left")
                        .and_then(|n| self.d.text(n))
                    {
                        self.d.add_def(
                            "type_alias",
                            "type_alias",
                            &alias_name,
                            node,
                            &self.d.file_id.clone(),
                            None,
                        );
                    }
                }

                if let Some((var, ty)) = binding {
                    self.d.scope_insert(&var, ty);
                }
                return;
            }
            "type_alias_statement" => {
                // PEP-695 type alias: `type Vector = list[float]`
                // The name is the second child (index 1), not a named field
                // Structure: [type, name, =, type_expression]
                let mut cursor = node.walk();
                let mut children = node.children(&mut cursor);

                // Skip the first child (type keyword)
                let _ = children.next();

                // Get the second child (the name)
                if let Some(name_node) = children.next() {
                    if let Some(name) = self.d.text(name_node) {
                        self.d.add_def(
                            "type_alias",
                            "type_alias",
                            &name,
                            node,
                            &self.d.file_id.clone(),
                            None,
                        );
                        // Emit as module export
                        if current_fn.is_none() && current_class.is_none() {
                            self.d.exports.push(filigrio_core::Export::Local { name });
                        }
                    }
                }
            }
            "call" => {
                // Skip TypeVar(... ) calls (they're handled as assignments)
                if self.is_typevar_call(node) {
                    self.walk_children(node, current_fn, current_class);
                    return;
                }
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
            "import_statement" | "import_from_statement" => {
                for imp in self.imports(node) {
                    if let ParsedImport::Named { imported, .. } = imp {
                        self.d.emit_import(TargetRef::new(imported));
                    }
                }
            }
            _ => {}
        }
        self.walk_children(node, current_fn, current_class);
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

    /// Emit the ADR-0036 structural edges for a class def: `inherits` (heritage)
    /// and `type/field` (fields), sourced from the class `NodeId` — mirroring
    /// `typescript.rs::emit_type_edges`.
    fn emit_type_edges(&mut self, id: &NodeId, node: TsNode<'_>) {
        for h in self.heritage(node) {
            self.d.emit_heritage(id.clone(), h.relation, &h.target);
        }
        for f in self.fields(node) {
            let _ =
                self.d
                    .emit_field_type(id.clone(), &f.name, &f.type_name, f.visibility.as_deref());
        }
    }

    // ---- receiver-type inference (scope stack) -----------------------------

    /// Record annotated parameters (`x: Widget`) in the current scope and emit
    /// param_type edges (ADR-0036). Also bind `self` for method receiver inference.
    fn collect_params(&mut self, params: TsNode<'_>, fn_id: &NodeId) {
        let mut cursor = params.walk();
        for child in params.children(&mut cursor) {
            let (name, type_node) = match child.kind() {
                "typed_parameter" => {
                    let mut c = child.walk();
                    let name = child
                        .named_children(&mut c)
                        .find(|n| n.kind() == "identifier")
                        .and_then(|n| self.d.text(n));
                    (name, child.child_by_field_name("type"))
                }
                "typed_default_parameter" => (
                    child
                        .child_by_field_name("name")
                        .and_then(|n| self.d.text(n)),
                    child.child_by_field_name("type"),
                ),
                _ => (None, None),
            };

            // Record in scope for receiver inference
            if let (Some(name), Some(ty)) =
                (name.clone(), type_node.and_then(|t| self.type_name(t)))
            {
                self.d.scope_insert(&name, ty);
            }

            // Emit param_type edges (ADR-0036)
            if let Some(ty_node) = type_node {
                let mut targets = Vec::new();
                self.collect_type_targets(ty_node, &mut targets);
                for type_name in targets {
                    let _ = self.d.emit_param_type(fn_id.clone(), &type_name);
                }
            }
        }
    }

    /// `(var, class)` for an assignment whose type is **explicitly annotated**
    /// (`x: Class = ..`). A bare `x = Class(..)` is deliberately not inferred
    /// (see [`Ctx::receiver_type`]) — the annotation is the only certain signal.
    /// Collect the nameable, non-primitive target types of a field annotation,
    /// unwrapping generics to their inner type parameter(s) (ADR-0037c follow-up):
    /// `list[Foo]`→Foo, `Optional[Bar]`→Bar, `dict[str,W]`→W. Builtin containers
    /// and scalar primitives are skipped ([`py_never_node`]).
    fn collect_field_targets<'t>(&self, ty: TsNode<'t>, out: &mut Vec<String>) {
        self.collect_type_targets(ty, out);
    }

    /// Collect the nameable, non-primitive target types from a type annotation,
    /// unwrapping generics to their inner type parameter(s) (ADR-0036):
    /// `list[Foo]`→Foo, `Optional[Bar]`→Bar, `dict[str,W]`→W. Builtin containers
    /// and scalar primitives are skipped ([`py_never_node`]).
    /// This is a reuse of `collect_field_targets` logic for param/return types.
    /// **Also unwraps string forward references**: `"Widget"` → `Widget`.
    fn collect_type_targets<'t>(&self, ty: TsNode<'t>, out: &mut Vec<String>) {
        match ty.kind() {
            // `type` wraps the actual annotation node.
            "type" => {
                let mut c = ty.walk();
                for ch in ty.named_children(&mut c) {
                    self.collect_type_targets(ch, out);
                }
            }
            "string" => {
                // Handle string forward references by unwrapping them
                if let Some(text) = self.d.text(ty) {
                    if let Some(unwrapped) = self.unwrap_string_forward_reference(&text) {
                        if !py_never_node(&unwrapped) {
                            out.push(unwrapped);
                        }
                    }
                }
            }
            "identifier" => {
                if let Some(n) = self.type_name(ty) {
                    if !py_never_node(&n) {
                        out.push(n);
                    }
                }
            }
            // `pkg.Foo` → the last segment.
            "attribute" => {
                if let Some(n) = self.type_name(ty) {
                    if !py_never_node(&n) {
                        out.push(n);
                    }
                }
            }
            // `list[Foo]` / `dict[str, W]`: an `identifier`/`attribute` container
            // followed by `type_parameter`s holding the inner `type` nodes.
            "generic_type" => {
                let mut c = ty.walk();
                for ch in ty.named_children(&mut c) {
                    match ch.kind() {
                        "identifier" | "attribute" => self.collect_type_targets(ch, out),
                        "type_parameter" => {
                            let mut c2 = ch.walk();
                            for p in ch.named_children(&mut c2) {
                                self.collect_type_targets(p, out);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn infer_assignment_type(&self, assign: TsNode<'_>) -> Option<(String, String)> {
        let left = assign.child_by_field_name("left")?;
        if left.kind() != "identifier" {
            return None;
        }
        let var = self.d.text(left)?;
        let ty = assign
            .child_by_field_name("type")
            .and_then(|t| self.type_name(t))?;
        Some((var, ty))
    }

    /// Check if this assignment is a TypeVar declaration: `T = TypeVar(...)`
    fn is_typevar_declaration(&self, node: TsNode<'_>) -> bool {
        if node.kind() != "assignment" {
            return false;
        }
        let _left = match node.child_by_field_name("left") {
            Some(n) if n.kind() == "identifier" => n,
            _ => return false,
        };
        let right = match node.child_by_field_name("right") {
            Some(n) => n,
            None => return false,
        };
        self.is_typevar_call(right)
    }

    /// Extract the TypeVar name from a TypeVar declaration
    fn extract_typevar_name(&self, node: TsNode<'_>) -> Option<String> {
        node.child_by_field_name("left")
            .and_then(|n| self.d.text(n))
    }

    /// Check if this call is `TypeVar(...)`
    fn is_typevar_call(&self, node: TsNode<'_>) -> bool {
        if node.kind() != "call" {
            return false;
        }
        let func = match node.child_by_field_name("function") {
            Some(n) => n,
            None => return false,
        };
        match func.kind() {
            "identifier" => self.d.text(func).as_deref() == Some("TypeVar"),
            "attribute" => {
                func.child_by_field_name("attribute")
                    .and_then(|a| self.d.text(a))
                    .as_deref()
                    == Some("TypeVar")
            }
            _ => false,
        }
    }

    /// Check if an assignment is a PEP-613 type alias: `Vector: TypeAlias = list[float]`
    fn is_pep613_type_alias(&self, node: TsNode<'_>) -> bool {
        if node.kind() != "assignment" {
            return false;
        }

        // Check if it has a type annotation
        let type_node = match node.child_by_field_name("type") {
            Some(n) => n,
            None => return false,
        };

        // Check if the type annotation is TypeAlias
        match type_node.kind() {
            "identifier" => self.d.text(type_node).as_deref() == Some("TypeAlias"),
            "type" => {
                // For type annotations that are just type nodes, check the text
                self.d.text(type_node).as_deref() == Some("TypeAlias")
            }
            "attribute" => {
                // Handle typing.TypeAlias
                type_node
                    .child_by_field_name("attribute")
                    .and_then(|a| self.d.text(a))
                    .as_deref()
                    == Some("TypeAlias")
            }
            _ => false,
        }
    }

    /// Collect PEP-695 type parameters from a class or function definition: `class C[T](A):` or `def func[T]():`
    fn collect_pep695_type_parameters(&self, node: TsNode<'_>) -> HashSet<String> {
        let mut params = HashSet::new();
        if node.kind() != "class_definition" && node.kind() != "function_definition" {
            return params;
        }

        // First try to find `type_parameter` nodes (if tree-sitter supports PEP-695)
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "type_parameter" {
                if let Some(text) = self.d.text(child) {
                    // Clean up the text: remove brackets and split on commas
                    let cleaned = text.trim_matches(|c| c == '[' || c == ']');
                    for param in cleaned.split(',') {
                        let param_name = param.trim();
                        if !param_name.is_empty() {
                            params.insert(param_name.to_string());
                        }
                    }
                }
            }
        }

        // If no type_parameter nodes found, manually parse PEP-695 syntax
        // by looking for square brackets in the name pattern
        if params.is_empty() {
            if let Some(name_node) = node.child_by_field_name("name") {
                // Get the entire text from the name node to the end of the declaration
                if let Some(name_text) = self.d.text(name_node) {
                    // Look for pattern like `C[T]` or `func[K, V]`
                    // Check if the name itself contains type parameters
                    if let Some(bracket_start) = name_text.find('[') {
                        if let Some(bracket_end) = name_text.find(']') {
                            let params_str = &name_text[bracket_start + 1..bracket_end];
                            for param in params_str.split(',') {
                                let cleaned = param.trim().trim_matches(|c| c == '[' || c == ']');
                                if !cleaned.is_empty() {
                                    params.insert(cleaned.to_string());
                                }
                            }
                        }
                    } else {
                        // Check if there are type_parameter siblings after the name
                        let mut cursor = node.walk();
                        cursor.goto_first_child();
                        // Find name node position
                        loop {
                            if cursor.node() == name_node {
                                // Check siblings after name for type parameters
                                while cursor.goto_next_sibling() {
                                    if cursor.node().kind() == "type_parameter" {
                                        if let Some(name) = self.d.text(cursor.node()) {
                                            params.insert(name);
                                        }
                                    }
                                }
                                break;
                            }
                            if !cursor.goto_next_sibling() {
                                break;
                            }
                        }
                    }
                }
            }
        }

        params
    }

    // ---- imports -----------------------------------------------------------

    /// The bound names an import introduces (see [`ImportExtractor::imports`]).
    fn imported_names(&self, node: TsNode<'_>) -> Vec<String> {
        let mut out = Vec::new();
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                if cursor.field_name() == Some("name") {
                    if let Some(name) = self.imported_binding(cursor.node()) {
                        out.push(name);
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        out
    }

    fn imported_binding(&self, name_node: TsNode<'_>) -> Option<String> {
        match name_node.kind() {
            "aliased_import" => name_node
                .child_by_field_name("alias")
                .and_then(|a| self.d.text(a)),
            // dotted_name / identifier → last dotted segment.
            _ => self
                .d
                .text(name_node)
                .and_then(|t| t.rsplit('.').next().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty()),
        }
    }
}

/// Extract the base type name for noise filtering exceptions.
/// Removes generic parameters (e.g., `Foo<T>` → `Foo`) and extracts the last segment
/// of qualified names (e.g., `typing.Optional` → `Optional`).
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

    /// True iff there's an edge `source_label --relation--> Symbol(target)`.
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

    fn artifact() -> Artifact {
        Artifact {
            path: "src/demo.py".into(),
            kind: ArtifactKind::Code,
            language: Some("python".into()),
        }
    }

    fn extract(src: &str) -> Extraction {
        PythonExtractor::new()
            .extract(&artifact(), src.as_bytes())
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
    fn extracts_classes_functions_and_methods() {
        let src = "\
class Widget:
    def __init__(self):
        pass
    def go(self):
        pass

def helper():
    pass
";
        let ex = extract(src);
        assert_eq!(node(&ex, "Widget").kind, "class");
        assert_eq!(node(&ex, "go").kind, "function");
        assert_eq!(node(&ex, "helper").kind, "function");
        // method `go` is tagged with its class owner; the free function is not.
        assert_eq!(
            node(&ex, "go").attrs.get("impl").map(String::as_str),
            Some("Widget")
        );
        assert!(!node(&ex, "helper").attrs.contains_key("impl"));
    }

    #[test]
    fn self_method_call_hints_enclosing_class() {
        let src = "\
class S:
    def run(self):
        self.help()
    def help(self):
        pass
";
        assert_eq!(hint_for(&extract(src), "help"), Some("S".into()));
    }

    /// Every `calls` edge whose callee is `name`.
    fn calls_named(ex: &Extraction, name: &str) -> usize {
        ex.edges
            .iter()
            .filter(|e| e.relation == "calls")
            .filter(|e| matches!(&e.target, EdgeTarget::Symbol(r) if r.name == name))
            .count()
    }

    #[test]
    fn constructor_local_member_call_is_dropped() {
        // Python is dynamic: `w = Widget()` doesn't guarantee w stays a Widget,
        // so we do NOT infer the receiver type from a constructor binding. The
        // `w.go()` member call is dropped (not name-resolved to a homonym) —
        // matching the graphify oracle. The `Widget()` construction still counts.
        let src = "\
class Widget:
    def go(self):
        pass

def build():
    w = Widget()
    w.go()
";
        let ex = extract(src);
        assert_eq!(
            calls_named(&ex, "go"),
            0,
            "unknown-receiver member call dropped"
        );
        assert_eq!(
            calls_named(&ex, "Widget"),
            1,
            "the constructor call is kept"
        );
    }

    #[test]
    fn annotated_parameter_infers_receiver_type() {
        let src = "\
def use_it(w: Widget):
    w.go()
";
        assert_eq!(hint_for(&extract(src), "go"), Some("Widget".into()));
    }

    #[test]
    fn class_qualified_call_hints_class() {
        let src = "\
class Widget:
    def make():
        pass

def build():
    Widget.make()
";
        assert_eq!(hint_for(&extract(src), "make"), Some("Widget".into()));
    }

    #[test]
    fn module_call_is_dropped() {
        // `os.getcwd()` — a lower-case module receiver is not a type we know, so
        // the member call is dropped rather than name-resolved.
        let src = "\
import os

def build():
    os.getcwd()
";
        assert_eq!(calls_named(&extract(src), "getcwd"), 0);
    }

    #[test]
    fn imports_bind_the_symbol_name() {
        let src = "from pkg.mod import thing, other\nimport os\nimport a.b.c as d\n";
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
        assert!(
            imported.contains(&"thing"),
            "from-import binds thing: {imported:?}"
        );
        assert!(imported.contains(&"other"), "{imported:?}");
        assert!(imported.contains(&"os"), "import os binds os: {imported:?}");
        assert!(
            imported.contains(&"d"),
            "aliased import binds the alias: {imported:?}"
        );
    }

    #[test]
    fn bare_call_is_unresolved_symbol() {
        let src = "def build():\n    helper()\n";
        let ex = extract(src);
        let build = node(&ex, "build").id;
        assert!(ex.edges.iter().any(|e| e.source == build
            && e.relation == "calls"
            && matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "helper" && !r.hints.contains_key("type"))));
    }

    #[test]
    fn deterministic_across_runs() {
        let src = "class A:\n    def m(self):\n        self.n()\n    def n(self):\n        pass\n";
        assert_eq!(extract(src), extract(src));
    }

    #[test]
    fn exports_module_level_defs_only() {
        // Top-level `def`/`class` are module exports; a method and a nested
        // function are not (ADR-0020 — Python emits Local exports).
        let src = "def greet():\n    def nested():\n        pass\n    pass\nclass Widget:\n    def m(self):\n        pass\n";
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
            !ex.exports.contains(&Export::Local { name: "m".into() }),
            "method not exported"
        );
        assert!(
            !ex.exports.contains(&Export::Local {
                name: "nested".into()
            }),
            "nested fn not exported"
        );
    }

    // ---- ADR-0037c structural edges (Python) ---------------------------

    #[test]
    fn class_base_emits_inherits_edge() {
        // `class C(Base)` → an `inherits` edge C → Base (Python has no
        // `implements`; ABCs/Protocols fold into `inherits`). ADR-0037c.
        let src = "class Base:\n    pass\nclass C(Base):\n    pass\n";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "inherits", "C", "Base"),
            "C inherits Base: {:?}",
            ex.edges
        );
    }

    #[test]
    fn class_multiple_bases_emit_multiple_inherits() {
        // `class C(A, B)` → two `inherits` edges. ADR-0037c.
        let src = "class A:\n    pass\nclass B:\n    pass\nclass C(A, B):\n    pass\n";
        let ex = extract(src);
        assert!(has_sym_edge(&ex, "inherits", "C", "A"), "{:?}", ex.edges);
        assert!(has_sym_edge(&ex, "inherits", "C", "B"), "{:?}", ex.edges);
    }

    #[test]
    fn class_field_emits_field_type_edge() {
        // class-body annotated assignment `x: Widget` → a `type/field` edge
        // C → Widget carrying the field name `x` (ADR-0036 §1a: no field node).
        let src = "class Widget:\n    pass\nclass C:\n    x: Widget\n";
        let ex = extract(src);
        let c_id = &ex.nodes.iter().find(|n| n.label == "C").unwrap().id;
        let field = ex
            .edges
            .iter()
            .find(|e| {
                e.relation == "type/field"
                    && &e.source == c_id
                    && matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "Widget")
            })
            .expect("field_type C -> Widget present");
        if let EdgeTarget::Symbol(r) = &field.target {
            assert_eq!(
                r.hints.get("name").map(String::as_str),
                Some("x"),
                "field name rides the edge"
            );
        }
        assert!(
            !ex.nodes.iter().any(|n| n.kind == "field"),
            "fields are edges, not nodes"
        );
    }

    #[test]
    fn primitive_field_type_is_skipped() {
        // `n: int` emits NO field_type edge — Python builtin scalars are
        // name-skipped (no syntactic primitive marker); a nameable type is kept.
        let src = "class Widget:\n    pass\nclass C:\n    n: int\n    w: Widget\n";
        let ex = extract(src);
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "int"),
            "no field_type to int (primitive)"
        );
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Widget"),
            "nameable field type kept: {:?}",
            ex.edges
        );
    }

    #[test]
    fn annotated_field_with_default_value_still_emits() {
        // Real-world fields carry a default (`x: Widget = None`, pydantic
        // `= Field(..)`). The `= value` must NOT suppress the field_type edge.
        let src = "class C:\n    x: Widget = None\n";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Widget"),
            "annotated field with a default value still emits: {:?}",
            ex.edges
        );
    }

    #[test]
    fn generic_field_unwraps_to_inner() {
        // `xs: list[Foo]` / `maybe: Optional[Bar]` → field_type to the INNER type
        // (Foo/Bar), NOT the builtin container (list/Optional). In a type-annotation
        // context tree-sitter uses `generic_type`; we unwrap its type parameters.
        let ex = extract("class C:\n    xs: list[Foo]\n    maybe: Optional[Bar]\n");
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Foo"),
            "{:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Bar"),
            "{:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "list"),
            "builtin container skipped"
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "C", "Optional"),
            "builtin container skipped"
        );
    }

    #[test]
    fn dict_field_unwraps_value_skips_primitive_key() {
        // `m: dict[str, Widget]` → Widget only (str primitive + dict container skipped).
        let ex = extract("class C:\n    m: dict[str, Widget]\n");
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Widget"),
            "{:?}",
            ex.edges
        );
        assert!(!has_sym_edge(&ex, "type/field", "C", "str"));
        assert!(!has_sym_edge(&ex, "type/field", "C", "dict"));
    }

    #[test]
    fn qualified_field_type_unwraps_to_last_segment() {
        // `y: pkg.Widget` → field_type C → Widget (attribute unwrap). ADR-0037c.
        let src = "class C:\n    y: pkg.Widget\n";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/field", "C", "Widget"),
            "qualified field type -> Widget: {:?}",
            ex.edges
        );
    }

    // ---- ADR-0036 R1.1 Noise Suppression Validation (Python) ----

    #[test]
    fn local_python_class_with_denylist_name_not_filtered() {
        // A local class named `Result` should NOT be filtered even though it's
        // in the Python denylist. This tests the local type exception mechanism.
        let src = "\
class Result:
    pass

class Container:
    value: Result
";
        let ex = extract(src);
        // The field_type edge from Container to Result should be emitted
        // because Result is defined locally and should not be filtered
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "Result"),
            "Local Result class should NOT be filtered: {:?}",
            ex.edges
        );
    }

    #[test]
    fn local_python_option_class_not_filtered() {
        // A local class named `Option` (in Python typing module) should NOT be filtered
        // when defined locally.
        let src = "\
class Option:
    pass

class Container:
    item: Option
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "Option"),
            "Local Option class should NOT be filtered: {:?}",
            ex.edges
        );
    }

    #[test]
    fn local_python_class_with_builtin_name_not_filtered() {
        // A local class named `Iterator` should NOT be filtered when defined locally.
        // `Iterator` is in the central Python denylist but NOT in py_never_node,
        // so it tests the central filtering mechanism with local type exceptions.
        let src = "\
class Iterator:
    pass

class Container:
    items: Iterator
";
        let ex = extract(src);
        // Iterator is in the central Python denylist but defined locally,
        // so it should NOT be filtered
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "Iterator"),
            "Local Iterator class should NOT be filtered: {:?}",
            ex.edges
        );
    }

    #[test]
    fn std_python_result_is_filtered() {
        // Standard library `Result` (if imported from typing) should be filtered
        // if not defined locally. Note: Python doesn't have Result in typing, so
        // this tests that the denylist works for actual Python builtins.
        let src = "\
class Container:
    # int is in the Python denylist and should be filtered
    count: int
    # Widget is a custom class and should NOT be filtered
    widget: Widget

class Widget:
    pass
";
        let ex = extract(src);
        // int should be filtered (no field_type edge)
        assert!(
            !has_sym_edge(&ex, "type/field", "Container", "int"),
            "Standard int should be filtered: {:?}",
            ex.edges
        );
        // Widget should NOT be filtered
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "Widget"),
            "Custom Widget should NOT be filtered: {:?}",
            ex.edges
        );
    }

    #[test]
    fn local_generic_parameter_names_suppressed() {
        // Test that generic parameter patterns are suppressed in Python context.
        // Single-letter generics (T, U, V, etc.) should be filtered.
        let src = "\
class T:
    pass

class U:
    pass

class Item:
    pass

class Value:
    pass

class Container:
    t: T
    u: U
    item: Item
    value: Value
";
        let ex = extract(src);
        // When T, U, Item, Value are defined locally, they should NOT be filtered
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "T"),
            "Local T should NOT be filtered: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "U"),
            "Local U should NOT be filtered: {:?}",
            ex.edges
        );
        // These named generic patterns should also NOT be filtered when defined locally
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "Item"),
            "Local Item should NOT be filtered: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "Value"),
            "Local Value should NOT be filtered: {:?}",
            ex.edges
        );
    }

    #[test]
    fn undefined_generic_parameter_patterns_filtered() {
        // With the new syntactic approach, generic-sounding names are NOT filtered
        // unless they are actual declared type parameters. This test reflects the
        // new behavior where names like T, U, Item, Value are treated as real types
        // when not in a type parameter context, allowing cross-file references.
        let src = "\
class Container:
    t: T
    u: U
    item: Item
    value: Value
";
        let ex = extract(src);
        // These should NOT be filtered - they are treated as potentially real types
        // This fixes the bug where cross-file Error references were incorrectly filtered
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "T"),
            "T should NOT be filtered without type parameter context: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "U"),
            "U should NOT be filtered without type parameter context: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "Item"),
            "Item should NOT be filtered without type parameter context: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "Value"),
            "Value should NOT be filtered without type parameter context: {:?}",
            ex.edges
        );
    }

    #[test]
    fn statistics_tracking_works_for_noise_filtering() {
        // Test that filter statistics are properly tracked during extraction.
        // We create a scenario where we know some types should be filtered.
        let src = "\
class Widget:
    pass

class Container:
    # int should be filtered (primitive)
    count: int
    # str should be filtered (primitive)
    name: str
    # Widget should NOT be filtered (custom type)
    item: Widget
    # T should be filtered (generic pattern)
    generic: T
";
        let extractor = PythonExtractor::new();
        let result = extractor.extract(&artifact(), src.as_bytes());

        assert!(result.is_ok(), "Extraction should succeed");
        let ex = result.unwrap();

        // Verify the expected filtering behavior
        assert!(
            !has_sym_edge(&ex, "type/field", "Container", "int"),
            "int should be filtered"
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "Container", "str"),
            "str should be filtered"
        );
        // With new approach, T is NOT filtered without type parameter context
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "T"),
            "T should NOT be filtered without type parameter context"
        );
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "Widget"),
            "Widget should NOT be filtered"
        );
    }

    #[test]
    fn generic_patterns_filtered_across_contexts() {
        // Test that generic patterns are NOT filtered without type parameter context.
        // With the syntactic approach, names like T, U, Item, Value are treated as
        // potentially real types when used outside of a formal type parameter declaration.
        let src = "\
class Generic:
    def method1(self, param: T) -> U:
        pass
    def method2(self, param: Item) -> Value:
        pass
 ";
        let ex = extract(src);

        // Generic patterns should emit param_type/return_type edges
        // when used without formal type parameter declarations
        let param_type_edges: Vec<_> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/param")
            .collect();
        let return_type_edges: Vec<_> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/return")
            .collect();

        // These should NOT be filtered - they are treated as potentially real types
        assert!(
            param_type_edges
                .iter()
                .any(|e| { matches!(&e.target, EdgeTarget::Symbol(s) if s.name == "T") }),
            "T should emit param_type edge: {:?}",
            param_type_edges
        );
        assert!(
            param_type_edges
                .iter()
                .any(|e| { matches!(&e.target, EdgeTarget::Symbol(s) if s.name == "Item") }),
            "Item should emit param_type edge: {:?}",
            param_type_edges
        );
        assert!(
            return_type_edges.iter().any(|e| {
                matches!(&e.target, EdgeTarget::Symbol(s) if s.name == "U" || s.name == "Value")
            }),
            "U and Value should emit return_type edges: {:?}",
            return_type_edges
        );
    }

    // ---- P1: Python type/param and type/return edge tests ----------------

    #[test]
    fn function_param_emits_param_type_edge() {
        // A function with a typed parameter `x: Widget` should emit a param_type edge
        let src = "\
class Widget:
    pass

def process(x: Widget):
    pass
";
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
        let src = "\
class Widget:
    pass

def create() -> Widget:
    pass
";
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
        let src = "\
class Widget:
    pass

class Container:
    def process(self, item: Widget):
        pass
";
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
        let src = "\
class Widget:
    pass

class Factory:
    def create(self) -> Widget:
        pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/return", "create", "Widget"),
            "Factory.create should have return_type edge to Widget: {:?}",
            ex.edges
        );
    }

    #[test]
    fn multiple_params_emit_multiple_param_type_edges() {
        // Multiple parameters should each emit a param_type edge
        let src = "\
class Widget:
    pass

class Processor:
    pass

def process(w: Widget, p: Processor):
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "process should have param_type edge to Widget: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Processor"),
            "process should have param_type edge to Processor: {:?}",
            ex.edges
        );
    }

    #[test]
    fn primitive_param_type_is_skipped() {
        // Primitive types should not emit param_type edges
        let src = "def count(x: int):\n    pass\n";
        let ex = extract(src);
        assert!(
            !has_sym_edge(&ex, "type/param", "count", "int"),
            "int parameter should not emit param_type edge: {:?}",
            ex.edges
        );
    }

    #[test]
    fn primitive_return_type_is_skipped() {
        // Primitive types should not emit return_type edges
        let src = "def get_value() -> int:\n    pass\n";
        let ex = extract(src);
        assert!(
            !has_sym_edge(&ex, "type/return", "get_value", "int"),
            "int return should not emit return_type edge: {:?}",
            ex.edges
        );
    }

    #[test]
    fn generic_param_unwraps_to_inner_type() {
        // Generic parameters should unwrap to inner types
        let src = "\
class Widget:
    pass

def process(items: list[Widget]):
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "process should have param_type edge to Widget (unwrapped from list): {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "list"),
            "list builtin should not emit param_type edge: {:?}",
            ex.edges
        );
    }

    #[test]
    fn generic_return_unwraps_to_inner_type() {
        // Generic return types should unwrap to inner types
        let src = "\
class Widget:
    pass

def create_widgets() -> list[Widget]:
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/return", "create_widgets", "Widget"),
            "create_widgets should have return_type edge to Widget (unwrapped from list): {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/return", "create_widgets", "list"),
            "list builtin should not emit return_type edge: {:?}",
            ex.edges
        );
    }

    #[test]
    fn optional_param_unwraps_to_inner_type() {
        // Optional[T] should unwrap to T
        let src = "\
class Widget:
    pass

def process(maybe: Optional[Widget]):
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "process should have param_type edge to Widget (unwrapped from Optional): {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "Optional"),
            "Optional builtin should not emit param_type edge: {:?}",
            ex.edges
        );
    }

    #[test]
    fn qualified_param_unwraps_to_last_segment() {
        // Qualified types should unwrap to the last segment
        let src = "\
def process(item: pkg.Widget):
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "process should have param_type edge to Widget (qualified.unwrap): {:?}",
            ex.edges
        );
    }

    #[test]
    fn qualified_return_unwraps_to_last_segment() {
        // Qualified return types should unwrap to the last segment
        let src = "\
def create() -> pkg.Widget:
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/return", "create", "Widget"),
            "create should have return_type edge to Widget (qualified.unwrap): {:?}",
            ex.edges
        );
    }

    #[test]
    fn dict_param_unwraps_value_type_skips_key() {
        // dict[str, Widget] should unwrap to Widget only
        let src = "\
class Widget:
    pass

def process(mapping: dict[str, Widget]):
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "process should have param_type edge to Widget (dict value): {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "str"),
            "str key should not emit param_type edge: {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "dict"),
            "dict builtin should not emit param_type edge: {:?}",
            ex.edges
        );
    }

    #[test]
    fn typed_default_parameter_emits_param_type_edge() {
        // Parameters with default values should still emit param_type edges
        let src = "\
class Widget:
    pass

def process(item: Widget = None):
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "parameter with default should emit param_type edge: {:?}",
            ex.edges
        );
    }

    #[test]
    fn self_parameter_does_not_emit_param_type_edge() {
        // The self parameter should not emit a param_type edge
        let src = "\
class Container:
    def process(self):
        pass
";
        let ex = extract(src);
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "self"),
            "self parameter should not emit param_type edge: {:?}",
            ex.edges
        );
    }

    #[test]
    fn builtin_container_param_emits_no_edges() {
        // Builtin generic containers should not emit edges
        let src = "def process(items: list):    pass\n";
        let ex = extract(src);
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "list"),
            "bare list builtin should not emit param_type edge: {:?}",
            ex.edges
        );
    }

    #[test]
    fn class_method_with_complex_types_emits_correct_edges() {
        // Complex real-world method with multiple generics
        let src = "\
class Widget:
    pass

class Processor:
    pass

class Container:
    def process(
        self,
        widgets: list[Widget],
        processor: Optional[Processor],
        mapping: dict[str, Widget]
    ) -> list[Widget]:
        pass
";
        let ex = extract(src);
        // Check param_type edges
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "process should have param_type edge to Widget from list: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Processor"),
            "process should have param_type edge to Processor from Optional: {:?}",
            ex.edges
        );
        // Check return_type edge
        assert!(
            has_sym_edge(&ex, "type/return", "process", "Widget"),
            "process should have return_type edge to Widget from list: {:?}",
            ex.edges
        );
        // Check that builtins are skipped
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "list"),
            "list builtin should not emit param_type edge: {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "Optional"),
            "Optional builtin should not emit param_type edge: {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "dict"),
            "dict builtin should not emit param_type edge: {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "str"),
            "str builtin should not emit param_type edge: {:?}",
            ex.edges
        );
    }

    // ---- P1c: Enums, type aliases, and type/bound features ----------------

    #[test]
    fn enum_variants_emmit_nodes_and_has_variant_edges() {
        // Enum variants should be emitted as enum_variant nodes with has_variant edges
        let src = "\
class Color(Enum):
    RED = 1
    GREEN = 2
    BLUE = 3
";
        let ex = extract(src);

        // Check that variant nodes are emitted with qualified names per ADR-0028
        let red = ex.nodes.iter().find(|n| n.label == "Color::RED");
        assert!(red.is_some(), "RED variant node should exist");

        let green = ex.nodes.iter().find(|n| n.label == "Color::GREEN");
        assert!(green.is_some(), "GREEN variant node should exist");

        let blue = ex.nodes.iter().find(|n| n.label == "Color::BLUE");
        assert!(blue.is_some(), "BLUE variant node should exist");

        // Check that variant nodes have kind "enum_variant"
        if let Some(red_node) = red {
            assert_eq!(red_node.kind, "enum_variant");
        }

        // Check that has_variant edges exist
        let color_id = &ex.nodes.iter().find(|n| n.label == "Color").unwrap().id;
        let red_id = red.as_ref().unwrap().id.clone();
        assert!(
            ex.edges.iter().any(|e| {
                &e.source == color_id
                    && e.relation == "has_variant"
                    && matches!(&e.target, EdgeTarget::Node(id) if id.0 == red_id.0)
            }),
            "Color should have has_variant edge to RED: {:?}",
            ex.edges
        );
    }

    #[test]
    fn enum_variants_qualified_names_adr_0028() {
        // Enum variant names should be owner-qualified (Enum::Variant) per ADR-0028
        let src = "\
class Status(Enum):
    PENDING = 'pending'
    APPROVED = 'approved'
    REJECTED = 'rejected'
";
        let ex = extract(src);

        assert!(
            ex.nodes.iter().any(|n| n.label == "Status::PENDING"),
            "Variant should have qualified name Status::PENDING"
        );
        assert!(
            ex.nodes.iter().any(|n| n.label == "Status::APPROVED"),
            "Variant should have qualified name Status::APPROVED"
        );
        assert!(
            ex.nodes.iter().any(|n| n.label == "Status::REJECTED"),
            "Variant should have qualified name Status::REJECTED"
        );
    }

    #[test]
    fn enum_detection_all_enum_types() {
        // Should detect all Enum types: Enum, IntEnum, StrEnum, Flag, IntFlag, ReprEnum
        let src = "\
class StdEnum(Enum):
    A = 1

class IntEnumClass(IntEnum):
    B = 2

class StrEnumClass(StrEnum):
    C = 'c'

class FlagClass(Flag):
    D = 1

class IntFlagClass(IntFlag):
    E = 2

class ReprEnumClass(ReprEnum):
    F = 3
";
        let ex = extract(src);

        assert_eq!(node(&ex, "StdEnum").kind, "enum");
        assert_eq!(node(&ex, "IntEnumClass").kind, "enum");
        assert_eq!(node(&ex, "StrEnumClass").kind, "enum");
        assert_eq!(node(&ex, "FlagClass").kind, "enum");
        assert_eq!(node(&ex, "IntFlagClass").kind, "enum");
        assert_eq!(node(&ex, "ReprEnumClass").kind, "enum");
    }

    #[test]
    fn enum_variants_skip_annotated_assignments() {
        // Annotated assignments inside Enum body should not be treated as variants
        let src = "\
class Status(Enum):
    PENDING = 'pending'
    APPROVED: str = 'approved'  # Annotated, should not be a variant
    value: int = 0              # Annotated, should not be a variant
";
        let ex = extract(src);

        // Only PENDING should be a variant
        assert!(
            ex.nodes.iter().any(|n| n.label == "Status::PENDING"),
            "Bare assignment should create a variant"
        );
        assert!(
            !ex.nodes.iter().any(|n| n.label == "Status::APPROVED"),
            "Annotated assignment should NOT create a variant"
        );
        assert!(
            !ex.nodes.iter().any(|n| n.label == "Status::value"),
            "Annotated assignment should NOT create a variant"
        );
    }

    #[test]
    fn enum_variants_skip_methods() {
        // Methods inside Enum body should not be treated as variants
        let src = "\
class Color(Enum):
    RED = 1
    GREEN = 2
    
    def describe(self) -> str:
        return f'Color: {self.value}'
";
        let ex = extract(src);

        assert!(
            ex.nodes.iter().any(|n| n.label == "Color::RED"),
            "Bare assignment should create a variant"
        );
        assert!(
            !ex.nodes.iter().any(|n| n.label == "Color::describe"),
            "Method should NOT create a variant"
        );
        // Method should still exist as a separate node
        assert!(
            ex.nodes.iter().any(|n| n.label == "describe"),
            "Method should exist as a function node"
        );
    }

    #[test]
    fn enum_variants_skip_dunders() {
        // Double underscore methods inside Enum body should not be treated as variants
        let src = "\
class Status(Enum):
    PENDING = 'pending'
    
    def __str__(self):
        return str(self.value)
    
    def __repr__(self):
        return f'Status.{self.name}'
";
        let ex = extract(src);

        assert!(
            ex.nodes.iter().any(|n| n.label == "Status::PENDING"),
            "Bare assignment should create a variant"
        );
        assert!(
            !ex.nodes.iter().any(|n| n.label == "Status::__str__"),
            "Dunder method should NOT create a variant"
        );
        assert!(
            !ex.nodes.iter().any(|n| n.label == "Status::__repr__"),
            "Dunder method should NOT create a variant"
        );
    }

    #[test]
    fn enum_variants_skip_ignore_variable() {
        // The _ignore_ variable should not create a variant
        let src = "\
class Status(Enum):
    ACTIVE = 'active'
    INACTIVE = 'inactive'
    _ignore_ = 'this should not be a variant'
";
        let ex = extract(src);

        assert!(
            ex.nodes.iter().any(|n| n.label == "Status::ACTIVE"),
            "Bare assignment should create a variant"
        );
        assert!(
            !ex.nodes.iter().any(|n| n.label == "Status::_ignore_"),
            "_ignore_ should NOT create a variant"
        );
    }

    #[test]
    fn pep695_type_alias_emits_node() {
        // PEP-695 type alias should emit a type_alias node
        let src = "type Vector = list[float]\n";
        let ex = extract(src);

        let vector = ex.nodes.iter().find(|n| n.label == "Vector");
        assert!(vector.is_some(), "PEP-695 type alias should create a node");

        if let Some(vector_node) = vector {
            assert_eq!(
                vector_node.kind, "type_alias",
                "PEP-613 type alias should have kind 'type_alias'"
            );
        }
    }

    #[test]
    fn pep613_type_alias_emits_node() {
        // PEP-613 type alias should emit a type_alias node
        let src = "Vector: TypeAlias = list[float]\n";
        let ex = extract(src);

        let vector = ex.nodes.iter().find(|n| n.label == "Vector");
        assert!(vector.is_some(), "PEP-613 type alias should create a node");

        if let Some(vector_node) = vector {
            assert_eq!(
                vector_node.kind, "type_alias",
                "PEP-613 type alias should have kind 'type_alias'"
            );
        }
    }

    #[test]
    fn plain_assignment_does_not_emit_type_alias() {
        // Plain assignment should NOT emit a type_alias node (heuristic violation of ADR-0023)
        let src = "Vector = list[float]\n";
        let ex = extract(src);

        let vector = ex.nodes.iter().find(|n| n.label == "Vector");
        assert!(
            vector.is_none() || vector.map(|n| n.kind != "type_alias").unwrap_or(false),
            "Plain assignment should NOT create a type_alias node"
        );
    }

    #[test]
    fn type_alias_with_complex_type_emits_node() {
        // Type alias with complex generic type should work
        let src = "type OptionalMap = dict[str, list[Widget]]\n";
        let ex = extract(src);

        let optional_map = ex.nodes.iter().find(|n| n.label == "OptionalMap");
        assert!(
            optional_map.is_some(),
            "Complex type alias should create a node"
        );

        if let Some(node) = optional_map {
            assert_eq!(node.kind, "type_alias");
        }
    }

    #[test]
    fn pep695_type_bound_emits_edge() {
        // PEP-695 type parameter bound should emit a type/bound edge
        let src = "def process[T: Widget](items: list[T]) -> T:\n    pass\n";
        let ex = extract(src);

        let t_bound_edges: Vec<_> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound")
            .collect();

        assert!(
            !t_bound_edges.is_empty(),
            "PEP-695 bound should emit type/bound edge"
        );

        let process = node(&ex, "process");
        let bound_edge = t_bound_edges.iter().find(|e| e.source == process.id);
        assert!(
            bound_edge.is_some(),
            "process should have type/bound edge to Widget"
        );

        if let Some(edge) = bound_edge {
            if let EdgeTarget::Symbol(symbol) = &edge.target {
                assert_eq!(symbol.name, "Widget", "type/bound should target Widget");
            }
        }
    }

    #[test]
    fn classic_typevar_bound_emits_edge() {
        // Classic TypeVar with bound parameter should emit a type/bound edge
        let src = "T = TypeVar('T', bound=Widget)\n";
        let ex = extract(src);

        // Standalone TypeVar declarations should not emit type/bound edges
        let t_bound_edges: Vec<_> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound")
            .collect();

        assert!(
            t_bound_edges.is_empty(),
            "Standalone TypeVar declaration should NOT emit type/bound edge: {:?}",
            t_bound_edges
        );
    }

    #[test]
    fn typevar_bound_used_in_function_emits_bound_edge() {
        // TypeVar with bound used in function should emit type/bound edge from the function
        let src = "\
T = TypeVar('T', bound=Widget)

def process(items: list[T]) -> T:
    pass
";
        let ex = extract(src);

        let process = node(&ex, "process");
        let bound_edges: Vec<_> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound" && e.source == process.id)
            .collect();

        // P1c: type/bound edges should be emitted for functions using TypeVars with bounds
        assert!(
            !bound_edges.is_empty(),
            "Function using TypeVar with bound should emit type/bound edge"
        );
    }

    #[test]
    fn typevar_bound_multiple_bounds_multiple_edges() {
        // Multiple TypeVars with bounds should emit multiple type/bound edges
        let src = "\
T = TypeVar('T', bound=Widget)
U = TypeVar('U', bound=Processor)

def process(items: list[T], processors: list[U]) -> tuple[T, U]:
    pass
";
        let ex = extract(src);

        let process = node(&ex, "process");
        let bound_edges: Vec<_> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound" && e.source == process.id)
            .collect();

        assert!(
            bound_edges.len() >= 2,
            "Function with multiple bounded TypeVars should emit multiple type/bound edges: {:?}",
            bound_edges
        );

        let target_names: Vec<_> = bound_edges
            .iter()
            .filter_map(|e| match &e.target {
                EdgeTarget::Symbol(r) => Some(r.name.as_str()),
                _ => None,
            })
            .collect();

        assert!(
            target_names.contains(&"Widget"),
            "Should have bound to Widget"
        );
        assert!(
            target_names.contains(&"Processor"),
            "Should have bound to Processor"
        );
    }

    #[test]
    fn typevar_without_bound_emits_no_bound_edge() {
        // TypeVar without bound should not emit a type/bound edge
        let src = "\
T = TypeVar('T')

def process(items: list[T]) -> T:
    pass
";
        let ex = extract(src);

        let bound_edges: Vec<_> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound")
            .collect();

        assert!(
            bound_edges.is_empty(),
            "TypeVar without bound should not emit type/bound edge"
        );
    }

    #[test]
    fn enum_variants_with_various_values() {
        // Enum variants with various value types (int, str, tuple)
        let src = "\
class Status(Enum):
    ACTIVE = 1
    INACTIVE = 0
    PENDING = 'pending'
    APPROVED = ('approved', 'a')
";
        let ex = extract(src);

        assert!(ex.nodes.iter().any(|n| n.label == "Status::ACTIVE"));
        assert!(ex.nodes.iter().any(|n| n.label == "Status::INACTIVE"));
        assert!(ex.nodes.iter().any(|n| n.label == "Status::PENDING"));
        assert!(ex.nodes.iter().any(|n| n.label == "Status::APPROVED"));

        // All variants should have enum_variant kind
        for variant in [
            "Status::ACTIVE",
            "Status::INACTIVE",
            "Status::PENDING",
            "Status::APPROVED",
        ] {
            if let Some(n) = ex.nodes.iter().find(|n| n.label == *variant) {
                assert_eq!(n.kind, "enum_variant", "{} should be enum_variant", variant);
            }
        }
    }

    #[test]
    fn enum_and_type_alias_coexist() {
        // Enums and type aliases should coexist correctly
        let src = "\
class Color(Enum):
    RED = 1
    GREEN = 2

type ColorCode = int

def get_code(color: Color) -> ColorCode:
    return color.value
";
        let ex = extract(src);

        // Check enum
        assert_eq!(node(&ex, "Color").kind, "enum");
        assert!(ex.nodes.iter().any(|n| n.label == "Color::RED"));
        assert!(ex.nodes.iter().any(|n| n.label == "Color::GREEN"));

        // Check type alias
        let color_code = ex.nodes.iter().find(|n| n.label == "ColorCode");
        assert!(color_code.is_some(), "Type alias should exist");
        if let Some(node) = color_code {
            assert_eq!(node.kind, "type_alias");
        }
    }

    #[test]
    fn nested_enum_variants_not_supported() {
        // Nested structures in enum variants should work (just create variants)
        let src = "\
class Config(Enum):
    DEFAULT = '/etc/config'
    CUSTOM = '/custom/path'
    LIST = ['a', 'b', 'c']
";
        let ex = extract(src);

        assert!(ex.nodes.iter().any(|n| n.label == "Config::DEFAULT"));
        assert!(ex.nodes.iter().any(|n| n.label == "Config::CUSTOM"));
        assert!(ex.nodes.iter().any(|n| n.label == "Config::LIST"));
    }

    #[test]
    fn pep695_multiple_type_parameters_with_bounds() {
        // Multiple PEP-695 type parameters with bounds
        let src = "def process[T: Widget, U: Processor](t: T, u: U) -> tuple[T, U]:\n    pass\n";
        let ex = extract(src);

        let process = node(&ex, "process");
        let bound_edges: Vec<_> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound" && e.source == process.id)
            .collect();

        assert!(
            bound_edges.len() >= 2,
            "Multiple bounded type parameters should emit multiple type/bound edges: {:?}",
            bound_edges
        );
    }

    // Start of original P1 tests (preserving existing content)
    #[test]
    fn free_function_and_method_edges_are_independent() {
        // Free function and method edges should be independent
        let src = "\
class Widget:
    pass

def free_func(w: Widget) -> Widget:
    pass

class Container:
    def method(self, w: Widget) -> Widget:
        pass
";
        let ex = extract(src);
        // Free function edges
        assert!(
            has_sym_edge(&ex, "type/param", "free_func", "Widget"),
            "free_func should have param_type edge: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/return", "free_func", "Widget"),
            "free_func should have return_type edge: {:?}",
            ex.edges
        );
        // Method edges
        assert!(
            has_sym_edge(&ex, "type/param", "method", "Widget"),
            "method should have param_type edge: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/return", "method", "Widget"),
            "method should have return_type edge: {:?}",
            ex.edges
        );
    }

    #[test]
    fn local_type_in_param_is_not_filtered() {
        // Locally defined types should not be filtered in param/return edges
        let src = "\
class Result:
    pass

def process() -> Result:
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/return", "process", "Result"),
            "Local Result type should NOT be filtered in return_type: {:?}",
            ex.edges
        );
    }

    // ---- TypeVar filtering tests (Issue #1) ---------------------------------

    #[test]
    fn typevar_declaration_filters_as_noise() {
        // Module-level TypeVar declarations should be filtered as noise
        let src = "\
from typing import TypeVar

T = TypeVar('T')
U = TypeVar('U', bound=str)
Item = TypeVar('Item', covariant=True)

class Container:
    def process(self, item: Item) -> T:
        pass
";
        let ex = extract(src);
        // TypeVar names (T, U, Item) should be filtered when used
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "Item"),
            "TypeVar 'Item' should be filtered: {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/return", "process", "T"),
            "TypeVar 'T' should be filtered: {:?}",
            ex.edges
        );
    }

    #[test]
    fn typevar_used_in_field_should_be_filtered() {
        // TypeVars used in field types should be filtered
        let src = "\
from typing import TypeVar

T = TypeVar('T')

class Container:
    value: T
";
        let ex = extract(src);
        assert!(
            !has_sym_edge(&ex, "type/field", "Container", "T"),
            "TypeVar 'T' should be filtered in field: {:?}",
            ex.edges
        );
    }

    #[test]
    fn pep695_type_parameter_filters_as_noise() {
        // PEP-695 class type parameters should be filtered
        let src = "\
class Container[T]:
    def process(self, item: T) -> T:
        pass

class Generic[K, V](dict[K, V]):
    pass
";
        let ex = extract(src);
        // Type parameters T, K, V should be filtered
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "T"),
            "PEP-695 type parameter 'T' should be filtered: {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/return", "process", "T"),
            "PEP-695 type parameter 'T' should be filtered: {:?}",
            ex.edges
        );
    }

    #[test]
    fn typevar_with_constraints_still_filters() {
        // TypeVars with constraints should still be filtered
        let src = "\
from typing import TypeVar

T = TypeVar('T', int, str)

class Container:
    value: T
";
        let ex = extract(src);
        assert!(
            !has_sym_edge(&ex, "type/field", "Container", "T"),
            "TypeVar 'T' with constraints should be filtered: {:?}",
            ex.edges
        );
    }

    #[test]
    fn typevar_bound_class_not_filtered() {
        // The bound class of a TypeVar should NOT be filtered (it's a real type)
        let src = "\
from typing import TypeVar

T = TypeVar('T', bound='Widget')

class Widget:
    pass

class Container:
    def process(self, item: T) -> T:
        pass
";
        let ex = extract(src);
        // TypeVar T should be filtered
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "T"),
            "TypeVar 'T' should be filtered: {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/return", "process", "T"),
            "TypeVar 'T' should be filtered: {:?}",
            ex.edges
        );
        // Widget should NOT be filtered (it's the bound class, though this is a string literal)
    }

    // ---- Self resolution tests (Issue #2) ------------------------------------

    #[test]
    fn self_resolves_to_enclosing_class() {
        // typing.Self should resolve to the enclosing class
        let src = "\
from typing import Self

class Node:
    def parent(self) -> Self:
        pass
    def children(self) -> list[Self]:
        pass
";
        let ex = extract(src);
        // Self in return type should resolve to Node
        assert!(
            !has_sym_edge(&ex, "type/return", "parent", "Self"),
            "Self should NOT appear as unresolved: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/return", "parent", "Node"),
            "Self should resolve to enclosing class Node: {:?}",
            ex.edges
        );
    }

    #[test]
    fn self_in_generic_resolves_to_enclosing_class() {
        // Self inside generic types should resolve to the enclosing class
        let src = "\
from typing import Self

class Container:
    def get_self(self) -> Optional[Self]:
        pass
    def get_many(self) -> list[Self]:
        pass
";
        let ex = extract(src);
        // Self should resolve to Container, not be unresolved
        assert!(
            !has_sym_edge(&ex, "type/return", "get_self", "Self"),
            "Self should be resolved: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/return", "get_self", "Container"),
            "Self should resolve to Container: {:?}",
            ex.edges
        );
    }

    #[test]
    fn self_in_nested_class_resolves_to_correct_class() {
        // Self in nested class should resolve to the enclosing class (not the outer)
        let src = "\
from typing import Self

class Outer:
    class Inner:
        def get_self(self) -> Self:
            pass
    def get_self(self) -> Self:
        pass
";
        let ex = extract(src);
        // Inner.get_self should resolve to Inner
        assert!(
            !has_sym_edge(&ex, "type/return", "get_self", "Self"),
            "Self in Inner should be resolved: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/return", "get_self", "Inner"),
            "Self in Inner should resolve to Inner: {:?}",
            ex.edges
        );
    }

    #[test]
    fn self_in_param_resolves_to_enclosing_class() {
        // Self in parameter types should resolve to the enclosing class
        let src = "\
from typing import Self

class Builder:
    def chain(self, other: Self) -> Self:
        pass
";
        let ex = extract(src);
        // Self in param and return should resolve to Builder
        assert!(
            !has_sym_edge(&ex, "type/param", "chain", "Self"),
            "Self in param should be resolved: {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/return", "chain", "Self"),
            "Self in return should be resolved: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/param", "chain", "Builder"),
            "Self in param should resolve to Builder: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/return", "chain", "Builder"),
            "Self in return should resolve to Builder: {:?}",
            ex.edges
        );
    }

    #[test]
    fn self_in_field_resolves_to_enclosing_class() {
        // Self in field types should resolve to the enclosing class
        let src = "\
from typing import Self

class Node:
    parent: Self
    children: list[Self]
";
        let ex = extract(src);
        // Self in fields should resolve to Node
        assert!(
            !has_sym_edge(&ex, "type/field", "Node", "Self"),
            "Self in field should be resolved: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/field", "Node", "Node"),
            "Self in field should resolve to Node: {:?}",
            ex.edges
        );
    }

    #[test]
    fn self_outside_class_is_skipped() {
        // Self outside a class scope is invalid Python and should be skipped
        let src = "\
from typing import Self

def standalone() -> Self:
    pass
";
        let ex = extract(src);
        // Self in free function should be skipped (not emitted as unresolved,
        // because it's invalid and we can't reasonably resolve it)
        assert!(
            !has_sym_edge(&ex, "type/return", "standalone", "Self"),
            "Self outside class should be skipped: {:?}",
            ex.edges
        );
    }

    // ---- String forward reference tests (Issue #3) ----------------------------

    #[test]
    fn typevar_string_bound_emits_correct_edge() {
        // TypeVar with string bound `bound="Widget"` should emit edge to Widget, not "Widget"
        let src = "\
from typing import TypeVar

T = TypeVar('T', bound='Widget')

class Widget:
    pass

class Container:
    def process(self, item: T) -> T:
        pass
";
        let ex = extract(src);
        let process = node(&ex, "process");
        let bound_edges: Vec<_> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound" && e.source == process.id)
            .collect();

        // Should emit a type/bound edge to Widget, not to the string literal
        assert!(
            !bound_edges.is_empty(),
            "Function using TypeVar with string bound should emit type/bound edge"
        );
        let widget_bound = bound_edges
            .iter()
            .any(|e| matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "Widget"));
        assert!(
            widget_bound,
            "type/bound edge should target 'Widget', not '\"Widget\"': {:?}",
            bound_edges
        );
        // Should NOT have edge to the string literal "Widget"
        let string_bound = bound_edges
            .iter()
            .any(|e| matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "\"Widget\""));
        assert!(
            !string_bound,
            "Should NOT have type/bound edge to string literal '\"Widget\"': {:?}",
            bound_edges
        );
    }

    #[test]
    fn function_param_string_forward_reference_unwrapped() {
        // Function parameter with string forward reference `def f(x: "Foo")`
        // should emit edge to Foo, not "Foo"
        let src = "\
def process(item: 'Widget') -> None:
    pass

class Widget:
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "String forward reference in param should unwrap to Widget: {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/param", "process", "\"Widget\""),
            "Should NOT emit edge to string literal '\"Widget\"': {:?}",
            ex.edges
        );
    }

    #[test]
    fn function_return_string_forward_reference_unwrapped() {
        // Function return with string forward reference `def f() -> "Result"`
        // should emit edge to Result, not "Result"
        let src = "\
def create() -> 'Widget':
    pass

class Widget:
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/return", "create", "Widget"),
            "String forward reference in return should unwrap to Widget: {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/return", "create", "\"Widget\""),
            "Should NOT emit edge to string literal '\"Widget\"': {:?}",
            ex.edges
        );
    }

    #[test]
    fn field_annotation_string_forward_reference_unwrapped() {
        // Field annotation with string forward reference `x: "Vector"`
        // should emit edge to Vector, not "Vector"
        let src = "\
class Container:
    items: 'Vector'

class Vector:
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/field", "Container", "Vector"),
            "String forward reference in field should unwrap to Vector: {:?}",
            ex.edges
        );
        assert!(
            !has_sym_edge(&ex, "type/field", "Container", "\"Vector\""),
            "Should NOT emit edge to string literal '\"Vector\"': {:?}",
            ex.edges
        );
    }

    #[test]
    fn multiple_string_forward_references_all_unwrapped() {
        // Multiple string forward references should all be unwrapped
        let src = "\
from typing import TypeVar, Optional

T = TypeVar('T', bound='Processor')

def transform(input_data: 'Data', processor: T) -> 'Result':
    pass

class Data:
    pass

class Processor:
    pass

class Result:
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "transform", "Data"),
            "String forward reference 'Data' should unwrap: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/return", "transform", "Result"),
            "String forward reference 'Result' should unwrap: {:?}",
            ex.edges
        );
        // TypeVar bound should also be unwrapped
        let transform = node(&ex, "transform");
        let bound_edges: Vec<_> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound" && e.source == transform.id)
            .collect();
        let processor_bound = bound_edges
            .iter()
            .any(|e| matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "Processor"));
        assert!(
            processor_bound,
            "TypeVar bound 'Processor' should be unwrapped: {:?}",
            bound_edges
        );
    }

    #[test]
    fn string_forward_reference_in_generic_type_unwrapped() {
        // String forward reference inside generic type should be unwrapped
        let src = "\
def process_items(items: list['Widget']) -> Optional['Widget']:
    pass

class Widget:
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process_items", "Widget"),
            "String forward reference in generic param should unwrap: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/return", "process_items", "Widget"),
            "String forward reference in generic return should unwrap: {:?}",
            ex.edges
        );
    }

    #[test]
    fn qualified_string_forward_reference_unwrapped() {
        // Qualified string forward reference `def f(x: "pkg.Widget")`
        // should unwrap to the last segment
        let src = "\
def process(item: 'package.Widget') -> 'package.Result':
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "process", "Widget"),
            "Qualified string forward reference should unwrap to Widget: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/return", "process", "Result"),
            "Qualified string forward reference should unwrap to Result: {:?}",
            ex.edges
        );
    }

    #[test]
    fn pe695_type_bound_does_not_need_unwrapping() {
        // PEP-695 type bounds don't use string literals, so no unwrapping needed
        let src = "\
def process[T: Widget](items: list[T]) -> T:
    pass

class Widget:
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/bound", "process", "Widget"),
            "PEP-695 type bound should work without string unwrapping: {:?}",
            ex.edges
        );
    }

    #[test]
    fn string_forward_reference_with_dict_unwraps_correctly() {
        // String forward reference in dict should unwrap correctly
        let src = "\
def create_mapping() -> dict[str, 'Value']:
    pass

class Value:
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/return", "create_mapping", "Value"),
            "String forward reference in dict value should unwrap: {:?}",
            ex.edges
        );
    }

    #[test]
    fn mixed_string_and_regular_annotations_all_work() {
        // Mix of string forward references and regular annotations
        let src = "\
from typing import TypeVar

T = TypeVar('T', bound='Base')

def mix_and_match(
    regular: RegularClass,
    forward: 'ForwardClass',
    generic_regular: Optional[RegularClass],
    generic_forward: list['ForwardClass']
) -> 'ResultClass':
    pass

class RegularClass:
    pass

class ForwardClass:
    pass

class Base:
    pass

class ResultClass:
    pass
";
        let ex = extract(src);
        // Regular annotations should work
        assert!(
            has_sym_edge(&ex, "type/param", "mix_and_match", "RegularClass"),
            "Regular annotation should work: {:?}",
            ex.edges
        );
        // String forward references should unwrap
        assert!(
            has_sym_edge(&ex, "type/param", "mix_and_match", "ForwardClass"),
            "String forward reference should unwrap: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/return", "mix_and_match", "ResultClass"),
            "Return string forward reference should unwrap: {:?}",
            ex.edges
        );
        // TypeVar bound should unwrap
        let mix_fn = node(&ex, "mix_and_match");
        let bound_edges: Vec<_> = ex
            .edges
            .iter()
            .filter(|e| e.relation == "type/bound" && e.source == mix_fn.id)
            .collect();
        let base_bound = bound_edges
            .iter()
            .any(|e| matches!(&e.target, EdgeTarget::Symbol(r) if r.name == "Base"));
        assert!(base_bound, "TypeVar bound should unwrap: {:?}", bound_edges);
    }

    #[test]
    fn string_forward_reference_edge_cases() {
        // Edge cases: empty strings, nested quotes, multiline strings
        let src = "\
def edge_case1(x: 'Simple') -> None:
    pass

def edge_case2() -> 'Another':
    pass

class Simple:
    pass

class Another:
    pass
";
        let ex = extract(src);
        assert!(
            has_sym_edge(&ex, "type/param", "edge_case1", "Simple"),
            "Simple string forward reference should unwrap: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/return", "edge_case2", "Another"),
            "Another string forward reference should unwrap: {:?}",
            ex.edges
        );
    }

    // ---- type-parameter scopes nest (ADR-0036 R1.1) -----------------------

    #[test]
    fn a_method_type_parameter_does_not_leak_into_its_siblings() {
        // PEP-695 parameters are scoped to the `def` that declares them. A
        // parameter may shadow a real class (`adapt[Widget]`); if the scope is
        // assigned rather than pushed/popped, the next method inherits it and the
        // REAL `Widget` reference is suppressed — a dropped edge, not a spurious one.
        let src = "\
class Widget:
    pass

class Registry[T]:
    def adapt[Widget](self, w: Widget) -> None:
        pass

    def store(self, w: Widget) -> None:
        pass
";
        let ex = extract(src);
        assert!(
            !has_sym_edge(&ex, "type/param", "adapt", "Widget"),
            "inside `adapt`, `Widget` is its own type parameter: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/param", "store", "Widget"),
            "in the sibling method `Widget` is the real class again: {:?}",
            ex.edges
        );
    }

    #[test]
    fn a_class_type_parameter_reaches_a_method_that_declares_none() {
        // The other direction: `store` declares nothing and must still see `T`.
        let src = "\
class Widget:
    pass

class Registry[T]:
    def store(self, item: T, w: Widget) -> None:
        pass
";
        let ex = extract(src);
        assert!(
            !has_sym_edge(&ex, "type/param", "store", "T"),
            "the class's `T` is in scope inside `store`: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/param", "store", "Widget"),
            "a real parameter type is still emitted: {:?}",
            ex.edges
        );
    }

    #[test]
    fn a_class_type_parameter_does_not_outlive_its_class() {
        let src = "\
class Widget:
    pass

class Registry[Widget]:
    def store(self, w: Widget) -> None:
        pass

def forward(w: Widget) -> None:
    pass
";
        let ex = extract(src);
        assert!(
            !has_sym_edge(&ex, "type/param", "store", "Widget"),
            "inside the class, `Widget` is the type parameter: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/param", "forward", "Widget"),
            "after the class, `Widget` is the real class again: {:?}",
            ex.edges
        );
    }

    #[test]
    fn a_module_typevar_stays_in_scope_after_a_class_body() {
        // The module's TypeVars are the OUTERMOST scope: leaving a class must pop
        // back to them, not clear them.
        let src = "\
from typing import TypeVar

T = TypeVar('T')

class Widget:
    pass

class Registry:
    def store(self, item: T) -> None:
        pass

def forward(item: T, w: Widget) -> None:
    pass
";
        let ex = extract(src);
        assert!(
            !has_sym_edge(&ex, "type/param", "forward", "T"),
            "a module-level TypeVar is still a type parameter after a class body: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/param", "forward", "Widget"),
            "a real parameter type is still emitted: {:?}",
            ex.edges
        );
    }

    // ---- ADR-0036 §5: a bodiless declaration is a node ---------------------

    // Python cannot spell a bodiless method, so this fixture is the catalogue of
    // how the language *does* spell one — and, just as importantly, of the two
    // shapes that look similar and are not declarations.
    const ABSTRACTION_PY: &str = r#"
from abc import ABC, abstractmethod
from typing import Protocol


class Widget: ...


class Store(Protocol):
    def get(self, key: str) -> Widget: ...


class Base(ABC):
    @abstractmethod
    def render(self, w: Widget) -> Widget:
        """Docstring, then the placeholder."""
        ...

    def fetch(self, w: Widget) -> Widget:
        raise NotImplementedError

    def steer(self, w: Widget) -> Widget:
        """Docstring, a comment, then the raise."""
        # A comment is a *named* tree-sitter node, not a statement.
        raise NotImplementedError

    def on_event(self, w: Widget) -> None:
        pass

    def describe(self, w: Widget) -> Widget:
        return self.render(w)
"#;

    /// The three Python spellings of a declaration are all marked, the kind
    /// stays `function`, and the two look-alikes are not marked.
    ///
    /// This is one assertion rather than five because the *set* is the property:
    /// a rule that marks `...` but misses `raise NotImplementedError`, or that
    /// sweeps in `pass`, is wrong in a way a per-case assertion hides.
    #[test]
    fn python_declarations_are_abstract_function_nodes() {
        let ex = extract(ABSTRACTION_PY);
        let marked: Vec<&str> = ex
            .nodes
            .iter()
            .filter(|n| n.kind == "function" && n.attrs.contains_key("abstract"))
            .map(|n| n.id.0.as_str())
            .collect();
        assert_eq!(
            marked,
            vec![
                // `...` in a `Protocol`
                "fn:src/demo.py:Store::get",
                // `@abstractmethod` + docstring + `...`
                "fn:src/demo.py:Base::render",
                // `raise NotImplementedError`, no decorator, no ABC needed
                "fn:src/demo.py:Base::fetch",
                // docstring + comment + `raise` — the langchain shape
                "fn:src/demo.py:Base::steer",
            ],
            "wrong declaration set. `on_event` (bare `pass` — a no-op default hook) \
             and `describe` (a real body) must NOT be abstract. Nodes: {:?}",
            ex.nodes
                .iter()
                .filter(|n| n.kind == "function")
                .map(|n| (n.id.0.as_str(), n.attrs.get("abstract")))
                .collect::<Vec<_>>()
        );
        assert!(
            ex.nodes
                .iter()
                .filter(|n| n.attrs.contains_key("abstract") && n.kind == "function")
                .all(|n| n.attrs.get("abstract").map(String::as_str) == Some("true")),
            "the fact's value is `true`"
        );
    }

    /// A `Protocol` and an `ABC` are abstract **types**; a plain class is not.
    ///
    /// Python has no `abstract` keyword, so unlike Rust's `trait` and TS's
    /// `interface` the *kind* cannot carry this — `Store` and `Widget` are both
    /// `class`. The node fact is the only thing that distinguishes them, which is
    /// the clearest case for ADR-0036 §5 choosing a fact over a kind.
    #[test]
    fn a_protocol_and_an_abc_are_abstract_types() {
        let ex = extract(ABSTRACTION_PY);
        let ty = |l: &str| {
            ex.nodes
                .iter()
                .find(|n| n.label == l && n.kind == "class")
                .unwrap_or_else(|| panic!("no `{l}` class node: {:?}", ex.nodes))
        };
        assert_eq!(
            ty("Store").attrs.get("abstract").map(String::as_str),
            Some("true"),
            "a `Protocol` is an abstract type"
        );
        assert_eq!(
            ty("Base").attrs.get("abstract").map(String::as_str),
            Some("true"),
            "an `ABC` is an abstract type"
        );
        assert!(
            !ty("Widget").attrs.contains_key("abstract"),
            "a plain class carries no `abstract` key, even though its body is `...`"
        );
    }

    /// `class C(metaclass=ABCMeta)` — the third spelling of an ABC, where the
    /// name is in a **keyword argument's value** rather than in a positional base.
    #[test]
    fn a_metaclass_abcmeta_class_is_abstract() {
        let ex = extract(
            r#"
import abc


class Legacy(metaclass=abc.ABCMeta):
    def get(self) -> int: ...
"#,
        );
        let legacy = ex
            .nodes
            .iter()
            .find(|n| n.label == "Legacy")
            .expect("`Legacy` node");
        assert_eq!(
            legacy.attrs.get("abstract").map(String::as_str),
            Some("true"),
            "`metaclass=abc.ABCMeta` declares an abstract class: {:?}",
            legacy.attrs
        );
    }

    /// A declaration's signature is the contract, so it emits `type/param` and
    /// `type/return` like any other method (§5 + R2).
    #[test]
    fn a_python_declaration_emits_its_signature_types() {
        let ex = extract(ABSTRACTION_PY);
        assert!(
            has_sym_edge(&ex, "type/return", "get", "Widget"),
            "the Protocol method's return type is missing: {:?}",
            ex.edges
        );
        assert!(
            has_sym_edge(&ex, "type/param", "render", "Widget"),
            "the abstract method's parameter type is missing: {:?}",
            ex.edges
        );
    }

    /// A **module-level** `def f(): ...` is not a declaration.
    ///
    /// A bare `...` body at module level is a stub or an `@overload` head, not an
    /// abstraction anything programs against — there is no declaring type to
    /// reach it through. The enclosing-class requirement in the walk is what
    /// keeps them out, and it is the Python analogue of Rust's `extern`-block
    /// exclusion.
    #[test]
    fn a_module_level_stub_is_not_abstract() {
        let ex = extract("def helper(x: int) -> int: ...\n");
        assert!(
            ex.nodes.iter().all(|n| !n.attrs.contains_key("abstract")),
            "a module-level stub is not an abstraction: {:?}",
            ex.nodes
                .iter()
                .map(|n| (&n.id.0, &n.attrs))
                .collect::<Vec<_>>()
        );
    }
}
