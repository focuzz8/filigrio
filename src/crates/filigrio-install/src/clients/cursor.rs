//! Cursor (ADR-0034 §3, verified against Cursor's own MCP documentation on
//! 2026-07-29 and re-verified 2026-08-08 — see [`super`] for what was read, and
//! `docs/vendor-path-verification.md` for the sweep).
//!
//! One artifact, **project-scoped by default**: `<project>/.cursor/mcp.json`, a
//! `mcpServers.filigrio` entry whose `command` is the absolute path to the
//! `filigrio-mcp` bridge (ADR-0032f §1). Project scope is the default because it
//! travels with the repository and covers a teammate's checkout.
//!
//! ## Both scopes, because Cursor documents both (ADR-0034 §17.1)
//!
//! Cursor's docs name `~/.cursor/mcp.json` as the user-scoped file alongside the
//! project one, so this is one of the two adapters whose
//! [`OtherScope`][super::OtherScope] is `Available`: `filigrio agent install
//! --agent cursor` writes the project file, `--agent cursor --global` writes
//! `~/.cursor/mcp.json`. Nothing is guessed — the two paths differ only in their
//! root, and both are the same `mcpServers` shape.
//!
//! ## No `.cursor/rules/*.mdc`, deliberately
//!
//! Cursor reads **`AGENTS.md`** at the repository root and documents it as the
//! simple alternative to `.cursor/rules`, for projects that want instructions
//! without structured-rule metadata. [`crate::docs`] writes it, on its own
//! command. Minting an `.mdc` rule file would be a second copy of a doc this
//! build already ships, in a format only Cursor reads, drifting from the
//! canonical prose the moment either is edited — the exact per-client
//! duplication ADR-0034 §4 exists to prevent. So this adapter is a
//! **registration writer** and says so, which is §7's split running in the
//! opposite direction from [`crate::docs`]. `.cursor/rules/*.mdc` does exist and
//! is project-scoped: this is a **judgement**, in ADR-0034 §18.1's sense, not an
//! absence like [`super::openclaw`]'s and [`super::hermes`]'s.
//!
//! "The plain-markdown alternative to `.cursor/rules`" is a **paraphrase** of
//! two vendor sentences, not a quotation — the exact wording is in
//! `docs/vendor-path-verification.md` (`cursor`, 2026-08-08).
//!
//! ## Why the entry carries no `type`
//!
//! Claude Code's docs show `{"type": "stdio", "command": …}`; Cursor's show
//! `{"command": …, "args": […], "env": {…}}` and nothing else for a local
//! server. Whether Cursor tolerates an extra `type` key is not something the
//! docs say, so we write the shape they *do* show rather than the shape that
//! would let one `json!` literal serve both clients. Convenience is not a
//! verification source.

use super::{
    bridge_args, note_detection, report_registration, ClientId, ClientInstaller, Detection,
    OtherScope, Scope, ScopeSupport, MCP_SERVERS,
};
use crate::capability::SERVER_NAME;
use crate::{json_entry, Environment, NoteKind, Report};
use serde_json::json;
use std::path::PathBuf;

pub struct Cursor;

/// Said on install *and* on status: "the registration is there" is only two
/// thirds of an answer, and the missing third has a name. The vendor clause is
/// a paraphrase, never a quotation (`docs/vendor-path-verification.md` has the
/// wording), and it is also why this adapter mints no `.mdc` rule file
/// (ADR-0034 §12, §18.1).
fn doc_note() -> String {
    super::registration_only_note(
        "Cursor",
        "Cursor's docs call it the plain-markdown alternative to .cursor/rules",
    )
}

/// Said on install *and* status, but **only for the global scope** — the scope
/// the user has to ask for. The project file needs no such warning: it is in the
/// repository, so it travels, which is the thing the other adapters' `SCOPE_NOTE`
/// exists to say is *not* true of them.
const GLOBAL_SCOPE_NOTE: &str =
    "Cursor's registration went to ~/.cursor/mcp.json (user scope, because you passed --global) — \
     it is machine-local and does not travel with the repository, so every teammate installs it \
     themselves. Cursor documents a project-scoped .cursor/mcp.json too; drop --global to write \
     that one instead";

impl Cursor {
    /// The two paths Cursor's docs name. One function, so the pair cannot drift
    /// apart between install, uninstall and status.
    fn mcp_json(env: &Environment, scope: Scope) -> PathBuf {
        match scope {
            Scope::Project => env.project_root.join(".cursor/mcp.json"),
            Scope::Global => env.home.join(".cursor/mcp.json"),
        }
    }

    /// The shape Cursor's docs show for a local (stdio) server.
    fn registration(env: &Environment) -> serde_json::Value {
        json!({
            "command": env.bridge_bin.display().to_string(),
            "args": bridge_args(env),
            "env": {}
        })
    }
}

impl ClientInstaller for Cursor {
    fn id(&self) -> ClientId {
        ClientId::Cursor
    }

    fn display_name(&self) -> &'static str {
        "Cursor"
    }

    /// §17.1's first row: the vendor documents both, so both are offered.
    fn scope_support(&self) -> ScopeSupport {
        ScopeSupport {
            default: Scope::Project,
            other: OtherScope::Available,
        }
    }

    fn detect(&self, env: &Environment) -> Detection {
        let home_dir = env.home.join(".cursor");
        if home_dir.is_dir() {
            Detection::Present(home_dir.display().to_string())
        } else {
            Detection::Absent(format!("no {}", home_dir.display()))
        }
    }

    fn install(&self, env: &Environment, scope: Scope, report: &mut Report) {
        note_detection(self.id(), self.display_name(), self.detect(env), report);
        report.note_kind(NoteKind::RegistrationOnly, self.id().slug(), doc_note());
        if scope == Scope::Global {
            report.note_kind(NoteKind::UserScoped, self.id().slug(), GLOBAL_SCOPE_NOTE);
        }

        let mcp = Self::mcp_json(env, scope);
        report.record(
            "cursor/mcp",
            &mcp,
            json_entry::upsert(&mcp, MCP_SERVERS, SERVER_NAME, Self::registration(env)),
        );
    }

    fn uninstall(&self, env: &Environment, scope: Scope, report: &mut Report) {
        let mcp = Self::mcp_json(env, scope);
        report.record(
            "cursor/mcp",
            &mcp,
            json_entry::remove(&mcp, MCP_SERVERS, SERVER_NAME),
        );
        // `.cursor/` may hold the user's rules; `prune_empty_dirs` only removes
        // it if it is empty, so a `.cursor/rules/` survives untouched. The walk
        // is bounded by [`super::prune_root`] — whichever root this scope wrote
        // under, never the other one.
        if let Some(dir) = mcp.parent() {
            crate::prune_empty_dirs(dir, super::prune_root(env, scope));
        }
    }

    fn status(&self, env: &Environment, scope: Scope, report: &mut Report) {
        note_detection(self.id(), self.display_name(), self.detect(env), report);
        report.note_kind(NoteKind::RegistrationOnly, self.id().slug(), doc_note());
        if scope == Scope::Global {
            report.note_kind(NoteKind::UserScoped, self.id().slug(), GLOBAL_SCOPE_NOTE);
        }

        let mcp = Self::mcp_json(env, scope);
        report_registration(
            "cursor/mcp",
            &mcp,
            json_entry::state(&mcp, MCP_SERVERS, SERVER_NAME, &Self::registration(env)),
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
    fn install_writes_the_documented_shape_at_the_documented_path() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        let mut r = Report::default();
        Cursor.install(&e, Scope::Project, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        let text = std::fs::read_to_string(d.path().join(".cursor/mcp.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let entry = &v["mcpServers"]["filigrio"];
        assert_eq!(entry["command"], "/opt/g/bin/filigrio-mcp");
        assert_eq!(entry["args"][0], "--socket");
        assert_eq!(entry["args"][1], "/run/filigrio.sock");
        assert!(entry["env"].is_object());
    }

    /// The shape Cursor's docs show has three keys. A `type` we never verified
    /// must not creep in from the Claude Code adapter next door.
    #[test]
    fn the_entry_carries_only_the_keys_cursors_docs_show() {
        let d = tempfile::tempdir().unwrap();
        let v = Cursor::registration(&env(d.path(), d.path()));
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["command", "args", "env"], "got {v}");
    }

    #[test]
    fn install_is_idempotent_and_uninstall_leaves_nothing() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());

        let mut r = Report::default();
        Cursor.install(&e, Scope::Project, &mut r);
        Cursor.install(&e, Scope::Project, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        assert!(r.steps.iter().any(|s| s.action == Action::Unchanged));

        let mut r = Report::default();
        Cursor.uninstall(&e, Scope::Project, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        assert!(!d.path().join(".cursor/mcp.json").exists());
        assert!(!d.path().join(".cursor").exists(), "our empty dir goes too");
    }

    /// A `.cursor/` the user already keeps rules in is not ours to remove.
    #[test]
    fn uninstall_keeps_a_cursor_directory_the_user_uses() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        Cursor.install(&e, Scope::Project, &mut Report::default());
        std::fs::create_dir_all(d.path().join(".cursor/rules")).unwrap();
        std::fs::write(d.path().join(".cursor/rules/mine.mdc"), "mine\n").unwrap();

        Cursor.uninstall(&e, Scope::Project, &mut Report::default());
        assert!(d.path().join(".cursor/rules/mine.mdc").exists());
    }

    /// A sibling server in the user's `.cursor/mcp.json` survives both
    /// directions, and the file comes back byte for byte.
    #[test]
    fn a_users_cursor_config_round_trips_byte_exactly() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        let p = d.path().join(".cursor/mcp.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let prior = "{\n  \"mcpServers\": {\n    \"sentry\": {\n      \"command\": \"npx\",\n      \"args\": [\n        \"-y\",\n        \"@sentry/mcp-server\"\n      ]\n    }\n  }\n}\n";
        std::fs::write(&p, prior).unwrap();

        Cursor.install(&e, Scope::Project, &mut Report::default());
        assert!(std::fs::read_to_string(&p).unwrap().contains("filigrio"));

        Cursor.uninstall(&e, Scope::Project, &mut Report::default());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    /// The honesty requirement: "the registration is there" is two thirds of an
    /// answer, so both verbs a user reads must name what documents this agent
    /// and the command that installs it.
    ///
    /// It used to name the *`agents-md` adapter*, which is a thing that no
    /// longer exists (ADR-0034 §18.2). What a user needs from this note is a
    /// command they can run, and `filigrio docs install` is one.
    #[test]
    fn both_install_and_status_name_what_documents_this_agent_and_how_to_install_it() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());

        for run in [
            {
                let mut r = Report::default();
                Cursor.install(&e, Scope::Project, &mut r);
                r
            },
            {
                let mut r = Report::default();
                Cursor.status(&e, Scope::Project, &mut r);
                r
            },
        ] {
            assert!(
                run.notes.iter().any(|n| n.text().contains("AGENTS.md")
                    && n.text().contains("`filigrio docs install`")),
                "notes were {:?}",
                run.notes
            );
        }
    }

    /// The three states a user reads, and the middle one is the whole point:
    /// a registration whose socket has moved is `Present` either way, so only
    /// the detail tells them apart. `status` said `registered` for both until
    /// this test existed.
    #[test]
    fn status_distinguishes_absent_current_and_stale() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        let mut r = Report::default();
        Cursor.status(&e, Scope::Project, &mut r);
        assert_eq!(r.steps[0].action, Action::Absent);
        assert_eq!(r.steps[0].detail, "not registered");

        Cursor.install(&e, Scope::Project, &mut Report::default());
        let mut r = Report::default();
        Cursor.status(&e, Scope::Project, &mut r);
        assert_eq!(r.steps[0].action, Action::Present);
        assert_eq!(r.steps[0].detail, "current");

        // The user re-ran with a different `--socket`. Nothing on disk changed;
        // what install *would* write did.
        let moved = Environment {
            socket_path: PathBuf::from("/run/user/1000/filigrio.sock"),
            ..e.clone()
        };
        let mut r = Report::default();
        Cursor.status(&moved, Scope::Project, &mut r);
        assert_eq!(r.steps[0].action, Action::Present);
        assert_eq!(r.steps[0].detail, STALE);

        // And the fix the message names actually works.
        Cursor.install(&moved, Scope::Project, &mut Report::default());
        let mut r = Report::default();
        Cursor.status(&moved, Scope::Project, &mut r);
        assert_eq!(r.steps[0].detail, "current");
    }

    #[test]
    fn an_absent_cursor_is_reported_and_the_file_still_lands() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut r = Report::default();
        Cursor.install(&env(d.path(), home.path()), Scope::Project, &mut r);
        assert!(r.notes.iter().any(|n| n.text().contains("not detected")));
        assert!(d.path().join(".cursor/mcp.json").exists());
    }
}
