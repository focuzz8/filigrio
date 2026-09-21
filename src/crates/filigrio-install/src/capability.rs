//! The capability doc — one canonical source, rendered per client (ADR-0034 §4).
//!
//! There are two halves, and the split is what makes "one truth" mechanical
//! rather than aspirational:
//!
//! - **Prose** — `assets/capability/body.md`, the addressing grammar, the
//!   relation vocabulary, the unresolved-edge contract, the honesty rules
//!   (ADR-0027/0029/0030). One file, `include_str!`'d, injected verbatim into
//!   every client template.
//! - **The tool list** — [`TOOLS`], *data*, so each template renders it in its
//!   own shape (a markdown table in a `SKILL.md`, an inline list in
//!   `AGENTS.md`) without the prose forking.
//!
//! A surface change — a new tool, ADR-0031's `semantic_search` — is one edit
//! here and every client updates.
//!
//! There are **two templates and four destinations**: [`skill`] renders the
//! `SKILL.md` that Claude Code and OpenCode each install into their own
//! namespace (ADR-0034 §18.1), and [`agents_md`] renders the managed-block body
//! of the repository's `AGENTS.md` ([`crate::docs`]). What varies between the
//! two skills is the *model*, never the template.

use crate::render::{self, AGENTS_MD_CAPABILITY, SKILL_CAPABILITY};
use crate::{Environment, InstallError};
use handlebars::Handlebars;
use serde::Serialize;

/// The canonical prose. Authored once; never per client.
pub const BODY: &str = include_str!("../assets/capability/body.md");

/// One MCP tool as the templates see it.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDoc {
    pub name: &'static str,
    pub summary: &'static str,
}

/// The `filigrio-mcp` bridge's tool surface.
///
/// **Source of truth is `filigrio-client-mcp`'s `handle_tools_list`**; this is a
/// documentation-side copy, because taking a real dependency would drag
/// `filigrio-core` — the engine's domain model — into the engine-free CLI's
/// link graph (ADR-0032f §1).
///
/// `filigrio-client-mcp/tests/capability_doc_drift.rs` spawns the bridge, asks
/// it for its own `tools/list`, and fails if the two lists disagree — so the
/// copy cannot rot quietly. The guard lives over there, and points this way
/// through a **dev**-dependency, precisely so this crate keeps linking nothing:
/// `tests/dependency_hygiene.rs` asserts that, and dev-dependencies are not in
/// the shipped link graph.
pub const TOOLS: &[ToolDoc] = &[
    ToolDoc {
        name: "graph_stats",
        summary: "size and confidence mix — orient here first",
    },
    ToolDoc {
        name: "project_graph",
        summary: "projects in the workspace and what depends on what",
    },
    ToolDoc {
        name: "god_nodes",
        summary: "the most-connected symbols (the hubs)",
    },
    ToolDoc {
        name: "query_graph",
        summary: "search the graph by topic or symbol name",
    },
    ToolDoc {
        name: "get_node",
        summary: "one symbol by id, or the candidates sharing a label",
    },
    ToolDoc {
        name: "get_neighbors",
        summary: "a symbol's edges; `direction: in` = callers, `out` = callees",
    },
    ToolDoc {
        name: "shortest_path",
        summary: "how one symbol reaches another",
    },
    ToolDoc {
        name: "list_communities",
        summary: "the clusters, largest first — the only source of a community id",
    },
    ToolDoc {
        name: "get_community",
        summary: "one cluster's members, by that id",
    },
];

/// Everything a capability template may name. The model *is* the contract
/// between Rust and the templates: a template asks for a field, this struct
/// supplies it, and `strict_mode` turns a typo into an error.
#[derive(Debug, Clone, Serialize)]
pub struct CapabilityModel {
    /// Human product name for headings.
    pub product: String,
    /// The MCP server name as registered in the client's config.
    pub server_name: String,
    /// Skill directory / display name, where the client has such a concept.
    pub skill_name: String,
    /// The canonical prose ([`BODY`]).
    pub body: String,
    pub tools: Vec<ToolDoc>,
    pub cli_bin: String,
    pub bridge_bin: String,
    pub socket_path: String,
    pub project_root: String,
    /// Where this client's MCP registration was written — quoted back to the
    /// agent so "the tools are missing" has somewhere to look.
    pub registration_path: String,
    pub version: String,
}

/// The MCP server name every adapter registers under. One name across clients
/// so a user reading two configs sees the same thing.
pub const SERVER_NAME: &str = "filigrio";
/// The skill / section name.
pub const SKILL_NAME: &str = "filigrio";
pub const PRODUCT: &str = "filigrio";

impl CapabilityModel {
    pub fn new(env: &Environment, registration_path: impl Into<String>) -> Self {
        Self {
            product: PRODUCT.into(),
            server_name: SERVER_NAME.into(),
            skill_name: SKILL_NAME.into(),
            body: BODY.trim_end().to_string(),
            tools: TOOLS.to_vec(),
            cli_bin: env.cli_bin.display().to_string(),
            bridge_bin: env.bridge_bin.display().to_string(),
            socket_path: env.socket_path.display().to_string(),
            project_root: env.project_root.display().to_string(),
            registration_path: registration_path.into(),
            version: env.version.clone(),
        }
    }
}

/// Render a `SKILL.md`.
///
/// Two agents call this — [`crate::clients::claude_code`] writing
/// `.claude/skills/filigrio/SKILL.md`, [`crate::clients::opencode`] writing
/// `.opencode/skills/filigrio/SKILL.md` (ADR-0034 §18.1) — and the **model** is
/// what differs between them, not the template:
/// [`CapabilityModel::registration_path`] names that agent's own registration,
/// so the two rendered files differ in one line and agree everywhere else.
pub fn skill(hb: &Handlebars<'static>, model: &CapabilityModel) -> Result<String, InstallError> {
    render::render(hb, SKILL_CAPABILITY, model)
}

/// Render the `AGENTS.md` section body (the managed-block payload; the markers
/// are [`crate::block`]'s job).
pub fn agents_md(
    hb: &Handlebars<'static>,
    model: &CapabilityModel,
) -> Result<String, InstallError> {
    render::render(hb, AGENTS_MD_CAPABILITY, model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn env() -> Environment {
        Environment {
            project_root: PathBuf::from("/repo"),
            home: PathBuf::from("/home/u"),
            cli_bin: PathBuf::from("/opt/filigrio/bin/filigrio"),
            bridge_bin: PathBuf::from("/opt/filigrio/bin/filigrio-mcp"),
            socket_path: PathBuf::from("/run/user/1000/filigrio-daemon.sock"),
            version: "0.0.1".into(),
        }
    }

    /// The ADR-0034 §4 property, asserted rather than asserted-about: the one
    /// canonical prose reaches *both* client formats, so a single edit to
    /// `body.md` updates every client.
    #[test]
    fn one_source_reaches_every_client_format() {
        let hb = render::registry().unwrap();
        let m = CapabilityModel::new(&env(), ".mcp.json");

        let claude = skill(&hb, &m).unwrap();
        let agents = agents_md(&hb, &m).unwrap();

        // A distinctive sentence from the canonical body, in both renderings.
        let probe = "`unresolved: 70 by-name caller(s)`";
        assert!(
            claude.contains(probe),
            "the SKILL.md lost the canonical body"
        );
        assert!(
            agents.contains(probe),
            "the AGENTS.md section lost the canonical body"
        );

        // And every tool reaches both, from the one list.
        for t in TOOLS {
            assert!(
                claude.contains(t.name),
                "the SKILL.md is missing {}",
                t.name
            );
            assert!(
                agents.contains(t.name),
                "the AGENTS.md section is missing {}",
                t.name
            );
        }
    }

    /// The two formats genuinely differ in *presentation* — this is the
    /// per-client variation the template layer exists to absorb.
    #[test]
    fn the_two_client_formats_differ_in_shape_not_in_content() {
        let hb = render::registry().unwrap();
        let m = CapabilityModel::new(&env(), ".mcp.json");
        let claude = skill(&hb, &m).unwrap();
        let agents = agents_md(&hb, &m).unwrap();

        // A SKILL.md gets YAML frontmatter and a markdown table.
        assert!(claude.starts_with("---\nname: filigrio\n"));
        assert!(claude.contains("| Tool | What it answers |"));
        assert!(claude.contains("| `god_nodes` |"));

        // AGENTS.md gets a `## filigrio` section and an inline, comma-joined
        // list with no trailing separator (the `@last` case).
        assert!(agents.contains("## filigrio"));
        assert!(!agents.contains("| Tool |"));
        assert!(
            agents.contains("`get_community` (one cluster's members, by that id).\n"),
            "the last tool must end the sentence, not the separator:\n{agents}"
        );
    }

    /// Absolute binary paths, not bare names: a client spawning `filigrio-mcp`
    /// from a GUI process may have no useful `PATH`.
    #[test]
    fn renderings_carry_the_absolute_bridge_path() {
        let hb = render::registry().unwrap();
        let m = CapabilityModel::new(&env(), ".mcp.json");
        assert!(skill(&hb, &m)
            .unwrap()
            .contains("/opt/filigrio/bin/filigrio-mcp"));
        assert!(agents_md(&hb, &m)
            .unwrap()
            .contains("/opt/filigrio/bin/filigrio-mcp"));
    }
}
