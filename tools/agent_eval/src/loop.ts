//! The agent loop: run one task with `generateText`, capping iterations with
//! `stopWhen: stepCountIs(N)` (AI SDK 7), and log every tool call via `onStepFinish`
//! (ADR-0025). Field access is defensive — AI SDK renamed `args`→`input` and
//! `result`→`output` across majors, so we read either.

import { generateText, stepCountIs } from "ai";
import { TranscriptLogger } from "./logger.ts";
import type { Transcript } from "./types.ts";

// deno-lint-ignore no-explicit-any
type Any = any;

export interface RunOpts {
  model: unknown;
  tools: Record<string, unknown>;
  system: string;
  prompt: string;
  task: string;
  maxSteps?: number;
  /** The task's legal RESULT tags — restated in the forced-final nudge so a run that
   *  finalizes after exhausting its steps still knows the vocabulary (the SDK's
   *  `response.messages` may not carry the original prompt; without this the model
   *  answers `UNVERIFIED` "because no tag list was provided"). */
  outcomes?: string[];
  /** Injectable clock so the loop is testable; defaults to `performance.now`. */
  now?: () => number;
}

/** Normalize the SDK's reasoning field (string, `reasoningText`, or an array of
 *  reasoning parts) to a single string — field shape drifts across SDK versions. */
export function reasoningText(step: Any): string {
  if (typeof step?.reasoningText === "string") return step.reasoningText;
  if (typeof step?.reasoning === "string") return step.reasoning;
  if (Array.isArray(step?.reasoning)) {
    return step.reasoning.map((r: Any) => r?.text ?? r?.reasoning ?? "").join("");
  }
  return "";
}

/** Pull the loggable fields out of one finished step, tolerant of SDK field drift.
 *  The model's own output (text + reasoning + why the step ended) is recorded first,
 *  then the tool calls that step made — so a reader sees the reasoning that led to
 *  each call, and an errored step (finishReason `error`, or empty with no calls) is
 *  still visible. */
export function recordStep(logger: TranscriptLogger, step: Any, elapsedMs: number): void {
  const calls: Any[] = step?.toolCalls ?? [];
  const results: Any[] = step?.toolResults ?? [];
  logger.recordModelStep({
    text: typeof step?.text === "string" ? step.text : "",
    reasoning: reasoningText(step),
    finishReason: step?.finishReason ?? "",
    toolNames: calls.map((c) => c.toolName ?? "?"),
    usage: step?.usage && typeof step.usage === "object" ? step.usage : undefined,
  });
  for (const c of calls) {
    const r = results.find((x) => x.toolCallId === c.toolCallId);
    const args = (c.input ?? c.args ?? {}) as Record<string, unknown>;
    const raw = r?.output ?? r?.result ?? "";
    const out = typeof raw === "string" ? raw : JSON.stringify(raw);
    logger.record(c.toolName ?? "?", args, out, elapsedMs);
  }
}

export async function runTask(opts: RunOpts): Promise<Transcript> {
  const now = opts.now ?? (() => performance.now());
  const logger = new TranscriptLogger(opts.task);
  let last = now();

  const result = await generateText({
    model: opts.model as Any,
    tools: opts.tools as Any,
    system: opts.system,
    prompt: opts.prompt,
    stopWhen: stepCountIs(opts.maxSteps ?? 8),
    onStepFinish: (step: Any) => {
      const t = now();
      recordStep(logger, step, t - last);
      last = t;
    },
  });

  let text = result.text ?? "";
  // Two ways a run ends without a usable answer: the step budget ran out mid-tool-call
  // (empty text), or the model wrote a prose answer that ignores the format (no RESULT
  // tag — the outcome the scorer needs). Either way, give it one no-tools turn to
  // produce the required STATUS / RESULT / EVIDENCE / WHY answer — never a wasted run.
  const hasResult = (s: string) => /^\s*[*`#\s]*RESULT:/im.test(s);
  if (!text.trim() || !hasResult(text)) {
    const closing = await generateText({
      model: opts.model as Any,
      system: opts.system,
      messages: [
        ...((result as Any).response?.messages ?? []),
        {
          role: "user",
          content:
            "Stop using tools now and give your final answer to the original question in " +
            "the required four-section STATUS / RESULT / EVIDENCE / WHY format, using only " +
            "what the tools already returned. STATUS is your execution status (OK / " +
            "INCOMPLETE / FAILED). " +
            (opts.outcomes?.length
              ? `RESULT must be exactly one of these tags: ${opts.outcomes.join(", ")} ` +
                `(or UNVERIFIED only if you truly cannot tell).`
              : "RESULT is the outcome/verdict."),
        },
      ],
    });
    recordStep(logger, { text: closing.text, finishReason: "stop", toolCalls: [] }, now() - last);
    // Keep the closing answer only if it produced the outcome tag; else the model could
    // not comply and the original stands (the failure is then honestly the model's).
    if (hasResult(closing.text ?? "")) text = closing.text ?? "";
    else if (!text.trim()) text = closing.text ?? "";
  }

  return logger.transcript(text);
}
