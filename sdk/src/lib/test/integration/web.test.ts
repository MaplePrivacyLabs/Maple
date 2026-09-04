import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { encodeURLSafe } from "@stablelib/base64";
import type { PcrConfig } from "../../pcr";
import {
  getApiPcrConfig,
  getApiUrl,
  setApiUrl,
  webExtract,
  webSearch,
  type WebExtractRequest,
  type WebExtractResponse,
  type WebSearchRequest,
  type WebSearchResponse
} from "../../api";
import { clearTransportV2Credentials, installTransportV2Credentials } from "../../transportV2/auth";
import {
  transportV2Runtime,
  type TransportV2RuntimeRequest,
  type TransportV2RuntimeResponse
} from "../../transportV2/runtime";

const apiUrl = "https://api.example.com";
const pcrConfig: PcrConfig = { environment: "development", remoteAttestation: false };
const originalApiUrl = getApiUrl();
const originalApiPcrConfig = getApiPcrConfig();
const originalRuntimeRequest = transportV2Runtime.request;

function segment(value: unknown): string {
  return encodeURLSafe(new TextEncoder().encode(JSON.stringify(value))).replace(/=+$/u, "");
}

function token(audience: string): string {
  return `${segment({ alg: "ES256K", typ: "JWT" })}.${segment({
    aud: audience,
    sub: "web-user",
    exp: 4_000_000_000,
    tf: 2
  })}.${segment("signature")}`;
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
  installTransportV2Credentials(
    apiUrl,
    "user",
    token("urn:opensecret:internal:transport-v2:user:access-token"),
    token("urn:opensecret:internal:transport-v2:user:refresh-token")
  );
});

afterEach(() => {
  transportV2Runtime.request = originalRuntimeRequest;
  transportV2Runtime.clear();
  clearTransportV2Credentials(apiUrl);
  setApiUrl(originalApiUrl, originalApiPcrConfig);
  globalThis.localStorage.clear();
  globalThis.sessionStorage.clear();
});

test("webSearch sends request data inside an authenticated V2 envelope", async () => {
  const request: WebSearchRequest = {
    query: "rust confidential computing",
    workflow: "news",
    page: 2,
    limit: 25,
    safe_search: false,
    timeout: 2.5,
    lens: {
      sites_included: ["example.com"],
      keywords_included: ["enclave"],
      time_relative: "week",
      search_region: "US"
    },
    filters: { region: "US" }
  };
  const response: WebSearchResponse = {
    trace_id: "trace-search-1",
    results: [
      {
        category: "news",
        url: "https://example.com/enclave",
        title: "Enclave update",
        snippet: "A short description.",
        published_at: "2026-07-16T12:00:00Z"
      }
    ]
  };
  let seen: TransportV2RuntimeRequest | undefined;
  let seenBody: Uint8Array | undefined;
  transportV2Runtime.request = mock(async (input) => {
    seen = input;
    seenBody = input.request.body ? new Uint8Array(input.request.body) : undefined;
    return exchange(Response.json(response));
  });

  await expect(webSearch(request)).resolves.toEqual(response);
  expect(seen?.request).toMatchObject({
    method: "POST",
    target: "/v1/web/search",
    headers: [{ name: "content-type", value: "application/json" }],
    credential: { kind: "bearer" }
  });
  expect(JSON.parse(new TextDecoder().decode(seenBody))).toEqual(request);
});

test("webExtract preserves ordered pages and typed partial failures", async () => {
  const request: WebExtractRequest = {
    urls: ["https://example.com/first", "https://example.com/second"],
    timeout: 4.5
  };
  const response: WebExtractResponse = {
    trace_id: "trace-extract-1",
    pages: [
      { url: request.urls[0], markdown: "# First\n\nExtracted text." },
      {
        url: request.urls[1],
        error: { code: "no_content", message: "No readable content was found." }
      }
    ]
  };
  let seenBody: Uint8Array | undefined;
  transportV2Runtime.request = mock(async (input) => {
    seenBody = input.request.body ? new Uint8Array(input.request.body) : undefined;
    return exchange(Response.json(response));
  });

  const result = await webExtract(request);
  expect(result).toEqual(response);
  expect(result.pages.map((page) => page.url)).toEqual(request.urls);
  expect(result.pages[1].error?.code).toBe("no_content");
  expect(JSON.parse(new TextDecoder().decode(seenBody))).toEqual(request);
});

test("web validation errors surface without retry or anonymous fallback", async () => {
  const request = mock(async (input: TransportV2RuntimeRequest) => {
    expect(input.request.credential?.kind).toBe("bearer");
    return exchange(
      Response.json(
        { status: 422, code: "invalid_request", message: "The web request is invalid." },
        { status: 422 }
      )
    );
  });
  transportV2Runtime.request = request;

  await expect(webSearch({ query: "maple privacy", limit: 51 })).rejects.toThrow(
    "The web request is invalid."
  );
  expect(request).toHaveBeenCalledTimes(1);
});
