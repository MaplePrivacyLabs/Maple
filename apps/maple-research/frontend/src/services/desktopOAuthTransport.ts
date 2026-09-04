export type DesktopOAuthTransport = "v1" | "v2";
export type DesktopOAuthProvider = "github" | "google" | "apple";

const DESKTOP_OAUTH_TRANSPORT_KEY = "maple_desktop_oauth_transport_v1";
const REDIRECT_TO_NATIVE_KEY = "redirect-to-native";
const TRANSPORT_V2_PENDING_KEY = "maple_desktop_oauth_pending_v2";
const TRANSPORT_V2_INITIATION_CLAIM_KEY = "maple_desktop_oauth_initiation_claim_v2";

export const TRANSPORT_V2_PENDING_TTL_MS = 15 * 60 * 1000;
export const TRANSPORT_V2_NATIVE_SESSION_QUERY = "native_session_id";
export const TRANSPORT_V2_NATIVE_REQUEST_QUERY = "native_request_id";

const TRANSPORT_V2_ID_PATTERN = /^[0-9a-f]{32}$/u;
const COMPACT_GRANT_PATTERN = /^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$/u;
const MAX_HANDOFF_GRANT_LENGTH = 4096;

export interface TransportV2DesktopOAuthState {
  provider: DesktopOAuthProvider;
  nativeSessionId: string;
  nativeRequestId: string;
  startedAt: number;
}

interface TransportV2DesktopAuthUrlOptions {
  provider: DesktopOAuthProvider;
  nativeSessionId: string;
  nativeRequestId: string;
}

type NativeHandoffGrantIssuer = (
  nativeSessionId: string,
  nativeRequestId: string
) => Promise<{ grant: string }>;

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

function pendingClaim(state: TransportV2DesktopOAuthState): string {
  return `${state.provider}:${state.nativeSessionId}:${state.nativeRequestId}`;
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
}

export function buildTransportV2DesktopAuthUrl({
  provider,
  nativeSessionId,
  nativeRequestId
}: TransportV2DesktopAuthUrlOptions): string {
  if (!isDesktopOAuthProvider(provider)) {
    throw new Error("Desktop authentication provider is missing or invalid");
  }
  assertTransportV2PublicId(nativeSessionId, "native session");
  assertTransportV2PublicId(nativeRequestId, "native request");

  const url = new URL("https://trymaple.ai/desktop-auth");
  url.searchParams.set("provider", provider);
  url.searchParams.set("transport", "v2");
  url.searchParams.set(TRANSPORT_V2_NATIVE_SESSION_QUERY, nativeSessionId);
  url.searchParams.set(TRANSPORT_V2_NATIVE_REQUEST_QUERY, nativeRequestId);
  return url.toString();
}

export function markDesktopOAuthTransport(transport: DesktopOAuthTransport): void {
  const storage = transport === "v2" ? sessionStorage : localStorage;
  storage.setItem(DESKTOP_OAUTH_TRANSPORT_KEY, transport);
  storage.setItem(REDIRECT_TO_NATIVE_KEY, "true");
}

export function markTransportV2DesktopOAuth(
  state: Omit<TransportV2DesktopOAuthState, "startedAt">,
  now = Date.now()
): void {
  if (!isDesktopOAuthProvider(state.provider)) {
    throw new Error("Desktop authentication provider is missing or invalid");
  }
  assertTransportV2PublicId(state.nativeSessionId, "native session");
  assertTransportV2PublicId(state.nativeRequestId, "native request");
  if (!Number.isSafeInteger(now) || now < 0) {
    throw new Error("Desktop authentication timestamp is invalid");
  }

  const existing = readTransportV2DesktopOAuth(undefined, now);
  const nextState: TransportV2DesktopOAuthState = {
    ...state,
    startedAt:
      existing && pendingClaim(existing) === pendingClaim({ ...state, startedAt: now })
        ? existing.startedAt
        : now
  };

  if (!existing || pendingClaim(existing) !== pendingClaim(nextState)) {
    sessionStorage.removeItem(TRANSPORT_V2_INITIATION_CLAIM_KEY);
  }
  sessionStorage.setItem(TRANSPORT_V2_PENDING_KEY, JSON.stringify(nextState));
  markDesktopOAuthTransport("v2");
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
      (expectedProvider !== undefined && parsed.provider !== expectedProvider) ||
      !isTransportV2PublicId(parsed.nativeSessionId) ||
      !isTransportV2PublicId(parsed.nativeRequestId) ||
      !hasValidTimestamp(parsed.startedAt, now)
    ) {
      throw new Error("Invalid pending desktop authentication state");
    }

    return parsed as TransportV2DesktopOAuthState;
  } catch {
    removeTransportV2PendingState();
    return null;
  }
}

export function claimTransportV2DesktopOAuthInitiation(
  expected: Omit<TransportV2DesktopOAuthState, "startedAt">,
  now = Date.now()
): boolean {
  const current = readTransportV2DesktopOAuth(expected.provider, now);
  if (
    !current ||
    current.nativeSessionId !== expected.nativeSessionId ||
    current.nativeRequestId !== expected.nativeRequestId
  ) {
    throw new Error("Desktop authentication state changed before initiation");
  }

  const claim = pendingClaim(current);
  if (sessionStorage.getItem(TRANSPORT_V2_INITIATION_CLAIM_KEY) === claim) {
    return false;
  }
  sessionStorage.setItem(TRANSPORT_V2_INITIATION_CLAIM_KEY, claim);
  return true;
}

export function readDesktopOAuthTransport(): DesktopOAuthTransport | null {
  if (sessionStorage.getItem(DESKTOP_OAUTH_TRANSPORT_KEY) === "v2") return "v2";
  return localStorage.getItem(DESKTOP_OAUTH_TRANSPORT_KEY) === "v1" ? "v1" : null;
}

export function clearDesktopOAuthTransport(): void {
  for (const storage of [sessionStorage, localStorage]) {
    storage.removeItem(DESKTOP_OAUTH_TRANSPORT_KEY);
    storage.removeItem(REDIRECT_TO_NATIVE_KEY);
  }
  removeTransportV2PendingState();
}

export function isNativeOAuthRedirect(): boolean {
  return (
    sessionStorage.getItem(REDIRECT_TO_NATIVE_KEY) === "true" &&
    sessionStorage.getItem(DESKTOP_OAUTH_TRANSPORT_KEY) === "v2"
  );
}

export function buildTransportV2NativeAuthDeepLink(handoffGrant: string): string {
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

  const deepLink = new URL("cloud.opensecret.maple://auth");
  deepLink.searchParams.set("handoff_grant", handoffGrant);
  return deepLink.toString();
}

export async function mintTransportV2NativeAuthDeepLink(
  provider: DesktopOAuthProvider,
  mintGrant: NativeHandoffGrantIssuer,
  now = Date.now()
): Promise<string> {
  const handoffTarget = readTransportV2DesktopOAuth(provider, now);
  if (!handoffTarget) {
    throw new Error("Desktop authentication state is missing or expired; please restart login");
  }

  const { grant } = await mintGrant(handoffTarget.nativeSessionId, handoffTarget.nativeRequestId);
  const deepLink = buildTransportV2NativeAuthDeepLink(grant);
  clearDesktopOAuthTransport();
  return deepLink;
}

export function shouldLoadLegacyDesktopOAuth(
  location: Pick<Location, "pathname" | "search">
): boolean {
  if (location.pathname === "/desktop-auth") {
    // Released clients did not send a transport selector. Any supplied
    // selector, including a malformed one, stays on the fail-closed current
    // bundle instead of silently downgrading.
    return new URLSearchParams(location.search).getAll("transport").length === 0;
  }

  if (!/^\/auth\/(github|google|apple)\/callback$/u.test(location.pathname)) {
    return false;
  }
  if (
    sessionStorage.getItem(REDIRECT_TO_NATIVE_KEY) === "true" &&
    sessionStorage.getItem(DESKTOP_OAUTH_TRANSPORT_KEY) === "v2"
  ) {
    return false;
  }
  if (localStorage.getItem(REDIRECT_TO_NATIVE_KEY) !== "true") return false;

  const marker = localStorage.getItem(DESKTOP_OAUTH_TRANSPORT_KEY);
  // A missing marker is an already-running flow started by the previously
  // deployed site. Unknown markers fail closed on the current bundle.
  return marker === null || marker === "v1";
}
