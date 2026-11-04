import type { UsageClientApp, UsagePool, UsageSource } from "@/billing/usageApi";

// Colors read the validated `.usage-viz` variables in index.css (adjacent CVD
// ΔE >= 8, normal vision >= 15 against light and dark cards). Coral stays
// reserved for the plan meter and Chat; models never use it.

/**
 * Fixed slots for current flagship models. Colors follow model identity, never
 * rank, so a model keeps its color across ranges. Every other model folds into
 * the gray "More models" series in charts; tables still list it by name.
 */
export const MODEL_SLOTS: Readonly<Record<string, string>> = {
  "glm-5-3": "var(--series-1)",
  "kimi-k3": "var(--series-2)",
  "deepseek-v4-1-flash": "var(--series-3)",
  "gpt-oss-120b": "var(--series-4)",
  "glm-5-3-flash": "var(--series-5)"
};

export const MORE_MODELS_KEY = "more";
export const MORE_MODELS_COLOR = "var(--series-other)";

export function modelColor(model: string): string {
  return MODEL_SLOTS[model] ?? MORE_MODELS_COLOR;
}

export type TokenKind = "input" | "cached" | "output";

/** Pricing-page order. Colors step by price: cached lightest, output darkest. */
export const TOKEN_KINDS: readonly TokenKind[] = ["input", "cached", "output"];

export const TOKEN_COLOR: Readonly<Record<TokenKind, string>> = {
  input: "var(--tok-input)",
  cached: "var(--tok-cached)",
  output: "var(--tok-output)"
};

export const TOKEN_LABEL: Readonly<Record<TokenKind, string>> = {
  input: "Input",
  cached: "Cached input",
  output: "Output"
};

/**
 * Where usage came from: the reporting app, or for usage with no reported app,
 * how it authenticated. An API key can run inside any app, so `api` here means
 * only API-key usage that named no app.
 */
export type UsageOrigin = UsageClientApp | UsageSource;

export const ORIGINS: readonly UsageOrigin[] = ["chat", "work", "proxy", "api", "app"];

export const ORIGIN_COLOR: Readonly<Record<UsageOrigin, string>> = {
  chat: "var(--src-chat)",
  work: "var(--src-work)",
  proxy: "var(--src-proxy)",
  api: "var(--src-api)",
  app: "var(--src-earlier)"
};

export const ORIGIN_LABEL: Readonly<Record<UsageOrigin, string>> = {
  chat: "Chat",
  work: "Work",
  proxy: "Proxy",
  api: "API keys",
  app: "Earlier versions"
};

export const POOLS: readonly UsagePool[] = ["plan", "extra"];

export const POOL_COLOR: Readonly<Record<UsagePool, string>> = {
  plan: "var(--pool-plan)",
  extra: "var(--pool-extra)"
};

export const POOL_LABEL: Readonly<Record<UsagePool, string>> = {
  plan: "Plan credits",
  extra: "Extra credits"
};
