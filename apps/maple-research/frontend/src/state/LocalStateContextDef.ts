import { createContext } from "react";
import { BillingStatus } from "@/billing/billingApi";
import type { Model } from "openai/resources/models.js";

// Extended Model type for OpenSecret API which includes additional properties
export type ModelAccessTier = "free" | "pro";

export type ModelCapabilities = {
  chat?: boolean;
  vision?: boolean;
  reasoning?: boolean;
  tool_use?: boolean;
};

export interface OpenSecretModel extends Model {
  tasks?: string[];
  provider?: string;
  provider_id?: string;
  display_name?: string;
  short_name?: string;
  description?: string;
  context_window?: number;
  max_context_tokens?: number;
  access?: ModelAccessTier;
  capabilities?: ModelCapabilities;
  badges?: string[];
  enabled?: boolean;
  deprecated?: boolean;
  sort_order?: number;
}

export type OpenSecretModelAlias = {
  id: "auto:quick" | "auto:powerful";
  label: string;
  short_name: string;
  description: string;
  target_model: string;
  access?: ModelAccessTier;
  capabilities?: ModelCapabilities;
};

export type OpenSecretModelCatalog = {
  object: "list";
  data: OpenSecretModel[];
  aliases: OpenSecretModelAlias[];
  defaults?: {
    quick: "auto:quick";
    powerful: "auto:powerful";
  };
  audio?: {
    transcription?: {
      available: boolean;
      model: string;
      display_name?: string;
    };
    speech?: {
      available: boolean;
      model: string;
      display_name?: string;
    };
  };
};

export type ModelState = {
  model: string;
  availableModels: OpenSecretModel[];
  modelAliases: OpenSecretModelAlias[];
  setModel: (model: string, modelMetadata?: OpenSecretModel | null) => void;
  setAvailableModels: (models: OpenSecretModel[]) => void;
  setModelAliases: (aliases: OpenSecretModelAlias[]) => void;
  /** Whether the whisper transcription model is available */
  hasWhisperModel: boolean;
  setHasWhisperModel: (hasWhisper: boolean) => void;
};

export type BillingState = {
  billingStatus: BillingStatus | null;
  setBillingStatus: (status: BillingStatus | null) => void;
};

export type SidebarSearchState = {
  /** Current search query for filtering chat history */
  searchQuery: string;
  /** Updates the current search query */
  setSearchQuery: (query: string) => void;
  /** Whether the search input is currently visible */
  isSearchVisible: boolean;
  /** Controls the visibility of the search input */
  setIsSearchVisible: (visible: boolean) => void;
};

export type SelectedProjectState = {
  /** Currently selected conversation project for sidebar/composer context */
  selectedProjectId: string | null;
  /** Updates the selected conversation project context */
  setSelectedProjectId: (projectId: string | null) => void;
};

export const ModelStateContext = createContext<ModelState | undefined>(undefined);
export const BillingStateContext = createContext<BillingState | undefined>(undefined);
export const SidebarSearchStateContext = createContext<SidebarSearchState | undefined>(undefined);
export const SelectedProjectStateContext = createContext<SelectedProjectState | undefined>(
  undefined
);
