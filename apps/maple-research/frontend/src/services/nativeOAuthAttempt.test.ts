import { afterEach, beforeEach, describe, expect, mock, spyOn, test } from "bun:test";

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
const USER_EMAIL = "signed-in@example.test";
const APPROVE_ACCOUNT = { confirmAccount: async () => true };
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
    redeem?: { userId: string; email: string | null; accessToken: string; refreshToken: string };
  } = {}
) {
  return async (command: string, args?: unknown): Promise<unknown> => {
    calls.push({ command, args });
    if (command === "native_oauth_begin") return options.begin ?? beginResponse();
    if (command === "native_oauth_redeem") {
      return (
        options.redeem ?? {
          userId: USER_ID,
          email: USER_EMAIL,
          accessToken: "access-token",
          refreshToken: "refresh-token"
        }
      );
    }
    return undefined;
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((onResolve, onReject) => {
    resolve = onResolve;
    reject = onReject;
  });
  return { promise, resolve, reject };
}

describe("native OAuth Transport V2 handoff", () => {
  let storage: MemoryStorage;
  let now: number;
  let restoreDateNow: () => void;

  beforeEach(() => {
    now = 1_000;
    const clock = spyOn(Date, "now").mockImplementation(() => now);
    restoreDateNow = () => clock.mockRestore();
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
    restoreDateNow();
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

  test("cancels a prepared native attempt when hosted entry configuration is rejected", async () => {
    const desktopTransport = await import("./desktopOAuthTransport");
    const buildUrl = spyOn(desktopTransport, "buildTransportV2DesktopAuthUrl").mockImplementation(
      () => {
        throw new Error("Authentication origin is invalid");
      }
    );
    const calls: InvokeCall[] = [];
    try {
      await expect(startNativeOAuth("github", API_URL, {}, nativeInvoke(calls))).rejects.toThrow(
        "Authentication origin is invalid"
      );
      expect(calls.map(({ command }) => command)).toEqual([
        "native_oauth_begin",
        "native_oauth_cancel"
      ]);
      expect(calls[1]?.args).toEqual({ request: { nativeOAuthAttempt: ATTEMPT_ID } });
      expect(readPendingNativeOAuthAttempt()).toBeNull();
    } finally {
      buildUrl.mockRestore();
    }
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

    const confirmAccount = mock(async () => true);
    const completed = await redeemNativeOAuthGrant(
      HANDOFF_GRANT,
      { confirmAccount },
      nativeInvoke(calls)
    );

    expect(confirmAccount).toHaveBeenCalledWith({ userId: USER_ID, email: USER_EMAIL });
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

    await expect(
      redeemNativeOAuthGrant(HANDOFF_GRANT, APPROVE_ACCOUNT, nativeInvoke(calls))
    ).rejects.toThrow("authority changed");
    expect(installCalls).toHaveLength(1);
    expect(readPendingNativeOAuthAttempt()).toBeNull();
  });

  test("keeps credentials out of storage and the confirmation callback until approval", async () => {
    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(API_URL, {}, now, nativeInvoke(calls));
    const decision = deferred<boolean>();
    const shown = deferred<void>();
    const confirmAccount = mock((account: { userId: string; email: string | null }) => {
      expect(account).toEqual({ userId: USER_ID, email: USER_EMAIL });
      shown.resolve();
      return decision.promise;
    });
    const completion = redeemNativeOAuthGrant(
      HANDOFF_GRANT,
      { confirmAccount },
      nativeInvoke(calls)
    );
    await shown.promise;

    expect(installCalls).toHaveLength(0);
    expect(calls.filter((call) => call.command === "native_oauth_redeem")).toHaveLength(1);
    const saved = storage.getItem(storage.key(0)!);
    expect(saved).not.toContain("access-token");
    expect(saved).not.toContain("refresh-token");
    expect(saved).not.toContain(USER_EMAIL);

    decision.resolve(true);
    expect(await completion).toMatchObject({ attemptId: ATTEMPT_ID });
    expect(installCalls).toHaveLength(1);
  });

  test("declining the account discards the one-use result without installing", async () => {
    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(API_URL, {}, now, nativeInvoke(calls));

    expect(
      await redeemNativeOAuthGrant(
        HANDOFF_GRANT,
        { confirmAccount: async () => false },
        nativeInvoke(calls)
      )
    ).toBeNull();
    expect(installCalls).toHaveLength(0);
    expect(readPendingNativeOAuthAttempt()).toBeNull();
    await expect(
      redeemNativeOAuthGrant(HANDOFF_GRANT, APPROVE_ACCOUNT, nativeInvoke(calls))
    ).rejects.toThrow("not pending");
    expect(calls.filter((call) => call.command === "native_oauth_redeem")).toHaveLength(1);
  });

  test("abort releases a pending confirmation immediately and ignores a later approval", async () => {
    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(API_URL, {}, now, nativeInvoke(calls));
    const decision = deferred<boolean>();
    const shown = deferred<void>();
    const controller = new AbortController();
    const completion = redeemNativeOAuthGrant(
      HANDOFF_GRANT,
      {
        signal: controller.signal,
        confirmAccount: () => {
          shown.resolve();
          return decision.promise;
        }
      },
      nativeInvoke(calls)
    );
    await shown.promise;
    controller.abort();

    expect(await completion).toBeNull();
    expect(installCalls).toHaveLength(0);
    expect(readPendingNativeOAuthAttempt()).toBeNull();
    decision.resolve(true);
    await Promise.resolve();
    expect(installCalls).toHaveLength(0);
  });

  test("an already aborted flow does not redeem or show confirmation", async () => {
    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(API_URL, {}, now, nativeInvoke(calls));
    const controller = new AbortController();
    controller.abort();
    const confirmAccount = mock(async () => true);

    expect(
      await redeemNativeOAuthGrant(
        HANDOFF_GRANT,
        { confirmAccount, signal: controller.signal },
        nativeInvoke(calls)
      )
    ).toBeNull();
    expect(calls).toHaveLength(1);
    expect(confirmAccount).not.toHaveBeenCalled();
    expect(installCalls).toHaveLength(0);
  });

  for (const transition of ["new attempt", "cancellation", "auth change", "expiry"] as const) {
    test(`${transition} during confirmation prevents installation of the older result`, async () => {
      const calls: InvokeCall[] = [];
      await beginNativeOAuthAttempt(API_URL, {}, now, nativeInvoke(calls));
      const decision = deferred<boolean>();
      const shown = deferred<void>();
      const completion = redeemNativeOAuthGrant(
        HANDOFF_GRANT,
        {
          confirmAccount: () => {
            shown.resolve();
            return decision.promise;
          }
        },
        nativeInvoke(calls)
      );
      await shown.promise;
      if (transition === "new attempt") {
        await beginNativeOAuthAttempt(
          API_URL,
          { next: "/settings" },
          now,
          nativeInvoke(calls, {
            begin: beginResponse(SECOND_ATTEMPT_ID, "02".repeat(16), "cd".repeat(16))
          })
        );
      } else if (transition === "cancellation") {
        await cancelNativeOAuthAttempt(ATTEMPT_ID, nativeInvoke(calls));
      } else if (transition === "auth change") {
        sharedState().installFailure = new Error("Transport V2 authority changed");
      } else {
        now += PENDING_NATIVE_OAUTH_ATTEMPT_TTL_MS + 1;
      }
      decision.resolve(true);

      if (transition === "auth change") {
        // The SDK's CAS is the final authority even before React observes a
        // login/logout. This mock models its rejection, not an installation.
        await expect(completion).rejects.toThrow("authority changed");
        expect(installCalls[0]?.expectedAuth).toEqual(EXPECTED_AUTH);
      } else {
        expect(await completion).toBeNull();
        expect(installCalls).toHaveLength(0);
      }
      if (transition === "new attempt") {
        expect(readPendingNativeOAuthAttempt()).toMatchObject({
          attemptId: SECOND_ATTEMPT_ID,
          next: "/settings"
        });
      } else {
        expect(readPendingNativeOAuthAttempt()).toBeNull();
      }
    });
  }

  for (const transition of ["new attempt", "abort", "expiry"] as const) {
    test(`${transition} during redemption does not show or install the stale account`, async () => {
      const calls: InvokeCall[] = [];
      const invokeCommand = nativeInvoke(calls);
      await beginNativeOAuthAttempt(API_URL, {}, now, invokeCommand);
      const response = deferred<unknown>();
      const controller = new AbortController();
      const confirmAccount = mock(async () => true);
      const completion = redeemNativeOAuthGrant(
        HANDOFF_GRANT,
        { confirmAccount, signal: controller.signal },
        () => response.promise
      );
      if (transition === "new attempt") {
        await beginNativeOAuthAttempt(
          API_URL,
          {},
          now,
          nativeInvoke(calls, {
            begin: beginResponse(SECOND_ATTEMPT_ID, "02".repeat(16), "cd".repeat(16))
          })
        );
      } else if (transition === "abort") {
        controller.abort();
      } else {
        now += PENDING_NATIVE_OAUTH_ATTEMPT_TTL_MS + 1;
      }
      response.resolve(await invokeCommand("native_oauth_redeem"));

      expect(await completion).toBeNull();
      expect(confirmAccount).not.toHaveBeenCalled();
      expect(installCalls).toHaveLength(0);
      expect(readPendingNativeOAuthAttempt()?.attemptId ?? null).toBe(
        transition === "new attempt" ? SECOND_ATTEMPT_ID : null
      );
    });
  }

  test("expired attempts do not invoke native redemption", async () => {
    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(API_URL, {}, now, nativeInvoke(calls));
    now += PENDING_NATIVE_OAUTH_ATTEMPT_TTL_MS + 1;

    expect(
      await redeemNativeOAuthGrant(HANDOFF_GRANT, APPROVE_ACCOUNT, nativeInvoke(calls))
    ).toBeNull();
    expect(calls).toHaveLength(1);
    expect(readPendingNativeOAuthAttempt()).toBeNull();
  });

  test("aborting a redemption that subsequently fails remains a silent cancellation", async () => {
    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(API_URL, {}, now, nativeInvoke(calls));
    const response = deferred<unknown>();
    const controller = new AbortController();
    const confirmAccount = mock(async () => true);
    const completion = redeemNativeOAuthGrant(
      HANDOFF_GRANT,
      { confirmAccount, signal: controller.signal },
      () => response.promise
    );
    controller.abort();
    response.reject(new Error("Native request failed"));

    expect(await completion).toBeNull();
    expect(confirmAccount).not.toHaveBeenCalled();
    expect(installCalls).toHaveLength(0);
    expect(readPendingNativeOAuthAttempt()).toBeNull();
  });

  test("the remaining attempt lifetime bounds an unanswered confirmation", async () => {
    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(API_URL, {}, now, nativeInvoke(calls));
    now += PENDING_NATIVE_OAUTH_ATTEMPT_TTL_MS - 20;
    const decision = deferred<boolean>();
    const confirmAccount = mock(() => decision.promise);

    expect(
      await redeemNativeOAuthGrant(HANDOFF_GRANT, { confirmAccount }, nativeInvoke(calls))
    ).toBeNull();
    expect(confirmAccount).toHaveBeenCalledTimes(1);
    expect(installCalls).toHaveLength(0);
    expect(readPendingNativeOAuthAttempt()).toBeNull();
    decision.resolve(true);
  });

  test("a confirmation failure discards the result and does not retry", async () => {
    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(API_URL, {}, now, nativeInvoke(calls));

    await expect(
      redeemNativeOAuthGrant(
        HANDOFF_GRANT,
        {
          confirmAccount: async () => {
            throw new Error("Confirmation unavailable");
          }
        },
        nativeInvoke(calls)
      )
    ).rejects.toThrow("Confirmation unavailable");
    expect(installCalls).toHaveLength(0);
    expect(readPendingNativeOAuthAttempt()).toBeNull();
    expect(calls.filter((call) => call.command === "native_oauth_redeem")).toHaveLength(1);
  });

  test("a missing native account identity fails before confirmation", async () => {
    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(API_URL, {}, now, nativeInvoke(calls));
    const confirmAccount = mock(async () => true);

    await expect(
      redeemNativeOAuthGrant(HANDOFF_GRANT, { confirmAccount }, async () => ({
        userId: USER_ID,
        accessToken: "access-token",
        refreshToken: "refresh-token"
      }))
    ).rejects.toThrow("invalid account");
    expect(confirmAccount).not.toHaveBeenCalled();
    expect(installCalls).toHaveLength(0);
  });

  test("an account without an email is confirmed using its verified user ID", async () => {
    const calls: InvokeCall[] = [];
    await beginNativeOAuthAttempt(API_URL, {}, now, nativeInvoke(calls));
    const confirmAccount = mock(async () => true);
    await redeemNativeOAuthGrant(
      HANDOFF_GRANT,
      { confirmAccount },
      nativeInvoke(calls, {
        redeem: {
          userId: USER_ID,
          email: null,
          accessToken: "access-token",
          refreshToken: "refresh-token"
        }
      })
    );

    expect(confirmAccount).toHaveBeenCalledWith({ userId: USER_ID, email: null });
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

    const completed = await redeemNativeOAuthGrant(
      HANDOFF_GRANT,
      APPROVE_ACCOUNT,
      nativeInvoke(calls)
    );
    expect(completed?.next).toBeUndefined();
    expect(completed?.selectedPlan).toBe("team");
    expect(completed?.redemptionCode).toBe("kept-locally");
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
