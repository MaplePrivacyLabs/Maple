import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { act, create, type ReactTestRenderer } from "react-test-renderer";

import type { AppleAuthorization } from "./AppleAuthProvider";

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

interface FakeScript {
  async: boolean;
  parentNode: FakeHead | null;
  src: string;
}

interface FakeHead {
  appendChild(script: FakeScript): FakeScript;
  removeChild(script: FakeScript): FakeScript;
}

class FakeDocument extends EventTarget {
  readonly addedEventTypes: string[] = [];
  readonly head: FakeHead = {
    appendChild: (script) => {
      script.parentNode = this.head;
      return script;
    },
    removeChild: (script) => {
      script.parentNode = null;
      return script;
    }
  };

  override addEventListener(
    type: string,
    callback: EventListenerOrEventListenerObject | null,
    options?: AddEventListenerOptions | boolean
  ): void {
    this.addedEventTypes.push(type);
    super.addEventListener(type, callback, options);
  }

  createElement(): FakeScript {
    return { async: false, parentNode: null, src: "" };
  }
}

interface SignInControl {
  reject(error: unknown): void;
  resolve(result: { authorization: AppleAuthorization }): void;
}

function appleEvent(type: "AppleIDSignInOnSuccess" | "AppleIDSignInOnFailure", detail: unknown) {
  const event = new Event(type);
  Object.defineProperty(event, "detail", { value: detail });
  return event;
}

let currentOpenSecret: Record<string, unknown>;
const realOpenSecret = await import("@mapleai/sdk");
mock.module("@mapleai/sdk", () => ({
  ...realOpenSecret,
  useOpenSecret: () => currentOpenSecret
}));

const { initBillingService } = await import("@/billing/billingService");
const { AppleAuthProvider } = await import("./AppleAuthProvider");

const originalGlobals = {
  document: Object.getOwnPropertyDescriptor(globalThis, "document"),
  localStorage: Object.getOwnPropertyDescriptor(globalThis, "localStorage"),
  sessionStorage: Object.getOwnPropertyDescriptor(globalThis, "sessionStorage"),
  window: Object.getOwnPropertyDescriptor(globalThis, "window")
};

function setGlobal(name: string, value: unknown): void {
  Object.defineProperty(globalThis, name, {
    configurable: true,
    value,
    writable: true
  });
}

function restoreGlobal(name: string, descriptor: PropertyDescriptor | undefined): void {
  if (descriptor) {
    Object.defineProperty(globalThis, name, descriptor);
  } else {
    Reflect.deleteProperty(globalThis, name);
  }
}

describe("AppleAuthProvider", () => {
  let documentTarget: FakeDocument;
  let renderer: ReactTestRenderer | null;
  let signInControls: SignInControl[];
  let initiateAppleAuth: ReturnType<typeof mock>;
  let handleAppleCallback: ReturnType<typeof mock>;
  let mintNativeHandoffGrant: ReturnType<typeof mock>;
  let onError: ReturnType<typeof mock>;
  let onSuccess: ReturnType<typeof mock>;
  let redirectAfterLogin: ReturnType<typeof mock>;
  let appleInit: ReturnType<typeof mock>;
  let appleSignIn: ReturnType<typeof mock>;
  let originalConsoleError: typeof console.error;

  beforeEach(() => {
    renderer = null;
    originalConsoleError = console.error;
    console.error = mock(() => {});
    documentTarget = new FakeDocument();
    signInControls = [];
    const states = ["state-one", "state-two", "state-three", "state-four"];
    const nonces = ["11", "22", "33", "44"].map((value) => value.repeat(32));

    initiateAppleAuth = mock(async () => ({
      auth_url: `https://appleid.apple.com/auth/authorize?nonce=${nonces.shift() ?? "55".repeat(32)}`,
      state: states.shift() ?? "unexpected-state"
    }));
    handleAppleCallback = mock(async () => {});
    mintNativeHandoffGrant = mock(async () => ({ grant: "aaa.bbb.ccc", expires_at: 42 }));
    onError = mock(() => {});
    onSuccess = mock(() => {});
    redirectAfterLogin = mock(() => {});
    appleInit = mock(() => {});
    appleSignIn = mock(
      () =>
        new Promise<{ authorization: AppleAuthorization }>((resolve, reject) => {
          signInControls.push({ resolve, reject });
        })
    );

    currentOpenSecret = {
      initiateAppleAuth,
      handleAppleCallback,
      mintNativeHandoffGrant
    };
    initBillingService(currentOpenSecret as never);

    const localStorage = new MemoryStorage();
    const sessionStorage = new MemoryStorage();
    const windowValue = {
      AppleID: {
        auth: {
          init: appleInit,
          signIn: appleSignIn
        }
      },
      localStorage,
      location: {
        href: "https://trymaple.ai/login",
        origin: "https://trymaple.ai",
        protocol: "https:"
      },
      sessionStorage
    };

    setGlobal("document", documentTarget);
    setGlobal("localStorage", localStorage);
    setGlobal("sessionStorage", sessionStorage);
    setGlobal("window", windowValue);
  });

  afterEach(() => {
    if (renderer) {
      act(() => renderer?.unmount());
    }
    restoreGlobal("document", originalGlobals.document);
    restoreGlobal("localStorage", originalGlobals.localStorage);
    restoreGlobal("sessionStorage", originalGlobals.sessionStorage);
    restoreGlobal("window", originalGlobals.window);
    console.error = originalConsoleError;
  });

  async function startAttempt(): Promise<{ completion: Promise<void>; control: SignInControl }> {
    let completion: Promise<void> | undefined;
    await act(async () => {
      completion = renderer?.root.findByType("button").props.onClick();
      for (let index = 0; index < 5 && signInControls.length === 0; index += 1) {
        await Promise.resolve();
      }
    });

    const control = signInControls.shift();
    if (!completion || !control) throw new Error("Apple sign-in attempt did not start");
    return { completion, control };
  }

  test("uses only the signIn promise and retries with fresh state after cancellation or rejection", async () => {
    await act(async () => {
      renderer = create(
        <AppleAuthProvider
          inviteCode="invite-one"
          onError={onError}
          onSuccess={onSuccess}
          redirectAfterLogin={redirectAfterLogin}
          selectedPlan="pro"
        />
      );
    });

    expect(
      documentTarget.addedEventTypes.filter((type) => type.startsWith("AppleIDSignIn"))
    ).toEqual([]);

    const firstAttempt = await startAttempt();
    await act(async () => {
      await renderer?.root.findByType("button").props.onClick();
    });
    expect(initiateAppleAuth).toHaveBeenCalledTimes(1);
    expect(appleSignIn).toHaveBeenCalledTimes(1);

    documentTarget.dispatchEvent(
      appleEvent("AppleIDSignInOnSuccess", {
        authorization: { code: "event-code", state: "state-one" }
      })
    );
    documentTarget.dispatchEvent(
      appleEvent("AppleIDSignInOnFailure", { error: "event-only-error" })
    );
    await Promise.resolve();

    expect(handleAppleCallback).toHaveBeenCalledTimes(0);
    expect(onError).toHaveBeenCalledTimes(0);

    await act(async () => {
      firstAttempt.control.reject({ error: "user_cancelled_authorize" });
      await firstAttempt.completion;
    });

    expect(onError).toHaveBeenCalledTimes(0);
    expect(handleAppleCallback).toHaveBeenCalledTimes(0);

    const legacyCancellationAttempt = await startAttempt();
    await act(async () => {
      legacyCancellationAttempt.control.reject({ error: "popup_closed_by_user" });
      await legacyCancellationAttempt.completion;
    });

    expect(onError).toHaveBeenCalledTimes(0);
    expect(handleAppleCallback).toHaveBeenCalledTimes(0);

    const failedAttempt = await startAttempt();
    await act(async () => {
      failedAttempt.control.reject({ error: "authorization_failed" });
      await failedAttempt.completion;
    });

    expect(onError).toHaveBeenCalledTimes(1);
    expect(onError.mock.calls[0]?.[0]).toEqual(new Error("authorization_failed"));
    expect(handleAppleCallback).toHaveBeenCalledTimes(0);

    await act(async () => {
      renderer?.update(
        <AppleAuthProvider
          inviteCode="invite-two"
          onError={onError}
          onSuccess={onSuccess}
          redirectAfterLogin={redirectAfterLogin}
          selectedPlan="max"
        />
      );
    });

    const retry = await startAttempt();
    documentTarget.dispatchEvent(
      appleEvent("AppleIDSignInOnSuccess", {
        authorization: { code: "promise-code", state: "state-two" }
      })
    );
    documentTarget.dispatchEvent(
      appleEvent("AppleIDSignInOnFailure", { error: "ignored-event-error" })
    );
    await Promise.resolve();
    expect(handleAppleCallback).toHaveBeenCalledTimes(0);
    expect(onError).toHaveBeenCalledTimes(1);

    await act(async () => {
      retry.control.resolve({
        authorization: { code: "promise-code", state: "state-four" }
      });
      await retry.completion;
    });

    expect(initiateAppleAuth).toHaveBeenCalledTimes(4);
    expect(initiateAppleAuth).toHaveBeenNthCalledWith(1, "invite-one");
    expect(initiateAppleAuth).toHaveBeenNthCalledWith(2, "invite-one");
    expect(initiateAppleAuth).toHaveBeenNthCalledWith(3, "invite-one");
    expect(initiateAppleAuth).toHaveBeenNthCalledWith(4, "invite-two");
    expect(appleInit).toHaveBeenCalledTimes(4);
    expect(appleInit.mock.calls[0]?.[0]).toMatchObject({
      nonce: "11".repeat(32),
      state: "state-one"
    });
    expect(appleInit.mock.calls[1]?.[0]).toMatchObject({
      nonce: "22".repeat(32),
      state: "state-two"
    });
    expect(appleInit.mock.calls[2]?.[0]).toMatchObject({
      nonce: "33".repeat(32),
      state: "state-three"
    });
    expect(appleInit.mock.calls[3]?.[0]).toMatchObject({
      nonce: "44".repeat(32),
      state: "state-four"
    });
    expect(handleAppleCallback).toHaveBeenCalledTimes(1);
    expect(handleAppleCallback).toHaveBeenCalledWith("promise-code", "state-four", "invite-two");
    expect(onError).toHaveBeenCalledTimes(1);
    expect(onSuccess).toHaveBeenCalledTimes(1);
    expect(redirectAfterLogin).toHaveBeenCalledTimes(1);
    expect(redirectAfterLogin).toHaveBeenCalledWith("max");
  });

  test("keeps a manual Open Maple fallback after hosted native Apple authentication", async () => {
    const { markTransportV2DesktopOAuth } = await import("@/services/desktopOAuthTransport");
    markTransportV2DesktopOAuth({
      provider: "apple",
      nativeSessionId: "01".repeat(16),
      nativeRequestId: "ab".repeat(16)
    });

    await act(async () => {
      renderer = create(<AppleAuthProvider onError={onError} />);
    });
    const attempt = await startAttempt();
    await act(async () => {
      attempt.control.resolve({
        authorization: { code: "native-code", state: "state-one" }
      });
      await attempt.completion;
    });

    expect(mintNativeHandoffGrant).toHaveBeenCalledWith("01".repeat(16), "ab".repeat(16));
    const fallback = renderer?.root.findByType("button");
    expect(fallback?.props.children).toBe("Open Maple");
    act(() => fallback?.props.onClick());
    expect(window.location.href).toBe("cloud.opensecret.maple://auth?handoff_grant=aaa.bbb.ccc");
  });

  for (const [name, authUrl] of [
    ["a malformed authorization URL", "not a URL"],
    ["a missing nonce", "https://appleid.apple.com/auth/authorize"],
    ["an empty nonce", "https://appleid.apple.com/auth/authorize?nonce="],
    [
      "duplicate nonces",
      `https://appleid.apple.com/auth/authorize?nonce=${"11".repeat(32)}&nonce=${"22".repeat(32)}`
    ],
    ["a noncanonical nonce", "https://appleid.apple.com/auth/authorize?nonce=ABCDEF"]
  ] as const) {
    test(`fails closed when initiation returns ${name}`, async () => {
      initiateAppleAuth.mockImplementationOnce(async () => ({
        auth_url: authUrl,
        state: "state-one"
      }));

      await act(async () => {
        renderer = create(<AppleAuthProvider onError={onError} />);
      });
      await act(async () => {
        await renderer?.root.findByType("button").props.onClick();
      });

      expect(onError).toHaveBeenCalledTimes(1);
      expect(onError.mock.calls[0]?.[0]).toEqual(
        new Error("Apple authorization response did not contain a valid nonce")
      );
      expect(appleInit).not.toHaveBeenCalled();
      expect(appleSignIn).not.toHaveBeenCalled();
      expect(handleAppleCallback).not.toHaveBeenCalled();
    });
  }
});
