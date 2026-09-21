"""Human-readable rendering of the compare.py metrics (for the CLI)."""

from __future__ import annotations

from .compare import Clustering, Resolution


def _show_edge(e) -> str:
    s, rel, d, c = e
    d = d if isinstance(d, str) else f"{d[0]}({d[1]})"
    return f"    {s}  --{rel}[{c}]-->  {d}"


def print_resolution(res: Resolution, tolerance: float) -> None:
    print("=" * 72)
    print("  ORACLE DIFF — resolved `calls` edges (filigrio vs Python graphify)")
    print("=" * 72)
    print(f"  oracle resolved calls : {res.o_calls}")
    print(f"  ours   resolved calls : {res.r_calls}")
    print(f"  matched (endpoints+confidence): {res.matched}")
    print(f"  recall (of oracle)    : {res.recall:.0%}")
    print(f"  precision (of ours)   : {res.precision:.0%}")
    if res.oracle_only:
        print("\n  -- in oracle, missing from ours --")
        for e in res.oracle_only:
            print(_show_edge(e))
    if res.ours_only_unexplained:
        print("\n  -- in ours, not in oracle (unexplained — SPURIOUS) --")
        for e in res.ours_only_unexplained:
            print(_show_edge(e))
    print("\n  -- accepted divergences (documented; not scored) --")
    print(f"    reciprocal calls: ours keeps both directions ({res.reciprocal_extra}), "
          "oracle collapses same-endpoint pairs to one")
    print(f"    unresolved calls: oracle drops ({res.o_unresolved}), "
          f"ours surfaces as Symbol ({res.r_unresolved})")
    print(f"    `references` (type usage): oracle emits {res.o_references}, ours 0 "
          "(Phase-1 extractor scope)")
    print(f"    imports: oracle {res.o_imports} (kept unresolved), "
          f"ours {res.r_imports} (resolved to def)")
    ok = res.recall >= tolerance
    print("\n  RESULT:", "PASS" if ok else "FAIL",
          f"(recall {res.recall:.0%} >= tolerance {tolerance:.0%})")
    print("=" * 72)


def print_clustering(clus: Clustering, min_nmi: float | None) -> None:
    print("=" * 72)
    print("  CLUSTERING — community partition (NMI / ARI over shared nodes)")
    print("=" * 72)
    print(f"  oracle communities : {clus.oracle_communities}")
    print(f"  ours   communities : {clus.ours_communities}")
    print(f"  shared nodes       : {clus.shared}   (core, non-hub: {clus.core_shared})")
    print(f"  NMI  all / core    : {clus.nmi:.3f} / {clus.nmi_core:.3f}   (1.0 = identical partition)")
    print(f"  ARI  all / core    : {clus.ari:.3f} / {clus.ari_core:.3f}   (core excludes `file` hubs)")
    print("\n  -- per-node (oracle → ours community) --")
    for key, oc, rc in clus.per_node:
        print(f"    {key:28}  {oc} → {rc}")
    if min_nmi is not None:
        print(f"\n  gate: NMI {clus.nmi:.3f} >= {min_nmi:.3f} →",
              "PASS" if clus.nmi >= min_nmi else "FAIL")
    print("=" * 72)


def print_timing(t_oracle: float, t_ours: float, release: bool) -> None:
    profile = "release" if release else "debug"
    faster = "ours" if t_ours < t_oracle else "oracle"
    lo, hi = min(t_oracle, t_ours), max(t_oracle, t_ours)
    ratio = hi / lo if lo > 0 else float("inf")
    print("=" * 72)
    print("  BUILD TIME (wall clock, cold; fixture-scale — a regression signal)")
    print("=" * 72)
    print(f"  oracle (update+cluster) : {t_oracle * 1000:7.1f} ms")
    print(f"  ours   (build, {profile:<7}) : {t_ours * 1000:7.1f} ms")
    print(f"  → {faster} faster by {ratio:.1f}x"
          + ("" if release else "   (debug build — pass --release for a fair speed comparison)"))
    print("=" * 72)
