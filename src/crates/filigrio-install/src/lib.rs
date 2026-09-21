//! filigrio-install — the installation surface (ADR-0034, amended 2026-07-29).
//!
//! After downloading the binary, four resource commands wire the tool into a
//! developer's environment (ADR-0034 §17, §18.2). Four artifact families, four
//! commands, one lifecycle — and **no command spans families**, which is what
//! makes oversupply unrepresentable rather than a default to get right:
//!
//! | command | family | what it writes | reversibility |
//! |---|---|---|---|
//! | `filigrio agent` | [`clients`] | one agent's MCP registration, plus its skill where the vendor has one | keyed JSON/TOML/YAML entry, whole file for a skill |
//! | `filigrio docs` | [`docs`] | the repository's `AGENTS.md` | managed block |
//! | `filigrio hooks` | [`hooks`] | the ADR-0032b git hooks | managed block inside an existing hook |
//! | `filigrio completions` | [`completions`] | clap-generated bash/zsh/fish completions | whole file, marker-guarded |
//!
//! `docs` is the fourth because `AGENTS.md` is a **repository** artifact and not
//! an agent's (§18.2): it is user-owned, read by agents this build supports and
//! by agents it does not, and adding it is a decision about the repository in the
//! same sense a README is. Modelling it as an eighth row in `--agent` produced an
//! "agent" that registered nothing and had to explain that there was nothing to
//! detect.
//!
//! Everything ships **inside the binary**: templates and the canonical
//! capability doc are `include_str!`'d, and the completions are generated from
//! the CLI's own [`clap::Command`], so nothing is fetched or looked up at
//! install time.
//!
//! ## The two invariants every writer here holds
//!
//! - **Reversible.** Uninstall removes exactly what install added. Shared files
//!   (`AGENTS.md`, a pre-existing `post-commit`) are edited through
//!   [`block`]'s marker-delimited managed blocks, never clobbered; `.mcp.json`
//!   through a single named key. See [`block`] for the byte-exactness argument.
//! - **Honest.** A failure is *reported*, not swallowed: an unwritable path or
//!   an absent client lands in [`Report::failures`] / [`Report::notes`] with the
//!   path and the reason, and the run continues so one broken target does not
//!   hide the other nine.
//!
//! This crate is a **library**; `filigrio-client-cli` owns the verbs. It links
//! no engine crate (ADR-0032f §1) — see `tests/dependency_hygiene.rs`.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod block;
pub mod capability;
pub mod clients;
pub mod completions;
pub mod docs;
pub mod hooks;
pub mod json_entry;
pub mod render;
pub mod toml_entry;
pub mod yaml_block;

use std::path::{Path, PathBuf};

pub use clients::{
    installer_for, ClientId, ClientInstaller, Detection, OtherScope, Scope, ScopeSupport,
    ALL_CLIENTS,
};
pub use completions::Shell;

/// The environment an install writes into: every path the artifacts need,
/// resolved once by the caller so nothing here guesses at runtime.
#[derive(Debug, Clone)]
pub struct Environment {
    /// The repository being wired up (where `.git`, `AGENTS.md`, `.mcp.json` live).
    pub project_root: PathBuf,
    /// The user's home directory — the root of every user-scoped destination.
    pub home: PathBuf,
    /// Absolute path to the `filigrio` CLI binary.
    pub cli_bin: PathBuf,
    /// Absolute path to the `filigrio-mcp` stdio bridge (ADR-0032f §1). This is
    /// what a client's MCP config points at — never the daemon, never the CLI.
    pub bridge_bin: PathBuf,
    /// The daemon socket the bridge will connect to.
    pub socket_path: PathBuf,
    /// Version stamped into generated artifacts — the **CLI's**, supplied by the
    /// caller rather than read from this crate. See [`Environment::discover`].
    pub version: String,
}

impl Environment {
    /// Resolve an environment from this process: `current_exe`'s directory is
    /// where the sibling binaries live, which is true for every shipping layout
    /// (a release tarball, `cargo install`, `target/debug`).
    ///
    /// Returns an error rather than a guess when a fact is unavailable — an
    /// installer that silently wrote `filigrio-mcp` as a bare name would
    /// produce an MCP config that only works when the binary is on the client's
    /// `PATH`, which is exactly the failure that reads as "the tools are just
    /// missing".
    ///
    /// `version` is a **parameter**, not `env!("CARGO_PKG_VERSION")` read here:
    /// that macro expands to *this library's* version, and what the artifacts
    /// claim is the CLI's. The two coincide today only because both crates read
    /// `version.workspace = true`. Should they ever diverge, the stamped version
    /// is inside the text [`completions::status`] re-renders and diffs against
    /// the file on disk, so every completion would report `stale` and be
    /// rewritten on each install — a phantom difference with no visible cause.
    /// The caller knows whose version it is; this crate does not.
    pub fn discover(
        project_root: PathBuf,
        socket_path: PathBuf,
        version: String,
    ) -> Result<Self, InstallError> {
        let exe = std::env::current_exe().map_err(|e| InstallError::Environment {
            what: "current executable path".into(),
            why: e.to_string(),
        })?;
        let dir = exe
            .parent()
            .ok_or_else(|| InstallError::Environment {
                what: "executable directory".into(),
                why: format!("{} has no parent", exe.display()),
            })?
            .to_path_buf();
        let home = home_dir().ok_or_else(|| InstallError::Environment {
            what: "home directory".into(),
            why: "neither $HOME nor $USERPROFILE is set".into(),
        })?;
        Ok(Self {
            project_root,
            home,
            cli_bin: dir.join("filigrio"),
            bridge_bin: dir.join("filigrio-mcp"),
            socket_path,
            version,
        })
    }
}

/// Verify that the binaries this environment names are actually on disk.
///
/// [`Environment::discover`] writes **absolute** paths into every artifact so a
/// client spawning the bridge from a GUI process with no useful `PATH` still
/// finds it. That closed one half of "the tools are just missing" and left the
/// other half open: a path can be absolute, correctly shaped, and point at
/// nothing. A partial install — a release tarball missing a binary, a
/// `cargo install` of one crate, a stale `target/debug` — then produces a
/// registration that can never work while every artifact reports `+` and the run
/// ends in `no failures`. A confidently wrong success is the one outcome this
/// crate's honest-failure posture exists to make unreachable.
///
/// So a missing binary is a **failure**, not a note. The line the crate draws
/// elsewhere is whether the artifact is meaningful without the absent thing: an
/// undetected client still gets its files, because they start working the moment
/// the client arrives. A registration naming a binary that does not exist is not
/// waiting for anything — it is wrong now, and wrong in the one way the user
/// cannot see from the output. `install` is idempotent, so the repair is to put
/// the binary where it belongs and run it again.
///
/// `need_bridge` is false for every command but `filigrio agent` (ADR-0034 §17):
/// the bridge is what an MCP registration names, and `hooks install` /
/// `completions install` write nothing that mentions it.
pub fn check_binaries(env: &Environment, need_bridge: bool, report: &mut Report) {
    let mut check = |role: &str, path: &Path, named_by: &str| {
        if path.is_file() {
            return;
        }
        report.fail(
            format!("binary/{role}"),
            path,
            format!(
                "{} is not on disk, and it is the command every {named_by} this run writes \
                 points at — they would be installed and inert. The usual cause is a partial \
                 install: a tarball missing a binary, a `cargo install` of one crate, or a \
                 stale build directory. Put `{role}` beside the CLI and run install again; \
                 installing twice changes nothing.",
                path.display()
            ),
        );
    };

    check(
        "filigrio",
        &env.cli_bin,
        "git hook, completion trailer and capability doc",
    );
    if need_bridge {
        check("filigrio-mcp", &env.bridge_bin, "MCP registration");
    }
}

/// `$HOME`, falling back to Windows' `%USERPROFILE%`. Deliberately not a `dirs`
/// dependency: this is the one lookup we need and both variables are the
/// documented contract on their platforms.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// What happened to one artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Written where nothing of ours was before.
    Installed,
    /// Our managed block was already there and its content changed.
    Updated,
    /// Our managed block was already there, byte-identical (the idempotent case).
    Unchanged,
    /// Ours was there and is now gone.
    Removed,
    /// Nothing of ours to remove, or nothing installed to report.
    Absent,
    /// Present and current (a `status` verdict).
    Present,
    /// A file we did **not** write is sitting at one of our destinations (a
    /// `status` verdict) — the hand-written `_filigrio` a user has had on their
    /// `fpath` for years.
    ///
    /// Its own variant because the alternative was [`Action::Present`], and
    /// `Present` renders `=` — the same glyph `Unchanged` prints for "installed
    /// and current". One file was then getting three different verdicts from
    /// three verbs: `status` said `=` and exited 0, `install` failed it, and
    /// `uninstall` refused it. Of the three, the silent one is the verb people
    /// run first, and `=` is the one symbol in this column a user reads as
    /// "nothing to do here".
    ///
    /// This says nothing about *why* — that is the detail's job — only that
    /// what is at the path is a stranger's, so no verdict about our artifact
    /// applies to it.
    Foreign,
}

impl Action {
    pub fn symbol(self) -> &'static str {
        match self {
            Action::Installed => "+",
            Action::Updated => "~",
            Action::Unchanged | Action::Present => "=",
            Action::Removed => "-",
            Action::Absent => ".",
            // `?` and not one of the five above, because every one of those is
            // a verdict about an *artifact of ours* and this line is not about
            // one. It is the question the run is handing back — "there is a
            // file here that filigrio did not write; what would you like done
            // with it?" — and the only property it must have is that a user
            // skimming the column cannot file it under "fine". `=` failed that
            // test; `?` cannot be read as success in any direction.
            Action::Foreign => "?",
        }
    }
}

/// The one sentence `status` uses for "ours is here, but it is not what install
/// would write".
///
/// Five kinds of artifact answer that question — the `AGENTS.md` block, the two
/// `SKILL.md`s, the hooks, the completions, and the seven MCP registrations —
/// and they must answer it in the same words. A sixth phrasing for one idea is a
/// small defect on its own, and the only reliable way to prevent one is for
/// there to be nothing to re-word.
pub const STALE: &str = "stale — re-run install to refresh";

/// What `status` found for a **named entry inside a config the user owns**:
/// [`json_entry`], [`toml_entry`] and [`yaml_block`] each answer with this.
///
/// ## Why this compares the entry, and never the file
///
/// Every `upsert` here already computes the text it would write and returns
/// [`Action::Unchanged`] when it matches the file — so "would install touch this
/// file?" is sitting right there, and it is the wrong question. It answers
/// **yes** for reasons that have nothing to do with our registration:
///
/// - `json_entry` reparses and re-emits at 2-space pretty, so a user whose
///   `.mcp.json` is indented with four spaces would be told `stale` on every run
///   forever — an alarm they cannot clear by doing what it asks;
/// - `toml_entry`'s entry carries whatever whitespace and quoting the
///   surrounding document uses, so a text comparison flags style, not content;
/// - `yaml_block`'s entry is indented to its container at splice time, while the
///   body an adapter produces is written at column 0 — as text those two are
///   never equal.
///
/// The question a user asks of `status` is "is filigrio's registration
/// current", so what is compared is the entry, by value wherever the format has
/// values. Each module's `state` says how it does that for its own format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    /// No entry of ours in this file.
    Absent,
    /// Our entry is there and says what install would write.
    Current,
    /// Our entry is there and says something else — a moved socket, a moved
    /// install directory, an argument the bridge has since gained.
    Stale,
}

/// One artifact's outcome.
#[derive(Debug, Clone)]
pub struct Step {
    pub target: String,
    pub path: PathBuf,
    pub action: Action,
    pub detail: String,
}

/// One artifact that could **not** be handled, with the reason. Never a silent
/// skip: this is the project's standing honest-failure posture.
#[derive(Debug, Clone)]
pub struct Failure {
    pub target: String,
    pub path: PathBuf,
    pub reason: String,
}

/// Why a note was recorded — the axis the default rendering folds on.
///
/// A **kind**, and never a pattern match over the note's own prose. Wiring every
/// agent at once records a dozen-odd notes, and all but one of them are the same
/// handful of sentences repeated once per adapter; aggregating them by looking
/// for substrings of `SCOPE_NOTE` / the registration-only sentence at render
/// time would work exactly until someone reworded one, and would then fail
/// *silently* — the report grows back to one line per adapter and nothing says
/// so. A kind is declared where the note is written, so a reworded sentence keeps
/// aggregating and a new adapter that forgets to declare one is loud (its note
/// prints in full).
///
/// The variants are the four claims the adapters actually make. Two of them are
/// about detection rather than one, because "Windsurf was not detected" is a
/// claim this crate cannot make — it never probes for Windsurf, on purpose (see
/// [`clients::windsurf`]) — and a line that is true of neither half of a merged
/// group is exactly the confidently-wrong output the rest of this crate is
/// built to avoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    /// The client was looked for and is not on this machine.
    NotDetected,
    /// The client leaves no trace this crate is willing to probe for.
    PresenceUnknown,
    /// The adapter wrote the MCP registration only; this agent's documentation
    /// is the repository's `AGENTS.md`, which is [`docs`]' artifact and not any
    /// agent's (ADR-0034 §18.2).
    RegistrationOnly,
    /// The vendor ships more than one agent reading more than one config
    /// layout, and this adapter wrote the one the vendor documents as current.
    ///
    /// A fifth kind rather than a sentence folded into one of the other four,
    /// because it is a claim none of them makes: the registration is *installed
    /// and correct* and there is still a configuration of the same product it
    /// does not reach. [`clients::windsurf`] is the only emitter today — Windsurf
    /// ships the legacy Cascade agent alongside the Devin Local agent that
    /// replaced it as the default — and it is a kind rather than an unkinded
    /// [`Note::Plain`] because it is reference for the user who is on the old
    /// agent, not an action for the majority who are not.
    VendorSplit,
    /// The registration went to a user-scoped file, so it does not travel with
    /// the repository.
    ///
    /// Three of these notes carry a second, client-specific fact — Codex's
    /// project-trust rule, Hermes' comment stripping, the 0600 credential
    /// mode. They stay *behind* `--explain`
    /// rather than earning short lines of their own, and the aggregate says so,
    /// because none of them is a thing to do: each is either a reason a scope
    /// was chosen or a description of something this crate already handled. The
    /// one credential fact that could ever require action — a `chmod` that
    /// failed — is not here at all; [`clients::openclaw`] and [`clients::hermes`]
    /// emit that as an unkinded [`Note::Plain`], which is never folded. Giving
    /// each caveat its own default line would also restore exactly what this
    /// aggregation removes: one line per adapter, growing with the next one.
    UserScoped,
}

impl NoteKind {
    /// Every kind, in the order the aggregate lines print. Iterated instead of
    /// collecting into a map, so the output order is a property of this list and
    /// not of a hash seed.
    pub const ALL: &'static [NoteKind] = &[
        NoteKind::NotDetected,
        NoteKind::PresenceUnknown,
        NoteKind::RegistrationOnly,
        NoteKind::VendorSplit,
        NoteKind::UserScoped,
    ];

    /// The one line that stands in for every note of this kind, naming the
    /// clients it covers.
    ///
    /// `subjects` is an `--agent` slug list rather than display names, because
    /// the question the aggregate has to keep answerable is "why did
    /// `--agent cursor` not write a doc?" — and the answer has to be reachable
    /// without the flag that prints the long version.
    fn summary(self, subjects: &str) -> String {
        match self {
            NoteKind::NotDetected => format!(
                "not detected on this machine: {subjects} — the artifacts were written anyway and \
                 are inert until the client arrives; `filigrio agent uninstall` removes them"
            ),
            NoteKind::PresenceUnknown => format!(
                "presence could not be determined: {subjects} — the only paths this crate verified \
                 for them are ones it writes itself, so a probe would find our own footprint"
            ),
            NoteKind::RegistrationOnly => format!(
                "registration only, no doc of their own: {subjects} — they read this repository's \
                 AGENTS.md, which `filigrio docs install` writes"
            ),
            NoteKind::VendorSplit => format!(
                "the vendor ships a second agent on a second config layout: {subjects} — the \
                 registration went to the layout the vendor documents as current, and \
                 `--explain` names the older file and who still reads it"
            ),
            // "registrations", not "artifacts": `opencode` writes a skill as
            // well and that one stays in the checkout at either scope
            // (ADR-0034 §18.1), so the wider word would make this line false of
            // the one adapter that has two.
            NoteKind::UserScoped => format!(
                "user-scoped registrations, so they do not travel with the repository and every \
                 teammate installs them: {subjects} — the per-client caveats (Codex's project-trust rule, \
                 Hermes' comment stripping, the 0600 credential mode) are reference, not action, \
                 and are in `--explain`"
            ),
        }
    }
}

/// Something the user should know that is not a failure.
///
/// The text is **never abridged at record time**: aggregation is a rendering
/// decision ([`Notes`]), so `--explain` can always print the sentence the
/// adapter actually wrote. Nothing here is thrown away.
#[derive(Debug, Clone)]
pub enum Note {
    /// Always printed in full, in every mode. This is the shape a note takes
    /// when it asks the user to **do** something — the zsh `fpath` line, a
    /// credential file we could not tighten, a hook that can never run — and it
    /// is the default for [`Report::note`] precisely so that a caller has to opt
    /// *in* to being foldable. Folding one of these into a count is the single
    /// outcome the aggregation must not produce.
    Plain { text: String },
    /// Foldable into one line per [`NoteKind`]. `subject` is the `--agent`
    /// slug the aggregate line names.
    Kinded {
        kind: NoteKind,
        subject: String,
        text: String,
    },
}

impl Note {
    /// The sentence as it was recorded — what `--explain` prints.
    pub fn text(&self) -> &str {
        match self {
            Note::Plain { text } | Note::Kinded { text, .. } => text,
        }
    }
}

/// How much of a run's note block a rendering prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notes {
    /// One line per [`NoteKind`] naming the clients it covers, then every
    /// [`Note::Plain`] in full, then a count of what was folded away.
    ///
    /// The default, and the measurement that made it the default: naming every
    /// agent in one `--agent` recorded **twenty** notes on the build this was
    /// written for, of which exactly one asked the user to do anything. The rest
    /// were a handful of sentences repeated once per adapter. A screen of
    /// reference material between the steps and the failures is how a `!` line
    /// scrolls past unread, and a failure nobody reads is the failure this
    /// crate's honest-failure posture exists to prevent.
    Aggregated,
    /// Every note verbatim, in the order recorded, with no aggregation and no
    /// footer — `filigrio agent … --explain`.
    Verbatim,
}

/// The outcome of a whole `install` / `uninstall` / `status` run.
#[derive(Debug, Default, Clone)]
pub struct Report {
    pub steps: Vec<Step>,
    pub failures: Vec<Failure>,
    /// Things the user should know that are not failures — "claude-code was not
    /// detected on this machine", "zsh completions need an `fpath` entry".
    pub notes: Vec<Note>,
}

impl Report {
    pub fn step(
        &mut self,
        target: impl Into<String>,
        path: impl AsRef<Path>,
        action: Action,
        detail: impl Into<String>,
    ) {
        self.steps.push(Step {
            target: target.into(),
            path: path.as_ref().to_path_buf(),
            action,
            detail: detail.into(),
        });
    }

    pub fn fail(
        &mut self,
        target: impl Into<String>,
        path: impl AsRef<Path>,
        reason: impl Into<String>,
    ) {
        self.failures.push(Failure {
            target: target.into(),
            path: path.as_ref().to_path_buf(),
            reason: reason.into(),
        });
    }

    /// Record a note that is **always shown at full detail**.
    ///
    /// The unkinded default is deliberate: a caller that has not thought about
    /// aggregation gets the safe answer, and the notes that reach a user through
    /// this door are the ones that ask them to act — [`completions::install`]'s
    /// zsh `fpath` line, [`hooks`]' unreachable-block warning, and the
    /// credential files [`clients::openclaw`] and [`clients::hermes`] could not
    /// tighten. Being foldable is opt-in ([`Report::note_kind`]).
    pub fn note(&mut self, note: impl Into<String>) {
        self.notes.push(Note::Plain { text: note.into() });
    }

    /// Record a note that the default rendering may fold into its kind's
    /// aggregate line, naming `subject` there.
    ///
    /// `subject` is the agent's `--agent` slug, not its display name: the
    /// aggregate has to keep "why did `--agent cursor` not write a doc?"
    /// answerable, and a line reading "Cursor" does not answer it as directly as
    /// one reading `cursor`. The full `text` is kept regardless — see [`Note`].
    pub fn note_kind(
        &mut self,
        kind: NoteKind,
        subject: impl Into<String>,
        note: impl Into<String>,
    ) {
        self.notes.push(Note::Kinded {
            kind,
            subject: subject.into(),
            text: note.into(),
        });
    }

    /// Record a fallible artifact write in one place: `Ok` becomes a step, `Err`
    /// becomes a failure carrying the path and the reason. Every writer funnels
    /// through this, which is why "unwritable path" cannot become a silent skip.
    pub fn record(
        &mut self,
        target: impl Into<String>,
        path: impl AsRef<Path>,
        outcome: Result<(Action, String), InstallError>,
    ) {
        match outcome {
            Ok((action, detail)) => self.step(target, path, action, detail),
            Err(e) => self.fail(target, path, e.to_string()),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// A human rendering: one line per artifact, then the notes at the requested
    /// density, then the failures — last and loud.
    ///
    /// `notes` is a parameter rather than a default because the note block is
    /// the only part of this rendering with a choice to make, and the choice is
    /// the user's (`--explain`). Every caller states which it wants.
    pub fn render(&self, notes: Notes) -> String {
        let mut out = String::new();
        for s in &self.steps {
            out.push_str(&format!(
                "  {} {:<26} {}{}\n",
                s.action.symbol(),
                s.target,
                s.path.display(),
                if s.detail.is_empty() {
                    String::new()
                } else {
                    format!("  ({})", s.detail)
                }
            ));
        }
        match notes {
            Notes::Verbatim => {
                for n in &self.notes {
                    out.push_str(&format!("  note: {}\n", n.text()));
                }
            }
            Notes::Aggregated => out.push_str(&self.aggregated_notes()),
        }
        for f in &self.failures {
            out.push_str(&format!(
                "  ! {:<26} {}\n      {}\n",
                f.target,
                f.path.display(),
                f.reason
            ));
        }
        out
    }

    /// The compressed note block: one line per kind that fired, then every
    /// unkinded note in the order it was recorded, then the count of what was
    /// folded away.
    ///
    /// The unkinded notes come **after** the aggregates on purpose: they are the
    /// ones that ask for action, and this puts them against the failure block —
    /// the loud end of the report — rather than buried above five lines of
    /// reference material.
    ///
    /// The footer counts *notes that no longer appear*, which is the kinded
    /// total minus the aggregate lines standing in for them, and it is omitted
    /// when that is zero. A run whose notes all fit — `--agent cursor`, say —
    /// must not be told to re-run with a flag that would show it the same thing.
    fn aggregated_notes(&self) -> String {
        let mut out = String::new();
        let mut kinded = 0usize;
        let mut lines = 0usize;

        for kind in NoteKind::ALL {
            let mut subjects: Vec<&str> = Vec::new();
            let mut count = 0usize;
            for n in &self.notes {
                if let Note::Kinded {
                    kind: k, subject, ..
                } = n
                {
                    if k == kind {
                        count += 1;
                        // One client can only be named once, however many notes
                        // of a kind it emitted.
                        if !subjects.contains(&subject.as_str()) {
                            subjects.push(subject);
                        }
                    }
                }
            }
            if count == 0 {
                continue;
            }
            out.push_str(&format!("  note: {}\n", kind.summary(&subjects.join(", "))));
            kinded += count;
            lines += 1;
        }

        for n in &self.notes {
            if let Note::Plain { text } = n {
                out.push_str(&format!("  note: {text}\n"));
            }
        }

        let suppressed = kinded - lines;
        if suppressed > 0 {
            out.push_str(&format!(
                "  note: {suppressed} further note{} folded into the lines above — re-run with \
                 `--explain` to read {} in full\n",
                if suppressed == 1 { "" } else { "s" },
                if suppressed == 1 { "it" } else { "every one" },
            ));
        }
        out
    }
}

/// Errors that name the artifact and the reason. Every variant carries enough
/// to print "which, and why" without the caller reconstructing context.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot create directory {path}: {source}")]
    Mkdir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not valid UTF-8, so its managed block cannot be edited safely")]
    NotUtf8 { path: PathBuf },

    #[error("{path} is not valid JSON ({source}); refusing to overwrite it")]
    BadJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("{path} holds a JSON {found}, not an object; refusing to overwrite it")]
    NotJsonObject { path: PathBuf, found: &'static str },

    /// Boxed because `TomlError` carries the offending source text and a span;
    /// unboxed it would make every `Result<_, InstallError>` in the crate pay
    /// for the one variant that needs it.
    #[error("{path} is not valid TOML ({source}); refusing to overwrite it")]
    BadToml {
        path: PathBuf,
        #[source]
        source: Box<toml_edit::TomlError>,
    },

    #[error("{path} holds `{key}` as {found}, not a table; refusing to overwrite it")]
    NotTomlTable {
        path: PathBuf,
        key: String,
        found: &'static str,
    },

    /// A YAML key written as an inline value (`mcp_servers: {}`) rather than a
    /// block mapping. Legal YAML, but splicing text into it would mean
    /// re-emitting the line — and a refusal is better than a rewrite.
    #[error(
        "{path} writes `{key}:` as an inline value rather than a block mapping; this installer \
         splices text and will not re-emit that line — convert it to a block mapping, or add the \
         entry by hand"
    )]
    YamlInlineMapping { path: PathBuf, key: String },

    #[error("{path} is not valid YAML ({why}); refusing to edit it")]
    BadYaml { path: PathBuf, why: String },

    /// More than one `---`-separated document. Loaders differ on which one they
    /// take, so choosing is guessing — and guessing wrong writes a registration
    /// into a document nothing reads.
    #[error(
        "{path} holds more than one YAML document; filigrio will not guess which one is the \
         config — add the entry by hand, or split the file"
    )]
    YamlMultiDocument { path: PathBuf },

    /// A symlink whose far end is inside a directory that is not there. See
    /// [`through_symlinks`] for why this is a refusal and not a `create_dir_all`
    /// — the short version is that inventing directories inside someone's
    /// dotfiles repo is a different act from creating the path we were asked to
    /// write.
    #[error(
        "{path} is a symlink to {target}, whose directory does not exist; filigrio creates the \
         directories it was asked to write to, not ones inside the tree your link points into — \
         create it, or repoint the link"
    )]
    SymlinkTargetDirMissing { path: PathBuf, target: PathBuf },

    /// A chain of symlinks that outlasts [`MAX_SYMLINK_HOPS`], which in practice
    /// means a loop. There is no file at the end of it to write to, so there is
    /// nothing to guess at.
    #[error(
        "{path} is a symlink chain still unresolved after {hops} hops, so it is a loop or deeper \
         than filigrio will follow; there is no file at the end of it to write — repoint the link"
    )]
    SymlinkLoop { path: PathBuf, hops: usize },

    #[error("template error: {0}")]
    Template(String),

    #[error("cannot determine {what}: {why}")]
    Environment { what: String, why: String },

    /// A file at one of our destinations that we did not write. Never
    /// overwritten and never deleted — reported, so the user decides.
    #[error("{path} was not written by filigrio ({why}); left untouched")]
    Foreign { path: PathBuf, why: String },

    #[error("{path} is not a git repository (no .git); run this from a checkout")]
    NotAGitRepo { path: PathBuf },
}

/// Read a UTF-8 file that may not exist. `Ok(None)` = absent; a decode failure
/// is an error, never an empty string — appending our block to a file we failed
/// to read would truncate it.
pub(crate) fn read_opt(path: &Path) -> Result<Option<String>, InstallError> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| InstallError::NotUtf8 {
                path: path.to_path_buf(),
            }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(InstallError::Read {
            path: path.to_path_buf(),
            source: e,
        }),
    }
}

/// Did the file we are about to rewrite end in a newline?
///
/// Found by the real-config exercise, not by reasoning: a real
/// `~/.config/opencode/opencode.json` ends without one, and a round trip that
/// adds it comes back one byte longer than it went in. "Byte-exact" has to mean
/// byte-exact, so the final newline is *the file's* property, not ours. A file
/// we are creating (or one that held nothing) gets one, POSIX-style.
///
/// One rule, two formats: `serde_json` never emits a final newline and
/// `toml_edit` always does once anything is inserted, so [`json_entry`] adds and
/// [`toml_entry`] both adds and strips — but *whether* to is this one answer,
/// and a second copy of it is a second place to get it wrong.
pub(crate) fn had_trailing_newline(before: Option<&str>) -> bool {
    match before {
        Some(text) if !text.trim().is_empty() => text.ends_with('\n'),
        _ => true,
    }
}

/// Write `content` to `path`, creating parents. Returns [`Action::Unchanged`]
/// when the bytes already match — the idempotent case must not touch mtime.
pub(crate) fn write_if_changed(path: &Path, content: &str) -> Result<Action, InstallError> {
    if let Some(existing) = read_opt(path)? {
        if existing == content {
            return Ok(Action::Unchanged);
        }
        write_all(path, content)?;
        return Ok(Action::Updated);
    }
    write_all(path, content)?;
    Ok(Action::Installed)
}

/// How many links a chain may have before we stop walking it. Eight is what
/// nobody's dotfiles need and every loop exceeds: `a -> b -> a` burns the budget
/// in four round trips, and the kernel's own `ELOOP` limit is forty.
const MAX_SYMLINK_HOPS: usize = 8;

/// Resolve `path` **through** any symlinks, to the file the user actually keeps.
///
/// # The rule for symlinks, both halves of it, in one place
///
/// Dotfile setups routinely symlink our destinations — `~/.codex/config.toml`,
/// `~/.hermes/config.yaml`, `AGENTS.md`, a completion file — into a git
/// checkout. Two things follow, and the second is the one that was missed:
///
/// - **A write lands on the file at the far end.** Renaming over the *link*
///   would replace it with a regular file and silently detach the user's config
///   from their dotfiles.
/// - **A removal does too — but it empties rather than unlinks.** Neither end of
///   a link is ours to delete: not the file, which lives in someone's dotfiles
///   repo, and not the link, which the user made and which nothing of ours
///   replaces. A removal is only ever reached when the file holds nothing but
///   ours, so what it writes *through* the link is nothing at all. Our bytes
///   go, the link stands, and the file the user's repo tracks is still tracked —
///   emptied, which shows up in `git status` as a change they can read and
///   revert, rather than deleted, which they would have to notice.
///
/// Deciding those two separately is what produced the defect this exists to
/// prevent: the write followed the link, the removal unlinked it, and an
/// uninstall of a symlinked `AGENTS.md` therefore destroyed the user's link
/// *and* left 3906 bytes of ours alive at the far end of it — invisible to
/// `status`, which then saw no file at all. Both halves are one decision, so
/// they are argued once, here, and every caller of either goes through this.
///
/// # Why `canonicalize` alone is not the resolution
///
/// It was, and it destroyed a link in the one case dotfiles make ordinary. A
/// user runs `ln -s ~/dotfiles/agents.md AGENTS.md` *before* the file exists —
/// which is the natural order, because the link is the setup step and the
/// content is what the installer is for. `canonicalize` fails on a path whose
/// last component does not resolve, the fallback handed back the path as given,
/// and the rename landed on the **link**: replaced by a regular file, the
/// dotfiles repo detached, and no error anywhere. The same shape as the bug
/// above — the write half deciding for itself what the removal half had already
/// decided — so it is fixed in the same place rather than beside it.
///
/// `canonicalize` stays the primary answer, because it resolves symlinked
/// *parent directories* too and a walk over the final component would miss
/// those. What follows it is a hand-walk for the case it cannot answer, and the
/// three ways a hand-walk goes wrong are each decided here:
///
/// - **A relative target resolves against the link's own directory**, never the
///   process cwd. `AGENTS.md -> ../dotfiles/agents.md` means the sibling of the
///   *link*; resolving it against wherever `filigrio` happened to be invoked
///   writes a file into the wrong tree and reports success. `Path::join`
///   returns an absolute target unchanged, so both spellings take one line.
/// - **Chains are followed, and bounded.** `canonicalize` walks a chain when it
///   succeeds; one `read_link` walks one hop, so a dangling `a -> b -> file`
///   would stop at `b`. The walk is therefore a loop, capped at
///   [`MAX_SYMLINK_HOPS`], and a chain that outlasts the cap — which in practice
///   means a loop — is [`InstallError::SymlinkLoop`]. Guessing which file the
///   user keeps is not available; there is no answer to guess at.
/// - **We do not create directories at the far end.** [`write_all`] does
///   `create_dir_all` on the *given* path's parent, and extending that to the
///   target would look consistent. It is not the same act. The given path is one
///   we or the user named — `~/.codex/`, `.claude/skills/filigrio/` — and
///   creating it is the install. The far end is a location the user chose inside
///   a tree we know nothing about, and a link pointing into a directory that
///   does not exist is far more likely to be a typo, a dotfiles repo that has
///   moved, or one not cloned on this machine than a request to `mkdir -p`
///   inside it. So that is [`InstallError::SymlinkTargetDirMissing`], naming
///   both ends: one command fixes it, where an invented `~/dotfiles/ai/` is a
///   directory tree the user has to notice before they can object to it.
fn through_symlinks(path: &Path) -> Result<PathBuf, InstallError> {
    // Every link in the chain resolves: this is the whole answer, parents
    // included, and the ordinary case for a config that already exists.
    if let Ok(real) = std::fs::canonicalize(path) {
        return Ok(real);
    }

    // It failed, which here means some component does not exist yet — either the
    // path itself (the create case) or the far end of a link (the bug above).
    let mut current = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        let Ok(link) = std::fs::read_link(&current) else {
            // Not a link. If we never followed one, `current` *is* `path` and a
            // path that does not exist yet is exactly right — that is a file we
            // are about to create. If we did follow one, this is the far end,
            // and it is only writable if its directory is already there.
            if current == path {
                return Ok(current);
            }
            let dir = current.parent().unwrap_or_else(|| Path::new("."));
            if !dir.is_dir() {
                return Err(InstallError::SymlinkTargetDirMissing {
                    path: path.to_path_buf(),
                    target: current,
                });
            }
            return Ok(current);
        };
        current = match current.parent() {
            // Relative to the link's directory; `join` passes an absolute
            // target through untouched. The result may keep a `..` in it, and
            // that is left alone deliberately: collapsing it lexically is wrong
            // the moment a symlinked directory is above it, and the kernel
            // resolves it correctly for both the temp file and the rename.
            Some(dir) => dir.join(link),
            None => link,
        };
        if let Ok(real) = std::fs::canonicalize(&current) {
            return Ok(real);
        }
    }

    Err(InstallError::SymlinkLoop {
        path: path.to_path_buf(),
        hops: MAX_SYMLINK_HOPS,
    })
}

/// Replace `path`'s contents **atomically**, creating parents.
///
/// The crate's whole contract is "we never damage a file the user owns", and a
/// plain `std::fs::write` is truncate-then-write: a crash, an `ENOSPC` or a
/// `kill -9` between the two leaves a *truncated* `~/.codex/config.toml`. So the
/// bytes land in a temp file next to the target, are `sync_all`'d, and the
/// target is `rename`d into place — a rename is atomic, so a reader sees the old
/// file or the new one, never half of either. The temp file is in the **same
/// directory** on purpose: `rename` across filesystems is `EXDEV`, and
/// `/tmp` is a different mount often enough to matter.
///
/// Two details the naive version gets wrong:
///
/// - **Symlinks.** See [`through_symlinks`], which both halves of the lifecycle
///   now go through and where the rule for both is argued.
/// - **Mode.** `std::fs::write` truncates in place and so preserves an existing
///   file's permissions; `rename` brings the temp file's own mode with it, and
///   `tempfile` creates at 0600. Both are handled below, because
///   [`clients::openclaw`] and [`clients::hermes`] depend on the first, and the
///   second would make every `.mcp.json` we created owner-only. A file we create
///   gets the **umask-derived** mode `std::fs::write` would have given it — not a
///   hard-coded one, which would override `umask 077` just as wrongly in the
///   other direction.
pub(crate) fn write_all(path: &Path, content: &str) -> Result<(), InstallError> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| InstallError::Mkdir {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    // Follow the symlink *before* choosing where the temp file lives and where
    // the rename lands, so both refer to the file the user actually keeps. It
    // can refuse — see [`through_symlinks`]; a link we cannot follow to a
    // writable file is a link we must not rename over.
    let target = through_symlinks(path)?;
    let dir = target.parent().unwrap_or_else(|| Path::new("."));

    // Errors keep naming the path the caller asked for, not the resolved one:
    // "cannot write ~/.codex/config.toml" is what the user recognises.
    let write_err = |source: std::io::Error| InstallError::Write {
        path: path.to_path_buf(),
        source,
    };

    // Read the mode before the rename, because after it there is nothing left to
    // read it from.
    let existing = std::fs::metadata(&target).ok().map(|m| m.permissions());

    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `tempfile` creates at 0600, and a rename carries that mode onto the
        // destination — so a file we created would come out owner-only. The
        // answer is *not* a hard-coded 0644 either: that ignores the process
        // umask in both directions, handing 0644 to someone whose `umask 077`
        // asked for 0600. Requesting 0666 makes the kernel apply the umask at
        // `open` time, which reproduces what `std::fs::write` gave a file it
        // created, exactly, at every umask — measured, not assumed.
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let mut tmp = builder.tempfile_in(dir).map_err(write_err)?;
    tmp.write_all(content.as_bytes()).map_err(write_err)?;

    // Their file, their mode — the same rule the credential-store adapters
    // state, held here so they do not each have to. A file we are creating keeps
    // the umask-derived mode chosen above.
    if let Some(perms) = existing {
        std::fs::set_permissions(tmp.path(), perms).map_err(write_err)?;
    }

    // After the mode, so the durable copy is the one we are about to publish.
    tmp.as_file().sync_all().map_err(write_err)?;
    tmp.persist(&target).map_err(|e| write_err(e.error))?;
    Ok(())
}

/// Remove the directories a removed artifact leaves behind, innermost first,
/// stopping at the first one that is not empty.
///
/// `std::fs::remove_dir` only succeeds on an empty directory, so a user's
/// sibling file is what stops the walk — no emptiness check to get wrong. The
/// walk is bounded at three levels and never passes `stop_at`, because
/// "uninstall deleted `~/.local/share`" is a far worse bug than a leftover
/// empty folder. Failures are ignored deliberately: a directory that will not
/// go is not a failed uninstall, and reporting it would be noise.
pub(crate) fn prune_empty_dirs(from: &Path, stop_at: &Path) {
    let mut dir = from;
    for _ in 0..3 {
        if dir == stop_at || !dir.starts_with(stop_at) || std::fs::remove_dir(dir).is_err() {
            return;
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return,
        }
    }
}

/// What a removal did. See [`remove_file`], and [`through_symlinks`] for why
/// there are two answers rather than one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Removal {
    /// The path was a regular file of ours, and it is gone.
    Unlinked,
    /// The path was the user's symlink. Our bytes were cleared *through* it; the
    /// link and the file at the far end are both still there.
    EmptiedThroughLink,
}

impl Removal {
    /// The detail a caller reports, given what the file held ("block", "entry",
    /// "completion", …).
    ///
    /// Spelled here rather than at the six call sites for the same reason
    /// [`STALE`] is: they all describe one outcome, and the outcome now has two
    /// shapes. Six copies of "file removed" would be six places to notice that
    /// on a symlinked destination the file is still there — which is exactly
    /// the sentence this crate was printing while the file was still there.
    pub(crate) fn detail(self, held: &str) -> String {
        match self {
            Removal::Unlinked => format!("file removed (held only our {held})"),
            Removal::EmptiedThroughLink => format!(
                "emptied through the symlink at this path (held only our {held}) — the link and \
                 the file it points at are the user's, so neither was deleted"
            ),
        }
    }
}

/// Remove our bytes from a file that may already be gone — absent is success,
/// not an error.
///
/// **Symmetric** with [`write_all`], which is the fix: both address the file at
/// the far end of a symlink, and neither destroys the link. A write puts content
/// there; a removal — which this crate only calls when the file holds nothing
/// but ours — puts *no* content there, leaving an empty file the user's dotfiles
/// repo goes on tracking. [`through_symlinks`] argues both
/// halves; the short version is that unlinking took the worst of both, deleting
/// the user's link while leaving our registration alive at the far end of it.
///
/// A plain file is still unlinked, because that file is ours and an empty husk
/// at `~/.local/share/…/filigrio` is a leftover nobody asked for.
///
/// A *dangling* link falls through to the unlink, which is right: there is no
/// file at the far end to preserve and nothing of ours can be in it. It is also
/// unreachable, because every caller has just read the file it is asking us to
/// remove.
pub(crate) fn remove_file(path: &Path) -> Result<Removal, InstallError> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink()) && path.exists() {
        write_all(path, "")?;
        return Ok(Removal::EmptiedThroughLink);
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(Removal::Unlinked),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Removal::Unlinked),
        Err(source) => Err(InstallError::Write {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_if_changed_is_idempotent_and_tells_you_which() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/b/c.txt");

        assert_eq!(write_if_changed(&p, "one").unwrap(), Action::Installed);
        assert_eq!(write_if_changed(&p, "one").unwrap(), Action::Unchanged);
        assert_eq!(write_if_changed(&p, "two").unwrap(), Action::Updated);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
    }

    /// `std::fs::write` truncated in place and so kept the mode; a rename does
    /// not. [`clients::openclaw`] and [`clients::hermes`] both promise "an
    /// existing file's mode is not ours to change" about files holding
    /// credentials, and this is where that promise actually lives.
    #[cfg(unix)]
    #[test]
    fn rewriting_an_existing_file_preserves_its_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("secrets.json");
        std::fs::write(&p, "old\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();

        write_all(&p, "new\n").unwrap();

        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "their mode survived the rename (got {mode:o})");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new\n");
    }

    /// The regression guard for the atomic rewrite, and the reason it asserts
    /// against a *reference file* rather than a literal mode.
    ///
    /// `tempfile` creates at 0600 and a rename carries that mode onto the
    /// destination, so a created `.mcp.json` would silently become owner-only —
    /// invisible until a teammate's client cannot read it. But hard-coding 0644
    /// is the same bug facing the other way: it hands 0644 to someone whose
    /// `umask 077` asked for 0600. The property that is actually correct is
    /// "whatever `std::fs::write` would have produced here", which is umask-
    /// derived, so the test writes a reference file in the same process and
    /// compares — and holds under any umask the suite happens to run with.
    #[cfg(unix)]
    #[test]
    fn a_file_we_create_gets_the_same_mode_std_fs_write_would_have_given_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let mode_of = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        let reference = dir.path().join("reference");
        std::fs::write(&reference, "{}\n").unwrap();

        let p = dir.path().join("nested/.mcp.json");
        write_all(&p, "{}\n").unwrap();

        assert_eq!(
            mode_of(&p),
            mode_of(&reference),
            "created files must follow the umask, not a literal: got {:o}, umask says {:o}",
            mode_of(&p),
            mode_of(&reference)
        );
        assert_ne!(
            mode_of(&p),
            0o600,
            "tempfile's 0600 must not ride the rename"
        );
    }

    /// Dotfiles setups symlink `~/.codex/config.toml` into a git repo. Renaming
    /// over the link would replace it with a regular file and detach the user's
    /// config from their dotfiles without a word, so the write resolves the link
    /// first and lands on the file at the far end of it.
    #[cfg(unix)]
    #[test]
    fn a_write_through_a_symlink_updates_the_target_and_keeps_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/config.toml");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "before\n").unwrap();

        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_all(&link, "after\n").unwrap();

        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "the link is still a link, not a regular file we dropped on top of it"
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "after\n");
    }

    /// **The reproduction for the write half.** `ln -s ~/dotfiles/agents.md
    /// AGENTS.md` before the file exists is the natural order — the link is the
    /// setup step and the content is what the installer is for — and it is the
    /// one case a plain `canonicalize` cannot answer, because a path whose last
    /// component does not resolve makes it fail. The fallback handed back the
    /// path as given, so the rename landed on the **link**: a regular file where
    /// the user's link had been, their dotfiles repo detached, and no error.
    ///
    /// The property is both halves at once, because either alone passes under
    /// the bug: the link is still a link, *and* the bytes are at the far end.
    #[cfg(unix)]
    #[test]
    fn a_write_through_a_dangling_symlink_fills_the_target_and_keeps_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        let target = dotfiles.join("agents.md");

        let link = dir.path().join("repo/AGENTS.md");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(
            !target.exists(),
            "the link dangles, which is the whole case"
        );

        write_all(&link, "## filigrio\n").unwrap();

        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "the user's link survived the first install"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "## filigrio\n");
    }

    /// A link target is relative to the **link's own directory**, not to
    /// wherever `filigrio` was invoked. `readlink` hands back the target
    /// verbatim, so joining it against the process cwd is a one-character
    /// mistake that writes a real file into an unrelated tree and reports
    /// success — the silent-wrong-place failure this crate rates worst.
    ///
    /// The fixture makes the two answers different on purpose: the link is two
    /// directories down and points back up through `..`, so a cwd-relative
    /// reading resolves somewhere that does not exist at all.
    #[cfg(unix)]
    #[test]
    fn a_relative_link_target_resolves_against_the_links_own_directory_not_the_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();

        let link = dir.path().join("repo/nested/AGENTS.md");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("../../dotfiles/agents.md", &link).unwrap();

        write_all(&link, "relative\n").unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(
            std::fs::read_to_string(dotfiles.join("agents.md")).unwrap(),
            "relative\n",
            "the target is the link's sibling-of-a-grandparent, not the cwd's"
        );
        assert!(
            !Path::new("../../dotfiles/agents.md").exists(),
            "a cwd-relative reading would have written outside the fixture"
        );
    }

    /// `canonicalize` walks a whole chain when it succeeds; one `read_link`
    /// walks exactly one hop. So the fallback has to be a loop, or a dangling
    /// `link -> link -> file` stops at the middle link and the rename destroys
    /// *that* one instead — the same defect one level in, which is the kind a
    /// fix for the single-hop case leaves behind.
    #[cfg(unix)]
    #[test]
    fn a_chain_of_dangling_links_is_followed_to_the_file_at_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        let target = dotfiles.join("agents.md");

        let middle = dir.path().join("mirror/AGENTS.md");
        std::fs::create_dir_all(middle.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &middle).unwrap();

        let link = dir.path().join("repo/AGENTS.md");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&middle, &link).unwrap();

        write_all(&link, "chained\n").unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert!(
            std::fs::symlink_metadata(&middle).unwrap().is_symlink(),
            "the middle link is as much the user's as the first one"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "chained\n");
    }

    /// A chain that outlasts the hop budget is a loop, and a loop has no file at
    /// the end to write to — so it is refused by name rather than resolved to
    /// whichever link the walk gave up on, which is a link we would then have
    /// renamed over. Both links are still links afterwards.
    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_is_refused_by_name_rather_than_written_over() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();

        let err = write_all(&a, "anything\n").unwrap_err();
        assert!(
            matches!(err, InstallError::SymlinkLoop { .. }),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("loop"),
            "the reason is told: {err}"
        );
        assert!(std::fs::symlink_metadata(&a).unwrap().is_symlink());
        assert!(std::fs::symlink_metadata(&b).unwrap().is_symlink());
    }

    /// [`write_all`] creates the *given* path's parent, and extending that to
    /// the far end of a link would look like consistency. It is a different act:
    /// the given path is one we or the user named, and creating it is the
    /// install; the far end is inside a tree we know nothing about, and a link
    /// pointing at a directory that is not there is much more likely a typo or a
    /// dotfiles repo that is not cloned on this machine than a request to
    /// `mkdir -p` inside it. So it is refused, naming both ends — and the
    /// directory is still absent afterwards, which is the half a refusal that
    /// only *reported* would fail.
    #[cfg(unix)]
    #[test]
    fn a_link_into_a_directory_that_does_not_exist_is_refused_rather_than_created() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles/ai/agents.md");

        let link = dir.path().join("repo/AGENTS.md");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = write_all(&link, "## filigrio\n").unwrap_err();
        assert!(
            matches!(err, InstallError::SymlinkTargetDirMissing { .. }),
            "got {err:?}"
        );
        let said = err.to_string();
        assert!(
            said.contains("AGENTS.md"),
            "the path the user knows: {said}"
        );
        assert!(said.contains("agents.md"), "and where it points: {said}");
        assert!(
            !dir.path().join("dotfiles").exists(),
            "nothing was invented inside their dotfiles tree"
        );
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    }

    /// **The reproduction.** `repo/AGENTS.md -> dotfiles/agents.md`, the target
    /// empty, is an ordinary dotfiles setup. Install wrote 3906 bytes through
    /// the link, correctly. Uninstall then took the worst of both halves: it
    /// unlinked the *link*, destroying something the user made and we never
    /// wrote, and left our 3906 bytes alive at the far end of it — in their
    /// dotfiles repo, still holding a `filigrio:start` block, and now invisible
    /// to `status`, which saw no file at all.
    ///
    /// The rule is [`through_symlinks`]': a removal addresses the same file a
    /// write does, and empties it rather than deleting either end. Which makes
    /// this the exact inverse of the install — the target was empty before and
    /// is empty after, byte for byte.
    #[cfg(unix)]
    #[test]
    fn an_uninstall_through_a_symlink_clears_our_bytes_and_leaves_the_link_standing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles/agents.md");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "").unwrap();

        let link = dir.path().join("repo/AGENTS.md");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        block::upsert_file(&link, "filigrio:start body", &block::MARKDOWN).unwrap();
        assert!(
            std::fs::read_to_string(&target).unwrap().contains("body"),
            "the install must go through the link, or this proves nothing"
        );

        let (action, detail) =
            block::remove_file_block(&link, &block::MARKDOWN, |rest| rest.trim().is_empty())
                .unwrap();

        assert_eq!(action, Action::Removed);
        assert!(
            std::fs::symlink_metadata(&link).is_ok_and(|m| m.is_symlink()),
            "the user's link was destroyed — it is not ours to delete"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "",
            "our block survived in their dotfiles, invisible to `status`"
        );
        assert!(
            !detail.contains("file removed"),
            "the file is still there; saying it was removed is the report lying: {detail}"
        );
        assert!(
            detail.contains("symlink"),
            "the detail must say what actually happened: {detail}"
        );
    }

    /// The half that was already right, pinned so the fix cannot cost it: a
    /// symlinked file with real prose in it round-trips byte-exactly and is
    /// never emptied — only the branch where the file held nothing but ours
    /// clears it.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_holding_the_users_prose_round_trips_byte_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let prose = "# My notes\n\nSomething I wrote.\n";
        let target = dir.path().join("dotfiles/agents.md");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, prose).unwrap();

        let link = dir.path().join("repo/AGENTS.md");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        block::upsert_file(&link, "ours", &block::MARKDOWN).unwrap();
        block::remove_file_block(&link, &block::MARKDOWN, |rest| rest.trim().is_empty()).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), prose);
    }

    /// The same hazard, one format over, because the fix is in the shared path
    /// and not in `AGENTS.md`'s adapter. `.mcp.json`, `~/.codex/config.toml`,
    /// `~/.openclaw/openclaw.json` and `~/.hermes/config.yaml` all reach
    /// [`remove_file`] through their own module's "held only our entry" branch,
    /// and all four are routinely dotfile-managed.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_json_config_is_emptied_through_the_link_like_every_other_format() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles/mcp.json");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "{}\n").unwrap();

        let link = dir.path().join("repo/.mcp.json");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let entry = serde_json::json!({"command": "/opt/g/bin/filigrio-mcp"});
        json_entry::upsert(&link, &["mcpServers"], "filigrio", entry).unwrap();
        let (action, detail) = json_entry::remove(&link, &["mcpServers"], "filigrio").unwrap();

        assert_eq!(action, Action::Removed);
        assert!(
            std::fs::symlink_metadata(&link).is_ok_and(|m| m.is_symlink()),
            "the link is the user's: {detail}"
        );
        assert!(
            !std::fs::read_to_string(&target)
                .unwrap()
                .contains("filigrio"),
            "our entry is still live in their dotfiles"
        );
    }

    /// A file that is really ours is still *deleted*. The symlink rule is about
    /// files the user keeps, and an empty husk at a destination we created is a
    /// leftover nobody asked for — the property
    /// `completions::tests::install_then_uninstall_leaves_the_tree_clean` reads
    /// as "the tree is clean".
    #[test]
    fn a_plain_file_of_ours_is_unlinked_not_emptied() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ours.txt");
        std::fs::write(&p, "ours\n").unwrap();

        assert_eq!(remove_file(&p).unwrap(), Removal::Unlinked);
        assert!(!p.exists(), "a file of ours must go, not linger empty");
    }

    /// The temp file is an implementation detail and must not outlive the write:
    /// a `.tmpXXXXXX` left next to a user's `AGENTS.md` is litter in a directory
    /// we were trusted with, and in a repository it shows up in `git status`.
    #[test]
    fn a_successful_write_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("AGENTS.md");

        write_all(&p, "one\n").unwrap();
        write_all(&p, "two\n").unwrap();

        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            left,
            vec![std::ffi::OsString::from("AGENTS.md")],
            "only the file we were asked to write"
        );
    }

    /// A non-UTF-8 file is an error, not an empty read. Treating it as empty
    /// would let an "append our block" path silently truncate a binary file.
    #[test]
    fn read_opt_refuses_non_utf8_rather_than_returning_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bin");
        std::fs::write(&p, [0xff, 0xfe, 0x00]).unwrap();
        assert!(matches!(read_opt(&p), Err(InstallError::NotUtf8 { .. })));
    }

    #[test]
    fn read_opt_absent_is_none_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_opt(&dir.path().join("nope")).unwrap().is_none());
    }

    #[test]
    fn prune_empty_dirs_stops_at_the_first_non_empty_directory() {
        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.path().join("a/keep.txt"), "mine\n").unwrap();

        prune_empty_dirs(&deep, root.path());

        assert!(!deep.exists(), "empty leaves go");
        assert!(!root.path().join("a/b").exists());
        assert!(root.path().join("a").exists(), "a/ holds the user's file");
        assert!(root.path().join("a/keep.txt").exists());
    }

    /// The bound that matters: the walk never removes — or passes — `stop_at`.
    /// "uninstall deleted ~/.local/share" must be unreachable.
    #[test]
    fn prune_empty_dirs_never_removes_or_escapes_the_stop_directory() {
        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("x/y");
        std::fs::create_dir_all(&deep).unwrap();

        prune_empty_dirs(&deep, root.path());
        assert!(root.path().exists(), "the stop directory must survive");

        // A path outside `stop_at` is refused outright.
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("z");
        std::fs::create_dir_all(&victim).unwrap();
        prune_empty_dirs(&victim, root.path());
        assert!(
            victim.exists(),
            "a path outside stop_at is not ours to touch"
        );
    }

    fn env_with_bin_dir(dir: &Path) -> Environment {
        Environment {
            project_root: dir.to_path_buf(),
            home: dir.to_path_buf(),
            cli_bin: dir.join("filigrio"),
            bridge_bin: dir.join("filigrio-mcp"),
            socket_path: PathBuf::from("/run/filigrio.sock"),
            version: "0.0.1".into(),
        }
    }

    /// The failure that used to be a `+`. An absolute path into thin air is the
    /// second half of "the tools are just missing" — the first half (a bare name
    /// that only resolves on a lucky `PATH`) was already closed by writing
    /// absolute paths, and this closes the rest.
    #[test]
    fn a_binary_the_artifacts_name_but_that_is_absent_is_a_failure_not_a_note() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("filigrio"), "#!/bin/sh\n").unwrap();
        // The bridge is deliberately not created: the partial-install shape.

        let mut r = Report::default();
        check_binaries(&env_with_bin_dir(dir.path()), true, &mut r);

        assert!(
            !r.is_ok(),
            "a registration to nothing must not be a success"
        );
        assert_eq!(r.failures.len(), 1, "only the bridge is missing");
        let f = &r.failures[0];
        assert_eq!(f.target, "binary/filigrio-mcp");
        assert!(
            f.reason.contains("filigrio-mcp") && f.reason.contains("install again"),
            "the reason must name the binary and the repair: {}",
            f.reason
        );
        assert!(
            r.notes.is_empty(),
            "a note would be lost among the client notes; this is a failure"
        );
    }

    /// The complete case stays silent — the check must not become noise on the
    /// run that is actually fine.
    #[test]
    fn a_complete_install_layout_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        for bin in ["filigrio", "filigrio-mcp"] {
            std::fs::write(dir.path().join(bin), "#!/bin/sh\n").unwrap();
        }
        let mut r = Report::default();
        check_binaries(&env_with_bin_dir(dir.path()), true, &mut r);
        assert!(r.is_ok() && r.notes.is_empty(), "{r:?}");
    }

    /// `filigrio hooks install` and `filigrio completions install` write
    /// nothing that names the bridge, so a missing bridge must not fail those
    /// runs — the check is scoped to what was written. Before ADR-0034 §17 the
    /// caller keyed this off `--target clients`; it now keys off the `agent`
    /// command, which is the same fact with no selector left to get wrong.
    #[test]
    fn a_run_that_writes_no_registration_does_not_require_the_bridge() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("filigrio"), "#!/bin/sh\n").unwrap();

        let mut r = Report::default();
        check_binaries(&env_with_bin_dir(dir.path()), false, &mut r);
        assert!(r.is_ok(), "the bridge is irrelevant here: {:?}", r.failures);
    }

    /// A directory where a binary should be is not a binary. `is_file` rather
    /// than `exists`, because `~/.local/bin/filigrio-mcp/` would pass the latter.
    #[test]
    fn a_directory_standing_where_the_binary_should_be_is_still_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("filigrio"), "#!/bin/sh\n").unwrap();
        std::fs::create_dir(dir.path().join("filigrio-mcp")).unwrap();

        let mut r = Report::default();
        check_binaries(&env_with_bin_dir(dir.path()), true, &mut r);
        assert_eq!(r.failures.len(), 1, "a directory is not the bridge");
    }

    /// An unwritable destination becomes a *failure with the path and the
    /// reason*, never a skipped step. This is the honest-failure contract.
    #[test]
    fn report_record_turns_an_io_error_into_a_named_failure() {
        let mut r = Report::default();
        r.record(
            "capability-doc",
            "/nope/AGENTS.md",
            Err(InstallError::Write {
                path: PathBuf::from("/nope/AGENTS.md"),
                source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            }),
        );
        assert!(!r.is_ok());
        assert_eq!(r.failures[0].target, "capability-doc");
        assert!(r.failures[0].reason.contains("/nope/AGENTS.md"));
        assert!(r.render(Notes::Aggregated).contains("capability-doc"));
    }

    // -----------------------------------------------------------------------
    // The note block: aggregation, and what it is forbidden to lose.
    // -----------------------------------------------------------------------

    /// The zsh line, standing in here for every unkinded note.
    const ZSH: &str = "zsh needs the directory on its fpath: add `fpath=(~/x $fpath)` before \
                       `compinit` in ~/.zshrc";

    /// A run shaped like a real one: several clients repeating the same few
    /// sentences, one note that asks for action, one artifact that failed.
    ///
    /// Six kinded notes across four kinds, so the arithmetic the footer has to
    /// get right (six folded into four lines = two that no longer appear) is not
    /// the same number as any other quantity in the fixture.
    fn a_run_with_notes() -> Report {
        let mut r = Report::default();
        r.step(
            "cursor/mcp",
            "/repo/.cursor/mcp.json",
            Action::Installed,
            "",
        );
        r.note_kind(
            NoteKind::NotDetected,
            "claude-code",
            "Claude Code was not detected …",
        );
        r.note_kind(NoteKind::NotDetected, "cursor", "Cursor was not detected …");
        r.note_kind(NoteKind::PresenceUnknown, "windsurf", "Windsurf presence …");
        r.note_kind(
            NoteKind::RegistrationOnly,
            "cursor",
            "Cursor's capability doc is AGENTS.md …",
        );
        r.note_kind(
            NoteKind::RegistrationOnly,
            "windsurf",
            "Windsurf's capability doc is AGENTS.md …",
        );
        r.note_kind(
            NoteKind::UserScoped,
            "windsurf",
            "Windsurf's registration went to ~/.config/devin/mcp_config.json …",
        );
        r.note(ZSH);
        r.fail(
            "hooks/post-commit",
            "/repo/.git/hooks/post-commit",
            "permission denied",
        );
        r
    }

    /// The `note:` lines of a rendering, in order.
    fn note_lines(text: &str) -> Vec<String> {
        text.lines()
            .filter_map(|l| l.trim_start().strip_prefix("note: "))
            .map(str::to_string)
            .collect()
    }

    /// The headline property: a default run says less, and every kind that fired
    /// still names the clients it covers.
    ///
    /// Naming them is what keeps the aggregate an *answer* rather than a
    /// summary — "why did `--agent windsurf` not write a doc?" has to be
    /// answerable without reaching for `--explain`, and the slug is the word the
    /// user typed.
    #[test]
    fn the_default_rendering_folds_each_kind_into_one_line_that_names_its_clients() {
        let r = a_run_with_notes();
        let default = note_lines(&r.render(Notes::Aggregated));
        let verbatim = note_lines(&r.render(Notes::Verbatim));

        assert_eq!(verbatim.len(), 7, "the fixture records seven notes");
        assert_eq!(
            default.len(),
            6,
            "four aggregates + the unkinded note + the footer, got:\n{}",
            default.join("\n")
        );

        let joined = default.join("\n");
        for (kind, subjects) in [
            ("not detected", vec!["claude-code", "cursor"]),
            ("presence could not be determined", vec!["windsurf"]),
            ("registration only", vec!["cursor", "windsurf"]),
            ("user-scoped", vec!["windsurf"]),
        ] {
            let line = default
                .iter()
                .find(|l| l.starts_with(kind))
                .unwrap_or_else(|| panic!("no `{kind}` aggregate in:\n{joined}"));
            for s in subjects {
                assert!(
                    line.contains(s),
                    "the `{kind}` line does not name `{s}`: {line}"
                );
            }
        }
    }

    /// A client that emitted two notes of one kind is named once, not twice.
    #[test]
    fn a_clients_slug_appears_once_per_aggregate_line_however_many_notes_it_wrote() {
        let mut r = Report::default();
        r.note_kind(NoteKind::NotDetected, "cursor", "one");
        r.note_kind(NoteKind::NotDetected, "cursor", "two");
        let line = note_lines(&r.render(Notes::Aggregated))
            .into_iter()
            .next()
            .expect("an aggregate line");
        assert_eq!(line.matches("cursor").count(), 1, "{line}");
    }

    /// The footer's number is *derived*, not asserted against a literal: it must
    /// equal the notes `--explain` prints minus the ones the default rendering
    /// still prints. Comparing the two renderings is the only formulation that
    /// fails when the count is computed from the wrong quantity — a literal
    /// would have to be edited into agreement with whatever the code did.
    #[test]
    fn the_suppressed_count_is_exactly_what_the_default_rendering_stopped_printing() {
        let r = a_run_with_notes();
        let default = note_lines(&r.render(Notes::Aggregated));
        let verbatim = note_lines(&r.render(Notes::Verbatim));

        let (footer, shown) = default.split_last().expect("a footer");
        let claimed: usize = footer
            .split_whitespace()
            .next()
            .and_then(|w| w.parse().ok())
            .unwrap_or_else(|| panic!("the footer must open with a count: {footer}"));

        assert_eq!(
            claimed,
            verbatim.len() - shown.len(),
            "footer claims {claimed}; --explain prints {} notes and the default prints {}",
            verbatim.len(),
            shown.len()
        );
        assert!(footer.contains("--explain"), "{footer}");
    }

    /// Nothing was folded, so there is nothing to advertise. Telling a
    /// `--agent cursor` run to re-run with `--explain` would promise it a
    /// longer answer that does not exist.
    #[test]
    fn the_footer_is_absent_when_the_aggregation_folded_nothing() {
        let mut r = Report::default();
        r.note_kind(NoteKind::NotDetected, "cursor", "Cursor was not detected …");
        r.note(ZSH);
        let out = r.render(Notes::Aggregated);
        assert!(
            !out.contains("--explain"),
            "one note of one kind folds nothing away:\n{out}"
        );
    }

    /// `--explain` is the promise that nothing was thrown away at record time:
    /// every note, in the order it was recorded, with its own words and no
    /// footer.
    #[test]
    fn explain_prints_every_note_verbatim_in_order_and_adds_no_footer() {
        let r = a_run_with_notes();
        let printed = note_lines(&r.render(Notes::Verbatim));
        let recorded: Vec<&str> = r.notes.iter().map(Note::text).collect();
        assert_eq!(printed, recorded);
        assert!(
            !printed.iter().any(|l| l.contains("--explain")),
            "the flag does not advertise itself: {printed:?}"
        );
    }

    /// The unkinded note is reproduced **whole**, in both modes. An aggregator
    /// that treated "no kind" as "fold it with the rest" would lose the only
    /// line in a default install that asks the user to act;
    /// `completions::tests::the_zsh_fpath_note_survives_at_full_detail_in_both_renderings`
    /// holds the same property over the real emitter.
    #[test]
    fn an_unkinded_note_is_printed_in_full_by_both_renderings() {
        let r = a_run_with_notes();
        for mode in [Notes::Aggregated, Notes::Verbatim] {
            let out = r.render(mode);
            assert!(
                out.contains(&format!("  note: {ZSH}\n")),
                "{mode:?} abridged an unkinded note:\n{out}"
            );
        }
    }

    /// The whole point of compressing the notes: a `!` line is not buried behind
    /// them. Pinned in both modes, because the reason for the change is worth
    /// nothing if the aggregation ever costs a failure its visibility.
    #[test]
    fn a_failure_stays_visible_with_its_path_and_reason_in_both_renderings() {
        let r = a_run_with_notes();
        for mode in [Notes::Aggregated, Notes::Verbatim] {
            let out = r.render(mode);
            let failure = out
                .lines()
                .position(|l| l.trim_start().starts_with("! hooks/post-commit"))
                .unwrap_or_else(|| panic!("{mode:?} lost the failure line:\n{out}"));
            assert!(
                out.lines()
                    .nth(failure + 1)
                    .is_some_and(|l| l.contains("permission denied")),
                "the reason must follow the failure:\n{out}"
            );
        }
    }
}
