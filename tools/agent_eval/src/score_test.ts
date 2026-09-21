import { assert, assertEquals } from "@std/assert";
import { isSubsequence, score } from "./score.ts";
import type { Task, Transcript } from "./types.ts";

function transcript(tools: string[], finalText = ""): Transcript {
  return {
    task: "t",
    modelSteps: [],
    toolCalls: tools.map((tool, i) => ({ step: i + 1, tool, args: {}, result: "", ms: 1 })),
    finalText,
  };
}

Deno.test("passes when all required tools and answer substrings are present", () => {
  const task: Task = {
    id: "hub",
    prompt: "?",
    rubric: { tools: ["god_nodes"], answerIncludes: ["parse"] },
  };
  const s = score(task, transcript(["graph_stats", "god_nodes"], "The hub is Parse."));
  assert(s.pass, s.reasons.join(", "));
  assertEquals(s.reasons, []);
});

Deno.test("reports each missing tool and missing answer fact", () => {
  const task: Task = {
    id: "x",
    prompt: "?",
    rubric: { tools: ["god_nodes", "get_neighbors"], answerIncludes: ["auth"] },
  };
  const s = score(task, transcript(["graph_stats"], "unrelated"));
  assert(!s.pass);
  assert(s.reasons.some((r) => r.includes("god_nodes")));
  assert(s.reasons.some((r) => r.includes("get_neighbors")));
  assert(s.reasons.some((r) => r.includes("auth")));
});

Deno.test("ordered rubric requires a subsequence, not adjacency", () => {
  const task: Task = {
    id: "o",
    prompt: "?",
    rubric: { ordered: ["god_nodes", "get_neighbors"] },
  };
  // god_nodes ... (query_graph) ... get_neighbors → still an ordered subsequence.
  assert(score(task, transcript(["god_nodes", "query_graph", "get_neighbors"])).pass);
  // reversed order → fail.
  const bad = score(task, transcript(["get_neighbors", "god_nodes"]));
  assert(!bad.pass);
  assert(bad.reasons[0].includes("not called in order"));
});

Deno.test("answer match is case-insensitive", () => {
  const task: Task = { id: "c", prompt: "?", rubric: { answerIncludes: ["PROJECT_GRAPH"] } };
  assert(score(task, transcript([], "see project_graph output")).pass);
});

Deno.test("outcome rubric checks the RESULT tag against the legal set (ADR-0030)", () => {
  const task: Task = {
    id: "reach",
    prompt: "?",
    rubric: { outcomes: ["REACHES", "DOES_NOT_REACH"] },
  };
  // A legal RESULT passes (the execution STATUS is a separate, un-checked field).
  assert(score(task, transcript([], "STATUS: OK\nRESULT: [REACHES]\nEVIDENCE:\n  a --calls--> b")).pass);
  // A RESULT outside the set (and not the escape hatch) fails, naming what it got.
  const bad = score(task, transcript([], "RESULT: MAYBE\nEVIDENCE:"));
  assert(!bad.pass);
  assert(bad.reasons.some((r) => r.toLowerCase().includes("result")));
  // The universal escape hatch is always a legal RESULT.
  assert(score(task, transcript([], "RESULT: [UNVERIFIED]")).pass);
});

Deno.test("expectedOutcome demands the correct RESULT; UNVERIFIED then fails (ADR-0030)", () => {
  const task: Task = {
    id: "reach",
    prompt: "?",
    rubric: { outcomes: ["REACHES", "DOES_NOT_REACH"], expectedOutcome: "REACHES" },
  };
  assert(score(task, transcript([], "STATUS: OK\nRESULT: REACHES\nEVIDENCE:")).pass);
  // Right vocabulary, wrong verdict.
  assert(!score(task, transcript([], "RESULT: DOES_NOT_REACH")).pass);
  // Dodging a task with a known answer is a miss (execution OK but no verdict).
  assert(!score(task, transcript([], "STATUS: OK\nRESULT: UNVERIFIED")).pass);
});

Deno.test("a missing RESULT reads as UNVERIFIED (lenient parse)", () => {
  const task: Task = { id: "o", prompt: "?", rubric: { outcomes: ["FOUND"] } };
  // No RESULT line → UNVERIFIED → legal (escape hatch), passes the outcome check.
  assert(score(task, transcript([], "I could not tell.")).pass);
});

Deno.test("isSubsequence basics", () => {
  assert(isSubsequence(["a", "c"], ["a", "b", "c"]));
  assert(!isSubsequence(["c", "a"], ["a", "b", "c"]));
  assert(isSubsequence([], ["a"]));
});
