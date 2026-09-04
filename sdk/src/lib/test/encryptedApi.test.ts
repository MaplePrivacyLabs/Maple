import { beforeEach, describe, expect, mock, test } from "bun:test";
import {
  authenticatedApiCallWithDependencies,
  authenticatedApiCallWithAuthorityAndDependencies,
  encryptedApiCallWithDependencies,
  openAiAuthenticatedApiCallWithDependencies,
  type EncryptedApiDependencies
} from "../encryptedApi";
import { snapshotPcrConfig, type PcrConfig } from "../pcr";
import {
  clearTransportV2CacheRoot,
  clearTransportV2Credentials,
  type StoredTransportV2Credentials
} from "../transportV2/auth";
import type { TransportV2Authority, TransportV2AuthRuntime } from "../transportV2/authRuntime";
import type {
  TransportV2Runtime,
  TransportV2RuntimeRequest,
  TransportV2RuntimeResponse
} from "../transportV2/runtime";

const appApiUrl = "https://api.example.test/gateway";
const platformApiUrl = "https://platform.example.test/gateway";

interface Harness {
  dependencies: EncryptedApiDependencies;
  authority: ReturnType<typeof mock>;
  noteResponse: ReturnType<typeof mock>;
  request: ReturnType<typeof mock>;
}

function credentials(kind: "user" | "platform", apiUrl: string): StoredTransportV2Credentials {
  return {
    kind,
    principalId: `${kind}-principal`,
    apiOrigin: new URL(apiUrl).origin,
    revision: 7,
    accessToken: `${kind}-access-token`,
    refreshToken: `${kind}-refresh-token`,
    accessExpiresAtUnixSeconds: 4_000_000_000,
    refreshExpiresAtUnixSeconds: 4_000_000_000
  };
}

function authorityFor(kind: "user" | "platform", apiUrl: string): TransportV2Authority {
  const stored = credentials(kind, apiUrl);
  return {
    credential: { kind: "bearer", value: stored.accessToken },
    credentials: stored,
    snapshot: {
      kind,
      principalId: stored.principalId,
      apiOrigin: stored.apiOrigin,
      revision: stored.revision
    },
    assertCurrent() {}
  };
}

function exchange(
  response: Response,
  rememberOAuthContinuation: TransportV2RuntimeResponse["rememberOAuthContinuation"] = () => {}
): TransportV2RuntimeResponse {
  return { response, rememberOAuthContinuation };
}

function harness(
  implementation: (input: TransportV2RuntimeRequest) => Promise<TransportV2RuntimeResponse>
): Harness {
  const request = mock(implementation);
  const authority = mock(async (apiUrl: string, _pcrConfig: unknown, kind: "user" | "platform") =>
    authorityFor(kind, apiUrl)
  );
  const noteResponse = mock(() => {});
  const runtime = { request } as unknown as TransportV2Runtime;
  const auth = { authority, noteResponse } as unknown as TransportV2AuthRuntime;
  return {
    request,
    authority,
    noteResponse,
    dependencies: {
      runtime,
      auth,
      getApiPcrConfig: () => snapshotPcrConfig({ environment: "development" }),
      getApiUrl: () => appApiUrl,
      getPlatformApiUrl: () => platformApiUrl,
      getPlatformPcrConfig: () => snapshotPcrConfig({ environment: "production" })
    }
  };
}

function copyInput(input: TransportV2RuntimeRequest): TransportV2RuntimeRequest {
  return {
    ...input,
    request: {
      ...input.request,
      headers: input.request.headers?.map((header) => ({ ...header })),
      body: input.request.body ? new Uint8Array(input.request.body) : input.request.body,
      cacheNamespaceRoot: input.request.cacheNamespaceRoot
        ? new Uint8Array(input.request.cacheNamespaceRoot)
        : undefined
    }
  };
}

function jsonResponse(value: unknown, init?: ResponseInit): Response {
  return new Response(JSON.stringify(value), {
    ...init,
    headers: { "content-type": "application/json", ...init?.headers }
  });
}

describe("simplified Transport V2 encrypted API seam", () => {
  beforeEach(() => {
    globalThis.localStorage?.clear();
    globalThis.sessionStorage?.clear();
    clearTransportV2Credentials(appApiUrl);
    clearTransportV2CacheRoot(appApiUrl);
    clearTransportV2Credentials(platformApiUrl);
    clearTransportV2CacheRoot(platformApiUrl);
  });

  test("puts the credential, query, body presence, and JSON bytes in the logical request", async () => {
    const seen: TransportV2RuntimeRequest[] = [];
    const testHarness = harness(async (input) => {
      seen.push(copyInput(input));
      return exchange(jsonResponse({ ok: true }));
    });

    await encryptedApiCallWithDependencies<undefined, { ok: boolean }>(
      `${appApiUrl}/protected/user?include=profile`,
      "get",
      undefined,
      "explicit-bearer",
      undefined,
      testHarness.dependencies
    );
    await encryptedApiCallWithDependencies<{ enabled: boolean }, { ok: boolean }>(
      `${appApiUrl}/protected/settings`,
      "patch",
      { enabled: true },
      "explicit-bearer",
      undefined,
      testHarness.dependencies
    );

    expect(seen[0].request).toEqual({
      credential: { kind: "bearer", value: "explicit-bearer" },
      cacheNamespaceRoot: undefined,
      method: "GET",
      target: "/protected/user?include=profile",
      headers: undefined,
      body: undefined
    });
    expect(seen[1].request).toMatchObject({
      credential: { kind: "bearer", value: "explicit-bearer" },
      method: "PATCH",
      target: "/protected/settings",
      headers: [{ name: "content-type", value: "application/json" }]
    });
    expect(new TextDecoder().decode(seen[1].request.body)).toBe('{"enabled":true}');
    expect(testHarness.request).toHaveBeenCalledTimes(2);
    expect(testHarness.authority).toHaveBeenCalledTimes(0);
  });

  test("selects independent user and platform authority and attested endpoints", async () => {
    const seen: TransportV2RuntimeRequest[] = [];
    const testHarness = harness(async (input) => {
      seen.push(copyInput(input));
      return exchange(jsonResponse({ ok: true }));
    });

    await authenticatedApiCallWithDependencies<undefined, { ok: boolean }>(
      `${appApiUrl}/protected/user`,
      "GET",
      undefined,
      undefined,
      testHarness.dependencies
    );
    await authenticatedApiCallWithDependencies<undefined, { ok: boolean }>(
      `${platformApiUrl}/platform/organizations`,
      "GET",
      undefined,
      undefined,
      testHarness.dependencies
    );

    expect(testHarness.authority).toHaveBeenNthCalledWith(
      1,
      appApiUrl,
      expect.objectContaining({ environment: "development" }),
      "user"
    );
    expect(testHarness.authority).toHaveBeenNthCalledWith(
      2,
      platformApiUrl,
      expect.objectContaining({ environment: "production" }),
      "platform"
    );
    expect(seen.map(({ apiUrl }) => apiUrl)).toEqual([appApiUrl, platformApiUrl]);
    expect(seen.map(({ request }) => request.credential)).toEqual([
      { kind: "bearer", value: "user-access-token" },
      { kind: "bearer", value: "platform-access-token" }
    ]);
    expect(seen.map(({ request }) => request.target)).toEqual([
      "/protected/user",
      "/platform/organizations"
    ]);
    expect(testHarness.noteResponse).toHaveBeenCalledTimes(2);
  });

  test("pins endpoint and PCR policy across an asynchronous authority lookup", async () => {
    const seen: TransportV2RuntimeRequest[] = [];
    const testHarness = harness(async (input) => {
      seen.push(copyInput(input));
      return exchange(jsonResponse({ ok: true }));
    });
    let configuredApiUrl = appApiUrl;
    let configuredPcr: PcrConfig = {
      environment: "development",
      remoteAttestation: false,
      pcr0DevValues: ["11".repeat(48)]
    };
    testHarness.dependencies.getApiUrl = () => configuredApiUrl;
    testHarness.dependencies.getApiPcrConfig = () => configuredPcr;
    testHarness.authority.mockImplementation(
      async (authorityApiUrl: string, authorityPcr: PcrConfig, kind: "user" | "platform") => {
        expect(authorityApiUrl).toBe(appApiUrl);
        expect(authorityPcr).toMatchObject({
          environment: "development",
          remoteAttestation: false,
          pcr0DevValues: ["11".repeat(48)]
        });
        configuredApiUrl = "https://replacement.example.test/gateway";
        configuredPcr = {
          environment: "production",
          remoteAttestation: true,
          pcr0Values: ["22".repeat(48)]
        };
        return authorityFor(kind, authorityApiUrl);
      }
    );

    await authenticatedApiCallWithDependencies<undefined, { ok: boolean }>(
      `${appApiUrl}/protected/user`,
      "GET",
      undefined,
      undefined,
      testHarness.dependencies
    );

    expect(seen).toHaveLength(1);
    expect(seen[0].apiUrl).toBe(appApiUrl);
    expect(seen[0].pcrConfig).toMatchObject({
      environment: "development",
      remoteAttestation: false,
      pcr0DevValues: ["11".repeat(48)]
    });
    expect(seen[0].request.target).toBe("/protected/user");
    expect(seen[0].request.credential).toEqual({
      kind: "bearer",
      value: "user-access-token"
    });
  });

  test("returns the exact authority used for the authenticated send and final fence", async () => {
    let sent: TransportV2RuntimeRequest | undefined;
    const testHarness = harness(async (input) => {
      sent = input;
      input.beforeSend?.();
      return exchange(jsonResponse({ ok: true }));
    });
    const result = await authenticatedApiCallWithAuthorityAndDependencies<
      undefined,
      { ok: boolean }
    >(
      `${appApiUrl}/protected/user`,
      "GET",
      undefined,
      undefined,
      {
        apiUrl: appApiUrl,
        pcrConfig: { environment: "development", remoteAttestation: false },
        kind: "user"
      },
      testHarness.dependencies
    );

    expect(result.data).toEqual({ ok: true });
    expect(result.authority.snapshot).toMatchObject({
      kind: "user",
      apiOrigin: "https://api.example.test",
      revision: 7
    });
    expect(result.apiUrl).toBe(appApiUrl);
    expect(result.pcrConfig).toMatchObject({ environment: "development" });
    expect(testHarness.authority).toHaveBeenCalledTimes(1);
    expect(sent?.request.credential).toBe(result.authority.credential);
    expect(sent?.beforeSend).toBeDefined();
  });

  test("uses an explicit API key and one stable cache root for API-key and user inference", async () => {
    const seen: TransportV2RuntimeRequest[] = [];
    const testHarness = harness(async (input) => {
      seen.push(copyInput(input));
      return exchange(jsonResponse({ object: "list", data: [] }));
    });

    await openAiAuthenticatedApiCallWithDependencies<undefined, unknown>(
      `${appApiUrl}/v1/models`,
      "GET",
      undefined,
      undefined,
      "api-key",
      testHarness.dependencies
    );
    await openAiAuthenticatedApiCallWithDependencies<undefined, unknown>(
      `${appApiUrl}/v1/models`,
      "GET",
      undefined,
      undefined,
      undefined,
      testHarness.dependencies
    );

    expect(seen.map(({ request }) => request.credential)).toEqual([
      { kind: "api_key", value: "api-key" },
      { kind: "bearer", value: "user-access-token" }
    ]);
    expect(seen[0].request.cacheNamespaceRoot).toHaveLength(32);
    expect(seen[1].request.cacheNamespaceRoot).toEqual(seen[0].request.cacheNamespaceRoot);
    expect(testHarness.authority).toHaveBeenCalledTimes(1);

    await expect(
      openAiAuthenticatedApiCallWithDependencies<undefined, unknown>(
        `${appApiUrl}/v1/models`,
        "GET",
        undefined,
        undefined,
        "",
        testHarness.dependencies
      )
    ).rejects.toThrow("API key cannot be empty");
    expect(testHarness.request).toHaveBeenCalledTimes(2);
  });

  test("keeps account-establishment routes anonymous for user and platform APIs", async () => {
    const seen: TransportV2RuntimeRequest[] = [];
    const testHarness = harness(async (input) => {
      seen.push(copyInput(input));
      return exchange(jsonResponse({ accepted: true }));
    });

    await encryptedApiCallWithDependencies<{ email: string }, { accepted: boolean }>(
      `${appApiUrl}/login`,
      "POST",
      { email: "person@example.test" },
      undefined,
      undefined,
      testHarness.dependencies
    );
    await encryptedApiCallWithDependencies<{ code: string }, { accepted: boolean }>(
      `${platformApiUrl}/platform/password-reset/confirm`,
      "POST",
      { code: "reset-code" },
      undefined,
      undefined,
      testHarness.dependencies
    );

    expect(seen.map(({ request }) => request.credential)).toEqual([undefined, undefined]);
    expect(seen.map(({ apiUrl }) => apiUrl)).toEqual([appApiUrl, platformApiUrl]);
    expect(testHarness.authority).toHaveBeenCalledTimes(0);
  });

  for (const provider of ["github", "google"] as const) {
    test(`${provider} initiation preserves public csrf_token shape and exact continuation state`, async () => {
      const remember = mock(() => {});
      const state = `${provider}-state`;
      const testHarness = harness(async () =>
        exchange(jsonResponse({ auth_url: `https://${provider}.example/auth`, state }), remember)
      );

      const value = await encryptedApiCallWithDependencies<
        { client_id: string },
        { auth_url: string; csrf_token: string }
      >(
        `${appApiUrl}/auth/${provider}`,
        "POST",
        { client_id: "client-id" },
        undefined,
        undefined,
        testHarness.dependencies
      );

      expect(value).toEqual({
        auth_url: `https://${provider}.example/auth`,
        csrf_token: state
      });
      expect(remember).toHaveBeenCalledWith(provider, state);
      expect(testHarness.authority).toHaveBeenCalledTimes(0);
    });
  }

  test("binds an OAuth callback to the exact provider and state without re-saving it", async () => {
    const remember = mock(() => {});
    let seen: TransportV2RuntimeRequest | undefined;
    const testHarness = harness(async (input) => {
      seen = copyInput(input);
      return exchange(jsonResponse({ access_token: "access", refresh_token: "refresh" }), remember);
    });

    const value = await encryptedApiCallWithDependencies<
      { code: string; state: string },
      { access_token: string; refresh_token: string }
    >(
      `${appApiUrl}/auth/github/callback`,
      "POST",
      { code: "provider-code", state: "exact-state" },
      undefined,
      undefined,
      testHarness.dependencies
    );

    expect(value).toEqual({ access_token: "access", refresh_token: "refresh" });
    expect(seen?.oauthCallback).toEqual({ provider: "github", state: "exact-state" });
    expect(seen?.request.credential).toBeUndefined();
    expect(remember).toHaveBeenCalledTimes(0);
  });

  test("returns the typed completed Responses object from authenticated SSE", async () => {
    const completed = { id: "resp_123", status: "completed", output: [] };
    const sse = [
      'event: response.created\ndata: {"type":"response.created"}',
      `event: response.completed\ndata: ${JSON.stringify({
        type: "response.completed",
        response: completed
      })}`,
      "data: [DONE]",
      ""
    ].join("\n\n");
    const testHarness = harness(async () =>
      exchange(new Response(sse, { headers: { "content-type": "text/event-stream" } }))
    );

    const value = await openAiAuthenticatedApiCallWithDependencies<
      { model: string },
      typeof completed
    >(
      `${appApiUrl}/v1/responses`,
      "POST",
      { model: "model" },
      undefined,
      "api-key",
      testHarness.dependencies
    );

    expect(value).toEqual(completed);
    expect(testHarness.request).toHaveBeenCalledTimes(1);
  });

  for (const stream of [
    {
      name: "explicit error",
      body: 'event: response.failed\ndata: {"type":"response.failed","error":{"message":"provider failed"}}\n\n',
      error: "provider failed"
    },
    {
      name: "missing application terminal",
      body: 'event: response.created\ndata: {"type":"response.created"}\n\n',
      error: "ended without a completed response"
    },
    {
      name: "invalid JSON",
      body: "event: response.completed\ndata: not-json\n\n",
      error: "contained invalid JSON"
    }
  ]) {
    test(`rejects Responses SSE with ${stream.name}`, async () => {
      const testHarness = harness(async () => exchange(new Response(stream.body)));
      await expect(
        openAiAuthenticatedApiCallWithDependencies<{ model: string }, unknown>(
          `${appApiUrl}/v1/responses`,
          "POST",
          { model: "model" },
          undefined,
          "api-key",
          testHarness.dependencies
        )
      ).rejects.toThrow(stream.error);
      expect(testHarness.request).toHaveBeenCalledTimes(1);
    });
  }

  test("surfaces authenticated logical errors and notifies user auth without retrying", async () => {
    const logicalError = jsonResponse(
      { message: "operation denied" },
      {
        status: 403,
        headers: {
          "x-opensecret-error-contract": "1",
          "x-opensecret-error-code": "operation_denied"
        }
      }
    );
    const testHarness = harness(async () => exchange(logicalError));

    await expect(
      authenticatedApiCallWithDependencies<undefined, never>(
        `${appApiUrl}/protected/user`,
        "GET",
        undefined,
        "fallback error",
        testHarness.dependencies
      )
    ).rejects.toThrow("operation denied");
    expect(testHarness.request).toHaveBeenCalledTimes(1);
    expect(testHarness.noteResponse).toHaveBeenCalledTimes(1);
  });

  test("does not dispatch when authenticated API authority changes in flight", async () => {
    let outerSends = 0;
    const testHarness = harness(async (input) => {
      input.beforeSend?.();
      outerSends += 1;
      return exchange(jsonResponse({ ok: true }));
    });
    const selected = authorityFor("user", appApiUrl);
    testHarness.authority.mockImplementation(async () => ({
      ...selected,
      assertCurrent() {
        throw new Error("Transport v2 authentication state changed before send.");
      }
    }));

    await expect(
      authenticatedApiCallWithDependencies<undefined, never>(
        `${appApiUrl}/protected/user`,
        "GET",
        undefined,
        undefined,
        testHarness.dependencies
      )
    ).rejects.toThrow("authentication state changed");
    expect(testHarness.request).toHaveBeenCalledTimes(1);
    expect(outerSends).toBe(0);
  });

  test("does not retry, resend, or fall back when V2 fails ambiguously", async () => {
    const testHarness = harness(async () => {
      throw new Error("transport v2 connection dropped after send");
    });

    await expect(
      encryptedApiCallWithDependencies<{ action: string }, never>(
        `${appApiUrl}/protected/action`,
        "POST",
        { action: "once" },
        "explicit-bearer",
        undefined,
        testHarness.dependencies
      )
    ).rejects.toThrow("transport v2 connection dropped after send");
    expect(testHarness.request).toHaveBeenCalledTimes(1);
  });
});
