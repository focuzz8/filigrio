# agent_eval — MCP tool-ergonomics harness

A Deno + Vercel AI SDK 7 harness that points a **small local model** (gpt-oss-20b via a
llama.cpp OpenAI-compatible server, served under the alias `openai/qwen35`) at filigrio's **MCP tools** + plain filesystem tools
(`read_file`, `ls`, `grep`), logs every tool call the model makes, and scores the run.
The point isn't a benchmark — it's to *watch how a real agent uses the tools* and use
those logs to sharpen the tool surface and the agent prompt (`skill.md`). See **ADR-0025**.

The filesystem tools are there to remove a confound: with only the graph on the table,
"the model used the graph" was forced rather than chosen. `ls`/`grep` give it a real
alternative, so the traces answer the product question — does it *prefer* the graph, and
for which questions does it fall back to files? `skill.md` names them neutrally on
purpose; it must not push the choice either way.

## Layout

| File | Role |
|------|------|
| `src/logger.ts` | Transcript logger (pure, deterministic) → JSONL |
| `src/score.ts` | Rubric scorer `score(task, transcript)` (pure) |
| `src/mcp.ts` | Connect to the `filigrio-mcp` bridge binary via the AI SDK's generic MCP client |
| `src/model.ts` | Model provider (local llama.cpp or hosted) + health check |
| `src/read_file.ts` | Local, path-jailed file-read tool given to the agent |
| `src/fs_tools.ts` | `ls` + `grep`, same jail, output capped and truncation announced |
| `src/loop.ts` | The `generateText` loop; logs tool calls via `onStepFinish` |
| `main.ts` | Runner: build store, run tasks, score, write `runs/` |
| `skill.md` | The agent's system prompt (**iterated from logs**) |
| `tasks.json` | `{ tasks[] }` — the eval tasks with rubrics (an optional `repo`, relative to this file, names the checkout they ask about) |

## Two halves (deterministic vs live)

**Deterministic harness** — no model, must pass, safe in CI:

```
deno task test      # logger + score + read_file/ls/grep jails + MCP smoke test
deno task lint
```

The **MCP smoke test** (`src/mcp_test.ts`) is the key ergonomics gate: it drives the real
`filigrio-mcp` bridge through a *standard* MCP client and asserts all 9 tools are visible
**with their parameters** — the direct proof the server is MCP-conformant (ADR-0025). It
skips if the `filigrio` debug binary isn't built.

**Live eval** — non-deterministic, skips off-box:

```
deno task eval                 # build store (if needed) + run all tasks
deno task eval -- --rebuild    # force-rebuild the graph store first
deno task eval -- --task hub   # one task
deno task eval -- --repo /path/to/repo   # or REFLEX_REPO in .env
```

Each invocation writes a **timestamped run directory** `runs/<ISO-timestamp>/` holding
`<task>.jsonl` (every tool call + final answer + the grounding verdict) and
`summary.json`, so history is preserved across runs; `runs/latest` symlinks the newest.
Skips with a message when the llama server (`LLAMA_BASE_URL`, default
`http://127.0.0.1:45285/v1`) is unreachable.

Every task also passes through the **grounding gate**: the final answer's citations
(`name [file:loc]`) and edge claims (`A --relation--> B`) are validated against the real
`graph.json`. A citation to a non-existent symbol, or an edge the graph never contains,
is a fabrication that fails the run — "never invent an edge" as a deterministic check,
not a hope.

## Env

| Var | Default |
|-----|---------|
| `LLAMA_BASE_URL` | `http://127.0.0.1:45285/v1` |
| `LLAMA_MODEL` | `openai/qwen35` |
| `REFLEX_REPO` | the checkout the tasks ask about — **required** unless `--repo` or `tasks.json`'s `repo` gives it; no built-in default |
| `FILIGRIO_SOCKET` | `$XDG_RUNTIME_DIR/filigrio-daemon.sock` (else `/tmp/…`) — the daemon's own default |
| `MAX_STEPS` | `10` |
