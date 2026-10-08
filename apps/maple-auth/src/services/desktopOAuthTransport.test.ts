import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  TRANSPORT_V2_PENDING_TTL_MS,
  buildTransportV2NativeAuthDeepLink,
  claimTransportV2DesktopOAuthInitiation,
  clearDesktopOAuthTarget,
  isNativeOAuthRedirect,
  isCurrentDesktopOAuthTarget,
  type NativeOAuthInput,
  markTransportV2DesktopOAuth,
  mintTransportV2NativeAuthReturn,
  readTransportV2DesktopOAuth,
  type TransportV2DesktopOAuthState
} from "./desktopOAuthTransport";

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

let originalEnvironment: string | undefined;
let originalLocalStorage: PropertyDescriptor | undefined;
let originalSessionStorage: PropertyDescriptor | undefined;

beforeEach(() => {
  originalEnvironment = process.env.VITE_AUTH_ENVIRONMENT;
  originalLocalStorage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  originalSessionStorage = Object.getOwnPropertyDescriptor(globalThis, "sessionStorage");
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: new MemoryStorage(),
    writable: true
  });
  Object.defineProperty(globalThis, "sessionStorage", {
    configurable: true,
    value: new MemoryStorage(),
    writable: true
  });
});

afterEach(() => {
  if (originalEnvironment === undefined) delete process.env.VITE_AUTH_ENVIRONMENT;
  else process.env.VITE_AUTH_ENVIRONMENT = originalEnvironment;
  if (originalLocalStorage) {
    Object.defineProperty(globalThis, "localStorage", originalLocalStorage);
  } else {
    Reflect.deleteProperty(globalThis, "localStorage");
  }
  if (originalSessionStorage) {
    Object.defineProperty(globalThis, "sessionStorage", originalSessionStorage);
  } else {
    Reflect.deleteProperty(globalThis, "sessionStorage");
  }
});

for (const environment of ["production", "development"] as const) {
  describe(`${environment} hosted V2 handoff state`, () => {
    beforeEach(() => {
      process.env.VITE_AUTH_ENVIRONMENT = environment;
    });
    const variant = environment === "development" ? { nativeAppVariant: "dev" as const } : {};
    const nativeScheme =
      environment === "development" ? "cloud.opensecret.maple.dev:" : "cloud.opensecret.maple:";
    const nativeSessionId = "00112233445566778899aabbccddeeff";
    const nativeRequestId = "ffeeddccbbaa99887766554433221100";
    const state = { provider: "github" as const, nativeSessionId, nativeRequestId, ...variant };

    test("stores the exact provider and target pair in same-tab state", () => {
      markTransportV2DesktopOAuth(state, 1_000);

      expect(isNativeOAuthRedirect()).toBe(true);
      expect(readTransportV2DesktopOAuth("github", 1_001)).toEqual({
        ...state,
        startedAt: 1_000
      });
      expect(readTransportV2DesktopOAuth("google", 1_001)).toBeNull();
    });

    test("expires hosted handoff state", () => {
      markTransportV2DesktopOAuth(state, 1_000);

      expect(
        readTransportV2DesktopOAuth("github", 1_000 + TRANSPORT_V2_PENDING_TTL_MS + 1)
      ).toBeNull();
    });

    test("claims provider initiation once without resetting on a StrictMode remount", () => {
      markTransportV2DesktopOAuth(state, 1_000);
      expect(claimTransportV2DesktopOAuthInitiation(state, 1_001)).toBe(true);

      markTransportV2DesktopOAuth(state, 1_002);
      expect(claimTransportV2DesktopOAuthInitiation(state, 1_003)).toBe(false);

      const replacement = { ...state, nativeRequestId: "11112222333344445555666677778888" };
      markTransportV2DesktopOAuth(replacement, 1_004);
      expect(claimTransportV2DesktopOAuthInitiation(replacement, 1_005)).toBe(true);
    });

    test("cannot claim initiation for another target pair", () => {
      markTransportV2DesktopOAuth(state, 1_000);
      expect(() =>
        claimTransportV2DesktopOAuthInitiation(
          { ...state, nativeRequestId: "11112222333344445555666677778888" },
          1_001
        )
      ).toThrow("state changed");
    });

    test("builds a deep link containing only the one-use grant", () => {
      const deepLink = buildTransportV2NativeAuthDeepLink(
        "head.payload.c2ln",
        state.nativeAppVariant
      );
      const parsed = new URL(deepLink);

      expect(parsed.protocol).toBe(nativeScheme);
      expect(deepLink).toBe(`${nativeScheme}//auth?handoff_grant=head.payload.c2ln`);
      expect(parsed.hostname).toBe("auth");
      expect([...parsed.searchParams.keys()]).toEqual(["handoff_grant"]);
      expect(parsed.searchParams.get("handoff_grant")).toBe("head.payload.c2ln");
      expect(parsed.searchParams.has("native_session_id")).toBe(false);
      expect(parsed.searchParams.has("native_request_id")).toBe(false);
      expect(parsed.searchParams.has("access_token")).toBe(false);
      expect(parsed.searchParams.has("refresh_token")).toBe(false);
    });

    test("mints a grant for the exact stored pair and consumes hosted state", async () => {
      markTransportV2DesktopOAuth(state, 1_000);
      const calls: string[][] = [];

      const deepLink = await mintTransportV2NativeAuthReturn(
        { ...state, startedAt: 1_000 },
        async (sessionId, requestId) => {
          calls.push([sessionId, requestId]);
          return { grant: "head.payload.c2ln" };
        },
        () => true,
        () => 1_001
      );

      expect(calls).toEqual([[nativeSessionId, nativeRequestId]]);
      expect(new URL(deepLink.url).search).toBe("?handoff_grant=head.payload.c2ln");
      expect(readTransportV2DesktopOAuth("github", 1_002)).toBeNull();
      expect(isNativeOAuthRedirect()).toBe(false);
    });

    test("does not mint for a different provider", async () => {
      markTransportV2DesktopOAuth(state, 1_000);
      let calls = 0;

      await expect(
        mintTransportV2NativeAuthReturn(
          { ...state, provider: "google", startedAt: 1_000 },
          async () => {
            calls += 1;
            return { grant: "head.payload.c2ln" };
          },
          () => true,
          () => 1_001
        )
      ).rejects.toThrow("changed or expired");
      expect(calls).toBe(0);
    });

    test("rejects duplicate approval while a mint is in flight", async () => {
      markTransportV2DesktopOAuth(state, 1_000);
      const target = { ...state, startedAt: 1_000 };
      let resolve!: (result: { grant: string }) => void;
      let calls = 0;
      const mint = () => {
        calls += 1;
        return new Promise<{ grant: string }>((done) => {
          resolve = done;
        });
      };
      const first = mintTransportV2NativeAuthReturn(
        target,
        mint,
        () => true,
        () => 1_001
      );
      await expect(
        mintTransportV2NativeAuthReturn(
          target,
          mint,
          () => true,
          () => 1_001
        )
      ).rejects.toThrow("already been submitted");
      resolve({ grant: "head.payload.c2ln" });
      await first;
      expect(calls).toBe(1);
    });

    for (const change of ["cancel", "target", "account", "expiry"] as const) {
      test(`discards a late mint after ${change} and preserves a newer target`, async () => {
        markTransportV2DesktopOAuth(state, 1_000);
        const target = { ...state, startedAt: 1_000 };
        let resolve!: (result: { grant: string }) => void;
        let ownsAccount = true;
        let now = 1_001;
        const pending = mintTransportV2NativeAuthReturn(
          target,
          () =>
            new Promise<{ grant: string }>((done) => {
              resolve = done;
            }),
          () => ownsAccount,
          () => now
        );
        const replacement = { ...state, nativeRequestId: "11".repeat(16) };
        if (change === "cancel") clearDesktopOAuthTarget(target);
        if (change === "target") markTransportV2DesktopOAuth(replacement, 1_002);
        if (change === "account") ownsAccount = false;
        if (change === "expiry") now += TRANSPORT_V2_PENDING_TTL_MS;
        if (change === "target") now = 1_003;
        resolve({ grant: "head.payload.c2ln" });
        await expect(pending).rejects.toThrow("changed or expired");
        if (change === "target") {
          expect(readTransportV2DesktopOAuth(undefined, 1_003)).toEqual({
            ...replacement,
            startedAt: 1_002
          });
        }
      });
    }

    test("does not retry after an ambiguous mint failure", async () => {
      markTransportV2DesktopOAuth(state, 1_000);
      const target = { ...state, startedAt: 1_000 };
      let calls = 0;
      const mint = async () => {
        calls += 1;
        throw new Error("network lost");
      };
      await expect(
        mintTransportV2NativeAuthReturn(
          target,
          mint,
          () => true,
          () => 1_001
        )
      ).rejects.toThrow("network lost");
      await expect(
        mintTransportV2NativeAuthReturn(
          target,
          mint,
          () => true,
          () => 1_002
        )
      ).rejects.toThrow("changed or expired");
      expect(calls).toBe(1);
    });

    test("rejects malformed or padded handoff grants", () => {
      expect(() =>
        buildTransportV2NativeAuthDeepLink("not-a-grant", state.nativeAppVariant)
      ).toThrow();
      expect(() =>
        buildTransportV2NativeAuthDeepLink("head.payload.signature=", state.nativeAppVariant)
      ).toThrow();
      expect(() =>
        buildTransportV2NativeAuthDeepLink(`YQ.Yg.${"a".repeat(4092)}`, state.nativeAppVariant)
      ).toThrow();
    });

    test("rejects a stored target from the other profile before minting", async () => {
      markTransportV2DesktopOAuth(state, 1_000);
      const wrongVariant = environment === "development" ? undefined : "dev";
      const target: TransportV2DesktopOAuthState = {
        ...state,
        nativeAppVariant: wrongVariant,
        startedAt: 1_000
      };
      sessionStorage.setItem("maple_desktop_oauth_pending_v2", JSON.stringify(target));
      let calls = 0;
      await expect(
        mintTransportV2NativeAuthReturn(
          target,
          async () => {
            calls += 1;
            return { grant: "head.payload.c2ln" };
          },
          () => true,
          () => 1_001
        )
      ).rejects.toThrow("changed or expired");
      expect(calls).toBe(0);
      expect(readTransportV2DesktopOAuth("github", 1_001)).toBeNull();
    });

    test("includes native identity in the pending target and initiation claim", () => {
      markTransportV2DesktopOAuth(state, 1_000);
      const wrongVariant = environment === "development" ? undefined : "dev";
      expect(() =>
        claimTransportV2DesktopOAuthInitiation({ ...state, nativeAppVariant: wrongVariant }, 1_001)
      ).toThrow("state changed");
      expect(() =>
        markTransportV2DesktopOAuth({ ...state, nativeAppVariant: wrongVariant }, 1_001)
      ).toThrow("does not match");
      expect(readTransportV2DesktopOAuth("github", 1_001)).toEqual({ ...state, startedAt: 1_000 });
    });

    const agent: NativeOAuthInput = {
      provider: "github",
      nativeSessionId,
      nativeRequestId,
      nativeApp: "agent",
      returnPort: 43123,
      returnState: "12".repeat(16),
      environment
    };

    test("persists the complete Agent target and claims it only once", () => {
      markTransportV2DesktopOAuth(agent, 1_000);
      expect(readTransportV2DesktopOAuth("github", 1_001)).toEqual({ ...agent, startedAt: 1_000 });
      expect(claimTransportV2DesktopOAuthInitiation(agent, 1_001)).toBe(true);
      markTransportV2DesktopOAuth(agent, 1_002);
      expect(claimTransportV2DesktopOAuthInitiation(agent, 1_003)).toBe(false);
      expect(readTransportV2DesktopOAuth("github", 1_003)?.startedAt).toBe(1_000);
    });

    test("returns only the fixed Agent URL and caps it at issuer expiry", async () => {
      markTransportV2DesktopOAuth(agent, 1_000);
      const target = readTransportV2DesktopOAuth("github", 1_001)!;
      localStorage.setItem("fixture-credentials", "retained");
      const calls: string[][] = [];
      const result = await mintTransportV2NativeAuthReturn(
        target,
        async (...pair) => {
          calls.push(pair);
          return { grant: "head.payload.c2ln", expires_at: 61 };
        },
        () => true,
        () => 1_001
      );
      expect(result).toEqual({
        url:
          "http://127.0.0.1:43123/auth/callback?handoff_grant=head.payload.c2ln&return_state=" +
          "12".repeat(16),
        expiresAt: 61_000
      });
      expect(calls).toEqual([[nativeSessionId, nativeRequestId]]);
      expect(readTransportV2DesktopOAuth("github", 1_002)).toBeNull();
      expect(localStorage.getItem("fixture-credentials")).toBe("retained");
    });

    test("keeps an explicit port throughout the accepted range", async () => {
      for (const returnPort of [1, 80, 65535]) {
        const input = { ...agent, returnPort };
        markTransportV2DesktopOAuth(input, 1_000);
        const result = await mintTransportV2NativeAuthReturn(
          { ...input, startedAt: 1_000 },
          async () => ({
            grant: "head.payload.c2ln",
            expires_at: 61
          }),
          () => true,
          () => 1_001
        );
        expect(result.url).toBe(
          `http://127.0.0.1:${returnPort}/auth/callback?handoff_grant=head.payload.c2ln&return_state=${agent.returnState}`
        );
      }
    });

    test("caps Agent return at the pending attempt's deadline", async () => {
      markTransportV2DesktopOAuth(agent, 1_000);
      const result = await mintTransportV2NativeAuthReturn(
        { ...agent, startedAt: 1_000 },
        async () => ({
          grant: "head.payload.c2ln",
          expires_at: 9_000
        }),
        () => true,
        () => 1_001
      );
      expect(result.expiresAt).toBe(1_000 + TRANSPORT_V2_PENDING_TTL_MS);
    });

    for (const replacement of [
      { ...agent, returnPort: 43124 },
      { ...agent, returnState: "34".repeat(16) },
      state
    ] as NativeOAuthInput[]) {
      test(`rejects a late mint after replacing Agent with ${replacement.nativeApp === "agent" ? replacement.returnPort + ":" + replacement.returnState : "Research"}`, async () => {
        markTransportV2DesktopOAuth(agent, 1_000);
        const target = readTransportV2DesktopOAuth("github", 1_001)!;
        let resolve!: (value: { grant: string; expires_at: number }) => void;
        const mint = mintTransportV2NativeAuthReturn(
          target,
          () =>
            new Promise((done) => {
              resolve = done;
            }),
          () => true,
          () => 1_003
        );
        markTransportV2DesktopOAuth(replacement, 1_002);
        expect(isCurrentDesktopOAuthTarget(target, 1_003)).toBe(false);
        expect(() => claimTransportV2DesktopOAuthInitiation(agent, 1_003)).toThrow("state changed");
        clearDesktopOAuthTarget(target);
        resolve({ grant: "head.payload.c2ln", expires_at: 61 });
        await expect(mint).rejects.toThrow("changed or expired");
        expect(readTransportV2DesktopOAuth("github", 1_004)).toEqual({
          ...replacement,
          startedAt: 1_002
        });
      });
    }

    test("rejects malformed, mixed-app and wrong-environment stored Agent targets", () => {
      for (const alteration of [
        { returnPort: 0 },
        { returnPort: 65536 },
        { returnPort: "43123" },
        { returnPort: 1.5 },
        { returnState: "AA".repeat(16) },
        { returnState: "12" },
        { nativeApp: "unknown" },
        { nativeApp: undefined },
        { nativeAppVariant: "dev" },
        { environment: environment === "development" ? "production" : "development" }
      ]) {
        sessionStorage.setItem(
          "maple_desktop_oauth_pending_v2",
          JSON.stringify({ ...agent, startedAt: 1_000, ...alteration })
        );
        expect(readTransportV2DesktopOAuth("github", 1_001)).toBeNull();
      }
    });

    test("does not expose an Agent return without a live safe issuer expiry", async () => {
      for (const expiry of [
        undefined,
        0,
        1,
        1.5,
        Number.NaN,
        Number.POSITIVE_INFINITY,
        Number.MAX_SAFE_INTEGER
      ]) {
        markTransportV2DesktopOAuth(agent, 1_000);
        let calls = 0;
        await expect(
          mintTransportV2NativeAuthReturn(
            { ...agent, startedAt: 1_000 },
            async () => {
              calls += 1;
              return { grant: "head.payload.c2ln", expires_at: expiry };
            },
            () => true,
            () => 1_001
          )
        ).rejects.toThrow();
        expect(calls).toBe(1);
        expect(readTransportV2DesktopOAuth("github", 1_002)).toBeNull();
      }
    });
  });
}
