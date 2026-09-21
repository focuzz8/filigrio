//! **Git-history convergence harness** — ADR-0032 §7 acceptance test driven by
//! *real* commits, and the ADR-0042 Phase 1 equivalence gate driven by *real*
//! changesets.
//!
//! `shadow.rs` probes equivalence with a synthetic edit (append one function to
//! one file), so it only ever exercises "add one def to one module". Real
//! history is where scoped re-resolution can actually break: deletions, renames,
//! file moves, multi-file changesets and cross-file signature changes. This
//! harness replays the last N first-parent commits of an arbitrary checkout and
//! asserts, at every step and at the end:
//!
//! ```text
//! per step : delta(Global) ≡ delta(Scoped)   and   state(Global) ≡ state(Scoped)
//! at the end: canonical(apply_all(cold_build(c1), diffs c1..cN)) == canonical(cold_build(cN))
//! ```
//!
//! The two strategy chains are **independent** (separate stores, separate
//! priors from the same cold base) so accumulated drift is detectable — a
//! per-step comparison from a shared prior would hide it.
//!
//! ## What "≡" means here (ADR-0042 Phase 1a)
//!
//! **Every** `GraphState` field: nodes (id/kind/label/source_file/span **and
//! `attrs`**), edges (incl. unresolved-symbol hints), `partition`, `symbols`,
//! `reverse`, `manifest`, `workspace`, `exports`, `symbol_index` — via
//! `common::state_diff`, whose field list is compiler-enforced exhaustive. The
//! single exclusion is `partition` in the chain-vs-**cold** comparison, because
//! clustering is warm-started (ADR-0024); the scope-vs-scope comparison excludes
//! nothing. `common::comparator` (tests/comparator.rs) is the negative control
//! proving each facet is actually observed.
//!
//! The corpus includes **project manifests** regardless of the ext filter, so
//! `workspace` (ADR-0019) and the source-tier `ModuleResolver` (ADR-0020) are
//! genuinely exercised rather than compared empty-to-empty.
//!
//! ## Safety
//!
//! The target repository is **never mutated**. The only thing this harness does
//! to it is `git worktree add --detach` into a temp dir (removed on the way out,
//! including on panic, via [`Worktree`]'s `Drop`). Commits are materialized by
//! `checkout --detach` *inside that worktree*; the repo's HEAD, index, branches,
//! stash and working tree are untouched — asserted by capturing `rev-parse HEAD`
//! and `status --porcelain` before and after and comparing them byte-for-byte.
//!
//! ## Running
//!
//! ```text
//! FILIGRIO_GIT_REPO=/…/next.js FILIGRIO_GIT_DEPTH=5 FILIGRIO_LEDGER_EXTS=ts,tsx,js,jsx \
//!   cargo test --release -p filigrio-resolve --test git_convergence -- --ignored --nocapture
//! ```
//!
//! | env | default | meaning |
//! |---|---|---|
//! | `FILIGRIO_GIT_REPO` | — (skip) | checkout to replay |
//! | `FILIGRIO_GIT_DEPTH` | 5 | commits to replay after the base |
//! | `FILIGRIO_GIT_HEAD` | `HEAD` | tip commit-ish |
//! | `FILIGRIO_GIT_BASE` | — | explicit base commit-ish; replays `BASE..HEAD` (ignores `DEPTH`) so an arbitrary — e.g. deliberately churny — range can be targeted |
//! | `FILIGRIO_LEDGER_EXTS` | `rs` | comma-separated corpus extensions (project manifests are always included) |
//! | `FILIGRIO_GIT_EXPECT_RENAMES` | unset | when set, **fail** unless the replayed range actually hit the rename/copy branch |
//! | `FILIGRIO_GIT_WORKTREE_DIR` | repo's parent dir | where the temp worktree goes (keep it off tmpfs) |

mod common;
use common::{
    apply_dyn, cold_ext, delta_facets, facet_mismatches, path_in_corpus, sample_diff, state_diff,
    DirSource, SKIP_DIRS,
};
use filigrio_core::{
    ChangeSet, Edge, EdgeTarget, Extractor, GraphState, GraphStore, Node, NodeId, Reference, Source,
};
use filigrio_index::DispatchExtractor;
use filigrio_resolve::LinkScope;
use filigrio_store::MemoryStore;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

// ---- git plumbing (read-only against the target repo) -----------------------

/// Run `git -C <dir> <args…>`, returning trimmed stdout. Panics with stderr on a
/// non-zero exit — a silent git failure would turn into a bogus "no changes"
/// step and a false green.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

/// Raw stdout bytes (for `-z` output, which is not line-oriented).
fn git_raw(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// A detached worktree of `repo`, removed on drop (**including on panic** — a
/// dangling worktree left in the user's repo is a defect, not a nuisance).
struct Worktree {
    repo: PathBuf,
    path: PathBuf,
    /// Owns the parent temp dir; dropped after `Drop::drop` runs, so the
    /// removal below still sees the directory.
    _tmp: tempfile::TempDir,
}

impl Worktree {
    /// Materialize `sha` of `repo` in a fresh worktree. The temp dir lives on
    /// the same filesystem as the repo by default (`FILIGRIO_GIT_WORKTREE_DIR`
    /// overrides) — `/tmp` is frequently tmpfs, and a next.js checkout in RAM is
    /// not a good idea.
    fn add(repo: &Path, sha: &str) -> Self {
        let parent = std::env::var("FILIGRIO_GIT_WORKTREE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                repo.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| repo.to_path_buf())
            });
        let tmp = tempfile::TempDir::new_in(&parent).expect("temp dir next to the repo");
        // `git worktree add` requires a non-existent path, so nest one level.
        let path = tmp.path().join("wt");
        git(
            repo,
            &["worktree", "add", "--detach", path.to_str().unwrap(), sha],
        );
        Worktree {
            repo: repo.to_path_buf(),
            path,
            _tmp: tmp,
        }
    }

    /// Advance the worktree to `sha` (cheap: git only touches changed paths).
    fn checkout(&self, sha: &str) {
        git(&self.path, &["checkout", "--detach", sha]);
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        // Best-effort: never panic in drop (it would mask the real failure).
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .output();
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "prune"])
            .output();
    }
}

// ---- instrumentation (ADR-0042 Phase 1a.5: instrument, don't infer) ---------

/// Counters for the harness's own silent branches. Every one of these was
/// previously *inferred* ("renames are covered because the range has renames",
/// "no files are skipped"), and two of them are structurally unfalsifiable
/// without a counter: the replayed view and the fresh reload apply the SAME
/// silent-skip rule, so a skipped file passes the drift check by construction.
#[derive(Default)]
struct Counters {
    /// `R` name-status records seen (any path, before corpus filtering).
    renames: usize,
    /// `C` (copy) name-status records seen.
    copies: usize,
    /// Rename/copy records where at least one side survived the corpus filter —
    /// i.e. the branch actually influenced a changeset.
    rename_in_corpus: usize,
    /// In-corpus paths git reported that could not be read as UTF-8 from the
    /// worktree, so the replay silently dropped them.
    skipped_unreadable: usize,
    /// In-corpus files the *directory walk* silently skipped for the same reason.
    skipped_walk: usize,
    /// Git said `A` for a path the view already had, or `M` for one it did not —
    /// `sync_source` overrides git's label with the tree's truth. A non-zero count
    /// means the raw git label would have desynced the engine's accounting.
    label_disagreements: usize,
    /// In-corpus **project manifest** paths appearing in a changeset (the
    /// ADR-0019/0020 workspace path this harness could not previously reach).
    manifest_changes: usize,
}

// ---- changeset derivation ---------------------------------------------------

/// Derive the corpus-filtered [`ChangeSet`] for `prev → cur` from git's own
/// name-status diff. `-z` (NUL-delimited) because real repos have paths with
/// spaces; `-M -C` so renames/copies arrive as such rather than as an
/// unrelated add + delete pair (a rename is the interesting shape here).
///
/// Status mapping: `A`→added, `M`/`T`→modified, `D`→removed,
/// `R###\told\tnew`→removed(old) + added(new), `C###`→added(new).
/// Each side of a rename is filtered independently, so a move *into* or *out of*
/// the corpus (e.g. `src/a.ts` → `dist/a.ts`) degrades correctly to a pure
/// add / pure remove.
fn changeset_for(repo: &Path, prev: &str, cur: &str, exts: &[&str], c: &mut Counters) -> ChangeSet {
    // `prev`/`cur` are already consecutive on the first-parent walk, so a plain
    // two-tree diff *is* the first-parent diff — merges collapse to one step.
    let raw = git_raw(
        repo,
        &["diff", "--name-status", "-z", "-M", "-C", prev, cur],
    );
    let mut fields = raw
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned());

    let mut cs = ChangeSet::default();
    while let Some(status) = fields.next() {
        let code = status.as_bytes()[0];
        match code {
            b'R' | b'C' => {
                let old = fields.next().expect("rename: old path");
                let new = fields.next().expect("rename: new path");
                if code == b'R' {
                    c.renames += 1;
                } else {
                    c.copies += 1;
                }
                let (old_in, new_in) = (path_in_corpus(&old, exts), path_in_corpus(&new, exts));
                if old_in || new_in {
                    c.rename_in_corpus += 1;
                }
                if code == b'R' && old_in {
                    cs.removed.push(old);
                }
                if new_in {
                    cs.added.push(new);
                }
            }
            _ => {
                let p = fields.next().expect("path after status");
                if !path_in_corpus(&p, exts) {
                    continue;
                }
                match code {
                    b'A' => cs.added.push(p),
                    b'M' | b'T' => cs.modified.push(p),
                    b'D' => cs.removed.push(p),
                    // U (unmerged) / X (unknown) cannot occur in a commit-to-commit
                    // diff; treat conservatively as a re-index rather than ignore.
                    _ => cs.modified.push(p),
                }
            }
        }
    }
    cs.added.sort();
    cs.added.dedup();
    cs.modified.sort();
    cs.modified.dedup();
    cs.removed.sort();
    cs.removed.dedup();
    c.manifest_changes += cs
        .touched()
        .filter(|p| filigrio_core::is_manifest(p))
        .count();
    cs
}

/// Replay `cs` onto the in-memory `src` view by re-reading from the worktree.
/// Returns the changeset actually applied — a path git reports but that cannot
/// be read as UTF-8 (binary, or filtered out) is demoted so the `Source` view and
/// the changeset never disagree.
fn sync_source(src: &DirSource, wt: &Path, cs: &ChangeSet, c: &mut Counters) -> ChangeSet {
    let mut out = ChangeSet::default();
    for p in &cs.removed {
        if src.contains(p) {
            src.remove(p);
            out.removed.push(p.clone());
        }
    }
    for (labelled_added, paths) in [(true, &cs.added), (false, &cs.modified)] {
        for p in paths {
            let existed = src.contains(p);
            match std::fs::read_to_string(wt.join(p)) {
                Ok(body) => {
                    src.set(p, &body);
                    // Trust the tree, not the label: a git "A" for a path already
                    // in the view (or "M" for one that is not) would otherwise
                    // desync the drop-and-re-extract accounting in `Engine::apply`.
                    if labelled_added == existed {
                        c.label_disagreements += 1;
                    }
                    if existed {
                        out.modified.push(p.clone());
                    } else {
                        out.added.push(p.clone());
                    }
                }
                Err(_) => {
                    // Unreadable (binary / vanished): drop it from the view too.
                    c.skipped_unreadable += 1;
                    if existed {
                        src.remove(p);
                        out.removed.push(p.clone());
                    }
                }
            }
        }
    }
    out
}

// ---- canonical comparison (non-cloning — states here are big) ---------------
//
// Both comparators below run on `common::state_diff`, which digests **every**
// `GraphState` field (nodes incl. `attrs` and span, edges incl. hints, partition,
// symbols, reverse, manifest, workspace, exports, symbol_index) — the field list
// is compiler-enforced exhaustive. The only difference between them is whether
// `partition` counts, and that difference is justified per pairing:

/// **Chain vs cold.** `partition` is excluded, and *only* `partition`: clustering
/// is warm-started (ADR-0024), so an incremental partition legitimately differs
/// from a cold build's. Every other field must match exactly.
fn resolution_diff(a: &GraphState, b: &GraphState) -> Vec<String> {
    state_diff(a, b, &["partition"])
}

/// **Global chain vs Scoped chain.** Nothing is excluded (ADR-0042 Phase 1a.2):
/// the two chains warm-start from the *same* prior partition over the same edge
/// set, so their partitions must agree too — excluding it here (as the previous
/// comparator did) left scope-to-scope clustering equivalence untested at scale.
fn scope_pair_diff(a: &GraphState, b: &GraphState) -> Vec<String> {
    state_diff(a, b, &[])
}

// ---- ADR-0042 Phase 1d P2: the would-be dirty-shard set ---------------------
//
// Pure measurement. No `ShardedStore` exists and none is implied: this models
// what a sharded store *would* have to rewrite for each real commit, so the
// Phase-2 go/stop decision rests on real changesets instead of §A's
// uniform-over-shards percentiles.
//
// Shard key = `module_of(source_file)` = the file path (identity for every
// shipped language). Three exemptions, all from the amendment:
//   * `EdgeTarget::Symbol` (unresolved, ~42 % of the graph) has no target node,
//     so it lives in its source shard only and contributes no partner;
//   * project-overlay elements (`kind == "project"`, no `source_file`) route to
//     the workspace sidecar (B1) and are never a shard — an endpoint with no
//     shard key contributes nothing;
//   * B6 diff-before-write: a boundary partner is dirty only when its mirror
//     set actually changed, which is exactly what reading the endpoints off the
//     *edge diff* computes. `naive` below is the un-refined §2 rule (every
//     partner of every changed shard) so B6's worth is visible.

/// node id → its shard key (`None` = file-less ⇒ workspace sidecar, B1).
type ShardMap = std::collections::HashMap<NodeId, Option<String>>;

fn shard_map_from(state: &GraphState, into: &mut ShardMap) {
    for n in &state.graph.nodes {
        into.insert(n.id.clone(), n.source_file.clone());
    }
}

/// The would-be content of one shard, in the record shape §2/B2 specifies.
#[derive(Default)]
struct ShardBuf<'a> {
    nodes: Vec<&'a Node>,
    /// edges this shard owns (intra + boundary out + unresolved out)
    owned: Vec<&'a Edge>,
    /// mirrored in-edges: boundary edges owned by another shard (§2's mirror
    /// rule — a boundary edge is written in *both* endpoint shards, once each)
    mirrors: Vec<&'a Edge>,
    /// `Reference`s whose *site* lives in this shard (B2: `reverse` shards here)
    refs: Vec<(&'a str, &'a Reference)>,
}

impl ShardBuf<'_> {
    /// Serialize as a real shard would: one tagged JSON record per line,
    /// `serde_json` encoding. Modeled, not measured — see §5i's caveat.
    fn bytes(&self) -> usize {
        let mut buf: Vec<u8> = Vec::new();
        for n in &self.nodes {
            buf.extend_from_slice(b"{\"t\":\"node\",\"v\":");
            serde_json::to_writer(&mut buf, n).unwrap();
            buf.extend_from_slice(b"}\n");
        }
        for e in &self.owned {
            buf.extend_from_slice(b"{\"t\":\"edge\",\"v\":");
            serde_json::to_writer(&mut buf, e).unwrap();
            buf.extend_from_slice(b"}\n");
        }
        for e in &self.mirrors {
            buf.extend_from_slice(b"{\"t\":\"mirror\",\"v\":");
            serde_json::to_writer(&mut buf, e).unwrap();
            buf.extend_from_slice(b"}\n");
        }
        for (name, r) in &self.refs {
            buf.extend_from_slice(b"{\"t\":\"ref\",\"n\":");
            serde_json::to_writer(&mut buf, name).unwrap();
            buf.extend_from_slice(b",\"v\":");
            serde_json::to_writer(&mut buf, r).unwrap();
            buf.extend_from_slice(b"}\n");
        }
        buf.len()
    }
}

/// Bucket the state's records into the requested shards (`None` = all shards).
fn shard_contents<'a>(
    state: &'a GraphState,
    shards: Option<&std::collections::HashSet<&str>>,
    map: &'a ShardMap,
) -> std::collections::HashMap<&'a str, ShardBuf<'a>> {
    let want = |s: &str| match shards {
        Some(w) => w.contains(s),
        None => true,
    };
    let mut out: std::collections::HashMap<&str, ShardBuf> = std::collections::HashMap::new();
    for n in &state.graph.nodes {
        if let Some(sf) = n.source_file.as_deref() {
            if want(sf) {
                out.entry(sf).or_default().nodes.push(n);
            }
        }
    }
    for e in &state.graph.edges {
        let src = map.get(&e.source).and_then(|o| o.as_deref());
        let Some(src) = src else { continue }; // overlay edge → sidecar (B1)
        if want(src) {
            out.entry(src).or_default().owned.push(e);
        }
        if let EdgeTarget::Node(t) = &e.target {
            if let Some(tgt) = map.get(t).and_then(|o| o.as_deref()) {
                if tgt != src && want(tgt) {
                    out.entry(tgt).or_default().mirrors.push(e);
                }
            }
        }
    }
    for (name, refs) in &state.reverse.refs {
        for r in refs {
            if let Some(site) = map.get(&r.source).and_then(|o| o.as_deref()) {
                if want(site) {
                    out.entry(site).or_default().refs.push((name.as_str(), r));
                }
            }
        }
    }
    out
}

/// Total bytes + shard count of the whole sharded graph — the denominator every
/// per-step fraction is expressed against — plus the **edge taxonomy** (§A):
/// intra-shard / unresolved / cross-shard boundary / overlay, as edge counts.
/// The taxonomy is the number that explains why corpora differ: unresolved
/// edges are mirror-exempt, so a corpus with many of them has a structurally
/// narrower write-set than one that resolves nearly everything.
fn whole_graph_shard_bytes(state: &GraphState) -> (usize, usize, [usize; 4]) {
    let mut map = ShardMap::new();
    shard_map_from(state, &mut map);
    let mut tax = [0usize; 4]; // intra, unresolved, boundary, overlay
    for e in &state.graph.edges {
        let src = map.get(&e.source).and_then(|o| o.as_deref());
        match (src, &e.target) {
            (None, _) => tax[3] += 1,
            (Some(_), EdgeTarget::Symbol(_)) => tax[1] += 1,
            (Some(s), EdgeTarget::Node(t)) => match map.get(t).and_then(|o| o.as_deref()) {
                None => tax[3] += 1,
                Some(t) if t == s => tax[0] += 1,
                Some(_) => tax[2] += 1,
            },
        }
    }
    let all = shard_contents(state, None, &map);
    (all.values().map(ShardBuf::bytes).sum(), all.len(), tax)
}

/// One step's dirty-shard measurement.
#[derive(Default, Clone)]
struct Dirty {
    /// files the changeset touched (`delta.dirty_files`)
    direct: usize,
    /// shards dirty under the B6 rule (direct ∪ both endpoint shards of every
    /// changed edge; `Symbol` targets contribute their source shard only)
    shards: usize,
    /// the un-refined §2 rule: every boundary partner of every dirty shard
    naive: usize,
    bytes: usize,
    /// largest number of *other* shards pulled in by a single shard's changed
    /// boundary edges — the §A fan-out tail, per commit
    top_fanout: usize,
}

fn measure_dirty(state: &GraphState, delta: &filigrio_core::GraphDelta, map: &ShardMap) -> Dirty {
    let mut dirty: BTreeSet<String> = delta.dirty_files.iter().cloned().collect();
    let direct = dirty.len();
    // partner attribution, for the fan-out tail
    let mut fanout: std::collections::HashMap<String, BTreeSet<String>> =
        std::collections::HashMap::new();
    for e in delta.edges_added.iter().chain(delta.edges_removed.iter()) {
        let src = map.get(&e.source).and_then(|o| o.clone());
        let tgt = match &e.target {
            EdgeTarget::Node(t) => map.get(t).and_then(|o| o.clone()),
            EdgeTarget::Symbol(_) => None, // exempt: no target node, no partner
        };
        if let Some(s) = &src {
            dirty.insert(s.clone());
        }
        if let Some(t) = &tgt {
            dirty.insert(t.clone());
        }
        if let (Some(s), Some(t)) = (&src, &tgt) {
            if s != t {
                fanout.entry(s.clone()).or_default().insert(t.clone());
                fanout.entry(t.clone()).or_default().insert(s.clone());
            }
        }
    }
    let top_fanout = fanout.values().map(BTreeSet::len).max().unwrap_or(0);

    // the naive alternative: every boundary partner of every dirty shard, in the
    // whole graph, regardless of whether its mirror set changed (no B6)
    let dirty_set: std::collections::HashSet<&str> = dirty.iter().map(String::as_str).collect();
    let mut naive: BTreeSet<&str> = dirty.iter().map(String::as_str).collect();
    for e in &state.graph.edges {
        let (Some(s), EdgeTarget::Node(t)) =
            (map.get(&e.source).and_then(|o| o.as_deref()), &e.target)
        else {
            continue;
        };
        let Some(t) = map.get(t).and_then(|o| o.as_deref()) else {
            continue;
        };
        if t == s {
            continue;
        }
        if dirty_set.contains(s) {
            naive.insert(t);
        }
        if dirty_set.contains(t) {
            naive.insert(s);
        }
    }

    let bufs = shard_contents(state, Some(&dirty_set), map);
    Dirty {
        direct,
        shards: dirty.len(),
        naive: naive.len(),
        bytes: bufs.values().map(ShardBuf::bytes).sum(),
        top_fanout,
    }
}

/// p50/p90/p99/max of a sample (nearest-rank).
fn pct(v: &mut [f64]) -> (f64, f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| {
        let i = ((q * v.len() as f64).ceil() as usize).saturating_sub(1);
        v[i.min(v.len() - 1)]
    };
    (at(0.5), at(0.9), at(0.99), v[v.len() - 1])
}

// ---- the harness ------------------------------------------------------------

/// A per-step row of the replay report.
struct Row {
    sha: String,
    subject: String,
    a: usize,
    m: usize,
    d: usize,
    /// Wall-clock of the Global apply and the Scoped apply of the *same*
    /// changeset. Extraction cost is identical across scopes (same files read
    /// and parsed), so the difference is the link stage — the ADR-0042 Phase 1
    /// ledger measured on real changesets instead of a synthetic one-fn probe.
    secs_g: f64,
    secs_s: f64,
    diverged: Vec<String>,
    /// ADR-0042 Phase 1d P2 — the would-be dirty-shard set for this commit.
    dirty: Dirty,
}

#[test]
#[ignore = "git-history convergence — needs FILIGRIO_GIT_REPO; run --release --ignored --nocapture"]
fn git_history_convergence() {
    let Some(repo) = std::env::var("FILIGRIO_GIT_REPO").ok().map(PathBuf::from) else {
        eprintln!("git_convergence: FILIGRIO_GIT_REPO unset — skipping");
        return;
    };
    assert!(
        repo.join(".git").exists(),
        "{} is not a git repo",
        repo.display()
    );
    let depth: usize = std::env::var("FILIGRIO_GIT_DEPTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let head = std::env::var("FILIGRIO_GIT_HEAD").unwrap_or_else(|_| "HEAD".into());
    let exts_raw = std::env::var("FILIGRIO_LEDGER_EXTS").unwrap_or_else(|_| "rs".into());
    let exts: Vec<&str> = exts_raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    // ---- safety baseline: nothing below may change either of these ----
    let base_head = git(&repo, &["rev-parse", "HEAD"]);
    let base_status = git(&repo, &["status", "--porcelain"]);

    // ---- commit list: oldest..newest along the first-parent line ----
    // `--first-parent` linearizes history so a merge commit is one step (its
    // second-parent side never appears as an independent changeset).
    //
    // `FILIGRIO_GIT_BASE` (ADR-0042 Phase 1a.6) replays an arbitrary `BASE..HEAD`
    // range instead of the trailing `HEAD~depth..HEAD` window, so a deliberately
    // high-churn slice of history can be targeted — the depth-40 next.js window
    // turned out to be ~0.5 % churn, a smoke test rather than a stress test.
    let mut shas: Vec<String> = match std::env::var("FILIGRIO_GIT_BASE").ok() {
        Some(base) => {
            let base_sha = git(&repo, &["rev-parse", &format!("{base}^{{commit}}")]);
            let listing = git(
                &repo,
                &["rev-list", "--first-parent", &format!("{base_sha}..{head}")],
            );
            let mut v: Vec<String> = listing.lines().map(str::to_string).collect();
            v.reverse();
            assert!(
                !v.is_empty(),
                "FILIGRIO_GIT_BASE={base} yields no commits up to {head} — \
                 is it an ancestor on the first-parent line?"
            );
            std::iter::once(base_sha).chain(v).collect()
        }
        None => {
            let n = format!("{}", depth + 1);
            let listing = git(&repo, &["rev-list", "--first-parent", "-n", &n, &head]);
            let mut v: Vec<String> = listing.lines().map(str::to_string).collect();
            v.reverse();
            v
        }
    };
    shas.dedup();
    assert!(
        shas.len() >= 2,
        "need at least 2 commits to replay, got {}",
        shas.len()
    );
    let base_sha = shas[0].clone();

    let wt = Worktree::add(&repo, &base_sha);
    let verdict = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        replay(&repo, &wt, &shas, &exts)
    }));

    // ---- safety epilogue: the repo must be exactly as we found it ----
    // Runs before we re-raise, so a divergence panic still gets audited.
    let after_head = git(&repo, &["rev-parse", "HEAD"]);
    let after_status = git(&repo, &["status", "--porcelain"]);
    drop(wt); // remove the worktree before listing
    let worktrees = git(&repo, &["worktree", "list"]);
    eprintln!("\n--- target repo audit ---");
    eprintln!("HEAD      : {base_head} -> {after_head}");
    eprintln!(
        "porcelain : {} line(s) -> {} line(s)",
        base_status.lines().count(),
        after_status.lines().count()
    );
    eprintln!("worktrees :\n{worktrees}");

    if let Err(e) = verdict {
        std::panic::resume_unwind(e);
    }
    assert_eq!(
        base_head, after_head,
        "target repo HEAD moved — harness bug"
    );
    assert_eq!(
        base_status, after_status,
        "target repo working tree changed — harness bug"
    );
    assert_eq!(
        worktrees.lines().count(),
        1,
        "a worktree was left dangling in {}:\n{worktrees}",
        repo.display()
    );
}

/// Replay `shas[0] → shas[1] → … → shas[n]` under both link scopes and assert
/// the §7 identity for each chain. Split out so the caller can audit the target
/// repo even when this panics.
fn replay(repo: &Path, wt: &Worktree, shas: &[String], exts: &[&str]) {
    let ext = DispatchExtractor::with_defaults();
    let extractor: &dyn Extractor = &ext;

    let mut counters = Counters::default();

    // ---- base: load the corpus at c1 and cold-build it once ----
    let (src, skipped) = DirSource::load_ext_counted(&wt.path, exts);
    counters.skipped_walk += skipped;
    assert!(
        !src.is_empty(),
        "empty corpus at base commit (exts={exts:?}, skip={SKIP_DIRS:?})"
    );
    let base_files = src.len();
    let base_manifests = src
        .paths()
        .iter()
        .filter(|p| filigrio_core::is_manifest(p))
        .count();
    eprintln!(
        "git_convergence: {} @ {} ({} commits), {} files ({} manifests), exts={exts:?}",
        repo.display(),
        &shas[0][..12],
        shas.len(),
        base_files,
        base_manifests,
    );

    let t = Instant::now();
    let base_cs = src.poll(None).unwrap();
    let base_delta = apply_dyn(
        &GraphState::default(),
        &base_cs,
        &src,
        extractor,
        LinkScope::Global,
    );
    // One extraction pass, two independent chains: same starting state, but each
    // advances through its own store from its own prior, so drift accumulates
    // (and is therefore detectable) instead of being reset each step.
    let (store_g, store_s) = (MemoryStore::new(), MemoryStore::new());
    store_g.apply_delta(&base_delta).unwrap();
    store_s.apply_delta(&base_delta).unwrap();
    let (mut state_g, mut state_s) = (store_g.current().unwrap(), store_s.current().unwrap());
    let base_nodes = state_g.graph.nodes.len();
    eprintln!(
        "  cold base: {} nodes, {} edges, {} projects in {:.1}s",
        base_nodes,
        state_g.graph.edges.len(),
        state_g.workspace.projects.len(),
        t.elapsed().as_secs_f64()
    );
    // ADR-0042 Phase 1a.4: prove the corpus actually reaches the workspace path.
    // Without manifests in the corpus this was `0` and every `workspace`
    // comparison in this harness was vacuous.
    assert!(
        !state_g.workspace.projects.is_empty(),
        "workspace is degenerate ({} manifests in a {}-file corpus) — the \
         workspace/module-resolution comparison would be vacuous",
        base_manifests,
        base_files
    );

    // ---- replay ----
    let mut rows: Vec<Row> = Vec::new();
    let mut touched: BTreeSet<String> = BTreeSet::new();
    for pair in shas.windows(2) {
        let (prev, cur) = (&pair[0], &pair[1]);
        let subject = git(repo, &["log", "-1", "--format=%s", cur]);
        let cs = changeset_for(repo, prev, cur, exts, &mut counters);
        wt.checkout(cur);
        let cs = sync_source(&src, &wt.path, &cs, &mut counters);
        touched.extend(cs.touched().cloned());

        let mut diverged = Vec::new();
        if cs.is_empty() {
            // Nothing in the corpus changed (e.g. a docs-only commit). Skip the
            // apply rather than feed the engine an empty changeset.
            rows.push(Row {
                sha: cur.clone(),
                subject,
                a: 0,
                m: 0,
                d: 0,
                secs_g: 0.0,
                secs_s: 0.0,
                diverged,
                dirty: Dirty::default(),
            });
            continue;
        }

        // P2: node → shard from the *prior* state, so an edge removed against a
        // node this apply deletes still resolves to a shard. Extended with the
        // new state's nodes below.
        let mut smap = ShardMap::new();
        shard_map_from(&state_g, &mut smap);

        let t = Instant::now();
        let dg = apply_dyn(&state_g, &cs, &src, extractor, LinkScope::Global);
        let secs_g = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let ds = apply_dyn(&state_s, &cs, &src, extractor, LinkScope::Scoped);
        let secs_s = t.elapsed().as_secs_f64();
        let df = facet_mismatches(&delta_facets(&dg), &delta_facets(&ds), &[]);
        if !df.is_empty() {
            diverged.push(format!("delta facets: {}", df.join(", ")));
        }
        store_g.apply_delta(&dg).unwrap();
        store_s.apply_delta(&ds).unwrap();
        state_g = store_g.current().unwrap();
        state_s = store_s.current().unwrap();
        // Delta equality is only the *step*; state equality is the accumulated
        // truth (two chains can emit equal-looking deltas from unequal priors).
        // Scope-vs-scope, so `partition` counts (Phase 1a.2).
        let sd = scope_pair_diff(&state_g, &state_s);
        if !sd.is_empty() {
            diverged.push(format!("state: {}", sd.join(" | ")));
        }

        // P2 (measurement only, outside the timed window): what would a sharded
        // store have had to rewrite for this commit?
        shard_map_from(&state_g, &mut smap);
        let dirty = measure_dirty(&state_g, &dg, &smap);
        drop(smap);

        rows.push(Row {
            sha: cur.clone(),
            subject,
            a: cs.added.len(),
            m: cs.modified.len(),
            d: cs.removed.len(),
            secs_g,
            secs_s,
            diverged,
            dirty,
        });
    }

    // ---- P2 denominator: the whole sharded graph at the final state ----
    // Computed once (not per step): every per-commit fraction below is against
    // this, so it is directly comparable to §5g's 210 MB `state.json`.
    let t = Instant::now();
    let (whole_bytes, whole_shards, tax) = whole_graph_shard_bytes(&state_g);
    let etot = state_g.graph.edges.len().max(1) as f64;
    eprintln!(
        "\n--- ADR-0042 P2: sharded-graph denominator ---\n\
         whole graph: {whole_shards} shards, {:.1} MB serialized ({:.1}s)",
        whole_bytes as f64 / 1e6,
        t.elapsed().as_secs_f64()
    );
    eprintln!(
        "edge taxonomy: intra {:.1}% ({}), unresolved {:.1}% ({}), cross-shard boundary {:.1}% ({}), overlay/sidecar {:.1}% ({})",
        100.0 * tax[0] as f64 / etot, tax[0],
        100.0 * tax[1] as f64 / etot, tax[1],
        100.0 * tax[2] as f64 / etot, tax[2],
        100.0 * tax[3] as f64 / etot, tax[3],
    );

    // ---- §7: both chains must equal a cold build of the final tree ----
    // Reload the tree from disk rather than reusing the replayed view — that also
    // proves the changesets covered every file that actually changed.
    let (fresh, skipped) = DirSource::load_ext_counted(&wt.path, exts);
    counters.skipped_walk += skipped;
    let view_drift = sample_diff(&src.paths(), &fresh.paths(), "source view vs final tree");
    let final_files = fresh.len();
    // Free everything the comparison no longer needs: at next.js scale a
    // `GraphState` is ~GBs and four would otherwise be resident at once.
    drop(src);
    drop(store_g);
    drop(store_s);
    let t = Instant::now();
    let cold = cold_ext(&fresh, extractor);
    let cold_secs = t.elapsed().as_secs_f64();

    let dg = resolution_diff(&state_g, &cold);
    let ds = resolution_diff(&state_s, &cold);

    // ---- report ----
    eprintln!("\n=== git-history replay ({} steps) ===", rows.len());
    eprintln!(
        "{:<12} {:>4} {:>4} {:>4} {:>8} {:>8} {:>6}  {:<8} subject",
        "sha", "+A", "~M", "-D", "global", "scoped", "x", "verdict"
    );
    for r in &rows {
        let verdict = if r.diverged.is_empty() {
            "OK"
        } else {
            "DIVERGED"
        };
        let subj: String = r.subject.chars().take(44).collect();
        let speedup = if r.secs_s > 0.0 {
            format!("{:.2}", r.secs_g / r.secs_s)
        } else {
            "-".into()
        };
        eprintln!(
            "{:<12} {:>4} {:>4} {:>4} {:>8.2} {:>8.2} {:>6}  {:<8} {}",
            &r.sha[..12.min(r.sha.len())],
            r.a,
            r.m,
            r.d,
            r.secs_g,
            r.secs_s,
            speedup,
            verdict,
            subj
        );
        for d in &r.diverged {
            eprintln!("             ! {d}");
        }
    }
    // Aggregate over the steps that actually applied something — the ADR-0042
    // Phase 1 ledger row for real history (the `shadow.rs` ledger measures the
    // same thing on a synthetic one-function edit).
    let live: Vec<&Row> = rows.iter().filter(|r| r.secs_g > 0.0).collect();
    if !live.is_empty() {
        let tg: f64 = live.iter().map(|r| r.secs_g).sum();
        let ts: f64 = live.iter().map(|r| r.secs_s).sum();
        let n = live.len() as f64;
        eprintln!(
            "\n{} applying steps: Global {:.2}s total ({:.2}s/apply), Scoped {:.2}s total ({:.2}s/apply) — {:.2}× ",
            live.len(),
            tg,
            tg / n,
            ts,
            ts / n,
            tg / ts
        );
    }
    // ---- P2: the dirty-shard distribution over real commits ----
    if !live.is_empty() {
        eprintln!("\n--- ADR-0042 P2: would-be dirty shards per commit ---");
        eprintln!(
            "{:<12} {:>6} {:>7} {:>7} {:>9} {:>8} {:>7}",
            "sha", "files", "shards", "naive", "bytes", "% graph", "fanout"
        );
        for r in &live {
            eprintln!(
                "{:<12} {:>6} {:>7} {:>7} {:>9} {:>7.3}% {:>7}",
                &r.sha[..12.min(r.sha.len())],
                r.dirty.direct,
                r.dirty.shards,
                r.dirty.naive,
                r.dirty.bytes,
                100.0 * r.dirty.bytes as f64 / whole_bytes.max(1) as f64,
                r.dirty.top_fanout,
            );
        }
        let mut sh: Vec<f64> = live.iter().map(|r| r.dirty.shards as f64).collect();
        let mut by: Vec<f64> = live.iter().map(|r| r.dirty.bytes as f64).collect();
        let mut nv: Vec<f64> = live.iter().map(|r| r.dirty.naive as f64).collect();
        let (sh50, sh90, sh99, shmax) = pct(&mut sh);
        let (by50, by90, by99, bymax) = pct(&mut by);
        let (nv50, nv90, nv99, nvmax) = pct(&mut nv);
        let fs = |v: f64| 100.0 * v / whole_shards.max(1) as f64;
        let fb = |v: f64| 100.0 * v / whole_bytes.max(1) as f64;
        eprintln!(
            "\ndirty shards (B6) : p50 {sh50:.0} ({:.3}%)  p90 {sh90:.0} ({:.3}%)  p99 {sh99:.0} ({:.3}%)  max {shmax:.0} ({:.3}%)",
            fs(sh50), fs(sh90), fs(sh99), fs(shmax)
        );
        eprintln!(
            "dirty shards (naive §2, no B6): p50 {nv50:.0} ({:.3}%)  p90 {nv90:.0} ({:.3}%)  p99 {nv99:.0} ({:.3}%)  max {nvmax:.0} ({:.3}%)",
            fs(nv50), fs(nv90), fs(nv99), fs(nvmax)
        );
        eprintln!(
            "dirty bytes       : p50 {:.2} MB ({:.3}%)  p90 {:.2} MB ({:.3}%)  p99 {:.2} MB ({:.3}%)  max {:.2} MB ({:.3}%)",
            by50 / 1e6, fb(by50), by90 / 1e6, fb(by90), by99 / 1e6, fb(by99), bymax / 1e6, fb(bymax)
        );
        let dominated = live
            .iter()
            .filter(|r| r.dirty.shards > 0 && r.dirty.top_fanout * 2 >= r.dirty.shards)
            .count();
        eprintln!(
            "fan-out tail      : max single-shard partner count {} ; {dominated}/{} steps have one shard supplying ≥50% of the dirty set",
            live.iter().map(|r| r.dirty.top_fanout).max().unwrap_or(0),
            live.len()
        );
    }

    eprintln!(
        "\ncold build of final commit: {} nodes, {} edges, {} projects in {cold_secs:.1}s",
        cold.graph.nodes.len(),
        cold.graph.edges.len(),
        cold.workspace.projects.len(),
    );

    // ---- churn: how hard did this window actually push? (Phase 1a.6) ----
    let noop = rows.len() - live.len();
    let (sa, sm, sd_) = rows
        .iter()
        .fold((0, 0, 0), |(a, m, d), r| (a + r.a, m + r.m, d + r.d));
    eprintln!("\n--- corpus churn ---");
    eprintln!(
        "files      : {base_files} at base -> {final_files} at tip; {} distinct files touched ({:.1}% of base)",
        touched.len(),
        100.0 * touched.len() as f64 / base_files.max(1) as f64,
    );
    eprintln!(
        "changesets : {sa} added, {sm} modified, {sd_} removed over {} applying steps ({noop} no-op)",
        live.len(),
    );
    eprintln!(
        "nodes      : {base_nodes} at base -> {} at tip ({:+})",
        cold.graph.nodes.len(),
        cold.graph.nodes.len() as i64 - base_nodes as i64,
    );

    // ---- instrumentation: the silent branches, counted not assumed (1a.5) ----
    eprintln!("\n--- harness counters ---");
    eprintln!(
        "renames R={} copies C={} (touching the corpus: {})",
        counters.renames, counters.copies, counters.rename_in_corpus
    );
    eprintln!(
        "silently skipped: {} on replay (unreadable/binary), {} on the directory walk",
        counters.skipped_unreadable, counters.skipped_walk
    );
    eprintln!(
        "git-label vs tree disagreements (sync_source overrode A/M): {}",
        counters.label_disagreements
    );
    eprintln!(
        "project manifests in changesets: {} (workspace projects: {})",
        counters.manifest_changes,
        cold.workspace.projects.len()
    );
    eprintln!(
        "§7  Global chain ≡ cold : {}",
        if dg.is_empty() {
            "PASS".into()
        } else {
            format!("FAIL — {}", dg.join(" | "))
        }
    );
    eprintln!(
        "§7  Scoped chain ≡ cold : {}",
        if ds.is_empty() {
            "PASS".into()
        } else {
            format!("FAIL — {}", ds.join(" | "))
        }
    );

    // ---- assertions ----
    assert!(
        view_drift.is_none(),
        "the replayed Source view drifted from the real tree at the final commit \
         (a changeset missed a file): {}",
        view_drift.unwrap()
    );
    let bad: Vec<&Row> = rows.iter().filter(|r| !r.diverged.is_empty()).collect();
    assert!(
        bad.is_empty(),
        "Global ≠ Scoped on {} of {} real commits: {:?}",
        bad.len(),
        rows.len(),
        bad.iter()
            .map(|r| format!("{} ({}) {:?}", &r.sha[..12], r.subject, r.diverged))
            .collect::<Vec<_>>()
    );
    assert!(
        dg.is_empty(),
        "ADR-0032 §7: Global incremental ≢ cold: {dg:?}"
    );
    assert!(
        ds.is_empty(),
        "ADR-0032 §7: Scoped incremental ≢ cold: {ds:?}"
    );
    // ADR-0042 Phase 1a.5: coverage of the rename/copy branch must be *asserted*
    // on a range known to contain renames, not assumed from "history has renames".
    if std::env::var("FILIGRIO_GIT_EXPECT_RENAMES").is_ok() {
        assert!(
            counters.renames + counters.copies > 0,
            "FILIGRIO_GIT_EXPECT_RENAMES is set but the R/C branch was never hit — \
             this range does not exercise renames (or `-M -C` stopped detecting them)"
        );
        assert!(
            counters.rename_in_corpus > 0,
            "renames were seen ({} R / {} C) but none touched the corpus, so the \
             rename path never reached the engine",
            counters.renames,
            counters.copies
        );
    }
}
