import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import { createElement } from "react";
import TestRenderer, { act, type ReactTestRenderer } from "react-test-renderer";
import {
  captureUserCredentialSnapshot,
  clearUserCredentialsIfCurrent,
  OpenSecretProvider,
  useOpenSecret,
  type OpenSecretContextType,
  type PcrConfig,
  type UserResponse
} from "../index";
import * as api from "../api";
import { apiConfig } from "../apiConfig";
import {
  clearTransportV2Credentials,
  installTransportV2Credentials,
  readTransportV2Credentials
} from "../transportV2/auth";
import { transportV2Runtime } from "../transportV2/runtime";

const API_URL = "https://react-user-cleanup.example.test/backend";
const CLIENT_ID = "00000000-0000-4000-8000-000000000001";
const API_KEY = "00000000-0000-4000-8000-000000000002";
const PCR_CONFIG: PcrConfig = { environment: "development" };

let renderer: ReactTestRenderer | undefined;
let observed: OpenSecretContextType | undefined;
let respondWithProfile: (principalId: string) => Promise<UserResponse>;
let requestedPrincipals: string[];
let unexpectedTransportRequests: string[];
let unexpectedNetworkRequests: number;
let restoreSpies: Array<() => void>;
let previousApiUrl: string;
let previousPcrConfig: PcrConfig;
let previousConfiguredAppUrl: string;
let previousConfiguredPlatformUrl: string;

function token(principalId: string, purpose: "access" | "refresh", version: number): string {
  const encode = (value: unknown) => Buffer.from(JSON.stringify(value)).toString("base64url");
  return [
    encode({ alg: "ES256K", typ: "JWT" }),
    encode({
      aud: `urn:opensecret:internal:transport-v2:user:${purpose}-token`,
      sub: principalId,
      tf: 2,
      exp: 4_000_000_000 + version
    }),
    Buffer.from(new Uint8Array(64).fill(1)).toString("base64url")
  ].join(".");
}

function installUser(principalId: string, version = 0) {
  return installTransportV2Credentials(
    API_URL,
    "user",
    token(principalId, "access", version),
    token(principalId, "refresh", version)
  );
}

function profile(principalId: string, name = "Initial profile"): UserResponse {
  return {
    user: {
      id: principalId,
      name,
      email: `${principalId}@example.test`,
      email_verified: true,
      login_method: "google",
      created_at: "2026-01-01T00:00:00Z",
      updated_at: "2026-01-01T00:00:00Z"
    }
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((fulfill) => {
    resolve = fulfill;
  });
  return { promise, resolve };
}

function current(): OpenSecretContextType {
  if (!observed) throw new Error("The provider consumer has not rendered.");
  return observed;
}

function Consumer() {
  observed = useOpenSecret();
  return null;
}

async function mountProvider(): Promise<void> {
  await act(async () => {
    const element = createElement(OpenSecretProvider, {
      apiUrl: API_URL,
      clientId: CLIENT_ID,
      pcrConfig: PCR_CONFIG,
      children: createElement(Consumer)
    });
    // The SDK uses React 19 types for its React 18/19 peer range; this renderer
    // has React 18 types. Both runtime packages are pinned to React 18.3.1.
    renderer = TestRenderer.create(element as Parameters<typeof TestRenderer.create>[0]);
  });
}

beforeEach(() => {
  previousApiUrl = api.getApiUrl();
  previousPcrConfig = api.getApiPcrConfig();
  previousConfiguredAppUrl = apiConfig.appApiUrl;
  previousConfiguredPlatformUrl = apiConfig.platformApiUrl;
  clearTransportV2Credentials(API_URL);
  observed = undefined;
  requestedPrincipals = [];
  unexpectedTransportRequests = [];
  unexpectedNetworkRequests = 0;
  respondWithProfile = async (principalId) => profile(principalId);

  // Keep the real provider, credential store, authority selection, and profile
  // publication fence. A successful deferred API result must be discarded by
  // the provider itself, independently of lower-level response guards.
  const fetchProfile = spyOn(api, "fetchUserWithTransportV2Authority").mockImplementation(
    async (_apiUrl, _pcrConfig, authority) => {
      const principalId = authority.credentials.principalId;
      requestedPrincipals.push(principalId);
      return respondWithProfile(principalId);
    }
  );
  const request = spyOn(transportV2Runtime, "request").mockImplementation(async (input) => {
    unexpectedTransportRequests.push(input.request.target);
    throw new Error("This provider cleanup test must not make a transport request.");
  });
  const fetch = spyOn(globalThis, "fetch").mockImplementation(async () => {
    unexpectedNetworkRequests += 1;
    throw new Error("This provider cleanup test must not access the network.");
  });
  restoreSpies = [
    () => fetchProfile.mockRestore(),
    () => request.mockRestore(),
    () => fetch.mockRestore()
  ];
});

afterEach(async () => {
  try {
    await act(async () => {
      renderer?.unmount();
    });
  } finally {
    renderer = undefined;
    observed = undefined;
    for (const restore of restoreSpies) restore();
    clearTransportV2Credentials(API_URL);
    api.setApiUrl(previousApiUrl, previousPcrConfig);
    apiConfig.configure(previousConfiguredAppUrl, previousConfiguredPlatformUrl);
  }
  expect(unexpectedTransportRequests).toEqual([]);
  expect(unexpectedNetworkRequests).toBe(0);
});

describe("public user credential cleanup through the React provider", () => {
  test("clears the published user while preserving the separately configured API key", async () => {
    installUser("first-user");
    await mountProvider();
    expect(current().auth).toEqual({ loading: false, user: profile("first-user") });
    const snapshot = captureUserCredentialSnapshot(API_URL);
    expect(snapshot).not.toBeNull();

    await act(async () => {
      current().setApiKey(API_KEY);
    });
    expect(current().apiKey).toBe(API_KEY);

    await act(async () => {
      expect(clearUserCredentialsIfCurrent(snapshot!)).toBe(true);
    });
    expect(current().auth).toEqual({ loading: false, user: undefined });
    expect(current().apiKey).toBe(API_KEY);
    expect(readTransportV2Credentials(API_URL, "user")).toBeNull();
    expect(requestedPrincipals).toEqual(["first-user"]);
  });

  test("a deferred successful profile result cannot republish the user after cleanup", async () => {
    installUser("first-user");
    await mountProvider();
    expect(current().auth.user).toEqual(profile("first-user"));
    const snapshot = captureUserCredentialSnapshot(API_URL);
    expect(snapshot).not.toBeNull();

    const started = deferred<void>();
    const pendingProfile = deferred<UserResponse>();
    respondWithProfile = () => {
      started.resolve();
      return pendingProfile.promise;
    };
    let refetch!: Promise<void>;
    await act(async () => {
      refetch = current().refetchUser();
      await started.promise;
    });
    expect(requestedPrincipals).toEqual(["first-user", "first-user"]);

    await act(async () => {
      expect(clearUserCredentialsIfCurrent(snapshot!)).toBe(true);
    });
    expect(current().auth).toEqual({ loading: false, user: undefined });

    await act(async () => {
      pendingProfile.resolve(profile("first-user", "Deferred stale profile"));
      await refetch;
    });
    expect(current().auth).toEqual({ loading: false, user: undefined });
    expect(readTransportV2Credentials(API_URL, "user")).toBeNull();
    expect(requestedPrincipals).toEqual(["first-user", "first-user"]);
  });

  for (const replacementId of ["first-user", "second-user"]) {
    test(`stale cleanup preserves the newer ${replacementId === "first-user" ? "same-account" : "other-account"} profile`, async () => {
      installUser("first-user");
      await mountProvider();
      expect(current().auth.user).toEqual(profile("first-user"));
      const snapshot = captureUserCredentialSnapshot(API_URL);
      expect(snapshot).not.toBeNull();

      respondWithProfile = async (principalId) => profile(principalId, "Newer login profile");
      let installed!: ReturnType<typeof installUser>;
      await act(async () => {
        installed = installUser(replacementId, 1);
        await current().refetchUser();
      });
      const newerProfile = profile(replacementId, "Newer login profile");
      expect(current().auth).toEqual({ loading: false, user: newerProfile });

      await act(async () => {
        expect(clearUserCredentialsIfCurrent(snapshot!)).toBe(false);
      });
      expect(current().auth).toEqual({ loading: false, user: newerProfile });
      expect(readTransportV2Credentials(API_URL, "user")).toEqual(installed);
      expect(requestedPrincipals).toEqual(["first-user", replacementId]);
    });
  }
});
