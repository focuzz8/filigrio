//! ADR-0042 **Phase 1b.1 — profile first**.
//!
//! `docs/perf/benchmarks.md` §5c reports a ~4.1 s fixed per-apply cost at
//! next.js scale, of which the global link stage is ~2.0 s; the remaining
//! ~2.1 s was attributed to derived-index rebuilds and clustering *by reading
//! the code*. This harness replaces that inference with a measurement: it runs
//! the same apply the ledger times, but through
//! [`filigrio_core::profile::capture`], so every stage of
//! `Engine::apply_with_scope` reports its own wall-clock — plus the store's
//! `apply_delta`/`snapshot`, which live outside the engine entirely.
//!
//! ```text
//! # this workspace (fast smoke run)
//! cargo test --release -p filigrio-resolve --test apply_profile -- --ignored --nocapture
//!
//! # next.js
//! FILIGRIO_LEDGER_ROOT=/…/next.js FILIGRIO_LEDGER_EXTS=ts,tsx,js,jsx \
//! FILIGRIO_PROFILE_N=3 cargo test --release -p filigrio-resolve \
//!   --test apply_profile -- --ignored --nocapture
//! ```
//!
//! | env | default | meaning |
//! |---|---|---|
//! | `FILIGRIO_LEDGER_ROOT` | this workspace's `crates/` | corpus root |
//! | `FILIGRIO_LEDGER_EXTS` | `rs` | corpus extensions (manifests always included) |
//! | `FILIGRIO_PROFILE_N` | 3 | applies per (scope, changeset) cell |
//! | `FILIGRIO_PROFILE_STORE` | unset | also time `FsStore` (serde + disk) per apply |

mod common;
use common::{apply_dyn, cold_ext, modified, DirSource};
use filigrio_core::profile::{self, Stage};
use filigrio_core::{ChangeSet, Extractor, GraphState, GraphStore, Source};
use filigrio_index::DispatchExtractor;
use filigrio_resolve::{ClusterConfig, Engine, LinkScope};
use filigrio_store::MemoryStore;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn crates_dir() -> Option<PathBuf> {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let crates = here.parent()?.to_path_buf();
    crates.is_dir().then_some(crates)
}

/// Mean per-stage duration over `runs` captures, plus the mean total wall-clock
/// and the mean *unaccounted* remainder (wall-clock minus the depth-0 stages) —
/// the number that says whether the instrumentation actually explains the apply.
struct Breakdown {
    stages: BTreeMap<&'static str, Duration>,
    /// Sub-stages (depth > 0) — reported separately so they never double-count
    /// against the top-level rows that contain them.
    nested: BTreeMap<&'static str, Duration>,
    order: Vec<&'static str>,
    wall: Duration,
    accounted: Duration,
    runs: u32,
}

impl Breakdown {
    fn collect(runs: u32, mut f: impl FnMut() -> (Vec<Stage>, Duration)) -> Self {
        let mut totals: BTreeMap<&'static str, Duration> = BTreeMap::new();
        let mut nested: BTreeMap<&'static str, Duration> = BTreeMap::new();
        let mut order: Vec<&'static str> = Vec::new();
        let (mut wall, mut acc) = (Duration::ZERO, Duration::ZERO);
        for _ in 0..runs {
            let (stages, w) = f();
            acc += profile::accounted(&stages);
            wall += w;
            for (name, depth, d) in stages {
                if depth != 0 {
                    *nested.entry(name).or_default() += d;
                    continue;
                }
                if !order.contains(&name) {
                    order.push(name);
                }
                *totals.entry(name).or_default() += d;
            }
        }
        Breakdown {
            stages: totals,
            nested,
            order,
            wall,
            accounted: acc,
            runs,
        }
    }

    fn per_run(&self, d: Duration) -> f64 {
        d.as_secs_f64() / f64::from(self.runs)
    }

    fn report(&self, label: &str) {
        let wall = self.per_run(self.wall);
        eprintln!("\n--- {label}: {:.1} ms/apply ---", wall * 1e3);
        eprintln!("{:<20} {:>10} {:>7}", "stage", "ms/apply", "% wall");
        // Ordered by cost, so the answer to "where does the time go" is the top row.
        let mut rows: Vec<(&&str, &Duration)> = self.stages.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1));
        for (name, d) in rows {
            let ms = self.per_run(*d) * 1e3;
            eprintln!("{name:<20} {ms:>10.1} {:>6.1}%", 100.0 * ms / (wall * 1e3));
        }
        let un = wall - self.per_run(self.accounted);
        eprintln!(
            "{:<20} {:>10.1} {:>6.1}%",
            "(unaccounted)",
            un * 1e3,
            100.0 * un / wall
        );
        if !self.nested.is_empty() {
            let mut rows: Vec<(&&str, &Duration)> = self.nested.iter().collect();
            rows.sort_by(|a, b| b.1.cmp(a.1));
            eprintln!("  sub-stages (inside the rows above, not additional):");
            for (name, d) in rows {
                let ms = self.per_run(*d) * 1e3;
                eprintln!(
                    "  {name:<18} {ms:>10.1} {:>6.1}%",
                    100.0 * ms / (wall * 1e3)
                );
            }
        }
        // Pipeline order, for reading the apply as a sequence rather than a ranking.
        eprintln!(
            "pipeline order: {}",
            self.order
                .iter()
                .map(|n| format!("{n} {:.0}ms", self.per_run(self.stages[n]) * 1e3))
                .collect::<Vec<_>>()
                .join(" > ")
        );
    }
}

/// Time one profiled apply, returning its stages and total wall-clock.
fn profiled_apply(
    prior: &GraphState,
    cs: &ChangeSet,
    src: &DirSource,
    ext: &dyn Extractor,
    scope: LinkScope,
) -> (Vec<Stage>, Duration) {
    let t = Instant::now();
    let (delta, stages) = profile::capture(|| {
        Engine::apply_with_scope(prior, cs, src, ext, &ClusterConfig::default(), scope)
            .expect("apply")
    });
    // `drop` INSIDE the measured window (ADR-0042 Phase 1d): the pre-1d delta
    // *moved* the whole edge vector out of the apply, so freeing it landed after
    // `elapsed()`; a patch delta frees it inside the apply. Timing one but not
    // the other would report a deallocation that both do as a regression.
    drop(delta);
    let wall = t.elapsed();
    (stages, wall)
}

#[test]
#[ignore = "perf profile — run with `cargo test --release -p filigrio-resolve --test apply_profile -- --ignored --nocapture`"]
fn apply_stage_profile() {
    let env_root = std::env::var("FILIGRIO_LEDGER_ROOT")
        .ok()
        .map(PathBuf::from);
    let exts_raw = std::env::var("FILIGRIO_LEDGER_EXTS").unwrap_or_else(|_| "rs".into());
    let exts: Vec<&str> = exts_raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let n: u32 = std::env::var("FILIGRIO_PROFILE_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let Some(root) = env_root.or_else(crates_dir) else {
        eprintln!("profile: corpus root not found — skipping");
        return;
    };

    let dispatch = DispatchExtractor::with_defaults();
    let ext: &dyn Extractor = &dispatch;
    let src = DirSource::load_ext(&root, &exts);
    assert!(!src.is_empty(), "empty corpus at {}", root.display());

    let t = Instant::now();
    let prior = cold_ext(&src, ext);
    let cold = t.elapsed().as_secs_f64();
    eprintln!("=== ADR-0042 Phase 1b.1 apply profile ===");
    eprintln!(
        "corpus: {} ({} files, exts={exts:?}) — {} nodes, {} edges, {} projects; cold {cold:.1}s",
        root.display(),
        src.len(),
        prior.graph.nodes.len(),
        prior.graph.edges.len(),
        prior.workspace.projects.len(),
    );

    // Two changeset sizes: the smallest real change (one file) and a wide one
    // (~1% of the corpus). §5c's claim is that the cost barely moves between
    // them — the per-stage split says *which* stages are the fixed part.
    let mut files: Vec<String> = prior
        .graph
        .nodes
        .iter()
        .filter_map(|n| n.source_file.clone())
        .collect();
    files.sort();
    files.dedup();
    assert!(!files.is_empty());
    let one = modified(&[files[files.len() / 2].as_str()]);
    let wide_n = (files.len() / 100).clamp(2, 128);
    let step = (files.len() / wide_n).max(1);
    let wide_paths: Vec<&str> = files
        .iter()
        .step_by(step)
        .take(wide_n)
        .map(String::as_str)
        .collect();
    let wide = modified(&wide_paths);

    // warm up (page in, let the allocator settle)
    let _ = apply_dyn(&prior, &one, &src, ext, LinkScope::Global);

    for (label, cs) in [("1 file", &one), (&format!("{wide_n} files"), &wide)] {
        for scope in [LinkScope::Global, LinkScope::Scoped] {
            let b = Breakdown::collect(n, || profiled_apply(&prior, cs, &src, ext, scope));
            b.report(&format!("{scope:?} / {label} changed"));
        }
    }

    // ---- the store, which the engine profile cannot see ----
    // `apply_delta` merges the delta into a full state (cloning every derived
    // index) and, for `FsStore`, re-serializes the whole `GraphState` to disk.
    let delta = apply_dyn(&prior, &one, &src, ext, LinkScope::Global);
    // Seed the store with the FULL prior (the cold delta) so `apply_delta` is
    // timed against a realistic resident state rather than an empty one. Since
    // ADR-0042 Phase 1d P1 the delta is a *patch*, so a store seeded from
    // nothing would be merging a one-file patch into an empty graph — not the
    // operation production performs, and not comparable to the pre-1d numbers.
    let cold_delta = apply_dyn(
        &GraphState::default(),
        &src.poll(None).expect("poll"),
        &src,
        ext,
        LinkScope::Global,
    );
    let mem = MemoryStore::new();
    mem.apply_delta(&cold_delta).unwrap();
    let t = Instant::now();
    for _ in 0..n {
        mem.apply_delta(&delta).unwrap();
    }
    let mem_ms = t.elapsed().as_secs_f64() / f64::from(n) * 1e3;
    let t = Instant::now();
    for _ in 0..n {
        let _ = mem.current().unwrap();
    }
    let cur_ms = t.elapsed().as_secs_f64() / f64::from(n) * 1e3;
    eprintln!("\n--- store (outside the engine) ---");
    eprintln!("MemoryStore::apply_delta  {mem_ms:>10.1} ms");
    eprintln!(
        "MemoryStore::current      {cur_ms:>10.1} ms (state clone the daemon pays per query)"
    );

    // Per-facet serialized size of the **full** state (`prior`, the cold build —
    // not `mem`, which was seeded from an empty store and therefore holds only
    // this delta's nodes): what a Phase-2 `ShardedStore` would have to rewrite
    // per apply if the derived indices stay full replacements. The *recompute*
    // cost of those indices is small (see the stage table); their *write* cost is
    // the number that decides whether patching them is worth it.
    let state = &prior;
    let mb = |v: &[u8]| v.len() as f64 / 1e6;
    eprintln!("state.json facets (serialized MB):");
    for (name, bytes) in [
        ("graph.nodes", serde_json::to_vec(&state.graph.nodes)),
        ("graph.edges", serde_json::to_vec(&state.graph.edges)),
        ("partition", serde_json::to_vec(&state.partition)),
        ("symbols", serde_json::to_vec(&state.symbols)),
        ("reverse", serde_json::to_vec(&state.reverse)),
        ("manifest", serde_json::to_vec(&state.manifest)),
        ("workspace", serde_json::to_vec(&state.workspace)),
        ("exports", serde_json::to_vec(&state.exports)),
        ("symbol_index", serde_json::to_vec(&state.symbol_index)),
    ] {
        eprintln!("  {name:<14} {:>8.1} MB", mb(&bytes.unwrap()));
    }

    if std::env::var("FILIGRIO_PROFILE_STORE").is_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = filigrio_store::FsStore::new(dir.path()).with_force(true);
        fs.apply_delta(&cold_delta).unwrap();
        let t = Instant::now();
        for _ in 0..n {
            fs.apply_delta(&delta).unwrap();
        }
        let fs_ms = t.elapsed().as_secs_f64() / f64::from(n) * 1e3;
        // Timed standalone over the same `n` runs as `apply_delta`: since
        // ADR-0042 F2 the pipeline no longer snapshots per apply — this row is
        // the on-demand export cost (and, before F2, the per-apply saving).
        let t = Instant::now();
        for _ in 0..n {
            fs.snapshot().unwrap();
        }
        let snap_ms = t.elapsed().as_secs_f64() / f64::from(n) * 1e3;
        eprintln!("FsStore::apply_delta      {fs_ms:>10.1} ms (read+merge+serialize state.json)");
        eprintln!("FsStore::snapshot         {snap_ms:>10.1} ms (graph.json interchange — explicit export, off the apply path)");
        // The *actual* bytes a production apply writes: the FULL prior state
        // written as `FsStore` really writes it (pretty-printed), not the
        // delta-seeded store above (which holds only this delta's nodes) and not
        // the facet table (compact, and since ADR-0042 F3 the file is no longer
        // the sum of every facet — two are `#[serde(skip)]`).
        let full = tempfile::tempdir().expect("tempdir");
        let fs_full = filigrio_store::FsStore::new(full.path());
        fs_full.save_state(&prior).unwrap();
        let t = Instant::now();
        for _ in 0..n {
            fs_full.save_state(&prior).unwrap();
        }
        let save_ms = t.elapsed().as_secs_f64() / f64::from(n) * 1e3;
        let len = std::fs::metadata(full.path().join("state.json"))
            .map(|m| m.len())
            .unwrap_or(0);
        eprintln!(
            "full state.json           {:>10.1} MB written in {save_ms:.1} ms (what an apply really persists)",
            len as f64 / 1e6
        );
    }
}
