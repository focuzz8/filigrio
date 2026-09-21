//! **ADR-0040's parity gate: oxc vs tree-sitter for TypeScript/JavaScript.**
//!
//! The status doc records this gate as having no home. It has one now, and it is
//! deliberately not a smoke test — a smoke test here is worse than nothing,
//! because "both frontends ran" is true even when one of them extracts an empty
//! graph. Two rules keep it honest:
//!
//! 1. **Parity is asserted on real output.** The comparison is over the
//!    cross-backend corpus plus a JS/TS-specific set, and every parity case must
//!    yield at least one definition node beyond the file node
//!    ([`assert_parity`]), so two equally-empty extractions cannot pass.
//! 2. **Known differences are registered, not tolerated.** [`DIVERGENCES`] pins
//!    each one *by direction* — which frontend emits what — and asserts it still
//!    holds. Closing a divergence turns this test red, so the register is
//!    corrected in the commit that fixes the frontend rather than drifting.
//!
//! Run under `just features-ts-oxc` (`cargo test -p filigrio-index --features
//! ts-oxc`). The whole `cross_backend_contract` suite also re-runs against oxc in
//! that configuration, because `DispatchExtractor::with_defaults()` swaps the TS
//! slot at compile time — so the P2 properties gate both frontends for free.

#![cfg(feature = "ts-oxc")]

mod common;

use common::*;
use filigrio_core::{classify, Export, Extraction, Extractor};
use filigrio_index::{TypeScriptExtractor, TypeScriptOxcExtractor};
use std::collections::BTreeSet;

fn tree_sitter(path: &str, src: &str) -> Extraction {
    TypeScriptExtractor::new()
        .extract(&classify(path), src.as_bytes())
        .unwrap_or_else(|e| panic!("tree-sitter failed on {path}: {e}"))
}

fn oxc(path: &str, src: &str) -> Extraction {
    TypeScriptOxcExtractor::new()
        .extract(&classify(path), src.as_bytes())
        .unwrap_or_else(|e| panic!("oxc failed on {path}: {e}"))
}

/// Exports as a multiset-preserving sorted list — duplicates are a real
/// difference (see the overload-signature divergence), so this must not dedupe.
fn export_names(ex: &Extraction) -> Vec<String> {
    let mut v: Vec<String> = ex
        .exports
        .iter()
        .map(|e| match e {
            Export::Local { name } => format!("local {name}"),
            Export::ReExport {
                name,
                specifier,
                imported,
            } => format!("reexport {name} <- {imported} @ {specifier}"),
            Export::Star { specifier } => format!("star @ {specifier}"),
        })
        .collect();
    v.sort();
    v
}

fn diff(a: &BTreeSet<(String, String)>, b: &BTreeSet<(String, String)>) -> Vec<String> {
    a.difference(b).map(|(x, y)| format!("{x}:{y}")).collect()
}

/// The two frontends agree on nodes, edges **and** the export table for `src`.
///
/// The non-triviality guard is the reason this is a gate and not a smoke test:
/// an extraction with nothing but the `file` node would otherwise "agree".
fn assert_parity(case: &str, path: &str, src: &str) {
    let (ts, ox) = (tree_sitter(path, src), oxc(path, src));

    let real_defs = kind_labels(&ts).iter().filter(|(k, _)| k != "file").count();
    assert!(
        real_defs > 0,
        "parity case `{case}` extracts no definition beyond the file node — it would \
         pass vacuously. Give it real content or move it to DIVERGENCES."
    );

    let (tn, on) = (kind_labels(&ts), kind_labels(&ox));
    assert_eq!(
        tn,
        on,
        "`{case}`: node parity broken.\n  tree-sitter only: {:?}\n  oxc only: {:?}",
        diff(&tn, &on),
        diff(&on, &tn),
    );

    let (te, oe) = (relation_targets(&ts), relation_targets(&ox));
    assert_eq!(
        te,
        oe,
        "`{case}`: edge parity broken.\n  tree-sitter only: {:?}\n  oxc only: {:?}",
        diff(&te, &oe),
        diff(&oe, &te),
    );

    assert_eq!(
        export_names(&ts),
        export_names(&ox),
        "`{case}`: export-table parity broken"
    );
}

// ---------------------------------------------------------------------------
// parity — the frontends must agree here
// ---------------------------------------------------------------------------

/// The cross-backend corpus, extracted by both frontends. This is the strongest
/// form of the gate: the same fixtures that carry the ADR-0036 relation-family
/// and linkability properties must produce byte-identical observables whichever
/// TS frontend is compiled in, or those properties mean something different
/// depending on a cargo feature.
#[test]
fn frontends_agree_on_the_cross_backend_corpus() {
    for f in CORPUS {
        let case = f.case(Backend::TypeScript).expect("backend-complete");
        assert_parity(f.name, &f.path(Backend::TypeScript), case.src);
    }
}

/// TS/JS shapes the shared corpus does not reach: object literals, function
/// expressions, CommonJS, generics with calls, and namespaces.
#[test]
fn frontends_agree_on_typescript_specific_shapes() {
    // Both capture only the shorthand method `get`, and neither captures the
    // `function`-expression or arrow properties. ADR-0040's "oxc object-literal
    // method shorthand" defect does **not** reproduce on this shape — recorded
    // as observed, not as a claim that the defect is gone.
    assert_parity(
        "object_literal_methods",
        "src/api.ts",
        "export const api = { get(u: string) { return u }, \
         post: function (u: string) { return u }, arrow: (u: string) => u }",
    );
    assert_parity(
        "function_expressions",
        "src/fx.js",
        "const f = function named() {}; const g = () => {}; module.exports = { f, g }",
    );
    assert_parity(
        "commonjs_require",
        "src/cjs.js",
        "const { join } = require('path'); function use() { return join('a') } \
         module.exports = use",
    );
    assert_parity(
        "generics_and_method_calls",
        "src/box.ts",
        "import { Widget } from './w'\n\
         export class Box<T extends Widget> { item: T; take(w: Widget): Widget { return w } }\n\
         export function run() { const b = new Box(); b.take(null as any) }",
    );
    assert_parity(
        "namespace",
        "src/ns.ts",
        "export namespace N { export class Inner {} }",
    );
    // ADR-0036 §5: the bodiless declarations. Nodes *and* the `type/param` /
    // `type/return` edges their signatures carry must agree — the signature
    // edges are half the point of §5, and a frontend that minted the node
    // without them would satisfy a node-only comparison.
    assert_parity(
        "interface_method_signatures",
        "src/iface.ts",
        "export class Widget {}\n\
         export interface Store { get(id: string): Widget; put(w: Widget): void }",
    );
    assert_parity(
        "abstract_class_methods",
        "src/abs.ts",
        "export class Widget {}\n\
         export abstract class Base { abstract render(w: Widget): Widget; \
         describe(w: Widget): Widget { return this.render(w) } }",
    );
}

// ---------------------------------------------------------------------------
// the divergence register — differences that are real, pinned by direction
// ---------------------------------------------------------------------------

/// A known frontend difference, pinned concretely enough that "they differ
/// somehow" cannot satisfy it.
struct Divergence {
    name: &'static str,
    path: &'static str,
    src: &'static str,
    diagnosis: &'static str,
    /// A reading of one frontend's output. `registered_divergences_still_hold`
    /// asserts the two frontends disagree on it, so the entry pins *what* differs
    /// rather than merely that something does.
    observe: fn(&Extraction) -> String,
}

const DIVERGENCES: &[Divergence] = &[
    Divergence {
        name: "class_overload_signature",
        path: "src/overload_method.ts",
        src: "export class Api { send(x: string): string; send(x: number): number; \
              send(x: any): any { return x } }",
        diagnosis: "what remains of the bodiless-declaration gap once ADR-0036 §5 landed: \
                    both frontends now mint the abstract method and the interface method \
                    signature, but a class **overload** signature is bodiless too, and oxc \
                    mints a node for each one (3 `send` nodes) where tree-sitter mints only \
                    the implementation. §5 is explicit that an overload signature is not an \
                    abstraction — the implementation is right below it — so tree-sitter is \
                    the side that matches the decision; the fix is to drop the extra oxc \
                    nodes, which is a change to oxc's def_method and out of §5's scope. Same \
                    family as the `overload_signatures` entry below, which records the \
                    export-table half of the identical defect",
        observe: |ex| {
            let mut v: Vec<String> = ex
                .nodes
                .iter()
                .filter(|n| n.kind == "function")
                .map(|n| n.id.0.clone())
                .collect();
            v.sort();
            format!("{v:?}")
        },
    },
    Divergence {
        name: "default_exported_class",
        path: "src/default.ts",
        src: "export default class D { m() {} }\nexport const x = 1",
        diagnosis: "tree-sitter records `export default class D` as `Local { name: \"D\" }`; \
                    oxc omits it from the export table entirely. This one is \
                    resolution-affecting in the direction that matters: under oxc a \
                    consumer's `import D from './default'` has no export entry to bind to \
                    (ADR-0020 export tables), so the import declines",
        observe: |ex| format!("{:?}", export_names_of(ex)),
    },
    Divergence {
        name: "overload_signatures",
        path: "src/overload.ts",
        src: "export function over(a: string): string;\n\
              export function over(a: number): number;\n\
              export function over(a: any): any { return a }",
        diagnosis: "oxc emits one `Local { name: \"over\" }` per overload signature (3), \
                    tree-sitter one. The export table is a set of names in ADR-0020's model, \
                    so the duplicates are redundant rather than wrong — but they inflate \
                    every export table on a declaration-heavy corpus",
        observe: |ex| format!("{:?}", export_names_of(ex)),
    },
];

fn export_names_of(ex: &Extraction) -> Vec<String> {
    let mut v: Vec<String> = ex
        .exports
        .iter()
        .map(|e| match e {
            Export::Local { name } => name.clone(),
            Export::ReExport { name, .. } => format!("re:{name}"),
            Export::Star { specifier } => format!("star:{specifier}"),
        })
        .collect();
    v.sort();
    v
}

/// Each registered divergence still holds, **in the direction recorded**.
///
/// Asserting the difference (rather than tolerating it) is what makes the
/// register self-maintaining: when a frontend is fixed this fails, naming the
/// entry to delete. The alternative — a comment in the ADR — is what let the
/// abstract-class gap sit unlisted in the status doc's defect table while a
/// routing test in `dispatch.rs` quietly used it as a discriminator.
#[test]
fn registered_divergences_still_hold() {
    for d in DIVERGENCES {
        let (ts, ox) = (tree_sitter(d.path, d.src), oxc(d.path, d.src));
        let (t, o) = ((d.observe)(&ts), (d.observe)(&ox));
        assert_ne!(
            t, o,
            "`{}` is registered as an oxc/tree-sitter divergence but the frontends now \
             agree ({t}).\n  {}\n  If the gap is closed, delete the entry and move the \
             case into `frontends_agree_on_typescript_specific_shapes`.",
            d.name, d.diagnosis,
        );
    }
}

/// The abstract-class gap, closed — and pinned in the direction it was closed.
///
/// The register above proves *a* difference remains; this proves *which*. Every
/// observable the gap used to swallow is now asserted to agree, so a regression
/// that re-dropped `abstract_class_declaration` could not hide behind the one
/// difference that is still legitimately registered
/// (`class_overload_signature`).
#[test]
fn both_frontends_capture_abstract_classes() {
    let src = "export abstract class C { abstract go(): void; concrete(): void {} }";
    let (ts, ox) = (
        tree_sitter("src/abstract.ts", src),
        oxc("src/abstract.ts", src),
    );

    for (who, ex) in [("tree-sitter", &ts), ("oxc", &ox)] {
        assert!(
            ex.nodes.iter().any(|n| n.kind == "class" && n.label == "C"),
            "{who} must emit the abstract class as a `class` node: {:?}",
            kind_labels(ex)
        );
        assert_eq!(
            export_names_of(ex),
            vec!["C".to_string()],
            "{who} must record `export abstract class C` in the export table"
        );
        // The concrete method is owner-qualified — the id carries `C::`, which is
        // what the gap used to cost (`fn:…:concrete`). The label stays bare per
        // ADR-0028, so this must be read off the id, not `kind_labels`.
        assert!(
            ex.nodes
                .iter()
                .any(|n| n.kind == "function" && n.id.0.ends_with(":C::concrete")),
            "{who} must own-qualify the abstract class's method: {:?}",
            ex.nodes.iter().map(|n| n.id.0.clone()).collect::<Vec<_>>()
        );
    }
}

/// **ADR-0036 §5, as a parity property: both frontends mint the bodiless
/// declaration, with the same fact on it.**
///
/// This case was the register's `bodiless_method_signature` divergence — oxc
/// minted `go`, tree-sitter did not — for as long as the port had no policy on
/// declarations. §5 set the policy, so the entry is gone and this is its
/// replacement, asserted in the *positive* direction: the node, its `function`
/// kind, its owner qualifier, and `attrs["abstract"]` on both the method and the
/// declaring type, from both frontends.
///
/// It is deliberately not folded into `assert_parity`: that compares
/// `(kind, label)` pairs and `(relation, target)` pairs, neither of which can see
/// a node *attr*. A frontend that minted the node and forgot the fact would pass
/// parity and still leave the abstraction unmarked.
#[test]
fn both_frontends_mint_bodiless_declarations_with_the_abstract_fact() {
    let src = "export interface Store { get(id: string): Widget }\n\
               export abstract class Base { abstract render(): Widget; \
               describe(): Widget { return this.render() } }\n\
               export class Widget {}";
    let (ts, ox) = (tree_sitter("src/decl.ts", src), oxc("src/decl.ts", src));

    for (who, ex) in [("tree-sitter", &ts), ("oxc", &ox)] {
        let abstract_fns: Vec<String> = ex
            .nodes
            .iter()
            .filter(|n| {
                n.kind == "function" && n.attrs.get("abstract").map(String::as_str) == Some("true")
            })
            .map(|n| n.id.0.clone())
            .collect();
        assert_eq!(
            abstract_fns,
            vec![
                "fn:src/decl.ts:Store::get".to_string(),
                "fn:src/decl.ts:Base::render".to_string(),
            ],
            "{who}: the two bodiless declarations must be `function` nodes marked \
             abstract, owner-qualified by their declaring type. Got: {:?}",
            ex.nodes
                .iter()
                .map(|n| (n.id.0.clone(), n.kind.clone(), n.attrs.clone()))
                .collect::<Vec<_>>()
        );

        // The signature edges, per frontend: §5's open sub-question 1 answered
        // yes, and the reason a declaration node is worth minting at all.
        let from_decl: Vec<String> = ex
            .edges
            .iter()
            .filter(|e| e.source.0 == "fn:src/decl.ts:Store::get")
            .map(|e| format!("{} -> {}", e.relation, target_name(e)))
            .collect();
        assert!(
            from_decl.contains(&"type/return -> Widget".to_string()),
            "{who}: the declaration must carry its return type: {from_decl:?}"
        );

        let abstract_types: Vec<String> = ex
            .nodes
            .iter()
            .filter(|n| {
                (n.kind == "class" || n.kind == "interface")
                    && n.attrs.get("abstract").map(String::as_str) == Some("true")
            })
            .map(|n| n.label.clone())
            .collect();
        assert_eq!(
            abstract_types,
            vec!["Store".to_string(), "Base".to_string()],
            "{who}: the interface and the abstract class carry the fact; the concrete \
             `Widget` does not"
        );
    }
}
