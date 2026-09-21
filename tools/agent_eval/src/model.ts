//! The model under test, behind an **OpenAI-compatible** provider (ADR-0025). Two
//! deployments share one code path: a local llama.cpp server (a deliberately strict
//! ergonomics probe — a small model only succeeds when the tools are genuinely easy),
//! or a hosted provider like Cerebras via `PROVIDER_API_KEY` + `AI_MODEL_ID`.
//!
//! Selection is env-driven and *pure* in `resolveModelConfig`, so it's unit-tested
//! without touching the network (the harness's deterministic-core discipline).

import { createOpenAICompatible } from "@ai-sdk/openai-compatible";
import { createCerebras } from "@ai-sdk/cerebras";

/** Known OpenAI-compatible endpoints, keyed by `AI_PROVIDER`. `AI_BASE_URL`
 *  overrides for anything not listed. */
export const PROVIDER_BASE_URLS: Record<string, string> = {
  cerebras: "https://api.cerebras.ai/v1",
  llama: "http://127.0.0.1:45285/v1",
  local: "http://127.0.0.1:45285/v1",
};

const DEFAULT_LOCAL_URL = PROVIDER_BASE_URLS.llama;
const DEFAULT_MODEL_ID = "openai/qwen35";

export interface ModelConfig {
  /** `AI_PROVIDER` (defaults to `llama`). Labels the provider + picks a base URL. */
  provider: string;
  baseURL: string;
  modelId: string;
  /** Present for hosted providers; absent for local llama (which needs no auth). */
  apiKey?: string;
}

type Env = (key: string) => string | undefined;

/** Resolve the provider/model/base-URL/key from the environment. Precedence:
 *  explicit `AI_*` (the current knobs) → legacy `LLAMA_*` (back-compat) → the
 *  provider's default URL → the local llama default. Pure: takes an env accessor. */
export function resolveModelConfig(env: Env): ModelConfig {
  const provider = env("AI_PROVIDER") || "llama";
  const apiKey = env("PROVIDER_API_KEY") || undefined;
  const baseURL = env("AI_BASE_URL") ||
    env("LLAMA_BASE_URL") ||
    PROVIDER_BASE_URLS[provider] ||
    DEFAULT_LOCAL_URL;
  const modelId = env("AI_MODEL_ID") || env("LLAMA_MODEL") || DEFAULT_MODEL_ID;
  return { provider, baseURL, modelId, apiKey };
}

export const CONFIG = resolveModelConfig((k) => Deno.env.get(k));
export const BASE_URL = CONFIG.baseURL;
export const MODEL_ID = CONFIG.modelId;
/** A hosted provider (has a key) vs the local probe — changes the skip message. */
export const IS_REMOTE = CONFIG.apiKey !== undefined;

export function model() {
  // Cerebras has a dedicated AI-SDK provider that gets the request shape right (the
  // generic openai-compatible one 400s on its tool-call payload). Everything else —
  // local llama.cpp, other OpenAI-compatible hosts — goes through the generic path.
  if (CONFIG.provider === "cerebras") {
    const cerebras = createCerebras({ apiKey: CONFIG.apiKey });
    return cerebras(CONFIG.modelId);
  }
  const provider = createOpenAICompatible({
    name: CONFIG.provider,
    baseURL: CONFIG.baseURL,
    ...(CONFIG.apiKey ? { apiKey: CONFIG.apiKey } : {}),
  });
  return provider(CONFIG.modelId);
}

/** Health check: is the provider's `/models` endpoint reachable (with auth, when a
 *  key is set)? The live eval skips when not, so the harness never fails off-box —
 *  a down local server, or an unreachable/misconfigured hosted one, both skip
 *  cleanly (mirrors the oracle-diff skip discipline). */
export async function providerUp(): Promise<boolean> {
  try {
    const headers: HeadersInit = CONFIG.apiKey
      ? { authorization: `Bearer ${CONFIG.apiKey}` }
      : {};
    const res = await fetch(`${CONFIG.baseURL}/models`, {
      headers,
      signal: AbortSignal.timeout(2500),
    });
    await res.body?.cancel();
    return res.ok;
  } catch {
    return false;
  }
}
