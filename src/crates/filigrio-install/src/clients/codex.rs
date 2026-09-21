//! Codex (ADR-0034 §13, verified 2026-07-29 against OpenAI's own configuration
//! reference — `https://learn.chatgpt.com/docs/config-file/config-reference`,
//! reached via a 308 from `https://developers.openai.com/codex/config-reference`
//! — and cross-checked against a live `codex-cli 0.146.0`; the reference page
//! and the trust gate were re-verified 2026-08-08, see
//! `docs/vendor-path-verification.md`).
//!
//! One artifact, and it is **user-scoped**: `~/.codex/config.toml`, an
//! `[mcp_servers.filigrio]` table carrying `command` and `args`.
//!
//! ## The format is TOML, which is the whole difference
//!
//! Every other registration here is a JSON key ([`crate::json_entry`]). Codex's
//! config is TOML, and it is the file holding the user's model, provider and
//! approval settings with their comments around them. [`crate::toml_entry`]
//! edits it through `toml_edit`, which is format-preserving; a parse +
//! re-serialise would reformat their file on the first install and fail the
//! reversibility contract immediately.
//!
//! ## Why not `codex mcp add`, which unlike OpenCode's *is* scriptable
//!
//! `codex mcp add <NAME> -- <COMMAND>…` / `codex mcp remove <NAME>` take real
//! flags, so shelling out was a genuine option. It was tested and **rejected on
//! measured fidelity**: against a config with a comment on an existing
//! `[mcp_servers.*]` header and no final newline, `codex mcp add` followed by
//! `codex mcp remove` **deleted the comment and appended a newline** — the
//! second being the exact defect ADR-0034 §10 corrected in our own JSON writer.
//! Our round trip returns those bytes untouched, which makes writing the file
//! ourselves strictly better rather than merely equivalent, and it does not
//! require `codex` on `PATH` at install time.
//!
//! ## Scope: user, deliberately — see `SCOPE_NOTE`
//!
//! Codex reads `~/.codex/config.toml` always, and `.codex/config.toml` **only
//! when the user has trusted the project**. A project write is therefore
//! silently inert until an unrelated trust decision is made, which is the
//! "almost-worked" failure this project rates worse than an honest one. So the
//! registration goes to the user scope — the same place `codex mcp add`'s own
//! default puts it ("Added *global* MCP server") — and the cost, that it is
//! machine-local and does not travel with the repository, is stated on install
//! and on status exactly as [`super::openclaw`] states its own.
//!
//! ## No Codex-specific capability doc
//!
//! Codex reads `AGENTS.md` — globally from `~/.codex/AGENTS.md`, and per project
//! from the git root down to the working directory, closest file last — which
//! [`crate::docs`] writes at the repository root. Plain
//! markdown, no frontmatter required. A Codex-flavoured copy would be a second
//! copy of a doc we already ship, free to drift; so this is a
//! **registration-only** adapter and says so, §7's split again.
//!
//! ## Codex has a project skill directory, and it is not the one anybody guesses
//!
//! `https://learn.chatgpt.com/docs/build-skills`
//! (2026-08-08 sweep; `docs/vendor-path-verification.md`, `codex`) documents
//! three repo-relative discovery roots — `$CWD/.agents/skills`,
//! `$CWD/../.agents/skills` and `$REPO_ROOT/.agents/skills`, the last "the
//! topmost root folder when you launch Codex inside a Git repository" — above
//! which sit `$HOME/.agents/skills`, `/etc/codex/skills` and a bundled set. The
//! layout is the usual one: "a directory with a `SKILL.md` file", `name` and
//! `description` required.
//!
//! **`.codex/skills/` appears nowhere in OpenAI's documentation** — the string
//! `.codex` occurs on that page twice, both times as `~/.codex/config.toml`
//! under `[[skills.config]]`. It is nonetheless what the Python original's
//! `_PLATFORM_CONFIG` writes and what the `.codex/config.toml` above invites you
//! to infer, so it is the inviting wrong answer here and it would produce a
//! directory Codex never reads.
//!
//! **The position: `.agents/skills/` exists, we write no skill in it** — on
//! ADR-0034 §12's judgement grounds, the same ones [`super::cursor`] stands on,
//! not on an absence. `AGENTS.md` already reaches Codex at the same repository
//! root, and a second copy of one doc in one repository is free to drift from
//! the first. §18.1 defers the larger question — four vendors here document
//! `.agents/skills/<name>/SKILL.md` — and [`super`] has why.
//!
//! ### Open: whether the trust gate reaches `.agents/skills/` — UNVERIFIED
//!
//! The scope section below turns on that gate, so how far it extends is worth
//! knowing and we do not know. The reference wording is scoped to `.codex/` and
//! enumerates three layers — "project-local config, hooks, and rules" — with
//! skills in neither the list nor the directory, which reads as **not** gated;
//! but that is our inference from an enumeration, and the build-skills page
//! contains no occurrence of "trust", "trusted", "untrusted" or "approve"
//! anywhere in its source (grepped 2026-08-08). `approval_policy.granular.skill_approval`
//! governs *running* a skill's scripts, not discovering it, and settles nothing.
//! `docs/vendor-path-verification.md` carries the five-minute probe that would
//! close it; it needs a `codex` install and a never-trusted repository.

use super::{
    bridge_args, note_detection, report_registration, ClientId, ClientInstaller, Detection,
    OtherScope, Scope, ScopeSupport,
};
use crate::capability::SERVER_NAME;
use crate::{toml_entry, Environment, NoteKind, Report};
use std::path::PathBuf;
use toml_edit::{value, Array, Table};

pub struct Codex;

/// The TOML table header Codex documents, `[mcp_servers.<name>]`. Not
/// `mcpServers`: this is a different vendor's fact that happens to sit next to
/// three copies of a similar-looking string, so it is spelled out here rather
/// than borrowed from [`super::MCP_SERVERS`].
const MCP_SERVERS_TOML: &str = "mcp_servers";

/// Exactly the two fields we write. The reference documents `cwd`, `env`,
/// `env_vars`, `startup_timeout_sec`, `tool_timeout_sec`, `enabled`,
/// `required`, `default_tools_approval_mode`, `enabled_tools` and
/// `disabled_tools` as well — every one of them a user preference with a working
/// default. Setting any would decide something the user did not ask for and give
/// uninstall one more key to reason about. A test holds the entry to this list.
#[cfg(test)]
const ENTRY_KEYS: &[&str] = &["command", "args"];

/// The scope decision, said on install *and* status. Both halves matter: what we
/// did, and what we deliberately did not do and why.
const SCOPE_NOTE: &str =
    "Codex's registration is written to ~/.codex/config.toml (user scope) — it is machine-local \
     and does not travel with the repository, so every teammate installs it themselves. The \
     project-scoped .codex/config.toml was not used deliberately: Codex loads that file only when \
     you have trusted the project, so a registration written there can sit on disk being ignored, \
     and a file existing is not the same as a file being read";

/// Said on install *and* on status, as in [`super::cursor`].
fn doc_note() -> String {
    super::registration_only_note(
        "Codex",
        "Codex reads AGENTS.md from the git root down to your working directory, and \
         ~/.codex/AGENTS.md globally — that second one is Codex's own fact, not this \
         convention's, and nothing here writes it",
    )
}

impl Codex {
    fn config_toml(env: &Environment) -> PathBuf {
        env.home.join(".codex/config.toml")
    }

    /// `command` + `args`, and nothing else — the shape the reference documents
    /// for a local/stdio server and the shape `codex mcp add` itself writes.
    fn registration(env: &Environment) -> Table {
        let mut table = Table::new();
        table["command"] = value(env.bridge_bin.display().to_string());
        table["args"] = value(bridge_args(env).into_iter().collect::<Array>());
        table
    }
}

impl ClientInstaller for Codex {
    fn id(&self) -> ClientId {
        ClientId::Codex
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    /// §17.1's fourth row, and the *opposite* refusal from
    /// [`super::claude_code`]'s: the project file exists and Codex reads it only
    /// in a trusted project, so a registration written there can sit on disk
    /// being ignored. `SCOPE_NOTE` already carries that sentence.
    fn scope_support(&self) -> ScopeSupport {
        ScopeSupport {
            default: Scope::Global,
            other: OtherScope::Refused(SCOPE_NOTE),
        }
    }

    fn detect(&self, _env: &Environment) -> Detection {
        // `~/.codex` is where our own config.toml goes, so probing for it — or
        // for the file itself — would make the first install teach every later
        // run to report "detected". `super::openclaw` documents the same trap.
        // The binary's name and location are not a fact this crate verified on
        // the user's machine, only on the author's, so they are not a probe
        // either.
        Detection::Unknown(
            "the only verified Codex paths are ~/.codex/config.toml, which this adapter writes \
             itself, and ~/.codex/AGENTS.md — probing either would only find our own footprint"
                .into(),
        )
    }

    fn install(&self, env: &Environment, _scope: Scope, report: &mut Report) {
        note_detection(self.id(), self.display_name(), self.detect(env), report);
        report.note_kind(NoteKind::UserScoped, self.id().slug(), SCOPE_NOTE);
        report.note_kind(NoteKind::RegistrationOnly, self.id().slug(), doc_note());

        let cfg = Self::config_toml(env);
        report.record(
            "codex/mcp",
            &cfg,
            toml_entry::upsert(&cfg, MCP_SERVERS_TOML, SERVER_NAME, Self::registration(env)),
        );
    }

    fn uninstall(&self, env: &Environment, _scope: Scope, report: &mut Report) {
        let cfg = Self::config_toml(env);
        report.record(
            "codex/mcp",
            &cfg,
            toml_entry::remove(&cfg, MCP_SERVERS_TOML, SERVER_NAME),
        );
        // Bounded at `$HOME`, and `~/.codex` only goes if it is empty — a real
        // Codex install keeps sessions and auth in there and the walk stops at
        // the first of them.
        if let Some(dir) = cfg.parent() {
            crate::prune_empty_dirs(dir, &env.home);
        }
    }

    fn status(&self, env: &Environment, _scope: Scope, report: &mut Report) {
        note_detection(self.id(), self.display_name(), self.detect(env), report);
        report.note_kind(NoteKind::UserScoped, self.id().slug(), SCOPE_NOTE);
        report.note_kind(NoteKind::RegistrationOnly, self.id().slug(), doc_note());

        let cfg = Self::config_toml(env);
        report_registration(
            "codex/mcp",
            &cfg,
            toml_entry::state(
                &cfg,
                MCP_SERVERS_TOML,
                SERVER_NAME,
                &Self::registration(env),
            ),
            report,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, STALE};

    fn env(root: &std::path::Path, home: &std::path::Path) -> Environment {
        Environment {
            project_root: root.to_path_buf(),
            home: home.to_path_buf(),
            cli_bin: PathBuf::from("/opt/g/bin/filigrio"),
            bridge_bin: PathBuf::from("/opt/g/bin/filigrio-mcp"),
            socket_path: PathBuf::from("/run/filigrio.sock"),
            version: "0.0.1".into(),
        }
    }

    #[test]
    fn install_writes_the_documented_table_at_the_user_scope_path() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let mut r = Report::default();
        Codex.install(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        let text = std::fs::read_to_string(home.path().join(".codex/config.toml")).unwrap();
        assert!(
            text.contains("[mcp_servers.filigrio]"),
            "the documented header, got:\n{text}"
        );
        assert!(
            !text.contains("mcpServers"),
            "`mcpServers` here means a JSON adapter's key leaked in:\n{text}"
        );
        let doc: toml_edit::DocumentMut = text.parse().unwrap();
        let entry = doc["mcp_servers"]["filigrio"].as_table().unwrap();
        assert_eq!(
            entry["command"].as_str().unwrap(),
            "/opt/g/bin/filigrio-mcp"
        );
        let args: Vec<&str> = entry["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(args, ["--socket", "/run/filigrio.sock"]);

        // Nothing landed in the repository — this adapter has no project scope.
        assert!(!d.path().join(".codex").exists());
    }

    /// The vendor documents ten more fields, all of them optional preferences.
    /// A `type` or an `environment` must not drift in from a neighbouring
    /// adapter, and neither must a `startup_timeout_sec` we decided for them.
    #[test]
    fn the_entry_carries_exactly_the_two_keys_we_chose_to_write() {
        let d = tempfile::tempdir().unwrap();
        let t = Codex::registration(&env(d.path(), d.path()));
        let keys: Vec<&str> = t.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ENTRY_KEYS, "got {t}");
    }

    /// The gap this adapter must never let a user discover on their own — both
    /// what the scope is, and that the trust-gated project file was refused.
    #[test]
    fn both_install_and_status_state_the_scope_and_the_trust_caveat() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());

        for run in [
            {
                let mut r = Report::default();
                Codex.install(&e, Scope::Global, &mut r);
                r
            },
            {
                let mut r = Report::default();
                Codex.status(&e, Scope::Global, &mut r);
                r
            },
        ] {
            assert!(
                run.notes
                    .iter()
                    .any(|n| n.text().contains("~/.codex/config.toml")
                        && n.text().contains("does not travel")
                        && n.text().contains("trusted the project")),
                "notes were {:?}",
                run.notes
            );
            assert!(
                run.notes.iter().any(|n| n.text().contains("AGENTS.md")
                    && n.text().contains("`filigrio docs install`")),
                "notes were {:?}",
                run.notes
            );
        }
    }

    /// Detection must not report our own footprint back to us.
    #[test]
    fn detection_stays_unknown_even_after_an_install() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        Codex.install(&e, Scope::Global, &mut Report::default());
        assert!(matches!(Codex.detect(&e), Detection::Unknown(_)));
    }

    #[test]
    fn install_is_idempotent_and_uninstall_leaves_no_trace_in_home() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());

        let mut r = Report::default();
        Codex.install(&e, Scope::Global, &mut r);
        Codex.install(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        assert!(r.steps.iter().any(|s| s.action == Action::Unchanged));

        let mut r = Report::default();
        Codex.uninstall(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        assert!(!home.path().join(".codex").exists(), "our dir goes too");
        assert!(home.path().exists(), "$HOME survives, obviously");
    }

    /// A real `~/.codex` holds sessions and auth; the prune must stop there.
    #[test]
    fn uninstall_keeps_a_codex_directory_codex_is_using() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        Codex.install(&e, Scope::Global, &mut Report::default());
        let theirs = home.path().join(".codex/auth.json");
        std::fs::write(&theirs, "{}\n").unwrap();

        Codex.uninstall(&e, Scope::Global, &mut Report::default());
        assert!(theirs.exists(), "Codex's own state must survive");
    }

    /// The headline promise, on the fixture shape `codex mcp add` itself fails:
    /// a comment above, a pre-existing `[mcp_servers.*]` whose header carries a
    /// comment, a comment below, and no trailing newline. This is a user's
    /// primary Codex config — there is no `git checkout` to undo a mistake here.
    #[test]
    fn a_users_real_shaped_config_round_trips_byte_exactly() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let p = home.path().join(".codex/config.toml");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let prior = concat!(
            "# my codex config — do not let an installer eat this\n",
            "model = \"o3\"\n",
            "approval_policy = \"on-request\"\n",
            "\n",
            "# an existing server I care about\n",
            "[mcp_servers.sentry]\n",
            "command = \"npx\"\n",
            "args = [\"-y\", \"@sentry/mcp\"]\n",
            "\n",
            "# and a trailing note about the TUI\n",
            "[tui]\n",
            "notifications = true"
        );
        assert!(!prior.ends_with('\n'), "the fixture's whole point");
        std::fs::write(&p, prior).unwrap();

        Codex.install(&e, Scope::Global, &mut Report::default());
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("[mcp_servers.filigrio]"));
        assert!(after.contains("[mcp_servers.sentry]"), "their server stays");
        assert!(
            after.contains("# an existing server I care about"),
            "the header comment `codex mcp add` deletes:\n{after}"
        );
        assert!(after.contains("model = \"o3\""));
        assert!(!after.ends_with('\n'), "no newline we invented:\n{after}");

        Codex.uninstall(&e, Scope::Global, &mut Report::default());
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            prior,
            "a user's real config must come back byte for byte"
        );
    }

    /// The same exercise against a *real* config, opt-in because it needs one:
    /// `FILIGRIO_REAL_CODEX_CONFIG=~/.codex/config.toml cargo test`. It copies
    /// the file to a scratch directory first — the real one is never opened for
    /// writing.
    #[test]
    fn a_real_codex_config_round_trips_when_one_is_pointed_at() {
        let Some(src) = std::env::var_os("FILIGRIO_REAL_CODEX_CONFIG") else {
            return;
        };
        let prior = std::fs::read_to_string(&src)
            .unwrap_or_else(|e| panic!("FILIGRIO_REAL_CODEX_CONFIG is unreadable: {e}"));

        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let p = home.path().join(".codex/config.toml");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, &prior).unwrap();

        let mut r = Report::default();
        Codex.install(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        Codex.uninstall(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            prior,
            "a real config did not come back byte-exact"
        );
    }

    /// A config we cannot parse is refused with its path, not rewritten.
    #[test]
    fn a_malformed_config_is_a_reported_failure_and_the_file_survives() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let p = home.path().join(".codex/config.toml");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let broken = "model = \"o3\"\n[mcp_servers\ncommand = \"x\"\n";
        std::fs::write(&p, broken).unwrap();

        let mut r = Report::default();
        Codex.install(&e, Scope::Global, &mut r);
        assert!(!r.is_ok(), "a config we cannot parse is a reported failure");
        assert!(r.failures[0].reason.contains("config.toml"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), broken);
    }

    /// The three states a user reads, and the middle one is the whole point:
    /// a registration whose socket has moved is `Present` either way, so only
    /// the detail tells them apart. `status` said `registered` for both until
    /// this test existed.
    #[test]
    fn status_distinguishes_absent_current_and_stale() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let mut r = Report::default();
        Codex.status(&e, Scope::Global, &mut r);
        assert_eq!(r.steps[0].action, Action::Absent);
        assert_eq!(r.steps[0].detail, "not registered");

        Codex.install(&e, Scope::Global, &mut Report::default());
        let mut r = Report::default();
        Codex.status(&e, Scope::Global, &mut r);
        assert_eq!(r.steps[0].action, Action::Present);
        assert_eq!(r.steps[0].detail, "current");

        // The user re-ran with a different `--socket`. Nothing on disk changed;
        // what install *would* write did.
        let moved = Environment {
            socket_path: PathBuf::from("/run/user/1000/filigrio.sock"),
            ..e.clone()
        };
        let mut r = Report::default();
        Codex.status(&moved, Scope::Global, &mut r);
        assert_eq!(r.steps[0].action, Action::Present);
        assert_eq!(r.steps[0].detail, STALE);

        // And the fix the message names actually works.
        Codex.install(&moved, Scope::Global, &mut Report::default());
        let mut r = Report::default();
        Codex.status(&moved, Scope::Global, &mut r);
        assert_eq!(r.steps[0].detail, "current");
    }
}
