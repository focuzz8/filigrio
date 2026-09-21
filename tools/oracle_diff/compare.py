"""Pure comparison metrics for the oracle diff (no I/O beyond reading the two
graph.json files; no printing — see `report.py` for the human-readable view).

Both tools emit a node-link JSON over the *same* single-language (Rust) fixture.
Node ids differ, so nodes are keyed by `(basename(source_file), symbol)` — the
fixture uses unique filenames, so this is unambiguous — and edges compare as
`(src_key, canonical_relation, dst_key_or_UNRESOLVED, confidence)`.

Two metric families:
  * `resolution_metrics` (Phase 2a) — resolved `calls` edges: endpoints *and*
    confidence must match. This is the correctness contract.
  * `clustering_metrics` (Phase 2b) — the community partition over the shared
    node set, compared label-invariantly by NMI and ARI (plus counts).
"""

from __future__ import annotations

import json
import math
import os
from collections import Counter
from dataclasses import dataclass, field

# Ours: contains/calls/imports. Oracle also emits `references` (type usage),
# which our Phase-1 extractor does not. `imports_from` is the oracle's name for
# what we call `imports`.
RELATION_ALIAS = {"imports_from": "imports"}


def _strip_parens(label: str) -> str:
    # Normalize the oracle's decorative labels to our bare symbol names:
    # `helper()` → `helper`, and Python methods `.area()` → `area`.
    s = label[:-2] if label.endswith("()") else label
    return s[1:] if s.startswith(".") else s


def _key_of(node: dict) -> str:
    sf = node.get("source_file") or ""
    return f"{os.path.basename(sf)}::{_strip_parens(node.get('label', node['id']))}"


def _load(path: str):
    with open(path) as f:
        g = json.load(f)
    nodes = g.get("nodes", [])
    edges = g.get("edges", g.get("links", []))  # ours=edges, oracle=links
    by_id = {n["id"]: _key_of(n) for n in nodes}
    return by_id, edges, nodes


def _norm_edges(by_id, edges):
    out = []
    for e in edges:
        src = by_id.get(e["source"])
        if src is None:
            continue
        tgt = e["target"]
        if tgt in by_id:
            dst, resolved = by_id[tgt], True
        else:
            dst, resolved = ("UNRESOLVED", _strip_parens(str(tgt))), False
        out.append(
            {
                "rel": RELATION_ALIAS.get(e["relation"], e["relation"]),
                "src": src,
                "dst": dst,
                "conf": e.get("confidence"),
                "resolved": resolved,
            }
        )
    return out


def _edge_key(e):
    return (e["src"], e["rel"], e["dst"], e["conf"])


# ---- resolution -------------------------------------------------------------

@dataclass
class Resolution:
    o_calls: int
    r_calls: int
    matched: int
    recall: float
    precision: float
    oracle_only: list = field(default_factory=list)
    ours_only: list = field(default_factory=list)
    # ours_only edges whose reverse the oracle *does* have — the oracle collapses
    # reciprocal same-endpoint calls to one edge; we keep both directions. Not
    # spurious, just a directedness difference (see README).
    reciprocal_extra: int = 0
    # ours_only that is NOT reciprocal-explained — the genuinely-spurious set.
    ours_only_unexplained: list = field(default_factory=list)
    o_unresolved: int = 0
    r_unresolved: int = 0
    o_references: int = 0
    o_imports: int = 0
    r_imports: int = 0


def resolution_metrics(oracle_path: str, ours_path: str) -> Resolution:
    o = _norm_edges(*_load(oracle_path)[:2])
    r = _norm_edges(*_load(ours_path)[:2])

    def resolved_calls(edges):
        return {_edge_key(e) for e in edges if e["rel"] == "calls" and e["resolved"]}

    oc, rc = resolved_calls(o), resolved_calls(r)
    matched = oc & rc
    ours_only = sorted(rc - oc)

    # An ours-only edge (s,rel,d) is "reciprocal-explained" if the oracle has the
    # reverse (d,rel,s) — same relationship, opposite direction — regardless of
    # confidence. Those are not counted as spurious.
    oracle_dir = {(s, rel, d) for (s, rel, d, _c) in oc}
    unexplained = [e for e in ours_only if (e[2], e[1], e[0]) not in oracle_dir]

    return Resolution(
        o_calls=len(oc),
        r_calls=len(rc),
        matched=len(matched),
        recall=len(matched) / len(oc) if oc else 1.0,
        precision=len(matched) / len(rc) if rc else 1.0,
        oracle_only=sorted(oc - rc),
        ours_only=ours_only,
        reciprocal_extra=len(ours_only) - len(unexplained),
        ours_only_unexplained=unexplained,
        o_unresolved=sum(1 for e in o if e["rel"] == "calls" and not e["resolved"]),
        r_unresolved=sum(1 for e in r if e["rel"] == "calls" and not e["resolved"]),
        o_references=sum(1 for e in o if e["rel"] == "references"),
        o_imports=sum(1 for e in o if e["rel"] == "imports"),
        r_imports=sum(1 for e in r if e["rel"] == "imports"),
    )


# ---- clustering -------------------------------------------------------------

def _communities(path: str) -> dict[str, int]:
    _, _, nodes = _load(path)
    return {_key_of(n): n["community"] for n in nodes if n.get("community") is not None}


def _file_keys(path: str) -> set[str]:
    """Keys of `file`-kind nodes (structural hubs). Only our export carries
    `kind`; the oracle's does not, so we identify hubs from our graph."""
    _, _, nodes = _load(path)
    return {_key_of(n) for n in nodes if n.get("kind") == "file"}


def _entropy(counts, n: int) -> float:
    h = 0.0
    for c in counts:
        if c > 0:
            p = c / n
            h -= p * math.log(p)
    return h


def nmi(a, b) -> float:
    """Normalized mutual information (arithmetic-mean normalization), in [0,1]."""
    n = len(a)
    if n == 0:
        return 1.0
    ca, cb = Counter(a), Counter(b)
    joint = Counter(zip(a, b))
    mi = sum((nxy / n) * math.log((nxy * n) / (ca[x] * cb[y])) for (x, y), nxy in joint.items())
    hx, hy = _entropy(ca.values(), n), _entropy(cb.values(), n)
    if hx == 0 and hy == 0:
        return 1.0
    denom = (hx + hy) / 2
    return mi / denom if denom > 0 else 0.0


def ari(a, b) -> float:
    """Adjusted Rand index (chance-corrected), ≤ 1."""
    n = len(a)
    if n < 2:
        return 1.0
    c2 = lambda k: k * (k - 1) // 2  # noqa: E731
    joint = Counter(zip(a, b))
    ca, cb = Counter(a), Counter(b)
    sum_ij = sum(c2(v) for v in joint.values())
    sum_a = sum(c2(v) for v in ca.values())
    sum_b = sum(c2(v) for v in cb.values())
    total = c2(n)
    expected = sum_a * sum_b / total if total else 0.0
    max_index = (sum_a + sum_b) / 2
    if max_index == expected:
        return 1.0
    return (sum_ij - expected) / (max_index - expected)


@dataclass
class Clustering:
    shared: int
    oracle_communities: int
    ours_communities: int
    nmi: float           # over all shared nodes
    ari: float
    core_shared: int     # shared nodes excluding structural `file` hubs
    nmi_core: float      # over core nodes — the semantic partition agreement
    ari_core: float
    per_node: list = field(default_factory=list)  # (key, oracle_comm, ours_comm)


def clustering_metrics(oracle_path: str, ours_path: str) -> Clustering:
    oc, rc = _communities(oracle_path), _communities(ours_path)
    shared = sorted(set(oc) & set(rc))
    a = [oc[k] for k in shared]
    b = [rc[k] for k in shared]

    # `file` nodes are structural hubs — a file whose functions split across
    # communities is a symmetric tie, so its assignment is arbitrary and differs
    # harmlessly between the two clusterers. The *semantic* partition (functions,
    # types) is the real fidelity signal, so also report NMI/ARI excluding hubs.
    hubs = _file_keys(ours_path)
    core = [k for k in shared if k not in hubs]
    a_core = [oc[k] for k in core]
    b_core = [rc[k] for k in core]

    return Clustering(
        shared=len(shared),
        oracle_communities=len(set(oc.values())),
        ours_communities=len(set(rc.values())),
        nmi=nmi(a, b),
        ari=ari(a, b),
        core_shared=len(core),
        nmi_core=nmi(a_core, b_core),
        ari_core=ari(a_core, b_core),
        per_node=[(k, oc[k], rc[k]) for k in shared],
    )
