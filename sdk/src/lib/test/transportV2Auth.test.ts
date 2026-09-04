import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  TransportV2AuthorityChangedError,
  canonicalizeTransportV2ApiOrigin,
  clearTransportV2CacheRoot,
  clearTransportV2Credentials,
  clearTransportV2CredentialsIfCurrent,
  getOrCreateTransportV2CacheRoot,
  installTransportV2Credentials,
  isTransportV2AuthSnapshotCurrent,
  readTransportV2Credentials,
  snapshotTransportV2Auth,
  subscribeTransportV2AuthInvalidation,
  transportV2ProfilePublicationDecision,
  type TransportV2AuthKind
} from "../transportV2/auth";

const API_URL = "https://api.example.test/service";
const OTHER_API_URL = "https://other.example.test/service";
const FALLBACK_API_URL = "https://fallback.example.test/service";
const STORAGE_PREFIX = "opensecret:transport-v2:auth:v1:";

const AUDIENCES = {
  user: {
    access: "urn:opensecret:internal:transport-v2:user:access-token",
    refresh: "urn:opensecret:internal:transport-v2:user:refresh-token"
  },
  platform: {
    access: "urn:opensecret:internal:transport-v2:platform:access-token",
    refresh: "urn:opensecret:internal:transport-v2:platform:refresh-token"
  }
} as const;

function base64Url(value: string | Uint8Array): string {
  return Buffer.from(value).toString("base64url");
}

function token(
  kind: TransportV2AuthKind,
  purpose: "access" | "refresh",
  principalId: string,
  expiresAt = 2_000_000_000,
  tokenFormat: number | null = kind === "user" ? 2 : null
): string {
  const claims: Record<string, unknown> = {
    aud: AUDIENCES[kind][purpose],
    sub: principalId,
    exp: expiresAt
  };
  if (tokenFormat !== null) claims.tf = tokenFormat;
  return [
    base64Url(JSON.stringify({ alg: "ES256K", typ: "JWT" })),
    base64Url(JSON.stringify(claims)),
    base64Url(new Uint8Array(64).fill(0x5a))
  ].join(".");
}

function credentials(kind: TransportV2AuthKind, principalId: string, version = 0) {
  return {
    access: token(kind, "access", principalId, 2_000_000_000 + version),
    refresh: token(kind, "refresh", principalId, 2_100_000_000 + version)
  };
}

function storageKeys(): string[] {
  return Array.from({ length: globalThis.localStorage.length }, (_, index) =>
    globalThis.localStorage.key(index)
  ).filter((key): key is string => key !== null);
}

function fixedRandom(expected: Uint8Array): Crypto {
  return {
    getRandomValues<T extends ArrayBufferView | null>(array: T): T {
      if (!(array instanceof Uint8Array) || array.byteLength !== expected.byteLength) {
        throw new Error("unexpected random request");
      }
      array.set(expected);
      return array;
    }
  } as Crypto;
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  return {
    promise: new Promise<T>((fulfill) => {
      resolve = fulfill;
    }),
    resolve
  };
}

beforeEach(() => {
  globalThis.localStorage.clear();
});

afterEach(() => {
  globalThis.localStorage.clear();
});

describe("Transport V2 authentication storage", () => {
  test("canonicalizes and scopes state to the API origin", () => {
    expect(canonicalizeTransportV2ApiOrigin("HTTPS://API.EXAMPLE.TEST:443/service///")).toBe(
      "https://api.example.test"
    );
    expect(canonicalizeTransportV2ApiOrigin("http://LOCALHOST:80/service")).toBe(
      "http://localhost"
    );
    expect(() => canonicalizeTransportV2ApiOrigin("http://api.example.test/service")).toThrow(
      "requires HTTPS"
    );
    expect(() => canonicalizeTransportV2ApiOrigin("https://user@api.example.test/service")).toThrow(
      "must not contain credentials"
    );
    expect(() => canonicalizeTransportV2ApiOrigin("https://api.example.test/?query=1")).toThrow(
      "must not contain credentials"
    );
  });

  test("atomically stores separate user and platform slots in one origin blob", () => {
    globalThis.localStorage.setItem("access_token", "legacy-access");
    globalThis.localStorage.setItem("refresh_token", "legacy-refresh");
    const user = credentials("user", "user-123");
    const platform = credentials("platform", "platform-456");

    installTransportV2Credentials(API_URL, "user", user.access, user.refresh);
    installTransportV2Credentials(
      "https://api.example.test/another-base",
      "platform",
      platform.access,
      platform.refresh
    );

    const v2Keys = storageKeys().filter((key) => key.startsWith(STORAGE_PREFIX));
    expect(v2Keys).toHaveLength(1);
    const state = JSON.parse(globalThis.localStorage.getItem(v2Keys[0])!) as Record<
      string,
      unknown
    >;
    expect(Object.keys(state).sort()).toEqual(
      ["api_origin", "cache_namespace_root", "platform", "user", "version"].sort()
    );
    expect(readTransportV2Credentials(API_URL, "user")).toMatchObject({
      principalId: "user-123",
      apiOrigin: "https://api.example.test",
      revision: 1
    });
    expect(readTransportV2Credentials(API_URL, "platform")).toMatchObject({
      principalId: "platform-456",
      apiOrigin: "https://api.example.test",
      revision: 1
    });
    expect(globalThis.localStorage.getItem("access_token")).toBeNull();
    expect(globalThis.localStorage.getItem("refresh_token")).toBeNull();
  });

  test("treats decoded JWT claims only as strict V2 hints", () => {
    const user = credentials("user", "user-123");
    const installed = installTransportV2Credentials(API_URL, "user", user.access, user.refresh);
    expect(installed).toMatchObject({
      principalId: "user-123",
      accessExpiresAtUnixSeconds: 2_000_000_000,
      refreshExpiresAtUnixSeconds: 2_100_000_000
    });

    expect(() =>
      installTransportV2Credentials(
        OTHER_API_URL,
        "user",
        token("platform", "access", "user-123"),
        user.refresh
      )
    ).toThrow("claims are invalid");
    expect(() =>
      installTransportV2Credentials(
        OTHER_API_URL,
        "user",
        user.access,
        token("user", "refresh", "user-456")
      )
    ).toThrow("principals do not match");
    expect(() =>
      installTransportV2Credentials(
        OTHER_API_URL,
        "user",
        token("user", "access", "user-123", 2_000_000_000, 3),
        user.refresh
      )
    ).toThrow("claims are invalid");
    expect(() =>
      installTransportV2Credentials(
        OTHER_API_URL,
        "user",
        token("user", "access", "user-123", 2_000_000_000, null),
        user.refresh
      )
    ).toThrow("claims are invalid");
    expect(readTransportV2Credentials(OTHER_API_URL, "user")).toBeNull();
  });

  test("fails closed when persisted authenticated state is malformed", () => {
    const user = credentials("user", "user-123");
    installTransportV2Credentials(API_URL, "user", user.access, user.refresh);
    const key = storageKeys().find((candidate) => candidate.startsWith(STORAGE_PREFIX));
    expect(key).toBeDefined();
    globalThis.localStorage.setItem(key!, "{malformed");

    expect(() => readTransportV2Credentials(API_URL, "user")).toThrow(
      "stored authentication state is invalid JSON"
    );
    expect(() => snapshotTransportV2Auth(API_URL, "user")).toThrow(
      "stored authentication state is invalid JSON"
    );
    expect(
      isTransportV2AuthSnapshotCurrent({
        kind: "user",
        principalId: "user-123",
        apiOrigin: "https://api.example.test",
        revision: 1
      })
    ).toBe(false);

    clearTransportV2Credentials(API_URL, "user");
    expect(readTransportV2Credentials(API_URL, "user")).toBeNull();
  });

  test("uses revisions to reject stale refresh and logout results", () => {
    const first = credentials("user", "user-123");
    installTransportV2Credentials(API_URL, "user", first.access, first.refresh);
    const stale = snapshotTransportV2Auth(API_URL, "user");

    const replacement = credentials("user", "user-456");
    const current = installTransportV2Credentials(
      API_URL,
      "user",
      replacement.access,
      replacement.refresh
    );
    expect(isTransportV2AuthSnapshotCurrent(stale)).toBe(false);
    expect(clearTransportV2CredentialsIfCurrent(stale)).toBe(false);
    expect(() =>
      installTransportV2Credentials(
        API_URL,
        "user",
        credentials("user", "user-123", 1).access,
        credentials("user", "user-123", 1).refresh,
        stale
      )
    ).toThrow(TransportV2AuthorityChangedError);
    expect(readTransportV2Credentials(API_URL, "user")).toEqual(current);

    const refreshSnapshot = snapshotTransportV2Auth(API_URL, "user");
    const refreshed = credentials("user", "user-456", 2);
    const refreshedState = installTransportV2Credentials(
      API_URL,
      "user",
      refreshed.access,
      refreshed.refresh,
      refreshSnapshot
    );
    expect(refreshedState.revision).toBe(current.revision + 1);
    expect(isTransportV2AuthSnapshotCurrent(refreshSnapshot)).toBe(false);
  });

  test("notifies React only when the slot's effective principal changes", () => {
    let userInvalidations = 0;
    let platformInvalidations = 0;
    const unsubscribeUser = subscribeTransportV2AuthInvalidation(API_URL, "user", () => {
      userInvalidations += 1;
    });
    const unsubscribePlatform = subscribeTransportV2AuthInvalidation(API_URL, "platform", () => {
      platformInvalidations += 1;
    });
    try {
      const first = credentials("user", "user-123");
      installTransportV2Credentials(API_URL, "user", first.access, first.refresh);
      const refreshSnapshot = snapshotTransportV2Auth(API_URL, "user");
      const refreshed = credentials("user", "user-123", 1);
      installTransportV2Credentials(
        API_URL,
        "user",
        refreshed.access,
        refreshed.refresh,
        refreshSnapshot
      );
      const replacement = credentials("user", "user-456");
      installTransportV2Credentials(API_URL, "user", replacement.access, replacement.refresh);
      const current = snapshotTransportV2Auth(API_URL, "user");
      expect(clearTransportV2CredentialsIfCurrent(refreshSnapshot)).toBe(false);
      expect(clearTransportV2CredentialsIfCurrent(current)).toBe(true);

      expect(userInvalidations).toBe(3); // sign-in, principal replacement, sign-out
      expect(platformInvalidations).toBe(0);
    } finally {
      unsubscribeUser();
      unsubscribePlatform();
    }
  });

  test("reloads a deferred profile after a same-principal credential refresh", async () => {
    for (const [kind, apiUrl] of [
      ["user", API_URL],
      ["platform", OTHER_API_URL]
    ] as const) {
      const initial = credentials(kind, `${kind}-123`);
      installTransportV2Credentials(apiUrl, kind, initial.access, initial.refresh);
      const sentWith = snapshotTransportV2Auth(apiUrl, kind);
      const profile = deferred<string>();
      const completion = profile.promise.then((value) => ({
        value,
        decision: transportV2ProfilePublicationDecision(sentWith, true)
      }));

      const refreshed = credentials(kind, `${kind}-123`, 1);
      installTransportV2Credentials(apiUrl, kind, refreshed.access, refreshed.refresh, sentWith);
      profile.resolve("profile fetched with the old revision");

      await expect(completion).resolves.toEqual({
        value: "profile fetched with the old revision",
        decision: "reload"
      });
      expect(
        transportV2ProfilePublicationDecision(snapshotTransportV2Auth(apiUrl, kind), true)
      ).toBe("publish");
    }
  });

  test("discards profile results after logout, account switch, or scope replacement", () => {
    const logoutCredentials = credentials("user", "user-logout");
    installTransportV2Credentials(
      API_URL,
      "user",
      logoutCredentials.access,
      logoutCredentials.refresh
    );
    const beforeLogout = snapshotTransportV2Auth(API_URL, "user");
    expect(clearTransportV2CredentialsIfCurrent(beforeLogout)).toBe(true);
    expect(transportV2ProfilePublicationDecision(beforeLogout, true)).toBe("discard");

    const firstAccount = credentials("user", "user-first");
    installTransportV2Credentials(API_URL, "user", firstAccount.access, firstAccount.refresh);
    const beforeSwitch = snapshotTransportV2Auth(API_URL, "user");
    const secondAccount = credentials("user", "user-second");
    installTransportV2Credentials(API_URL, "user", secondAccount.access, secondAccount.refresh);
    expect(transportV2ProfilePublicationDecision(beforeSwitch, true)).toBe("discard");

    const platform = credentials("platform", "platform-123");
    installTransportV2Credentials(FALLBACK_API_URL, "platform", platform.access, platform.refresh);
    const replacedScope = snapshotTransportV2Auth(FALLBACK_API_URL, "platform");
    expect(transportV2ProfilePublicationDecision(replacedScope, false)).toBe("discard");
  });

  test("keeps one stable random cache root per canonical origin", () => {
    const expected = Uint8Array.from({ length: 32 }, (_, index) => index + 1);
    let calls = 0;
    const random = fixedRandom(expected);
    const countingRandom = {
      getRandomValues<T extends ArrayBufferView | null>(array: T): T {
        calls += 1;
        return random.getRandomValues(array);
      }
    } as Crypto;

    const first = getOrCreateTransportV2CacheRoot(API_URL, countingRandom);
    expect(first).toEqual(expected);
    first.fill(0);
    const second = getOrCreateTransportV2CacheRoot(
      "https://api.example.test/other-base",
      countingRandom
    );
    expect(second).toEqual(expected);
    expect(calls).toBe(1);

    clearTransportV2Credentials(API_URL);
    expect(getOrCreateTransportV2CacheRoot(API_URL, countingRandom)).toEqual(expected);
    expect(calls).toBe(1);
  });

  test("clearing one authority slot preserves the other slot and cache root", () => {
    const user = credentials("user", "user-123");
    const platform = credentials("platform", "platform-456");
    installTransportV2Credentials(API_URL, "user", user.access, user.refresh);
    installTransportV2Credentials(API_URL, "platform", platform.access, platform.refresh);
    const root = getOrCreateTransportV2CacheRoot(API_URL, fixedRandom(new Uint8Array(32).fill(7)));

    globalThis.localStorage.setItem("access_token", "legacy-access");
    globalThis.localStorage.setItem("refresh_token", "legacy-refresh");
    clearTransportV2Credentials(API_URL, "user");

    expect(readTransportV2Credentials(API_URL, "user")).toBeNull();
    expect(readTransportV2Credentials(API_URL, "platform")?.principalId).toBe("platform-456");
    expect(getOrCreateTransportV2CacheRoot(API_URL)).toEqual(root);
    expect(globalThis.localStorage.getItem("access_token")).toBeNull();
    expect(globalThis.localStorage.getItem("refresh_token")).toBeNull();
  });

  test("credential cleanup never reports success while an older durable token remains", () => {
    const originalStorage = globalThis.localStorage;
    const user = credentials("user", "user-123");
    installTransportV2Credentials(API_URL, "user", user.access, user.refresh);
    const snapshot = snapshotTransportV2Auth(API_URL, "user");
    const readOnlyStorage = {
      get length() {
        return originalStorage.length;
      },
      clear: () => originalStorage.clear(),
      getItem: (key: string) => originalStorage.getItem(key),
      key: (index: number) => originalStorage.key(index),
      removeItem: (key: string) => originalStorage.removeItem(key),
      setItem(): never {
        throw new Error("storage is read-only");
      }
    } as Storage;
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      value: readOnlyStorage
    });
    try {
      expect(() => clearTransportV2CredentialsIfCurrent(snapshot)).toThrow(
        "cleanup could not be persisted"
      );
      expect(readTransportV2Credentials(API_URL, "user")?.principalId).toBe("user-123");
    } finally {
      Object.defineProperty(globalThis, "localStorage", {
        configurable: true,
        value: originalStorage
      });
    }

    expect(readTransportV2Credentials(API_URL, "user")?.principalId).toBe("user-123");
    clearTransportV2Credentials(API_URL);
  });

  test("credential cleanup fails closed when the localStorage getter is inaccessible", () => {
    const originalDescriptor = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
    const user = credentials("user", "user-123");
    installTransportV2Credentials(API_URL, "user", user.access, user.refresh);
    const snapshot = snapshotTransportV2Auth(API_URL, "user");
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      get(): never {
        throw new Error("storage access denied");
      }
    });
    try {
      expect(() => clearTransportV2CredentialsIfCurrent(snapshot)).toThrow(
        "could not access persistent storage"
      );
    } finally {
      if (originalDescriptor) {
        Object.defineProperty(globalThis, "localStorage", originalDescriptor);
      } else {
        delete (globalThis as { localStorage?: Storage }).localStorage;
      }
    }

    expect(readTransportV2Credentials(API_URL, "user")?.principalId).toBe("user-123");
    clearTransportV2Credentials(API_URL);
  });

  test("falls back to process memory when localStorage is unavailable", () => {
    const originalStorage = globalThis.localStorage;
    const first = credentials("user", "first-user");
    installTransportV2Credentials(FALLBACK_API_URL, "user", first.access, first.refresh);
    globalThis.localStorage.setItem("access_token", "legacy-access");
    globalThis.localStorage.setItem("refresh_token", "legacy-refresh");
    const unavailableStorage = {
      getItem(): never {
        throw new Error("storage unavailable");
      },
      setItem(): never {
        throw new Error("storage unavailable");
      },
      removeItem(): never {
        throw new Error("storage unavailable");
      }
    } as unknown as Storage;
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      value: unavailableStorage
    });
    try {
      const user = credentials("user", "fallback-user");
      installTransportV2Credentials(
        FALLBACK_API_URL,
        "user",
        user.access,
        user.refresh,
        snapshotTransportV2Auth(FALLBACK_API_URL, "user")
      );
      expect(readTransportV2Credentials(FALLBACK_API_URL, "user")?.principalId).toBe(
        "fallback-user"
      );
      expect(
        getOrCreateTransportV2CacheRoot(FALLBACK_API_URL, fixedRandom(new Uint8Array(32).fill(9)))
      ).toEqual(new Uint8Array(32).fill(9));
    } finally {
      Object.defineProperty(globalThis, "localStorage", {
        configurable: true,
        value: originalStorage
      });
    }

    // The next successful read migrates the complete fallback blob with one
    // persistent write; no partial credential fields are exposed.
    expect(readTransportV2Credentials(FALLBACK_API_URL, "user")?.principalId).toBe("fallback-user");
    expect(storageKeys().filter((key) => key.startsWith(STORAGE_PREFIX))).toHaveLength(1);
    expect(globalThis.localStorage.getItem("access_token")).toBeNull();
    expect(globalThis.localStorage.getItem("refresh_token")).toBeNull();
    clearTransportV2Credentials(FALLBACK_API_URL);
    clearTransportV2CacheRoot(FALLBACK_API_URL);
  });
});
