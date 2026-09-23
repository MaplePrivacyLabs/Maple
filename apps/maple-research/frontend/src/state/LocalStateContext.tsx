import { useCallback, useMemo, useRef, useState } from "react";
import { BillingStatus } from "@/billing/billingApi";
import {
  BillingStateContext,
  ModelStateContext,
  OpenSecretModel,
  OpenSecretModelAlias,
  SelectedProjectStateContext,
  SidebarSearchStateContext,
  type BillingState,
  type ModelState,
  type SelectedProjectState,
  type SidebarSearchState
} from "./LocalStateContextDef";
import { aliasModelName, migrateStickyModelName } from "@/utils/utils";

export const QUICK_MODEL_ALIAS = "auto:quick";
export const POWERFUL_MODEL_ALIAS = "auto:powerful";
export const DEFAULT_MODEL_ID = QUICK_MODEL_ALIAS;
export const SELECTED_MODEL_RESET_AT_KEY = "selectedModelResetAt";
const SELECTED_MODEL_METADATA_KEY = "selectedModelMetadata";
type LocalStateStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

function getBrowserStorage(): LocalStateStorage | null {
  if (typeof window === "undefined") return null;

  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

const DEFAULT_MODEL_ALIASES: OpenSecretModelAlias[] = [
  {
    id: QUICK_MODEL_ALIAS,
    label: "Quick",
    short_name: "Quick",
    description: "Fast, everyday responses",
    target_model: "",
    access: "free",
    capabilities: { chat: true, vision: false, reasoning: true, tool_use: true }
  },
  {
    id: POWERFUL_MODEL_ALIAS,
    label: "Powerful",
    short_name: "Powerful",
    description: "Deeper thinking & analysis",
    target_model: "",
    access: "pro",
    capabilities: { chat: true, vision: true, reasoning: true, tool_use: true }
  }
];

// One-time migration: clear stale webSearchEnabled values that were auto-persisted
// by the old useEffect (not explicit user choices). After this migration, only values
// written by explicit user toggle clicks are trusted.
function migrateWebSearchDefault(storage: LocalStateStorage | null): void {
  if (!storage) return;

  try {
    if (storage.getItem("webSearchDefaultMigrated") === null) {
      storage.removeItem("webSearchEnabled");
      storage.setItem("webSearchDefaultMigrated", "true");
    }
  } catch {
    // Ignore storage errors
  }
}

// Helper to get the initial web search state from localStorage.
// Web search is on by default for all users, but respects the user's explicit preference.
export function getInitialWebSearchEnabled(
  storage: LocalStateStorage | null = getBrowserStorage()
): boolean {
  migrateWebSearchDefault(storage);
  try {
    // If user has explicitly toggled web search before, respect that
    const webSearchSetting = storage?.getItem("webSearchEnabled");
    if (webSearchSetting != null) {
      return webSearchSetting === "true";
    }
  } catch (error) {
    console.error("Failed to get initial web search state:", error);
  }
  // Default to enabled for all users
  return true;
}

// One-time reset: clear any previously stickied model so Quick becomes the
// shared default. Presence of the timestamp is the boolean gate; the ISO
// value is only diagnostic. A later reset should use a new key.
function resetSelectedModelOnce(storage: LocalStateStorage | null): void {
  if (!storage) return;

  try {
    if (storage.getItem(SELECTED_MODEL_RESET_AT_KEY) != null) return;

    storage.removeItem("selectedModel");
    storage.removeItem(SELECTED_MODEL_METADATA_KEY);
    storage.setItem(SELECTED_MODEL_RESET_AT_KEY, new Date().toISOString());
  } catch {
    // Ignore storage errors
  }
}

// Helper to get the initial model from an explicit sticky choice or the shared default.
function getInitialModel(storage: LocalStateStorage | null): string {
  resetSelectedModelOnce(storage);

  // Check for dev override first
  if (import.meta.env.VITE_DEV_MODEL_OVERRIDE) {
    return aliasModelName(import.meta.env.VITE_DEV_MODEL_OVERRIDE);
  }

  try {
    const selectedModel = storage?.getItem("selectedModel");
    if (selectedModel) {
      if (getCachedSelectedModelMetadata(selectedModel, storage)) {
        return selectedModel;
      }

      return migrateStickyModelName(selectedModel);
    }
  } catch (error) {
    console.error("Failed to load initial model:", error);
  }

  return DEFAULT_MODEL_ID;
}

function normalizeAvailableModels(models: OpenSecretModel[]): OpenSecretModel[] {
  const normalizedModels = new Map<string, OpenSecretModel>();

  for (const model of models) {
    if (!normalizedModels.has(model.id)) {
      normalizedModels.set(model.id, model);
    }
  }

  return Array.from(normalizedModels.values());
}

function isAutoModelAlias(modelId: string): boolean {
  return modelId === QUICK_MODEL_ALIAS || modelId === POWERFUL_MODEL_ALIAS;
}

function getCachedSelectedModelMetadata(
  modelId: string,
  storage: LocalStateStorage | null
): OpenSecretModel | null {
  if (!modelId || isAutoModelAlias(modelId)) return null;

  try {
    const cachedMetadata = storage?.getItem(SELECTED_MODEL_METADATA_KEY);
    if (!cachedMetadata) return null;

    const parsedMetadata = JSON.parse(cachedMetadata) as OpenSecretModel;
    if (parsedMetadata.id !== modelId) return null;

    return {
      ...parsedMetadata,
      object: "model",
      created: parsedMetadata.created || Date.now(),
      owned_by: parsedMetadata.owned_by || "opensecret"
    };
  } catch (error) {
    console.error("Failed to load selected model metadata:", error);
    return null;
  }
}

function cacheSelectedModelMetadata(
  modelId: string,
  storage: LocalStateStorage | null,
  modelMetadata?: OpenSecretModel | null
) {
  if (!storage) return;

  try {
    if (!modelMetadata || isAutoModelAlias(modelId)) {
      storage.removeItem(SELECTED_MODEL_METADATA_KEY);
      return;
    }

    const cacheableMetadata: OpenSecretModel = {
      ...modelMetadata,
      id: modelId,
      object: "model",
      created: modelMetadata.created || Date.now(),
      owned_by: modelMetadata.owned_by || "opensecret"
    };

    storage.setItem(SELECTED_MODEL_METADATA_KEY, JSON.stringify(cacheableMetadata));
  } catch (error) {
    console.error("Failed to cache selected model metadata:", error);
  }
}

export const LocalStateProvider = ({
  children,
  storage = getBrowserStorage()
}: {
  children: React.ReactNode;
  storage?: LocalStateStorage | null;
}) => {
  const [modelState, setModelState] = useState(() => {
    const model = getInitialModel(storage);
    const cachedSelectedModel = getCachedSelectedModelMetadata(model, storage);

    return {
      model,
      availableModels: cachedSelectedModel ? [cachedSelectedModel] : ([] as OpenSecretModel[]),
      modelAliases: DEFAULT_MODEL_ALIASES,
      hasWhisperModel: true // Default to true to avoid hiding button during loading
    };
  });
  const [billingStatus, setBillingStatusState] = useState<BillingStatus | null>(null);
  const [searchQuery, setSearchQueryState] = useState("");
  const [isSearchVisible, setIsSearchVisibleState] = useState(false);
  const [selectedProjectId, setSelectedProjectIdState] = useState<string | null>(null);
  const currentModelRef = useRef(modelState.model);

  const setBillingStatus = useCallback((status: BillingStatus | null) => {
    setBillingStatusState(status);
  }, []);

  const setSearchQuery = useCallback((query: string) => setSearchQueryState(query), []);

  const setIsSearchVisible = useCallback(
    (visible: boolean) => setIsSearchVisibleState(visible),
    []
  );

  const setSelectedProjectId = useCallback((projectId: string | null) => {
    setSelectedProjectIdState(projectId);
  }, []);

  // Public model setter — records the choice as a user-initiated selection.
  // After this, we won't auto-override their model choice.
  const setModel = useCallback(
    (model: string, modelMetadata?: OpenSecretModel | null) => {
      const nextModel = modelMetadata ? model : aliasModelName(model);
      if (currentModelRef.current === nextModel && !modelMetadata) return;
      currentModelRef.current = nextModel;

      // Save to localStorage as user's explicit choice. Keep this outside the
      // state updater because React may replay updater functions.
      try {
        storage?.setItem("selectedModel", nextModel);
        cacheSelectedModelMetadata(nextModel, storage, modelMetadata);
      } catch (error) {
        console.error("Failed to save model to localStorage:", error);
      }

      setModelState((prev) => {
        const availableModels =
          modelMetadata && !isAutoModelAlias(nextModel)
            ? normalizeAvailableModels([modelMetadata, ...prev.availableModels])
            : prev.availableModels;

        return { ...prev, model: nextModel, availableModels };
      });
    },
    [storage]
  );

  const setAvailableModels = useCallback((models: OpenSecretModel[]) => {
    setModelState((prev) => ({
      ...prev,
      availableModels: normalizeAvailableModels(models)
    }));
  }, []);

  const setModelAliases = useCallback((aliases: OpenSecretModelAlias[]) => {
    setModelState((prev) => ({
      ...prev,
      modelAliases: aliases.length > 0 ? aliases : DEFAULT_MODEL_ALIASES
    }));
  }, []);

  const setHasWhisperModel = useCallback((hasWhisper: boolean) => {
    setModelState((prev) => ({ ...prev, hasWhisperModel: hasWhisper }));
  }, []);

  const modelValue = useMemo<ModelState>(
    () => ({
      model: modelState.model,
      availableModels: modelState.availableModels,
      modelAliases: modelState.modelAliases,
      setModel,
      setAvailableModels,
      setModelAliases,
      hasWhisperModel: modelState.hasWhisperModel,
      setHasWhisperModel
    }),
    [modelState, setAvailableModels, setHasWhisperModel, setModel, setModelAliases]
  );
  const billingValue = useMemo<BillingState>(
    () => ({ billingStatus, setBillingStatus }),
    [billingStatus, setBillingStatus]
  );
  const sidebarSearchValue = useMemo<SidebarSearchState>(
    () => ({ searchQuery, setSearchQuery, isSearchVisible, setIsSearchVisible }),
    [isSearchVisible, searchQuery, setIsSearchVisible, setSearchQuery]
  );
  const selectedProjectValue = useMemo<SelectedProjectState>(
    () => ({ selectedProjectId, setSelectedProjectId }),
    [selectedProjectId, setSelectedProjectId]
  );

  return (
    <ModelStateContext.Provider value={modelValue}>
      <BillingStateContext.Provider value={billingValue}>
        <SidebarSearchStateContext.Provider value={sidebarSearchValue}>
          <SelectedProjectStateContext.Provider value={selectedProjectValue}>
            {children}
          </SelectedProjectStateContext.Provider>
        </SidebarSearchStateContext.Provider>
      </BillingStateContext.Provider>
    </ModelStateContext.Provider>
  );
};
