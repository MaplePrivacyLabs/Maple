import type { UsageClientApp, UsagePool } from "@/billing/usageApi";

// Colors read the validated `.usage-viz` variables in index.css (adjacent CVD
// ΔE >= 8, normal vision >= 15 against light and dark cards). Coral stays
// reserved for the plan meter and Chat; models never use it.

export type ModelHighlights = {
  groups: readonly { id: string; label: string; models: readonly string[] }[];
  /** Group labels for catalog models, including explicitly ungrouped ones. */
  groupsByModel: ReadonlyMap<string, string | undefined>;
  /** A usable served catalog completely replaces the legacy hardcoded ranks. */
  fromCatalog: boolean;
};

/** Compatibility with an older or unavailable catalog. The served catalog,
 * when present, owns this hierarchy instead. Usage never reorders these
 * stable identities; unlisted models retain a series until the chart fills.
 */
export const HIGHLIGHT_GROUPS = [
  { id: "capable", label: "Most capable", models: ["kimi-k3-1-pro", "kimi-k3", "glm-5-3"] },
  {
    id: "uncensored",
    label: "Uncensored",
    models: ["deepseek-v4-1-flash-1017-uncensored"]
  },
  {
    id: "fast",
    label: "Fast & cheap",
    models: ["gemma4-31b", "glm-5-3-flash", "deepseek-v4-1-flash"]
  }
] as const;

const LEGACY_MODELS: readonly string[] = HIGHLIGHT_GROUPS.flatMap(({ models }) => models);
const LEGACY_HIGHLIGHTS: ModelHighlights = {
  groups: HIGHLIGHT_GROUPS,
  groupsByModel: new Map(),
  fromCatalog: false
};

// Catalog order is membership metadata, not a capped palette index. Cache it
// since the chart and every row consult it repeatedly for the same catalog.
const HIGHLIGHT_RANKS = new WeakMap<ModelHighlights, ReadonlyMap<string, number>>([
  [LEGACY_HIGHLIGHTS, new Map(LEGACY_MODELS.map((model, index) => [model, index]))]
]);

/** Only the catalog fields that affect Usage. Full SDK catalog responses
 * satisfy this shape. `data` order is the picker's within-group order. */
export type UsageModelCatalog = {
  data: readonly { id: string; group?: string; available_in_agent?: boolean }[];
  groups?: readonly { id: string; label: string }[];
};

export function modelHighlights(catalog?: UsageModelCatalog): ModelHighlights {
  if (!catalog?.groups?.length) return LEGACY_HIGHLIGHTS;
  const labels = new Map(catalog.groups.map(({ id, label }) => [id, label || id]));
  const groups = catalog.groups.map(({ id, label }) => ({
    id,
    label: label || id,
    models: catalog.data
      .filter((model) => model.group === id && model.available_in_agent !== false)
      .map((model) => model.id)
  }));
  return {
    groups,
    groupsByModel: new Map(catalog.data.map((model) => [model.id, labels.get(model.group ?? "")])),
    fromCatalog: true
  };
}

/** Named series before the rest fold. Seven distinct palette colors. */
export const MAX_NAMED_MODELS = 7;

export const MORE_MODELS_KEY = "more";
export const MORE_MODELS_COLOR = "var(--series-other)";

export function highlightGroup(
  model: string,
  highlights: ModelHighlights = LEGACY_HIGHLIGHTS
): string | undefined {
  for (const { label, models } of highlights.groups) {
    if (models.includes(model)) return label;
  }
  if (highlights.fromCatalog) return highlights.groupsByModel.get(model);
  return HIGHLIGHT_GROUPS.find(({ models }) => (models as readonly string[]).includes(model))
    ?.label;
}

/** Catalog membership order, including models beyond the palette capacity. */
export function catalogIndex(
  model: string,
  highlights: ModelHighlights = LEGACY_HIGHLIGHTS
): number | undefined {
  let ranks = HIGHLIGHT_RANKS.get(highlights);
  if (!ranks) {
    const indexed = new Map<string, number>();
    highlights.groups
      .flatMap(({ models }) => models)
      .forEach((id, index) => {
        if (!indexed.has(id)) indexed.set(id, index);
      });
    ranks = indexed;
    HIGHLIGHT_RANKS.set(highlights, ranks);
  }
  const index = ranks.get(model);
  if (index !== undefined || highlights.fromCatalog) return index;
  const legacy = LEGACY_MODELS.indexOf(model);
  return legacy === -1 ? undefined : legacy;
}

/** A fixed palette slot. It is not a catalog membership test. */
export function highlightIndex(
  model: string,
  highlights: ModelHighlights = LEGACY_HIGHLIGHTS
): number | undefined {
  const index = catalogIndex(model, highlights);
  return index !== undefined && index < MAX_NAMED_MODELS ? index : undefined;
}

export function seriesColor(slot: number): string {
  return `var(--series-${slot + 1})`;
}

/** Full model order: served catalog first, then historical IDs. */
export function seriesBefore(left: string, right: string, highlights: ModelHighlights): number {
  const l = catalogIndex(left, highlights);
  const r = catalogIndex(right, highlights);
  if (l !== undefined && r !== undefined) return l - r;
  if (l !== undefined) return -1;
  if (r !== undefined) return 1;
  return left < right ? -1 : left > right ? 1 : 0;
}

/**
 * Color slot for a model drawn with `peers`. Highlights keep a fixed slot.
 * Other peers take the lowest free slot, in id order. Undefined when the
 * model is not highlighted and no free slot is left.
 */
export function modelColorSlot(
  model: string,
  peers: readonly string[],
  highlights: ModelHighlights = LEGACY_HIGHLIGHTS
): number | undefined {
  if (!peers.includes(model)) return undefined;
  const pinned = highlightIndex(model, highlights);
  if (pinned !== undefined) return pinned;
  const taken = new Set<number>();
  for (const peer of peers) {
    const slot = highlightIndex(peer, highlights);
    if (slot !== undefined) taken.add(slot);
  }
  const rank = peers.filter(
    (peer) =>
      highlightIndex(peer, highlights) === undefined && seriesBefore(peer, model, highlights) < 0
  ).length;
  let free = 0;
  for (let slot = 0; slot < MAX_NAMED_MODELS; slot++) {
    if (taken.has(slot)) continue;
    if (free === rank) return slot;
    free += 1;
  }
  return undefined;
}

export function modelColor(
  model: string,
  peers?: readonly string[],
  highlights: ModelHighlights = LEGACY_HIGHLIGHTS
): string {
  if (peers) {
    const slot = modelColorSlot(model, peers, highlights);
    return slot === undefined ? MORE_MODELS_COLOR : seriesColor(slot);
  }
  const pinned = highlightIndex(model, highlights);
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
