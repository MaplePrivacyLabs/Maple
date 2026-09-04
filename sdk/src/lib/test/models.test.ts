import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { encodeURLSafe } from "@stablelib/base64";
import type { PcrConfig } from "../pcr";
import { fetchModelCatalog, fetchModels, getApiPcrConfig, getApiUrl, setApiUrl } from "../api";
import { clearTransportV2Credentials, installTransportV2Credentials } from "../transportV2/auth";
import {
  transportV2Runtime,
  type TransportV2RuntimeRequest,
  type TransportV2RuntimeResponse
} from "../transportV2/runtime";

const apiUrl = "https://models.example.com";
const pcrConfig: PcrConfig = { environment: "development", remoteAttestation: false };
const modelsResponse = {
  object: "list" as const,
  data: [
    {
      id: "test-model",
      object: "model" as const,
      created: 0,
      owned_by: "opensecret"
    }
  ]
};

const originalApiUrl = getApiUrl();
const originalApiPcrConfig = getApiPcrConfig();
const originalRuntimeRequest = transportV2Runtime.request;

function segment(value: unknown): string {
  return encodeURLSafe(new TextEncoder().encode(JSON.stringify(value))).replace(/=+$/u, "");
}

function token(audience: string): string {
  return `${segment({ alg: "ES256K", typ: "JWT" })}.${segment({
    aud: audience,
    sub: "models-user",
    exp: 4_000_000_000,
    tf: 2
  })}.${segment("signature")}`;
}

function installUserCredentials(): void {
  installTransportV2Credentials(
    apiUrl,
    "user",
    token("urn:opensecret:internal:transport-v2:user:access-token"),
    token("urn:opensecret:internal:transport-v2:user:refresh-token")
  );
}

function exchange(response: Response): TransportV2RuntimeResponse {
  return { response, rememberOAuthContinuation: () => {} };
}

beforeEach(() => {
  globalThis.localStorage.clear();
  globalThis.sessionStorage.clear();
  clearTransportV2Credentials(apiUrl);
  transportV2Runtime.clear();
  setApiUrl(apiUrl, pcrConfig);
});

afterEach(() => {
  transportV2Runtime.request = originalRuntimeRequest;
  transportV2Runtime.clear();
  clearTransportV2Credentials(apiUrl);
  setApiUrl(originalApiUrl, originalApiPcrConfig);
  globalThis.localStorage.clear();
  globalThis.sessionStorage.clear();
});

test("fetchModels uses an anonymous encrypted V2 request before sign-in", async () => {
  let seen: TransportV2RuntimeRequest | undefined;
  transportV2Runtime.request = mock(async (input) => {
    seen = input;
    return exchange(Response.json(modelsResponse));
  });

  await expect(fetchModels()).resolves.toEqual(modelsResponse.data);
  expect(seen?.request).toMatchObject({
    method: "GET",
    target: "/v1/models",
    credential: undefined,
    cacheNamespaceRoot: undefined,
    body: undefined
  });
});

test("fetchModels preserves V2 user authentication inside the request envelope", async () => {
  installUserCredentials();
  let seen: TransportV2RuntimeRequest | undefined;
  transportV2Runtime.request = mock(async (input) => {
    seen = input;
    return exchange(Response.json(modelsResponse));
  });

  await expect(fetchModels()).resolves.toEqual(modelsResponse.data);
  expect(seen?.request.credential).toMatchObject({ kind: "bearer" });
  expect(seen?.request.cacheNamespaceRoot).toHaveLength(32);
});

test("fetchModels never downgrades a rejected stored credential to anonymous access", async () => {
  installUserCredentials();
  const request = mock(async (input: TransportV2RuntimeRequest) => {
    expect(input.request.credential?.kind).toBe("bearer");
    return exchange(Response.json({ message: "Invalid JWT" }, { status: 401 }));
  });
  transportV2Runtime.request = request;

  await expect(fetchModels()).rejects.toThrow("Invalid JWT");
  expect(request).toHaveBeenCalledTimes(1);
});

test("fetchModels never downgrades a rejected API key to anonymous access", async () => {
  installUserCredentials();
  const request = mock(async (input: TransportV2RuntimeRequest) => {
    expect(input.request.credential).toEqual({ kind: "api_key", value: "invalid-api-key" });
    return exchange(Response.json({ message: "Invalid API key" }, { status: 401 }));
  });
  transportV2Runtime.request = request;

  await expect(fetchModels("invalid-api-key")).rejects.toThrow("Invalid API key");
  expect(request).toHaveBeenCalledTimes(1);
});

test("fetchModels rejects an explicitly empty API key before establishing a session", async () => {
  const request = mock(async () => exchange(Response.json(modelsResponse)));
  transportV2Runtime.request = request;

  await expect(fetchModels("")).rejects.toThrow("API key cannot be empty");
  expect(request).toHaveBeenCalledTimes(0);
});

test("fetchModelCatalog remains authentication-required", async () => {
  const request = mock(async () => exchange(Response.json(modelsResponse)));
  transportV2Runtime.request = request;

  await expect(fetchModelCatalog()).rejects.toThrow("No access token available");
  expect(request).toHaveBeenCalledTimes(0);
});
