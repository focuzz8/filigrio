# filigrio

A knowledge-graph index of your code, queried by AI agents over **MCP**. A
resident daemon holds the graph, a `filigrio-mcp` bridge exposes it to your agent
as nine tools, and a filesystem watcher plus git hooks keep it current as you
work. Extraction is deterministic AST parsing — tree-sitter for Rust, Python and
TypeScript/JavaScript — so an edge in the graph is a reference that exists in the
source, not a guess: a call the extractor cannot bind stays *honestly unresolved*
rather than being pointed at a plausible homonym
([ADR-0023](docs/adr/0023-opaque-method-calls-prefer-unresolved.md)).

This repository is the Rust port of the Python
[graphify](docs/graphify-description.md); it is an engine with a thin product
around it. Read [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for how it is
built and the ADRs under [`docs/adr/`](docs/adr/) for why. This file is how to
run it.

## Install

There is no release tarball yet — build it:

```sh
cd src
cargo build --release -p filigrio-client-cli -p filigrio-client-mcp -p filigrio-daemon
export PATH="$PWD/target/release:$PATH"
```

That produces three binaries — `filigrio` (the CLI), `filigrio-mcp` (the stdio
bridge your agent spawns) and `filigrio-daemon` (the only supported process that
links the engine). **Keep them in one directory.** The CLI and the bridge both
auto-start `filigrio-daemon` from their *own* directory when nothing is
listening, and the installer writes registrations pointing at the `filigrio-mcp`
beside the `filigrio` it runs as — so a `filigrio` on your `PATH` without its
siblings starts nothing, or registers a binary that is not there.

## Run it by hand

Three processes, all started by you, and no installer: the **daemon** holds the
graph, the **CLI** registers and indexes repositories, and the **bridge** is the
MCP server your agent launches. The installer ([below](#the-installer-optional))
only automates step 4; it is still maturing, and nothing here depends on it.

### 1. Start the daemon

```sh
filigrio-daemon start --idle-timeout 0   # foreground: logs in this terminal, Ctrl-C stops it
```

It listens on `$XDG_RUNTIME_DIR/filigrio-daemon.sock` (`/tmp/filigrio-daemon.sock`
when `XDG_RUNTIME_DIR` is unset); `--socket PATH` puts it elsewhere — keep the
path under 108 bytes, the Unix socket limit. `filigrio daemon status` from another
terminal confirms it, and `filigrio daemon stop` (or Ctrl-C) shuts it down
cleanly. Two alternatives: `filigrio daemon start` launches the same daemon in
the background with a 300-second idle timeout, and any client — the CLI or the
bridge — auto-starts one, detached with no idle timeout, if nothing is listening.

### 2. Register and index a repository

```sh
cd /path/to/your/repo
filigrio project register        # tell the daemon this repo exists
filigrio project index           # build the graph (runs to completion)
filigrio project watch on        # optional: follow file edits live
```

The graph lands in `<repo>/.filigrio-out/` — **not gitignored for you**, add it —
and the registration in `$XDG_CONFIG_HOME/filigrio/registry.json`. Check it from
the CLI before involving an agent:

```sh
filigrio graph stats             # nodes, edges, communities, confidence mix
filigrio graph god --top 10      # the highest-degree nodes
filigrio graph query auth        # fuzzy seed search + a packed subgraph
filigrio graph report --out -    # the full human report, to stdout
```

Registering and indexing are CLI-only on purpose: the bridge has **no command
that writes** ([ADR-0042 F9](docs/adr/0042-module-sharded-storage.md)), so an
agent can read any registered graph but never register, index or change one.

### 3. The MCP bridge

`filigrio-mcp` is a **stdio** MCP server: JSON-RPC on stdin/stdout, logs on
stderr (filter with `FILIGRIO_LOG`, or `--verbose`). Its only other flag is
`--socket PATH`. It holds no graph — each tool call is a request to the daemon
([ADR-0032f](docs/adr/0032f-client-server-topology.md)) — and it exposes nine
read-only tools: `query_graph`, `get_node`, `get_neighbors`, `god_nodes`,
`list_communities`, `get_community`, `shortest_path`, `graph_stats`,
`project_graph`. It has no `--help`; started bare it waits on stdin, so this is
the whole smoke test:

```sh
cd /path/to/your/repo
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"manual","version":"0"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"graph_stats","arguments":{}}}' \
  | filigrio-mcp 2>/dev/null
```

Two things decide what it answers:

- **Which daemon** — the `--socket` it was given, else the default above,
  computed from *its own* environment. An agent launched from a desktop session
  may not have your shell's `XDG_RUNTIME_DIR`; a bridge that computes a different
  socket finds nothing there and auto-starts a second daemon. **Pass `--socket`
  explicitly in the registration** and there is only ever one.
- **Which project** — the registered project containing the bridge's **working
  directory**, which is whatever directory your agent launches it from. Most
  agents launch MCP servers from the directory you opened, which is what you
  want. If yours does not, every call fails with `project not registered: '/'`
  (or your home directory) plus the list of registered projects — wrap the
  command in a `cd` (below). A tool call can also name a registered project in
  its `project` argument, which the tool schema offers the model.

### 4. Register it with your agent

Use **absolute paths**: agents do not reliably inherit your shell's `PATH`.
Claude Code reads `<repo>/.mcp.json`:

```json
{
  "mcpServers": {
    "filigrio": {
      "type": "stdio",
      "command": "/abs/path/to/filigrio-mcp",
      "args": ["--socket", "/run/user/1000/filigrio-daemon.sock"]
    }
  }
}
```

Cursor (`<repo>/.cursor/mcp.json`) and Windsurf (`<repo>/.devin/mcp_config.json`)
take the same entry without `"type"`; Codex reads `~/.codex/config.toml`:

```toml
[mcp_servers.filigrio]
command = "/abs/path/to/filigrio-mcp"
args = ["--socket", "/run/user/1000/filigrio-daemon.sock"]
```

`/run/user/1000` in these examples is one user's `$XDG_RUNTIME_DIR` — use yours
(`echo $XDG_RUNTIME_DIR`), or whatever `--socket` you gave the daemon. Any other
MCP client needs the same two facts: a stdio server, that command, those args. For an agent that does not launch servers in the repository, pin the
directory instead:

```json
"command": "sh",
"args": ["-c", "cd /path/to/your/repo && exec /abs/path/to/filigrio-mcp --socket /run/user/1000/filigrio-daemon.sock"]
```

The tools describe themselves, but a small model does markedly better with the
usage guide — how to address a node, what an unresolved edge means, the answer
format ([ADR-0027](docs/adr/0027-node-addressability-at-mcp-boundary.md),
[ADR-0030](docs/adr/0030-structured-answer-contract.md)). `filigrio docs install`
writes it as a managed block in the repository's `AGENTS.md`, which
`filigrio docs uninstall` removes byte-exactly.

## The installer (optional)

Everything in [Run it by hand](#run-it-by-hand) step 4, automated — and, like
step 4, **optional**:

```sh
cd /path/to/your/repo
filigrio agent install --agent claude-code       # wire the agents you use
filigrio docs install                            # …and document them in AGENTS.md
```

Besides `.filigrio-out/`, the installer puts the per-agent files below into your
repository.

### Wiring your agent

Four families of artifact, and **four commands**, because they answer four
different questions ([ADR-0034 §17](docs/adr/0034-client-integration-installer.md),
[§18.2](docs/adr/0034-client-integration-installer.md)):

```sh
filigrio agent       install|uninstall|status   # one agent's MCP registration, and its skill
filigrio docs        install|uninstall|status   # this repository's AGENTS.md
filigrio hooks       install|uninstall|status   # the git hooks (see "Keeping it fresh")
filigrio completions install|uninstall|status   # bash/zsh/fish
```

No command spans families. There is no `--all`. Every artifact is reversible, and
every one is idempotent: a second run reports `=` and does not touch an mtime.

Seven `--agent` slugs exist, and **each one writes into its own namespace and
nobody else's**. What that is varies by agent because vendors differ, never by
what you asked for:

| `--agent` | Registration | Default scope | Its own manual |
|---|---|---|---|
| `claude-code` | `<repo>/.mcp.json` | **project** | `<repo>/.claude/skills/filigrio/SKILL.md` |
| `opencode` | `<repo>/opencode.json` | **project** (`--global`: `~/.config/opencode/opencode.json`) | `<repo>/.opencode/skills/filigrio/SKILL.md` — **always in the repo**, see below |
| `cursor` | `<repo>/.cursor/mcp.json` | **project** (`--global`: `~/.cursor/mcp.json`) | — reads `AGENTS.md` |
| `windsurf` | `<repo>/.devin/mcp_config.json` | **project** (`--global`: `~/.config/devin/mcp_config.json`) | — reads `AGENTS.md` |
| `codex` | `~/.codex/config.toml` | **global only** | — reads `AGENTS.md` |
| `openclaw` | `~/.openclaw/openclaw.json` | **global only** | — reads `AGENTS.md` |
| `hermes` | `~/.hermes/config.yaml` | **global only** | — reads `AGENTS.md` |

The two `SKILL.md` files are **one template rendered twice**, differing in a
single line: the config each tells its reader to check when the tools are
missing. The five that read `AGENTS.md` get it from `filigrio docs install`, not
from another agent's adapter — `AGENTS.md` is the *repository's* documentation
and has a command of its own.

**A skill never follows `--global`.** `--agent opencode --global` puts the
registration in `~/.config/opencode/opencode.json` and leaves the skill at
`<repo>/.opencode/skills/filigrio/SKILL.md`. The two are different kinds of
claim: a registration says *this server exists and here is how to reach it*,
which is true on the machine wherever you stand, while the skill opens by saying
*"Answer questions about this codebase by querying its knowledge graph"* — true
of an indexed repository and false of every other project on the machine.
OpenCode is the only agent today with both a global scope and a skill, so it is
the only place the difference is visible; the rule lives above the adapters so
that the next one inherits it.

**`--global` is a consent gate, not a scope preference.** It means *I agree to
writes outside this repository*, and its corollary is the one sentence worth
memorising: **`filigrio agent install` without `--global` cannot write outside the
repository.** So the three global-only agents are refused without it — by name,
with the reason, and never silently skipped:

```sh
filigrio agent install --agent claude-code,cursor,opencode,windsurf  # this repo
filigrio agent install --agent codex,openclaw,hermes --global        # under $HOME
filigrio agent install --agent hermes          # error: no project-scoped config
filigrio agent install --agent claude-code --global   # error: ~/.claude.json is live session state
```

Two more things in that table surprise people. Both are also reported by the tool
itself, on `install` and on `status`.

**Global scope does not travel with the repository.** Codex, OpenClaw and Hermes
document no project-scoped MCP config, so their registration is machine-local: a
teammate who clones the repo gets the capability doc but no server, and installs
the registration themselves. (Codex *has* a project file, `.codex/config.toml`,
but Codex reads it only once you have trusted the project — a registration
sitting on disk being ignored is worse than one you know is machine-local, so it
goes to the user scope.) Each adapter's module header records its own path,
format and reasoning:
[`hermes.rs`](src/crates/filigrio-install/src/clients/hermes.rs),
[`codex.rs`](src/crates/filigrio-install/src/clients/codex.rs),
[`openclaw.rs`](src/crates/filigrio-install/src/clients/openclaw.rs),
[`windsurf.rs`](src/crates/filigrio-install/src/clients/windsurf.rs).

**Windsurf ships two agents on two config layouts, and we write one of them.**
`.devin/mcp_config.json` and `~/.config/devin/mcp_config.json` are read by the
Devin Local agent, which `docs.devin.ai` calls the default agent for new tabs.
The older `~/.codeium/windsurf/mcp_config.json` is documented as applying "to the
legacy Cascade agent only", so filigrio does not write it and says so on every
install and status. That file *was* the only path this project wrote until the
2026-08-08 vendor sweep, alongside a claim that Windsurf documented no
project-scoped file — the vendor documents `.devin/mcp_config.json` as committed
to version control, and always did. See
[`docs/vendor-path-verification.md`](docs/vendor-path-verification.md).

**Wiring an agent is not the same decision as documenting the repository.** Five
of the seven get a registration and no manual of their own. For OpenClaw and
Hermes that is an absence — every skill root they document is under `$HOME`. For
Cursor, Windsurf and Codex it is a judgement: `.cursor/rules/*.mdc`,
`.devin/skills/` and `.agents/skills/` all exist, and writing a second copy of a
doc this build already ships in a single-vendor format is the duplication
ADR-0034 §4 exists to prevent. Either way they read `AGENTS.md`, and that file
belongs to the repository rather than to any of them. So this:

```sh
filigrio agent install --agent hermes --global   # registration only, no AGENTS.md
```

gives Hermes the tools but not the doc that tells it how to address a node, what
an unresolved edge means, and not to invent an edge — which is what makes the
tools legible to a small model at all
([ADR-0027](docs/adr/0027-node-addressability-at-mcp-boundary.md),
[ADR-0030](docs/adr/0030-structured-answer-contract.md)). The doc is one more
command, and `agent install` says so on every run:

```sh
filigrio docs install                            # AGENTS.md, in this repo
filigrio agent install --agent hermes --global   # the registration, under $HOME
```

`filigrio docs` takes **no `--global`** and no member selector: `agents.md` puts
the file at the repository root by definition, so there is one destination and
one artifact, and `docs install` just installs.

The other selectors:

```sh
filigrio agent install --agent cursor,opencode       # comma list, or repeat --agent
filigrio agent install --project /path/to/repo …     # wire a repo you are not standing in
filigrio agent install --explain …                   # every note in full, unaggregated
```

**A bare `filigrio agent install` installs nothing.** It prints the roster — every
agent, its scope, whether it was detected, whether it is already installed — and
exits non-zero, because nothing about "no arguments" says *all of them* and the
previous reading of it put sixteen artifacts on disk (ADR-0034 §17.2). **There is
no `--agent all`** either: an agent that gets installed is an agent you named.
An unknown slug is a hard error listing the valid ones, never a silent no-op —
and `agents-md`, which used to be an eighth slug, is now one of those unknown
names. `AGENTS.md` is `filigrio docs`.

**`uninstall` and `status` are the deliberate exceptions.** Given no `--agent`
they cover every agent: a sweep that removes can only remove what filigrio wrote
— a marked block, a named key, never a line of yours — and a sweep that reports
writes nothing at all. A sweep that *creates* is the one you cannot undo by not
having asked for it. `--global` still gates reach on removal, so a bare
`filigrio agent uninstall` cleans the repository and then tells you how many
registrations are left under `$HOME` and which flag removes them.

### What your agent gets

The same bridge and nine tools as a hand-written registration — see
[step 3](#3-the-mcp-bridge) for which daemon and which project it answers for.

The socket is baked in at install time — into all seven MCP registrations *and*
into the four generated hook scripts — so re-run `agent install` and `hooks
install` if you change `--socket`. You do not have to remember to: the `status`
verbs report every artifact that names the old one as
`stale — re-run install to refresh`.

## Keeping it fresh

Three mechanisms, in increasing laziness:

```sh
filigrio project index           # explicit: reconcile now, run to completion
filigrio project watch on        # converge, then follow file changes live
filigrio project watch off
```

`watch on` is a synchronous deep reconcile followed by a filesystem watcher
([ADR-0032a](docs/adr/0032a-filesystem-watcher.md)), so it sees **uncommitted**
edits. The git hooks cover the commit boundary instead — `post-commit`,
`post-checkout`, `post-merge`, `post-rewrite`
([ADR-0032b](docs/adr/0032b-git-hooks.md)) — computing the diff client-side and
submitting one high-priority changeset. They chain rather than clobber: an
existing `post-commit` (husky, `pre-commit`, hand-written) gets a
marker-delimited block appended. They never block and never fail a commit, they
always exit 0, and `FILIGRIO_SKIP_HOOK=1` opts out. If the daemon is down they
spool to `$XDG_CACHE_HOME/filigrio/spool` and the next hook run replays.

Under `watch`, a save is queryable a few seconds later — about 3.5 s on next.js
(125k nodes), mostly the per-apply cost of relinking a graph that size. Two things
trail it on purpose: **communities** are recomputed once your edits pause (a new
symbol has none for about half a second), and **disk**: applies are write-behind,
so the daemon serves the newest graph but `.filigrio-out/` can lag. Two verbs close
that gap:

```sh
filigrio project flush           # persist resident state to .filigrio-out now
filigrio project export          # write the graph.json interchange snapshot
```

### The monorepo limitation

**A hook submits the worktree root.** The daemon resolves an exact project id or
the nearest *ancestor* root. So a monorepo whose sub-projects you registered
individually — `repo/packages/web`, `repo/packages/api` — gets **no** hook-driven
freshness: `repo` is what the hook sends, and a project registered below it is a
descendant, not an ancestor, so it can never receive the changeset.
Per-subproject hook routing is not built.

Register the repository root. The root index already models sub-projects
internally — `filigrio graph project-graph` reports them and their `depends_on`
edges ([ADR-0019](docs/adr/0019-workspace-model-incremental-state.md)) — so
registering one level up loses you nothing. `filigrio hooks status` tells you
which case you are in.

## Uninstall and status

```sh
filigrio agent status                        # every agent, project scope
filigrio agent status --global               # …and the ones that live under $HOME
filigrio docs status                         # the AGENTS.md block
filigrio hooks status
filigrio completions status

filigrio agent uninstall                     # every agent's artifacts in this repo
filigrio agent uninstall --global            # …and the ones under $HOME
filigrio docs uninstall                      # the AGENTS.md block, leaving your prose
filigrio hooks uninstall
filigrio completions uninstall
```

`status` prints one line per artifact with a verdict — `=` current, `~` stale,
`.` absent, `!` failed — and `filigrio hooks status` then answers a question
`install` cannot: **which project id will my hooks submit under, and is
anything registered to receive it?** ([ADR-0032b
OQ4](docs/adr/0032b-git-hooks.md), commit `db3dca5`.) It resolves the target the
way a hook does, asks the daemon, and reports:

```
git hooks submit under: /path/to/repo
  daemon:   running (/run/user/1000/filigrio-daemon.sock)
  receiver: project `repo` — 4 file(s), 9 node(s)
  spool:    empty
```

`receiver: NONE` is the failure this exists to surface, and it prints the fix.
The probe starts nothing and writes nothing.

`uninstall` is byte-exact. Shared files are edited through marker-delimited
managed blocks or a single named key, never rewritten: `AGENTS.md`, a
pre-existing `post-commit`, `~/.hermes/config.yaml` and `~/.codex/config.toml`
all come back with the same SHA-256 they went in with — comments and final
newline included. A file that held only our entry is deleted; a file at one of
our destinations that we did not write is reported and left alone. Nothing
touches `.filigrio-out/`, so your index survives an uninstall.

## Troubleshooting

**Run `filigrio agent status` and `filigrio hooks status` first.** Most of what
follows is a line in their output.

**The agent's tools return nothing, or the graph never updates after a commit.**
The project is probably not registered. Hook delivery is fire-and-forget by
design — a hook that can stall a `git commit` is worse than no hook — so an
unregistered repository has every commit submitted, every submission declined,
and nothing said about it. The hook prints `submitted to the daemon` and is
telling the truth; the daemon is the half that declines. `filigrio hooks status`
reports `receiver: NONE`; `filigrio project register` from the repository root is
the fix, and `filigrio project index` catches up the drift.

**Behaviour that does not match the binary you just built.** `cargo build`
refreshes binaries; it cannot refresh a process that started hours ago. The
resident daemon keeps serving the code it was launched with — and, the trap,
`filigrio daemon start` against a live socket logs `Daemon is already running`,
**exits 0, and replaces nothing**, so a rebuild-and-restart cycle that looks
successful can leave the old process in place. This has cost this project real
measurements: two full agent-eval runs were served by a nine-hour-old daemon that
had never learned the query values the bridge was advertising, and the result
read as "the graph holds no call edges"
([ADR-0025](docs/adr/0025-mcp-conformance-and-agent-eval-harness.md), commit
`a5f354b`). Stop it explicitly, then start:

```sh
filigrio daemon stop && filigrio daemon start
filigrio daemon status           # uptime is the number to read
```

**A graph that differs from a cold build.** The daemon re-resolves only the
references a change can affect (scoped linking, proven equal to a full relink by
`just shadow` and `just convergence`). If you suspect it, start the daemon with
`FILIGRIO_LINK_SCOPE=global` — every apply then re-links the whole graph, slower
but the reference behaviour — and compare. The daemon logs which it is using at
startup (`Link scope: …`).

**Nothing is listening.** `filigrio daemon status` says `State: stopped`. Any
client command auto-starts one; `--verbose` puts the client's own log lines on
stderr if the handshake is what is failing.

**An `AGENTS.md` or hook edit vanished.** Content *inside* the managed markers is
overwritten by design — the block is generated. Put your own prose outside it.

**zsh completions do nothing.** They need their directory on `fpath`; `install`
prints the exact line to add before `compinit`.

**A config we would not touch.** A JSONC `opencode.json` (comments are legal
there), an unparseable TOML, or `mcp_servers: {}` written as an inline YAML
mapping are each *refused with the reason* rather than reformatted, and the run
continues so one bad target does not hide the rest. Add the entry by hand, or
convert the mapping.

## Validation

What has been measured, and how to reproduce it. Performance rows live in
[`docs/perf/benchmarks.md`](docs/perf/benchmarks.md) — the only place this
repository makes performance claims, each stamped with its commit, machine
state and exact commands. Numbers below are from a single developer machine;
treat them as orders of magnitude, not a spec.

### Correctness

| Check | Result | Reproduce |
|---|---|---|
| Default suite — fmt, clippy `-D warnings`, every non-heavy test | 1236 passed, 0 failed | `just gate` |
| Scoped ≡ Global linking, sampled edits | next.js (22,089 files) and this repo (207): 16 edits each, zero divergence | `just shadow next.js` · `just shadow self` |
| Real-history convergence | 40 next.js commits (250 added / 300 modified / 16 removed files) and 30 of this repo's: Global ≡ Scoped at every step, both chains ≡ a cold build | `just convergence next.js 40` · `just convergence self 30` |
| Differential check vs the Python graphify oracle | 96 passed, 14 skipped (clustering checks on fixtures too small to measure) | `uv run pytest` |

### Performance against the original

From [`benchmarks.md` §9](docs/perf/benchmarks.md) — there is **no single
"N× faster" number**, because the original's cost grows with the corpus and ours
does not:

| Corpus | Python graphify (oracle) | filigrio | Ratio |
|---|---|---|---|
| ironclaw (~3k files, Rust-dense) | 166 s | ~41 s | ≈4× |
| next.js (~24k files) | ≈4 hours, single-threaded | ~31 s tree-sitter · ~15 s oxc | ≈460× · ≈940× |

Peak memory depends on the corpus the same way: the oracle is lighter on the
small repo and heavier on the large one.

| Corpus | Python graphify (oracle) | filigrio |
|---|---|---|
| ironclaw | 768 MB | ~2.2 GB — oracle ≈2.9× lighter |
| next.js | 5.1 GB | ~2.0 GB tree-sitter · ~1.9 GB oxc — filigrio ≈2.6× lighter |

One caveat the same section records, and a fair comparison repeats: the two
engines do not emit identical artifacts (different file sets, node models and
edge semantics — per-language node counts are close). Commands: §9.6.

### Freshness on a live daemon

next.js @ `163e45e401`, release build, the daemon started by hand
([Run it by hand](#run-it-by-hand)) with `project watch on`:

| | |
|---|---|
| Cold `project index` | 33 s for 23,345 files |
| Save → queryable, ordinary edit (a function body, a non-exported helper) | ~3.3–3.9 s |
| Save → queryable, export added or removed | ~2.5 s more — an export change re-resolves every reference, by design |
| Deleting a function | lands like any edit |
| Communities after a burst of saves | recomputed once, ~0.5 s after the burst |

To reproduce: index a large checkout, `project watch on`, edit a file, and poll
`filigrio graph get-node 'fn:<file>:<name>'` until the new symbol answers.

### Agent evaluation

[`tools/agent_eval`](tools/agent_eval/README.md) points a local model at the MCP
tools and grounds every citation and edge in the answer against the real graph.
With gpt-oss-20b on llama.cpp: 8 of 17 tasks pass. In the failures examined, the
graph held the answer and the model did not use or report it (one task's rubric
is known-wrong and flagged in `tasks.json`). The task set asks
about one specific private codebase, so on another repository write your own
`tasks.json`: `deno task test` (deterministic, no model) · `deno task eval --
--repo /path/to/checkout`.

### Corpora

The `just` recipes take a corpus **name** — a sibling checkout next to this
repository (`../next.js`, `../ironclaw`, `../langchain`, `../moon`) — or an
**absolute path** to any git checkout (set `FILIGRIO_LEDGER_EXTS`, e.g. `ts,tsx`,
for a non-Rust one). Nothing is hardcoded to a machine:

```sh
git clone https://github.com/vercel/next.js ../next.js
git -C ../next.js checkout 163e45e401    # the commit the rows above were measured at
just convergence next.js 40
just convergence /abs/path/to/repo 30
```

`just --list` shows every recipe.

## Repository layout

| Path | What | Toolchain |
|---|---|---|
| [`src/`](src/) | the Rust port — a 14-crate Cargo workspace | cargo |
| [`docs/`](docs/) | [architecture map](docs/ARCHITECTURE.md), [ADRs](docs/adr/), [HLD](docs/hld.md), [glossary](docs/glossary.md), [perf ledger](docs/perf/benchmarks.md) | — |
| [`tools/`](tools/) | Python dev tooling (the oracle diff) | uv |

### Developing

There is no CI; the [`justfile`](justfile) at the repository root is the gate
runner, and `just gate` (fmt-check + clippy `-D warnings` + the workspace suite)
is the pre-commit action. `just gate-all` adds the non-default feature configs;
the heavy corpus suites are opt-in (`just convergence next.js`, `just shadow`).
`just --list` documents every recipe — see
[`docs/ARCHITECTURE.md` § Gates](docs/ARCHITECTURE.md#gates).

Per-commit performance **and** correctness rows live in
[`docs/perf/benchmarks.md`](docs/perf/benchmarks.md), which is the only place
this repository makes performance claims. Every row is stamped with the commit it
was measured on.

The repository root is also a [uv](https://docs.astral.sh/uv/) project, owning
the cross-checks against the Python graphify **oracle**
([ADR-0017](docs/adr/0017-graphjson-interchange-not-storage.md)). The diff builds
a shared Rust fixture with both implementations and compares resolved `calls`
edges (endpoints + confidence), the community partition (NMI / ARI) and build
time; [`tools/oracle_diff/README.md`](tools/oracle_diff/README.md) documents the
metrics and the accepted divergences.

```sh
uv sync                              # create .venv from the lockfile
uv run pytest                        # the oracle-diff suite (skips if oracle/cargo absent)
uv run python -m tools.oracle_diff   # one human-readable report
```

`filigrio-classic` is a fourth binary — the pre-daemon, engine-linked one-shot
(`graph build/import/query/god/report/path`), kept for the direct build path and
the only binary carrying the `ts-oxc` feature. See
[`src/README.md`](src/README.md) for it, for what is real versus
mocked per crate, and for the deliberate simplifications.
