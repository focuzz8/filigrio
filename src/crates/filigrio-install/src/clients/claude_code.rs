//! Claude Code (ADR-0034 §3, verified against `code.claude.com/docs/en` on
//! 2026-07-29 — see [`super`] for what was read).
//!
//! Two artifacts, both **project-scoped**:
//!
//! 1. `<project>/.mcp.json` — `mcpServers.filigrio`, a `stdio` entry whose
//!    `command` is the absolute path to the `filigrio-mcp` bridge (ADR-0032f §1:
//!    the bridge, never the daemon and never the CLI). Claude Code prompts for
//!    approval before using a project-scoped server, which is the correct
//!    posture — we do not try to pre-approve it. Since v2.1.196 that approval
//!    also sits behind a workspace-trust dialog: a cloned repository cannot
//!    approve its own servers until the user trusts the workspace, even with
//!    `enableAllProjectMcpServers` committed — the same shape as Codex's trust
//!    gate, not a contrast to it (`docs/vendor-path-verification.md`,
//!    `claude-code`).
//! 2. `<project>/.claude/skills/filigrio/SKILL.md` — the ADR-0027 capability
//!    doc as a skill, so its body costs nothing until Claude loads it.
//!
//! **Why not user scope.** The docs are explicit that user- and local-scope MCP
//! servers live in `~/.claude.json`, which is also where Claude Code keeps
//! per-project session state. Rewriting that file to add one key would put an
//! installer in the middle of a live application's state file for no gain: a
//! project-scoped `.mcp.json` already covers the repository being wired, and it
//! is the file the docs tell teams to commit. Recorded as an ADR open question
//! rather than silently decided.
//!
//! That decision is now a **refusal the CLI can quote** rather than prose only
//! this file carries: `USER_SCOPE_REFUSED` is the sentence
//! `--agent claude-code --global` prints (ADR-0034 §17.1). It is a
//! [`OtherScope::Refused`], not a `NotOffered` — the vendor documents the file;
//! we decline it — and that distinction is the whole reason `--global` is not a
//! `bool` on the adapter.

use super::{
    bridge_args, note_detection, report_registration, ClientId, ClientInstaller, Detection,
    OtherScope, Scope, ScopeSupport, MCP_SERVERS,
};
use crate::capability::{self, CapabilityModel, SERVER_NAME, SKILL_NAME};
use crate::{json_entry, render, Environment, InstallError, Report};
use serde_json::json;
use std::path::PathBuf;

pub struct ClaudeCode;

/// The one copy of §9's refusal, printed by the CLI when `--global` names this
/// adapter (ADR-0034 §17.1). The module header argues it; this is the sentence.
const USER_SCOPE_REFUSED: &str =
    "Claude Code's user-scoped MCP servers live in ~/.claude.json, which is also where Claude Code \
     keeps its live per-project session state — filigrio will not rewrite a running application's \
     state file to add one key. The project-scoped .mcp.json covers this repository and is the \
     file Claude Code's own docs tell teams to commit, so drop --global to install it";

impl ClaudeCode {
    fn mcp_json(env: &Environment) -> PathBuf {
        env.project_root.join(".mcp.json")
    }

    /// The root is [`super::skill_root`] rather than `env.project_root` spelled
    /// out, even though this adapter refuses `--global` and can only ever be
    /// handed [`Scope::Project`]. Two call sites make "a skill stays in the
    /// checkout" a rule with a home; one makes it a coincidence in
    /// [`super::opencode`].
    fn skill_md(env: &Environment, requested: Scope) -> PathBuf {
        super::skill_root(env, requested)
            .join(".claude/skills")
            .join(SKILL_NAME)
            .join("SKILL.md")
    }

    /// The registration Claude Code's docs show for a local stdio server.
    ///
    /// The explicit `"type": "stdio"` is Claude Code's own documented shape and
    /// is *not* shared with [`super::cursor`] / [`super::windsurf`], whose docs
    /// show `{command, args, env}` with no `type` — see [`super::cursor`].
    fn registration(env: &Environment) -> serde_json::Value {
        json!({
            "type": "stdio",
            "command": env.bridge_bin.display().to_string(),
            "args": bridge_args(env),
            "env": {}
        })
    }

    /// The rendered `SKILL.md`.
    ///
    /// [`capability::skill`] is shared with [`super::opencode`], which writes
    /// the same document into its own namespace (ADR-0034 §18.1). The model is
    /// where the two part company: `registration_path` is **this** agent's
    /// `.mcp.json`, so the file each agent reads points at the config that
    /// agent's own registration went into.
    fn skill_body(env: &Environment) -> Result<String, InstallError> {
        let hb = render::registry()?;
        let model = CapabilityModel::new(env, Self::mcp_json(env).display().to_string());
        capability::skill(&hb, &model)
    }
}

impl ClientInstaller for ClaudeCode {
    fn id(&self) -> ClientId {
        ClientId::ClaudeCode
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    /// §17.1's second row — a refusal, and the reason is the point of it.
    fn scope_support(&self) -> ScopeSupport {
        ScopeSupport {
            default: Scope::Project,
            other: OtherScope::Refused(USER_SCOPE_REFUSED),
        }
    }

    fn detect(&self, env: &Environment) -> Detection {
        let home_dir = env.home.join(".claude");
        if home_dir.is_dir() {
            Detection::Present(home_dir.display().to_string())
        } else {
            Detection::Absent(format!("no {}", home_dir.display()))
        }
    }

    fn install(&self, env: &Environment, scope: Scope, report: &mut Report) {
        note_detection(self.id(), self.display_name(), self.detect(env), report);

        let mcp = Self::mcp_json(env);
        report.record(
            "claude-code/mcp",
            &mcp,
            json_entry::upsert(&mcp, MCP_SERVERS, SERVER_NAME, Self::registration(env)),
        );

        let skill = Self::skill_md(env, scope);
        super::install_skill("claude-code/skill", &skill, Self::skill_body(env), report);
    }

    fn uninstall(&self, env: &Environment, scope: Scope, report: &mut Report) {
        let mcp = Self::mcp_json(env);
        report.record(
            "claude-code/mcp",
            &mcp,
            json_entry::remove(&mcp, MCP_SERVERS, SERVER_NAME),
        );

        // The skill directory is wholly ours (`.claude/skills/filigrio/`);
        // [`super::remove_skill`] holds the rule that a user's `reference.md`
        // beside our SKILL.md keeps the directory alive.
        let skill = Self::skill_md(env, scope);
        super::remove_skill(env, scope, "claude-code/skill", &skill, report);
    }

    fn status(&self, env: &Environment, scope: Scope, report: &mut Report) {
        note_detection(self.id(), self.display_name(), self.detect(env), report);

        let mcp = Self::mcp_json(env);
        report_registration(
            "claude-code/mcp",
            &mcp,
            json_entry::state(&mcp, MCP_SERVERS, SERVER_NAME, &Self::registration(env)),
            report,
        );

        let skill = Self::skill_md(env, scope);
        super::report_skill_status("claude-code/skill", &skill, Self::skill_body(env), report);
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
    fn install_writes_the_documented_mcp_shape_pointing_at_the_bridge() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        let mut r = Report::default();
        ClaudeCode.install(&e, Scope::Project, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(d.path().join(".mcp.json")).unwrap())
                .unwrap();
        let entry = &v["mcpServers"]["filigrio"];
        assert_eq!(entry["type"], "stdio");
        assert_eq!(entry["command"], "/opt/g/bin/filigrio-mcp");
        assert_eq!(entry["args"][0], "--socket");
        assert!(entry["env"].is_object());
    }

    #[test]
    fn the_skill_lands_where_claude_code_looks_for_it() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        let mut r = Report::default();
        ClaudeCode.install(&e, Scope::Project, &mut r);

        let skill = d.path().join(".claude/skills/filigrio/SKILL.md");
        let text = std::fs::read_to_string(&skill).unwrap();
        assert!(text.starts_with("---\n"), "skills need YAML frontmatter");
        assert!(text.contains("\ndescription: "));
        assert!(text.contains("get_neighbors"));
    }

    #[test]
    fn install_is_idempotent_and_uninstall_leaves_nothing() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());

        let mut r = Report::default();
        ClaudeCode.install(&e, Scope::Project, &mut r);
        ClaudeCode.install(&e, Scope::Project, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        assert!(
            r.steps.iter().any(|s| s.action == Action::Unchanged),
            "a second install must report Unchanged, not rewrite"
        );

        let mut r = Report::default();
        ClaudeCode.uninstall(&e, Scope::Project, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        assert!(!d.path().join(".mcp.json").exists());
        assert!(!d.path().join(".claude/skills/filigrio").exists());
    }

    /// A sibling file the user put in our skill directory keeps the directory
    /// alive — uninstall removes what it installed, not the folder's contents.
    #[test]
    fn uninstall_keeps_a_skill_directory_the_user_added_to() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        let mut r = Report::default();
        ClaudeCode.install(&e, Scope::Project, &mut r);

        let extra = d.path().join(".claude/skills/filigrio/reference.md");
        std::fs::write(&extra, "mine\n").unwrap();

        ClaudeCode.uninstall(&e, Scope::Project, &mut Report::default());
        assert!(extra.exists(), "the user's file must survive");
    }

    /// Both artifacts, all three states — and the two must be able to disagree.
    ///
    /// This adapter is the only one that owns a registration *and* a rendered
    /// doc, so it is the place where "the skill is stale" and "the registration
    /// is stale" must be separately reachable. They were not: the registration
    /// said `registered` whatever it pointed at.
    #[test]
    fn status_distinguishes_absent_current_and_stale_for_both_artifacts() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());

        let mut r = Report::default();
        ClaudeCode.status(&e, Scope::Project, &mut r);
        assert!(r.steps.iter().all(|s| s.action == Action::Absent));
        assert_eq!(r.steps[0].detail, "not registered");

        ClaudeCode.install(&e, Scope::Project, &mut Report::default());
        let mut r = Report::default();
        ClaudeCode.status(&e, Scope::Project, &mut r);
        assert!(r.steps.iter().all(|s| s.action == Action::Present));
        assert!(
            r.steps.iter().all(|s| s.detail == "current"),
            "a fresh install is current in both artifacts: {:?}",
            r.steps
        );

        // The socket moved. Both go stale, and both should: the registration
        // spawns the bridge with `--socket`, and the skill quotes that same path
        // back to the agent so "the tools are missing" has somewhere to look.
        let moved = Environment {
            socket_path: PathBuf::from("/run/user/1000/filigrio.sock"),
            ..e.clone()
        };
        let mut r = Report::default();
        ClaudeCode.status(&moved, Scope::Project, &mut r);
        assert_eq!(r.steps[0].target, "claude-code/mcp");
        assert_eq!(
            r.steps[0].detail, STALE,
            "the registration points at a socket that moved"
        );
        assert_eq!(r.steps[1].detail, STALE, "and the skill names it too");

        // The *CLI* binary moved. The skill quotes it; the registration spawns
        // the bridge and never mentions it — so the two verdicts are genuinely
        // independent rather than one answer printed twice.
        let relocated_cli = Environment {
            cli_bin: PathBuf::from("/usr/local/bin/filigrio"),
            ..e.clone()
        };
        let mut r = Report::default();
        ClaudeCode.status(&relocated_cli, Scope::Project, &mut r);
        assert_eq!(
            r.steps[0].detail, "current",
            "the registration points at the bridge, not the CLI"
        );
        assert_eq!(r.steps[1].detail, STALE);
    }

    /// An absent client is a *note*, and the artifacts are still written —
    /// never a silent skip.
    #[test]
    fn an_absent_claude_code_is_reported_and_the_files_still_land() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let mut r = Report::default();
        ClaudeCode.install(&e, Scope::Project, &mut r);
        assert!(r.notes.iter().any(|n| n.text().contains("not detected")));
        assert!(d.path().join(".mcp.json").exists());
    }
}
