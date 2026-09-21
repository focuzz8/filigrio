//! The **manifest layer** (ADR-0018/0019) — how to *read* a project manifest.
//! This is the hard part of building the `Workspace`, so it lives here in
//! `filigrio-resolve` next to the workspace machinery it feeds.
//!
//! *Which* files are manifests is a different question and lives in the kernel
//! with the rest of the path-classification vocabulary
//! ([`filigrio_core::MANIFEST_NAMES`] / [`filigrio_core::manifest_basename`] /
//! [`filigrio_core::is_manifest`] / [`filigrio_core::is_indexable`], moved there
//! 2026-07-28 per audit §F1): it is a pure basename match with no resolve
//! semantics, and keeping it here forced `filigrio-ingest` to depend on the
//! whole semantic core for one call.
//!
//! Every language marks a project boundary with a package/module manifest that
//! also *names* the package — the name is the import specifier other projects use
//! (`@acme/ui`, a crate name, a Go module path). This module maps each format to a
//! common [`Manifest`] `{ name, entry, deps }`, feeding project discovery (a
//! manifest = a boundary), the `ModuleResolver` (name → the package's files), and
//! the project graph (`deps` → `depends_on`).
//!
//! Deliberately dependency-light: JSON via `serde_json`, TOML/`go.mod` via focused
//! line scans (we need `name`/`main`/dep *keys*, not a full parse). A toolchain-backed
//! tier (`cargo metadata`, real TOML) can replace this later behind the same shape.

/// The parsed, format-independent view of a manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    /// The package/module name = the specifier other projects import it by
    /// (`@acme/ui`, `serde`, `example.com/mod`). `None` if the format/file omits
    /// it (a private leaf `package.json`, a bare `Cargo.toml`).
    pub name: Option<String>,
    /// Entry file **relative to the manifest dir**, when the format declares one
    /// (npm `main`, deno `exports` string). `None` ⇒ the resolver default-probes
    /// (`index.*`, `src/index.*`, `src/lib.rs`, …).
    pub entry: Option<String>,
    /// Declared dependency names (workspace-internal *and* external). The project
    /// graph keeps only those that name another project here (ADR-0019).
    pub deps: Vec<String>,
}

/// Parse a manifest by basename. Unknown/garbled content yields an empty
/// `Manifest` (still a valid boundary — it just contributes no name).
pub fn parse(basename: &str, bytes: &[u8]) -> Manifest {
    match basename {
        "package.json" | "deno.json" | "deno.jsonc" => parse_json(bytes),
        "Cargo.toml" => parse_toml(bytes, "package"),
        "pyproject.toml" => parse_pyproject(bytes),
        "go.mod" => parse_go_mod(bytes),
        _ => Manifest::default(),
    }
}

/// npm/deno: `name` + `main` (deno's `exports` may be a bare string entry) +
/// the keys of `dependencies`/`devDependencies`/`peerDependencies`.
fn parse_json(bytes: &[u8]) -> Manifest {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Manifest::default();
    };
    let name = v.get("name").and_then(|n| n.as_str()).map(str::to_string);
    let entry = v
        .get("main")
        .and_then(|m| m.as_str())
        .or_else(|| v.get("module").and_then(|m| m.as_str()))
        .or_else(|| v.get("exports").and_then(|e| e.as_str()))
        .map(str::to_string);
    let mut deps = Vec::new();
    for field in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(obj) = v.get(field).and_then(|d| d.as_object()) {
            deps.extend(obj.keys().cloned());
        }
    }
    Manifest { name, entry, deps }
}

/// Cargo (`[package] name` + `[dependencies]`/`[dev-dependencies]` keys) and any
/// TOML with a `name` under `section`. A focused scan, not a full TOML parse.
fn parse_toml(bytes: &[u8], section: &str) -> Manifest {
    let text = String::from_utf8_lossy(bytes);
    let mut m = Manifest::default();
    let mut cur = String::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(table) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            cur = table.trim().to_string();
            continue;
        }
        if cur == section {
            if let Some(name) = toml_str_value(t, "name") {
                m.name.get_or_insert(name);
            }
        }
        // `[dependencies]` / `[dev-dependencies]` — each `key = …` is a dep name.
        if cur == "dependencies" || cur == "dev-dependencies" {
            if let Some((lhs, _)) = t.split_once('=') {
                let key = lhs.trim();
                if !key.is_empty() && !key.starts_with('[') {
                    m.deps.push(key.to_string());
                }
            }
        }
    }
    m
}

/// pyproject: `name` under `[project]` (PEP 621) or `[tool.poetry]`. (Dependency
/// extraction — PEP 508 arrays / poetry tables — is deferred; `deps` stays empty.)
fn parse_pyproject(bytes: &[u8]) -> Manifest {
    let project = parse_toml(bytes, "project");
    if project.name.is_some() {
        return Manifest {
            deps: Vec::new(),
            ..project
        };
    }
    Manifest {
        deps: Vec::new(),
        ..parse_toml(bytes, "tool.poetry")
    }
}

/// `go.mod`: the `module <path>` directive names the module; `require` lines
/// (single or in a `require ( … )` block) are the dependency module paths.
fn parse_go_mod(bytes: &[u8]) -> Manifest {
    let text = String::from_utf8_lossy(bytes);
    let mut m = Manifest::default();
    let mut in_require = false;
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("module ") {
            let name = rest.trim().trim_matches('"').trim();
            if !name.is_empty() {
                m.name.get_or_insert_with(|| name.to_string());
            }
        } else if t == "require (" {
            in_require = true;
        } else if in_require && t == ")" {
            in_require = false;
        } else if let Some(one) = t.strip_prefix("require ") {
            if let Some(path) = one.split_whitespace().next() {
                m.deps.push(path.to_string());
            }
        } else if in_require {
            if let Some(path) = t.split_whitespace().next() {
                if !path.is_empty() {
                    m.deps.push(path.to_string());
                }
            }
        }
    }
    m
}

/// Value of a `key = "quoted"` TOML line, if `line` assigns `key`.
fn toml_str_value(line: &str, key: &str) -> Option<String> {
    let (lhs, rhs) = line.split_once('=')?;
    if lhs.trim() != key {
        return None;
    }
    let val = rhs.trim();
    // strip a trailing inline comment, then the quotes.
    let val = val.split('#').next().unwrap_or(val).trim();
    let unquoted = val.strip_prefix('"').and_then(|s| s.strip_suffix('"'))?;
    Some(unquoted.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_json_name_and_main() {
        let m = parse(
            "package.json",
            br#"{ "name": "@acme/ui", "version": "1.0.0", "main": "src/index.ts" }"#,
        );
        assert_eq!(m.name.as_deref(), Some("@acme/ui"));
        assert_eq!(m.entry.as_deref(), Some("src/index.ts"));
    }

    #[test]
    fn package_json_without_main() {
        let m = parse("package.json", br#"{ "name": "leaf" }"#);
        assert_eq!(m.name.as_deref(), Some("leaf"));
        assert_eq!(m.entry, None);
    }

    #[test]
    fn cargo_toml_package_name() {
        let src = br#"
[package]
name = "filigrio-core"
version = "0.1.0"

[dependencies]
serde = "1"
"#;
        let m = parse("Cargo.toml", src);
        assert_eq!(m.name.as_deref(), Some("filigrio-core"));
    }

    #[test]
    fn cargo_toml_ignores_dependency_names() {
        // `name` must come from [package], not a [dependencies] entry.
        let src = br#"
[dependencies]
name = "not-this"

[package]
name = "real-crate"
"#;
        assert_eq!(parse("Cargo.toml", src).name.as_deref(), Some("real-crate"));
    }

    #[test]
    fn go_mod_module_path() {
        let m = parse("go.mod", b"module example.com/foo/bar\n\ngo 1.21\n");
        assert_eq!(m.name.as_deref(), Some("example.com/foo/bar"));
    }

    #[test]
    fn pyproject_pep621_and_poetry() {
        let pep = parse("pyproject.toml", b"[project]\nname = \"mypkg\"\n");
        assert_eq!(pep.name.as_deref(), Some("mypkg"));
        let poetry = parse("pyproject.toml", b"[tool.poetry]\nname = \"poetrypkg\"\n");
        assert_eq!(poetry.name.as_deref(), Some("poetrypkg"));
    }

    #[test]
    fn garbled_is_empty_not_a_panic() {
        assert_eq!(parse("package.json", b"{ not json"), Manifest::default());
        assert_eq!(parse("Cargo.toml", b"\x00\x01"), Manifest::default());
    }
}
