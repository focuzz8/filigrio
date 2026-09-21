import { assertEquals } from "@std/assert";
import { resolveModelConfig } from "./model.ts";

/** A fake env from a plain record (undefined for absent keys). */
const envOf = (m: Record<string, string>) => (k: string): string | undefined => m[k];

Deno.test("defaults to the local llama probe when nothing is set", () => {
  const c = resolveModelConfig(envOf({}));
  assertEquals(c.provider, "llama");
  assertEquals(c.baseURL, "http://127.0.0.1:45285/v1");
  assertEquals(c.modelId, "openai/qwen35");
  assertEquals(c.apiKey, undefined);
});

Deno.test("AI_PROVIDER=cerebras + key + model → hosted endpoint", () => {
  const c = resolveModelConfig(envOf({
    AI_PROVIDER: "cerebras",
    PROVIDER_API_KEY: "sk-secret",
    AI_MODEL_ID: "zai-glm-4.7",
  }));
  assertEquals(c.provider, "cerebras");
  assertEquals(c.baseURL, "https://api.cerebras.ai/v1");
  assertEquals(c.modelId, "zai-glm-4.7");
  assertEquals(c.apiKey, "sk-secret");
});

Deno.test("AI_BASE_URL overrides an unknown provider's endpoint", () => {
  const c = resolveModelConfig(envOf({
    AI_PROVIDER: "acme",
    AI_BASE_URL: "https://acme.example/v1",
    AI_MODEL_ID: "acme-1",
  }));
  assertEquals(c.baseURL, "https://acme.example/v1");
  assertEquals(c.provider, "acme");
});

Deno.test("legacy LLAMA_* still works, but AI_* wins when both are set", () => {
  const legacy = resolveModelConfig(envOf({
    LLAMA_BASE_URL: "http://box:9000/v1",
    LLAMA_MODEL: "openai/legacy",
  }));
  assertEquals(legacy.baseURL, "http://box:9000/v1");
  assertEquals(legacy.modelId, "openai/legacy");

  const both = resolveModelConfig(envOf({
    AI_BASE_URL: "https://new/v1",
    LLAMA_BASE_URL: "http://box:9000/v1",
    AI_MODEL_ID: "new",
    LLAMA_MODEL: "openai/legacy",
  }));
  assertEquals(both.baseURL, "https://new/v1");
  assertEquals(both.modelId, "new");
});
