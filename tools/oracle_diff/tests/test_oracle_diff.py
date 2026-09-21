"""Differential-diff suite: filigrio vs the Python graphify oracle.

An *integration* suite — it builds each shared fixture with both implementations.
It skips cleanly when the oracle binary or cargo is unavailable, so it never
fails off-box. Each fixture's two graphs are built once per session and shared
across the assertions for that fixture.

    uv run pytest                          # from the repo root
    uv run pytest -k cross_cutting -s      # one fixture, with the readout

Fixtures (`fixtures/<lang>/<name>/`):
  * `rust/basic`         — resolution corner cases; communities happen per-file.
  * `rust/cross_cutting` — two modules that each SPAN two files, so correct
                           clustering cuts across dirs (per-file would fail).
  * `rust/call_vs_file`  — call structure and file structure in direct conflict
                           (4 triangles interleaved across 3 files). Both tools
                           agree on the semantic partition; they differ only on
                           the arbitrary assignment of symmetric file hub-nodes —
                           so `nmi` (all nodes) is low but `nmi_core` (functions)
                           is 1.0. Guards that the *semantic* clustering matches.
"""

from __future__ import annotations

import pytest

from tools.oracle_diff import harness
from tools.oracle_diff.compare import clustering_metrics, resolution_metrics

pytestmark = pytest.mark.skipif(
    not harness.available(), reason="Python graphify oracle or cargo not available"
)

# The real fidelity gate is on the *semantic* partition (functions/types),
# excluding symmetric `file` hub-nodes whose assignment is an arbitrary tie.
CORE_NMI_FLOOR = 0.9
# The all-node NMI is a loose sanity floor only (hub tie-breaking drags it down).
OVERALL_NMI_FLOOR = 0.5

# Per-fixture expectations (keyed by `<lang>/<name>`) layered on the generic
# checks. Recognized keys:
#   * `cross_file_community` (bool) — a community must span >1 file.
#   * `extra_calls` (list[edge-key]) — resolved `calls` edges **we** have that the
#     oracle does NOT, and that are *correct*. The oracle is a reference, not a
#     ceiling: where we legitimately exceed it we PIN the exact extra edges here.
#     The spurious-calls test then asserts our ours-only set is *exactly* this —
#     so a **new** unexpected resolution fails (no silent degradation into wrong
#     edges) AND **losing** a documented lead fails (the pinned edge disappears).
#     An edge-key is `(src, "calls", dst, CONF)` with `src`/`dst` = `basename::sym`.
#   * `skip_clustering` (str) — reason a fixture is too small for a meaningful
#     clustering comparison (single-level Louvain vs the oracle's Leiden disagree
#     on tiny dense graphs; they agree on the richer fixtures). Skips ONLY the
#     clustering-partition tests — resolution stays fully asserted.
EXPECT = {
    "rust/basic": {"cross_file_community": False},
    "rust/cross_cutting": {"cross_file_community": True},
    "rust/call_vs_file": {"cross_file_community": True},
    # Rust `pub use crate::api::greet as hello` re-export (ADR-0020 gap closed):
    # `handlers` imports `hello` through the `prelude` barrel and calls it. We
    # follow the mod-path re-export to `api::greet` and bind EXTRACTED; graphify
    # does not follow re-exports, so it leaves `hello()` unresolved (no `hello`
    # def). A pinned ours-only lead, exactly like `typescript/reexport_alias`.
    "rust/reexport": {
        "extra_calls": [
            ("handlers.rs::boot", "calls", "api.rs::greet", "EXTRACTED"),
        ],
    },
    # The "negative zone": two modules define the SAME name `greet`. Name-based
    # (file/project-level) resolution is genuinely AMBIGUOUS, so graphify leaves
    # `boot`'s call unresolved. `use crate::api::greet` resolves vs the MODULE —
    # we bind to *api*'s greet specifically (EXTRACTED). The disambiguation only
    # module-scoped resolution can do; a pinned ours-only lead over the oracle.
    "rust/module_scope": {
        "extra_calls": [
            ("handlers.rs::boot", "calls", "api.rs::greet", "EXTRACTED"),
        ],
    },
    # We follow aliased re-exports (`export { greet as hello } from …`); graphify
    # does not — so we resolve `boot → greet` (via `hello`), it doesn't. Pinned.
    "typescript/reexport_alias": {
        "extra_calls": [
            ("index.ts::boot", "calls", "api.ts::greet", "EXTRACTED"),
        ],
        "skip_clustering": (
            "tiny (<6 semantic nodes) fixture — single-level Louvain and the "
            "oracle's Leiden disagree on small dense graphs; clustering fidelity "
            "is gated on the richer rust/* fixtures (nmi_core 1.0 there)"
        ),
    },
}

# Target fixtures that document a known gap live here as `xfail(non-strict)`
# until the fix lands, then graduate to full green. Both `typescript/monorepo`
# (ADR-0018 project-scoped resolution) and `typescript/barrel` (ADR-0020
# symbol-level / re-export resolution) have graduated. Empty; add the next gap.
XFAIL_UNTIL_PROJECT_RESOLUTION: dict[str, str] = {}


@pytest.fixture(autouse=True)
def _xfail_known_gaps(request):
    params = getattr(getattr(request.node, "callspec", None), "params", {})
    name = params.get("case")
    if name in XFAIL_UNTIL_PROJECT_RESOLUTION:
        request.node.add_marker(
            pytest.mark.xfail(reason=XFAIL_UNTIL_PROJECT_RESOLUTION[name], strict=False)
        )


@pytest.fixture(scope="session", params=harness.fixtures())
def case(request, tmp_path_factory):
    name = request.param
    work = tmp_path_factory.mktemp("oracle-diff-" + name.replace("/", "-"))
    ours_bin = harness.cargo_build(release=False)
    built = harness.build_graphs(work, harness.FIXTURES_DIR / name, harness.find_oracle(), ours_bin)
    og, rg = str(built.oracle_graph), str(built.ours_graph)
    # Also build ours with the multi-level `Full` strategy (ADR-0024) over the
    # same tree. The oracle itself runs multi-level Louvain, so `Full` is the
    # apples-to-apples comparison — the `clus_full` metrics gate that our
    # aggregation agrees with the oracle at least as well as single-level does.
    ours_full = str(harness.build_ours(built.workdir, ours_bin, "ours-full", ["--cluster", "full"]))
    return {
        "name": name,
        "expect": EXPECT.get(name, {}),
        "res": resolution_metrics(og, rg),
        "clus": clustering_metrics(og, rg),
        "clus_full": clustering_metrics(og, ours_full),
        "t_oracle": built.t_oracle,
        "t_ours": built.t_ours,
    }


# ---- resolution (Phase 2a — the correctness contract) -----------------------

def test_resolved_calls_recall_is_total(case):
    res = case["res"]
    assert res.o_calls > 0, "fixture must exercise resolved cross-file calls"
    assert res.recall == 1.0, f"missed oracle resolved calls: {res.oracle_only}"


def test_ours_only_calls_are_exactly_expected(case):
    # Ours-only resolved calls must be EXACTLY the documented set: the reciprocal
    # (reverse-of-oracle) edges the oracle collapses, plus any `extra_calls`
    # pinned in EXPECT where we legitimately resolve more than the oracle. This
    # never skips: an UNEXPECTED extra (a new wrong resolution) fails, and a
    # DISAPPEARED expected extra (degrading to the oracle) also fails.
    expected = {tuple(e) for e in case["expect"].get("extra_calls", [])}
    got = set(case["res"].ours_only_unexplained)
    assert got == expected, (
        f"ours-only resolved calls differ from expected.\n"
        f"  unexpected (new / possibly wrong): {sorted(got - expected)}\n"
        f"  missing (lost lead over oracle):   {sorted(expected - got)}"
    )


# ---- clustering (Phase 2b — a quality signal) -------------------------------

def test_clustering_does_not_collapse(case):
    assert case["clus"].ours_communities > 1, "clustering must find real structure"


def _skip_clustering_if_tiny(case):
    reason = case["expect"].get("skip_clustering")
    if reason:
        pytest.skip(f"clustering not measured: {reason}")


def test_community_count_close_to_oracle(case):
    _skip_clustering_if_tiny(case)
    clus = case["clus"]
    assert abs(clus.oracle_communities - clus.ours_communities) <= 1, (
        f"community count drift: oracle {clus.oracle_communities}, ours {clus.ours_communities}"
    )


def test_semantic_partition_matches_oracle(case):
    _skip_clustering_if_tiny(case)
    # The real check: agreement on the function/type partition (hubs excluded).
    clus = case["clus"]
    assert clus.nmi_core >= CORE_NMI_FLOOR, (
        f"semantic-partition NMI {clus.nmi_core:.3f} below floor {CORE_NMI_FLOOR}: {clus.per_node}"
    )


def test_overall_partition_sane(case):
    _skip_clustering_if_tiny(case)
    # Loose sanity floor over all nodes (hub tie-breaking can drag this down).
    clus = case["clus"]
    assert clus.nmi >= OVERALL_NMI_FLOOR, (
        f"all-node NMI {clus.nmi:.3f} below sanity floor {OVERALL_NMI_FLOOR}: {clus.per_node}"
    )


def test_clustering_is_cross_file_when_expected(case):
    if not case["expect"].get("cross_file_community"):
        pytest.skip("fixture communities are not expected to span files")
    # Some ours-community must contain nodes from >1 distinct file — proving the
    # clustering is structural, not per-directory.
    by_comm: dict[int, set[str]] = {}
    for key, _oc, rc in case["clus"].per_node:
        by_comm.setdefault(rc, set()).add(key.split("::", 1)[0])
    assert any(len(files) > 1 for files in by_comm.values()), (
        f"expected a community spanning multiple files, got {by_comm}"
    )


# ---- multi-level `Full` strategy (ADR-0024) ---------------------------------
# The oracle runs multi-level Louvain, so our `Full` strategy — not the default
# single-level `Simple` — is the true apples-to-apples clusterer. These gate that
# our aggregation is correct: it must agree with the oracle's semantic partition,
# and switching Simple→Full must not *lose* oracle agreement.

def test_full_strategy_matches_oracle_semantic_partition(case):
    _skip_clustering_if_tiny(case)
    clus = case["clus_full"]
    assert clus.nmi_core >= CORE_NMI_FLOOR, (
        f"Full-strategy semantic-partition NMI {clus.nmi_core:.3f} below floor "
        f"{CORE_NMI_FLOOR}: {clus.per_node}"
    )


def test_full_strategy_no_worse_than_simple_vs_oracle(case):
    _skip_clustering_if_tiny(case)
    # Multi-level aggregation must not degrade agreement with the oracle's own
    # multi-level partition (a small epsilon absorbs id-tie noise). This is the
    # "is Full ok?" tripwire the whole comparison exists to answer.
    simple, full = case["clus"], case["clus_full"]
    assert full.nmi_core >= simple.nmi_core - 1e-9, (
        f"Full regressed vs oracle: nmi_core simple={simple.nmi_core:.3f} "
        f"full={full.nmi_core:.3f}"
    )


# ---- performance (a coarse regression tripwire) -----------------------------

def test_build_not_slower_than_oracle(case):
    # Deliberately loose: we are ~orders faster; trips only on a gross regression.
    assert case["t_ours"] < case["t_oracle"], (
        f"ours {case['t_ours']*1000:.0f}ms >= oracle {case['t_oracle']*1000:.0f}ms"
    )
