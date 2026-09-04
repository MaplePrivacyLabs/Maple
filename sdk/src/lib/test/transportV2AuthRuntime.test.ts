import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import {
  TransportV2AuthorityChangedError,
  clearTransportV2Credentials,
  installTransportV2Credentials,
  isTransportV2AuthSnapshotCurrent,
  readTransportV2Credentials,
  type TransportV2AuthKind
} from "../transportV2/auth";
import { TransportV2AuthRuntime } from "../transportV2/authRuntime";
import type { TransportV2Runtime, TransportV2RuntimeRequest } from "../transportV2/runtime";

const NOW = 1_900_000_000;
const AUDIENCES = {
  user: {
    access: "urn:opensecret:internal:transport-v2:user:access-token",
    refresh: "urn:opensecret:internal:transport-v2:user:refresh-token"
  },
  platform: {
    access: "urn:opensecret:internal:transport-v2:platform:access-token",
    refresh: "urn:opensecret:internal:transport-v2:platform:refresh-token"
  }
} as const;

let testId = 0;

function apiUrl(label: string): string {
  testId += 1;
  return `https://${label}-${testId}.example.test/service`;
}

function base64Url(value: string | Uint8Array): string {
  return Buffer.from(value).toString("base64url");
}

function token(
  kind: TransportV2AuthKind,
  purpose: "access" | "refresh",
  principalId: string,
  expiresAt: number,
  marker: number
): string {
  return [
    base64Url(JSON.stringify({ alg: "ES256K", typ: "JWT" })),
    base64Url(
      JSON.stringify({
        aud: AUDIENCES[kind][purpose],
        sub: principalId,
        exp: expiresAt,
        tf: 2,
        marker
      })
    ),
    base64Url(new Uint8Array(64).fill(marker))
  ].join(".");
}

function credentialPair(
  kind: TransportV2AuthKind,
  principalId: string,
  accessExpiry: number,
  marker: number
) {
  return {
    access: token(kind, "access", principalId, accessExpiry, marker),
    refresh: token(kind, "refresh", principalId, NOW + 86_400, marker)
  };
}

function runtimeWith(implementation: (input: TransportV2RuntimeRequest) => Promise<Response>): {
  runtime: TransportV2Runtime;
  request: ReturnType<typeof mock>;
} {
  const request = mock(async (input: TransportV2RuntimeRequest) => ({
    response: await implementation(input),
    rememberOAuthContinuation() {}
  }));
  return { runtime: { request } as unknown as TransportV2Runtime, request };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  return {
    promise: new Promise<T>((fulfill) => {
      resolve = fulfill;
    }),
    resolve
  };
}

async function waitFor(predicate: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 20; attempt += 1) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
  throw new Error("condition did not become true");
}

beforeEach(() => {
  globalThis.localStorage.clear();
});

afterEach(() => {
  globalThis.localStorage.clear();
});

describe("Transport V2 authentication runtime", () => {
  test("refreshes before send and coalesces concurrent callers by credential revision", async () => {
    const api = apiUrl("coalesced");
    const initial = credentialPair("user", "user-1", NOW + 20, 1);
    const refreshed = credentialPair("user", "user-1", NOW + 3_600, 2);
    const response = deferred<Response>();
    const harness = runtimeWith(async () => response.promise);
    const auth = new TransportV2AuthRuntime({
      runtime: harness.runtime,
      nowUnixSeconds: () => NOW
    });
    installTransportV2Credentials(api, "user", initial.access, initial.refresh);

    let originalSends = 0;
    const operation = async () => {
      const authority = await auth.authority(api, { remoteAttestation: false }, "user");
      originalSends += 1;
      return authority;
    };
    const first = operation();
    const second = operation();
    expect(harness.request).toHaveBeenCalledTimes(1);
    response.resolve(
      Response.json({ access_token: refreshed.access, refresh_token: refreshed.refresh })
    );

    const [firstAuthority, secondAuthority] = await Promise.all([first, second]);
    expect(firstAuthority.credential).toEqual({ kind: "bearer", value: refreshed.access });
    expect(secondAuthority.credential).toEqual({ kind: "bearer", value: refreshed.access });
    expect(firstAuthority.credentials.revision).toBe(2);
    expect(secondAuthority.credentials.revision).toBe(2);
    expect(firstAuthority.snapshot.revision).toBe(2);
    expect(secondAuthority.snapshot.revision).toBe(2);
    expect(isTransportV2AuthSnapshotCurrent(firstAuthority.snapshot)).toBe(true);
    expect(originalSends).toBe(2);
    expect(harness.request).toHaveBeenCalledTimes(1);
    expect(harness.request.mock.calls[0][0].request).toEqual({
      method: "POST",
      target: "/refresh",
      credential: { kind: "resumption", value: initial.refresh }
    });
  });

  test("does not coalesce refreshes across distinct attestation policies", async () => {
    const api = apiUrl("policy-scope");
    const initial = credentialPair("user", "user-1", NOW + 5, 11);
    const firstRefresh = credentialPair("user", "user-1", NOW + 3_600, 12);
    const secondRefresh = credentialPair("user", "user-1", NOW + 7_200, 13);
    const firstResponse = deferred<Response>();
    const secondResponse = deferred<Response>();
    const harness = runtimeWith(async () =>
      harness.request.mock.calls.length === 1 ? firstResponse.promise : secondResponse.promise
    );
    const auth = new TransportV2AuthRuntime({
      runtime: harness.runtime,
      nowUnixSeconds: () => NOW
    });
    installTransportV2Credentials(api, "user", initial.access, initial.refresh);

    const first = auth.authority(
      api,
      { environment: "development", remoteAttestation: false },
      "user"
    );
    const second = auth.authority(
      api,
      { environment: "production", remoteAttestation: false },
      "user"
    );
    expect(harness.request).toHaveBeenCalledTimes(2);
    expect(harness.request.mock.calls[0][0].pcrConfig).toMatchObject({
      environment: "development"
    });
    expect(harness.request.mock.calls[1][0].pcrConfig).toMatchObject({
      environment: "production"
    });

    firstResponse.resolve(
      Response.json({ access_token: firstRefresh.access, refresh_token: firstRefresh.refresh })
    );
    await expect(first).resolves.toMatchObject({
      credential: { kind: "bearer", value: firstRefresh.access }
    });
    secondResponse.resolve(
      Response.json({ access_token: secondRefresh.access, refresh_token: secondRefresh.refresh })
    );
    await expect(second).rejects.toBeInstanceOf(TransportV2AuthorityChangedError);
  });

  test("does not send the original operation or clear credentials after transient refresh failure", async () => {
    const api = apiUrl("transient");
    const initial = credentialPair("user", "user-1", NOW + 10, 3);
    const harness = runtimeWith(
      async () => new Response("temporarily unavailable", { status: 503 })
    );
    const auth = new TransportV2AuthRuntime({
      runtime: harness.runtime,
      nowUnixSeconds: () => NOW
    });
    const installed = installTransportV2Credentials(api, "user", initial.access, initial.refresh);
    let originalSends = 0;

    const operation = async () => {
      await auth.authority(api, undefined, "user");
      originalSends += 1;
    };
    await expect(operation()).rejects.toThrow("temporarily unavailable");
    expect(originalSends).toBe(0);
    expect(harness.request).toHaveBeenCalledTimes(1);
    expect(harness.request.mock.calls[0][0].request.target).toBe("/refresh");
    expect(readTransportV2Credentials(api, "user")).toEqual(installed);
  });

  test("clears a current revision on definitive rejection but never clears its replacement", async () => {
    const currentApi = apiUrl("definitive-current");
    const current = credentialPair("platform", "platform-1", NOW + 5, 4);
    const currentHarness = runtimeWith(async () => new Response("denied", { status: 403 }));
    const currentAuth = new TransportV2AuthRuntime({
      runtime: currentHarness.runtime,
      nowUnixSeconds: () => NOW
    });
    installTransportV2Credentials(currentApi, "platform", current.access, current.refresh);
    await expect(currentAuth.authority(currentApi, undefined, "platform")).rejects.toThrow(
      "denied"
    );
    expect(readTransportV2Credentials(currentApi, "platform")).toBeNull();

    const racedApi = apiUrl("definitive-race");
    const stale = credentialPair("user", "user-old", NOW + 5, 5);
    const replacement = credentialPair("user", "user-new", NOW + 3_600, 6);
    const response = deferred<Response>();
    const racedHarness = runtimeWith(async () => response.promise);
    const racedAuth = new TransportV2AuthRuntime({
      runtime: racedHarness.runtime,
      nowUnixSeconds: () => NOW
    });
    installTransportV2Credentials(racedApi, "user", stale.access, stale.refresh);
    const pending = racedAuth.authority(racedApi, undefined, "user");
    const replacementState = installTransportV2Credentials(
      racedApi,
      "user",
      replacement.access,
      replacement.refresh
    );
    response.resolve(new Response("expired", { status: 401 }));
    await expect(pending).rejects.toThrow("expired");
    expect(readTransportV2Credentials(racedApi, "user")).toEqual(replacementState);
  });

  test("does not send a stale refresh after the authority changes during session setup", async () => {
    const api = apiUrl("refresh-send-fence");
    const stale = credentialPair("user", "user-old", NOW + 5, 9);
    const replacement = credentialPair("user", "user-new", NOW + 3_600, 10);
    let replacementState: ReturnType<typeof installTransportV2Credentials> | undefined;
    const harness = runtimeWith(async (input) => {
      replacementState = installTransportV2Credentials(
        api,
        "user",
        replacement.access,
        replacement.refresh
      );
      input.beforeSend?.();
      throw new Error("refresh transport must not be sent");
    });
    const auth = new TransportV2AuthRuntime({
      runtime: harness.runtime,
      nowUnixSeconds: () => NOW
    });
    installTransportV2Credentials(api, "user", stale.access, stale.refresh);

    await expect(auth.authority(api, undefined, "user")).rejects.toThrow(
      "authentication state changed"
    );
    expect(readTransportV2Credentials(api, "user")).toEqual(replacementState);
    expect(harness.request).toHaveBeenCalledTimes(1);
  });

  test("keeps an authenticated expiry response and refreshes only for a later request", async () => {
    const api = apiUrl("post-response");
    const initial = credentialPair("user", "user-1", NOW + 3_600, 7);
    const refreshed = credentialPair("user", "user-1", NOW + 7_200, 8);
    const response = deferred<Response>();
    const harness = runtimeWith(async () => response.promise);
    const auth = new TransportV2AuthRuntime({
      runtime: harness.runtime,
      nowUnixSeconds: () => NOW
    });
    installTransportV2Credentials(api, "user", initial.access, initial.refresh);
    const sent = await auth.authority(api, undefined, "user");
    expect(harness.request).not.toHaveBeenCalled();

    const original = new Response("the original operation expired", {
      status: 401,
      headers: {
        "x-opensecret-error-contract": "1",
        "x-opensecret-error-code": "access_token_expired"
      }
    });
    auth.noteResponse(original, api, undefined, "user", sent);
    auth.noteResponse(original.clone(), api, undefined, "user", sent);
    expect(harness.request).toHaveBeenCalledTimes(1);
    expect(harness.request.mock.calls[0][0].request.target).toBe("/refresh");
    response.resolve(
      Response.json({ access_token: refreshed.access, refresh_token: refreshed.refresh })
    );
    await waitFor(() => readTransportV2Credentials(api, "user")?.accessToken === refreshed.access);

    expect(original.status).toBe(401);
    expect(await original.text()).toBe("the original operation expired");
    expect(harness.request).toHaveBeenCalledTimes(1);
  });
});
