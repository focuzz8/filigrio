//! The cross-backend fixture corpus (ADR-0037 status doc, **P2**).
//!
//! ## Why this exists
//!
//! `filigrio-index` carries five backends behind one [`Extractor`] port. Every
//! per-backend test to date asserted *"this backend emitted that edge"*, which is
//! a property of one backend in isolation — and P3 proved that shape of test is
//! structurally unable to catch the defect that mattered. `LINKABLE` in
//! `filigrio-resolve` omitted `interface` and `type_alias`, so 2 095 of next.js's
//! 60 685 nodes could never become link candidates and *every* reference to a
//! TypeScript interface was unresolvable, with `implements` stuck at 4.4 %
//! resolved for weeks. Each backend's own tests stayed green throughout: the
//! edges *were* emitted. What no test expressed was that TypeScript alone was
//! failing on infrastructure Rust and Python used successfully.
//!
//! So this corpus is not "more tests for `filigrio-index`". It is **one program,
//! written three times**, with the expectations *declared per backend in a table*
//! so that a backend which silently diverges from its peers fails a test rather
//! than quietly under-reporting. Divergence is allowed — languages genuinely
//! differ — but only when it is written down here, next to the reason.
//!
//! ## The table
//!
//! Each [`Fixture`] holds one [`Case`] per [`Backend`], and every case lists the
//! **same** relation keys (enforced by
//! `cross_backend_contract::fixture_tables_are_backend_complete`). A key is never
//! omitted to mean "not applicable"; it is spelled with an [`Expect`] variant that
//! says which kind of absence it is:
//!
//! * [`Expect::Emitted`] — the relation must appear.
//! * [`Expect::NoSuchConstruct`] — the language has no such construct (Rust has no
//!   class inheritance, so `inherits` cannot exist).
//! * [`Expect::NotYetEmitted`] — the construct exists and the backend does *not*
//!   emit it. Asserted **absent**, so that closing the gap turns this row red and
//!   forces the table to be updated in the same commit as the extractor.

#![allow(dead_code)] // each test binary uses a subset of the harness

use filigrio_core::{classify, Artifact, Edge, EdgeTarget, Extraction, Extractor, Node};
use filigrio_index::DispatchExtractor;
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// backends
// ---------------------------------------------------------------------------

/// A language backend under test. The corpus is exhaustive over this list, so
/// adding a real backend (Go, C#) is a compile error until it has fixtures.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Backend {
    Rust,
    Python,
    TypeScript,
}

/// Every backend the corpus covers. Iterated by every cross-backend property.
pub const BACKENDS: &[Backend] = &[Backend::Rust, Backend::Python, Backend::TypeScript];

impl Backend {
    /// The extension that routes an artifact to this backend through
    /// `filigrio_core::classify` — the *production* dispatch key, so a fixture
    /// cannot reach a backend by a path production would not.
    pub fn ext(self) -> &'static str {
        match self {
            Backend::Rust => "rs",
            Backend::Python => "py",
            Backend::TypeScript => "ts",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Backend::Rust => "rust",
            Backend::Python => "python",
            Backend::TypeScript => "typescript",
        }
    }
}

// ---------------------------------------------------------------------------
// expectations
// ---------------------------------------------------------------------------

/// What a backend is required to do with one relation in one fixture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Expect {
    /// The relation must be emitted by this backend for this fixture.
    Emitted,
    /// The language has no such construct at all — a real, permanent asymmetry
    /// (Rust has no class inheritance; Python has no `implements`). Carries the
    /// reason so the asymmetry is documented rather than assumed.
    NoSuchConstruct(&'static str),
    /// The language *has* the construct and this backend does not emit it yet —
    /// a tracked gap, named by its item in `docs/adr/0037-implementation-status.md`.
    /// Asserted **absent**: when the gap closes, this row fails, which is how the
    /// table stays true to the code.
    NotYetEmitted(&'static str),
}

impl Expect {
    /// Must the relation be present?
    pub fn wants_emission(self) -> bool {
        matches!(self, Expect::Emitted)
    }

    pub fn why(self) -> &'static str {
        match self {
            Expect::Emitted => "must be emitted",
            Expect::NoSuchConstruct(r) | Expect::NotYetEmitted(r) => r,
        }
    }
}

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// One backend's rendering of a fixture: the source, and what it must produce.
pub struct Case {
    pub backend: Backend,
    pub src: &'static str,
    /// Relation → expectation. Every case of a fixture lists the **same** keys;
    /// see the module docs for why an omission is not allowed to mean anything.
    pub expect: &'static [(&'static str, Expect)],
    /// Node kinds this case must emit. Used by the linkability property to prove
    /// the corpus actually exercises each backend's declared vocabulary — an
    /// exclusion register that no fixture reaches proves nothing.
    pub kinds: &'static [&'static str],
}

/// The same small program in every language, plus the per-backend expectations.
pub struct Fixture {
    pub name: &'static str,
    pub doc: &'static str,
    pub cases: &'static [Case],
}

impl Fixture {
    pub fn case(&self, backend: Backend) -> Option<&Case> {
        self.cases.iter().find(|c| c.backend == backend)
    }

    /// `fixtures/<name>.<ext>` — the path a case is extracted under.
    pub fn path(&self, backend: Backend) -> String {
        format!("fixtures/{}.{}", self.name, backend.ext())
    }
}

use filigrio_core::relation::{
    BOUND_TYPE, EXTENDS, FIELD_TYPE, HAS_VARIANT, IMPLEMENTS, INHERITS, PARAM_TYPE, RETURN_TYPE,
};

/// ADR-0036 R2: the type-reference family, one relation per position. The
/// signature fixture below exercises all four in every language that emits them.
pub const TYPE_FAMILY: &[&str] = &[FIELD_TYPE, PARAM_TYPE, RETURN_TYPE, BOUND_TYPE];

/// The status doc's P4: TypeScript signature positions, both frontends (COMPLETED).
const TS_P4: &str = "TS signature positions, both frontends (COMPLETED in P4)";
/// The status doc's P5: TypeScript `<T extends X>` bounds.
const TS_P5: &str = "TS `<T extends X>` bounds are unbuilt (status doc P5)";

// ---- fixture 1: the ADR-0036 type-reference family -------------------------

const SIGNATURE_RUST: &str = r#"
pub trait Shape {}

pub struct WidgetId;

pub struct Widget {
    pub id: WidgetId,
}

pub fn build(w: Widget) -> WidgetId {
    w.id
}

pub fn bounded<T: Shape>(t: T) -> WidgetId {
    WidgetId
}

pub struct Holder<T: Shape> {
    pub item: T,
}

// Rust *has* type aliases; this backend does not mint a `type_alias` node for
// one (status doc "Nodes" matrix: oracle Rust and port Rust both ❌).
pub type WidgetName = String;
"#;

const SIGNATURE_PYTHON: &str = r#"
class Shape: ...


class WidgetId: ...


class Widget:
    id: WidgetId


def build(w: Widget) -> WidgetId:
    return w.id


def bounded[T: Shape](t: T) -> WidgetId:
    return WidgetId()


class Holder[T: Shape]:
    item: T


type WidgetName = str
"#;

const SIGNATURE_TYPESCRIPT: &str = r#"
export interface Shape {}

export class WidgetId {}

export class Widget {
  id: WidgetId
}

export function build(w: Widget): WidgetId {
  return w.id
}

export function bounded<T extends Shape>(t: T): WidgetId {
  return new WidgetId()
}

export class Holder<T extends Shape> {
  item: T
}

export type WidgetName = string
"#;

/// The ADR-0036 relation family: a field, a parameter, a return and a bound, in
/// one program, in three languages. Every language here *has* all four
/// constructs, so every divergence below is a port gap rather than a language
/// difference — which is exactly the distinction the table forces you to make.
pub const SIGNATURE: Fixture = Fixture {
    name: "signature",
    doc: "a field, a parameter, a return type and a generic bound",
    cases: &[
        Case {
            backend: Backend::Rust,
            src: SIGNATURE_RUST,
            expect: &[
                (FIELD_TYPE, Expect::Emitted),
                (PARAM_TYPE, Expect::Emitted),
                (RETURN_TYPE, Expect::Emitted),
                (BOUND_TYPE, Expect::Emitted),
            ],
            kinds: &["file", "struct", "trait", "function"],
        },
        Case {
            backend: Backend::Python,
            src: SIGNATURE_PYTHON,
            expect: &[
                (FIELD_TYPE, Expect::Emitted),
                (PARAM_TYPE, Expect::Emitted),
                (RETURN_TYPE, Expect::Emitted),
                (BOUND_TYPE, Expect::Emitted),
            ],
            kinds: &["file", "class", "function", "type_alias"],
        },
        Case {
            backend: Backend::TypeScript,
            src: SIGNATURE_TYPESCRIPT,
            expect: &[
                (FIELD_TYPE, Expect::Emitted),
                (PARAM_TYPE, Expect::Emitted),
                (RETURN_TYPE, Expect::Emitted),
                (BOUND_TYPE, Expect::NotYetEmitted(TS_P5)),
            ],
            kinds: &["file", "class", "interface", "function", "type_alias"],
        },
    ],
};

// ---- fixture 4: the abstraction (ADR-0036 §5) ------------------------------

const ABSTRACTION_RUST: &str = r#"
pub struct Widget;

pub struct WidgetId;

pub trait Store {
    fn get(&self, id: WidgetId) -> Widget;
}
"#;

const ABSTRACTION_PYTHON: &str = r#"
from typing import Protocol


class Widget: ...


class WidgetId: ...


class Store(Protocol):
    def get(self, id: WidgetId) -> Widget: ...
"#;

const ABSTRACTION_TYPESCRIPT: &str = r#"
export class Widget {}

export class WidgetId {}

export interface Store {
  get(id: WidgetId): Widget
}
"#;

/// **A bodiless declaration**, in the three ways the three languages spell it: a
/// Rust trait's `fn get(&self) -> …;`, a Python `Protocol` method whose body is
/// `...`, a TypeScript `interface` member.
///
/// The construct exists in every language here, so — like [`SIGNATURE`] — every
/// divergence this fixture could expose is a port gap rather than a language
/// difference. Its `type/param` + `type/return` rows are the ones that matter:
/// ADR-0036 §5 decided a declaration's signature is a contract and emits the
/// family exactly as a definition's does, and a backend that mints the node but
/// drops the signature edges leaves the abstraction invisible to the ranking §5
/// exists to fix.
pub const ABSTRACTION: Fixture = Fixture {
    name: "abstraction",
    doc: "a bodiless method declaration and the type that declares it",
    cases: &[
        Case {
            backend: Backend::Rust,
            src: ABSTRACTION_RUST,
            expect: &[
                (PARAM_TYPE, Expect::Emitted),
                (RETURN_TYPE, Expect::Emitted),
            ],
            kinds: &["file", "struct", "trait", "function"],
        },
        Case {
            backend: Backend::Python,
            src: ABSTRACTION_PYTHON,
            expect: &[
                (PARAM_TYPE, Expect::Emitted),
                (RETURN_TYPE, Expect::Emitted),
            ],
            kinds: &["file", "class", "function"],
        },
        Case {
            backend: Backend::TypeScript,
            src: ABSTRACTION_TYPESCRIPT,
            expect: &[
                (PARAM_TYPE, Expect::Emitted),
                (RETURN_TYPE, Expect::Emitted),
            ],
            kinds: &["file", "class", "interface", "function"],
        },
    ],
};

// ---- fixture 2: heritage ---------------------------------------------------

const HERITAGE_RUST: &str = r#"
pub trait Shape {}

pub trait Solid: Shape {}

pub struct Widget;

impl Shape for Widget {}
"#;

const HERITAGE_PYTHON: &str = r#"
class Shape: ...


class Widget(Shape): ...
"#;

const HERITAGE_TYPESCRIPT: &str = r#"
export interface Shape {}

export interface Solid extends Shape {}

export class Widget implements Shape {}

export class Gadget extends Widget {}
"#;

/// `implements` / `extends` / `inherits` — the one place the three languages
/// genuinely differ, so every cell here is either `Emitted` or a written-down
/// `NoSuchConstruct`. This is the fixture whose `implements` row sat at 4.4 %
/// resolved for weeks (P3); the resolved-rate half of that lives in
/// `filigrio-resolve/tests/cross_backend_resolution.rs`.
pub const HERITAGE: Fixture = Fixture {
    name: "heritage",
    doc: "implements / extends / inherits",
    cases: &[
        Case {
            backend: Backend::Rust,
            src: HERITAGE_RUST,
            expect: &[
                (IMPLEMENTS, Expect::Emitted),
                (EXTENDS, Expect::Emitted),
                (
                    INHERITS,
                    Expect::NoSuchConstruct("Rust has no class inheritance"),
                ),
            ],
            kinds: &["file", "struct", "trait"],
        },
        Case {
            backend: Backend::Python,
            src: HERITAGE_PYTHON,
            expect: &[
                (
                    IMPLEMENTS,
                    Expect::NoSuchConstruct("Python has no interface/implements construct"),
                ),
                (
                    EXTENDS,
                    Expect::NoSuchConstruct("Python base classes are modelled as `inherits`"),
                ),
                (INHERITS, Expect::Emitted),
            ],
            kinds: &["file", "class"],
        },
        Case {
            backend: Backend::TypeScript,
            src: HERITAGE_TYPESCRIPT,
            expect: &[
                (IMPLEMENTS, Expect::Emitted),
                (EXTENDS, Expect::Emitted),
                (
                    INHERITS,
                    Expect::NoSuchConstruct("TS class/interface heritage is modelled as `extends`"),
                ),
            ],
            kinds: &["file", "class", "interface"],
        },
    ],
};

// ---- fixture 3: enumerations ----------------------------------------------

const ENUM_RUST: &str = r#"
pub enum Color {
    Red,
    Green,
}
"#;

const ENUM_PYTHON: &str = r#"
from enum import Enum


class Color(Enum):
    RED = 1
    GREEN = 2
"#;

const ENUM_TYPESCRIPT: &str = r#"
export enum Color {
  Red,
  Green,
}
"#;

/// `enum` + `enum_variant` + `has_variant`. This fixture is what makes the
/// `enum_variant` entry in [`NOT_LINKABLE_BY_DESIGN`] a *live* exclusion rather
/// than a comment: the corpus emits the kind, so the register is exercised.
pub const ENUMERATION: Fixture = Fixture {
    name: "enumeration",
    doc: "an enum and its variants",
    cases: &[
        Case {
            backend: Backend::Rust,
            src: ENUM_RUST,
            expect: &[(HAS_VARIANT, Expect::Emitted)],
            kinds: &["file", "enum", "enum_variant"],
        },
        Case {
            backend: Backend::Python,
            src: ENUM_PYTHON,
            expect: &[(HAS_VARIANT, Expect::Emitted)],
            kinds: &["file", "enum", "enum_variant"],
        },
        Case {
            backend: Backend::TypeScript,
            src: ENUM_TYPESCRIPT,
            expect: &[(HAS_VARIANT, Expect::Emitted)],
            kinds: &["file", "enum", "enum_variant"],
        },
    ],
};

/// The whole corpus. Every cross-backend property iterates this.
pub const CORPUS: &[&Fixture] = &[&SIGNATURE, &HERITAGE, &ENUMERATION, &ABSTRACTION];

// ---------------------------------------------------------------------------
// cross-backend behaviours that are not "is this relation emitted"
// ---------------------------------------------------------------------------

/// Does a cross-backend behaviour hold for one backend?
///
/// The same discipline as [`Expect`]: a backend is allowed to differ, but only
/// out loud. [`Holds::No`] is asserted to *still* be false, so repairing the
/// backend turns the row red and the register is corrected in the commit that
/// fixes the code — the register cannot quietly outlive the defect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Holds {
    Yes,
    /// A live divergence, with its diagnosis.
    No(&'static str),
    /// The language has no such construct, so the property is vacuous here.
    NotApplicable(&'static str),
}

/// The generic parameter every backend's `signature` fixture declares
/// (`fn bounded<T: Shape>`, `struct Holder<T: Shape>` and the equivalents).
pub const TYPE_PARAMETER: &str = "T";

/// **ADR-0036 R1.1 — a declared type parameter is not a type reference.**
///
/// `T` in `struct Holder<T: Shape> { item: T }` names nothing in the corpus; a
/// `type/field -> T` edge can never bind, so it is pure noise in exactly the
/// class R1.1 exists to remove ("read from the declaring `type_parameters` list",
/// per the status doc's Resolution & hygiene row). The `signature` fixture
/// declares `T` in both a function and a type in all three languages, so this is
/// a like-for-like comparison.
///
/// **This table is a finding.** R1.1's syntactic suppression is implemented on
/// the Rust backend only.
pub const TYPE_PARAMETER_SUPPRESSION: &[(Backend, Holds)] = &[
    (Backend::Rust, Holds::Yes),
    (
        Backend::Python,
        Holds::No(
            "PEP-695 type parameters (`def bounded[T: Shape]`, `class Holder[T: Shape]`) are \
             not registered as local type parameters, so `t: T` emits `type/param -> T` and \
             `item: T` emits `type/field -> T`. Distinct from the status doc's P1b, which is \
             about a `TypeVar` *imported* from another module; this one is declared in the \
             same file, two lines up",
        ),
    ),
    (
        Backend::TypeScript,
        Holds::Yes, // COMPLETED in P4: type parameters now correctly suppressed
    ),
];

/// **A generic declaration's bound is attributed to the declaration.**
///
/// Rust's `struct Holder<T: Shape>` yields `type/bound: Holder -> Shape`. The
/// status doc's P1c made the same claim for Python ("`type/bound` is sourced from
/// the *function* generic over the `TypeVar`") but only for functions — a PEP-695
/// *class* type parameter emits nothing.
pub const BOUND_ON_GENERIC_TYPE_DECL: &[(Backend, Holds)] = &[
    (Backend::Rust, Holds::Yes),
    (
        Backend::Python,
        Holds::No(
            "PEP-695 *class* type parameters emit no `type/bound`; P1c wired the bound walk \
             into the function path (`def bounded[T: Shape]` works) and `class Holder[T: Shape]` \
             was not covered",
        ),
    ),
    (
        Backend::TypeScript,
        Holds::No("TS `<T extends X>` bounds are unbuilt (status doc P5)"),
    ),
];

/// **An `enum_variant` node's label is the bare variant name.**
///
/// Rust and TypeScript label the variant `Red` and carry the owner in the `impl`
/// attr and in the id (`variant:<file>:Color::Red`, ADR-0028). The label is what
/// the agent-facing tools render, so a backend that folds the owner into it makes
/// the same enum read differently depending on the language it was written in.
pub const BARE_VARIANT_LABEL: &[(Backend, Holds)] = &[
    (Backend::Rust, Holds::Yes),
    (
        Backend::Python,
        Holds::No(
            "labels the variant `Color::RED` — the owner is folded into the label as well as \
             into the id and the `impl` attr, so the same enum renders differently in Python \
             than in Rust or TypeScript",
        ),
    ),
    (Backend::TypeScript, Holds::Yes),
];

/// The abstract **type** each [`ABSTRACTION`] case declares, and the bodiless
/// member it declares. The label is the same in all three languages by
/// construction — the fixture is one program written three times — so the
/// property test can name them once.
pub const DECLARING_TYPE: &str = "Store";
/// The bodiless member [`ABSTRACTION`] declares on [`DECLARING_TYPE`].
pub const DECLARATION: &str = "get";

// ---------------------------------------------------------------------------
// the deliberate-exclusion register (P3's defect, as a policy)
// ---------------------------------------------------------------------------

/// Node kinds a backend emits that are **deliberately not** link candidates, each
/// with the reason it is not.
///
/// `filigrio_resolve::is_linkable` is the authority for what *is* linkable; this
/// is the only sanctioned way to be outside it. A kind that is in neither is the
/// P3 defect — a definition that can be declared, exported and imported and still
/// never enter the link candidates, silently, with every per-backend test green.
///
/// Both directions are checked (`cross_backend_contract`): an entry here must
/// actually be emitted by some backend, and must actually be non-linkable, so the
/// register cannot rot into a list of excuses.
pub const NOT_LINKABLE_BY_DESIGN: &[(&str, &str)] = &[
    (
        "file",
        "a file is a container (`contains`), not a symbol; references bind to the \
         definitions inside it, and making files link candidates would let any \
         reference bind a path",
    ),
    (
        "enum_variant",
        "a variant is reached from its enum via `has_variant` (ADR-0036) and is \
         never an import target in its own right — `Color::Red` is addressed \
         through `Color`",
    ),
];

/// Is `kind` on the deliberate-exclusion register?
pub fn excluded_by_design(kind: &str) -> Option<&'static str> {
    NOT_LINKABLE_BY_DESIGN
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, why)| *why)
}

// ---------------------------------------------------------------------------
// extraction helpers
// ---------------------------------------------------------------------------

/// Extract `src` as `path` through the **production** dispatcher: `classify`
/// routes on the extension exactly as the pipeline does, so a fixture cannot
/// reach a backend by a route production has not got. Under `--features ts-oxc`
/// the TS slot is the oxc frontend, so this whole corpus runs against oxc in that
/// configuration for free (ADR-0040).
pub fn extract_at(path: &str, src: &str) -> Extraction {
    let artifact: Artifact = classify(path);
    let dispatch = DispatchExtractor::with_defaults();
    dispatch
        .extract(&artifact, src.as_bytes())
        .unwrap_or_else(|e| panic!("extraction failed for {path}: {e}"))
}

/// Extract one [`Case`] of a fixture.
pub fn extract_case(fixture: &Fixture, backend: Backend) -> Extraction {
    let case = fixture
        .case(backend)
        .unwrap_or_else(|| panic!("fixture `{}` has no {} case", fixture.name, backend.name()));
    extract_at(&fixture.path(backend), case.src)
}

/// The distinct node kinds in an extraction.
pub fn kinds(ex: &Extraction) -> BTreeSet<String> {
    ex.nodes.iter().map(|n| n.kind.clone()).collect()
}

/// The distinct edge relations in an extraction.
pub fn relations(ex: &Extraction) -> BTreeSet<String> {
    ex.edges.iter().map(|e| e.relation.clone()).collect()
}

/// Edges of one relation, rendered as `source -> target` for readable failures.
pub fn edges_of<'a>(ex: &'a Extraction, relation: &str) -> Vec<&'a Edge> {
    ex.edges.iter().filter(|e| e.relation == relation).collect()
}

/// The unresolved-symbol name an extraction edge points at (extraction always
/// emits `Symbol`; binding is the resolver's job).
pub fn target_name(e: &Edge) -> String {
    match &e.target {
        EdgeTarget::Symbol(t) => t.name.clone(),
        EdgeTarget::Node(id) => id.0.clone(),
    }
}

/// `(relation, target-name)` pairs — the comparable observable for frontend
/// parity, independent of node ids.
pub fn relation_targets(ex: &Extraction) -> BTreeSet<(String, String)> {
    ex.edges
        .iter()
        .map(|e| (e.relation.clone(), target_name(e)))
        .collect()
}

/// `(kind, label)` pairs — node identity without ids or spans.
pub fn kind_labels(ex: &Extraction) -> BTreeSet<(String, String)> {
    ex.nodes
        .iter()
        .map(|n| (n.kind.clone(), n.label.clone()))
        .collect()
}

/// A `Node` carrying just a kind — enough for `is_linkable`, which reads only
/// `kind`. Used to check the exclusion register against the authority.
pub fn probe_node(kind: &str) -> Node {
    Node::new(format!("probe:{kind}"), "probe", kind)
}
