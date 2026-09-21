//! Tree-ish resolution and diff → `ChangeSet` (ADR-0032b §1).
//!
//! This is the whole "git-aware half" the ADR puts client-side: the hook has
//! git and the worktree in hand, so it resolves the two endpoints of the
//! transition, runs one `git diff`, and hands the daemon a ready-made
//! [`ChangeSet`]. No extraction, no engine, no filtering by language — the
//! daemon's scope+dedup gate owns which of these paths are indexable
//! (ADR-0042 F6c), and a client that duplicated that policy would be a second
//! place for it to drift.
//!
//! Every git invocation here runs with git's **hook environment stripped**.
//! Git exports `GIT_DIR`/`GIT_INDEX_FILE`/`GIT_WORK_TREE`/`GIT_PREFIX` to its
//! hooks, and `GIT_INDEX_FILE` in particular points at the *transient* index a
//! merge or commit is building — inheriting it makes a plain `git diff HEAD`
//! answer a different question than the one asked. Discovering the repository
//! from the working directory instead is both deterministic and what makes the
//! module testable against a scratch repo.

use anyhow::{anyhow, Context, Result};
use filigrio_protocol::ChangeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The four commit-boundary transitions ADR-0032b §1 subscribes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    /// `HEAD~1..HEAD` (or the whole tree, on a root commit).
    PostCommit,
    /// `<prev> <new> <branch-flag>` — old→new tree diff, branch switches only.
    PostCheckout,
    /// Old→new tree diff; git passes only the squash flag, so the old endpoint
    /// comes from `ORIG_HEAD` (see [`Endpoints::for_event`]).
    PostMerge,
    /// rebase / `commit --amend`; the rewrite pairs arrive on **stdin**.
    PostRewrite,
}

impl HookEvent {
    /// Parse git's own hook name. Unknown names are `None` — the caller warns
    /// and exits 0 rather than failing the git operation.
    pub fn parse(name: &str) -> Option<HookEvent> {
        match name {
            "post-commit" => Some(HookEvent::PostCommit),
            "post-checkout" => Some(HookEvent::PostCheckout),
            "post-merge" => Some(HookEvent::PostMerge),
            "post-rewrite" => Some(HookEvent::PostRewrite),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::PostCommit => "post-commit",
            HookEvent::PostCheckout => "post-checkout",
            HookEvent::PostMerge => "post-merge",
            HookEvent::PostRewrite => "post-rewrite",
        }
    }

    /// Whether git feeds this hook on stdin. Only `post-rewrite` does, and
    /// reading stdin for the others would block on an inherited pipe that
    /// nobody is going to close.
    pub fn reads_stdin(self) -> bool {
        matches!(self, HookEvent::PostRewrite)
    }
}

/// A repository, addressed by its worktree root.
pub struct Git {
    root: PathBuf,
}

impl Git {
    /// Discover the worktree containing `cwd`, or `None` if there is no
    /// repository there (a hook invoked by hand from the wrong place).
    pub fn discover(cwd: &Path) -> Option<Git> {
        let out = run(cwd, &["rev-parse", "--show-toplevel"]).ok()?;
        let root = String::from_utf8(out).ok()?.trim().to_string();
        if root.is_empty() {
            return None;
        }
        Some(Git {
            root: PathBuf::from(root),
        })
    }

    /// The worktree root — also the project path the changeset is submitted
    /// against, since git runs hooks from the top level and `git diff` reports
    /// paths relative to it.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn raw(&self, args: &[&str]) -> Result<Vec<u8>> {
        run(&self.root, args)
    }

    /// A revision resolved to an object id, or `None` when it does not exist
    /// (no parent on a root commit, no `ORIG_HEAD` outside a merge).
    fn resolve(&self, rev: &str) -> Option<String> {
        let out = self
            .raw(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{rev}^{{commit}}"),
            ])
            .ok()?;
        let id = String::from_utf8(out).ok()?.trim().to_string();
        (!id.is_empty()).then_some(id)
    }

    /// The repository's empty tree — the "before" endpoint of a root commit or
    /// a fresh clone. Asked of git rather than hardcoded, because the constant
    /// everyone memorises (`4b825dc…`) is the **SHA-1** empty tree and a
    /// SHA-256 repository has a different one.
    fn empty_tree(&self) -> Result<String> {
        let out = self.raw(&["hash-object", "-t", "tree", "/dev/null"])?;
        Ok(String::from_utf8(out)
            .context("git hash-object emitted non-UTF-8")?
            .trim()
            .to_string())
    }

    /// `git diff --name-status` between two tree-ishes, or between one
    /// tree-ish and the **worktree** when `new` is `None` (the squash-merge
    /// case, where HEAD has not moved but the files have).
    fn diff(&self, old: &str, new: Option<&str>) -> Result<ChangeSet> {
        let mut args = vec!["diff", "--name-status", "-z", "-M", "-C", old];
        if let Some(new) = new {
            args.push(new);
        }
        Ok(parse_name_status(&self.raw(&args)?))
    }
}

/// Run git in `cwd` with the hook environment stripped. Non-zero exit is an
/// error carrying git's own stderr; the caller decides whether that is fatal
/// (it never is — the hook exits 0 regardless).
fn run(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        // See the module header: git's exported hook environment describes the
        // operation in progress, not the repository we want to interrogate.
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_PREFIX")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// The two endpoints of a transition, plus why they were chosen.
///
/// `new == None` means "the worktree" — only `git merge --squash` produces it,
/// and only because that is the one transition where HEAD does not move.
#[derive(Debug, PartialEq, Eq)]
pub struct Endpoints {
    pub old: String,
    pub new: Option<String>,
    pub why: &'static str,
}

/// The outcome of resolving a hook invocation: either a diff to run, or a
/// deliberate no-op with a reason to print.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolved {
    Diff(Endpoints),
    /// Nothing to do, by design (a file-level checkout, an empty rewrite list).
    NoOp(&'static str),
}

impl Endpoints {
    /// Resolve `event`'s two tree-ish endpoints from git's own hook arguments
    /// (ADR-0032b §1).
    ///
    /// - **post-commit** — `HEAD^1..HEAD`; a root commit has no parent, so the
    ///   empty tree stands in and the whole tree arrives as `added`. A merge
    ///   commit created by `git commit` after resolving conflicts fires
    ///   *post-commit*, not post-merge, so the first parent is used explicitly
    ///   rather than relying on `git diff-tree`'s "merges show nothing" default.
    /// - **post-checkout** — `<prev> <new> <branch-flag>`; gated on the flag
    ///   (ADR-0032b OQ3: a file-level checkout must not submit a whole-tree
    ///   diff). A fresh clone reports the null oid as `prev` → empty tree.
    /// - **post-merge** — git passes **only** the squash flag, not the
    ///   endpoints, so the old side is `ORIG_HEAD` (which `git merge` sets),
    ///   falling back to `HEAD^1` for a merge commit. `--squash` leaves HEAD
    ///   alone, so there the new side is the worktree.
    /// - **post-rewrite** — the *last* pair on stdin is the rewritten tip, so
    ///   its old side is the pre-rewrite tip and the new side is `HEAD`. For
    ///   `commit --amend` there is exactly one pair, which makes this exactly
    ///   the amend delta.
    pub fn for_event(
        git: &Git,
        event: HookEvent,
        args: &[String],
        stdin: &str,
    ) -> Result<Resolved> {
        let empty = || git.empty_tree();
        match event {
            HookEvent::PostCommit => {
                let old = match git.resolve("HEAD^1") {
                    Some(parent) => parent,
                    None => empty()?,
                };
                Ok(Resolved::Diff(Endpoints {
                    old,
                    new: Some("HEAD".to_string()),
                    why: "HEAD^1..HEAD",
                }))
            }
            HookEvent::PostCheckout => {
                // Git always passes all three. Fewer means a malformed
                // invocation — a hand-run hook, or a generated script that
                // dropped `"$@"` — and the gate must fail **closed**: inferring
                // "branch switch" from a missing flag would submit a whole-tree
                // diff on exactly the inputs we understand least, which is the
                // opposite of what OQ3 asks for.
                let (Some(prev), Some(new), Some(branch)) = (
                    args.first().map(String::as_str),
                    args.get(1).map(String::as_str),
                    args.get(2).map(String::as_str),
                ) else {
                    return Ok(Resolved::NoOp(
                        "post-checkout without git's <prev> <new> <branch-flag> arguments",
                    ));
                };
                if branch != "1" {
                    return Ok(Resolved::NoOp(
                        "file-level checkout (branch flag 0) — not a tree transition",
                    ));
                }
                if prev == new {
                    return Ok(Resolved::NoOp("checkout did not move HEAD"));
                }
                let old = if prev.is_empty() || is_null_oid(prev) {
                    empty()?
                } else {
                    prev.to_string()
                };
                Ok(Resolved::Diff(Endpoints {
                    old,
                    new: Some(new.to_string()),
                    why: "checkout endpoints",
                }))
            }
            HookEvent::PostMerge => {
                let squash = args.first().map(String::as_str) == Some("1");
                if squash {
                    // `git merge --squash` stages the merge without committing:
                    // HEAD is unchanged, so the only honest "new" side is the
                    // worktree itself.
                    return Ok(Resolved::Diff(Endpoints {
                        old: "HEAD".to_string(),
                        new: None,
                        why: "squash merge: HEAD..worktree",
                    }));
                }
                let (old, why) = match git.resolve("ORIG_HEAD") {
                    Some(orig) => (orig, "ORIG_HEAD..HEAD"),
                    None => match git.resolve("HEAD^1") {
                        Some(parent) => (parent, "HEAD^1..HEAD (no ORIG_HEAD)"),
                        None => (empty()?, "empty tree..HEAD (no ORIG_HEAD, no parent)"),
                    },
                };
                Ok(Resolved::Diff(Endpoints {
                    old,
                    new: Some("HEAD".to_string()),
                    why,
                }))
            }
            HookEvent::PostRewrite => {
                let Some(last) = rewrite_pairs(stdin).last().cloned() else {
                    return Ok(Resolved::NoOp("post-rewrite carried no rewrite pairs"));
                };
                Ok(Resolved::Diff(Endpoints {
                    old: last.0,
                    new: Some("HEAD".to_string()),
                    why: "pre-rewrite tip..HEAD",
                }))
            }
        }
    }
}

/// `post-rewrite`'s stdin: one `<old-sha> <new-sha>` pair per line, oldest
/// first, with optional trailing fields git reserves for future use.
///
/// Kept as a list even though only the last pair is used: the last pair *being*
/// the rewritten tip is the property this parser has to preserve, and a parser
/// that returned only one value would hide a malformed line instead of skipping
/// it.
pub fn rewrite_pairs(stdin: &str) -> Vec<(String, String)> {
    stdin
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let old = f.next()?;
            let new = f.next()?;
            (!is_null_oid(old)).then(|| (old.to_string(), new.to_string()))
        })
        .collect()
}

/// The all-zero object id git uses for "nothing was there" (a fresh clone's
/// `post-checkout`, a dropped commit's rewrite pair).
fn is_null_oid(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b == b'0')
}

/// Parse `git diff --name-status -z -M -C` into a [`ChangeSet`].
///
/// The mapping is the one `filigrio-resolve`'s `git_convergence` harness
/// already replays real history with, so the shape the hook produces at a
/// commit boundary is the shape the convergence gate proves converges:
/// `A`→added, `M`/`T`→modified, `D`→removed, `R###`→removed(old) + added(new),
/// `C###`→added(new). `U` (unmerged) and anything unrecognised degrade to
/// `modified` — a conservative re-index, never a silent drop.
///
/// `-z` because real repositories have paths with spaces and quotes, and the
/// quoted form is ambiguous to parse back.
pub fn parse_name_status(raw: &[u8]) -> ChangeSet {
    let mut fields = raw
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned());

    let mut cs = ChangeSet::default();
    while let Some(status) = fields.next() {
        let Some(code) = status.as_bytes().first().copied() else {
            continue;
        };
        match code {
            b'R' | b'C' => {
                // A rename/copy record carries two paths. A truncated record
                // (git killed mid-write) is dropped rather than mis-paired.
                let (Some(old), Some(new)) = (fields.next(), fields.next()) else {
                    break;
                };
                if code == b'R' {
                    cs.removed.push(old);
                }
                cs.added.push(new);
            }
            _ => {
                let Some(path) = fields.next() else { break };
                match code {
                    b'A' => cs.added.push(path),
                    b'D' => cs.removed.push(path),
                    _ => cs.modified.push(path),
                }
            }
        }
    }
    normalize(&mut cs);
    cs
}

fn normalize(cs: &mut ChangeSet) {
    for v in [&mut cs.added, &mut cs.modified, &mut cs.removed] {
        v.sort();
        v.dedup();
    }
}

/// Our own output, which some repositories commit. Submitting it would ask the
/// daemon to re-index the artifact its own apply just wrote — the oracle's
/// rebuild loop, avoided here for the same reason `filigrio-ingest` prunes
/// these directories from the walk.
fn is_own_output(path: &str) -> bool {
    path.starts_with(".filigrio-out/") || path.starts_with(".filigrio/")
}

/// Drop our own output from a changeset. Returns the number of paths dropped so
/// the caller can say so rather than silently shrinking the submission.
pub fn strip_own_output(cs: &mut ChangeSet) -> usize {
    let before = cs.len();
    for v in [&mut cs.added, &mut cs.modified, &mut cs.removed] {
        v.retain(|p| !is_own_output(p));
    }
    before - cs.len()
}

/// The changeset for one hook invocation, or `Err`/`NoOp` with a reason.
pub fn changeset_for(
    git: &Git,
    event: HookEvent,
    args: &[String],
    stdin: &str,
) -> Result<(ChangeSet, &'static str)> {
    match Endpoints::for_event(git, event, args, stdin)? {
        Resolved::NoOp(why) => Ok((ChangeSet::default(), why)),
        Resolved::Diff(ep) => {
            let mut cs = git.diff(&ep.old, ep.new.as_deref())?;
            strip_own_output(&mut cs);
            Ok((cs, ep.why))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_round_trip_and_reject_strangers() {
        for name in ["post-commit", "post-checkout", "post-merge", "post-rewrite"] {
            let e = HookEvent::parse(name).expect("known event");
            assert_eq!(e.as_str(), name);
        }
        assert!(HookEvent::parse("pre-commit").is_none());
        assert!(HookEvent::parse("").is_none());
        assert!(HookEvent::parse("post-commit ").is_none());
    }

    /// Only `post-rewrite` is fed on stdin. Reading it for the others would
    /// block on whatever pipe the invoking shell happened to leave open.
    #[test]
    fn only_post_rewrite_reads_stdin() {
        assert!(HookEvent::PostRewrite.reads_stdin());
        for e in [
            HookEvent::PostCommit,
            HookEvent::PostCheckout,
            HookEvent::PostMerge,
        ] {
            assert!(!e.reads_stdin());
        }
    }

    #[test]
    fn name_status_maps_every_status_code() {
        let raw = b"A\0new.rs\0M\0edit.rs\0D\0gone.rs\0T\0type.rs\0U\0conflict.rs\0".to_vec();
        let cs = parse_name_status(&raw);
        assert_eq!(cs.added, vec!["new.rs"]);
        assert_eq!(cs.modified, vec!["conflict.rs", "edit.rs", "type.rs"]);
        assert_eq!(cs.removed, vec!["gone.rs"]);
    }

    /// A rename is a remove + an add; a **copy** leaves its source in place, so
    /// it is an add only. Getting this backwards would delete a live file from
    /// the graph on every `git mv`-adjacent copy detection.
    #[test]
    fn renames_split_and_copies_do_not_remove_their_source() {
        let cs = parse_name_status(b"R100\0old.rs\0new.rs\0C75\0src.rs\0dst.rs\0");
        assert_eq!(cs.added, vec!["dst.rs", "new.rs"]);
        assert_eq!(cs.removed, vec!["old.rs"]);
        assert!(cs.modified.is_empty());
    }

    /// Paths with spaces are exactly why the diff is `-z`; a truncated trailing
    /// record must be dropped, never mis-paired with the next status field.
    #[test]
    fn spaces_survive_and_a_truncated_record_is_dropped() {
        let cs = parse_name_status(b"M\0a file.rs\0R100\0only-one-side.rs\0");
        assert_eq!(cs.modified, vec!["a file.rs"]);
        assert!(cs.added.is_empty() && cs.removed.is_empty());
    }

    #[test]
    fn rewrite_pairs_take_the_first_two_fields_and_skip_junk() {
        let pairs = rewrite_pairs("aaa bbb\nccc ddd extra-field\nnot-a-pair\n\n");
        assert_eq!(
            pairs,
            vec![
                ("aaa".to_string(), "bbb".to_string()),
                ("ccc".to_string(), "ddd".to_string())
            ]
        );
    }

    #[test]
    fn null_oids_are_recognised_and_ordinary_ids_are_not() {
        assert!(is_null_oid("0000000000000000000000000000000000000000"));
        assert!(!is_null_oid("4b825dc642cb6eb9a060e54bf8d69288fbee4904"));
        assert!(!is_null_oid(""));
    }

    /// Our own output is pruned client-side for the same reason the ingest walk
    /// prunes it: a repository that commits `.filigrio-out/` would otherwise ask
    /// the daemon to re-index the file its own apply just wrote.
    #[test]
    fn own_output_is_stripped_and_counted() {
        let mut cs = ChangeSet {
            added: vec![".filigrio-out/graph.json".into(), "src/a.rs".into()],
            modified: vec![".filigrio/state.json".into()],
            removed: vec![],
        };
        assert_eq!(strip_own_output(&mut cs), 2);
        assert_eq!(cs.added, vec!["src/a.rs"]);
        assert!(cs.modified.is_empty());
    }
}
