import { encodeCanonicalBase64 } from "./protocol";
import {
  TransportV2AuthorityChangedError,
  canonicalizeTransportV2ApiOrigin,
  getOrCreatePersistedTransportV2CacheRoot,
  installTransportV2Credentials,
  readTransportV2Credentials,
  snapshotTransportV2Auth,
  transportV2CredentialPrincipalHint,
  type TransportV2AuthSnapshot
} from "./auth";

const NATIVE_AUTH_FENCE_VERSION = 1 as const;

/**
 * A serializable compare-and-swap fence for one native OAuth handoff.
 *
 * The fence contains no credential. It is safe to persist while the system
 * browser completes OAuth and prevents an older callback from replacing a
 * newer browser authority.
 */
export interface NativeOAuthHandoffAuthFence {
  version: typeof NATIVE_AUTH_FENCE_VERSION;
  apiOrigin: string;
  userRevision: number;
  principalId: null;
}

export interface NativeOAuthHandoffPreparation {
  expectedAuth: NativeOAuthHandoffAuthFence;
  /** Canonical padded base64 for the installation-scoped 32-byte cache root. */
  cacheNamespaceRootBase64: string;
}

export interface NativeUserCredentialPair {
  accessToken: string;
  refreshToken: string;
}

export interface NativeUserAuthState {
  apiOrigin: string;
  revision: number;
  principalId: string | null;
  credentials: NativeUserCredentialPair | null;
  /** Canonical padded base64 for the installation-scoped 32-byte cache root. */
  cacheNamespaceRootBase64: string;
}

function exactKeys(value: object, expected: readonly string[]): boolean {
  const actual = Object.keys(value).sort();
  const sortedExpected = [...expected].sort();
  return (
    actual.length === sortedExpected.length &&
    actual.every((key, index) => key === sortedExpected[index])
  );
}

function canonicalCacheRoot(apiUrl: string): string {
  const root = getOrCreatePersistedTransportV2CacheRoot(apiUrl);
  try {
    return encodeCanonicalBase64(root);
  } finally {
    root.fill(0);
  }
}

function anonymousSnapshot(fence: NativeOAuthHandoffAuthFence): TransportV2AuthSnapshot {
  if (
    typeof fence !== "object" ||
    fence === null ||
    !exactKeys(fence, ["version", "apiOrigin", "userRevision", "principalId"]) ||
    fence.version !== NATIVE_AUTH_FENCE_VERSION ||
    fence.principalId !== null ||
    typeof fence.apiOrigin !== "string" ||
    canonicalizeTransportV2ApiOrigin(fence.apiOrigin) !== fence.apiOrigin ||
    typeof fence.userRevision !== "number" ||
    !Number.isSafeInteger(fence.userRevision) ||
    fence.userRevision < 0
  ) {
    throw new Error("Native OAuth handoff authentication fence is invalid.");
  }
  return {
    kind: "user",
    principalId: null,
    apiOrigin: fence.apiOrigin,
    revision: fence.userRevision
  };
}

function assertCredentialPair(
  credentials: NativeUserCredentialPair
): asserts credentials is NativeUserCredentialPair {
  if (
    typeof credentials !== "object" ||
    credentials === null ||
    !exactKeys(credentials, ["accessToken", "refreshToken"]) ||
    typeof credentials.accessToken !== "string" ||
    typeof credentials.refreshToken !== "string" ||
    credentials.accessToken.length === 0 ||
    credentials.refreshToken.length === 0
  ) {
    throw new Error("Native OAuth handoff returned an invalid credential pair.");
  }
}

/**
 * Captures the anonymous browser authority and stable cache root used to start
 * a native OAuth handoff. A signed-in browser must not start this transition.
 */
export function prepareNativeOAuthHandoff(apiUrl: string): NativeOAuthHandoffPreparation {
  const apiOrigin = canonicalizeTransportV2ApiOrigin(apiUrl);
  const beforeRoot = snapshotTransportV2Auth(apiOrigin, "user");
  if (beforeRoot.principalId !== null) {
    throw new Error("Native OAuth handoff requires an anonymous user authority.");
  }

  const cacheNamespaceRootBase64 = canonicalCacheRoot(apiOrigin);
  // Read again after root creation. Root persistence shares the same atomic
  // origin blob with credentials, so the returned fence must describe the
  // state that exists after that write.
  const expected = snapshotTransportV2Auth(apiOrigin, "user");
  if (expected.principalId !== null) {
    throw new TransportV2AuthorityChangedError();
  }
  return {
    expectedAuth: {
      version: NATIVE_AUTH_FENCE_VERSION,
      apiOrigin,
      userRevision: expected.revision,
      principalId: null
    },
    cacheNamespaceRootBase64
  };
}

/** Returns the current user credential pair and the same stable native root. */
export function readNativeUserAuth(apiUrl: string): NativeUserAuthState {
  const apiOrigin = canonicalizeTransportV2ApiOrigin(apiUrl);
  const cacheNamespaceRootBase64 = canonicalCacheRoot(apiOrigin);
  const before = snapshotTransportV2Auth(apiOrigin, "user");
  const credentials = readTransportV2Credentials(apiOrigin, "user");
  const after = snapshotTransportV2Auth(apiOrigin, "user");
  if (
    before.revision !== after.revision ||
    before.principalId !== after.principalId ||
    credentials?.revision !== (after.principalId === null ? undefined : after.revision) ||
    (credentials?.principalId ?? null) !== after.principalId
  ) {
    throw new TransportV2AuthorityChangedError();
  }
  return {
    apiOrigin,
    revision: after.revision,
    principalId: credentials?.principalId ?? null,
    credentials: credentials
      ? {
          accessToken: credentials.accessToken,
          refreshToken: credentials.refreshToken
        }
      : null,
    cacheNamespaceRootBase64
  };
}

/**
 * Installs the credential pair returned by a native OAuth redemption only if
 * the browser is still the exact anonymous authority that began the flow.
 */
export function installNativeOAuthHandoffCredentials(
  apiUrl: string,
  credentials: NativeUserCredentialPair,
  expectedAuth: NativeOAuthHandoffAuthFence,
  expectedPrincipalId: string
): NativeUserAuthState {
  assertCredentialPair(credentials);
  if (typeof expectedPrincipalId !== "string" || expectedPrincipalId.length === 0) {
    throw new Error("Native OAuth handoff returned an invalid account identity.");
  }
  if (
    transportV2CredentialPrincipalHint(
      "user",
      credentials.accessToken,
      credentials.refreshToken
    ) !== expectedPrincipalId
  ) {
    throw new Error("Native OAuth handoff credential identity does not match the response.");
  }
  const expected = anonymousSnapshot(expectedAuth);
  const apiOrigin = canonicalizeTransportV2ApiOrigin(apiUrl);
  if (apiOrigin !== expected.apiOrigin) {
    throw new TransportV2AuthorityChangedError();
  }
  installTransportV2Credentials(
    apiOrigin,
    "user",
    credentials.accessToken,
    credentials.refreshToken,
    expected
  );
  return readNativeUserAuth(apiOrigin);
}
