import { ChevronDown, Check, Lock, ChevronLeft, Zap, Brain } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
  DropdownMenuSeparator
} from "@/components/ui/dropdown-menu";
import { useBillingState, useModelState } from "@/state/useLocalState";
import { useOpenSecret, type ModelCatalogV2Response } from "@mapleai/sdk";
import {
  canUseModel,
  normalizeModelCatalogV2,
  isCatalogEndpointAbsent,
  planLabel,
  type ModelPlanId
} from "@/services/modelAccess";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { UpgradePromptDialog } from "@/components/UpgradePromptDialog";
import { POWERFUL_MODEL_ALIAS, QUICK_MODEL_ALIAS } from "@/utils/utils";
import type {
  ModelAccessTier,
  OpenSecretModel,
  OpenSecretModelAlias,
  OpenSecretModelCatalog
} from "@/state/LocalStateContextDef";

const PRIMARY_MODELS = [
  {
    id: QUICK_MODEL_ALIAS,
    label: "Quick",
    icon: Zap,
    description: "Fast, everyday responses",
    access: "free" as ModelAccessTier,
    capabilities: { vision: false, reasoning: true }
  },
  {
    id: POWERFUL_MODEL_ALIAS,
    label: "Powerful",
    icon: Brain,
    description: "Deeper thinking & analysis",
    access: "pro" as ModelAccessTier,
    capabilities: { vision: true, reasoning: true }
  }
] as const;

type ModelCatalogClient = {
  fetchModelCatalogV2?: () => Promise<ModelCatalogV2Response>;
  fetchModelCatalog?: () => Promise<OpenSecretModelCatalog>;
  fetchModels?: () => Promise<OpenSecretModel[]>;
};

function isAutoModelAlias(modelId: string): boolean {
  return modelId === QUICK_MODEL_ALIAS || modelId === POWERFUL_MODEL_ALIAS;
}

function isSelectableChatModel(model: OpenSecretModel): boolean {
  return model.enabled !== false && model.deprecated !== true && model.capabilities?.chat !== false;
}

const FALLBACK_ALIAS_TARGETS = {
  [QUICK_MODEL_ALIAS]: "gpt-oss-120b",
  [POWERFUL_MODEL_ALIAS]: "glm-5-3"
} as const;

function buildFallbackModelAliases(models: OpenSecretModel[]): OpenSecretModelAlias[] {
  const modelById = new Map(models.map((availableModel) => [availableModel.id, availableModel]));

  return PRIMARY_MODELS.map((primaryModel) => {
    const targetModel = modelById.get(FALLBACK_ALIAS_TARGETS[primaryModel.id]);

    return {
      id: primaryModel.id,
      label: primaryModel.label,
      short_name: primaryModel.label,
      description: primaryModel.description,
      target_model: targetModel?.id || "",
      access: targetModel?.access || primaryModel.access,
      capabilities: targetModel?.capabilities || primaryModel.capabilities
    };
  });
}

export type ModelSelectorProps = {
  disabled?: boolean;
  model?: string;
  onModelChange?: (modelId: string, metadata?: OpenSecretModel) => void;
};

export function ModelSelector({
  disabled = false,
  model: controlledModel,
  onModelChange
}: ModelSelectorProps = {}) {
  const {
    model: unmanagedModel,
    setModel: setUnmanagedModel,
    availableModels,
    setAvailableModels,
    modelAliases,
    setModelAliases,
    setHasWhisperModel
  } = useModelState();
  const { billingStatus } = useBillingState();
  const os = useOpenSecret();
  const catalogClientRef = useRef(os);
  catalogClientRef.current = os;
  const catalogOwnerId = os.auth.user?.user.id;
  const isManagedSelection = onModelChange != null;
  const model = isManagedSelection ? (controlledModel ?? "") : unmanagedModel;
  const currentModelRef = useRef(model);
  const [upgradeDialogOpen, setUpgradeDialogOpen] = useState(false);
  const [selectedModelName, setSelectedModelName] = useState<string>("");
  const [eligiblePlans, setEligiblePlans] = useState<ModelPlanId[] | undefined>();
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [catalogRefresh, setCatalogRefresh] = useState(0);

  const setModel = useCallback(
    (modelId: string, metadata?: OpenSecretModel) => {
      if (onModelChange) {
        onModelChange(modelId, metadata);
        return;
      }
      setUnmanagedModel(modelId, metadata);
    },
    [onModelChange, setUnmanagedModel]
  );

  useEffect(() => {
    currentModelRef.current = model;
  }, [model]);

  const modelById = useMemo(() => {
    return new Map(availableModels.map((availableModel) => [availableModel.id, availableModel]));
  }, [availableModels]);

  const aliasById = useMemo(() => {
    return new Map(modelAliases.map((alias) => [alias.id, alias]));
  }, [modelAliases]);

  const reconcileSelectedConcreteModel = useCallback(
    (models: OpenSecretModel[]) => {
      const currentModel = currentModelRef.current;
      if (!currentModel || isAutoModelAlias(currentModel)) return;

      const selectedModel = models.find((availableModel) => availableModel.id === currentModel);
      if (selectedModel) {
        setModel(currentModel, selectedModel);
      } else {
        setModel(QUICK_MODEL_ALIAS);
      }
    },
    [setModel]
  );

  useEffect(() => {
    if (isManagedSelection) return;
    let current = true;
    setAvailableModels([]);
    setModelAliases([]);
    setUpgradeDialogOpen(false);
    const publish = (catalog: OpenSecretModelCatalog) => {
      if (!current) return;
      const models = catalog.data.filter(isSelectableChatModel);
      setAvailableModels(models);
      setModelAliases(catalog.aliases);
      setHasWhisperModel(
        catalog.audio?.transcription?.available ??
          catalog.data.some((model) => model.id === "whisper-large-v3")
      );
      reconcileSelectedConcreteModel(models);
    };
    const fetchCatalog = async () => {
      const client = catalogClientRef.current as unknown as ModelCatalogClient;
      if (client.fetchModelCatalogV2) {
        try {
          publish(normalizeModelCatalogV2(await client.fetchModelCatalogV2()));
          return;
        } catch (error) {
          if (!isCatalogEndpointAbsent(error)) throw error;
        }
      }
      if (client.fetchModelCatalog) {
        try {
          publish(await client.fetchModelCatalog());
          return;
        } catch (error) {
          if (!isCatalogEndpointAbsent(error)) throw error;
        }
      }
      if (client.fetchModels) {
        const models = await client.fetchModels();
        const generate = models.filter((model) =>
          model.tasks?.length ? model.tasks.includes("generate") : !/whisper|embed/i.test(model.id)
        );
        publish({
          object: "list",
          data: generate,
          aliases: buildFallbackModelAliases(generate),
          audio: {
            transcription: {
              available: models.some((model) => model.id === "whisper-large-v3"),
              model: "whisper-large-v3"
            }
          }
        });
      }
    };
    void fetchCatalog().catch((error: unknown) => {
      if (current && import.meta.env.DEV) console.warn("Failed to fetch model catalog:", error);
    });
    return () => {
      current = false;
    };
  }, [
    catalogOwnerId,
    billingStatus?.product_name,
    isManagedSelection,
    catalogRefresh,
    setAvailableModels,
    setModelAliases,
    setHasWhisperModel,
    reconcileSelectedConcreteModel
  ]);

  const getAlias = useCallback(
    (modelId: string): OpenSecretModelAlias | undefined => {
      const alias = aliasById.get(modelId as OpenSecretModelAlias["id"]);
      if (alias) return alias;

      const fallback = PRIMARY_MODELS.find((primaryModel) => primaryModel.id === modelId);
      if (!fallback) return undefined;

      return {
        id: fallback.id,
        label: fallback.label,
        short_name: fallback.label,
        description: fallback.description,
        target_model: "",
        access: fallback.access,
        capabilities: fallback.capabilities
      };
    },
    [aliasById]
  );

  const getTargetModel = useCallback(
    (alias: OpenSecretModelAlias | undefined) => {
      if (!alias?.target_model) return undefined;
      return modelById.get(alias.target_model);
    },
    [modelById]
  );

  const getModelAccess = useCallback(
    (modelId: string) => {
      const alias = getAlias(modelId);
      return alias ? (getTargetModel(alias) ?? alias) : modelById.get(modelId);
    },
    [getAlias, getTargetModel, modelById]
  );

  const hasAccessToModel = useCallback(
    (modelId: string) => canUseModel(getModelAccess(modelId), billingStatus?.product_name),
    [getModelAccess, billingStatus?.product_name]
  );

  const openModelUpgrade = (modelId: string, name: string) => {
    setSelectedModelName(name);
    setEligiblePlans(getModelAccess(modelId)?.accessPolicy?.eligible_plans);
    setUpgradeDialogOpen(true);
  };

  const getDisplayLabel = (modelId: string): string => {
    const alias = getAlias(modelId);
    if (alias) return alias.short_name || alias.label;

    const selectedModel = modelById.get(modelId);
    return selectedModel?.short_name || selectedModel?.display_name || modelId;
  };

  const getDisplayNameText = (modelId: string): string => {
    const alias = getAlias(modelId);
    if (alias) return alias.label;

    const selectedModel = modelById.get(modelId);
    return selectedModel?.display_name || selectedModel?.short_name || modelId;
  };

  // Handle primary option selection
  const handlePrimarySelect = (targetModel: string) => {
    if (!hasAccessToModel(targetModel)) {
      openModelUpgrade(targetModel, getDisplayNameText(targetModel));
      return;
    }

    setModel(targetModel);
  };

  // Get dynamic badges for a model based on billing status
  const getModelBadges = (modelId: string): string[] => {
    const selectedModel = modelById.get(modelId);
    const badges = selectedModel?.badges || [];
    return badges.filter(
      (badge) =>
        badge !== "Pro" &&
        (selectedModel?.access === "free" || badge.toLowerCase() !== selectedModel?.access)
    );
  };

  const getDisplayName = (modelId: string, showLock = false) => {
    const selectedModel = modelById.get(modelId);
    const elements: React.ReactNode[] = [];

    if (selectedModel) {
      elements.push(selectedModel.display_name || selectedModel.short_name || modelId);

      const badges = getModelBadges(modelId);
      if (badges && badges.length > 0) {
        badges.forEach((badge, index) => {
          let badgeClass = "rounded-md px-1.5 py-0.5 text-[10px] font-medium";

          if (badge === "Coming Soon") {
            badgeClass += " bg-muted text-muted-foreground";
          } else if (badge === "Pro") {
            badgeClass +=
              " bg-gradient-to-r from-[hsl(var(--maple-primary))]/10 to-[hsl(var(--maple-tertiary))]/10 text-[hsl(var(--maple-primary))]";
          } else if (badge === "New") {
            badgeClass += " bg-maple-info/10 text-maple-info";
          } else if (badge === "Reasoning") {
            badgeClass += " bg-maple-error/10 text-maple-error";
          } else if (badge === "Beta") {
            badgeClass += " bg-maple-warning/10 text-maple-warning";
          } else {
            badgeClass += " bg-[hsl(var(--maple-primary))]/10 text-[hsl(var(--maple-primary))]";
          }

          elements.push(
            <span key={`badge-${index}`} className={badgeClass}>
              {badge}
            </span>
          );
        });
      }

      if (showLock && !hasAccessToModel(modelId)) {
        elements.push(<Lock key="lock" className="h-3 w-3 opacity-50" />);
      }
    } else {
      elements.push(getDisplayNameText(modelId));
    }

    return <span className="flex items-center gap-1">{elements}</span>;
  };

  // Show current category or model name in the collapsed view
  const modelDisplay = (
    <div className="flex items-center gap-1">
      <div className="text-xs font-medium">{getDisplayLabel(model)}</div>
    </div>
  );

  // Always show dropdown even with single model (it may be loading more)

  return (
    <>
      <DropdownMenu
        modal={false}
        onOpenChange={(open) => {
          if (open && !isManagedSelection) setCatalogRefresh((value) => value + 1);
        }}
      >
        <DropdownMenuTrigger asChild>
          <Button
            disabled={disabled}
            variant="ghost"
            size="sm"
            className="h-8 gap-1 px-2 text-[hsl(var(--maple-secondary-700))] hover:bg-[hsl(var(--maple-primary-container))] hover:text-[hsl(var(--maple-secondary-700))]"
            data-testid="model-selector-button"
            aria-label={`Current model: ${getDisplayNameText(model)}. Click to change model.`}
          >
            {modelDisplay}
            <ChevronDown className="h-3 w-3" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" className="w-64 p-0">
          {!showAdvanced ? (
            <div className="p-1 flex flex-col">
              {/* Primary options */}
              {PRIMARY_MODELS.map((primaryModel) => {
                const alias = getAlias(primaryModel.id);
                const Icon = primaryModel.icon;
                const targetModel = primaryModel.id;
                const isActive = model === targetModel;
                const hasAccess = hasAccessToModel(targetModel);
                const requiresUpgrade = !hasAccess;

                return (
                  <DropdownMenuItem
                    key={targetModel}
                    onClick={() => handlePrimarySelect(targetModel)}
                    className={`flex items-center gap-2 px-3 py-1.5 cursor-pointer ${
                      requiresUpgrade
                        ? "hover:bg-[hsl(var(--maple-primary-container))] dark:hover:bg-[hsl(var(--maple-primary))]/10"
                        : ""
                    }`}
                  >
                    <Icon className="h-4 w-4 opacity-70" />
                    <div className="flex-1">
                      <div className="flex items-center gap-1.5">
                        <span className="text-sm font-medium">
                          {alias?.label || primaryModel.label}
                        </span>
                        {requiresUpgrade && <Lock className="h-3 w-3 opacity-50" />}
                      </div>
                      <div className="text-xs text-muted-foreground">
                        {alias?.description || primaryModel.description}
                      </div>
                    </div>
                    {isActive && <Check className="h-4 w-4" />}
                  </DropdownMenuItem>
                );
              })}

              <DropdownMenuSeparator />

              {/* More models option */}
              <DropdownMenuItem
                onSelect={(e) => {
                  e.preventDefault();
                  setShowAdvanced(true);
                }}
                className="flex items-center gap-2 px-3 py-1.5 cursor-pointer"
              >
                <ChevronLeft className="h-4 w-4 opacity-70 rotate-180" />
                <div className="flex-1">
                  <span className="text-sm font-medium">More models</span>
                  <div className="text-xs text-muted-foreground">All models</div>
                </div>
              </DropdownMenuItem>
            </div>
          ) : (
            <div className="p-1 flex flex-col">
              {/* Back button */}
              <DropdownMenuItem
                onSelect={(e) => {
                  e.preventDefault();
                  setShowAdvanced(false);
                }}
                className="flex items-center gap-2 px-3 py-1.5 cursor-pointer mb-1"
              >
                <ChevronLeft className="h-4 w-4" />
                <span className="text-sm font-medium">Back</span>
              </DropdownMenuItem>

              <DropdownMenuSeparator />

              {/* Scrollable model list */}
              <div className="overflow-y-auto flex-1">
                {availableModels.length === 0 ? (
                  <DropdownMenuItem disabled className="px-3 py-2 text-sm text-muted-foreground">
                    Loading models...
                  </DropdownMenuItem>
                ) : (
                  [...availableModels]
                    .filter(isSelectableChatModel)
                    // Remove duplicates by id
                    .filter(
                      (m, index, self) => self.findIndex((model) => model.id === m.id) === index
                    )
                    .sort((a, b) => {
                      const aDisabled = a.enabled === false;
                      const bDisabled = b.enabled === false;
                      const aRestricted = !hasAccessToModel(a.id);
                      const bRestricted = !hasAccessToModel(b.id);

                      // Disabled models go last
                      if (aDisabled && !bDisabled) return 1;
                      if (!aDisabled && bDisabled) return -1;

                      // Restricted models go after available but before disabled
                      if (aRestricted && !bRestricted) return 1;
                      if (!aRestricted && bRestricted) return -1;

                      return (a.sort_order ?? 999) - (b.sort_order ?? 999);
                    })
                    .map((availableModel) => {
                      const isDisabled = availableModel.enabled === false;
                      const hasAccess = hasAccessToModel(availableModel.id);
                      const isRestricted = !hasAccess;

                      const effectivelyDisabled = isDisabled;
                      const selectedAliasTarget = getAlias(model)?.target_model;
                      const isActive =
                        model === availableModel.id || selectedAliasTarget === availableModel.id;

                      return (
                        <DropdownMenuItem
                          key={`advanced-${availableModel.id}`}
                          onClick={() => {
                            if (effectivelyDisabled) return;
                            if (isRestricted) {
                              openModelUpgrade(
                                availableModel.id,
                                availableModel.display_name || availableModel.id
                              );
                            } else {
                              setModel(availableModel.id, availableModel);
                              setShowAdvanced(false);
                            }
                          }}
                          className={`flex items-center justify-between group ${
                            effectivelyDisabled ? "opacity-50 cursor-not-allowed" : ""
                          } ${
                            isRestricted
                              ? "hover:bg-[hsl(var(--maple-primary-container))] dark:hover:bg-[hsl(var(--maple-primary))]/10"
                              : ""
                          }`}
                          disabled={effectivelyDisabled}
                        >
                          <div className="flex items-center gap-2 flex-1">
                            <div>
                              <div className="text-sm">
                                {getDisplayName(availableModel.id, true)}
                              </div>
                              {isRestricted && availableModel.accessPolicy && (
                                <div className="text-xs text-muted-foreground">
                                  Requires {planLabel(availableModel.accessPolicy.eligible_plans)}
                                </div>
                              )}
                            </div>
                          </div>
                          {isActive && <Check className="h-4 w-4" />}
                        </DropdownMenuItem>
                      );
                    })
                )}
              </div>
            </div>
          )}
        </DropdownMenuContent>
      </DropdownMenu>

      <UpgradePromptDialog
        open={upgradeDialogOpen}
        onOpenChange={setUpgradeDialogOpen}
        feature="model"
        modelName={selectedModelName}
        eligiblePlans={eligiblePlans}
      />
    </>
  );
}
