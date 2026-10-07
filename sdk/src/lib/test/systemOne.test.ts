import { describe, expect, mock, test } from "bun:test";
import {
  SystemOneError,
  systemOneWithDependencies,
  type SystemOneRequest,
  type SystemOneResponse
} from "../systemOne";
import type { EncryptedApiDependencies } from "../encryptedApi";
import { snapshotPcrConfig } from "../pcr";
import type { StoredTransportV2Credentials } from "../transportV2/auth";
import type { TransportV2Authority, TransportV2AuthRuntime } from "../transportV2/authRuntime";
import type {
  TransportV2Runtime,
  TransportV2RuntimeRequest,
  TransportV2RuntimeResponse
} from "../transportV2/runtime";

const apiUrl = "https://api.example.test/gateway";

function credentials(): StoredTransportV2Credentials {
  return {
    kind: "user",
    principalId: "user-principal",
    apiOrigin: new URL(apiUrl).origin,
    revision: 7,
    accessToken: "user-access-token",
    refreshToken: "user-refresh-token",
    accessExpiresAtUnixSeconds: 4_000_000_000,
    refreshExpiresAtUnixSeconds: 4_000_000_000
  };
}

function authority(): TransportV2Authority {
  const stored = credentials();
  return {
    credential: { kind: "bearer", value: stored.accessToken },
    credentials: stored,
    snapshot: {
      kind: "user",
      principalId: stored.principalId,
      apiOrigin: stored.apiOrigin,
      revision: stored.revision
    },
    assertCurrent() {}
  };
}

function harness(respond: (input: TransportV2RuntimeRequest) => Response | Promise<Response>) {
  const seen: TransportV2RuntimeRequest[] = [];
  const request = mock(
    async (input: TransportV2RuntimeRequest): Promise<TransportV2RuntimeResponse> => {
      // The SDK zeroes the body buffer after the call; keep a copy to assert on.
      seen.push({
        ...input,
        request: {
          ...input.request,
          body: input.request.body ? new Uint8Array(input.request.body) : input.request.body
        }
      });
      return { response: await respond(input), rememberOAuthContinuation: () => {} };
    }
  );
  const auth = {
    authority: mock(async () => authority()),
    noteResponse: mock(() => {})
  } as unknown as TransportV2AuthRuntime;
  const dependencies: EncryptedApiDependencies = {
    runtime: { request } as unknown as TransportV2Runtime,
    auth,
    readCredentials: () => credentials(),
    getApiPcrConfig: () => snapshotPcrConfig({ environment: "development" }),
    getApiUrl: () => apiUrl,
    getPlatformApiUrl: () => "https://platform.example.test/gateway",
    getPlatformPcrConfig: () => snapshotPcrConfig({ environment: "production" })
  };
  return { seen, dependencies };
}

function jsonResponse(value: unknown, init?: ResponseInit): Response {
  return new Response(JSON.stringify(value), {
    ...init,
    headers: { "content-type": "application/json", ...init?.headers }
  });
}

function sentBody(input: TransportV2RuntimeRequest): string {
  const body = input.request.body;
  if (!body) throw new Error("request body missing");
  return new TextDecoder().decode(body);
}

function sentTarget(input: TransportV2RuntimeRequest): string | undefined {
  return Object.values(input.request).find(
    (value): value is string => typeof value === "string" && value.includes("/v1/systemone")
  );
}

const request: SystemOneRequest = {
  state: { ticket: "Charged twice, need a refund today." },
  questions: {
    zebra: { type: "noul", instructions: "Does the customer need help today?" },
    intent: {
      type: "choice",
      instructions: "What does the customer want?",
      criteria: { refund: "money back", question: "information", praise: null }
    },
    frustration: { type: "score", instructions: "How frustrated?", criteria: ["Low", "High"] }
  },
  temperature: 1
};

const answers: SystemOneResponse = {
  id: "so_1",
  model: "glm-5-3-flash",
  answers: {
    zebra: { type: "noul", noul: 0.93, temperature: 2.48, option_mass: 1 },
    intent: {
      type: "choice",
      choice: "refund",
      probabilities: { refund: 0.86, question: 0.1, praise: 0.04 },
      confidence: 0.6,
      temperature: 2.41,
      option_mass: 0.99
    },
    frustration: {
      type: "score",
      score: 1.2,
      legend: { "0": "Low", "1": "High" },
      probabilities: { "0": 0.4, "1": 0.6 },
      confidence: 0.03,
      temperature: 2.48,
      option_mass: 0.99
    }
  },
  usage: { input_tokens: 388, output_tokens: 3, cached_tokens: 0, requests: 3 }
};

describe("systemOne", () => {
  test("posts the request in caller order with the session credential and returns typed answers", async () => {
    const { seen, dependencies } = harness(() => jsonResponse(answers));

    const response = await systemOneWithDependencies(request, undefined, dependencies);

    expect(seen).toHaveLength(1);
    expect(sentTarget(seen[0])).toEndWith("/v1/systemone");
    expect(seen[0].request.credential).toEqual({ kind: "bearer", value: "user-access-token" });
    expect(sentBody(seen[0])).toBe(JSON.stringify(request));
    expect(Object.keys(JSON.parse(sentBody(seen[0])).questions)).toEqual([
      "zebra",
      "intent",
      "frustration"
    ]);
    expect(response).toEqual(answers);
    const intent = response.answers.intent;
    expect(intent.type).toBe("choice");
    if (intent.type === "choice") {
      expect(Object.keys(intent.probabilities)).toEqual(["refund", "question", "praise"]);
    }
  });

  test("uses an explicit API key when one is given", async () => {
    const { seen, dependencies } = harness(() => jsonResponse(answers));

    await systemOneWithDependencies(request, { apiKey: "maple-api-key" }, dependencies);

    expect(seen[0].request.credential).toEqual({ kind: "api_key", value: "maple-api-key" });
  });

  test("turns backend rejections into SystemOneError with status and code", async () => {
    const message = "A choice needs between 2 and 255 options.";
    const { dependencies } = harness(() =>
      jsonResponse(
        {
          status: 422,
          message,
          error: { message, code: "system_one_bad_option_count" }
        },
        {
          status: 422,
          headers: {
            "x-opensecret-error-contract": "1",
            "x-opensecret-error-code": "system_one_bad_option_count"
          }
        }
      )
    );

    const rejection = await systemOneWithDependencies(request, undefined, dependencies).catch(
      (error: unknown) => error
    );

    expect(rejection).toBeInstanceOf(SystemOneError);
    const error = rejection as SystemOneError;
    expect(error.status).toBe(422);
    expect(error.code).toBe("system_one_bad_option_count");
    expect(error.message).toBe(message);
  });

  test("keeps quota rejections readable and passes transport failures through unchanged", async () => {
    const quota = harness(() =>
      jsonResponse(
        { status: 403, message: "Usage limit reached" },
        { status: 403, headers: { "x-opensecret-error-code": "usage_limit_reached" } }
      )
    );
    const quotaError = (await systemOneWithDependencies(
      request,
      undefined,
      quota.dependencies
    ).catch((error: unknown) => error)) as SystemOneError;
    expect(quotaError).toBeInstanceOf(SystemOneError);
    expect(quotaError.status).toBe(403);
    expect(quotaError.code).toBe("usage_limit_reached");

    // A transport failure never reached the backend: the SDK reports it as a
    // generic error, not as a System One rejection.
    const broken = harness(() => {
      throw new Error("session lost");
    });
    const passed = (await systemOneWithDependencies(request, undefined, broken.dependencies).catch(
      (error: unknown) => error
    )) as Error & { status?: number };
    expect(passed).toBeInstanceOf(Error);
    expect(passed).not.toBeInstanceOf(SystemOneError);
    expect(passed.message).toBe("session lost");
    expect(passed.status).toBe(500);
  });
});
