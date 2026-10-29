import type { UsagePool, UsageSource } from "@/billing/usageApi";

// Colors read the validated `.usage-viz` variables in index.css (adjacent CVD
// ΔE >= 8, normal vision >= 15 against light and dark cards). Coral stays
// reserved for the plan meter and "Maple apps"; models never use it.

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

export const SOURCES: readonly UsageSource[] = ["app", "api"];

export const SOURCE_COLOR: Readonly<Record<UsageSource, string>> = {
  app: "var(--src-app)",
  api: "var(--src-api)"
};

export const SOURCE_LABEL: Readonly<Record<UsageSource, string>> = {
  app: "Maple apps",
  api: "API keys"
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
