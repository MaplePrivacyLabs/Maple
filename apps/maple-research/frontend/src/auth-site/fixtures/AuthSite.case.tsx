import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { StrictMode, type Provider } from "react";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import { OpenSecretContext, type OpenSecretContextType } from "@mapleai/sdk";
import { HostedNativeSignInConfirmation } from "@/components/HostedNativeSignInConfirmation";
import {
  markTransportV2DesktopOAuth,
  readTransportV2DesktopOAuth,
  type DesktopOAuthProvider
} from "@/services/desktopOAuthTransport";
import { AuthSite } from "../AuthSite";

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

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

const nativeSessionId = "00112233445566778899aabbccddeeff";
const nativeRequestId = "ffeeddccbbaa99887766554433221100";
const authOrigin = "https://auth.example.com";
const providerUrl = "https://provider.example.com/authorize?state=fixture-provider-state";
const originalGlobals = {
  window: Object.getOwnPropertyDescriptor(globalThis, "window"),
  navigator: Object.getOwnPropertyDescriptor(globalThis, "navigator"),
  localStorage: Object.getOwnPropertyDescriptor(globalThis, "localStorage"),
  sessionStorage: Object.getOwnPropertyDescriptor(globalThis, "sessionStorage")
};

type SdkContext = OpenSecretContextType;
// The linked SDK's development declarations use React 19; this app uses React 18.
// Runtime React is deduplicated by the test preload, as it is by Vite in production.
const SdkProvider = OpenSecretContext.Provider as unknown as Provider<SdkContext>;

function setGlobal(name: string, value: unknown): void {
  Object.defineProperty(globalThis, name, { configurable: true, writable: true, value });
}

function restoreGlobal(name: string, descriptor: PropertyDescriptor | undefined): void {
  if (descriptor) Object.defineProperty(globalThis, name, descriptor);
  else Reflect.deleteProperty(globalThis, name);
}

function startUrl(provider: DesktopOAuthProvider): string {
  const params = new URLSearchParams({
    transport: "v2",
    provider,
    native_session_id: nativeSessionId,
    native_request_id: nativeRequestId
  });
  return `${authOrigin}/start?${params.toString()}`;
}

function callbackUrl(provider: DesktopOAuthProvider = "github"): string {
  return `${authOrigin}/auth/${provider}/callback?code=fixture-code&state=opaque-fixture-state#retained-fragment`;
}

describe("hosted authentication entry", () => {
  let renderer: ReactTestRenderer | null;
  let sdk: SdkContext;
  let initiateGitHubAuth: ReturnType<typeof mock>;
  let initiateGoogleAuth: ReturnType<typeof mock>;
  let initiateAppleAuth: ReturnType<typeof mock>;
  let handleGitHubCallback: ReturnType<typeof mock>;
  let handleGoogleCallback: ReturnType<typeof mock>;
  let handleAppleCallback: ReturnType<typeof mock>;
  let mintNativeHandoffGrant: ReturnType<typeof mock>;

  beforeEach(() => {
    renderer = null;
    const localStorage = new MemoryStorage();
    const sessionStorage = new MemoryStorage();
    setGlobal("localStorage", localStorage);
    setGlobal("sessionStorage", sessionStorage);
    setGlobal("window", { localStorage, sessionStorage, location: new URL(authOrigin) });
    initiateGitHubAuth = mock(async () => ({ auth_url: providerUrl, state: "fixture-state" }));
    initiateGoogleAuth = mock(async () => ({ auth_url: providerUrl, state: "fixture-state" }));
    initiateAppleAuth = mock(async () => ({ auth_url: providerUrl, state: "fixture-state" }));
    handleGitHubCallback = mock(async () => {});
    handleGoogleCallback = mock(async () => {});
    handleAppleCallback = mock(async () => {});
    mintNativeHandoffGrant = mock(async () => ({ grant: "aaa.bbb.ccc", expires_at: 42 }));
    sdk = {
      apiUrl: "https://api.example.com",
      auth: { loading: false },
      initiateGitHubAuth,
      initiateGoogleAuth,
      initiateAppleAuth,
      handleGitHubCallback,
      handleGoogleCallback,
      handleAppleCallback,
      mintNativeHandoffGrant
    } as unknown as SdkContext;
  });

  afterEach(async () => {
    await act(async () => renderer?.unmount());
    restoreGlobal("window", originalGlobals.window);
    restoreGlobal("navigator", originalGlobals.navigator);
    restoreGlobal("localStorage", originalGlobals.localStorage);
    restoreGlobal("sessionStorage", originalGlobals.sessionStorage);
  });

  async function renderAt(url: string): Promise<void> {
    Object.defineProperty(window, "location", {
      configurable: true,
      value: new URL(url),
      writable: true
    });
    await act(async () => {
      renderer = create(
        <StrictMode>
          <SdkProvider value={sdk}>
            <AuthSite />
          </SdkProvider>
        </StrictMode>
      );
    });
  }

  function expectNoSdkCalls(): void {
    for (const method of [
      initiateGitHubAuth,
      initiateGoogleAuth,
      initiateAppleAuth,
      handleGitHubCallback,
      handleGoogleCallback,
      handleAppleCallback,
      mintNativeHandoffGrant
    ]) {
      expect(method).not.toHaveBeenCalled();
    }
  }

  function expectFailureWithoutNavigation(originalUrl: string): void {
    expect(window.location.href).toBe(originalUrl);
    expect(renderer!.root.findAllByProps({ role: "alert" }).length).toBeGreaterThan(0);
    expect(renderer!.root.findAllByType(HostedNativeSignInConfirmation)).toHaveLength(0);
    expect(mintNativeHandoffGrant).not.toHaveBeenCalled();
  }

  for (const path of [
    "/desktop-auth?provider=github",
    "/desktop-auth?transport=v1&provider=google",
    "/start?transport=v2&provider=github",
    "/login",
    "/pricing",
    "/auth/github/callback?code=fixture-code",
    "/auth/google/callback?error=access_denied&state=fixture-state"
  ]) {
    test(`rejects ${path} without invoking the SDK or navigating`, async () => {
      const originalUrl = `${authOrigin}${path}`;
      await renderAt(originalUrl);
      expectNoSdkCalls();
      expectFailureWithoutNavigation(originalUrl);
    });
  }

  test("leaves a complete page inert", async () => {
    const originalUrl = `${authOrigin}/complete`;
    await renderAt(originalUrl);
    expectNoSdkCalls();
    expect(window.location.href).toBe(originalUrl);
    expect(renderer!.root.findByProps({ role: "status" }).children.join("")).toContain(
      "return to Maple"
    );
  });

  for (const provider of ["github", "google"] as const) {
    test(`initiates ${provider} once with its same-origin callback and only opens the provider`, async () => {
      await renderAt(startUrl(provider));
      const initiate = provider === "github" ? initiateGitHubAuth : initiateGoogleAuth;
      const otherInitiate = provider === "github" ? initiateGoogleAuth : initiateGitHubAuth;
      expect(initiate).toHaveBeenCalledTimes(1);
      expect(initiate).toHaveBeenCalledWith("", `${authOrigin}/auth/${provider}/callback`);
      expect(otherInitiate).not.toHaveBeenCalled();
      expect(initiateAppleAuth).not.toHaveBeenCalled();
      expect(window.location.href).toBe(providerUrl);
      expect(mintNativeHandoffGrant).not.toHaveBeenCalled();
      expect(readTransportV2DesktopOAuth(provider)).toMatchObject({
        provider,
        nativeSessionId,
        nativeRequestId
      });
    });
  }

  test("does not initiate the same native attempt twice across a remount", async () => {
    const pending = deferred<{ auth_url: string; state: string }>();
    initiateGitHubAuth.mockImplementation(() => pending.promise);
    const originalUrl = startUrl("github");
    await renderAt(originalUrl);
    await act(async () => renderer?.unmount());
    renderer = null;
    await renderAt(originalUrl);
    expect(initiateGitHubAuth).toHaveBeenCalledTimes(1);

    await act(async () => pending.resolve({ auth_url: providerUrl, state: "fixture-state" }));
    expectFailureWithoutNavigation(originalUrl);
  });

  test("does not navigate when the native target changes during initiation", async () => {
    const pending = deferred<{ auth_url: string; state: string }>();
    initiateGoogleAuth.mockImplementation(() => pending.promise);
    const originalUrl = startUrl("google");
    await renderAt(originalUrl);
    markTransportV2DesktopOAuth({
      provider: "google",
      nativeSessionId,
      nativeRequestId: "11112222333344445555666677778888"
    });
    const replacement = readTransportV2DesktopOAuth("google");
    await act(async () => pending.resolve({ auth_url: providerUrl, state: "fixture-state" }));
    expectFailureWithoutNavigation(originalUrl);
    expect(readTransportV2DesktopOAuth("google")).toEqual(replacement);
  });

  test("does not redeem a callback without a same-tab native target", async () => {
    const originalUrl = callbackUrl();
    await renderAt(originalUrl);
    expectNoSdkCalls();
    expectFailureWithoutNavigation(originalUrl);
  });

  test("copies the full callback address only after the user requests it", async () => {
    const writeText = mock(async () => {});
    setGlobal("navigator", { clipboard: { writeText } });
    const originalUrl = callbackUrl();
    await renderAt(originalUrl);
    expect(writeText).not.toHaveBeenCalled();
    expect(renderer!.root.findAllByProps({ role: "status" })).toHaveLength(0);

    await act(async () => renderer!.root.findByType("button").props.onClick());

    expect(writeText).toHaveBeenCalledTimes(1);
    expect(writeText).toHaveBeenCalledWith(originalUrl);
    expect(renderer!.root.findByProps({ role: "status" }).children.join("")).toBe(
      "Address copied. Paste it only into the Maple sign-in you started."
    );
    expectNoSdkCalls();
    expect(window.location.href).toBe(originalUrl);
  });

  test("preserves the callback address and offers manual copying when clipboard access fails", async () => {
    const writeText = mock(async () => {
      throw new Error("Fixture clipboard permission denial");
    });
    setGlobal("navigator", { clipboard: { writeText } });
    const originalUrl = callbackUrl("google");
    await renderAt(originalUrl);
    expect(writeText).not.toHaveBeenCalled();

    await act(async () => renderer!.root.findByType("button").props.onClick());

    expect(writeText).toHaveBeenCalledTimes(1);
    expect(writeText).toHaveBeenCalledWith(originalUrl);
    expect(renderer!.root.findByProps({ role: "status" }).children.join("")).toBe(
      "Copy the full address from your browser's address bar instead."
    );
    expectNoSdkCalls();
    expect(window.location.href).toBe(originalUrl);
  });

  test("does not consume another provider's pending target", async () => {
    markTransportV2DesktopOAuth({ provider: "apple", nativeSessionId, nativeRequestId });
    const pending = readTransportV2DesktopOAuth("apple");
    const originalUrl = callbackUrl("google");
    await renderAt(originalUrl);
    expectNoSdkCalls();
    expectFailureWithoutNavigation(originalUrl);
    expect(readTransportV2DesktopOAuth("apple")).toEqual(pending);
  });

  for (const provider of ["github", "google"] as const) {
    test(`requires native confirmation after a successful ${provider} callback`, async () => {
      markTransportV2DesktopOAuth({ provider, nativeSessionId, nativeRequestId });
      const target = readTransportV2DesktopOAuth(provider);
      const originalUrl = callbackUrl(provider);
      await renderAt(originalUrl);
      const callback = {
        github: handleGitHubCallback,
        google: handleGoogleCallback
      }[provider];
      expect(callback).toHaveBeenCalledTimes(1);
      expect(callback).toHaveBeenCalledWith("fixture-code", "opaque-fixture-state", "");
      expect(renderer!.root.findByType(HostedNativeSignInConfirmation).props.target).toEqual(
        target
      );
      expect(mintNativeHandoffGrant).not.toHaveBeenCalled();
      expect(window.location.href).toBe(originalUrl);
    });
  }

  for (const url of [callbackUrl("apple"), `${authOrigin}/auth/apple/callback`]) {
    test(`keeps the Apple callback passive with popup retry guidance: ${new URL(url).search || "no query"}`, async () => {
      markTransportV2DesktopOAuth({ provider: "apple", nativeSessionId, nativeRequestId });
      const target = readTransportV2DesktopOAuth("apple");
      await renderAt(url);
      expectNoSdkCalls();
      expectFailureWithoutNavigation(url);
      expect(JSON.stringify(renderer!.toJSON())).toContain(
        "Apple sign-in must finish in its popup"
      );
      expect(readTransportV2DesktopOAuth("apple")).toEqual(target);
      expect(renderer!.root.findAllByType("button")).toHaveLength(0);
    });
  }

  test("preserves the full callback address on an SDK error", async () => {
    markTransportV2DesktopOAuth({ provider: "github", nativeSessionId, nativeRequestId });
    handleGitHubCallback.mockImplementation(async () => {
      throw new Error("Fixture callback rejection");
    });
    const originalUrl = callbackUrl();
    await renderAt(originalUrl);
    expect(handleGitHubCallback).toHaveBeenCalledTimes(1);
    expectFailureWithoutNavigation(originalUrl);
  });

  for (const outcome of ["resolve", "reject"] as const) {
    test(`preserves a newer pending flow when an old callback ${outcome}s`, async () => {
      const pending = deferred<void>();
      handleGitHubCallback.mockImplementation(() => pending.promise);
      markTransportV2DesktopOAuth({ provider: "github", nativeSessionId, nativeRequestId });
      const originalUrl = callbackUrl();
      await renderAt(originalUrl);
      markTransportV2DesktopOAuth({
        provider: "github",
        nativeSessionId,
        nativeRequestId: "11112222333344445555666677778888"
      });
      const replacement = readTransportV2DesktopOAuth("github");
      await act(async () => {
        if (outcome === "resolve") pending.resolve();
        else pending.reject(new Error("Fixture stale callback rejection"));
      });
      expectFailureWithoutNavigation(originalUrl);
      expect(readTransportV2DesktopOAuth("github")).toEqual(replacement);
    });
  }
});
