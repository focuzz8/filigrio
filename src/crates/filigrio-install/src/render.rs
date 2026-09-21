//! The template layer (ADR-0034 §6, added by the 2026-07-29 amendment).
//!
//! Every *text* artifact this crate writes is a template rendered against a
//! typed model. The property that buys is narrow and specific: **adding a
//! client format is adding a template file, not editing a renderer.** The
//! model declares what is available; each template decides how to present it.
//!
//! ### Why handlebars
//!
//! - Its model is a plain `serde::Serialize` struct. `ramhorns` needs its own
//!   `#[derive(Content)]`, which makes the model an engine artifact rather than
//!   a domain type; it also describes itself as "Experimental". The `mustache`
//!   crate is unmaintained.
//! - `{{ … }}` is mustache-compatible, so the templates stay readable to anyone
//!   who has met mustache, while `{{#each}}` / `{{#if}}` / `@last` cover the
//!   presentation divergence that is already real between our two capability
//!   formats (a markdown *table* of tools in a `SKILL.md` vs a comma-joined
//!   inline list in `AGENTS.md`; a branch-flag guard present only in the
//!   `post-checkout` hook).
//!
//! ### Two settings that are load-bearing, not defaults
//!
//! - **`no_escape`.** handlebars HTML-escapes `{{ }}` by default, which would
//!   turn a Windows path's `&` or a shell script's `>` into entities inside a
//!   markdown or `sh` artifact. Every output here is markdown or shell, never
//!   HTML.
//! - **`strict_mode`.** A template naming a field the model does not have is an
//!   **error**, not an empty string. A silently-empty `{{cli_bin}}` would ship a
//!   hook that runs the empty command — the exact class of quiet breakage this
//!   project's honest-failure posture exists to prevent.
//!
//! What is deliberately *not* templated: `.mcp.json`. That artifact is **merged
//! into** a document the user owns, key by key, so it is built with
//! `serde_json` and diffed structurally ([`crate::json_entry`]). Rendering JSON
//! from a template would mean re-parsing our own output to merge it.

use crate::InstallError;
use handlebars::Handlebars;
use serde::Serialize;

/// Registered template names, named for the **artifact** they render, never the
/// agent that happens to read it (ADR-0034 §18.1): a `SKILL.md` is the portable
/// skill format that Claude Code, OpenCode, Codex, Cursor and others all read,
/// and [`crate::clients::claude_code`] and [`crate::clients::opencode`] each
/// render `capability/skill` into their own namespace — a name that said
/// `claude-code` would make the second destination read as a copy of the first
/// agent's file. `capability/agents-md` keeps its name because `AGENTS.md` *is*
/// the artifact's name.
pub const SKILL_CAPABILITY: &str = "capability/skill";
pub const AGENTS_MD_CAPABILITY: &str = "capability/agents-md";
pub const GIT_HOOK: &str = "hooks/hook.sh";
pub const COMPLETION_TRAILER: &str = "completions/trailer";

/// Build the registry. Templates are `include_str!`'d, so a shipped binary
/// carries every artifact it can write and an install never touches the network
/// or a data directory.
pub fn registry() -> Result<Handlebars<'static>, InstallError> {
    let mut hb = Handlebars::new();
    hb.register_escape_fn(handlebars::no_escape);
    hb.set_strict_mode(true);

    let templates: [(&str, &str); 4] = [
        (
            SKILL_CAPABILITY,
            include_str!("../assets/capability/skill.hbs"),
        ),
        (
            AGENTS_MD_CAPABILITY,
            include_str!("../assets/capability/agents-md.hbs"),
        ),
        (GIT_HOOK, include_str!("../assets/hooks/hook.sh.hbs")),
        (
            COMPLETION_TRAILER,
            include_str!("../assets/completions/trailer.hbs"),
        ),
    ];
    for (name, src) in templates {
        hb.register_template_string(name, src)
            .map_err(|e| InstallError::Template(format!("{name}: {e}")))?;
    }
    Ok(hb)
}

/// Render `template` against `model`.
pub fn render<M: Serialize>(
    hb: &Handlebars<'static>,
    template: &str,
    model: &M,
) -> Result<String, InstallError> {
    hb.render(template, model)
        .map_err(|e| InstallError::Template(format!("{template}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[test]
    fn every_shipped_template_compiles() {
        let hb = registry().unwrap();
        for name in [
            SKILL_CAPABILITY,
            AGENTS_MD_CAPABILITY,
            GIT_HOOK,
            COMPLETION_TRAILER,
        ] {
            assert!(hb.get_template(name).is_some(), "{name} not registered");
        }
    }

    /// A path containing `&` or a script containing `>` must survive verbatim.
    /// With handlebars' default escape fn this test fails, which is the point
    /// of pinning it.
    #[test]
    fn output_is_not_html_escaped() {
        #[derive(Serialize)]
        struct M {
            p: &'static str,
        }
        let mut hb = registry().unwrap();
        hb.register_template_string("t", "cmd {{p}} out").unwrap();
        assert_eq!(
            render(&hb, "t", &M { p: "a&b>c 'd'" }).unwrap(),
            "cmd a&b>c 'd' out"
        );
    }

    /// A field the model lacks is an error, never a silent empty string.
    #[test]
    fn a_missing_model_field_is_an_error() {
        #[derive(Serialize)]
        struct M {
            present: &'static str,
        }
        let mut hb = registry().unwrap();
        hb.register_template_string("t", "{{present}} {{absent}}")
            .unwrap();
        let err = render(&hb, "t", &M { present: "x" }).unwrap_err();
        assert!(
            matches!(err, InstallError::Template(_)),
            "expected a template error, got {err}"
        );
    }
}
