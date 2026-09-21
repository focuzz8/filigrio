//! Agent-eval runner (ADR-0025). Points the model-under-test at the filigrio MCP
//! tools + local `read_file` / `ls` / `grep` tools, runs each task in `tasks.json`
//! against a built store, logs every tool call, scores the run, and writes a summary. This is the
//! *non-deterministic* half of the harness: it skips cleanly when the provider is
//! unreachable and never asserts model quality — it's a probe, read the logs.
//!
//! The model is env-driven (`src/model.ts`): local llama.cpp by default, or a hosted
//! provider via `AI_PROVIDER` + `PROVIDER_API_KEY` + `AI_MODEL_ID` (loaded from `.env`).
//!
//!   deno task eval                 # build store if needed, run all tasks
//!   deno task eval -- --rebuild    # force-rebuild the store first
//!   deno task eval -- --task hub   # run one task
//!   deno task eval -- --repo /path # the checkout the tasks ask about (or REFLEX_REPO)

import { resolve } from "node:path";
import { BASE_URL, IS_REMOTE, model as buildModel, MODEL_ID, providerUp } from "./src/model.ts";
import { connectFiligrio } from "./src/mcp.ts";
import { readFileTool } from "./src/read_file.ts";
import { grepTool, lsTool } from "./src/fs_tools.ts";
import { runTask } from "./src/loop.ts";
import { transcriptEvents } from "./src/logger.ts";
import { score } from "./src/score.ts";
import { checkGrounding, loadGraphFacts, parseSections } from "./src/grounding.ts";
import type { Task } from "./src/types.ts";

const HERE = import.meta.dirname!;
const REPO = resolve(HERE, "../..");
const BIN = resolve(REPO, "src/target/debug/filigrio-mcp");
const CLI_BIN = resolve(REPO, "src/target/debug/filigrio"); // Main CLI for project management
// Default daemon socket: the daemon's own rule (`default_socket_path`) —
// `$XDG_RUNTIME_DIR/filigrio-daemon.sock`, else `/tmp/filigrio-daemon.sock`.
// Overridable so a diagnostic run can use its own daemon instead of stopping the
// one you are already working against.
const SOCKET = Deno.env.get("FILIGRIO_SOCKET") ??
  resolve(Deno.env.get("XDG_RUNTIME_DIR") || "/tmp", "filigrio-daemon.sock");
const RUNS = resolve(HERE, "runs");
const MAX_STEPS = Number(Deno.env.get("MAX_STEPS") ?? "10");

function argValue(flag: string): string | undefined {
  const i = Deno.args.indexOf(flag);
  return i >= 0 ? Deno.args[i + 1] : undefined;
}

/**
 * The checkout the tasks ask about. `--repo` wins, then `REFLEX_REPO` (both
 * relative to the current directory), then a `repo` in `tasks.json` (relative to
 * that file, so it stays portable). There is deliberately no built-in default:
 * the task set is about one specific codebase, and a guessed path would score
 * a different repository without saying so.
 */
function resolveRepo(spec: unknown): string {
  const flag = argValue("--repo") ?? Deno.env.get("REFLEX_REPO");
  const fromSpec = Array.isArray(spec) ? undefined : (spec as { repo?: string }).repo;
  const repo = flag ? resolve(Deno.cwd(), flag) : fromSpec ? resolve(HERE, fromSpec) : undefined;
  if (!repo) {
    throw new Error(
      "no target repository: pass --repo <path>, set REFLEX_REPO (e.g. in .env), or add \"repo\" to tasks.json",
    );
  }
  let isDir = false;
  try {
    isDir = Deno.statSync(repo).isDirectory;
  } catch { /* reported below */ }
  if (!isDir) throw new Error(`target repository ${repo} is not a directory`);
  return repo;
}

async function sh(cmd: string, args: string[], cwd?: string): Promise<void> {
  const { success, code } = await new Deno.Command(cmd, { args, cwd, stdout: "inherit", stderr: "inherit" }).output();
  if (!success) throw new Error(`${cmd} ${args.join(" ")} exited ${code}`);
}

async function fileExists(p: string): Promise<boolean> {
  try {
    await Deno.stat(p);
    return true;
  } catch {
    return false;
  }
}

// Always rebuild, for the same reason as the CLI below. This function used to
// return early when the binary merely *existed*, and the 2026-07-29 run paid for
// it: the tool schema had just gained the ADR-0036 `type/…` relation enum, the
// on-disk bridge predated it, and the eval measured the old surface.
async function ensureMCPBinary(): Promise<void> {
  console.log("• building filigrio-mcp binary (cargo)…");
  await sh("cargo", ["build", "-q", "-p", "filigrio-client-mcp"], resolve(REPO, "src"));
}

// Always rebuild rather than trusting an existing binary: the CLI's verb surface
// changed repeatedly under ADR-0042 (F5 collapsed `build` into `index`, F6c
// deleted `--wait`, F7 renamed `--force` to `--clean`), and a stale binary on
// disk fails as a mystery — the harness would run against whatever CLI happened
// to be built last. Cargo no-ops when nothing changed, so this costs nothing.
// `filigrio daemon start` resolves the daemon as a *sibling of its own
// executable* (`current_exe().with_file_name("filigrio-daemon")`), so the two
// must be built together or the thin client drives a stale engine — which is
// where the extractors, the responder and the whole relation vocabulary live.
// Building only the CLI is how the 2026-07-29 run served a week-old graph.
async function ensureCLIBinary(): Promise<void> {
  console.log("• building filigrio CLI + daemon binaries (cargo)…");
  await sh("cargo", ["build", "-q", "-p", "filigrio-client-cli", "--bin", "filigrio"], resolve(REPO, "src"));
  await sh("cargo", ["build", "-q", "-p", "filigrio-daemon"], resolve(REPO, "src"));
}

/** Wait until `SOCKET` exists (`want=true`) or is gone (`want=false`). */
async function waitForSocket(want: boolean, tries = 40): Promise<boolean> {
  for (let i = 0; i < tries; i++) {
    if ((await fileExists(SOCKET)) === want) return true;
    await new Promise((r) => setTimeout(r, 200));
  }
  return false;
}

async function ensureStore(repo: string, rebuild: boolean): Promise<void> {
  // ADR-0032f: Start daemon, register and index project in daemon
  console.log(`• starting daemon...`);

  try {
    // ---------------------------------------------------------------------
    // ALWAYS restart a daemon that is already listening.
    //
    // This is the same class of bug as the two `ensure*Binary` comments below,
    // and it is the one they could not catch. `cargo build` refreshes the
    // binaries on disk; it cannot refresh a *process* that started hours ago.
    // `filigrio daemon start` against a live socket exits non-zero ("Daemon is
    // already running"), and this function spawned it detached with stderr
    // discarded and never checked — so the run silently proceeded against
    // whatever daemon happened to be up, and the daemon is where the
    // extractors, the responder and the whole relation vocabulary live.
    //
    // The 2026-07-29 16-30 and 17-06 runs are what this costs. Both were served
    // by a daemon started 07:36, nine hours before `d5e9f5d` taught the
    // responder the words `semantic` and `any`. The bridge (rebuilt) advertised
    // them; `skill.md` (edited) told the model to name them; the daemon (stale)
    // returned an empty list for every one. 33 of 33 `relations:["semantic"]`
    // calls came back empty, the model concluded "the graph does not contain
    // any call edges", and the score fell 14 → 9 → 7 in a way that read as
    // model quality. Verified after the fact: the same query against a daemon
    // built from the same tree returns the rows.
    // ---------------------------------------------------------------------
    if (await fileExists(SOCKET)) {
      console.log(`• stopping the daemon already on ${SOCKET} (it may predate the current build)`);
      await sh(CLI_BIN, ["daemon", "stop", "--socket", SOCKET]).catch(() => {});
      if (!(await waitForSocket(false))) {
        throw new Error(
          `a daemon is still listening on ${SOCKET} and would not stop. ` +
            `Refusing to run: it may have been built from a different tree, and ` +
            `an eval against a stale engine scores the fixture, not the model.`,
        );
      }
    }

    // Start the daemon in background. stderr is captured to a file rather than
    // discarded: a failure here used to be completely invisible.
    const daemonLog = resolve(RUNS, "daemon.log");
    await Deno.mkdir(RUNS, { recursive: true });
    const errFile = await Deno.open(daemonLog, { write: true, create: true, truncate: true });
    const daemon = new Deno.Command(CLI_BIN, {
      args: ["daemon", "start", "--socket", SOCKET, "--idle-timeout", "600"],
      cwd: resolve(REPO, "src"),
      stdout: "null",
      stderr: errFile.writable ? "piped" : "null",
      detached: true,
    });
    const child = daemon.spawn();
    child.stderr?.pipeTo(errFile.writable).catch(() => {});

    console.log(`• waiting for daemon socket...`);
    if (!(await waitForSocket(true))) {
      throw new Error(`daemon failed to start within timeout (see ${daemonLog})`);
    }

    // `--rebuild` was documented in this file's header and in the README and
    // implemented nowhere: `argValue("--task")` was the only flag ever read. It
    // matters because `project index` is *incremental* against `state.json`, so
    // a graph built by last week's extractor is carried forward file-by-file and
    // never re-derived. The CLI's own `--clean` is reserved-not-implemented
    // (ADR-0042 F7), so the honest way to force a cold build is to remove the
    // store the daemon would otherwise load.
    if (rebuild) {
      const state = resolve(repo, ".filigrio-out", "state.json");
      console.log(`• --rebuild: removing ${state} to force a cold index`);
      await Deno.remove(state).catch(() => {});
    }

    console.log(`• registering project ${repo} in daemon (${SOCKET})`);

    // Registration is the one tolerated failure: re-registering an existing
    // project is expected on a re-run. Indexing is NOT tolerated — see below.
    await sh(CLI_BIN, ["project", "register", "--socket", SOCKET, "--force"], repo)
      .catch(() => console.log(`• project already registered`));

    // `project index` is synchronous since ADR-0042 F6c: it applies, then the
    // response IS the outcome, and a failure exits non-zero. There is no
    // `--wait` (waiting is the only behaviour) and no `--force` (it is
    // `--clean` since F7, and reserved). If this fails, the eval must not run:
    // scoring an agent against an unindexed graph produces numbers that look
    // like model quality and are actually a broken fixture.
    console.log(`• indexing project in daemon...`);
    await sh(CLI_BIN, ["project", "index", "--socket", SOCKET], repo);

    // `graph.json` is the grounding harness's fact source (below), and since
    // ADR-0042 F2 an apply no longer writes it: the snapshot left the apply
    // path (~157 MB per apply for a file nothing in production reads) and is
    // produced ONLY by this explicit verb. Without it, `loadGraphFacts` falls
    // back to an empty fact set and every citation silently fails to ground —
    // which reads as a model that never cites its evidence.
    console.log(`• exporting graph.json for the grounding facts...`);
    await sh(CLI_BIN, ["project", "export", "--socket", SOCKET], repo);

    console.log(`• daemon started, project registered, indexed and exported`);
  } catch (e: unknown) {
    throw new Error(
      `agent-eval setup failed: ${(e as Error).message}\n` +
        `Refusing to run: an eval against an unindexed or stale graph scores the ` +
        `fixture, not the model.`,
    );
  }
}

/** Call one MCP tool and return its text, whatever the client's result shape. */
// deno-lint-ignore no-explicit-any
async function callTool(tools: any, name: string, args: Record<string, unknown>): Promise<string> {
  const out = await tools[name].execute(args, { toolCallId: "probe", messages: [] });
  if (typeof out === "string") return out;
  // deno-lint-ignore no-explicit-any
  const content = (out as any)?.content;
  return Array.isArray(content)
    ? content.map((c: { text?: string }) => c.text ?? "").join("\n")
    : JSON.stringify(out);
}

/**
 * Refuse to run if the tool surface does not answer the questions `skill.md`
 * teaches. Three probes on the graph's own top hub, all cheap:
 *
 *   1. `relations:["any"]`      — the widest filter must return rows at all.
 *   2. `relations:["semantic"]` — the *named default*. A daemon older than
 *      `d5e9f5d` accepts the word and matches nothing, so this returns empty
 *      while (1) is full. That is exactly the silent failure that made the
 *      2026-07-29 16-30 and 17-06 runs unreadable, and nothing in the transcript
 *      said so: 33 of 33 such calls came back blank and the model concluded the
 *      graph had no call edges.
 *   3. `relations:["callers"]`  — ADR-0044's named question. If the bridge on
 *      disk predates it, the value is not in the enum and never resolves.
 *
 * A probe that fails means the harness would be measuring a stale engine, which
 * produces numbers that look like model quality. Same refusal principle as
 * `ensureStore`: better no result than a result about the wrong thing.
 */
// deno-lint-ignore no-explicit-any
async function assertSurfaceAnswers(tools: any): Promise<void> {
  const hub = await callTool(tools, "god_nodes", { limit: 1 });
  const id = hub.match(/id=([^\]\s]+)/)?.[1];
  if (!id) throw new Error(`surface probe: god_nodes returned no addressable node:\n${hub}`);

  const rows = (text: string) => text.split("\n").filter((l) => l.includes("-->") || l.includes("<--")).length;
  const any = await callTool(tools, "get_neighbors", { id, relations: ["any"] });
  if (rows(any) === 0) {
    throw new Error(`surface probe: the top hub ${id} has no edges under relations:["any"]:\n${any}`);
  }
  const semantic = await callTool(tools, "get_neighbors", { id, relations: ["semantic"] });
  if (rows(semantic) === 0) {
    throw new Error(
      `surface probe: relations:["semantic"] returned no rows on ${id}, but ["any"] returned ` +
        `${rows(any)}. The daemon on ${SOCKET} does not understand a value the tool schema ` +
        `advertises and skill.md teaches — it is almost certainly older than the binaries on ` +
        `disk. Refusing to run: this scores a stale engine as if it were the model.\n${semantic}`,
    );
  }

  // ADR-0044's named questions are resolved by the *bridge*, so a stale
  // `filigrio-mcp` would pass the word straight through. Row count can't test
  // that — the top hub is a type, and a type legitimately has no `callers` — but
  // the no-match note echoes the filter it actually applied, so translation is
  // observable either way: rows, or a note that says `calls`, never `callers`.
  const callers = await callTool(tools, "get_neighbors", { id, relations: ["callers"] });
  if (rows(callers) === 0 && !callers.includes("relations=[calls]")) {
    throw new Error(
      `surface probe: relations:["callers"] was not translated into (calls, in) — the bridge on ` +
        `disk predates ADR-0044's named questions, so every question name the skill teaches is ` +
        `an unrecognised filter that silently matches nothing.\n${callers}`,
    );
  }
  console.log(`• surface probe ok (any=${rows(any)} semantic=${rows(semantic)} rows; callers translated)`);
}

async function main(): Promise<void> {
  if (!(await providerUp())) {
    const hint = IS_REMOTE
      ? `check PROVIDER_API_KEY / AI_MODEL_ID and network`
      : `set LLAMA_BASE_URL, or point AI_PROVIDER at a hosted model`;
    console.log(`SKIP: model provider not reachable at ${BASE_URL} (${hint}).`);
    console.log("The deterministic harness (deno task test) runs without a model.");
    return;
  }

  // Tasks file is `{ repo, tasks }` (repo grounds the eval to a checkout); a bare
  // array is still accepted. The repo path resolves env → file → built-in default.
  const spec = JSON.parse(await Deno.readTextFile(resolve(HERE, "tasks.json")));
  const allTasks: Task[] = Array.isArray(spec) ? spec : spec.tasks;
  const repo = resolveRepo(spec);

  await ensureMCPBinary();
  await ensureCLIBinary();
  await ensureStore(repo, Deno.args.includes("--rebuild"));

  const skill = await Deno.readTextFile(resolve(HERE, "skill.md"));

  const only = argValue("--task");
  const tasks = only ? allTasks.filter((t) => t.id === only) : allTasks;
  if (tasks.length === 0) throw new Error(`no task matching --task ${only}`);

  // Each invocation gets its own timestamped run directory so history is preserved
  // and a re-run never clobbers the last one. `runs/latest` points at the newest.
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const runDir = resolve(RUNS, stamp);
  await Deno.mkdir(runDir, { recursive: true });
  // The store lives in the *project's own* `.filigrio-out/`
  // (filigrio-daemon/src/project.rs: `output_dir = root.join(".filigrio-out")`).
  // `state.json` is written by the apply; `graph.json` is NOT — since ADR-0042
  // F2 it is produced only by `project export`, which `ensureStore` now runs
  // for exactly this reason. The empty-facts fallback below is kept for the
  // case where the file is genuinely absent, but note what it costs: an empty
  // fact set grounds nothing, so a missing export shows up as a model that
  // never cites evidence rather than as a broken fixture. Hence the warning.
  const graphJsonPath = resolve(repo, ".filigrio-out", "graph.json");
  const facts = await loadGraphFacts(graphJsonPath).catch((e) => {
    console.log(`• grounding facts unavailable (${(e as Error).message}) — citations won't ground`);
    return {
      labelFiles: new Map<string, Set<string>>(),
      edgesDirected: new Set<string>(),
      pairs: new Set<string>(),
      unresolvedDirected: new Set<string>(),
      unresolvedPairs: new Set<string>(),
      projects: new Set<string>(),
      nodeIds: new Set<string>(),
    };
  });
  const model = buildModel();
  const client = await connectFiligrio(BIN, SOCKET, repo);

  const summary: Array<{ id: string; pass: boolean; grounded: boolean; steps: number; reasons: string[] }> = [];
  try {
    const mcpTools = await client.tools();
    // The graph tools, plus the plain filesystem an engineer would otherwise use.
    // `read_file` alone left the model no alternative, so "it used the graph" was a
    // property of the toolset, not a preference: with `ls`/`grep` on the table the
    // choice is free, and every call is logged, so the traces show which was reached
    // for first — and per task, which questions the graph answers badly.
    const tools = {
      ...mcpTools,
      read_file: readFileTool(repo),
      ls: lsTool(repo),
      grep: grepTool(repo),
    };
    await assertSurfaceAnswers(mcpTools);
    console.log(`• model=${MODEL_ID}  tools=${Object.keys(tools).join(", ")}\n`);
    // Run-level context, once — the model and the exact system prompt in force, so a
    // run's traces are fully reproducible without checking out the source at that time.
    await Deno.writeTextFile(
      resolve(runDir, "_run.json"),
      JSON.stringify({ model: MODEL_ID, tools: Object.keys(tools), system: skill }, null, 2),
    );

    for (const task of tasks) {
      console.log(`▶ ${task.id}: ${task.prompt}`);
      // Surface this task's legal STATUS vocabulary in the prompt (ADR-0030), so the
      // model answers in the outcome set the scorer checks. The escape hatch is always
      // available; the STATUS/EVIDENCE/WHY shape itself is taught once in skill.md.
      const outcomeLine = task.rubric.outcomes?.length
        ? `\n\nEnd with the required STATUS / RESULT / EVIDENCE / WHY answer. Your RESULT ` +
          `line MUST copy exactly one of these tags verbatim: ${task.rubric.outcomes.join(", ")}` +
          ` (or UNVERIFIED if you truly cannot tell). Do NOT invent a different tag, and do ` +
          `NOT reuse a tag from the skill's example unless it appears in that list.`
        : "";
      // The exact request the model was given — recorded first in the trace so a run is
      // self-describing and the original task is recoverable even after the harness
      // reframes prompts (the system prompt is captured once per run in _run.json).
      const request = task.prompt + outcomeLine;
      let transcript;
      try {
        transcript = await runTask({
          model,
          tools,
          system: skill,
          prompt: request,
          task: task.id,
          maxSteps: task.maxSteps ?? MAX_STEPS,
          outcomes: task.rubric.outcomes,
        });
      } catch (e) {
        // Persist the failure so it's readable, not just a one-line console note:
        // the provider's response body (400s, rate limits, bad tool schema) is the
        // detail you need to tell a task/rubric bug from a model/provider error.
        // deno-lint-ignore no-explicit-any
        const err = e as any;
        const detail = {
          step: "error",
          tool: "_error",
          name: err?.name ?? "Error",
          message: err?.message ?? String(e),
          statusCode: err?.statusCode ?? err?.status,
          url: err?.url,
          responseBody: (err?.responseBody ?? err?.cause?.responseBody ?? "")?.toString().slice(0, 4000),
        };
        // Record the request even on failure, so an errored run is still recoverable.
        await Deno.writeTextFile(
          resolve(runDir, `${task.id}.jsonl`),
          JSON.stringify({ step: "request", tool: "_request", task: task.id, prompt: request }) +
            "\n" + JSON.stringify(detail) + "\n",
        );
        console.log(`  ✗ run error: ${detail.message}\n`);
        summary.push({ id: task.id, pass: false, grounded: false, steps: 0, reasons: [`run error: ${detail.message}`] });
        continue;
      }
      const s = score(task, transcript);
      // Ground ONLY the EVIDENCE block (ADR-0030): the WHY prose is never parsed, so a
      // sentence mentioning a word can no longer read as a fabricated citation.
      const sections = parseSections(transcript.finalText);
      const g = checkGrounding(sections.evidence, facts);
      const groundedFacts = g.citations.grounded.length + g.edges.grounded.length;
      // A symbol-level open-ended task (no verdict AND no `answerIncludes`) is judged
      // purely on the facts it produced, so it must cite ≥1 grounded fact — else an
      // empty `FOUND`/`UNVERIFIED` would pass. Tasks with an `answerIncludes` are graded
      // on that instead: their answer may be *aggregate* (an overview of stats/projects,
      // a project-dependency map) with no code symbol to cite `[src= loc=]`. Decidable
      // tasks (an `expectedOutcome`) are gated by the verdict.
      const needsCitedEvidence =
        !task.rubric.expectedOutcome && !task.rubric.answerIncludes?.length;
      const evidenceReasons =
        needsCitedEvidence && groundedFacts === 0
          ? ["no grounded evidence — the answer must cite ≥1 real fact"]
          : [];
      const pass = s.pass && g.ok && evidenceReasons.length === 0;
      const reasons = [...s.reasons, ...g.reasons, ...evidenceReasons];
      // Ordered records: each model step (text + reasoning) then its tool calls,
      // the full final answer, and the grounding verdict — so a run is fully
      // readable, including the model's reasoning on the tasks that look wrong.
      const jsonl = [
        JSON.stringify({ step: "request", tool: "_request", task: task.id, prompt: request }),
        ...transcriptEvents(transcript).map((ev) => JSON.stringify(ev)),
      ]
        .concat(
          JSON.stringify({ step: "final", tool: "_answer", text: transcript.finalText }),
          JSON.stringify({ step: "grounding", ...g }),
        )
        .join("\n") + "\n";
      await Deno.writeTextFile(resolve(runDir, `${task.id}.jsonl`), jsonl);
      const used = transcript.toolCalls.map((c) => c.tool).join(" → ") || "(no tools)";
      console.log(`  tools: ${used}`);
      console.log(`  status=${sections.status} · result=${sections.result}`);
      console.log(
        `  grounding: ${g.citations.grounded.length} cites ok, ` +
          `${g.edges.grounded.length} edges ok` +
          (g.ok ? "" : ` · ${g.citations.unknownLabel.length + g.edges.unknown.length} FABRICATED`),
      );
      console.log(`  ${pass ? "✓ pass" : "✗ fail"}${reasons.length ? " — " + reasons.join("; ") : ""}\n`);
      summary.push({ id: task.id, pass, grounded: g.ok, steps: transcript.toolCalls.length, reasons });
    }
  } finally {
    await client.close();
  }

  await Deno.writeTextFile(resolve(runDir, "summary.json"), JSON.stringify(summary, null, 2) + "\n");
  // Refresh runs/latest → this run (best-effort; symlinks may be unsupported).
  const latest = resolve(RUNS, "latest");
  await Deno.remove(latest).catch(() => {});
  await Deno.symlink(runDir, latest).catch(() => {});
  const passed = summary.filter((r) => r.pass).length;
  console.log(`── summary: ${passed}/${summary.length} passed · logs in ${runDir}/`);
  for (const r of summary) {
    console.log(
      `   ${r.pass ? "✓" : "✗"} ${r.id.padEnd(16)} ${r.steps} tool calls · ` +
        `grounding ${r.grounded ? "ok" : "FABRICATED"}`,
    );
  }
}

if (import.meta.main) {
  await main();
}
