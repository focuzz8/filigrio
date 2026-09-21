//! filigrio-ingest — the `Source` port (HLD §4, ADR-0014).
//!
//! `FsSource` is a **real** adapter: it walks a directory honoring `.gitignore`
//! (getting files is the cheapest real thing worth exercising) and reads their
//! bytes. Path *classification* (`code` vs `doc` — `filigrio_core::classify`) is
//! kernel vocab, not a Source concern, so it lives in core; this crate is purely
//! the physical walk/read. Warm `poll(Some(rev))` (git diff / webhook) is a
//! Phase-5 concern; here the warm path falls back to a full walk so the Kappa
//! contract still holds.

#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]
use filigrio_core::{ChangeSet, Error, Result, Revision, Source};
use std::path::{Path, PathBuf};

mod boundary;
pub use boundary::SourceBoundary;

/// The change-producer taxonomy (ADR-0032e): `Producer`/`Produced` push sources,
/// and `TriggeredProducer` turning a pull `Source` into one via a trigger. The
/// lane a producer declares is `filigrio_core::Priority` — imported from core at
/// each use site, not re-exported from here (one name, one home).
mod producer;
pub use producer::{Produced, ProducedKind, Producer, TriggeredProducer};

/// The native-push producer: a filesystem watcher (ADR-0032a, relocated by
/// ADR-0032e).
mod watcher;
pub use watcher::{FsWatcher, WatcherConfig};

/// A `.gitignore`-aware filesystem source (ADR-0022). It walks the tree honoring
/// the repo's own `.gitignore` files (nested, negation, anchoring, `**` — via
/// ripgrep's `ignore` crate), plus a built-in safety net of universal noise dirs.
/// Ignore evaluation is **repo-local and deterministic** — no global git config
/// or machine state — so the same tree yields the same walk everywhere (HLD §8).
pub struct FsSource {
    root: PathBuf,
    /// One boundary for this source's lifetime — its `.gitignore` matcher cache is
    /// reused across every `walk()`/`in_scope()` call rather than rebuilt per call.
    boundary: SourceBoundary,
}

impl FsSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let boundary = SourceBoundary::new(root.clone());
        FsSource { root, boundary }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Walk the tree, honoring `.gitignore` + the built-in net, returning sorted
    /// root-relative paths (stable ids across machines — HLD §11.4). Delegates to
    /// the held [`SourceBoundary`] so the enumerate and single-path (`in_scope`)
    /// verdicts share one implementation (ADR-0032a R1).
    fn walk(&self) -> Result<Vec<String>> {
        self.boundary.walk()
    }
}

impl Source for FsSource {
    fn poll(&self, _since: Option<&Revision>) -> Result<ChangeSet> {
        if !self.root.exists() {
            return Err(Error::Io(format!(
                "source root does not exist: {}",
                self.root.display()
            )));
        }
        // Skeleton: warm and cold both do a full walk → all-added. `build()`
        // reconciles this snapshot against the prior manifest to synthesize
        // removals (ADR-0022) — deletions and newly-ignored files both prune there.
        Ok(ChangeSet::all_added(self.walk()?))
    }

    fn read(&self, path: &str) -> Result<Vec<u8>> {
        let full = self.root.join(path);
        std::fs::read(&full).map_err(|e| Error::Io(format!("{}: {e}", full.display())))
    }

    /// One `stat` instead of the whole file. `read` succeeds exactly for readable
    /// regular files, so `is_file()` is the same predicate without the copy — the
    /// module resolver probes ~19 candidate paths per unresolved specifier.
    fn exists(&self, path: &str) -> bool {
        self.root.join(path).is_file()
    }

    fn root(&self) -> Option<&Path> {
        Some(&self.root)
    }

    /// A file is in scope iff it survives the same `.gitignore` + noise boundary
    /// `walk()` uses (ADR-0032a R1). This is the scope authority the daemon apply
    /// path consults (R2) so no producer can inject an out-of-scope file.
    fn in_scope(&self, rel: &str) -> bool {
        self.boundary.matches(rel)
    }
}

/// Universal build/dependency-artifact dirs pruned regardless of any
/// `.gitignore` — a floor so we never *descend* into a 100k-file `node_modules`
/// even in a repo that forgot to ignore it (ADR-0022). `.gitignore` handles the
/// rest; `hidden(true)` handles dotfiles. `.filigrio-out`/`.filigrio` are OUR own
/// output (state.json + on-demand exports): a resident daemon watches the project root,
/// so if these weren't pruned it would watch its own writes — a self-trigger loop.
pub(crate) fn is_builtin_noise(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "target"
            | "node_modules"
            | ".venv"
            | "__pycache__"
            | ".mypy_cache"
            | ".filigrio-out"
            | ".filigrio"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// Write `content` to `root/rel`, creating parent dirs.
    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    /// The sorted set of relative paths `poll` surfaces.
    fn walked(root: &Path) -> Vec<String> {
        FsSource::new(root).poll(None).unwrap().added
    }

    #[test]
    fn gitignore_excludes_matched_files_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, ".gitignore", "build/\n*.log\n");
        write(r, "src/main.rs", "fn main() {}");
        write(r, "build/out.js", "// generated");
        write(r, "app.log", "noise");
        let files = walked(r);
        assert!(files.contains(&"src/main.rs".to_string()), "{files:?}");
        assert!(
            !files.iter().any(|f| f.contains("build/")),
            "build/ dir ignored: {files:?}"
        );
        assert!(!files.contains(&"app.log".to_string()), "*.log ignored");
    }

    #[test]
    fn nested_gitignore_adds_rules() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/keep.rs", "fn a() {}");
        write(r, "src/generated/.gitignore", "*.rs\n");
        write(r, "src/generated/gen.rs", "// generated");
        let files = walked(r);
        assert!(files.contains(&"src/keep.rs".to_string()));
        assert!(
            !files.contains(&"src/generated/gen.rs".to_string()),
            "nested .gitignore ignores gen.rs: {files:?}"
        );
    }

    #[test]
    fn negation_reincludes_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, ".gitignore", "*.log\n!keep.log\n");
        write(r, "keep.log", "keep me");
        write(r, "drop.log", "drop me");
        let files = walked(r);
        assert!(files.contains(&"keep.log".to_string()), "{files:?}");
        assert!(!files.contains(&"drop.log".to_string()));
    }

    #[test]
    fn gitignore_applies_without_a_git_dir() {
        // Fixtures/tempdirs have no `.git/` — rules must still apply
        // (require_git(false)).
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, ".gitignore", "secret.rs\n");
        write(r, "public.rs", "fn a() {}");
        write(r, "secret.rs", "fn s() {}");
        assert!(!r.join(".git").exists());
        let files = walked(r);
        assert!(files.contains(&"public.rs".to_string()));
        assert!(!files.contains(&"secret.rs".to_string()), "{files:?}");
    }

    #[test]
    fn builtin_noise_ignored_without_gitignore() {
        // No `.gitignore` at all — the built-in safety net still prunes the
        // universal noise dirs (and never descends into them).
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/lib.rs", "fn a() {}");
        write(r, "node_modules/pkg/index.js", "module.exports={}");
        write(r, "target/debug/build.rs", "// artifact");
        let files = walked(r);
        assert_eq!(
            files,
            vec!["src/lib.rs".to_string()],
            "only source: {files:?}"
        );
    }
}
