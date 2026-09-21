# filigrio — walking-skeleton (backbone) implementation

A **backbone** of the Rust port described in [`../docs/hld.md`](../docs/hld.md).
Every crate boundary from HLD §3 exists and one request flows end to end through
the five ports (HLD §4). The wiring was proven first (Phase 0/1) with most
stages **mocked**; stages are now being turned real one at a time per
[`../docs/migration-plan.md`](../docs/migration-plan.md). **Real so far:**
ingestion (fs walk), the **Rust / Python / TS-JS extractors** (tree-sitter, plus
an optional oxc TS/JS frontend), **cross-file resolution** (`apply` steps 1–3 —
incremental relink via the `ReverseIndex`), **clustering** (`apply` step 4 —
modularity-based, warm-started, id-stable), **query** (a petgraph index:
traversal, degree, shortest-path, trigram+IDF seed ranking), **storage** (native
`state.json` + a `graph.json` interchange adapter, import + export — ADR-0017),
and the **resident daemon + engine-free clients** (ADR-0032f). The remaining
stub is the `MockExtractor` fallback for languages with no real frontend.

> For the current-state map — crate roles, runtime flows, glossary, which ADR
> owns what — read [`../docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md). This
> README is the short orientation and the commands.
>
> **If you want to *use* it** — install it, index a repo, wire it into an agent,
> keep it fresh — read [`../README.md`](../README.md) instead. This file is aimed
> at someone working on the port.

## What is real vs mock

| Crate | Port | Status | Notes |
|-------|------|--------|-------|
| `filigrio-core` | — | **real** | All domain types + the 5 port traits. No I/O. |
| `filigrio-ingest` | `Source` | **real** | `FsSource` actually walks a dir, ignores `.git`/`target`/… , classifies files. Warm `poll(Some(rev))` still falls back to a full walk. |
| `filigrio-index` | `Extractor` | **real (Rust + Python + TS/JS) / mock (fallback)** | `RustExtractor` (`.rs`) + `PythonExtractor` (`.py`) + `TypeScriptExtractor` (`.ts`/`.tsx`/`.js`) — real tree-sitter AST → `file`/`function`/`class`/`struct`/`enum`/`trait` nodes + `calls`/`imports` `Symbol` edges, edit-stable ids. **Calls carry a receiver-type hint** for method-homonym disambiguation: the *static* languages (Rust, TS) infer from `new`/annotations/`self`/`this` via local dataflow — a type-directed link is certain (`EXTRACTED` even cross-file); *dynamic* Python resolves only certain receivers and **drops** the rest (no name-guessing). Methods are contained by their class (Python/TS); TS also lifts arrow-consts (`const f = () => …`) to function nodes. `MockExtractor` is the fallback via `DispatchExtractor`. |
| `filigrio-resolve` | — (`apply`) | **real** | Steps 1–3 (Phase 2a): cross-file link with `EXTRACTED`(same-file)/`INFERRED`(cross-file)/`AMBIGUOUS`(≥2 defs) provenance, unresolved edges surfaced as `Symbol`, parallel-edge dedup, **incremental relink** via the `ReverseIndex` (a def change re-binds exactly its dependents, no rescan; incremental == cold). **Receiver-type narrowing:** a call's type hint (from `T::method` *and* inferred `x.method()`/`self.method()` receivers) picks the right type's method over homonyms (`Vec::new`-style unknown types → unresolved, not mis-bound) — splits the merged-constructor god-node, cut AMBIGUOUS calls 8%→2% on our own graph. **Opaque-receiver decline (ADR-0023):** a method call whose receiver type could *not* be inferred (`recv=opaque` — method chains, builders, external types) refuses the cross-file bare-name bind, staying honest-unresolved; same-file defs still bind. Kills the fake `iter`/`map`/`clone` god nodes (dogfood: 1490/1532 opaque calls declined, oracle diff unchanged). Step 4 (Phase 2b): **modularity clustering** (`cluster`) — a deterministic Louvain local-move over the resolved graph, warm-started from the prior partition, with **community-id stability** (max-overlap remap). Two strategies (ADR-0024): `Simple` (one single-level local-move pass, the default) and `Full` (**multi-level** Louvain). Leiden refinement is still a follow-on. |
| `filigrio-store` | `GraphStore` | **real (simple format)** | `FsStore` — a dir with native `state.json` + a **`graph.json` interchange** snapshot (`graphjson::{import, export}`, schema-compatible with Python graphify — ADR-0017), bulk `apply_delta`, shrink-guard. `MemoryStore` for demo/tests. Native format is deliberately serde JSON — swappable to redb/columnar behind the port. |
| `filigrio-query` | `GraphQuery` | **real** | petgraph `DiGraph` built at load: BFS/DFS-to-budget, degree-ranked god nodes (structural `contains`/`imports` excluded), A* `shortest_path`. Phase 2b slice 2 — **analysis/report** (`report`, `render_markdown`): deterministic **community labels** (top semantic node), **cross-community bridges**, confidence breakdown → `GRAPH_REPORT.md`. Slice 3 — **retrieval** (`seed_scores`): fuzzy **IDF + trigram** seed ranking (typo-tolerant, rare-trigram-weighted), **budget packing** in seed-priority order, DFS + relation `context_filter`; results **cite community labels** (`Subgraph.communities`). |
| `filigrio-client-mcp` | — | **real** | Ships the `filigrio-mcp` **bridge binary** — line-delimited JSON-RPC 2.0 over stdio (a subset of MCP), holding no graph and forwarding each tool call to the daemon. The **9 tools** are `query_graph`, `get_node`, `get_neighbors`, `god_nodes`, `list_communities`, `get_community`, `shortest_path`, `graph_stats`, `project_graph` (verified against a live `tools/list`), rendered **`serve.py`-equivalent** (`NODE …`/`EDGE …` lines, confidence UPPERCASE). **Token-bounded** output (`token_budget`, ~3 chars/token, truncation marker) + label **sanitization** at this boundary (ADR-0006/0013). Data plane only: it builds with the protocol's `control` feature **off**, so it has no mutation surface at compile time (ADR-0042 F9). Its library half is the in-process `McpServer` that `filigrio-classic mcp serve` still uses. |
| `filigrio-pipeline` | `WorkQueue` | **real wiring** | `Pipeline` (cold/warm build) + `Worker` (reconciler, inline) + `ChannelQueue` (in-proc `WorkQueue`). |
| `filigrio-protocol` | — | **real** | The one wire contract (ADR-0032f §2/§3): `Request`/`Response` split into a data plane and a `control`-gated control plane, the length-prefixed socket framing, and the `SocketClient`. Depends only on `filigrio-core`. |
| `filigrio-daemon` | — | **real** | Ships `filigrio-daemon`, the **composition root**: it is the only supported process that links the engine. Owns the registry, work queue + scheduler, per-project locks, the LRU state cache, the write-behind flusher, the watcher lifecycle, and the responder that serves the contract. |
| `filigrio-client-core` | — | **real** | Shared client plumbing: default socket path, project-path resolution, the auto-start handshake (flock-guarded), and the one-shot runner. |
| `filigrio-client-cli` | — | driver | Ships **`filigrio`**, the engine-free human CLI — a thin proxy over the contract, no graph state at runtime *or* link time. Groups: `daemon` (start/stop/status), `project` (register/remove/list/status/index/watch/export/flush), `graph` (query/god/path/stats/get-node/neighbors/project-graph). |
| `filigrio-classic` | — | driver (legacy) | Ships **`filigrio-classic`**, the pre-ADR-0032f **engine-linked** binary, kept for the direct one-shot build/analysis path: `graph build/import/query/god/report/path`, `project list`, `mcp serve`. The only binary carrying the `ts-oxc` feature. |

## The three flows you asked to prove

- **Ingestion:** `FsSource::poll` → `ChangeSet` → `filigrio_ingest::classify` → `Artifact`.
- **Worker API:** `WorkQueue<Job>` → `Worker::run_once` → `Engine::apply(prior, Δ)` → `GraphDelta` → `GraphStore::apply_delta`. This is the reconciler of ADR-0015 (inline/day-1 posture) and the stateful operator of ADR-0016.
- **MCP:** JSON-RPC line → the `filigrio-mcp` bridge → daemon over the §3
  contract → `GraphQuery` → token-ish text envelope. (`filigrio-classic mcp
  serve` runs the same envelope in-process against a store.)

## Run it

Three binaries. `filigrio` + `filigrio-daemon` are the supported pair;
`filigrio-classic` is the legacy engine-linked one-shot.

```sh
just gate            # fmt + clippy + the whole default test suite
just gate-features   # the non-default feature configs
```

**Daemon + thin client** (ADR-0032f/0042 — the daemon auto-starts on first use):

```sh
cargo build -p filigrio-client-cli -p filigrio-daemon
alias filigrio=./target/debug/filigrio

filigrio project register            # register the cwd
filigrio project index               # build/update its graph (runs to completion)
filigrio graph stats
filigrio graph god --top 10
filigrio graph query poll --depth 2
filigrio project watch on            # converge, then follow changes live
filigrio project export              # write the graph.json interchange snapshot
filigrio daemon status               # add --verbose for the client's own logs (stderr)
filigrio daemon stop
```

**MCP bridge** — what an agent connects to (stdout is the JSON-RPC transport,
logs go to stderr):

```sh
cargo build -p filigrio-client-mcp
echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"graph_stats","arguments":{}}}' \
  | ./target/debug/filigrio-mcp
```

**`filigrio-classic`** — engine in-process, no daemon; `--store` is a directory
(default `.filigrio`):

```sh
cargo build -p filigrio-classic
alias gc=./target/debug/filigrio-classic

gc graph build ./crates --store /tmp/gx
gc graph query poll   --store /tmp/gx
gc graph god          --store /tmp/gx --top 10
gc graph report       --store /tmp/gx --out GRAPH_REPORT.md
gc graph path build apply --store /tmp/gx   # → build → reconcile_and_apply → apply
gc project list       --store /tmp/gx
# migrate an existing Python graphify graph in (ADR-0017):
gc graph import ./graphify-out/graph.json --store /tmp/gx
```

`graph build` writes `/tmp/gx/state.json` (native) and `/tmp/gx/graph.json`
(graphify-schema — loads in Python graphify, the differential oracle). On the
daemon path the snapshot is **not** written by every apply (ADR-0042 F2):
`filigrio project export` is its only producer.

## Deliberate simplifications (where the real work goes)

- `GraphDelta` carries the full resolved edge set (store replaces wholesale)
  rather than a true edge-level patch. Resolution itself *is* incrementally
  relinked via the `ReverseIndex` (Phase 2a); the delta is just materialized as
  a full set for the simple store.
- `apply` re-clusters all nodes each call. Warm-start + community-id stability
  (HLD §11.1–11.2) **landed**; **Leiden refinement** has not.
- `Source::poll(Some(rev))` ignores `rev` (full walk). **Git-diff / webhook
  sources** are **Phase 5** (ADR-0014). The daemon does not use `poll` — its
  changes arrive from the fs watcher as a Producer (ADR-0032e).

Grep for `MOCK`, `Skeleton`, and `Phase 2/5` in the sources to find every seam
where real behaviour is stubbed.
