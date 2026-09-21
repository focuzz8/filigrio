//! OpenCode (ADR-0034 §3, verified against `https://opencode.ai/config.json` —
//! the `$schema` an OpenCode config declares for itself — on 2026-07-29).
//!
//! Two artifacts, **project-scoped by default**: `<project>/opencode.json`, an
//! entry under the top-level `mcp` key, and
//! `<project>/.opencode/skills/filigrio/SKILL.md`, the capability doc. OpenCode
//! also reads
//! `~/.config/opencode/opencode.json`; its docs are explicit that the two
//! **merge** (project overriding global on a conflicting key, searching from the
//! working directory up to the nearest git directory), so a project write is
//! additive and leaves the user's global config alone. That is the file with
//! their provider definitions and endpoints in it, and not touching it by default
//! is worth more than the marginal reach.
//!
//! ## Both scopes, because OpenCode documents both (ADR-0034 §17.1)
//!
//! With the default no longer meaning "everything", declining to write the global
//! file when the user asks for it has nothing left to protect: `--global` *is*
//! the consent. So this is one of the adapters whose
//! [`OtherScope`][super::OtherScope] is `Available` —
//! `--agent opencode --global` writes `~/.config/opencode/opencode.json`, the
//! path this module's header verified against `https://opencode.ai/config.json`
//! on 2026-07-29. The **shape is identical** at both scopes; only the root moves.
//!
//! ## …but only the registration moves (ADR-0034 §18.1)
//!
//! `--global` names the scope of the **registration**. The `SKILL.md` stays at
//! `<project>/.opencode/skills/filigrio/SKILL.md` either way, and this adapter is
//! the only one in the tree that can currently tell the difference: it is the
//! only agent with both a global scope and a skill of its own.
//!
//! A registration says *this server exists and here is how to reach it*, which
//! is true on the machine wherever you stand. The skill says *"Answer questions
//! about **this codebase** by querying **its** knowledge graph"*, which is a
//! claim about the repository you are in and false in every project that has not
//! been indexed — so `~/.config/opencode/skills/filigrio/SKILL.md`, a real entry
//! in OpenCode's own scan list, would tell every future project it has a graph.
//! The rule and its reasoning live in [`super::skill_root`], not here, because
//! the second agent to acquire both a global scope and a skill must inherit it
//! rather than copy the registration's `match scope`.
//!
//! One visible consequence, and it is honest rather than awkward: after a
//! `--global` install, `filigrio agent status --agent opencode` *without*
//! `--global` reports the skill `stale`. The file on disk names
//! `~/.config/opencode/opencode.json` as the registration to check, and a
//! project-scope install would name the project's `opencode.json` — which is
//! what `stale` means everywhere else in this crate. Re-running at the scope you
//! installed at reports `current`.
//!
//! ## Four differences from every other registration here, all of them silent
//!
//! Nothing about an MCP registration that OpenCode dislikes produces an error a
//! user sees; the server simply is not there. The differences, from
//! `$defs/McpLocalConfig`:
//!
//! | | the `mcpServers` clients | OpenCode |
//! |---|---|---|
//! | container | `mcpServers` | **`mcp`** |
//! | transport tag | `"type": "stdio"` (or absent) | **`"type": "local"`** (enum `local`\|`remote`) |
//! | invocation | `command` string **+** `args` array | **`command`, one array**: `[bin, …args]` |
//! | environment | `env` | **`environment`** |
//!
//! and the reason to write them out rather than trust a diff: `McpLocalConfig`
//! is `"additionalProperties": false`, so an `args` or an `env` we leaked in
//! from the adapter next door is a **schema violation**, not a key OpenCode
//! ignores. Required: `type`, `command`. Optional: `cwd`, `enabled`, `timeout`
//! (ms, default 5000). `ALLOWED_KEYS` / `REQUIRED_KEYS` hold that list, and
//! a test holds our entry to it.
//!
//! `opencode mcp add` exists but takes no flags — it is an interactive prompt,
//! so registration is a file write, not a shell-out.
//!
//! ## Its own skill, in its own namespace (ADR-0034 §18)
//!
//! One template rendered to two destinations — regenerated on every install and
//! removed on every uninstall, so there is no drift surface to speak of — and
//! `--agent opencode` wires OpenCode *completely*: its registration and its
//! manual, without minting a `.claude/` directory in a repository where nobody
//! runs Claude Code. The tests assert both directions of that boundary.
//!
//! That OpenCode also scans `.claude/skills/` is OpenCode's own compatibility
//! feature, not a licence for us to write into a namespace we were not given.
//! Its documented scan list is six directories and **`.opencode/skills/` is the
//! first of them** — three project-local (`.opencode/skills/`, `.claude/skills/`,
//! `.agents/skills/`) and three global (`~/.config/opencode/skills/`,
//! `~/.claude/skills/`, `~/.agents/skills/`), verified against
//! `https://opencode.ai/docs/skills` on 2026-08-08. So the skill goes where the
//! vendor puts *its own*, and among the six it is the project-local one — see
//! above.
//!
//! ## OpenCode validates the frontmatter Claude Code treats as optional
//!
//! Claude Code documents every `SKILL.md` frontmatter field as optional;
//! OpenCode **requires** `name` and `description` and *validates* them — `name`
//! must match `^[a-z0-9]+(-[a-z0-9]+)*$`, be 1–64 characters, and **equal the
//! directory holding the file**; `description` must be 1–1024 characters. A
//! violation is not an error message: the skill is dropped without a word.
//!
//! The constraint is a property of a template this adapter renders itself, and
//! [`tests::the_skill_frontmatter_this_adapter_writes_satisfies_opencodes_validation`]
//! asserts it on the rendered file rather than on any constant.
//!
//! ## Two things deliberately not done
//!
//! - **We do not add `$schema`** to a file we create. It is a valid `Config`
//!   property, but it is the user's editor affordance, not part of our
//!   registration; adding it would mean uninstall either leaves a key behind or
//!   removes one it cannot prove it wrote.
//! - **A JSONC config is refused, not rewritten.** The vendor schema sets
//!   `allowComments` and `allowTrailingCommas`, so a *valid* `opencode.json` may
//!   hold `//` comments — which `serde_json` cannot parse. That lands as
//!   [`crate::InstallError::BadJson`] naming the file and the position, the run
//!   reports it, and the user's file is untouched. Silently reformatting their
//!   comments away would be the oracle's `except JSONDecodeError` bug wearing a
//!   nicer hat. Recorded as the honest gap it is.

use super::{
    bridge_args, note_detection, report_registration, ClientId, ClientInstaller, Detection,
    OtherScope, Scope, ScopeSupport,
};
use crate::capability::{self, CapabilityModel, SERVER_NAME, SKILL_NAME};
use crate::{json_entry, render, Environment, InstallError, NoteKind, Report};
use serde_json::json;
use std::path::{Path, PathBuf};

pub struct OpenCode;

/// **`mcp`**, not `mcpServers` — the first of the four traps.
const MCP_CONTAINER: &[&str] = &["mcp"];

/// Every key `$defs/McpLocalConfig` declares. The type is
/// `"additionalProperties": false`, so this is a **closed** set: anything
/// outside it is a schema violation, not a key OpenCode ignores.
#[cfg(test)]
const ALLOWED_KEYS: &[&str] = &[
    "type",
    "command",
    "cwd",
    "environment",
    "enabled",
    "timeout",
];

/// The two keys `McpLocalConfig` requires.
#[cfg(test)]
const REQUIRED_KEYS: &[&str] = &["type", "command"];

/// Said on install *and* status, but **only for the global scope** — see
/// [`super::cursor`], where the same asymmetry is argued.
const GLOBAL_SCOPE_NOTE: &str =
    "OpenCode's registration went to ~/.config/opencode/opencode.json (user scope, because you \
     passed --global) — it is machine-local and does not travel with the repository, so every \
     teammate installs it themselves. That file also holds your provider definitions and \
     endpoints; only the filigrio entry under `mcp` was touched. The skill stayed in this \
     repository at .opencode/skills/filigrio/ and does not follow --global: a registration says a \
     server exists, which is true wherever you stand on this machine, but the skill says *this \
     codebase* has a knowledge graph, which is false in every project you have not indexed. Drop \
     --global to write the project's opencode.json instead, which OpenCode merges over the global \
     one";

impl OpenCode {
    /// The two paths OpenCode's schema documentation names. One function, so the
    /// pair cannot drift apart between install, uninstall and status.
    fn opencode_json(env: &Environment, scope: Scope) -> PathBuf {
        match scope {
            Scope::Project => env.project_root.join("opencode.json"),
            Scope::Global => env.home.join(".config/opencode/opencode.json"),
        }
    }

    /// The **first** entry in OpenCode's own scan list (ADR-0034 §18.1). Never
    /// `.claude/skills/`: that path is OpenCode reading somebody else's
    /// namespace, and inverting the direction would have `--agent opencode`
    /// create a `.claude/` directory in a repository where nobody runs Claude
    /// Code.
    ///
    /// The root is [`super::skill_root`] and **not** a `match` on `scope` — the
    /// registration moves under `--global` and the skill does not. The scope is
    /// still threaded through so the discarding happens once, where the rule is
    /// argued, rather than being a parameter this function quietly never took.
    ///
    /// The last component of the directory is [`SKILL_NAME`] and so is the
    /// `name:` in the frontmatter, because OpenCode requires them to match.
    fn skill_md(env: &Environment, requested: Scope) -> PathBuf {
        super::skill_root(env, requested)
            .join(".opencode/skills")
            .join(SKILL_NAME)
            .join("SKILL.md")
    }

    /// The rendered `SKILL.md` — the same [`capability::skill`] template
    /// [`super::claude_code`] renders, against a model whose
    /// `registration_path` names **this** agent's config.
    ///
    /// That one field is the whole difference between the two files, and it is
    /// the field that has to differ: the skill's "if the tools are missing,
    /// check …" line is only useful if it names the config the reader's own
    /// registration went into. A shared template with a per-agent model is what
    /// makes "one source, two destinations" mechanical rather than a promise.
    fn skill_body(env: &Environment, scope: Scope) -> Result<String, InstallError> {
        let hb = render::registry()?;
        let model =
            CapabilityModel::new(env, Self::opencode_json(env, scope).display().to_string());
        capability::skill(&hb, &model)
    }

    /// `$defs/McpLocalConfig`, and every field of it is one of the four traps.
    fn registration(env: &Environment) -> serde_json::Value {
        let mut command = vec![env.bridge_bin.display().to_string()];
        command.extend(bridge_args(env));
        json!({
            "type": "local",
            "command": command,
            "environment": {}
        })
    }

    /// Is `~/.config/opencode` nothing beyond what a `--global` install of this
    /// adapter leaves — at most a lone `opencode.json` holding only our
    /// `mcp.filigrio` entry?
    ///
    /// [`super::codex`], [`super::openclaw`] and [`super::hermes`] refuse to
    /// probe paths they write at all; this adapter's probe survives because the
    /// directory is also the vendor's own config home and a real install
    /// routinely holds more than our file. The one state a bare existence check
    /// cannot tell apart is the one this function names, and in that state the
    /// honest answer is [`Detection::Unknown`] — a probe that reported its own
    /// footprint as `Present` would teach every run after a `--global` install
    /// that the client is here.
    fn dir_holds_only_our_footprint(dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        let mut saw_config = false;
        for entry in entries {
            match entry {
                Ok(e) if e.file_name() == "opencode.json" => saw_config = true,
                // Anything else in the directory — the vendor's, the user's —
                // is evidence we could not have planted.
                _ => return false,
            }
        }
        if !saw_config {
            // An empty directory proves nothing in either direction.
            return true;
        }
        let Ok(text) = std::fs::read_to_string(dir.join("opencode.json")) else {
            return false;
        };
        let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else {
            return false;
        };
        root.as_object().is_some_and(|obj| {
            obj.len() == 1
                && obj
                    .get("mcp")
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|mcp| mcp.len() == 1 && mcp.contains_key(SERVER_NAME))
        })
    }
}

impl ClientInstaller for OpenCode {
    fn id(&self) -> ClientId {
        ClientId::OpenCode
    }

    fn display_name(&self) -> &'static str {
        "OpenCode"
    }

    /// §17.1's first row: the vendor documents both, so both are offered.
    fn scope_support(&self) -> ScopeSupport {
        ScopeSupport {
            default: Scope::Project,
            other: OtherScope::Available,
        }
    }

    fn detect(&self, env: &Environment) -> Detection {
        // The literal documented path. Whether OpenCode honours
        // `XDG_CONFIG_HOME` is not something we verified, and a detection that
        // guesses is worse than one that admits a miss — this is a note either
        // way, never a reason to skip.
        //
        // The directory is also where `--global` writes, so a bare existence
        // check would report our own work back to us; see
        // [`Self::dir_holds_only_our_footprint`].
        let config_dir = env.home.join(".config/opencode");
        if !config_dir.is_dir() {
            Detection::Absent(format!("no {}", config_dir.display()))
        } else if Self::dir_holds_only_our_footprint(&config_dir) {
            Detection::Unknown(format!(
                "{} holds nothing beyond what a --global install of this adapter writes, so its \
                 presence is our own footprint rather than evidence of OpenCode",
                config_dir.display()
            ))
        } else {
            Detection::Present(config_dir.display().to_string())
        }
    }

    fn install(&self, env: &Environment, scope: Scope, report: &mut Report) {
        note_detection(self.id(), self.display_name(), self.detect(env), report);
        if scope == Scope::Global {
            report.note_kind(NoteKind::UserScoped, self.id().slug(), GLOBAL_SCOPE_NOTE);
        }

        let cfg = Self::opencode_json(env, scope);
        report.record(
            "opencode/mcp",
            &cfg,
            json_entry::upsert(&cfg, MCP_CONTAINER, SERVER_NAME, Self::registration(env)),
        );

        let skill = Self::skill_md(env, scope);
        super::install_skill(
            "opencode/skill",
            &skill,
            Self::skill_body(env, scope),
            report,
        );
    }

    fn uninstall(&self, env: &Environment, scope: Scope, report: &mut Report) {
        let cfg = Self::opencode_json(env, scope);
        report.record(
            "opencode/mcp",
            &cfg,
            json_entry::remove(&cfg, MCP_CONTAINER, SERVER_NAME),
        );
        // Only the global path has a directory of ours to reclaim; the project
        // file sits at the repository root, which is nobody's to prune.
        if scope == Scope::Global {
            if let Some(dir) = cfg.parent() {
                crate::prune_empty_dirs(dir, &env.home);
            }
        }

        // `.opencode/skills/filigrio/` is wholly ours; [`super::remove_skill`]
        // holds the rule that a user's `reference.md` beside our SKILL.md keeps
        // the directory alive. It is removed at **either** scope, from the
        // repository, because that is where install put it at either scope:
        // `uninstall --global` has to reclaim both halves from wherever each
        // actually went, not from wherever the requested scope points.
        let skill = Self::skill_md(env, scope);
        super::remove_skill(env, scope, "opencode/skill", &skill, report);
    }

    fn status(&self, env: &Environment, scope: Scope, report: &mut Report) {
        note_detection(self.id(), self.display_name(), self.detect(env), report);
        if scope == Scope::Global {
            report.note_kind(NoteKind::UserScoped, self.id().slug(), GLOBAL_SCOPE_NOTE);
        }

        let cfg = Self::opencode_json(env, scope);
        report_registration(
            "opencode/mcp",
            &cfg,
            json_entry::state(&cfg, MCP_CONTAINER, SERVER_NAME, &Self::registration(env)),
            report,
        );

        let skill = Self::skill_md(env, scope);
        super::report_skill_status(
            "opencode/skill",
            &skill,
            Self::skill_body(env, scope),
            report,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::claude_code::ClaudeCode;
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

    /// All four differences at once, because all four are silent when wrong.
    #[test]
    fn install_writes_opencodes_shape_and_not_the_mcpservers_one() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        let mut r = Report::default();
        OpenCode.install(&e, Scope::Project, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        let text = std::fs::read_to_string(d.path().join("opencode.json")).unwrap();
        assert!(
            !text.contains("mcpServers"),
            "the container is `mcp`; `mcpServers` here means the Claude Code shape leaked:\n{text}"
        );
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let entry = &v["mcp"]["filigrio"];

        assert_eq!(entry["type"], "local", "not \"stdio\"");
        assert_eq!(
            entry["command"],
            json!(["/opt/g/bin/filigrio-mcp", "--socket", "/run/filigrio.sock"]),
            "`command` is one array of binary + args, not a string plus `args`"
        );
        assert!(entry["args"].is_null(), "there is no `args` key here");
        assert!(entry["env"].is_null(), "the key is `environment`");
        assert!(entry["environment"].is_object());
    }

    /// `McpLocalConfig` is `additionalProperties: false`, so a stray key is a
    /// violation rather than something ignored. This is the guard that says so.
    #[test]
    fn the_entry_uses_only_keys_mcplocalconfig_declares() {
        let d = tempfile::tempdir().unwrap();
        let v = OpenCode::registration(&env(d.path(), d.path()));
        let obj = v.as_object().unwrap();

        for k in obj.keys() {
            assert!(
                ALLOWED_KEYS.contains(&k.as_str()),
                "`{k}` is not a McpLocalConfig property; the type is additionalProperties:false, \
                 so this is a schema violation, not an ignored key"
            );
        }
        for req in REQUIRED_KEYS {
            assert!(obj.contains_key(*req), "`{req}` is required");
        }
    }

    /// **The defect ADR-0034 §18 exists to fix, asserted by path.**
    ///
    /// `--agent opencode` alone must wire OpenCode *completely* — its
    /// registration and its manual — and must not touch one byte of any other
    /// agent's namespace. Before §18 this run wrote `opencode.json` and nothing
    /// else, and the only way to get OpenCode a capability doc was to install
    /// Claude Code, which minted a `.claude/` directory in a repository where
    /// nobody uses it.
    ///
    /// The negative half is the load-bearing one, and it is asserted on the
    /// **directory** rather than on the `SKILL.md` inside it: a run that created
    /// `.claude/` and left it empty would still have made the decision this test
    /// exists to forbid.
    #[test]
    fn installing_opencode_writes_opencodes_own_skill_and_creates_no_claude_directory() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        let mut r = Report::default();
        OpenCode.install(&e, Scope::Project, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        // The negative first, deliberately: it is the defect being guarded, so
        // it is the sentence a regression should print. A positive assertion
        // ahead of it fails first and reports "the skill is missing" for a build
        // whose actual mistake was writing that skill into `.claude/`.
        assert!(
            !d.path().join(".claude").exists(),
            "wiring OpenCode must not create a Claude Code namespace; `.claude/` holds: {:?}",
            std::fs::read_dir(d.path().join(".claude"))
                .map(|it| it
                    .filter_map(Result::ok)
                    .map(|e| e.file_name())
                    .collect::<Vec<_>>())
                .unwrap_or_default()
        );
        assert!(
            d.path().join("opencode.json").is_file(),
            "OpenCode's registration"
        );
        assert!(
            d.path()
                .join(".opencode/skills/filigrio/SKILL.md")
                .is_file(),
            "OpenCode's own skill, at the first path in OpenCode's own scan list"
        );
    }

    /// The mirror, so the property is a boundary rather than a one-way rule:
    /// wiring Claude Code writes `.claude/` and does not reach into OpenCode's
    /// namespace either. Without this, "each agent writes only its own files"
    /// would be pinned in one direction and merely believed in the other.
    #[test]
    fn installing_claude_code_writes_its_own_namespace_and_creates_no_opencode_directory() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        let mut r = Report::default();
        ClaudeCode.install(&e, Scope::Project, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        assert!(d.path().join(".claude/skills/filigrio/SKILL.md").is_file());
        assert!(
            !d.path().join(".opencode").exists(),
            "wiring Claude Code must not create an OpenCode namespace"
        );
        assert!(!d.path().join("opencode.json").exists());
    }

    /// **One template, two destinations** (ADR-0034 §18.1) — asserted on the
    /// bytes, because "rendered from the same template" is a claim about the
    /// output and not about which function was called.
    ///
    /// The two files must agree everywhere the model agrees and differ *only*
    /// where it should: `registration_path`, which each skill quotes so its
    /// reader has somewhere to look when the tools are missing. Line-by-line
    /// rather than by a substring search, so a second divergence that crept in
    /// anywhere else in the document is caught by the same assertion.
    #[test]
    fn both_skills_render_from_one_template_and_differ_only_in_the_registration_they_name() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        ClaudeCode.install(&e, Scope::Project, &mut Report::default());
        OpenCode.install(&e, Scope::Project, &mut Report::default());

        let claude =
            std::fs::read_to_string(d.path().join(".claude/skills/filigrio/SKILL.md")).unwrap();
        let opencode =
            std::fs::read_to_string(d.path().join(".opencode/skills/filigrio/SKILL.md")).unwrap();

        assert_ne!(claude, opencode, "they name different registrations");

        let differing: Vec<(&str, &str)> = claude
            .lines()
            .zip(opencode.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(
            claude.lines().count(),
            opencode.lines().count(),
            "one template means one shape:\n{claude}\n---\n{opencode}"
        );
        assert_eq!(
            differing.len(),
            1,
            "exactly one line may differ, and it is the registration path; got {differing:#?}"
        );
        let (in_claude, in_opencode) = differing[0];
        assert!(
            in_claude.contains(".mcp.json") && in_opencode.contains("opencode.json"),
            "the differing line must be the one naming each agent's own registration: \
             {in_claude:?} vs {in_opencode:?}"
        );
    }

    /// OpenCode **validates** the `SKILL.md` frontmatter Claude Code documents
    /// as optional: `name` must match `^[a-z0-9]+(-[a-z0-9]+)*$`, be 1–64 chars
    /// and equal the containing directory's name, and `description` must be
    /// 1–1024 chars — or the skill is ignored without a word.
    ///
    /// It is asserted on the file **this adapter now writes** rather than on the
    /// Claude Code adapter's (ADR-0034 §18): the constraint did not disappear
    /// when OpenCode got its own skill, it stopped being a cross-adapter hazard
    /// and became a property of a template this adapter renders itself. Still
    /// asserted on the rendered bytes and never on a constant, so an edit to
    /// `skill.hbs` cannot quietly make every OpenCode install a no-op.
    #[test]
    fn the_skill_frontmatter_this_adapter_writes_satisfies_opencodes_validation() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        OpenCode.install(&e, Scope::Project, &mut Report::default());

        let skill = d.path().join(".opencode/skills/filigrio/SKILL.md");
        let text = std::fs::read_to_string(&skill).unwrap();
        let fm = frontmatter(&text).unwrap_or_else(|| {
            panic!(
                "OpenCode *requires* SKILL.md frontmatter; the rendered file starts:\n{}",
                &text[..text.len().min(120)]
            )
        });

        let name = fm
            .iter()
            .find(|(k, _)| k == "name")
            .map(|(_, v)| v.as_str())
            .expect("OpenCode requires a `name` in SKILL.md frontmatter");
        assert!(
            is_opencode_skill_name(name),
            "`name: {name}` does not match OpenCode's ^[a-z0-9]+(-[a-z0-9]+)*$"
        );
        assert!((1..=64).contains(&name.chars().count()), "1–64 chars");

        let description = fm
            .iter()
            .find(|(k, _)| k == "description")
            .map(|(_, v)| v.as_str())
            .expect("OpenCode requires a `description` in SKILL.md frontmatter");
        assert!(
            (1..=1024).contains(&description.chars().count()),
            "description must be 1–1024 chars, got {}",
            description.chars().count()
        );

        // OpenCode requires `name` to **equal the directory holding the file**,
        // so this reads the directory off the installed path rather than
        // comparing two constants to each other: a `SKILL_NAME` that changed in
        // only one of the two places is exactly the silent drop this guards.
        let dir = skill
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .expect("the skill lives in a named directory");
        assert_eq!(
            name, dir,
            "OpenCode matches `name` against the directory name; a mismatch drops the skill silently"
        );
    }

    /// `^[a-z0-9]+(-[a-z0-9]+)*$`, spelled out rather than pulled in as a regex
    /// dependency for one pattern.
    fn is_opencode_skill_name(s: &str) -> bool {
        !s.is_empty()
            && s.split('-').all(|seg| {
                !seg.is_empty()
                    && seg
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            })
    }

    /// The `key: value` pairs of a leading `---` block, or `None` if there is
    /// no frontmatter at all.
    fn frontmatter(text: &str) -> Option<Vec<(String, String)>> {
        let rest = text.strip_prefix("---\n")?;
        let end = rest.find("\n---\n")?;
        Some(
            rest[..end]
                .lines()
                .filter_map(|l| l.split_once(": "))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .collect(),
        )
    }

    #[test]
    fn install_is_idempotent_and_uninstall_leaves_nothing() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());

        let mut r = Report::default();
        OpenCode.install(&e, Scope::Project, &mut r);
        OpenCode.install(&e, Scope::Project, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        assert!(r.steps.iter().any(|s| s.action == Action::Unchanged));

        let mut r = Report::default();
        OpenCode.uninstall(&e, Scope::Project, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        assert!(!d.path().join("opencode.json").exists());
        assert!(
            !d.path().join(".opencode/skills/filigrio").exists(),
            "the skill directory is wholly ours, so an empty one is pruned too"
        );
    }

    /// The sibling of [`super::super::claude_code`]'s own guard: a file the user
    /// put in our skill directory keeps the directory alive. Uninstall removes
    /// what install added, never the folder's other contents.
    #[test]
    fn uninstall_keeps_a_skill_directory_the_user_added_to() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        OpenCode.install(&e, Scope::Project, &mut Report::default());

        let extra = d.path().join(".opencode/skills/filigrio/reference.md");
        std::fs::write(&extra, "mine\n").unwrap();

        OpenCode.uninstall(&e, Scope::Project, &mut Report::default());
        assert!(extra.exists(), "the user's file must survive");
    }

    /// **`--global` moves the registration and leaves the skill in the
    /// repository** (ADR-0034 §18.1).
    ///
    /// §17.1's gate is a rule about **reach** — a run without `--global` cannot
    /// write outside the repository — and a skill that stays inside the
    /// repository cannot violate it in either direction. The alternative puts a
    /// document opening *"Answer questions about this codebase by querying its
    /// knowledge graph"* into `~/.config/opencode/skills/`, where OpenCode
    /// loads it in every project on the machine — a false claim installed once
    /// and made in perpetuity.
    ///
    /// The `$HOME` assertion is on the **skills directory**, not on the
    /// `SKILL.md`: a run that created `~/.config/opencode/skills/filigrio/` and
    /// left it empty would still have made the decision this test forbids.
    #[test]
    fn a_global_install_puts_the_registration_under_home_and_the_skill_in_the_repository() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(repo.path(), home.path());

        let mut r = Report::default();
        OpenCode.install(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        assert!(
            home.path().join(".config/opencode/opencode.json").is_file(),
            "the registration is an availability declaration and follows --global"
        );
        assert!(
            !home.path().join(".config/opencode/skills").exists(),
            "a skill claiming *this* repository has a graph must never be installed \
             machine-wide; $HOME holds: {:?}",
            std::fs::read_dir(home.path().join(".config/opencode"))
                .map(|it| it
                    .filter_map(Result::ok)
                    .map(|e| e.file_name())
                    .collect::<Vec<_>>())
                .unwrap_or_default()
        );
        assert!(
            repo.path()
                .join(".opencode/skills/filigrio/SKILL.md")
                .is_file(),
            "the skill is a claim about this checkout, so it stays in this checkout"
        );
        assert!(
            !repo.path().join("opencode.json").exists(),
            "…and the registration did not also land in the repository"
        );

        // Uninstall at the same scope reclaims both halves from wherever each
        // actually went — which is the half of this property a one-sided
        // uninstall would leave on disk for ever.
        OpenCode.uninstall(&e, Scope::Global, &mut Report::default());
        assert!(!home.path().join(".config/opencode/opencode.json").exists());
        assert!(
            !repo.path().join(".opencode/skills/filigrio").exists(),
            "`uninstall --global` must remove the repository-side skill it wrote"
        );
    }

    /// The headline safety case. A real `opencode.json` is not a config we
    /// generated — it holds provider definitions and endpoints — so the promise
    /// is that our key goes in, comes out, and the file is byte-identical to
    /// what the user had.
    #[test]
    fn a_users_opencode_config_survives_install_and_uninstall_byte_exactly() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        let p = d.path().join("opencode.json");
        // Shaped like a real one: `$schema` first, a provider block with an
        // endpoint, an array, and an existing `mcp` server of their own.
        let prior = concat!(
            "{\n",
            "  \"$schema\": \"https://opencode.ai/config.json\",\n",
            "  \"share\": \"disabled\",\n",
            "  \"autoupdate\": false,\n",
            "  \"disabled_providers\": [\n",
            "    \"exa\",\n",
            "    \"ollama\"\n",
            "  ],\n",
            "  \"provider\": {\n",
            "    \"ollama\": {\n",
            "      \"name\": \"llamacpp (local)\",\n",
            "      \"npm\": \"@ai-sdk/openai-compatible\",\n",
            "      \"options\": {\n",
            "        \"baseURL\": \"http://localhost:9292/v1\"\n",
            "      }\n",
            "    }\n",
            "  },\n",
            "  \"mcp\": {\n",
            "    \"sentry\": {\n",
            "      \"type\": \"local\",\n",
            "      \"command\": [\n",
            "        \"npx\",\n",
            "        \"-y\",\n",
            "        \"@sentry/mcp-server\"\n",
            "      ]\n",
            "    }\n",
            "  }\n",
            "}\n"
        );
        std::fs::write(&p, prior).unwrap();

        OpenCode.install(&e, Scope::Project, &mut Report::default());
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("\"filigrio\""));
        assert!(after.contains("\"sentry\""), "their server survives");
        assert!(
            after.contains("http://localhost:9292/v1"),
            "their endpoint survives"
        );

        OpenCode.uninstall(&e, Scope::Project, &mut Report::default());
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            prior,
            "a user's real config must come back byte for byte"
        );
    }

    /// The same exercise against a *real* config, opt-in because it needs one:
    /// `FILIGRIO_REAL_OPENCODE_CONFIG=~/.config/opencode/opencode.json cargo test`.
    /// It copies the file to a scratch directory first — the real one is never
    /// opened for writing.
    #[test]
    fn a_real_opencode_config_round_trips_when_one_is_pointed_at() {
        let Some(src) = std::env::var_os("FILIGRIO_REAL_OPENCODE_CONFIG") else {
            return;
        };
        let prior = std::fs::read_to_string(&src)
            .unwrap_or_else(|e| panic!("FILIGRIO_REAL_OPENCODE_CONFIG is unreadable: {e}"));

        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        let p = d.path().join("opencode.json");
        std::fs::write(&p, &prior).unwrap();

        let mut r = Report::default();
        OpenCode.install(&e, Scope::Project, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        OpenCode.uninstall(&e, Scope::Project, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            prior,
            "a real config did not come back byte-exact"
        );
    }

    /// A valid `opencode.json` may be JSONC — the vendor schema sets
    /// `allowComments`. We cannot parse that, so we refuse and say so; the one
    /// thing we must never do is drop the user's comments on the floor.
    #[test]
    fn a_jsonc_config_is_refused_with_its_path_not_rewritten() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());
        let p = d.path().join("opencode.json");
        let prior = "{\n  // my endpoints\n  \"share\": \"disabled\"\n}\n";
        std::fs::write(&p, prior).unwrap();

        let mut r = Report::default();
        OpenCode.install(&e, Scope::Project, &mut r);

        assert!(!r.is_ok(), "a config we cannot parse is a reported failure");
        assert!(r.failures[0].reason.contains("opencode.json"));
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            prior,
            "the user's comments must survive being refused"
        );
    }

    /// **The note that explained the old shape is gone, and its absence is the
    /// property** (ADR-0034 §18.3).
    ///
    /// This adapter used to print a paragraph on every install and every status
    /// saying its capability doc was somebody else's file and naming the other
    /// command to run. It writes its own doc now, so that paragraph is not
    /// stale prose to reword — it is a sentence with nothing left to say, and a
    /// report that still carried it would be telling the user to go and install
    /// Claude Code for a file that is already on their disk.
    ///
    /// Asserted on both verbs, because the note was emitted by both.
    #[test]
    fn neither_verb_still_tells_the_user_another_adapter_owns_this_skill() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path(), d.path());

        for run in [
            {
                let mut r = Report::default();
                OpenCode.install(&e, Scope::Project, &mut r);
                r
            },
            {
                let mut r = Report::default();
                OpenCode.status(&e, Scope::Project, &mut r);
                r
            },
        ] {
            for note in &run.notes {
                assert!(
                    !note.text().contains("claude-code"),
                    "OpenCode writes its own skill; nothing should still point at another \
                     adapter: {note:?}"
                );
                assert!(
                    !note.text().contains(".claude/skills"),
                    "and nothing should still name another agent's namespace: {note:?}"
                );
            }
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
        OpenCode.status(&e, Scope::Project, &mut r);
        assert_eq!(r.steps[0].action, Action::Absent);
        assert_eq!(r.steps[0].detail, "not registered");

        OpenCode.install(&e, Scope::Project, &mut Report::default());
        let mut r = Report::default();
        OpenCode.status(&e, Scope::Project, &mut r);
        assert_eq!(r.steps[0].action, Action::Present);
        assert_eq!(r.steps[0].detail, "current");

        // The user re-ran with a different `--socket`. Nothing on disk changed;
        // what install *would* write did.
        let moved = Environment {
            socket_path: PathBuf::from("/run/user/1000/filigrio.sock"),
            ..e.clone()
        };
        let mut r = Report::default();
        OpenCode.status(&moved, Scope::Project, &mut r);
        assert_eq!(r.steps[0].action, Action::Present);
        assert_eq!(r.steps[0].detail, STALE);

        // And the fix the message names actually works.
        OpenCode.install(&moved, Scope::Project, &mut Report::default());
        let mut r = Report::default();
        OpenCode.status(&moved, Scope::Project, &mut r);
        assert_eq!(r.steps[0].detail, "current");
    }

    /// The probe must not report our own `--global` footprint as the vendor —
    /// the trap [`super::super::codex`], [`super::super::openclaw`] and
    /// [`super::super::hermes`] refuse by not probing at all, and which this
    /// adapter's probe has to survive because `~/.config/opencode` is both the
    /// vendor's config home and a directory `--global` creates.
    #[test]
    fn a_global_install_does_not_teach_detection_to_report_present() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(repo.path(), home.path());

        assert!(
            matches!(OpenCode.detect(&e), Detection::Absent(_)),
            "nothing exists yet, so absence is a real observation"
        );

        OpenCode.install(&e, Scope::Global, &mut Report::default());
        assert!(
            matches!(OpenCode.detect(&e), Detection::Unknown(_)),
            "after a --global install the directory holds only our own registration, \
             which is not evidence of OpenCode: {:?}",
            OpenCode.detect(&e)
        );

        // Anything of the vendor's or the user's beside our file is evidence
        // we could not have planted.
        std::fs::write(home.path().join(".config/opencode/auth.json"), "{}\n").unwrap();
        assert!(
            matches!(OpenCode.detect(&e), Detection::Present(_)),
            "a directory holding more than our footprint is a real detection"
        );
    }

    #[test]
    fn an_absent_opencode_is_reported_and_the_file_still_lands() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut r = Report::default();
        OpenCode.install(&env(d.path(), home.path()), Scope::Project, &mut r);
        assert!(r.notes.iter().any(|n| n.text().contains("not detected")));
        assert!(d.path().join("opencode.json").exists());
    }
}
