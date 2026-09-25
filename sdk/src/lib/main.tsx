import { clearUserCredentialsForSignOut } from "./credentialIdentity";
import React, { createContext, useState, useEffect, useRef } from "react";
import * as api from "./api";
import { createCustomFetch } from "./ai";
import type { Model } from "openai/resources/models.js";
import { authenticate } from "./attestation";
import {
  parseAttestationForView,
  AWS_ROOT_CERT_DER,
  EXPECTED_ROOT_CERT_HASH,
  ParsedAttestationView
} from "./attestationForView";
import type { AttestationDocument } from "./attestation";
import type { LoginResponse, ThirdPartyTokenResponse, DocumentResponse } from "./api";
import { PcrConfig } from "./pcr";
import {
  readTransportV2Credentials,
  snapshotTransportV2Auth,
  subscribeTransportV2AuthInvalidation,
  transportV2ProfilePublicationDecision
} from "./transportV2/auth";
import type { Attestation } from "./getAttestation";
import { getTransportV2PublicAttestation, transportV2Runtime } from "./transportV2/runtime";
import {
  snapshotTransportV2AuthorityScope,
  transportV2AuthRuntime
} from "./transportV2/authRuntime";

const DEFAULT_PCR_CONFIG: PcrConfig = { environment: "production" };

export type OpenSecretAuthState = {
  loading: boolean;
  user?: api.UserResponse;
};

export type OpenSecretContextType = {
  auth: OpenSecretAuthState;

  /**
   * The client ID for this project/tenant.
   * A UUID that identifies which project/tenant this instance belongs to.
   */
  clientId: string;

  /**
   * Optional API key for OpenAI endpoints.
   * When set, this will be used instead of JWT for /v1/* endpoints.
   */
  apiKey?: string;

  /**
   * Sets the API key to use for OpenAI endpoints.
   * @param key - The API key (UUID format) or undefined to clear
   */
  setApiKey: (key: string | undefined) => void;

  /**
   * Authenticates a user with email and password.
   *
   * - Calls the login API endpoint with the configured clientId
   * - Stores access_token and refresh_token in localStorage
   * - Updates the auth state with user information
   * - Throws an error if authentication fails
   *
   * @param email - User's email address
   * @param password - User's password
   * @returns A promise that resolves when authentication is complete
   * @throws {Error} If login fails
   */
  signIn: (email: string, password: string) => Promise<void>;

  /**
   * Creates a new user account
   * @param email - User's email address
   * @param password - User's chosen password
   * @param inviteCode - Invitation code for registration
   * @param name - Optional user's full name
   * @returns A promise that resolves when account creation is complete
   * @throws {Error} If signup fails
   *
   *
   * - Calls the registration API endpoint
   * - Stores access_token and refresh_token in localStorage
   * - Updates the auth state with new user information
   * - Throws an error if account creation fails
   */
  signUp: (email: string, password: string, inviteCode: string, name?: string) => Promise<void>;

  /**
   * Authenticates a guest user with user id and password
   * @param id - User's unique id
   * @param password - User's password
   * @returns A promise that resolves when authentication is complete
   * @throws {Error} If login fails
   *
   *
   * - Calls the login API endpoint
   * - Stores access_token and refresh_token in localStorage
   * - Updates the auth state with user information
   * - Throws an error if authentication fails
   */
  signInGuest: (id: string, password: string) => Promise<void>;

  /**
   * Creates a new long-lived guest account with no email recovery.
   * @param password - User's chosen password. It can be changed while authenticated, but it cannot be recovered via email unless an email address is later added to the account.
   * @param inviteCode - Invitation code for registration
   * @returns A promise that resolves to the login response containing the guest ID
   * @throws {Error} If signup fails
   *
   *
   * - Calls the registration API endpoint
   * - Stores access_token and refresh_token in localStorage
   * - Updates the auth state with new user information
   * - Throws an error if account creation fails
   */
  signUpGuest: (password: string, inviteCode: string) => Promise<LoginResponse>;

  /**
   * Logs out the current user
   * @returns A promise that resolves when logout is complete
   * @throws {Error} If logout fails
   *
   *
   * - Calls the logout API endpoint with the current refresh_token
   * - Removes access_token, refresh_token from localStorage
   * - Removes session-related items from sessionStorage
   * - Resets the auth state to show no user is authenticated
   */
  signOut: () => Promise<void>;

  verifyEmail: typeof api.verifyEmail;
  requestNewVerificationCode: typeof api.requestNewVerificationCode;
  requestNewVerificationEmail: typeof api.requestNewVerificationCode;
  refetchUser: () => Promise<void>;
  changePassword: typeof api.changePassword;
  refreshAccessToken: typeof api.refreshToken;
  requestPasswordReset: (email: string, hashedSecret: string) => Promise<void>;
  confirmPasswordReset: (
    email: string,
    alphanumericCode: string,
    plaintextSecret: string,
    newPassword: string
  ) => Promise<void>;
  /**
   * Initiates the account deletion process for logged-in users
   * @param hashedSecret - Client-side hashed secret for verification
   * @returns A promise resolving to void
   * @throws {Error} If request fails
   *
   * This function:
   * 1. Requires the user to be logged in (uses authenticatedApiCall)
   * 2. Sends a verification email to the user's email address
   * 3. The email contains a confirmation code that will be needed for confirmation
   * 4. The client must store the plaintext secret for confirmation
   */
  requestAccountDeletion: (hashedSecret: string) => Promise<void>;

  /**
   * Confirms and completes the account deletion process
   * @param confirmationCode - The confirmation code from the verification email
   * @param plaintextSecret - The plaintext secret that was hashed in the request step
   * @returns A promise resolving to void
   * @throws {Error} If confirmation fails
   *
   * This function:
   * 1. Requires the user to be logged in (uses authenticatedApiCall)
   * 2. Verifies both the confirmation code from email and the secret known only to the client
   * 3. Permanently deletes the user account and all associated data
   * 4. After successful deletion, the client should clear all local storage and tokens
   */
  confirmAccountDeletion: (confirmationCode: string, plaintextSecret: string) => Promise<void>;
  initiateGitHubAuth: (inviteCode: string, redirectUrl?: string) => Promise<api.GithubAuthResponse>;
  handleGitHubCallback: (code: string, state: string, inviteCode: string) => Promise<void>;
  initiateGoogleAuth: (inviteCode: string, redirectUrl?: string) => Promise<api.GoogleAuthResponse>;
  handleGoogleCallback: (code: string, state: string, inviteCode: string) => Promise<void>;
  initiateAppleAuth: (inviteCode: string, redirectUrl?: string) => Promise<api.AppleAuthResponse>;
  handleAppleCallback: (code: string, state: string, inviteCode: string) => Promise<void>;
  handleAppleNativeSignIn: (appleUser: api.AppleUser, inviteCode?: string) => Promise<void>;
  mintNativeHandoffGrant: typeof api.mintNativeHandoffGrant;

  /**
   * Retrieves the user's private key mnemonic phrase
   * @param options - Optional key derivation options
   * @returns A promise resolving to the private key response
   * @throws {Error} If the private key cannot be retrieved
   *
   *
   * This function supports two modes:
   *
   * 1. Master mnemonic (no parameters)
   *    - Returns the user's master 12-word BIP39 mnemonic
   *
   * 2. BIP-85 derived mnemonic
   *    - Derives a child mnemonic using BIP-85
   *    - Requires seed_phrase_derivation_path in options
   *    - Example: "m/83696968'/39'/0'/12'/0'"
   */
  getPrivateKey: typeof api.fetchPrivateKey;

  /**
   * Retrieves the private key bytes for the given derivation options
   * @param options - Optional key derivation options or legacy BIP32 derivation path string
   * @returns A promise resolving to the private key bytes response
   * @throws {Error} If:
   * - The private key bytes cannot be retrieved
   * - The derivation paths are invalid
   *
   *
   * This function supports multiple derivation approaches:
   *
   * 1. Master key only (no parameters)
   *    - Returns the master private key bytes
   *
   * 2. BIP-32 derivation only
   *    - Uses a single derivation path to derive a child key from the master seed
   *    - Supports both absolute and relative paths with hardened derivation:
   *      - Absolute path: "m/44'/0'/0'/0/0"
   *      - Relative path: "0'/0'/0'/0/0"
   *      - Hardened notation: "44'" or "44h"
   *    - Common paths:
   *      - BIP44 (Legacy): m/44'/0'/0'/0/0
   *      - BIP49 (SegWit): m/49'/0'/0'/0/0
   *      - BIP84 (Native SegWit): m/84'/0'/0'/0/0
   *      - BIP86 (Taproot): m/86'/0'/0'/0/0
   *
   * 3. BIP-85 derivation only
   *    - Derives a child mnemonic from the master seed using BIP-85
   *    - Then returns the master private key of that derived seed
   *    - Example path: "m/83696968'/39'/0'/12'/0'"
   *
   * 4. Combined BIP-85 and BIP-32 derivation
   *    - First derives a child mnemonic via BIP-85
   *    - Then applies BIP-32 derivation to that derived seed
   */
  getPrivateKeyBytes: typeof api.fetchPrivateKeyBytes;

  /**
   * Retrieves the user's public key for the specified algorithm
   * @param algorithm - The signing algorithm ('schnorr' or 'ecdsa')
   * @param options - Optional key derivation options or legacy BIP32 derivation path string
   * @returns A promise resolving to the public key response
   * @throws {Error} If the public key cannot be retrieved
   *
   *
   * The derivation paths determine which key is used to generate the public key:
   *
   * 1. Master key (no derivation parameters)
   *    - Returns the public key corresponding to the master private key
   *
   * 2. BIP-32 derived key
   *    - Returns the public key for a derived child key
   *
   * 3. BIP-85 derived key
   *    - Returns the public key for the master key of a BIP-85 derived seed
   *
   * 4. Combined BIP-85 and BIP-32 derivation
   *    - First derives a child mnemonic via BIP-85
   *    - Then applies BIP-32 derivation to get the corresponding public key
   */
  getPublicKey: typeof api.fetchPublicKey;

  /**
   * Signs a message using the specified algorithm.
   * This function supports multiple signing approaches: master key (no derivation),
   * BIP-32 derived key, BIP-85 derived key, or combined BIP-85 and BIP-32 derivation.
   *
   * @param messageBytes - The message to sign as a Uint8Array
   * @param algorithm - The signing algorithm ('schnorr' or 'ecdsa')
   * @param options - Optional key derivation options or legacy BIP32 derivation path string
   * @returns A promise resolving to the signature response
   * @throws {Error} If the message signing fails
   */
  signMessage: typeof api.signMessage;

  /**
   * Custom fetch function for AI requests that handles encryption
   * and token refreshing.
   *
   * Meant to be used with the OpenAI JS library
   *
   * Example:
   * ```tsx
   * const openai = new OpenAI({
   *   baseURL: `${os.apiUrl}/v1/`,
   *   dangerouslyAllowBrowser: true,
   *   apiKey: "the-api-key-doesnt-matter",
   *   defaultHeaders: {
   *     "Accept-Encoding": "identity"
   *   },
   *   fetch: os.aiCustomFetch,
   *   // OpenSecret exposes an explicit replay contract; keep transport retries disabled.
   *   maxRetries: 0
   * });
   * ```
   */
  aiCustomFetch: (input: string | URL | Request, init?: RequestInit) => Promise<Response>;

  /**
   * Returns the current OpenSecret enclave API URL being used
   * @returns The current API URL
   */
  apiUrl: string;

  /**
   * PCR0 trust policy enforced before every non-loopback session key exchange.
   * The trust environment defaults to production.
   */
  pcrConfig: PcrConfig;

  /**
   * Gets an attested session after enforcing the effective PCR0 trust policy
   */
  getAttestation: (
    forceRefresh?: boolean,
    explicitApiUrl?: string,
    explicitPcrConfig?: PcrConfig
  ) => Promise<Attestation>;

  /**
   * Authenticates an attestation document
   */
  authenticate: typeof authenticate;

  /**
   * Parses an attestation document for viewing
   */
  parseAttestationForView: (
    document: AttestationDocument,
    cabundle: Uint8Array[],
    pcrConfig?: PcrConfig
  ) => Promise<ParsedAttestationView>;

  /**
   * AWS root certificate in DER format
   */
  awsRootCertDer: typeof AWS_ROOT_CERT_DER;

  /**
   * Expected hash of the AWS root certificate
   */
  expectedRootCertHash: typeof EXPECTED_ROOT_CERT_HASH;

  /**
   * Gets and verifies an attestation document from the enclave
   * @returns A promise resolving to the parsed attestation document
   * @throws {Error} If attestation fails or is invalid
   *
   *
   * This is a convenience function that:
   * 1. Fetches the attestation document with a random nonce
   * 2. Authenticates the document
   * 3. Parses it for viewing
   */
  getAttestationDocument: () => Promise<ParsedAttestationView>;

  /**
   * Generates a JWT token for use with third-party services
   * @param audience - Optional audience value for the target service
   * @returns A promise resolving to the token response
   * @throws {Error} If:
   * - The user is not authenticated
   * - The audience value is invalid (if provided)
   *
   *
   * - Generates a signed JWT token for use with third-party services
   * - If audience is provided, it can be a valid URL or another accepted audience string
   * - If audience is omitted, a token with no audience restriction will be generated
   * - Requires an active authentication session
   * - Token can be used to authenticate with the specified service
   */
  generateThirdPartyToken: (audience?: string) => Promise<ThirdPartyTokenResponse>;

  /**
   * Encrypts arbitrary string data using the user's private key
   * @param data - String content to be encrypted
   * @param options - Optional key derivation options or legacy BIP32 derivation path string
   * @returns A promise resolving to the encrypted data response
   * @throws {Error} If:
   * - The derivation paths are invalid
   * - Authentication fails
   * - Server-side encryption error occurs
   *
   *
   * This function supports multiple encryption approaches:
   *
   * 1. Encrypt with master key (no derivation parameters)
   *
   * 2. Encrypt with BIP-32 derived key
   *    - Derives a child key from the master seed using BIP-32
   *    - Example: "m/44\'/0\'/0\'/0/0"
   *
   * 3. Encrypt with BIP-85 derived key
   *    - Derives a child mnemonic using BIP-85, then uses its master key
   *    - Example: { seed_phrase_derivation_path: "m/83696968\'/39\'/0\'/12\'/0\'" }
   *
   * 4. Encrypt with combined BIP-85 and BIP-32 derivation
   *    - First derives a child mnemonic via BIP-85
   *    - Then applies BIP-32 derivation to derive a key from that seed
   *    - Example: {
   *        seed_phrase_derivation_path: "m/83696968\'/39\'/0\'/12\'/0\'",
   *        private_key_derivation_path: "m/44\'/0\'/0\'/0/0"
   *      }
   *
   * Technical details:
   * - Encrypts data with AES-256-GCM
   * - A random nonce is generated for each encryption operation (included in the result)
   * - The encrypted_data format includes the nonce and is base64-encoded
   */
  encryptData: typeof api.encryptData;

  /**
   * Decrypts data that was previously encrypted with the user's key
   * @param encryptedData - Base64-encoded encrypted data string
   * @param options - Optional key derivation options or legacy BIP32 derivation path string
   * @returns A promise resolving to the decrypted string
   * @throws {Error} If:
   * - The encrypted data is malformed
   * - The derivation paths are invalid
   * - Authentication fails
   * - Server-side decryption error occurs
   *
   *
   * This function supports multiple decryption approaches:
   *
   * 1. Decrypt with master key (no derivation parameters)
   *
   * 2. Decrypt with BIP-32 derived key
   *    - Derives a child key from the master seed using BIP-32
   *
   * 3. Decrypt with BIP-85 derived key
   *    - Derives a child mnemonic using BIP-85, then uses its master key
   *
   * 4. Decrypt with combined BIP-85 and BIP-32 derivation
   *    - First derives a child mnemonic via BIP-85
   *    - Then applies BIP-32 derivation to derive a key from that seed
   *
   * IMPORTANT: You must use the exact same derivation options for decryption
   * that were used for encryption.
   */
  decryptData: typeof api.decryptData;

  /**
   * Fetches available AI models from the OpenAI-compatible API
   * @returns A promise resolving to an array of Model objects
   * @throws {Error} If:
   * - The request fails
   *
   *
   * - Returns a list of available AI models from the configured OpenAI-compatible API
   * - Response is encrypted and automatically decrypted
   * - Before sign-in, uses an attested encrypted session without identity authorization
   * - When a JWT or API key is present, preserves authenticated model filtering
   */
  fetchModels: () => Promise<Model[]>;

  /**
   * Fetches OpenSecret's model catalog with display metadata and stable aliases.
   */
  fetchModelCatalog: () => Promise<api.ModelCatalogResponse>;

  /**
   * Uploads a document for text extraction and processing
   * @param file - The file to upload (File or Blob object)
   * @returns A promise resolving to the task ID and initial metadata
   * @throws {Error} If:
   * - The file exceeds 10MB size limit
   * - The user is not authenticated (or is a guest user)
   * - Usage limits are exceeded (403)
   * - Processing fails (500)
   *
   * @description
   * This function uploads a document to the Tinfoil processing service which:
   * 1. Accepts the document and returns a task ID immediately
   * 2. Processes the document asynchronously in the background
   * 3. Maintains end-to-end encryption using session keys
   *
   * Common supported formats include PDF, DOCX, XLSX, PPTX, TXT, RTF, and more.
   * Guest users will receive a 401 Unauthorized error.
   *
   * Example usage:
   * ```typescript
   * const file = new File(["content"], "document.pdf", { type: "application/pdf" });
   * const result = await context.uploadDocument(file);
   * console.log(result.task_id); // Task ID to check status
   * ```
   */
  uploadDocument: (file: File | Blob) => Promise<api.DocumentUploadInitResponse>;

  /**
   * Checks the status of a document processing task
   * @param taskId - The task ID returned from uploadDocument
   * @returns A promise resolving to the current status and optionally the processed document
   * @throws {Error} If:
   * - The user is not authenticated
   * - The task ID is not found (404)
   * - The user doesn't have access to the task (403)
   *
   * @description
   * This function checks the status of an async document processing task.
   * Status values include:
   * - "pending": Document is queued for processing
   * - "started": Document processing has begun
   * - "success": Processing completed successfully (document field will be populated)
   * - "failure": Processing failed (error field will contain details)
   *
   * Example usage:
   * ```typescript
   * const status = await context.checkDocumentStatus(taskId);
   * if (status.status === "success" && status.document) {
   *   console.log(status.document.text);
   * }
   * ```
   */
  checkDocumentStatus: (taskId: string) => Promise<api.DocumentStatusResponse>;

  /**
   * Uploads a document and polls for completion
   * @param file - The file to upload (File or Blob object)
   * @param options - Optional configuration for polling behavior
   * @returns A promise resolving to the processed document
   * @throws {Error} If:
   * - Upload fails (see uploadDocument errors)
   * - Processing fails (error from server)
   * - Processing times out (exceeds maxAttempts)
   *
   * @description
   * This is a convenience function that combines uploadDocument and checkDocumentStatus
   * to provide a simple interface that handles the async processing automatically.
   *
   * Options:
   * - pollInterval: Time between status checks in milliseconds (default: 2000)
   * - maxAttempts: Maximum number of status checks before timeout (default: 150 = 5 minutes)
   * - onProgress: Callback function called on each status update
   *
   * Example usage:
   * ```typescript
   * const file = new File(["content"], "document.pdf", { type: "application/pdf" });
   * const result = await context.uploadDocumentWithPolling(file, {
   *   onProgress: (status, progress) => {
   *     console.log(`Status: ${status}, Progress: ${progress || 0}%`);
   *   }
   * });
   * console.log(result.text);
   * ```
   */
  uploadDocumentWithPolling: (
    file: File | Blob,
    options?: {
      pollInterval?: number;
      maxAttempts?: number;
      onProgress?: (status: string, progress?: number) => void;
    }
  ) => Promise<DocumentResponse>;

  /**
   * Creates a new API key for the authenticated user
   * @param name - A descriptive name for the API key
   * @returns A promise resolving to the API key details with the key value (only shown once)
   * @throws {Error} If the user is not authenticated or the request fails
   *
   * IMPORTANT: The `key` field is only returned once during creation and cannot be retrieved again.
   * The SDK consumer should prompt users to save the key immediately.
   */
  createApiKey: typeof api.createApiKey;

  /**
   * Lists all API keys for the authenticated user
   * @returns A promise resolving to an object containing an array of API key metadata (without the actual keys)
   * @throws {Error} If the user is not authenticated or the request fails
   *
   * Returns metadata about all API keys associated with the user's account.
   * Note that the actual key values are never returned - they are only shown once during creation.
   * The keys are sorted by created_at in descending order (newest first).
   */
  listApiKeys: typeof api.listApiKeys;

  /**
   * Deletes an API key by its name
   * @param name - The name of the API key to delete
   * @returns A promise that resolves when the key is deleted
   * @throws {Error} If the user is not authenticated or the API key is not found
   *
   * Permanently deletes an API key. This action cannot be undone.
   * Any requests using the deleted key will immediately fail with 401 Unauthorized.
   * Names are unique per user, so this uniquely identifies the key to delete.
   */
  deleteApiKey: typeof api.deleteApiKey;

  /**
   * Transcribes audio using the Whisper API
   * @param file - The audio file to transcribe (File or Blob object)
   * @param options - Optional transcription parameters
   * @returns A promise resolving to the transcription response
   * @throws {Error} If the user is not authenticated or transcription fails
   *
   * @description
   * This function transcribes audio using OpenAI's Whisper model via the encrypted API.
   *
   * Options:
   * - model: Model to use (default: "whisper-large-v3", routes to Tinfoil's whisper-large-v3-turbo)
   * - language: Optional ISO-639-1 language code (e.g., "en", "es", "fr")
   * - prompt: Optional context or previous segment transcript
   * - temperature: Sampling temperature between 0 and 1 (default: 0.0)
   *
   * Supported audio formats: MP3, WAV, MP4, M4A, FLAC, OGG, WEBM
   *
   * Example usage:
   * ```typescript
   * const audioFile = new File([audioData], "recording.mp3", { type: "audio/mpeg" });
   * const result = await context.transcribeAudio(audioFile, {
   *   language: "en",
   *   prompt: "This is a technical discussion about AI"
   * });
   * console.log(result.text);
   * ```
   */
  transcribeAudio: typeof api.transcribeAudio;

  /** Searches the public web through OpenSecret's configured search provider. */
  webSearch: typeof api.webSearch;

  /** Extracts sanitized Markdown from public URLs through OpenSecret's configured provider. */
  webExtract: typeof api.webExtract;

  /**
   * Lists user's responses with pagination
   * @param params - Optional parameters for pagination and filtering
   * @returns A promise resolving to a paginated list of responses
   * @throws {Error} If:
   * - The user is not authenticated
   * - The request fails
   * - Invalid pagination parameters
   *
   * @description
   * This function fetches a paginated list of the user's responses.
   * In list view, the usage and output fields are always null for performance reasons.
   *
   * Query Parameters:
   * - limit: Number of results per page (1-100, default: 20)
   * - after: UUID cursor for forward pagination
   * - before: UUID cursor for backward pagination
   * - order: Sort order (currently not implemented, reserved for future use)
   *
   * Pagination Examples:
   * ```typescript
   * // First page
   * const responses = await context.fetchResponsesList({ limit: 20 });
   *
   * // Next page
   * const nextPage = await context.fetchResponsesList({
   *   limit: 20,
   *   after: responses.last_id
   * });
   *
   * // Previous page
   * const prevPage = await context.fetchResponsesList({
   *   limit: 20,
   *   before: responses.first_id
   * });
   * ```
   */
  fetchResponsesList: (params?: api.ResponsesListParams) => Promise<api.ResponsesListResponse>;

  /**
   * Retrieves a single response by ID
   * @param responseId - The UUID of the response to retrieve
   * @returns A promise resolving to the response details
   */
  fetchResponse: (responseId: string) => Promise<api.ResponsesRetrieveResponse>;

  /**
   * Cancels an in-progress response
   * @param responseId - The UUID of the response to cancel
   * @returns A promise resolving to the cancelled response
   */
  cancelResponse: (responseId: string) => Promise<api.ResponsesCancelResponse>;

  /**
   * Deletes a response permanently
   * @param responseId - The UUID of the response to delete
   * @returns A promise resolving to deletion confirmation
   */
  deleteResponse: (responseId: string) => Promise<api.ResponsesDeleteResponse>;

  /**
   * Creates a new response with conversation support
   * @param request - The request parameters for creating a response
   * @returns A promise resolving to the created response or a stream
   */
  createResponse: typeof api.createResponse;

  /**
   * Creates a single conversation with optional metadata/project/pin state.
   */
  createConversation: typeof api.createConversation;

  /**
   * Retrieves a single conversation.
   */
  getConversation: typeof api.getConversation;

  /**
   * Updates a single conversation's metadata/project/pin state.
   */
  updateConversation: typeof api.updateConversation;

  /**
   * Deletes a single conversation.
   */
  deleteConversation: typeof api.deleteConversation;

  /**
   * Lists items in a single conversation.
   */
  listConversationItems: typeof api.listConversationItems;

  /**
   * Retrieves a single conversation item.
   */
  getConversationItem: typeof api.getConversationItem;

  /**
   * Lists all conversations with pagination (non-standard endpoint)
   * @param params - Optional pagination parameters
   * @returns A promise resolving to a paginated list of conversations
   * @description
   * This is a custom extension not part of the standard OpenAI API.
   * For standard conversation operations, use the OpenAI client directly:
   * - openai.conversations.create()
   * - openai.conversations.retrieve()
   * - openai.conversations.update()
   * - openai.conversations.delete()
   * - openai.conversations.items.list()
   * - openai.conversations.items.retrieve()
   */
  listConversations: typeof api.listConversations;

  /**
   * Deletes all conversations
   * @returns A promise resolving to deletion confirmation
   */
  deleteConversations: typeof api.deleteConversations;

  /**
   * Batch deletes multiple conversations by their IDs
   * @param ids - Array of conversation UUIDs to delete
   * @returns A promise resolving to per-item deletion results
   */
  batchDeleteConversations: typeof api.batchDeleteConversations;

  /**
   * Batch moves conversations into a project or clears project assignment.
   */
  batchUpdateConversationProject: typeof api.batchUpdateConversationProject;

  /**
   * Creates a new conversation project.
   */
  createConversationProject: typeof api.createConversationProject;

  /**
   * Lists conversation projects with pagination.
   */
  listConversationProjects: typeof api.listConversationProjects;

  /**
   * Retrieves a single conversation project.
   */
  getConversationProject: typeof api.getConversationProject;

  /**
   * Updates a conversation project and/or its project-scoped instructions.
   */
  updateConversationProject: typeof api.updateConversationProject;

  /**
   * Deletes a conversation project.
   */
  deleteConversationProject: typeof api.deleteConversationProject;

  /**
   * Creates a new instruction
   * @param request - The instruction creation parameters
   * @returns A promise resolving to the created instruction
   * @throws {Error} If the user is not authenticated or the request fails
   *
   * @description
   * Creates a new user instruction (system prompt).
   * If is_default is set to true, all other instructions are automatically set to is_default: false.
   * The prompt_tokens field is automatically calculated.
   */
  createInstruction: typeof api.createInstruction;

  /**
   * Lists user's instructions with pagination
   * @param params - Optional parameters for pagination and ordering
   * @returns A promise resolving to a paginated list of instructions
   * @throws {Error} If the user is not authenticated or the request fails
   *
   * @description
   * Fetches a paginated list of the user's instructions.
   * Results are ordered by updated_at by default (most recently updated first).
   */
  listInstructions: typeof api.listInstructions;

  /**
   * Retrieves a single instruction by ID
   * @param instructionId - The UUID of the instruction to retrieve
   * @returns A promise resolving to the instruction details
   * @throws {Error} If the instruction is not found or user doesn't have access
   */
  getInstruction: typeof api.getInstruction;

  /**
   * Updates an existing instruction
   * @param instructionId - The UUID of the instruction to update
   * @param request - The fields to update
   * @returns A promise resolving to the updated instruction
   * @throws {Error} If the instruction is not found or validation fails
   *
   * @description
   * At least one field must be provided.
   * If is_default: true is set, all other instructions are automatically set to is_default: false.
   * The prompt_tokens field is recalculated automatically if prompt changes.
   */
  updateInstruction: typeof api.updateInstruction;

  /**
   * Deletes an instruction permanently
   * @param instructionId - The UUID of the instruction to delete
   * @returns A promise resolving to deletion confirmation
   * @throws {Error} If the instruction is not found or user doesn't have access
   *
   * @description
   * Permanently deletes an instruction. This action cannot be undone.
   */
  deleteInstruction: typeof api.deleteInstruction;

  /**
   * Sets an instruction as the default
   * @param instructionId - The UUID of the instruction to set as default
   * @returns A promise resolving to the updated instruction
   * @throws {Error} If the instruction is not found
   *
   * @description
   * Sets the specified instruction as the default.
   * All other instructions for this user are automatically set to is_default: false.
   * This operation is idempotent.
   */
  setDefaultInstruction: typeof api.setDefaultInstruction;
};

export const OpenSecretContext = createContext<OpenSecretContextType>({
  auth: {
    loading: true,
    user: undefined
  },
  clientId: "",
  apiKey: undefined,
  setApiKey: () => {},
  signIn: async () => {},
  signUp: async () => {},
  signInGuest: async () => {},
  signUpGuest: async (): Promise<LoginResponse> => ({
    id: "",
    email: undefined,
    access_token: "",
    refresh_token: ""
  }),
  signOut: async () => {},
  verifyEmail: api.verifyEmail,
  requestNewVerificationCode: api.requestNewVerificationCode,
  requestNewVerificationEmail: api.requestNewVerificationCode,
  refetchUser: async () => {},
  changePassword: api.changePassword,
  refreshAccessToken: api.refreshToken,
  requestPasswordReset: async () => {},
  confirmPasswordReset: async () => {},
  requestAccountDeletion: async () => {},
  confirmAccountDeletion: async () => {},
  initiateGitHubAuth: async () => ({ auth_url: "", csrf_token: "" }),
  handleGitHubCallback: async () => {},
  initiateGoogleAuth: async () => ({ auth_url: "", csrf_token: "" }),
  handleGoogleCallback: async () => {},
  initiateAppleAuth: async () => ({ auth_url: "", state: "" }),
  handleAppleCallback: async () => {},
  handleAppleNativeSignIn: async () => {},
  mintNativeHandoffGrant: api.mintNativeHandoffGrant,
  getPrivateKey: api.fetchPrivateKey,
  getPrivateKeyBytes: api.fetchPrivateKeyBytes,
  getPublicKey: api.fetchPublicKey,
  signMessage: api.signMessage,
  aiCustomFetch: async () => new Response(),
  apiUrl: "",
  pcrConfig: DEFAULT_PCR_CONFIG,
  getAttestation: async () => ({
    sessionKey: null,
    sessionId: null
  }),
  authenticate,
  parseAttestationForView,
  awsRootCertDer: AWS_ROOT_CERT_DER,
  expectedRootCertHash: EXPECTED_ROOT_CERT_HASH,
  getAttestationDocument: async () => {
    throw new Error("getAttestationDocument called outside of OpenSecretProvider");
  },
  generateThirdPartyToken: async () => ({ token: "" }),
  encryptData: api.encryptData,
  decryptData: api.decryptData,
  fetchModels: api.fetchModels,
  fetchModelCatalog: api.fetchModelCatalog,
  uploadDocument: api.uploadDocument,
  checkDocumentStatus: api.checkDocumentStatus,
  uploadDocumentWithPolling: api.uploadDocumentWithPolling,
  createApiKey: api.createApiKey,
  listApiKeys: api.listApiKeys,
  deleteApiKey: api.deleteApiKey,
  transcribeAudio: api.transcribeAudio,
  webSearch: api.webSearch,
  webExtract: api.webExtract,
  fetchResponsesList: api.fetchResponsesList,
  fetchResponse: api.fetchResponse,
  cancelResponse: api.cancelResponse,
  deleteResponse: api.deleteResponse,
  createResponse: api.createResponse,
  createConversation: api.createConversation,
  getConversation: api.getConversation,
  updateConversation: api.updateConversation,
  deleteConversation: api.deleteConversation,
  listConversationItems: api.listConversationItems,
  getConversationItem: api.getConversationItem,
  listConversations: api.listConversations,
  deleteConversations: api.deleteConversations,
  batchDeleteConversations: api.batchDeleteConversations,
  batchUpdateConversationProject: api.batchUpdateConversationProject,
  createConversationProject: api.createConversationProject,
  listConversationProjects: api.listConversationProjects,
  getConversationProject: api.getConversationProject,
  updateConversationProject: api.updateConversationProject,
  deleteConversationProject: api.deleteConversationProject,
  createInstruction: api.createInstruction,
  listInstructions: api.listInstructions,
  getInstruction: api.getInstruction,
  updateInstruction: api.updateInstruction,
  deleteInstruction: api.deleteInstruction,
  setDefaultInstruction: api.setDefaultInstruction
});

/**
 * Provider component for OpenSecret authentication and API access.
 *
 * @param props - Configuration properties for the OpenSecret provider
 * @param props.children - React child components to be wrapped by the provider
 * @param props.apiUrl - URL of OpenSecret enclave backend
 * @param props.clientId - UUID identifying which project/tenant this instance belongs to
 * @param props.pcrConfig - Optional PCR0 trust policy enforced before session establishment; its environment defaults to production
 *
 * @remarks
 * This provider manages:
 * - User authentication state
 * - Authentication methods (sign in, sign up, sign out)
 * - Project/tenant identification via clientId
 *
 * @example
 * ```tsx
 * <OpenSecretProvider
 *   apiUrl='https://preview.opensecret.ai'
 *   clientId='550e8400-e29b-41d4-a716-446655440000'
 * >
 *   <App />
 * </OpenSecretProvider>
 * ```
 */
export function OpenSecretProvider({
  children,
  apiUrl,
  clientId,
  pcrConfig = DEFAULT_PCR_CONFIG
}: {
  children: React.ReactNode;
  apiUrl: string;
  clientId: string;
  pcrConfig?: PcrConfig;
}) {
  const [auth, setAuth] = useState<OpenSecretAuthState>({
    loading: true,
    user: undefined
  });
  const authenticatedUserId = auth.user?.user.id;
  const authLoadGeneration = useRef(0);
  const authorityScope = snapshotTransportV2AuthorityScope(apiUrl, pcrConfig, "user");
  const authorityScopeKey = JSON.stringify([authorityScope.key, clientId]);
  const currentAuthorityScopeKey = useRef(authorityScopeKey);
  currentAuthorityScopeKey.current = authorityScopeKey;
  const [apiKey, setApiKeyState] = useState<string | undefined>();
  const [aiCustomFetch, setAiCustomFetch] = useState<OpenSecretContextType["aiCustomFetch"]>();

  // Validates UUID-with-dashes (v1–v5) and trims input; set undefined to clear
  const setApiKey = (key: string | undefined) => {
    if (key === undefined) {
      setApiKeyState(undefined);
      return;
    }
    const trimmed = key.trim();
    const uuidWithDashes =
      /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
    if (!uuidWithDashes.test(trimmed)) {
      console.warn("setApiKey: provided key does not look like a UUID; clearing apiKey");
      setApiKeyState(undefined);
      return;
    }
    setApiKeyState(trimmed);
  };

  useEffect(() => {
    if (!apiUrl || apiUrl.trim() === "") {
      throw new Error(
        "OpenSecretProvider requires a non-empty apiUrl. Please provide a valid API endpoint URL."
      );
    }
    if (!clientId || clientId.trim() === "") {
      throw new Error(
        "OpenSecretProvider requires a non-empty clientId. Please provide a valid project UUID."
      );
    }
    api.setApiUrl(apiUrl, pcrConfig);

    // Configure the apiConfig service with the app URL
    // Using dynamic import to avoid circular dependencies
    import("./apiConfig").then(({ apiConfig }) => {
      const platformUrl = apiConfig.platformApiUrl || "";
      apiConfig.configure(apiUrl, platformUrl);
    });
  }, [apiUrl, clientId, pcrConfig]);

  // Create aiCustomFetch when API is configured (supports JWT or API key internally)
  useEffect(() => {
    if (apiUrl) {
      // Pass API key if available, otherwise falls back to JWT
      setAiCustomFetch(() =>
        createCustomFetch({ apiKey, apiUrl, pcrConfig, expectedUserId: authenticatedUserId })
      );
    } else {
      setAiCustomFetch(undefined);
    }
  }, [apiUrl, apiKey, authenticatedUserId, pcrConfig]);

  async function fetchUser() {
    const generation = authLoadGeneration.current + 1;
    authLoadGeneration.current = generation;
    const capturedScope = authorityScope;
    const capturedScopeKey = authorityScopeKey;
    const ownsPublication = () =>
      authLoadGeneration.current === generation &&
      currentAuthorityScopeKey.current === capturedScopeKey;
    if (!readTransportV2Credentials(capturedScope.apiUrl, "user")) {
      if (!ownsPublication()) return;
      setAuth({
        loading: false,
        user: undefined
      });
      return;
    }

    const attempted = snapshotTransportV2Auth(capturedScope.apiUrl, "user");
    let authority: Awaited<ReturnType<typeof transportV2AuthRuntime.authority>> | undefined;
    try {
      authority = await transportV2AuthRuntime.authority(
        capturedScope.apiUrl,
        capturedScope.pcrConfig,
        "user"
      );
      const user = await api.fetchUserWithTransportV2Authority(
        capturedScope.apiUrl,
        capturedScope.pcrConfig,
        authority
      );
      const decision = transportV2ProfilePublicationDecision(authority.snapshot, ownsPublication());
      if (decision === "reload") {
        void fetchUser();
        return;
      }
      if (decision === "discard") return;
      setAuth({
        loading: false,
        user
      });
    } catch (error) {
      if (!ownsPublication()) return;
      const sentWith = authority?.snapshot ?? attempted;
      const decision = transportV2ProfilePublicationDecision(sentWith, true);
      if (decision === "reload") {
        void fetchUser();
        return;
      }
      if (decision === "discard") return;
      console.error("Failed to fetch user:", error);
      setAuth({
        loading: false,
        user: undefined
      });
    }
  }

  useEffect(() => {
    setAuth({ loading: true, user: undefined });
    void fetchUser();
    const unsubscribe = subscribeTransportV2AuthInvalidation(authorityScope.apiUrl, "user", () => {
      authLoadGeneration.current += 1;
      setAuth({ loading: false, user: undefined });
    });
    return () => {
      authLoadGeneration.current += 1;
      unsubscribe();
    };
  }, [authorityScopeKey]);

  async function signIn(email: string, password: string) {
    console.log("Signing in");
    try {
      await api.fetchLogin(email, password, clientId);
      // Clear API key on new sign-in to ensure user-scoped keys
      setApiKey(undefined);
      await fetchUser();
    } catch (error) {
      console.error(error);
      throw error;
    }
  }

  async function signUp(email: string, password: string, inviteCode: string, name?: string) {
    try {
      await api.fetchSignUp(email, password, inviteCode, clientId, name || null);
      // Clear API key on new sign-up to ensure user-scoped keys
      setApiKey(undefined);
      await fetchUser();
    } catch (error) {
      console.error(error);
      throw error;
    }
  }

  async function signInGuest(id: string, password: string) {
    console.log("Signing in Guest");
    try {
      await api.fetchGuestLogin(id, password, clientId);
      // Clear API key on guest sign-in to ensure user-scoped keys
      setApiKey(undefined);
      await fetchUser();
    } catch (error) {
      console.error(error);
      throw error;
    }
  }

  async function signUpGuest(password: string, inviteCode: string) {
    try {
      const { access_token, refresh_token, id } = await api.fetchGuestSignUp(
        password,
        inviteCode,
        clientId
      );
      // Clear API key on guest sign-up to ensure user-scoped keys
      setApiKey(undefined);
      await fetchUser();
      return { access_token, refresh_token, id };
    } catch (error) {
      console.error(error);
      throw error;
    }
  }

  async function signOut() {
    const refresh_token = clearUserCredentialsForSignOut(apiUrl, authenticatedUserId);
    setApiKey(undefined);
    setAuth({ loading: false, user: undefined });
    if (refresh_token) {
      try {
        await api.fetchLogout(refresh_token);
      } catch (error) {
        console.error("Error during logout:", error);
      }
    }
  }

  const initiateGitHubAuth = async (inviteCode: string, redirectUrl?: string) => {
    try {
      return await api.initiateGitHubAuth(clientId, inviteCode, redirectUrl);
    } catch (error) {
      console.error("Failed to initiate GitHub auth:", error);
      throw error;
    }
  };

  const handleGitHubCallback = async (code: string, state: string, inviteCode: string) => {
    try {
      await api.handleGitHubCallback(code, state, inviteCode);
      // Clear API key on OAuth sign-in to ensure user-scoped keys
      setApiKey(undefined);
      await fetchUser();
    } catch (error) {
      console.error("GitHub callback error:", error);
      throw error;
    }
  };

  const initiateGoogleAuth = async (inviteCode: string, redirectUrl?: string) => {
    try {
      return await api.initiateGoogleAuth(clientId, inviteCode, redirectUrl);
    } catch (error) {
      console.error("Failed to initiate Google auth:", error);
      throw error;
    }
  };

  const handleGoogleCallback = async (code: string, state: string, inviteCode: string) => {
    try {
      await api.handleGoogleCallback(code, state, inviteCode);
      // Clear API key on OAuth sign-in to ensure user-scoped keys
      setApiKey(undefined);
      await fetchUser();
    } catch (error) {
      console.error("Google callback error:", error);
      throw error;
    }
  };

  const initiateAppleAuth = async (inviteCode: string, redirectUrl?: string) => {
    try {
      return await api.initiateAppleAuth(clientId, inviteCode, redirectUrl);
    } catch (error) {
      console.error("Failed to initiate Apple auth:", error);
      throw error;
    }
  };

  const handleAppleCallback = async (code: string, state: string, inviteCode: string) => {
    try {
      await api.handleAppleCallback(code, state, inviteCode);
      // Clear API key on OAuth sign-in to ensure user-scoped keys
      setApiKey(undefined);
      await fetchUser();
    } catch (error) {
      console.error("Apple callback error:", error);
      throw error;
    }
  };

  const handleAppleNativeSignIn = async (appleUser: api.AppleUser, inviteCode?: string) => {
    try {
      await api.handleAppleNativeSignIn(appleUser, clientId, inviteCode);
      // Clear API key on OAuth sign-in to ensure user-scoped keys
      setApiKey(undefined);
      await fetchUser();
    } catch (error) {
      console.error("Apple native sign-in error:", error);
      throw error;
    }
  };

  const getAttestationDocument = async () => {
    const nonce = window.crypto.randomUUID();
    const response = await fetch(`${apiUrl}/attestation/${nonce}`);
    if (!response.ok) {
      throw new Error("Failed to fetch attestation document");
    }

    const data = await response.json();
    const verifiedDocument = await authenticate(
      data.attestation_document,
      AWS_ROOT_CERT_DER,
      nonce
    );
    return parseAttestationForView(verifiedDocument, verifiedDocument.cabundle, pcrConfig);
  };

  const value: OpenSecretContextType = {
    auth,
    clientId,
    apiKey,
    setApiKey,
    signIn,
    signInGuest,
    signOut,
    signUp,
    signUpGuest,
    refetchUser: fetchUser,
    verifyEmail: api.verifyEmail,
    requestNewVerificationCode: api.requestNewVerificationCode,
    requestNewVerificationEmail: api.requestNewVerificationCode,
    changePassword: api.changePassword,
    refreshAccessToken: api.refreshToken,
    requestPasswordReset: (email: string, hashedSecret: string) =>
      api.requestPasswordReset(email, hashedSecret, clientId),
    confirmPasswordReset: (
      email: string,
      alphanumericCode: string,
      plaintextSecret: string,
      newPassword: string
    ) => api.confirmPasswordReset(email, alphanumericCode, plaintextSecret, newPassword, clientId),
    requestAccountDeletion: (hashedSecret) =>
      api.requestAccountDeletion(hashedSecret, authenticatedUserId),
    confirmAccountDeletion: (confirmationCode, plaintextSecret) =>
      api.confirmAccountDeletion(confirmationCode, plaintextSecret, authenticatedUserId),
    initiateGitHubAuth,
    handleGitHubCallback,
    initiateGoogleAuth,
    handleGoogleCallback,
    initiateAppleAuth,
    handleAppleCallback,
    handleAppleNativeSignIn,
    mintNativeHandoffGrant: api.mintNativeHandoffGrant,
    getPrivateKey: api.fetchPrivateKey,
    getPrivateKeyBytes: api.fetchPrivateKeyBytes,
    getPublicKey: api.fetchPublicKey,
    signMessage: api.signMessage,
    aiCustomFetch: aiCustomFetch || (async () => new Response()),
    apiUrl,
    pcrConfig,
    getAttestation: (forceRefresh, explicitApiUrl, explicitPcrConfig) =>
      getTransportV2PublicAttestation(
        transportV2Runtime,
        apiUrl,
        pcrConfig,
        forceRefresh,
        explicitApiUrl,
        explicitPcrConfig
      ),
    authenticate,
    parseAttestationForView,
    awsRootCertDer: AWS_ROOT_CERT_DER,
    expectedRootCertHash: EXPECTED_ROOT_CERT_HASH,
    getAttestationDocument,
    generateThirdPartyToken: api.generateThirdPartyToken,
    encryptData: api.encryptData,
    decryptData: api.decryptData,
    fetchModels: () => api.fetchModels(apiKey),
    fetchModelCatalog: () => api.fetchModelCatalog(apiKey),
    uploadDocument: api.uploadDocument,
    checkDocumentStatus: api.checkDocumentStatus,
    uploadDocumentWithPolling: api.uploadDocumentWithPolling,
    createApiKey: api.createApiKey,
    listApiKeys: api.listApiKeys,
    deleteApiKey: (name) => api.deleteApiKey(name, authenticatedUserId),
    transcribeAudio: api.transcribeAudio,
    webSearch: api.webSearch,
    webExtract: api.webExtract,
    fetchResponsesList: api.fetchResponsesList,
    fetchResponse: api.fetchResponse,
    cancelResponse: (responseId) => api.cancelResponse(responseId, authenticatedUserId),
    deleteResponse: (responseId) => api.deleteResponse(responseId, authenticatedUserId),
    createResponse: api.createResponse,
    createConversation: api.createConversation,
    getConversation: api.getConversation,
    updateConversation: api.updateConversation,
    deleteConversation: (conversationId) =>
      api.deleteConversation(conversationId, authenticatedUserId),
    listConversationItems: api.listConversationItems,
    getConversationItem: api.getConversationItem,
    listConversations: api.listConversations,
    deleteConversations: () => api.deleteConversations(authenticatedUserId),
    batchDeleteConversations: (ids) => api.batchDeleteConversations(ids, authenticatedUserId),
    batchUpdateConversationProject: api.batchUpdateConversationProject,
    createConversationProject: api.createConversationProject,
    listConversationProjects: api.listConversationProjects,
    getConversationProject: api.getConversationProject,
    updateConversationProject: api.updateConversationProject,
    deleteConversationProject: (projectId) =>
      api.deleteConversationProject(projectId, authenticatedUserId),
    createInstruction: api.createInstruction,
    listInstructions: api.listInstructions,
    getInstruction: api.getInstruction,
    updateInstruction: api.updateInstruction,
    deleteInstruction: (instructionId) => api.deleteInstruction(instructionId, authenticatedUserId),
    setDefaultInstruction: api.setDefaultInstruction
  };

  return <OpenSecretContext.Provider value={value}>{children}</OpenSecretContext.Provider>;
}
