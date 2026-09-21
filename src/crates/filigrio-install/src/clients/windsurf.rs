//! Windsurf / Devin Desktop (ADR-0034 §3, **re-verified 2026-08-08** against
//! `docs.devin.ai`; the 2026-07-29 reading of this adapter was wrong in both
//! halves — see `docs/vendor-path-verification.md`).
//!
//! One artifact, **project-scoped by default** and in the same `mcpServers`
//! container and `{command, args, env}` server shape Cursor documents:
//!
//! | scope | path | committed? |
//! |---|---|---|
//! | project (default) | `<project>/.devin/mcp_config.json` | yes, the vendor says so |
//! | user (`--global`) | `~/.config/devin/mcp_config.json` | n/a |
//!
//! ## Which agent each path reaches
//!
//! Windsurf ships **two** agents reading **two** config layouts.
//! `https://docs.devin.ai/desktop/cascade/mcp` (fetched 2026-08-08)
//! opens with:
//!
//! > The MCP configuration on this page applies to the legacy Cascade agent
//! > only. The Devin Local agent—the default agent for new tabs—configures MCP
//! > servers in the Devin CLI config files instead.
//!
//! and the page it describes is `~/.codeium/windsurf/mcp_config.json`. So:
//!
//! - **`~/.codeium/windsurf/mcp_config.json` reaches the legacy Cascade agent
//!   only.** We no longer write it. It is documented, so this is a decision
//!   rather than an absence, and `LEGACY_AGENT_NOTE` is the sentence a user on
//!   that agent needs, printed on install and on status.
//! - **`.devin/mcp_config.json` and `~/.config/devin/mcp_config.json` reach the
//!   Devin Local agent**, which is the default agent for new tabs in Devin
//!   Desktop and the same harness as the Devin CLI.
//!
//! ### The join closes on vendor pages
//!
//! The step from "the Devin Local agent's MCP config" to a named file is the
//! vendor's own hyperlink chain, not our inference: the desktop page links to
//! `https://docs.devin.ai/cli/extensibility/mcp/configuration`, which names
//! `.devin/mcp_config.json` (project), `.devin/mcp_config.local.json`
//! (project-local) and `~/.config/devin/mcp_config.json` (user), and
//! `https://docs.devin.ai/desktop/devin-local` tabulates the same locations for
//! the desktop side — the project file "checked into version control". The MCP
//! page also states the migration (dedicated `mcp_config.json` files split out
//! of the main configs as of v3000.3, old entries migrated on startup). Page
//! quotes in `docs/vendor-path-verification.md` (`windsurf`, 2026-08-08).
//!
//! ## Why `~/.config/devin/mcp_config.json` is the user-scope path
//!
//! The choice was between the two documented user-scope files, and it is the
//! same question as above one layer up: `~/.codeium/windsurf/mcp_config.json` is
//! documented and reaches the legacy agent; `~/.config/devin/mcp_config.json` is
//! documented and reaches the default one. A registration the default agent
//! cannot see is the silent failure this crate exists to refuse, so the path
//! that reaches the default agent wins. `~/.codeium/` is also the namespace the
//! vendor is migrating *away* from — it still holds the Cascade MCP config, the
//! global rules file and a channel-scoped skills root while the new user home is
//! `~/.config/devin/`, and two homes for one product is a transitional state.
//!
//! ## Project scope
//!
//! A retired `SCOPE_NOTE` claimed "Windsurf documents no project-scoped file";
//! the vendor documents one, as committed to version control. Telling users a
//! vendor limitation existed where the real cause was an unread page is the
//! failure mode §3 makes per-client paths *facts to check on the day* to
//! prevent.
//!
//! Reversibility follows the scope. At project scope uninstall removes our one
//! key from `.devin/mcp_config.json` and prunes `.devin` **only if it is then
//! empty** — a repository with `.devin/rules/` or `.devin/config.json` keeps its
//! directory, because `prune_empty_dirs` stops at the first non-empty one. At
//! user scope the same walk is bounded by `$HOME`.
//!
//! ## Three project skill directories exist, and we write none of them
//!
//! `https://docs.devin.ai/cli/extensibility/skills/overview` (2026-08-08;
//! `docs/vendor-path-verification.md`) documents three project-local roots, all
//! committed to version control — `.devin/skills/<name>/SKILL.md`,
//! `.windsurf/skills/<name>/SKILL.md`, `.agents/skills/<name>/SKILL.md` — plus
//! `~/.config/devin/skills/` and `~/.codeium/<channel>/skills/` globally, with
//! the usual layout: YAML frontmatter, `name` and `description` required.
//!
//! **The position: they exist, we write no skill in them** — on ADR-0034 §12's
//! judgement grounds, as [`super::cursor`] and [`super::codex`] do, because
//! `AGENTS.md` already reaches this vendor through the same Rules engine.
//! §18.1 defers the wider question — four vendors here document
//! `.agents/skills/<name>/SKILL.md`; see [`super`].
//!
//! ## Detection: `Present` or `Unknown`, never `Absent`
//!
//! Now that this adapter no longer writes `~/.codeium/`, that directory is a
//! vendor-documented marker we cannot have created — so finding it is real
//! evidence, where before it would only have been our own footprint. Not
//! finding it proves nothing: a Devin Desktop install that never ran the
//! Codeium-era product need not have one, and the new user home
//! (`~/.config/devin/`) *is* a path `--global` writes, so probing it would put
//! the detector back to reporting its own work. Hence [`Detection::Present`] on
//! the legacy marker and [`Detection::Unknown`] otherwise — never `Absent`,
//! which would be a claim this crate cannot support.
//!
//! ## Re-verify this adapter first, every time
//!
//! The Codeium→Devin rebrand moved everything at once — docs domains redirect,
//! the rules convention went `.windsurf/rules/` → `.devin/rules/` (old path
//! kept as "a backward compatibility fallback"), MCP config split out of the
//! main config "as of v3000.3", and the default agent changed under the same
//! product name — and this adapter was not updated when it happened. This
//! vendor moves fastest of the seven. The rules move still does not touch us —
//! we write `AGENTS.md` (via [`crate::docs`]), which the same rules engine
//! processes.

use super::{
    bridge_args, note_detection, report_registration, ClientId, ClientInstaller, Detection,
    OtherScope, Scope, ScopeSupport, MCP_SERVERS,
};
use crate::capability::SERVER_NAME;
use crate::{json_entry, Environment, NoteKind, Report};
use serde_json::json;
use std::path::PathBuf;

pub struct Windsurf;

/// **One vendor, two agents, two config layouts** — said on install *and* on
/// status, at *both* scopes.
///
/// The user this exists for is the one still on the legacy Cascade agent, for
/// whom this install writes a file their agent does not read. We cannot tell
/// which agent they run, and a registration that silently reaches nobody is the
/// failure mode this crate is built to refuse — so the fact is stated and the
/// remedy is named, rather than the path being guessed at.
///
/// It replaces a `SCOPE_NOTE` that said the opposite of what the vendor
/// documents. See the module header for the pages.
const LEGACY_AGENT_NOTE: &str =
    "Windsurf ships two agents reading two config layouts, and this registration went to the \
     Devin CLI layout (.devin/mcp_config.json, or ~/.config/devin/mcp_config.json with --global) \
     — the one the Devin Local agent reads, which docs.devin.ai calls the default agent for new \
     tabs. The older ~/.codeium/windsurf/mcp_config.json is documented as applying \"to the \
     legacy Cascade agent only\" and filigrio does not write it: if you are on the legacy Cascade \
     agent, add the same command there by hand or switch to a new tab";

/// Said on install *and* status, but **only for the global scope** — the scope
/// the user has to ask for, as in [`super::cursor`]. The project file needs no
/// such warning: it is in the repository, so it travels.
const GLOBAL_SCOPE_NOTE: &str =
    "Windsurf's registration went to ~/.config/devin/mcp_config.json (user scope, because you \
     passed --global) — it is machine-local and does not travel with the repository, so every \
     teammate installs it themselves. The vendor documents a project-scoped \
     .devin/mcp_config.json as committed to version control; drop --global to write that one \
     instead";

/// Said on install *and* on status, as in [`super::cursor`].
fn doc_note() -> String {
    super::registration_only_note(
        "Windsurf",
        "Windsurf processes it through the same Rules engine; root-level is always-on",
    )
}

impl Windsurf {
    /// The two paths `docs.devin.ai` names for MCP servers. One function, so the
    /// pair cannot drift apart between install, uninstall and status.
    ///
    /// Neither is `~/.codeium/windsurf/mcp_config.json`: that file is the legacy
    /// Cascade agent's, and writing it would register the bridge with the agent
    /// the vendor no longer opens new tabs in. See the module header.
    fn mcp_config(env: &Environment, scope: Scope) -> PathBuf {
        match scope {
            Scope::Project => env.project_root.join(".devin/mcp_config.json"),
            Scope::Global => env.home.join(".config/devin/mcp_config.json"),
        }
    }

    /// The directory the vendor documents as its own on-disk marker *and* which
    /// this adapter never writes — the two conditions that make a probe evidence
    /// rather than a footprint.
    fn legacy_marker(env: &Environment) -> PathBuf {
        env.home.join(".codeium/windsurf")
    }

    /// The shape the vendor's own example shows — the same three keys as Cursor,
    /// and as with Cursor no `type`, because their docs show none. `disabled` is
    /// documented as optional and is the user's switch, not ours.
    fn registration(env: &Environment) -> serde_json::Value {
        json!({
            "command": env.bridge_bin.display().to_string(),
            "args": bridge_args(env),
            "env": {}
        })
    }
}

impl ClientInstaller for Windsurf {
    fn id(&self) -> ClientId {
        ClientId::Windsurf
    }

    fn display_name(&self) -> &'static str {
        "Windsurf"
    }

    /// §17.1's first row: the vendor documents both scopes, so both are
    /// offered.
    fn scope_support(&self) -> ScopeSupport {
        ScopeSupport {
            default: Scope::Project,
            other: OtherScope::Available,
        }
    }

    fn detect(&self, env: &Environment) -> Detection {
        let marker = Self::legacy_marker(env);
        if marker.is_dir() {
            Detection::Present(marker.display().to_string())
        } else {
            // Never `Absent`. The new layout's user directory is one `--global`
            // install writes, so probing it would report our own footprint, and
            // a Devin Desktop install that post-dates the Codeium naming need
            // never have had a `~/.codeium/` at all.
            Detection::Unknown(format!(
                "no {}, and the Devin Desktop layout that replaced it lives under the same \
                 ~/.config/devin this adapter writes — so an absence proves nothing and a \
                 presence would be our own footprint",
                marker.display()
            ))
        }
    }

    fn install(&self, env: &Environment, scope: Scope, report: &mut Report) {
        note_detection(self.id(), self.display_name(), self.detect(env), report);
        report.note_kind(NoteKind::VendorSplit, self.id().slug(), LEGACY_AGENT_NOTE);
        report.note_kind(NoteKind::RegistrationOnly, self.id().slug(), doc_note());
        if scope == Scope::Global {
            report.note_kind(NoteKind::UserScoped, self.id().slug(), GLOBAL_SCOPE_NOTE);
        }

        let mcp = Self::mcp_config(env, scope);
        report.record(
            "windsurf/mcp",
            &mcp,
            json_entry::upsert(&mcp, MCP_SERVERS, SERVER_NAME, Self::registration(env)),
        );
    }

    fn uninstall(&self, env: &Environment, scope: Scope, report: &mut Report) {
        let mcp = Self::mcp_config(env, scope);
        report.record(
            "windsurf/mcp",
            &mcp,
            json_entry::remove(&mcp, MCP_SERVERS, SERVER_NAME),
        );
        // `.devin/` is the vendor's directory, not ours: it holds `config.json`,
        // `rules/` and `skills/` in a repository that uses them, so it goes only
        // if we are the last thing in it. Bounded by [`super::prune_root`] —
        // whichever root this scope wrote under, never the other one.
        if let Some(dir) = mcp.parent() {
            crate::prune_empty_dirs(dir, super::prune_root(env, scope));
        }
    }

    fn status(&self, env: &Environment, scope: Scope, report: &mut Report) {
        note_detection(self.id(), self.display_name(), self.detect(env), report);
        report.note_kind(NoteKind::VendorSplit, self.id().slug(), LEGACY_AGENT_NOTE);
        report.note_kind(NoteKind::RegistrationOnly, self.id().slug(), doc_note());
        if scope == Scope::Global {
            report.note_kind(NoteKind::UserScoped, self.id().slug(), GLOBAL_SCOPE_NOTE);
        }

        let mcp = Self::mcp_config(env, scope);
        report_registration(
            "windsurf/mcp",
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

    /// **The default install writes a file inside the repository** (2026-08-08
    /// sweep; ADR-0034 §3, §17.1).
    ///
    /// This adapter declared `default: Global, other: NotOffered` on the claim
    /// that "Windsurf documents no project-scoped file". The vendor documents
    /// `.devin/mcp_config.json` and documents it as committed to version
    /// control, so the claim was ours and it was false — and the cost of it was
    /// a registration that could not travel with a checkout for a reason that
    /// did not exist.
    ///
    /// The `$HOME` half is asserted too, because a project default that also
    /// wrote under `$HOME` would pass the first assertion and break §17.1's
    /// gate.
    #[test]
    fn the_default_scope_writes_the_project_file_the_vendor_documents_as_committed() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(repo.path(), home.path());
        let mut r = Report::default();
        Windsurf.install(&e, Scope::Project, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        let p = repo.path().join(".devin/mcp_config.json");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let entry = &v["mcpServers"]["filigrio"];
        assert_eq!(entry["command"], "/opt/g/bin/filigrio-mcp");
        assert_eq!(entry["args"][1], "/run/filigrio.sock");

        assert!(
            crate::clients::installer_for(crate::clients::ClientId::Windsurf)
                .scope_support()
                .default
                == Scope::Project,
            "the declared default and what install writes must be the same fact"
        );
        assert_eq!(
            std::fs::read_dir(home.path()).unwrap().count(),
            0,
            "a project-scoped install must not touch $HOME"
        );
    }

    /// **`--global` reaches the user file the vendor names, and never the legacy
    /// one.**
    ///
    /// Both are documented and only one reaches the Devin Local agent, so the
    /// negative assertion is the load-bearing half: an adapter that kept writing
    /// `~/.codeium/windsurf/mcp_config.json` would report success on every run
    /// and register the bridge with an agent the vendor no longer opens new tabs
    /// in.
    #[test]
    fn the_global_scope_writes_the_devin_user_config_and_never_the_legacy_cascade_one() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(repo.path(), home.path());
        let mut r = Report::default();
        Windsurf.install(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        assert!(
            home.path().join(".config/devin/mcp_config.json").is_file(),
            "the documented user-scope path is ~/.config/devin/mcp_config.json"
        );
        assert!(
            !home.path().join(".codeium").exists(),
            "~/.codeium/windsurf/mcp_config.json applies to the legacy Cascade agent only; \
             writing it registers the bridge where the default agent cannot see it"
        );
        assert!(
            !repo.path().join(".devin").exists(),
            "a --global install writes nothing into the repository"
        );
    }

    #[test]
    fn the_entry_carries_only_the_keys_the_docs_show() {
        let d = tempfile::tempdir().unwrap();
        let v = Windsurf::registration(&env(d.path(), d.path()));
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["command", "args", "env"], "got {v}");
    }

    /// **Both verbs say which of the vendor's two agents this registration
    /// reaches**, at both scopes.
    ///
    /// The sentence this replaces said "Windsurf documents no project-scoped
    /// file", which was printed on every install and every status for two
    /// verification rounds and was not true on either of them. What a user
    /// actually cannot find out for themselves is that the product ships two
    /// agents on two layouts and we write one of them — so that is what is said,
    /// and it is said at project scope too, where no scope warning applies.
    #[test]
    fn both_verbs_name_the_agent_this_registration_reaches_and_the_one_it_does_not() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(repo.path(), home.path());

        for scope in [Scope::Project, Scope::Global] {
            for run in [
                {
                    let mut r = Report::default();
                    Windsurf.install(&e, scope, &mut r);
                    r
                },
                {
                    let mut r = Report::default();
                    Windsurf.status(&e, scope, &mut r);
                    r
                },
            ] {
                assert!(
                    run.notes
                        .iter()
                        .any(|n| n.text().contains(".devin/mcp_config.json")
                            && n.text().contains("legacy Cascade agent")),
                    "{scope:?}: notes were {:?}",
                    run.notes
                );
                assert!(
                    run.notes.iter().any(|n| n.text().contains("AGENTS.md")
                        && n.text().contains("`filigrio docs install`")),
                    "{scope:?}: notes were {:?}",
                    run.notes
                );
                assert!(
                    !run.notes
                        .iter()
                        .any(|n| n.text().contains("documents no project-scoped file")),
                    "the retracted claim must not survive anywhere: {:?}",
                    run.notes
                );
            }
        }
    }

    /// The "does not travel" warning belongs to the scope the user had to ask
    /// for, and to that scope only — the project file is in the repository, so
    /// it does travel and a warning there would be false.
    #[test]
    fn only_the_global_scope_warns_that_the_registration_stays_on_this_machine() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(repo.path(), home.path());

        let mut project = Report::default();
        Windsurf.install(&e, Scope::Project, &mut project);
        assert!(
            !project
                .notes
                .iter()
                .any(|n| n.text().contains("does not travel")),
            "a committed project file travels: {:?}",
            project.notes
        );

        let mut global = Report::default();
        Windsurf.install(&e, Scope::Global, &mut global);
        assert!(
            global
                .notes
                .iter()
                .any(|n| n.text().contains("does not travel") && n.text().contains("drop --global")),
            "{:?}",
            global.notes
        );
    }

    /// Detection must not report our own footprint back to us — and now that the
    /// legacy directory is one this adapter never writes, "we have not seen it"
    /// is `Unknown` rather than `Absent`: a Devin Desktop install postdating the
    /// Codeium naming need never have had one.
    #[test]
    fn detection_stays_unknown_after_an_install_and_finds_only_the_directory_we_never_write() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(repo.path(), home.path());
        Windsurf.install(&e, Scope::Project, &mut Report::default());
        Windsurf.install(&e, Scope::Global, &mut Report::default());
        assert!(
            matches!(Windsurf.detect(&e), Detection::Unknown(_)),
            "neither install may make this adapter think it found Windsurf"
        );

        std::fs::create_dir_all(home.path().join(".codeium/windsurf")).unwrap();
        assert!(
            matches!(Windsurf.detect(&e), Detection::Present(_)),
            "the legacy home is vendor-documented and cannot be ours, so it is evidence"
        );
    }

    #[test]
    fn install_is_idempotent_and_uninstall_leaves_no_trace_at_either_scope() {
        for scope in [Scope::Project, Scope::Global] {
            let repo = tempfile::tempdir().unwrap();
            let home = tempfile::tempdir().unwrap();
            let e = env(repo.path(), home.path());

            let mut r = Report::default();
            Windsurf.install(&e, scope, &mut r);
            Windsurf.install(&e, scope, &mut r);
            assert!(r.is_ok(), "{:?}", r.failures);
            assert!(r.steps.iter().any(|s| s.action == Action::Unchanged));

            let mut r = Report::default();
            Windsurf.uninstall(&e, scope, &mut r);
            assert!(r.is_ok(), "{:?}", r.failures);
            assert!(
                !repo.path().join(".devin").exists() && !home.path().join(".config").exists(),
                "{scope:?}: our empty dirs go too"
            );
            assert!(repo.path().exists() && home.path().exists());
        }
    }

    /// `.devin/` is the vendor's directory and holds `config.json`, `rules/` and
    /// `skills/` in a repository that uses them. The prune must stop at the
    /// first thing that is not ours.
    #[test]
    fn uninstall_keeps_a_devin_directory_the_repository_is_using() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(repo.path(), home.path());
        Windsurf.install(&e, Scope::Project, &mut Report::default());
        let theirs = repo.path().join(".devin/rules/house-style.md");
        std::fs::create_dir_all(theirs.parent().unwrap()).unwrap();
        std::fs::write(&theirs, "theirs\n").unwrap();

        Windsurf.uninstall(&e, Scope::Project, &mut Report::default());
        assert!(theirs.exists(), "the repository's own rules must survive");
    }

    /// A user's config with a sibling server comes back byte for byte. Asserted
    /// at **global** scope, because that file is not in a repository and has no
    /// `git checkout` to undo it.
    #[test]
    fn a_users_global_config_round_trips_byte_exactly() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(repo.path(), home.path());
        let p = home.path().join(".config/devin/mcp_config.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let prior = "{\n  \"mcpServers\": {\n    \"sentry\": {\n      \"command\": \"npx\",\n      \"args\": [\n        \"-y\",\n        \"@sentry/mcp-server\"\n      ],\n      \"env\": {\n        \"SENTRY_AUTH_TOKEN\": \"redacted\"\n      }\n    }\n  }\n}\n";
        std::fs::write(&p, prior).unwrap();

        Windsurf.install(&e, Scope::Global, &mut Report::default());
        assert!(std::fs::read_to_string(&p).unwrap().contains("filigrio"));

        Windsurf.uninstall(&e, Scope::Global, &mut Report::default());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    /// The three states a user reads, and the middle one is the whole point:
    /// a registration whose socket has moved is `Present` either way, so only
    /// the detail tells them apart. `status` said `registered` for both until
    /// this test existed.
    #[test]
    fn status_distinguishes_absent_current_and_stale() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(repo.path(), home.path());
        let mut r = Report::default();
        Windsurf.status(&e, Scope::Project, &mut r);
        assert_eq!(r.steps[0].action, Action::Absent);
        assert_eq!(r.steps[0].detail, "not registered");

        Windsurf.install(&e, Scope::Project, &mut Report::default());
        let mut r = Report::default();
        Windsurf.status(&e, Scope::Project, &mut r);
        assert_eq!(r.steps[0].action, Action::Present);
        assert_eq!(r.steps[0].detail, "current");

        // The user re-ran with a different `--socket`. Nothing on disk changed;
        // what install *would* write did.
        let moved = Environment {
            socket_path: PathBuf::from("/run/user/1000/filigrio.sock"),
            ..e.clone()
        };
        let mut r = Report::default();
        Windsurf.status(&moved, Scope::Project, &mut r);
        assert_eq!(r.steps[0].action, Action::Present);
        assert_eq!(r.steps[0].detail, STALE);

        // And the fix the message names actually works.
        Windsurf.install(&moved, Scope::Project, &mut Report::default());
        let mut r = Report::default();
        Windsurf.status(&moved, Scope::Project, &mut r);
        assert_eq!(r.steps[0].detail, "current");
    }
}
