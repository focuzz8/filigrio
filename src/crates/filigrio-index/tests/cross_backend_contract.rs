//! **Cross-backend conformance** for `filigrio-index` (status doc P2).
//!
//! One program, three languages, one table of expectations — see `common/mod.rs`
//! for the shape and the reasoning. Every test here is a property that must hold
//! *across* backends; none of them can be satisfied by looking at a single
//! backend, which is the whole point:
//!
//! | property | what it would have caught |
//! |---|---|
//! | [`every_emitted_node_kind_is_linkable_or_deliberately_excluded`] | **P3** — `interface`/`type_alias` missing from `LINKABLE` |
//! | [`adr0036_relations_are_emitted_consistently`] | a relation quietly dropped from one backend |
//! | [`a_declared_type_parameter_is_never_a_type_reference`] | ADR-0036 R1.1 implemented on one backend only |
//! | [`a_generic_declaration_carries_its_bound`] | a bound walk wired into the function path only |
//! | [`enum_variant_labels_are_bare`] | the same enum rendering differently per language |
//! | [`a_bodiless_declaration_is_a_linkable_node_carrying_the_abstract_fact`] | ADR-0036 §5 landing in one backend, or landing as an unlinkable kind |
//!
//! The resolved-rate half of P3 needs the `Engine` and therefore lives in
//! `filigrio-resolve/tests/cross_backend_resolution.rs`; see that file's header.

mod common;

use common::*;
use filigrio_core::relation::{
    is_type_family, BOUND_TYPE, CALLS, CONTAINS, DEPENDS_ON, EXTENDS, FIELD_TYPE, HAS_VARIANT,
    IMPLEMENTS, IMPORTS, INHERITS,
};
use filigrio_resolve::is_linkable;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// harness integrity — a table that can be one-sided proves nothing
// ---------------------------------------------------------------------------

/// Every fixture covers every backend, and every case of a fixture declares the
/// **same** relation keys.
///
/// This is the load-bearing rule. Without it, "TypeScript does not emit
/// `type/param`" could be expressed by *leaving the row out*, which is precisely
/// the silence the corpus exists to abolish: a divergence has to be spelled as
/// [`Expect::NoSuchConstruct`] or [`Expect::NotYetEmitted`], with a reason, or
/// the table does not compile as valid.
#[test]
fn fixture_tables_are_backend_complete() {
    for f in CORPUS {
        let keys: BTreeSet<&str> = f
            .cases
            .first()
            .map(|c| c.expect.iter().map(|(r, _)| *r).collect())
            .unwrap_or_default();
        assert!(!keys.is_empty(), "fixture `{}` expects nothing", f.name);

        for b in BACKENDS {
            let case = f.case(*b).unwrap_or_else(|| {
                panic!(
                    "fixture `{}` has no {} case — a one-sided fixture cannot express a \
                     cross-backend property",
                    f.name,
                    b.name()
                )
            });
            let mine: BTreeSet<&str> = case.expect.iter().map(|(r, _)| *r).collect();
            assert_eq!(
                mine.len(),
                case.expect.len(),
                "fixture `{}` / {}: a relation is listed twice",
                f.name,
                b.name()
            );
            assert_eq!(
                mine,
                keys,
                "fixture `{}` / {}: relation keys differ from the other backends' \
                 (missing {:?}, extra {:?}). Spell the difference as an `Expect` variant \
                 with a reason; do not omit the row",
                f.name,
                b.name(),
                keys.difference(&mine).collect::<Vec<_>>(),
                mine.difference(&keys).collect::<Vec<_>>(),
            );
            assert!(
                !case.kinds.is_empty(),
                "fixture `{}` / {} declares no node kinds",
                f.name,
                b.name()
            );
        }
    }
}

/// Each case's declared `kinds` is **exactly** what it emits.
///
/// Two-way on purpose. `emitted ⊆ declared` catches a backend that starts
/// minting a kind nobody registered — the P3 shape. `declared ⊆ emitted` stops
/// the declaration rotting into aspiration: a kind that no fixture actually
/// produces cannot be used to claim coverage.
#[test]
fn declared_node_kinds_are_exactly_what_is_emitted() {
    for f in CORPUS {
        for b in BACKENDS {
            let case = f.case(*b).expect("backend-complete");
            let emitted = kinds(&extract_case(f, *b));
            let declared: BTreeSet<String> = case.kinds.iter().map(|k| (*k).to_string()).collect();
            assert_eq!(
                emitted,
                declared,
                "fixture `{}` / {}: emitted kinds {:?} != declared {:?}",
                f.name,
                b.name(),
                emitted,
                declared
            );
        }
    }
}

// ---------------------------------------------------------------------------
// P3, as a property
// ---------------------------------------------------------------------------

/// **Every node kind any backend emits is a link candidate, or is on the
/// deliberate-exclusion register.**
///
/// This is the P3 defect expressed as a test. `filigrio_resolve::is_linkable` is
/// the authority — the *same* function the linker calls, not a copy of its list,
/// because a copy is what drifted. A kind that is in neither `LINKABLE` nor
/// [`NOT_LINKABLE_BY_DESIGN`] can be declared, exported and imported and still
/// never enter the link candidates: every reference to it is unresolvable, and
/// every per-backend test stays green because the edge *was* emitted.
///
/// It is cross-backend by construction: run over three backends, the failure
/// message names which backend is alone in minting an unlinkable kind, which is
/// the signal ("TypeScript alone failing on infrastructure Python and Rust use
/// successfully") that nobody had for weeks.
#[test]
fn every_emitted_node_kind_is_linkable_or_deliberately_excluded() {
    // kind → the backends that mint it, so the failure says who is out of step.
    let mut minted: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
    let mut offenders: Vec<String> = Vec::new();

    for f in CORPUS {
        for b in BACKENDS {
            for node in extract_case(f, *b).nodes {
                minted
                    .entry(node.kind.clone())
                    .or_default()
                    .insert(b.name());
                if is_linkable(&node) || excluded_by_design(&node.kind).is_some() {
                    continue;
                }
                offenders.push(format!(
                    "{} mints `{}` (e.g. `{}` in fixtures/{}.{}) — it is neither in \
                     filigrio-resolve's LINKABLE nor on NOT_LINKABLE_BY_DESIGN",
                    b.name(),
                    node.kind,
                    node.label,
                    f.name,
                    b.ext(),
                ));
            }
        }
    }

    offenders.sort();
    offenders.dedup();
    assert!(
        offenders.is_empty(),
        "unlinkable node kinds (the P3 defect: a definition that can be declared, \
         exported and imported and still never become a link candidate):\n  {}\n\
         kinds minted per backend: {:?}\n\
         Fix by adding the kind to LINKABLE, or — if it is genuinely never a \
         reference target — to NOT_LINKABLE_BY_DESIGN with the reason.",
        offenders.join("\n  "),
        minted,
    );
}

/// The exclusion register is **live and consistent**: every entry is actually
/// minted by some backend in the corpus, and none of them is linkable.
///
/// Without the first half the register is a list of excuses nobody exercises —
/// and an exclusion that no fixture reaches proves nothing about the linker.
/// Without the second half the register could contradict `LINKABLE` and hide a
/// real regression behind a stale comment.
#[test]
fn exclusion_register_is_live_and_consistent() {
    let minted: BTreeSet<String> = CORPUS
        .iter()
        .flat_map(|f| BACKENDS.iter().map(move |b| (f, b)))
        .flat_map(|(f, b)| kinds(&extract_case(f, *b)))
        .collect();

    for (kind, why) in NOT_LINKABLE_BY_DESIGN {
        assert!(
            minted.contains(*kind),
            "`{kind}` is on the exclusion register but no fixture emits it — an \
             exclusion the corpus never reaches proves nothing. Add a fixture or \
             drop the entry. (minted: {minted:?})"
        );
        assert!(
            !is_linkable(&probe_node(kind)),
            "`{kind}` is on the exclusion register ({why}) *and* in \
             filigrio-resolve's LINKABLE — the register and the authority \
             contradict each other"
        );
    }
}

// ---------------------------------------------------------------------------
// the ADR-0036 relation family
// ---------------------------------------------------------------------------

/// **The ADR-0036 relation family is emitted consistently across backends.**
///
/// For a field, a parameter, a return, a bound and each heritage relation, the
/// same relation appears in every language that has the construct. A language
/// that genuinely lacks it is [`Expect::NoSuchConstruct`]; a backend that has the
/// construct and does not emit it yet is [`Expect::NotYetEmitted`] and is
/// asserted *absent*, so closing the gap fails this test and the table is
/// corrected in the same commit as the extractor.
#[test]
fn adr0036_relations_are_emitted_consistently() {
    for f in CORPUS {
        for b in BACKENDS {
            let case = f.case(*b).expect("backend-complete");
            let ex = extract_case(f, *b);
            let seen = relations(&ex);
            for (relation, expect) in case.expect {
                let present = seen.contains(*relation);
                match expect {
                    Expect::Emitted => assert!(
                        present,
                        "fixture `{}` / {}: `{relation}` must be emitted but was not. \
                         Emitted: {seen:?}",
                        f.name,
                        b.name(),
                    ),
                    Expect::NoSuchConstruct(why) | Expect::NotYetEmitted(why) => assert!(
                        !present,
                        "fixture `{}` / {}: `{relation}` is declared absent ({why}) but the \
                         backend emitted it: {:?}. If the gap has closed, flip the row to \
                         `Expect::Emitted`",
                        f.name,
                        b.name(),
                        edges_of(&ex, relation)
                            .iter()
                            .map(|e| format!("{} -> {}", e.source.0, target_name(e)))
                            .collect::<Vec<_>>(),
                    ),
                }
            }
        }
    }
}

/// Every relation any backend emits is one the kernel names.
///
/// Relations are an open vocabulary by design (ADR-0021 "do NOT enum"), which is
/// exactly why a backend can invent one by typo and nothing notices. The query
/// layer only reasons about the constants in `filigrio_core::relation`, so a
/// relation outside them is invisible to `god_nodes` and to `get_neighbors`
/// filters — emitted, stored, and unreachable.
#[test]
fn every_emitted_relation_is_in_the_kernel_vocabulary() {
    const KNOWN: &[&str] = &[
        CALLS,
        CONTAINS,
        IMPORTS,
        DEPENDS_ON,
        IMPLEMENTS,
        EXTENDS,
        INHERITS,
        HAS_VARIANT,
        FIELD_TYPE,
        BOUND_TYPE,
        filigrio_core::relation::PARAM_TYPE,
        filigrio_core::relation::RETURN_TYPE,
    ];
    for f in CORPUS {
        for b in BACKENDS {
            for r in relations(&extract_case(f, *b)) {
                assert!(
                    KNOWN.contains(&r.as_str()),
                    "fixture `{}` / {} emits `{r}`, which is not a `filigrio_core::relation` \
                     constant. A relation the kernel does not name is invisible to the query \
                     layer's filters and to god-node ranking{}",
                    f.name,
                    b.name(),
                    if is_type_family(&r) {
                        " (it does carry the `type/` prefix, so add the constant)"
                    } else {
                        ""
                    },
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// cross-backend behaviours beyond "is the relation there"
// ---------------------------------------------------------------------------

/// Assert one [`Holds`] row: `actual` is what the backend does.
fn check(rows: &[(Backend, Holds)], property: &str, actual: impl Fn(Backend) -> Option<String>) {
    assert_eq!(
        rows.len(),
        BACKENDS.len(),
        "`{property}` must declare a row per backend"
    );
    for b in BACKENDS {
        let (_, holds) = rows
            .iter()
            .find(|(rb, _)| rb == b)
            .unwrap_or_else(|| panic!("`{property}` has no {} row", b.name()));
        let violation = actual(*b);
        match holds {
            Holds::Yes | Holds::NotApplicable(_) => assert!(
                violation.is_none(),
                "{}: `{property}` is declared to hold but does not — {}",
                b.name(),
                violation.unwrap_or_default(),
            ),
            Holds::No(diagnosis) => assert!(
                violation.is_some(),
                "{}: `{property}` is registered as a live divergence ({diagnosis}) but the \
                 backend now satisfies it. Flip the row to `Holds::Yes` in the same commit \
                 as the fix",
                b.name(),
            ),
        }
    }
}

/// **ADR-0036 R1.1 — a declared type parameter is not a type reference.**
///
/// `T` in `struct Holder<T: Shape> { item: T }` names nothing; a `type/field -> T`
/// edge can never bind. R1.1 suppresses it by reading the declaring
/// `type_parameters` list — syntactically, which is what makes it a per-backend
/// walk rather than shared infrastructure, and therefore exactly the kind of thing
/// one backend can be missing without anyone noticing.
///
/// **This test currently records two live divergences** (Python, TypeScript);
/// see [`TYPE_PARAMETER_SUPPRESSION`] for each diagnosis.
#[test]
fn a_declared_type_parameter_is_never_a_type_reference() {
    check(
        TYPE_PARAMETER_SUPPRESSION,
        "type-parameter suppression (ADR-0036 R1.1)",
        |b| {
            let ex = extract_case(&SIGNATURE, b);
            let leaks: Vec<String> = ex
                .edges
                .iter()
                .filter(|e| is_type_family(&e.relation) && target_name(e) == TYPE_PARAMETER)
                .map(|e| format!("{} {} -> {}", e.source.0, e.relation, target_name(e)))
                .collect();
            (!leaks.is_empty()).then(|| leaks.join(", "))
        },
    );
}

/// **A generic type declaration carries its own bound.**
///
/// `struct Holder<T: Shape>` must yield `type/bound: Holder -> Shape`, the same
/// way `fn bounded<T: Shape>` yields it from the function. Asserting on the
/// *source* node is what separates this from the relation table above, which is
/// satisfied by the function-position edge alone — and that is how a bound walk
/// wired into only one of the two declaration sites stays hidden.
#[test]
fn a_generic_declaration_carries_its_bound() {
    check(
        BOUND_ON_GENERIC_TYPE_DECL,
        "type/bound from a generic type declaration",
        |b| {
            let ex = extract_case(&SIGNATURE, b);
            let from_holder = edges_of(&ex, BOUND_TYPE)
                .iter()
                .any(|e| e.source.0.ends_with(":Holder") && target_name(e) == "Shape");
            (!from_holder).then(|| {
                format!(
                    "no `type/bound: Holder -> Shape`; type/bound edges present: {:?}",
                    edges_of(&ex, BOUND_TYPE)
                        .iter()
                        .map(|e| format!("{} -> {}", e.source.0, target_name(e)))
                        .collect::<Vec<_>>()
                )
            })
        },
    );
}

/// **An `enum_variant` node's label is the bare variant name.**
///
/// The owner belongs in the `impl` attr and in the id (`variant:<file>:Color::Red`,
/// ADR-0028) — the label is what the agent-facing tools render, so folding the
/// owner into it makes the same enum read differently depending on which language
/// it was written in. A per-backend test cannot see that; it only sees a label it
/// chose itself.
#[test]
fn enum_variant_labels_are_bare() {
    check(BARE_VARIANT_LABEL, "bare enum_variant labels", |b| {
        let ex = extract_case(&ENUMERATION, b);
        let qualified: Vec<String> = ex
            .nodes
            .iter()
            .filter(|n| n.kind == "enum_variant" && n.label.contains("::"))
            .map(|n| n.label.clone())
            .collect();
        (!qualified.is_empty()).then(|| format!("owner-qualified labels: {qualified:?}"))
    });
}

/// Every `enum_variant` is reachable from its enum by `has_variant`.
///
/// The exclusion register's justification for `enum_variant` — "reached via
/// `has_variant`, never an import target" — is a *claim about the graph*. If a
/// backend minted a variant without the edge, the exclusion would strand it:
/// unlinkable by policy and unreachable in fact.
#[test]
fn every_variant_is_reachable_from_its_enum() {
    for b in BACKENDS {
        let ex = extract_case(&ENUMERATION, *b);
        let reached: BTreeSet<String> = edges_of(&ex, HAS_VARIANT)
            .iter()
            .map(|e| target_name(e))
            .collect();
        for n in ex.nodes.iter().filter(|n| n.kind == "enum_variant") {
            assert!(
                reached.contains(&n.id.0),
                "{}: variant `{}` ({}) has no `has_variant` edge, so it is unlinkable by \
                 policy *and* unreachable in fact. Reached: {reached:?}",
                b.name(),
                n.label,
                n.id.0,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// ADR-0036 §5 — the abstraction is a node
// ---------------------------------------------------------------------------

/// **A bodiless declaration is a node, it carries `abstract`, and it is
/// linkable** — in every backend that has the construct.
///
/// Three claims, and each one is a different way for §5 to be built wrong:
///
/// 1. **The node exists.** The measurement behind §5 is that next.js's `Server`
///    ranks #1762 with semantic degree 13 *because* its 34 abstract method
///    signatures are not nodes, so nothing calling through the abstraction can
///    bind to it. A backend that skips them keeps that defect for its language.
/// 2. **The kind is `function`, and it is a link candidate.** This is the P3
///    check applied before the fact: `filigrio_resolve::is_linkable` is the
///    authority the linker itself calls, so an `abstract_method` /
///    `method_signature` kind would be a definition that can be declared,
///    exported and imported and still never bind — the node would exist and
///    change nothing. That is exactly why §5 chose Kythe's node fact over SCIP's
///    six kind variants, and the assertion is the one P3 taught us to write
///    rather than assume.
/// 3. **The declaring type contains it.** A declaration reachable only by name
///    is not an interface; "traverse the abstraction to its operations" is the
///    query §5 unblocks, and §4 already had to fix exactly this for Rust methods.
///
/// Cross-backend by construction: three languages, one fixture, one property. A
/// per-backend test can (and did, for weeks, in P3's case) stay green while one
/// language alone is failing on infrastructure the others use successfully.
#[test]
fn a_bodiless_declaration_is_a_linkable_node_carrying_the_abstract_fact() {
    for b in BACKENDS {
        let ex = extract_case(&ABSTRACTION, *b);

        let decl = ex
            .nodes
            .iter()
            .find(|n| n.label == DECLARATION && n.kind != "file")
            .unwrap_or_else(|| {
                panic!(
                    "{}: the bodiless `{DECLARATION}` is not a node — the abstraction \
                     cannot be ranked or bound to. Nodes: {:?}",
                    b.name(),
                    kind_labels(&ex),
                )
            });

        assert_eq!(
            decl.kind,
            "function",
            "{}: a declaration's kind must stay `function` (ADR-0036 §5 — a node \
             fact, not a node kind)",
            b.name(),
        );
        assert_eq!(
            decl.attrs.get("abstract").map(String::as_str),
            Some("true"),
            "{}: `{DECLARATION}` is a node but carries no `abstract` fact, so nothing \
             downstream can tell a declaration from a definition. attrs: {:?}",
            b.name(),
            decl.attrs,
        );
        assert!(
            is_linkable(decl),
            "{}: `{DECLARATION}` is not a link candidate (kind `{}`), so every call \
             through the abstraction stays unresolved — the node exists and changes \
             nothing. This is the P3 defect: add the kind to LINKABLE",
            b.name(),
            decl.kind,
        );

        let owner = ex
            .nodes
            .iter()
            .find(|n| n.label == DECLARING_TYPE && n.kind != "function")
            .unwrap_or_else(|| panic!("{}: no `{DECLARING_TYPE}` node", b.name()));
        assert_eq!(
            owner.attrs.get("abstract").map(String::as_str),
            Some("true"),
            "{}: the declaring type `{DECLARING_TYPE}` must carry the fact too — \
             Python's ABC/`Protocol` is a plain `class` node, so the fact is the \
             only thing that marks it. attrs: {:?}",
            b.name(),
            owner.attrs,
        );
        assert!(
            ex.edges.iter().any(|e| {
                e.relation == CONTAINS
                    && e.source == owner.id
                    && matches!(&e.target, filigrio_core::EdgeTarget::Node(t) if *t == decl.id)
            }),
            "{}: `{DECLARING_TYPE}` does not contain `{DECLARATION}` — a declaration \
             reachable only by name is not an interface. contains edges: {:?}",
            b.name(),
            edges_of(&ex, CONTAINS)
                .iter()
                .map(|e| format!("{} -> {}", e.source.0, target_name(e)))
                .collect::<Vec<_>>(),
        );
    }
}

/// **A definition is not marked abstract** — the half that keeps the fact
/// meaningful.
///
/// Read from the [`SIGNATURE`] fixture, whose functions all have bodies in all
/// three languages. A backend that marked every method (say, by stamping the
/// fact in `add_def`) would pass the property above in full and be useless:
/// `abstract` would select everything. The Python rule is the one with real room
/// to over-reach — it must read a *placeholder* body rather than "a short body"
/// — so this is where a rule that swept in `pass` or a docstring-only method
/// would show up.
///
/// Scoped to `function` nodes on purpose. The fixture's `trait Shape` /
/// `interface Shape` *are* abstract types and are marked; that is the type half
/// of §5, asserted positively above.
#[test]
fn a_definition_with_a_body_is_never_marked_abstract() {
    for b in BACKENDS {
        let ex = extract_case(&SIGNATURE, *b);
        let marked: Vec<String> = ex
            .nodes
            .iter()
            .filter(|n| n.kind == "function" && n.attrs.contains_key("abstract"))
            .map(|n| format!("{} ({})", n.id.0, n.kind))
            .collect();
        assert!(
            marked.is_empty(),
            "{}: every function in the `signature` fixture has a body, but these \
             carry `abstract`: {marked:?}",
            b.name(),
        );
    }
}
