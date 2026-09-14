import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mintNativeHandoffGrantWithCall, parseNativeHandoffGrantResponse, setApiUrl } from "../api";
import {
  TransportV2AuthorityChangedError,
  installTransportV2Credentials,
  isTransportV2AuthSnapshotCurrent,
  readTransportV2Credentials,
  snapshotTransportV2Auth,
  subscribeTransportV2AuthInvalidation
} from "../transportV2/auth";
import {
  installNativeOAuthHandoffCredentials,
  prepareNativeOAuthHandoff,
  readNativeUserAuth
} from "../transportV2/nativeAuth";

const API_URL = "https://api.example.test/service";
const SESSION_ID = "0a".repeat(16);
const REQUEST_ID = "ab".repeat(16);

function base64Url(value: string | Uint8Array): string {
  return Buffer.from(value).toString("base64url");
}

function userToken(purpose: "access" | "refresh", principalId: string, version = 0): string {
  const audience =
    purpose === "access"
      ? "urn:opensecret:internal:transport-v2:user:access-token"
      : "urn:opensecret:internal:transport-v2:user:refresh-token";
  return [
    base64Url(JSON.stringify({ alg: "ES256K", typ: "JWT" })),
    base64Url(
      JSON.stringify({
        aud: audience,
        sub: principalId,
        exp: 2_000_000_000 + version,
        tf: 2
      })
    ),
    base64Url(new Uint8Array(64).fill(0x5a))
  ].join(".");
}

function userCredentials(principalId: string, version = 0) {
  return {
    accessToken: userToken("access", principalId, version),
    refreshToken: userToken("refresh", principalId, version)
  };
}

function grant(): string {
  return [
    base64Url(JSON.stringify({ alg: "ES256K", typ: "JWT" })),
    base64Url(JSON.stringify({ sub: "user-123", exp: 2_000_000_000 })),
    base64Url(new Uint8Array(64).fill(0x4a))
  ].join(".");
}

beforeEach(() => {
  globalThis.localStorage.clear();
  setApiUrl(API_URL);
});

afterEach(() => {
  globalThis.localStorage.clear();
});

describe("native Transport V2 authentication bridge", () => {
  test("prepares a serializable anonymous CAS fence and one stable padded cache root", () => {
    const prepared = prepareNativeOAuthHandoff(API_URL);
    expect(prepared.expectedAuth).toEqual({
      version: 1,
      apiOrigin: "https://api.example.test",
      userRevision: 0,
      principalId: null
    });
    expect(prepared.cacheNamespaceRootBase64).toMatch(/^[A-Za-z0-9+/]{43}=$/u);

    const restored = JSON.parse(JSON.stringify(prepared.expectedAuth));
    const second = prepareNativeOAuthHandoff("https://api.example.test/another-base");
    expect(second.expectedAuth).toEqual(restored);
    expect(second.cacheNamespaceRootBase64).toBe(prepared.cacheNamespaceRootBase64);
    expect(readNativeUserAuth(API_URL).cacheNamespaceRootBase64).toBe(
      prepared.cacheNamespaceRootBase64
    );
  });

  test("installs a native credential pair only into the exact anonymous authority", () => {
    const prepared = prepareNativeOAuthHandoff(API_URL);
    const credentials = userCredentials("user-123");
    const installed = installNativeOAuthHandoffCredentials(
      API_URL,
      credentials,
      JSON.parse(JSON.stringify(prepared.expectedAuth)),
      "user-123"
    );

    expect(installed).toMatchObject({
      apiOrigin: "https://api.example.test",
      revision: 1,
      principalId: "user-123",
      credentials
    });
    expect(installed.cacheNamespaceRootBase64).toBe(prepared.cacheNamespaceRootBase64);
    expect(readTransportV2Credentials(API_URL, "user")?.principalId).toBe("user-123");

    expect(() =>
      installNativeOAuthHandoffCredentials(API_URL, credentials, prepared.expectedAuth, "user-123")
    ).toThrow(TransportV2AuthorityChangedError);
  });

  test("does not let a stale native result replace newer browser authentication", () => {
    const prepared = prepareNativeOAuthHandoff(API_URL);
    const newer = userCredentials("newer-user");
    installTransportV2Credentials(API_URL, "user", newer.accessToken, newer.refreshToken);

    expect(() =>
      installNativeOAuthHandoffCredentials(
        API_URL,
        userCredentials("stale-native-user"),
        prepared.expectedAuth,
        "stale-native-user"
      )
    ).toThrow(TransportV2AuthorityChangedError);
    expect(readNativeUserAuth(API_URL)).toMatchObject({
      principalId: "newer-user",
      credentials: newer
    });
  });

  test("a failed native credential write leaves anonymous authority intact after storage recovers", () => {
    const prepared = prepareNativeOAuthHandoff(API_URL);
    const before = readNativeUserAuth(API_URL);
    const snapshot = snapshotTransportV2Auth(API_URL, "user");
    const originalStorage = globalThis.localStorage;
    const credentials = userCredentials("uncommitted-user");
    let invalidations = 0;
    const unsubscribe = subscribeTransportV2AuthInvalidation(API_URL, "user", () => {
      invalidations += 1;
    });
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      value: {
        getItem: (key: string) => originalStorage.getItem(key),
        removeItem: (key: string) => originalStorage.removeItem(key),
        setItem(): never {
          throw new Error("fixture storage quota exceeded");
        }
      } as Storage
    });
    try {
      expect(() =>
        installNativeOAuthHandoffCredentials(
          API_URL,
          credentials,
          prepared.expectedAuth,
          "uncommitted-user"
        )
      ).toThrow("native credential installation could not be persisted");
      expect(readTransportV2Credentials(API_URL, "user")).toBeNull();
      expect(isTransportV2AuthSnapshotCurrent(snapshot)).toBe(true);
      expect(readNativeUserAuth(API_URL)).toEqual(before);
      expect(invalidations).toBe(0);
    } finally {
      Object.defineProperty(globalThis, "localStorage", {
        configurable: true,
        value: originalStorage
      });
      unsubscribe();
    }

    // Reads after storage recovery must not flush a rejected credential pair.
    expect(readTransportV2Credentials(API_URL, "user")).toBeNull();
    expect(snapshotTransportV2Auth(API_URL, "user")).toEqual(snapshot);
    expect(readNativeUserAuth(API_URL)).toEqual(before);
    expect(prepareNativeOAuthHandoff(API_URL)).toEqual(prepared);
  });

  test("publishes a native install only after persistence without a fallible post-commit read", () => {
    const prepared = prepareNativeOAuthHandoff(API_URL);
    const originalStorage = globalThis.localStorage;
    const credentials = userCredentials("committed-user");
    let writtenState: Record<string, unknown> | undefined;
    let invalidations = 0;
    const unsubscribe = subscribeTransportV2AuthInvalidation(API_URL, "user", () => {
      expect(writtenState).toMatchObject({
        cache_namespace_root: prepared.cacheNamespaceRootBase64,
        user: {
          revision: 1,
          credentials: {
            access_token: credentials.accessToken,
            refresh_token: credentials.refreshToken
          }
        }
      });
      invalidations += 1;
    });
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      value: {
        getItem(key: string): string | null {
          if (writtenState) throw new Error("fixture read failure after commit");
          return originalStorage.getItem(key);
        },
        removeItem: (key: string) => originalStorage.removeItem(key),
        setItem(key: string, value: string) {
          originalStorage.setItem(key, value);
          writtenState = JSON.parse(value);
        }
      } as Storage
    });
    try {
      expect(
        installNativeOAuthHandoffCredentials(
          API_URL,
          credentials,
          prepared.expectedAuth,
          "committed-user"
        )
      ).toEqual({
        apiOrigin: "https://api.example.test",
        revision: 1,
        principalId: "committed-user",
        credentials,
        cacheNamespaceRootBase64: prepared.cacheNamespaceRootBase64
      });
      expect(invalidations).toBe(1);
    } finally {
      Object.defineProperty(globalThis, "localStorage", {
        configurable: true,
        value: originalStorage
      });
      unsubscribe();
    }

    expect(readNativeUserAuth(API_URL)).toMatchObject({
      principalId: "committed-user",
      credentials,
      cacheNamespaceRootBase64: prepared.cacheNamespaceRootBase64
    });
  });

  test("rejects signed-in preparation and forged or cross-origin fences", () => {
    const prepared = prepareNativeOAuthHandoff(API_URL);
    const credentials = userCredentials("user-123");
    installTransportV2Credentials(
      API_URL,
      "user",
      credentials.accessToken,
      credentials.refreshToken
    );
    expect(() => prepareNativeOAuthHandoff(API_URL)).toThrow("requires an anonymous");

    const forged = {
      ...prepared.expectedAuth,
      apiOrigin: "https://other.example.test"
    };
    expect(() =>
      installNativeOAuthHandoffCredentials(API_URL, credentials, forged, "user-123")
    ).toThrow(TransportV2AuthorityChangedError);
    expect(() =>
      installNativeOAuthHandoffCredentials(
        API_URL,
        credentials,
        {
          ...prepared.expectedAuth,
          // The public fence is intentionally strict when restored from storage.
          principalId: "user-123"
        } as never,
        "user-123"
      )
    ).toThrow("fence is invalid");
  });

  test("rejects a native response identity mismatch before committing credentials", () => {
    const prepared = prepareNativeOAuthHandoff(API_URL);
    const credentials = userCredentials("credential-user");

    expect(() =>
      installNativeOAuthHandoffCredentials(
        API_URL,
        credentials,
        prepared.expectedAuth,
        "response-user"
      )
    ).toThrow("credential identity does not match");
    expect(readTransportV2Credentials(API_URL, "user")).toBeNull();
  });
});

describe("native handoff grant API", () => {
  test("sends the exact canonical target pair and strictly decodes the grant", async () => {
    const expected = { grant: grant(), expires_at: 2_000_000_000 };
    const calls: unknown[][] = [];
    const response = await mintNativeHandoffGrantWithCall(
      SESSION_ID,
      REQUEST_ID,
      async (...args) => {
        calls.push(args);
        return expected;
      }
    );

    expect(calls).toEqual([
      [
        `${API_URL}/auth/native-handoff/grant`,
        "POST",
        { native_session_id: SESSION_ID, native_request_id: REQUEST_ID },
        "Failed to mint native OAuth handoff grant"
      ]
    ]);
    expect(response).toEqual(expected);
  });

  test("rejects non-canonical identifiers before making a request", async () => {
    let calls = 0;
    const call = async () => {
      calls += 1;
      return { grant: grant(), expires_at: 2_000_000_000 };
    };
    for (const id of [SESSION_ID.toUpperCase(), `${SESSION_ID}-`, "0".repeat(31), "z".repeat(32)]) {
      await expect(mintNativeHandoffGrantWithCall(id, REQUEST_ID, call)).rejects.toThrow(
        "canonical transport v2 identifiers"
      );
    }
    expect(calls).toBe(0);
  });

  test("rejects non-canonical, oversized, or shape-ambiguous responses", () => {
    const validGrant = grant();
    expect(parseNativeHandoffGrantResponse({ grant: validGrant, expires_at: 42 })).toEqual({
      grant: validGrant,
      expires_at: 42
    });
    for (const value of [
      { grant: validGrant, expires_at: 42, extra: true },
      { grant: `${validGrant}=`, expires_at: 42 },
      { grant: "a.b", expires_at: 42 },
      { grant: `a.${"a".repeat(4_097)}.b`, expires_at: 42 },
      { grant: validGrant, expires_at: 1.5 },
      { grant: validGrant, expires_at: Number.MAX_SAFE_INTEGER + 1 }
    ]) {
      expect(() => parseNativeHandoffGrantResponse(value)).toThrow("response is invalid");
    }
  });
});
