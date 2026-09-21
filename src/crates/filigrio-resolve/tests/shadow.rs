//! ADR-0042 Phase 1.4 — shadow mode + link-stage ledger.
//!
//! **Shadow mode** runs both link strategies over a *real* corpus (this
//! workspace's own crates, parsed by the real `RustExtractor`) and asserts the
//! resulting states are identical — the empty-divergence gate the user reviews
//! before ever flipping the default. **Ledger** (`link_stage_ledger`, `#[ignore]`
//! — run `--release --ignored`) times the incremental link stage Global vs Scoped
//! on a fixed corpus so `docs/perf/benchmarks.md` gets a before/after row.

mod common;
use common::{apply_dyn, cold_ext, delta_facets, facet_mismatches, modified, DirSource};
use filigrio_core::Extractor;
use filigrio_index::{DispatchExtractor, RustExtractor};
use filigrio_resolve::LinkScope;
use std::path::PathBuf;

/// The workspace crates dir (`crates/`), or `None` if it can't be located.
fn crates_dir() -> Option<PathBuf> {
    // CARGO_MANIFEST_DIR = .../crates/filigrio-resolve
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let crates = here.parent()?.to_path_buf();
    crates.is_dir().then_some(crates)
}

// ---- shadow: Global ≡ Scoped over the workspace corpus ----------------------

#[test]
fn shadow_global_equals_scoped_over_workspace() {
    let Some(root) = crates_dir() else {
        eprintln!("shadow: crates/ not found — skipping");
        return;
    };
    let ext = RustExtractor::new();
    let src = DirSource::load(&root);
    assert!(
        src.len() > 20,
        "expected a non-trivial corpus, got {}",
        src.len()
    );
    shadow_over(&src, &ext, 8);
}

/// The big-repo equivalence gate (ADR-0042 Phase 1.4): the same
/// empty-divergence assertion as the workspace run, over an arbitrary corpus —
/// **the check that must pass on a TS repo before the default flips to
/// `Scoped`**, since TypeScript resolution has tiers (bare-name receiver
/// fallback, import/export binding, re-export chains) the Rust fixtures never
/// exercise. `#[ignore]`d because it needs an external checkout:
///
/// ```text
/// FILIGRIO_LEDGER_ROOT=/…/next.js FILIGRIO_LEDGER_EXTS=ts,tsx,js,jsx \
/// FILIGRIO_SHADOW_SAMPLE=8 cargo test --release -p filigrio-resolve \
///   --test shadow -- --ignored --nocapture shadow_big_repo_equivalence
/// ```
#[test]
#[ignore = "big-repo equivalence gate — needs FILIGRIO_LEDGER_ROOT; run --release --ignored"]
fn shadow_big_repo_equivalence() {
    let Some(root) = std::env::var("FILIGRIO_LEDGER_ROOT")
        .ok()
        .map(PathBuf::from)
    else {
        eprintln!("shadow(big): FILIGRIO_LEDGER_ROOT unset — skipping");
        return;
    };
    let exts_raw = std::env::var("FILIGRIO_LEDGER_EXTS").unwrap_or_else(|_| "rs".into());
    let exts: Vec<&str> = exts_raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let sample: usize = std::env::var("FILIGRIO_SHADOW_SAMPLE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);

    // `DispatchExtractor` routes each file to its language backend, so this same
    // gate serves a Rust, TS/JS, or mixed corpus.
    let dispatch = DispatchExtractor::with_defaults();
    let src = DirSource::load_ext(&root, &exts);
    assert!(
        src.len() > 20,
        "expected a non-trivial corpus, got {}",
        src.len()
    );
    eprintln!(
        "shadow(big): {} ({} files, exts={exts:?}), sample={sample}",
        root.display(),
        src.len()
    );
    shadow_over(&src, &dispatch, sample);
}

/// Run the empty-divergence gate over `src`: for a spread of files, apply the
/// SAME change under both strategies from the SAME prior and diff the results.
/// Panics (with the diverging files named) if Global ≠ Scoped anywhere.
fn shadow_over(src: &DirSource, ext: &dyn Extractor, sample_n: usize) {
    // Cold prior (Global).
    let prior = cold_ext(src, ext);
    // ADR-0042 Phase 1a.4: the corpus now includes project manifests, so the
    // `workspace` facet compared below is real rather than an empty-vs-empty
    // tautology. Assert it, don't assume it.
    assert!(
        !prior.workspace.projects.is_empty(),
        "workspace is degenerate — manifests missing from the corpus, so every \
         workspace/module-resolution comparison here would be vacuous"
    );

    // Pick a spread of edited files: re-index (no-op modify) + a real edit.
    let mut targets: Vec<String> = prior
        .graph
        .nodes
        .iter()
        .filter_map(|n| n.source_file.clone())
        .collect();
    targets.sort();
    targets.dedup();
    // Sample a bounded spread of files so the shadow run stays quick but broad
    // (each edit drives a full Global + Scoped apply with clustering).
    let step = (targets.len() / sample_n.max(1)).max(1);
    let sample: Vec<String> = targets
        .iter()
        .step_by(step)
        .take(sample_n)
        .cloned()
        .collect();
    assert!(!sample.is_empty());

    let mut diverged = Vec::new();
    for f in &sample {
        // (1) idempotent re-index (content unchanged).
        let g = apply_dyn(&prior, &modified(&[f]), src, ext, LinkScope::Global);
        let s = apply_dyn(&prior, &modified(&[f]), src, ext, LinkScope::Scoped);
        let d = facet_mismatches(&delta_facets(&g), &delta_facets(&s), &[]);
        if !d.is_empty() {
            diverged.push(format!("reindex {f} [{}]", d.join(",")));
        }

        // (2) a real edit: append a private fn to the file, then diff. The probe
        // must be VALID SOURCE IN THAT FILE'S LANGUAGE — appending Rust `fn` to a
        // `.ts` file would only produce a parse error, testing nothing.
        let orig = src.get(f).unwrap();
        src.set(f, &format!("{orig}{}", probe_for(f)));
        let g = apply_dyn(&prior, &modified(&[f]), src, ext, LinkScope::Global);
        let s = apply_dyn(&prior, &modified(&[f]), src, ext, LinkScope::Scoped);
        let d = facet_mismatches(&delta_facets(&g), &delta_facets(&s), &[]);
        if !d.is_empty() {
            diverged.push(format!("edit {f} [{}]", d.join(",")));
        }
        src.set(f, &orig); // restore
    }

    assert!(
        diverged.is_empty(),
        "shadow divergence (Global ≠ Scoped) on {} of {} sampled edits: {:?}",
        diverged.len(),
        sample.len() * 2,
        diverged
    );
    eprintln!(
        "shadow: {} files, {} edits, zero Global/Scoped divergence",
        src.len(),
        sample.len() * 2
    );
}

/// A syntactically valid "add one private function" probe for the file's
/// language, so the edit actually produces a new def node (and therefore a real
/// re-resolution) instead of a parse error.
fn probe_for(path: &str) -> &'static str {
    if path.ends_with(".rs") {
        "\nfn __adr0042_probe_fn() {}\n"
    } else if path.ends_with(".py") {
        "\ndef __adr0042_probe_fn():\n    pass\n"
    } else {
        // .ts/.tsx/.js/.jsx — valid in all four.
        "\nfunction __adr0042_probe_fn() {}\n"
    }
}

// ---- ledger: link-stage wall-clock, Global vs Scoped ------------------------

#[test]
#[ignore = "perf ledger — run with `cargo test --release -p filigrio-resolve --test shadow -- --ignored --nocapture`"]
fn link_stage_ledger() {
    // Default: the workspace `crates/` (Rust). Override for a big-repo run, e.g.
    //   FILIGRIO_LEDGER_ROOT=/…/next.js FILIGRIO_LEDGER_EXTS=ts,tsx,js,jsx \
    //   FILIGRIO_LEDGER_N=8 cargo test --release -p filigrio-resolve \
    //     --test shadow -- --ignored --nocapture link_stage_ledger
    let env_root = std::env::var("FILIGRIO_LEDGER_ROOT")
        .ok()
        .map(PathBuf::from);
    let exts_raw = std::env::var("FILIGRIO_LEDGER_EXTS").unwrap_or_else(|_| "rs".into());
    let exts: Vec<&str> = exts_raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let n: u32 = std::env::var("FILIGRIO_LEDGER_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);

    let root = match env_root.or_else(crates_dir) {
        Some(r) => r,
        None => {
            eprintln!("ledger: corpus root not found — skipping");
            return;
        }
    };
    // `DispatchExtractor` routes each file to its language backend (Rust/Python/TS
    // + mock fallback), so the same harness serves the Rust workspace and a
    // TS-heavy repo; for a pure-`rs` corpus it dispatches to the same
    // `RustExtractor` the shadow test uses.
    let dispatch = DispatchExtractor::with_defaults();
    let ext: &dyn Extractor = &dispatch;
    let src = DirSource::load_ext(&root, &exts);
    assert!(!src.is_empty(), "empty corpus at {}", root.display());
    let t_cold = std::time::Instant::now();
    let prior = cold_ext(&src, ext);
    let cold_secs = t_cold.elapsed().as_secs_f64();
    let nodes = prior.graph.nodes.len();
    let edges = prior.graph.edges.len();

    // Fixed change: modify one representative mid-size file, applied N times per
    // strategy from the same prior. Extraction cost is identical across
    // strategies, so the wall-clock delta is the link stage.
    let target = prior
        .graph
        .nodes
        .iter()
        .filter_map(|n| n.source_file.clone())
        .find(|f| f.ends_with("lib.rs"))
        .or_else(|| prior.graph.nodes.iter().find_map(|n| n.source_file.clone()))
        .expect("a source file");
    let cs = modified(&[&target]);

    let time = |scope: LinkScope| {
        let t = std::time::Instant::now();
        for _ in 0..n {
            let _ = apply_dyn(&prior, &cs, &src, ext, scope);
        }
        t.elapsed().as_secs_f64() / f64::from(n)
    };
    // warm up
    let _ = apply_dyn(&prior, &cs, &src, ext, LinkScope::Global);
    let g = time(LinkScope::Global);
    let s = time(LinkScope::Scoped);

    eprintln!("=== ADR-0042 link-stage ledger ===");
    eprintln!(
        "corpus: {} ({} files, exts={exts:?})",
        root.display(),
        src.len()
    );
    eprintln!("        {nodes} nodes, {edges} edges; cold build {cold_secs:.1}s");
    eprintln!("change: modify {target}, {n} applies/strategy");
    eprintln!("Global apply : {:.3} ms/apply", g * 1e3);
    eprintln!("Scoped apply : {:.3} ms/apply", s * 1e3);
    eprintln!("speedup      : {:.2}×", g / s);
}
