//! Transcript logger — accumulates the model's per-step output and tool calls and
//! serializes them to JSONL (ADR-0025). Pure and deterministic: no clock, no I/O.
//! `ms` is supplied by the caller so tests stay reproducible.

import type { ModelStep, ToolCall, Transcript } from "./types.ts";

/** Tool results can be large (whole subgraphs); cap what we retain per call. */
export const RESULT_CAP = 2000;
/** The final answer is capped higher — it's the payload we most want to read. */
export const ANSWER_CAP = 8000;
/** Model text/reasoning is capped generously — reading it is the whole point. */
export const REASONING_CAP = 16000;

export function cap(s: string, n: number): string {
  return s.length > n ? `${s.slice(0, n)}…[+${s.length - n} chars]` : s;
}

/** One serialized JSONL record: a `model` step or a `tool` call, in call order. */
export type Event =
  | ({ kind: "model" } & ModelStep)
  | ({ kind: "tool" } & ToolCall);

/** Rebuild the ordered model/tool event stream from a finished transcript: each
 *  model step, then the tool calls it made (attributed by `toolNames` count). The
 *  single source of truth for "transcript → readable records" (used to serialize a
 *  run's JSONL). */
export function transcriptEvents(t: Transcript): Event[] {
  const events: Event[] = [];
  let ti = 0;
  for (const ms of t.modelSteps) {
    events.push({ kind: "model", ...ms });
    for (let k = 0; k < ms.toolNames.length && ti < t.toolCalls.length; k++) {
      events.push({ kind: "tool", ...t.toolCalls[ti++] });
    }
  }
  for (; ti < t.toolCalls.length; ti++) events.push({ kind: "tool", ...t.toolCalls[ti] });
  return events;
}

export class TranscriptLogger {
  readonly task: string;
  #events: Event[] = [];
  #calls: ToolCall[] = [];
  #steps: ModelStep[] = [];

  constructor(task: string) {
    this.task = task;
  }

  /** Append one model generation step (its text, reasoning, and why it ended).
   *  Recorded before that step's tool calls, so the JSONL reads reasoning→tools. */
  recordModelStep(
    fields: { text: string; reasoning: string; finishReason: string; toolNames: string[]; usage?: Record<string, number> },
  ): ModelStep {
    const ms: ModelStep = {
      step: this.#steps.length + 1,
      text: cap(fields.text, REASONING_CAP),
      reasoning: cap(fields.reasoning, REASONING_CAP),
      finishReason: fields.finishReason,
      toolNames: fields.toolNames,
      usage: fields.usage,
    };
    this.#steps.push(ms);
    this.#events.push({ kind: "model", ...ms });
    return ms;
  }

  /** Append one tool call; `step` is assigned 1-based in call order. */
  record(
    tool: string,
    args: Record<string, unknown>,
    result: string,
    ms: number,
  ): ToolCall {
    const tc: ToolCall = {
      step: this.#calls.length + 1,
      tool,
      args,
      result: cap(result, RESULT_CAP),
      ms: Math.max(0, Math.round(ms)),
    };
    this.#calls.push(tc);
    this.#events.push({ kind: "tool", ...tc });
    return tc;
  }

  get toolCalls(): ToolCall[] {
    return [...this.#calls];
  }

  get modelSteps(): ModelStep[] {
    return [...this.#steps];
  }

  transcript(finalText: string): Transcript {
    return { task: this.task, modelSteps: this.modelSteps, toolCalls: this.toolCalls, finalText };
  }

  /** One JSON object per line — each model step and tool call in order, then a
   *  final `_answer` record with the full answer text. */
  toJsonl(finalText: string): string {
    const lines = this.#events.map((e) => JSON.stringify(e));
    lines.push(
      JSON.stringify({ step: "final", tool: "_answer", text: cap(finalText, ANSWER_CAP) }),
    );
    return `${lines.join("\n")}\n`;
  }
}
