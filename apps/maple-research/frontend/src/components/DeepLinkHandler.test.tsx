import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import { NotificationProvider } from "@/contexts/NotificationContext";

interface DeepLinkEvent {
  payload: string;
}

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

const API_URL = "https://api.example.test/v1";
const API_ORIGIN = "https://api.example.test";
const CACHE_ROOT_BASE64 = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const ATTEMPT_ID = "12345678-1234-4234-8234-123456789abc";
const SESSION_ID = "01".repeat(16);
const REQUEST_ID = "ab".repeat(16);
const VALID_GRANT = "aaa.bbb.ccc";
const USER_ID = "12345678-1234-4234-8234-123456789abc";
const EXPECTED_AUTH: AuthFence = {
  version: 1,
  apiOrigin: API_ORIGIN,
  userRevision: 7,
  principalId: null
};

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

let deepLinkListener: ((event: DeepLinkEvent) => Promise<void> | void) | undefined;
const unlisten = mock(() => {});

mock.module("@tauri-apps/api/event", () => ({
  listen: async (_eventName: string, listener: (event: DeepLinkEvent) => Promise<void> | void) => {
    deepLinkListener = listener;
    return unlisten;
  }
}));

type PendingNativeOAuthAttempt = import("@/services/nativeOAuthAttempt").PendingNativeOAuthAttempt;
const { beginNativeOAuthAttempt, readPendingNativeOAuthAttempt } =
  await import("@/services/nativeOAuthAttempt");
const { DeepLinkHandler } = await import("./DeepLinkHandler");
const { NativeOAuthAccountConfirmation } = await import("./NativeOAuthAccountConfirmation");

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

const originalGlobals = {
  localStorage: Object.getOwnPropertyDescriptor(globalThis, "localStorage"),
  sessionStorage: Object.getOwnPropertyDescriptor(globalThis, "sessionStorage"),
  window: Object.getOwnPropertyDescriptor(globalThis, "window")
};

function restoreGlobal(name: string, descriptor: PropertyDescriptor | undefined): void {
  if (descriptor) {
    Object.defineProperty(globalThis, name, descriptor);
  } else {
    Reflect.deleteProperty(globalThis, name);
  }
}

async function flushDeepLink(payload: string): Promise<void> {
  await act(async () => {
    await deepLinkListener?.({ payload });
  });
}

describe("DeepLinkHandler native auth callbacks", () => {
  let location: { href: string };
  let renderer: ReactTestRenderer | null;
  let storage: MemoryStorage;
  let nativeCalls: Array<{ command: string; args: unknown }>;
  let originalConsoleError: typeof console.error;
  let originalConsoleLog: typeof console.log;
  let originalConsoleWarn: typeof console.warn;

  beforeEach(async () => {
    deepLinkListener = undefined;
    renderer = null;
    storage = new MemoryStorage();
    location = { href: "tauri://localhost/" };
    nativeCalls = [];

    const state = sharedState();
    state.currentUser = undefined;
    state.prepareCalls = [];
    state.installCalls = [];
    state.installFailure = null;
    state.tauriInvoke = async (command, args) => {
      nativeCalls.push({ command, args });
      if (command === "native_oauth_redeem") {
        return {
          userId: USER_ID,
          email: "verified@example.com",
          accessToken: "incoming-access",
          refreshToken: "incoming-refresh"
        };
      }
      throw new Error(`Unexpected native OAuth invocation: ${command}`);
    };

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
    Object.defineProperty(globalThis, "window", {
      configurable: true,
      value: { localStorage: storage, sessionStorage: storage, location },
      writable: true
    });

    originalConsoleError = console.error;
    originalConsoleLog = console.log;
    originalConsoleWarn = console.warn;
    console.error = mock(() => {});
    console.log = mock(() => {});
    console.warn = mock(() => {});

    await act(async () => {
      renderer = create(
        <NotificationProvider>
          <DeepLinkHandler tauri />
        </NotificationProvider>
      );
      await Promise.resolve();
    });
    if (!deepLinkListener) throw new Error("Deep-link listener was not registered");
  });

  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    restoreGlobal("localStorage", originalGlobals.localStorage);
    restoreGlobal("sessionStorage", originalGlobals.sessionStorage);
    restoreGlobal("window", originalGlobals.window);
    console.error = originalConsoleError;
    console.log = originalConsoleLog;
    console.warn = originalConsoleWarn;
  });

  async function prepareAttempt(
    navigation: {
      selectedPlan?: string;
      next?: string;
      redemptionCode?: string;
    } = {}
  ): Promise<PendingNativeOAuthAttempt> {
    await beginNativeOAuthAttempt(API_URL, navigation, Date.now(), async (command) => {
      if (command !== "native_oauth_begin") throw new Error(`Unexpected command: ${command}`);
      return {
        nativeOAuthAttempt: ATTEMPT_ID,
        sessionId: SESSION_ID,
        requestId: REQUEST_ID
      };
    });
    const pending = readPendingNativeOAuthAttempt();
    if (!pending) throw new Error("Expected a pending native OAuth attempt");
    return pending;
  }

  async function startDeepLink(payload: string): Promise<{ completion: Promise<void> }> {
    let completion: Promise<void> = Promise.resolve();
    await act(async () => {
      completion = Promise.resolve(deepLinkListener?.({ payload }));
      await Promise.resolve();
    });
    return { completion };
  }

  async function decideAccount(approved: boolean, completion: Promise<void>): Promise<void> {
    await act(async () => {
      renderer?.root.findByType(NativeOAuthAccountConfirmation).props.onDecision(approved);
      await completion;
    });
  }

  async function approveDeepLink(payload: string): Promise<void> {
    const { completion } = await startDeepLink(payload);
    await decideAccount(true, completion);
  }

  test("shows the verified account and waits for approval before installation and navigation", async () => {
    await prepareAttempt({ next: "/settings/security" });

    const { completion } = await startDeepLink(
      `cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}`
    );

    expect(renderer?.root.findByType(NativeOAuthAccountConfirmation).props.account).toEqual({
      userId: USER_ID,
      email: "verified@example.com"
    });
    expect(sharedState().installCalls).toHaveLength(0);
    expect(location.href).toBe("tauri://localhost/");
    expect(JSON.stringify(readPendingNativeOAuthAttempt())).not.toContain("incoming-access");
    await flushDeepLink(`cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}`);
    expect(nativeCalls).toHaveLength(1);

    await decideAccount(true, completion);

    expect(nativeCalls).toEqual([
      {
        command: "native_oauth_redeem",
        args: { request: { handoffGrant: VALID_GRANT } }
      }
    ]);
    expect(sharedState().installCalls).toHaveLength(1);
    expect(readPendingNativeOAuthAttempt()).toBeNull();
    expect(location.href).toBe("/settings/security");
  });

  test("cancelling the account confirmation discards the result without signing in", async () => {
    await prepareAttempt();
    const payload = `cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}`;
    const { completion } = await startDeepLink(payload);
    await decideAccount(false, completion);

    expect(sharedState().installCalls).toHaveLength(0);
    expect(readPendingNativeOAuthAttempt()).toBeNull();
    expect(renderer?.root.findAllByType(NativeOAuthAccountConfirmation)).toHaveLength(0);
    expect(location.href).toBe("tauri://localhost/");
    await flushDeepLink(payload);
    expect(nativeCalls).toHaveLength(1);
  });

  test("unmounting while confirmation is open discards the result", async () => {
    await prepareAttempt();
    const { completion } = await startDeepLink(
      `cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}`
    );
    await act(async () => {
      renderer?.unmount();
      renderer = null;
      await completion;
    });
    expect(sharedState().installCalls).toHaveLength(0);
    expect(readPendingNativeOAuthAttempt()).toBeNull();
    expect(location.href).toBe("tauri://localhost/");
  });

  test("another sign-in while confirmation is open cancels the pending approval", async () => {
    await prepareAttempt();
    const { completion } = await startDeepLink(
      `cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}`
    );
    sharedState().currentUser = { id: "another-account" };
    await act(async () => {
      renderer?.update(
        <NotificationProvider>
          <DeepLinkHandler tauri />
        </NotificationProvider>
      );
    });
    await act(async () => await completion);
    expect(sharedState().installCalls).toHaveLength(0);
    expect(renderer?.root.findAllByType(NativeOAuthAccountConfirmation)).toHaveLength(0);
    expect(location.href).toBe("tauri://localhost/");
  });

  test("rejects legacy credentials, extra state, and malformed auth envelopes without consuming pending state", async () => {
    const pending = await prepareAttempt();
    const malformedCallbacks = [
      "cloud.opensecret.maple://auth?access_token=secret&refresh_token=secret",
      `cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}&next=%2Fsettings`,
      `cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}&cache_namespace_root=secret`,
      `cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}&handoff_grant=${VALID_GRANT}`,
      `cloud.opensecret.maple://auth/path?handoff_grant=${VALID_GRANT}`,
      `cloud.opensecret.maple://other?handoff_grant=${VALID_GRANT}`,
      `https://auth/?handoff_grant=${VALID_GRANT}`,
      "cloud.opensecret.maple://auth?handoff_grant=not-a-valid-grant",
      `cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}#fragment`,
      `cloud.opensecret.maple.dev://auth?handoff_grant=${VALID_GRANT}`
    ];

    for (const payload of malformedCallbacks) await flushDeepLink(payload);

    expect(nativeCalls).toHaveLength(0);
    expect(readPendingNativeOAuthAttempt()).toEqual(pending);
    expect(location.href).toBe("tauri://localhost/");
  });

  test("Maple Dev ignores production auth and payment returns, then accepts its own confirmed grant", async () => {
    await act(async () => {
      renderer?.update(
        <NotificationProvider>
          <DeepLinkHandler tauri appVariant="dev" />
        </NotificationProvider>
      );
    });
    const pending = await prepareAttempt();
    await flushDeepLink(`cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}`);
    await flushDeepLink("cloud.opensecret.maple://payment-success");
    await flushDeepLink("https://trymaple.ai/payment-success");
    expect(nativeCalls).toHaveLength(0);
    expect(readPendingNativeOAuthAttempt()).toEqual(pending);
    expect(location.href).toBe("tauri://localhost/");
    await approveDeepLink(`cloud.opensecret.maple.dev://auth?handoff_grant=${VALID_GRANT}`);
    expect(sharedState().installCalls).toHaveLength(1);
  });

  test("ignores callbacks without a pending attempt or while already authenticated", async () => {
    await flushDeepLink(`cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}`);

    await prepareAttempt();
    sharedState().currentUser = { id: "current-user" };
    await act(async () => {
      renderer?.update(
        <NotificationProvider>
          <DeepLinkHandler tauri />
        </NotificationProvider>
      );
    });
    await flushDeepLink(`cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}`);

    expect(nativeCalls).toHaveLength(0);
    expect(readPendingNativeOAuthAttempt()).not.toBeNull();
    expect(location.href).toBe("tauri://localhost/");
  });

  test("coalesces duplicate callbacks while redemption is in progress", async () => {
    await prepareAttempt({ next: "/settings" });
    let releaseRedemption: ((value: unknown) => void) | undefined;
    sharedState().tauriInvoke = async (command, args) => {
      nativeCalls.push({ command, args });
      return new Promise((resolve) => {
        releaseRedemption = resolve;
      });
    };

    const payload = `cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}`;
    const first = deepLinkListener?.({ payload });
    await Promise.resolve();
    const duplicate = deepLinkListener?.({ payload });
    await Promise.resolve();

    expect(nativeCalls).toHaveLength(1);
    releaseRedemption?.({
      userId: USER_ID,
      email: "verified@example.com",
      accessToken: "incoming-access",
      refreshToken: "incoming-refresh"
    });
    await act(async () => {
      await duplicate;
      await Promise.resolve();
    });

    await decideAccount(true, Promise.resolve(first));

    expect(nativeCalls).toHaveLength(1);
    expect(location.href).toBe("/settings");
  });

  test("a stale browser authority fails closed without navigation", async () => {
    await prepareAttempt({ next: "/settings" });
    sharedState().installFailure = new Error("Transport V2 authority changed");

    await approveDeepLink(`cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}`);

    expect(nativeCalls).toHaveLength(1);
    expect(sharedState().installCalls).toHaveLength(1);
    expect(JSON.stringify(renderer?.toJSON())).toContain("Sign-in could not be completed");
    expect(JSON.stringify(renderer?.toJSON())).toContain("Please restart sign-in from Maple.");
    expect(location.href).toBe("tauri://localhost/");
  });

  test("does not install an older redemption after a newer attempt replaces local state", async () => {
    await prepareAttempt({ next: "/settings" });
    let releaseRedemption: ((value: unknown) => void) | undefined;
    sharedState().tauriInvoke = async (command, args) => {
      nativeCalls.push({ command, args });
      if (command === "native_oauth_redeem") {
        return new Promise((resolve) => {
          releaseRedemption = resolve;
        });
      }
      throw new Error(`Unexpected native OAuth invocation: ${command}`);
    };

    const redemption = deepLinkListener?.({
      payload: `cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}`
    });
    await Promise.resolve();
    localStorage.setItem(
      "maple_pending_native_oauth_attempt_v2",
      JSON.stringify({
        ...readPendingNativeOAuthAttempt(),
        attemptId: "abcdefab-cdef-4abc-8def-abcdefabcdef"
      })
    );
    releaseRedemption?.({
      userId: USER_ID,
      email: "verified@example.com",
      accessToken: "stale-access",
      refreshToken: "stale-refresh"
    });

    await act(async () => {
      await redemption;
    });

    expect(sharedState().installCalls).toHaveLength(0);
    expect(location.href).toBe("tauri://localhost/");
    expect(renderer?.root.findAllByType(NativeOAuthAccountConfirmation)).toHaveLength(0);
  });

  test("keeps plan and redemption navigation out of the callback", async () => {
    await prepareAttempt({ selectedPlan: "team annual", redemptionCode: "local code" });
    const callback = `cloud.opensecret.maple://auth?handoff_grant=${VALID_GRANT}`;
    expect(callback).not.toContain("team");
    expect(callback).not.toContain("local%20code");

    await approveDeepLink(callback);

    expect(location.href).toBe("/pricing?selected_plan=team%20annual");
  });
});

describe("DeepLinkHandler payment callbacks", () => {
  let location: { href: string };
  let renderer: ReactTestRenderer | null;
  let originalConsoleError: typeof console.error;
  let originalConsoleLog: typeof console.log;
  let originalConsoleWarn: typeof console.warn;

  beforeEach(async () => {
    deepLinkListener = undefined;
    sharedState().currentUser = undefined;
    renderer = null;
    location = { href: "tauri://localhost/" };
    Object.defineProperty(globalThis, "window", {
      configurable: true,
      value: { location },
      writable: true
    });
    originalConsoleError = console.error;
    originalConsoleLog = console.log;
    originalConsoleWarn = console.warn;
    console.error = mock(() => {});
    console.log = mock(() => {});
    console.warn = mock(() => {});
    await act(async () => {
      renderer = create(
        <NotificationProvider>
          <DeepLinkHandler tauri />
        </NotificationProvider>
      );
      await Promise.resolve();
    });
    if (!deepLinkListener) throw new Error("Deep-link listener was not registered");
  });

  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    restoreGlobal("window", originalGlobals.window);
    console.error = originalConsoleError;
    console.log = originalConsoleLog;
    console.warn = originalConsoleWarn;
  });

  test("preserves success, credits, cancellation, and unknown payment routing", async () => {
    await flushDeepLink("cloud.opensecret.maple://payment-success");
    expect(location.href).toBe("/pricing?success=true");

    await flushDeepLink("cloud.opensecret.maple://payment-success-credits");
    expect(location.href).toBe("/?credits_success=true");

    await flushDeepLink("cloud.opensecret.maple://payment-canceled");
    expect(location.href).toBe("/pricing?canceled=true");

    await flushDeepLink("cloud.opensecret.maple://payment?source=desktop");
    expect(location.href).toBe("/pricing");
  });
});
