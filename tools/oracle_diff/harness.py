"""Build orchestration shared by the CLI and the pytest suite: run the oracle
and our binary over the shared fixture and time each build. No comparison logic
lives here (see `compare.py`)."""

from __future__ import annotations

import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from time import perf_counter

REPO_ROOT = Path(__file__).resolve().parents[2]
FIXTURES_DIR = Path(__file__).resolve().parent / "fixtures"
RS_ROOT = REPO_ROOT / "src"
DEFAULT_ORACLE = REPO_ROOT.parent / "graphify" / ".venv" / "bin" / "graphify"


def fixtures() -> list[str]:
    """Available fixtures as `<lang>/<name>` (dirs `fixtures/<lang>/<name>/src`)."""
    out = []
    for lang in sorted(p for p in FIXTURES_DIR.iterdir() if p.is_dir()):
        for fx in sorted(p for p in lang.iterdir() if (p / "src").is_dir()):
            out.append(f"{lang.name}/{fx.name}")
    return out


def find_oracle(explicit: str | Path | None = None) -> Path | None:
    """The Python graphify CLI to diff against, or None if unavailable."""
    p = Path(explicit) if explicit else DEFAULT_ORACLE
    return p if p.exists() else None


def available(oracle: str | Path | None = None) -> bool:
    """True iff both the oracle binary and cargo are present — the pytest suite
    skips when this is false, so it never fails spuriously off-box."""
    return find_oracle(oracle) is not None and shutil.which("cargo") is not None


def _run(cmd, **kw):
    """Run `cmd`, and on failure raise with the command and its output.

    Output used to go to DEVNULL, so a broken invocation surfaced as a bare
    `CalledProcessError` with no clue why — which is how this harness sat
    calling a subcommand the binary did not have.
    """
    proc = subprocess.run(cmd, capture_output=True, text=True, **kw)
    if proc.returncode != 0:
        tail = (proc.stderr or proc.stdout or "").strip()[-2000:]
        raise RuntimeError(
            f"command failed (exit {proc.returncode}): {' '.join(map(str, cmd))}\n{tail}"
        )


# The engine-linked binary. `filigrio` (package `filigrio-client-cli`) is the
# thin daemon proxy since ADR-0032f and has no `graph build` at all — the build
# verb lives on `filigrio-classic`, which is what this diff has always meant.
OURS_PKG = "filigrio-classic"
OURS_BIN = "filigrio-classic"


def cargo_build(release: bool = False) -> Path:
    """Compile our binary up front (so build timing isn't polluted by rustc)."""
    cmd = ["cargo", "build", "-q", "-p", OURS_PKG, "--bin", OURS_BIN]
    if release:
        cmd.append("--release")
    _run(cmd, cwd=RS_ROOT)
    return RS_ROOT / "target" / ("release" if release else "debug") / OURS_BIN


@dataclass
class Built:
    workdir: Path
    oracle_graph: Path
    ours_graph: Path
    t_oracle: float  # seconds
    t_ours: float


def build_graphs(workdir: Path, fixture_dir: Path, oracle_bin: Path, ours_bin: Path) -> Built:
    """Populate `workdir` from `fixture_dir` and build both graphs, timed."""
    # The oracle is a sibling checkout, routinely absent off-box. Say so here
    # rather than letting `_run` report a confusing ENOENT mid-timing.
    if not Path(oracle_bin).exists():
        raise RuntimeError(
            f"Python graphify oracle not found at {oracle_bin} — this diff needs it; "
            f"pass --oracle-bin, or check the sibling checkout ({DEFAULT_ORACLE})"
        )

    shutil.copytree(fixture_dir, workdir, dirs_exist_ok=True)

    # Oracle: extract+resolve, then Leiden clustering (both no-LLM).
    t0 = perf_counter()
    _run([str(oracle_bin), "update", str(workdir), "--no-cluster"])
    _run([str(oracle_bin), "cluster-only", str(workdir), "--no-viz", "--no-label"])
    t_oracle = perf_counter() - t0
    oracle_graph = workdir / "graphify-out" / "graph.json"

    # Ours: build (extract+resolve+cluster) in one shot. Build only the source
    # tree so the oracle's graphify-out/ isn't ingested.
    ours_store = workdir / ".ours"
    t0 = perf_counter()
    _run([str(ours_bin), "graph", "build", str(workdir / "src"), "--store", str(ours_store)])
    t_ours = perf_counter() - t0

    return Built(workdir, oracle_graph, ours_store / "graph.json", t_oracle, t_ours)


def build_ours(workdir: Path, ours_bin: Path, store_name: str, extra_args=()) -> Path:
    """Build *our* graph again over an already-populated `workdir` (the oracle
    graph is unchanged), with extra CLI flags — e.g. `--cluster full` (ADR-0024).
    Returns the graph.json path. Cheap: no oracle run, no re-copy of the fixture."""
    store = workdir / store_name
    _run([str(ours_bin), "graph", "build", str(workdir / "src"), "--store", str(store), *extra_args])
    return store / "graph.json"
