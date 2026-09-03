import { ChaCha20Poly1305 } from "@stablelib/chacha20poly1305";
import {
  RECORD_NONCE_BYTES,
  RECORD_TAG_BYTES,
  REQUEST_ID_BYTES,
  SESSION_ID_BYTES,
  TRAFFIC_KEY_BYTES,
  TransportV2ProtocolError,
  bytesToHex,
  concatBytes,
  uint64,
  utf8
} from "./protocol";

const HANDSHAKE_DOMAIN = utf8("opensecret/transport-v2/session/v1");
export const ATTESTATION_USER_DATA_DOMAIN = utf8(
  "opensecret/transport-v2/session/v1/client-public-key"
);
const REQUEST_KEY_INFO = utf8("opensecret/transport-v2/request-key/v1");
const RESPONSE_KEY_INFO = utf8("opensecret/transport-v2/response-key/v1");
const SESSION_ID_INFO = utf8("opensecret/transport-v2/session-id/v1");
const REQUEST_SUBKEY_INFO = utf8("opensecret/transport-v2/request-subkey/v1");
const RESPONSE_SUBKEY_INFO = utf8("opensecret/transport-v2/response-subkey/v1");
const REQUEST_RECORD_DOMAIN = utf8("opensecret/transport-v2/request-record/v1");
const RESPONSE_RECORD_DOMAIN = utf8("opensecret/transport-v2/response-record/v1");
const ZERO = new Uint8Array([0]);
const ZERO_NONCE = new Uint8Array(RECORD_NONCE_BYTES);

export interface TransportV2Transcript {
  challenge: Uint8Array;
  clientPublicKey: Uint8Array;
  serverPublicKey: Uint8Array;
}

export interface TransportV2SessionKeys {
  sessionId: string;
  sessionIdBytes: Uint8Array;
  requestKey: Uint8Array;
  responseKey: Uint8Array;
}

function requireLength(value: Uint8Array, length: number, description: string): void {
  if (value.byteLength !== length) {
    throw new TransportV2ProtocolError(`${description} has an invalid length.`);
  }
}

async function digestTranscript(
  transcript: TransportV2Transcript,
  subtle: SubtleCrypto
): Promise<Uint8Array> {
  requireLength(transcript.challenge, 32, "Transport v2 challenge");
  requireLength(transcript.clientPublicKey, 32, "Transport v2 client public key");
  requireLength(transcript.serverPublicKey, 32, "Transport v2 server public key");
  return new Uint8Array(
    await subtle.digest(
      "SHA-256",
      concatBytes(
        HANDSHAKE_DOMAIN,
        ZERO,
        transcript.challenge,
        transcript.clientPublicKey,
        transcript.serverPublicKey
      )
    )
  );
}

async function hkdf(
  inputKeyMaterial: Uint8Array,
  salt: Uint8Array,
  info: Uint8Array,
  length: number,
  subtle: SubtleCrypto
): Promise<Uint8Array> {
  try {
    const key = await subtle.importKey("raw", inputKeyMaterial, "HKDF", false, ["deriveBits"]);
    const bits = await subtle.deriveBits(
      { name: "HKDF", hash: "SHA-256", salt, info },
      key,
      length * 8
    );
    return new Uint8Array(bits);
  } catch {
    throw new TransportV2ProtocolError("Transport v2 key derivation failed.");
  }
}

async function hkdfExpandPrk(
  prk: Uint8Array,
  info: Uint8Array,
  subtle: SubtleCrypto
): Promise<Uint8Array> {
  requireLength(prk, TRAFFIC_KEY_BYTES, "Transport v2 traffic key");
  try {
    const key = await subtle.importKey("raw", prk, { name: "HMAC", hash: "SHA-256" }, false, [
      "sign"
    ]);
    // One SHA-256 block is enough for the 32-byte record subkey. This is
    // RFC 5869 HKDF-Expand T(1) = HMAC(PRK, info || 0x01).
    const block = await subtle.sign("HMAC", key, concatBytes(info, new Uint8Array([1])));
    return new Uint8Array(block);
  } catch {
    throw new TransportV2ProtocolError("Transport v2 record-key derivation failed.");
  }
}

export async function deriveTransportV2SessionKeys(
  sharedSecret: Uint8Array,
  transcript: TransportV2Transcript,
  subtle: SubtleCrypto = globalThis.crypto.subtle
): Promise<TransportV2SessionKeys> {
  requireLength(sharedSecret, TRAFFIC_KEY_BYTES, "Transport v2 shared secret");
  if (sharedSecret.every((byte) => byte === 0)) {
    throw new TransportV2ProtocolError("Transport v2 X25519 exchange is non-contributory.");
  }
  const digest = await digestTranscript(transcript, subtle);
  try {
    const requestInfo = concatBytes(REQUEST_KEY_INFO, ZERO, digest);
    const responseInfo = concatBytes(RESPONSE_KEY_INFO, ZERO, digest);
    const sessionInfo = concatBytes(SESSION_ID_INFO, ZERO, digest);
    const requestKey = await hkdf(
      sharedSecret,
      transcript.challenge,
      requestInfo,
      TRAFFIC_KEY_BYTES,
      subtle
    );
    try {
      const responseKey = await hkdf(
        sharedSecret,
        transcript.challenge,
        responseInfo,
        TRAFFIC_KEY_BYTES,
        subtle
      );
      try {
        const sessionIdBytes = await hkdf(
          sharedSecret,
          transcript.challenge,
          sessionInfo,
          SESSION_ID_BYTES,
          subtle
        );
        return {
          sessionId: bytesToHex(sessionIdBytes),
          sessionIdBytes,
          requestKey,
          responseKey
        };
      } catch (error) {
        responseKey.fill(0);
        throw error;
      }
    } catch (error) {
      requestKey.fill(0);
      throw error;
    }
  } finally {
    digest.fill(0);
  }
}

export function attestationUserData(clientPublicKey: Uint8Array): Uint8Array {
  requireLength(clientPublicKey, 32, "Transport v2 client public key");
  return concatBytes(ATTESTATION_USER_DATA_DOMAIN, ZERO, clientPublicKey);
}

function requestAad(sessionId: Uint8Array, requestId: Uint8Array): Uint8Array {
  return concatBytes(REQUEST_RECORD_DOMAIN, ZERO, sessionId, requestId);
}

function responseAad(sessionId: Uint8Array, requestId: Uint8Array, sequence: bigint): Uint8Array {
  return concatBytes(RESPONSE_RECORD_DOMAIN, ZERO, sessionId, requestId, uint64(sequence));
}

async function recordSubkey(
  trafficKey: Uint8Array,
  label: Uint8Array,
  sessionId: Uint8Array,
  requestId: Uint8Array,
  subtle: SubtleCrypto
): Promise<Uint8Array> {
  requireLength(sessionId, SESSION_ID_BYTES, "Transport v2 session ID");
  requireLength(requestId, REQUEST_ID_BYTES, "Transport v2 request ID");
  return hkdfExpandPrk(trafficKey, concatBytes(label, ZERO, sessionId, requestId), subtle);
}

function seal(key: Uint8Array, nonce: Uint8Array, plaintext: Uint8Array, aad: Uint8Array) {
  const cipher = new ChaCha20Poly1305(key);
  try {
    return cipher.seal(nonce, plaintext, aad);
  } finally {
    cipher.clean();
  }
}

function open(key: Uint8Array, nonce: Uint8Array, ciphertext: Uint8Array, aad: Uint8Array) {
  if (ciphertext.byteLength < RECORD_TAG_BYTES) {
    throw new TransportV2ProtocolError("Transport v2 record is truncated.");
  }
  const cipher = new ChaCha20Poly1305(key);
  try {
    const plaintext = cipher.open(nonce, ciphertext, aad);
    if (!plaintext) {
      throw new TransportV2ProtocolError("Transport v2 record authentication failed.");
    }
    return plaintext;
  } finally {
    cipher.clean();
  }
}

export async function encryptTransportV2Request(
  keys: TransportV2SessionKeys,
  requestId: Uint8Array,
  plaintext: Uint8Array,
  subtle: SubtleCrypto = globalThis.crypto.subtle
): Promise<Uint8Array> {
  const key = await recordSubkey(
    keys.requestKey,
    REQUEST_SUBKEY_INFO,
    keys.sessionIdBytes,
    requestId,
    subtle
  );
  try {
    return concatBytes(
      requestId,
      seal(key, ZERO_NONCE, plaintext, requestAad(keys.sessionIdBytes, requestId))
    );
  } finally {
    key.fill(0);
  }
}

export class TransportV2ResponseOpener {
  #key: Uint8Array;
  #sessionId: Uint8Array;
  #requestId: Uint8Array;
  #sequence = 0n;
  #disposed = false;

  constructor(key: Uint8Array, sessionId: Uint8Array, requestId: Uint8Array) {
    this.#key = key;
    this.#sessionId = new Uint8Array(sessionId);
    this.#requestId = new Uint8Array(requestId);
  }

  openNext(ciphertext: Uint8Array): Uint8Array {
    if (this.#disposed) {
      throw new TransportV2ProtocolError("Transport v2 response opener is disposed.");
    }
    const sequence = this.#sequence;
    this.#sequence += 1n;
    const nonce = new Uint8Array(RECORD_NONCE_BYTES);
    nonce.set(uint64(sequence), 4);
    return open(
      this.#key,
      nonce,
      ciphertext,
      responseAad(this.#sessionId, this.#requestId, sequence)
    );
  }

  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    this.#key.fill(0);
    this.#sessionId.fill(0);
    this.#requestId.fill(0);
  }
}

export async function createTransportV2ResponseOpener(
  keys: TransportV2SessionKeys,
  requestId: Uint8Array,
  subtle: SubtleCrypto = globalThis.crypto.subtle
): Promise<TransportV2ResponseOpener> {
  const key = await recordSubkey(
    keys.responseKey,
    RESPONSE_SUBKEY_INFO,
    keys.sessionIdBytes,
    requestId,
    subtle
  );
  return new TransportV2ResponseOpener(key, keys.sessionIdBytes, requestId);
}

/** @internal Test-vector helper; production response encryption lives in the enclave. */
export async function encryptTransportV2ResponseForTesting(
  keys: TransportV2SessionKeys,
  requestId: Uint8Array,
  sequence: bigint,
  plaintext: Uint8Array,
  subtle: SubtleCrypto = globalThis.crypto.subtle
): Promise<Uint8Array> {
  const key = await recordSubkey(
    keys.responseKey,
    RESPONSE_SUBKEY_INFO,
    keys.sessionIdBytes,
    requestId,
    subtle
  );
  try {
    const nonce = new Uint8Array(RECORD_NONCE_BYTES);
    nonce.set(uint64(sequence), 4);
    return seal(key, nonce, plaintext, responseAad(keys.sessionIdBytes, requestId, sequence));
  } finally {
    key.fill(0);
  }
}
