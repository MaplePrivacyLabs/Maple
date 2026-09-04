import { decodeURLSafe, encodeURLSafe } from "@stablelib/base64";
import { decodeCanonicalBase64, encodeCanonicalBase64 } from "./protocol";

const STORAGE_PREFIX = "opensecret:transport-v2:auth:v1:";
const MAX_TOKEN_BYTES = 16 * 1024;
const MAX_STORED_STATE_BYTES = 96 * 1024;
const CACHE_ROOT_BYTES = 32;

const USER_ACCESS_AUDIENCE = "urn:opensecret:internal:transport-v2:user:access-token";
const USER_REFRESH_AUDIENCE = "urn:opensecret:internal:transport-v2:user:refresh-token";
const PLATFORM_ACCESS_AUDIENCE = "urn:opensecret:internal:transport-v2:platform:access-token";
const PLATFORM_REFRESH_AUDIENCE = "urn:opensecret:internal:transport-v2:platform:refresh-token";

export type TransportV2AuthKind = "user" | "platform";

export interface StoredTransportV2Credentials {
  kind: TransportV2AuthKind;
  principalId: string;
  apiOrigin: string;
  revision: number;
  accessToken: string;
  refreshToken: string;
  accessExpiresAtUnixSeconds: number;
  refreshExpiresAtUnixSeconds: number;
}

/** A process-local compare-and-swap token for one origin-scoped credential slot. */
export interface TransportV2AuthSnapshot {
  kind: TransportV2AuthKind;
  principalId: string | null;
  apiOrigin: string;
  revision: number;
}

export type TransportV2ProfilePublicationDecision = "publish" | "reload" | "discard";

export class TransportV2AuthorityChangedError extends Error {
  constructor() {
    super("Transport v2 authentication state changed while the operation was in progress.");
    this.name = "TransportV2AuthorityChangedError";
  }
}

interface PersistedCredentials {
  access_token: string;
  refresh_token: string;
}

interface PersistedSlot {
  revision: number;
  credentials: PersistedCredentials | null;
}

interface PersistedState {
  version: 1;
  api_origin: string;
  cache_namespace_root: string | null;
  user: PersistedSlot;
  platform: PersistedSlot;
}

interface TokenHints {
  principalId: string;
  expiresAtUnixSeconds: number;
}

const memoryBlobs = new Map<string, string>();
const fallbackOnlyKeys = new Set<string>();
const pendingRemovalKeys = new Set<string>();
const invalidationListeners = new Set<{
  apiOrigin: string;
  kind: TransportV2AuthKind;
  listener: () => void;
}>();

type PersistentStorageResult =
  { kind: "available"; storage: Storage } | { kind: "absent" } | { kind: "access_error" };

function persistentStorageResult(): PersistentStorageResult {
  if (!("localStorage" in globalThis)) return { kind: "absent" };
  try {
    const storage = globalThis.localStorage;
    return storage ? { kind: "available", storage } : { kind: "absent" };
  } catch {
    return { kind: "access_error" };
  }
}

function persistentStorage(): Storage | undefined {
  const result = persistentStorageResult();
  return result.kind === "available" ? result.storage : undefined;
}

function flushPendingRemovals(storage: Storage): void {
  for (const key of pendingRemovalKeys) {
    try {
      storage.removeItem(key);
      pendingRemovalKeys.delete(key);
    } catch {
      // Retry when storage is usable again.
    }
  }
}

function readBlob(key: string): string | null {
  const storage = persistentStorage();
  if (!storage) return memoryBlobs.get(key) ?? null;
  flushPendingRemovals(storage);
  if (fallbackOnlyKeys.has(key)) {
    const fallback = memoryBlobs.get(key) ?? null;
    if (fallback !== null) {
      try {
        storage.setItem(key, fallback);
        fallbackOnlyKeys.delete(key);
      } catch {
        // Do not let an older persistent blob replace a newer process-local
        // commit while storage remains read-only or over quota.
      }
      return fallback;
    }
  }
  try {
    const persisted = storage.getItem(key);
    if (persisted !== null) {
      memoryBlobs.set(key, persisted);
      return persisted;
    }
    memoryBlobs.delete(key);
    return null;
  } catch {
    // The process-local copy remains authoritative while persistent storage is
    // unavailable (for example, in a sandboxed or quota-restricted context).
  }
  return memoryBlobs.get(key) ?? null;
}

function writeBlob(key: string, value: string): void {
  memoryBlobs.set(key, value);
  try {
    const storage = persistentStorage();
    if (!storage) {
      fallbackOnlyKeys.add(key);
      return;
    }
    flushPendingRemovals(storage);
    // All V2 credentials, both authority slots, their revisions, and the
    // cache root commit through this one atomic localStorage replacement.
    storage.setItem(key, value);
    fallbackOnlyKeys.delete(key);
  } catch {
    fallbackOnlyKeys.add(key);
  }
}

function removeStorageKey(key: string): void {
  memoryBlobs.delete(key);
  fallbackOnlyKeys.delete(key);
  try {
    const storage = persistentStorage();
    if (!storage) {
      pendingRemovalKeys.add(key);
      return;
    }
    storage.removeItem(key);
    pendingRemovalKeys.delete(key);
  } catch {
    pendingRemovalKeys.add(key);
    // A later V2 install still ignores these legacy names. Removal is best
    // effort when persistent browser storage itself is unavailable.
  }
}

function clearLegacyCredentials(): void {
  removeStorageKey("access_token");
  removeStorageKey("refresh_token");
}

function unpaddedBase64Url(bytes: Uint8Array): string {
  return encodeURLSafe(bytes).replace(/=+$/u, "");
}

function storageKey(apiOrigin: string): string {
  return `${STORAGE_PREFIX}${unpaddedBase64Url(new TextEncoder().encode(apiOrigin))}`;
}

/**
 * Returns the canonical HTTP origin used to isolate V2 browser authority.
 * A configured base path is intentionally not part of the credential scope.
 */
export function canonicalizeTransportV2ApiOrigin(apiUrl: string): string {
  let url: URL;
  try {
    url = new URL(apiUrl);
  } catch {
    throw new Error("Transport v2 requires a valid API URL.");
  }
  if (url.username || url.password || url.search || url.hash) {
    throw new Error("Transport v2 API URL must not contain credentials, a query, or a fragment.");
  }
  const loopbackHosts = new Set(["localhost", "127.0.0.1", "[::1]"]);
  if (
    url.protocol !== "https:" &&
    !(url.protocol === "http:" && loopbackHosts.has(url.hostname.toLowerCase()))
  ) {
    throw new Error("Transport v2 requires HTTPS outside exact loopback development.");
  }
  return url.origin;
}

function exactKeys(value: Record<string, unknown>, expected: readonly string[]): boolean {
  const actual = Object.keys(value).sort();
  const sortedExpected = [...expected].sort();
  return (
    actual.length === sortedExpected.length &&
    actual.every((key, index) => key === sortedExpected[index])
  );
}

function parseSlot(value: unknown): PersistedSlot {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("Transport v2 stored credential slot is invalid.");
  }
  const slot = value as Record<string, unknown>;
  if (
    !exactKeys(slot, ["revision", "credentials"]) ||
    typeof slot.revision !== "number" ||
    !Number.isSafeInteger(slot.revision) ||
    slot.revision < 0
  ) {
    throw new Error("Transport v2 stored credential slot is invalid.");
  }
  if (slot.credentials === null) {
    return { revision: slot.revision, credentials: null };
  }
  if (
    typeof slot.credentials !== "object" ||
    Array.isArray(slot.credentials) ||
    !exactKeys(slot.credentials as Record<string, unknown>, ["access_token", "refresh_token"])
  ) {
    throw new Error("Transport v2 stored credentials are invalid.");
  }
  const credentials = slot.credentials as Record<string, unknown>;
  if (
    typeof credentials.access_token !== "string" ||
    typeof credentials.refresh_token !== "string"
  ) {
    throw new Error("Transport v2 stored credentials are invalid.");
  }
  return {
    revision: slot.revision,
    credentials: {
      access_token: credentials.access_token,
      refresh_token: credentials.refresh_token
    }
  };
}

function emptyState(apiOrigin: string): PersistedState {
  return {
    version: 1,
    api_origin: apiOrigin,
    cache_namespace_root: null,
    user: { revision: 0, credentials: null },
    platform: { revision: 0, credentials: null }
  };
}

function parseState(raw: string, apiOrigin: string): PersistedState {
  if (new TextEncoder().encode(raw).byteLength > MAX_STORED_STATE_BYTES) {
    throw new Error("Transport v2 stored authentication state is too large.");
  }
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    throw new Error("Transport v2 stored authentication state is invalid JSON.");
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("Transport v2 stored authentication state is invalid.");
  }
  const state = value as Record<string, unknown>;
  if (
    !exactKeys(state, ["version", "api_origin", "cache_namespace_root", "user", "platform"]) ||
    state.version !== 1 ||
    state.api_origin !== apiOrigin ||
    (state.cache_namespace_root !== null && typeof state.cache_namespace_root !== "string")
  ) {
    throw new Error("Transport v2 stored authentication state is invalid.");
  }
  if (typeof state.cache_namespace_root === "string") {
    const root = decodeCanonicalBase64(state.cache_namespace_root, CACHE_ROOT_BYTES);
    root.fill(0);
  }
  return {
    version: 1,
    api_origin: apiOrigin,
    cache_namespace_root: state.cache_namespace_root,
    user: parseSlot(state.user),
    platform: parseSlot(state.platform)
  };
}

function readState(apiOrigin: string): PersistedState {
  const raw = readBlob(storageKey(apiOrigin));
  if (raw === null) return emptyState(apiOrigin);
  // A malformed persisted credential is distinguishable from no credential.
  // Callers must not silently turn corrupt authenticated state into an
  // anonymous request.
  return parseState(raw, apiOrigin);
}

function commitState(state: PersistedState, requirePersistentWrite = false): void {
  const encoded = JSON.stringify(state);
  if (new TextEncoder().encode(encoded).byteLength > MAX_STORED_STATE_BYTES) {
    throw new Error("Transport v2 authentication state exceeds its storage limit.");
  }
  const key = storageKey(state.api_origin);
  if (!requirePersistentWrite) {
    writeBlob(key, encoded);
    return;
  }

  const persistent = persistentStorageResult();
  if (persistent.kind === "access_error") {
    throw new Error("Transport v2 credential cleanup could not access persistent storage.");
  }
  if (persistent.kind === "absent") {
    // Some supported browser contexts have no persistent storage at all. In
    // those contexts the process-local blob is the only credential copy.
    writeBlob(key, encoded);
    return;
  }
  const storage = persistent.storage;
  flushPendingRemovals(storage);
  try {
    // A destructive clear must not report success while an older credential
    // remains durable and can reappear after restart.
    storage.setItem(key, encoded);
  } catch {
    throw new Error("Transport v2 credential cleanup could not be persisted.");
  }
  memoryBlobs.set(key, encoded);
  fallbackOnlyKeys.delete(key);
}

function nextRevision(revision: number): number {
  if (revision >= Number.MAX_SAFE_INTEGER) {
    throw new Error("Transport v2 authentication revision is exhausted.");
  }
  return revision + 1;
}

function decodeBase64UrlSegment(value: string): Uint8Array {
  if (!/^[A-Za-z0-9_-]+$/u.test(value) || value.length % 4 === 1) {
    throw new Error("Transport v2 credential is not a canonical JWT.");
  }
  const padded = `${value}${"=".repeat((4 - (value.length % 4)) % 4)}`;
  let decoded: Uint8Array;
  try {
    decoded = decodeURLSafe(padded);
  } catch {
    throw new Error("Transport v2 credential is not a canonical JWT.");
  }
  if (unpaddedBase64Url(decoded) !== value) {
    decoded.fill(0);
    throw new Error("Transport v2 credential is not a canonical JWT.");
  }
  return decoded;
}

function tokenHints(token: string, expectedAudience: string): TokenHints {
  const requiresTokenFormat =
    expectedAudience === USER_ACCESS_AUDIENCE || expectedAudience === USER_REFRESH_AUDIENCE;
  if (new TextEncoder().encode(token).byteLength > MAX_TOKEN_BYTES) {
    throw new Error("Transport v2 credential has an invalid length.");
  }
  const segments = token.split(".");
  if (segments.length !== 3 || segments.some((segment) => segment.length === 0)) {
    throw new Error("Transport v2 credential is not a canonical JWT.");
  }
  // Validate the outer compact shape even though only payload hints are read.
  for (const segment of [segments[0], segments[2]]) {
    const decoded = decodeBase64UrlSegment(segment);
    decoded.fill(0);
  }
  const payload = decodeBase64UrlSegment(segments[1]);
  try {
    let claims: unknown;
    try {
      claims = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(payload));
    } catch {
      throw new Error("Transport v2 credential claims are invalid.");
    }
    if (typeof claims !== "object" || claims === null || Array.isArray(claims)) {
      throw new Error("Transport v2 credential claims are invalid.");
    }
    const object = claims as Record<string, unknown>;
    if (
      object.aud !== expectedAudience ||
      typeof object.sub !== "string" ||
      object.sub.length === 0 ||
      object.sub.length > 256 ||
      typeof object.exp !== "number" ||
      !Number.isSafeInteger(object.exp) ||
      object.exp <= 0 ||
      (requiresTokenFormat ? object.tf !== 2 : object.tf !== undefined && object.tf !== 2)
    ) {
      throw new Error("Transport v2 credential claims are invalid.");
    }
    // These are unverified scheduling and identity hints. Signature, expiry,
    // authorization, and active account state remain backend authority when
    // the credential is presented inside an encrypted request.
    return { principalId: object.sub, expiresAtUnixSeconds: object.exp };
  } finally {
    payload.fill(0);
  }
}

function audiences(kind: TransportV2AuthKind): { access: string; refresh: string } {
  return kind === "user"
    ? { access: USER_ACCESS_AUDIENCE, refresh: USER_REFRESH_AUDIENCE }
    : { access: PLATFORM_ACCESS_AUDIENCE, refresh: PLATFORM_REFRESH_AUDIENCE };
}

function validatedCredentials(
  kind: TransportV2AuthKind,
  accessToken: string,
  refreshToken: string
): { access: TokenHints; refresh: TokenHints } {
  const expected = audiences(kind);
  const access = tokenHints(accessToken, expected.access);
  const refresh = tokenHints(refreshToken, expected.refresh);
  if (access.principalId !== refresh.principalId) {
    throw new Error("Transport v2 credential principals do not match.");
  }
  return { access, refresh };
}

function credentialsFromSlot(
  apiOrigin: string,
  kind: TransportV2AuthKind,
  slot: PersistedSlot
): StoredTransportV2Credentials | null {
  if (!slot.credentials) return null;
  const hints = validatedCredentials(
    kind,
    slot.credentials.access_token,
    slot.credentials.refresh_token
  );
  return {
    kind,
    principalId: hints.access.principalId,
    apiOrigin,
    revision: slot.revision,
    accessToken: slot.credentials.access_token,
    refreshToken: slot.credentials.refresh_token,
    accessExpiresAtUnixSeconds: hints.access.expiresAtUnixSeconds,
    refreshExpiresAtUnixSeconds: hints.refresh.expiresAtUnixSeconds
  };
}

function notifyInvalidated(apiOrigin: string, kind: TransportV2AuthKind): void {
  for (const subscription of invalidationListeners) {
    if (subscription.apiOrigin !== apiOrigin || subscription.kind !== kind) continue;
    try {
      subscription.listener();
    } catch {
      // A listener cannot roll back a credential commit or block cleanup.
    }
  }
}

function snapshotMatchesState(snapshot: TransportV2AuthSnapshot, state: PersistedState): boolean {
  if (snapshot.apiOrigin !== state.api_origin) return false;
  const slot = state[snapshot.kind];
  if (slot.revision !== snapshot.revision) return false;
  try {
    return (
      credentialsFromSlot(state.api_origin, snapshot.kind, slot)?.principalId ===
      (snapshot.principalId ?? undefined)
    );
  } catch {
    return false;
  }
}

export function readTransportV2Credentials(
  apiUrl: string,
  kind: TransportV2AuthKind
): StoredTransportV2Credentials | null {
  const apiOrigin = canonicalizeTransportV2ApiOrigin(apiUrl);
  const state = readState(apiOrigin);
  return credentialsFromSlot(apiOrigin, kind, state[kind]);
}

export function snapshotTransportV2Auth(
  apiUrl: string,
  kind: TransportV2AuthKind
): TransportV2AuthSnapshot {
  const apiOrigin = canonicalizeTransportV2ApiOrigin(apiUrl);
  const state = readState(apiOrigin);
  const credentials = credentialsFromSlot(apiOrigin, kind, state[kind]);
  return {
    kind,
    principalId: credentials?.principalId ?? null,
    apiOrigin,
    revision: credentials?.revision ?? state[kind].revision
  };
}

export function isTransportV2AuthSnapshotCurrent(snapshot: TransportV2AuthSnapshot): boolean {
  try {
    const apiOrigin = canonicalizeTransportV2ApiOrigin(snapshot.apiOrigin);
    if (apiOrigin !== snapshot.apiOrigin) return false;
    return snapshotMatchesState(snapshot, readState(apiOrigin));
  } catch {
    return false;
  }
}

/**
 * Classifies an async profile result against the authority that fetched it.
 * A same-principal forward revision is a refresh: reload under that current
 * credential instead of publishing the stale result or leaving loading stuck.
 */
export function transportV2ProfilePublicationDecision(
  sentWith: TransportV2AuthSnapshot,
  ownsPublication: boolean
): TransportV2ProfilePublicationDecision {
  if (!ownsPublication) return "discard";
  if (isTransportV2AuthSnapshotCurrent(sentWith)) return "publish";
  if (sentWith.principalId === null) return "discard";
  try {
    const current = snapshotTransportV2Auth(sentWith.apiOrigin, sentWith.kind);
    return current.principalId === sentWith.principalId && current.revision > sentWith.revision
      ? "reload"
      : "discard";
  } catch {
    return "discard";
  }
}

export function installTransportV2Credentials(
  apiUrl: string,
  kind: TransportV2AuthKind,
  accessToken: string,
  refreshToken: string,
  expected?: TransportV2AuthSnapshot
): StoredTransportV2Credentials {
  const apiOrigin = canonicalizeTransportV2ApiOrigin(apiUrl);
  const hints = validatedCredentials(kind, accessToken, refreshToken);
  const state = readState(apiOrigin);
  if (expected && !snapshotMatchesState(expected, state)) {
    throw new TransportV2AuthorityChangedError();
  }
  const previousPrincipal = credentialsFromSlot(apiOrigin, kind, state[kind])?.principalId ?? null;
  const revision = nextRevision(state[kind].revision);
  state[kind] = {
    revision,
    credentials: { access_token: accessToken, refresh_token: refreshToken }
  };
  commitState(state);
  clearLegacyCredentials();
  if (previousPrincipal !== hints.access.principalId) notifyInvalidated(apiOrigin, kind);
  return {
    kind,
    principalId: hints.access.principalId,
    apiOrigin,
    revision,
    accessToken,
    refreshToken,
    accessExpiresAtUnixSeconds: hints.access.expiresAtUnixSeconds,
    refreshExpiresAtUnixSeconds: hints.refresh.expiresAtUnixSeconds
  };
}

export function clearTransportV2CredentialsIfCurrent(expected: TransportV2AuthSnapshot): boolean {
  const apiOrigin = canonicalizeTransportV2ApiOrigin(expected.apiOrigin);
  let state: PersistedState;
  try {
    state = readState(apiOrigin);
  } catch {
    return false;
  }
  if (!snapshotMatchesState(expected, state)) return false;
  const hadCredentials = state[expected.kind].credentials !== null;
  state[expected.kind] = {
    revision: nextRevision(state[expected.kind].revision),
    credentials: null
  };
  commitState(state, true);
  clearLegacyCredentials();
  if (hadCredentials) notifyInvalidated(apiOrigin, expected.kind);
  return true;
}

export function clearTransportV2Credentials(apiUrl: string, kind?: TransportV2AuthKind): void {
  const apiOrigin = canonicalizeTransportV2ApiOrigin(apiUrl);
  let state: PersistedState;
  let corrupted = false;
  try {
    state = readState(apiOrigin);
  } catch {
    // Explicit local cleanup must remain able to recover from a corrupt blob.
    // Since its individual slots and root can no longer be trusted or safely
    // preserved, reset the whole origin rather than guessing at partial state.
    state = emptyState(apiOrigin);
    corrupted = true;
  }
  const kinds: readonly TransportV2AuthKind[] = kind ? [kind] : ["user", "platform"];
  const invalidated: TransportV2AuthKind[] = [];
  for (const selected of kinds) {
    if (state[selected].credentials) invalidated.push(selected);
    state[selected] = {
      revision: nextRevision(state[selected].revision),
      credentials: null
    };
  }
  commitState(state, true);
  clearLegacyCredentials();
  if (corrupted) {
    notifyInvalidated(apiOrigin, "user");
    notifyInvalidated(apiOrigin, "platform");
    return;
  }
  for (const selected of invalidated) notifyInvalidated(apiOrigin, selected);
}

export function subscribeTransportV2AuthInvalidation(
  apiUrl: string,
  kind: TransportV2AuthKind,
  listener: () => void
): () => void {
  const subscription = {
    apiOrigin: canonicalizeTransportV2ApiOrigin(apiUrl),
    kind,
    listener
  };
  invalidationListeners.add(subscription);
  return () => invalidationListeners.delete(subscription);
}

export function clearLegacyTransportV1Credentials(): void {
  clearLegacyCredentials();
}

export function getOrCreateTransportV2CacheRoot(
  apiUrl: string,
  random: Crypto = globalThis.crypto
): Uint8Array {
  const apiOrigin = canonicalizeTransportV2ApiOrigin(apiUrl);
  const state = readState(apiOrigin);
  if (state.cache_namespace_root !== null) {
    return decodeCanonicalBase64(state.cache_namespace_root, CACHE_ROOT_BYTES);
  }
  if (!random?.getRandomValues) {
    throw new Error("Secure randomness is unavailable for the transport v2 cache root.");
  }
  const root = random.getRandomValues(new Uint8Array(CACHE_ROOT_BYTES));
  if (!(root instanceof Uint8Array) || root.byteLength !== CACHE_ROOT_BYTES) {
    throw new Error("Transport v2 cache root generator returned the wrong length.");
  }
  state.cache_namespace_root = encodeCanonicalBase64(root);
  commitState(state);
  return root;
}

export function clearTransportV2CacheRoot(apiUrl: string): void {
  const apiOrigin = canonicalizeTransportV2ApiOrigin(apiUrl);
  const state = readState(apiOrigin);
  state.cache_namespace_root = null;
  commitState(state);
}
