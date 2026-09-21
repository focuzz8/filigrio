//! Reconciliation logic (ADR-0032 §6).
//!
//! Startup reconcile: always mtime scan, deep rehash only on crash.

use crate::dedup::{dedup_file, get_mtime};
use filigrio_core::{ChangeSet, GraphState};

/// Configuration for reconciliation.
///
/// Deliberately has NO `fix` knob: reconcile is pure detection and ALWAYS
/// returns the changeset. The caller (`Pipeline::reconcile_and_apply`) applies
/// it iff there is drift — the old `fix:false → None` dry-run trap caused two
/// real bugs, and ADR-0042 F6 removed the read-only mode entirely.
#[derive(Clone, Debug, Default)]
pub struct ReconcileConfig {
    /// Whether to run deep reconcile (ignore mtime fast-path).
    pub deep: bool,
}

/// Report from reconciliation.
#[derive(Clone, Debug)]
pub struct ReconcileReport {
    /// Number of files checked.
    pub files_checked: usize,
    /// Number of files changed.
    pub files_changed: usize,
    /// Number of files added.
    pub files_added: usize,
    /// Number of files removed.
    pub files_removed: usize,
    /// Whether drift was detected.
    pub has_drift: bool,
}

/// Reconcile a project against its manifest. The changeset is ALWAYS returned
/// (empty when there is no drift) — applying it is the caller's decision.
pub fn reconcile(
    root: &std::path::Path,
    state: &GraphState,
    config: &ReconcileConfig,
) -> Result<(ReconcileReport, ChangeSet), String> {
    let mut report = ReconcileReport {
        files_checked: 0,
        files_changed: 0,
        files_added: 0,
        files_removed: 0,
        has_drift: false,
    };

    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut removed = Vec::new();

    // Enumerate through the SHARED source boundary (ADR-0032a R1) — the same
    // `.gitignore` + noise walk the cold build uses — instead of the old
    // hand-rolled `read_dir` recursion, which bypassed `.gitignore` (reconcile ≠
    // cold) AND descended into `node_modules`/`target` before filtering (O(tree)).
    let boundary = filigrio_ingest::SourceBoundary::new(root);
    let files = boundary
        .walk()
        .map_err(|e| format!("boundary walk failed: {}", e))?;
    for path_str in &files {
        let path = root.join(path_str);

        report.files_checked += 1;

        // Add/modify detection covers files whose *changes affect the graph*: code
        // (→ nodes) and project manifests (→ workspace, ADR-0019 — dropping them broke
        // cross-file resolution). Skip the rest (README, images, lockfiles) so we don't
        // hash them every run. The `removed` pass below still uses the FULL walk, so a
        // still-present non-indexable manifest entry is never spuriously dropped.
        if !filigrio_core::is_indexable(path_str) {
            continue;
        }

        match dedup_file(
            &path,
            path_str,
            &state.manifest,
            get_mtime(&path),
            config.deep,
        ) {
            Ok(crate::dedup::DedupResult::NotFound) => {
                // New file
                added.push(path_str.to_string());
                report.files_added += 1;
                report.has_drift = true;
            }
            Ok(crate::dedup::DedupResult::Changed) => {
                // Changed file
                modified.push(path_str.to_string());
                report.files_changed += 1;
                report.has_drift = true;
            }
            Ok(crate::dedup::DedupResult::Unchanged) => {
                // No change
            }
            Err(_) => {
                // Error reading file — treat as changed
                modified.push(path_str.to_string());
                report.files_changed += 1;
                report.has_drift = true;
            }
        }
    }

    // A manifest entry is removed when it is no longer in the boundary walk — which
    // covers BOTH a deleted file AND one that is newly `.gitignore`d but still on
    // disk (ADR-0032a R3, the re-scoping case with no FS event of its own). Keying
    // off `!exists()` alone missed the latter, rotting the graph. The set is the
    // full walk (not code-filtered) so a still-present non-code manifest entry, if
    // any, is not spuriously dropped.
    let in_scope: std::collections::HashSet<&str> = files.iter().map(|s| s.as_str()).collect();
    for manifest_path in state.manifest.entries.keys() {
        if !in_scope.contains(manifest_path.as_str()) {
            removed.push(manifest_path.clone());
            report.files_removed += 1;
            report.has_drift = true;
        }
    }

    // The changeset is always returned — empty when no drift was detected.
    let changeset = ChangeSet {
        added,
        modified,
        removed,
    };

    Ok((report, changeset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use filigrio_core::GraphState;
    use std::fs;
    use tempfile::TempDir;

    /// R1/T5 — reconcile enumerates through the shared `.gitignore` boundary, so a
    /// gitignored code file is NOT reported as drift. Before R1 the hand-rolled
    /// `read_dir` walk ignored `.gitignore`, so `vendor/lib.rs` showed as `added` —
    /// reconcile ≠ cold, reintroducing the pollution ADR-0022 fixed.
    #[test]
    fn reconcile_respects_gitignore_boundary() {
        let temp = TempDir::new().unwrap();
        let r = temp.path();
        let root = r;

        fs::create_dir_all(r.join("src")).unwrap();
        fs::create_dir_all(r.join("vendor")).unwrap();
        fs::write(r.join(".gitignore"), "vendor/\n").unwrap();
        fs::write(r.join("src/lib.rs"), "fn a() {}").unwrap();
        fs::write(r.join("vendor/lib.rs"), "fn vendored() {}").unwrap();

        // Empty manifest → everything in-scope is "added"; nothing out-of-scope is.
        let state = GraphState::default();
        let (report, changeset) = reconcile(root, &state, &ReconcileConfig::default()).unwrap();

        assert!(report.has_drift, "src/lib.rs is new → drift");
        let cs = changeset;
        assert!(
            cs.added.iter().any(|p| p == "src/lib.rs"),
            "in-scope file reported: {:?}",
            cs.added
        );
        assert!(
            !cs.added.iter().any(|p| p.contains("vendor")),
            "gitignored vendor/ must NOT appear as drift: {:?}",
            cs.added
        );
    }

    /// R3/T4 — a file that becomes `.gitignore`d **while still on disk** must be
    /// reported `removed`. This is the re-scoping case with no FS event of its own:
    /// `vendor/x.rs` was indexed, then `vendor/` is ignored — the file still exists,
    /// so the old `!exists()` check missed it and it rotted in the graph. Removal
    /// must key off "no longer in the boundary walk", not "no longer on disk".
    #[test]
    fn reconcile_removes_a_newly_gitignored_but_still_present_file() {
        let temp = TempDir::new().unwrap();
        let r = temp.path();
        let root = r;

        fs::create_dir_all(r.join("src")).unwrap();
        fs::create_dir_all(r.join("vendor")).unwrap();
        fs::write(r.join("src/lib.rs"), "fn a() {}").unwrap();
        fs::write(r.join("vendor/x.rs"), "fn v() {}").unwrap(); // still on disk
        fs::write(r.join(".gitignore"), "vendor/\n").unwrap(); // ...but now ignored

        // Manifest as if BOTH were indexed before the .gitignore existed.
        let mut state = GraphState::default();
        for p in ["src/lib.rs", "vendor/x.rs"] {
            state.manifest.entries.insert(
                p.to_string(),
                filigrio_core::ManifestEntry {
                    hash: 1,
                    last_modified: None,
                    revision: None,
                },
            );
        }

        let (report, changeset) = reconcile(root, &state, &ReconcileConfig::default()).unwrap();
        assert!(report.has_drift);
        let cs = changeset;
        assert!(
            cs.removed.iter().any(|p| p == "vendor/x.rs"),
            "newly-gitignored file (still on disk) must be removed: {:?}",
            cs.removed
        );
        assert!(
            !cs.removed.iter().any(|p| p == "src/lib.rs"),
            "in-scope file must NOT be removed: {:?}",
            cs.removed
        );
    }

    /// R1 — reconcile must not descend into (or report) built-in noise dirs even
    /// with no `.gitignore`. The old `read_dir` walk descended into `node_modules`
    /// fully before filtering; the boundary prunes it.
    #[test]
    fn reconcile_prunes_builtin_noise() {
        let temp = TempDir::new().unwrap();
        let r = temp.path();
        let root = r;

        fs::create_dir_all(r.join("src")).unwrap();
        fs::create_dir_all(r.join("node_modules/pkg")).unwrap();
        fs::write(r.join("src/lib.rs"), "fn a() {}").unwrap();
        fs::write(r.join("node_modules/pkg/index.js"), "module.exports={}").unwrap();

        let state = GraphState::default();
        let (_report, changeset) = reconcile(root, &state, &ReconcileConfig::default()).unwrap();
        let cs = changeset;
        assert!(
            !cs.added.iter().any(|p| p.contains("node_modules")),
            "node_modules must be pruned: {:?}",
            cs.added
        );
    }

    /// The new contract (fix-footgun removal): `reconcile` ALWAYS returns the
    /// changeset — there is no `fix` knob anywhere in its API. Applying it is
    /// the caller's job (`Pipeline::reconcile_and_apply` applies iff drift);
    /// the old `fix:false → None` dry-run trap caused two real bugs (silent
    /// no-op ProjectIndex, silent one-shot write skips) before ADR-0042 F6
    /// removed the read-only mode entirely.
    #[test]
    fn reconcile_always_returns_the_changeset() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn a() {}").unwrap();

        // Drift exists (empty manifest → src/lib.rs is new). Even a caller that
        // would NOT apply still gets the non-empty changeset — no Option, no knob.
        let state = GraphState::default();
        let (report, changeset) =
            reconcile(root, &state, &ReconcileConfig { deep: false }).unwrap();
        assert!(report.has_drift);
        assert!(
            !changeset.added.is_empty(),
            "drift exists → the returned changeset is non-empty, unconditionally"
        );
    }

    #[test]
    fn test_reconcile_new_file() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create a new file
        let test_file = temp.path().join("src/test.rs");
        fs::create_dir_all(test_file.parent().unwrap()).unwrap();
        fs::write(&test_file, "fn test() {}").unwrap();

        let state = GraphState::default();
        let config = ReconcileConfig::default();

        let (report, changeset) = reconcile(root, &state, &config).unwrap();

        assert!(report.has_drift);
        assert_eq!(report.files_added, 1);

        assert_eq!(changeset.added.len(), 1);
        assert!(changeset.added[0].contains("test.rs"));
    }

    #[test]
    fn test_reconcile_removed_file() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create a file and add to manifest
        let test_file = temp.path().join("src/test.rs");
        fs::create_dir_all(test_file.parent().unwrap()).unwrap();
        fs::write(&test_file, "fn test() {}").unwrap();

        let mut state = GraphState::default();
        state.manifest.entries.insert(
            "src/test.rs".to_string(),
            filigrio_core::ManifestEntry {
                hash: 12345,
                last_modified: None,
                revision: None,
            },
        );

        // Remove the file
        fs::remove_file(&test_file).unwrap();

        let config = ReconcileConfig::default();
        let (report, _changeset) = reconcile(root, &state, &config).unwrap();

        assert!(report.has_drift);
        assert_eq!(report.files_removed, 1);
    }

    #[test]
    fn deep_reconcile_catches_mtime_unchanged_content_drift() {
        // The case deep exists for: a file whose manifest mtime MATCHES disk but whose
        // hash does NOT — content drifted with no mtime bump (restored backup, clock
        // reset, mtime-preserving copy). The SHALLOW pass trusts the mtime fast-path and
        // misses it; the DEEP pass re-hashes and catches it.
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let test_file = temp.path().join("src/main.rs");
        fs::create_dir_all(test_file.parent().unwrap()).unwrap();
        fs::write(&test_file, "fn main() {}\n").unwrap();

        // Manifest keyed relative (as the Engine keys it): mtime matches disk, hash WRONG.
        let mut state = GraphState::default();
        state.manifest.entries.insert(
            "src/main.rs".to_string(),
            filigrio_core::ManifestEntry {
                hash: 0xDEAD_BEEF,                    // not the real content hash
                last_modified: get_mtime(&test_file), // matches disk
                revision: None,
            },
        );

        // Shallow: mtime matches → fast-path returns Unchanged → NO drift (missed).
        let (shallow, _) = reconcile(root, &state, &ReconcileConfig { deep: false }).unwrap();
        assert!(
            !shallow.has_drift,
            "shallow reconcile trusts mtime and misses the content drift"
        );

        // Deep: mtime fast-path skipped → re-hash → mismatch → drift (main.rs modified).
        let (deep, changeset) = reconcile(root, &state, &ReconcileConfig { deep: true }).unwrap();
        assert!(
            deep.has_drift,
            "deep reconcile re-hashes and catches the content drift the shallow pass missed"
        );
        assert!(
            changeset.modified.iter().any(|p| p.contains("main.rs")),
            "deep reconcile reports main.rs as modified"
        );
    }
}
