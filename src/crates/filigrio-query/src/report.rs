//! Renders a [`GraphReport`] to `GRAPH_REPORT.md` — the human-facing *map* of a
//! built graph (Phase 2b, slice 2). Pure formatting, mirroring the oracle-diff's
//! `report.py` split: the numbers come from `analysis`, the prose from here.
//!
//! Deterministic: every section iterates already-sorted data, so the same graph
//! renders byte-identically (asserted in the spec).

// ADR-0041 Class F: every `.unwrap()` below is `writeln!` on a `String` buffer,
// which `std::fmt::Write` cannot fail — allowed locally (not fixed) so the
// crate-wide `unwrap_used`/`expect_used` lint (ADR-0041 Validation §2) stays a
// signal for *new* violations instead of 27 permanent, already-triaged warnings.
#![allow(clippy::unwrap_used)]

use crate::GraphReport;
use filigrio_core::EdgeTarget;
use std::collections::BTreeMap;
use std::fmt::Write;

/// Render the report as GitHub-flavoured Markdown.
pub fn render_markdown(r: &GraphReport) -> String {
    let mut out = String::new();
    let labels = community_labels(r);

    writeln!(out, "# Graph Report\n").unwrap();
    writeln!(
        out,
        "**{} nodes**, {} edges, {} communities.\n",
        r.stats.nodes, r.stats.edges, r.stats.communities
    )
    .unwrap();

    // ---- projects (the monorepo architecture map) -----------------------
    writeln!(out, "## Projects\n").unwrap();
    writeln!(
        out,
        "The project dependency graph — packages and their `depends_on` edges (ADR-0019).\n"
    )
    .unwrap();
    if r.projects.projects.is_empty() {
        writeln!(out, "_none_\n").unwrap();
    } else {
        // root → display name, so `depends_on` targets render by package name.
        let name_of: BTreeMap<&str, &str> = r
            .projects
            .projects
            .iter()
            .map(|p| (p.root.as_str(), project_label(p)))
            .collect();
        writeln!(out, "| project | files | depends on |").unwrap();
        writeln!(out, "|---|---|---|").unwrap();
        for p in &r.projects.projects {
            let deps = p
                .depends_on
                .iter()
                .map(|root| *name_of.get(root.as_str()).unwrap_or(&root.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            let deps = if deps.is_empty() {
                "—".to_string()
            } else {
                deps
            };
            writeln!(out, "| {} | {} | {} |", project_label(p), p.files, deps).unwrap();
        }
        writeln!(out).unwrap();
    }

    // ---- confidence breakdown ------------------------------------------
    writeln!(out, "## Confidence\n").unwrap();
    writeln!(out, "| confidence | edges |").unwrap();
    writeln!(out, "|---|---|").unwrap();
    for (conf, count) in &r.stats.by_confidence {
        writeln!(out, "| {conf} | {count} |").unwrap();
    }
    writeln!(out).unwrap();

    // ---- god nodes ------------------------------------------------------
    writeln!(out, "## God nodes\n").unwrap();
    writeln!(
        out,
        "The most connected abstractions (semantic degree, structural edges excluded).\n"
    )
    .unwrap();
    writeln!(out, "| # | node | kind | degree | community |").unwrap();
    writeln!(out, "|---|---|---|---|---|").unwrap();
    for (i, (node, deg)) in r.god_nodes.iter().enumerate() {
        let comm = labels
            .get(&node.id_community(r))
            .cloned()
            .unwrap_or_else(|| "—".into());
        writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            i + 1,
            node.label,
            node.kind,
            deg,
            comm
        )
        .unwrap();
    }
    writeln!(out).unwrap();

    // ---- communities ----------------------------------------------------
    writeln!(out, "## Communities\n").unwrap();
    for c in &r.communities {
        writeln!(
            out,
            "### {} — community {} ({} nodes, cohesion {:.2})\n",
            c.label,
            c.id.0,
            c.size,
            c.cohesion()
        )
        .unwrap();
        let roster = c
            .members
            .iter()
            .map(|n| n.label.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(out, "{roster}\n").unwrap();
    }

    // ---- cross-community bridges ---------------------------------------
    writeln!(out, "## Cross-community bridges\n").unwrap();
    writeln!(
        out,
        "Semantic edges whose endpoints fall in different communities — the seams.\n"
    )
    .unwrap();
    if r.bridges.is_empty() {
        writeln!(out, "_none_").unwrap();
    } else {
        for b in &r.bridges {
            let from = labels.get(&b.from).map(String::as_str).unwrap_or("—");
            let to = labels.get(&b.to).map(String::as_str).unwrap_or("—");
            let dst = match &b.edge.target {
                EdgeTarget::Node(t) => t.0.clone(),
                EdgeTarget::Symbol(s) => s.name.clone(),
            };
            writeln!(
                out,
                "- **{}** → **{}**: `{}` --{}--> `{}`",
                from, to, b.edge.source.0, b.edge.relation, dst
            )
            .unwrap();
        }
    }

    out
}

/// A project's display name: its package name, else its root (`<root>` at the
/// repo root).
fn project_label(p: &filigrio_core::ProjectNode) -> &str {
    match &p.name {
        Some(n) => n,
        None if p.root.is_empty() => "<root>",
        None => &p.root,
    }
}

/// community id → derived label, from the report's summaries.
fn community_labels(r: &GraphReport) -> BTreeMap<filigrio_core::CommunityId, String> {
    r.communities
        .iter()
        .map(|c| (c.id, c.label.clone()))
        .collect()
}

// A tiny extension so the god-node table can name each node's community without
// re-plumbing the partition through the report struct.
trait NodeCommunity {
    fn id_community(&self, r: &GraphReport) -> filigrio_core::CommunityId;
}
impl NodeCommunity for filigrio_core::Node {
    fn id_community(&self, r: &GraphReport) -> filigrio_core::CommunityId {
        r.communities
            .iter()
            .find(|c| c.members.iter().any(|m| m.id == self.id))
            .map(|c| c.id)
            .unwrap_or(filigrio_core::CommunityId(u64::MAX))
    }
}
