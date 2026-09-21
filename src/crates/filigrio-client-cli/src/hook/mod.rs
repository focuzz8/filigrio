//! `filigrio hooks run <event> [git's own arguments…]` — the client-side half
//! of ADR-0032b.
//!
//! A git hook script installed by ADR-0034 calls this verb and nothing else.
//! The verb resolves the transition's two tree-ish endpoints, runs one
//! `git diff`, and hands the daemon a ready-made `ChangeSet` on the §3 `Submit`
//! path ([`git`], [`deliver`]). It is a *diff-to-changeset producer*: no
//! extraction, no engine, no graph.
//!
//! Three properties are the contract, and each is load-bearing:
//!
//! 1. **It never fails the git operation.** Every path through [`run`] returns
//!    an [`Outcome`] and the binary exits 0 — a hook that can break `git
//!    commit` is worse than no hook.
//! 2. **It never blocks it either** (§4). The socket write is fire-and-forget;
//!    see [`deliver`]'s module header for why that is the only shape that
//!    honours §4 given a synchronous `Submit`.
//! 3. **It never writes to stdout.** Hook output is interleaved into the git
//!    command's own, and stdout is where a `filigrio graph … | jq` pipeline
//!    reads from.
//!
//! `FILIGRIO_SKIP_HOOK` is the opt-out — the oracle's convention
//! (`../graphify/graphify/hooks.py`) under our own name, so skipping one tool's
//! hooks never skips the other's.

pub mod deliver;
pub mod git;
pub mod spool;
pub mod target;

pub use deliver::{Delivery, Replay};
pub use git::{Git, HookEvent};
pub use spool::{SpoolDir, SpooledJob};
use std::path::{Path, PathBuf};
pub use target::{HookTarget, Registration};

/// The environment a hook run reads, lifted out of `std::env` so both arms of
/// every branch are testable without `set_var` (which is process-global and
/// would race every other test in the binary).
pub struct HookEnv {
    /// `FILIGRIO_SKIP_HOOK`, raw.
    pub skip: Option<String>,
    /// Where undelivered jobs go.
    pub spool_dir: PathBuf,
}

impl HookEnv {
    pub fn from_env() -> HookEnv {
        HookEnv {
            skip: std::env::var("FILIGRIO_SKIP_HOOK").ok(),
            spool_dir: SpoolDir::default_dir(),
        }
    }

    /// Whether the opt-out is engaged.
    ///
    /// The oracle tests `= "1"` exactly; we accept any truthy spelling, because
    /// `FILIGRIO_SKIP_HOOK=true` silently *not* skipping is the kind of
    /// almost-worked that costs an afternoon. Unset, empty, `0`, `false` and
    /// `no` all mean "run".
    pub fn skipping(&self) -> bool {
        match self.skip.as_deref() {
            None => false,
            Some(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "no"
            ),
        }
    }
}

/// What a hook run did. Every variant is a **success**: the binary prints it to
/// stderr and exits 0.
#[derive(Debug)]
pub enum Outcome {
    /// `FILIGRIO_SKIP_HOOK` is set.
    Skipped,
    /// The event name is not one of the four ADR-0032b subscribes to.
    UnknownEvent(String),
    /// The working directory is not inside a git worktree.
    NotARepository(PathBuf),
    /// Git itself refused; the hook reports and stands down.
    GitFailed(String),
    /// Nothing to submit, and why (a file-level checkout, an empty diff).
    Nothing { why: String, replay: Replay },
    /// A changeset went down the ladder.
    Submitted {
        project: PathBuf,
        event: HookEvent,
        why: &'static str,
        added: usize,
        modified: usize,
        removed: usize,
        delivery: Delivery,
        replay: Replay,
    },
}

impl Outcome {
    /// The stderr line(s) for this outcome. Prefixed like the oracle's, so a
    /// user grepping a commit transcript finds both.
    pub fn report(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self {
            Outcome::Skipped => {}
            Outcome::UnknownEvent(name) => out.push(format!(
                "unknown hook event {name:?} — expected post-commit, post-checkout, post-merge or post-rewrite"
            )),
            Outcome::NotARepository(cwd) => {
                out.push(format!("{} is not inside a git worktree", cwd.display()))
            }
            Outcome::GitFailed(err) => out.push(format!("git said: {err}")),
            Outcome::Nothing { why, replay } => {
                push_replay(&mut out, replay);
                out.push(format!("nothing to submit ({why})"));
            }
            Outcome::Submitted {
                project,
                event,
                why,
                added,
                modified,
                removed,
                delivery,
                replay,
            } => {
                push_replay(&mut out, replay);
                out.push(format!(
                    "{} [{why}] {added} added, {modified} modified, {removed} removed in {} — {}",
                    event.as_str(),
                    project.display(),
                    delivery.describe()
                ));
            }
        }
        out
    }
}

fn push_replay(out: &mut Vec<String>, replay: &Replay) {
    if replay.delivered > 0 {
        out.push(format!(
            "replayed {} spooled changeset(s)",
            replay.delivered
        ));
    }
    if replay.discarded > 0 {
        out.push(format!(
            "discarded {} unreadable spooled job(s)",
            replay.discarded
        ));
    }
}

/// Run one hook invocation. Never returns an error: the caller exits 0.
///
/// `stdin` is read by the caller and only for `post-rewrite`
/// ([`HookEvent::reads_stdin`]).
pub fn run(
    event_name: &str,
    args: &[String],
    stdin: &str,
    cwd: &Path,
    socket: &Path,
    env: &HookEnv,
) -> Outcome {
    if env.skipping() {
        return Outcome::Skipped;
    }
    let Some(event) = HookEvent::parse(event_name) else {
        return Outcome::UnknownEvent(event_name.to_string());
    };
    let Some(git) = Git::discover(cwd) else {
        return Outcome::NotARepository(cwd.to_path_buf());
    };

    let spool = SpoolDir::new(&env.spool_dir);
    // Drain first: a backlog that is older than this changeset should reach the
    // daemon before it, and doing it here is what makes the spool self-healing
    // without a replay verb or a retry daemon.
    let replay = deliver::replay(socket, &spool);

    let (changeset, why) = match git::changeset_for(&git, event, args, stdin) {
        Ok(v) => v,
        Err(e) => return Outcome::GitFailed(format!("{e:#}")),
    };
    if changeset.is_empty() {
        return Outcome::Nothing {
            why: why.to_string(),
            replay,
        };
    }

    let project = git.root().to_path_buf();
    let (added, modified, removed) = (
        changeset.added.len(),
        changeset.modified.len(),
        changeset.removed.len(),
    );
    let delivery = deliver::deliver(
        socket,
        &spool,
        &project.to_string_lossy(),
        event.as_str(),
        changeset,
    );

    Outcome::Submitted {
        project,
        event,
        why,
        added,
        modified,
        removed,
        delivery,
        replay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(skip: Option<&str>, dir: &Path) -> HookEnv {
        HookEnv {
            skip: skip.map(str::to_string),
            spool_dir: dir.join("spool"),
        }
    }

    /// The opt-out's truth table. `FILIGRIO_SKIP_HOOK=true` silently *not*
    /// skipping is exactly the almost-worked this widens the oracle's `= "1"`
    /// to avoid.
    #[test]
    fn skip_hook_accepts_every_truthy_spelling_and_no_falsy_one() {
        let dir = tempfile::tempdir().unwrap();
        for on in ["1", "true", "TRUE", "yes", "on", " 1 "] {
            assert!(env(Some(on), dir.path()).skipping(), "{on:?} must skip");
        }
        for off in ["", "0", "false", "FALSE", "no", "  "] {
            assert!(!env(Some(off), dir.path()).skipping(), "{off:?} must run");
        }
        assert!(!env(None, dir.path()).skipping(), "unset must run");
    }

    /// The opt-out short-circuits **before** git: a skipped hook must not shell
    /// out at all, which is why this passes a directory that is not a repo and
    /// still expects a clean skip.
    #[test]
    fn skipping_happens_before_anything_else_and_prints_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run(
            "post-commit",
            &[],
            "",
            dir.path(),
            &dir.path().join("nothing.sock"),
            &env(Some("1"), dir.path()),
        );
        assert!(matches!(outcome, Outcome::Skipped));
        assert!(outcome.report().is_empty(), "a skip is silent");
        assert!(!dir.path().join("spool").exists(), "a skip touches nothing");
    }

    /// An event name the ADR does not subscribe to is reported, not obeyed —
    /// and never a process failure.
    #[test]
    fn an_unknown_event_is_named_in_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run(
            "pre-commit",
            &[],
            "",
            dir.path(),
            &dir.path().join("nothing.sock"),
            &env(None, dir.path()),
        );
        let Outcome::UnknownEvent(name) = &outcome else {
            panic!("expected UnknownEvent, got {outcome:?}");
        };
        assert_eq!(name, "pre-commit");
        assert!(outcome.report()[0].contains("post-commit"));
    }

    /// A hook run outside a worktree says so instead of guessing a project.
    #[test]
    fn a_non_repository_is_reported_not_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run(
            "post-commit",
            &[],
            "",
            dir.path(),
            &dir.path().join("nothing.sock"),
            &env(None, dir.path()),
        );
        assert!(
            matches!(outcome, Outcome::NotARepository(_)),
            "got {outcome:?}"
        );
    }
}
