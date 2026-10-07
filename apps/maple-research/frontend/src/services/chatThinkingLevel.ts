import type {
  ModelReasoning,
  OpenSecretModel,
  OpenSecretModelAlias,
  ReasoningEffort
} from "@/state/LocalStateContextDef";

/**
 * The composer's thinking choice: let the model decide, or one effort the
 * selected model's catalog entry accepts.
 */
export type ThinkingLevelChoice = "default" | ReasoningEffort;

export type ThinkingLevelOption = {
  value: ThinkingLevelChoice;
  label: string;
  description?: string;
};

export const THINKING_LEVEL_STORAGE_KEY = "chatThinkingLevel";

const EFFORT_ORDER: ReasoningEffort[] = [
  "none",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max"
];

const EFFORT_LABELS: Record<ReasoningEffort, string> = {
  none: "Off",
  minimal: "Minimal",
  low: "Low",
  medium: "Medium",
  high: "High",
  xhigh: "Extra high",
  max: "Max"
};

export function isReasoningEffort(value: unknown): value is ReasoningEffort {
  return typeof value === "string" && (EFFORT_ORDER as string[]).includes(value);
}

export function isThinkingLevelChoice(value: unknown): value is ThinkingLevelChoice {
  return value === "default" || isReasoningEffort(value);
}

export function thinkingLevelLabel(choice: ThinkingLevelChoice): string {
  return choice === "default" ? "Default" : EFFORT_LABELS[choice];
}

/**
 * The reasoning controls published for the selected model or alias, or
 * `undefined` while the catalog has not loaded or the model is unknown.
 */
export function resolveModelReasoning(
  modelId: string,
  models: readonly OpenSecretModel[],
  aliases: readonly OpenSecretModelAlias[]
): ModelReasoning | null | undefined {
  const alias = aliases.find((candidate) => candidate.id === modelId);
  if (alias) return alias.reasoning ?? (alias.reasoning === null ? null : undefined);
  const model = models.find((candidate) => candidate.id === modelId);
  if (!model) return undefined;
  return model.reasoning ?? (model.reasoning === null ? null : undefined);
}

/**
 * The choices the composer offers for a model: the model's default, `Off`
 * when reasoning is not mandatory, then every supported effort from lowest to
 * highest. Empty when the model never reasons, so the control stays hidden.
 */
export function thinkingLevelOptions(
  reasoning: ModelReasoning | null | undefined
): ThinkingLevelOption[] {
  if (!reasoning) return [];
  const options: ThinkingLevelOption[] = [
    {
      value: "default",
      label: "Default",
      description: reasoning.default_effort
        ? `Model default (${EFFORT_LABELS[reasoning.default_effort].toLowerCase()})`
        : undefined
    }
  ];
  if (!reasoning.mandatory) {
    options.push({ value: "none", label: EFFORT_LABELS.none });
  }
  const supported = reasoning.supported_efforts ?? [];
  for (const effort of EFFORT_ORDER) {
    if (effort !== "none" && supported.includes(effort)) {
      options.push({ value: effort, label: EFFORT_LABELS[effort] });
    }
  }
  return options;
}

export type ResolvedThinkingLevel = {
  options: ThinkingLevelOption[];
  /** The stored choice when the model offers it, otherwise `default`. */
  effective: ThinkingLevelChoice;
  /** The `reasoning.effort` to send, or `undefined` to let the model decide. */
  effort: ReasoningEffort | undefined;
};

/**
 * Resolve a stored choice against the selected model. A choice the model does
 * not offer falls back to the model's default, so switching models never
 * sends a level the catalog did not publish.
 */
export function resolveThinkingLevel(
  choice: ThinkingLevelChoice,
  reasoning: ModelReasoning | null | undefined
): ResolvedThinkingLevel {
  const options = thinkingLevelOptions(reasoning);
  const effective = options.some((option) => option.value === choice) ? choice : "default";
  return {
    options,
    effective,
    effort: effective === "default" ? undefined : effective
  };
}

type ThinkingLevelStorage = Pick<Storage, "getItem" | "setItem">;

function browserStorage(): ThinkingLevelStorage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}

export function getStoredThinkingLevel(
  storage: ThinkingLevelStorage | null = browserStorage()
): ThinkingLevelChoice {
  try {
    const stored = storage?.getItem(THINKING_LEVEL_STORAGE_KEY);
    return isThinkingLevelChoice(stored) ? stored : "default";
  } catch {
    return "default";
  }
}

export function storeThinkingLevel(
  choice: ThinkingLevelChoice,
  storage: ThinkingLevelStorage | null = browserStorage()
): void {
  try {
    storage?.setItem(THINKING_LEVEL_STORAGE_KEY, choice);
  } catch {
    // Storage is a convenience; the in-memory choice still applies.
  }
}
