//! The edge-relation vocabulary — one home for the relation strings so the
//! structural/semantic taxonomy (ADR-0021) is not stringly-typed across crates.
//!
//! Edge `relation`s stay `String` in the model (open vocabulary — a new language
//! or edge kind adds a relation without touching core, mirroring the open
//! `Node.kind`, ADR-0021 "do NOT enum"). These constants name the ones the
//! engine and query layer reason about, so a typo is a compile error, not a
//! silent mis-link.
//!
//! **Structural vs semantic** (ADR-0021 decision 2, extended by ADR-0036):
//! *structural* edges express **physical** containment/wiring (`contains`,
//! `imports`, `depends_on`, `has_variant`); *semantic* edges express **code
//! meaning** (`calls`, and the ADR-0036 type-relation family `implements` /
//! `extends` / `inherits` / `type/field` / `type/param` / …). [`is_structural`] is
//! the single source of that split — the query layer excludes structural edges from
//! god-node degree so files never dominate the ranking, while a widely-implemented
//! trait / widely-used type *does* rank as a hub (a real architectural fact).

/// A function/method invocation — the primary *semantic* edge.
pub const CALLS: &str = "calls";
/// Physical containment: a file contains a def, a project contains a file.
pub const CONTAINS: &str = "contains";
/// A module import (`import … from …`, `use …`) — physical wiring.
pub const IMPORTS: &str = "imports";
/// A project → project dependency (the monorepo architecture map).
pub const DEPENDS_ON: &str = "depends_on";

// ---- ADR-0036 structural extraction contract (type/interface relations) -----

/// A type → the interface/trait it implements (`impl Trait for T`, `class C
/// implements I`). *Semantic* — a widely-implemented trait is a real hub.
pub const IMPLEMENTS: &str = "implements";
/// A type → a supertype it extends (`class`/`interface extends`, Rust supertrait
/// `trait A: B`). *Semantic*.
pub const EXTENDS: &str = "extends";
/// A subtype → its base (Python base classes). *Semantic*.
pub const INHERITS: &str = "inherits";
/// The hierarchical prefix shared by the **type-reference family** — the
/// relations that say *"this code position references that type"*, distinguished
/// by the position (ADR-0036 R2, named in R2.1).
///
/// The precedent is **Kythe**, whose edge kinds are hierarchical with meaningful
/// prefixes — `/kythe/edge/ref`, `ref/call`, `ref/imports`, `defines/binding`,
/// `overrides/transitive`. The prefix exists so the family **closes under
/// extension**: `relations=["type"]` prefix-matches every member, and a future
/// member (`type/decorator`, ADR-0036 R5.2) joins it *structurally* rather than
/// via an alias table someone must remember to update. Enumerating the members
/// in a client filter silently under-reports the moment one is added; matching
/// the prefix does not.
///
/// Only this family is prefixed. `implements`/`extends`/`inherits`/`has_variant`
/// stay flat — they are separate queries, not a family with a closure problem.
///
/// Use [`is_type_family`] / the query layer's relation matcher rather than
/// hand-rolling `starts_with`.
pub const TYPE_FAMILY_PREFIX: &str = "type/";

/// An owning type → a declared field's type (field name/visibility ride the edge
/// as attrs; there is **no field node**, ADR-0036 §1a). *Semantic*.
pub const FIELD_TYPE: &str = "type/field";
/// An enum → one of its `enum_variant` nodes (containment). *Structural*.
pub const HAS_VARIANT: &str = "has_variant";
/// A function/method's parameter → its declared type. *Semantic*.
pub const PARAM_TYPE: &str = "type/param";
/// A function/method's return position → its declared type. *Semantic*.
pub const RETURN_TYPE: &str = "type/return";
/// A type → a trait bound appearing in its type parameters or where clause. *Semantic*.
pub const BOUND_TYPE: &str = "type/bound";

/// Whether `relation` belongs to the [`TYPE_FAMILY_PREFIX`] family — i.e. it is a
/// type reference distinguished by position (ADR-0036 R2.1). True for every
/// current member *and* for members that do not exist yet, which is the point of
/// the prefix.
pub fn is_type_family(relation: &str) -> bool {
    relation.starts_with(TYPE_FAMILY_PREFIX)
}

/// The **query-filter vocabulary** (ADR-0044) — words a *caller* may put in a
/// relation-filter set that are **not relations**.
///
/// They live in their own namespace, and not beside [`CALLS`]/[`IMPORTS`]
/// above, because the distinction is the whole point: no extractor emits one,
/// no [`crate::Edge`] ever carries one, and nothing in the graph is named
/// `any` or `semantic`. They exist only in the space between a client's
/// `relations` array and [`relation_matches_filter`] — which is where the
/// **filter contract** lives, hence this module sitting next to the matcher
/// rather than in a transport crate where the two surfaces could drift.
///
/// Why they exist at all: a grammar-constrained decoder *selects* declared
/// values and cannot reason its way to *"if I omit this field, structural edges
/// get dropped"*. Anything that changes the answer must be nameable, so the
/// overloaded empty array is split into two names the caller can actually emit.
pub mod filter {
    /// **Every relation, structural scaffolding included** — the widest filter,
    /// exactly equivalent to applying no filter at all.
    pub const ANY: &str = "any";
    /// **The curated default**: code meaning (`calls`, `implements`, the whole
    /// `type/…` family) with the physical `contains`/`imports`/`depends_on`
    /// scaffolding dropped — i.e. [`super::is_structural`] inverted, which is
    /// the one home for that split. This is what an omitted or empty filter
    /// means (see [`normalize`]).
    pub const SEMANTIC: &str = "semantic";

    /// Both filter words, in the order a schema should advertise them: widest
    /// first, then the default. A tool's `relations` enum is this slice
    /// concatenated with the relation constants, so a rename here is a compile
    /// error at every declaration site rather than schema drift.
    pub const VOCABULARY: &[&str] = &[ANY, SEMANTIC];

    /// **The one home for the empty case.** An absent or empty filter set means
    /// [`SEMANTIC`] (ADR-0044) — a caller asking for "whatever is there" through
    /// absence gets the curated answer, and now gets it *identically* on every
    /// surface.
    ///
    /// This function exists because that was not true: `get_neighbors` read an
    /// empty array as "drop structural" and `query_graph` read the same empty
    /// array as "no filter at all" — identical input, opposite meanings, on the
    /// two tools an agent uses most. Every read surface normalizes here, so the
    /// two cannot answer the same empty array differently again.
    ///
    /// Borrows when there is nothing to change, so the common (explicit-filter)
    /// path allocates nothing.
    pub fn normalize(entries: &[String]) -> std::borrow::Cow<'_, [String]> {
        if entries.is_empty() {
            std::borrow::Cow::Owned(vec![SEMANTIC.to_string()])
        } else {
            std::borrow::Cow::Borrowed(entries)
        }
    }
}

/// Whether a client-supplied relation-filter `entry` selects `relation`
/// (**ADR-0036 R2.1** — the one home for filter matching; do not re-implement it
/// with `==` or an ad-hoc `starts_with` at a call site).
///
/// Three ways to match, and only three:
///
/// 0. **As a [`filter`] word** — [`filter::ANY`] selects every relation,
///    [`filter::SEMANTIC`] selects every non-[`is_structural`] one. These are
///    query vocabulary rather than relations (ADR-0044), so they are answered
///    here, at the matcher, instead of being special-cased at each call site —
///    which is how the empty-array divergence happened in the first place.
/// 1. **Exactly** — `"calls"` selects `calls`, `"type/param"` selects
///    `type/param` and nothing else.
/// 2. **As a family prefix** — an entry selects every relation under it in the
///    `/`-separated hierarchy. `"type"` (or the equivalent `"type/"`) selects
///    `type/field`, `type/param`, `type/return`, `type/bound`, and any member
///    added later — which is the whole reason the family is prefixed rather
///    than enumerated: a client asking for "all type references" must not
///    silently under-report when `type/decorator` (R5.2) lands.
///
/// The `/` is part of the test, so `"type"` never matches `typechecks`, and a
/// flat relation like `"calls"` has no descendants and therefore matches only
/// itself. Matching is hierarchical at every level, following Kythe
/// (`ref` selects `ref/call`, `ref/imports`).
pub fn relation_matches_filter(entry: &str, relation: &str) -> bool {
    // The filter words are not relations and have no place in the hierarchy
    // below — `any` must not be read as a family prefix, and `semantic` is a
    // *predicate*, not a name. Answered first, so an entry set mixing a word
    // with a relation (`["semantic", "imports"]`) is still a plain OR.
    match entry {
        filter::ANY => return true,
        filter::SEMANTIC => return !is_structural(relation),
        _ => {}
    }
    if entry == relation {
        return true;
    }
    // Accept both `"type"` and `"type/"` as the family spelling.
    let family = entry.strip_suffix('/').unwrap_or(entry);
    relation.len() > family.len()
        && relation.starts_with(family)
        && relation.as_bytes()[family.len()] == b'/'
}

/// Whether a relation is *structural* (physical wiring/containment) rather than
/// *semantic* (code meaning). The one home for the taxonomy (ADR-0021/0036): the
/// query layer excludes structural edges from importance ranking. The type-relation
/// family (`implements`/`extends`/`inherits` and the whole `type/…` family) is
/// **semantic** — it counts toward importance; only `has_variant` (containment)
/// joins the structural set.
///
/// The `type/…` test is a **prefix** test, not an enumeration, so a future family
/// member is semantic by construction (ADR-0036 R2.1).
pub fn is_structural(relation: &str) -> bool {
    if is_type_family(relation) {
        return false;
    }
    matches!(relation, CONTAINS | IMPORTS | DEPENDS_ON | HAS_VARIANT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_split_matches_taxonomy() {
        // Physical wiring/containment is structural.
        assert!(is_structural(CONTAINS));
        assert!(is_structural(IMPORTS));
        assert!(is_structural(DEPENDS_ON));
        assert!(is_structural(HAS_VARIANT));
        // Code meaning is semantic — including the ADR-0036 type-relation family,
        // which counts toward god-node importance.
        assert!(!is_structural(CALLS));
        assert!(!is_structural(IMPLEMENTS));
        assert!(!is_structural(EXTENDS));
        assert!(!is_structural(INHERITS));
        assert!(!is_structural(FIELD_TYPE));
        assert!(!is_structural(PARAM_TYPE));
        assert!(!is_structural(RETURN_TYPE));
        assert!(!is_structural(BOUND_TYPE));
        assert!(!is_structural("uses"));
    }

    #[test]
    fn type_family_is_semantic_by_prefix_not_enumeration() {
        // Every current member carries the prefix …
        for r in [FIELD_TYPE, PARAM_TYPE, RETURN_TYPE, BOUND_TYPE] {
            assert!(is_type_family(r), "{r} is in the type/ family");
            assert!(r.starts_with(TYPE_FAMILY_PREFIX));
            assert!(!is_structural(r), "{r} is semantic");
        }
        // … and so does one that does not exist yet: the taxonomy must close
        // under extension (ADR-0036 R2.1), so `type/decorator` (R5.2) is already
        // semantic without touching this function.
        assert!(is_type_family("type/decorator"));
        assert!(!is_structural("type/decorator"));
        assert!(is_type_family("type/not-invented-yet"));
        assert!(!is_structural("type/not-invented-yet"));

        // The flat relations are not swept in by the prefix.
        for r in [
            CALLS,
            CONTAINS,
            IMPORTS,
            DEPENDS_ON,
            IMPLEMENTS,
            EXTENDS,
            INHERITS,
            HAS_VARIANT,
        ] {
            assert!(!is_type_family(r), "{r} is not in the type/ family");
        }
        // A bare `type` (no separator) is not a member either — the prefix
        // includes the `/` so `typechecks` cannot masquerade as one.
        assert!(!is_type_family("type"));
        assert!(!is_type_family("typechecks"));
    }

    #[test]
    fn filter_entry_matches_exactly() {
        assert!(relation_matches_filter(CALLS, CALLS));
        assert!(relation_matches_filter(PARAM_TYPE, PARAM_TYPE));
        assert!(relation_matches_filter("type/param", "type/param"));
    }

    #[test]
    fn filter_entry_matches_the_whole_family_by_prefix() {
        for entry in ["type", "type/"] {
            for r in [FIELD_TYPE, PARAM_TYPE, RETURN_TYPE, BOUND_TYPE] {
                assert!(
                    relation_matches_filter(entry, r),
                    "filter {entry:?} must select {r}"
                );
            }
            // Closure under extension: a member that does not exist yet.
            assert!(relation_matches_filter(entry, "type/decorator"));
        }
    }

    #[test]
    fn family_entry_does_not_match_a_non_family_relation() {
        for r in [
            CALLS,
            CONTAINS,
            IMPORTS,
            DEPENDS_ON,
            IMPLEMENTS,
            EXTENDS,
            INHERITS,
            HAS_VARIANT,
        ] {
            assert!(
                !relation_matches_filter("type", r),
                "filter \"type\" must not select {r}"
            );
        }
        // The `/` is load-bearing: a bare-prefix string match would sweep these in.
        assert!(!relation_matches_filter("type", "typechecks"));
        assert!(!relation_matches_filter("type", "types"));
    }

    #[test]
    fn non_family_entry_matches_only_itself() {
        assert!(relation_matches_filter(CALLS, CALLS));
        for r in [
            CONTAINS, IMPORTS, DEPENDS_ON, IMPLEMENTS, FIELD_TYPE, PARAM_TYPE,
        ] {
            assert!(
                !relation_matches_filter(CALLS, r),
                "filter \"calls\" must not select {r}"
            );
        }
        assert!(!relation_matches_filter(IMPLEMENTS, EXTENDS));
    }

    // ---- ADR-0044: the two filter words --------------------------------------

    /// `any` is the widest filter: every relation, structural included — which
    /// is to say it is exactly what "no filter at all" used to mean, now
    /// spelled as a value a caller can actually select.
    #[test]
    fn any_selects_every_relation_structural_included() {
        for r in [
            CALLS,
            CONTAINS,
            IMPORTS,
            DEPENDS_ON,
            IMPLEMENTS,
            EXTENDS,
            INHERITS,
            HAS_VARIANT,
            FIELD_TYPE,
            PARAM_TYPE,
            RETURN_TYPE,
            BOUND_TYPE,
            "uses",
            // Closed under extension, like the family prefix: a relation that
            // does not exist yet is still "any".
            "type/decorator",
            "not-invented-yet",
        ] {
            assert!(
                relation_matches_filter(filter::ANY, r),
                "`any` must select {r}"
            );
        }
    }

    /// `semantic` is `any` minus the physical scaffolding — and it is
    /// [`is_structural`] inverted, not a second hand-maintained list, so the
    /// two cannot disagree about a relation added later.
    #[test]
    fn semantic_is_any_minus_the_structural_scaffolding() {
        for r in [
            CALLS,
            IMPLEMENTS,
            EXTENDS,
            INHERITS,
            FIELD_TYPE,
            PARAM_TYPE,
            RETURN_TYPE,
            BOUND_TYPE,
            "uses",
            "type/decorator",
        ] {
            assert!(
                relation_matches_filter(filter::SEMANTIC, r),
                "`semantic` must select the code-meaning relation {r}"
            );
        }
        for r in [CONTAINS, IMPORTS, DEPENDS_ON, HAS_VARIANT] {
            assert!(
                !relation_matches_filter(filter::SEMANTIC, r),
                "`semantic` must drop the structural relation {r}"
            );
            assert!(
                relation_matches_filter(filter::ANY, r),
                "… which is the whole difference from `any`: {r}"
            );
        }
        // The definition, not a copy of it.
        for r in [CALLS, CONTAINS, IMPORTS, PARAM_TYPE, HAS_VARIANT, "uses"] {
            assert_eq!(
                relation_matches_filter(filter::SEMANTIC, r),
                !is_structural(r),
                "`semantic` is `is_structural` inverted for {r}"
            );
        }
    }

    /// The filter words are **query vocabulary, not relations**: no relation
    /// constant is spelled like one, and no relation entry selects one — a
    /// filter word only ever appears on the *entry* side of the matcher,
    /// because nothing in the graph is named `any` or `semantic`.
    #[test]
    fn filter_words_are_not_relations() {
        // No relation constant collides with a filter word.
        for r in [
            CALLS,
            CONTAINS,
            IMPORTS,
            DEPENDS_ON,
            IMPLEMENTS,
            EXTENDS,
            INHERITS,
            HAS_VARIANT,
            FIELD_TYPE,
            PARAM_TYPE,
            RETURN_TYPE,
            BOUND_TYPE,
        ] {
            assert!(!filter::VOCABULARY.contains(&r), "{r} is a relation");
        }
        // A relation entry never selects a filter word treated as a relation
        // string — including the family matcher, which must not read `any` as
        // a prefix of anything.
        for entry in [CALLS, IMPORTS, PARAM_TYPE, TYPE_FAMILY_PREFIX, "type"] {
            for word in filter::VOCABULARY {
                assert!(
                    !relation_matches_filter(entry, word),
                    "relation filter {entry:?} must not select the filter word {word:?}"
                );
            }
        }
    }

    /// An absent/empty filter set is `semantic` — the defect this closes is
    /// that two call sites each decided the empty case for themselves and
    /// decided it differently.
    #[test]
    fn an_empty_filter_set_normalizes_to_semantic() {
        assert_eq!(&*filter::normalize(&[]), &[filter::SEMANTIC.to_string()]);
        // An explicit set is carried through untouched — and borrowed, not
        // cloned.
        let explicit = vec![CALLS.to_string(), PARAM_TYPE.to_string()];
        assert!(matches!(
            filter::normalize(&explicit),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(&*filter::normalize(&explicit), &explicit[..]);
    }

    /// The words compose with the hierarchy as a plain OR — no precedence, no
    /// special case at the set level.
    #[test]
    fn filter_words_or_with_relation_entries() {
        let set = [filter::SEMANTIC.to_string(), IMPORTS.to_string()];
        let selects = |r: &str| set.iter().any(|e| relation_matches_filter(e, r));
        assert!(selects(CALLS), "kept by `semantic`");
        assert!(selects(IMPORTS), "kept by the explicit entry");
        assert!(!selects(CONTAINS), "kept by neither");
    }

    #[test]
    fn a_family_member_entry_does_not_match_a_sibling() {
        assert!(!relation_matches_filter("type/param", "type/return"));
        assert!(!relation_matches_filter("type/param", "type/field"));
        assert!(!relation_matches_filter("type/return", "type/param"));
        assert!(!relation_matches_filter("type/param", "type"));
    }
}
