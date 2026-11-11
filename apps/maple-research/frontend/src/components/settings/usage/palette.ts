import type { UsageClientApp, UsagePool } from "@/billing/usageApi";

// Colors read the validated `.usage-viz` variables in index.css (adjacent CVD
// ΔE >= 8, normal vision >= 15 against light and dark cards). Coral stays
// reserved for the plan meter and Chat; models never use it.

/** Display groups matching the model picker. Selection and daily usage do
 * not reorder these stable identities. Unlisted models retain a series until
 * the seven-color chart fills; overflow folds into "More models".
 */
export const HIGHLIGHT_GROUPS = [
  { label: "Most capable", models: ["kimi-k3-1-pro", "kimi-k3", "glm-5-3"] },
  { label: "Uncensored", models: ["deepseek-v4-1-flash-1017-uncensored"] },
  { label: "Fast & cheap", models: ["gemma4-31b", "glm-5-3-flash", "deepseek-v4-1-flash"] }
] as const;

const HIGHLIGHT_MODELS: readonly string[] = HIGHLIGHT_GROUPS.flatMap(({ models }) => models);

export function highlightGroup(model: string): string | undefined {
  return HIGHLIGHT_GROUPS.find(({ models }) => (models as readonly string[]).includes(model))
    ?.label;
}

/** Named series before the rest fold. One per color, and every model Work offers. */
export const MAX_NAMED_MODELS = HIGHLIGHT_MODELS.length;

export const MORE_MODELS_KEY = "more";
export const MORE_MODELS_COLOR = "var(--series-other)";

export function highlightIndex(model: string): number | undefined {
  const index = HIGHLIGHT_MODELS.indexOf(model);
  return index === -1 ? undefined : index;
}

export function seriesColor(slot: number): string {
  return `var(--series-${slot + 1})`;
}

/**
 * Color slot for a model drawn with `peers`. Highlights keep a fixed slot.
 * Other peers take the lowest free slot, in id order. Undefined when the
 * model is not highlighted and no free slot is left.
 */
export function modelColorSlot(model: string, peers: readonly string[]): number | undefined {
  if (!peers.includes(model)) return undefined;
  const pinned = highlightIndex(model);
  if (pinned !== undefined) return pinned;
  const taken = new Set<number>();
  for (const peer of peers) {
    const slot = highlightIndex(peer);
    if (slot !== undefined) taken.add(slot);
  }
  const rank = peers.filter((peer) => highlightIndex(peer) === undefined && peer < model).length;
  let free = 0;
  for (let slot = 0; slot < MAX_NAMED_MODELS; slot++) {
    if (taken.has(slot)) continue;
    if (free === rank) return slot;
    free += 1;
  }
  return undefined;
}

export function modelColor(model: string, peers?: readonly string[]): string {
  if (peers) {
    const slot = modelColorSlot(model, peers);
    return slot === undefined ? MORE_MODELS_COLOR : seriesColor(slot);
  }
  const pinned = highlightIndex(model);
  return pinned === undefined ? MORE_MODELS_COLOR : seriesColor(pinned);
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
 * Where usage came from: the reporting app, or `api` for API-key usage that
 * named no app. An API key can run inside any app, so `api` means only that.
 */
export type UsageOrigin = UsageClientApp | "api";

export const ORIGINS: readonly UsageOrigin[] = ["chat", "work", "proxy", "api"];

export const ORIGIN_COLOR: Readonly<Record<UsageOrigin, string>> = {
  chat: "var(--src-chat)",
  work: "var(--src-work)",
  proxy: "var(--src-proxy)",
  api: "var(--src-api)"
};

export const ORIGIN_LABEL: Readonly<Record<UsageOrigin, string>> = {
  chat: "Chat",
  work: "Work",
  proxy: "Proxy",
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
