//! Token-bounded, sanitized text rendering for the MCP tools (Phase 3,
//! ADR-0006/0013). Output shape mirrors Python graphify's `serve.py` so an agent
//! sees equivalent text. Two boundary concerns live here, both transport-only:
//!
//!   * **sanitization** — every LLM-derived field (labels, paths, community
//!     names) is stripped of control chars and length-capped before it enters
//!     the model's context (ADR-0013 / graphify F-010);
//!   * **token budgeting** — a traversal's body is cut at ~3 chars/token with a
//!     marker naming how many nodes were dropped, so a big subgraph can't blow
//!     the context window.

use filigrio_core::{Confidence, Node};

/// graphify's default (`_subgraph_to_text` `token_budget=2000`).
pub const DEFAULT_TOKEN_BUDGET: usize = 2000;

/// graphify's `_subgraph_to_text` uses ~3 chars per token.
const CHARS_PER_TOKEN: usize = 3;
/// graphify's `_MAX_LABEL_LEN`.
const MAX_LABEL_LEN: usize = 256;

/// Strip control characters and cap length — the F-010 boundary (ADR-0013).
pub fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_LABEL_LEN)
        .collect()
}

/// Confidence as graphify writes it (UPPERCASE).
pub fn confidence(c: Confidence) -> &'static str {
    match c {
        Confidence::Extracted => "EXTRACTED",
        Confidence::Inferred => "INFERRED",
        Confidence::Ambiguous => "AMBIGUOUS",
    }
}

/// `NODE <label> [src=<file> loc=<loc> community=<name> id=<id>]`. The `id=` is the
/// copy-able address (ADR-0027): query_graph is a discovery path an agent uses to
/// *find* a node to address, so it must hand back the id, not just src/loc.
pub fn node_line(n: &Node, community: &str) -> String {
    format!(
        "NODE {} [src={} loc={} community={} id={}]",
        sanitize(&n.label),
        sanitize(n.source_file.as_deref().unwrap_or("")),
        sanitize(&n.loc()),
        sanitize(community),
        sanitize(&n.id.0),
    )
}

/// `EDGE <src> --<relation> [<CONF>]--> <dst>`
pub fn edge_line(src_label: &str, relation: &str, conf: Confidence, dst_label: &str) -> String {
    format!(
        "EDGE {} --{} [{}]--> {}",
        sanitize(src_label),
        sanitize(relation),
        confidence(conf),
        sanitize(dst_label),
    )
}

/// Cut `body` at the token budget (approx 3 chars/token), on a line boundary,
/// appending a marker that names how many `NODE` lines were dropped. Bodies
/// within budget are returned unchanged. Mirrors graphify's `_subgraph_to_text`
/// truncation, so the same over-budget subgraph reports the same way.
pub fn budget_cut(body: String, token_budget: usize) -> String {
    let char_budget = token_budget.saturating_mul(CHARS_PER_TOKEN);
    if body.len() <= char_budget {
        return body;
    }
    // Back off to a UTF-8 boundary, then to the last line break before it.
    let mut cap = char_budget.min(body.len());
    while cap > 0 && !body.is_char_boundary(cap) {
        cap -= 1;
    }
    let cut_at = body[..cap].rfind('\n').filter(|&i| i > 0).unwrap_or(cap);

    let total_nodes = body.lines().filter(|l| l.starts_with("NODE ")).count();
    let shown_nodes = body[..cut_at]
        .lines()
        .filter(|l| l.starts_with("NODE "))
        .count();
    let cut = total_nodes.saturating_sub(shown_nodes);
    format!(
        // No parameter is named here on purpose. This renderer serves BOTH the
        // library server (whose `query_graph` does take `relations`) and the
        // `filigrio-mcp` bridge (whose `query_graph` does NOT — it sends
        // `relations: vec![]` unconditionally), so the old `relations=["calls"]`
        // hint pointed the bridge's reader at a parameter its schema never had.
        // The relation taxonomy is taught where it is actually accepted:
        // `get_neighbors`.
        "{}\n... (truncated — {} more nodes cut by ~{}-token budget. \
         Narrow the query, or use get_node / get_neighbors for a specific symbol)",
        &body[..cut_at],
        cut,
        token_budget,
    )
}
