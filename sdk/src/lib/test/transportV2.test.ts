import { describe, expect, mock, test } from "bun:test";
import nacl from "tweetnacl";
import { TransportV2Client, type TransportV2ClientDependencies } from "../transportV2/client";
import {
  deriveTransportV2SessionKeys,
  encryptTransportV2Request,
  encryptTransportV2ResponseForTesting,
  type TransportV2SessionKeys
} from "../transportV2/crypto";
import {
  TransportV2ProtocolError,
  concatBytes,
  encodeCanonicalBase64,
  encodeRequestEnvelope,
  uint32,
  utf8
} from "../transportV2/protocol";
import { TransportV2Session } from "../transportV2/session";

const vectorTranscript = {
  challenge: new Uint8Array(32).fill(0x11),
  clientPublicKey: new Uint8Array(32).fill(0x22),
  serverPublicKey: new Uint8Array(32).fill(0x33)
};

function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

async function vectorKeys(): Promise<TransportV2SessionKeys> {
  return deriveTransportV2SessionKeys(new Uint8Array(32).fill(0x44), vectorTranscript);
}

function responsePlaintext(
  kind: "start" | "chunk" | "end" | "error",
  value?: Uint8Array | string
): Uint8Array {
  if (kind === "start") {
    return concatBytes(
      new Uint8Array([1]),
      utf8(
        JSON.stringify({
          status: 200,
          headers: [{ name: "content-type", value: "text/event-stream" }]
        })
      )
    );
  }
  if (kind === "chunk") {
    return concatBytes(new Uint8Array([2]), value as Uint8Array);
  }
  if (kind === "end") return new Uint8Array([3]);
  return concatBytes(new Uint8Array([4]), utf8(JSON.stringify({ code: value as string })));
}

async function framedResponse(
  keys: TransportV2SessionKeys,
  requestId: Uint8Array,
  records: Uint8Array[],
  splitEvery = Number.MAX_SAFE_INTEGER
): Promise<Response> {
  const frames: Uint8Array[] = [];
  for (let index = 0; index < records.length; index += 1) {
    const ciphertext = await encryptTransportV2ResponseForTesting(
      keys,
      requestId,
      BigInt(index),
      records[index]
    );
    frames.push(uint32(ciphertext.byteLength), ciphertext);
  }
  const wire = concatBytes(...frames);
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      for (let offset = 0; offset < wire.byteLength; offset += splitEvery) {
        controller.enqueue(wire.slice(offset, Math.min(offset + splitEvery, wire.byteLength)));
      }
      controller.close();
    }
  });
  return new Response(stream, {
    status: 200,
    headers: { "content-type": "application/octet-stream" }
  });
}

async function consume(body: ReadableStream<Uint8Array>): Promise<string> {
  return new Response(body).text();
}

describe("Transport V2 protocol engine", () => {
  test("matches the backend key and record vectors", async () => {
    const keys = await vectorKeys();
    expect(keys.sessionId).toBe("f7258fb103137c612baab47ced4a5a02");
    expect(hex(keys.requestKey)).toBe(
      "00f898a5f2dcd40a703f42221f2a2b842b7e97ed5a555caa362c4153a5e1c491"
    );
    expect(hex(keys.responseKey)).toBe(
      "e4fb003c5c829f5385531eebfdbd0ee3d8430a0bd71322e9f3e41ace915c3190"
    );

    const request = await encryptTransportV2Request(
      keys,
      new Uint8Array(16).fill(0x55),
      utf8("vector plaintext")
    );
    expect(hex(request)).toBe(
      "55555555555555555555555555555555671f5c411205cb00f769e6b2705052b795e91f44516fc6165e16a152e686b209"
    );
    const response = await encryptTransportV2ResponseForTesting(
      keys,
      new Uint8Array(16).fill(0x66),
      0n,
      utf8("vector response")
    );
    expect(hex(response)).toBe("25a2d5ed89864bd7b5e13c83eb49b1f314a70abf8bd7e871b706bb6768c9e1");
  });

  test("puts credential, cache root, target, headers, and raw body inside one record", async () => {
    const keys = await vectorKeys();
    const session = new TransportV2Session(keys, 3900);
    let next = 0;
    const random = {
      getRandomValues<T extends ArrayBufferView | null>(array: T): T {
        if (array instanceof Uint8Array) array.fill(++next);
        return array;
      }
    } as Crypto;

    const request = {
      credential: { kind: "bearer" as const, value: "header.payload.signature" },
      cacheNamespaceRoot: new Uint8Array(32).fill(0x77),
      method: "POST",
      target: "/v1/chat/completions?stream=true",
      headers: [{ name: "content-type", value: "application/json" }],
      body: new Uint8Array([0, 1, 2, 0xff])
    };
    const envelope = encodeRequestEnvelope(request);
    const metadataLength = new DataView(envelope.buffer).getUint32(0, false);
    const metadata = JSON.parse(
      new TextDecoder().decode(envelope.subarray(4, 4 + metadataLength))
    ) as Record<string, unknown>;
    expect(metadata).toMatchObject({
      version: 2,
      credential: request.credential,
      cache_namespace_root: encodeCanonicalBase64(request.cacheNamespaceRoot),
      method: request.method,
      target: request.target,
      headers: request.headers,
      body_present: true
    });
    expect(envelope.subarray(4 + metadataLength)).toEqual(request.body);

    const sealed = await session.sealRequest(request, random);

    expect(sealed.path).toBe("/v2/request");
    expect(Object.keys(sealed.init.headers as Record<string, string>).sort()).toEqual([
      "content-type",
      "x-session-id"
    ]);
    expect((sealed.init.headers as Record<string, string>).authorization).toBeUndefined();
    expect(sealed.init.credentials).toBe("omit");
    expect(sealed.init.redirect).toBe("error");
    expect((sealed.init.body as Uint8Array).subarray(0, 16)).toEqual(new Uint8Array(16).fill(1));
  });

  test("streams ordered chunks and requires one authenticated terminal record", async () => {
    const keys = await vectorKeys();
    const session = new TransportV2Session(keys, 3900);
    const requestId = new Uint8Array(16).fill(0x71);
    const response = await framedResponse(
      keys,
      requestId,
      [
        responsePlaintext("start"),
        responsePlaintext("chunk", utf8("hello ")),
        responsePlaintext("chunk", utf8("world")),
        responsePlaintext("end")
      ],
      3
    );
    const logical = await session.openResponse(response, requestId);
    expect(logical.status).toBe(200);
    expect(logical.headers).toEqual([{ name: "content-type", value: "text/event-stream" }]);
    // The admitted stream owns its derived response opener. Retiring the
    // session must not corrupt an in-flight response.
    session.dispose();
    expect(await consume(logical.body)).toBe("hello world");
  });

  test("rejects EOF, a post-terminal frame, response transplant, and sequence reordering", async () => {
    const keys = await vectorKeys();
    const session = new TransportV2Session(keys, 3900);
    const requestId = new Uint8Array(16).fill(0x72);

    const truncated = await session.openResponse(
      await framedResponse(keys, requestId, [
        responsePlaintext("start"),
        responsePlaintext("chunk", utf8("partial"))
      ]),
      requestId
    );
    await expect(consume(truncated.body)).rejects.toThrow(/terminal/i);

    const extra = await session.openResponse(
      await framedResponse(keys, requestId, [
        responsePlaintext("start"),
        responsePlaintext("end"),
        responsePlaintext("chunk", utf8("late"))
      ]),
      requestId
    );
    await expect(consume(extra.body)).rejects.toThrow(/continued after/i);

    await expect(
      session.openResponse(
        await framedResponse(keys, requestId, [
          responsePlaintext("start"),
          responsePlaintext("end")
        ]),
        new Uint8Array(16).fill(0x73)
      )
    ).rejects.toThrow(/authentication/i);

    const first = await encryptTransportV2ResponseForTesting(
      keys,
      requestId,
      1n,
      responsePlaintext("start")
    );
    const reordered = new Response(concatBytes(uint32(first.byteLength), first), {
      status: 200,
      headers: { "content-type": "application/octet-stream" }
    });
    await expect(session.openResponse(reordered, requestId)).rejects.toThrow(/authentication/i);
  });

  test("establishes only after attestation verification and sends no outer credentials", async () => {
    const challenge = new Uint8Array(32).fill(0x81);
    const clientSecret = new Uint8Array(32).fill(0x22);
    const clientKeyPair = nacl.box.keyPair.fromSecretKey(clientSecret);
    const serverSecret = new Uint8Array(32).fill(0x33);
    const serverPublicKey = nacl.scalarMult.base(serverSecret);
    let verified = false;
    let observedInit: RequestInit | undefined;

    const dependencies: TransportV2ClientDependencies = {
      randomBytes: () => new Uint8Array(challenge),
      generateKeyPair: () => ({
        publicKey: new Uint8Array(clientKeyPair.publicKey),
        secretKey: new Uint8Array(clientSecret)
      }),
      verifyDocument: mock(async (_document, actualChallenge, actualClientKey) => {
        expect(actualChallenge).toEqual(challenge);
        expect(actualClientKey).toEqual(clientKeyPair.publicKey);
        verified = true;
        return new Uint8Array(serverPublicKey);
      })
    };
    const fetchMock = mock(async (_input: string | URL | Request, init?: RequestInit) => {
      observedInit = init;
      const body = JSON.parse(init?.body as string) as {
        challenge: string;
        client_public_key: string;
      };
      expect(body.challenge).toBe(encodeCanonicalBase64(challenge));
      expect(body.client_public_key).toBe(encodeCanonicalBase64(clientKeyPair.publicKey));
      const sharedSecret = nacl.scalarMult(serverSecret, clientKeyPair.publicKey);
      const keys = await deriveTransportV2SessionKeys(sharedSecret, {
        challenge,
        clientPublicKey: clientKeyPair.publicKey,
        serverPublicKey
      });
      return Response.json({
        version: 2,
        session_id: keys.sessionId,
        attestation_document: "verified-by-test-boundary",
        expires_in_seconds: 3900
      });
    });

    const client = await TransportV2Client.establish(
      {
        apiUrl: "https://enclave.example.test",
        pcrConfig: { remoteAttestation: false },
        fetch: fetchMock as typeof fetch
      },
      dependencies
    );
    expect(verified).toBe(true);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(Object.keys(observedInit?.headers as Record<string, string>)).toEqual(["content-type"]);
    expect((observedInit?.headers as Record<string, string>).authorization).toBeUndefined();
    expect(observedInit?.credentials).toBe("omit");
    expect(observedInit?.redirect).toBe("error");
    client.dispose();
  });

  test("does not derive or return a session when attestation verification fails", async () => {
    const keyPair = nacl.box.keyPair();
    const dependencies: TransportV2ClientDependencies = {
      randomBytes: () => new Uint8Array(32).fill(0x91),
      generateKeyPair: () => ({
        publicKey: new Uint8Array(keyPair.publicKey),
        secretKey: new Uint8Array(keyPair.secretKey)
      }),
      verifyDocument: async () => {
        throw new Error("untrusted PCR0");
      }
    };
    const fetchMock = mock(async () =>
      Response.json({
        version: 2,
        session_id: "00000000000000000000000000000000",
        attestation_document: "untrusted",
        expires_in_seconds: 3900
      })
    );
    await expect(
      TransportV2Client.establish(
        { apiUrl: "https://enclave.example.test", fetch: fetchMock as typeof fetch },
        dependencies
      )
    ).rejects.toThrow("untrusted PCR0");
  });

  test("rejects an untrusted lifetime and expires conservatively from handshake start", async () => {
    const keyPair = nacl.box.keyPair();
    const verifyDocument = mock(async () => new Uint8Array(32).fill(1));
    const dependencies: TransportV2ClientDependencies = {
      randomBytes: () => new Uint8Array(32).fill(0x92),
      generateKeyPair: () => ({
        publicKey: new Uint8Array(keyPair.publicKey),
        secretKey: new Uint8Array(keyPair.secretKey)
      }),
      verifyDocument
    };
    await expect(
      TransportV2Client.establish(
        {
          apiUrl: "https://enclave.example.test",
          fetch: mock(async () =>
            Response.json({
              version: 2,
              session_id: "00000000000000000000000000000000",
              attestation_document: "not-yet-verified",
              expires_in_seconds: 3901
            })
          ) as typeof fetch
        },
        dependencies
      )
    ).rejects.toThrow(/session response is invalid/i);
    expect(verifyDocument).not.toHaveBeenCalled();

    const keys = await vectorKeys();
    expect(() => new TransportV2Session(keys, 3900, Date.now() - (3900 * 1000 - 29_000))).toThrow(
      /expired during establishment/i
    );
  });

  test("fails closed on an unauthenticated outer response without a V1 fallback", async () => {
    const keys = await vectorKeys();
    const session = new TransportV2Session(keys, 3900);
    await expect(
      session.openResponse(
        new Response("legacy plaintext", {
          status: 404,
          headers: { "content-type": "text/plain" }
        }),
        new Uint8Array(16).fill(0xa1)
      )
    ).rejects.toBeInstanceOf(TransportV2ProtocolError);
  });
});
