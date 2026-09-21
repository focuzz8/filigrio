//! Git hooks — the ADR-0032b commit-boundary producer's *script half*.
//!
//! ADR-0032b decided what the hooks do; this module generates, installs and
//! removes them. Four hooks, one template, one managed block each:
//!
//! | hook | transition | git's arguments |
//! |---|---|---|
//! | `post-commit` | a commit (or an amend) | none |
//! | `post-checkout` | a checkout | `$1` prev, `$2` new, `$3` branch-switch flag |
//! | `post-merge` | a merge or pull | `$1` squash flag |
//! | `post-rewrite` | a rebase or `commit --amend` | `$1` command; **pairs on stdin** |
//!
//! ### The script is a shim; `filigrio hooks run` is the producer
//!
//! Each block resolves the binary and calls
//!
//! ```sh
//! "<filigrio>" --socket "<socket>" hooks run <event> "$@" || true
//! ```
//!
//! passing git's own arguments through positionally and leaving stdin alone
//! (`post-rewrite` needs it). Everything with a decision in it lives in that
//! verb, not in generated shell: the client-side diff, the delivery ladder
//! (socket → spool → drop), the non-blocking behaviour, the `FILIGRIO_SKIP_HOOK`
//! opt-out, and the unconditional exit 0. That is the *right* split — shell is
//! the worst place to keep logic, and the oracle's hooks are 60 lines of
//! `sh` per file with a two-level Python launcher inside them
//! (`graphify/hooks.py:205-313`) precisely because it put the producer there.
//!
//! ### The socket is written in, not defaulted to
//!
//! `--socket` is spelled out, at the one position the CLI parses it (before the
//! verb — `hooks run`'s own arguments are trailing and hyphen-tolerant, so
//! anything after the event word is git's). A hook that defaulted the socket
//! while the seven MCP registrations carried whatever `install` was given
//! produces two halves that can never meet, on a run that reported every
//! artifact installed and exited 0 — the silent almost-worked this crate exists
//! to make impossible. Writing it in also buys the repair for free: [`status`]
//! compares the block it would write against the one on disk, so changing
//! `--socket` reports the hooks `stale` instead of `current`.
//!
//! One semantic the script therefore no longer carries, and which
//! `filigrio hooks run` owns: the `post-checkout` branch-flag gate (`$3 = 1`, so
//! a file checkout is not a tree transition — ADR-0032b OQ3). It fails *closed*
//! — fewer than three arguments is a no-op, not an assumed branch switch.
//!
//! Two guards the oracle's scripts carry are **deliberately absent here**, by
//! measurement rather than oversight (ADR-0032b, Implementation):
//!
//! * **rebase/merge-in-progress.** A conflicted merge fires no hook while it is
//!   unresolved, so such a guard would suppress the one submission that carries
//!   the merge; a conflicted rebase only ever emits real tree transitions. And a
//!   hook run leaves `git status --porcelain` and `git write-tree` byte-identical,
//!   which refutes the oracle's stated reason for guarding.
//! * **linked worktree.** `--show-toplevel` resolves to the linked worktree's own
//!   root, so it is addressed as itself. The oracle guards because its hook
//!   rebuilds into a shared `graphify-out/`; ours never does.
//!
//! Both are pinned by tests in `filigrio-client-cli`, so if either belief about
//! git is wrong it fails in the suite rather than in the field.
//!
//! ### Chain, never clobber (§3)
//!
//! Husky, `pre-commit`, and hand-written hooks are common. An existing hook
//! file gets our managed block **appended**; a missing one is created with a
//! `#!/bin/sh` shebang and mode 0755. `uninstall` removes exactly the block, and
//! deletes the file only when nothing but a shebang remains — the oracle's
//! `hooks.py:462-480` pattern, which is the cleanest round-trip in that
//! codebase.
//!
//! One case the oracle does not handle and we report: an existing hook that
//! **exits before our block would run**. Appending to a script whose last
//! effective statement is `exit 0` installs dead code. We cannot rewrite the
//! user's hook, so `install` and `status` say so ([`Report::notes`]) instead of
//! leaving them to wonder why nothing happens.
//!
//! ### Never block, never fail (§4)
//!
//! This is `filigrio hooks run`'s contract, not the shell's: it always exits 0
//! and owns its own delivery timing. The generated block deliberately does
//! **not** background the call — an `&` here would fork before the verb could
//! decide, would need every descriptor redirected (which would break
//! `post-rewrite`'s stdin), and would duplicate a guarantee that already exists
//! one layer down. The `|| true` on the call is belt and braces so a *missing*
//! binary — the one failure the verb cannot report, because it never ran —
//! still cannot fail the commit.

use crate::{block, render, Action, Environment, InstallError, Report, STALE};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The opt-out, spelled as the oracle spells it so a user who knows one knows
/// both (`graphify/hooks.py:287`).
pub const SKIP_ENV: &str = "FILIGRIO_SKIP_HOOK";

/// One installable hook. `name` is git's filename; `event` is the word passed
/// to `filigrio hooks run` — the same word today, kept as separate fields
/// because the file name is git's vocabulary and the event is ours.
#[derive(Debug, Clone, Copy)]
pub struct Hook {
    pub name: &'static str,
    pub event: &'static str,
    transition: &'static str,
    /// git feeds `post-rewrite` its commit pairs on stdin; the generated block
    /// says so, so nobody later adds a redirection that eats them.
    reads_stdin: bool,
}

impl Hook {
    /// Resolve a `--hook` value. `None` is an unknown name, which the CLI turns
    /// into a hard error listing [`HOOKS`] — never a silently empty selection.
    pub fn parse(name: &str) -> Option<Hook> {
        HOOKS.iter().copied().find(|h| h.name == name)
    }
}

/// Two hooks are the same hook when they are the same git file. Derived
/// equality would compare the prose fields too, which are ours and not the
/// identity.
impl PartialEq for Hook {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}
impl Eq for Hook {}

pub const HOOKS: &[Hook] = &[
    Hook {
        name: "post-commit",
        event: "post-commit",
        transition: "commit",
        reads_stdin: false,
    },
    Hook {
        name: "post-checkout",
        event: "post-checkout",
        transition: "checkout",
        reads_stdin: false,
    },
    Hook {
        name: "post-merge",
        event: "post-merge",
        transition: "merge",
        reads_stdin: false,
    },
    Hook {
        name: "post-rewrite",
        event: "post-rewrite",
        transition: "history rewrite",
        reads_stdin: true,
    },
];

/// Everything the hook template may name.
#[derive(Debug, Serialize)]
struct HookModel {
    marker_start: String,
    marker_end: String,
    hook_name: &'static str,
    event: &'static str,
    transition: &'static str,
    reads_stdin: bool,
    skip_env: &'static str,
    cli_bin: String,
    /// The socket the *registrations* were written with. The hook has to name
    /// the same one or the two halves of an install address different daemons.
    socket_path: String,
}

fn model(env: &Environment, hook: &Hook) -> HookModel {
    HookModel {
        marker_start: block::SHELL.start.to_string(),
        marker_end: block::SHELL.end.to_string(),
        hook_name: hook.name,
        event: hook.event,
        transition: hook.transition,
        reads_stdin: hook.reads_stdin,
        skip_env: SKIP_ENV,
        cli_bin: env.cli_bin.display().to_string(),
        socket_path: env.socket_path.display().to_string(),
    }
}

/// Render one hook's script block (markers included — the shell template owns
/// its own delimiters so the whole artifact is visible in one file).
pub fn script(env: &Environment, hook: &Hook) -> Result<String, InstallError> {
    let hb = render::registry()?;
    render::render(&hb, render::GIT_HOOK, &model(env, hook))
}

/// Where git keeps this repository's hooks.
///
/// `git rev-parse --git-path hooks` is asked rather than `.git/hooks` assumed,
/// because `core.hooksPath`, submodules, and linked worktrees all move it. A
/// repository without git on `PATH` falls back to `<root>/.git/hooks`, and only
/// after checking that `<root>/.git` exists at all.
pub fn hooks_dir(project_root: &Path) -> Result<PathBuf, InstallError> {
    let dot_git = project_root.join(".git");
    if !dot_git.exists() {
        return Err(InstallError::NotAGitRepo {
            path: project_root.to_path_buf(),
        });
    }

    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["rev-parse", "--git-path", "hooks"])
        .output();

    if let Ok(out) = out {
        if out.status.success() {
            let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
            // Reject anything with an embedded newline/NUL: git <2.31 has been
            // known to echo flags back literally, and a phantom directory is
            // worse than a fallback.
            if !raw.is_empty() && !raw.contains(['\n', '\r', '\0']) {
                let p = PathBuf::from(&raw);
                return Ok(if p.is_absolute() {
                    p
                } else {
                    project_root.join(p)
                });
            }
        }
    }
    Ok(dot_git.join("hooks"))
}

/// Does this hook script exit before our appended block could run?
///
/// A conservative, purely textual check: an `exit 0` (or `exit`) at column zero
/// outside our own block, with nothing after it but blanks, comments, and our
/// block. False negatives are fine — this exists to catch the common Husky-ish
/// shape, not to parse shell.
fn exits_before_our_block(content: &str) -> bool {
    let mut saw_exit = false;
    for line in content.lines() {
        if line.trim_start() != line {
            continue; // indented: inside a function or conditional
        }
        let t = line.trim();
        if t == block::SHELL.start {
            return saw_exit;
        }
        if t == "exit" || t == "exit 0" {
            saw_exit = true;
        } else if !t.is_empty() && !t.starts_with('#') {
            saw_exit = false;
        }
    }
    saw_exit
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), InstallError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|source| InstallError::Read {
            path: path.to_path_buf(),
            source,
        })?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|source| InstallError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), InstallError> {
    Ok(()) // Windows has no mode bit; git runs hooks through sh regardless.
}

/// Would git run this file?
///
/// git tests the executable bit before invoking a hook, so a hook whose content
/// is exactly what `install` writes but which lost its `+x` is installed and
/// inert. A hook loses it easily and invisibly: a tarball or `rsync` restore, a
/// `chmod -R` over the worktree, a `core.hooksPath` directory on a filesystem
/// that drops the bit, an edit from the Windows side of a shared checkout.
///
/// A missing answer is *not* reported as a defect — [`status`] only asks this
/// after it has already read the file, so the metadata call cannot realistically
/// fail, and inventing a `stale` out of an unreadable mode would be a worse lie
/// than the one this exists to stop.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(true)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true // No mode bit to lose.
}

const SHEBANG: &str = "#!/bin/sh";

fn install_one(
    env: &Environment,
    dir: &Path,
    hook: &Hook,
    report: &mut Report,
) -> Result<(Action, String), InstallError> {
    let path = dir.join(hook.name);
    let body = script(env, hook)?;
    // The template carries its own markers, so the block body handed to
    // `upsert` is the *inner* text.
    let inner = body
        .lines()
        .skip(1)
        .take_while(|l| l.trim() != block::SHELL.end)
        .collect::<Vec<_>>()
        .join("\n");

    let existing = crate::read_opt(&path)?;
    let (action, detail) = match existing {
        None => {
            let content = block::upsert(&format!("{SHEBANG}\n"), &inner, &block::SHELL);
            crate::write_all(&path, &content)?;
            (Action::Installed, "hook created".to_string())
        }
        Some(prior) => {
            let updated = block::upsert(&prior, &inner, &block::SHELL);
            if updated == prior {
                (Action::Unchanged, "already current".to_string())
            } else {
                let had = block::contains(&prior, &block::SHELL);
                crate::write_all(&path, &updated)?;
                if had {
                    (Action::Updated, "managed block refreshed".to_string())
                } else {
                    if exits_before_our_block(&updated) {
                        report.note(format!(
                            "{}: the pre-existing hook ends in an unconditional `exit`, so the \
                             appended filigrio block will never run — move it above that exit, or \
                             chain filigrio from the existing script",
                            path.display()
                        ));
                    }
                    (
                        Action::Installed,
                        "appended to the existing hook".to_string(),
                    )
                }
            }
        }
    };
    // Unconditionally, including on the `Unchanged` path. The mode is part of
    // the artifact — git will not run a hook without it — and it can be lost
    // without the *content* changing, so a repair keyed on "did the text
    // change?" is a repair that never fires in exactly the case that needs it.
    // `lib.rs`'s standing instruction is "install is idempotent, so the repair
    // is to put it right and run it again"; this is what makes that true here.
    let restored_mode = !is_executable(&path);
    make_executable(&path)?;

    // …and the report has to describe the file this run *left*, not the one it
    // read. `already current` on a run that just chmod'd the hook would be the
    // same small lie in the other direction. Only the otherwise-`Unchanged` case
    // is rephrased: on the two write paths the mode is part of the write.
    if restored_mode && action == Action::Unchanged {
        return Ok((Action::Updated, "executable bit restored".to_string()));
    }
    Ok((action, detail))
}

/// Install the named hooks. `hooks` is a selection, not a filter applied later:
/// `filigrio hooks install --hook post-commit` must not touch `post-merge`, and
/// the only way to be sure of that is for the loop never to see it.
pub fn install(env: &Environment, hooks: &[Hook], report: &mut Report) {
    let dir = match hooks_dir(&env.project_root) {
        Ok(d) => d,
        Err(e) => {
            report.fail("hooks", &env.project_root, e.to_string());
            return;
        }
    };
    for hook in hooks {
        let path = dir.join(hook.name);
        let outcome = install_one(env, &dir, hook, report);
        report.record(format!("hooks/{}", hook.name), &path, outcome);
    }
}

pub fn uninstall(env: &Environment, hooks: &[Hook], report: &mut Report) {
    let dir = match hooks_dir(&env.project_root) {
        Ok(d) => d,
        Err(e) => {
            report.fail("hooks", &env.project_root, e.to_string());
            return;
        }
    };
    for hook in hooks {
        let path = dir.join(hook.name);
        // "Nothing left but a shebang" means the file was ours; anything else
        // is the user's hook and stays.
        let outcome = block::remove_file_block(&path, &block::SHELL, |rest| {
            let t = rest.trim();
            t.is_empty() || t == SHEBANG || t == "#!/bin/bash"
        });
        report.record(format!("hooks/{}", hook.name), &path, outcome);
    }
}

pub fn status(env: &Environment, hooks: &[Hook], report: &mut Report) {
    let dir = match hooks_dir(&env.project_root) {
        Ok(d) => d,
        Err(e) => {
            report.fail("hooks", &env.project_root, e.to_string());
            return;
        }
    };
    for hook in hooks {
        let path = dir.join(hook.name);
        match (crate::read_opt(&path), script(env, hook)) {
            (Ok(Some(text)), Ok(current)) => {
                let want = current
                    .lines()
                    .skip(1)
                    .take_while(|l| l.trim() != block::SHELL.end)
                    .collect::<Vec<_>>()
                    .join("\n");
                match block::body_of(&text, &block::SHELL) {
                    Some(have) if have.trim() == want.trim() => {
                        if exits_before_our_block(&text) {
                            report.note(format!(
                                "{}: an unconditional `exit` precedes the filigrio block — it \
                                 cannot run",
                                path.display()
                            ));
                        }
                        // A byte-current block in a file git will not execute is
                        // not `current`; it is an installation that does
                        // nothing. `STALE` is the right word — the artifact is
                        // not what `install` would leave, and re-running
                        // `install` is exactly the repair — and the note carries
                        // the part a text diff of the block cannot show.
                        if !is_executable(&path) {
                            report.note(format!(
                                "{}: the filigrio block is current but the hook is not \
                                 executable, so git will never run it",
                                path.display()
                            ));
                            report.step(
                                format!("hooks/{}", hook.name),
                                &path,
                                Action::Present,
                                STALE,
                            )
                        } else {
                            report.step(
                                format!("hooks/{}", hook.name),
                                &path,
                                Action::Present,
                                "current",
                            )
                        }
                    }
                    Some(_) => report.step(
                        format!("hooks/{}", hook.name),
                        &path,
                        Action::Present,
                        STALE,
                    ),
                    None => report.step(
                        format!("hooks/{}", hook.name),
                        &path,
                        Action::Absent,
                        "hook exists but holds no filigrio block",
                    ),
                }
            }
            (Ok(None), _) => report.step(
                format!("hooks/{}", hook.name),
                &path,
                Action::Absent,
                "not installed",
            ),
            (Err(e), _) | (_, Err(e)) => {
                report.fail(format!("hooks/{}", hook.name), &path, e.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(root: &Path) -> Environment {
        Environment {
            project_root: root.to_path_buf(),
            home: root.to_path_buf(),
            cli_bin: PathBuf::from("/opt/g/bin/filigrio"),
            bridge_bin: PathBuf::from("/opt/g/bin/filigrio-mcp"),
            socket_path: PathBuf::from("/run/filigrio.sock"),
            version: "0.0.1".into(),
        }
    }

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".git/hooks")).unwrap();
        d
    }

    /// The shared contract with `filigrio hooks run`: the absolute binary, the
    /// socket, the event word, git's own arguments passed through positionally,
    /// and a failure that cannot reach git.
    #[test]
    fn every_hook_calls_the_agreed_verb_with_gits_arguments() {
        let d = repo();
        let e = env(d.path());
        for hook in HOOKS {
            let s = script(&e, hook).unwrap();
            assert!(
                s.contains(&format!(
                    "\"/opt/g/bin/filigrio\" --socket \"/run/filigrio.sock\" hooks run {} \"$@\" \
                     || true",
                    hook.event
                )),
                "{} does not call the agreed verb:\n{s}",
                hook.name
            );
        }
    }

    /// The half of an install that the socket flag used to miss.
    ///
    /// `--socket` is written into all seven MCP registrations at install time;
    /// the hook script carried none, so `filigrio hooks run` fell back to
    /// `default_socket_path()` and the two halves addressed different daemons
    /// while every artifact reported installed and the run exited 0.
    ///
    /// The *position* is load-bearing and is asserted rather than assumed:
    /// `--socket` is a global clap flag, but `hooks run`'s own arguments are
    /// `trailing_var_arg` + `allow_hyphen_values`, so a `--socket` placed after
    /// the event word would be handed to the verb as one of git's arguments and
    /// silently ignored. It has to precede `hooks run`, and git's `"$@"` has to
    /// stay last and verbatim.
    #[test]
    fn a_non_default_socket_reaches_the_generated_hook_ahead_of_the_verb() {
        let d = repo();
        let e = Environment {
            socket_path: PathBuf::from("/custom/filigrio.sock"),
            ..env(d.path())
        };
        for hook in HOOKS {
            let s = script(&e, hook).unwrap();
            let line = s
                .lines()
                .find(|l| l.contains("/opt/g/bin/filigrio\""))
                .unwrap_or_else(|| panic!("{} has no call line:\n{s}", hook.name));

            let flag = line.find("--socket").expect("the socket must be named");
            let socket = line.find("\"/custom/filigrio.sock\"").unwrap_or_else(|| {
                panic!("{} does not carry the chosen socket: {line}", hook.name)
            });
            let verb = line.find(" hooks run ").expect("the verb");
            assert!(
                flag < socket && socket < verb,
                "`--socket <path>` must precede the verb, or clap hands it to git's argv: {line}"
            );
            assert!(
                line.trim_end().ends_with("\"$@\" || true"),
                "git's own arguments must stay last and verbatim: {line}"
            );
        }
    }

    /// The socket is part of what `install` would write, so moving it makes the
    /// hooks *stale* rather than silently wrong — the same verdict the MCP
    /// registrations already give, and the same repair.
    #[test]
    fn moving_the_socket_makes_the_installed_hooks_stale() {
        let d = repo();
        let e = env(d.path());
        install(&e, HOOKS, &mut Report::default());

        let moved = Environment {
            socket_path: PathBuf::from("/run/user/1000/filigrio.sock"),
            ..e.clone()
        };
        let mut r = Report::default();
        status(&moved, HOOKS, &mut r);
        assert!(
            r.steps.iter().all(|s| s.detail == STALE),
            "a moved socket must not read as current: {:?}",
            r.steps
        );

        // And the repair the message names actually works.
        install(&moved, HOOKS, &mut Report::default());
        let mut r = Report::default();
        status(&moved, HOOKS, &mut r);
        assert!(
            r.steps.iter().all(|s| s.detail == "current"),
            "{:?}",
            r.steps
        );
    }

    /// The opt-out is *documented*, not re-implemented: `filigrio hooks run`
    /// owns it, and a second check in shell would be a second place to get it
    /// wrong.
    #[test]
    fn the_opt_out_is_named_in_the_script_but_implemented_in_the_binary() {
        let d = repo();
        let e = env(d.path());
        for hook in HOOKS {
            let s = script(&e, hook).unwrap();
            assert!(
                s.contains(SKIP_ENV),
                "{} never mentions {SKIP_ENV}",
                hook.name
            );
            assert!(
                !s.contains(&format!("[ \"${SKIP_ENV}\"")),
                "{} re-implements the opt-out the binary owns:\n{s}",
                hook.name
            );
        }
    }

    /// Nothing in the block may block git or touch stdin. Backgrounding is the
    /// binary's business; a stray `<`/`>` redirection here would eat
    /// `post-rewrite`'s commit pairs.
    #[test]
    fn the_block_neither_backgrounds_nor_redirects() {
        let d = repo();
        let e = env(d.path());
        for hook in HOOKS {
            let s = script(&e, hook).unwrap();
            let code: Vec<&str> = s
                .lines()
                .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
                .collect();
            assert_eq!(
                code.len(),
                1,
                "{} is not a one-line shim: {code:?}",
                hook.name
            );
            let line = code[0];
            assert!(!line.contains('&') || line.contains("&&"), "{line}");
            assert!(!line.contains('>') && !line.contains('<'), "{line}");
        }
    }

    /// The one genuine per-hook divergence left in the template: `post-rewrite`
    /// reads stdin, and the block says so.
    #[test]
    fn only_post_rewrite_documents_its_stdin() {
        let d = repo();
        let e = env(d.path());
        for hook in HOOKS {
            let s = script(&e, hook).unwrap();
            assert_eq!(
                s.contains("on **stdin**"),
                hook.name == "post-rewrite",
                "stdin note misplaced on {}",
                hook.name
            );
        }
    }

    #[test]
    fn a_fresh_repo_gets_executable_hooks_with_a_shebang() {
        let d = repo();
        let e = env(d.path());
        let mut r = Report::default();
        install(&e, HOOKS, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        for hook in HOOKS {
            let p = d.path().join(".git/hooks").join(hook.name);
            let text = std::fs::read_to_string(&p).unwrap();
            assert!(text.starts_with("#!/bin/sh\n"), "{}", hook.name);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&p).unwrap().permissions().mode();
                assert_eq!(mode & 0o111, 0o111, "{} is not executable", hook.name);
            }
        }
    }

    #[test]
    fn install_is_idempotent() {
        let d = repo();
        let e = env(d.path());
        install(&e, HOOKS, &mut Report::default());
        let once = std::fs::read_to_string(d.path().join(".git/hooks/post-commit")).unwrap();

        let mut r = Report::default();
        install(&e, HOOKS, &mut r);
        assert!(r.steps.iter().all(|s| s.action == Action::Unchanged));
        assert_eq!(
            std::fs::read_to_string(d.path().join(".git/hooks/post-commit")).unwrap(),
            once
        );
        assert_eq!(once.matches(block::SHELL.start).count(), 1);
    }

    /// Chain, never clobber: an existing hook keeps working and comes back
    /// byte-exact.
    #[test]
    fn an_existing_hook_is_appended_to_and_restored_byte_exactly() {
        let d = repo();
        let e = env(d.path());
        let p = d.path().join(".git/hooks/post-commit");
        let prior = "#!/bin/sh\n# husky\n. \"$(dirname -- \"$0\")/_/husky.sh\"\nnpm run lint\n";
        std::fs::write(&p, prior).unwrap();

        install(&e, HOOKS, &mut Report::default());
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.starts_with(prior), "the user's hook must stay first");
        assert!(after.contains(block::SHELL.start));

        uninstall(&e, HOOKS, &mut Report::default());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    #[test]
    fn a_hook_we_created_is_deleted_on_uninstall() {
        let d = repo();
        let e = env(d.path());
        install(&e, HOOKS, &mut Report::default());
        let mut r = Report::default();
        uninstall(&e, HOOKS, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        for hook in HOOKS {
            assert!(
                !d.path().join(".git/hooks").join(hook.name).exists(),
                "{}",
                hook.name
            );
        }
    }

    /// Appending under an unconditional `exit` installs dead code. We cannot
    /// fix the user's script, so we must say so.
    #[test]
    fn an_early_exit_in_the_users_hook_is_reported_not_ignored() {
        let d = repo();
        let e = env(d.path());
        let p = d.path().join(".git/hooks/post-commit");
        std::fs::write(&p, "#!/bin/sh\nrun-my-thing\nexit 0\n").unwrap();

        let mut r = Report::default();
        install(&e, HOOKS, &mut r);
        assert!(
            r.notes.iter().any(|n| n.text().contains("never run")),
            "notes were {:?}",
            r.notes
        );
    }

    #[test]
    fn a_hook_with_real_work_after_an_exit_is_not_flagged() {
        assert!(!exits_before_our_block(
            "#!/bin/sh\nexit 0\nmore-work\n# filigrio-hook-start\n"
        ));
        assert!(exits_before_our_block(
            "#!/bin/sh\nwork\nexit 0\n\n# filigrio-hook-start\n"
        ));
        assert!(!exits_before_our_block(
            "#!/bin/sh\nif [ x ]; then\n  exit 0\nfi\n# filigrio-hook-start\n"
        ));
    }

    #[test]
    fn a_directory_without_git_fails_with_a_reason_not_a_panic() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let mut r = Report::default();
        install(&e, HOOKS, &mut r);
        assert!(!r.is_ok());
        assert!(r.failures[0].reason.contains("not a git repository"));
    }

    #[test]
    fn status_reports_absent_current_and_stale() {
        let d = repo();
        let e = env(d.path());

        let mut r = Report::default();
        status(&e, HOOKS, &mut r);
        assert!(r.steps.iter().all(|s| s.action == Action::Absent));

        install(&e, HOOKS, &mut Report::default());
        let mut r = Report::default();
        status(&e, HOOKS, &mut r);
        assert!(
            r.steps.iter().all(|s| s.detail == "current"),
            "{:?}",
            r.steps
        );

        let p = d.path().join(".git/hooks/post-commit");
        let t = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, block::upsert(&t, "old body", &block::SHELL)).unwrap();
        let mut r = Report::default();
        status(&e, HOOKS, &mut r);
        assert!(r.steps.iter().any(|s| s.detail.contains("stale")));
    }

    /// A hook that lost its `+x` is installed and inert, and the crate's
    /// standing repair — "install is idempotent, so put it right and run it
    /// again" — has to actually put it right.
    ///
    /// Both halves are the defect: `status` called the hook `current` because it
    /// only ever compared text, and `install` reported `already current` and
    /// changed nothing because `make_executable` sat on the two *write* paths
    /// only. A hook loses the bit through a tarball or `rsync` restore, a
    /// `chmod -R`, or an edit from the Windows side of a shared checkout.
    #[cfg(unix)]
    #[test]
    fn a_hook_that_lost_its_executable_bit_is_not_current_and_install_repairs_it() {
        use std::os::unix::fs::PermissionsExt;

        let d = repo();
        let e = env(d.path());
        install(&e, HOOKS, &mut Report::default());
        let p = d.path().join(".git/hooks/post-commit");

        let before = std::fs::read_to_string(&p).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        let mut r = Report::default();
        status(&e, HOOKS, &mut r);
        let step = r
            .steps
            .iter()
            .find(|s| s.target == "hooks/post-commit")
            .expect("a post-commit line");
        assert_ne!(
            step.detail, "current",
            "git will not run a 0644 hook; status must not call it current"
        );
        assert!(
            r.notes.iter().any(|n| n.text().contains("not executable")),
            "the reason is not visible in a diff of the block, so it has to be said: {:?}",
            r.notes
        );

        let mut r = Report::default();
        install(&e, HOOKS, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o755,
            "install must repair the mode it is responsible for"
        );
        let repaired = r
            .steps
            .iter()
            .find(|s| s.target == "hooks/post-commit")
            .expect("a post-commit line");
        assert_eq!(
            (repaired.action, repaired.detail.as_str()),
            (Action::Updated, "executable bit restored"),
            "the report must describe the file this run left, not the one it read"
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "and must not have rewritten the content to do it"
        );

        let mut r = Report::default();
        status(&e, HOOKS, &mut r);
        assert!(
            r.steps.iter().all(|s| s.detail == "current"),
            "{:?}",
            r.steps
        );
    }

    /// `core.hooksPath` moves the directory; we ask git rather than assume.
    #[test]
    fn hooks_dir_follows_core_hookspath() {
        let d = repo();
        // A real git repo is needed for `rev-parse` to answer.
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(d.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return; // no git on this box; the fallback path is covered above
        }
        std::process::Command::new("git")
            .args(["config", "core.hooksPath", "myhooks"])
            .current_dir(d.path())
            .status()
            .unwrap();
        let dir = hooks_dir(d.path()).unwrap();
        assert!(
            dir.ends_with("myhooks"),
            "expected core.hooksPath to be honoured, got {}",
            dir.display()
        );
    }
}
