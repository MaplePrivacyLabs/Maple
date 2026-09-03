import nacl from "tweetnacl";
import awsRootCertDer from "../../assets/aws_root.der";
import {
  authenticateBytes,
  isLocalDevelopmentApiUrl,
  parseDocumentData,
  parseDocumentPayload,
  type AttestationDocument
} from "../attestation";
import { requireTrustedPcr0, snapshotPcrConfig, type PcrConfig } from "../pcr";
import {
  attestationUserData,
  deriveTransportV2SessionKeys,
  type TransportV2SessionKeys
} from "./crypto";
import {
  CHALLENGE_BYTES,
  TRANSPORT_V2_VERSION,
  TransportV2ProtocolError,
  X25519_KEY_BYTES,
  encodeCanonicalBase64,
  hexToFixedBytes,
  type TransportV2Request
} from "./protocol";
import { TransportV2Session, type TransportV2LogicalResponse } from "./session";

const MAX_SESSION_RESPONSE_BYTES = 64 * 1024;
const EXPECTED_SESSION_LIFETIME_SECONDS = 65 * 60;

interface X25519KeyPair {
  publicKey: Uint8Array;
  secretKey: Uint8Array;
}

export interface TransportV2ClientOptions {
  apiUrl: string;
  pcrConfig?: PcrConfig;
  fetch?: typeof globalThis.fetch;
}

/** @internal Dependency boundary for deterministic protocol tests. */
export interface TransportV2ClientDependencies {
  randomBytes: (length: number) => Uint8Array;
  generateKeyPair: () => X25519KeyPair;
  verifyDocument: (
    encodedDocument: string,
    challenge: Uint8Array,
    clientPublicKey: Uint8Array,
    apiUrl: string,
    pcrConfig: PcrConfig
  ) => Promise<Uint8Array>;
}

function equalBytes(left: Uint8Array, right: Uint8Array): boolean {
  if (left.byteLength !== right.byteLength) return false;
  let difference = 0;
  for (let index = 0; index < left.byteLength; index += 1) {
    difference |= left[index] ^ right[index];
  }
  return difference === 0;
}

function canonicalApiUrl(value: string): string {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new TransportV2ProtocolError("Transport v2 requires a valid API URL.");
  }
  if (
    (url.protocol !== "https:" && !isLocalDevelopmentApiUrl(url.toString())) ||
    url.username ||
    url.password ||
    url.search ||
    url.hash
  ) {
    throw new TransportV2ProtocolError(
      "Transport v2 requires HTTPS, except for an exact loopback development URL."
    );
  }
  const path = url.pathname === "/" ? "" : url.pathname.replace(/\/+$/, "");
  return `${url.origin}${path}`;
}

async function localDocument(encodedDocument: string): Promise<AttestationDocument> {
  const parsed = await parseDocumentData(encodedDocument);
  return parseDocumentPayload(parsed.payload);
}

async function verifyDocument(
  encodedDocument: string,
  challenge: Uint8Array,
  clientPublicKey: Uint8Array,
  apiUrl: string,
  pcrConfig: PcrConfig
): Promise<Uint8Array> {
  const local = isLocalDevelopmentApiUrl(apiUrl);
  const document = local
    ? await localDocument(encodedDocument)
    : await authenticateBytes(encodedDocument, awsRootCertDer, challenge);

  if (!document.nonce || !equalBytes(document.nonce, challenge)) {
    throw new TransportV2ProtocolError("Transport v2 attestation challenge does not match.");
  }
  if (!document.public_key || document.public_key.byteLength !== X25519_KEY_BYTES) {
    throw new TransportV2ProtocolError("Transport v2 attestation server key is invalid.");
  }
  const expectedUserData = attestationUserData(clientPublicKey);
  if (!document.user_data || !equalBytes(document.user_data, expectedUserData)) {
    throw new TransportV2ProtocolError("Transport v2 attestation client key binding is invalid.");
  }
  if (!local) {
    await requireTrustedPcr0(document.pcrs, pcrConfig);
  }
  return new Uint8Array(document.public_key);
}

const defaultDependencies: TransportV2ClientDependencies = {
  randomBytes: (length) => globalThis.crypto.getRandomValues(new Uint8Array(length)),
  generateKeyPair: () => nacl.box.keyPair(),
  verifyDocument
};

async function readBoundedBody(response: Response, maximum: number): Promise<Uint8Array> {
  if (!response.body) {
    throw new TransportV2ProtocolError("Transport v2 session response has no body.");
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let length = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      if (!value) continue;
      length += value.byteLength;
      if (length > maximum) {
        await reader.cancel("session response too large");
        throw new TransportV2ProtocolError("Transport v2 session response is too large.");
      }
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  const body = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return body;
}

function parseSessionResponse(body: Uint8Array) {
  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(body));
  } catch {
    throw new TransportV2ProtocolError("Transport v2 session response is invalid JSON.");
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new TransportV2ProtocolError("Transport v2 session response is invalid.");
  }
  const object = value as Record<string, unknown>;
  const keys = Object.keys(object).sort();
  const expected = ["attestation_document", "expires_in_seconds", "session_id", "version"];
  if (keys.length !== expected.length || keys.some((key, index) => key !== expected[index])) {
    throw new TransportV2ProtocolError("Transport v2 session response has an unexpected shape.");
  }
  if (
    object.version !== TRANSPORT_V2_VERSION ||
    typeof object.session_id !== "string" ||
    typeof object.attestation_document !== "string" ||
    !Number.isSafeInteger(object.expires_in_seconds) ||
    object.expires_in_seconds !== EXPECTED_SESSION_LIFETIME_SECONDS
  ) {
    throw new TransportV2ProtocolError("Transport v2 session response is invalid.");
  }
  hexToFixedBytes(object.session_id, 16);
  return {
    sessionId: object.session_id,
    attestationDocument: object.attestation_document,
    expiresInSeconds: object.expires_in_seconds as number
  };
}

function isJsonContentType(value: string | null): boolean {
  return value?.split(";", 1)[0].trim().toLowerCase() === "application/json";
}

/**
 * Dormant Transport V2 client engine. It is deliberately absent from the
 * package entry point until the coordinated SDK cutover.
 */
export class TransportV2Client {
  #apiUrl: string;
  #fetch: typeof globalThis.fetch;
  #session: TransportV2Session;

  private constructor(
    apiUrl: string,
    fetchImplementation: typeof globalThis.fetch,
    session: TransportV2Session
  ) {
    this.#apiUrl = apiUrl;
    this.#fetch = fetchImplementation;
    this.#session = session;
  }

  static async establish(
    options: TransportV2ClientOptions,
    dependencies: TransportV2ClientDependencies = defaultDependencies
  ): Promise<TransportV2Client> {
    const establishmentStartedAtMs = Date.now();
    const apiUrl = canonicalApiUrl(options.apiUrl);
    const fetchImplementation = options.fetch ?? globalThis.fetch;
    const policy = snapshotPcrConfig(options.pcrConfig);
    const challenge = dependencies.randomBytes(CHALLENGE_BYTES);
    if (challenge.byteLength !== CHALLENGE_BYTES) {
      throw new TransportV2ProtocolError("Transport v2 challenge generator returned wrong length.");
    }
    const keyPair = dependencies.generateKeyPair();
    if (
      keyPair.publicKey.byteLength !== X25519_KEY_BYTES ||
      keyPair.secretKey.byteLength !== X25519_KEY_BYTES
    ) {
      keyPair.secretKey.fill(0);
      throw new TransportV2ProtocolError("Transport v2 X25519 key pair is invalid.");
    }

    const requestBody = JSON.stringify({
      version: TRANSPORT_V2_VERSION,
      challenge: encodeCanonicalBase64(challenge),
      client_public_key: encodeCanonicalBase64(keyPair.publicKey)
    });
    let sessionKeys: TransportV2SessionKeys | undefined;
    let serverPublicKey: Uint8Array | undefined;
    let sharedSecret: Uint8Array | undefined;
    try {
      const response = await fetchImplementation(`${apiUrl}/v2/session`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: requestBody,
        credentials: "omit",
        redirect: "error"
      });
      if (
        response.status !== 200 ||
        response.redirected ||
        !isJsonContentType(response.headers.get("content-type"))
      ) {
        await response.body
          ?.cancel("unauthenticated transport-v2 session response")
          .catch(() => {});
        throw new TransportV2ProtocolError(
          "Transport v2 session establishment returned an unauthenticated outer response."
        );
      }
      const body = await readBoundedBody(response, MAX_SESSION_RESPONSE_BYTES);
      const sessionResponse = parseSessionResponse(body);

      // Full Nitro verification and PCR policy enforcement happen before any
      // key derived from this response is trusted or returned to a caller.
      serverPublicKey = await dependencies.verifyDocument(
        sessionResponse.attestationDocument,
        challenge,
        keyPair.publicKey,
        apiUrl,
        policy
      );
      if (serverPublicKey.byteLength !== X25519_KEY_BYTES) {
        throw new TransportV2ProtocolError("Transport v2 attested server key is invalid.");
      }
      sharedSecret = nacl.scalarMult(keyPair.secretKey, serverPublicKey);
      sessionKeys = await deriveTransportV2SessionKeys(sharedSecret, {
        challenge,
        clientPublicKey: keyPair.publicKey,
        serverPublicKey
      });
      if (sessionKeys.sessionId !== sessionResponse.sessionId) {
        throw new TransportV2ProtocolError("Transport v2 derived session ID does not match.");
      }
      const session = new TransportV2Session(
        sessionKeys,
        sessionResponse.expiresInSeconds,
        establishmentStartedAtMs
      );
      return new TransportV2Client(apiUrl, fetchImplementation, session);
    } finally {
      challenge.fill(0);
      keyPair.publicKey.fill(0);
      keyPair.secretKey.fill(0);
      serverPublicKey?.fill(0);
      sharedSecret?.fill(0);
      sessionKeys?.sessionIdBytes.fill(0);
      sessionKeys?.requestKey.fill(0);
      sessionKeys?.responseKey.fill(0);
    }
  }

  async request(request: TransportV2Request): Promise<TransportV2LogicalResponse> {
    const outer = await this.#session.sealRequest(request);
    const response = await this.#fetch(`${this.#apiUrl}${outer.path}`, outer.init);
    return this.#session.openResponse(response, outer.requestId);
  }

  dispose(): void {
    this.#session.dispose();
  }
}
