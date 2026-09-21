# filigrio gates. `just` with no arguments lists every recipe.
#
# Two groups, and the split is the point:
#
#   fast   `just gate`   — run before EVERY commit. fmt + clippy + the whole
#                          default test suite. Tens of seconds, no external state.
#   heavy  `just heavy`  — opt-in. The `#[ignore]`d suites that need an external
#                          corpus checkout and minutes-to-tens-of-minutes. Never
#                          wired into `gate`: a pre-commit gate that takes 20
#                          minutes gets bypassed, which is worse than no gate.
#
# Corpora are sibling checkouts of this repo (`../next.js`, `../ironclaw`,
# `../langchain`, `../moon`); `self` is this repo. Every heavy recipe takes the
# corpus name as its first argument — the harnesses are env-driven
# (FILIGRIO_GIT_REPO / FILIGRIO_GIT_BASE / FILIGRIO_GIT_DEPTH / FILIGRIO_GIT_HEAD /
# FILIGRIO_LEDGER_ROOT / FILIGRIO_LEDGER_EXTS), so one recipe covers all corpora.
# Known-good invocations: docs/perf/benchmarks.md §5b / §5d / §5i.
#
# Doc convention: the LAST comment line above a recipe is what `just --list`
# prints, so the one-line summary goes last and the detail above it.

set shell := ["bash", "-euo", "pipefail", "-c"]

ws := justfile_directory() / "src"
corpora := parent_directory(justfile_directory())

# List every recipe (the default).
default:
    @just --list --unsorted

# ---------------------------------------------------------------------------
# fast — the pre-commit gate
# ---------------------------------------------------------------------------

# The pre-commit gate: fmt-check + clippy + the full default test suite.
gate: fmt-check clippy test

# Rewrite the workspace with rustfmt (the fixer for `just fmt-check`).
fmt:
    cd {{ ws }} && cargo fmt --all

# Fail on any unformatted hunk — keeps later diffs reviewable (audit §M2).
fmt-check:
    cd {{ ws }} && cargo fmt --all -- --check

# Clippy over every target, warnings are errors — ADR-0041's gate.
clippy:
    cd {{ ws }} && cargo clippy --workspace --all-targets -- -D warnings

# The default workspace suite (the `#[ignore]`d heavy suites are excluded).
test:
    cd {{ ws }} && cargo test --workspace

# ---------------------------------------------------------------------------
# feature configs — a feature that only ever builds one way breaks silently
# ---------------------------------------------------------------------------

# Every non-default feature config: ts-oxc, control-off, oxc_resolver-off.
gate-features: features-ts-oxc features-no-control features-no-oxc-resolver

# `gate` + the feature matrix. Run before finishing a phase or handing work over.
gate-all: gate gate-features

# ADR-0040: the oxc TS/JS frontend (`filigrio-index/ts-oxc`), off by default.
features-ts-oxc:
    cd {{ ws }} && cargo clippy -p filigrio-index --features ts-oxc --all-targets -- -D warnings
    cd {{ ws }} && cargo test -p filigrio-index --features ts-oxc
    cd {{ ws }} && cargo build -p filigrio-classic --features ts-oxc

# The MCP bridge and the client library depend on filigrio-protocol with
# `default-features = false`, so the control plane (Command/ControlOp/MetaQuery)
# is structurally *absent* from their build. A `--workspace` build unifies the
# feature ON, so it proves nothing — these `-p` builds are the only place the
# control-off configuration is exercised at all.
#
# ADR-0042 F9: the MCP bridge + client library must build with `control` OFF.
features-no-control:
    cd {{ ws }} && cargo build -p filigrio-protocol --no-default-features
    cd {{ ws }} && cargo clippy -p filigrio-client-mcp -p filigrio-client-core --all-targets -- -D warnings
    cd {{ ws }} && cargo test -p filigrio-client-mcp
    cd {{ ws }} && cargo test -p filigrio-client-core

# filigrio-resolve without `oxc_resolver` — the hand-rolled source-only tier alone.
features-no-oxc-resolver:
    cd {{ ws }} && cargo clippy -p filigrio-resolve --no-default-features --all-targets -- -D warnings
    cd {{ ws }} && cargo test -p filigrio-resolve --no-default-features

# ---------------------------------------------------------------------------
# heavy — external corpora, minutes to tens of minutes, opt-in only
# ---------------------------------------------------------------------------

# Resolve a corpus name to its checkout + extensions. Internal.
[private]
_corpus name:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{ name }}" in
      self|.)    repo="{{ justfile_directory() }}"; exts=rs ;;
      next.js)   repo="{{ corpora }}/next.js";      exts=ts,tsx,js,jsx ;;
      ironclaw)  repo="{{ corpora }}/ironclaw";     exts=rs ;;
      langchain) repo="{{ corpora }}/langchain";    exts=py ;;
      moon)      repo="{{ corpora }}/moon";         exts=rs ;;
      /*)        repo="{{ name }}";                 exts="${FILIGRIO_LEDGER_EXTS:-rs}" ;;
      *)         echo "unknown corpus '{{ name }}' — known: self, next.js, ironclaw, langchain, moon, or an absolute path" >&2; exit 1 ;;
    esac
    if [[ ! -d "$repo/.git" ]]; then
      echo "corpus '{{ name }}' resolves to '$repo', which is not a git checkout" >&2
      exit 1
    fi
    echo "FILIGRIO_GIT_REPO=$repo"
    echo "FILIGRIO_LEDGER_ROOT=$repo"
    echo "FILIGRIO_LEDGER_EXTS=${FILIGRIO_LEDGER_EXTS:-$exts}"

# Print the environment a heavy recipe would use for a corpus (a dry run).
corpus name:
    @just _corpus {{ name }}

# ADR-0032 §7 + ADR-0042 P2. Replays real commit history with two independent
# chains (Global and Scoped) and asserts zero divergence; the same run prints
# the would-be dirty-shard set per commit, which is the Phase-2 measurement
# (ledger §5i). `base`/`head` pin an explicit range instead of `depth` back.
#
#   just convergence self
#   just convergence next.js 40
#   just convergence next.js 0 b7f84ed7c71d 1a7e6bda0f10   # §5b's high-churn window
#
# [heavy] Real-history convergence gate + the dirty-shard measurement.
convergence corpus="self" depth="30" base="" head="":
    #!/usr/bin/env bash
    set -euo pipefail
    eval "$(just _corpus {{ corpus }} | sed 's/^/export /')"
    export FILIGRIO_GIT_DEPTH={{ depth }}
    if [[ -n "{{ base }}" ]]; then export FILIGRIO_GIT_BASE={{ base }}; fi
    if [[ -n "{{ head }}" ]]; then export FILIGRIO_GIT_HEAD={{ head }}; fi
    echo "convergence: repo=$FILIGRIO_GIT_REPO exts=$FILIGRIO_LEDGER_EXTS depth={{ depth }} base='{{ base }}' head='{{ head }}'"
    cd {{ ws }}
    cargo test --release -p filigrio-resolve --test git_convergence -- --ignored --nocapture

# ADR-0042 Phase 1's big-repo equivalence gate: `sample` files are each edited in
# turn and Scoped must equal Global on every one.
#
# [heavy] Scoped ≡ Global on a big repo (`shadow_big_repo_equivalence`).
shadow corpus="next.js" sample="8":
    #!/usr/bin/env bash
    set -euo pipefail
    eval "$(just _corpus {{ corpus }} | sed 's/^/export /')"
    export FILIGRIO_SHADOW_SAMPLE={{ sample }}
    cd {{ ws }}
    cargo test --release -p filigrio-resolve --test shadow -- --ignored --nocapture shadow_big_repo_equivalence

# The ledger rows behind §5d/§5e. `store=1` also times the store write path.
#
#   just profile next.js 3 1
#
# [heavy] Per-stage apply profile on a real corpus.
profile corpus="next.js" n="3" store="1":
    #!/usr/bin/env bash
    set -euo pipefail
    eval "$(just _corpus {{ corpus }} | sed 's/^/export /')"
    export FILIGRIO_PROFILE_N={{ n }}
    if [[ "{{ store }}" == "1" ]]; then export FILIGRIO_PROFILE_STORE=1; fi
    cd {{ ws }}
    cargo test --release -p filigrio-resolve --test apply_profile -- --ignored --nocapture

# The synthetic bound, not a real-history number — see §5a's warning about
# quoting a single-fixture microbenchmark as a scope comparison.
#
# [heavy] Link-stage ledger, Global vs Scoped on a fixed corpus.
link-ledger corpus="self" n="5":
    #!/usr/bin/env bash
    set -euo pipefail
    eval "$(just _corpus {{ corpus }} | sed 's/^/export /')"
    export FILIGRIO_LEDGER_N={{ n }}
    cd {{ ws }}
    cargo test --release -p filigrio-resolve --test shadow -- --ignored --nocapture link_stage_ledger

# Builds its own fixture, so it takes no corpus.
#
# [heavy] ADR-0042 F4 write-behind ledger — what a watch-mode burst writes.
flush-ledger:
    cd {{ ws }} && cargo test --release -p filigrio-daemon --test flush_ledger -- --ignored --nocapture

# Builds its own synthetic fixture, so it takes no corpus. `n` is the node
# counts to measure (comma-separated); the 100k row is next.js scale and is the
# one that costs minutes on a pre-fix tree.
#
#   just read-ledger
#   just read-ledger 2000,20000,100000
#
# [heavy] The read-path ledger (§6): per-tool-call latency, components, view memory.
read-ledger n="2000,20000":
    #!/usr/bin/env bash
    set -euo pipefail
    export FILIGRIO_READ_N={{ n }}
    cd {{ ws }}
    cargo test --release -p filigrio-daemon --test read_ledger  -- --ignored --nocapture
    cargo test --release -p filigrio-query  --test read_profile -- --ignored --nocapture
    cargo test --release -p filigrio-query  --test view_memory  -- --ignored --nocapture

# [heavy] The whole heavy group for one corpus. Tens of minutes on next.js.
heavy corpus="next.js": (convergence corpus) (shadow corpus) (profile corpus)
