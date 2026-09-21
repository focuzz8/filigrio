//! The **node-fact vocabulary** — `Node.attrs` keys the engine reasons about.
//!
//! `Node.attrs` is an open map, exactly like `Node.kind` and `Edge.relation`
//! (ADR-0021 "do NOT enum"): a backend may stamp anything it knows. These
//! constants name the facts something *downstream* reads, so a typo is a compile
//! error rather than a silently-never-matched key.
//!
//! The bar for living here is **crossing a crate boundary**: one crate stamps
//! the fact, another reads it, and nothing but the spelling connects them. Keys
//! that never leave their producer stay literals — `manifest` / `name` / `root`
//! on the ADR-0019 project overlay are written and read only inside
//! `filigrio-resolve`, so a typo there is caught by that crate's own tests.

/// **A declaration with no body** — an interface / abstract-class method
/// signature, a Rust trait `fn get(&self) -> T;`, a Python `Protocol`/ABC
/// method — and, on a type node, the abstract type itself (`abstract class C`,
/// a `trait`, an `interface`, a `Protocol`/ABC). Value: `"true"` when present;
/// the key is **absent**, never `"false"`, on a concrete definition.
///
/// **A node fact, not a node kind** (ADR-0036 §5, decided 2026-07-29). The kind
/// of a bodiless method stays `function`: a method is a method whether or not it
/// has a body, and abstractness is a property of the thing rather than a
/// different thing. The precedent read on both sides:
///
/// * **Kythe** encodes it as the node fact `tag/abstract` — *"non-instantiable
///   class or method which must be defined by subclasses"* — one concept, one
///   fact, every language.
/// * **SCIP** put it in its `Kind` enum instead and then needed `AbstractMethod`,
///   `MethodSpecification`, `ProtocolMethod`, `PureVirtualMethod`, `TraitMethod`
///   and `TypeClassMethod` — six kinds for one concept, because a kind enum
///   cannot compose.
///
/// Consequence for the linker: a bodiless declaration is a **link candidate like
/// any other `function`** (`filigrio_resolve::is_linkable` keys on `kind`), which
/// is the whole point — a call through the abstraction binds to the abstraction.
pub const ABSTRACT: &str = "abstract";

/// The value [`ABSTRACT`] carries when set.
///
/// **Presence-encoded, not a boolean.** `Node.attrs` is a `String → String` open
/// map — every fact on it is a string (`impl` = the owning type, `language` =
/// `"typescript"`, `returns` = a type name) — so a real `bool` would mean giving
/// the attrs map a typed value, which changes the kernel model, its serde form,
/// the store format and the daemon's canonicalize/diff path. Kythe has the same
/// shape for the same reason: facts are byte strings, because an *open* fact
/// vocabulary cannot carry a per-key type without a schema.
///
/// So the test is `attrs.contains_key(ABSTRACT)`; the string is a readable
/// payload, and the key is **absent** rather than `"false"` on a concrete
/// definition. If attrs ever gain a typed value, this is one of the keys to move.
pub const TRUE: &str = "true";

/// **The owning type of a method** — `Type` for a Rust `impl Type`, the class or
/// interface for a TS/Python method. Stamped by every extractor; read by
/// `filigrio-resolve` to type a receiver (`x.method()` → which `method`).
///
/// Crosses `filigrio-index` → `filigrio-resolve`, which is why it is named here:
/// a disagreement about the spelling is a silent resolution failure, not an
/// error. Note the node's *label* stays bare (ADR-0028) — the owner rides here
/// and in the id, not in the label.
pub const IMPL: &str = "impl";

/// **A function's declared return type**, base name only. Stamped by the Rust
/// extractor; read by `filigrio-resolve` for ADR-0026 return-type inference —
/// `let x = f(); x.method()` resolves `method` against `f`'s return type.
///
/// Crosses `filigrio-index` → `filigrio-resolve`. Distinct from the
/// `type/return` **relation**, which is the traversable edge (ADR-0036 R2): this
/// attr is a resolution *input*, the relation is a query *output*.
pub const RETURNS: &str = "returns";

/// **The community label a node was clustered into** (ADR-0024). Stamped by
/// `filigrio-query` when it builds a view; read by `filigrio-daemon` and the MCP
/// bridge to render `community=` on a node line.
///
/// Crosses `filigrio-query` → `filigrio-daemon` / `filigrio-client-mcp`, and it
/// is agent-visible: the value appears verbatim in tool output, so a spelling
/// drift here shows up as a missing field in an answer rather than as a failure.
pub const COMMUNITY: &str = "community";

/// **The source language of a file node**, as classified at extraction
/// (`"rust"`, `"typescript"`, `"python"`). Stamped by the driver on the file
/// node; read by the R1.1 noise filter to select a per-language denylist.
pub const LANGUAGE: &str = "language";
