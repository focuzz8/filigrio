//! OpenClaw (ADR-0034 §14, verified 2026-07-29 against the **live binary** —
//! `OpenClaw 2026.7.1-2 (0790d9f)`, exercised against a scratch config via
//! `OPENCLAW_CONFIG_PATH`; the real `~/.openclaw/openclaw.json` was never
//! written).
//!
//! One artifact, and it is **user-scoped**: `~/.openclaw/openclaw.json`, an
//! entry at **`mcp.servers.filigrio`**.
//!
//! ## The one structural difference: the container is two levels deep
//!
//! Every other JSON registration here nests once — `mcpServers.<name>` for
//! Claude Code, Cursor and Windsurf, `mcp.<name>` for OpenCode. OpenClaw nests
//! **twice**: `mcp` → `servers` → `<name>`. That is a silent failure like
//! OpenCode's four (the server is simply absent), and it is why
//! [`crate::json_entry`] now takes a container *path* rather than a container
//! key. One module with a depth parameter, not a second copy of it — and the
//! path is what uninstall walks back up, pruning each container it created
//! innermost-first so no `{"mcp": {"servers": {}}}` husk is left behind.
//!
//! ## Why not `openclaw mcp add`, which is fully scriptable
//!
//! `openclaw mcp add <name> --command … --arg … --no-probe` / `openclaw mcp
//! unset <name>` take real flags, so shelling out was a genuine option — and it
//! is the **second** vendor CLI in two adapters to fail the measurement:
//!
//! 1. it **reformats the whole file**, expanding compact objects the user wrote
//!    on one line, and
//! 2. it injects a `meta` block with `lastTouchedVersion` / `lastTouchedAt`
//!    which **survives the removal** — worse, `unset` *refreshes* the
//!    timestamp on its way out.
//!
//! So an `add`/`unset` round trip can never return the original bytes, **by
//! construction**. Codex's CLI failed the same test by deleting a comment and
//! appending a newline. Both vendors' own tools are lossier than a keyed merge;
//! that is now a measured pattern rather than a preference (ADR-0034 §14).
//!
//! ## Two of its behaviours we match, and one we do not
//!
//! - **Refuse rather than rewrite.** OpenClaw's config schema is strict — an
//!   unrecognised root key is rejected by name and the write refused, leaving
//!   the file untouched. A config we cannot parse gets the same treatment
//!   ([`crate::InstallError::BadJson`], naming the file and the position).
//! - **No third backup scheme.** OpenClaw rotates its own `openclaw.json.bak` /
//!   `.bak.1` beside the config. The keyed entry *is* our reversibility
//!   mechanism; adding backups of our own would be a second answer to a question
//!   already answered twice.
//! - **We do not write its skills.** OpenClaw documents a skills system and
//!   **none of its roots is a repository path** — argued below. It reads
//!   `AGENTS.md` natively, which [`crate::docs`] writes. Registration-only,
//!   the fourth of that shape.
//!
//! ## No OpenClaw skill: no root of its skills system is a repository path
//!
//! `https://docs.openclaw.ai/tools/skills` (2026-08-08) ranks
//! `<workspace>/skills` and `<workspace>/.agents/skills` first — but
//! `<workspace>` resolves to the *agent's* home (`~/.openclaw/workspace` by
//! default), relocated only by machine-level settings, so both are `$HOME`
//! paths spelled relatively, and the remaining roots (`~/.agents/skills`,
//! `<state-dir>/skills`) do not pretend otherwise. The absence is a vendor
//! fact, not a judgement like [`super::cursor`]'s; the page quotes are in
//! `docs/vendor-path-verification.md` (`openclaw`, 2026-08-08).
//!
//! ## The file holds credentials, so a file *we* create is 0600
//!
//! The vendor writes this config mode **0600** — measured, and unsurprising for
//! a file holding API credentials. Our shared writer uses `std::fs::write`,
//! which keeps an existing file's mode but creates a new one at the umask
//! default (0664 on the machine this was written on). So when — and only when —
//! this adapter *creates* the config, it tightens it to owner-only. An existing
//! file's mode is the user's and is never touched. See
//! [`super::restrict_to_owner`], shared with [`super::hermes`].

use super::{
    bridge_args, note_detection, report_registration, ClientId, ClientInstaller, Detection,
    OtherScope, Scope, ScopeSupport,
};
use crate::capability::SERVER_NAME;
use crate::{json_entry, Environment, NoteKind, Report};
use serde_json::json;
use std::path::PathBuf;

pub struct OpenClaw;

/// **`mcp` → `servers`**, two levels. The trap: every neighbouring adapter is
/// one level, and a registration at the wrong depth fails silently.
const MCP_PATH: &[&str] = &["mcp", "servers"];

/// Exactly the two fields we write. `openclaw mcp add --help` also documents
/// `cwd`, `env`, `connect-timeout`, `timeout`, `include`/`exclude`, `disabled`
/// and `parallel` (plus HTTP/OAuth fields that do not apply to a stdio server) —
/// every one a user preference with a working default. A test holds the entry to
/// this list by equality, so a `type` or an `environment` cannot drift in from
/// the adapters next door.
#[cfg(test)]
const ENTRY_KEYS: &[&str] = &["command", "args"];

/// Said on install *and* status: OpenClaw documents no project-scoped config and
/// `openclaw mcp add` has no scope flag, so this is machine-local — the same gap
/// [`super::hermes`] has, stated the same way.
const SCOPE_NOTE: &str =
    "OpenClaw's config is global only (~/.openclaw/openclaw.json) — it documents no \
     project-scoped file and `openclaw mcp add` has no scope flag, so this registration is \
     machine-local and does not travel with the repository; every teammate installs it \
     themselves. That file also holds credentials and is owner-only (0600): if this install \
     created it we matched that mode, and if it already existed we left its mode alone";

/// Said on install *and* on status, as in [`super::cursor`]. The skills clause
/// is a fact about this vendor a user cannot recover on their own: OpenClaw has
/// a skills system, no root of it is a repository path (module header), and
/// nothing of ours goes in it.
fn doc_note() -> String {
    super::registration_only_note(
        "OpenClaw",
        "OpenClaw reads it natively; its skill roots sit under <workspace>, which defaults to \
         ~/.openclaw/workspace and is a machine-level setting rather than the checkout, so a \
         repository cannot supply a skill and nothing is written there",
    )
}

impl OpenClaw {
    fn config_json(env: &Environment) -> PathBuf {
        env.home.join(".openclaw/openclaw.json")
    }

    /// The shape `openclaw mcp add` writes for a stdio server.
    fn registration(env: &Environment) -> serde_json::Value {
        json!({
            "command": env.bridge_bin.display().to_string(),
            "args": bridge_args(env),
        })
    }
}

impl ClientInstaller for OpenClaw {
    fn id(&self) -> ClientId {
        ClientId::OpenClaw
    }

    fn display_name(&self) -> &'static str {
        "OpenClaw"
    }

    /// §17.1's third row, as [`super::hermes`]: no project-scoped file exists,
    /// and `SCOPE_NOTE` is the sentence that says so.
    fn scope_support(&self) -> ScopeSupport {
        ScopeSupport {
            default: Scope::Global,
            other: OtherScope::NotOffered(SCOPE_NOTE),
        }
    }

    fn detect(&self, _env: &Environment) -> Detection {
        // `~/.openclaw` is where our own config goes, so probing it — or the
        // file itself — would make the first install teach every later run to
        // report "detected". `super::hermes` and `super::codex` document the
        // same trap.
        Detection::Unknown(
            "the only verified OpenClaw path is ~/.openclaw/openclaw.json, which this adapter \
             writes itself, so probing for it would only find our own footprint"
                .into(),
        )
    }

    fn install(&self, env: &Environment, _scope: Scope, report: &mut Report) {
        note_detection(self.id(), self.display_name(), self.detect(env), report);
        report.note_kind(NoteKind::UserScoped, self.id().slug(), SCOPE_NOTE);
        report.note_kind(NoteKind::RegistrationOnly, self.id().slug(), doc_note());

        let cfg = Self::config_json(env);
        // Asked *before* the write: afterwards every file exists, and the
        // difference between "we made this" and "the user had one" is the whole
        // basis for touching the mode at all.
        let existed = cfg.exists();
        let outcome = json_entry::upsert(&cfg, MCP_PATH, SERVER_NAME, Self::registration(env));
        if !existed && outcome.is_ok() {
            super::restrict_to_owner(&cfg, "OpenClaw", report);
        }
        report.record("openclaw/mcp", &cfg, outcome);
    }

    fn uninstall(&self, env: &Environment, _scope: Scope, report: &mut Report) {
        let cfg = Self::config_json(env);
        report.record(
            "openclaw/mcp",
            &cfg,
            json_entry::remove(&cfg, MCP_PATH, SERVER_NAME),
        );
        // Bounded at `$HOME`, and `~/.openclaw` only goes if it is empty — a
        // real install keeps its `.bak` rotation and plugin skills in there and
        // the walk stops at the first of them.
        if let Some(dir) = cfg.parent() {
            crate::prune_empty_dirs(dir, &env.home);
        }
    }

    fn status(&self, env: &Environment, _scope: Scope, report: &mut Report) {
        note_detection(self.id(), self.display_name(), self.detect(env), report);
        report.note_kind(NoteKind::UserScoped, self.id().slug(), SCOPE_NOTE);
        report.note_kind(NoteKind::RegistrationOnly, self.id().slug(), doc_note());

        let cfg = Self::config_json(env);
        report_registration(
            "openclaw/mcp",
            &cfg,
            json_entry::state(&cfg, MCP_PATH, SERVER_NAME, &Self::registration(env)),
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

    /// The depth is the whole trap: a one-level `mcpServers.filigrio` here is a
    /// server OpenClaw never sees, with no error to say so.
    #[test]
    fn install_writes_the_entry_two_levels_deep_not_one() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let mut r = Report::default();
        OpenClaw.install(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        let text = std::fs::read_to_string(home.path().join(".openclaw/openclaw.json")).unwrap();
        assert!(
            !text.contains("mcpServers"),
            "`mcpServers` here means a flat adapter's shape leaked in:\n{text}"
        );
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let entry = &v["mcp"]["servers"]["filigrio"];
        assert_eq!(entry["command"], "/opt/g/bin/filigrio-mcp");
        assert_eq!(entry["args"][0], "--socket");
        assert_eq!(entry["args"][1], "/run/filigrio.sock");

        // Nothing landed in the repository — this client has no project scope.
        assert!(!d.path().join(".openclaw").exists());
    }

    /// `openclaw mcp add --help` documents nine more fields, all optional. A
    /// `type` or an `environment` must not drift in from a neighbouring adapter.
    #[test]
    fn the_entry_carries_exactly_the_two_keys_we_chose_to_write() {
        let d = tempfile::tempdir().unwrap();
        let v = OpenClaw::registration(&env(d.path(), d.path()));
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ENTRY_KEYS, "got {v}");
    }

    #[test]
    fn both_install_and_status_state_the_scope_and_name_the_doc_owner() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());

        for run in [
            {
                let mut r = Report::default();
                OpenClaw.install(&e, Scope::Global, &mut r);
                r
            },
            {
                let mut r = Report::default();
                OpenClaw.status(&e, Scope::Global, &mut r);
                r
            },
        ] {
            assert!(
                run.notes
                    .iter()
                    .any(|n| n.text().contains("global only")
                        && n.text().contains("does not travel")),
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

    #[test]
    fn detection_stays_unknown_even_after_an_install() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        OpenClaw.install(&e, Scope::Global, &mut Report::default());
        assert!(matches!(OpenClaw.detect(&e), Detection::Unknown(_)));
    }

    #[test]
    fn install_is_idempotent_and_uninstall_leaves_no_trace_in_home() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());

        let mut r = Report::default();
        OpenClaw.install(&e, Scope::Global, &mut r);
        OpenClaw.install(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        assert!(r.steps.iter().any(|s| s.action == Action::Unchanged));

        let mut r = Report::default();
        OpenClaw.uninstall(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        assert!(!home.path().join(".openclaw").exists(), "our dir goes too");
        assert!(home.path().exists(), "$HOME survives, obviously");
    }

    /// A real `~/.openclaw` holds the vendor's own `.bak` rotation and plugin
    /// skills; the prune must stop there.
    #[test]
    fn uninstall_keeps_an_openclaw_directory_the_vendor_is_using() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        OpenClaw.install(&e, Scope::Global, &mut Report::default());
        let theirs = home.path().join(".openclaw/openclaw.json.bak");
        std::fs::write(&theirs, "{}\n").unwrap();

        OpenClaw.uninstall(&e, Scope::Global, &mut Report::default());
        assert!(theirs.exists(), "OpenClaw's own backup must survive");
    }

    /// The headline promise, on the shape the vendor's own CLI cannot manage:
    /// a compact one-line sibling server it would expand, and no `meta` block it
    /// would inject. This is a credential-bearing global config — there is no
    /// `git checkout` to undo a mistake here.
    #[test]
    fn a_users_global_config_round_trips_byte_exactly() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let p = home.path().join(".openclaw/openclaw.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let prior = concat!(
            "{\n",
            "  \"theme\": \"dark\",\n",
            "  \"mcp\": {\n",
            "    \"servers\": {\n",
            "      \"sentry\": {\n",
            "        \"command\": \"npx\",\n",
            "        \"args\": [\n",
            "          \"-y\",\n",
            "          \"@sentry/mcp\"\n",
            "        ]\n",
            "      }\n",
            "    }\n",
            "  }\n",
            "}\n"
        );
        std::fs::write(&p, prior).unwrap();

        OpenClaw.install(&e, Scope::Global, &mut Report::default());
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("\"filigrio\""));
        assert!(after.contains("\"sentry\""), "their server survives");
        assert!(
            !after.contains("lastTouchedAt"),
            "we inject no timestamp — that is the vendor CLI's defect:\n{after}"
        );

        OpenClaw.uninstall(&e, Scope::Global, &mut Report::default());
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            prior,
            "a user's real config must come back byte for byte"
        );
    }

    /// The same exercise against a *real* config, opt-in and copied to scratch
    /// first: `FILIGRIO_REAL_OPENCLAW_CONFIG=… cargo test`. The real file is
    /// never opened for writing — it is mode 0600 and holds credentials.
    #[test]
    fn a_real_openclaw_config_round_trips_when_one_is_pointed_at() {
        let Some(src) = std::env::var_os("FILIGRIO_REAL_OPENCLAW_CONFIG") else {
            return;
        };
        let prior = std::fs::read_to_string(&src)
            .unwrap_or_else(|e| panic!("FILIGRIO_REAL_OPENCLAW_CONFIG is unreadable: {e}"));

        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let p = home.path().join(".openclaw/openclaw.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, &prior).unwrap();

        let mut r = Report::default();
        OpenClaw.install(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        OpenClaw.uninstall(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            prior,
            "a real config did not come back byte-exact"
        );
    }

    /// A config we cannot parse is refused with its path, not rewritten — the
    /// same posture OpenClaw's own strict schema takes.
    #[test]
    fn a_malformed_config_is_a_reported_failure_and_the_file_survives() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let p = home.path().join(".openclaw/openclaw.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let broken = "{ \"mcp\": { \"servers\": ";
        std::fs::write(&p, broken).unwrap();

        let mut r = Report::default();
        OpenClaw.install(&e, Scope::Global, &mut r);
        assert!(!r.is_ok(), "a config we cannot parse is a reported failure");
        assert!(r.failures[0].reason.contains("openclaw.json"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), broken);
    }

    /// The vendor writes this file 0600 because it holds credentials. A file
    /// *we* create must not be looser; a file the user already had is theirs.
    #[cfg(unix)]
    #[test]
    fn a_config_we_create_is_owner_only_and_an_existing_ones_mode_is_untouched() {
        use std::os::unix::fs::PermissionsExt;

        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let p = home.path().join(".openclaw/openclaw.json");

        OpenClaw.install(&e, Scope::Global, &mut Report::default());
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "we created it, so it is owner-only (got {mode:o})"
        );

        // Their file, their mode — even a loose one.
        OpenClaw.uninstall(&e, Scope::Global, &mut Report::default());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{\n  \"theme\": \"dark\"\n}\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        OpenClaw.install(&e, Scope::Global, &mut Report::default());
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "an existing file's mode is not ours to change");
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
        OpenClaw.status(&e, Scope::Global, &mut r);
        assert_eq!(r.steps[0].action, Action::Absent);
        assert_eq!(r.steps[0].detail, "not registered");

        OpenClaw.install(&e, Scope::Global, &mut Report::default());
        let mut r = Report::default();
        OpenClaw.status(&e, Scope::Global, &mut r);
        assert_eq!(r.steps[0].action, Action::Present);
        assert_eq!(r.steps[0].detail, "current");

        // The user re-ran with a different `--socket`. Nothing on disk changed;
        // what install *would* write did.
        let moved = Environment {
            socket_path: PathBuf::from("/run/user/1000/filigrio.sock"),
            ..e.clone()
        };
        let mut r = Report::default();
        OpenClaw.status(&moved, Scope::Global, &mut r);
        assert_eq!(r.steps[0].action, Action::Present);
        assert_eq!(r.steps[0].detail, STALE);

        // And the fix the message names actually works.
        OpenClaw.install(&moved, Scope::Global, &mut Report::default());
        let mut r = Report::default();
        OpenClaw.status(&moved, Scope::Global, &mut r);
        assert_eq!(r.steps[0].detail, "current");
    }
}
