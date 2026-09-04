import { decode, encode } from "@stablelib/base64";

export const TRANSPORT_V2_VERSION = 2 as const;
export const SESSION_ID_BYTES = 16;
export const REQUEST_ID_BYTES = 16;
export const TRAFFIC_KEY_BYTES = 32;
export const CHALLENGE_BYTES = 32;
export const X25519_KEY_BYTES = 32;
export const RECORD_NONCE_BYTES = 12;
export const RECORD_TAG_BYTES = 16;

export const MAX_REQUEST_METADATA_BYTES = 128 * 1024;
export const MAX_REQUEST_BODY_BYTES = 50 * 1024 * 1024;
export const MAX_RESPONSE_CHUNK_BYTES = 64 * 1024;
export const MAX_RESPONSE_METADATA_BYTES = 64 * 1024;
export const MAX_RESPONSE_CIPHERTEXT_BYTES = 1 + MAX_RESPONSE_CHUNK_BYTES + RECORD_TAG_BYTES;

const MAX_CREDENTIAL_BYTES = 16 * 1024;
const MAX_METHOD_BYTES = 32;
const MAX_TARGET_BYTES = 16 * 1024;
const MAX_HEADER_COUNT = 64;
const MAX_RESPONSE_HEADER_COUNT = 32;
const MAX_ERROR_CODE_BYTES = 64;

const HTTP_TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
const LOWERCASE_HEX_32 = /^[0-9a-f]{32}$/;
const ERROR_CODE = /^[a-z0-9_]+$/;
const GATEWAY_HEADERS = new Set([
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
  "x-session-id"
]);

const textEncoder = new TextEncoder();
const fatalTextDecoder = new TextDecoder("utf-8", { fatal: true });

export class TransportV2ProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "TransportV2ProtocolError";
  }
}

export type TransportV2CredentialKind = "bearer" | "api_key" | "resumption";

export interface TransportV2Credential {
  kind: TransportV2CredentialKind;
  value: string;
}

export interface TransportV2Header {
  name: string;
  value: string;
}

export interface TransportV2Request {
  credential?: TransportV2Credential;
  cacheNamespaceRoot?: Uint8Array;
  method: string;
  target: string;
  headers?: readonly TransportV2Header[];
  /** `undefined` means no logical body; an empty Uint8Array is a present empty body. */
  body?: Uint8Array;
}

interface RequestMetadata {
  version: typeof TRANSPORT_V2_VERSION;
  credential: TransportV2Credential | null;
  cache_namespace_root: string | null;
  method: string;
  target: string;
  headers: TransportV2Header[];
  body_present: boolean;
}

export type TransportV2ResponseRecord =
  | { kind: "start"; status: number; headers: TransportV2Header[] }
  | { kind: "chunk"; bytes: Uint8Array }
  | { kind: "end" }
  | { kind: "error"; code: string };

export function concatBytes(...parts: readonly Uint8Array[]): Uint8Array {
  const length = parts.reduce((total, part) => total + part.byteLength, 0);
  const result = new Uint8Array(length);
  let offset = 0;
  for (const part of parts) {
    result.set(part, offset);
    offset += part.byteLength;
  }
  return result;
}

export function utf8(value: string): Uint8Array {
  return textEncoder.encode(value);
}

export function decodeUtf8(value: Uint8Array): string {
  try {
    return fatalTextDecoder.decode(value);
  } catch {
    throw new TransportV2ProtocolError("Transport v2 metadata is not valid UTF-8.");
  }
}

export function bytesToHex(value: Uint8Array): string {
  return Array.from(value, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export function hexToFixedBytes(value: string, expectedBytes: number): Uint8Array {
  if (expectedBytes !== 16 || !LOWERCASE_HEX_32.test(value)) {
    throw new TransportV2ProtocolError("Transport v2 identifier is not canonical lowercase hex.");
  }
  const result = new Uint8Array(expectedBytes);
  for (let index = 0; index < expectedBytes; index += 1) {
    result[index] = Number.parseInt(value.slice(index * 2, index * 2 + 2), 16);
  }
  return result;
}

export function encodeCanonicalBase64(value: Uint8Array): string {
  return encode(value);
}

export function decodeCanonicalBase64(value: unknown, expectedBytes: number): Uint8Array {
  if (typeof value !== "string" || value.length > Math.ceil(expectedBytes / 3) * 4) {
    throw new TransportV2ProtocolError("Transport v2 field is not canonical base64.");
  }
  let decoded: Uint8Array;
  try {
    decoded = decode(value);
  } catch {
    throw new TransportV2ProtocolError("Transport v2 field is not canonical base64.");
  }
  if (decoded.byteLength !== expectedBytes || encode(decoded) !== value) {
    decoded.fill(0);
    throw new TransportV2ProtocolError("Transport v2 field is not canonical base64.");
  }
  return decoded;
}

export function generateRequestId(random: Crypto = globalThis.crypto): Uint8Array {
  if (!random?.getRandomValues) {
    throw new TransportV2ProtocolError("Secure randomness is unavailable.");
  }
  return random.getRandomValues(new Uint8Array(REQUEST_ID_BYTES));
}

export function uint32(value: number): Uint8Array {
  const result = new Uint8Array(4);
  new DataView(result.buffer).setUint32(0, value, false);
  return result;
}

export function uint64(value: bigint): Uint8Array {
  if (value < 0n || value > 0xffff_ffff_ffff_ffffn) {
    throw new TransportV2ProtocolError("Transport v2 sequence is outside the u64 range.");
  }
  const result = new Uint8Array(8);
  new DataView(result.buffer).setBigUint64(0, value, false);
  return result;
}

function validVisibleAscii(value: string, allowTab: boolean): boolean {
  return Array.from(value).every((character) => {
    const code = character.charCodeAt(0);
    return (allowTab && code === 0x09) || (code >= 0x20 && code <= 0x7e);
  });
}

function validateCredential(credential: TransportV2Credential | undefined): void {
  if (!credential) return;
  const valueBytes = utf8(credential.value).byteLength;
  if (
    !["bearer", "api_key", "resumption"].includes(credential.kind) ||
    valueBytes === 0 ||
    valueBytes > MAX_CREDENTIAL_BYTES ||
    !validVisibleAscii(credential.value, false) ||
    credential.value.includes(" ")
  ) {
    throw new TransportV2ProtocolError("Transport v2 credential is invalid.");
  }
}

function validateHeaders(
  headers: readonly TransportV2Header[],
  maximum: number,
  allowGatewayHeaders: boolean
): TransportV2Header[] {
  if (headers.length > maximum) {
    throw new TransportV2ProtocolError("Transport v2 has too many logical headers.");
  }
  return headers.map(({ name, value }) => {
    if (
      !HTTP_TOKEN.test(name) ||
      name !== name.toLowerCase() ||
      (!allowGatewayHeaders && GATEWAY_HEADERS.has(name)) ||
      !validVisibleAscii(value, true)
    ) {
      throw new TransportV2ProtocolError("Transport v2 logical header is invalid.");
    }
    return { name, value };
  });
}

export function encodeRequestEnvelope(request: TransportV2Request): Uint8Array {
  validateCredential(request.credential);
  if (
    utf8(request.method).byteLength === 0 ||
    utf8(request.method).byteLength > MAX_METHOD_BYTES ||
    !HTTP_TOKEN.test(request.method)
  ) {
    throw new TransportV2ProtocolError("Transport v2 method is invalid.");
  }
  if (
    utf8(request.target).byteLength === 0 ||
    utf8(request.target).byteLength > MAX_TARGET_BYTES ||
    !request.target.startsWith("/") ||
    request.target.startsWith("//") ||
    request.target.includes("#") ||
    request.target.includes("\\") ||
    !validVisibleAscii(request.target, false)
  ) {
    throw new TransportV2ProtocolError("Transport v2 relative target is invalid.");
  }
  const headers = validateHeaders(request.headers ?? [], MAX_HEADER_COUNT, false);
  const body = request.body;
  if (body && body.byteLength > MAX_REQUEST_BODY_BYTES) {
    throw new TransportV2ProtocolError("Transport v2 request body is too large.");
  }
  let cacheNamespaceRoot: string | null = null;
  if (request.cacheNamespaceRoot) {
    if (request.cacheNamespaceRoot.byteLength !== 32) {
      throw new TransportV2ProtocolError("Transport v2 cache namespace root is invalid.");
    }
    cacheNamespaceRoot = encodeCanonicalBase64(request.cacheNamespaceRoot);
  }

  const metadata: RequestMetadata = {
    version: TRANSPORT_V2_VERSION,
    credential: request.credential ? { ...request.credential } : null,
    cache_namespace_root: cacheNamespaceRoot,
    method: request.method,
    target: request.target,
    headers,
    body_present: body !== undefined
  };
  const encodedMetadata = utf8(JSON.stringify(metadata));
  if (encodedMetadata.byteLength > MAX_REQUEST_METADATA_BYTES) {
    throw new TransportV2ProtocolError("Transport v2 request metadata is too large.");
  }
  return concatBytes(uint32(encodedMetadata.byteLength), encodedMetadata, body ?? new Uint8Array());
}

function exactObject(value: unknown, keys: readonly string[], description: string) {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new TransportV2ProtocolError(`${description} is not an object.`);
  }
  const object = value as Record<string, unknown>;
  const actual = Object.keys(object).sort();
  const expected = [...keys].sort();
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) {
    throw new TransportV2ProtocolError(`${description} has an unexpected shape.`);
  }
  return object;
}

function parseMetadata(value: Uint8Array, keys: readonly string[], description: string) {
  if (value.byteLength > MAX_RESPONSE_METADATA_BYTES) {
    throw new TransportV2ProtocolError(`${description} is too large.`);
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(decodeUtf8(value));
  } catch (error) {
    if (error instanceof TransportV2ProtocolError) throw error;
    throw new TransportV2ProtocolError(`${description} is not valid JSON.`);
  }
  return exactObject(parsed, keys, description);
}

export function decodeResponseRecord(plaintext: Uint8Array): TransportV2ResponseRecord {
  if (plaintext.byteLength < 1 || plaintext.byteLength > 1 + MAX_RESPONSE_CHUNK_BYTES) {
    throw new TransportV2ProtocolError("Transport v2 response record has an invalid length.");
  }
  const tag = plaintext[0];
  const payload = plaintext.subarray(1);
  if (tag === 1) {
    const object = parseMetadata(payload, ["status", "headers"], "Transport v2 response start");
    const status = object.status;
    if (!Number.isInteger(status) || (status as number) < 200 || (status as number) > 599) {
      throw new TransportV2ProtocolError("Transport v2 response status is invalid.");
    }
    if (!Array.isArray(object.headers)) {
      throw new TransportV2ProtocolError("Transport v2 response headers are invalid.");
    }
    const headers = object.headers.map((value) => {
      const header = exactObject(value, ["name", "value"], "Transport v2 response header");
      if (typeof header.name !== "string" || typeof header.value !== "string") {
        throw new TransportV2ProtocolError("Transport v2 response header is invalid.");
      }
      return { name: header.name, value: header.value };
    });
    return {
      kind: "start",
      status: status as number,
      headers: validateHeaders(headers, MAX_RESPONSE_HEADER_COUNT, false)
    };
  }
  if (tag === 2) return { kind: "chunk", bytes: new Uint8Array(payload) };
  if (tag === 3) {
    if (payload.byteLength !== 0) {
      throw new TransportV2ProtocolError("Transport v2 end record has trailing bytes.");
    }
    return { kind: "end" };
  }
  if (tag === 4) {
    const object = parseMetadata(payload, ["code"], "Transport v2 response error");
    if (
      typeof object.code !== "string" ||
      object.code.length === 0 ||
      utf8(object.code).byteLength > MAX_ERROR_CODE_BYTES ||
      !ERROR_CODE.test(object.code)
    ) {
      throw new TransportV2ProtocolError("Transport v2 response error code is invalid.");
    }
    return { kind: "error", code: object.code };
  }
  throw new TransportV2ProtocolError("Transport v2 response record tag is invalid.");
}
