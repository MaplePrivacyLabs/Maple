import { beforeEach, describe, expect, test } from "bun:test";
import {
  TRANSPORT_V2_PENDING_TTL_MS,
  buildTransportV2DesktopAuthUrl,
  buildTransportV2NativeAuthDeepLink,
  claimTransportV2DesktopOAuthInitiation,
  clearDesktopOAuthTransport,
  clearDesktopOAuthTarget,
  isNativeOAuthRedirect,
  markDesktopOAuthTransport,
  markTransportV2DesktopOAuth,
  mintTransportV2NativeAuthDeepLink,
  readDesktopOAuthTransport,
  readTransportV2DesktopOAuth,
  shouldLoadLegacyDesktopOAuth
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

function location(pathname: string, search = ""): Pick<Location, "pathname" | "search"> {
  return { pathname, search };
}

beforeEach(() => {
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

describe("desktop OAuth transport selection", () => {
  const nativeSessionId = "00112233445566778899aabbccddeeff";
  const nativeRequestId = "ffeeddccbbaa99887766554433221100";
  const state = { provider: "github" as const, nativeSessionId, nativeRequestId };

  test("builds an explicit V2 URL with only the public target pair", () => {
    const authUrl = buildTransportV2DesktopAuthUrl(state);
    const parsed = new URL(authUrl);

    expect(parsed.origin).toBe("https://trymaple.ai");
    expect(parsed.pathname).toBe("/desktop-auth");
    expect(parsed.searchParams.get("provider")).toBe("github");
    expect(parsed.searchParams.get("transport")).toBe("v2");
    expect(parsed.searchParams.get("native_session_id")).toBe(nativeSessionId);
    expect(parsed.searchParams.get("native_request_id")).toBe(nativeRequestId);
    expect(parsed.searchParams.has("native_oauth_attempt")).toBe(false);
    expect(parsed.searchParams.has("next")).toBe(false);
    expect(parsed.hash).toBe("");
  });

  test("rejects non-canonical target identifiers", () => {
    expect(() =>
      buildTransportV2DesktopAuthUrl({ ...state, nativeSessionId: nativeSessionId.toUpperCase() })
    ).toThrow("native session");
    expect(() =>
      buildTransportV2DesktopAuthUrl({ ...state, nativeRequestId: "not-a-request" })
    ).toThrow("native request");
  });

  test("stores the exact provider and target pair in same-tab state", () => {
    markTransportV2DesktopOAuth(state, 1_000);

    expect(readDesktopOAuthTransport()).toBe("v2");
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
    const deepLink = buildTransportV2NativeAuthDeepLink("head.payload.c2ln");
    const parsed = new URL(deepLink);

    expect(parsed.protocol).toBe("cloud.opensecret.maple:");
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

    const deepLink = await mintTransportV2NativeAuthDeepLink(
      { ...state, startedAt: 1_000 },
      async (sessionId, requestId) => {
        calls.push([sessionId, requestId]);
        return { grant: "head.payload.c2ln" };
      },
      () => true,
      () => 1_001
    );

    expect(calls).toEqual([[nativeSessionId, nativeRequestId]]);
    expect(new URL(deepLink).search).toBe("?handoff_grant=head.payload.c2ln");
    expect(readTransportV2DesktopOAuth("github", 1_002)).toBeNull();
    expect(isNativeOAuthRedirect()).toBe(false);
  });

  test("does not mint for a different provider", async () => {
    markTransportV2DesktopOAuth(state, 1_000);
    let calls = 0;

    await expect(
      mintTransportV2NativeAuthDeepLink(
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
    const first = mintTransportV2NativeAuthDeepLink(
      target,
      mint,
      () => true,
      () => 1_001
    );
    await expect(
      mintTransportV2NativeAuthDeepLink(
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
      const pending = mintTransportV2NativeAuthDeepLink(
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
      mintTransportV2NativeAuthDeepLink(
        target,
        mint,
        () => true,
        () => 1_001
      )
    ).rejects.toThrow("network lost");
    await expect(
      mintTransportV2NativeAuthDeepLink(
        target,
        mint,
        () => true,
        () => 1_002
      )
    ).rejects.toThrow("changed or expired");
    expect(calls).toBe(1);
  });

  test("rejects malformed or padded handoff grants", () => {
    expect(() => buildTransportV2NativeAuthDeepLink("not-a-grant")).toThrow();
    expect(() => buildTransportV2NativeAuthDeepLink("head.payload.signature=")).toThrow();
    expect(() => buildTransportV2NativeAuthDeepLink(`YQ.Yg.${"a".repeat(4092)}`)).toThrow();
  });

  test("keeps unversioned desktop-auth on the pinned V1 bridge", () => {
    expect(shouldLoadLegacyDesktopOAuth(location("/desktop-auth", "?provider=github"))).toBe(true);
  });

  test("never downgrades supplied or malformed transport selectors", () => {
    expect(
      shouldLoadLegacyDesktopOAuth(location("/desktop-auth", "?provider=github&transport=v2"))
    ).toBe(false);
    expect(
      shouldLoadLegacyDesktopOAuth(location("/desktop-auth", "?provider=github&transport=invalid"))
    ).toBe(false);
    expect(
      shouldLoadLegacyDesktopOAuth(location("/desktop-auth", "?provider=github&transport="))
    ).toBe(false);
  });

  test("preserves callbacks from an already-running released-client flow", () => {
    localStorage.setItem("redirect-to-native", "true");
    expect(shouldLoadLegacyDesktopOAuth(location("/auth/github/callback"))).toBe(true);
  });

  test("never loads V1 for marked V2 or malformed callbacks", () => {
    markDesktopOAuthTransport("v2");
    expect(shouldLoadLegacyDesktopOAuth(location("/auth/google/callback"))).toBe(false);

    localStorage.setItem("maple_desktop_oauth_transport_v1", "invalid");
    expect(shouldLoadLegacyDesktopOAuth(location("/auth/google/callback"))).toBe(false);
  });

  test("does not load the legacy bridge for ordinary web callbacks", () => {
    expect(shouldLoadLegacyDesktopOAuth(location("/auth/apple/callback"))).toBe(false);
  });

  test("clears transport and pending target state after handoff", () => {
    markTransportV2DesktopOAuth(state, 1_000);
    clearDesktopOAuthTransport();

    expect(readDesktopOAuthTransport()).toBeNull();
    expect(readTransportV2DesktopOAuth("github", 1_001)).toBeNull();
    expect(localStorage.getItem("redirect-to-native")).toBeNull();
  });
});
