//! Seed selection for retrieval (Phase 2b, slice 3, HLD §6) — the fuzzy,
//! discriminative front door to a `query`.
//!
//! A query is scored against every node by **IDF-weighted trigram overlap**:
//! break both the query and each node label into character 3-grams, and score a
//! node by the summed IDF of the trigrams it shares with the query. Trigrams
//! make it typo-tolerant (a substring stub misses `clustr`→`cluster`); IDF makes
//! it *discriminative* (a rare, meaningful trigram outweighs a boilerplate one).
//! Ranking is deterministic — score desc, then label asc.

use crate::GraphView;
use filigrio_core::Node;
use std::collections::{BTreeMap, BTreeSet};

/// Character trigrams of `s`, lowercased. Strings shorter than 3 chars yield one
/// gram (the whole string) so short symbols (`id`, `fs`) still match.
fn trigrams(s: &str) -> BTreeSet<String> {
    let chars: Vec<char> = s.to_lowercase().chars().collect();
    let mut out = BTreeSet::new();
    if chars.len() < 3 {
        if !chars.is_empty() {
            out.insert(chars.into_iter().collect());
        }
        return out;
    }
    for w in chars.windows(3) {
        out.insert(w.iter().collect());
    }
    out
}

impl GraphView {
    /// Rank nodes by relevance to `query` via IDF-weighted trigram overlap.
    /// Returns only positive-scoring nodes, sorted by score desc then label asc.
    pub fn seed_scores(&self, query: &str) -> Vec<(Node, f64)> {
        let q = trigrams(query);
        if q.is_empty() {
            return Vec::new();
        }
        let nodes = &self.state.graph.nodes;
        let n = nodes.len() as f64;

        // Per-node trigram sets + document frequency of each trigram (the corpus
        // is the node labels — the "documents").
        let node_grams: Vec<BTreeSet<String>> =
            nodes.iter().map(|nd| trigrams(&nd.label)).collect();
        let mut df: BTreeMap<&str, usize> = BTreeMap::new();
        for grams in &node_grams {
            for g in grams {
                *df.entry(g.as_str()).or_default() += 1;
            }
        }
        // Smoothed IDF: always positive, monotonically higher for rarer trigrams.
        let idf = |t: &str| -> f64 {
            let d = df.get(t).copied().unwrap_or(0) as f64;
            ((n + 1.0) / (d + 1.0)).ln() + 1.0
        };

        let mut scored: Vec<(Node, f64)> = Vec::new();
        for (nd, grams) in nodes.iter().zip(&node_grams) {
            let s: f64 = q
                .iter()
                .filter(|t| grams.contains(*t))
                .map(|t| idf(t))
                .sum();
            if s > 0.0 {
                scored.push((nd.clone(), s));
            }
        }
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.label.cmp(&b.0.label))
        });
        scored
    }
}
