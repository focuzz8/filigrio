//! The source boundary (ADR-0022; ADR-0032a R1) — the **single** predicate for
//! "is this path inside the indexable source tree."
//!
//! Before this, "in scope?" was answered three ways that drifted (ADR-0032a
//! Validation #1): `FsSource::walk()` (the correct `ignore`-crate boundary), the
//! daemon watcher's extension allowlist, and `reconcile`'s hand-rolled `read_dir`.
//! `SourceBoundary` collapses them: `walk()` enumerates and `matches()` tests one
//! path, **both from the same rules**, so the cold walk and the incremental
//! single-path scope check cannot disagree by construction.

use crate::is_builtin_noise;
use filigrio_core::{Error, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{Match, WalkBuilder};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::warn;

/// The `.gitignore`-aware source boundary for one project root (ADR-0022).
/// Ignore evaluation is repo-local and deterministic (no global git config /
/// machine state), so the same tree yields the same answer everywhere.
pub struct SourceBoundary {
    root: PathBuf,
    /// Per-directory compiled `.gitignore` (`None` = no file there), memoised so a
    /// per-file `matches()` sweep over a changeset doesn't re-read + re-parse the
    /// same ancestors on every call. Lifetime = this boundary (one apply / one
    /// enumerate), so it can't go stale — a `.gitignore` edit yields a fresh
    /// boundary via reconcile (ADR-0032a R3).
    gitignores: Mutex<HashMap<PathBuf, Option<Arc<Gitignore>>>>,
}

impl SourceBoundary {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        SourceBoundary {
            root: root.into(),
            gitignores: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The compiled `.gitignore` for `dir`, cached for this boundary's lifetime.
    fn gitignore_for(&self, dir: &Path) -> Option<Arc<Gitignore>> {
        if let Some(hit) = self
            .gitignores
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(dir)
        {
            return hit.clone();
        }
        let compiled = load_gitignore(dir).map(Arc::new);
        self.gitignores
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(dir.to_path_buf(), compiled.clone());
        compiled
    }

    /// Walk the tree, honoring `.gitignore` + the built-in noise net, returning
    /// sorted root-relative paths (stable ids across machines — HLD §11.4). This
    /// is the enumerate half of the boundary.
    pub fn walk(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let walker = WalkBuilder::new(&self.root)
            .hidden(true) // skip dotfiles/dirs (.git, .env, …)
            .git_ignore(true) // honor the repo's own .gitignore (nested)
            .git_global(false) // determinism: no ~/.config/git/ignore
            .git_exclude(false) // determinism: no .git/info/exclude
            .ignore(false) // no ripgrep `.ignore` files
            .parents(false) // never climb above the scan root
            .require_git(false) // apply .gitignore even with no .git dir (fixtures)
            .filter_entry(|e| !is_builtin_noise(&e.file_name().to_string_lossy()))
            .build();
        for entry in walker {
            let entry = entry.map_err(|e| Error::Io(format!("walk: {e}")))?;
            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                let rel = entry
                    .path()
                    .strip_prefix(&self.root)
                    .unwrap_or(entry.path());
                // A path that is not valid UTF-8 is outside the **addressable**
                // vocabulary: `ChangeSet` is `Vec<String>`, so there is no way to
                // name it downstream. Skip it loudly. This used to be
                // `to_string_lossy()`, which yielded a U+FFFD path naming no real
                // file — the apply's F8 reconcile then probed `exists()`, got
                // `false`, and folded it into `removed` with a `vanished` bump, so
                // the only trace of an unindexable file was a counter that means
                // something else entirely ("a path disappeared between detection
                // and apply"). Skipping does not index it either — nothing here
                // can — but it stops the report from lying about why.
                let Some(rel) = rel.to_str() else {
                    warn!(
                        "skipping non-UTF-8 path (not addressable — changesets are UTF-8): {}",
                        rel.display()
                    );
                    continue;
                };
                out.push(rel.replace('\\', "/"));
            }
        }
        out.sort(); // determinism (HLD §8)
        out.dedup();
        Ok(out)
    }

    /// Whether the single root-relative `rel` is inside the boundary — the same
    /// verdict `walk()` would give for that path, without walking the tree.
    ///
    /// This is the scope-authority half (ADR-0032a R2): the apply path consults
    /// it so an out-of-scope file named by any producer never enters the graph.
    pub fn matches(&self, rel: &str) -> bool {
        let rel = rel.replace('\\', "/");
        let parts: Vec<&str> = rel
            .trim_start_matches("./")
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        if parts.is_empty() {
            return false;
        }

        // The `.gitignore` matcher stack, built lazily as we descend, shallow →
        // deep. Deeper files override shallower ones (git semantics), and a file
        // can't be re-included once an ancestor dir is pruned — both handled by
        // deciding each subpath against the stack built *so far* and returning
        // early on a dir-level ignore.
        let mut stack: Vec<Arc<Gitignore>> = Vec::new();
        if let Some(gi) = self.gitignore_for(&self.root) {
            stack.push(gi);
        }

        let mut cur = self.root.clone();
        let last = parts.len() - 1;
        for (i, name) in parts.iter().enumerate() {
            let is_dir = i < last;

            // Name-based prune, mirroring the walk: `hidden(true)` drops any
            // dot-prefixed component (file or dir), and the walk's `filter_entry`
            // drops ANY entry named like builtin noise — files included, not just
            // dirs — so the noise check here must be component-kind-agnostic too.
            if name.starts_with('.') || is_builtin_noise(name) {
                return false;
            }

            cur.push(name);

            if let Some(true) = decision(&stack, &cur, is_dir) {
                return false; // ignored file, or pruned dir subtree
            }

            // Descend: this dir's own `.gitignore` governs everything below it.
            if is_dir {
                if let Some(gi) = self.gitignore_for(&cur) {
                    stack.push(gi);
                }
            }
        }
        true
    }
}

/// Load the `.gitignore` living in `dir` (rooted at `dir`, so its patterns are
/// interpreted relative to `dir` — the nested-gitignore semantics). `None` when
/// absent or unparseable (best-effort; reconcile is the authority — ADR-0032a R1).
fn load_gitignore(dir: &Path) -> Option<Gitignore> {
    let gi_path = dir.join(".gitignore");
    if !gi_path.is_file() {
        return None;
    }
    let mut b = GitignoreBuilder::new(dir);
    b.add(&gi_path); // returns Option<Error>; ignore parse errors, best-effort
    b.build().ok()
}

/// Decide one absolute path against the stack: `Some(true)` = ignored,
/// `Some(false)` = whitelisted, `None` = no rule. Iterates shallow → deep so a
/// deeper file's verdict overrides a shallower one (git last-match-wins across
/// levels); within a single file `Gitignore::matched` already applies it.
fn decision(stack: &[Arc<Gitignore>], abs: &Path, is_dir: bool) -> Option<bool> {
    let mut result = None;
    for gi in stack {
        match gi.matched(abs, is_dir) {
            Match::Ignore(_) => result = Some(true),
            Match::Whitelist(_) => result = Some(false),
            Match::None => {}
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    /// Raw-enumerate EVERY file under `root` (no filtering at all), root-relative
    /// with `/` separators — the ground truth the boundary is a filter over.
    ///
    /// "Every file" means every **addressable** file: a non-UTF-8 name has no
    /// `&str` form, so it is not in the vocabulary either half speaks
    /// (`matches` takes `&str`, `walk` returns `String`). Using
    /// `to_string_lossy()` here would invent a U+FFFD path that names no real
    /// file and then demand the two halves agree about it.
    fn all_files(root: &Path) -> BTreeSet<String> {
        fn rec(dir: &Path, root: &Path, out: &mut BTreeSet<String>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    rec(&p, root, out);
                } else if let Some(rel) = p.strip_prefix(root).unwrap().to_str() {
                    out.insert(rel.replace('\\', "/"));
                }
            }
        }
        let mut out = BTreeSet::new();
        rec(root, root, &mut out);
        out
    }

    /// THE parity invariant (R1's whole point): for every real file on disk,
    /// `matches(rel)` agrees with `walk()` membership. If these two ever drift,
    /// incremental ≠ cold — exactly the bug R1 exists to kill.
    fn assert_parity(root: &Path) {
        let boundary = SourceBoundary::new(root);
        let walked: BTreeSet<String> = boundary.walk().unwrap().into_iter().collect();
        for rel in all_files(root) {
            let in_walk = walked.contains(&rel);
            let matched = boundary.matches(&rel);
            assert_eq!(
                matched, in_walk,
                "matches({rel:?})={matched} but walk-membership={in_walk}"
            );
        }
    }

    #[test]
    fn parity_root_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, ".gitignore", "build/\n*.log\n");
        write(r, "src/main.rs", "fn main() {}");
        write(r, "build/out.js", "// generated");
        write(r, "app.log", "noise");
        assert_parity(r);
        assert!(SourceBoundary::new(r).matches("src/main.rs"));
        assert!(!SourceBoundary::new(r).matches("build/out.js"));
        assert!(!SourceBoundary::new(r).matches("app.log"));
    }

    #[test]
    fn parity_nested_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/keep.rs", "fn a() {}");
        write(r, "src/generated/.gitignore", "*.rs\n");
        write(r, "src/generated/gen.rs", "// generated");
        assert_parity(r);
        assert!(SourceBoundary::new(r).matches("src/keep.rs"));
        assert!(!SourceBoundary::new(r).matches("src/generated/gen.rs"));
    }

    #[test]
    fn parity_negation() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, ".gitignore", "*.log\n!keep.log\n");
        write(r, "keep.log", "keep me");
        write(r, "drop.log", "drop me");
        assert_parity(r);
        assert!(SourceBoundary::new(r).matches("keep.log"));
        assert!(!SourceBoundary::new(r).matches("drop.log"));
    }

    #[test]
    fn parity_no_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, ".gitignore", "secret.rs\n");
        write(r, "public.rs", "fn a() {}");
        write(r, "secret.rs", "fn s() {}");
        assert!(!r.join(".git").exists());
        assert_parity(r);
        assert!(!SourceBoundary::new(r).matches("secret.rs"));
    }

    #[test]
    fn parity_builtin_noise() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/lib.rs", "fn a() {}");
        write(r, "node_modules/pkg/index.js", "module.exports={}");
        write(r, "target/debug/build.rs", "// artifact");
        assert_parity(r);
        assert!(SourceBoundary::new(r).matches("src/lib.rs"));
        assert!(!SourceBoundary::new(r).matches("node_modules/pkg/index.js"));
        assert!(!SourceBoundary::new(r).matches("target/debug/build.rs"));
    }

    /// A **file** named like a builtin-noise dir (`target`, `node_modules`) must
    /// be out-of-scope in BOTH halves. `walk()` already drops it — its
    /// `filter_entry` checks every entry's name, files included — but `matches()`
    /// used to apply the noise check only to directory components, answering
    /// `true` for a path the walk can never yield. That is `matches ≠ walk`, the
    /// exact drift R1 exists to kill.
    #[test]
    fn file_named_like_noise_dir_is_out_of_scope_in_both_halves() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/lib.rs", "fn a() {}");
        write(r, "target", "not a dir, a file");
        write(r, "src/node_modules", "also a file");
        assert_parity(r);
        let b = SourceBoundary::new(r);
        assert!(b.matches("src/lib.rs"));
        assert!(
            !b.matches("target"),
            "file named `target` must be out of scope"
        );
        assert!(!b.matches("src/node_modules"));
    }

    /// A filename that is not valid UTF-8 is **skipped**, not lossily renamed.
    /// `to_string_lossy()` used to hand downstream a U+FFFD path that names no
    /// real file — which the apply then read back as a *vanished* file (audit
    /// §H). `ChangeSet` is `Vec<String>`, so such a name simply has no
    /// representation in the vocabulary the walk feeds.
    ///
    /// Skips cleanly (rather than false-passing) where the filesystem refuses
    /// the name — the `store/src/atomic.rs` pattern.
    #[cfg(unix)]
    #[test]
    fn non_utf8_filename_is_skipped_not_lossily_renamed() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/ok.rs", "fn a() {}");
        // 0xFF can never appear in valid UTF-8.
        let bad = r.join("src").join(OsStr::from_bytes(b"bad\xff.rs"));
        if fs::write(&bad, "fn hidden() {}").is_err() || !bad.exists() {
            eprintln!("skipped: this filesystem will not hold a non-UTF-8 filename");
            return;
        }

        let walked = SourceBoundary::new(r).walk().unwrap();
        assert_eq!(
            walked,
            vec!["src/ok.rs".to_string()],
            "the unaddressable name must be skipped, not replacement-charactered in"
        );
        assert!(
            !walked.iter().any(|p| p.contains('\u{FFFD}')),
            "a U+FFFD path names no real file: {walked:?}"
        );
        // Parity still holds: neither half claims the file (it is not in the
        // vocabulary), so `matches` and `walk` agree by omission.
        assert_parity(r);
    }

    #[test]
    fn hidden_dotfile_is_out_of_scope() {
        // `.gitignore` itself is hidden → walk never returns it, so matches() must
        // agree (R3 watches ignore files by an explicit path, not via matches()).
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, ".gitignore", "x\n");
        write(r, "src/lib.rs", "fn a() {}");
        assert!(!SourceBoundary::new(r).matches(".gitignore"));
        assert!(!SourceBoundary::new(r).matches(".config/app.rs"));
        assert_parity(r);
    }
}
