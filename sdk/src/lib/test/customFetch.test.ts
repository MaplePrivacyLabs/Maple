import { describe, expect, mock, test } from "bun:test";
import OpenAI from "openai";
import {
  createCustomFetchWithDependencies,
  findOpenSecretInferenceCapacityError,
  OPEN_SECRET_INFERENCE_SEND_LIMIT_HEADER,
  OpenSecretInferenceCapacityError,
  type CustomFetchDependencies
} from "../ai";
import type { PcrConfig } from "../pcr";
import type { StoredTransportV2Credentials } from "../transportV2/auth";
import type { TransportV2RuntimeRequest, TransportV2RuntimeResponse } from "../transportV2/runtime";

const apiUrl = "https://api.example.test/base";
const cacheRoot = new Uint8Array(32).fill(0x42);

function deferred<T>() {
  let resolve!: (value: T) => void;
  return {
    promise: new Promise<T>((fulfill) => {
      resolve = fulfill;
    }),
    resolve
  };
}

function userCredentials(): StoredTransportV2Credentials {
  return {
    kind: "user",
    principalId: "00112233-4455-6677-8899-aabbccddeeff",
    apiOrigin: "https://api.example.test",
    revision: 1,
    accessToken: "user-access-token",
    refreshToken: "user-refresh-token",
    accessExpiresAtUnixSeconds: 4_000_000_000,
    refreshExpiresAtUnixSeconds: 4_000_000_000
  };
}

function result(response: Response): TransportV2RuntimeResponse {
  return { response, rememberOAuthContinuation: () => {} };
}

function dependencies(
  implementation: (input: TransportV2RuntimeRequest) => Promise<TransportV2RuntimeResponse>,
  credentials: StoredTransportV2Credentials | null = null
): CustomFetchDependencies {
  return {
    auth: {
      authority: mock(async () => {
        if (!credentials) throw new Error("No access token available");
        return {
          credential: { kind: "bearer", value: credentials.accessToken },
          credentials,
          snapshot: {
            apiOrigin: credentials.apiOrigin,
            kind: "user",
            principalId: credentials.principalId,
            revision: credentials.revision
          },
          assertCurrent() {}
        };
      }),
      noteResponse: mock(() => {})
    },
    runtime: { request: mock(implementation) },
    getApiPcrConfig: () => ({ environment: "development" }),
    getApiUrl: () => apiUrl,
    getCacheRoot: () => new Uint8Array(cacheRoot),
    readUserCredentials: () => credentials
  };
}

function capacityContractError(
  status: number,
  options?: {
    contract?: string | null;
    code?: string | null;
    replay?: string | null;
    retryAfter?: string | null;
  }
): Response {
  const headers = new Headers();
  if (options?.contract !== null) {
    headers.set("x-opensecret-error-contract", options?.contract ?? "1");
  }
  if (options?.code !== null) {
    headers.set("x-opensecret-error-code", options?.code ?? "inference_capacity");
  }
  if (options?.replay !== null) {
    headers.set("x-opensecret-client-replay", options?.replay ?? "safe");
  }
  if (options?.retryAfter !== undefined && options.retryAfter !== null) {
    headers.set("retry-after", options.retryAfter);
  }
  return new Response("private upstream capacity detail", { status, headers });
}

function capacityDependencies(response: () => Promise<Response>): CustomFetchDependencies {
  return dependencies(async (input) => {
    input.beforeSend?.();
    return result(await response());
  });
}

function copyRequest(input: TransportV2RuntimeRequest): TransportV2RuntimeRequest {
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

describe("createCustomFetch inference-capacity contract", () => {
  for (const status of [429, 503] as const) {
    test(`classifies exact ${status} without consuming or exposing its body`, async () => {
      let bodyRead = false;
      let bodyCancelled = false;
      const capacityResponse = capacityContractError(status, { retryAfter: "7" });
      const responseBody = capacityResponse.body;
      if (!responseBody) throw new Error("capacity test response must have a body");
      const cancelBody = responseBody.cancel.bind(responseBody);
      responseBody.cancel = async (reason?: unknown) => {
        bodyCancelled = true;
        return cancelBody(reason);
      };
      capacityResponse.text = async () => {
        bodyRead = true;
        return "private upstream capacity detail";
      };
      const customFetch = createCustomFetchWithDependencies(
        { apiKey: "test-api-key", apiUrl },
        capacityDependencies(async () => capacityResponse)
      );

      let error: unknown;
      try {
        await customFetch(`${apiUrl}/v1/responses`, {
          method: "POST",
          body: '{"prompt":"hello"}'
        });
      } catch (caught) {
        error = caught;
      }

      expect(error).toBeInstanceOf(OpenSecretInferenceCapacityError);
      expect(error).toMatchObject({
        name: "OpenSecretInferenceCapacityError",
        message: "Inference capacity is temporarily unavailable.",
        status,
        retryDelayMs: 7_000,
        inferenceSendCount: 1
      });
      expect(String(error)).not.toContain("private upstream");
      expect(bodyRead).toBe(false);
      expect(bodyCancelled).toBe(true);
    });
  }

  test("uses strict bounded delta-seconds retry hints", async () => {
    const cases: Array<[string | undefined, number | null]> = [
      [undefined, 1_000],
      ["0", 0],
      ["7", 7_000],
      ["60", 60_000],
      ["61", null],
      ["01", 1_000],
      ["-1", 1_000],
      ["1.5", 1_000],
      ["1e2", 1_000],
      ["Wed, 21 Oct 2015 07:28:00 GMT", 1_000],
      ["7, 9", 1_000],
      ["999999999999999999999999999999999999999999", null]
    ];

    for (const [retryAfter, expectedDelay] of cases) {
      const customFetch = createCustomFetchWithDependencies(
        { apiKey: "test-api-key", apiUrl },
        capacityDependencies(async () => capacityContractError(503, { retryAfter }))
      );

      let error: unknown;
      try {
        await customFetch(`${apiUrl}/v1/responses`);
      } catch (caught) {
        error = caught;
      }
      expect(error).toBeInstanceOf(OpenSecretInferenceCapacityError);
      expect((error as OpenSecretInferenceCapacityError).retryDelayMs).toBe(expectedDelay);
    }
  });

  test("rejects missing, future, duplicated, and status-mismatched required headers", async () => {
    const invalid = [
      capacityContractError(429, { contract: null }),
      capacityContractError(429, { contract: "2" }),
      capacityContractError(429, { contract: "1, 1" }),
      capacityContractError(429, { code: null }),
      capacityContractError(429, { code: "inference_capacity_v2" }),
      capacityContractError(429, { code: "inference_capacity, inference_capacity" }),
      capacityContractError(429, { replay: null }),
      capacityContractError(429, { replay: "true" }),
      capacityContractError(429, { replay: "safe, safe" }),
      capacityContractError(529),
      capacityContractError(500)
    ];

    const duplicateContract = capacityContractError(503);
    duplicateContract.headers.append("x-opensecret-error-contract", "1");
    invalid.push(duplicateContract);
    const duplicateCode = capacityContractError(503);
    duplicateCode.headers.append("x-opensecret-error-code", "inference_capacity");
    invalid.push(duplicateCode);
    const duplicateReplay = capacityContractError(503);
    duplicateReplay.headers.append("x-opensecret-client-replay", "safe");
    invalid.push(duplicateReplay);

    for (const response of invalid) {
      const customFetch = createCustomFetchWithDependencies(
        { apiKey: "test-api-key", apiUrl },
        capacityDependencies(async () => response.clone())
      );

      const returned = await customFetch(`${apiUrl}/v1/responses`);
      expect(returned.status).toBe(response.status);
      expect(findOpenSecretInferenceCapacityError(returned)).toBeNull();
    }
  });

  test("finds only the SDK-owned typed error through a bounded cause chain", () => {
    const capacity = new OpenSecretInferenceCapacityError(503, 1_000);
    const wrapped = new Error("outer", { cause: new Error("middle", { cause: capacity }) });
    expect(findOpenSecretInferenceCapacityError(wrapped)).toBe(capacity);
    expect(
      findOpenSecretInferenceCapacityError({
        name: "OpenSecretInferenceCapacityError",
        status: 503,
        retryDelayMs: 1_000
      })
    ).toBeNull();

    const cycle: { cause?: unknown } = {};
    cycle.cause = cycle;
    expect(findOpenSecretInferenceCapacityError(cycle)).toBeNull();
  });

  test("survives the real OpenAI wrapper with its transport retries disabled", async () => {
    let sends = 0;
    const customFetch = createCustomFetchWithDependencies(
      { apiKey: "test-api-key", apiUrl },
      capacityDependencies(async () => {
        sends += 1;
        return capacityContractError(503, { retryAfter: "0" });
      })
    );
    const openai = new OpenAI({
      apiKey: "not-a-real-api-key",
      baseURL: `${apiUrl}/v1/`,
      dangerouslyAllowBrowser: true,
      fetch: customFetch,
      maxRetries: 0
    });

    let error: unknown;
    try {
      await openai.responses.create({ model: "kimi-k3", input: "hello" });
    } catch (caught) {
      error = caught;
    }

    expect(sends).toBe(1);
    expect(error).toMatchObject({ name: "Error" });
    const capacity = findOpenSecretInferenceCapacityError(error);
    expect(capacity).toBeInstanceOf(OpenSecretInferenceCapacityError);
    expect(capacity).toMatchObject({ status: 503, retryDelayMs: 0 });
    expect((error as { cause?: unknown }).cause).toBe(capacity);
  });

  test("counts each runtime pre-send fence and removes the client-only limit", async () => {
    const seen: TransportV2RuntimeRequest[] = [];
    let sends = 0;
    const customFetch = createCustomFetchWithDependencies(
      { apiKey: "test-api-key", apiUrl },
      dependencies(async (input) => {
        seen.push(copyRequest(input));
        input.beforeSend?.();
        sends += 1;
        input.beforeSend?.();
        sends += 1;
        return result(capacityContractError(503, { retryAfter: "0" }));
      })
    );

    await expect(
      customFetch(`${apiUrl}/v1/responses`, {
        headers: { [OPEN_SECRET_INFERENCE_SEND_LIMIT_HEADER]: "2" }
      })
    ).rejects.toMatchObject({
      name: "OpenSecretInferenceCapacityError",
      status: 503,
      inferenceSendCount: 2
    });
    expect(sends).toBe(2);
    expect(seen[0].request.headers).not.toContainEqual({
      name: OPEN_SECRET_INFERENCE_SEND_LIMIT_HEADER,
      value: "2"
    });
  });

  test("the pre-send fence enforces a one-send ceiling", async () => {
    let sends = 0;
    const customFetch = createCustomFetchWithDependencies(
      { apiKey: "test-api-key", apiUrl },
      dependencies(async (input) => {
        input.beforeSend?.();
        sends += 1;
        input.beforeSend?.();
        sends += 1;
        return result(Response.json({ unexpected: true }));
      })
    );

    await expect(
      customFetch(`${apiUrl}/v1/responses`, {
        headers: { [OPEN_SECRET_INFERENCE_SEND_LIMIT_HEADER]: "1" }
      })
    ).rejects.toThrow("Inference request send budget exhausted");
    expect(sends).toBe(1);
  });
});

describe("Transport V2 custom Fetch adapter", () => {
  test("encrypts the exact binary body, target, safe headers, API key, and cache root", async () => {
    const seen: TransportV2RuntimeRequest[] = [];
    const deps = dependencies(async (input) => {
      seen.push(copyRequest(input));
      return result(new Response("ok", { headers: { "x-authenticated": "yes" } }));
    });
    const customFetch = createCustomFetchWithDependencies(
      { apiKey: "real-api-key", apiUrl, pcrConfig: { environment: "development" } },
      deps
    );
    const plaintext = new Uint8Array([0, 1, 2, 0xff]);

    const response = await customFetch(`${apiUrl}/v1/audio/transcriptions?b=2&a=1`, {
      method: "POST",
      body: plaintext,
      headers: {
        authorization: "Bearer caller-placeholder",
        "content-type": "application/octet-stream",
        "content-length": "4",
        "x-stainless-lang": "js",
        "x-stainless-retry-count": "0",
        "x-session-id": "caller-session",
        "x-openai-api-key": "caller-key",
        accept: "application/json",
        "x-safe-metadata": "kept"
      }
    });

    expect(await response.text()).toBe("ok");
    expect(response.headers.get("x-authenticated")).toBe("yes");
    expect(seen).toHaveLength(1);
    expect(seen[0].apiUrl).toBe(apiUrl);
    expect(seen[0].pcrConfig).toMatchObject({ environment: "development" });
    expect(seen[0].request).toMatchObject({
      credential: { kind: "api_key", value: "real-api-key" },
      method: "POST",
      target: "/v1/audio/transcriptions?b=2&a=1"
    });
    expect(seen[0].request.body).toEqual(plaintext);
    expect(seen[0].request.cacheNamespaceRoot).toEqual(cacheRoot);
    expect(seen[0].request.headers).toEqual([
      { name: "accept", value: "application/json" },
      { name: "content-type", value: "application/octet-stream" },
      { name: "x-safe-metadata", value: "kept" }
    ]);
  });

  test("preserves an absent body versus an explicitly empty body", async () => {
    const seen: TransportV2RuntimeRequest[] = [];
    const deps = dependencies(async (input) => {
      seen.push(copyRequest(input));
      return result(Response.json({ ok: true }));
    });
    const customFetch = createCustomFetchWithDependencies({ apiKey: "key", apiUrl }, deps);

    await customFetch(`${apiUrl}/v1/models`, { method: "GET" });
    await customFetch(`${apiUrl}/v1/audio/transcriptions`, { method: "POST" });
    await customFetch(`${apiUrl}/v1/audio/transcriptions`, {
      method: "POST",
      body: undefined
    });
    await customFetch(`${apiUrl}/v1/audio/transcriptions`, { method: "POST", body: "" });

    expect(seen.map(({ request }) => request.body)).toEqual([
      undefined,
      undefined,
      undefined,
      new Uint8Array(0)
    ]);
  });

  test("puts the signed-in user bearer and stable cache root inside the envelope", async () => {
    let seen: TransportV2RuntimeRequest | undefined;
    const deps = dependencies(async (input) => {
      seen = copyRequest(input);
      return result(Response.json({ ok: true }));
    }, userCredentials());
    const customFetch = createCustomFetchWithDependencies({ apiUrl }, deps);

    await customFetch(`${apiUrl}/v1/responses`, { method: "POST", body: "{}" });

    expect(seen?.request.credential).toEqual({ kind: "bearer", value: "user-access-token" });
    expect(seen?.request.cacheNamespaceRoot).toEqual(cacheRoot);
    expect(deps.auth.authority).toHaveBeenCalledTimes(1);
    expect(deps.auth.noteResponse).toHaveBeenCalledTimes(1);
  });

  test("does not dispatch after the selected user authority changes in flight", async () => {
    const stored = userCredentials();
    let outerSends = 0;
    const deps = dependencies(async (input) => {
      input.beforeSend?.();
      outerSends += 1;
      return result(Response.json({ ok: true }));
    }, stored);
    deps.auth.authority = mock(async () => ({
      credential: { kind: "bearer", value: stored.accessToken },
      credentials: stored,
      snapshot: {
        apiOrigin: stored.apiOrigin,
        kind: "user",
        principalId: stored.principalId,
        revision: stored.revision
      },
      assertCurrent() {
        throw new Error("Transport v2 authentication state changed before send.");
      }
    }));
    const customFetch = createCustomFetchWithDependencies({ apiUrl }, deps);

    await expect(
      customFetch(`${apiUrl}/v1/responses`, { method: "POST", body: "{}" })
    ).rejects.toThrow("authentication state changed");
    expect(deps.runtime.request).toHaveBeenCalledTimes(1);
    expect(outerSends).toBe(0);
  });

  test("allows only the models endpoint to be anonymous", async () => {
    const seen: TransportV2RuntimeRequest[] = [];
    const deps = dependencies(async (input) => {
      seen.push(copyRequest(input));
      return result(Response.json({ object: "list", data: [] }));
    });
    const customFetch = createCustomFetchWithDependencies({ apiUrl }, deps);

    await customFetch(`${apiUrl}/v1/models?available=true`, { method: "GET" });
    expect(seen[0].request.credential).toBeUndefined();
    expect(seen[0].request.cacheNamespaceRoot).toBeUndefined();
    await expect(
      customFetch(`${apiUrl}/v1/responses`, { method: "POST", body: "{}" })
    ).rejects.toThrow("fresh transport v2 sign-in");
    expect(seen).toHaveLength(1);
  });

  test("treats an explicitly empty API key as invalid instead of falling back to the user", async () => {
    const request = mock(async () => result(Response.json({ ok: true })));
    const customFetch = createCustomFetchWithDependencies(
      { apiKey: "", apiUrl },
      dependencies(request, userCredentials())
    );

    await expect(customFetch(`${apiUrl}/v1/models`)).rejects.toThrow("API key must not be empty");
    expect(request).toHaveBeenCalledTimes(0);
  });

  test("rejects automatic OpenAI retries before making another transport request", async () => {
    const request = mock(async () => result(Response.json({ ok: true })));
    const customFetch = createCustomFetchWithDependencies(
      { apiKey: "key", apiUrl },
      dependencies(request)
    );

    await expect(
      customFetch(`${apiUrl}/v1/responses`, {
        method: "POST",
        body: "{}",
        headers: { "x-stainless-retry-count": "1" }
      })
    ).rejects.toThrow("Configure maxRetries: 0");
    expect(request).toHaveBeenCalledTimes(0);
  });

  test("never retries an ambiguous transport failure", async () => {
    const request = mock(async () => {
      throw new Error("connection dropped after send");
    });
    const customFetch = createCustomFetchWithDependencies(
      { apiKey: "key", apiUrl },
      dependencies(request)
    );

    await expect(
      customFetch(`${apiUrl}/v1/responses`, { method: "POST", body: "{}" })
    ).rejects.toThrow("connection dropped after send");
    expect(request).toHaveBeenCalledTimes(1);
  });

  test("rejects cross-origin, outside-base, and fragment targets before transport", async () => {
    const request = mock(async () => result(Response.json({ ok: true })));
    const customFetch = createCustomFetchWithDependencies(
      { apiKey: "key", apiUrl },
      dependencies(request)
    );

    for (const target of [
      "https://other.example.test/base/v1/models",
      "https://api.example.test/base-evil/v1/models",
      "https://api.example.test/base/v1/models#hidden"
    ]) {
      await expect(customFetch(target)).rejects.toThrow("attested API");
    }
    expect(request).toHaveBeenCalledTimes(0);
  });

  test("returns authenticated SSE and native binary responses without rewriting them", async () => {
    const encoder = new TextEncoder();
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(encoder.encode("data: first\n\n"));
        controller.enqueue(encoder.encode("data: [DONE]\n\n"));
        controller.close();
      }
    });
    const sse = new Response(stream, { headers: { "content-type": "text/event-stream" } });
    const audio = new Uint8Array([0, 1, 2, 0xff]);
    const responses = [sse, new Response(audio, { headers: { "content-type": "audio/mpeg" } })];
    const deps = dependencies(async () => result(responses.shift()!));
    const customFetch = createCustomFetchWithDependencies({ apiKey: "key", apiUrl }, deps);

    const returnedSse = await customFetch(`${apiUrl}/v1/responses`, {
      method: "POST",
      body: "{}"
    });
    expect(returnedSse).toBe(sse);
    expect(await returnedSse.text()).toBe("data: first\n\ndata: [DONE]\n\n");

    const returnedAudio = await customFetch(`${apiUrl}/v1/audio/speech`, {
      method: "POST",
      body: "{}"
    });
    expect(returnedAudio.headers.get("content-type")).toBe("audio/mpeg");
    expect(new Uint8Array(await returnedAudio.arrayBuffer())).toEqual(audio);
  });

  test("performs no transport work for a pre-aborted request", async () => {
    const request = mock(async () => result(Response.json({ ok: true })));
    const customFetch = createCustomFetchWithDependencies(
      { apiKey: "key", apiUrl },
      dependencies(request)
    );
    const controller = new AbortController();
    controller.abort();

    await expect(
      customFetch(`${apiUrl}/v1/responses`, {
        method: "POST",
        body: "{}",
        signal: controller.signal
      })
    ).rejects.toMatchObject({ name: "AbortError" });
    expect(request).toHaveBeenCalledTimes(0);
  });

  test("an abort during authority preparation prevents the application send", async () => {
    const stored = userCredentials();
    const authorityReady = deferred<void>();
    const request = mock(async () => result(Response.json({ ok: true })));
    const deps = dependencies(request, stored);
    deps.auth.authority = mock(async () => {
      await authorityReady.promise;
      return {
        credential: { kind: "bearer", value: stored.accessToken },
        credentials: stored,
        snapshot: {
          apiOrigin: stored.apiOrigin,
          kind: "user",
          principalId: stored.principalId,
          revision: stored.revision
        },
        assertCurrent() {}
      };
    });
    const customFetch = createCustomFetchWithDependencies({ apiUrl }, deps);
    const controller = new AbortController();

    const pending = customFetch(`${apiUrl}/v1/responses`, {
      method: "POST",
      body: "{}",
      signal: controller.signal
    });
    expect(deps.auth.authority).toHaveBeenCalledTimes(1);
    controller.abort();
    authorityReady.resolve();

    await expect(pending).rejects.toMatchObject({ name: "AbortError" });
    expect(request).toHaveBeenCalledTimes(0);
  });

  test("preserves Fetch signal inheritance and explicit null detachment", async () => {
    const request = mock(async (input: TransportV2RuntimeRequest) => {
      expect(input.signal).toBeNull();
      return result(Response.json({ object: "list", data: [] }));
    });
    const customFetch = createCustomFetchWithDependencies(
      { apiKey: "key", apiUrl },
      dependencies(request)
    );

    const inheritedController = new AbortController();
    inheritedController.abort();
    const inherited = new Request(`${apiUrl}/v1/models`, {
      signal: inheritedController.signal
    });
    await expect(customFetch(inherited, { signal: undefined })).rejects.toMatchObject({
      name: "AbortError"
    });
    expect(request).toHaveBeenCalledTimes(0);

    const detachedController = new AbortController();
    detachedController.abort();
    const detached = new Request(`${apiUrl}/v1/models`, {
      signal: detachedController.signal
    });
    await expect(customFetch(detached, { signal: null })).resolves.toBeInstanceOf(Response);
    expect(request).toHaveBeenCalledTimes(1);
  });

  test("pins API key, endpoint, and PCR policy when caller options mutate in flight", async () => {
    const mutablePcrConfig: PcrConfig = {
      environment: "development",
      remoteAttestation: false,
      pcr0DevValues: ["11".repeat(48)]
    };
    const options = {
      apiKey: "first-api-key",
      apiUrl,
      pcrConfig: mutablePcrConfig
    };
    let seen: TransportV2RuntimeRequest | undefined;
    const customFetch = createCustomFetchWithDependencies(
      options,
      dependencies(async (input) => {
        seen = copyRequest(input);
        return result(Response.json({ ok: true }));
      })
    );

    const pending = customFetch(`${apiUrl}/v1/responses`, { method: "POST", body: "{}" });
    options.apiKey = "second-api-key";
    options.apiUrl = "https://other.example.test";
    mutablePcrConfig.environment = "production";
    mutablePcrConfig.remoteAttestation = true;
    mutablePcrConfig.pcr0DevValues![0] = "22".repeat(48);

    await pending;
    expect(seen?.apiUrl).toBe(apiUrl);
    expect(seen?.request.credential).toEqual({ kind: "api_key", value: "first-api-key" });
    expect(seen?.pcrConfig).toMatchObject({
      environment: "development",
      remoteAttestation: false,
      pcr0DevValues: ["11".repeat(48)]
    });
  });
});
