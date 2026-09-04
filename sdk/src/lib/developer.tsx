import React, { createContext, useState, useEffect, useRef } from "react";
import * as platformApi from "./platformApi";
import { setPlatformApiUrl } from "./platformApi";
import { authenticate } from "./attestation";
import {
  parseAttestationForView,
  AWS_ROOT_CERT_DER,
  EXPECTED_ROOT_CERT_HASH,
  ParsedAttestationView
} from "./attestationForView";
import type { AttestationDocument } from "./attestation";
import { PcrConfig } from "./pcr";
import {
  clearTransportV2CredentialsIfCurrent,
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
import type {
  Organization,
  Project,
  ProjectSecret,
  ProjectSettings,
  EmailSettings,
  OAuthSettings,
  PushSettings,
  OrganizationMember,
  PlatformOrg,
  PlatformUser,
  OrganizationInvite
} from "./platformApi";

export type DeveloperRole = "owner" | "admin" | "developer" | "viewer";

export type OrganizationDetails = Organization;

export type ProjectDetails = Project;

export { type ProjectSettings, type PushSettings };

export type DeveloperResponse = PlatformUser & { organizations: PlatformOrg[] };

export type OpenSecretDeveloperAuthState = {
  loading: boolean;
  developer?: DeveloperResponse;
};

export type OpenSecretDeveloperContextType = {
  auth: OpenSecretDeveloperAuthState;

  /**
   * Signs in a developer with email and password
   * @param email - Developer's email address
   * @param password - Developer's password
   * @returns A promise that resolves to the login response with access and refresh tokens
   *
   *
   * - Calls the login API endpoint
   * - Stores access_token and refresh_token in localStorage
   * - Updates the developer state with user information
   * - Throws an error if authentication fails
   */
  signIn: (email: string, password: string) => Promise<platformApi.PlatformLoginResponse>;

  /**
   * Verifies a platform user's email using the verification code
   * @param code - The verification code sent to the user's email
   * @returns A promise that resolves when verification is complete
   * @throws {Error} If verification fails
   *
   *
   * - Takes the verification code from the verification email link
   * - Calls the verification API endpoint
   * - Updates email_verified status if successful
   */
  verifyEmail: typeof platformApi.verifyPlatformEmail;

  /**
   * Requests a new verification email for the current user
   * @returns A promise that resolves to a success message
   * @throws {Error} If the user is already verified or request fails
   *
   *
   * - Used when the user needs a new verification email
   * - Requires the user to be authenticated
   * - Sends a new verification email to the user's registered email address
   */
  requestNewVerificationCode: typeof platformApi.requestNewPlatformVerificationCode;

  /**
   * Alias for requestNewVerificationCode - for consistency with OpenSecretContext
   */
  requestNewVerificationEmail: typeof platformApi.requestNewPlatformVerificationCode;

  /**
   * Initiates the password reset process for a platform developer account
   * @param email - Developer's email address
   * @param hashedSecret - Hashed secret used for additional security verification
   * @returns A promise that resolves when the reset request is successfully processed
   * @throws {Error} If the request fails or the email doesn't exist
   *
   *
   * - Sends a password reset request for a platform developer
   * - The server will send an email with an alphanumeric code
   * - The email and hashed_secret are paired for the reset process
   * - Use confirmPasswordReset to complete the process
   */
  requestPasswordReset: typeof platformApi.requestPlatformPasswordReset;

  /**
   * Completes the password reset process for a platform developer account
   * @param email - Developer's email address
   * @param alphanumericCode - Code received via email
   * @param plaintextSecret - The plaintext secret that corresponds to the hashed_secret sent in the request
   * @param newPassword - New password to set
   * @returns A promise that resolves when the password is successfully reset
   * @throws {Error} If the verification fails or the request is invalid
   *
   *
   * - Completes the password reset process using the code from the email
   * - Requires the plaintext_secret that matches the previously sent hashed_secret
   * - Sets the new password if all verification succeeds
   * - The user can then log in with the new password
   */
  confirmPasswordReset: typeof platformApi.confirmPlatformPasswordReset;

  /**
   * Changes password for a platform developer account
   * @param currentPassword - Current password for verification
   * @param newPassword - New password to set
   * @returns A promise that resolves when the password is successfully changed
   * @throws {Error} If current password is incorrect or the request fails
   *
   *
   * - Requires the user to be authenticated
   * - Verifies the current password before allowing the change
   * - Updates to the new password if verification succeeds
   */
  changePassword: typeof platformApi.changePlatformPassword;

  /**
   * Registers a new developer account
   * @param email - Developer's email address
   * @param password - Developer's password
   * @param invite_code - Required invitation code in UUID format
   * @param name - Optional developer name
   * @returns A promise that resolves to the login response with access and refresh tokens
   *
   *
   * - Calls the registration API endpoint
   * - Stores access_token and refresh_token in localStorage
   * - Updates the developer state with new user information
   * - Throws an error if account creation fails
   */
  signUp: (
    email: string,
    password: string,
    invite_code: string,
    name?: string
  ) => Promise<platformApi.PlatformLoginResponse>;

  /**
   * Signs out the current developer by removing authentication tokens
   *
   *
   * - Calls the logout API endpoint with the current refresh_token
   * - Removes access_token, refresh_token from localStorage
   * - Resets the developer state to show no user is authenticated
   */
  signOut: () => Promise<void>;

  /**
   * Refreshes the developer's authentication state
   * @returns A promise that resolves when the refresh is complete
   * @throws {Error} If the refresh fails
   *
   *
   * - Retrieves the latest developer information from the server
   * - Updates the developer state with fresh data
   * - Useful after making changes that affect developer profile or organization membership
   */
  refetchDeveloper: () => Promise<void>;

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
   * Creates a new organization
   * @param name - Organization name
   * @returns A promise that resolves to the created organization
   */
  createOrganization: (name: string) => Promise<Organization>;

  /**
   * Lists all organizations the developer has access to
   * @returns A promise resolving to array of organization details
   */
  listOrganizations: () => Promise<Organization[]>;

  /**
   * Deletes an organization (requires owner role)
   * @param orgId - Organization ID
   */
  deleteOrganization: (orgId: string) => Promise<void>;

  /**
   * Creates a new project within an organization
   * @param orgId - Organization ID
   * @param name - Project name
   * @param description - Optional project description
   * @returns A promise that resolves to the project details including client ID
   */
  createProject: (orgId: string, name: string, description?: string) => Promise<Project>;

  /**
   * Lists all projects within an organization
   * @param orgId - Organization ID
   * @returns A promise resolving to array of project details
   */
  listProjects: (orgId: string) => Promise<Project[]>;

  /**
   * Gets a single project by ID
   * @param orgId - Organization ID
   * @param projectId - Project ID
   * @returns A promise resolving to the project details
   */
  getProject: (orgId: string, projectId: string) => Promise<Project>;

  /**
   * Updates project details
   * @param orgId - Organization ID
   * @param projectId - Project ID
   * @param updates - Object containing fields to update
   */
  updateProject: (
    orgId: string,
    projectId: string,
    updates: { name?: string; description?: string; status?: string }
  ) => Promise<Project>;

  /**
   * Deletes a project
   * @param orgId - Organization ID
   * @param projectId - Project ID
   */
  deleteProject: (orgId: string, projectId: string) => Promise<void>;

  /**
   * Creates a new secret for a project
   * @param orgId - Organization ID
   * @param projectId - Project ID
   * @param keyName - Secret key name (must be alphanumeric)
   * @param secret - Secret value (must be base64 encoded by the caller)
   *
   * Example:
   * ```typescript
   * // To encode a string secret
   * import { encode } from "@stablelib/base64";
   * const encodedSecret = encode(new TextEncoder().encode("my-secret-value"));
   *
   * // Now pass the encoded secret to the function
   * createProjectSecret(orgId, projectId, "mySecretKey", encodedSecret);
   * ```
   */
  createProjectSecret: (
    orgId: string,
    projectId: string,
    keyName: string,
    secret: string
  ) => Promise<ProjectSecret>;

  /**
   * Lists all secrets for a project
   * @param orgId - Organization ID
   * @param projectId - Project ID
   */
  listProjectSecrets: (orgId: string, projectId: string) => Promise<ProjectSecret[]>;

  /**
   * Deletes a project secret
   * @param orgId - Organization ID
   * @param projectId - Project ID
   * @param keyName - Secret key name
   */
  deleteProjectSecret: (orgId: string, projectId: string, keyName: string) => Promise<void>;

  /**
   * Gets email configuration for a project
   * @param orgId - Organization ID
   * @param projectId - Project ID
   */
  getEmailSettings: (orgId: string, projectId: string) => Promise<EmailSettings>;

  /**
   * Updates email configuration
   * @param orgId - Organization ID
   * @param projectId - Project ID
   * @param settings - Email settings
   */
  updateEmailSettings: (
    orgId: string,
    projectId: string,
    settings: EmailSettings
  ) => Promise<EmailSettings>;

  /**
   * Gets OAuth settings for a project
   * @param orgId - Organization ID
   * @param projectId - Project ID
   */
  getOAuthSettings: (orgId: string, projectId: string) => Promise<OAuthSettings>;

  /**
   * Updates OAuth configuration
   * @param orgId - Organization ID
   * @param projectId - Project ID
   * @param settings - OAuth settings
   */
  updateOAuthSettings: (
    orgId: string,
    projectId: string,
    settings: OAuthSettings
  ) => Promise<OAuthSettings>;

  /**
   * Gets push settings for a project
   * @param orgId - Organization ID
   * @param projectId - Project ID
   */
  getPushSettings: (orgId: string, projectId: string) => Promise<PushSettings>;

  /**
   * Updates push configuration
   * @param orgId - Organization ID
   * @param projectId - Project ID
   * @param settings - Push settings
   */
  updatePushSettings: (
    orgId: string,
    projectId: string,
    settings: PushSettings
  ) => Promise<PushSettings>;

  /**
   * Creates an invitation to join an organization
   * @param orgId - Organization ID
   * @param email - Developer's email address
   * @param role - Role to assign (defaults to "admin")
   */
  inviteDeveloper: (orgId: string, email: string, role?: string) => Promise<OrganizationInvite>;

  /**
   * Lists all members of an organization
   * @param orgId - Organization ID
   */
  listOrganizationMembers: (orgId: string) => Promise<OrganizationMember[]>;

  /**
   * Lists all pending invitations for an organization
   * @param orgId - Organization ID
   */
  listOrganizationInvites: (orgId: string) => Promise<OrganizationInvite[]>;

  /**
   * Gets a specific invitation by code
   * @param orgId - Organization ID
   * @param inviteCode - Invitation UUID code
   */
  getOrganizationInvite: (orgId: string, inviteCode: string) => Promise<OrganizationInvite>;

  /**
   * Deletes an invitation
   * @param orgId - Organization ID
   * @param inviteCode - Invitation UUID code
   */
  deleteOrganizationInvite: (orgId: string, inviteCode: string) => Promise<{ message: string }>;

  /**
   * Updates a member's role
   * @param orgId - Organization ID
   * @param userId - User ID to update
   * @param role - New role to assign
   */
  updateMemberRole: (orgId: string, userId: string, role: string) => Promise<OrganizationMember>;

  /**
   * Removes a member from the organization
   * @param orgId - Organization ID
   * @param userId - User ID to remove
   */
  removeMember: (orgId: string, userId: string) => Promise<void>;

  /**
   * Accepts an organization invitation
   * @param code - Invitation UUID code
   */
  acceptInvite: (code: string) => Promise<{ message: string }>;

  /**
   * Returns the current OpenSecret developer API URL being used
   */
  apiUrl: string;
};

export const OpenSecretDeveloperContext = createContext<OpenSecretDeveloperContextType>({
  auth: {
    loading: true,
    developer: undefined
  },
  signIn: async () => {
    throw new Error("signIn called outside of OpenSecretDeveloper provider");
  },
  signUp: async () => {
    throw new Error("signUp called outside of OpenSecretDeveloper provider");
  },
  signOut: async () => {
    throw new Error("signOut called outside of OpenSecretDeveloper provider");
  },
  refetchDeveloper: async () => {
    throw new Error("refetchDeveloper called outside of OpenSecretDeveloper provider");
  },
  verifyEmail: platformApi.verifyPlatformEmail,
  requestNewVerificationCode: platformApi.requestNewPlatformVerificationCode,
  requestNewVerificationEmail: platformApi.requestNewPlatformVerificationCode,
  requestPasswordReset: platformApi.requestPlatformPasswordReset,
  confirmPasswordReset: platformApi.confirmPlatformPasswordReset,
  changePassword: platformApi.changePlatformPassword,
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
    throw new Error("getAttestationDocument called outside of OpenSecretDeveloper provider");
  },
  createOrganization: platformApi.createOrganization,
  listOrganizations: platformApi.listOrganizations,
  deleteOrganization: platformApi.deleteOrganization,
  createProject: platformApi.createProject,
  listProjects: platformApi.listProjects,
  getProject: platformApi.getProject,
  updateProject: platformApi.updateProject,
  deleteProject: platformApi.deleteProject,
  createProjectSecret: platformApi.createProjectSecret,
  listProjectSecrets: platformApi.listProjectSecrets,
  deleteProjectSecret: platformApi.deleteProjectSecret,
  getEmailSettings: platformApi.getEmailSettings,
  updateEmailSettings: platformApi.updateEmailSettings,
  getOAuthSettings: platformApi.getOAuthSettings,
  updateOAuthSettings: platformApi.updateOAuthSettings,
  getPushSettings: platformApi.getPushSettings,
  updatePushSettings: platformApi.updatePushSettings,
  inviteDeveloper: platformApi.inviteDeveloper,
  listOrganizationMembers: platformApi.listOrganizationMembers,
  listOrganizationInvites: platformApi.listOrganizationInvites,
  getOrganizationInvite: platformApi.getOrganizationInvite,
  deleteOrganizationInvite: platformApi.deleteOrganizationInvite,
  updateMemberRole: platformApi.updateMemberRole,
  removeMember: platformApi.removeMember,
  acceptInvite: platformApi.acceptInvite,
  apiUrl: ""
});

/**
 * Provider component for OpenSecret developer operations.
 * This provider is used for managing organizations, projects, and developer access.
 *
 * @param props - Configuration properties for the OpenSecret developer provider
 * @param props.children - React child components to be wrapped by the provider
 * @param props.apiUrl - URL of OpenSecret developer API
 * @param props.pcrConfig - Optional PCR0 trust policy enforced before session establishment; its environment defaults to production
 *
 * @example
 * ```tsx
 * <OpenSecretDeveloper
 *   apiUrl='https://developer.opensecret.cloud'
 * >
 *   <App />
 * </OpenSecretDeveloper>
 * ```
 */
export function OpenSecretDeveloper({
  children,
  apiUrl,
  pcrConfig = DEFAULT_PCR_CONFIG
}: {
  children: React.ReactNode;
  apiUrl: string;
  pcrConfig?: PcrConfig;
}) {
  const [auth, setAuth] = useState<OpenSecretDeveloperAuthState>({
    loading: true,
    developer: undefined
  });
  const authLoadGeneration = useRef(0);
  const authorityScope = snapshotTransportV2AuthorityScope(apiUrl, pcrConfig, "platform");
  const authorityScopeKey = authorityScope.key;
  const currentAuthorityScopeKey = useRef(authorityScopeKey);
  currentAuthorityScopeKey.current = authorityScopeKey;

  useEffect(() => {
    if (!apiUrl || apiUrl.trim() === "") {
      throw new Error(
        "OpenSecretDeveloper requires a non-empty apiUrl. Please provide a valid API endpoint URL."
      );
    }
    setPlatformApiUrl(apiUrl, pcrConfig);

    // Configure the apiConfig service with the platform URL
    // Using dynamic import to avoid circular dependencies
    import("./apiConfig")
      .then(({ apiConfig }) => {
        const appUrl = apiConfig.appApiUrl || "";
        apiConfig.configure(appUrl, apiUrl);
      })
      .catch((error) => {
        console.error("Failed to load apiConfig:", error);
        throw new Error(
          "Failed to initialize OpenSecretDeveloper - could not load required dependencies"
        );
      });
  }, [apiUrl, pcrConfig]);

  async function fetchDeveloper() {
    const generation = authLoadGeneration.current + 1;
    authLoadGeneration.current = generation;
    const capturedScope = authorityScope;
    const capturedScopeKey = authorityScopeKey;
    const ownsPublication = () =>
      authLoadGeneration.current === generation &&
      currentAuthorityScopeKey.current === capturedScopeKey;
    if (!readTransportV2Credentials(capturedScope.apiUrl, "platform")) {
      if (!ownsPublication()) return;
      setAuth({
        loading: false,
        developer: undefined
      });
      return;
    }

    const attempted = snapshotTransportV2Auth(capturedScope.apiUrl, "platform");
    let authority: Awaited<ReturnType<typeof transportV2AuthRuntime.authority>> | undefined;
    try {
      authority = await transportV2AuthRuntime.authority(
        capturedScope.apiUrl,
        capturedScope.pcrConfig,
        "platform"
      );
      const response = await platformApi.platformMeWithTransportV2Authority(
        capturedScope.apiUrl,
        capturedScope.pcrConfig,
        authority
      );
      const decision = transportV2ProfilePublicationDecision(authority.snapshot, ownsPublication());
      if (decision === "reload") {
        void fetchDeveloper();
        return;
      }
      if (decision === "discard") return;
      setAuth({
        loading: false,
        developer: {
          ...response.user,
          organizations: response.organizations
        }
      });
    } catch (error) {
      if (!ownsPublication()) return;
      const sentWith = authority?.snapshot ?? attempted;
      const decision = transportV2ProfilePublicationDecision(sentWith, true);
      if (decision === "reload") {
        void fetchDeveloper();
        return;
      }
      if (decision === "discard") return;
      console.error("Failed to fetch developer:", error);
      setAuth({
        loading: false,
        developer: undefined
      });
    }
  }

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

  useEffect(() => {
    setAuth({ loading: true, developer: undefined });
    void fetchDeveloper();
    const unsubscribe = subscribeTransportV2AuthInvalidation(
      authorityScope.apiUrl,
      "platform",
      () => {
        authLoadGeneration.current += 1;
        setAuth({ loading: false, developer: undefined });
      }
    );
    return () => {
      authLoadGeneration.current += 1;
      unsubscribe();
    };
  }, [authorityScopeKey]);

  async function signIn(email: string, password: string) {
    try {
      const { access_token, refresh_token } = await platformApi.platformLogin(email, password);
      await fetchDeveloper();
      return { access_token, refresh_token, id: "", email };
    } catch (error) {
      console.error("Login error:", error);
      throw error;
    }
  }

  async function signUp(email: string, password: string, invite_code: string, name?: string) {
    try {
      const { access_token, refresh_token } = await platformApi.platformRegister(
        email,
        password,
        invite_code,
        name
      );
      await fetchDeveloper();
      return { access_token, refresh_token, id: "", email, name };
    } catch (error) {
      console.error("Registration error:", error);
      throw error;
    }
  }

  const value: OpenSecretDeveloperContextType = {
    auth,
    signIn,
    signUp,
    refetchDeveloper: fetchDeveloper,
    signOut: async () => {
      const credentials = readTransportV2Credentials(apiUrl, "platform");
      const snapshot = snapshotTransportV2Auth(apiUrl, "platform");
      clearTransportV2CredentialsIfCurrent(snapshot);
      setAuth({ loading: false, developer: undefined });
      const refresh_token = credentials?.refreshToken;
      if (refresh_token) {
        try {
          await platformApi.platformLogout(refresh_token);
        } catch (error) {
          console.error("Error during logout:", error);
        }
      }
    },
    verifyEmail: platformApi.verifyPlatformEmail,
    requestNewVerificationCode: platformApi.requestNewPlatformVerificationCode,
    requestNewVerificationEmail: platformApi.requestNewPlatformVerificationCode,
    requestPasswordReset: platformApi.requestPlatformPasswordReset,
    confirmPasswordReset: platformApi.confirmPlatformPasswordReset,
    changePassword: platformApi.changePlatformPassword,
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
    createOrganization: platformApi.createOrganization,
    listOrganizations: platformApi.listOrganizations,
    deleteOrganization: platformApi.deleteOrganization,
    createProject: platformApi.createProject,
    listProjects: platformApi.listProjects,
    getProject: platformApi.getProject,
    updateProject: platformApi.updateProject,
    deleteProject: platformApi.deleteProject,
    createProjectSecret: platformApi.createProjectSecret,
    listProjectSecrets: platformApi.listProjectSecrets,
    deleteProjectSecret: platformApi.deleteProjectSecret,
    getEmailSettings: platformApi.getEmailSettings,
    updateEmailSettings: platformApi.updateEmailSettings,
    getOAuthSettings: platformApi.getOAuthSettings,
    updateOAuthSettings: platformApi.updateOAuthSettings,
    getPushSettings: platformApi.getPushSettings,
    updatePushSettings: platformApi.updatePushSettings,
    inviteDeveloper: platformApi.inviteDeveloper,
    listOrganizationMembers: platformApi.listOrganizationMembers,
    listOrganizationInvites: platformApi.listOrganizationInvites,
    getOrganizationInvite: platformApi.getOrganizationInvite,
    deleteOrganizationInvite: platformApi.deleteOrganizationInvite,
    updateMemberRole: platformApi.updateMemberRole,
    removeMember: platformApi.removeMember,
    acceptInvite: platformApi.acceptInvite,
    apiUrl
  };

  return (
    <OpenSecretDeveloperContext.Provider value={value}>
      {children}
    </OpenSecretDeveloperContext.Provider>
  );
}
