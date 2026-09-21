//! Node-id collision handling (ADR-0028). A definition's id is the *semantic*
//! `<kind>:<path>:<name>`. Only when a name **collides** within a file (overloads,
//! several methods named `from`) do all the colliding nodes get a short hash suffix
//! — `…:name#<hash>` of their declaration text — so they stay uniquely, stably, and
//! meaningfully addressable *without* a signature parser. A hashless id therefore
//! means "the only thing with this name in this file."
//!
//! The extractor mints a *provisional* unique id per def during its walk (the old
//! `~n` counter), records each def's `(base, declaration-text)`, and calls
//! [`finalize_collisions`] once at the end — which rewrites only the collided ids
//! and patches every edge that referenced them. Non-colliding ids are untouched.

use filigrio_core::{Edge, EdgeTarget, Node, NodeId};
use std::collections::{HashMap, HashSet};

/// FNV-1a → 8 hex chars. Deterministic and stable across runs and platforms
/// (unlike `std`'s `DefaultHasher`), so ids persisted in `graph.json` compare
/// across builds. The hash only ever *over-splits* (a re-typed signature yields a
/// new hash); it never merges two distinct decls.
pub(crate) fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", h as u32)
}

/// A definition's id *name*: owner-qualified for a method (`Owner::name`, SCIP-style
/// — semantic, and unique across types without a hash), the bare name for a free
/// function or type. The node's *label* stays the bare name; only the id qualifies.
pub(crate) fn qualify(name: &str, owner: Option<&str>) -> String {
    match owner {
        Some(o) => format!("{o}::{name}"),
        None => name.to_string(),
    }
}

/// Per-def minting metadata captured during the walk, keyed by the provisional id.
pub(crate) struct IdMeta {
    /// The collision base `<kind>:<path>:<name>` (ids with the same base collide).
    pub base: String,
    /// The declaration text the hash disambiguates by (signature up to the body).
    pub decl: String,
}

/// Rewrite every node whose `base` appears more than once to `base#<hash(decl)>`,
/// and patch every edge that referenced the old provisional id (a `contains`
/// target, or a call `source`). Singletons keep their bare base. A rare hash tie
/// within one base (textually identical decls) falls back to an ordinal so ids stay
/// unique. `meta` is keyed by provisional id; node/edge order is preserved so the
/// rewrite is deterministic.
pub(crate) fn finalize_collisions(
    nodes: &mut [Node],
    edges: &mut [Edge],
    meta: &HashMap<String, IdMeta>,
) {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for m in meta.values() {
        *counts.entry(m.base.as_str()).or_default() += 1;
    }

    let mut remap: HashMap<String, String> = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();
    for n in nodes.iter() {
        let Some(m) = meta.get(&n.id.0) else { continue };
        if counts.get(m.base.as_str()).copied().unwrap_or(0) <= 1 {
            continue; // singleton — keep the clean semantic id
        }
        let hashed = format!("{}#{}", m.base, short_hash(&m.decl));
        let mut candidate = hashed.clone();
        let mut k = 1;
        while used.contains(&candidate) {
            candidate = format!("{hashed}~{k}");
            k += 1;
        }
        used.insert(candidate.clone());
        remap.insert(n.id.0.clone(), candidate);
    }
    if remap.is_empty() {
        return;
    }

    for n in nodes.iter_mut() {
        if let Some(new) = remap.get(&n.id.0) {
            n.id = NodeId::new(new.clone());
        }
    }
    for e in edges.iter_mut() {
        if let Some(new) = remap.get(&e.source.0) {
            e.source = NodeId::new(new.clone());
        }
        if let EdgeTarget::Node(t) = &e.target {
            if let Some(new) = remap.get(&t.0) {
                e.target = EdgeTarget::Node(NodeId::new(new.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filigrio_core::Confidence;

    fn node(id: &str) -> Node {
        Node::new(id, "n", "function")
    }
    fn meta(base: &str, decl: &str) -> IdMeta {
        IdMeta {
            base: base.into(),
            decl: decl.into(),
        }
    }

    #[test]
    fn short_hash_is_deterministic_and_differs_by_input() {
        assert_eq!(short_hash("fn from(a: A)"), short_hash("fn from(a: A)"));
        assert_ne!(short_hash("fn from(a: A)"), short_hash("fn from(b: B)"));
    }

    #[test]
    fn singletons_keep_clean_ids_collisions_get_hashed() {
        let mut nodes = vec![
            node("fn:x.rs:from"),
            node("fn:x.rs:from~1"),
            node("fn:x.rs:parse"),
        ];
        let mut edges = vec![
            Edge {
                source: NodeId::new("file:x.rs"),
                relation: "contains".into(),
                confidence: Confidence::Extracted,
                target: EdgeTarget::Node(NodeId::new("fn:x.rs:from~1")),
            },
            Edge {
                source: NodeId::new("fn:x.rs:from"),
                relation: "calls".into(),
                confidence: Confidence::Extracted,
                target: EdgeTarget::Symbol(filigrio_core::TargetRef::new("g")),
            },
        ];
        let mut m = HashMap::new();
        m.insert(
            "fn:x.rs:from".to_string(),
            meta("fn:x.rs:from", "fn from(a: A)"),
        );
        m.insert(
            "fn:x.rs:from~1".to_string(),
            meta("fn:x.rs:from", "fn from(b: B)"),
        );
        m.insert(
            "fn:x.rs:parse".to_string(),
            meta("fn:x.rs:parse", "fn parse()"),
        );
        finalize_collisions(&mut nodes, &mut edges, &m);

        assert_eq!(nodes[2].id.0, "fn:x.rs:parse", "singleton untouched");
        assert!(
            nodes[0].id.0.starts_with("fn:x.rs:from#"),
            "collided → hashed: {}",
            nodes[0].id.0
        );
        assert!(nodes[1].id.0.starts_with("fn:x.rs:from#"));
        assert_ne!(nodes[0].id.0, nodes[1].id.0, "distinct hashes");
        assert!(
            !nodes.iter().any(|n| n.id.0.contains('~')),
            "no order-counter survives"
        );
        // edges patched: the contains target and the call source follow the rewrite.
        assert_eq!(edges[0].target, EdgeTarget::Node(nodes[1].id.clone()));
        assert_eq!(edges[1].source, nodes[0].id);
    }

    #[test]
    fn identical_decls_fall_back_to_an_ordinal() {
        let mut nodes = vec![node("t:x.rs:D"), node("t:x.rs:D~1")];
        let mut edges = vec![];
        let mut m = HashMap::new();
        m.insert("t:x.rs:D".to_string(), meta("t:x.rs:D", "same"));
        m.insert("t:x.rs:D~1".to_string(), meta("t:x.rs:D", "same"));
        finalize_collisions(&mut nodes, &mut edges, &m);
        assert_ne!(
            nodes[0].id.0, nodes[1].id.0,
            "ties still unique via ordinal"
        );
    }
}
