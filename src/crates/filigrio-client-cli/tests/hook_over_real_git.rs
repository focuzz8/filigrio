//! ADR-0032b §1 against a **real repository**, driven by real git operations.
//!
//! The unit tests in `src/hook/git.rs` pin the parser; this file pins the
//! *contract with git*, which is the part that cannot be reasoned out from the
//! documentation. Each scenario installs recording hooks into a scratch repo,
//! performs the actual git operation, and feeds the arguments and stdin **git
//! itself passed** into `changeset_for`. So the test cannot drift from git's
//! calling convention by agreeing with an assumption — a wrong belief about
//! (say) what `post-merge` receives fails here rather than in the field.
//!
//! `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` are neutered for every invocation:
//! a developer's `commit.gpgsign`, `core.hooksPath` or `init.defaultBranch`
//! must not decide whether this suite passes.

use filigrio_client_cli::hook::git::{changeset_for, Git, HookEvent};
use filigrio_protocol::ChangeSet;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

/// One recorded hook invocation: what git passed, and what it wrote to stdin.
#[derive(Debug, Clone)]
struct Record {
    args: Vec<String>,
    stdin: String,
}

struct Repo {
    _dir: TempDir,
    root: PathBuf,
    log: PathBuf,
    /// `XDG_CACHE_HOME` for hook subprocesses — deliberately a **sibling** of
    /// the worktree, never inside it: the real spool lives in the user's cache
    /// dir, and putting it under the repo would make the harness itself the
    /// thing that dirties the tree.
    cache: PathBuf,
}

impl Repo {
    fn new() -> Repo {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("work");
        let log = dir.path().join("hooklog");
        std::fs::create_dir_all(&root).expect("mkdir work");
        std::fs::create_dir_all(&log).expect("mkdir hooklog");

        let cache = dir.path().join("cache");
        let repo = Repo {
            _dir: dir,
            root,
            log,
            cache,
        };
        repo.git(&["init", "-q", "-b", "main", "."]);
        repo.git(&["config", "user.email", "hook@test"]);
        repo.git(&["config", "user.name", "Hook Test"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.install_recording_hooks();
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            // Hermetic: the developer's own git config must not be an input.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Hooks that record `"$@"` (and, for `post-rewrite`, stdin) into
    /// `hooklog/<event>.log` as `<<<`-delimited records.
    ///
    /// Only `post-rewrite` reads stdin — `cat` in a `post-commit` hook blocks
    /// on whatever git left attached, which is the same reason the real verb
    /// gates on [`HookEvent::reads_stdin`].
    fn install_recording_hooks(&self) {
        let hooks = self.root.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks).expect("mkdir hooks");
        for event in ["post-commit", "post-checkout", "post-merge", "post-rewrite"] {
            let read_stdin = if event == "post-rewrite" { "cat" } else { ":" };
            let script = format!(
                "#!/bin/sh\n\
                 LOG={log}/{event}.log\n\
                 {{ echo '<<<'; echo \"ARGS $*\"; {read_stdin}; echo '>>>'; }} >> \"$LOG\"\n\
                 exit 0\n",
                log = self.log.display(),
            );
            let path = hooks.join(event);
            std::fs::write(&path, script).expect("write hook");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod hook");
            }
        }
    }

    fn write(&self, rel: &str, body: &str) {
        let p = self.root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("mkdir -p");
        }
        std::fs::write(p, body).expect("write file");
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    /// Every invocation git made of `event`, oldest first.
    fn records(&self, event: &str) -> Vec<Record> {
        let path = self.log.join(format!("{event}.log"));
        let Ok(body) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut current: Option<(Vec<String>, Vec<String>)> = None;
        for line in body.lines() {
            match line {
                "<<<" => current = Some((Vec::new(), Vec::new())),
                ">>>" => {
                    if let Some((args, stdin)) = current.take() {
                        let mut stdin = stdin.join("\n");
                        if !stdin.is_empty() {
                            stdin.push('\n');
                        }
                        out.push(Record { args, stdin });
                    }
                }
                _ => {
                    if let Some((args, stdin)) = current.as_mut() {
                        match line.strip_prefix("ARGS ") {
                            Some(rest) if args.is_empty() && stdin.is_empty() => {
                                *args = rest.split_whitespace().map(str::to_string).collect();
                            }
                            _ => stdin.push(line.to_string()),
                        }
                    }
                }
            }
        }
        out
    }

    /// The most recent invocation of `event`.
    fn last(&self, event: &str) -> Record {
        self.records(event)
            .pop()
            .unwrap_or_else(|| panic!("git never fired {event}"))
    }

    /// The changeset the hook verb would produce for git's most recent
    /// invocation of `event`.
    fn changeset(&self, event: &str) -> ChangeSet {
        let rec = self.last(event);
        self.changeset_from(event, &rec)
    }

    fn changeset_from(&self, event: &str, rec: &Record) -> ChangeSet {
        let git = Git::discover(&self.root).expect("discover repo");
        let ev = HookEvent::parse(event).expect("known event");
        changeset_for(&git, ev, &rec.args, &rec.stdin)
            .unwrap_or_else(|e| panic!("changeset_for({event}): {e:#}"))
            .0
    }
}

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// The **root commit** has no `HEAD~1`. Falling back to the repository's empty
/// tree makes the first commit arrive as an all-added changeset — which is
/// exactly the cold-build shape (`ChangeSet::all_added`, ADR-0012), so the very
/// first commit in a repo indexes it rather than producing nothing.
#[test]
fn a_root_commit_arrives_as_the_whole_tree_added() {
    let repo = Repo::new();
    repo.write("src/a.rs", "fn a() {}\n");
    repo.write("src/b.rs", "fn b() {}\n");
    repo.commit("root");

    let cs = repo.changeset("post-commit");
    assert_eq!(cs.added, v(&["src/a.rs", "src/b.rs"]));
    assert!(cs.modified.is_empty() && cs.removed.is_empty());
}

/// An ordinary commit is the `HEAD^1..HEAD` diff, with each of the three
/// buckets populated from git's own status letters.
#[test]
fn an_ordinary_commit_splits_into_added_modified_and_removed() {
    let repo = Repo::new();
    repo.write("keep.rs", "1\n");
    repo.write("gone.rs", "2\n");
    repo.commit("root");

    repo.write("keep.rs", "1 changed\n");
    repo.write("new.rs", "3\n");
    std::fs::remove_file(repo.root.join("gone.rs")).expect("rm");
    repo.commit("second");

    let cs = repo.changeset("post-commit");
    assert_eq!(cs.added, v(&["new.rs"]));
    assert_eq!(cs.modified, v(&["keep.rs"]));
    assert_eq!(cs.removed, v(&["gone.rs"]));
}

/// `-M -C` is what makes a move arrive as a move. A rename must be
/// **removed(old) + added(new)** — the same mapping `filigrio-resolve`'s
/// `git_convergence` harness replays real history with, so what the hook emits
/// at a commit boundary is what the convergence gate has proven converges.
#[test]
fn a_rename_becomes_a_removal_plus_an_addition() {
    let repo = Repo::new();
    repo.write("src/old.rs", "fn stable() {}\n");
    repo.commit("root");

    repo.git(&["mv", "src/old.rs", "src/new.rs"]);
    repo.commit("move");

    let cs = repo.changeset("post-commit");
    assert_eq!(cs.removed, v(&["src/old.rs"]));
    assert_eq!(cs.added, v(&["src/new.rs"]));
    assert!(cs.modified.is_empty());
}

/// A **copy** leaves its source in place, so it must never produce a removal —
/// getting this backwards deletes a live file from the graph.
#[test]
fn a_copy_adds_the_destination_and_keeps_the_source() {
    let repo = Repo::new();
    // Copy detection needs enough content to score a similarity.
    let body: String = (0..40).map(|n| format!("fn f{n}() {{ }}\n")).collect();
    repo.write("src/orig.rs", &body);
    repo.commit("root");

    repo.write("src/clone.rs", &body);
    repo.commit("copy");

    let cs = repo.changeset("post-commit");
    assert_eq!(cs.added, v(&["src/clone.rs"]));
    assert!(
        cs.removed.is_empty(),
        "a copy must not remove its source: {cs:?}"
    );
}

/// `git commit --amend` fires **both** hooks, and each is correct in its own
/// way: `post-commit` re-submits the amended commit's full `HEAD^1..HEAD`
/// (a safe superset — re-indexing an unchanged file is idempotent and the
/// daemon's manifest gate dedups it), while `post-rewrite` — whose single
/// stdin pair is `<pre-amend> <post-amend>` — yields the **exact amend delta**.
#[test]
fn an_amend_yields_the_exact_delta_on_post_rewrite_and_a_superset_on_post_commit() {
    let repo = Repo::new();
    repo.write("base.rs", "0\n");
    repo.commit("root");

    repo.write("first.rs", "1\n");
    repo.commit("second");

    // Amend: add a second file to the same commit.
    repo.write("amended.rs", "2\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "--amend", "--no-edit"]);

    let rewrite = repo.last("post-rewrite");
    assert_eq!(
        rewrite.args,
        v(&["amend"]),
        "git names the rewrite command in argv"
    );
    assert_eq!(
        rewrite.stdin.lines().count(),
        1,
        "an amend rewrites exactly one commit: {:?}",
        rewrite.stdin
    );

    let exact = repo.changeset_from("post-rewrite", &rewrite);
    assert_eq!(exact.added, v(&["amended.rs"]), "the amend delta only");
    assert!(exact.modified.is_empty() && exact.removed.is_empty());

    let superset = repo.changeset("post-commit");
    assert_eq!(
        superset.added,
        v(&["amended.rs", "first.rs"]),
        "post-commit re-submits the whole amended commit"
    );
}

/// `post-checkout` gets `<prev> <new> <branch-flag>`. A **branch** switch is a
/// tree transition; a file-level checkout (flag `0`) is not, and must not
/// submit a whole-tree diff — ADR-0032b OQ3, closed here.
#[test]
fn a_branch_switch_diffs_the_two_tips_and_a_file_checkout_does_nothing() {
    let repo = Repo::new();
    repo.write("main-only.rs", "m\n");
    repo.commit("root");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.write("feature-only.rs", "f\n");
    repo.commit("feature work");
    repo.git(&["checkout", "-q", "main"]);

    let rec = repo.last("post-checkout");
    assert_eq!(rec.args.len(), 3, "prev, new, branch-flag: {rec:?}");
    assert_eq!(rec.args[2], "1", "a branch switch sets the flag");

    let cs = repo.changeset_from("post-checkout", &rec);
    assert_eq!(
        cs.removed,
        v(&["feature-only.rs"]),
        "switching away from feature removes its file"
    );
    assert!(cs.added.is_empty() && cs.modified.is_empty());

    // The same endpoints with the flag git uses for a *file* checkout.
    let file_checkout = Record {
        args: vec![rec.args[0].clone(), rec.args[1].clone(), "0".to_string()],
        stdin: String::new(),
    };
    let cs = repo.changeset_from("post-checkout", &file_checkout);
    assert!(
        cs.is_empty(),
        "a file-level checkout must not submit a tree diff: {cs:?}"
    );
}

/// The OQ3 gate must fail **closed**. Git always passes all three arguments, so
/// fewer means a malformed invocation (a hand-run hook, a generated script that
/// dropped `"$@"`) — and inferring "branch switch" from a *missing* flag would
/// submit a whole-tree diff on exactly the input we understand least.
#[test]
fn a_post_checkout_missing_gits_arguments_is_a_no_op_not_an_assumed_branch_switch() {
    let repo = Repo::new();
    repo.write("main-only.rs", "m\n");
    repo.commit("root");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.write("feature-only.rs", "f\n");
    repo.commit("feature work");
    repo.git(&["checkout", "-q", "main"]);

    let full = repo.last("post-checkout");
    assert!(
        !repo.changeset_from("post-checkout", &full).is_empty(),
        "the well-formed call is the control: it must produce a diff"
    );

    // The same real endpoints, with the flag missing — and with everything
    // missing. Both must decline rather than guess.
    for truncated in [&full.args[..2], &full.args[..1], &full.args[..0]] {
        let rec = Record {
            args: truncated.to_vec(),
            stdin: String::new(),
        };
        let cs = repo.changeset_from("post-checkout", &rec);
        assert!(
            cs.is_empty(),
            "post-checkout with {} argument(s) must decline, got {cs:?}",
            truncated.len()
        );
    }
}

/// **Git passes `post-merge` only the squash flag**, not the endpoints — the
/// old side has to come from `ORIG_HEAD`, which `git merge` sets. This test is
/// the reason that is a fact rather than a belief: the recorded argv is
/// asserted, and the changeset is the merge's real payload.
#[test]
fn a_merge_diffs_orig_head_to_head_because_git_passes_no_endpoints() {
    let repo = Repo::new();
    repo.write("base.rs", "0\n");
    repo.commit("root");

    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("from-side.rs", "s\n");
    repo.commit("side work");

    repo.git(&["checkout", "-q", "main"]);
    repo.write("from-main.rs", "m\n");
    repo.commit("main work");

    repo.git(&["merge", "-q", "--no-ff", "-m", "merge side", "side"]);

    let rec = repo.last("post-merge");
    assert_eq!(
        rec.args,
        v(&["0"]),
        "post-merge receives ONLY the squash flag — not the endpoints"
    );

    let cs = repo.changeset_from("post-merge", &rec);
    assert_eq!(
        cs.added,
        v(&["from-side.rs"]),
        "the merge brought in the side branch's file"
    );
    assert!(cs.removed.is_empty());
}

/// A rebase emits one rewrite pair per replayed commit, oldest first, so the
/// **last** pair's old side is the pre-rebase tip. Diffing that to `HEAD` is
/// the exact worktree transition the rebase caused.
#[test]
fn a_rebase_diffs_the_pre_rewrite_tip_to_head() {
    let repo = Repo::new();
    repo.write("base.rs", "0\n");
    repo.commit("root");

    repo.git(&["checkout", "-q", "-b", "topic"]);
    repo.write("topic-a.rs", "a\n");
    repo.commit("topic a");
    repo.write("topic-b.rs", "b\n");
    repo.commit("topic b");

    repo.git(&["checkout", "-q", "main"]);
    repo.write("main-new.rs", "n\n");
    repo.commit("main moves on");

    repo.git(&["checkout", "-q", "topic"]);
    repo.git(&["rebase", "-q", "main"]);

    let rec = repo.last("post-rewrite");
    assert_eq!(rec.args, v(&["rebase"]));
    assert_eq!(
        rec.stdin.lines().count(),
        2,
        "two topic commits were replayed: {:?}",
        rec.stdin
    );

    let cs = repo.changeset_from("post-rewrite", &rec);
    assert_eq!(
        cs.added,
        v(&["main-new.rs"]),
        "rebasing onto main brings main's new file into the worktree"
    );
    assert!(cs.removed.is_empty(), "nothing left the tree: {cs:?}");
}

/// A repository that commits `.filigrio-out/` must not ask the daemon to
/// re-index the artifact its own apply wrote — the oracle's rebuild loop. When
/// the graph output is the *only* thing that changed, the hook has nothing to
/// submit at all.
#[test]
fn committed_graph_output_never_reaches_the_daemon() {
    let repo = Repo::new();
    repo.write("src/a.rs", "fn a() {}\n");
    repo.commit("root");

    repo.write(".filigrio-out/graph.json", "{}\n");
    repo.write("src/b.rs", "fn b() {}\n");
    repo.commit("code plus output");
    let cs = repo.changeset("post-commit");
    assert_eq!(cs.added, v(&["src/b.rs"]), "our own output is stripped");

    repo.write(".filigrio-out/graph.json", "{\"n\":1}\n");
    repo.commit("output only");
    let cs = repo.changeset("post-commit");
    assert!(
        cs.is_empty(),
        "an output-only commit must submit nothing: {cs:?}"
    );
}

/// The engine owns which paths are indexable (the daemon's scope+dedup gate),
/// so the hook deliberately does **not** filter by extension. A client that
/// duplicated that policy would be a second place for it to drift.
#[test]
fn paths_outside_any_extension_filter_are_still_reported() {
    let repo = Repo::new();
    repo.write("src/a.rs", "fn a() {}\n");
    repo.commit("root");

    repo.write("README.md", "# docs\n");
    repo.write("assets/logo.svg", "<svg/>\n");
    repo.write("Cargo.toml", "[package]\n");
    repo.commit("non-code");

    let cs = repo.changeset("post-commit");
    assert_eq!(
        cs.added,
        v(&["Cargo.toml", "README.md", "assets/logo.svg"]),
        "extension policy belongs to the daemon's scope gate, not the hook"
    );
}

/// Paths with spaces are why the diff is `-z`. A quoted-path parser would emit
/// `"a file.rs"` (with the quotes), which matches nothing in the manifest.
#[test]
fn paths_with_spaces_survive_the_diff() {
    let repo = Repo::new();
    repo.write("plain.rs", "0\n");
    repo.commit("root");

    repo.write("a directory/a file.rs", "fn spaced() {}\n");
    repo.commit("spaces");

    let cs = repo.changeset("post-commit");
    assert_eq!(cs.added, v(&["a directory/a file.rs"]));
}

/// The changeset is computed against the **worktree root**, which is the
/// project path the `Submit` is addressed to, and `git diff` reports paths
/// relative to it. The two agreeing is what makes the daemon able to find the
/// files at all.
#[test]
fn the_project_is_the_worktree_root_and_paths_are_relative_to_it() {
    let repo = Repo::new();
    repo.write("nested/deep/file.rs", "0\n");
    repo.commit("root");

    let git =
        Git::discover(&repo.root.join("nested").join("deep")).expect("discover from a subdir");
    assert_eq!(
        std::fs::canonicalize(git.root()).expect("canonicalize"),
        std::fs::canonicalize(&repo.root).expect("canonicalize"),
        "discovery from a subdirectory must still find the worktree root"
    );

    let cs = repo.changeset("post-commit");
    assert_eq!(cs.added, v(&["nested/deep/file.rs"]));
}

// ---------------------------------------------------------------------------
// Why there is no "operation in progress" guard, and no linked-worktree guard.
//
// The oracle's hook skips whenever `rebase-merge`/`MERGE_HEAD`/`CHERRY_PICK_HEAD`
// exists, and skips inside a linked worktree. Both guards exist because *its*
// hook rebuilds into a shared `graphify-out/` in the worktree — which leaves
// unstaged changes that block `git rebase --continue`, and which belongs to the
// primary checkout. Ours writes nothing to the worktree and addresses the
// daemon by path, so neither reason transfers. The three tests below are what
// makes that a measurement rather than an assumption: if a guard were needed,
// one of them would fail.
// ---------------------------------------------------------------------------

/// A conflicted merge fires **no** hook while it is unresolved; the
/// `post-commit` that follows `git commit` describes the merge exactly. A
/// `MERGE_HEAD` guard would suppress the one submission that carries the merge.
#[test]
fn a_conflicted_merge_submits_its_payload_when_the_resolution_is_committed() {
    let repo = Repo::new();
    repo.write("base.rs", "0\n");
    repo.write("shared.rs", "v1\n");
    repo.commit("root");

    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("from-side.rs", "s\n");
    repo.write("shared.rs", "v-side\n");
    repo.commit("side work");

    repo.git(&["checkout", "-q", "main"]);
    repo.write("shared.rs", "v-main\n");
    repo.commit("main work");

    let before = repo.records("post-commit").len();
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(&repo.root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("merge");
    assert!(!merge.status.success(), "this merge is meant to conflict");
    assert!(repo.root.join(".git").join("MERGE_HEAD").exists());
    assert_eq!(
        repo.records("post-commit").len(),
        before,
        "an unresolved conflicted merge fires no post-commit — there is nothing to guard against"
    );

    repo.write("shared.rs", "resolved\n");
    repo.commit("resolve the merge");

    let cs = repo.changeset("post-commit");
    assert_eq!(
        cs.added,
        v(&["from-side.rs"]),
        "the merge commit's first-parent diff is the side branch's payload"
    );
    assert_eq!(cs.modified, v(&["shared.rs"]));
}

/// A conflicted rebase fires `post-checkout` for each real move of the
/// worktree, and `post-rewrite` once it completes. Every one of those describes
/// a transition that genuinely happened — a `rebase-merge` guard would drop
/// them all, leaving the index stale at exactly the moment the tree moved most.
/// The degenerate `prev == new` checkout a rebase also emits is already a
/// no-op, so the noise costs nothing.
#[test]
fn a_rebase_in_progress_only_ever_describes_real_tree_transitions() {
    let repo = Repo::new();
    repo.write("base.rs", "0\n");
    repo.write("shared.rs", "v1\n");
    repo.commit("root");

    repo.git(&["checkout", "-q", "-b", "topic"]);
    repo.write("topic.rs", "t\n");
    repo.write("shared.rs", "v-topic\n");
    repo.commit("topic work");

    repo.git(&["checkout", "-q", "main"]);
    repo.write("main-new.rs", "n\n");
    repo.write("shared.rs", "v-main\n");
    repo.commit("main moves on");

    repo.git(&["checkout", "-q", "topic"]);
    let rebase = Command::new("git")
        .args(["rebase", "main"])
        .current_dir(&repo.root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rebase");
    assert!(!rebase.status.success(), "this rebase is meant to conflict");
    assert!(repo.root.join(".git").join("rebase-merge").exists());

    // Every post-checkout git fired mid-rebase must either decline (HEAD did
    // not move) or describe a diff both of whose endpoints are real commits.
    let mid = repo.records("post-checkout");
    assert!(!mid.is_empty(), "a rebase moves the worktree");
    for rec in &mid {
        assert_eq!(rec.args.len(), 3, "git always passes all three: {rec:?}");
        let cs = repo.changeset_from("post-checkout", rec);
        if rec.args[0] == rec.args[1] {
            assert!(cs.is_empty(), "a checkout that did not move HEAD: {cs:?}");
        }
    }

    repo.write("shared.rs", "resolved\n");
    repo.git(&["add", "shared.rs"]);
    let cont = Command::new("git")
        .args(["rebase", "--continue"])
        .current_dir(&repo.root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_EDITOR", "true")
        .output()
        .expect("rebase --continue");
    assert!(
        cont.status.success(),
        "the rebase must complete:\n{}",
        String::from_utf8_lossy(&cont.stderr)
    );

    let cs = repo.changeset("post-rewrite");
    assert_eq!(
        cs.added,
        v(&["main-new.rs"]),
        "the completed rebase brought main's new file into the worktree"
    );
}

/// The oracle's stated reason for its in-progress guard is that its rebuild
/// leaves unstaged changes that block `git rebase --continue`. Ours cannot: the
/// hook computes a diff and writes a socket frame. This asserts the worktree is
/// byte-for-byte unchanged across a hook run — the direct refutation.
#[test]
fn a_hook_run_leaves_the_worktree_and_index_untouched() {
    let repo = Repo::new();
    repo.write("a.rs", "0\n");
    repo.commit("root");
    repo.write("b.rs", "1\n");
    repo.commit("second");

    let status_before = repo.git(&["status", "--porcelain=v1", "--untracked-files=all"]);
    let tree_before = repo.git(&["write-tree"]);

    let out = run_hook_binary(&repo, &["post-commit"], &[]);
    assert!(out.status.success());

    assert_eq!(
        repo.git(&["status", "--porcelain=v1", "--untracked-files=all"]),
        status_before,
        "a hook run must not dirty the worktree — this is what would block `rebase --continue`"
    );
    assert_eq!(
        repo.git(&["write-tree"]),
        tree_before,
        "the index is untouched"
    );
}

/// In a **linked worktree** (`git worktree add`), `--show-toplevel` resolves to
/// that worktree's own root, so the changeset is addressed to it and its paths
/// are relative to it. That is the correct answer for us — a linked worktree is
/// either its own registered project or an unregistered path the daemon
/// declines — so the oracle's skip-inside-a-worktree guard would only suppress
/// legitimate indexing.
#[test]
fn a_linked_worktree_is_addressed_as_itself_not_as_the_primary_checkout() {
    let repo = Repo::new();
    repo.write("primary.rs", "p\n");
    repo.commit("root");

    let linked = repo.root.parent().expect("parent").join("linked");
    repo.git(&[
        "worktree",
        "add",
        "-q",
        linked.to_str().expect("utf-8 path"),
        "-b",
        "wt",
    ]);
    std::fs::write(linked.join("only-here.rs"), "w\n").expect("write");
    for args in [
        vec!["add", "-A"],
        vec!["commit", "-q", "-m", "in the linked worktree"],
    ] {
        let out = Command::new("git")
            .args(&args)
            .current_dir(&linked)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git in the linked worktree");
        assert!(out.status.success(), "{args:?}");
    }

    let git = Git::discover(&linked).expect("discover the linked worktree");
    assert_eq!(
        std::fs::canonicalize(git.root()).expect("canonicalize"),
        std::fs::canonicalize(&linked).expect("canonicalize"),
        "the linked worktree is its own root, not the primary checkout"
    );

    let cs = changeset_for(&git, HookEvent::PostCommit, &[], "")
        .expect("changeset in the linked worktree")
        .0;
    assert_eq!(cs.added, v(&["only-here.rs"]));
}

/// Git exports `GIT_DIR`/`GIT_INDEX_FILE`/`GIT_PREFIX` to its hooks, and
/// `GIT_INDEX_FILE` points at the **transient** index an operation is building.
/// Inheriting either makes `git diff` answer a different question than the one
/// asked. The module strips them; this runs the real binary with both poisoned
/// and asserts the answer is unchanged.
#[test]
fn gits_exported_hook_environment_does_not_leak_into_our_plumbing() {
    let repo = Repo::new();
    repo.write("a.rs", "0\n");
    repo.commit("root");
    repo.write("b.rs", "1\n");
    repo.commit("second");

    let clean = repo.changeset("post-commit");
    assert_eq!(clean.added, v(&["b.rs"]));

    let out = run_hook_binary(
        &repo,
        &["post-commit"],
        &[
            ("GIT_DIR", repo.root.join("no-such-git-dir")),
            ("GIT_INDEX_FILE", repo.root.join("no-such-index")),
            ("GIT_PREFIX", PathBuf::from("nested/")),
        ],
    );
    assert!(out.status.success(), "a hook always exits 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("1 added, 0 modified, 0 removed"),
        "the poisoned run must still see the real diff; got:\n{stderr}"
    );
}

/// Run the real `filigrio hooks run` binary against `repo`, with the daemon socket
/// pointed at nothing (so the run exercises the spool rung) and `XDG_CACHE_HOME`
/// inside the scratch repo (so nothing lands in the developer's cache).
fn run_hook_binary(
    repo: &Repo,
    hook_args: &[&str],
    extra_env: &[(&str, PathBuf)],
) -> std::process::Output {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_filigrio"));
    cmd.arg("--socket")
        .arg(repo.root.join("no-daemon-here.sock"))
        .args(["hooks", "run"])
        .args(hook_args)
        .current_dir(&repo.root)
        .env("XDG_CACHE_HOME", &repo.cache)
        .env_remove("FILIGRIO_SKIP_HOOK");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("run the hook verb")
}
