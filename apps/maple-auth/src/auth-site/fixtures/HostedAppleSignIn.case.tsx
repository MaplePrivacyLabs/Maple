import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import type { Provider } from "react";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import {
  OpenSecretContext,
  installNativeOAuthHandoffCredentials,
  prepareNativeOAuthHandoff,
  readNativeUserAuth,
  type OpenSecretContextType
} from "@mapleai/sdk";
import { HostedNativeSignInConfirmation } from "@/components/HostedNativeSignInConfirmation";
import {
  clearDesktopOAuthTarget,
  markTransportV2DesktopOAuth,
  readTransportV2DesktopOAuth,
  TRANSPORT_V2_PENDING_TTL_MS,
  type TransportV2DesktopOAuthState
} from "@/services/desktopOAuthTransport";
import { HostedAppleSignIn } from "../HostedAppleSignIn";

// Bridge the linked SDK's React 19 development declarations to this React 18 consumer.
// Both use the real, deduplicated React implementation in these tests.
const SdkProvider = OpenSecretContext.Provider as unknown as Provider<OpenSecretContextType>;

class MemoryStorage implements Storage {
  values = new Map<string, string>();
  get length() {
    return this.values.size;
  }
  clear() {
    this.values.clear();
  }
  key(index: number) {
    return [...this.values.keys()][index] ?? null;
  }
  getItem(key: string) {
    return this.values.get(key) ?? null;
  }
  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
  removeItem(key: string) {
    this.values.delete(key);
  }
}

const originals = Object.fromEntries(
  ["window", "localStorage", "sessionStorage"].map((key) => [
    key,
    Object.getOwnPropertyDescriptor(globalThis, key)
  ])
);
const nativeTarget = {
  provider: "apple" as const,
  nativeSessionId: "11".repeat(16),
  nativeRequestId: "22".repeat(16)
};
const fixtureUser = { id: "fixture-user", email: "fixture@example.test" };
let sequence = 0;

function credentials() {
  const token = (purpose: "access" | "refresh") =>
    [
      Buffer.from(JSON.stringify({ alg: "ES256K", typ: "JWT" })).toString("base64url"),
      Buffer.from(
        JSON.stringify({
          aud: `urn:opensecret:internal:transport-v2:user:${purpose}-token`,
          sub: fixtureUser.id,
          exp: 2_000_000_000,
          tf: 2
        })
      ).toString("base64url"),
      Buffer.from(new Uint8Array(64).fill(0x5a)).toString("base64url")
    ].join(".");
  return { accessToken: token("access"), refreshToken: token("refresh") };
}

describe("hosted Apple popup and shared native confirmation", () => {
  let renderer: ReactTestRenderer | null;
  let client: OpenSecretContextType;
  let local: MemoryStorage;
  let target: TransportV2DesktopOAuthState;
  let popupResolve: (value: { authorization: { code: string; state: string } }) => void;
  let popupReject: (error: unknown) => void;
  let initiate: ReturnType<typeof mock>;
  let callback: ReturnType<typeof mock>;
  let mint: ReturnType<typeof mock>;
  let init: ReturnType<typeof mock>;
  let signIn: ReturnType<typeof mock>;
  let stateNumber: number;

  beforeEach(() => {
    renderer = null;
    stateNumber = 0;
    local = new MemoryStorage();
    const session = new MemoryStorage();
    init = mock(() => {});
    signIn = mock(
      () =>
        new Promise<{ authorization: { code: string; state: string } }>((resolve, reject) => {
          popupResolve = resolve;
          popupReject = reject;
        })
    );
    const windowValue = {
      localStorage: local,
      sessionStorage: session,
      location: { origin: "https://auth.example.test", href: "https://auth.example.test/start" },
      AppleID: { auth: { init, signIn } }
    };
    for (const [key, value] of Object.entries({
      window: windowValue,
      localStorage: local,
      sessionStorage: session
    }))
      Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
    initiate = mock(async () => ({
      state: `fixture-state-${++stateNumber}`,
      auth_url: `https://appleid.apple.com/auth/authorize?nonce=${"aa".repeat(32)}`
    }));
    callback = mock(async () => {});
    mint = mock(async () => ({ grant: "aaa.bbb.ccc", expires_at: 42 }));
    client = {
      apiUrl: `https://apple-fixture-${++sequence}.example.test`,
      auth: { loading: false, user: { user: fixtureUser } },
      initiateAppleAuth: initiate,
      handleAppleCallback: callback,
      mintNativeHandoffGrant: mint
    } as unknown as OpenSecretContextType;
    markTransportV2DesktopOAuth(nativeTarget);
    target = readTransportV2DesktopOAuth("apple")!;
  });

  afterEach(async () => {
    await act(async () => renderer?.unmount());
    for (const [key, descriptor] of Object.entries(originals)) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor);
      else Reflect.deleteProperty(globalThis, key);
    }
  });

  const installCredentials = () => {
    const prepared = prepareNativeOAuthHandoff(client.apiUrl);
    installNativeOAuthHandoffCredentials(
      client.apiUrl,
      credentials(),
      prepared.expectedAuth,
      fixtureUser.id
    );
  };

  const renderApple = async () => {
    await act(async () => {
      renderer = create(
        <SdkProvider value={client}>
          <HostedAppleSignIn target={target} />
        </SdkProvider>
      );
    });
  };

  const button = (label: string) =>
    renderer!.root.findAllByType("button").find((node) => node.children.join("") === label)!;

  test("prepares a same-origin popup with the existing Services ID, then opens synchronously on click", async () => {
    await renderApple();
    expect(initiate).toHaveBeenCalledWith("", "https://auth.example.test/auth/apple/callback");
    expect(init).toHaveBeenCalledWith({
      clientId: "cloud.opensecret.maple.services",
      scope: "name email",
      redirectURI: "https://auth.example.test/auth/apple/callback",
      state: "fixture-state-1",
      nonce: "aa".repeat(32),
      usePopup: true
    });
    expect(signIn).not.toHaveBeenCalled();
    act(() => {
      button("Sign in with Apple").props.onClick();
      expect(signIn).toHaveBeenCalledTimes(1);
    });
    expect(callback).not.toHaveBeenCalled();
    expect(mint).not.toHaveBeenCalled();
  });

  for (const error of [
    "popup_blocked_by_browser",
    "user_cancelled_authorize",
    "popup_closed_by_user"
  ]) {
    test(`${error} permits preparation of a fresh popup attempt without clearing credentials`, async () => {
      installCredentials();
      const before = [...local.values];
      await renderApple();
      act(() => button("Sign in with Apple").props.onClick());
      await act(async () => popupReject({ error }));
      expect(button("Try again")).toBeDefined();
      await act(async () => button("Try again").props.onClick());
      expect(initiate).toHaveBeenCalledTimes(2);
      expect(init.mock.calls[1][0].state).toBe("fixture-state-2");
      expect(button("Sign in with Apple").props.disabled).toBe(false);
      expect(callback).not.toHaveBeenCalled();
      expect(mint).not.toHaveBeenCalled();
      expect([...local.values]).toEqual(before);
    });
  }

  test("a mismatched popup state never completes SDK authentication", async () => {
    await renderApple();
    act(() => button("Sign in with Apple").props.onClick());
    await act(async () =>
      popupResolve({ authorization: { code: "fixture-code", state: "other" } })
    );
    expect(callback).not.toHaveBeenCalled();
    expect(button("Try again")).toBeDefined();
  });

  test("a replaced native attempt cannot accept a late popup", async () => {
    await renderApple();
    act(() => button("Sign in with Apple").props.onClick());
    markTransportV2DesktopOAuth({ ...nativeTarget, nativeRequestId: "33".repeat(16) });
    await act(async () =>
      popupResolve({
        authorization: {
          code: "fixture-code",
          state: "fixture-state-1"
        }
      })
    );
    expect(callback).not.toHaveBeenCalled();
    expect(readTransportV2DesktopOAuth("apple")?.nativeRequestId).toBe("33".repeat(16));
  });

  test("an existing account still requires confirmation and cancellation preserves credentials", async () => {
    installCredentials();
    const before = [...local.values];
    await renderApple();
    act(() => button("Sign in with Apple").props.onClick());
    await act(async () =>
      popupResolve({
        authorization: {
          code: "fixture-code",
          state: "fixture-state-1"
        }
      })
    );
    expect(callback).toHaveBeenCalledWith("fixture-code", "fixture-state-1", "");
    expect(renderer!.root.findByType(HostedNativeSignInConfirmation)).toBeDefined();
    expect(JSON.stringify(renderer!.toJSON())).toContain(fixtureUser.email);
    expect(mint).not.toHaveBeenCalled();
    act(() => button("Cancel").props.onClick());
    expect(mint).not.toHaveBeenCalled();
    expect([...local.values]).toEqual(before);
    expect(window.location.href).toBe("https://auth.example.test/start");
  });

  test("a newly signed-in account requires consent and retains credentials through mint and manual Open Maple", async () => {
    client.auth = { loading: false, user: undefined };
    callback.mockImplementation(async () => {
      installCredentials();
      client.auth = {
        loading: false,
        user: { user: fixtureUser }
      } as OpenSecretContextType["auth"];
    });
    await renderApple();
    act(() => button("Sign in with Apple").props.onClick());
    await act(async () =>
      popupResolve({
        authorization: {
          code: "fixture-code",
          state: "fixture-state-1"
        }
      })
    );
    const before = [...local.values];
    expect(mint).not.toHaveBeenCalled();
    await act(async () => button("Continue to Maple").props.onClick());
    expect(mint).toHaveBeenCalledWith(nativeTarget.nativeSessionId, nativeTarget.nativeRequestId);
    expect(window.location.href).toBe("cloud.opensecret.maple://auth?handoff_grant=aaa.bbb.ccc");
    expect(readTransportV2DesktopOAuth("apple")).toBeNull();
    window.location.href = "https://auth.example.test/start";
    act(() => button("Open Maple").props.onClick());
    expect(window.location.href).toBe("cloud.opensecret.maple://auth?handoff_grant=aaa.bbb.ccc");
    expect(mint).toHaveBeenCalledTimes(1);
    expect([...local.values]).toEqual(before);
  });

  test("confirmation expiry preserves the signed-in browser credentials", async () => {
    installCredentials();
    const before = [...local.values];
    clearDesktopOAuthTarget(target);
    markTransportV2DesktopOAuth(nativeTarget, Date.now() - TRANSPORT_V2_PENDING_TTL_MS + 20);
    const expiring = readTransportV2DesktopOAuth("apple")!;
    await act(async () => {
      renderer = create(
        <SdkProvider value={client}>
          <HostedNativeSignInConfirmation target={expiring} />
        </SdkProvider>
      );
    });
    await act(async () => new Promise((resolve) => setTimeout(resolve, 30)));
    expect(JSON.stringify(renderer!.toJSON())).toContain("expired");
    expect(mint).not.toHaveBeenCalled();
    expect(readTransportV2DesktopOAuth("apple")).toBeNull();
    expect(readNativeUserAuth(client.apiUrl).principalId).toBe(fixtureUser.id);
    expect([...local.values]).toEqual(before);
  });
});
