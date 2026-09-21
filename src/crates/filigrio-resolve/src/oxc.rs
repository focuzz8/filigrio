//! `OxcResolver` — the **exact** JS/TS `ModuleResolver` tier (ADR-0018), backed
//! by [`oxc_resolver`] (the Rust port of webpack enhanced-resolve +
//! tsconfig-paths that Rspack/Rolldown use). It resolves against the *real*
//! filesystem, so it is only selected when the `Source` exposes a root
//! ([`filigrio_core::Source::root`]); a mock source falls back to the hand-rolled
//! [`crate::SourceModuleResolver`].
//!
//! Install-free bridge: a not-installed checkout has no `node_modules` symlinks,
//! so cross-project package names (`@acme/ui`) are fed to oxc as **aliases**
//! built from our [`Workspace`] (name → the package's absolute dir). oxc then
//! does the hard part exactly — extension probing, `main`/`module`/`exports`
//! resolution, `index` files, relative paths, and (when present) real
//! `node_modules`. Anything that resolves into `node_modules` or outside the root
//! is an external boundary → `None` (not indexed), matching the source-only tier.

use filigrio_core::{ModuleResolver, Workspace};
use oxc_resolver::{AliasValue, ResolveOptions, Resolver};
use std::path::{Path, PathBuf};

pub struct OxcResolver {
    root: PathBuf,
    resolver: Resolver,
}

impl OxcResolver {
    /// Build an oxc resolver rooted at `root`, with workspace-package aliases
    /// derived from `ws` and JS/TS extensions added to oxc's defaults.
    ///
    /// oxc resolves against **absolute** paths, so the root is canonicalized once
    /// here; results are re-relativized to it (matching node `source_file`s).
    pub fn new(root: &Path, ws: &Workspace) -> Self {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let alias: Vec<(String, Vec<AliasValue>)> = ws
            .by_name()
            .into_iter()
            .map(|(name, p)| {
                let abs = root.join(&p.root);
                (
                    name.to_string(),
                    vec![AliasValue::Path(abs.to_string_lossy().into_owned())],
                )
            })
            .collect();
        let extensions: Vec<String> = [
            ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".json",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let options = ResolveOptions {
            alias,
            extensions,
            main_fields: vec!["module".into(), "main".into()],
            condition_names: vec![
                "node".into(),
                "import".into(),
                "require".into(),
                "default".into(),
            ],
            ..Default::default()
        };
        OxcResolver {
            root,
            resolver: Resolver::new(options),
        }
    }
}

impl ModuleResolver for OxcResolver {
    fn resolve(&self, importing_file: &str, specifier: &str) -> Option<String> {
        // oxc resolves *from a directory*: the importing file's parent.
        let abs_file = self.root.join(importing_file);
        let dir = abs_file.parent()?;
        let resolution = self.resolver.resolve(dir, specifier).ok()?;
        // Re-relativize to the scan root so it matches node `source_file`s.
        let rel = resolution
            .path()
            .strip_prefix(&self.root)
            .ok()?
            .to_string_lossy()
            .replace('\\', "/");
        // A dep followed into node_modules is an external boundary, not indexed.
        if rel.split('/').any(|seg| seg == "node_modules") {
            return None;
        }
        Some(rel)
    }
}
