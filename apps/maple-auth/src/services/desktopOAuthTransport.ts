import {
  authEnvironment,
  isNativeAppVariantAllowed,
  type AuthEnvironment,
  nativeAuthReturnUrl,
  type NativeAppVariant
} from "@/config/authEnvironment";

export type DesktopOAuthProvider = "github" | "google" | "apple";

const DESKTOP_OAUTH_TRANSPORT_KEY = "maple_desktop_oauth_transport_v1";
const REDIRECT_TO_NATIVE_KEY = "redirect-to-native";
const TRANSPORT_V2_PENDING_KEY = "maple_desktop_oauth_pending_v2";
const TRANSPORT_V2_MINT_CLAIM_KEY = "maple_desktop_oauth_mint_claim_v2";
const TRANSPORT_V2_INITIATION_CLAIM_KEY = "maple_desktop_oauth_initiation_claim_v2";

export const TRANSPORT_V2_PENDING_TTL_MS = 15 * 60 * 1000;
export const TRANSPORT_V2_NATIVE_SESSION_QUERY = "native_session_id";
export const TRANSPORT_V2_NATIVE_REQUEST_QUERY = "native_request_id";

const TRANSPORT_V2_ID_PATTERN = /^[0-9a-f]{32}$/u;
const COMPACT_GRANT_PATTERN = /^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$/u;
const MAX_HANDOFF_GRANT_LENGTH = 4096;

type NativeDestination =
  | {
      nativeApp?: undefined;
      nativeAppVariant?: NativeAppVariant;
      returnPort?: never;
      returnState?: never;
      environment?: never;
    }
  | {
      nativeApp: "agent";
      nativeAppVariant?: never;
      returnPort: number;
      returnState: string;
      environment: AuthEnvironment;
    };

export type NativeOAuthInput = NativeDestination & {
  provider: DesktopOAuthProvider;
  nativeSessionId: string;
  nativeRequestId: string;
};

export type TransportV2DesktopOAuthState = NativeOAuthInput & { startedAt: number };
export type NativeAuthReturn = { url: string; expiresAt?: number };

type NativeHandoffGrantIssuer = (
  nativeSessionId: string,
  nativeRequestId: string
) => Promise<{ grant: string; expires_at?: number }>;

export function isAgentReturnPort(value: unknown): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 1 && value <= 65535;
}

/** Stored destinations are untrusted and must still match this compiled environment. */
export function isNativeTargetAllowed(value: unknown): boolean {
  if (!value || typeof value !== "object") return false;
  const target = value as Record<string, unknown>;
  if (target.nativeApp === "agent") {
    return (
      target.nativeAppVariant === undefined &&
      target.environment === authEnvironment() &&
      isAgentReturnPort(target.returnPort) &&
      isTransportV2PublicId(target.returnState)
    );
  }
  return (
    target.nativeApp === undefined &&
    target.returnPort === undefined &&
    target.returnState === undefined &&
    target.environment === undefined &&
    isNativeAppVariantAllowed(target.nativeAppVariant)
  );
}

export function nativeAppLabel(target: TransportV2DesktopOAuthState): string {
  if (target.nativeApp !== "agent") return "Maple";
  return authEnvironment() === "development" ? "Maple Agent Dev" : "Maple Agent";
}

function isDesktopOAuthProvider(value: unknown): value is DesktopOAuthProvider {
  return value === "github" || value === "google" || value === "apple";
}

export function isTransportV2PublicId(value: unknown): value is string {
  return typeof value === "string" && TRANSPORT_V2_ID_PATTERN.test(value);
}

function assertTransportV2PublicId(value: unknown, label: string): asserts value is string {
  if (!isTransportV2PublicId(value)) {
    throw new Error(`Desktop authentication ${label} is missing or invalid`);
  }
}

function pendingClaim(state: NativeOAuthInput): string {
  const identity = `${state.provider}:${state.nativeSessionId}:${state.nativeRequestId}`;
  return state.nativeApp === "agent"
    ? `${identity}:agent:${state.environment}:${state.returnPort}:${state.returnState}`
    : `${identity}${state.nativeAppVariant ? `:${state.nativeAppVariant}` : ""}`;
}

function hasValidTimestamp(startedAt: unknown, now: number): startedAt is number {
  if (
    typeof startedAt !== "number" ||
    !Number.isSafeInteger(startedAt) ||
    startedAt < 0 ||
    !Number.isSafeInteger(now) ||
    now < 0
  ) {
    return false;
  }

  const age = now - startedAt;
  return age >= 0 && age <= TRANSPORT_V2_PENDING_TTL_MS;
}

function removeTransportV2PendingState(): void {
  sessionStorage.removeItem(TRANSPORT_V2_PENDING_KEY);
  sessionStorage.removeItem(TRANSPORT_V2_INITIATION_CLAIM_KEY);
  sessionStorage.removeItem(TRANSPORT_V2_MINT_CLAIM_KEY);
}

export function markTransportV2DesktopOAuth(state: NativeOAuthInput, now = Date.now()): void {
  if (!isDesktopOAuthProvider(state.provider)) {
    throw new Error("Desktop authentication provider is missing or invalid");
  }
  if (!isNativeTargetAllowed(state)) {
    throw new Error("Native application does not match this authentication environment");
  }
  assertTransportV2PublicId(state.nativeSessionId, "native session");
  assertTransportV2PublicId(state.nativeRequestId, "native request");
  if (!Number.isSafeInteger(now) || now < 0) {
    throw new Error("Desktop authentication timestamp is invalid");
  }

  const existing = readTransportV2DesktopOAuth(undefined, now);
  const nextState: TransportV2DesktopOAuthState = {
    ...state,
    startedAt: existing && pendingClaim(existing) === pendingClaim(state) ? existing.startedAt : now
  };

  if (!existing || pendingClaim(existing) !== pendingClaim(nextState)) {
    sessionStorage.removeItem(TRANSPORT_V2_INITIATION_CLAIM_KEY);
    sessionStorage.removeItem(TRANSPORT_V2_MINT_CLAIM_KEY);
  }
  sessionStorage.setItem(TRANSPORT_V2_PENDING_KEY, JSON.stringify(nextState));
  sessionStorage.setItem(DESKTOP_OAUTH_TRANSPORT_KEY, "v2");
  sessionStorage.setItem(REDIRECT_TO_NATIVE_KEY, "true");
}

export function readTransportV2DesktopOAuth(
  expectedProvider?: DesktopOAuthProvider,
  now = Date.now()
): TransportV2DesktopOAuthState | null {
  const encoded = sessionStorage.getItem(TRANSPORT_V2_PENDING_KEY);
  if (!encoded) return null;

  try {
    const parsed = JSON.parse(encoded) as Partial<TransportV2DesktopOAuthState>;
    if (
      !isDesktopOAuthProvider(parsed.provider) ||
      !isNativeTargetAllowed(parsed) ||
      !isTransportV2PublicId(parsed.nativeSessionId) ||
      !isTransportV2PublicId(parsed.nativeRequestId) ||
      !hasValidTimestamp(parsed.startedAt, now)
    ) {
      throw new Error("Invalid pending desktop authentication state");
    }

    if (expectedProvider !== undefined && parsed.provider !== expectedProvider) return null;
    return parsed as TransportV2DesktopOAuthState;
  } catch {
    removeTransportV2PendingState();
    return null;
  }
}

export function claimTransportV2DesktopOAuthInitiation(
  expected: NativeOAuthInput,
  now = Date.now()
): boolean {
  const current = readTransportV2DesktopOAuth(expected.provider, now);
  if (!current || pendingClaim(current) !== pendingClaim(expected)) {
    throw new Error("Desktop authentication state changed before initiation");
  }

  const claim = pendingClaim(current);
  if (sessionStorage.getItem(TRANSPORT_V2_INITIATION_CLAIM_KEY) === claim) {
    return false;
  }
  sessionStorage.setItem(TRANSPORT_V2_INITIATION_CLAIM_KEY, claim);
  return true;
}

export function isNativeOAuthRedirect(): boolean {
  return (
    sessionStorage.getItem(REDIRECT_TO_NATIVE_KEY) === "true" &&
    sessionStorage.getItem(DESKTOP_OAUTH_TRANSPORT_KEY) === "v2"
  );
}

function assertHandoffGrant(handoffGrant: string): void {
  const grantSegments = handoffGrant.split(".");
  if (
    handoffGrant.length === 0 ||
    handoffGrant.length > MAX_HANDOFF_GRANT_LENGTH ||
    handoffGrant.trim() !== handoffGrant ||
    !COMPACT_GRANT_PATTERN.test(handoffGrant) ||
    grantSegments.some((segment) => segment.length % 4 === 1)
  ) {
    throw new Error("The desktop authentication grant is missing or invalid");
  }
}

export function buildTransportV2NativeAuthDeepLink(
  handoffGrant: string,
  nativeAppVariant?: NativeAppVariant
): string {
  assertHandoffGrant(handoffGrant);
  const deepLink = new URL(nativeAuthReturnUrl(nativeAppVariant));
  deepLink.searchParams.set("handoff_grant", handoffGrant);
  return deepLink.toString();
}

function sameDesktopOAuthTarget(
  left: TransportV2DesktopOAuthState,
  right: TransportV2DesktopOAuthState
): boolean {
  return pendingClaim(left) === pendingClaim(right) && left.startedAt === right.startedAt;
}

export function isCurrentDesktopOAuthTarget(
  expected: TransportV2DesktopOAuthState,
  now = Date.now()
): boolean {
  const current = readTransportV2DesktopOAuth(undefined, now);
  return isNativeOAuthRedirect() && current !== null && sameDesktopOAuthTarget(current, expected);
}

/** Clear only this flow, including expired state, without disturbing a newer login. */
export function clearDesktopOAuthTarget(expected: TransportV2DesktopOAuthState): void {
  const encoded = sessionStorage.getItem(TRANSPORT_V2_PENDING_KEY);
  if (!encoded) return;
  try {
    if (!sameDesktopOAuthTarget(JSON.parse(encoded), expected)) return;
  } catch {
    return;
  }
  removeTransportV2PendingState();
  sessionStorage.removeItem(DESKTOP_OAUTH_TRANSPORT_KEY);
  sessionStorage.removeItem(REDIRECT_TO_NATIVE_KEY);
}

export async function mintTransportV2NativeAuthReturn(
  handoffTarget: TransportV2DesktopOAuthState,
  mintGrant: NativeHandoffGrantIssuer,
  ownsConfirmation: () => boolean,
  now: () => number = Date.now
): Promise<NativeAuthReturn> {
  if (!isCurrentDesktopOAuthTarget(handoffTarget, now()) || !ownsConfirmation()) {
    throw new Error("Native sign-in changed or expired; please restart login in Maple.");
  }
  // Persist before sending: a remount or an ambiguous network failure must not mint again.
  const claim = `${pendingClaim(handoffTarget)}:${handoffTarget.startedAt}`;
  if (sessionStorage.getItem(TRANSPORT_V2_MINT_CLAIM_KEY) === claim) {
    throw new Error("Native sign-in has already been submitted; please restart login in Maple.");
  }
  sessionStorage.setItem(TRANSPORT_V2_MINT_CLAIM_KEY, claim);
  try {
    const { grant, expires_at } = await mintGrant(
      handoffTarget.nativeSessionId,
      handoffTarget.nativeRequestId
    );
    if (!isCurrentDesktopOAuthTarget(handoffTarget, now()) || !ownsConfirmation()) {
      throw new Error("Native sign-in changed or expired; please restart login in Maple.");
    }
    if (handoffTarget.nativeApp === "agent") {
      assertHandoffGrant(grant);
      // The SDK reports Unix seconds. Cap the manual return at both the grant
      // expiry and the pending attempt's deadline; never persist its URL.
      if (
        typeof expires_at !== "number" ||
        !Number.isSafeInteger(expires_at) ||
        !Number.isSafeInteger(expires_at * 1000)
      ) {
        throw new Error("Native authentication grant has no valid expiry");
      }
      const expiresAt = Math.min(
        expires_at * 1000,
        handoffTarget.startedAt + TRANSPORT_V2_PENDING_TTL_MS
      );
      if (expiresAt <= now()) throw new Error("Native authentication grant expired");
      const query = new URLSearchParams({
        handoff_grant: grant,
        return_state: handoffTarget.returnState
      });
      return {
        url: `http://127.0.0.1:${handoffTarget.returnPort}/auth/callback?${query}`,
        expiresAt
      };
    }
    return { url: buildTransportV2NativeAuthDeepLink(grant, handoffTarget.nativeAppVariant) };
  } finally {
    clearDesktopOAuthTarget(handoffTarget);
  }
}
