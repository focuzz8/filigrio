//! `SourceModuleResolver` — the **source-only** `ModuleResolver` tier (ADR-0018):
//! install-free specifier resolution over the `Source` (no `node_modules`, no
//! toolchain). It resolves two shapes:
//!
//!   * **relative** (`./x`, `../y`) — joined to the importing file's dir, then
//!     probed with the usual extensions / `index.*`;
//!   * **workspace package** (`@acme/ui`, `@acme/ui/sub`) — looked up in a
//!     name→dir map built from project manifests ([`crate::manifest`]), then the
//!     package `entry` (or a default) / sub-path is probed.
//!
//! Anything else (`react`, `jsr:…`, a URL) resolves to `None` — an external
//! boundary the caller keeps unresolved. `oxc_resolver` is the drop-in
//! production backend for the JS/TS case behind the same `ModuleResolver` port;
//! this hand-rolled tier covers the install-free path across languages.

use filigrio_core::{ModuleResolver, Source, Workspace};
use std::collections::BTreeMap;

/// A named package: its dir and optional entry file.
#[derive(Clone, Debug)]
struct Package {
    dir: String,
    entry: Option<String>,
}

pub struct SourceModuleResolver<'a> {
    source: &'a dyn Source,
    /// package name → package (longest-name-first matching handled at lookup).
    packages: BTreeMap<String, Package>,
    /// every project root, for the longest-prefix `root_of` a Rust `crate::` path
    /// needs (the importing file's crate).
    roots: Vec<String>,
}

/// Extensions probed for an extension-less specifier, in order (JS/TS first,
/// then other source-only languages).
const EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs", "rs", "py", "go"];
/// Default entry files probed when a package declares no explicit entry.
const DEFAULT_ENTRIES: &[&str] = &[
    "index.ts",
    "index.js",
    "src/index.ts",
    "src/index.js",
    "src/lib.rs",
    "src/main.rs",
];

impl<'a> SourceModuleResolver<'a> {
    /// Build the workspace-package map from the already-parsed `Workspace`
    /// (ADR-0019) — no manifest re-reads. `source` is still needed to *probe*
    /// candidate target files (relative imports, entry points).
    pub fn from_workspace(source: &'a dyn Source, ws: &Workspace) -> Self {
        let packages = ws
            .by_name()
            .into_iter()
            .map(|(name, p)| {
                (
                    name.to_string(),
                    Package {
                        dir: p.root.clone(),
                        entry: p.entry.clone(),
                    },
                )
            })
            .collect();
        let roots = ws.projects.keys().cloned().collect();
        SourceModuleResolver {
            source,
            packages,
            roots,
        }
    }

    /// Existence only — never the bytes. This used to be `read(path).is_ok()`,
    /// i.e. it materialized a whole file to learn a boolean; `Source::exists` is
    /// the metadata question, answered by a `stat` or a map lookup. That is the
    /// bulk of [`probe`](Self::probe)'s cost (ADR-0042 Phase 1b, item A).
    fn exists(&self, path: &str) -> bool {
        self.source.exists(path)
    }

    /// Probe a path that may lack an extension: exact, then `.<ext>`, then
    /// `<path>/index.<ext>`. Returns the first file that exists.
    ///
    /// One reused buffer rather than a `format!` per candidate: this is ≤ 19
    /// candidates per specifier and it runs for every import in the repo on every
    /// apply, so the allocations were the probe (ADR-0042 Phase 1b, item A).
    fn probe(&self, path: &str) -> Option<String> {
        if self.exists(path) {
            return Some(path.to_string());
        }
        let mut buf = String::with_capacity(path.len() + 12);
        for (sep, ext) in EXTS
            .iter()
            .map(|e| (".", e))
            .chain(EXTS.iter().map(|e| ("/index.", e)))
        {
            buf.clear();
            buf.push_str(path);
            buf.push_str(sep);
            buf.push_str(ext);
            if self.exists(&buf) {
                return Some(buf);
            }
        }
        None
    }

    /// The longest workspace package that `specifier` **names** (`@acme/ui`) or
    /// **lies under** (`@acme/ui/sub`), or `None`.
    ///
    /// Probes the specifier's own `/`-delimited prefixes, longest first: a package
    /// name that matches is *by definition* one of those ≤ depth prefixes, so the
    /// first hit is the longest match and no other package can be a candidate. The
    /// previous form scanned **every** package and built a `format!("{name}/")`
    /// per package per specifier — 694 packages × every import in next.js. Same
    /// shape as `Workspace::project_of` (ADR-0042 Phase 1b).
    fn package_of(&self, specifier: &str) -> Option<(&str, &Package)> {
        let mut end = specifier.len();
        loop {
            match self.packages.get_key_value(&specifier[..end]) {
                Some((name, pkg)) => return Some((name.as_str(), pkg)),
                None => end = specifier[..end].rfind('/')?,
            }
        }
    }

    /// The entry file of a package dir: its declared entry, else a default probe.
    fn entry_of(&self, pkg: &Package) -> Option<String> {
        if let Some(entry) = &pkg.entry {
            if let Some(hit) = self.probe(&join(&pkg.dir, entry)) {
                return Some(hit);
            }
        }
        for cand in DEFAULT_ENTRIES {
            let p = join(&pkg.dir, cand);
            if self.exists(&p) {
                return Some(p);
            }
        }
        None
    }
}

impl SourceModuleResolver<'_> {
    /// The project root the importing file belongs to (longest-prefix match over
    /// the known roots). `""` (the whole scan) is a valid root.
    fn root_of(&self, file: &str) -> Option<&str> {
        self.roots
            .iter()
            .filter(|r| r.is_empty() || file == r.as_str() || is_under(file, r))
            .max_by_key(|r| r.len())
            .map(String::as_str)
    }

    /// The directory that holds a crate's *root module* file (`lib.rs`/`main.rs`)
    /// — `<root>/src` in a normal repo, or `<root>` itself when the scan root is
    /// already the crate's src dir (the oracle-diff harness builds `<fixture>/src`).
    fn crate_module_base(&self, root: &str) -> String {
        let src = join(root, "src");
        for cand in [&src, &root.to_string()] {
            if self.exists(&join(cand, "lib.rs")) || self.exists(&join(cand, "main.rs")) {
                return cand.to_string();
            }
        }
        src
    }

    /// Resolve a Rust `use`/`pub use` **module path** (`crate::a::b`, `self::x`,
    /// `super::y`, `depcrate::a`) to the target module *file*, by the standard
    /// Cargo file convention (module = file). The imported symbol has already
    /// been split off by the extractor, so `spec` is a module path.
    fn resolve_rust(&self, importing_file: &str, spec: &str) -> Option<String> {
        let segs: Vec<&str> = spec
            .split("::")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        let (head, rest) = segs.split_first()?;
        let root = self.root_of(importing_file)?;
        // Pick the crate module base + the module-path prefix `rest` extends.
        let (base, mut path): (String, Vec<String>) = match *head {
            "crate" => (self.crate_module_base(root), Vec::new()),
            "self" => (
                self.crate_module_base(root),
                self.module_segments(root, importing_file),
            ),
            "super" => {
                let mut segs = self.module_segments(root, importing_file);
                segs.pop(); // drop the current module → its parent
                (self.crate_module_base(root), segs)
            }
            // An external crate by name → a workspace member's src, else unknown.
            dep => {
                let pkg = self.packages.get(dep)?;
                (self.crate_module_base(&pkg.dir), Vec::new())
            }
        };
        path.extend(rest.iter().map(|s| s.to_string()));
        self.probe_rust_module(&base, &path)
    }

    /// The module-path segments of `file` relative to its crate's module base:
    /// `src/api/v1.rs` (base `src`) → `["api", "v1"]`; a `mod.rs`/`lib.rs`/
    /// `main.rs` names its *directory* module, so its file stem is dropped.
    fn module_segments(&self, root: &str, file: &str) -> Vec<String> {
        let base = self.crate_module_base(root);
        let rel = file
            .strip_prefix(&base)
            .map(|r| r.trim_start_matches('/'))
            .unwrap_or(file);
        let stem = rel.strip_suffix(".rs").unwrap_or(rel);
        let mut segs: Vec<String> = stem.split('/').map(str::to_string).collect();
        if matches!(
            segs.last().map(String::as_str),
            Some("mod" | "lib" | "main")
        ) {
            segs.pop();
        }
        segs
    }

    /// Probe a mod-path (`base` + segments) as a file: `<base>/a/b.rs`, then
    /// `<base>/a/b/mod.rs`; an empty path is the crate root module (`lib.rs`/
    /// `main.rs`).
    fn probe_rust_module(&self, base: &str, segs: &[String]) -> Option<String> {
        if segs.is_empty() {
            for root_file in ["lib.rs", "main.rs", "mod.rs"] {
                let p = join(base, root_file);
                if self.exists(&p) {
                    return Some(p);
                }
            }
            return None;
        }
        let stem = join(base, &segs.join("/"));
        let file = format!("{stem}.rs");
        if self.exists(&file) {
            return Some(file);
        }
        let mod_rs = format!("{stem}/mod.rs");
        if self.exists(&mod_rs) {
            return Some(mod_rs);
        }
        None
    }
}

impl ModuleResolver for SourceModuleResolver<'_> {
    fn resolve(&self, importing_file: &str, specifier: &str) -> Option<String> {
        // Rust importers resolve mod-paths (`crate::a`, `self`/`super`, dep crates)
        // — a distinct scheme from JS/TS specifiers (ADR-0021: same port, per-
        // language adapter), dispatched by the importing file's language. JS-shaped
        // specifiers (`./x`, `@scope/pkg`, `a/b`) are never valid Rust `use` paths;
        // they appear only in language-neutral DSL fixtures, so they fall through
        // to the generic source resolution below.
        if importing_file.ends_with(".rs")
            && !specifier.starts_with('.')
            && !specifier.starts_with('@')
            && !specifier.contains('/')
        {
            return self.resolve_rust(importing_file, specifier);
        }
        if specifier.starts_with('.') {
            let joined = normalize_join(dir_of(importing_file), specifier);
            return self.probe(&joined);
        }
        // Workspace package: exact name → entry; `name/sub` → dir/sub. The longest
        // matching name wins, so `@acme/ui` beats a hypothetical `@acme`.
        let (name, pkg) = self.package_of(specifier)?;
        if specifier == name {
            return self.entry_of(pkg);
        }
        let sub = &specifier[name.len() + 1..];
        self.probe(&join(&pkg.dir, sub))
    }
}

/// Directory part of a `/`-separated relative path (`a/b/x.ts` → `a/b`, `x.ts`
/// → `""`). Paths are normalized to `/` by the `Source` (HLD §11.4). The crate's
/// single home for this helper (also used by `Engine`'s workspace maintenance).
pub(crate) fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Is `path` strictly **inside** the directory/namespace `prefix` (`a/b/c` under
/// `a/b`)? The allocation-free form of `path.starts_with(&format!("{prefix}/"))`,
/// which this crate used to evaluate once per project per resolution.
fn is_under(path: &str, prefix: &str) -> bool {
    path.len() > prefix.len() && path.as_bytes()[prefix.len()] == b'/' && path.starts_with(prefix)
}

/// Join a dir and a relative name, dropping the empty-dir leading slash.
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), name)
    }
}

/// Resolve a `./`-relative specifier against `base`, honoring `.`/`..` segments.
fn normalize_join(base: &str, rel: &str) -> String {
    let mut segs: Vec<&str> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('/').collect()
    };
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            other => segs.push(other),
        }
    }
    segs.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use filigrio_core::{ChangeSet, Error, Result, Revision};
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    struct MapSource(RefCell<BTreeMap<String, String>>);
    impl MapSource {
        fn new(files: &[(&str, &str)]) -> Self {
            MapSource(RefCell::new(
                files
                    .iter()
                    .map(|(p, c)| (p.to_string(), c.to_string()))
                    .collect(),
            ))
        }
    }
    impl Source for MapSource {
        fn poll(&self, _since: Option<&Revision>) -> Result<ChangeSet> {
            Ok(ChangeSet::default())
        }
        fn read(&self, path: &str) -> Result<Vec<u8>> {
            self.0
                .borrow()
                .get(path)
                .map(|s| s.clone().into_bytes())
                .ok_or_else(|| Error::NotFound(path.into()))
        }
        fn exists(&self, path: &str) -> bool {
            self.0.borrow().contains_key(path)
        }
    }

    /// A `Workspace` from `(root, name, entry?)` triples.
    fn ws(projects: &[(&str, &str, Option<&str>)]) -> Workspace {
        Workspace {
            projects: projects
                .iter()
                .map(|(root, name, entry)| {
                    (
                        root.to_string(),
                        filigrio_core::Project {
                            root: root.to_string(),
                            manifest: "package.json".into(),
                            name: Some(name.to_string()),
                            entry: entry.map(str::to_string),
                            deps: Vec::new(),
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn resolves_workspace_package_by_name_and_main() {
        let src = MapSource::new(&[("packages/ui/src/index.ts", "export function greet() {}")]);
        let r = SourceModuleResolver::from_workspace(
            &src,
            &ws(&[("packages/ui", "@acme/ui", Some("src/index.ts"))]),
        );
        assert_eq!(
            r.resolve("packages/app/src/index.ts", "@acme/ui")
                .as_deref(),
            Some("packages/ui/src/index.ts"),
        );
    }

    #[test]
    fn resolves_workspace_subpath() {
        let src = MapSource::new(&[("pkg/lib/util.ts", "")]);
        let r = SourceModuleResolver::from_workspace(&src, &ws(&[("pkg", "@acme/ui", None)]));
        assert_eq!(
            r.resolve("app/main.ts", "@acme/ui/lib/util").as_deref(),
            Some("pkg/lib/util.ts"),
        );
    }

    #[test]
    fn resolves_relative_with_extension_probe() {
        let src = MapSource::new(&[("src/shapes.ts", ""), ("src/main.ts", "")]);
        let r = SourceModuleResolver::from_workspace(&src, &Workspace::default());
        assert_eq!(
            r.resolve("src/main.ts", "./shapes").as_deref(),
            Some("src/shapes.ts")
        );
    }

    #[test]
    fn resolves_relative_parent_and_index() {
        let src = MapSource::new(&[("a/b/main.ts", ""), ("a/util/index.ts", "")]);
        let r = SourceModuleResolver::from_workspace(&src, &Workspace::default());
        assert_eq!(
            r.resolve("a/b/main.ts", "../util").as_deref(),
            Some("a/util/index.ts"),
        );
    }

    #[test]
    fn default_entry_when_no_main() {
        let src = MapSource::new(&[("pkg/src/index.ts", "")]);
        let r = SourceModuleResolver::from_workspace(&src, &ws(&[("pkg", "p", None)]));
        assert_eq!(r.resolve("x.ts", "p").as_deref(), Some("pkg/src/index.ts"));
    }

    #[test]
    fn external_specifier_is_none() {
        let src = MapSource::new(&[("pkg/x.ts", "")]);
        let r = SourceModuleResolver::from_workspace(&src, &ws(&[("pkg", "p", None)]));
        assert_eq!(r.resolve("x.ts", "react"), None);
        assert_eq!(r.resolve("x.ts", "jsr:@std/path"), None);
    }

    // ---- Rust mod-path resolution (ADR-0020 `pub use` / `use` following) ------

    /// A single Rust crate rooted at `root` (its `Cargo.toml` dir).
    fn rust_ws(root: &str, name: &str) -> Workspace {
        Workspace {
            projects: [(
                root.to_string(),
                filigrio_core::Project {
                    root: root.to_string(),
                    manifest: "Cargo.toml".into(),
                    name: Some(name.to_string()),
                    entry: None,
                    deps: Vec::new(),
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn rust_crate_path_to_file() {
        // `crate::api` from a crate rooted at "" whose src is `src/` → `src/api.rs`.
        let src = MapSource::new(&[("src/lib.rs", ""), ("src/api.rs", "")]);
        let r = SourceModuleResolver::from_workspace(&src, &rust_ws("", "app"));
        assert_eq!(
            r.resolve("src/main.rs", "crate::api").as_deref(),
            Some("src/api.rs"),
        );
    }

    #[test]
    fn rust_crate_path_when_build_root_is_src() {
        // The oracle-diff harness builds `<fixture>/src` directly, so files carry
        // no `src/` prefix and the crate module base is the root itself.
        let src = MapSource::new(&[("lib.rs", ""), ("api.rs", "")]);
        let r = SourceModuleResolver::from_workspace(&src, &rust_ws("", "app"));
        assert_eq!(
            r.resolve("main.rs", "crate::api").as_deref(),
            Some("api.rs"),
        );
    }

    #[test]
    fn rust_nested_and_mod_rs() {
        // `crate::api::v1` → `src/api/v1.rs`; `crate::api` when api is a directory
        // module → `src/api/mod.rs`.
        let src = MapSource::new(&[
            ("src/lib.rs", ""),
            ("src/api/mod.rs", ""),
            ("src/api/v1.rs", ""),
        ]);
        let r = SourceModuleResolver::from_workspace(&src, &rust_ws("", "app"));
        assert_eq!(
            r.resolve("src/lib.rs", "crate::api::v1").as_deref(),
            Some("src/api/v1.rs"),
        );
        assert_eq!(
            r.resolve("src/lib.rs", "crate::api").as_deref(),
            Some("src/api/mod.rs"),
        );
    }

    #[test]
    fn rust_self_and_super() {
        // From `src/api/v1.rs` (module `crate::api::v1`): `self::inner` →
        // `src/api/v1/inner.rs`; `super::shared` → `src/api/shared.rs`.
        let src = MapSource::new(&[
            ("src/api/v1.rs", ""),
            ("src/api/v1/inner.rs", ""),
            ("src/api/shared.rs", ""),
        ]);
        let r = SourceModuleResolver::from_workspace(&src, &rust_ws("", "app"));
        assert_eq!(
            r.resolve("src/api/v1.rs", "self::inner").as_deref(),
            Some("src/api/v1/inner.rs"),
        );
        assert_eq!(
            r.resolve("src/api/v1.rs", "super::shared").as_deref(),
            Some("src/api/shared.rs"),
        );
    }

    #[test]
    fn rust_dependency_crate() {
        // `mylib::thing` from the `app` crate resolves into the `mylib` crate.
        let src = MapSource::new(&[
            ("app/src/main.rs", ""),
            ("libs/mylib/src/lib.rs", ""),
            ("libs/mylib/src/thing.rs", ""),
        ]);
        let mut ws = rust_ws("app", "app");
        ws.projects.insert(
            "libs/mylib".into(),
            filigrio_core::Project {
                root: "libs/mylib".into(),
                manifest: "Cargo.toml".into(),
                name: Some("mylib".into()),
                entry: None,
                deps: Vec::new(),
            },
        );
        let r = SourceModuleResolver::from_workspace(&src, &ws);
        assert_eq!(
            r.resolve("app/src/main.rs", "mylib::thing").as_deref(),
            Some("libs/mylib/src/thing.rs"),
        );
    }

    #[test]
    fn rust_external_crate_is_none() {
        // A `use` from a crate we don't have (std / a non-workspace dep) → None.
        let src = MapSource::new(&[("src/lib.rs", "")]);
        let r = SourceModuleResolver::from_workspace(&src, &rust_ws("", "app"));
        assert_eq!(r.resolve("src/lib.rs", "std::collections"), None);
        assert_eq!(r.resolve("src/lib.rs", "serde::de"), None);
    }
}
