// Export types from api
export type {
  KVListItem,
  LoginResponse,
  UserResponse,
  GithubAuthResponse,
  GoogleAuthResponse,
  NativeHandoffGrantResponse,
  DocumentResponse,
  DocumentUploadInitResponse,
  DocumentStatusRequest,
  DocumentStatusResponse,
  ApiKey,
  ApiKeyCreateResponse,
  ApiKeyListResponse,
  ResponsesRetrieveResponse,
  ResponsesListResponse,
  ResponsesListParams,
  ResponsesCancelResponse,
  ResponsesDeleteResponse,
  ResponsesCreateRequest,
  ModelAliasId,
  ModelId,
  ModelAccessTier,
  ModelCapabilities,
  ModelCatalogItem,
  ModelAlias,
  ModelCatalogResponse,
  Conversation,
  ConversationItem,
  ConversationCreateOptions,
  ConversationCreateRequest,
  ConversationUpdateOptions,
  ConversationUpdateRequest,
  ConversationItemsResponse,
  ConversationsListResponse,
  ConversationsListParams,
  ConversationDeleteResponse,
  ConversationsDeleteResponse,
  BatchDeleteConversationsRequest,
  BatchDeleteItemResult,
  BatchDeleteConversationsResponse,
  BatchUpdateConversationProjectRequest,
  BatchUpdateConversationProjectResponse,
  ConversationProject,
  ConversationProjectListItem,
  ConversationProjectsListResponse,
  ConversationProjectCreateRequest,
  ConversationProjectUpdateRequest,
  ConversationProjectListParams,
  ConversationProjectDeleteResponse,
  WebSearchWorkflow,
  WebSearchTimeRelative,
  WebSearchLens,
  WebSearchFilters,
  WebSearchRequest,
  WebSearchResult,
  WebSearchResponse,
  WebExtractRequest,
  WebExtractPageError,
  WebExtractPage,
  WebExtractResponse,
  AgentCreatedBy,
  MainAgentResponse,
  CreateSubagentRequest,
  AgentItemsListParams,
  ListSubagentsParams,
  SubagentResponse,
  SubagentListResponse,
  AgentDeletedObjectResponse,
  AgentMessageEvent,
  AgentDoneEvent,
  AgentErrorEvent
} from "./api";

// Export API key management functions
export { createApiKey, listApiKeys, deleteApiKey } from "./api";

export { mintNativeHandoffGrant } from "./api";

export {
  prepareNativeOAuthHandoff,
  readNativeUserAuth,
  installNativeOAuthHandoffCredentials,
  type NativeOAuthHandoffAuthFence,
  type NativeOAuthHandoffPreparation,
  type NativeUserCredentialPair,
  type NativeUserAuthState
} from "./transportV2/nativeAuth";

export { fetchModels, fetchModelCatalog } from "./api";

// Export conversation and conversation-project API functions
export {
  createConversation,
  getConversation,
  updateConversation,
  deleteConversation,
  listConversationItems,
  getConversationItem,
  listConversations,
  deleteConversations,
  batchDeleteConversations,
  batchUpdateConversationProject,
  createConversationProject,
  listConversationProjects,
  getConversationProject,
  updateConversationProject,
  deleteConversationProject
} from "./api";

// Export provider-neutral web API functions
export { webSearch, webExtract } from "./api";

// Export Agent API functions
export {
  getMainAgent,
  deleteMainAgent,
  listMainAgentItems,
  getMainAgentItem,
  listSubagents,
  getSubagent,
  createSubagent,
  deleteSubagent,
  listSubagentItems,
  getSubagentItem
} from "./api";

// Export AI customization options
export {
  createCustomFetch,
  findOpenSecretInferenceCapacityError,
  OPEN_SECRET_INFERENCE_SEND_LIMIT_HEADER,
  OpenSecretInferenceCapacityError,
  type CustomFetchOptions
} from "./ai";

// Re-export Model type from OpenAI for convenience
export type { Model } from "openai/resources/models.js";

// Export API configuration
export { apiConfig, type ApiContext, type ApiEndpoint } from "./apiConfig";

// Export the provider and context
export { OpenSecretProvider, OpenSecretContext } from "./main";
export { OpenSecretDeveloper, OpenSecretDeveloperContext } from "./developer";

// Export the hooks
export { useOpenSecret } from "./context";
export { useOpenSecretDeveloper } from "./developerContext";

// Export types needed by consumers
export type { OpenSecretAuthState, OpenSecretContextType } from "./main";
export type {
  OpenSecretDeveloperAuthState,
  OpenSecretDeveloperContextType,
  DeveloperRole,
  OrganizationDetails,
  ProjectDetails,
  ProjectSettings,
  PushSettings
} from "./developer";
export type { AttestationDocument } from "./attestation";
export type { ParsedAttestationView } from "./attestationForView";
export type { PcrConfig, PcrEnvironment, Pcr0ValidationResult } from "./pcr";

// Export crypto utilities
// TODO: these can actually just be used internally by the password reset function
export { generateSecureSecret, hashSecret } from "./crypto";
