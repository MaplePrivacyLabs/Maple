import { serializePcrConfig, snapshotPcrConfig, type PcrConfig } from "../pcr";
import {
  TransportV2Client,
  canonicalizeTransportV2ApiUrl,
  type TransportV2ClientOptions
} from "./client";
import {
  TransportV2ProtocolError,
  encodeCanonicalBase64,
  utf8,
  type TransportV2Request
} from "./protocol";
import type { SerializedTransportV2Session, TransportV2LogicalResponse } from "./session";

const OAUTH_CONTINUATION_PREFIX = "opensecret:transport-v2:oauth-session:v1:";

export type TransportV2OAuthProvider = "github" | "google" | "apple";

export interface TransportV2SessionInfo {
  protocolVersion: 2;
  sessionId: string;
  expiresAtUnixSeconds: number;
}

export interface TransportV2PublicAttestation {
  sessionKey: null;
  sessionId: string;
}

export interface TransportV2RuntimeRequest {
  apiUrl: string;
  pcrConfig?: PcrConfig;
  request: TransportV2Request;
  signal?: AbortSignal | null;
  /** Synchronous authority fence run after encryption and immediately before fetch. */
  beforeSend?: () => void;
  oauthCallback?: {
    provider: TransportV2OAuthProvider;
    state: string;
  };
}

export interface TransportV2RuntimeResponse {
  response: Response;
  rememberOAuthContinuation(provider: TransportV2OAuthProvider, state: string): void;
}

export interface TransportV2RuntimeDependencies {
  establish(options: TransportV2ClientOptions): Promise<TransportV2Client>;
  restore(
    options: Pick<TransportV2ClientOptions, "apiUrl" | "fetch">,
    state: SerializedTransportV2Session
  ): TransportV2Client;
  fetch?: typeof globalThis.fetch;
}

interface PendingEstablishment {
  generation: number;
  promise: Promise<TransportV2Client>;
}

function abortReason(signal: AbortSignal): unknown {
  return signal.reason ?? new DOMException("The operation was aborted.", "AbortError");
}

async function waitForSharedEstablishment<T>(
  pending: Promise<T>,
  signal?: AbortSignal | null
): Promise<T> {
  if (!signal) return pending;
  if (signal.aborted) throw abortReason(signal);
  return new Promise<T>((resolve, reject) => {
    const onAbort = () => {
      signal.removeEventListener("abort", onAbort);
      reject(abortReason(signal));
    };
    signal.addEventListener("abort", onAbort, { once: true });
    pending.then(resolve, reject).finally(() => signal.removeEventListener("abort", onAbort));
  });
}

/**
 * Resolve an application URL against the exact attested API base. The outer
 * request always goes to /v2/request; this relative target is authenticated
 * inside the record, including its query string.
 */
export function transportV2LogicalTarget(apiUrl: string, requestUrl: string): string {
  const canonicalApiUrl = canonicalizeTransportV2ApiUrl(apiUrl);
  const base = new URL(canonicalApiUrl);
  let requested: URL;
  try {
    requested = new URL(requestUrl, `${canonicalApiUrl}/`);
  } catch {
    throw new TransportV2ProtocolError("Transport v2 request URL is invalid.");
  }
  if (
    requested.origin !== base.origin ||
    requested.username ||
    requested.password ||
    requested.hash
  ) {
    throw new TransportV2ProtocolError(
      "Transport v2 request must remain inside its attested API origin."
    );
  }
  const basePath = base.pathname === "/" ? "" : base.pathname.replace(/\/+$/u, "");
  if (
    basePath &&
    requested.pathname !== basePath &&
    !requested.pathname.startsWith(`${basePath}/`)
  ) {
    throw new TransportV2ProtocolError(
      "Transport v2 request must remain inside its attested API base path."
    );
  }
  const pathname = requested.pathname.slice(basePath.length) || "/";
  return `${pathname}${requested.search}`;
}

interface OAuthContinuation {
  version: 2;
  api_url: string;
  pcr_policy: string;
  provider: TransportV2OAuthProvider;
  state: string;
  session: SerializedTransportV2Session;
}

function sessionStorageOrUndefined(): Storage | undefined {
  try {
    return globalThis.sessionStorage;
  } catch {
    return undefined;
  }
}

function continuationStorageKey(apiUrl: string, provider: TransportV2OAuthProvider): string {
  const scope = encodeCanonicalBase64(utf8(`${apiUrl}\n${provider}`))
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/u, "");
  return `${OAUTH_CONTINUATION_PREFIX}${scope}`;
}

function exactObject(value: unknown, expectedKeys: readonly string[]): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new TransportV2ProtocolError("Transport v2 OAuth continuation is invalid.");
  }
  const object = value as Record<string, unknown>;
  const actual = Object.keys(object).sort();
  const expected = [...expectedKeys].sort();
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) {
    throw new TransportV2ProtocolError("Transport v2 OAuth continuation is invalid.");
  }
  return object;
}

function parseSerializedSession(value: unknown): SerializedTransportV2Session {
  const object = exactObject(value, [
    "version",
    "session_id",
    "routing_key",
    "request_key",
    "response_key",
    "expires_at_ms"
  ]);
  if (
    object.version !== 2 ||
    typeof object.session_id !== "string" ||
    typeof object.routing_key !== "string" ||
    typeof object.request_key !== "string" ||
    typeof object.response_key !== "string" ||
    typeof object.expires_at_ms !== "number"
  ) {
    throw new TransportV2ProtocolError("Transport v2 OAuth continuation session is invalid.");
  }
  return object as unknown as SerializedTransportV2Session;
}

function parseContinuation(raw: string): OAuthContinuation {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    throw new TransportV2ProtocolError("Transport v2 OAuth continuation is invalid JSON.");
  }
  const object = exactObject(value, [
    "version",
    "api_url",
    "pcr_policy",
    "provider",
    "state",
    "session"
  ]);
  if (
    object.version !== 2 ||
    typeof object.api_url !== "string" ||
    typeof object.pcr_policy !== "string" ||
    (object.provider !== "github" && object.provider !== "google" && object.provider !== "apple") ||
    typeof object.state !== "string" ||
    object.state.length === 0
  ) {
    throw new TransportV2ProtocolError("Transport v2 OAuth continuation is invalid.");
  }
  return {
    version: 2,
    api_url: object.api_url,
    pcr_policy: object.pcr_policy,
    provider: object.provider,
    state: object.state,
    session: parseSerializedSession(object.session)
  };
}

function headersFromLogical(response: TransportV2LogicalResponse): Headers {
  const headers = new Headers();
  for (const { name, value } of response.headers) headers.append(name, value);
  return headers;
}

async function requireEmptyBody(body: ReadableStream<Uint8Array>): Promise<void> {
  const bytes = new Uint8Array(await new Response(body).arrayBuffer());
  if (bytes.byteLength !== 0) {
    bytes.fill(0);
    throw new TransportV2ProtocolError("Transport v2 bodyless response contained a body.");
  }
}

async function toFetchResponse(logical: TransportV2LogicalResponse): Promise<Response> {
  const headers = headersFromLogical(logical);
  if (logical.status === 204 || logical.status === 205 || logical.status === 304) {
    await requireEmptyBody(logical.body);
    return new Response(null, { status: logical.status, headers });
  }
  return new Response(logical.body, { status: logical.status, headers });
}

function defaultDependencies(): TransportV2RuntimeDependencies {
  return {
    establish: (options) => TransportV2Client.establish(options),
    restore: (options, state) => TransportV2Client.restore(options, state)
  };
}

/**
 * Process-local owner of attested cryptographic sessions. Authentication is
 * deliberately not session state: each credential remains inside its own
 * whole-request AEAD envelope.
 */
export class TransportV2Runtime {
  #clients = new Map<string, TransportV2Client>();
  #establishing = new Map<string, PendingEstablishment>();
  #generations = new Map<string, number>();
  #active = new WeakMap<TransportV2Client, number>();
  #retired = new WeakSet<TransportV2Client>();
  #dependencies: TransportV2RuntimeDependencies;

  constructor(dependencies: TransportV2RuntimeDependencies = defaultDependencies()) {
    this.#dependencies = dependencies;
  }

  #retain(client: TransportV2Client): void {
    this.#active.set(client, (this.#active.get(client) ?? 0) + 1);
  }

  #release(client: TransportV2Client): void {
    const remaining = (this.#active.get(client) ?? 1) - 1;
    if (remaining > 0) {
      this.#active.set(client, remaining);
      return;
    }
    this.#active.delete(client);
    if (this.#retired.has(client)) client.dispose();
  }

  #retire(scope: string, client: TransportV2Client): void {
    if (this.#clients.get(scope) === client) this.#clients.delete(scope);
    if (this.#retired.has(client)) return;
    this.#retired.add(client);
    if ((this.#active.get(client) ?? 0) === 0) client.dispose();
  }

  #generation(scope: string): number {
    return this.#generations.get(scope) ?? 0;
  }

  #invalidateScope(scope: string): void {
    const generation = this.#generation(scope);
    if (generation >= Number.MAX_SAFE_INTEGER) {
      throw new TransportV2ProtocolError("Transport v2 session generation is exhausted.");
    }
    this.#generations.set(scope, generation + 1);
    const client = this.#clients.get(scope);
    if (client) this.#retire(scope, client);
  }

  #identity(apiUrl: string, pcrConfig?: PcrConfig) {
    const canonicalApiUrl = canonicalizeTransportV2ApiUrl(apiUrl);
    const policy = snapshotPcrConfig(pcrConfig);
    const serializedPolicy = serializePcrConfig(policy);
    return {
      apiUrl: canonicalApiUrl,
      policy,
      serializedPolicy,
      scope: `${canonicalApiUrl}\n${serializedPolicy}`
    };
  }

  async #clientFor(
    apiUrl: string,
    pcrConfig?: PcrConfig,
    signal?: AbortSignal | null
  ): Promise<TransportV2Client> {
    const identity = this.#identity(apiUrl, pcrConfig);
    const current = this.#clients.get(identity.scope);
    if (current?.isUsable()) return current;
    if (current) this.#retire(identity.scope, current);

    const generation = this.#generation(identity.scope);
    let pending = this.#establishing.get(identity.scope);
    if (!pending || pending.generation !== generation) {
      let created!: PendingEstablishment;
      const promise = this.#dependencies
        .establish({
          apiUrl: identity.apiUrl,
          pcrConfig: identity.policy,
          fetch: this.#dependencies.fetch
        })
        .then(
          (client) => {
            if (this.#generation(identity.scope) === generation) {
              this.#clients.set(identity.scope, client);
            } else {
              this.#retire(identity.scope, client);
            }
            if (this.#establishing.get(identity.scope) === created) {
              this.#establishing.delete(identity.scope);
            }
            return client;
          },
          (error: unknown) => {
            if (this.#establishing.get(identity.scope) === created) {
              this.#establishing.delete(identity.scope);
            }
            throw error;
          }
        );
      created = { generation, promise };
      pending = created;
      this.#establishing.set(identity.scope, created);
      // The shared operation outlives individual aborting waiters. Its error
      // is still observed here when every waiter has detached.
      void created.promise.catch(() => {});
    }
    const client = await waitForSharedEstablishment(pending.promise, signal);
    if (this.#generation(identity.scope) !== pending.generation) {
      this.#retire(identity.scope, client);
      return this.#clientFor(identity.apiUrl, identity.policy, signal);
    }
    this.#clients.set(identity.scope, client);
    return client;
  }

  #rememberOAuth(
    client: TransportV2Client,
    apiUrl: string,
    serializedPolicy: string,
    provider: TransportV2OAuthProvider,
    state: string
  ): void {
    if (!state) throw new TransportV2ProtocolError("Transport v2 OAuth state is missing.");
    const storage = sessionStorageOrUndefined();
    if (!storage) {
      throw new TransportV2ProtocolError("Transport v2 OAuth requires same-tab session storage.");
    }
    const continuation: OAuthContinuation = {
      version: 2,
      api_url: apiUrl,
      pcr_policy: serializedPolicy,
      provider,
      state,
      session: client.serializeSession()
    };
    try {
      storage.setItem(continuationStorageKey(apiUrl, provider), JSON.stringify(continuation));
    } catch {
      throw new TransportV2ProtocolError(
        "Transport v2 OAuth could not preserve its attested continuation."
      );
    }
  }

  #takeOAuth(
    apiUrl: string,
    serializedPolicy: string,
    provider: TransportV2OAuthProvider,
    state: string
  ): TransportV2Client {
    const storage = sessionStorageOrUndefined();
    if (!storage) {
      throw new TransportV2ProtocolError("Transport v2 OAuth requires same-tab session storage.");
    }
    const key = continuationStorageKey(apiUrl, provider);
    const raw = storage.getItem(key);
    storage.removeItem(key);
    if (!raw) {
      throw new TransportV2ProtocolError("Transport v2 OAuth continuation is missing.");
    }
    const continuation = parseContinuation(raw);
    if (
      continuation.api_url !== apiUrl ||
      continuation.pcr_policy !== serializedPolicy ||
      continuation.provider !== provider ||
      continuation.state !== state
    ) {
      throw new TransportV2ProtocolError("Transport v2 OAuth continuation does not match.");
    }
    return this.#dependencies.restore(
      { apiUrl, fetch: this.#dependencies.fetch },
      continuation.session
    );
  }

  async request(input: TransportV2RuntimeRequest): Promise<TransportV2RuntimeResponse> {
    const identity = this.#identity(input.apiUrl, input.pcrConfig);
    let client: TransportV2Client;
    if (input.oauthCallback) {
      client = this.#takeOAuth(
        identity.apiUrl,
        identity.serializedPolicy,
        input.oauthCallback.provider,
        input.oauthCallback.state
      );
      this.#invalidateScope(identity.scope);
      this.#clients.set(identity.scope, client);
    } else {
      client = await this.#clientFor(identity.apiUrl, identity.policy, input.signal);
    }

    this.#retain(client);
    let logical: TransportV2LogicalResponse;
    try {
      logical = await client.request(input.request, input.signal, input.beforeSend);
    } catch (error) {
      this.#retire(identity.scope, client);
      // A failed or ambiguous send is never replayed. The next independent
      // operation may establish a fresh session.
      throw error;
    } finally {
      this.#release(client);
    }
    const response = await toFetchResponse(logical);
    return {
      response,
      rememberOAuthContinuation: (provider, state) =>
        this.#rememberOAuth(client, identity.apiUrl, identity.serializedPolicy, provider, state)
    };
  }

  async sessionInfo(apiUrl: string, pcrConfig?: PcrConfig): Promise<TransportV2SessionInfo> {
    const client = await this.#clientFor(apiUrl, pcrConfig);
    return {
      protocolVersion: 2,
      sessionId: client.sessionId,
      expiresAtUnixSeconds: Math.floor(client.expiresAtMs / 1000)
    };
  }

  clearScope(apiUrl: string, pcrConfig?: PcrConfig): void {
    const identity = this.#identity(apiUrl, pcrConfig);
    this.#invalidateScope(identity.scope);
  }

  clear(apiUrl?: string): void {
    if (apiUrl === undefined) {
      const scopes = new Set([...this.#clients.keys(), ...this.#establishing.keys()]);
      for (const scope of scopes) this.#invalidateScope(scope);
      return;
    }
    const canonical = canonicalizeTransportV2ApiUrl(apiUrl);
    const scopes = new Set([...this.#clients.keys(), ...this.#establishing.keys()]);
    for (const scope of scopes) {
      if (scope.startsWith(`${canonical}\n`)) {
        this.#invalidateScope(scope);
      }
    }
  }
}

/** @internal Preserves the provider's legacy public shape without exporting V2 traffic keys. */
export async function getTransportV2PublicAttestation(
  runtime: Pick<TransportV2Runtime, "clearScope" | "sessionInfo">,
  apiUrl: string,
  pcrConfig: PcrConfig,
  forceRefresh?: boolean,
  explicitApiUrl?: string,
  explicitPcrConfig?: PcrConfig
): Promise<TransportV2PublicAttestation> {
  const targetApiUrl = explicitApiUrl ?? apiUrl;
  const targetPcrConfig = snapshotPcrConfig(explicitPcrConfig ?? pcrConfig);
  if (forceRefresh) runtime.clearScope(targetApiUrl, targetPcrConfig);
  const session = await runtime.sessionInfo(targetApiUrl, targetPcrConfig);
  return { sessionKey: null, sessionId: session.sessionId };
}

export const transportV2Runtime = new TransportV2Runtime();
