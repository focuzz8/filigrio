//! The **export graph** (ADR-0020): per-module export tables + the walk that
//! follows re-exports (barrels) to a symbol's real definition.
//!
//! Modules are keyed by [`ModuleId`], derived from each file via the
//! [`module_of`](crate::module_of) bridge (ADR-0021) — identity today (module =
//! file for TS/JS/Python). When Go packages / Rust mod-trees land, only
//! `module_of` changes; the tables and the walk here are unchanged.
//!
//! Re-export specifiers are resolved to a target module by the same
//! [`ModuleResolver`] used for imports (ADR-0018). The walk falls back to "any
//! def named `x` in the entry module" so a language that emits no exports (or an
//! incomplete table) degrades to the previous file-level behavior — the change
//! never regresses an already-resolved edge.

use crate::{module_of, ModuleId};
use filigrio_core::{Export, ModuleResolver, NodeId};
use std::collections::{BTreeMap, BTreeSet};

/// One resolved export-table entry.
enum Entry {
    /// Defined and exported here.
    Terminal(NodeId),
    /// `export { imported } from specifier` — follow.
    ReExport { specifier: String, imported: String },
}

pub struct ExportGraph<'a> {
    /// module → exported name → entry.
    tables: BTreeMap<ModuleId, BTreeMap<String, Entry>>,
    /// module → `export *` specifiers.
    stars: BTreeMap<ModuleId, Vec<String>>,
    /// `(module, name)` → a definition in that module — the module-level fallback.
    local_def: &'a BTreeMap<(ModuleId, String), NodeId>,
    resolver: &'a dyn ModuleResolver,
}

impl<'a> ExportGraph<'a> {
    /// Build the tables from each file's export list. `local_def` maps
    /// `(file, name)` to a definition, used to resolve `Local` exports to their
    /// node (and as the fallback).
    pub fn build(
        exports_by_file: &BTreeMap<String, Vec<Export>>,
        local_def: &'a BTreeMap<(ModuleId, String), NodeId>,
        resolver: &'a dyn ModuleResolver,
    ) -> Self {
        let mut tables: BTreeMap<ModuleId, BTreeMap<String, Entry>> = BTreeMap::new();
        let mut stars: BTreeMap<ModuleId, Vec<String>> = BTreeMap::new();
        for (file, exports) in exports_by_file {
            // The physical file's exports belong to its semantic module (ADR-0021).
            let module = module_of(file);
            let table = tables.entry(module.clone()).or_default();
            for export in exports {
                match export {
                    Export::Local { name } => {
                        if let Some(def) = local_def.get(&(module.clone(), name.clone())) {
                            table.insert(name.clone(), Entry::Terminal(def.clone()));
                        }
                    }
                    Export::ReExport {
                        name,
                        specifier,
                        imported,
                    } => {
                        table.insert(
                            name.clone(),
                            Entry::ReExport {
                                specifier: specifier.clone(),
                                imported: imported.clone(),
                            },
                        );
                    }
                    Export::Star { specifier } => {
                        stars
                            .entry(module.clone())
                            .or_default()
                            .push(specifier.clone());
                    }
                }
            }
        }
        ExportGraph {
            tables,
            stars,
            local_def,
            resolver,
        }
    }

    /// The definition that `imported` refers to when exported by module `entry` —
    /// following named/`*` re-exports, then falling back to a def in `entry`.
    /// `entry` is a [`ModuleId`] (the caller bridges the resolved file via
    /// [`module_of`](crate::module_of)).
    pub fn resolve_import(&self, entry: &ModuleId, imported: &str) -> Option<NodeId> {
        let mut visited = BTreeSet::new();
        self.walk(entry, imported, &mut visited).or_else(|| {
            self.local_def
                .get(&(entry.clone(), imported.to_string()))
                .cloned()
        })
    }

    fn walk(
        &self,
        module: &ModuleId,
        name: &str,
        visited: &mut BTreeSet<(ModuleId, String)>,
    ) -> Option<NodeId> {
        if !visited.insert((module.clone(), name.to_string())) {
            return None; // cycle
        }
        if let Some(entry) = self.tables.get(module).and_then(|t| t.get(name)) {
            match entry {
                Entry::Terminal(def) => return Some(def.clone()),
                Entry::ReExport {
                    specifier,
                    imported,
                } => {
                    // Resolver returns the target *file*; key the walk by its
                    // module (identity today — the ADR-0021 bridge).
                    let target = self.resolver.resolve(module, specifier)?;
                    return self.walk(&module_of(&target), imported, visited);
                }
            }
        }
        // `export *` — search each wildcard source (first match wins; deep
        // multi-source ambiguity is not yet flagged AMBIGUOUS — ADR-0020).
        if let Some(specs) = self.stars.get(module) {
            for spec in specs {
                if let Some(target) = self.resolver.resolve(module, spec) {
                    if let Some(def) = self.walk(&module_of(&target), name, visited) {
                        return Some(def);
                    }
                }
            }
        }
        None
    }
}
