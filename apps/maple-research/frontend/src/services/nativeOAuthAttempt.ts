import { invoke } from "@tauri-apps/api/core";
import {
  installNativeOAuthHandoffCredentials,
  prepareNativeOAuthHandoff,
  type NativeOAuthHandoffAuthFence
} from "@mapleai/sdk";
import { getSafeInternalRedirect } from "@/utils/internalRedirect";
import { buildTransportV2DesktopAuthUrl } from "./desktopOAuthTransport";

type InvokeCommand = (command: string, args?: Parameters<typeof invoke>[1]) => Promise<unknown>;

const PENDING_NATIVE_OAUTH_ATTEMPT_KEY = "maple_pending_native_oauth_attempt_v2";
export const PENDING_NATIVE_OAUTH_ATTEMPT_TTL_MS = 15 * 60 * 1000;

export interface NativeOAuthBeginResponse {
  nativeOAuthAttempt: string;
  sessionId: string;
  requestId: string;
}

export interface NativeOAuthAccount {
  userId: string;
  email: string | null;
}

interface NativeOAuthRedeemResponse extends NativeOAuthAccount {
  accessToken: string;
  refreshToken: string;
}

interface NativeOAuthConfirmationOptions {
  confirmAccount: (account: NativeOAuthAccount) => Promise<boolean>;
  signal?: AbortSignal;
}

export interface NativeOAuthNavigation {
  selectedPlan?: string;
  next?: string;
  redemptionCode?: string;
}

export type NativeOAuthProvider = "github" | "google" | "apple";

export interface PendingNativeOAuthAttempt extends NativeOAuthNavigation {
  attemptId: string;
  sessionId: string;
  requestId: string;
  apiUrl: string;
  expectedAuth: NativeOAuthHandoffAuthFence;
  startedAt: number;
}

export type NativeOAuthCallbackAuthorization =
  | "accepted"
  | "already_authenticated"
  | "missing_or_expired_attempt";

const ATTEMPT_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/u;
const TRANSPORT_ID_PATTERN = /^[0-9a-f]{32}$/u;
const NIL_ATTEMPT = "00000000-0000-0000-0000-000000000000";

export function isNativeOAuthAttemptId(value: unknown): value is string {
  return typeof value === "string" && value !== NIL_ATTEMPT && ATTEMPT_PATTERN.test(value);
}

export function isNativeOAuthTransportId(value: unknown): value is string {
  return typeof value === "string" && TRANSPORT_ID_PATTERN.test(value);
}

export function isNativeOAuthHandoffGrant(value: unknown): value is string {
  if (typeof value !== "string" || value.length === 0) return false;
  if (new TextEncoder().encode(value).byteLength > 4 * 1024) return false;
  const segments = value.split(".");
  return (
    segments.length === 3 &&
    segments.every(
      (segment) =>
        segment.length > 0 && segment.length % 4 !== 1 && /^[A-Za-z0-9_-]+$/u.test(segment)
    )
  );
}

function isExpectedAuthFence(value: unknown): value is NativeOAuthHandoffAuthFence {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  return (
    Object.keys(object).length === 4 &&
    object.version === 1 &&
    typeof object.apiOrigin === "string" &&
    object.apiOrigin.length > 0 &&
    typeof object.userRevision === "number" &&
    Number.isSafeInteger(object.userRevision) &&
    object.userRevision >= 0 &&
    object.principalId === null
  );
}

export function readPendingNativeOAuthAttempt(): PendingNativeOAuthAttempt | null {
  let encoded: string | null;
  try {
    encoded = localStorage.getItem(PENDING_NATIVE_OAUTH_ATTEMPT_KEY);
  } catch {
    return null;
  }
  if (!encoded) return null;

  try {
    const attempt = JSON.parse(encoded) as Partial<PendingNativeOAuthAttempt>;
    if (
      !isNativeOAuthAttemptId(attempt.attemptId) ||
      !isNativeOAuthTransportId(attempt.sessionId) ||
      !isNativeOAuthTransportId(attempt.requestId) ||
      typeof attempt.apiUrl !== "string" ||
      !attempt.apiUrl ||
      !isExpectedAuthFence(attempt.expectedAuth) ||
      typeof attempt.startedAt !== "number" ||
      !Number.isSafeInteger(attempt.startedAt) ||
      attempt.startedAt < 0 ||
      (attempt.selectedPlan !== undefined && typeof attempt.selectedPlan !== "string") ||
      (attempt.redemptionCode !== undefined && typeof attempt.redemptionCode !== "string") ||
      (attempt.next !== undefined && getSafeInternalRedirect(attempt.next) !== attempt.next)
    ) {
      throw new Error("Invalid pending native OAuth attempt");
    }
    return attempt as PendingNativeOAuthAttempt;
  } catch {
    try {
      localStorage.removeItem(PENDING_NATIVE_OAUTH_ATTEMPT_KEY);
    } catch {
      // A later authorization check will continue to fail closed.
    }
    return null;
  }
}

function removePendingNativeOAuthAttempt(attemptId?: string): boolean {
  try {
    if (attemptId && readPendingNativeOAuthAttempt()?.attemptId !== attemptId) return false;
    localStorage.removeItem(PENDING_NATIVE_OAUTH_ATTEMPT_KEY);
    return localStorage.getItem(PENDING_NATIVE_OAUTH_ATTEMPT_KEY) === null;
  } catch {
    return false;
  }
}

export async function beginNativeOAuthAttempt(
  apiUrl: string,
  navigation: NativeOAuthNavigation = {},
  now = Date.now(),
  invokeCommand: InvokeCommand = invoke
): Promise<NativeOAuthBeginResponse> {
  if (!Number.isSafeInteger(now) || now < 0) {
    throw new Error("Cannot start native OAuth with an invalid timestamp");
  }
  const next = getSafeInternalRedirect(navigation.next) ?? undefined;
  const preparedAuth = prepareNativeOAuthHandoff(apiUrl);
  const response = (await invokeCommand("native_oauth_begin", {
    request: {
      apiUrl: preparedAuth.expectedAuth.apiOrigin,
      cacheNamespaceRootBase64: preparedAuth.cacheNamespaceRootBase64
    }
  })) as NativeOAuthBeginResponse;
  if (
    !isNativeOAuthAttemptId(response.nativeOAuthAttempt) ||
    !isNativeOAuthTransportId(response.sessionId) ||
    !isNativeOAuthTransportId(response.requestId)
  ) {
    throw new Error("Native OAuth initiation returned invalid state");
  }

  const pending: PendingNativeOAuthAttempt = {
    attemptId: response.nativeOAuthAttempt,
    sessionId: response.sessionId,
    requestId: response.requestId,
    apiUrl: preparedAuth.expectedAuth.apiOrigin,
    expectedAuth: preparedAuth.expectedAuth,
    startedAt: now,
    ...(navigation.selectedPlan ? { selectedPlan: navigation.selectedPlan } : {}),
    ...(navigation.redemptionCode ? { redemptionCode: navigation.redemptionCode } : {}),
    ...(next ? { next } : {})
  };
  try {
    localStorage.setItem(PENDING_NATIVE_OAUTH_ATTEMPT_KEY, JSON.stringify(pending));
  } catch (error) {
    await invokeCommand("native_oauth_cancel", {
      request: { nativeOAuthAttempt: response.nativeOAuthAttempt }
    }).catch(() => undefined);
    throw error;
  }
  return response;
}

export async function startNativeOAuth(
  provider: NativeOAuthProvider,
  apiUrl: string,
  navigation: NativeOAuthNavigation = {},
  invokeCommand: InvokeCommand = invoke
): Promise<void> {
  const prepared = await beginNativeOAuthAttempt(apiUrl, navigation, Date.now(), invokeCommand);
  const url = buildTransportV2DesktopAuthUrl({
    provider,
    nativeSessionId: prepared.sessionId,
    nativeRequestId: prepared.requestId
  });
  try {
    await invokeCommand("plugin:opener|open_url", { url });
  } catch (error) {
    await cancelNativeOAuthAttempt(prepared.nativeOAuthAttempt, invokeCommand).catch(
      () => undefined
    );
    throw error;
  }
}

export async function cancelNativeOAuthAttempt(
  attemptId: string,
  invokeCommand: InvokeCommand = invoke
): Promise<void> {
  try {
    await invokeCommand("native_oauth_cancel", {
      request: { nativeOAuthAttempt: attemptId }
    });
  } finally {
    removePendingNativeOAuthAttempt(attemptId);
  }
}

export async function redeemNativeOAuthGrant(
  handoffGrant: string,
  options: NativeOAuthConfirmationOptions,
  invokeCommand: InvokeCommand = invoke
): Promise<PendingNativeOAuthAttempt | null> {
  const pending = readPendingNativeOAuthAttempt();
  if (!pending) throw new Error("Native authentication is not pending; restart sign-in");

  const stillPending = () =>
    !options.signal?.aborted &&
    readPendingNativeOAuthAttempt()?.attemptId === pending.attemptId &&
    isNativeOAuthAttemptCurrent(pending);
  let response: NativeOAuthRedeemResponse | undefined;
  try {
    if (!stillPending()) return null;
    response = (await invokeCommand("native_oauth_redeem", {
      request: { handoffGrant }
    })) as NativeOAuthRedeemResponse;

    // Redemption is one-use. Only this function holds its credentials while
    // the user checks the verified account; the dialog receives no tokens.
    if (!stillPending()) return null;
    if (
      !response ||
      typeof response.userId !== "string" ||
      response.userId.length === 0 ||
      (response.email !== null && typeof response.email !== "string") ||
      typeof response.accessToken !== "string" ||
      response.accessToken.length === 0 ||
      typeof response.refreshToken !== "string" ||
      response.refreshToken.length === 0
    ) {
      throw new Error("Native authentication returned an invalid account");
    }
    const approved = await confirmNativeOAuthAccount(
      { userId: response.userId, email: response.email },
      options,
      pending.startedAt + PENDING_NATIVE_OAUTH_ATTEMPT_TTL_MS - Date.now()
    );
    if (!approved || !stillPending()) return null;

    // The existing synchronous installer rechecks the original anonymous
    // auth generation. A login/logout during confirmation cannot be replaced
    // by this older result, even if the UI has not observed that change yet.
    const installed = installNativeOAuthHandoffCredentials(
      pending.apiUrl,
      { accessToken: response.accessToken, refreshToken: response.refreshToken },
      pending.expectedAuth,
      response.userId
    );
    if (!installed.principalId) {
      throw new Error("Native authentication did not establish an account");
    }
    return pending;
  } catch (error) {
    if (options.signal?.aborted) return null;
    throw error;
  } finally {
    // Drop our references on approval, cancellation, expiry, or failure. JS
    // strings cannot be reliably zeroized; nothing is persisted before approval.
    response = undefined;
    // An ambiguous native failure also consumes the local attempt. Never
    // resend it, and never erase a newer attempt's navigation state.
    removePendingNativeOAuthAttempt(pending.attemptId);
  }
}

function isNativeOAuthAttemptCurrent(attempt: PendingNativeOAuthAttempt, now = Date.now()) {
  const age = now - attempt.startedAt;
  return Number.isSafeInteger(now) && age >= 0 && age <= PENDING_NATIVE_OAUTH_ATTEMPT_TTL_MS;
}

async function confirmNativeOAuthAccount(
  account: NativeOAuthAccount,
  options: NativeOAuthConfirmationOptions,
  remainingMs: number
): Promise<boolean> {
  if (options.signal?.aborted || remainingMs <= 0) return false;
  let timeout: ReturnType<typeof setTimeout> | undefined;
  let onAbort: (() => void) | undefined;
  try {
    return await new Promise<boolean>((resolve, reject) => {
      onAbort = () => resolve(false);
      options.signal?.addEventListener("abort", onAbort, { once: true });
      timeout = setTimeout(() => resolve(false), remainingMs);
      Promise.resolve()
        .then(() => (options.signal?.aborted ? false : options.confirmAccount(account)))
        .then(resolve, reject);
    });
  } finally {
    if (timeout !== undefined) clearTimeout(timeout);
    if (onAbort) options.signal?.removeEventListener("abort", onAbort);
  }
}

export function authorizeNativeOAuthCallback(
  isAuthenticated: boolean,
  now = Date.now()
): NativeOAuthCallbackAuthorization {
  if (isAuthenticated) return "already_authenticated";
  const attempt = readPendingNativeOAuthAttempt();
  if (!attempt) return "missing_or_expired_attempt";

  if (!isNativeOAuthAttemptCurrent(attempt, now)) {
    removePendingNativeOAuthAttempt(attempt.attemptId);
    return "missing_or_expired_attempt";
  }
  return "accepted";
}
