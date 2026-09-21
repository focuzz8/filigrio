//! Shell completions, generated from the CLI's real `clap::Command`.
//!
//! Not part of ADR-0034 as written — added by the 2026-07-29 amendment. The
//! generator is [`clap_complete`] rather than hand-written scripts precisely
//! because the CLI is `clap`-derive: a hand-maintained completion file drifts
//! from the verb surface the first time a subcommand is added, silently, and
//! the drift only shows up as a missing tab-completion nobody reports.
//!
//! ## Where they go, and why
//!
//! User scope only — a completion is a property of *this user's shell*, and a
//! system-wide install would need root for no benefit.
//!
//! | shell | destination | picked up by |
//! |---|---|---|
//! | bash | `${XDG_DATA_HOME:-~/.local/share}/bash-completion/completions/filigrio` | `bash-completion` ≥ 2.x, automatically |
//! | zsh | `${XDG_DATA_HOME:-~/.local/share}/zsh/site-functions/_filigrio` | `compinit`, once the directory is on `fpath` |
//! | fish | `${XDG_CONFIG_HOME:-~/.config}/fish/completions/filigrio.fish` | fish, automatically |
//!
//! zsh is the one that needs a user action, so [`install`] emits a note with the
//! exact `fpath` line rather than leaving a file that quietly does nothing.
//!
//! ## Reversibility for a file we own whole
//!
//! There is no managed block here — the file *is* ours. What makes uninstall
//! safe is the trailer: every generated script ends with a `# filigrio-completion`
//! comment line, and uninstall removes the file **only if that line is present**.
//! A hand-written `_filigrio` that happens to share the name is reported and
//! left alone. The trailer goes last because zsh requires `#compdef` to be the
//! very first line of the file.

use crate::{read_opt, remove_file, render, write_if_changed, Action, Environment};
use crate::{InstallError, Report, STALE};
use clap::Command;
use serde::Serialize;
use std::path::PathBuf;

/// The shells we generate for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

pub const ALL_SHELLS: &[Shell] = &[Shell::Bash, Shell::Zsh, Shell::Fish];

/// The marker that identifies a completion file as ours.
pub const MARKER: &str = "# filigrio-completion";

impl Shell {
    pub fn slug(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bash" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            "fish" => Some(Shell::Fish),
            _ => None,
        }
    }

    fn generator(self) -> clap_complete::Shell {
        match self {
            Shell::Bash => clap_complete::Shell::Bash,
            Shell::Zsh => clap_complete::Shell::Zsh,
            Shell::Fish => clap_complete::Shell::Fish,
        }
    }

    /// Where this shell looks for a user's completions.
    pub fn destination(self, env: &Environment) -> PathBuf {
        let xdg_data = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| env.home.join(".local/share"));
        let xdg_config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| env.home.join(".config"));

        match self {
            Shell::Bash => xdg_data.join("bash-completion/completions/filigrio"),
            Shell::Zsh => xdg_data.join("zsh/site-functions/_filigrio"),
            Shell::Fish => xdg_config.join("fish/completions/filigrio.fish"),
        }
    }
}

#[derive(Debug, Serialize)]
struct TrailerModel {
    marker_start: &'static str,
    version: String,
    cli_bin: String,
}

/// Generate one shell's completion script from the live `Command`.
pub fn generate(
    cmd: &mut Command,
    bin_name: &str,
    shell: Shell,
    env: &Environment,
) -> Result<String, InstallError> {
    let mut buf: Vec<u8> = Vec::new();
    clap_complete::generate(shell.generator(), cmd, bin_name, &mut buf);
    let mut script = String::from_utf8(buf).map_err(|e| {
        InstallError::Template(format!(
            "clap_complete emitted non-UTF-8 for {}: {e}",
            shell.slug()
        ))
    })?;

    let hb = render::registry()?;
    let trailer = render::render(
        &hb,
        render::COMPLETION_TRAILER,
        &TrailerModel {
            marker_start: MARKER,
            version: env.version.clone(),
            cli_bin: env.cli_bin.display().to_string(),
        },
    )?;
    if !script.ends_with('\n') {
        script.push('\n');
    }
    script.push_str(trailer.trim_start_matches('\n'));
    Ok(script)
}

fn is_ours(text: &str) -> bool {
    text.lines().any(|l| l.trim_start().starts_with(MARKER))
}

/// Is there anything at this path that belongs to somebody else?
///
/// An **empty** file is nobody's: there is nothing in it to belong to anyone,
/// so writing over it takes nothing away. That clause is not a convenience —
/// it is what keeps this family idempotent on a symlinked destination, where a
/// removal empties the file *through* the link rather than deleting either end
/// of it (see [`crate::remove_file`]). Without it, the install after an
/// uninstall would report the file we had just emptied ourselves as a
/// stranger's and refuse to write it.
fn is_a_strangers(text: &str) -> bool {
    !text.trim().is_empty() && !is_ours(text)
}

/// Install the named shells' completions. `shells` is a selection, not a filter:
/// `filigrio completions install --shell fish` must not write a bash file, and
/// the loop never seeing it is the only way to be sure.
pub fn install(
    cmd: &mut Command,
    bin_name: &str,
    env: &Environment,
    shells: &[Shell],
    report: &mut Report,
) {
    for shell in shells {
        let path = shell.destination(env);
        let outcome = (|| {
            if let Some(existing) = read_opt(&path)? {
                if is_a_strangers(&existing) {
                    return Err(InstallError::Foreign {
                        path: path.clone(),
                        why: "no `# filigrio-completion` marker; remove it by hand or point \
                              XDG_DATA_HOME elsewhere"
                            .into(),
                    });
                }
            }
            let script = generate(cmd, bin_name, *shell, env)?;
            write_if_changed(&path, &script).map(|a| (a, String::new()))
        })();
        report.record(format!("completions/{}", shell.slug()), &path, outcome);
    }

    // Only when zsh was actually asked for. This is the one note in the family
    // that asks the user to *act*, and a `--shell fish` run that told them to
    // edit `~/.zshrc` would be the same category of confident wrongness as a
    // report naming a file it never wrote.
    if shells.contains(&Shell::Zsh) {
        let zsh = Shell::Zsh.destination(env);
        report.note(format!(
            "zsh needs the directory on its fpath: add `fpath=({} $fpath)` before `compinit` in \
             ~/.zshrc",
            zsh.parent().unwrap_or(&zsh).display()
        ));
    }
}

pub fn uninstall(env: &Environment, shells: &[Shell], report: &mut Report) {
    for shell in shells {
        let path = shell.destination(env);
        let outcome = (|| match read_opt(&path)? {
            None => Ok((Action::Absent, "not installed".to_string())),
            Some(text) if text.trim().is_empty() => Ok((
                Action::Absent,
                "not installed (the file here is empty)".to_string(),
            )),
            Some(text) if is_ours(&text) => {
                let removal = remove_file(&path)?;
                if let Some(dir) = path.parent() {
                    crate::prune_empty_dirs(dir, &env.home);
                }
                Ok((Action::Removed, removal.detail("completion")))
            }
            Some(_) => Err(InstallError::Foreign {
                path: path.clone(),
                why: "no `# filigrio-completion` marker, so it is not ours to delete".into(),
            }),
        })();
        report.record(format!("completions/{}", shell.slug()), &path, outcome);
    }
}

pub fn status(
    cmd: &mut Command,
    bin_name: &str,
    env: &Environment,
    shells: &[Shell],
    report: &mut Report,
) {
    for shell in shells {
        let path = shell.destination(env);
        let target = format!("completions/{}", shell.slug());
        let verdict = read_opt(&path).and_then(|on_disk| {
            let current = generate(cmd, bin_name, *shell, env)?;
            Ok(match on_disk {
                None => (Action::Absent, "not installed"),
                Some(text) if text.trim().is_empty() => {
                    (Action::Absent, "not installed (the file here is empty)")
                }
                Some(text) if text == current => (Action::Present, "current"),
                Some(text) if is_ours(&text) => (Action::Present, STALE),
                // [`Action::Foreign`], not `Present`: this is the one arm where
                // the file at our destination is not ours at all, and `install`
                // fails it while `uninstall` refuses it. Reporting it as
                // `Present` gave the same `=` a working, current completion
                // gets — three verbs, one file, three different verdicts, and
                // the quiet one was the verb people run first.
                Some(_) => (
                    Action::Foreign,
                    "not ours (no filigrio marker) — left alone; install will refuse it",
                ),
            })
        });
        match verdict {
            Ok((action, detail)) => report.step(target, &path, action, detail),
            Err(e) => report.fail(target, &path, e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Parser, Subcommand};

    #[derive(Parser)]
    #[command(name = "filigrio")]
    struct TestCli {
        #[command(subcommand)]
        cmd: TestCmd,
    }

    #[derive(Subcommand)]
    enum TestCmd {
        /// Graph commands.
        Graph {
            #[arg(long)]
            depth: usize,
        },
        /// Agent wiring lifecycle (ADR-0034 §17).
        Agent,
        /// Completion lifecycle.
        Completions,
    }

    fn env(home: &std::path::Path) -> Environment {
        Environment {
            project_root: home.to_path_buf(),
            home: home.to_path_buf(),
            cli_bin: PathBuf::from("/opt/g/bin/filigrio"),
            bridge_bin: PathBuf::from("/opt/g/bin/filigrio-mcp"),
            socket_path: PathBuf::from("/run/filigrio.sock"),
            version: "0.0.1".into(),
        }
    }

    fn cmd() -> Command {
        use clap::CommandFactory;
        TestCli::command()
    }

    /// Every shell generates, non-empty, and mentions the real verbs — which is
    /// what "generated from the live Command" buys.
    #[test]
    fn completions_generate_for_every_shell_from_the_real_command() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        for shell in ALL_SHELLS {
            let s = generate(&mut cmd(), "filigrio", *shell, &e).unwrap();
            assert!(s.len() > 100, "{} produced {} bytes", shell.slug(), s.len());
            assert!(s.contains("graph"), "{} lost a verb", shell.slug());
            assert!(s.contains("agent"), "{} lost a verb", shell.slug());
            assert!(s.contains(MARKER), "{} lost the marker", shell.slug());
        }
    }

    /// zsh's `#compdef` must stay the very first line — hence the trailer, not
    /// a header.
    #[test]
    fn the_zsh_script_still_begins_with_compdef() {
        let d = tempfile::tempdir().unwrap();
        let s = generate(&mut cmd(), "filigrio", Shell::Zsh, &env(d.path())).unwrap();
        assert!(
            s.starts_with("#compdef"),
            "zsh header clobbered:\n{}",
            &s[..80]
        );
        assert!(s.trim_end().ends_with("completions uninstall`."));
    }

    #[test]
    fn install_then_uninstall_leaves_the_tree_clean() {
        let d = tempfile::tempdir().unwrap();
        // Keep XDG out of it so the test writes under the temp home.
        temp_env(|| {
            let e = env(d.path());
            let mut r = Report::default();
            install(&mut cmd(), "filigrio", &e, ALL_SHELLS, &mut r);
            assert!(r.is_ok(), "{:?}", r.failures);
            for shell in ALL_SHELLS {
                assert!(shell.destination(&e).exists(), "{}", shell.slug());
            }

            let mut r = Report::default();
            install(&mut cmd(), "filigrio", &e, ALL_SHELLS, &mut r);
            assert!(
                r.steps.iter().all(|s| s.action == Action::Unchanged),
                "second install must be a no-op: {:?}",
                r.steps
            );

            let mut r = Report::default();
            uninstall(&e, ALL_SHELLS, &mut r);
            assert!(r.is_ok(), "{:?}", r.failures);
            for shell in ALL_SHELLS {
                assert!(!shell.destination(&e).exists(), "{}", shell.slug());
            }
        });
    }

    /// A hand-written completion of the same name is neither overwritten nor
    /// deleted — it is reported.
    #[test]
    fn a_foreign_completion_file_is_refused_in_both_directions() {
        let d = tempfile::tempdir().unwrap();
        temp_env(|| {
            let e = env(d.path());
            let p = Shell::Bash.destination(&e);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "# hand written by me\ncomplete -F x filigrio\n").unwrap();

            let mut r = Report::default();
            install(&mut cmd(), "filigrio", &e, ALL_SHELLS, &mut r);
            assert!(!r.is_ok());
            assert!(
                r.failures[0].reason.contains("was not written by filigrio"),
                "{}",
                r.failures[0].reason
            );
            assert_eq!(
                std::fs::read_to_string(&p).unwrap(),
                "# hand written by me\ncomplete -F x filigrio\n"
            );

            let mut r = Report::default();
            uninstall(&e, ALL_SHELLS, &mut r);
            assert!(!r.is_ok());
            assert!(p.exists(), "someone else's file must survive uninstall");
        });
    }

    /// zsh silently does nothing without an `fpath` entry, so the note is part
    /// of the contract, not a nicety.
    #[test]
    fn install_tells_the_user_about_zsh_fpath() {
        let d = tempfile::tempdir().unwrap();
        temp_env(|| {
            let mut r = Report::default();
            install(&mut cmd(), "filigrio", &env(d.path()), ALL_SHELLS, &mut r);
            assert!(
                r.notes.iter().any(|n| n.text().contains("fpath")),
                "{:?}",
                r.notes
            );
        });
    }

    /// **The one note that must never be folded.**
    ///
    /// It is the only line in a default install that asks the user to do
    /// something, and without it the completions are on disk and silently do
    /// nothing — the failure is indistinguishable from "filigrio has no
    /// completions". So it is emitted unkinded ([`Report::note`]) and this holds
    /// the whole path from emitter to renderer: the *exact* sentence, including
    /// the resolved directory, has to survive `Notes::Aggregated` as well as
    /// `Notes::Verbatim`.
    ///
    /// Asserting on the rendered text rather than on `r.notes` is deliberate —
    /// an aggregator that swallowed unkinded notes would leave `r.notes`
    /// untouched and this test green.
    #[test]
    fn the_zsh_fpath_note_survives_at_full_detail_in_both_renderings() {
        let d = tempfile::tempdir().unwrap();
        temp_env(|| {
            let e = env(d.path());
            let mut r = Report::default();
            install(&mut cmd(), "filigrio", &e, ALL_SHELLS, &mut r);

            let dir = Shell::Zsh.destination(&e);
            let dir = dir.parent().unwrap_or(&dir).display().to_string();
            let expected = format!(
                "  note: zsh needs the directory on its fpath: add `fpath=({dir} $fpath)` before \
                 `compinit` in ~/.zshrc\n"
            );

            for mode in [crate::Notes::Aggregated, crate::Notes::Verbatim] {
                let out = r.render(mode);
                assert!(
                    out.contains(&expected),
                    "{mode:?} lost or abridged the fpath note.\nwanted: {expected}\ngot:\n{out}"
                );
            }
        });
    }

    #[test]
    fn an_unwritable_destination_reports_which_and_why() {
        let d = tempfile::tempdir().unwrap();
        temp_env(|| {
            let e = env(d.path());
            // A *file* where the completions directory must be: mkdir fails.
            let bash = Shell::Bash.destination(&e);
            let parent = bash.parent().unwrap().to_path_buf();
            std::fs::create_dir_all(parent.parent().unwrap()).unwrap();
            std::fs::write(&parent, "not a directory\n").unwrap();

            let mut r = Report::default();
            install(&mut cmd(), "filigrio", &e, ALL_SHELLS, &mut r);
            assert!(!r.is_ok(), "an unwritable path must not pass silently");
            let f = &r.failures[0];
            assert!(f.target.contains("bash"));
            assert!(
                f.reason.contains(&parent.display().to_string()),
                "the reason must name the path: {}",
                f.reason
            );
        });
    }

    /// Three of the four verdicts. The fourth — a file that is not ours — is
    /// [`a_file_a_stranger_wrote_is_reported_as_foreign_and_not_as_present`],
    /// and it is separate because this test used to claim it and did not have
    /// it: its "foreign" fixture was `format!("old\n{{MARKER}} v0\n")`, which
    /// *carries* our marker, so it was the stale case a second time under a
    /// name that said otherwise.
    #[test]
    fn status_distinguishes_absent_current_and_stale() {
        let d = tempfile::tempdir().unwrap();
        temp_env(|| {
            let e = env(d.path());
            let mut r = Report::default();
            status(&mut cmd(), "filigrio", &e, ALL_SHELLS, &mut r);
            assert!(r.steps.iter().all(|s| s.action == Action::Absent));

            install(
                &mut cmd(),
                "filigrio",
                &e,
                ALL_SHELLS,
                &mut Report::default(),
            );
            let mut r = Report::default();
            status(&mut cmd(), "filigrio", &e, ALL_SHELLS, &mut r);
            assert!(
                r.steps.iter().all(|s| s.detail == "current"),
                "{:?}",
                r.steps
            );

            let fish = Shell::Fish.destination(&e);
            std::fs::write(&fish, format!("old\n{MARKER} v0\n")).unwrap();
            let mut r = Report::default();
            status(&mut cmd(), "filigrio", &e, ALL_SHELLS, &mut r);
            let stale = r
                .steps
                .iter()
                .find(|s| s.path == fish)
                .expect("a step for the file we just rewrote");
            assert_eq!(stale.action, Action::Present, "ours, and out of date");
            assert!(stale.detail.contains("stale"), "{:?}", stale.detail);
        });
    }

    /// **One file, three verbs, one verdict.**
    ///
    /// A hand-written `filigrio` completion is neither ours nor current, and
    /// the three verbs used to disagree about it in the loudest possible way:
    ///
    /// ```text
    /// status     = completions/bash  (not ours (no filigrio marker) …)   exit 0
    /// install    ! completions/bash  was not written by filigrio …       exit 1
    /// uninstall  ! completions/bash  … not ours to delete                exit 1
    /// ```
    ///
    /// `=` is the glyph a working, current completion gets. In a report a user
    /// scans as a column of symbols, that reads as "installed and fine" — about
    /// the one artifact the very next `install` will refuse. [`Action::Foreign`]
    /// exists for this arm and prints `?`, which cannot be read as success.
    ///
    /// The fixture carries **no marker at all**, which is what the test this
    /// splits off from lacked: with a marker present the arm is unreachable and
    /// changing its verdict string to `"current"` failed nothing.
    #[test]
    fn a_file_a_stranger_wrote_is_reported_as_foreign_and_not_as_present() {
        let d = tempfile::tempdir().unwrap();
        temp_env(|| {
            let e = env(d.path());
            let bash = Shell::Bash.destination(&e);
            let theirs = "# hand written by me, years ago\ncomplete -F _mine filigrio\n";
            std::fs::create_dir_all(bash.parent().unwrap()).unwrap();
            std::fs::write(&bash, theirs).unwrap();

            let mut r = Report::default();
            status(&mut cmd(), "filigrio", &e, ALL_SHELLS, &mut r);
            let step = r
                .steps
                .iter()
                .find(|s| s.path == bash)
                .expect("a step for the stranger's file");

            assert_eq!(
                step.action,
                Action::Foreign,
                "a file we did not write is not `{:?}`; detail was {:?}",
                step.action,
                step.detail
            );
            assert_ne!(
                step.action.symbol(),
                Action::Present.symbol(),
                "the symbol a current artifact gets must not be the symbol a \
                 stranger's file gets — that is the whole defect"
            );
            assert!(
                step.detail.contains("not ours") && step.detail.contains("marker"),
                "the detail must say why we left it alone: {:?}",
                step.detail
            );
            assert!(
                step.detail.contains("refuse"),
                "and must agree with what `install` will do to it: {:?}",
                step.detail
            );

            // The stranger's file is read, never touched.
            assert_eq!(std::fs::read_to_string(&bash).unwrap(), theirs);
        });
    }

    /// Provenance, not tidiness: the version in the trailer is the one the
    /// caller handed to [`Environment::discover`], never
    /// `env!("CARGO_PKG_VERSION")` read inside this crate — that macro reports
    /// *this library's* version, and the trailer speaks for the CLI.
    ///
    /// [`status`] decides `current` vs `stale` by re-rendering this very text
    /// and diffing it against the file on disk, so the day
    /// `filigrio-install`'s version moved independently of the CLI's, every
    /// completion would report `stale` and be rewritten on the next install —
    /// a difference with nothing in the repository to explain it.
    #[test]
    fn the_trailer_stamps_the_version_discover_was_given_not_this_crates_own() {
        let sentinel = "9999.0.0-from-the-caller";
        let e = Environment::discover(
            PathBuf::from("/nonexistent/project"),
            PathBuf::from("/run/filigrio.sock"),
            sentinel.to_string(),
        )
        .expect("discover needs $HOME and current_exe, both of which a test process has");
        assert_eq!(e.version, sentinel, "discover must not substitute its own");

        let script = generate(&mut cmd(), "filigrio", Shell::Bash, &e).unwrap();
        let trailer = script
            .lines()
            .find(|l| l.trim_start().starts_with(MARKER))
            .expect("every generated script ends with the marker line");
        assert!(
            trailer.contains(&format!("v{sentinel}")),
            "the caller's version must reach the trailer: {trailer}"
        );
        assert!(
            !trailer.contains(env!("CARGO_PKG_VERSION")),
            "this crate's version leaked into a line that speaks for the CLI: {trailer}"
        );
    }

    /// XDG vars are process-global; these tests each clear them for the
    /// duration so the destinations land under the temp home.
    fn temp_env(f: impl FnOnce()) {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let data = std::env::var_os("XDG_DATA_HOME");
        let config = std::env::var_os("XDG_CONFIG_HOME");
        std::env::remove_var("XDG_DATA_HOME");
        std::env::remove_var("XDG_CONFIG_HOME");
        f();
        if let Some(v) = data {
            std::env::set_var("XDG_DATA_HOME", v);
        }
        if let Some(v) = config {
            std::env::set_var("XDG_CONFIG_HOME", v);
        }
    }
}
