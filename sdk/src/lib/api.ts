import { encode } from "@stablelib/base64";
import {
  authenticatedApiCall,
  authenticatedApiCallWithAuthority,
  authenticatedApiCallWithSelectedAuthority,
  encryptedApiCall,
  openAiAuthenticatedApiCall
} from "./encryptedApi";
import type { Model } from "openai/resources/models.js";
import { snapshotPcrConfig, type PcrConfig } from "./pcr";
import {
  clearLegacyTransportV1Credentials,
  installTransportV2Credentials,
  readTransportV2Credentials,
  snapshotTransportV2Auth,
  type TransportV2AuthSnapshot
} from "./transportV2/auth";
import { transportV2AuthRuntime } from "./transportV2/authRuntime";

let apiUrl = "";
let apiPcrConfig: PcrConfig = snapshotPcrConfig();
clearLegacyTransportV1Credentials();

export function setApiUrl(url: string, pcrConfig?: PcrConfig) {
  apiUrl = url;
  apiPcrConfig = snapshotPcrConfig(pcrConfig);
  clearLegacyTransportV1Credentials();
}

export function getApiUrl(): string {
  return apiUrl;
}

export function getApiPcrConfig(): PcrConfig {
  return snapshotPcrConfig(apiPcrConfig);
}

export type LoginResponse = {
  id: string;
  email?: string;
  access_token: string;
  refresh_token: string;
};

export type UserResponse = {
  user: {
    id: string;
    name: string | null;
    email?: string;
    email_verified: boolean;
    login_method: string;
    created_at: string;
    updated_at: string;
  };
};

type RefreshResponse = {
  access_token: string;
  refresh_token: string;
};

type CredentialUpdateResponse = {
  message: string;
  access_token?: string;
  refresh_token?: string;
};

function installLoginResponse(
  response: LoginResponse,
  expected: TransportV2AuthSnapshot
): LoginResponse {
  installTransportV2Credentials(
    apiUrl,
    "user",
    response.access_token,
    response.refresh_token,
    expected
  );
  return response;
}

function installCredentialUpdate(
  response: CredentialUpdateResponse,
  startedWith: TransportV2AuthSnapshot,
  targetApiUrl: string
): void {
  if (!response.access_token && !response.refresh_token) return;
  if (!response.access_token || !response.refresh_token) {
    throw new Error("Transport v2 credential update returned an incomplete token pair.");
  }
  installTransportV2Credentials(
    targetApiUrl,
    "user",
    response.access_token,
    response.refresh_token,
    startedWith
  );
}

export type KVListItem = {
  key: string;
  value: string;
  created_at: number;
  updated_at: number;
};

export async function fetchLogin(
  email: string,
  password: string,
  client_id: string
): Promise<LoginResponse> {
  const expected = snapshotTransportV2Auth(apiUrl, "user");
  const response = await encryptedApiCall<
    { email: string; password: string; client_id: string },
    LoginResponse
  >(`${apiUrl}/login`, "POST", { email, password, client_id });
  return installLoginResponse(response, expected);
}

export async function fetchGuestLogin(
  id: string,
  password: string,
  client_id: string
): Promise<LoginResponse> {
  const expected = snapshotTransportV2Auth(apiUrl, "user");
  const response = await encryptedApiCall<
    { id: string; password: string; client_id: string },
    LoginResponse
  >(`${apiUrl}/login`, "POST", { id, password, client_id });
  return installLoginResponse(response, expected);
}

export async function fetchSignUp(
  email: string,
  password: string,
  inviteCode: string,
  client_id: string,
  name?: string | null
): Promise<LoginResponse> {
  const expected = snapshotTransportV2Auth(apiUrl, "user");
  const response = await encryptedApiCall<
    {
      email: string;
      password: string;
      inviteCode: string;
      name?: string | null;
      client_id: string;
    },
    LoginResponse
  >(`${apiUrl}/register`, "POST", {
    email,
    password,
    inviteCode: inviteCode.toLowerCase(),
    client_id,
    name
  });
  return installLoginResponse(response, expected);
}

export async function fetchGuestSignUp(
  password: string,
  inviteCode: string,
  client_id: string
): Promise<LoginResponse> {
  const expected = snapshotTransportV2Auth(apiUrl, "user");
  const response = await encryptedApiCall<
    { password: string; inviteCode: string; client_id: string },
    LoginResponse
  >(`${apiUrl}/register`, "POST", {
    password,
    inviteCode: inviteCode.toLowerCase(),
    client_id
  });
  return installLoginResponse(response, expected);
}

export async function refreshToken(): Promise<RefreshResponse> {
  const refreshed = await transportV2AuthRuntime.refresh(apiUrl, apiPcrConfig, "user");
  return {
    access_token: refreshed.accessToken,
    refresh_token: refreshed.refreshToken
  };
}

export async function fetchUser(): Promise<UserResponse> {
  return authenticatedApiCall<void, UserResponse>(
    `${apiUrl}/protected/user`,
    "GET",
    undefined,
    "Failed to fetch user"
  );
}

/** @internal Keeps React publication tied to the exact authority used for this request. */
export async function fetchUserWithTransportV2Authority(
  targetApiUrl: string,
  pcrConfig: PcrConfig,
  authority: import("./transportV2/authRuntime").TransportV2Authority
) {
  return authenticatedApiCallWithSelectedAuthority<void, UserResponse>(
    `${targetApiUrl}/protected/user`,
    "GET",
    undefined,
    "Failed to fetch user",
    { apiUrl: targetApiUrl, pcrConfig, kind: "user", authority }
  );
}

export async function fetchPut(key: string, value: string): Promise<string> {
  return authenticatedApiCall<string, string>(
    `${apiUrl}/protected/kv/${key}`,
    "PUT",
    value,
    "Failed to put key-value pair"
  );
}

export async function fetchDelete(key: string): Promise<void> {
  return authenticatedApiCall<void, void>(
    `${apiUrl}/protected/kv/${key}`,
    "DELETE",
    undefined,
    "Failed to delete key-value pair"
  );
}

export async function fetchDeleteAllKV(): Promise<void> {
  return authenticatedApiCall<void, void>(
    `${apiUrl}/protected/kv`,
    "DELETE",
    undefined,
    "Failed to delete all key-value pairs"
  );
}

export async function fetchGet(key: string): Promise<string | undefined> {
  try {
    const data = await authenticatedApiCall<void, string>(
      `${apiUrl}/protected/kv/${key}`,
      "GET",
      undefined,
      "Failed to get key-value pair"
    );
    return data;
  } catch (error) {
    console.error(`Error fetching key "${key}":`, error);
    return undefined;
  }
}

export async function fetchList(): Promise<KVListItem[]> {
  return authenticatedApiCall<void, KVListItem[]>(
    `${apiUrl}/protected/kv`,
    "GET",
    undefined,
    "Failed to list key-value pairs"
  );
}

export async function fetchLogout(refresh_token: string): Promise<void> {
  const refreshData = { refresh_token };
  return encryptedApiCall<typeof refreshData, void>(`${apiUrl}/logout`, "POST", refreshData);
}

export async function verifyEmail(code: string): Promise<void> {
  return encryptedApiCall<void, void>(
    `${apiUrl}/verify-email/${code}`,
    "GET",
    undefined,
    undefined,
    "Failed to verify email"
  );
}

export async function requestNewVerificationCode(): Promise<void> {
  return authenticatedApiCall<void, void>(
    `${apiUrl}/protected/request_verification`,
    "POST",
    undefined,
    "Failed to request new verification code"
  );
}

export async function fetchAttestationDocument(
  nonce: string,
  explicitApiUrl?: string
): Promise<string> {
  const url = explicitApiUrl || apiUrl;
  const response = await fetch(`${url}/attestation/${nonce}`);
  if (!response.ok) {
    throw new Error(`Request failed with status ${response.status}`);
  }
  const data = await response.json();
  return data.attestation_document;
}

export async function keyExchange(
  clientPublicKey: string,
  nonce: string,
  explicitApiUrl?: string
): Promise<{ encrypted_session_key: string; session_id: string }> {
  const url = explicitApiUrl || apiUrl;
  const response = await fetch(`${url}/key_exchange`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json"
    },
    body: JSON.stringify({ client_public_key: clientPublicKey, nonce })
  });

  if (!response.ok) {
    throw new Error("Key exchange failed");
  }

  return response.json();
}

export async function requestPasswordReset(
  email: string,
  hashedSecret: string,
  client_id: string
): Promise<void> {
  const resetData = {
    email,
    hashed_secret: hashedSecret,
    client_id
  };
  return encryptedApiCall<typeof resetData, void>(
    `${apiUrl}/password-reset/request`,
    "POST",
    resetData,
    undefined,
    "Failed to request password reset"
  );
}

export async function confirmPasswordReset(
  email: string,
  alphanumericCode: string,
  plaintextSecret: string,
  newPassword: string,
  client_id: string
): Promise<void> {
  const confirmData = {
    email,
    alphanumeric_code: alphanumericCode,
    plaintext_secret: plaintextSecret,
    new_password: newPassword,
    client_id
  };
  return encryptedApiCall<typeof confirmData, void>(
    `${apiUrl}/password-reset/confirm`,
    "POST",
    confirmData,
    undefined,
    "Failed to confirm password reset"
  );
}

export async function changePassword(currentPassword: string, newPassword: string): Promise<void> {
  const targetApiUrl = apiUrl;
  const pcrConfig = snapshotPcrConfig(apiPcrConfig);
  const changePasswordData = {
    current_password: currentPassword,
    new_password: newPassword
  };
  const result = await authenticatedApiCallWithAuthority<
    typeof changePasswordData,
    CredentialUpdateResponse
  >(
    `${targetApiUrl}/protected/change_password`,
    "POST",
    changePasswordData,
    "Failed to change password",
    { apiUrl: targetApiUrl, pcrConfig, kind: "user" }
  );
  installCredentialUpdate(result.data, result.authority.snapshot, result.apiUrl);
}

export async function initiateGitHubAuth(
  client_id: string,
  inviteCode?: string
): Promise<GithubAuthResponse> {
  try {
    return await encryptedApiCall<{ invite_code?: string; client_id: string }, GithubAuthResponse>(
      `${apiUrl}/auth/github`,
      "POST",
      inviteCode ? { invite_code: inviteCode, client_id } : { client_id },
      undefined,
      "Failed to initiate GitHub auth"
    );
  } catch (error) {
    if (error instanceof Error) {
      if (error.message.includes("Invalid invite code")) {
        throw new Error("Invalid invite code. Please check and try again.");
      }
    }
    throw error;
  }
}

export async function handleGitHubCallback(
  code: string,
  state: string,
  inviteCode: string
): Promise<LoginResponse> {
  const expected = snapshotTransportV2Auth(apiUrl, "user");
  const callbackData = { code, state, invite_code: inviteCode };
  try {
    const response = await encryptedApiCall<typeof callbackData, LoginResponse>(
      `${apiUrl}/auth/github/callback`,
      "POST",
      callbackData,
      undefined,
      "GitHub callback failed"
    );
    return installLoginResponse(response, expected);
  } catch (error) {
    console.error("Detailed GitHub callback error:", error);
    if (error instanceof Error) {
      if (
        error.message.includes("User exists") ||
        error.message.includes("Email already registered")
      ) {
        throw new Error(
          "An account with this email already exists. Please sign in using your existing account."
        );
      } else if (error.message.includes("Invalid invite code")) {
        throw new Error("Invalid invite code. Please try signing up with a valid invite code.");
      } else if (error.message.includes("User not found")) {
        throw new Error(
          "User not found. Please sign up first before attempting to log in with GitHub."
        );
      } else {
        throw new Error("Failed to authenticate with GitHub. Please try again.");
      }
    }
    throw error;
  }
}

export type GithubAuthResponse = {
  auth_url: string;
  csrf_token: string;
};

export type GoogleAuthResponse = {
  auth_url: string;
  csrf_token: string;
};

/**
 * Response from initiating Apple OAuth authentication
 * @property auth_url - The Apple authorization URL to redirect the user to
 * @property state - The state parameter used to prevent CSRF attacks
 */
export type AppleAuthResponse = {
  auth_url: string;
  state: string;
};

export async function initiateGoogleAuth(
  client_id: string,
  inviteCode?: string
): Promise<GoogleAuthResponse> {
  try {
    return await encryptedApiCall<{ invite_code?: string; client_id: string }, GoogleAuthResponse>(
      `${apiUrl}/auth/google`,
      "POST",
      inviteCode ? { invite_code: inviteCode, client_id } : { client_id },
      undefined,
      "Failed to initiate Google auth"
    );
  } catch (error) {
    if (error instanceof Error) {
      if (error.message.includes("Invalid invite code")) {
        throw new Error("Invalid invite code. Please check and try again.");
      }
    }
    throw error;
  }
}

export async function handleGoogleCallback(
  code: string,
  state: string,
  inviteCode: string
): Promise<LoginResponse> {
  const expected = snapshotTransportV2Auth(apiUrl, "user");
  const callbackData = { code, state, invite_code: inviteCode };
  try {
    const response = await encryptedApiCall<typeof callbackData, LoginResponse>(
      `${apiUrl}/auth/google/callback`,
      "POST",
      callbackData,
      undefined,
      "Google callback failed"
    );
    return installLoginResponse(response, expected);
  } catch (error) {
    console.error("Detailed Google callback error:", error);
    if (error instanceof Error) {
      if (
        error.message.includes("User exists") ||
        error.message.includes("Email already registered")
      ) {
        throw new Error(
          "An account with this email already exists. Please sign in using your existing account."
        );
      } else if (error.message.includes("Invalid invite code")) {
        throw new Error("Invalid invite code. Please try signing up with a valid invite code.");
      } else if (error.message.includes("User not found")) {
        throw new Error(
          "User not found. Please sign up first before attempting to log in with Google."
        );
      } else {
        throw new Error("Failed to authenticate with Google. Please try again.");
      }
    }
    throw error;
  }
}

/**
 * Initiates Apple OAuth authentication flow
 * @param client_id - The client ID for your OpenSecret project
 * @param inviteCode - Optional invite code for new user registration
 * @returns A promise resolving to the Apple auth response containing auth URL and state
 * @description
 * This function starts the Apple OAuth authentication process by:
 * 1. Generating a secure state parameter to prevent CSRF attacks
 * 2. Getting an authorization URL from the OpenSecret backend
 * 3. Returning the URL that the client should redirect to
 *
 * After the user authenticates with Apple, they will be redirected back to your application.
 * The handleAppleCallback function should be used to complete the authentication process.
 */
export async function initiateAppleAuth(
  client_id: string,
  inviteCode?: string
): Promise<AppleAuthResponse> {
  try {
    return await encryptedApiCall<{ invite_code?: string; client_id: string }, AppleAuthResponse>(
      `${apiUrl}/auth/apple`,
      "POST",
      inviteCode ? { invite_code: inviteCode, client_id } : { client_id },
      undefined,
      "Failed to initiate Apple auth"
    );
  } catch (error) {
    if (error instanceof Error) {
      if (error.message.includes("Invalid invite code")) {
        throw new Error("Invalid invite code. Please check and try again.");
      }
    }
    throw error;
  }
}

/**
 * Completes Apple OAuth authentication after user is redirected back to your app
 * @param code - The authorization code from Apple
 * @param state - The state parameter returned by Apple (should match the original state)
 * @param inviteCode - Invite code for new user registration
 * @returns A promise resolving to login response with access and refresh tokens
 * @description
 * This function completes the Apple OAuth authentication process by:
 * 1. Validating the state parameter to prevent CSRF attacks
 * 2. Exchanging the authorization code for tokens
 * 3. Creating or authenticating the user account
 *
 * This function should be called in your OAuth callback route after
 * the user is redirected back from Apple's authentication page.
 */
export async function handleAppleCallback(
  code: string,
  state: string,
  inviteCode: string
): Promise<LoginResponse> {
  const expected = snapshotTransportV2Auth(apiUrl, "user");
  const callbackData = { code, state, invite_code: inviteCode };
  try {
    const response = await encryptedApiCall<typeof callbackData, LoginResponse>(
      `${apiUrl}/auth/apple/callback`,
      "POST",
      callbackData,
      undefined,
      "Apple callback failed"
    );
    return installLoginResponse(response, expected);
  } catch (error) {
    console.error("Detailed Apple callback error:", error);
    if (error instanceof Error) {
      if (
        error.message.includes("User exists") ||
        error.message.includes("Email already registered")
      ) {
        throw new Error(
          "An account with this email already exists. Please sign in using your existing account."
        );
      } else if (error.message.includes("Invalid invite code")) {
        throw new Error("Invalid invite code. Please try signing up with a valid invite code.");
      } else if (error.message.includes("User not found")) {
        throw new Error(
          "User not found. Please sign up first before attempting to log in with Apple."
        );
      } else {
        throw new Error("Failed to authenticate with Apple. Please try again.");
      }
    }
    throw error;
  }
}

/**
 * Apple user information returned from native Apple Sign-In
 * @property user_identifier - The user's unique ID from Apple
 * @property identity_token - The JWT token from Apple used for authentication
 * @property email - Optional email address (only provided on first sign-in)
 * @property given_name - Optional user's first name (only provided on first sign-in)
 * @property family_name - Optional user's last name (only provided on first sign-in)
 * @property nonce - Optional nonce for preventing replay attacks
 */
export type AppleUser = {
  user_identifier: string; // The user's unique ID from Apple
  identity_token: string; // The JWT token from Apple
  email?: string; // Optional: only provided on first sign-in
  given_name?: string; // Optional: only provided on first sign-in
  family_name?: string; // Optional: only provided on first sign-in
  nonce?: string; // Optional: nonce for validating the token
};

/**
 * Handles native Apple Sign-In for iOS devices
 * @param appleUser - Apple user data from the native Sign in with Apple API
 * @param client_id - The client ID for your OpenSecret project
 * @param inviteCode - Optional invite code for new user registration
 * @returns A promise resolving to login response with access and refresh tokens
 * @description
 * This function is specifically for use with iOS native Sign in with Apple:
 * 1. Validates the Apple identity token and user information
 * 2. Creates or authenticates the user account
 * 3. Returns authentication tokens
 *
 * Unlike OAuth flow, this method doesn't require redirects and is used
 * directly with the credential data from Apple's native authentication.
 *
 * Note: Email and name information are only provided by Apple on the first
 * authentication. Your backend should store this information for future use.
 *
 * The nonce parameter (optional) can be provided as part of the appleUser object.
 * When using Sign in with Apple, you can generate a nonce on your client and pass
 * it both to Apple during authentication initiation and to this function for validation.
 * The backend will verify that the nonce in the JWT matches what was provided.
 */
export async function handleAppleNativeSignIn(
  appleUser: AppleUser,
  client_id: string,
  inviteCode?: string
): Promise<LoginResponse> {
  const expected = snapshotTransportV2Auth(apiUrl, "user");
  // Combine the Apple user data with our app's client ID
  const signInData = {
    ...appleUser,
    client_id,
    ...(inviteCode ? { invite_code: inviteCode } : {})
  };

  try {
    const response = await encryptedApiCall<typeof signInData, LoginResponse>(
      `${apiUrl}/auth/apple/native`,
      "POST",
      signInData,
      undefined,
      "Apple Sign-In failed"
    );
    return installLoginResponse(response, expected);
  } catch (error) {
    console.error("Detailed Apple Sign-In error:", error);
    if (error instanceof Error) {
      if (
        error.message.includes("User exists") ||
        error.message.includes("Email already registered")
      ) {
        throw new Error(
          "An account with this email already exists. Please sign in using your existing account."
        );
      } else if (error.message.includes("Invalid invite code")) {
        throw new Error("Invalid invite code. Please try signing up with a valid invite code.");
      } else if (error.message.includes("User not found")) {
        throw new Error(
          "User not found. Please sign up first before attempting to log in with Apple."
        );
      } else if (error.message.includes("No email found")) {
        throw new Error("Unable to retrieve email from Apple. Please try another sign-in method.");
      } else {
        throw new Error("Failed to authenticate with Apple. Please try again.");
      }
    }
    throw error;
  }
}

export type PrivateKeyResponse = {
  /** 12-word BIP39 mnemonic phrase */
  mnemonic: string;
};

export type PrivateKeyBytesResponse = {
  /** 32-byte hex string (64 characters) representing the private key */
  private_key: string;
};

/**
 * Options for key derivation operations
 */
export type KeyOptions = {
  /**
   * BIP-85 derivation path to derive a child mnemonic
   * Examples: "m/83696968'/39'/0'/12'/0'"
   */
  seed_phrase_derivation_path?: string;

  /**
   * BIP-32 derivation path to derive a child key from the master (or BIP-85 derived) seed
   * Examples: "m/44'/0'/0'/0/0"
   */
  private_key_derivation_path?: string;
};

/**
 * Fetches the private key as a mnemonic phrase with optional derivation paths
 * @param key_options - Optional key derivation options (see KeyOptions type)
 *
 * @returns A promise resolving to the private key response containing the mnemonic
 *
 * @description
 * - If seed_phrase_derivation_path is provided, a child mnemonic is derived via BIP-85
 * - If seed_phrase_derivation_path is omitted, the master mnemonic is returned
 * - BIP-85 allows deriving child mnemonics from a master seed in a deterministic way
 * - Common BIP-85 path format: m/83696968'/39'/0'/[entropy in bits]'/[index]'
 *   where entropy is typically 12' for 12-word mnemonics
 */
export async function fetchPrivateKey(key_options?: KeyOptions): Promise<PrivateKeyResponse> {
  // Build URL with query parameters
  let url = `${apiUrl}/protected/private_key`;
  const queryParams = [];

  // Add seed phrase derivation path if present
  if (key_options?.seed_phrase_derivation_path) {
    queryParams.push(
      `seed_phrase_derivation_path=${encodeURIComponent(key_options.seed_phrase_derivation_path)}`
    );
  }

  // Add private key derivation path if present
  if (key_options?.private_key_derivation_path) {
    queryParams.push(
      `private_key_derivation_path=${encodeURIComponent(key_options.private_key_derivation_path)}`
    );
  }

  // Append query parameters if any exist
  if (queryParams.length > 0) {
    url += `?${queryParams.join("&")}`;
  }

  return authenticatedApiCall<void, PrivateKeyResponse>(
    url,
    "GET",
    undefined,
    "Failed to fetch private key"
  );
}

/**
 * Fetches private key bytes for the given derivation options
 * @param key_options - Key derivation options (see KeyOptions type)
 *
 * @returns A promise resolving to the private key bytes response
 *
 * @description
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
 *    - This allows for more complex derivation schemes
 */
export async function fetchPrivateKeyBytes(
  key_options?: KeyOptions
): Promise<PrivateKeyBytesResponse> {
  // Build URL with query parameters
  let url = `${apiUrl}/protected/private_key_bytes`;
  const queryParams = [];

  // Add seed phrase derivation path if present
  if (key_options?.seed_phrase_derivation_path) {
    queryParams.push(
      `seed_phrase_derivation_path=${encodeURIComponent(key_options.seed_phrase_derivation_path)}`
    );
  }

  // Add private key derivation path if present
  if (key_options?.private_key_derivation_path) {
    queryParams.push(
      `private_key_derivation_path=${encodeURIComponent(key_options.private_key_derivation_path)}`
    );
  }

  // Append query parameters if any exist
  if (queryParams.length > 0) {
    url += `?${queryParams.join("&")}`;
  }

  return authenticatedApiCall<void, PrivateKeyBytesResponse>(
    url,
    "GET",
    undefined,
    "Failed to fetch private key bytes"
  );
}

export type SignMessageResponse = {
  /** Signature in hex format */
  signature: string;
  /** Message hash in hex format */
  message_hash: string;
};

type SigningAlgorithm = "schnorr" | "ecdsa";

export type SignMessageRequest = {
  /** Base64-encoded message to sign */
  message_base64: string;
  /** Signing algorithm to use (schnorr or ecdsa) */
  algorithm: SigningAlgorithm;
  /** Optional key derivation options */
  key_options?: {
    /** Optional BIP32 derivation path (e.g., "m/44'/0'/0'/0/0") */
    private_key_derivation_path?: string;
    /** Optional BIP-85 seed phrase derivation path (e.g., "m/83696968'/39'/0'/12'/0'") */
    seed_phrase_derivation_path?: string;
  };
};

/**
 * Signs a message using the specified algorithm and derivation options
 * @param message_bytes - Message to sign as Uint8Array
 * @param algorithm - Signing algorithm (schnorr or ecdsa)
 * @param key_options - Key derivation options (see KeyOptions type)
 *
 * @returns A promise resolving to the signature response
 *
 * @description
 * This function supports multiple signing approaches:
 *
 * 1. Sign with master key (no derivation parameters)
 *
 * 2. Sign with BIP-32 derived key
 *    - Derives a child key from the master seed using BIP-32
 *
 * 3. Sign with BIP-85 derived key
 *    - Derives a child mnemonic using BIP-85, then uses its master key
 *
 * 4. Sign with combined BIP-85 and BIP-32 derivation
 *    - First derives a child mnemonic via BIP-85
 *    - Then applies BIP-32 derivation to derive a key from that seed
 *
 * Example message preparation:
 * ```typescript
 * // From string
 * const messageBytes = new TextEncoder().encode("Hello, World!");
 *
 * // From hex
 * const messageBytes = new Uint8Array(Buffer.from("deadbeef", "hex"));
 * ```
 */
export async function signMessage(
  message_bytes: Uint8Array,
  algorithm: SigningAlgorithm,
  key_options?: KeyOptions
): Promise<SignMessageResponse> {
  const message_base64 = encode(message_bytes);

  const requestData = {
    message_base64,
    algorithm,
    ...(key_options && Object.keys(key_options).length > 0 && { key_options })
  };

  return authenticatedApiCall<typeof requestData, SignMessageResponse>(
    `${apiUrl}/protected/sign_message`,
    "POST",
    requestData,
    "Failed to sign message"
  );
}

export type PublicKeyResponse = {
  /** Public key in hex format */
  public_key: string;
  /** The algorithm used (schnorr or ecdsa) */
  algorithm: SigningAlgorithm;
};

/**
 * Retrieves the public key for a given algorithm and derivation options
 * @param algorithm - Signing algorithm (schnorr or ecdsa)
 * @param key_options - Key derivation options (see KeyOptions type)
 *
 * @returns A promise resolving to the public key response
 *
 * @description
 * The derivation paths determine which key is used to generate the public key:
 *
 * 1. Master key (no derivation parameters)
 *    - Returns the public key corresponding to the master private key
 *
 * 2. BIP-32 derived key
 *    - Returns the public key for a derived child key
 *    - Useful for:
 *      - Separating keys by purpose (e.g., different chains or applications)
 *      - Generating deterministic addresses
 *      - Supporting different address formats (Legacy, SegWit, Native SegWit, Taproot)
 *
 * 3. BIP-85 derived key
 *    - Returns the public key for the master key of a BIP-85 derived seed
 *
 * 4. Combined BIP-85 and BIP-32 derivation
 *    - First derives a child mnemonic via BIP-85
 *    - Then applies BIP-32 derivation to get the corresponding public key
 */
export async function fetchPublicKey(
  algorithm: SigningAlgorithm,
  key_options?: KeyOptions
): Promise<PublicKeyResponse> {
  // Build URL with query parameters
  let url = `${apiUrl}/protected/public_key?algorithm=${algorithm}`;

  // Add seed phrase derivation path if present
  if (key_options?.seed_phrase_derivation_path) {
    url += `&seed_phrase_derivation_path=${encodeURIComponent(key_options.seed_phrase_derivation_path)}`;
  }

  // Add private key derivation path if present
  if (key_options?.private_key_derivation_path) {
    url += `&private_key_derivation_path=${encodeURIComponent(key_options.private_key_derivation_path)}`;
  }

  return authenticatedApiCall<void, PublicKeyResponse>(
    url,
    "GET",
    undefined,
    "Failed to fetch public key"
  );
}

export type ThirdPartyTokenRequest = {
  audience?: string;
};

export type ThirdPartyTokenResponse = {
  token: string;
};

/**
 * Generates a JWT token for use with third-party services
 * @param audience - Optional audience value for the target service
 * @returns A promise resolving to the token response containing the JWT
 *
 * @description
 * - If audience is provided, it can be a valid URL or another accepted audience string
 * - If audience is omitted, a token with no audience restriction will be generated
 */
export async function generateThirdPartyToken(audience?: string): Promise<ThirdPartyTokenResponse> {
  return authenticatedApiCall<ThirdPartyTokenRequest, ThirdPartyTokenResponse>(
    `${apiUrl}/protected/third_party_token`,
    "POST",
    audience ? { audience } : {},
    "Failed to generate third party token"
  );
}

export type EncryptDataRequest = {
  data: string;
  key_options?: {
    private_key_derivation_path?: string;
    seed_phrase_derivation_path?: string;
  };
};

export type EncryptDataResponse = {
  encrypted_data: string;
};

/**
 * Encrypts arbitrary string data using the user's private key
 * @param data - String content to be encrypted
 * @param key_options - Key derivation options (see KeyOptions type)
 * @returns A promise resolving to the encrypted data response
 * @throws {Error} If:
 * - The derivation paths are invalid
 * - Authentication fails
 * - Server-side encryption error occurs
 *
 * @description
 * This function supports multiple encryption approaches:
 *
 * 1. Encrypt with master key (no derivation parameters)
 *
 * 2. Encrypt with BIP-32 derived key
 *    - Derives a child key from the master seed using BIP-32
 *
 * 3. Encrypt with BIP-85 derived key
 *    - Derives a child mnemonic using BIP-85, then uses its master key
 *
 * 4. Encrypt with combined BIP-85 and BIP-32 derivation
 *    - First derives a child mnemonic via BIP-85
 *    - Then applies BIP-32 derivation to derive a key from that seed
 *
 * Technical details:
 * - Encrypts data with AES-256-GCM
 * - A random nonce is generated for each encryption operation (included in the result)
 * - The encrypted_data format:
 *   - First 12 bytes: Nonce used for encryption (prepended to ciphertext)
 *   - Remaining bytes: AES-256-GCM encrypted ciphertext + authentication tag
 *   - The entire payload is base64-encoded for safe transport
 */
export async function encryptData(
  data: string,
  key_options?: KeyOptions
): Promise<EncryptDataResponse> {
  const requestData = {
    data,
    ...(key_options && Object.keys(key_options).length > 0 && { key_options })
  };

  return authenticatedApiCall<typeof requestData, EncryptDataResponse>(
    `${apiUrl}/protected/encrypt`,
    "POST",
    requestData,
    "Failed to encrypt data"
  );
}

export type DecryptDataRequest = {
  encrypted_data: string;
  key_options?: {
    private_key_derivation_path?: string;
    seed_phrase_derivation_path?: string;
  };
};

/**
 * Decrypts data that was previously encrypted with the user's key
 * @param encryptedData - Base64-encoded encrypted data string
 * @param key_options - Key derivation options (see KeyOptions type)
 * @returns A promise resolving to the decrypted string
 * @throws {Error} If:
 * - The encrypted data is malformed
 * - The derivation paths are invalid
 * - Authentication fails
 * - Server-side decryption error occurs
 *
 * @description
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
 * IMPORTANT: You must use the exact same derivation paths for decryption
 * that were used for encryption.
 *
 * Technical details:
 * - Uses AES-256-GCM decryption with authentication tag verification
 * - Extracts the nonce from the first 12 bytes of the encrypted data
 * - The encrypted_data must be in the exact format returned by the encrypt endpoint
 */
export async function decryptData(
  encryptedData: string,
  key_options?: KeyOptions
): Promise<string> {
  const requestData = {
    encrypted_data: encryptedData,
    ...(key_options && Object.keys(key_options).length > 0 && { key_options })
  };

  return authenticatedApiCall<typeof requestData, string>(
    `${apiUrl}/protected/decrypt`,
    "POST",
    requestData,
    "Failed to decrypt data"
  );
}

/**
 * Initiates the account deletion process for logged-in users
 * @param hashedSecret - Client-side hashed secret for verification
 * @returns A promise resolving to void
 *
 * @description
 * This function:
 * 1. Requires the user to be logged in (uses authenticatedApiCall)
 * 2. Sends a verification email to the user's email address
 * 3. The email contains a confirmation code that will be needed for confirmation
 * 4. The client must store the plaintext secret for confirmation
 */
export async function requestAccountDeletion(hashedSecret: string): Promise<void> {
  const deleteData = {
    hashed_secret: hashedSecret
  };
  return authenticatedApiCall<typeof deleteData, void>(
    `${apiUrl}/protected/delete-account/request`,
    "POST",
    deleteData,
    "Failed to request account deletion"
  );
}

/**
 * Confirms and completes the account deletion process
 * @param confirmationCode - The confirmation code from the verification email
 * @param plaintextSecret - The plaintext secret that was hashed in the request step
 * @returns A promise resolving to void
 *
 * @description
 * This function:
 * 1. Requires the user to be logged in (uses authenticatedApiCall)
 * 2. Verifies both the confirmation code from email and the secret known only to the client
 * 3. Permanently deletes the user account and all associated data
 * 4. After successful deletion, the client should clear all local storage and tokens
 */
export async function confirmAccountDeletion(
  confirmationCode: string,
  plaintextSecret: string
): Promise<void> {
  const confirmData = {
    confirmation_code: confirmationCode,
    plaintext_secret: plaintextSecret
  };
  return authenticatedApiCall<typeof confirmData, void>(
    `${apiUrl}/protected/delete-account/confirm`,
    "POST",
    confirmData,
    "Failed to confirm account deletion"
  );
}

type ModelsListResponse = {
  object: "list";
  data: Model[];
};

export type ModelAliasId = "auto:quick" | "auto:powerful";
export type ModelId = ModelAliasId | (string & {});
export type ModelAccessTier = "free" | "starter" | "pro";

function isModelAliasId(value: unknown): value is ModelAliasId {
  return value === "auto:quick" || value === "auto:powerful";
}

export type ModelCapabilities = {
  chat: boolean;
  vision: boolean;
  reasoning: boolean;
  tool_use: boolean;
};

export type ModelCatalogItem = Model & {
  provider?: string;
  provider_id?: string;
  display_name: string;
  short_name: string;
  description?: string;
  context_window: number;
  max_context_tokens: number;
  access: ModelAccessTier;
  capabilities: ModelCapabilities;
  tasks?: string[];
  badges?: string[];
  enabled: boolean;
  deprecated: boolean;
  sort_order?: number;
};

export type ModelAlias = {
  id: ModelAliasId;
  label: string;
  short_name: string;
  description: string;
  target_model: string;
  access: ModelAccessTier;
  capabilities: ModelCapabilities;
};

export type ModelCatalogResponse = {
  object: "list";
  data: ModelCatalogItem[];
  aliases: ModelAlias[];
  defaults: {
    quick: ModelAliasId;
    powerful: ModelAliasId;
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

/**
 * Fetches available AI models from the OpenAI-compatible API
 * @param apiKey - Optional API key to use instead of a stored JWT token
 * @returns A promise resolving to an array of Model objects
 * @throws {Error} If:
 * - The request fails
 * - The response format is invalid
 *
 * When no API key or stored JWT is available, the request uses the attested
 * encrypted session without an Authorization header. Credential-bearing calls
 * stay on the authenticated path and are never retried anonymously if
 * validation fails.
 */
export async function fetchModels(apiKey?: string): Promise<Model[]> {
  try {
    const hasIdentityCredential =
      apiKey !== undefined || readTransportV2Credentials(apiUrl, "user") !== null;
    const response = hasIdentityCredential
      ? await openAiAuthenticatedApiCall<void, ModelsListResponse>(
          `${apiUrl}/v1/models`,
          "GET",
          undefined,
          "Failed to fetch models",
          apiKey
        )
      : await encryptedApiCall<void, ModelsListResponse>(
          `${apiUrl}/v1/models`,
          "GET",
          undefined,
          undefined,
          "Failed to fetch models"
        );

    // Validate response structure
    if (!response || typeof response !== "object") {
      throw new Error("Invalid response from models endpoint");
    }

    if (response.object !== "list" || !Array.isArray(response.data)) {
      throw new Error("Models response missing expected 'object' or 'data' fields");
    }

    return response.data;
  } catch (error) {
    console.error("Error fetching models:", error);
    throw error;
  }
}

/**
 * Fetches the OpenSecret model catalog with display metadata and stable aliases.
 * @param apiKey - Optional API key to use instead of JWT token
 * @returns A promise resolving to the model catalog response
 */
export async function fetchModelCatalog(apiKey?: string): Promise<ModelCatalogResponse> {
  try {
    const response = await openAiAuthenticatedApiCall<void, ModelCatalogResponse>(
      `${apiUrl}/v1/models/catalog`,
      "GET",
      undefined,
      "Failed to fetch model catalog",
      apiKey
    );

    if (!response || typeof response !== "object") {
      throw new Error("Invalid response from model catalog endpoint");
    }

    if (response.object !== "list" || !Array.isArray(response.data)) {
      throw new Error("Model catalog response missing expected 'object' or 'data' fields");
    }

    if (!Array.isArray(response.aliases)) {
      throw new Error("Model catalog response missing expected 'aliases' field");
    }

    if (
      !response.defaults ||
      typeof response.defaults !== "object" ||
      !isModelAliasId(response.defaults.quick) ||
      !isModelAliasId(response.defaults.powerful)
    ) {
      throw new Error("Model catalog response missing expected 'defaults' field");
    }

    return response;
  } catch (error) {
    console.error("Error fetching model catalog:", error);
    throw error;
  }
}

export type DocumentUploadRequest = {
  filename: string;
  content_base64: string;
};

export type DocumentResponse = {
  text: string;
  filename: string;
  size: number;
};

export type DocumentUploadInitResponse = {
  task_id: string;
  filename: string;
  size: number;
};

export type ApiKey = {
  name: string;
  created_at: string;
};

export type ApiKeyCreateResponse = ApiKey & {
  key: string; // Only returned on creation - UUID with dashes
};

export type ApiKeyListResponse = ApiKey[];

export type DocumentStatusRequest = {
  task_id: string;
};

export type DocumentStatusResponse = {
  status: string; // "pending", "started", "success", "failure"
  progress?: number;
  error?: string;
  document?: DocumentResponse;
};

const MAX_FILE_SIZE = 10 * 1024 * 1024; // 10MB

/**
 * Utility function to delay execution for a specified time
 * @param ms - Milliseconds to delay
 * @returns Promise that resolves after the delay
 */
function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Creates a new API key for the authenticated user
 * @param name - A descriptive name for the API key
 * @returns A promise resolving to the API key details with the key value (only shown once)
 * @throws {Error} If:
 * - The user is not authenticated
 * - The name is invalid
 * - The request fails
 *
 * @description
 * IMPORTANT: The `key` field is only returned once during creation and cannot be retrieved again.
 * The SDK consumer should prompt users to save the key immediately.
 *
 * Example usage:
 * ```typescript
 * const apiKey = await createApiKey("Production Key");
 * console.log(apiKey.key); // UUID format: 550e8400-e29b-41d4-a716-446655440000
 * // Save this key securely - it won't be shown again!
 * ```
 */
export async function createApiKey(name: string): Promise<ApiKeyCreateResponse> {
  return authenticatedApiCall<{ name: string }, ApiKeyCreateResponse>(
    `${apiUrl}/protected/api-keys`,
    "POST",
    { name },
    "Failed to create API key"
  );
}

/**
 * Lists all API keys for the authenticated user
 * @returns A promise resolving to an object containing an array of API key metadata (without the actual keys)
 * @throws {Error} If:
 * - The user is not authenticated
 * - The request fails
 *
 * @description
 * Returns metadata about all API keys associated with the user's account.
 * Note that the actual key values are never returned - they are only shown once during creation.
 * The keys are sorted by created_at in descending order (newest first).
 *
 * Example usage:
 * ```typescript
 * const response = await listApiKeys();
 * response.keys.forEach(key => {
 *   console.log(`${key.name} created at ${key.created_at}`);
 * });
 * ```
 */
export async function listApiKeys(): Promise<{ keys: ApiKeyListResponse }> {
  const response = await authenticatedApiCall<void, { keys: ApiKeyListResponse }>(
    `${apiUrl}/protected/api-keys`,
    "GET",
    undefined,
    "Failed to list API keys"
  );

  // Sort by created_at descending (newest first)
  response.keys.sort((a, b) => new Date(b.created_at).getTime() - new Date(a.created_at).getTime());

  return response;
}

/**
 * Deletes an API key by its name
 * @param name - The name of the API key to delete
 * @returns A promise resolving to void
 * @throws {Error} If:
 * - The user is not authenticated
 * - The API key with this name is not found
 * - The user doesn't own the API key
 * - The request fails
 *
 * @description
 * Permanently deletes an API key. This action cannot be undone.
 * Any requests using the deleted key will immediately fail with 401 Unauthorized.
 * Names are unique per user, so this uniquely identifies the key to delete.
 *
 * Example usage:
 * ```typescript
 * await deleteApiKey("Production Key");
 * console.log("API key deleted successfully");
 * ```
 */
export async function deleteApiKey(name: string): Promise<void> {
  // URL-encode the name to handle special characters
  const encodedName = encodeURIComponent(name);
  return authenticatedApiCall<void, void>(
    `${apiUrl}/protected/api-keys/${encodedName}`,
    "DELETE",
    undefined,
    "Failed to delete API key"
  );
}

/**
 * Uploads a document for text extraction and processing
 * @param file - The file to upload (File or Blob object)
 * @returns A promise resolving to the task ID and initial metadata
 * @throws {Error} If:
 * - The file exceeds 10MB size limit
 * - The user is not authenticated
 * - The user is a guest (401)
 * - Usage limits are exceeded (403)
 * - Processing fails (500)
 *
 * @description
 * This function uploads a document to the Tinfoil processing service which:
 * 1. Accepts the document and returns a task ID immediately
 * 2. Processes the document asynchronously in the background
 * 3. Maintains end-to-end encryption using session keys
 *
 * The file is converted to base64 before upload due to encryption requirements.
 * Common supported formats include PDF, DOCX, XLSX, PPTX, TXT, RTF, and more.
 *
 * Example usage:
 * ```typescript
 * const file = new File(["content"], "document.pdf", { type: "application/pdf" });
 * const result = await uploadDocument(file);
 * console.log(result.task_id); // Task ID to check status
 * ```
 */
export async function uploadDocument(file: File | Blob): Promise<DocumentUploadInitResponse> {
  // Validate file size
  if (file.size > MAX_FILE_SIZE) {
    throw new Error(`File size exceeds maximum limit of ${MAX_FILE_SIZE / 1024 / 1024}MB`);
  }

  // Convert file to base64
  const arrayBuffer = await file.arrayBuffer();
  const bytes = new Uint8Array(arrayBuffer);
  const base64Data = encode(bytes);

  // Get filename
  const filename = file instanceof File ? file.name : "document";

  const requestData: DocumentUploadRequest = {
    filename,
    content_base64: base64Data
  };

  return authenticatedApiCall<DocumentUploadRequest, DocumentUploadInitResponse>(
    `${apiUrl}/v1/documents/upload`,
    "POST",
    requestData,
    "Failed to upload document"
  );
}

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
 * const status = await checkDocumentStatus(taskId);
 * if (status.status === "success" && status.document) {
 *   console.log(status.document.text);
 * }
 * ```
 */
export async function checkDocumentStatus(taskId: string): Promise<DocumentStatusResponse> {
  const requestData: DocumentStatusRequest = {
    task_id: taskId
  };

  return authenticatedApiCall<DocumentStatusRequest, DocumentStatusResponse>(
    `${apiUrl}/v1/documents/status`,
    "POST",
    requestData,
    "Failed to check document status"
  );
}

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
 * It uploads the document, then polls the status endpoint until processing completes.
 *
 * Options:
 * - pollInterval: Time between status checks in milliseconds (default: 2000)
 * - maxAttempts: Maximum number of status checks before timeout (default: 150 = 5 minutes)
 * - onProgress: Callback function called on each status update
 *
 * Example usage:
 * ```typescript
 * const file = new File(["content"], "document.pdf", { type: "application/pdf" });
 * const result = await uploadDocumentWithPolling(file, {
 *   onProgress: (status, progress) => {
 *     console.log(`Status: ${status}, Progress: ${progress || 0}%`);
 *   }
 * });
 * console.log(result.text);
 * ```
 */
export async function uploadDocumentWithPolling(
  file: File | Blob,
  options?: {
    pollInterval?: number; // milliseconds, default 2000
    maxAttempts?: number; // default 150 (5 minutes with 2s interval)
    onProgress?: (status: string, progress?: number) => void;
  }
): Promise<DocumentResponse> {
  const { pollInterval = 2000, maxAttempts = 150, onProgress } = options || {};

  // Upload the document and get task ID
  const initResponse = await uploadDocument(file);
  let attempts = 0;

  // Poll for completion
  while (attempts < maxAttempts) {
    const statusResponse = await checkDocumentStatus(initResponse.task_id);

    if (onProgress) {
      onProgress(statusResponse.status, statusResponse.progress);
    }

    switch (statusResponse.status) {
      case "success":
        if (!statusResponse.document) {
          throw new Error("Document processing succeeded but no document returned");
        }
        return statusResponse.document;

      case "failure":
        throw new Error(statusResponse.error || "Document processing failed");

      case "pending":
      case "started":
        // Continue polling
        await delay(pollInterval);
        attempts++;
        break;

      default:
        throw new Error(`Unknown document status: ${statusResponse.status}`);
    }
  }

  throw new Error("Document processing timed out");
}

export type WhisperTranscriptionRequest = {
  file: string; // Base64-encoded audio file data
  filename: string;
  content_type: string;
  model: string;
  language?: string;
  prompt?: string;
  temperature?: number;
};

export type WhisperTranscriptionResponse = {
  text: string;
};

/**
 * Transcribes audio using the Whisper API
 * @param file - The audio file to transcribe (File or Blob object)
 * @param model - Model to use (default: "whisper-large-v3")
 * @param language - Optional ISO-639-1 language code (e.g., "en", "es", "fr")
 * @param prompt - Optional context or previous segment transcript
 * @param temperature - Sampling temperature between 0 and 1
 * @param apiKey - Optional API key to use instead of JWT token
 * @returns A promise resolving to the transcription response
 * @throws {Error} If:
 * - The user is not authenticated
 * - The file cannot be read
 * - The transcription fails
 *
 * @description
 * This function transcribes audio using OpenAI's Whisper model via the encrypted API.
 * Default model "whisper-large-v3" routes to Tinfoil's whisper-large-v3-turbo.
 *
 * Supported audio formats:
 * - MP3 (audio/mpeg)
 * - WAV (audio/wav)
 * - MP4 (audio/mp4)
 * - M4A (audio/m4a)
 * - FLAC (audio/flac)
 * - OGG (audio/ogg)
 * - WEBM (audio/webm)
 *
 * Example usage:
 * ```typescript
 * const audioFile = new File([audioData], "recording.mp3", { type: "audio/mpeg" });
 * const result = await transcribeAudio(audioFile, undefined, "en", "Meeting about Q4");
 * console.log(result.text);
 * ```
 */
export async function transcribeAudio(
  file: File | Blob,
  model?: string,
  language?: string,
  prompt?: string,
  temperature?: number,
  apiKey?: string
): Promise<WhisperTranscriptionResponse> {
  // Convert file to base64
  const arrayBuffer = await file.arrayBuffer();
  const bytes = new Uint8Array(arrayBuffer);
  const base64Data = encode(bytes);

  // Get filename and content type
  const filename = file instanceof File ? file.name : "audio";
  const contentType = file.type || "audio/mpeg";

  const requestData: WhisperTranscriptionRequest = {
    file: base64Data,
    filename,
    content_type: contentType,
    model: model || "whisper-large-v3",
    ...(language && { language }),
    ...(prompt && { prompt }),
    ...(temperature !== undefined && { temperature })
  };

  // Use openAiAuthenticatedApiCall to support both JWT and API key auth
  return openAiAuthenticatedApiCall<WhisperTranscriptionRequest, WhisperTranscriptionResponse>(
    `${apiUrl}/v1/audio/transcriptions`,
    "POST",
    requestData,
    "Failed to transcribe audio",
    apiKey
  );
}

export type ResponsesRetrieveResponse = {
  id: string;
  object: "response";
  created_at: number;
  status: "queued" | "in_progress" | "completed" | "failed" | "cancelled";
  model: string;
  usage?: {
    input_tokens: number;
    input_tokens_details: {
      cached_tokens: number;
    };
    output_tokens: number;
    output_tokens_details: {
      reasoning_tokens: number;
    };
    total_tokens: number;
  };
  output?: string | unknown[];
};

export type ThreadListItem = {
  id: string;
  object: "thread";
  created_at: number;
  updated_at: number;
  title: string;
};

export type ResponsesListResponse = {
  object: "list";
  data: ThreadListItem[];
  has_more: boolean;
  first_id?: string;
  last_id?: string;
};

export type ResponsesListParams = {
  limit?: number; // 1-100, default: 20
  after?: string; // UUID cursor for forward pagination
  before?: string; // UUID cursor for backward pagination
  order?: string; // Currently not implemented, reserved for future use
};

export type ConversationItem = {
  id: string;
  type: "message";
  status: "completed" | "in_progress" | "incomplete";
  role: "user" | "assistant" | "system";
  content: Array<{
    type: "text" | "input_text" | "input_audio" | "item";
    text?: string;
    audio?: string;
    transcript?: string;
    id?: string;
  }>;
};

export type Conversation = {
  id: string;
  object: "conversation";
  created_at: number;
  metadata?: Record<string, unknown>;
  project_id?: string | null;
  pinned: boolean;
  last_activity_at: number;
};

export type ConversationCreateOptions = {
  project_id?: string;
  pinned?: boolean;
};

export type ConversationCreateRequest = {
  metadata?: Record<string, unknown>;
  project_id?: string;
  pinned?: boolean;
};

export type ConversationUpdateOptions = {
  project_id?: string | null;
  pinned?: boolean;
};

export type ConversationUpdateRequest = {
  metadata?: Record<string, unknown>;
  project_id?: string | null;
  pinned?: boolean;
};

export type ConversationItemsResponse = {
  object: "list";
  data: ConversationItem[];
  first_id?: string;
  last_id?: string;
  has_more: boolean;
};

export type ConversationsListResponse = {
  object: "list";
  data: Conversation[];
  first_id?: string;
  last_id?: string;
  has_more: boolean;
};

export type ConversationDeleteResponse = {
  id: string;
  object: "conversation.deleted";
  deleted: boolean;
};

export type ConversationsDeleteResponse = {
  object: "list.deleted";
  deleted: boolean;
};

export type BatchDeleteConversationsRequest = {
  ids: string[];
};

export type BatchDeleteItemResult = {
  id: string;
  object: "conversation.deleted";
  deleted: boolean;
  error?: "not_found" | "delete_failed";
};

export type BatchDeleteConversationsResponse = {
  object: "list";
  data: BatchDeleteItemResult[];
};

export type BatchUpdateConversationProjectRequest = {
  ids: string[];
  project_id: string | null;
};

export type BatchUpdateConversationProjectResponse = {
  success: boolean;
};

export type ConversationsListParams = {
  limit?: number;
  after?: string;
  before?: string;
  order?: "asc" | "desc";
  project_id?: string;
  unassigned_project?: boolean;
  pinned?: boolean;
};

export type ConversationProject = {
  id: string;
  object: "conversation.project";
  name: string;
  instructions: string | null;
  created_at: number;
  updated_at: number;
};

export type ConversationProjectListItem = {
  id: string;
  object: "conversation.project";
  name: string;
  created_at: number;
  updated_at: number;
};

export type ConversationProjectsListResponse = {
  object: "list";
  data: ConversationProjectListItem[];
  first_id?: string;
  last_id?: string;
  has_more: boolean;
};

export type ConversationProjectCreateRequest = {
  name: string;
};

export type ConversationProjectUpdateRequest = {
  name?: string;
  instructions?: string | null;
};

export type ConversationProjectListParams = {
  limit?: number;
  after?: string;
  order?: "asc" | "desc";
};

export type ConversationProjectDeleteResponse = {
  id: string;
  object: "conversation.project.deleted";
  deleted: boolean;
};

/**
 * Lists user's conversation threads with pagination
 * @param params - Optional parameters for pagination and filtering
 * @returns A promise resolving to a paginated list of conversation threads
 * @throws {Error} If:
 * - The user is not authenticated
 * - The request fails
 * - Invalid pagination parameters
 *
 * @description
 * This function fetches a paginated list of the user's conversation threads.
 * Each thread represents a conversation, not individual messages.
 * Threads are sorted by updated_at (most recently active threads first).
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
 * const threads = await fetchResponsesList({ limit: 20 });
 *
 * // Next page
 * const nextPage = await fetchResponsesList({
 *   limit: 20,
 *   after: threads.last_id
 * });
 *
 * // Previous page
 * const prevPage = await fetchResponsesList({
 *   limit: 20,
 *   before: threads.first_id
 * });
 * ```
 */
export async function fetchResponsesList(
  params?: ResponsesListParams
): Promise<ResponsesListResponse> {
  let url = `${apiUrl}/v1/responses`;
  const queryParams = [];

  if (params?.limit !== undefined) {
    queryParams.push(`limit=${params.limit}`);
  }
  if (params?.after) {
    queryParams.push(`after=${encodeURIComponent(params.after)}`);
  }
  if (params?.before) {
    queryParams.push(`before=${encodeURIComponent(params.before)}`);
  }
  if (params?.order) {
    queryParams.push(`order=${encodeURIComponent(params.order)}`);
  }

  if (queryParams.length > 0) {
    url += `?${queryParams.join("&")}`;
  }

  return authenticatedApiCall<void, ResponsesListResponse>(
    url,
    "GET",
    undefined,
    "Failed to list responses"
  );
}

/**
 * Retrieves a single response by ID
 * @param responseId - The UUID of the response to retrieve
 * @returns A promise resolving to the response details
 * @throws {Error} If:
 * - The user is not authenticated
 * - The response is not found
 * - The user doesn't have access to the response
 *
 * @description
 * This function fetches detailed information about a specific response,
 * including full output and usage statistics.
 *
 * @example
 * ```typescript
 * const response = await fetchResponse("550e8400-e29b-41d4-a716-446655440000");
 * console.log(response.output); // The full response text
 * console.log(response.usage);  // Token usage statistics
 * ```
 */
export async function fetchResponse(responseId: string): Promise<ResponsesRetrieveResponse> {
  return authenticatedApiCall<void, ResponsesRetrieveResponse>(
    `${apiUrl}/v1/responses/${encodeURIComponent(responseId)}`,
    "GET",
    undefined,
    "Failed to retrieve response"
  );
}

export type ResponsesCancelResponse = {
  id: string;
  object: "response";
  created_at: number;
  status: "cancelled";
  model: string;
};

/**
 * Cancels an in-progress response
 * @param responseId - The UUID of the response to cancel
 * @returns A promise resolving to the cancelled response
 * @throws {Error} If:
 * - The user is not authenticated
 * - The response is not found
 * - The response is not in 'in_progress' status
 * - The user doesn't have access to the response
 *
 * @description
 * This function cancels a response that is currently being processed.
 * Only responses with status 'in_progress' can be cancelled.
 *
 * @example
 * ```typescript
 * try {
 *   const cancelled = await cancelResponse("550e8400-e29b-41d4-a716-446655440000");
 *   console.log("Response cancelled:", cancelled.status);
 * } catch (error) {
 *   console.error("Cannot cancel:", error.message);
 * }
 * ```
 */
export async function cancelResponse(responseId: string): Promise<ResponsesCancelResponse> {
  return authenticatedApiCall<void, ResponsesCancelResponse>(
    `${apiUrl}/v1/responses/${encodeURIComponent(responseId)}/cancel`,
    "POST",
    undefined,
    "Failed to cancel response"
  );
}

export type ResponsesDeleteResponse = {
  id: string;
  object: "response.deleted";
  deleted: boolean;
};

export type ResponsesCreateRequest = {
  model: ModelId;
  input: string;
  conversation?: string | { id: string };
  previous_response_id?: string; // Deprecated but still supported
  stream?: boolean;
  metadata?: Record<string, unknown>;
};

/**
 * Creates a new conversation
 * @param metadata - Optional metadata to attach to the conversation
 * @returns A promise resolving to the created conversation
 * @throws {Error} If:
 * - The user is not authenticated
 * - The request fails
 *
 * @description
 * This function creates a new conversation that can be used to group
 * related responses together. The conversation can have metadata
 * attached for organization and filtering purposes.
 *
 * NOTE: Prefer using the OpenAI client directly for conversation operations:
 * ```typescript
 * const openai = new OpenAI({ fetch: customFetch, maxRetries: 0 });
 * const conversation = await openai.conversations.create({
 *   metadata: { title: "Product Support", category: "technical" }
 * });
 * ```
 *
 * @deprecated Use openai.conversations.create() instead
 */
export async function createConversation(
  metadata?: Record<string, unknown>,
  options?: ConversationCreateOptions
): Promise<Conversation> {
  const requestData: ConversationCreateRequest = {
    ...(metadata !== undefined && { metadata }),
    ...(options?.project_id !== undefined && { project_id: options.project_id }),
    ...(options?.pinned !== undefined && { pinned: options.pinned })
  };

  return authenticatedApiCall<ConversationCreateRequest, Conversation>(
    `${apiUrl}/v1/conversations`,
    "POST",
    requestData,
    "Failed to create conversation"
  );
}

/**
 * @deprecated Use openai.conversations.retrieve() instead
 * Retrieves a conversation by ID
 * @param conversationId - The UUID of the conversation to retrieve
 * @returns A promise resolving to the conversation
 * @throws {Error} If:
 * - The user is not authenticated
 * - The conversation is not found
 * - The user doesn't have access to the conversation
 *
 * @example
 * ```typescript
 * const conversation = await getConversation("550e8400-e29b-41d4-a716-446655440000");
 * console.log(conversation.metadata);
 * ```
 */
export async function getConversation(conversationId: string): Promise<Conversation> {
  return authenticatedApiCall<void, Conversation>(
    `${apiUrl}/v1/conversations/${encodeURIComponent(conversationId)}`,
    "GET",
    undefined,
    "Failed to retrieve conversation"
  );
}

/**
 * @deprecated Use openai.conversations.update() instead
 * Updates a conversation's metadata
 * @param conversationId - The UUID of the conversation to update
 * @param metadata - The metadata to update
 * @returns A promise resolving to the updated conversation
 * @throws {Error} If:
 * - The user is not authenticated
 * - The conversation is not found
 * - The user doesn't have access to the conversation
 *
 * @example
 * ```typescript
 * const updated = await updateConversation("550e8400-e29b-41d4-a716-446655440000", {
 *   metadata: { title: "Updated Title", status: "resolved" }
 * });
 * ```
 */
export async function updateConversation(
  conversationId: string,
  metadata?: Record<string, unknown>,
  options?: ConversationUpdateOptions
): Promise<Conversation> {
  const requestData: ConversationUpdateRequest = {};

  if (metadata !== undefined) {
    requestData.metadata = metadata;
  }

  if (options && Object.prototype.hasOwnProperty.call(options, "project_id")) {
    requestData.project_id = options.project_id;
  }

  if (options?.pinned !== undefined) {
    requestData.pinned = options.pinned;
  }

  if (
    requestData.metadata === undefined &&
    requestData.project_id === undefined &&
    requestData.pinned === undefined
  ) {
    throw new Error("At least one conversation field must be provided");
  }

  return authenticatedApiCall<ConversationUpdateRequest, Conversation>(
    `${apiUrl}/v1/conversations/${encodeURIComponent(conversationId)}`,
    "POST",
    requestData,
    "Failed to update conversation"
  );
}

/**
 * @deprecated Use openai.conversations.delete() instead
 * Deletes a conversation permanently
 * @param conversationId - The UUID of the conversation to delete
 * @returns A promise resolving to deletion confirmation
 * @throws {Error} If:
 * - The user is not authenticated
 * - The conversation is not found
 * - The user doesn't have access to the conversation
 *
 * @description
 * This function permanently deletes a conversation and all associated items.
 * This action cannot be undone.
 *
 * @example
 * ```typescript
 * const result = await deleteConversation("550e8400-e29b-41d4-a716-446655440000");
 * if (result.deleted) {
 *   console.log("Conversation deleted successfully");
 * }
 * ```
 */
export async function deleteConversation(
  conversationId: string
): Promise<ConversationDeleteResponse> {
  return authenticatedApiCall<void, ConversationDeleteResponse>(
    `${apiUrl}/v1/conversations/${encodeURIComponent(conversationId)}`,
    "DELETE",
    undefined,
    "Failed to delete conversation"
  );
}

/**
 * Deletes all conversations
 * @returns A promise resolving to deletion confirmation
 * @throws {Error} If:
 * - The user is not authenticated
 * - The request fails
 *
 * @description
 * This function permanently deletes all conversations and their associated items.
 * This action cannot be undone.
 *
 * @example
 * ```typescript
 * const result = await deleteConversations();
 * if (result.deleted) {
 *   console.log("All conversations deleted successfully");
 * }
 * ```
 */
export async function deleteConversations(): Promise<ConversationsDeleteResponse> {
  return authenticatedApiCall<void, ConversationsDeleteResponse>(
    `${apiUrl}/v1/conversations`,
    "DELETE",
    undefined,
    "Failed to delete conversations"
  );
}

/**
 * Batch deletes multiple conversations by their IDs
 * @param ids - Array of conversation UUIDs to delete
 * @returns A promise resolving to per-item deletion results
 * @throws {Error} If:
 * - The user is not authenticated
 * - The request fails
 *
 * @description
 * This function deletes multiple conversations in a single request.
 * Returns per-item results so callers can handle partial failures.
 *
 * @example
 * ```typescript
 * const result = await batchDeleteConversations([
 *   "550e8400-e29b-41d4-a716-446655440000",
 *   "550e8400-e29b-41d4-a716-446655440001"
 * ]);
 * for (const item of result.data) {
 *   if (item.deleted) {
 *     console.log(`Conversation ${item.id} deleted`);
 *   } else {
 *     console.log(`Failed to delete ${item.id}: ${item.error}`);
 *   }
 * }
 * ```
 */
export async function batchDeleteConversations(
  ids: string[]
): Promise<BatchDeleteConversationsResponse> {
  return authenticatedApiCall<BatchDeleteConversationsRequest, BatchDeleteConversationsResponse>(
    `${apiUrl}/v1/conversations/batch-delete`,
    "POST",
    { ids },
    "Failed to batch delete conversations"
  );
}

export async function batchUpdateConversationProject(
  ids: string[],
  projectId: string | null
): Promise<BatchUpdateConversationProjectResponse> {
  return authenticatedApiCall<
    BatchUpdateConversationProjectRequest,
    BatchUpdateConversationProjectResponse
  >(
    `${apiUrl}/v1/conversations/batch-update-project`,
    "POST",
    {
      ids,
      project_id: projectId
    },
    "Failed to batch update conversation project"
  );
}

/**
 * @deprecated Use openai.conversations.create() with items parameter instead
 * Adds items to a conversation
 * @param conversationId - The UUID of the conversation
 * @param items - Array of items to add to the conversation
 * @returns A promise resolving to the updated conversation with items
 * @throws {Error} If:
 * - The user is not authenticated
 * - The conversation is not found
 * - The user doesn't have access to the conversation
 *
 * @example
 * ```typescript
 * await addConversationItems("550e8400-e29b-41d4-a716-446655440000", [
 *   {
 *     type: "message",
 *     role: "user",
 *     content: [{ type: "text", text: "Hello" }]
 *   }
 * ]);
 * ```
 */
export async function addConversationItems(
  conversationId: string,
  items: Partial<ConversationItem>[]
): Promise<Conversation> {
  return authenticatedApiCall<{ items: Partial<ConversationItem>[] }, Conversation>(
    `${apiUrl}/v1/conversations/${encodeURIComponent(conversationId)}/items`,
    "POST",
    { items },
    "Failed to add conversation items"
  );
}

/**
 * @deprecated Use openai.conversations.items.list() instead
 * Lists items in a conversation
 * @param conversationId - The UUID of the conversation
 * @param params - Optional pagination parameters
 * @returns A promise resolving to a paginated list of conversation items
 * @throws {Error} If:
 * - The user is not authenticated
 * - The conversation is not found
 * - The user doesn't have access to the conversation
 *
 * @example
 * ```typescript
 * const items = await listConversationItems("550e8400-e29b-41d4-a716-446655440000", {
 *   limit: 20
 * });
 * for (const item of items.data) {
 *   console.log(item.role, item.content);
 * }
 * ```
 */
export async function listConversationItems(
  conversationId: string,
  params?: {
    limit?: number;
    after?: string;
    before?: string;
  }
): Promise<ConversationItemsResponse> {
  let url = `${apiUrl}/v1/conversations/${encodeURIComponent(conversationId)}/items`;
  const queryParams = [];

  if (params?.limit !== undefined) {
    queryParams.push(`limit=${params.limit}`);
  }
  if (params?.after) {
    queryParams.push(`after=${encodeURIComponent(params.after)}`);
  }
  if (params?.before) {
    queryParams.push(`before=${encodeURIComponent(params.before)}`);
  }

  if (queryParams.length > 0) {
    url += `?${queryParams.join("&")}`;
  }

  return authenticatedApiCall<void, ConversationItemsResponse>(
    url,
    "GET",
    undefined,
    "Failed to list conversation items"
  );
}

/**
 * Retrieves a single item from a conversation
 * @param conversationId - The UUID of the conversation
 * @param itemId - The UUID of the item to retrieve
 * @returns A promise resolving to the conversation item
 */
export async function getConversationItem(
  conversationId: string,
  itemId: string
): Promise<ConversationItem> {
  return authenticatedApiCall<void, ConversationItem>(
    `${apiUrl}/v1/conversations/${encodeURIComponent(conversationId)}/items/${encodeURIComponent(itemId)}`,
    "GET",
    undefined,
    "Failed to retrieve conversation item"
  );
}

/**
 * Lists all conversations with pagination (non-standard endpoint)
 * @param params - Optional pagination parameters
 * @returns A promise resolving to a paginated list of conversations
 * @throws {Error} If:
 * - The user is not authenticated
 * - The request fails
 *
 * @description
 * This is a custom extension not part of the standard OpenAI Conversations API.
 * This function fetches a paginated list of the user's conversations.
 * Conversations are sorted by last_activity_at (most recent activity first).
 *
 * @example
 * ```typescript
 * const conversations = await listConversations({ limit: 20 });
 * for (const conv of conversations.data) {
 *   console.log(conv.id, conv.metadata);
 * }
 * ```
 */
export async function listConversations(
  params?: ConversationsListParams
): Promise<ConversationsListResponse> {
  let url = `${apiUrl}/v1/conversations`;
  const queryParams = [];

  if (params?.limit !== undefined) {
    queryParams.push(`limit=${params.limit}`);
  }
  if (params?.after) {
    queryParams.push(`after=${encodeURIComponent(params.after)}`);
  }
  if (params?.before) {
    queryParams.push(`before=${encodeURIComponent(params.before)}`);
  }
  if (params?.order) {
    queryParams.push(`order=${encodeURIComponent(params.order)}`);
  }
  if (params?.project_id) {
    queryParams.push(`project_id=${encodeURIComponent(params.project_id)}`);
  }
  if (params?.unassigned_project !== undefined) {
    queryParams.push(`unassigned_project=${params.unassigned_project}`);
  }
  if (params?.pinned !== undefined) {
    queryParams.push(`pinned=${params.pinned}`);
  }

  if (queryParams.length > 0) {
    url += `?${queryParams.join("&")}`;
  }

  return authenticatedApiCall<void, ConversationsListResponse>(
    url,
    "GET",
    undefined,
    "Failed to list conversations"
  );
}

export async function createConversationProject(
  request: ConversationProjectCreateRequest
): Promise<ConversationProject> {
  return authenticatedApiCall<ConversationProjectCreateRequest, ConversationProject>(
    `${apiUrl}/v1/conversation-projects`,
    "POST",
    request,
    "Failed to create conversation project"
  );
}

export async function listConversationProjects(
  params?: ConversationProjectListParams
): Promise<ConversationProjectsListResponse> {
  let url = `${apiUrl}/v1/conversation-projects`;
  const queryParams = [];

  if (params?.limit !== undefined) {
    queryParams.push(`limit=${params.limit}`);
  }
  if (params?.after) {
    queryParams.push(`after=${encodeURIComponent(params.after)}`);
  }
  if (params?.order) {
    queryParams.push(`order=${encodeURIComponent(params.order)}`);
  }

  if (queryParams.length > 0) {
    url += `?${queryParams.join("&")}`;
  }

  return authenticatedApiCall<void, ConversationProjectsListResponse>(
    url,
    "GET",
    undefined,
    "Failed to list conversation projects"
  );
}

export async function getConversationProject(projectId: string): Promise<ConversationProject> {
  return authenticatedApiCall<void, ConversationProject>(
    `${apiUrl}/v1/conversation-projects/${encodeURIComponent(projectId)}`,
    "GET",
    undefined,
    "Failed to retrieve conversation project"
  );
}

export async function updateConversationProject(
  projectId: string,
  request: ConversationProjectUpdateRequest
): Promise<ConversationProject> {
  if (request.name === undefined && request.instructions === undefined) {
    throw new Error("At least one conversation project field must be provided");
  }

  return authenticatedApiCall<ConversationProjectUpdateRequest, ConversationProject>(
    `${apiUrl}/v1/conversation-projects/${encodeURIComponent(projectId)}`,
    "POST",
    request,
    "Failed to update conversation project"
  );
}

export async function deleteConversationProject(
  projectId: string
): Promise<ConversationProjectDeleteResponse> {
  return authenticatedApiCall<void, ConversationProjectDeleteResponse>(
    `${apiUrl}/v1/conversation-projects/${encodeURIComponent(projectId)}`,
    "DELETE",
    undefined,
    "Failed to delete conversation project"
  );
}

/**
 * Creates a new response with conversation support
 * @param request - The request parameters for creating a response
 * @returns A promise resolving to the created response or a stream
 * @throws {Error} If:
 * - The user is not authenticated
 * - The request fails
 *
 * @description
 * This function creates a new response in the OpenAI-compatible API.
 * It supports both the new conversation parameter and the deprecated
 * previous_response_id parameter for backward compatibility.
 *
 * @example
 * ```typescript
 * // Create a response in a conversation
 * const response = await createResponse({
 *   model: "gpt-4",
 *   input: "Hello, how are you?",
 *   conversation: "550e8400-e29b-41d4-a716-446655440000"
 * });
 *
 * // Or with the deprecated previous_response_id
 * const response = await createResponse({
 *   model: "gpt-4",
 *   input: "Tell me more",
 *   previous_response_id: "response-uuid"
 * });
 * ```
 */
export async function createResponse(
  request: ResponsesCreateRequest
): Promise<ResponsesRetrieveResponse> {
  return authenticatedApiCall<ResponsesCreateRequest, ResponsesRetrieveResponse>(
    `${apiUrl}/v1/responses`,
    "POST",
    request,
    "Failed to create response"
  );
}

/**
 * Deletes a response permanently
 * @param responseId - The UUID of the response to delete
 * @returns A promise resolving to deletion confirmation
 * @throws {Error} If:
 * - The user is not authenticated
 * - The response is not found
 * - The user doesn't have access to the response
 *
 * @description
 * This function permanently deletes a response and all associated data.
 * This action cannot be undone.
 *
 * @example
 * ```typescript
 * const result = await deleteResponse("550e8400-e29b-41d4-a716-446655440000");
 * if (result.deleted) {
 *   console.log("Response deleted successfully");
 * }
 * ```
 */
export async function deleteResponse(responseId: string): Promise<ResponsesDeleteResponse> {
  return authenticatedApiCall<void, ResponsesDeleteResponse>(
    `${apiUrl}/v1/responses/${encodeURIComponent(responseId)}`,
    "DELETE",
    undefined,
    "Failed to delete response"
  );
}

export type Instruction = {
  id: string;
  object: "instruction";
  name: string;
  prompt: string;
  prompt_tokens: number;
  is_default: boolean;
  created_at: number;
  updated_at: number;
};

export type InstructionCreateRequest = {
  name: string;
  prompt: string;
  is_default?: boolean;
};

export type InstructionUpdateRequest = {
  name?: string;
  prompt?: string;
  is_default?: boolean;
};

export type InstructionListParams = {
  limit?: number;
  after?: string;
  order?: string;
};

export type InstructionListResponse = {
  object: "list";
  data: Instruction[];
  has_more: boolean;
  first_id: string | null;
  last_id: string | null;
};

export type InstructionDeleteResponse = {
  id: string;
  object: "instruction.deleted";
  deleted: true;
};

/**
 * Creates a new instruction
 * @param request - The instruction creation parameters
 * @returns A promise resolving to the created instruction
 * @throws {Error} If:\n * - The user is not authenticated
 * - The request fails
 * - Name or prompt are empty strings
 *
 * @description
 * This function creates a new user instruction (system prompt).
 * If is_default is set to true, all other instructions are automatically set to is_default: false.
 * The prompt_tokens field is automatically calculated.
 *
 * @example
 * ```typescript
 * const instruction = await createInstruction({
 *   name: "Helpful Assistant",
 *   prompt: "You are a helpful assistant that...",
 *   is_default: true
 * });
 * ```
 */
export async function createInstruction(request: InstructionCreateRequest): Promise<Instruction> {
  return authenticatedApiCall<InstructionCreateRequest, Instruction>(
    `${apiUrl}/v1/instructions`,
    "POST",
    request,
    "Failed to create instruction"
  );
}

/**
 * Lists user's instructions with pagination
 * @param params - Optional parameters for pagination and ordering
 * @returns A promise resolving to a paginated list of instructions
 * @throws {Error} If:\n * - The user is not authenticated
 * - The request fails
 *
 * @description
 * This function fetches a paginated list of the user's instructions.
 * Results are ordered by updated_at by default (most recently updated first).
 *
 * Query Parameters:
 * - limit: Number of results per page (1-100, default: 20)
 * - after: UUID cursor for forward pagination
 * - order: Sort order ("asc" or "desc", default: "desc")
 *
 * @example
 * ```typescript
 * const instructions = await listInstructions({ limit: 20 });
 *
 * // Next page
 * const nextPage = await listInstructions({
 *   limit: 20,
 *   after: instructions.last_id
 * });
 * ```
 */
export async function listInstructions(
  params?: InstructionListParams
): Promise<InstructionListResponse> {
  let url = `${apiUrl}/v1/instructions`;
  const queryParams = [];

  if (params?.limit !== undefined) {
    queryParams.push(`limit=${params.limit}`);
  }
  if (params?.after) {
    queryParams.push(`after=${encodeURIComponent(params.after)}`);
  }
  if (params?.order) {
    queryParams.push(`order=${encodeURIComponent(params.order)}`);
  }

  if (queryParams.length > 0) {
    url += `?${queryParams.join("&")}`;
  }

  return authenticatedApiCall<void, InstructionListResponse>(
    url,
    "GET",
    undefined,
    "Failed to list instructions"
  );
}

/**
 * Retrieves a single instruction by ID
 * @param instructionId - The UUID of the instruction to retrieve
 * @returns A promise resolving to the instruction details
 * @throws {Error} If:\n * - The user is not authenticated
 * - The instruction is not found (404)
 * - The user doesn't have access to the instruction
 *
 * @example
 * ```typescript
 * const instruction = await getInstruction("550e8400-e29b-41d4-a716-446655440000");
 * console.log(instruction.name, instruction.prompt);
 * ```
 */
export async function getInstruction(instructionId: string): Promise<Instruction> {
  return authenticatedApiCall<void, Instruction>(
    `${apiUrl}/v1/instructions/${encodeURIComponent(instructionId)}`,
    "GET",
    undefined,
    "Failed to retrieve instruction"
  );
}

/**
 * Updates an existing instruction
 * @param instructionId - The UUID of the instruction to update
 * @param request - The fields to update
 * @returns A promise resolving to the updated instruction
 * @throws {Error} If:\n * - The user is not authenticated
 * - The instruction is not found (404)
 * - No fields are provided (400)
 * - Name or prompt are empty strings (400)
 *
 * @description
 * At least one field must be provided.
 * If is_default: true is set, all other instructions are automatically set to is_default: false.
 * The prompt_tokens field is recalculated automatically if prompt changes.
 *
 * @example
 * ```typescript
 * const updated = await updateInstruction("550e8400-e29b-41d4-a716-446655440000", {
 *   name: "Updated Name",
 *   prompt: "Updated prompt text"
 * });
 * ```
 */
export async function updateInstruction(
  instructionId: string,
  request: InstructionUpdateRequest
): Promise<Instruction> {
  return authenticatedApiCall<InstructionUpdateRequest, Instruction>(
    `${apiUrl}/v1/instructions/${encodeURIComponent(instructionId)}`,
    "POST",
    request,
    "Failed to update instruction"
  );
}

/**
 * Deletes an instruction permanently
 * @param instructionId - The UUID of the instruction to delete
 * @returns A promise resolving to deletion confirmation
 * @throws {Error} If:\n * - The user is not authenticated
 * - The instruction is not found (404)
 * - The user doesn't have access to the instruction
 *
 * @description
 * This function permanently deletes an instruction.
 * This action cannot be undone.
 *
 * @example
 * ```typescript
 * const result = await deleteInstruction("550e8400-e29b-41d4-a716-446655440000");
 * if (result.deleted) {
 *   console.log("Instruction deleted successfully");
 * }
 * ```
 */
export async function deleteInstruction(instructionId: string): Promise<InstructionDeleteResponse> {
  return authenticatedApiCall<void, InstructionDeleteResponse>(
    `${apiUrl}/v1/instructions/${encodeURIComponent(instructionId)}`,
    "DELETE",
    undefined,
    "Failed to delete instruction"
  );
}

/**
 * Sets an instruction as the default
 * @param instructionId - The UUID of the instruction to set as default
 * @returns A promise resolving to the updated instruction
 * @throws {Error} If:\n * - The user is not authenticated
 * - The instruction is not found (404)
 *
 * @description
 * This function sets the specified instruction as the default.
 * All other instructions for this user are automatically set to is_default: false.
 * This operation is idempotent - calling it on an already-default instruction succeeds.
 *
 * @example
 * ```typescript
 * const instruction = await setDefaultInstruction("550e8400-e29b-41d4-a716-446655440000");
 * console.log(instruction.is_default); // Always true
 * ```
 */
export async function setDefaultInstruction(instructionId: string): Promise<Instruction> {
  return authenticatedApiCall<void, Instruction>(
    `${apiUrl}/v1/instructions/${encodeURIComponent(instructionId)}/set-default`,
    "POST",
    undefined,
    "Failed to set default instruction"
  );
}

// ============================================================================
// Web API Types
// ============================================================================

export type WebSearchWorkflow = "search" | "images" | "videos" | "news" | "podcasts";

export type WebSearchTimeRelative = "day" | "week" | "month";

export type WebSearchLens = {
  sites_included?: string[];
  sites_excluded?: string[];
  keywords_included?: string[];
  keywords_excluded?: string[];
  file_type?: string;
  time_after?: string;
  time_before?: string;
  time_relative?: WebSearchTimeRelative;
  search_region?: string;
};

export type WebSearchFilters = {
  region?: string;
  after?: string;
  before?: string;
};

export type WebSearchRequest = {
  query: string;
  /** Defaults to `search`. */
  workflow?: WebSearchWorkflow;
  /** One-based result page, from 1 through 10. */
  page?: number;
  /** Maximum results to return, from 1 through 50. Defaults to 10. */
  limit?: number;
  /** Whether to omit potentially unsafe content. Defaults to true. */
  safe_search?: boolean;
  /** Search collection timeout in seconds, from 0.5 through 4. */
  timeout?: number;
  lens_id?: string;
  lens?: WebSearchLens;
  filters?: WebSearchFilters;
};

export type WebSearchResult = {
  category: string;
  url: string;
  title: string;
  snippet?: string;
  published_at?: string;
};

export type WebSearchResponse = {
  trace_id?: string;
  results: WebSearchResult[];
};

export type WebExtractRequest = {
  /** Public HTTPS URLs to extract, from 1 through 10 entries. */
  urls: string[];
  /** Bulk extraction timeout in seconds, from 0.5 through 10. */
  timeout?: number;
};

export type WebExtractPageError = {
  code: string;
  message: string;
};

export type WebExtractPage = {
  url: string;
  markdown?: string;
  error?: WebExtractPageError;
};

export type WebExtractResponse = {
  trace_id?: string;
  pages: WebExtractPage[];
};

// ============================================================================
// Web API Functions
// ============================================================================

/** Searches the public web through OpenSecret's configured search provider. */
export async function webSearch(request: WebSearchRequest): Promise<WebSearchResponse> {
  return authenticatedApiCall<WebSearchRequest, WebSearchResponse>(
    `${apiUrl}/v1/web/search`,
    "POST",
    request,
    "Failed to search the web"
  );
}

/** Extracts sanitized Markdown from public URLs through OpenSecret's configured provider. */
export async function webExtract(request: WebExtractRequest): Promise<WebExtractResponse> {
  return authenticatedApiCall<WebExtractRequest, WebExtractResponse>(
    `${apiUrl}/v1/web/extract`,
    "POST",
    request,
    "Failed to extract web pages"
  );
}

// ============================================================================
// Agent API Types
// ============================================================================

export type CreateSubagentRequest = {
  display_name?: string;
  purpose: string;
};

export type AgentCreatedBy = "user" | "agent";

export type MainAgentResponse = {
  id: string;
  object: "agent.main";
  kind: "main";
  conversation_id: string;
  display_name: string;
  created_at: number;
  updated_at: number;
};

export type SubagentResponse = {
  id: string;
  object: "agent.subagent";
  kind: "subagent";
  conversation_id: string;
  display_name: string;
  purpose: string;
  created_by: AgentCreatedBy;
  created_at: number;
  updated_at: number;
};

export type AgentItemsListParams = {
  limit?: number;
  after?: string;
  order?: string;
  include?: string[];
};

export type ListSubagentsParams = {
  limit?: number;
  after?: string;
  order?: string;
  created_by?: AgentCreatedBy;
};

export type SubagentListResponse = {
  object: "list";
  data: SubagentResponse[];
  first_id?: string;
  last_id?: string;
  has_more: boolean;
};

export type AgentDeletedObjectResponse = {
  id: string;
  object: string;
  deleted: boolean;
};

export type AgentMessageEvent = {
  messages: string[];
  step: number;
};

export type AgentDoneEvent = {
  total_steps: number;
  total_messages: number;
};

export type AgentErrorEvent = {
  error: string;
};

// ============================================================================
// Agent API Functions
// ============================================================================

export async function getMainAgent(): Promise<MainAgentResponse> {
  return authenticatedApiCall<void, MainAgentResponse>(
    `${apiUrl}/v1/agent`,
    "GET",
    undefined,
    "Failed to get main agent"
  );
}

export async function deleteMainAgent(): Promise<AgentDeletedObjectResponse> {
  return authenticatedApiCall<void, AgentDeletedObjectResponse>(
    `${apiUrl}/v1/agent`,
    "DELETE",
    undefined,
    "Failed to delete main agent"
  );
}

export async function listMainAgentItems(
  params?: AgentItemsListParams
): Promise<ConversationItemsResponse> {
  let url = `${apiUrl}/v1/agent/items`;
  const queryParams: string[] = [];

  if (params?.limit !== undefined) {
    queryParams.push(`limit=${params.limit}`);
  }
  if (params?.after) {
    queryParams.push(`after=${encodeURIComponent(params.after)}`);
  }
  if (params?.order) {
    queryParams.push(`order=${encodeURIComponent(params.order)}`);
  }
  if (params?.include) {
    for (const includeValue of params.include) {
      queryParams.push(`include=${encodeURIComponent(includeValue)}`);
    }
  }

  if (queryParams.length > 0) {
    url += `?${queryParams.join("&")}`;
  }

  return authenticatedApiCall<void, ConversationItemsResponse>(
    url,
    "GET",
    undefined,
    "Failed to list main agent items"
  );
}

export async function getMainAgentItem(itemId: string): Promise<ConversationItem> {
  return authenticatedApiCall<void, ConversationItem>(
    `${apiUrl}/v1/agent/items/${encodeURIComponent(itemId)}`,
    "GET",
    undefined,
    "Failed to get main agent item"
  );
}

export async function listSubagents(params?: ListSubagentsParams): Promise<SubagentListResponse> {
  let url = `${apiUrl}/v1/agent/subagents`;
  const queryParams: string[] = [];

  if (params?.limit !== undefined) {
    queryParams.push(`limit=${params.limit}`);
  }
  if (params?.after) {
    queryParams.push(`after=${encodeURIComponent(params.after)}`);
  }
  if (params?.order) {
    queryParams.push(`order=${encodeURIComponent(params.order)}`);
  }
  if (params?.created_by) {
    queryParams.push(`created_by=${encodeURIComponent(params.created_by)}`);
  }

  if (queryParams.length > 0) {
    url += `?${queryParams.join("&")}`;
  }

  return authenticatedApiCall<void, SubagentListResponse>(
    url,
    "GET",
    undefined,
    "Failed to list subagents"
  );
}

export async function getSubagent(id: string): Promise<SubagentResponse> {
  return authenticatedApiCall<void, SubagentResponse>(
    `${apiUrl}/v1/agent/subagents/${encodeURIComponent(id)}`,
    "GET",
    undefined,
    "Failed to get subagent"
  );
}

export async function createSubagent(request: CreateSubagentRequest): Promise<SubagentResponse> {
  return authenticatedApiCall<CreateSubagentRequest, SubagentResponse>(
    `${apiUrl}/v1/agent/subagents`,
    "POST",
    request,
    "Failed to create subagent"
  );
}

export async function deleteSubagent(id: string): Promise<AgentDeletedObjectResponse> {
  return authenticatedApiCall<void, AgentDeletedObjectResponse>(
    `${apiUrl}/v1/agent/subagents/${encodeURIComponent(id)}`,
    "DELETE",
    undefined,
    "Failed to delete subagent"
  );
}

export async function listSubagentItems(
  id: string,
  params?: AgentItemsListParams
): Promise<ConversationItemsResponse> {
  let url = `${apiUrl}/v1/agent/subagents/${encodeURIComponent(id)}/items`;
  const queryParams: string[] = [];

  if (params?.limit !== undefined) {
    queryParams.push(`limit=${params.limit}`);
  }
  if (params?.after) {
    queryParams.push(`after=${encodeURIComponent(params.after)}`);
  }
  if (params?.order) {
    queryParams.push(`order=${encodeURIComponent(params.order)}`);
  }
  if (params?.include) {
    for (const includeValue of params.include) {
      queryParams.push(`include=${encodeURIComponent(includeValue)}`);
    }
  }

  if (queryParams.length > 0) {
    url += `?${queryParams.join("&")}`;
  }

  return authenticatedApiCall<void, ConversationItemsResponse>(
    url,
    "GET",
    undefined,
    "Failed to list subagent items"
  );
}

export async function getSubagentItem(id: string, itemId: string): Promise<ConversationItem> {
  return authenticatedApiCall<void, ConversationItem>(
    `${apiUrl}/v1/agent/subagents/${encodeURIComponent(id)}/items/${encodeURIComponent(itemId)}`,
    "GET",
    undefined,
    "Failed to get subagent item"
  );
}
