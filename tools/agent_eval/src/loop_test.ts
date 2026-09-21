import { assertEquals } from "@std/assert";
import { recordStep, reasoningText } from "./loop.ts";
import { TranscriptLogger, transcriptEvents } from "./logger.ts";

Deno.test("recordStep matches results to calls by toolCallId (AI SDK 7 shape)", () => {
  const log = new TranscriptLogger("t");
  const step = {
    toolCalls: [
      { toolCallId: "a", toolName: "graph_stats", input: {} },
      { toolCallId: "b", toolName: "get_node", input: { label: "parse" } },
    ],
    toolResults: [
      // deliberately out of order — must still pair by id, not index.
      { toolCallId: "b", output: "Node: parse" },
      { toolCallId: "a", output: "Nodes: 3" },
    ],
  };
  recordStep(log, step, 42);
  const calls = log.toolCalls;
  assertEquals(calls.map((c) => c.tool), ["graph_stats", "get_node"]);
  assertEquals(calls[0].result, "Nodes: 3");
  assertEquals(calls[1].result, "Node: parse");
  assertEquals(calls[1].args, { label: "parse" });
});

Deno.test("recordStep tolerates legacy args/result field names", () => {
  const log = new TranscriptLogger("t");
  recordStep(log, {
    toolCalls: [{ toolCallId: "x", toolName: "god_nodes", args: { top_n: 3 } }],
    toolResults: [{ toolCallId: "x", result: "gods" }],
  }, 1);
  assertEquals(log.toolCalls[0].args, { top_n: 3 });
  assertEquals(log.toolCalls[0].result, "gods");
});

Deno.test("recordStep captures a step with no tool calls (reasoning stays visible)", () => {
  const log = new TranscriptLogger("t");
  // A reasoning-only / errored step must still be logged, so a run that made no
  // tool calls isn't a blank file — that's how you see why it went wrong.
  recordStep(log, { text: "I think the hub is parse.", finishReason: "stop" }, 1);
  assertEquals(log.toolCalls.length, 0);
  assertEquals(log.modelSteps.length, 1);
  assertEquals(log.modelSteps[0].text, "I think the hub is parse.");
  assertEquals(log.modelSteps[0].finishReason, "stop");
});

Deno.test("recordStep captures per-step text + reasoning + finishReason", () => {
  const log = new TranscriptLogger("t");
  recordStep(log, {
    text: "Answer: parse.",
    reasoningText: "parse has the most edges, so it is the hub.",
    finishReason: "tool-calls",
    toolCalls: [{ toolCallId: "a", toolName: "god_nodes", input: { top_n: 1 } }],
    toolResults: [{ toolCallId: "a", output: "1. parse - 2 edges" }],
    usage: { totalTokens: 42 },
  }, 5);
  const ms = log.modelSteps[0];
  assertEquals(ms.reasoning, "parse has the most edges, so it is the hub.");
  assertEquals(ms.toolNames, ["god_nodes"]);
  assertEquals(ms.usage, { totalTokens: 42 });
});

Deno.test("reasoningText normalizes string, reasoningText, and array shapes", () => {
  assertEquals(reasoningText({ reasoningText: "a" }), "a");
  assertEquals(reasoningText({ reasoning: "b" }), "b");
  assertEquals(reasoningText({ reasoning: [{ text: "x" }, { text: "y" }] }), "xy");
  assertEquals(reasoningText({}), "");
});

Deno.test("transcriptEvents interleaves each model step before its tool calls", () => {
  const log = new TranscriptLogger("t");
  recordStep(log, {
    text: "step1",
    toolCalls: [{ toolCallId: "a", toolName: "god_nodes", input: {} }],
    toolResults: [{ toolCallId: "a", output: "gods" }],
  }, 1);
  recordStep(log, {
    text: "step2",
    toolCalls: [{ toolCallId: "b", toolName: "get_node", input: {} }],
    toolResults: [{ toolCallId: "b", output: "node" }],
  }, 1);
  const events = transcriptEvents(log.transcript("done"));
  assertEquals(events.map((e) => e.kind), ["model", "tool", "model", "tool"]);
  assertEquals((events[1] as { tool: string }).tool, "god_nodes");
  assertEquals((events[3] as { tool: string }).tool, "get_node");
});
