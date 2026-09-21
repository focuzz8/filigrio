"""Oracle diff — human-readable one-shot CLI.

Builds the shared fixture with both implementations and prints the resolution,
clustering, and build-time comparisons. The same checks run as assertions under
`uv run pytest`; this CLI is for a quick look.

    uv run python -m tools.oracle_diff [--oracle-bin <graphify>]
        [--tolerance 1.0] [--min-nmi F] [--release] [--keep]
"""

from __future__ import annotations

import argparse
import shutil
import sys
import tempfile
from pathlib import Path

from . import harness, report
from .compare import clustering_metrics, resolution_metrics


def main() -> int:
    ap = argparse.ArgumentParser(prog="oracle-diff")
    ap.add_argument("--oracle-bin", default=None,
                    help="Python graphify CLI (default: sibling repo venv)")
    ap.add_argument("--fixture", default=None, choices=harness.fixtures(),
                    help="a single fixture to diff (default: all)")
    ap.add_argument("--tolerance", type=float, default=1.0,
                    help="min fraction of oracle resolved-calls we must match (default 1.0)")
    ap.add_argument("--min-nmi", type=float, default=None,
                    help="if set, gate on clustering NMI >= this (default: report only)")
    ap.add_argument("--release", action="store_true",
                    help="build+time our release binary (fairer perf comparison)")
    ap.add_argument("--keep", action="store_true", help="keep the temp work dir")
    args = ap.parse_args()

    oracle_bin = harness.find_oracle(args.oracle_bin)
    if oracle_bin is None:
        print("error: Python graphify oracle not found; pass --oracle-bin", file=sys.stderr)
        return 2

    print(f">> cargo build ({'release' if args.release else 'debug'}) …")
    ours_bin = harness.cargo_build(release=args.release)
    names = [args.fixture] if args.fixture else harness.fixtures()

    ok = True
    for name in names:
        print(f"\n########## fixture: {name} ##########")
        work = Path(tempfile.mkdtemp(prefix=f"oracle-diff-{name.replace('/', '-')}-"))
        try:
            built = harness.build_graphs(work, harness.FIXTURES_DIR / name, oracle_bin, ours_bin)
            res = resolution_metrics(str(built.oracle_graph), str(built.ours_graph))
            clus = clustering_metrics(str(built.oracle_graph), str(built.ours_graph))
            report.print_resolution(res, args.tolerance)
            print()
            report.print_clustering(clus, args.min_nmi)
            report.print_timing(built.t_oracle, built.t_ours, args.release)
            ok = ok and res.recall >= args.tolerance and not res.ours_only_unexplained
            ok = ok and (args.min_nmi is None or clus.nmi >= args.min_nmi)
        finally:
            if args.keep:
                print(f"(kept work dir: {work})")
            else:
                shutil.rmtree(work, ignore_errors=True)
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
