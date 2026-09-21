//! **Cross-backend resolved-rate parity** (status doc P2, property 3).
//!
//! ## Why this lives in `filigrio-resolve` and not in `filigrio-index`
//!
//! P2 asked for a `filigrio-index` integration surface, and most of it is there
//! (`filigrio-index/tests/cross_backend_contract.rs`). This property is the one
//! piece that cannot be: *resolution* is what P3 broke, and resolution needs the
//! `Engine`, the export tables, the module resolvers and the symbol index — the
//! entire `filigrio-resolve` apply path. Extraction was **already correct** while
//! `implements` sat at 4.4 % resolved; the defect was only ever visible on the
//! far side of `Engine::apply`. So the property goes where the machinery is, and
//! it reuses this crate's `tests/common` harness (`DirSource` + `cold_ext`) that
//! `ts_type_linking.rs` — the P3 reproduction — already uses.
//!
//! ## The property
//!
//! The same small program in Rust, Python and TypeScript: a set of types defined
//! in one file and referenced from another as a **parameter**, a **return** and a
//! **field**, plus the heritage forms each language has. Every reference names
//! something in the corpus, so every backend should bind every one of them.
//!
//! What makes it the assertion that would have caught P3 is that it is measured
//! **per backend and compared**. TypeScript alone was failing on infrastructure
//! Rust and Python used successfully — a per-backend threshold would have been
//! tuned to whatever TypeScript happened to score. A *comparison* has nothing to
//! tune: the reference language is the other language.
//!
//! Reported per **definition kind**, too, because that is the axis the defect lay
//! on: `class` bound and `interface` did not, in the same file, through the same
//! import. A rate averaged over kinds would have read 47 % and looked like
//! "TypeScript is a bit weaker", which is exactly how it survived.

mod common;

use common::{cold_ext, DirSource};
use filigrio_core::relation::is_type_family;
use filigrio_core::{
    relation::{EXTENDS, IMPLEMENTS, IMPORTS, INHERITS},
    Edge, EdgeTarget, Extractor, GraphState, Node,
};
use filigrio_index::DispatchExtractor;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

// ---------------------------------------------------------------------------
// the equivalent program, three times
// ---------------------------------------------------------------------------

struct Program {
    lang: &'static str,
    exts: &'static [&'static str],
    /// The consumer file, whose outbound references are the measured population.
    consumer: &'static str,
    files: &'static [(&'static str, &'static str)],
    /// Definition kinds the *defs* file mints, each of which a cross-file
    /// reference must be able to bind. This is the P3 axis.
    defined_kinds: &'static [&'static str],
}

const RUST: Program = Program {
    lang: "rust",
    exts: &["rs"],
    consumer: "src/consumer.rs",
    defined_kinds: &["struct", "trait"],
    files: &[
        (
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
        ),
        ("src/lib.rs", "pub mod defs;\npub mod consumer;\n"),
        (
            "src/defs.rs",
            "pub struct Widget;\n\
             pub struct WidgetId;\n\
             pub trait Shape {}\n",
        ),
        (
            "src/consumer.rs",
            "use crate::defs::{Shape, Widget, WidgetId};\n\
             \n\
             pub struct Holder {\n    pub item: Widget,\n}\n\
             \n\
             impl Shape for Holder {}\n\
             \n\
             pub fn take(w: Widget) -> WidgetId {\n    WidgetId\n}\n",
        ),
    ],
};

const PYTHON: Program = Program {
    lang: "python",
    exts: &["py"],
    consumer: "pkg/consumer.py",
    defined_kinds: &["class"],
    files: &[
        ("pyproject.toml", "[project]\nname = \"fixture\"\n"),
        (
            "pkg/defs.py",
            "class Widget: ...\n\n\nclass WidgetId: ...\n\n\nclass Shape: ...\n",
        ),
        (
            "pkg/consumer.py",
            "from .defs import Shape, Widget, WidgetId\n\
             \n\
             \n\
             class Holder(Shape):\n    item: Widget\n\
             \n\
             \n\
             def take(w: Widget) -> WidgetId:\n    return WidgetId()\n",
        ),
    ],
};

const TYPESCRIPT: Program = Program {
    lang: "typescript",
    exts: &["ts", "tsx"],
    consumer: "src/consumer.ts",
    // The two kinds P3 was about (`interface`, `type_alias`) *and* the control
    // (`class`) that kept working, in one file — the de-confounding the ADR needed.
    defined_kinds: &["class", "interface", "type_alias"],
    files: &[
        ("package.json", "{\"name\":\"fixture\"}"),
        (
            "src/defs.ts",
            "export class Widget {}\n\
             export class WidgetId {}\n\
             export interface Shape { area: number }\n\
             export type WidgetName = string\n",
        ),
        (
            "src/consumer.ts",
            "import { Shape, Widget, WidgetId, WidgetName } from './defs'\n\
             \n\
             export class Holder implements Shape {\n\
             \x20 item: Widget\n\
             \x20 id: WidgetId\n\
             \x20 name: WidgetName\n\
             \x20 area = 0\n\
             }\n\
             \n\
             export function take(w: Widget): WidgetId {\n\
             \x20 return new WidgetId()\n\
             }\n",
        ),
    ],
};

const PROGRAMS: &[&Program] = &[&RUST, &PYTHON, &TYPESCRIPT];

// ---------------------------------------------------------------------------
// measurement
// ---------------------------------------------------------------------------

fn build(p: &Program) -> GraphState {
    let dir = tempfile::tempdir().expect("tempdir");
    for (rel, body) in p.files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).expect("mkdir");
        fs::write(path, body).expect("write");
    }
    let dispatch = DispatchExtractor::with_defaults();
    let ext: &dyn Extractor = &dispatch;
    cold_ext(&DirSource::load_ext(dir.path(), p.exts), ext)
}

/// The measured population: **semantic, cross-file** references leaving the
/// consumer file — the type-reference family plus imports and heritage.
///
/// `contains` and `calls` are excluded deliberately. `contains` is structural and
/// resolved at extraction, so including it would dilute the rate with edges that
/// cannot fail; `calls` is a different resolver path (receiver typing, ADR-0026)
/// with its own homonym story, and P3's whole point was that `calls` looked fine
/// while type references did not.
fn measured(state: &GraphState, consumer: &str) -> Vec<Edge> {
    let in_consumer: Vec<&Node> = state
        .graph
        .nodes
        .iter()
        .filter(|n| n.source_file.as_deref() == Some(consumer))
        .collect();
    state
        .graph
        .edges
        .iter()
        .filter(|e| {
            (is_type_family(&e.relation)
                || matches!(
                    e.relation.as_str(),
                    IMPORTS | IMPLEMENTS | EXTENDS | INHERITS
                ))
                && in_consumer.iter().any(|n| n.id == e.source)
        })
        .cloned()
        .collect()
}

fn is_resolved(e: &Edge) -> bool {
    matches!(e.target, EdgeTarget::Node(_))
}

fn render(edges: &[Edge]) -> Vec<String> {
    let mut v: Vec<String> = edges
        .iter()
        .map(|e| {
            let t = match &e.target {
                EdgeTarget::Node(id) => format!("-> {id}"),
                EdgeTarget::Symbol(s) => format!("UNRESOLVED {}", s.name),
            };
            format!("{} {} {t}", e.source.0, e.relation)
        })
        .collect();
    v.sort();
    v
}

/// One backend's resolved-rate reading over the measured population.
struct Rate {
    resolved: usize,
    total: usize,
    /// relation → `(resolved, total)`. The per-relation split is the reading that
    /// matters: an average over relations is how `implements` at 4.4 % hid behind
    /// a healthy `imports` (P3).
    per_relation: BTreeMap<String, (usize, usize)>,
    /// Every measured edge, rendered for a readable failure.
    rendered: Vec<String>,
}

impl Rate {
    fn percent(&self) -> f64 {
        100.0 * self.resolved as f64 / self.total as f64
    }
}

fn rate(state: &GraphState, consumer: &str) -> Rate {
    let edges = measured(state, consumer);
    let mut per_relation: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for e in &edges {
        let slot = per_relation.entry(e.relation.clone()).or_insert((0, 0));
        slot.1 += 1;
        if is_resolved(e) {
            slot.0 += 1;
        }
    }
    Rate {
        resolved: edges.iter().filter(|e| is_resolved(e)).count(),
        total: edges.len(),
        per_relation,
        rendered: render(&edges),
    }
}

// ---------------------------------------------------------------------------
// the properties
// ---------------------------------------------------------------------------

/// Every backend binds **every** cross-file reference in the equivalent program.
///
/// The fixture is self-contained — every referenced name is defined in the
/// sibling file — so there is no honest-decline population to argue about
/// (ADR-0023). Anything short of 100 % is a resolver gap, and because the rate is
/// computed identically for three backends the failure message says which one is
/// out of step rather than just "the number is low".
#[test]
fn every_backend_binds_every_cross_file_reference() {
    let mut table: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    for p in PROGRAMS {
        let state = build(p);
        let r = rate(&state, p.consumer);
        assert!(
            r.total > 0,
            "{}: the fixture produced no measurable cross-file references — the \
             comparison would be vacuous. Nodes: {:?}",
            p.lang,
            state
                .graph
                .nodes
                .iter()
                .map(|n| format!("{}:{}", n.kind, n.label))
                .collect::<Vec<_>>()
        );
        table.push(format!(
            "{:>11}: {}/{} ({:.0} %) {:?}",
            p.lang,
            r.resolved,
            r.total,
            r.percent(),
            r.per_relation,
        ));
        if r.resolved != r.total {
            failures.push(format!("{}:\n    {}", p.lang, r.rendered.join("\n    ")));
        }
    }

    assert!(
        failures.is_empty(),
        "cross-backend resolved-rate parity broken.\n  {}\n\nunbound references:\n  {}",
        table.join("\n  "),
        failures.join("\n  "),
    );
}

/// **A cross-file reference binds to a definition of every kind the backend
/// mints.** This is P3 at resolution level, on the axis the defect actually lay
/// on.
///
/// A rate averaged over kinds hides it: with `interface` unlinkable, TypeScript's
/// `class` references still bound, so the average moved but never to zero. Split
/// by kind, the failing kind reads 0/n and names itself. `type_alias` and
/// `interface` are in TypeScript's list precisely because those are the two kinds
/// ADR-0037b introduced and `LINKABLE` never learned about.
#[test]
fn a_reference_binds_to_a_definition_of_every_kind() {
    for p in PROGRAMS {
        let state = build(p);
        let kind_of: BTreeMap<&str, &str> = state
            .graph
            .nodes
            .iter()
            .map(|n| (n.id.0.as_str(), n.kind.as_str()))
            .collect();

        // kind → how many measured references bound to a node of that kind.
        let mut bound: BTreeMap<&str, usize> = BTreeMap::new();
        for e in measured(&state, p.consumer) {
            if let EdgeTarget::Node(id) = &e.target {
                if let Some(kind) = kind_of.get(id.0.as_str()) {
                    *bound.entry(kind).or_default() += 1;
                }
            }
        }

        for kind in p.defined_kinds {
            assert!(
                bound.get(kind).copied().unwrap_or(0) > 0,
                "{}: no cross-file reference bound to any `{kind}` definition. That is the \
                 P3 shape — a kind that can be declared, exported and imported and still \
                 never enter the link candidates (check `LINKABLE` / `is_linkable`).\n  \
                 bound by kind: {bound:?}\n  references:\n    {}",
                p.lang,
                rate(&state, p.consumer).rendered.join("\n    "),
            );
        }
    }
}

/// The three positions ADR-0036 distinguishes — **field**, **param**, **return** —
/// bind in every backend that emits them.
///
/// Emission is the status doc's P4 (TypeScript has no `type/param` / `type/return`
/// yet), so this asserts over what is *emitted*: for each backend, every position
/// it produces at all must be fully bound. Once P4 lands, TypeScript's rows appear
/// here automatically and are held to the same bar as Rust's and Python's — no
/// edit to this test.
#[test]
fn every_emitted_signature_position_binds() {
    use filigrio_core::relation::{FIELD_TYPE, PARAM_TYPE, RETURN_TYPE};

    let mut summary: Vec<String> = Vec::new();
    for p in PROGRAMS {
        let state = build(p);
        let r = rate(&state, p.consumer);
        for position in [FIELD_TYPE, PARAM_TYPE, RETURN_TYPE] {
            let Some((resolved, total)) = r.per_relation.get(position).copied() else {
                summary.push(format!("{:>11} {position}: not emitted", p.lang));
                continue;
            };
            summary.push(format!("{:>11} {position}: {resolved}/{total}", p.lang));
            assert_eq!(
                resolved,
                total,
                "{}: `{position}` emitted {total} cross-file references and bound only \
                 {resolved}. Every target is defined in the sibling file, so an unbound one \
                 is a resolver gap, not an honest decline.\n  {}\n  positions: {}",
                p.lang,
                r.rendered.join("\n  "),
                summary.join(" | "),
            );
        }
    }
    assert!(
        summary.iter().any(|s| s.contains("type/param")),
        "no backend emitted a cross-file `type/param` — the fixture has stopped exercising \
         the position it exists to measure: {summary:?}"
    );
}
