import { serializePcrConfig, snapshotPcrConfig, type PcrConfig } from "../pcr";
import {
  TransportV2AuthorityChangedError,
  clearTransportV2CredentialsIfCurrent,
  installTransportV2Credentials,
  isTransportV2AuthSnapshotCurrent,
  readTransportV2Credentials,
  type StoredTransportV2Credentials,
  type TransportV2AuthKind,
  type TransportV2AuthSnapshot
} from "./auth";
import { canonicalizeTransportV2ApiUrl } from "./client";
import { utf8, type TransportV2Credential } from "./protocol";
import { transportV2Runtime, type TransportV2Runtime } from "./runtime";

const REFRESH_SKEW_SECONDS = 30;
const ERROR_CONTRACT_HEADER = "x-opensecret-error-contract";
const ERROR_CODE_HEADER = "x-opensecret-error-code";
const ACCESS_TOKEN_EXPIRED = "access_token_expired";

interface RefreshResponse {
  access_token: string;
  refresh_token: string;
}

export interface TransportV2Authority {
  credential: TransportV2Credential;
  snapshot: TransportV2AuthSnapshot;
  credentials: StoredTransportV2Credentials;
  assertCurrent(): void;
}

export interface TransportV2AuthRuntimeDependencies {
  runtime: TransportV2Runtime;
  nowUnixSeconds(): number;
}

const defaultDependencies: TransportV2AuthRuntimeDependencies = {
  runtime: transportV2Runtime,
  nowUnixSeconds: () => Math.floor(Date.now() / 1000)
};

export function snapshotTransportV2AuthorityScope(
  apiUrl: string,
  pcrConfig: PcrConfig | undefined,
  kind: TransportV2AuthKind
) {
  const canonicalApiUrl = canonicalizeTransportV2ApiUrl(apiUrl);
  const policy = snapshotPcrConfig(pcrConfig);
  return {
    apiUrl: canonicalApiUrl,
    pcrConfig: policy,
    key: `${kind}\n${canonicalApiUrl}\n${serializePcrConfig(policy)}`
  };
}

function refreshPath(kind: TransportV2AuthKind): string {
  return kind === "user" ? "/refresh" : "/platform/refresh";
}

function responseHasAccessExpired(response: Response): boolean {
  return (
    response.status === 401 &&
    response.headers.get(ERROR_CONTRACT_HEADER) === "1" &&
    response.headers.get(ERROR_CODE_HEADER) === ACCESS_TOKEN_EXPIRED
  );
}

async function parseRefreshResponse(response: Response): Promise<RefreshResponse> {
  let value: unknown;
  try {
    value = await response.json();
  } catch {
    throw new Error("Transport v2 refresh returned an invalid response.");
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("Transport v2 refresh returned an invalid response.");
  }
  const object = value as Record<string, unknown>;
  if (typeof object.access_token !== "string" || typeof object.refresh_token !== "string") {
    throw new Error("Transport v2 refresh returned invalid credentials.");
  }
  return { access_token: object.access_token, refresh_token: object.refresh_token };
}

/**
 * Authentication lifecycle independent of the cryptographic session. Refresh
 * is a separate request and never causes an already-sent request to be replayed.
 */
export class TransportV2AuthRuntime {
  #dependencies: TransportV2AuthRuntimeDependencies;
  #refreshes = new Map<string, Promise<StoredTransportV2Credentials>>();

  constructor(dependencies: TransportV2AuthRuntimeDependencies = defaultDependencies) {
    this.#dependencies = dependencies;
  }

  async #refreshSnapshot(
    apiUrl: string,
    pcrConfig: PcrConfig | undefined,
    kind: TransportV2AuthKind,
    credentials: StoredTransportV2Credentials
  ): Promise<StoredTransportV2Credentials> {
    const snapshot: TransportV2AuthSnapshot = {
      apiOrigin: credentials.apiOrigin,
      kind,
      principalId: credentials.principalId,
      revision: credentials.revision
    };
    if (!isTransportV2AuthSnapshotCurrent(snapshot)) {
      throw new Error("Transport v2 authentication state changed before refresh.");
    }
    const policy = snapshotPcrConfig(pcrConfig);
    const scope = `${canonicalizeTransportV2ApiUrl(apiUrl)}\n${serializePcrConfig(policy)}\n${credentials.apiOrigin}\n${kind}\n${credentials.revision}`;
    let pending = this.#refreshes.get(scope);
    if (!pending) {
      pending = (async () => {
        const target = refreshPath(kind);
        const { response } = await this.#dependencies.runtime.request({
          apiUrl,
          pcrConfig: policy,
          beforeSend: () => {
            if (!isTransportV2AuthSnapshotCurrent(snapshot)) {
              throw new TransportV2AuthorityChangedError();
            }
          },
          request: {
            method: "POST",
            target,
            credential: { kind: "resumption", value: credentials.refreshToken }
          }
        });

        if (!response.ok) {
          // Only a definitive, authenticated refresh rejection invalidates the
          // local bearer. Transient and protocol failures preserve it.
          if (response.status === 401 || response.status === 403) {
            clearTransportV2CredentialsIfCurrent(snapshot);
          }
          let detail = `Transport v2 refresh failed with status ${response.status}.`;
          try {
            const body = await response.text();
            if (body) detail = body;
          } catch {
            // The authenticated status remains sufficient for classification.
          }
          throw new Error(detail);
        }

        const refreshed = await parseRefreshResponse(response);
        return installTransportV2Credentials(
          apiUrl,
          kind,
          refreshed.access_token,
          refreshed.refresh_token,
          snapshot
        );
      })();
      this.#refreshes.set(scope, pending);
    }
    try {
      return await pending;
    } finally {
      if (this.#refreshes.get(scope) === pending) this.#refreshes.delete(scope);
    }
  }

  async authority(
    apiUrl: string,
    pcrConfig: PcrConfig | undefined,
    kind: TransportV2AuthKind
  ): Promise<TransportV2Authority> {
    let credentials = readTransportV2Credentials(apiUrl, kind);
    if (!credentials) {
      throw new Error(
        kind === "user" ? "No access token available" : "No platform access token available"
      );
    }
    if (
      credentials.accessExpiresAtUnixSeconds <=
      this.#dependencies.nowUnixSeconds() + REFRESH_SKEW_SECONDS
    ) {
      // This happens before the application request is sent. A transient
      // refresh failure is returned to the caller and the original request is
      // never transmitted under a token known locally to be near expiry.
      credentials = await this.#refreshSnapshot(apiUrl, pcrConfig, kind, credentials);
    }
    const snapshot: TransportV2AuthSnapshot = {
      apiOrigin: credentials.apiOrigin,
      kind,
      principalId: credentials.principalId,
      revision: credentials.revision
    };
    return {
      credential: { kind: "bearer", value: credentials.accessToken },
      credentials,
      snapshot,
      assertCurrent() {
        if (!isTransportV2AuthSnapshotCurrent(snapshot)) {
          throw new TransportV2AuthorityChangedError();
        }
      }
    };
  }

  async refresh(
    apiUrl: string,
    pcrConfig: PcrConfig | undefined,
    kind: TransportV2AuthKind
  ): Promise<StoredTransportV2Credentials> {
    const credentials = readTransportV2Credentials(apiUrl, kind);
    if (!credentials) throw new Error("No refresh token available");
    return this.#refreshSnapshot(apiUrl, pcrConfig, kind, credentials);
  }

  /**
   * An authenticated expiry response can refresh credentials for a later
   * operation, but the response that triggered it is always returned as-is.
   */
  noteResponse(
    response: Response,
    apiUrl: string,
    pcrConfig: PcrConfig | undefined,
    kind: TransportV2AuthKind,
    sent: TransportV2Authority
  ): void {
    if (!responseHasAccessExpired(response)) return;
    void this.#refreshSnapshot(apiUrl, pcrConfig, kind, sent.credentials).catch(() => {
      // The original authenticated 401 is authoritative for this operation.
      // Refresh failure is deliberately not substituted for it.
    });
  }
}

export const transportV2AuthRuntime = new TransportV2AuthRuntime();

export function jsonBody(value: unknown): Uint8Array {
  return utf8(JSON.stringify(value));
}
