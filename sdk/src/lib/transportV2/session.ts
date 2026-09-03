import {
  createTransportV2ResponseOpener,
  encryptTransportV2Request,
  type TransportV2ResponseOpener,
  type TransportV2SessionKeys
} from "./crypto";
import {
  MAX_RESPONSE_CIPHERTEXT_BYTES,
  RECORD_TAG_BYTES,
  TransportV2ProtocolError,
  decodeResponseRecord,
  encodeRequestEnvelope,
  generateRequestId,
  type TransportV2Header,
  type TransportV2Request
} from "./protocol";

const OUTER_CONTENT_TYPE = "application/octet-stream";
const SESSION_EXPIRY_SKEW_MS = 30_000;

export class TransportV2RemoteError extends Error {
  readonly code: string;

  constructor(code: string) {
    super(`Transport v2 response ended with ${code}.`);
    this.name = "TransportV2RemoteError";
    this.code = code;
  }
}

export interface TransportV2LogicalResponse {
  requestId: Uint8Array;
  status: number;
  headers: TransportV2Header[];
  /** Consuming this stream verifies ordered records and authenticated finality. */
  body: ReadableStream<Uint8Array>;
}

export interface TransportV2OuterRequest {
  requestId: Uint8Array;
  path: "/v2/request";
  init: RequestInit;
}

class ByteQueue {
  #chunks: Uint8Array[] = [];
  #headIndex = 0;
  #headOffset = 0;
  #length = 0;

  get length(): number {
    return this.#length;
  }

  push(chunk: Uint8Array): void {
    if (chunk.byteLength === 0) return;
    this.#chunks.push(chunk);
    this.#length += chunk.byteLength;
  }

  take(length: number): Uint8Array {
    if (length > this.#length) {
      throw new TransportV2ProtocolError("Transport v2 response frame is truncated.");
    }
    const result = new Uint8Array(length);
    let outputOffset = 0;
    while (outputOffset < length) {
      const head = this.#chunks[this.#headIndex];
      const available = head.byteLength - this.#headOffset;
      const count = Math.min(length - outputOffset, available);
      result.set(head.subarray(this.#headOffset, this.#headOffset + count), outputOffset);
      outputOffset += count;
      this.#headOffset += count;
      this.#length -= count;
      if (this.#headOffset === head.byteLength) {
        this.#headIndex += 1;
        this.#headOffset = 0;
        if (this.#headIndex === this.#chunks.length) {
          this.#chunks = [];
          this.#headIndex = 0;
        } else if (this.#headIndex >= 64 && this.#headIndex * 2 >= this.#chunks.length) {
          this.#chunks = this.#chunks.slice(this.#headIndex);
          this.#headIndex = 0;
        }
      }
    }
    return result;
  }
}

class CiphertextFrameReader {
  #reader: ReadableStreamDefaultReader<Uint8Array>;
  #queue = new ByteQueue();
  #eof = false;
  #released = false;

  constructor(body: ReadableStream<Uint8Array>) {
    this.#reader = body.getReader();
  }

  async #fill(minimum: number): Promise<void> {
    while (this.#queue.length < minimum && !this.#eof) {
      const { done, value } = await this.#reader.read();
      if (done) {
        this.#eof = true;
      } else if (value) {
        this.#queue.push(value);
      }
    }
  }

  async next(): Promise<Uint8Array | null> {
    await this.#fill(4);
    if (this.#queue.length === 0 && this.#eof) return null;
    if (this.#queue.length < 4) {
      throw new TransportV2ProtocolError("Transport v2 response frame prefix is truncated.");
    }
    const prefix = this.#queue.take(4);
    const length = new DataView(prefix.buffer).getUint32(0, false);
    if (length < RECORD_TAG_BYTES || length > MAX_RESPONSE_CIPHERTEXT_BYTES) {
      throw new TransportV2ProtocolError("Transport v2 response frame length is invalid.");
    }
    await this.#fill(length);
    if (this.#queue.length < length) {
      throw new TransportV2ProtocolError("Transport v2 response ciphertext is truncated.");
    }
    return this.#queue.take(length);
  }

  async requireEof(): Promise<void> {
    if ((await this.next()) !== null) {
      throw new TransportV2ProtocolError(
        "Transport v2 response continued after its terminal record."
      );
    }
    this.#release();
  }

  async cancel(reason?: unknown): Promise<void> {
    if (this.#released) return;
    try {
      await this.#reader.cancel(reason);
    } finally {
      this.#release();
    }
  }

  #release(): void {
    if (this.#released) return;
    this.#released = true;
    this.#reader.releaseLock();
  }
}

/**
 * An established, attested Transport V2 session.
 *
 * This class is intentionally not exported from the package entry point yet.
 * It has no V1 fallback and performs no automatic retry.
 */
export class TransportV2Session {
  #keys: TransportV2SessionKeys;
  #expiresAtMs: number;
  #disposed = false;

  constructor(
    keys: TransportV2SessionKeys,
    expiresInSeconds: number,
    establishmentStartedAtMs = Date.now()
  ) {
    if (!Number.isSafeInteger(expiresInSeconds) || expiresInSeconds <= 0) {
      throw new TransportV2ProtocolError("Transport v2 session lifetime is invalid.");
    }
    const expiresAtMs = establishmentStartedAtMs + expiresInSeconds * 1000 - SESSION_EXPIRY_SKEW_MS;
    if (!Number.isSafeInteger(expiresAtMs) || expiresAtMs <= Date.now()) {
      throw new TransportV2ProtocolError("Transport v2 session expired during establishment.");
    }
    this.#keys = {
      sessionId: keys.sessionId,
      sessionIdBytes: new Uint8Array(keys.sessionIdBytes),
      requestKey: new Uint8Array(keys.requestKey),
      responseKey: new Uint8Array(keys.responseKey)
    };
    this.#expiresAtMs = expiresAtMs;
  }

  get sessionId(): string {
    this.#requireActive();
    return this.#keys.sessionId;
  }

  #requireActive(): void {
    if (this.#disposed) {
      throw new TransportV2ProtocolError("Transport v2 session is disposed.");
    }
    if (Date.now() >= this.#expiresAtMs) {
      this.dispose();
      throw new TransportV2ProtocolError("Transport v2 session is expired.");
    }
  }

  async sealRequest(
    request: TransportV2Request,
    random: Crypto = globalThis.crypto
  ): Promise<TransportV2OuterRequest> {
    this.#requireActive();
    const requestId = generateRequestId(random);
    const plaintext = encodeRequestEnvelope(request);
    try {
      const body = await encryptTransportV2Request(this.#keys, requestId, plaintext);
      return {
        requestId,
        path: "/v2/request",
        init: {
          method: "POST",
          headers: {
            "content-type": OUTER_CONTENT_TYPE,
            "x-session-id": this.#keys.sessionId
          },
          body,
          credentials: "omit",
          redirect: "error"
        }
      };
    } finally {
      plaintext.fill(0);
    }
  }

  async openResponse(
    response: Response,
    requestId: Uint8Array
  ): Promise<TransportV2LogicalResponse> {
    this.#requireActive();
    if (
      response.status !== 200 ||
      response.redirected ||
      response.headers.get("content-type") !== OUTER_CONTENT_TYPE ||
      !response.body
    ) {
      await response.body?.cancel("unauthenticated transport-v2 outer response").catch(() => {});
      throw new TransportV2ProtocolError(
        "Transport v2 returned an unauthenticated outer response."
      );
    }

    const reader = new CiphertextFrameReader(response.body);
    let opener: TransportV2ResponseOpener;
    try {
      opener = await createTransportV2ResponseOpener(this.#keys, requestId);
    } catch (error) {
      await reader.cancel(error).catch(() => {});
      throw error;
    }
    const readRecord = async () => {
      const ciphertext = await reader.next();
      if (!ciphertext) {
        throw new TransportV2ProtocolError(
          "Transport v2 response ended without a terminal record."
        );
      }
      const plaintext = opener.openNext(ciphertext);
      try {
        return decodeResponseRecord(plaintext);
      } finally {
        plaintext.fill(0);
      }
    };

    let start;
    try {
      start = await readRecord();
    } catch (error) {
      opener.dispose();
      await reader.cancel(error).catch(() => {});
      throw error;
    }
    if (start.kind !== "start") {
      opener.dispose();
      await reader.cancel("response did not start with Start").catch(() => {});
      throw new TransportV2ProtocolError("Transport v2 response did not begin with Start.");
    }

    let pulling = false;
    const body = new ReadableStream<Uint8Array>({
      pull: async (controller) => {
        if (pulling) return;
        pulling = true;
        try {
          while (true) {
            const record = await readRecord();
            if (record.kind === "chunk") {
              if (record.bytes.byteLength === 0) continue;
              controller.enqueue(record.bytes);
              return;
            }
            if (record.kind === "start") {
              throw new TransportV2ProtocolError("Transport v2 response contains a second Start.");
            }
            await reader.requireEof();
            opener.dispose();
            if (record.kind === "end") {
              controller.close();
            } else {
              controller.error(new TransportV2RemoteError(record.code));
            }
            return;
          }
        } catch (error) {
          opener.dispose();
          controller.error(error);
          await reader.cancel(error).catch(() => {});
        } finally {
          pulling = false;
        }
      },
      cancel: (reason) => {
        opener.dispose();
        return reader.cancel(reason);
      }
    });

    return {
      requestId: new Uint8Array(requestId),
      status: start.status,
      headers: start.headers,
      body
    };
  }

  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    this.#keys.sessionIdBytes.fill(0);
    this.#keys.requestKey.fill(0);
    this.#keys.responseKey.fill(0);
  }
}
