//! Deterministic task scorer (ADR-0025). A pure function over a `Transcript` and a
//! `Rubric` — no LLM, no I/O — so it can gate regressions and is unit-testable. The
//! rubric checks *tool usage* (did the agent reach for the right endpoints, in a
//! sensible order?) and *answer content* (did it surface the expected facts?).

import type { Score, Task, Transcript } from "./types.ts";
import { parseSections } from "./grounding.ts";

/** The universal escape hatch (ADR-0030): always a legal `STATUS`, so a model that
 *  cannot honestly fit a task's outcomes says so instead of fabricating a verdict. */
const ESCAPE = "UNVERIFIED";

export function score(task: Task, t: Transcript): Score {
  const reasons: string[] = [];
  const used = t.toolCalls.map((c) => c.tool);
  const { tools, ordered, answerIncludes, outcomes, expectedOutcome } = task.rubric;

  for (const tool of tools ?? []) {
    if (!used.includes(tool)) reasons.push(`missing tool: ${tool}`);
  }

  if (ordered && !isSubsequence(ordered, used)) {
    reasons.push(
      `tools not called in order: expected subsequence [${ordered.join(" → ")}], ` +
        `got [${used.join(" → ") || "none"}]`,
    );
  }

  // The structured verdict (ADR-0030): the `RESULT` tag (the *outcome*, distinct from
  // the *execution* STATUS) must be a legal outcome, and — when the task has one correct
  // answer — the expected one. `UNVERIFIED` is always a legal RESULT (honest escape),
  // but does not satisfy a fixed `expectedOutcome`.
  if (outcomes || expectedOutcome) {
    const result = parseSections(t.finalText).result;
    const expected = expectedOutcome ? [expectedOutcome].flat() : [];
    const legal = new Set([...(outcomes ?? []), ESCAPE, ...expected]);
    if (!legal.has(result)) {
      reasons.push(`result '${result}' not one of [${[...legal].join(", ")}]`);
    } else if (expected.length && !expected.includes(result)) {
      reasons.push(`expected outcome ${expected.map((e) => `'${e}'`).join(" or ")}, got '${result}'`);
    }
  }

  const hay = t.finalText.toLowerCase();
  for (const sub of answerIncludes ?? []) {
    if (!hay.includes(sub.toLowerCase())) reasons.push(`answer missing: "${sub}"`);
  }

  return { pass: reasons.length === 0, reasons };
}

/** True iff `needle` appears as an ordered (not necessarily contiguous) subsequence. */
export function isSubsequence(needle: string[], hay: string[]): boolean {
  let i = 0;
  for (const h of hay) {
    if (i < needle.length && h === needle[i]) i++;
  }
  return i === needle.length;
}
