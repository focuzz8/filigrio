//! Shared types for the agent-eval harness (ADR-0025).

/** One tool invocation the model made, as logged. */
export interface ToolCall {
  step: number;
  tool: string;
  args: Record<string, unknown>;
  result: string;
  ms: number;
}

/** The model's own output for one generation step — its visible text and (for a
 *  reasoning model) its thinking, plus why the step ended. This is what you read to
 *  see the model's actual responses, not just the tools it happened to call. */
export interface ModelStep {
  step: number;
  text: string;
  reasoning: string;
  finishReason: string;
  toolNames: string[];
  usage?: Record<string, number>;
}

/** A full run of one task: the model's per-step output, every tool call, and the
 *  final answer. */
export interface Transcript {
  task: string;
  modelSteps: ModelStep[];
  toolCalls: ToolCall[];
  finalText: string;
}

/** Deterministic pass/fail rubric for a task (see `score`). */
export interface Rubric {
  /** Tools that must ALL appear at least once (any order). */
  tools?: string[];
  /** Tools that must appear as an ordered subsequence of the calls made. */
  ordered?: string[];
  /** Case-insensitive substrings the final answer must contain. */
  answerIncludes?: string[];
  /** The legal `STATUS` verdicts for this task (ADR-0030). The model's status tag
   *  must be one of these — plus the universal escape hatch `UNVERIFIED`, always
   *  legal. Surfaced to the model in the prompt so it knows the vocabulary. */
  outcomes?: string[];
  /** The *correct* verdict(s), when the task has one. The RESULT must be one of them;
   *  `UNVERIFIED` (the honest escape) then fails — dodging a task with a known answer is
   *  a miss. A set (not just one string) covers cases with several honest answers — e.g.
   *  a chain that reaches through a confirmed *unresolved* hop is fairly `REACHES` or
   *  `PARTIAL`. Omit for open tasks where any legal outcome (incl. `UNVERIFIED`) is
   *  acceptable so long as the evidence grounds (ADR-0030). */
  expectedOutcome?: string | string[];
}

export interface Task {
  id: string;
  prompt: string;
  rubric: Rubric;
  /** Per-task agent-step budget (LLM rounds). Falls back to the global MAX_STEPS.
   *  Heavier tasks (multi-way comparisons, path traces) legitimately need more;
   *  easy lookups stay cheap. Exhaustion is left to fail visibly (no forced answer)
   *  so the trace shows where the model ran out. */
  maxSteps?: number;
}

export interface Score {
  pass: boolean;
  reasons: string[];
}
