import { authEnvironment } from "@/config/authEnvironment";
import { afterEach, beforeEach, describe, expect, mock, spyOn, test } from "bun:test";
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
  markTransportV2DesktopOAuth,
  readTransportV2DesktopOAuth,
  TRANSPORT_V2_PENDING_TTL_MS,
  type DesktopOAuthProvider,
  type TransportV2DesktopOAuthState
} from "@/services/desktopOAuthTransport";

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
const environment = authEnvironment();
const appLabel = environment === "development" ? "Maple Agent Dev" : "Maple Agent";
const startUrl = "https://auth.example.test/start";
const fixtureUser = { id: "agent-fixture-user", email: "agent-fixture@example.test" };
const nativeTarget = {
  provider: "github" as DesktopOAuthProvider,
  nativeSessionId: "11".repeat(16),
  nativeRequestId: "22".repeat(16),
  nativeApp: "agent" as const,
  environment,
  returnPort: 49231,
  returnState: "33".repeat(16)
};
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

describe("Agent loopback return with the real SDK credential fence", () => {
  let renderer: ReactTestRenderer | null;
  let client: OpenSecretContextType;
  let local: MemoryStorage;
  let target: TransportV2DesktopOAuthState;
  let mint: ReturnType<typeof mock>;
  let now: number;
  let expiresAt: number;
  let clock: ReturnType<typeof spyOn<typeof Date, "now">>;

  const installCredentials = () => {
    const prepared = prepareNativeOAuthHandoff(client.apiUrl);
    return installNativeOAuthHandoffCredentials(
      client.apiUrl,
      credentials(),
      prepared.expectedAuth,
      fixtureUser.id
    );
  };

  const advanceCredentialRevision = () => {
    // Simulate another tab's completed storage write. Keep the installed token pair
    // and principal unchanged so the real SDK read must detect the revision alone.
    const previous = readNativeUserAuth(client.apiUrl);
    const entry = [...local.values].find(([, value]) => {
      try {
        return JSON.parse(value).api_origin === client.apiUrl;
      } catch {
        return false;
      }
    });
    expect(entry).toBeDefined();
    const stored = JSON.parse(entry![1]);
    stored.user.revision += 1;
    local.setItem(entry![0], JSON.stringify(stored));
    const current = readNativeUserAuth(client.apiUrl);
    expect(current.revision).toBe(previous.revision + 1);
    expect(current.credentials).toEqual(previous.credentials);
    expect(current.principalId).toBe(previous.principalId);
    return current;
  };

  beforeEach(() => {
    renderer = null;
    now = 1_900_000_000_000;
    expiresAt = now / 1000 + 60;
    clock = spyOn(Date, "now").mockImplementation(() => now);
    local = new MemoryStorage();
    const session = new MemoryStorage();
    for (const [key, value] of Object.entries({
      window: {
        localStorage: local,
        sessionStorage: session,
        location: { origin: "https://auth.example.test", href: startUrl }
      },
      localStorage: local,
      sessionStorage: session
    }))
      Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
    mint = mock(async () => ({ grant: "aaa.bbb.ccc", expires_at: expiresAt }));
    client = {
      apiUrl: `https://agent-fixture-${++sequence}.example.test`,
      auth: { loading: false, user: { user: fixtureUser } },
      mintNativeHandoffGrant: mint
    } as unknown as OpenSecretContextType;
    installCredentials();
    markTransportV2DesktopOAuth(nativeTarget);
    target = readTransportV2DesktopOAuth("github")!;
  });

  afterEach(async () => {
    await act(async () => renderer?.unmount());
    clock.mockRestore();
    for (const [key, descriptor] of Object.entries(originals)) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor);
      else Reflect.deleteProperty(globalThis, key);
    }
  });

  const renderConfirmation = async () => {
    await act(async () => {
      renderer = create(
        <SdkProvider value={client}>
          <HostedNativeSignInConfirmation target={target} />
        </SdkProvider>
      );
    });
  };

  const button = (label: string) =>
    renderer!.root.findAllByType("button").find((node) => node.children.join("") === label);
  const continueButton = () => button(`Continue to ${appLabel}`)!;
  const returnButton = () => button(`Return to ${appLabel}`)!;
  const approve = async () => {
    await act(async () => continueButton().props.onClick());
  };
  const expectCredentialsRetained = (before: [string, string][]) => {
    expect([...local.values]).toEqual(before);
    expect(readNativeUserAuth(client.apiUrl).principalId).toBe(fixtureUser.id);
  };

  for (const provider of ["github", "google", "apple"] as const) {
    test(`${provider} identifies the Agent audience and account before minting`, async () => {
      markTransportV2DesktopOAuth({ ...nativeTarget, provider });
      target = readTransportV2DesktopOAuth(provider)!;
      await renderConfirmation();
      expect(target.provider).toBe(provider);
      expect(continueButton()).toBeDefined();
      expect(returnButton()).toBeUndefined();
      expect(JSON.stringify(renderer!.toJSON())).toContain(fixtureUser.email);
      expect(button("Continue to Maple")).toBeUndefined();
      expect(button("Open Maple")).toBeUndefined();
      expect(mint).not.toHaveBeenCalled();
      expect(window.location.href).toBe(startUrl);
    });
  }

  test("mints once and waits for a second explicit click before the fixed loopback return", async () => {
    const before = [...local.values];
    await renderConfirmation();
    const confirm = continueButton().props.onClick;
    await act(async () => {
      await Promise.all([confirm(), confirm()]);
    });
    expect(mint).toHaveBeenCalledTimes(1);
    expect(mint).toHaveBeenCalledWith(nativeTarget.nativeSessionId, nativeTarget.nativeRequestId);
    expect(window.location.href).toBe(startUrl);
    expect(returnButton()).toBeDefined();
    expect(button("Open Maple")).toBeUndefined();
    expect(readTransportV2DesktopOAuth()).toBeNull();
    act(() => returnButton().props.onClick());
    expect(window.location.href).toBe(
      `http://127.0.0.1:${nativeTarget.returnPort}/auth/callback?handoff_grant=aaa.bbb.ccc&return_state=${nativeTarget.returnState}`
    );
    expect(mint).toHaveBeenCalledTimes(1);
    expectCredentialsRetained(before);
  });

  test("issuer expiry at the exact boundary blocks the manual return even while the native attempt is fresh", async () => {
    const before = [...local.values];
    await renderConfirmation();
    await approve();
    now = expiresAt * 1000;
    act(() => returnButton().props.onClick());
    expect(window.location.href).toBe(startUrl);
    expect(returnButton()).toBeUndefined();
    expect(mint).toHaveBeenCalledTimes(1);
    expectCredentialsRetained(before);
  });

  for (const replacement of [{ returnPort: 49232 }, { returnState: "44".repeat(16) }]) {
    test(`a newer flow with changed ${Object.keys(replacement)[0]} blocks the old manual return`, async () => {
      const before = [...local.values];
      await renderConfirmation();
      await approve();
      markTransportV2DesktopOAuth({ ...nativeTarget, ...replacement });
      const newer = readTransportV2DesktopOAuth();
      act(() => returnButton().props.onClick());
      expect(window.location.href).toBe(startUrl);
      expect(returnButton()).toBeUndefined();
      expect(readTransportV2DesktopOAuth()).toEqual(newer);
      expectCredentialsRetained(before);
    });
  }

  test("a newer credential revision blocks manual return even for the same account", async () => {
    await renderConfirmation();
    await approve();
    const previous = readNativeUserAuth(client.apiUrl);
    const current = advanceCredentialRevision();
    expect(current.revision).not.toBe(previous.revision);
    const before = [...local.values];
    act(() => returnButton().props.onClick());
    expect(window.location.href).toBe(startUrl);
    expect(returnButton()).toBeUndefined();
    expectCredentialsRetained(before);
  });

  test("cancellation before confirmation mints nothing and retains browser credentials", async () => {
    const before = [...local.values];
    await renderConfirmation();
    act(() => button("Cancel")!.props.onClick());
    expect(mint).not.toHaveBeenCalled();
    expect(readTransportV2DesktopOAuth()).toBeNull();
    expect(window.location.href).toBe(startUrl);
    expect(continueButton()).toBeUndefined();
    expect(returnButton()).toBeUndefined();
    expectCredentialsRetained(before);
  });

  test("cancellation after minting discards the return without clearing browser credentials", async () => {
    const before = [...local.values];
    await renderConfirmation();
    await approve();
    act(() => button("Cancel")!.props.onClick());
    expect(window.location.href).toBe(startUrl);
    expect(returnButton()).toBeUndefined();
    expect(mint).toHaveBeenCalledTimes(1);
    expectCredentialsRetained(before);
  });

  for (const invalidation of ["cancel", "replacement", "credentials", "unmount", "expiry"]) {
    test(`${invalidation} while minting discards the late grant`, async () => {
      let resolveMint!: (value: { grant: string; expires_at: number }) => void;
      mint.mockImplementation(
        () =>
          new Promise<{ grant: string; expires_at: number }>((resolve) => {
            resolveMint = resolve;
          })
      );
      await renderConfirmation();
      let approval!: Promise<void>;
      act(() => {
        approval = continueButton().props.onClick();
      });
      expect(mint).toHaveBeenCalledTimes(1);
      expect(window.location.href).toBe(startUrl);
      let newer: TransportV2DesktopOAuthState | null = null;
      if (invalidation === "cancel") act(() => button("Cancel")!.props.onClick());
      if (invalidation === "replacement") {
        markTransportV2DesktopOAuth({ ...nativeTarget, returnState: "55".repeat(16) });
        newer = readTransportV2DesktopOAuth();
      }
      if (invalidation === "credentials") advanceCredentialRevision();
      if (invalidation === "expiry") now += TRANSPORT_V2_PENDING_TTL_MS + 1;
      if (invalidation === "unmount") {
        await act(async () => renderer!.unmount());
        renderer = null;
      }
      const before = [...local.values];
      await act(async () => {
        resolveMint({ grant: "aaa.bbb.ccc", expires_at: expiresAt });
        await approval;
      });
      expect(window.location.href).toBe(startUrl);
      expect(mint).toHaveBeenCalledTimes(1);
      if (renderer) expect(returnButton()).toBeUndefined();
      if (newer) expect(readTransportV2DesktopOAuth()).toEqual(newer);
      else expect(readTransportV2DesktopOAuth()).toBeNull();
      expectCredentialsRetained(before);
    });
  }

  test("an already expired grant never becomes a manual return action", async () => {
    expiresAt = now / 1000;
    const before = [...local.values];
    await renderConfirmation();
    await approve();
    expect(window.location.href).toBe(startUrl);
    expect(returnButton()).toBeUndefined();
    expect(readTransportV2DesktopOAuth()).toBeNull();
    expect(mint).toHaveBeenCalledTimes(1);
    expectCredentialsRetained(before);
  });
});
