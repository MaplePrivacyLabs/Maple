import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";

interface AuthFence {
  version: 1;
  apiOrigin: string;
  userRevision: number;
  principalId: null;
}

interface CredentialPair {
  accessToken: string;
  refreshToken: string;
}

const API_URL = "https://api.example.test/v1";
const API_ORIGIN = "https://api.example.test";
const CACHE_ROOT_BASE64 = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const ATTEMPT_ID = "12345678-1234-4234-8234-123456789abc";
const SECOND_ATTEMPT_ID = "87654321-4321-4321-8321-cba987654321";
const SESSION_ID = "01".repeat(16);
const REQUEST_ID = "ab".repeat(16);
const HANDOFF_GRANT = "aaa.bbb.ccc";
const USER_ID = "12345678-1234-4234-8234-123456789abc";
const EXPECTED_AUTH: AuthFence = {
  version: 1,
  apiOrigin: API_ORIGIN,
  userRevision: 7,
  principalId: null
};

let prepareCalls: string[] = [];
let installCalls: Array<{
  apiUrl: string;
  credentials: CredentialPair;
  expectedAuth: AuthFence;
  expectedPrincipalId: string;
}> = [];
let installFailure: Error | null = null;

interface SharedNativeOAuthTestState {
  currentUser: { id: string } | undefined;
  prepareCalls: string[];
  installCalls: Array<{
    apiUrl: string;
    credentials: CredentialPair;
    expectedAuth: AuthFence;
    expectedPrincipalId: string;
  }>;
  installFailure: Error | null;
  tauriInvoke: (command: string, args?: unknown) => Promise<unknown>;
}

const testGlobal = globalThis as typeof globalThis & {
  __mapleNativeOAuthTestState?: SharedNativeOAuthTestState;
};

function sharedState(): SharedNativeOAuthTestState {
  testGlobal.__mapleNativeOAuthTestState ??= {
    currentUser: undefined,
    prepareCalls: [],
    installCalls: [],
    installFailure: null,
    tauriInvoke: async () => {
      throw new Error("Unexpected native OAuth invocation");
    }
  };
  return testGlobal.__mapleNativeOAuthTestState;
}

const realOpenSecret = await import("@mapleai/sdk");
mock.module("@mapleai/sdk", () => ({
  ...realOpenSecret,
  useOpenSecret: () => ({ auth: { user: sharedState().currentUser } }),
  prepareNativeOAuthHandoff: (apiUrl: string) => {
    sharedState().prepareCalls.push(apiUrl);
    return {
      expectedAuth: { ...EXPECTED_AUTH },
      cacheNamespaceRootBase64: CACHE_ROOT_BASE64
    };
  },
  installNativeOAuthHandoffCredentials: (
    apiUrl: string,
    credentials: CredentialPair,
    expectedAuth: AuthFence,
    expectedPrincipalId: string
  ) => {
    const state = sharedState();
    state.installCalls.push({ apiUrl, credentials, expectedAuth, expectedPrincipalId });
    if (state.installFailure) throw state.installFailure;
    return {
      apiOrigin: API_ORIGIN,
      revision: EXPECTED_AUTH.userRevision + 1,
      principalId: expectedPrincipalId,
      credentials,
      cacheNamespaceRootBase64: CACHE_ROOT_BASE64
    };
  }
}));

const realTauriCore = await import("@tauri-apps/api/core");
mock.module("@tauri-apps/api/core", () => ({
  ...realTauriCore,
  invoke: (command: string, args?: unknown) => sharedState().tauriInvoke(command, args)
}));

const {
  authorizeNativeOAuthCallback,
  beginNativeOAuthAttempt,
  cancelNativeOAuthAttempt,
  isNativeOAuthHandoffGrant,
  PENDING_NATIVE_OAUTH_ATTEMPT_TTL_MS,
  readPendingNativeOAuthAttempt,
  redeemNativeOAuthGrant,
  startNativeOAuth
} = await import("./nativeOAuthAttempt");

class MemoryStorage implements Storage {
  private readonly values = new Map<string, string>();

  get length(): number {
    return this.values.size;
  }

  clear(): void {
    this.values.clear();
  }

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  key(index: number): string | null {
    return [...this.values.keys()][index] ?? null;
  }

  removeItem(key: string): void {
    this.values.delete(key);
  }

  setItem(key: string, value: string): void {
    this.values.set(key, value);
  }
}

interface InvokeCall {
  command: string;
  args: unknown;
}

const originalStorages = {
  localStorage: Object.getOwnPropertyDescriptor(globalThis, "localStorage"),
  sessionStorage: Object.getOwnPropertyDescriptor(globalThis, "sessionStorage")
};

function restoreStorage(name: "localStorage" | "sessionStorage"): void {
  const descriptor = originalStorages[name];
  if (descriptor) {
    Object.defineProperty(globalThis, name, descriptor);
  } else {
    Reflect.deleteProperty(globalThis, name);
  }
}

function beginResponse(attemptId = ATTEMPT_ID, sessionId = SESSION_ID, requestId = REQUEST_ID) {
  return { nativeOAuthAttempt: attemptId, sessionId, requestId };
}

function nativeInvoke(
  calls: InvokeCall[],
  options: {
    begin?: ReturnType<typeof beginResponse>;
    redeem?: { userId: string; accessToken: string; refreshToken: string };
  } = {}
) {
  return async (command: string, args?: unknown): Promise<unknown> => {
    calls.push({ command, args });
    if (command === "native_oauth_begin") return options.begin ?? beginResponse();
    if (command === "native_oauth_redeem") {
      return (
        options.redeem ?? {
          userId: USER_ID,
          accessToken: "access-token",
          refreshToken: "refresh-token"
        }
      );
    }
    return undefined;
  };
}

describe("native OAuth Transport V2 handoff", () => {
  let storage: MemoryStorage;

  beforeEach(() => {
    storage = new MemoryStorage();
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      value: storage,
      writable: true
    });
    Object.defineProperty(globalThis, "sessionStorage", {
      configurable: true,
      value: storage,
      writable: true
    });
    const state = sharedState();
    state.currentUser = undefined;
    state.prepareCalls = [];
    state.installCalls = [];
    state.installFailure = null;
    state.tauriInvoke = async () => {
      throw new Error("Unexpected native OAuth invocation");
    };
    prepareCalls = state.prepareCalls;
    installCalls = state.installCalls;
    installFailure = state.installFailure;
  });

  afterEach(() => {
    restoreStorage("localStorage");
    restoreStorage("sessionStorage");
  });

  test("opens the hosted flow with exactly the native session and request ids", async () => {
    const calls: InvokeCall[] = [];
    await startNativeOAuth(
      "github",
      API_URL,
      {
        selectedPlan: "pro",
        next: "/settings/api?tab=keys#credits",
        redemptionCode: "local-redemption-secret"
      },
      nativeInvoke(calls)
    );

    expect(prepareCalls).toEqual([API_URL]);
    expect(calls[0]).toEqual({
      command: "native_oauth_begin",
      args: {
        request: {
          apiUrl: API_ORIGIN,
          cacheNamespaceRootBase64: CACHE_ROOT_BASE64
        }
      }
    });
    expect(calls[1]?.command).toBe("plugin:opener|open_url");

    const openerArgs = calls[1]?.args as { url: string };
    const opened = new URL(openerArgs.url);
    expect(opened.origin).toBe("https://trymaple.ai");
    expect(opened.pathname).toBe("/desktop-auth");
    const expectedEntries: [string, string][] = [
      ["native_request_id", REQUEST_ID],
      ["native_session_id", SESSION_ID],
      ["provider", "github"],
      ["transport", "v2"]
    ];
    expect([...opened.searchParams.entries()].sort()).toEqual(expectedEntries.sort());
    expect(openerArgs.url).not.toContain(ATTEMPT_ID);
    expect(openerArgs.url).not.toContain(CACHE_ROOT_BASE64);
    expect(openerArgs.url).not.toContain("local-redemption-secret");
    expect(openerArgs.url).not.toContain("settings");
    expect(openerArgs.url).not.toContain("access-token");
    expect(openerArgs.url).not.toContain("refresh-token");
  });

  test("redeems with the signed grant only and installs through the captured CAS fence", async () => {
    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(
      API_URL,
      {
        selectedPlan: "pro",
        next: "/settings/api?credits_success=true#balance",
        redemptionCode: "local-redemption-code"
      },
      1_000,
      nativeInvoke(calls)
    );

    const completed = await redeemNativeOAuthGrant(HANDOFF_GRANT, nativeInvoke(calls));

    expect(calls[1]).toEqual({
      command: "native_oauth_redeem",
      args: { request: { handoffGrant: HANDOFF_GRANT } }
    });
    expect(installCalls).toEqual([
      {
        apiUrl: API_ORIGIN,
        credentials: { accessToken: "access-token", refreshToken: "refresh-token" },
        expectedAuth: EXPECTED_AUTH,
        expectedPrincipalId: USER_ID
      }
    ]);
    expect(completed).toMatchObject({
      selectedPlan: "pro",
      next: "/settings/api?credits_success=true#balance",
      redemptionCode: "local-redemption-code",
      sessionId: SESSION_ID,
      requestId: REQUEST_ID
    });
    expect(readPendingNativeOAuthAttempt()).toBeNull();
  });

  test("rejects a stale browser authority instead of accepting the native result", async () => {
    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(API_URL, {}, 1_000, nativeInvoke(calls));
    installFailure = new Error("Transport V2 authority changed");
    sharedState().installFailure = installFailure;

    await expect(redeemNativeOAuthGrant(HANDOFF_GRANT, nativeInvoke(calls))).rejects.toThrow(
      "authority changed"
    );
    expect(installCalls).toHaveLength(1);
  });

  test("rejecting a malformed callback does not consume the pending handoff", async () => {
    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(API_URL, {}, 1_000, nativeInvoke(calls));

    for (const invalid of ["", "not-a-grant", "a.b", "a.b.c=", "a.aaaaa.c"]) {
      expect(isNativeOAuthHandoffGrant(invalid)).toBeFalse();
    }
    expect(authorizeNativeOAuthCallback(false, 1_001)).toBe("accepted");
    expect(readPendingNativeOAuthAttempt()).toMatchObject({
      attemptId: ATTEMPT_ID,
      sessionId: SESSION_ID,
      requestId: REQUEST_ID
    });
  });

  test("persists only safe local navigation and returns it after redemption", async () => {
    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(
      API_URL,
      {
        selectedPlan: "team",
        next: "https://evil.example/steal",
        redemptionCode: "kept-locally"
      },
      1_000,
      nativeInvoke(calls)
    );

    expect(readPendingNativeOAuthAttempt()).toMatchObject({
      selectedPlan: "team",
      redemptionCode: "kept-locally"
    });
    expect(readPendingNativeOAuthAttempt()?.next).toBeUndefined();

    const completed = await redeemNativeOAuthGrant(HANDOFF_GRANT, nativeInvoke(calls));
    expect(completed.next).toBeUndefined();
    expect(completed.selectedPlan).toBe("team");
    expect(completed.redemptionCode).toBe("kept-locally");
  });

  test("an older cancellation cannot remove a newer pending attempt", async () => {
    const calls: InvokeCall[] = [];
    const invokeCommand = nativeInvoke(calls);
    await beginNativeOAuthAttempt(API_URL, {}, 1_000, invokeCommand);
    await beginNativeOAuthAttempt(
      API_URL,
      {},
      2_000,
      nativeInvoke(calls, {
        begin: beginResponse(SECOND_ATTEMPT_ID, "02".repeat(16), "cd".repeat(16))
      })
    );

    await cancelNativeOAuthAttempt(ATTEMPT_ID, invokeCommand);

    expect(readPendingNativeOAuthAttempt()).toMatchObject({ attemptId: SECOND_ATTEMPT_ID });
    expect(authorizeNativeOAuthCallback(false, 2_001)).toBe("accepted");
  });

  test("expires or rejects callbacks without an anonymous pending handoff", async () => {
    expect(authorizeNativeOAuthCallback(false, 1_000)).toBe("missing_or_expired_attempt");

    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(API_URL, {}, 1_000, nativeInvoke(calls));
    expect(authorizeNativeOAuthCallback(true, 1_001)).toBe("already_authenticated");
    expect(readPendingNativeOAuthAttempt()).not.toBeNull();

    expect(
      authorizeNativeOAuthCallback(false, 1_000 + PENDING_NATIVE_OAUTH_ATTEMPT_TTL_MS + 1)
    ).toBe("missing_or_expired_attempt");
    expect(readPendingNativeOAuthAttempt()).toBeNull();
  });
});
