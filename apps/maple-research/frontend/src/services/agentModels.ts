import { POWERFUL_MODEL_ALIAS, QUICK_MODEL_ALIAS } from "@/utils/utils";

export const DEFAULT_AGENT_MODEL = QUICK_MODEL_ALIAS;
export const PRIMARY_AGENT_MODEL_IDS = [QUICK_MODEL_ALIAS, POWERFUL_MODEL_ALIAS] as const;
export const PREVIOUS_RECOMMENDED_AGENT_MODEL = "glm-5-2";

type AgentModelReference = {
  id: string;
  capabilities?: {
    vision?: boolean;
  };
};

type AgentModelAliasReference = {
  id: string;
  target_model?: string;
  capabilities?: {
    vision?: boolean;
  };
};

type AgentModelContextReference = {
  id: string;
  context_window?: unknown;
  max_context_tokens?: unknown;
};

type AgentModelContextCatalogReference = {
  data: AgentModelContextReference[];
  aliases: Array<{
    id: string;
    target_model?: string;
  }>;
};

export function fallbackAgentModel(): string {
  return DEFAULT_AGENT_MODEL;
}

export function migrateAgentModelPreference(
  currentModel: string | null | undefined
): string | null {
  const model = currentModel?.trim() || null;
  if (!model || model === PREVIOUS_RECOMMENDED_AGENT_MODEL || model === "deepseek-v4-flash") {
    return null;
  }
  return model;
}

export function reconcileAgentModel(currentModel: string, models: AgentModelReference[]): string {
  if (!currentModel) return fallbackAgentModel();
  if (currentModel === QUICK_MODEL_ALIAS || currentModel === POWERFUL_MODEL_ALIAS) {
    return currentModel;
  }
  if (models.some((model) => model.id === currentModel)) return currentModel;
  return fallbackAgentModel();
}

export function reconcileAgentModelForCatalog(
  currentModel: string,
  models: AgentModelReference[],
  isModelLocked: boolean
): string {
  return isModelLocked ? currentModel : reconcileAgentModel(currentModel, models);
}

export function resolveAgentModelForSession(
  newTaskModel: string,
  sessionModel: string | null | undefined,
  isLocked: boolean
): string {
  return isLocked && sessionModel ? sessionModel : newTaskModel;
}

export function resolveAgentModelVisionCapability(
  modelId: string,
  models: AgentModelReference[],
  aliases: AgentModelAliasReference[]
): boolean {
  const alias = aliases.find((candidate) => candidate.id === modelId);
  if (alias) {
    const target = alias.target_model
      ? models.find((candidate) => candidate.id === alias.target_model)
      : undefined;
    return target?.capabilities?.vision ?? alias.capabilities?.vision ?? false;
  }

  return models.find((candidate) => candidate.id === modelId)?.capabilities?.vision ?? false;
}

function isValidContextLimit(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0;
}

function contextLimitFromModel(model: AgentModelContextReference | undefined): number | undefined {
  if (!model) return undefined;

  const contextWindow = model.context_window;
  const maxContextTokens = model.max_context_tokens;
  const hasContextWindow = contextWindow !== undefined;
  const hasMaxContextTokens = maxContextTokens !== undefined;

  if (!hasContextWindow && !hasMaxContextTokens) return undefined;
  if (hasContextWindow && !isValidContextLimit(contextWindow)) return undefined;
  if (hasMaxContextTokens && !isValidContextLimit(maxContextTokens)) return undefined;
  if (hasContextWindow && hasMaxContextTokens && contextWindow !== maxContextTokens) {
    return undefined;
  }

  return isValidContextLimit(contextWindow)
    ? contextWindow
    : isValidContextLimit(maxContextTokens)
      ? maxContextTokens
      : undefined;
}

export function resolveAgentModelContextLimit(
  modelId: string,
  catalog: AgentModelContextCatalogReference | null
): number | undefined {
  if (!catalog) return undefined;

  const alias = catalog.aliases.find((candidate) => candidate.id === modelId);
  const concreteModelId = alias?.target_model || modelId;
  return contextLimitFromModel(catalog.data.find((candidate) => candidate.id === concreteModelId));
}
