import {
  assertExpectedAccountPrincipal,
  guardAccountResponse,
  isAccountCredentialMismatchError
} from "./credentialIdentity";
import { getApiPcrConfig, getApiUrl } from "./api";
import { getPlatformApiUrl, getPlatformPcrConfig } from "./platformApi";
import { snapshotPcrConfig, type PcrConfig } from "./pcr";
import { getOrCreateTransportV2CacheRoot, readTransportV2Credentials } from "./transportV2/auth";
import {
  transportV2AuthRuntime,
  type TransportV2AuthRuntime,
  type TransportV2Authority
} from "./transportV2/authRuntime";
import { utf8, type TransportV2Credential, type TransportV2Header } from "./transportV2/protocol";
import {
  transportV2LogicalTarget,
  transportV2Runtime,
  type TransportV2OAuthProvider,
  type TransportV2Runtime
} from "./transportV2/runtime";

interface ApiResponse<T> {
  status: number;
  hasData: boolean;
  data?: T;
  error?: string;
  errorCode?: string;
  headers?: Headers;
}

/** @internal Exported for deterministic transport tests, not from the package entry point. */
export interface EncryptedApiDependencies {
  runtime: TransportV2Runtime;
  auth: TransportV2AuthRuntime;
  readCredentials: typeof readTransportV2Credentials;
  getApiPcrConfig: typeof getApiPcrConfig;
  getApiUrl: typeof getApiUrl;
  getPlatformApiUrl: typeof getPlatformApiUrl;
  getPlatformPcrConfig: typeof getPlatformPcrConfig;
}

const defaultDependencies: EncryptedApiDependencies = {
  runtime: transportV2Runtime,
  auth: transportV2AuthRuntime,
  readCredentials: readTransportV2Credentials,
  getApiPcrConfig: () => getApiPcrConfig(),
  getApiUrl: () => getApiUrl(),
  getPlatformApiUrl: () => getPlatformApiUrl(),
  getPlatformPcrConfig: () => getPlatformPcrConfig()
};

function isPlatformUrl(url: string): boolean {
  return new URL(url).pathname.includes("/platform/");
}

function endpoint(
  url: string,
  dependencies: EncryptedApiDependencies
): { apiUrl: string; pcrConfig: PcrConfig; kind: "user" | "platform"; target: string } {
  const kind = isPlatformUrl(url) ? "platform" : "user";
  const apiUrl = kind === "platform" ? dependencies.getPlatformApiUrl() : dependencies.getApiUrl();
  return endpointFor(
    url,
    apiUrl,
    kind === "platform" ? dependencies.getPlatformPcrConfig() : dependencies.getApiPcrConfig(),
    kind
  );
}

function endpointFor(
  url: string,
  apiUrl: string,
  pcrConfig: PcrConfig | undefined,
  kind: "user" | "platform"
): { apiUrl: string; pcrConfig: PcrConfig; kind: "user" | "platform"; target: string } {
  if ((kind === "platform") !== isPlatformUrl(url)) {
    throw new Error("Transport v2 authenticated request authority kind does not match its route.");
  }
  return {
    apiUrl,
    pcrConfig: snapshotPcrConfig(pcrConfig),
    kind,
    target: transportV2LogicalTarget(apiUrl, url)
  };
}

function anonymousPath(kind: "user" | "platform", target: string): boolean {
  const path = target.split("?", 1)[0];
  if (kind === "platform") {
    return (
      path === "/platform/login" ||
      path === "/platform/register" ||
      path.startsWith("/platform/password-reset/") ||
      path === "/platform/logout"
    );
  }
  return (
    path === "/login" ||
    path === "/register" ||
    path.startsWith("/password-reset/") ||
    path === "/auth/github" ||
    path === "/auth/github/callback" ||
    path === "/auth/google" ||
    path === "/auth/google/callback" ||
    path === "/auth/apple" ||
    path === "/auth/apple/callback" ||
    path === "/auth/apple/native" ||
    path === "/logout"
  );
}

function oauthProvider(target: string): TransportV2OAuthProvider | undefined {
  const match = target.split("?", 1)[0].match(/^\/auth\/(github|google|apple)(?:\/callback)?$/u);
  return match?.[1] as TransportV2OAuthProvider | undefined;
}

function oauthCallback(
  target: string,
  data: unknown
): { provider: TransportV2OAuthProvider; state: string } | undefined {
  if (!target.split("?", 1)[0].endsWith("/callback")) return undefined;
  const provider = oauthProvider(target);
  if (!provider || typeof data !== "object" || data === null || Array.isArray(data)) {
    throw new Error("OAuth callback requires its initiating Transport V2 state.");
  }
  const state = (data as Record<string, unknown>).state;
  if (typeof state !== "string" || state.length === 0) {
    throw new Error("OAuth callback requires its initiating Transport V2 state.");
  }
  return { provider, state };
}

function logicalHeaders(body: Uint8Array | undefined): TransportV2Header[] | undefined {
  return body === undefined ? undefined : [{ name: "content-type", value: "application/json" }];
}

class ResponsesStreamError extends Error {
  constructor(
    message: string,
    readonly code?: string
  ) {
    super(message);
  }
}

function streamErrorDetails(value: unknown): ResponsesStreamError | undefined {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
  const record = value as Record<string, unknown>;
  if (typeof record.message !== "string") return undefined;
  return new ResponsesStreamError(
    record.message,
    typeof record.code === "string" ? record.code : undefined
  );
}

function streamEventError(value: unknown, fallback: string, failedResponse = false): Error {
  if (typeof value === "object" && value !== null) {
    const record = value as Record<string, unknown>;
    const directError = streamErrorDetails(record) ?? streamErrorDetails(record.error);
    if (directError) return directError;
    // response.failed carries the persisted public error inside the response
    // object. Project only its message/code, never output or arbitrary fields.
    if (failedResponse && typeof record.response === "object" && record.response !== null) {
      const error = streamErrorDetails((record.response as Record<string, unknown>).error);
      if (error) return error;
    }
  }
  return new Error(fallback);
}

function completedResponseFromSse(text: string): unknown {
  for (const eventBlock of text.split(/\r?\n\r?\n/u)) {
    if (!eventBlock.trim()) continue;
    let eventName = "";
    const data: string[] = [];
    for (const line of eventBlock.split(/\r?\n/u)) {
      if (line.startsWith("event:")) eventName = line.slice(6).trim();
      else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /u, ""));
    }
    if (data.length === 0 || data.join("\n") === "[DONE]") continue;
    let value: unknown;
    try {
      value = JSON.parse(data.join("\n"));
    } catch {
      throw new Error("Transport v2 Responses stream contained invalid JSON.");
    }
    const type =
      typeof value === "object" &&
      value !== null &&
      typeof (value as Record<string, unknown>).type === "string"
        ? ((value as Record<string, unknown>).type as string)
        : eventName;
    if (type === "response.completed") {
      const completed = (value as Record<string, unknown>).response;
      if (typeof completed !== "object" || completed === null) {
        throw new Error("Transport v2 Responses completion contained no response.");
      }
      return completed;
    }
    if (type === "response.error" || type === "response.failed" || type === "error") {
      throw streamEventError(value, "The response failed.", type === "response.failed");
    }
    if (type === "response.cancelled") {
      throw streamEventError(value, "The response was cancelled.");
    }
  }
  throw new Error("Transport v2 Responses stream ended without a completed response.");
}

function compatibilityResponseShape(url: string, value: unknown): unknown {
  const path = new URL(url).pathname;
  if (
    (path.endsWith("/auth/github") || path.endsWith("/auth/google")) &&
    typeof value === "object" &&
    value !== null &&
    typeof (value as Record<string, unknown>).state === "string"
  ) {
    const { state, ...rest } = value as Record<string, unknown>;
    return { ...rest, csrf_token: state };
  }
  return value;
}

async function readError(response: Response, fallback?: string): Promise<string> {
  let text = "";
  try {
    text = await response.text();
    const value = JSON.parse(text) as { message?: unknown; error?: unknown };
    if (typeof value.message === "string") return value.message;
    if (typeof value.error === "string") return value.error;
  } catch {
    if (text.trim()) return text;
  }
  return fallback || `HTTP error! Status: ${response.status}`;
}

async function performTransportV2Call<T, U>(
  url: string,
  resolved: ReturnType<typeof endpoint>,
  method: string,
  data: T,
  credential: TransportV2Credential | undefined,
  authority: TransportV2Authority | undefined,
  cacheNamespaceRoot: Uint8Array | undefined,
  errorFallback: string | undefined,
  dependencies: EncryptedApiDependencies
): Promise<ApiResponse<U>> {
  const body = data === undefined ? undefined : utf8(JSON.stringify(data));
  try {
    const exchange = await dependencies.runtime.request({
      apiUrl: resolved.apiUrl,
      pcrConfig: resolved.pcrConfig,
      beforeSend: authority ? () => authority.assertCurrent() : undefined,
      oauthCallback: oauthCallback(resolved.target, data),
      request: {
        credential,
        cacheNamespaceRoot,
        method: method.toUpperCase(),
        target: resolved.target,
        headers: logicalHeaders(body),
        body
      }
    });
    const assertAccount = () => {
      if (authority) {
        assertExpectedAccountPrincipal(
          authority.credentials.principalId,
          dependencies.readCredentials(resolved.apiUrl, resolved.kind)?.principalId ?? null
        );
      }
    };
    const response = authority
      ? guardAccountResponse(exchange.response, assertAccount)
      : exchange.response;
    if (authority) {
      dependencies.auth.noteResponse(
        response,
        resolved.apiUrl,
        resolved.pcrConfig,
        resolved.kind,
        authority
      );
    }
    if (!response.ok) {
      const error = await readError(response, errorFallback);
      assertAccount();
      return { status: response.status, hasData: false, error, headers: response.headers };
    }

    const text = await response.text();
    assertAccount();
    if (method.toUpperCase() === "POST" && resolved.target.split("?", 1)[0] === "/v1/responses") {
      return {
        status: response.status,
        hasData: true,
        data: completedResponseFromSse(text) as U
      };
    }
    if (text.length === 0) {
      return { status: response.status, hasData: true, data: undefined as U };
    }
    let value: unknown;
    try {
      value = JSON.parse(text);
    } catch {
      return {
        status: 500,
        hasData: false,
        error: "Failed to parse the authenticated response"
      };
    }
    const provider = oauthProvider(resolved.target);
    if (
      provider &&
      !resolved.target.split("?", 1)[0].endsWith("/callback") &&
      typeof value === "object" &&
      value !== null &&
      typeof (value as Record<string, unknown>).state === "string"
    ) {
      exchange.rememberOAuthContinuation(
        provider,
        (value as Record<string, unknown>).state as string
      );
    }
    return {
      status: response.status,
      hasData: true,
      data: compatibilityResponseShape(url, value) as U
    };
  } catch (error) {
    if (isAccountCredentialMismatchError(error)) throw error;
    return {
      status: 500,
      hasData: false,
      error: error instanceof Error ? error.message : "Unknown error occurred",
      ...(error instanceof ResponsesStreamError && error.code !== undefined
        ? { errorCode: error.code }
        : {})
    };
  } finally {
    body?.fill(0);
    cacheNamespaceRoot?.fill(0);
  }
}

function unwrapApiResponse<U>(response: ApiResponse<U>, missingDataMessage: string): U {
  if (response.error)
    throw Object.assign(new Error(response.error), {
      status: response.status,
      headers: response.headers,
      ...(response.errorCode !== undefined ? { code: response.errorCode } : {})
    });
  if (!response.hasData) throw new Error(missingDataMessage);
  return response.data as U;
}

export async function authenticatedApiCall<T, U>(
  url: string,
  method: string,
  data: T,
  errorMessage?: string,
  expectedUserId?: string
): Promise<U> {
  return authenticatedApiCallWithDependencies(
    url,
    method,
    data,
    errorMessage,
    defaultDependencies,
    expectedUserId
  );
}

export interface TransportV2AuthenticatedCallResult<U> {
  data: U;
  authority: TransportV2Authority;
  apiUrl: string;
  pcrConfig: PcrConfig;
  kind: "user" | "platform";
}

async function performAuthenticatedApiCall<T, U>(
  url: string,
  resolved: ReturnType<typeof endpoint>,
  method: string,
  data: T,
  errorFallback: string | undefined,
  dependencies: EncryptedApiDependencies,
  expectedUserId?: string
): Promise<TransportV2AuthenticatedCallResult<U>> {
  const expectedPrincipal =
    expectedUserId ?? dependencies.readCredentials(resolved.apiUrl, resolved.kind)?.principalId;
  const assertAccount = () => {
    if (expectedPrincipal !== undefined) {
      assertExpectedAccountPrincipal(
        expectedPrincipal,
        dependencies.readCredentials(resolved.apiUrl, resolved.kind)?.principalId ?? null
      );
    }
  };
  assertAccount();
  const authority = await dependencies.auth.authority(
    resolved.apiUrl,
    resolved.pcrConfig,
    resolved.kind
  );
  assertAccount();
  if (expectedPrincipal !== undefined)
    assertExpectedAccountPrincipal(expectedPrincipal, authority.credentials.principalId);
  const response = await performTransportV2Call<T, U>(
    url,
    resolved,
    method,
    data,
    authority.credential,
    authority,
    undefined,
    errorFallback,
    dependencies
  );
  return {
    data: unwrapApiResponse(response, "No data received from the server"),
    authority,
    apiUrl: resolved.apiUrl,
    pcrConfig: resolved.pcrConfig,
    kind: resolved.kind
  };
}

/** @internal Selects one authority and uses it for the send, final fence, and caller CAS. */
export async function authenticatedApiCallWithAuthority<T, U>(
  url: string,
  method: string,
  data: T,
  errorFallback: string | undefined,
  scope: { apiUrl: string; pcrConfig?: PcrConfig; kind: "user" | "platform" }
): Promise<TransportV2AuthenticatedCallResult<U>> {
  return performAuthenticatedApiCall(
    url,
    endpointFor(url, scope.apiUrl, scope.pcrConfig, scope.kind),
    method,
    data,
    errorFallback,
    defaultDependencies
  );
}

/** @internal Deterministic dependency boundary for Transport V2 tests. */
export async function authenticatedApiCallWithAuthorityAndDependencies<T, U>(
  url: string,
  method: string,
  data: T,
  errorFallback: string | undefined,
  scope: { apiUrl: string; pcrConfig?: PcrConfig; kind: "user" | "platform" },
  dependencies: EncryptedApiDependencies
): Promise<TransportV2AuthenticatedCallResult<U>> {
  return performAuthenticatedApiCall(
    url,
    endpointFor(url, scope.apiUrl, scope.pcrConfig, scope.kind),
    method,
    data,
    errorFallback,
    dependencies
  );
}

/** @internal Sends with an authority already selected for the exact captured endpoint policy. */
export async function authenticatedApiCallWithSelectedAuthority<T, U>(
  url: string,
  method: string,
  data: T,
  errorFallback: string | undefined,
  scope: {
    apiUrl: string;
    pcrConfig?: PcrConfig;
    kind: "user" | "platform";
    authority: TransportV2Authority;
  }
): Promise<U> {
  const resolved = endpointFor(url, scope.apiUrl, scope.pcrConfig, scope.kind);
  if (
    scope.authority.snapshot.kind !== resolved.kind ||
    scope.authority.snapshot.apiOrigin !== new URL(resolved.apiUrl).origin
  ) {
    throw new Error("Transport v2 authenticated request authority does not match its endpoint.");
  }
  const response = await performTransportV2Call<T, U>(
    url,
    resolved,
    method,
    data,
    scope.authority.credential,
    scope.authority,
    undefined,
    errorFallback,
    defaultDependencies
  );
  return unwrapApiResponse(response, "No data received from the server");
}

/** @internal Exported for deterministic transport tests, not from the package entry point. */
export async function authenticatedApiCallWithDependencies<T, U>(
  url: string,
  method: string,
  data: T,
  errorFallback: string | undefined,
  dependencies: EncryptedApiDependencies,
  expectedUserId?: string
): Promise<U> {
  const resolved = endpoint(url, dependencies);
  return (
    await performAuthenticatedApiCall<T, U>(
      url,
      resolved,
      method,
      data,
      errorFallback,
      dependencies,
      expectedUserId
    )
  ).data;
}

export async function openAiAuthenticatedApiCall<T, U>(
  url: string,
  method: string,
  data: T,
  errorMessage?: string,
  apiKey?: string
): Promise<U> {
  return openAiAuthenticatedApiCallWithDependencies(
    url,
    method,
    data,
    errorMessage,
    apiKey,
    defaultDependencies
  );
}

/** @internal Exported for deterministic transport tests, not from the package entry point. */
export async function openAiAuthenticatedApiCallWithDependencies<T, U>(
  url: string,
  method: string,
  data: T,
  errorFallback: string | undefined,
  apiKey: string | undefined,
  dependencies: EncryptedApiDependencies
): Promise<U> {
  const resolved = endpoint(url, dependencies);
  let credential: TransportV2Credential;
  let authority: TransportV2Authority | undefined;
  if (apiKey !== undefined) {
    if (!apiKey) throw new Error("API key cannot be empty");
    credential = { kind: "api_key", value: apiKey };
  } else {
    authority = await dependencies.auth.authority(resolved.apiUrl, resolved.pcrConfig, "user");
    credential = authority.credential;
  }
  const root = getOrCreateTransportV2CacheRoot(resolved.apiUrl);
  const response = await performTransportV2Call<T, U>(
    url,
    resolved,
    method,
    data,
    credential,
    authority,
    root,
    errorFallback,
    dependencies
  );
  return unwrapApiResponse(response, errorFallback || `Request to ${url} failed`);
}

export async function encryptedApiCall<T, U>(
  url: string,
  method: string,
  data: T,
  accessToken?: string,
  errorMessage?: string
): Promise<U> {
  return encryptedApiCallWithDependencies(
    url,
    method,
    data,
    accessToken,
    errorMessage,
    defaultDependencies
  );
}

/** @internal Exported for deterministic transport tests, not from the package entry point. */
export async function encryptedApiCallWithDependencies<T, U>(
  url: string,
  method: string,
  data: T,
  accessToken: string | undefined,
  errorFallback: string | undefined,
  dependencies: EncryptedApiDependencies
): Promise<U> {
  const resolved = endpoint(url, dependencies);
  let credential: TransportV2Credential | undefined;
  let authority: TransportV2Authority | undefined;
  if (accessToken !== undefined) {
    if (!accessToken) throw new Error("Access token cannot be empty");
    credential = { kind: "bearer", value: accessToken };
  } else if (!anonymousPath(resolved.kind, resolved.target)) {
    const stored = dependencies.readCredentials(resolved.apiUrl, resolved.kind);
    if (stored) {
      authority = await dependencies.auth.authority(
        resolved.apiUrl,
        resolved.pcrConfig,
        resolved.kind
      );
      credential = authority.credential;
    }
  }
  const response = await performTransportV2Call<T, U>(
    url,
    resolved,
    method,
    data,
    credential,
    authority,
    undefined,
    errorFallback,
    dependencies
  );
  return unwrapApiResponse(response, "No data received from the server");
}
