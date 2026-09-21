#![cfg(feature = "oxc")]
//! Integration spec for the exact `oxc_resolver` tier over a real temp tree.

use filigrio_core::{ModuleResolver, Project, Workspace};
use filigrio_resolve::OxcResolver;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn ws(projects: &[(&str, &str, Option<&str>)]) -> Workspace {
    let mut m = BTreeMap::new();
    for (root, name, entry) in projects {
        m.insert(
            root.to_string(),
            Project {
                root: root.to_string(),
                manifest: "package.json".into(),
                name: Some(name.to_string()),
                entry: entry.map(str::to_string),
                deps: Vec::new(),
            },
        );
    }
    Workspace { projects: m }
}

#[test]
fn resolves_workspace_package_via_oxc() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "packages/ui/package.json",
        r#"{"name":"@acme/ui","main":"src/index.ts"}"#,
    );
    write(
        root,
        "packages/ui/src/index.ts",
        "export function greet(){}",
    );
    write(
        root,
        "packages/app/src/index.ts",
        "import {greet} from '@acme/ui';",
    );

    let r = OxcResolver::new(
        root,
        &ws(&[("packages/ui", "@acme/ui", Some("src/index.ts"))]),
    );
    let got = r.resolve("packages/app/src/index.ts", "@acme/ui");
    assert_eq!(
        got.as_deref(),
        Some("packages/ui/src/index.ts"),
        "oxc should resolve the workspace package to its entry"
    );
}

#[test]
fn resolves_relative_via_oxc() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/shapes.ts", "export class Circle{}");
    write(root, "src/main.ts", "import {Circle} from './shapes';");
    let r = OxcResolver::new(root, &Workspace::default());
    assert_eq!(
        r.resolve("src/main.ts", "./shapes").as_deref(),
        Some("src/shapes.ts")
    );
}

#[test]
fn external_specifier_is_none_via_oxc() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/main.ts", "import x from 'react';");
    let r = OxcResolver::new(root, &Workspace::default());
    assert_eq!(r.resolve("src/main.ts", "react"), None);
}

/// The monorepo-fixture layout: nested `src/packages/*`, three named packages,
/// `main` pointing at a `.ts` entry. The cross-package import must bind to `ui`.
#[test]
fn resolves_fixture_style_monorepo_layout() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for pkg in ["ui", "admin", "app"] {
        write(
            root,
            &format!("src/packages/{pkg}/package.json"),
            &format!(r#"{{"name":"@acme/{pkg}","version":"1.0.0","main":"src/index.ts"}}"#),
        );
        write(
            root,
            &format!("src/packages/{pkg}/src/index.ts"),
            "export function greet(){}",
        );
    }
    let w = ws(&[
        ("src/packages/ui", "@acme/ui", Some("src/index.ts")),
        ("src/packages/admin", "@acme/admin", Some("src/index.ts")),
        ("src/packages/app", "@acme/app", Some("src/index.ts")),
    ]);
    let r = OxcResolver::new(root, &w);
    assert_eq!(
        r.resolve("src/packages/app/src/index.ts", "@acme/ui")
            .as_deref(),
        Some("src/packages/ui/src/index.ts"),
    );
}

/// oxc requires absolute paths; the resolver must canonicalize a **relative**
/// root (as the CLI passes) rather than silently failing.
#[test]
fn resolves_with_a_relative_root() {
    let dir = tempfile::tempdir().unwrap();
    let abs = dir.path();
    write(abs, "pkg/index.ts", "export const x = 1;");
    write(abs, "app/main.ts", "import {x} from '@acme/lib';");
    // Pass a relative path to the temp dir as the root.
    let cwd = std::env::current_dir().unwrap();
    let rel = pathdiff_relative(&cwd, abs);
    let w = ws(&[("pkg", "@acme/lib", None)]);
    let r = OxcResolver::new(&rel, &w);
    assert_eq!(
        r.resolve("app/main.ts", "@acme/lib").as_deref(),
        Some("pkg/index.ts"),
    );
}

/// Minimal relative-path helper (no dep): number of `..` to climb from `base`
/// to the shared root, then the tail of `target`. Both are absolute.
fn pathdiff_relative(base: &Path, target: &Path) -> std::path::PathBuf {
    let b: Vec<_> = base.components().collect();
    let t: Vec<_> = target.components().collect();
    let common = b.iter().zip(&t).take_while(|(x, y)| x == y).count();
    let mut out = std::path::PathBuf::new();
    for _ in 0..(b.len() - common) {
        out.push("..");
    }
    for c in &t[common..] {
        out.push(c.as_os_str());
    }
    out
}
