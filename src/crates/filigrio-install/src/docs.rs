//! `AGENTS.md` — the repository's own agent documentation (ADR-0034 §18.2).
//!
//! A plain-markdown file at the repository root, no required headings and no
//! frontmatter, read by Codex, Jules, Aider, Zed, VS Code, Cursor, Copilot, Warp,
//! Junie and others (verified against `agents.md` on 2026-07-29).
//!
//! ## Why this is a resource and not an agent
//!
//! It shipped as `--agent agents-md`, an eighth row in [`crate::clients`]'s
//! roster, and the model produced its own tell: an "agent" that registers
//! nothing, and whose `detect()` had to explain that there was nothing on the
//! machine to detect. That is a category error rather than an awkward row.
//! `AGENTS.md` is **user-owned, shared, and read by agents this build supports
//! and by agents it does not** — the reason it is edited through a
//! marker-delimited managed block ([`crate::block`]) in the first place. Which
//! editor one contributor happens to run is not what decides whether a
//! repository documents its tooling, in the same sense that it is not what
//! decides whether the repository has a README.
//!
//! So it is its own resource, `filigrio docs install | uninstall | status`,
//! alongside `agent`, `hooks` and `completions` (ADR-0038 §1 as amended by
//! ADR-0034 §17).
//!
//! ## What that costs, and what it buys
//!
//! Five of the seven agents now get a registration and no manual unless
//! `filigrio docs install` is also run. That is the same degraded-not-broken
//! state §7 already produced — the capability doc is a manual, not a dependency,
//! and the MCP tool descriptions teach the surface on their own — but it is now
//! **legible**: you wired Cursor; separately you decided whether this repository
//! carries agent documentation. Nobody has to learn that installing Claude Code
//! is how one documents Cursor.
//!
//! ## Three things this module deliberately does not have
//!
//! - **No `--global`.** `agents.md` defines the file as living at the repository
//!   root; the clients that read it look in the repository, closest file
//!   winning. A `~/AGENTS.md` is not a user-scoped version of this artifact, it
//!   is a different file that no client in ADR-0034 §3's list reads *for this
//!   repository*. (Codex additionally reads `~/.codex/AGENTS.md`
//!   — Codex's own fact, not this convention's, and `--agent codex` writes
//!   Codex's registration rather than a second copy of this doc.)
//! - **No `detect()`.** Nothing about a file convention is present or absent on
//!   a machine, and the sentence that used to say so was the model apologising
//!   for itself.
//! - **No member selector.** There is one artifact, so `docs install` just
//!   installs — the same reasoning ADR-0034 §17.2 records for `hooks` and
//!   `completions`: few, cohesive, inside the checkout, nothing outside the
//!   repository to consent to. A second artifact would arrive as a `Doc` member
//!   slice threaded through these three functions exactly as [`crate::hooks`]
//!   threads `&[Hook]`, which is why the verbs take `env` and `report` and not a
//!   struct that would have to be widened.

use crate::capability::{self, CapabilityModel};
use crate::{block, render, Action, Environment, InstallError, Report, STALE};
use std::path::PathBuf;

/// The one artifact, at the one place the convention puts it.
pub fn agents_md_path(env: &Environment) -> PathBuf {
    env.project_root.join("AGENTS.md")
}

/// The managed-block payload (the markers are [`crate::block`]'s job).
fn section(env: &Environment) -> Result<String, InstallError> {
    let hb = render::registry()?;
    // There is no registration file for this convention; the model field says so
    // rather than carrying a path that does not exist.
    let model = CapabilityModel::new(env, "(per-agent; AGENTS.md carries no MCP config)");
    capability::agents_md(&hb, &model)
}

/// Write (or refresh) the `filigrio` block in `AGENTS.md`.
///
/// Idempotent, and byte-exact around the block: everything outside the markers
/// is the user's and is never re-emitted.
pub fn install(env: &Environment, report: &mut Report) {
    let path = agents_md_path(env);
    let outcome = section(env).and_then(|body| block::upsert_file(&path, &body, &block::MARKDOWN));
    report.record("docs/agents-md", &path, outcome);
}

/// Remove the block, and the file too if the block was all it held.
pub fn uninstall(env: &Environment, report: &mut Report) {
    let path = agents_md_path(env);
    report.record(
        "docs/agents-md",
        &path,
        block::remove_file_block(&path, &block::MARKDOWN, |rest| rest.trim().is_empty()),
    );
}

/// Absent, current, or stale — worded exactly as every other family words it
/// ([`crate::STALE`]).
pub fn status(env: &Environment, report: &mut Report) {
    let path = agents_md_path(env);
    let current = section(env);
    match (crate::read_opt(&path), current) {
        (Ok(Some(text)), Ok(body)) => match block::body_of(&text, &block::MARKDOWN) {
            Some(ref have) if have.trim() == body.trim() => {
                report.step("docs/agents-md", &path, Action::Present, "current")
            }
            Some(_) => report.step("docs/agents-md", &path, Action::Present, STALE),
            None => report.step(
                "docs/agents-md",
                &path,
                Action::Absent,
                "file exists but holds no filigrio block",
            ),
        },
        (Ok(None), _) => report.step("docs/agents-md", &path, Action::Absent, "not installed"),
        (Err(e), _) | (_, Err(e)) => report.fail("docs/agents-md", &path, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(root: &std::path::Path) -> Environment {
        Environment {
            project_root: root.to_path_buf(),
            home: root.to_path_buf(),
            cli_bin: PathBuf::from("/opt/g/bin/filigrio"),
            bridge_bin: PathBuf::from("/opt/g/bin/filigrio-mcp"),
            socket_path: PathBuf::from("/run/filigrio.sock"),
            version: "0.0.1".into(),
        }
    }

    /// The headline reversibility case for a *shared* user-owned file.
    #[test]
    fn a_user_authored_agents_md_survives_install_and_uninstall_byte_exactly() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let path = d.path().join("AGENTS.md");
        let prior = "# Agents\n\n## Setup commands\n\n- `npm install`\n\n## Code style\n\nTabs.\n";
        std::fs::write(&path, prior).unwrap();

        install(&e, &mut Report::default());
        let after_install = std::fs::read_to_string(&path).unwrap();
        assert!(after_install.starts_with(prior), "user text stays on top");
        assert!(after_install.contains("## filigrio"));

        uninstall(&e, &mut Report::default());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), prior);
    }

    /// The specific case the brief calls out: the user edits *around* the block
    /// after installing.
    #[test]
    fn user_edits_around_the_block_are_preserved_on_uninstall() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let path = d.path().join("AGENTS.md");
        std::fs::write(&path, "# Agents\n\nOriginal.\n").unwrap();

        install(&e, &mut Report::default());

        let installed = std::fs::read_to_string(&path).unwrap();
        let edited = installed.replace("Original.", "Rewritten by hand.")
            + "\n## Added afterwards\n\nMore.\n";
        std::fs::write(&path, &edited).unwrap();

        uninstall(&e, &mut Report::default());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# Agents\n\nRewritten by hand.\n\n## Added afterwards\n\nMore.\n"
        );
    }

    #[test]
    fn installing_twice_leaves_exactly_one_block() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let mut r = Report::default();
        install(&e, &mut r);
        install(&e, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        let text = std::fs::read_to_string(d.path().join("AGENTS.md")).unwrap();
        assert_eq!(text.matches(block::MARKDOWN.start).count(), 1);
        assert!(r.steps.iter().any(|s| s.action == Action::Unchanged));
    }

    #[test]
    fn an_agents_md_we_created_is_removed_on_uninstall() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        install(&e, &mut Report::default());
        assert!(d.path().join("AGENTS.md").exists());
        uninstall(&e, &mut Report::default());
        assert!(!d.path().join("AGENTS.md").exists());
    }

    /// **`docs install` writes a document and nothing else** (ADR-0034 §18.2).
    ///
    /// The resource is defined by what it does not reach as much as by what it
    /// writes: no MCP registration anywhere, no agent namespace, nothing under
    /// `$HOME`. This is the whole reason it carries no `--global` — there is no
    /// scope question to consent to, so the safest thing the command can be is
    /// one that just installs.
    #[test]
    fn docs_install_writes_one_repository_file_and_touches_no_agent_namespace() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = Environment {
            home: home.path().to_path_buf(),
            ..env(repo.path())
        };

        let mut r = Report::default();
        install(&e, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        assert_eq!(r.steps.len(), 1, "one artifact: {:?}", r.steps);
        assert_eq!(r.steps[0].path, repo.path().join("AGENTS.md"));
        for stray in [
            ".claude",
            ".opencode",
            ".cursor",
            ".mcp.json",
            "opencode.json",
        ] {
            assert!(
                !repo.path().join(stray).exists(),
                "`docs install` must not write `{stray}`"
            );
        }
        assert_eq!(
            std::fs::read_dir(home.path()).unwrap().count(),
            0,
            "and nothing at all under $HOME"
        );
    }

    /// **The guidance lives in the body, and stays an HTML comment there.**
    ///
    /// It used to be spliced into the start marker, which made the delimiter
    /// move whenever the command did ([`crate::block`]'s module docs). The body
    /// is regenerated on every install, so it can carry the sentence for ever
    /// without that cost.
    ///
    /// A *comment*, though, and not prose: this file is injected verbatim into
    /// the context of every agent that reads it, so a visible "edits are
    /// overwritten" line would promote a file-maintenance instruction into the
    /// model's reading material on every request. The two assertions are the two
    /// halves — inside the block, and not visible when the markdown renders.
    #[test]
    fn the_managed_by_guidance_is_a_comment_in_the_body_not_part_of_the_delimiter() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        install(&e, &mut Report::default());
        let text = std::fs::read_to_string(d.path().join("AGENTS.md")).unwrap();

        assert!(
            !block::MARKDOWN.start.contains("managed by"),
            "the delimiter must carry no guidance: {}",
            block::MARKDOWN.start
        );
        let body = block::body_of(&text, &block::MARKDOWN).expect("a block was written");
        let first = body.lines().next().unwrap();
        assert!(
            first.starts_with("<!-- managed by") && first.ends_with("-->"),
            "the guidance must be the body's first line and an HTML comment: {first}"
        );
        assert!(
            body.lines()
                .filter(|l| !l.trim_start().starts_with("<!--"))
                .all(|l| !l.contains("overwritten")),
            "the guidance must not also appear as prose the model reads:\n{body}"
        );
    }

    #[test]
    fn status_reports_absent_then_current_then_stale() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());

        let mut r = Report::default();
        status(&e, &mut r);
        assert_eq!(r.steps[0].action, Action::Absent);

        install(&e, &mut Report::default());
        let mut r = Report::default();
        status(&e, &mut r);
        assert_eq!(r.steps[0].detail, "current");

        let path = d.path().join("AGENTS.md");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, block::upsert(&text, "stale body", &block::MARKDOWN)).unwrap();
        let mut r = Report::default();
        status(&e, &mut r);
        assert!(r.steps[0].detail.contains("stale"));
    }

    /// `status` says nothing beyond the artifact line.
    ///
    /// The adapter this replaced emitted two notes on every `install` and every
    /// `status` — one explaining that a file convention cannot be detected, one
    /// explaining that it writes no MCP registration. Both were answers to
    /// questions only the agent roster could raise. A resource that has to
    /// explain what it is not is the shape ADR-0034 §18.3 is about, so the
    /// absence is asserted rather than assumed.
    #[test]
    fn no_verb_here_explains_what_this_resource_is_not() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        for run in [
            {
                let mut r = Report::default();
                install(&e, &mut r);
                r
            },
            {
                let mut r = Report::default();
                status(&e, &mut r);
                r
            },
        ] {
            assert!(
                run.notes.is_empty(),
                "an artifact that installs cleanly has nothing to add: {:?}",
                run.notes
            );
        }
    }
}
