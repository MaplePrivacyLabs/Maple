import * as api from "./api";
import { snapshotPcrConfig, type PcrConfig } from "./pcr";
import {
  getOrCreateTransportV2CacheRoot,
  readTransportV2Credentials,
  type StoredTransportV2Credentials
} from "./transportV2/auth";
import {
  transportV2AuthRuntime,
  type TransportV2Authority,
  type TransportV2AuthRuntime
} from "./transportV2/authRuntime";
import type { TransportV2Credential, TransportV2Header } from "./transportV2/protocol";
import {
  transportV2LogicalTarget,
  transportV2Runtime,
  type TransportV2Runtime
} from "./transportV2/runtime";

export interface CustomFetchOptions {
  /** Optional API key to use instead of the signed-in user's V2 bearer. */
  apiKey?: string;
  /** Fixed API URL whose attestation policy governs every request. */
  apiUrl?: string;
  /** PCR0 trust policy enforced before non-loopback session establishment. */
  pcrConfig?: PcrConfig;
}

const INFERENCE_CAPACITY_ERROR_MESSAGE = "Inference capacity is temporarily unavailable.";
const INFERENCE_CAPACITY_CONTRACT_HEADER = "x-opensecret-error-contract";
const INFERENCE_CAPACITY_CODE_HEADER = "x-opensecret-error-code";
const INFERENCE_CAPACITY_REPLAY_HEADER = "x-opensecret-client-replay";
const INFERENCE_CAPACITY_CONTRACT_VERSION = "1";
const INFERENCE_CAPACITY_ERROR_CODE = "inference_capacity";
const INFERENCE_CAPACITY_REPLAY_SAFE = "safe";
const DEFAULT_INFERENCE_CAPACITY_RETRY_DELAY_MS = 1_000;
const MAX_INFERENCE_CAPACITY_RETRY_DELAY_SECS = 60n;
/** Client-only header consumed by createCustomFetch and never forwarded upstream. */
export const OPEN_SECRET_INFERENCE_SEND_LIMIT_HEADER = "x-opensecret-client-inference-send-limit";

export class OpenSecretInferenceCapacityError extends Error {
  readonly status: 429 | 503;
  /** Null means the server delay exceeds Maple's bounded automatic-replay window. */
  readonly retryDelayMs: number | null;
  /** Number of inference HTTP sends consumed before this terminal response. */
  readonly inferenceSendCount: number;

  constructor(status: 429 | 503, retryDelayMs: number | null, inferenceSendCount = 1) {
    super(INFERENCE_CAPACITY_ERROR_MESSAGE);
    this.name = "OpenSecretInferenceCapacityError";
    this.status = status;
    this.retryDelayMs = retryDelayMs;
    this.inferenceSendCount = inferenceSendCount;
  }
}

/** Finds the SDK-owned capacity error through wrappers such as OpenAI APIConnectionError. */
export function findOpenSecretInferenceCapacityError(
  error: unknown
): OpenSecretInferenceCapacityError | null {
  const seen = new Set<unknown>();
  let current = error;

  for (let depth = 0; depth < 8 && current !== null && current !== undefined; depth += 1) {
    if (current instanceof OpenSecretInferenceCapacityError) return current;
    if (typeof current !== "object" || seen.has(current)) return null;
    seen.add(current);
    current = (current as { cause?: unknown }).cause;
  }

  return null;
}

function retryDelayFromCapacityHeaders(headers: Headers): number | null {
  const retryAfter = headers.get("retry-after");
  if (retryAfter === null || !/^(0|[1-9]\d*)$/.test(retryAfter)) {
    return DEFAULT_INFERENCE_CAPACITY_RETRY_DELAY_MS;
  }

  const seconds = BigInt(retryAfter);
  if (seconds > MAX_INFERENCE_CAPACITY_RETRY_DELAY_SECS) return null;
  return Number(seconds) * 1_000;
}

function inferenceCapacityError(
  response: Response,
  inferenceSendCount: number
): OpenSecretInferenceCapacityError | null {
  if (response.status !== 429 && response.status !== 503) return null;
  if (
    response.headers.get(INFERENCE_CAPACITY_CONTRACT_HEADER) !==
      INFERENCE_CAPACITY_CONTRACT_VERSION ||
    response.headers.get(INFERENCE_CAPACITY_CODE_HEADER) !== INFERENCE_CAPACITY_ERROR_CODE ||
    response.headers.get(INFERENCE_CAPACITY_REPLAY_HEADER) !== INFERENCE_CAPACITY_REPLAY_SAFE
  ) {
    return null;
  }

  return new OpenSecretInferenceCapacityError(
    response.status,
    retryDelayFromCapacityHeaders(response.headers),
    inferenceSendCount
  );
}

function takeInferenceSendLimit(headers: Headers): number {
  const rawLimit = headers.get(OPEN_SECRET_INFERENCE_SEND_LIMIT_HEADER);
  headers.delete(OPEN_SECRET_INFERENCE_SEND_LIMIT_HEADER);
  return rawLimit === "1" ? 1 : 2;
}

/** @internal Exported for deterministic transport tests, not from the package entry point. */
export interface CustomFetchDependencies {
  auth: Pick<TransportV2AuthRuntime, "authority" | "noteResponse">;
  runtime: Pick<TransportV2Runtime, "request">;
  getApiPcrConfig: typeof api.getApiPcrConfig;
  getApiUrl: typeof api.getApiUrl;
  getCacheRoot(apiUrl: string): Uint8Array;
  readUserCredentials(apiUrl: string): StoredTransportV2Credentials | null;
}

const defaultDependencies: CustomFetchDependencies = {
  auth: transportV2AuthRuntime,
  runtime: transportV2Runtime,
  getApiPcrConfig: () => api.getApiPcrConfig(),
  getApiUrl: () => api.getApiUrl(),
  getCacheRoot: (apiUrl) => getOrCreateTransportV2CacheRoot(apiUrl),
  readUserCredentials: (apiUrl) => readTransportV2Credentials(apiUrl, "user")
};

// These fields either control the untrusted outer hop or can carry a second,
// conflicting credential. All ordinary application headers remain inside the
// authenticated request envelope.
const OMITTED_LOGICAL_HEADERS = new Set([
  "authorization",
  "proxy-authorization",
  "cookie",
  "set-cookie",
  "host",
  "content-length",
  "transfer-encoding",
  "connection",
  "keep-alive",
  "te",
  "trailer",
  "upgrade",
  "forwarded",
  "via",
  "x-forwarded-for",
  "x-forwarded-host",
  "x-forwarded-proto",
  "x-session-id",
  "x-api-key",
  "api-key",
  "x-openai-api-key",
  "x-tinfoil-api-key",
  "x-goog-api-key",
  "x-anthropic-api-key"
]);

function logicalHeaders(input: Headers): TransportV2Header[] {
  const output: TransportV2Header[] = [];
  input.forEach((value, name) => {
    const normalizedName = name.toLowerCase();
    if (
      !OMITTED_LOGICAL_HEADERS.has(normalizedName) &&
      !normalizedName.startsWith("x-stainless-")
    ) {
      output.push({ name: normalizedName, value });
    }
  });
  return output;
}

function rejectAutomaticOpenAiRetry(headers: Headers): void {
  const retryCount = headers.get("x-stainless-retry-count");
  if (retryCount !== null && retryCount !== "0") {
    throw new Error(
      "Transport v2 rejected an automatic OpenAI retry after a potentially sent request. Configure maxRetries: 0."
    );
  }
}

function bodyIsPresent(
  input: string | URL | Request,
  init: RequestInit | undefined,
  normalized: Request
): boolean {
  if (normalized.method === "GET" || normalized.method === "HEAD") return false;
  if (init && Object.prototype.hasOwnProperty.call(init, "body") && init.body !== undefined) {
    return init.body !== null;
  }
  if (normalized.body !== null && normalized.body !== undefined) return true;
  return input instanceof Request && input.body !== null && input.body !== undefined;
}

async function requestBody(
  input: string | URL | Request,
  init: RequestInit | undefined,
  normalized: Request
): Promise<Uint8Array | undefined> {
  if (!bodyIsPresent(input, init, normalized)) return undefined;
  return new Uint8Array(await normalized.arrayBuffer());
}

function requestSignal(
  input: string | URL | Request,
  init: RequestInit | undefined
): AbortSignal | null | undefined {
  if (init?.signal === null) return null;
  return init?.signal ?? (input instanceof Request ? input.signal : undefined);
}

function normalizedRequest(input: string | URL | Request, init: RequestInit | undefined): Request {
  if (init?.signal !== null) return new Request(input, init);
  // Bun currently inherits an already-aborted source Request signal even when
  // RequestInit.signal is explicitly null. Use a fresh never-aborted signal to
  // preserve Fetch's public detachment semantics while passing null onward.
  return new Request(input, { ...init, signal: new AbortController().signal });
}

async function authorityFor(
  apiUrl: string,
  pcrConfig: PcrConfig,
  target: string,
  apiKey: string | undefined,
  dependencies: CustomFetchDependencies
): Promise<{
  credential?: TransportV2Credential;
  authority?: TransportV2Authority;
}> {
  if (apiKey !== undefined) {
    if (apiKey.length === 0) {
      throw new Error("Transport v2 API key must not be empty.");
    }
    return { credential: { kind: "api_key", value: apiKey } };
  }
  if (!dependencies.readUserCredentials(apiUrl)) {
    if (new URL(target, "https://logical.invalid").pathname === "/v1/models") return {};
    throw new Error("A fresh transport v2 sign-in or API key is required.");
  }
  const authority = await dependencies.auth.authority(apiUrl, pcrConfig, "user");
  return { credential: authority.credential, authority };
}

/**
 * Creates an attested Transport V2 fetch adapter for OpenAI-compatible calls.
 * Set the OpenAI client to `maxRetries: 0`; this adapter additionally refuses
 * a nonzero Stainless retry before a second enclave request can be sent.
 */
export function createCustomFetch(
  options?: CustomFetchOptions
): (input: string | URL | Request, init?: RequestInit) => Promise<Response> {
  return createCustomFetchWithDependencies(options, defaultDependencies);
}

/** @internal Exported for deterministic transport tests, not from the package entry point. */
export function createCustomFetchWithDependencies(
  options: CustomFetchOptions | undefined,
  dependencies: CustomFetchDependencies
): (input: string | URL | Request, init?: RequestInit) => Promise<Response> {
  return async (input: string | URL | Request, init?: RequestInit): Promise<Response> => {
    const configuredApiKey = options?.apiKey;
    const apiUrl = options?.apiUrl ?? dependencies.getApiUrl();
    const pcrConfig = snapshotPcrConfig(options?.pcrConfig ?? dependencies.getApiPcrConfig());
    const signal = requestSignal(input, init);
    const normalized = normalizedRequest(input, init);
    signal?.throwIfAborted();
    rejectAutomaticOpenAiRetry(normalized.headers);
    const maxInferenceSends = takeInferenceSendLimit(normalized.headers);
    let inferenceSendCount = 0;

    const target = transportV2LogicalTarget(apiUrl, normalized.url);
    const { credential, authority } = await authorityFor(
      apiUrl,
      pcrConfig,
      target,
      configuredApiKey,
      dependencies
    );
    const body = await requestBody(input, init, normalized);
    let cacheNamespaceRoot: Uint8Array | undefined;

    try {
      signal?.throwIfAborted();
      cacheNamespaceRoot = credential ? dependencies.getCacheRoot(apiUrl) : undefined;
      const result = await dependencies.runtime.request({
        apiUrl,
        pcrConfig,
        beforeSend: () => {
          authority?.assertCurrent();
          // The runtime calls this synchronous fence immediately before each
          // application fetch, including a session-recovery replay. Attestation
          // and auth preparation do not consume the inference send budget.
          if (inferenceSendCount >= maxInferenceSends) {
            throw new Error("Inference request send budget exhausted");
          }
          inferenceSendCount += 1;
        },
        signal,
        request: {
          credential,
          cacheNamespaceRoot,
          method: normalized.method,
          target,
          headers: logicalHeaders(normalized.headers),
          body
        }
      });
      if (authority) {
        dependencies.auth.noteResponse(result.response, apiUrl, pcrConfig, "user", authority);
      }
      const capacityError = inferenceCapacityError(result.response, inferenceSendCount);
      if (capacityError) {
        await result.response.body?.cancel("inference capacity response").catch(() => {});
        throw capacityError;
      }
      // Transport V2 decrypts opaque body bytes. Returning the authenticated
      // Response unchanged preserves incremental SSE and native TTS bytes.
      return result.response;
    } finally {
      body?.fill(0);
      cacheNamespaceRoot?.fill(0);
    }
  };
}
