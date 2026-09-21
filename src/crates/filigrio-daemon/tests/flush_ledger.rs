//! ADR-0042 Phase 1c **F4** perf-ledger harness — what watch-mode editing costs
//! the store, counted.
//!
//! Not a correctness gate (that is `write_behind.rs`); this exists to produce
//! the §5g row. It runs a **real corpus** — this workspace's own Rust sources,
//! copied to a scratch tree so the burst may edit them — through the real apply
//! path, and reports the two quantities §5g cares about in order: **store writes
//! and bytes**, then wall-clock.
//!
//! Two runs of the identical edit sequence:
//!
//! - `write-through` — every apply on the client lane (`Persistence::Flush`),
//!   i.e. the pre-F4 behaviour of every apply including the watcher's;
//! - `write-behind` — the same edits on the producer lane, then one quiescence
//!   flush, i.e. what a watcher actually does after F4.
//!
//! Run:
//! ```text
//! cargo test --release -p filigrio-daemon --test flush_ledger -- --ignored --nocapture
//! ```

use filigrio_core::GraphStore;
use filigrio_daemon::{ChangeSet, Command, Daemon, DaemonConfig, Project, Request, TestClock};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The repository root (this file is `crates/filigrio-daemon/tests/…`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// Copy the workspace's `crates/**/*.rs` + manifests into `dest` — a real Rust
/// corpus small enough to copy and safe to edit.
fn copy_corpus(dest: &Path) -> usize {
    let src = repo_root().join("crates");
    let mut files = 0;
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let keep = path.extension().is_some_and(|e| e == "rs" || e == "toml");
            if !keep {
                continue;
            }
            let rel = path.strip_prefix(&src).expect("rel");
            let out = dest.join("crates").join(rel);
            std::fs::create_dir_all(out.parent().expect("parent")).expect("mkdir");
            std::fs::copy(&path, &out).expect("copy");
            files += 1;
        }
    }
    std::fs::write(dest.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("manifest");
    files
}

fn cs_modified(rel: &str) -> ChangeSet {
    ChangeSet {
        added: vec![],
        modified: vec![rel.to_string()],
        removed: vec![],
    }
}

fn state_bytes(dir: &Path) -> u64 {
    std::fs::metadata(dir.join("state.json"))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// One run: cold index (client lane, always flushed), then `edits` single-file
/// edits applied on `lane`, then — for the write-behind lane — one quiescence
/// flush. Returns `(writes, bytes_written, elapsed_of_the_burst)`.
fn run(write_behind: bool, edits: usize) -> (usize, u64, Duration, u64) {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let root = scratch.path().to_path_buf();
    let files = copy_corpus(&root);

    let project = Project::new(root.clone());
    let out = project.output_dir.clone();
    std::fs::create_dir_all(&out).expect("outdir");
    let id = project.id.clone();
    let clock = Arc::new(TestClock::new());
    let mut daemon = Daemon::new(DaemonConfig {
        clock: clock.clone(),
        ..DaemonConfig::default()
    });
    daemon.registry.lock().add(project).expect("register");

    // Cold index — a client command, so it flushes. Not part of the burst.
    let t0 = Instant::now();
    daemon.handle_request(Request::command(Command::ProjectIndex {
        project: id.clone(),
        clean: false,
    }));
    let cold = t0.elapsed();
    let checkpoint = state_bytes(&out);
    let writes_after_cold = daemon.flusher().writes_total();

    // The burst: `edits` edits to one file, the shape of a save-storm.
    let target = "crates/filigrio-core/src/lib.rs";
    let victim = root.join(target);
    let original = std::fs::read_to_string(&victim).expect("read victim");

    let t1 = Instant::now();
    for i in 0..edits {
        std::fs::write(
            &victim,
            format!("{original}\npub fn ledger_probe_{i}() {{}}\n"),
        )
        .expect("edit");
        if write_behind {
            daemon
                .apply_producer(&id, &cs_modified(target))
                .expect("apply");
            // The flusher polls throughout; inside the window nothing is due.
            clock.advance(Duration::from_secs(1));
            daemon.flusher().flush_due();
        } else {
            daemon
                .apply_project(&id, &cs_modified(target))
                .expect("apply");
        }
    }
    if write_behind {
        clock.advance(Duration::from_secs(31));
        daemon.flusher().flush_due();
    }
    let burst = t1.elapsed();

    let writes = daemon.flusher().writes_total() - writes_after_cold;
    println!(
        "  corpus {files} files | cold index {:.0} ms, checkpoint {:.1} MB",
        cold.as_secs_f64() * 1000.0,
        checkpoint as f64 / 1e6
    );
    (writes, writes as u64 * checkpoint, burst, checkpoint)
}

#[test]
#[ignore = "perf ledger — run with `cargo test --release -p filigrio-daemon --test flush_ledger -- --ignored --nocapture`"]
fn f4_watch_burst_write_volume() {
    const EDITS: usize = 20;

    println!("\n=== ADR-0042 F4 — watch-mode burst, {EDITS} applies ===");
    println!("write-through (pre-F4: every apply persists)");
    let (wt_writes, wt_bytes, wt_time, checkpoint) = run(false, EDITS);
    println!("write-behind  (post-F4: producer lane, one quiescence flush)");
    let (wb_writes, wb_bytes, wb_time, _) = run(true, EDITS);

    println!("\n| | store writes | bytes written | burst wall-clock |");
    println!("|---|---:|---:|---:|");
    println!(
        "| write-through | {wt_writes} | {:.1} MB | {:.0} ms |",
        wt_bytes as f64 / 1e6,
        wt_time.as_secs_f64() * 1000.0
    );
    println!(
        "| write-behind | {wb_writes} | {:.1} MB | {:.0} ms |",
        wb_bytes as f64 / 1e6,
        wb_time.as_secs_f64() * 1000.0
    );
    println!(
        "| ratio | {:.1}× | {:.1}× | {:.2}× |",
        wt_writes as f64 / wb_writes.max(1) as f64,
        wt_bytes as f64 / wb_bytes.max(1) as f64,
        wt_time.as_secs_f64() / wb_time.as_secs_f64()
    );
    println!("checkpoint size: {:.2} MB\n", checkpoint as f64 / 1e6);

    assert_eq!(wb_writes, 1, "the whole burst must collapse to one write");
    assert_eq!(wt_writes, EDITS, "write-through writes once per apply");
}

/// The store-facing cost of **one apply**, isolated from the cadence — the other
/// half of F4, and the one the burst table above cannot show.
///
/// Pre-F4 the daemon handed `Pipeline` an `FsStore`, so one apply was
/// `read state.json → merge → write state.json`, and the daemon then did a
/// *second* full read to refresh its resident cache. Post-F4 it hands over a
/// `DeferredStore`: the merge runs against the resident prior (no read at all)
/// and the write is the flusher's, on its own schedule.
///
/// Measured against one real prior + one real delta, captured from the corpus.
#[test]
#[ignore = "perf ledger — run with `cargo test --release -p filigrio-daemon --test flush_ledger -- --ignored --nocapture`"]
fn f4_per_apply_store_cost() {
    use filigrio_core::{GraphDelta, GraphState, Result as CoreResult};
    use filigrio_index::DispatchExtractor;
    use filigrio_ingest::FsSource;
    use filigrio_pipeline::Pipeline;
    use filigrio_store::{DeferredStore, FsStore};
    use parking_lot::Mutex;

    /// Captures the delta the pipeline produces, so the two store paths can be
    /// timed against the *same* (prior, delta) pair.
    struct Capture<'a> {
        prior: &'a GraphState,
        delta: Mutex<Option<GraphDelta>>,
    }
    impl GraphStore for Capture<'_> {
        fn load_state(&self) -> CoreResult<Option<GraphState>> {
            Ok(Some(self.prior.clone()))
        }
        fn apply_delta(&self, delta: &GraphDelta) -> CoreResult<()> {
            *self.delta.lock() = Some(delta.clone());
            Ok(())
        }
        fn snapshot(&self) -> CoreResult<()> {
            Ok(())
        }
    }

    let scratch = tempfile::TempDir::new().expect("scratch");
    let root = scratch.path().to_path_buf();
    let files = copy_corpus(&root);
    let out = root.join(".filigrio-out");
    std::fs::create_dir_all(&out).expect("outdir");

    let source = FsSource::new(&root);
    let extractor = DispatchExtractor::with_defaults();
    let fs_store = FsStore::new(&out).with_force(true);

    // Cold build → the prior every measured apply starts from.
    Pipeline::new(&source, &extractor, &fs_store)
        .with_shrink_guard(true)
        .build()
        .expect("cold build");
    let prior = fs_store.load_state().expect("load").unwrap_or_default();
    let checkpoint = state_bytes(&out);

    // One real one-file edit → one real delta.
    let target = "crates/filigrio-core/src/lib.rs";
    let victim = root.join(target);
    let original = std::fs::read_to_string(&victim).expect("read victim");
    std::fs::write(
        &victim,
        format!(
            "{original}
pub fn ledger_probe() {{}}
"
        ),
    )
    .expect("edit");
    let capture = Capture {
        prior: &prior,
        delta: Mutex::new(None),
    };
    Pipeline::new(&source, &extractor, &capture)
        .with_shrink_guard(true)
        .apply(&prior, &cs_modified(target))
        .expect("apply");
    let delta = capture.delta.lock().take().expect("a delta was produced");

    const N: usize = 5;
    let mut old = Duration::ZERO;
    let mut client = Duration::ZERO;
    let mut producer = Duration::ZERO;

    for _ in 0..N {
        // Pre-F4: FsStore::apply_delta (read+merge+write) + the daemon's
        // post-apply `load_state()` cache refresh (a second full read).
        fs_store.save_state(&prior).expect("reset");
        let t = Instant::now();
        fs_store.apply_delta(&delta).expect("old apply");
        let _refreshed = fs_store.load_state().expect("old refresh");
        old += t.elapsed();

        // Post-F4, client lane: merge onto the resident prior + one write.
        let t = Instant::now();
        let store = DeferredStore::new(&prior);
        store.apply_delta(&delta).expect("new apply");
        let merged = store.take().expect("merged");
        fs_store.save_state(&merged).expect("flush");
        client += t.elapsed();

        // Post-F4, producer lane: the merge only; the write is the flusher's.
        let t = Instant::now();
        let store = DeferredStore::new(&prior);
        store.apply_delta(&delta).expect("new apply");
        let _merged = store.take().expect("merged");
        producer += t.elapsed();
    }

    let ms = |d: Duration| d.as_secs_f64() * 1000.0 / N as f64;
    println!("\n=== ADR-0042 F4 — per-apply store cost ({files} files, checkpoint {:.2} MB, mean of {N}) ===", checkpoint as f64 / 1e6);
    println!("| store path | ms/apply |");
    println!("|---|---:|");
    println!(
        "| pre-F4: FsStore::apply_delta + post-apply load_state | {:.1} |",
        ms(old)
    );
    println!(
        "| F4 client lane: DeferredStore merge + save_state | {:.1} |",
        ms(client)
    );
    println!(
        "| F4 producer lane: DeferredStore merge only | {:.1} |",
        ms(producer)
    );
    println!(
        "ratios — client {:.2}×, producer {:.2}×\n",
        old.as_secs_f64() / client.as_secs_f64(),
        old.as_secs_f64() / producer.as_secs_f64()
    );
}
