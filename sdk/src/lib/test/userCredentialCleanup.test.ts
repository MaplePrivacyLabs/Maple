import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  captureUserCredentialSnapshot,
  clearUserCredentialsIfCurrent,
  type UserCredentialSnapshot
} from "../index";
import {
  TransportV2AuthorityChangedError,
  getOrCreateTransportV2CacheRoot,
  installTransportV2Credentials,
  readTransportV2Credentials,
  subscribeTransportV2AuthInvalidation,
  type TransportV2AuthKind
} from "../transportV2/auth";
import { TransportV2AuthRuntime } from "../transportV2/authRuntime";
import type { TransportV2Runtime, TransportV2RuntimeRequest } from "../transportV2/runtime";

const STORAGE_PREFIX = "opensecret:transport-v2:auth:v1:";
const AUDIENCE_PREFIX = "urn:opensecret:internal:transport-v2:";

class TestStorage implements Storage {
  readonly values = new Map<string, string>();
  readError = false;
  writeError = false;
  writes = 0;

  get length(): number {
    return this.values.size;
  }

  clear(): void {
    this.values.clear();
  }

  getItem(key: string): string | null {
    if (this.readError) throw new Error("test storage read denied");
    return this.values.get(key) ?? null;
  }

  key(index: number): string | null {
    return [...this.values.keys()][index] ?? null;
  }

  removeItem(key: string): void {
    this.values.delete(key);
  }

  setItem(key: string, value: string): void {
    this.writes += 1;
    if (this.writeError) throw new Error("test storage write denied");
    this.values.set(key, value);
  }
}

let storage: TestStorage;
let apiUrl: string;
let otherApiUrl: string;
let testId = 0;
let originalStorage: PropertyDescriptor | undefined;
let originalFetch: PropertyDescriptor | undefined;
let fetchCalls = 0;
let unsubscribe: Array<() => void>;

function restoreGlobal(name: string, descriptor: PropertyDescriptor | undefined): void {
  if (descriptor) Object.defineProperty(globalThis, name, descriptor);
  else Reflect.deleteProperty(globalThis, name);
}

function exposeStorage(): void {
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    writable: true,
    value: storage
  });
}

function token(
  kind: TransportV2AuthKind,
  purpose: "access" | "refresh",
  user: string,
  marker: number
): string {
  const claims = {
    aud: `${AUDIENCE_PREFIX}${kind}:${purpose}-token`,
    sub: user,
    exp: 2_100_000_000 + marker,
    ...(kind === "user" ? { tf: 2 } : {}),
    marker
  };
  return [
    Buffer.from(JSON.stringify({ alg: "ES256K", typ: "JWT" })).toString("base64url"),
    Buffer.from(JSON.stringify(claims)).toString("base64url"),
    Buffer.from(new Uint8Array(64).fill(marker)).toString("base64url")
  ].join(".");
}

function pair(user = "user-a", marker = 1, kind: TransportV2AuthKind = "user") {
  return {
    access: token(kind, "access", user, marker),
    refresh: token(kind, "refresh", user, marker)
  };
}

function install(user = "user-a", marker = 1, api = apiUrl, kind: TransportV2AuthKind = "user") {
  const credentials = pair(user, marker, kind);
  installTransportV2Credentials(api, kind, credentials.access, credentials.refresh);
  return credentials;
}

function key(api = apiUrl): string {
  return `${STORAGE_PREFIX}${Buffer.from(new URL(api).origin).toString("base64url")}`;
}

function captured(): UserCredentialSnapshot {
  const snapshot = captureUserCredentialSnapshot(apiUrl);
  expect(snapshot).not.toBeNull();
  if (!snapshot) throw new Error("test credentials were not captured");
  return snapshot;
}

function listen(api: string, kind: TransportV2AuthKind, listener: () => void): void {
  unsubscribe.push(subscribeTransportV2AuthInvalidation(api, kind, listener));
}

beforeEach(() => {
  originalStorage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  originalFetch = Object.getOwnPropertyDescriptor(globalThis, "fetch");
  storage = new TestStorage();
  testId += 1;
  apiUrl = `https://cleanup-${testId}.example.test/service`;
  otherApiUrl = `https://other-cleanup-${testId}.example.test/service`;
  unsubscribe = [];
  fetchCalls = 0;
  exposeStorage();
  Object.defineProperty(globalThis, "fetch", {
    configurable: true,
    writable: true,
    value: async () => {
      fetchCalls += 1;
      throw new Error("credential cleanup must not make a network request");
    }
  });
});

afterEach(() => {
  for (const stop of unsubscribe) stop();
  restoreGlobal("localStorage", originalStorage);
  restoreGlobal("fetch", originalFetch);
});

describe("public conditional user credential cleanup", () => {
  test("captures a frozen tokenless handle and clears only its current user slot", () => {
    const credentials = install();
    install("platform-user", 2, apiUrl, "platform");
    install("other-user", 3, otherApiUrl);
    const root = getOrCreateTransportV2CacheRoot(apiUrl);
    const before = JSON.parse(storage.getItem(key())!);
    const otherBefore = storage.getItem(key(otherApiUrl));
    for (const [name, value] of Object.entries({
      access_token: "legacy-access",
      refresh_token: "legacy-refresh",
      api_key: "unrelated-api-key"
    }))
      storage.setItem(name, value);
    let userNotifications = 0;
    let otherNotifications = 0;
    listen(apiUrl, "user", () => {
      userNotifications += 1;
    });
    listen(apiUrl, "platform", () => {
      otherNotifications += 1;
    });
    listen(otherApiUrl, "user", () => {
      otherNotifications += 1;
    });

    const snapshot = captured();
    expect(Object.isFrozen(snapshot)).toBe(true);
    expect(Object.keys(snapshot)).toEqual([]);
    expect(JSON.stringify(snapshot)).toBe("{}");
    expect(Object.values(snapshot)).not.toContain(credentials.access);
    expect(Object.values(snapshot)).not.toContain(credentials.refresh);
    const secondSnapshot = captured();
    expect(clearUserCredentialsIfCurrent(snapshot)).toBe(true);
    const after = JSON.parse(storage.getItem(key())!);
    expect(after.user).toEqual({ revision: before.user.revision + 1, credentials: null });
    expect(after.platform).toEqual(before.platform);
    expect(after.cache_namespace_root).toBe(before.cache_namespace_root);
    expect(getOrCreateTransportV2CacheRoot(apiUrl)).toEqual(root);
    expect(storage.getItem(key(otherApiUrl))).toBe(otherBefore);
    expect(storage.getItem("access_token")).toBe("legacy-access");
    expect(storage.getItem("refresh_token")).toBe("legacy-refresh");
    expect(storage.getItem("api_key")).toBe("unrelated-api-key");
    expect(readTransportV2Credentials(apiUrl, "user")).toBeNull();
    expect(clearUserCredentialsIfCurrent(snapshot)).toBe(false);
    expect(clearUserCredentialsIfCurrent(secondSnapshot)).toBe(false);
    expect(userNotifications).toBe(1);
    expect(otherNotifications).toBe(0);
    expect(fetchCalls).toBe(0);
  });

  test("returns null for no durable user credentials without creating storage", () => {
    expect(captureUserCredentialSnapshot(apiUrl)).toBeNull();
    expect(storage.length).toBe(0);
    install("platform-user", 2, apiUrl, "platform");
    const before = storage.getItem(key());
    const writes = storage.writes;
    expect(captureUserCredentialSnapshot(apiUrl)).toBeNull();
    expect(storage.getItem(key())).toBe(before);
    expect(storage.writes).toBe(writes);
  });

  for (const replacement of [
    { description: "account switch", user: "user-b", marker: 2 },
    { description: "same-user refresh", user: "user-a", marker: 2 },
    { description: "same-user re-login with identical tokens", user: "user-a", marker: 1 }
  ]) {
    test(`preserves credentials after ${replacement.description}`, () => {
      install();
      const snapshot = captured();
      install(replacement.user, replacement.marker);
      const before = storage.getItem(key());
      let notifications = 0;
      listen(apiUrl, "user", () => {
        notifications += 1;
      });
      expect(clearUserCredentialsIfCurrent(snapshot)).toBe(false);
      expect(storage.getItem(key())).toBe(before);
      expect(notifications).toBe(0);
      expect(clearUserCredentialsIfCurrent(captured())).toBe(true);
      expect(notifications).toBe(1);
    });
  }

  for (const changedToken of ["access_token", "refresh_token"] as const) {
    test(`observes a sequential external ${changedToken} replacement even at the same revision`, () => {
      install();
      const snapshot = captured();
      const original = storage.getItem(key())!;
      const external = JSON.parse(original);
      const replacement = pair("user-a", 7);
      external.user.credentials[changedToken] =
        changedToken === "access_token" ? replacement.access : replacement.refresh;
      storage.setItem(key(), JSON.stringify(external));
      const before = storage.getItem(key());
      expect(clearUserCredentialsIfCurrent(snapshot)).toBe(false);
      expect(storage.getItem(key())).toBe(before);
      // A stale handle is consumed even if a later writer restores its old bytes.
      storage.setItem(key(), original);
      expect(clearUserCredentialsIfCurrent(snapshot)).toBe(false);
      expect(storage.getItem(key())).toBe(original);
    });
  }

  test("does not clear recreated storage with a reused revision and different credentials", () => {
    install();
    const snapshot = captured();
    storage.removeItem(key());
    install("user-a", 8);
    expect(JSON.parse(storage.getItem(key())!).user.revision).toBe(1);
    const before = storage.getItem(key());
    expect(clearUserCredentialsIfCurrent(snapshot)).toBe(false);
    expect(storage.getItem(key())).toBe(before);
    expect(clearUserCredentialsIfCurrent(captured())).toBe(true);
  });

  test("returns false after durable removal without restoring its memory copy", () => {
    install();
    const snapshot = captured();
    storage.removeItem(key());
    const writes = storage.writes;
    expect(clearUserCredentialsIfCurrent(snapshot)).toBe(false);
    expect(captureUserCredentialSnapshot(apiUrl)).toBeNull();
    expect(storage.getItem(key())).toBeNull();
    expect(storage.writes).toBe(writes);
  });

  test("rejects forged, copied and serialized handles without consuming the real one", () => {
    install();
    const snapshot = captured();
    for (const forged of [{}, { ...snapshot }, JSON.parse(JSON.stringify(snapshot)), null]) {
      expect(() => clearUserCredentialsIfCurrent(forged as UserCredentialSnapshot)).toThrow();
    }
    expect(clearUserCredentialsIfCurrent(snapshot)).toBe(true);
  });

  for (const failure of ["missing", "getter", "read"] as const) {
    test(`fails closed on ${failure} storage without notifying or consuming the handle`, () => {
      install();
      const snapshot = captured();
      const before = storage.getItem(key());
      let notifications = 0;
      listen(apiUrl, "user", () => {
        notifications += 1;
      });
      if (failure === "missing") Reflect.deleteProperty(globalThis, "localStorage");
      if (failure === "getter") {
        Object.defineProperty(globalThis, "localStorage", {
          configurable: true,
          get() {
            throw new Error("test storage inaccessible");
          }
        });
      }
      if (failure === "read") storage.readError = true;
      expect(() => captureUserCredentialSnapshot(apiUrl)).toThrow();
      expect(() => clearUserCredentialsIfCurrent(snapshot)).toThrow();
      expect(notifications).toBe(0);
      storage.readError = false;
      exposeStorage();
      expect(storage.getItem(key())).toBe(before);
      expect(clearUserCredentialsIfCurrent(snapshot)).toBe(true);
      expect(notifications).toBe(1);
    });
  }

  test("keeps failed persistent writes retryable and never reports successful cleanup", () => {
    install();
    const snapshot = captured();
    const before = storage.getItem(key());
    let notifications = 0;
    listen(apiUrl, "user", () => {
      notifications += 1;
    });
    storage.writeError = true;
    expect(() => clearUserCredentialsIfCurrent(snapshot)).toThrow("could not be persisted");
    expect(storage.getItem(key())).toBe(before);
    expect(notifications).toBe(0);
    storage.writeError = false;
    expect(clearUserCredentialsIfCurrent(snapshot)).toBe(true);
    expect(notifications).toBe(1);
  });

  test("rejects malformed durable state rather than clearing its last good memory copy", () => {
    install();
    const snapshot = captured();
    const original = storage.getItem(key())!;
    storage.setItem(key(), "{invalid");
    expect(() => captureUserCredentialSnapshot(apiUrl)).toThrow();
    expect(() => clearUserCredentialsIfCurrent(snapshot)).toThrow();
    expect(storage.getItem(key())).toBe("{invalid");
    storage.setItem(key(), original);
    expect(clearUserCredentialsIfCurrent(snapshot)).toBe(true);
  });

  test("never captures or republishes a memory-only installation", () => {
    storage.writeError = true;
    install();
    expect(storage.getItem(key())).toBeNull();
    storage.writeError = false;
    const writes = storage.writes;
    expect(() => captureUserCredentialSnapshot(apiUrl)).toThrow("synchronized persistent storage");
    expect(storage.getItem(key())).toBeNull();
    expect(storage.writes).toBe(writes);
  });

  test("does not use a durable old snapshot while a newer installation exists only in memory", () => {
    install();
    const snapshot = captured();
    const before = storage.getItem(key());
    storage.writeError = true;
    install("user-b", 9);
    storage.writeError = false;
    const writes = storage.writes;
    let notifications = 0;
    listen(apiUrl, "user", () => {
      notifications += 1;
    });
    expect(() => captureUserCredentialSnapshot(apiUrl)).toThrow("synchronized persistent storage");
    expect(() => clearUserCredentialsIfCurrent(snapshot)).toThrow(
      "synchronized persistent storage"
    );
    expect(storage.getItem(key())).toBe(before);
    expect(storage.writes).toBe(writes);
    expect(notifications).toBe(0);
  });

  test("an already-pending refresh cannot reinstall credentials after successful cleanup", async () => {
    install();
    const snapshot = captured();
    const refreshed = pair("user-a", 12);
    let complete!: (response: Response) => void;
    const pendingResponse = new Promise<Response>((resolve) => {
      complete = resolve;
    });
    let refreshRequests = 0;
    const runtime = {
      async request(input: TransportV2RuntimeRequest) {
        input.beforeSend?.();
        expect(input.request.target).toBe("/refresh");
        refreshRequests += 1;
        return { response: await pendingResponse, rememberOAuthContinuation() {} };
      }
    } as unknown as TransportV2Runtime;
    const auth = new TransportV2AuthRuntime({ runtime, nowUnixSeconds: () => 1_900_000_000 });
    const pendingRefresh = auth.refresh(apiUrl, { remoteAttestation: false }, "user");
    const outcome = pendingRefresh.then(
      () => ({ error: undefined }),
      (error: unknown) => ({ error })
    );
    expect(refreshRequests).toBe(1);
    expect(clearUserCredentialsIfCurrent(snapshot)).toBe(true);
    complete(Response.json({ access_token: refreshed.access, refresh_token: refreshed.refresh }));
    expect((await outcome).error).toBeInstanceOf(TransportV2AuthorityChangedError);
    expect(readTransportV2Credentials(apiUrl, "user")).toBeNull();
    expect(JSON.parse(storage.getItem(key())!).user.credentials).toBeNull();
    expect(fetchCalls).toBe(0);
  });
});
