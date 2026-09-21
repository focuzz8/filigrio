import { assert, assertEquals } from "@std/assert";
import { ANSWER_CAP, cap, RESULT_CAP, TranscriptLogger } from "./logger.ts";

Deno.test("records tool calls with 1-based step order", () => {
  const log = new TranscriptLogger("t1");
  log.record("graph_stats", {}, "Nodes: 3", 12.4);
  log.record("god_nodes", { top_n: 5 }, "God nodes...", 30.9);
  const calls = log.toolCalls;
  assertEquals(calls.map((c) => c.step), [1, 2]);
  assertEquals(calls.map((c) => c.tool), ["graph_stats", "god_nodes"]);
  assertEquals(calls[1].args, { top_n: 5 });
  assertEquals(calls[1].ms, 31, "ms rounded");
});

Deno.test("caps oversized tool results", () => {
  const log = new TranscriptLogger("t");
  const huge = "x".repeat(RESULT_CAP + 500);
  const tc = log.record("query_graph", { q: "a" }, huge, 1);
  assert(tc.result.length < huge.length);
  assert(tc.result.includes("[+500 chars]"), tc.result.slice(-40));
});

Deno.test("toolCalls returns a copy (no external mutation)", () => {
  const log = new TranscriptLogger("t");
  log.record("graph_stats", {}, "ok", 1);
  log.toolCalls.push({ step: 99, tool: "x", args: {}, result: "", ms: 0 });
  assertEquals(log.toolCalls.length, 1, "internal state untouched");
});

Deno.test("toJsonl emits one line per call plus a final answer record", () => {
  const log = new TranscriptLogger("t");
  log.record("graph_stats", {}, "ok", 1);
  log.record("get_node", { label: "parse" }, "Node: parse", 2);
  const lines = log.toJsonl("The hub is parse.").trimEnd().split("\n");
  assertEquals(lines.length, 3);
  const recs = lines.map((l) => JSON.parse(l));
  assertEquals(recs[0].tool, "graph_stats");
  assertEquals(recs[1].args.label, "parse");
  assertEquals(recs[2].step, "final");
  assertEquals(recs[2].text, "The hub is parse.");
});

Deno.test("cap leaves short strings untouched", () => {
  assertEquals(cap("short", 100), "short");
  assert(cap("y".repeat(ANSWER_CAP + 1), ANSWER_CAP).includes("[+1 chars]"));
});
