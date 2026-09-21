# oracle-diff — differential check vs Python graphify

The Phase-2a/2b fidelity check (migration-plan §Phase 2, ADR-0017): our resolved
edges *and* community partition should track the Python graphify oracle. This
tool is that check, kept runnable so it doesn't rot — it also times both builds
so a performance regression shows up.

It ships two front-ends over one comparison core (`compare.py`, `harness.py`):

```sh
# 1. the pytest suite (assertions — for CI / TDD):
uv run pytest                       # from the repo root
uv run pytest -k clustering -s      # one area, with the readout

# 2. the CLI (one human-readable report):
uv run python -m tools.oracle_diff              # debug build
uv run python -m tools.oracle_diff --release    # time our optimized binary
uv run python -m tools.oracle_diff --min-nmi 0.8  # also gate on clustering NMI
uv run python -m tools.oracle_diff --keep       # keep the temp work dir
```

The suite **skips cleanly** when the Python graphify oracle (sibling repo venv)
or `cargo` is missing, so it never fails off-box. Both graphs are built once per
session and shared across the assertions.

## Layout

| File | Role |
|------|------|
| `compare.py` | pure metrics — `resolution_metrics`, `clustering_metrics` (NMI/ARI). No I/O beyond reading the two graphs; no printing. |
| `harness.py` | build orchestration — run oracle + ours over the fixture, timed; `available()` guards the skip. |
| `report.py` | human-readable printers for the CLI. |
| `__main__.py` | the CLI. |
| `tests/` | the pytest suite (assertions, parametrized over fixtures). |
| `fixtures/<lang>/<name>/src/` | shared inputs, grouped by language (Rust today; the prefix leaves room for the multi-language phase). |

## Fixtures

| Fixture | Purpose |
|---|---|
| `rust/basic` | resolution corner cases (same-file/cross-file/ambiguous/unresolved, scope preference). Its communities happen to be per-file. |
| `rust/cross_cutting` | two modules that each **span two files** (dense intra-module calls, one bridge). Correct clustering cuts across directories — a per-file clusterer would give 4 communities; both tools give **2**, each spanning two files. This gives the NMI metric teeth. |
| `rust/call_vs_file` | call structure and file structure in **direct conflict** (4 triangles interleaved so each file holds one node from every triangle). Both tools cluster by **call** structure (identical semantic partition, `nmi_core` = 1.0); they differ only on the arbitrary community of the symmetric **file hub-nodes**, so all-node `nmi` is ~0.67. Proves we match Leiden on the part that matters and isolates the benign disagreement. |
| `python/basic` | the **2nd language**. Classes with homonym methods (`Circle.area` / `Square.area`), `self.method()` resolution, cross-file imports + constructor calls. Validates that the resolver/clustering are language-agnostic: methods contained by their class (so a class + its methods form one community), and member calls resolve by receiver type or are dropped (dynamic typing — no name-guessing). |
| `typescript/basic` | the **3rd language** (statically typed). Cohesive classes (`this.method()` intra-class calls), a cross-file `new Circle()` + `c.describe()` member call, named imports. Exercises **type-directed member resolution** — `c.describe()` binds to `Circle.describe` `EXTRACTED` (certain via the `new Circle()` type) even cross-file, matching graphify. Arrow-const functions become nodes. |

## What it does (per fixture)

1. Copies the fixture (`fixtures/<lang>/<name>/src/*.rs`) to a temp dir.
2. Builds it with the **oracle** — `graphify update --no-cluster` then
   `cluster-only --no-viz --no-label` (both no-LLM) → `graphify-out/graph.json`
   (node-link JSON: edges under `links`, communities on nodes).
3. Builds it with **ours** — `filigrio-classic graph build` (extract+resolve+cluster
   in one; the engine-linked binary — the thin `filigrio` client has no build verb) →
   `graph.json` (edges under `edges`, communities on nodes).
4. Normalizes both and compares (`compare.py`), timing steps 2 and 3.

Node ids differ between the tools, so nodes are keyed by
`(basename(source_file), symbol)` — fixtures use unique filenames, so this is
unambiguous. Edges compare as `(src, relation, dst, confidence)`, with
`imports_from` aliased to our `imports`.

## The metrics

**Resolution (pass/fail): resolved `calls` edges** — both endpoints are real
nodes; endpoints *and* confidence must match. `--tolerance` is the minimum
fraction of the oracle's resolved calls we must reproduce (default `1.0`).
Current result: **5/5, recall 100%, precision 100%** — including the
scope-preference case (a same-file def shadows a cross-file homonym →
`EXTRACTED`, not `AMBIGUOUS`), which this diff is what surfaced.

**Clustering: the community partition**, compared label-invariantly by **NMI**
and **ARI**, reported two ways:
- **`nmi_core` / `ari_core`** over *semantic* nodes (functions/types) — the real
  fidelity signal. The suite gates on `nmi_core >= 0.9`.
- **`nmi` / `ari`** over *all* nodes, including `file` hub-nodes. A file whose
  functions split across communities is a symmetric tie, so its assignment is
  arbitrary and differs harmlessly between the two clusterers — this drags the
  all-node number down (only a loose `>= 0.5` sanity floor).

Result: **`nmi_core` = 1.000 on all three fixtures** — our single-level Louvain
local-move reproduces graphify's Leiden *semantic* partition node-for-node,
including where communities **span files** (`cross_cutting`) and where call- and
file-structure **conflict** (`call_vs_file`, whose all-node `nmi` is ~0.67 purely
from hub tie-breaking). Real parity evidence on non-trivial structure; single
level still isn't *guaranteed* to match Leiden on larger graphs, hence the floor
rather than an equality gate.

**Build time (regression signal): wall-clock of each build**, cold, at
fixture-scale. Ours is a *debug* build unless `--release`. Meaningful mainly as a
self-regression tripwire and on larger inputs (process startup dominates here).

## Documented, accepted divergences (reported, not scored)

These are deliberate port choices or out-of-scope-for-2a extractor gaps, not
resolution bugs:

| Divergence | Oracle | Ours | Why |
|---|---|---|---|
| **Reciprocal calls** (`a→b` *and* `b→a`) | collapses same-endpoint pairs to one edge | keeps **both directions** | Direction is a real fact in a call graph (who calls whom) — the query layer needs it for callers-vs-callees / impact analysis. Surfaced by `rust/cross_cutting`; recall stays 100%, so these are reciprocal-explained, not spurious. (Clustering still projects to *undirected, one edge per endpoint pair*, so direction doesn't skew community detection.) |
| **Unresolved calls** (no def anywhere, e.g. an extern fn) | dropped | kept as `Symbol` | Phase-2a deliberately **surfaces** unresolved edges (honest provenance) instead of dropping them. |
| **`references`** (return-type / type usage) | emitted | not emitted | Our Phase-1 Rust extractor doesn't emit type-usage edges yet — extractor scope, not resolution. |
| **`imports`** target | kept unresolved (bare symbol) | resolved to the def (`INFERRED`) | We link in-repo imports to their definition; the oracle leaves the import symbol unresolved. |
| **Node ids / `links` vs `edges`** | `src_lib_helper`, `links` | `fn:lib.rs:helper`, `edges` | Native id scheme + key name differ; interchange is semantic, not byte-identical (ADR-0017). The importer tolerates both keys. |
| **Python method labels** (`python/*`) | `.area()`, `helper()` | `area`, `helper` | Decorative leading `.` / trailing `()`; `_strip_parens` normalizes both sides for keying. Bare symbol name is the same. |
| **Python unresolvable member calls** (`python/*`) | dropped | dropped | We match the oracle here: a `x.method()` whose receiver type isn't statically certain is dropped, not name-resolved to a homonym (dynamic typing). Only `self.`/`Class.`/annotated receivers resolve. |

When the Phase-1 extractor or resolution rules change, re-run this and update the
table (and the tolerance) rather than letting the two silently drift.
