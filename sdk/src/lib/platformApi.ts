import {
  encryptedApiCall,
  authenticatedApiCall,
  authenticatedApiCallWithAuthority,
  authenticatedApiCallWithSelectedAuthority
} from "./encryptedApi";
import { snapshotPcrConfig, type PcrConfig } from "./pcr";
import {
  clearLegacyTransportV1Credentials,
  installTransportV2Credentials,
  snapshotTransportV2Auth,
  type TransportV2AuthSnapshot
} from "./transportV2/auth";
import { transportV2AuthRuntime } from "./transportV2/authRuntime";

// Platform Auth Types
export type PlatformLoginResponse = {
  id: string;
  email: string;
  name?: string;
  access_token: string;
  refresh_token: string;
};

export type PlatformRefreshResponse = {
  access_token: string;
  refresh_token: string;
};

type PlatformCredentialUpdateResponse = {
  message: string;
  access_token?: string;
  refresh_token?: string;
};

// Platform User Types
export type PlatformOrg = {
  id: string;
  name: string;
  role?: string;
  created_at?: string;
  updated_at?: string;
};

export type PlatformUser = {
  id: string;
  email: string;
  name?: string;
  email_verified: boolean;
  created_at: string;
  updated_at: string;
};

export type MeResponse = {
  user: PlatformUser;
  organizations: PlatformOrg[];
};

// Organization Types
export type Organization = {
  id: string;
  name: string;
};

export type OrganizationInvite = {
  code: string; // UUID of the invite
  email: string;
  role: string;
  used: boolean;
  expires_at: string;
  created_at: string;
  updated_at: string;
  organization_name?: string;
};

export type Project = {
  id: string;
  client_id: string;
  name: string;
  description?: string;
  status: string;
  created_at: string;
};

export type ProjectSecret = {
  key_name: string;
  created_at: string;
  updated_at: string;
};

export type ProjectSettings = {
  category: string;
  settings: Record<string, unknown>;
  created_at: string;
  updated_at: string;
};

export type EmailSettings = {
  provider: string;
  send_from: string;
  email_verification_url: string;
};

/**
 * Provider-specific OAuth settings
 */
export type OAuthProviderSettings = {
  client_id: string;
  redirect_url: string;
  team_id?: string; // Apple-specific: The Apple Developer Team ID
  key_id?: string; // Apple-specific: The Apple Developer Key ID for Sign in with Apple
};

export type OAuthSettings = {
  google_oauth_enabled: boolean;
  github_oauth_enabled: boolean;
  apple_oauth_enabled: boolean;
  google_oauth_settings?: OAuthProviderSettings;
  github_oauth_settings?: OAuthProviderSettings;
  apple_oauth_settings?: OAuthProviderSettings;
};

export type PushEnvironment = "dev" | "prod";

export type IosPushSettings = {
  enabled: boolean;
  bundle_id: string;
  apns_environment: PushEnvironment;
  team_id: string;
  key_id: string;
};

export type AndroidPushSettings = {
  enabled: boolean;
  firebase_project_id: string;
  package_name: string;
};

export type PushSettings = {
  encrypted_preview_enabled: boolean;
  ios?: IosPushSettings;
  android?: AndroidPushSettings;
};

export type OrganizationMember = {
  user_id: string;
  role: string;
  name?: string;
};

let platformApiUrl = "";
let platformPcrConfig: PcrConfig = snapshotPcrConfig();
clearLegacyTransportV1Credentials();

export function setPlatformApiUrl(url: string, pcrConfig?: PcrConfig) {
  platformApiUrl = url;
  platformPcrConfig = snapshotPcrConfig(pcrConfig);
  clearLegacyTransportV1Credentials();
}

export function getPlatformApiUrl(): string {
  return platformApiUrl;
}

export function getPlatformPcrConfig(): PcrConfig {
  return snapshotPcrConfig(platformPcrConfig);
}

function installPlatformLoginResponse(
  response: PlatformLoginResponse,
  expected: TransportV2AuthSnapshot
): PlatformLoginResponse {
  installTransportV2Credentials(
    platformApiUrl,
    "platform",
    response.access_token,
    response.refresh_token,
    expected
  );
  return response;
}

function installPlatformCredentialUpdate(
  response: PlatformCredentialUpdateResponse,
  startedWith: TransportV2AuthSnapshot,
  targetApiUrl: string
): void {
  if (!response.access_token && !response.refresh_token) return;
  if (!response.access_token || !response.refresh_token) {
    throw new Error("Transport v2 credential update returned an incomplete token pair.");
  }
  installTransportV2Credentials(
    targetApiUrl,
    "platform",
    response.access_token,
    response.refresh_token,
    startedWith
  );
}

// Platform Authentication
export async function platformLogin(
  email: string,
  password: string
): Promise<PlatformLoginResponse> {
  const expected = snapshotTransportV2Auth(platformApiUrl, "platform");
  const response = await encryptedApiCall<
    { email: string; password: string },
    PlatformLoginResponse
  >(`${platformApiUrl}/platform/login`, "POST", { email, password }, undefined, "Failed to login");
  return installPlatformLoginResponse(response, expected);
}

/**
 * Registers a new platform developer account
 * @param email Developer's email address
 * @param password Developer's password
 * @param invite_code Required invitation code in UUID format
 * @param name Optional developer name
 * @returns A promise that resolves to the login response with access and refresh tokens
 */
export async function platformRegister(
  email: string,
  password: string,
  invite_code: string,
  name?: string
): Promise<PlatformLoginResponse> {
  const expected = snapshotTransportV2Auth(platformApiUrl, "platform");
  const response = await encryptedApiCall<
    { email: string; password: string; invite_code: string; name?: string },
    PlatformLoginResponse
  >(
    `${platformApiUrl}/platform/register`,
    "POST",
    { email, password, invite_code, name },
    undefined,
    "Failed to register"
  );
  return installPlatformLoginResponse(response, expected);
}

export async function platformLogout(refresh_token: string): Promise<void> {
  return encryptedApiCall<{ refresh_token: string }, void>(
    `${platformApiUrl}/platform/logout`,
    "POST",
    { refresh_token },
    undefined,
    "Failed to logout"
  );
}

/**
 * Refreshes platform access and refresh tokens
 *
 * This function:
 * 1. Reads the origin-scoped V2 resumption credential
 * 2. Sends it inside an encrypted credential field with no logical body
 * 3. Atomically installs the returned V2 credential pair
 *
 * The backend remains authoritative for signature, expiry, and account state.
 */
export async function platformRefreshToken(): Promise<PlatformRefreshResponse> {
  const refreshed = await transportV2AuthRuntime.refresh(
    platformApiUrl,
    platformPcrConfig,
    "platform"
  );
  return {
    access_token: refreshed.accessToken,
    refresh_token: refreshed.refreshToken
  };
}

// Organization Management
export async function createOrganization(name: string): Promise<Organization> {
  return authenticatedApiCall<{ name: string }, Organization>(
    `${platformApiUrl}/platform/orgs`,
    "POST",
    { name }
  );
}

export async function listOrganizations(): Promise<Organization[]> {
  return authenticatedApiCall<void, Organization[]>(
    `${platformApiUrl}/platform/orgs`,
    "GET",
    undefined
  );
}

export async function deleteOrganization(orgId: string): Promise<void> {
  return authenticatedApiCall<void, void>(
    `${platformApiUrl}/platform/orgs/${orgId}`,
    "DELETE",
    undefined
  );
}

// Project Management
export async function createProject(
  orgId: string,
  name: string,
  description?: string
): Promise<Project> {
  return authenticatedApiCall<{ name: string; description?: string }, Project>(
    `${platformApiUrl}/platform/orgs/${orgId}/projects`,
    "POST",
    { name, description }
  );
}

export async function listProjects(orgId: string): Promise<Project[]> {
  return authenticatedApiCall<void, Project[]>(
    `${platformApiUrl}/platform/orgs/${orgId}/projects`,
    "GET",
    undefined
  );
}

export async function getProject(orgId: string, projectId: string): Promise<Project> {
  return authenticatedApiCall<void, Project>(
    `${platformApiUrl}/platform/orgs/${orgId}/projects/${projectId}`,
    "GET",
    undefined
  );
}

export async function updateProject(
  orgId: string,
  projectId: string,
  updates: { name?: string; description?: string; status?: string }
): Promise<Project> {
  return authenticatedApiCall<typeof updates, Project>(
    `${platformApiUrl}/platform/orgs/${orgId}/projects/${projectId}`,
    "PATCH",
    updates
  );
}

export async function deleteProject(orgId: string, projectId: string): Promise<void> {
  return authenticatedApiCall<void, void>(
    `${platformApiUrl}/platform/orgs/${orgId}/projects/${projectId}`,
    "DELETE",
    undefined
  );
}

// Helper function to check if a string is valid base64
function isValidBase64(str: string): boolean {
  // Base64 should have a length that is a multiple of 4
  // It should only contain characters A-Z, a-z, 0-9, +, /, and end with '=' or '=='
  const base64Regex = /^[A-Za-z0-9+/]*[=]{0,2}$/;

  // Check if the string length is a multiple of 4
  const validLength = str.length % 4 === 0;

  // Check if the string only contains valid base64 characters
  const validChars = base64Regex.test(str);

  return validLength && validChars;
}

// Project Secrets
export async function createProjectSecret(
  orgId: string,
  projectId: string,
  keyName: string,
  secret: string
): Promise<ProjectSecret> {
  // Validate that the secret is base64 encoded
  if (!isValidBase64(secret)) {
    throw new Error(
      "Secret must be base64 encoded. Use @stablelib/base64's encode function to encode your data."
    );
  }

  return authenticatedApiCall<{ key_name: string; secret: string }, ProjectSecret>(
    `${platformApiUrl}/platform/orgs/${orgId}/projects/${projectId}/secrets`,
    "POST",
    { key_name: keyName, secret }
  );
}

export async function listProjectSecrets(
  orgId: string,
  projectId: string
): Promise<ProjectSecret[]> {
  return authenticatedApiCall<void, ProjectSecret[]>(
    `${platformApiUrl}/platform/orgs/${orgId}/projects/${projectId}/secrets`,
    "GET",
    undefined
  );
}

export async function deleteProjectSecret(
  orgId: string,
  projectId: string,
  keyName: string
): Promise<void> {
  return authenticatedApiCall<void, void>(
    `${platformApiUrl}/platform/orgs/${orgId}/projects/${projectId}/secrets/${keyName}`,
    "DELETE",
    undefined
  );
}

// Email Settings
export async function getEmailSettings(orgId: string, projectId: string): Promise<EmailSettings> {
  return authenticatedApiCall<void, EmailSettings>(
    `${platformApiUrl}/platform/orgs/${orgId}/projects/${projectId}/settings/email`,
    "GET",
    undefined
  );
}

export async function updateEmailSettings(
  orgId: string,
  projectId: string,
  settings: EmailSettings
): Promise<EmailSettings> {
  return authenticatedApiCall<EmailSettings, EmailSettings>(
    `${platformApiUrl}/platform/orgs/${orgId}/projects/${projectId}/settings/email`,
    "PUT",
    settings
  );
}

// OAuth Settings
export async function getOAuthSettings(orgId: string, projectId: string): Promise<OAuthSettings> {
  return authenticatedApiCall<void, OAuthSettings>(
    `${platformApiUrl}/platform/orgs/${orgId}/projects/${projectId}/settings/oauth`,
    "GET",
    undefined
  );
}

export async function updateOAuthSettings(
  orgId: string,
  projectId: string,
  settings: OAuthSettings
): Promise<OAuthSettings> {
  return authenticatedApiCall<OAuthSettings, OAuthSettings>(
    `${platformApiUrl}/platform/orgs/${orgId}/projects/${projectId}/settings/oauth`,
    "PUT",
    settings
  );
}

// Push Settings
export async function getPushSettings(orgId: string, projectId: string): Promise<PushSettings> {
  return authenticatedApiCall<void, PushSettings>(
    `${platformApiUrl}/platform/orgs/${orgId}/projects/${projectId}/settings/push`,
    "GET",
    undefined
  );
}

export async function updatePushSettings(
  orgId: string,
  projectId: string,
  settings: PushSettings
): Promise<PushSettings> {
  return authenticatedApiCall<PushSettings, PushSettings>(
    `${platformApiUrl}/platform/orgs/${orgId}/projects/${projectId}/settings/push`,
    "PUT",
    settings
  );
}

// Organization Membership
export async function inviteDeveloper(
  orgId: string,
  email: string,
  role?: string
): Promise<OrganizationInvite> {
  // Add validation for empty emails
  if (!email || email.trim() === "") {
    throw new Error("Email is required");
  }

  return authenticatedApiCall<{ email: string; role?: string }, OrganizationInvite>(
    `${platformApiUrl}/platform/orgs/${orgId}/invites`,
    "POST",
    { email, role }
  );
}

export async function listOrganizationInvites(orgId: string): Promise<OrganizationInvite[]> {
  return authenticatedApiCall<void, OrganizationInvite[]>(
    `${platformApiUrl}/platform/orgs/${orgId}/invites`,
    "GET",
    undefined
  );
}

export async function getOrganizationInvite(
  orgId: string,
  inviteCode: string
): Promise<OrganizationInvite> {
  return authenticatedApiCall<void, OrganizationInvite>(
    `${platformApiUrl}/platform/orgs/${orgId}/invites/${inviteCode}`,
    "GET",
    undefined
  );
}

export async function deleteOrganizationInvite(
  orgId: string,
  inviteCode: string
): Promise<{ message: string }> {
  return authenticatedApiCall<void, { message: string }>(
    `${platformApiUrl}/platform/orgs/${orgId}/invites/${inviteCode}`,
    "DELETE",
    undefined
  );
}

export async function listOrganizationMembers(orgId: string): Promise<OrganizationMember[]> {
  return authenticatedApiCall<void, OrganizationMember[]>(
    `${platformApiUrl}/platform/orgs/${orgId}/memberships`,
    "GET",
    undefined
  );
}

export async function updateMemberRole(
  orgId: string,
  userId: string,
  role: string
): Promise<OrganizationMember> {
  return authenticatedApiCall<{ role: string }, OrganizationMember>(
    `${platformApiUrl}/platform/orgs/${orgId}/memberships/${userId}`,
    "PATCH",
    { role }
  );
}

export async function removeMember(orgId: string, userId: string): Promise<void> {
  return authenticatedApiCall<void, void>(
    `${platformApiUrl}/platform/orgs/${orgId}/memberships/${userId}`,
    "DELETE",
    undefined
  );
}

export async function acceptInvite(code: string): Promise<{ message: string }> {
  return authenticatedApiCall<void, { message: string }>(
    `${platformApiUrl}/platform/accept_invite/${code}`,
    "POST",
    undefined
  );
}

// Platform User
export async function platformMe(): Promise<MeResponse> {
  return authenticatedApiCall<void, MeResponse>(`${platformApiUrl}/platform/me`, "GET", undefined);
}

/** @internal Keeps React publication tied to the exact authority used for this request. */
export async function platformMeWithTransportV2Authority(
  targetApiUrl: string,
  pcrConfig: PcrConfig,
  authority: import("./transportV2/authRuntime").TransportV2Authority
) {
  return authenticatedApiCallWithSelectedAuthority<void, MeResponse>(
    `${targetApiUrl}/platform/me`,
    "GET",
    undefined,
    undefined,
    { apiUrl: targetApiUrl, pcrConfig, kind: "platform", authority }
  );
}

/**
 * Verifies a platform user's email using the verification code
 * @param code - The verification code sent to the user's email
 * @returns A promise that resolves when verification is complete
 * @throws {Error} If verification fails
 */
export async function verifyPlatformEmail(code: string): Promise<void> {
  return encryptedApiCall<void, void>(
    `${platformApiUrl}/platform/verify-email/${code}`,
    "GET",
    undefined,
    undefined,
    "Failed to verify email"
  );
}

/**
 * Requests a new verification email for a platform user
 * @returns A promise that resolves to a success message
 * @throws {Error} If the user is already verified or request fails
 */
export async function requestNewPlatformVerificationCode(): Promise<{ message: string }> {
  return authenticatedApiCall<void, { message: string }>(
    `${platformApiUrl}/platform/request_verification`,
    "POST",
    undefined,
    "Failed to request new verification code"
  );
}

/**
 * Initiates the password reset process for a platform developer account
 * @param email - Developer's email address
 * @param hashedSecret - Hashed secret used for additional security verification
 * @returns A promise that resolves when the reset request is successfully processed
 * @throws {Error} If the request fails or the email doesn't exist
 *
 * @description
 * This function:
 * 1. Sends a password reset request for a platform developer
 * 2. The server will send an email with an alphanumeric code
 * 3. The email and hashed_secret are paired for the reset process
 * 4. Use confirmPlatformPasswordReset to complete the process
 */
export async function requestPlatformPasswordReset(
  email: string,
  hashedSecret: string
): Promise<void> {
  const resetData = {
    email,
    hashed_secret: hashedSecret
  };
  return encryptedApiCall<typeof resetData, void>(
    `${platformApiUrl}/platform/password-reset/request`,
    "POST",
    resetData,
    undefined,
    "Failed to request platform password reset"
  );
}

/**
 * Completes the password reset process for a platform developer account
 * @param email - Developer's email address
 * @param alphanumericCode - Code received via email
 * @param plaintextSecret - The plaintext secret that corresponds to the hashed_secret sent in the request
 * @param newPassword - New password to set
 * @returns A promise that resolves when the password is successfully reset
 * @throws {Error} If the verification fails or the request is invalid
 *
 * @description
 * This function:
 * 1. Completes the password reset process using the code from the email
 * 2. Requires the plaintext_secret that matches the previously sent hashed_secret
 * 3. Sets the new password if all verification succeeds
 * 4. The user can then log in with the new password
 */
export async function confirmPlatformPasswordReset(
  email: string,
  alphanumericCode: string,
  plaintextSecret: string,
  newPassword: string
): Promise<{ message: string }> {
  const confirmData = {
    email,
    alphanumeric_code: alphanumericCode,
    plaintext_secret: plaintextSecret,
    new_password: newPassword
  };
  return encryptedApiCall<typeof confirmData, { message: string }>(
    `${platformApiUrl}/platform/password-reset/confirm`,
    "POST",
    confirmData,
    undefined,
    "Failed to confirm platform password reset"
  );
}

/**
 * Changes password for a platform developer account
 * @param currentPassword - Current password for verification
 * @param newPassword - New password to set
 * @returns A promise that resolves when the password is successfully changed
 * @throws {Error} If current password is incorrect or the request fails
 *
 * @description
 * This function:
 * 1. Requires the user to be authenticated
 * 2. Verifies the current password before allowing the change
 * 3. Updates to the new password if verification succeeds
 */
export async function changePlatformPassword(
  currentPassword: string,
  newPassword: string
): Promise<{ message: string }> {
  const targetApiUrl = platformApiUrl;
  const pcrConfig = snapshotPcrConfig(platformPcrConfig);
  const changePasswordData = {
    current_password: currentPassword,
    new_password: newPassword
  };
  const result = await authenticatedApiCallWithAuthority<
    typeof changePasswordData,
    PlatformCredentialUpdateResponse
  >(
    `${targetApiUrl}/platform/change-password`,
    "POST",
    changePasswordData,
    "Failed to change platform password",
    { apiUrl: targetApiUrl, pcrConfig, kind: "platform" }
  );
  installPlatformCredentialUpdate(result.data, result.authority.snapshot, result.apiUrl);
  return result.data;
}
