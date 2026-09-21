//! ADR-0037b (RESOLVED 2026-07-29) — **TypeScript `interface` / `type_alias`
//! definitions are linkable symbols.**
//!
//! The ADR's reproduction — `import type { Params }` unresolved beside a working
//! `import { Widget }` from the *same* specifier — confounded two variables: the
//! type-only import *and* the fact that `Params` is an `interface` while `Widget`
//! is a `class`. De-confounded below (a 2×2: {type-only, plain} × {interface,
//! class}), the discriminator is the **node kind**, not the import syntax. The
//! `interface` and `type_alias` kinds the ADR-0037b structural pass emits were
//! missing from `LINKABLE`, so such a definition could be declared, exported and
//! imported and still never enter `local_def` or the link candidates.
//!
//! This runs the real TS extractor through the real `Engine`, because the defect
//! was invisible at the extraction level: extraction was already correct (the
//! `imports` edge carried its specifier for every form — see the
//! `type_only_imports_bind_like_value_imports` tests in `filigrio-index`).

mod common;
use common::{cold_ext, DirSource};
use filigrio_core::{EdgeTarget, Extractor, GraphState, NodeId};
use filigrio_index::DispatchExtractor;
use std::fs;
use std::path::Path;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// Cold-build a TS tree and return the linked state.
fn build(files: &[(&str, &str)]) -> GraphState {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "package.json", r#"{"name":"repro"}"#);
    for (rel, body) in files {
        write(dir.path(), rel, body);
    }
    let dispatch = DispatchExtractor::with_defaults();
    let ext: &dyn Extractor = &dispatch;
    let src = DirSource::load_ext(dir.path(), &["ts", "tsx"]);
    cold_ext(&src, ext)
}

/// The node id an edge of `relation` from `source` whose bound name is `name`
/// resolved to — `None` when it stayed an unresolved `Symbol`.
fn target_of(state: &GraphState, relation: &str, source: &str, name: &str) -> Option<String> {
    let label_of = |id: &NodeId| {
        state
            .graph
            .nodes
            .iter()
            .find(|n| &n.id == id)
            .map(|n| n.label.clone())
    };
    state
        .graph
        .edges
        .iter()
        .filter(|e| e.relation == relation && e.source.0 == source)
        .find_map(|e| match &e.target {
            EdgeTarget::Node(id) if label_of(id).as_deref() == Some(name) => Some(id.to_string()),
            _ => None,
        })
}

const TYPES: &str = r#"
export interface Params { a: string }
export interface Other { b: string }
export class Widget { x = 1 }
export class Gadget { y = 2 }
export type Alias = { c: string }
"#;

/// The 2×2 that de-confounds the ADR's reproduction. Before the fix the two
/// *interface* rows failed and the two *class* rows passed — regardless of
/// whether the import was type-only, which is what the ADR blamed.
#[test]
fn interface_and_class_imports_bind_under_both_import_forms() {
    let consumer = "src/deep/nested/consumer.ts";
    let state = build(&[
        ("src/other/types.ts", TYPES),
        (
            consumer,
            r#"
import type { Params } from '../../other/types'
import { Other } from '../../other/types'
import type { Widget } from '../../other/types'
import { Gadget } from '../../other/types'
import type { Alias } from '../../other/types'
"#,
        ),
    ]);
    let src = format!("file:{consumer}");
    let want = "type:src/other/types.ts";
    for (name, form) in [
        ("Params", "interface via `import type`"), // the ADR's failing case
        ("Other", "interface via a plain import"), // fails identically — so it is not the syntax
        ("Widget", "class via `import type`"),     // bound *before* the fix — likewise
        ("Gadget", "class via a plain import"),    // the ADR's working control
        ("Alias", "type alias via `import type`"),
    ] {
        assert_eq!(
            target_of(&state, "imports", &src, name).as_deref(),
            Some(format!("{want}:{name}").as_str()),
            "{form}: `{name}` must bind to its definition"
        );
    }
}

/// The same missing kinds starved the ADR-0036 structural relations: an
/// `implements`/`extends`/`type/field` reference to an interface had no candidate
/// at all. This is the bulk of the measured gain (`type/field` 11.3 % → 78.3 %
/// resolved on `next.js/packages/next`).
#[test]
fn structural_edges_bind_to_an_interface_or_type_alias() {
    let state = build(&[
        ("src/other/types.ts", TYPES),
        (
            "src/use.ts",
            r#"
import type { Params, Alias } from './other/types'
import { Widget } from './other/types'

export interface Sub extends Params { z: number }
export class Impl extends Widget implements Params {
    p: Params
    a: Alias
}
"#,
        ),
    ]);
    let want = "type:src/other/types.ts";
    for (rel, source, name) in [
        ("extends", "type:src/use.ts:Sub", "Params"),
        ("implements", "type:src/use.ts:Impl", "Params"),
        ("extends", "type:src/use.ts:Impl", "Widget"),
        ("type/field", "type:src/use.ts:Impl", "Params"),
        ("type/field", "type:src/use.ts:Impl", "Alias"),
    ] {
        assert_eq!(
            target_of(&state, rel, source, name).as_deref(),
            Some(format!("{want}:{name}").as_str()),
            "{rel} from {source} must bind `{name}`"
        );
    }
}

/// A type-only re-export is a re-export: the ADR-0020 barrel walk must follow
/// `export type { X } from '…'` to the real definition, so an interface behind a
/// barrel binds like a class behind one.
#[test]
fn type_only_reexport_barrel_reaches_the_definition() {
    let consumer = "src/consumer.ts";
    let state = build(&[
        ("src/other/types.ts", TYPES),
        (
            "src/barrel.ts",
            r#"
export type { Params } from './other/types'
export { Widget } from './other/types'
"#,
        ),
        (
            consumer,
            r#"
import type { Params } from './barrel'
import { Widget } from './barrel'
"#,
        ),
    ]);
    let src = format!("file:{consumer}");
    for name in ["Params", "Widget"] {
        assert_eq!(
            target_of(&state, "imports", &src, name).as_deref(),
            Some(format!("type:src/other/types.ts:{name}").as_str()),
            "`{name}` must resolve through the barrel to its definition"
        );
    }
}
