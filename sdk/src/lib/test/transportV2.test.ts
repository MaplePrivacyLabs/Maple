import { describe, expect, mock, test } from "bun:test";
import nacl from "tweetnacl";
import type { AttestationDocument } from "../attestation";
import type { PcrConfig } from "../pcr";
import {
  TransportV2Client,
  verifyTransportV2AttestationDocumentWithDependencies,
  type TransportV2AttestationVerificationDependencies,
  type TransportV2ClientDependencies
} from "../transportV2/client";
import {
  attestationUserData,
  deriveTransportV2SessionKeys,
  encryptTransportV2Request,
  encryptTransportV2ResponseForTesting,
  type TransportV2SessionKeys
} from "../transportV2/crypto";
import {
  TransportV2ProtocolError,
  concatBytes,
  decodeResponseRecord,
  encodeCanonicalBase64,
  encodeRequestEnvelope,
  uint32,
  utf8
} from "../transportV2/protocol";
import { TransportV2Session, type SerializedTransportV2Session } from "../transportV2/session";

const vectorTranscript = {
  challenge: new Uint8Array(32).fill(0x11),
  clientPublicKey: new Uint8Array(32).fill(0x22),
  serverPublicKey: new Uint8Array(32).fill(0x33)
};
const SESSION_LIFETIME_SECONDS = 60 * 60;
const ROUTING_KEY = encodeCanonicalBase64(vectorTranscript.challenge);

function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

async function vectorKeys(): Promise<TransportV2SessionKeys> {
  return deriveTransportV2SessionKeys(new Uint8Array(32).fill(0x44), vectorTranscript);
}

function attestationDocument(
  challenge: Uint8Array,
  clientPublicKey: Uint8Array,
  serverPublicKey: Uint8Array
): AttestationDocument {
  return {
    module_id: "transport-v2-test",
    digest: "SHA384",
    timestamp: Date.now(),
    pcrs: new Map([[0, new Uint8Array(48).fill(0x41)]]),
    certificate: new Uint8Array([1]),
    cabundle: [new Uint8Array([2])],
    public_key: new Uint8Array(serverPublicKey),
    user_data: attestationUserData(clientPublicKey),
    nonce: new Uint8Array(challenge)
  };
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

function deferred<T>() {
  let resolve!: (value: T) => void;
  return {
    promise: new Promise<T>((fulfill) => {
      resolve = fulfill;
    }),
    resolve
  };
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
    const session = new TransportV2Session(keys, ROUTING_KEY, SESSION_LIFETIME_SECONDS);
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
      headers: [
        { name: "content-type", value: "application/json" },
        { name: "x-example", value: "first" },
        { name: "x-example", value: "second" }
      ],
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
      "x-opensecret-routing-key",
      "x-session-id"
    ]);
    expect((sealed.init.headers as Record<string, string>)["x-opensecret-routing-key"]).toBe(
      ROUTING_KEY
    );
    expect((sealed.init.headers as Record<string, string>).authorization).toBeUndefined();
    expect(sealed.init.credentials).toBe("omit");
    expect(sealed.init.redirect).toBe("error");
    expect((sealed.init.body as Uint8Array).subarray(0, 16)).toEqual(new Uint8Array(16).fill(1));

    expect(() =>
      encodeRequestEnvelope({
        method: "GET",
        target: "/v1/models",
        headers: [{ name: "x-opensecret-routing-key", value: ROUTING_KEY }]
      })
    ).toThrow(/logical header is invalid/i);
  });

  test("strictly persists the public routing key with the attested session", async () => {
    const keys = await vectorKeys();
    const session = new TransportV2Session(keys, ROUTING_KEY, SESSION_LIFETIME_SECONDS);
    const serialized = session.serialize();
    expect(serialized.routing_key).toBe(ROUTING_KEY);

    const restored = TransportV2Session.restore(serialized);
    const sealed = await restored.sealRequest({ method: "GET", target: "/v1/models" });
    expect((sealed.init.headers as Record<string, string>)["x-opensecret-routing-key"]).toBe(
      ROUTING_KEY
    );

    const missingRoutingKey = { ...serialized } as Partial<SerializedTransportV2Session>;
    delete missingRoutingKey.routing_key;
    expect(() =>
      TransportV2Session.restore(missingRoutingKey as unknown as SerializedTransportV2Session)
    ).toThrow(/stored session is invalid/i);

    for (const routingKey of [
      "not-base64",
      ROUTING_KEY.replace(/=$/u, ""),
      encodeCanonicalBase64(new Uint8Array(31).fill(0x11))
    ]) {
      expect(() => TransportV2Session.restore({ ...serialized, routing_key: routingKey })).toThrow(
        /canonical base64/i
      );
    }

    restored.dispose();
    session.dispose();
  });

  test("preserves ordered duplicate logical response headers", () => {
    const record = decodeResponseRecord(
      concatBytes(
        new Uint8Array([1]),
        utf8(
          JSON.stringify({
            status: 200,
            headers: [
              { name: "x-example", value: "first" },
              { name: "x-example", value: "second" }
            ]
          })
        )
      )
    );

    expect(record).toEqual({
      kind: "start",
      status: 200,
      headers: [
        { name: "x-example", value: "first" },
        { name: "x-example", value: "second" }
      ]
    });
  });

  test("streams ordered chunks and requires one authenticated terminal record", async () => {
    const keys = await vectorKeys();
    const session = new TransportV2Session(keys, ROUTING_KEY, SESSION_LIFETIME_SECONDS);
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

  test("rejects EOF, post-terminal frames, response transplants, and sequence reordering", async () => {
    const keys = await vectorKeys();
    const session = new TransportV2Session(keys, ROUTING_KEY, SESSION_LIFETIME_SECONDS);
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

    const otherSessionId = new Uint8Array(keys.sessionIdBytes);
    otherSessionId[0] ^= 0xff;
    const otherSessionKeys: TransportV2SessionKeys = {
      ...keys,
      sessionId: hex(otherSessionId),
      sessionIdBytes: otherSessionId
    };
    await expect(
      session.openResponse(
        await framedResponse(otherSessionKeys, requestId, [
          responsePlaintext("start"),
          responsePlaintext("end")
        ]),
        requestId
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
    const pcrConfig: PcrConfig = {
      environment: "development",
      remoteAttestation: false,
      pcr0DevValues: ["11".repeat(48)]
    };
    let verified = false;
    let observedInit: RequestInit | undefined;

    const dependencies: TransportV2ClientDependencies = {
      randomBytes: () => new Uint8Array(challenge),
      generateKeyPair: () => ({
        publicKey: new Uint8Array(clientKeyPair.publicKey),
        secretKey: new Uint8Array(clientSecret)
      }),
      verifyDocument: mock(
        async (_document, actualChallenge, actualClientKey, actualApiUrl, actualPcrConfig) => {
          expect(actualChallenge).toEqual(challenge);
          expect(actualClientKey).toEqual(clientKeyPair.publicKey);
          expect(actualApiUrl).toBe("https://enclave.example.test");
          expect(actualPcrConfig).toMatchObject({
            environment: "development",
            remoteAttestation: false,
            pcr0DevValues: ["11".repeat(48)]
          });
          verified = true;
          return new Uint8Array(serverPublicKey);
        }
      )
    };
    const fetchMock = mock(async (_input: string | URL | Request, init?: RequestInit) => {
      observedInit = init;
      const body = JSON.parse(init?.body as string) as {
        challenge: string;
        client_public_key: string;
      };
      expect(body.challenge).toBe(encodeCanonicalBase64(challenge));
      expect(body.client_public_key).toBe(encodeCanonicalBase64(clientKeyPair.publicKey));
      pcrConfig.environment = "production";
      pcrConfig.remoteAttestation = true;
      pcrConfig.pcr0DevValues![0] = "22".repeat(48);
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
        expires_in_seconds: SESSION_LIFETIME_SECONDS
      });
    });

    const client = await TransportV2Client.establish(
      {
        apiUrl: "https://enclave.example.test",
        pcrConfig,
        fetch: fetchMock as typeof fetch
      },
      dependencies
    );
    expect(verified).toBe(true);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(Object.keys(observedInit?.headers as Record<string, string>)).toEqual([
      "content-type",
      "x-opensecret-routing-key"
    ]);
    expect((observedInit?.headers as Record<string, string>)["x-opensecret-routing-key"]).toBe(
      encodeCanonicalBase64(challenge)
    );
    expect((observedInit?.headers as Record<string, string>).authorization).toBeUndefined();
    expect(observedInit?.credentials).toBe("omit");
    expect(observedInit?.redirect).toBe("error");
    client.dispose();
  });

  test("runs Nitro authentication and the configured PCR gate before returning a server key", async () => {
    const challenge = new Uint8Array(32).fill(0x82);
    const clientPublicKey = new Uint8Array(32).fill(0x83);
    const serverPublicKey = new Uint8Array(32).fill(0x84);
    const document = attestationDocument(challenge, clientPublicKey, serverPublicKey);
    const order: string[] = [];
    const policy = { environment: "development" as const, remoteAttestation: false };
    const dependencies: TransportV2AttestationVerificationDependencies = {
      isLocalDevelopmentApiUrl: () => false,
      authenticateDocument: mock(async (_encoded, _root, actualChallenge) => {
        expect(actualChallenge).toEqual(challenge);
        order.push("nitro");
        return document;
      }),
      parseLocalDocument: async () => {
        throw new Error("remote verification must not use the local parser");
      },
      requireTrustedPcr0: mock(async (pcrs, actualPolicy) => {
        expect(order).toEqual(["nitro"]);
        expect(pcrs).toBe(document.pcrs);
        expect(actualPolicy).toEqual(policy);
        order.push("pcr");
        return {
          hash: "41".repeat(48),
          validation: { isMatch: true, text: "test trust root" }
        };
      })
    };

    const returned = await verifyTransportV2AttestationDocumentWithDependencies(
      "signed-document",
      challenge,
      clientPublicKey,
      "https://enclave.example.test",
      policy,
      dependencies
    );
    expect(returned).toEqual(serverPublicKey);
    expect(returned).not.toBe(serverPublicKey);
    expect(order).toEqual(["nitro", "pcr"]);

    dependencies.requireTrustedPcr0 = mock(async () => {
      throw new Error("untrusted PCR0");
    });
    await expect(
      verifyTransportV2AttestationDocumentWithDependencies(
        "signed-document",
        challenge,
        clientPublicKey,
        "https://enclave.example.test",
        policy,
        dependencies
      )
    ).rejects.toThrow("untrusted PCR0");
  });

  test("bypasses PCR policy only for exact HTTP loopback development", async () => {
    const challenge = new Uint8Array(32).fill(0x85);
    const clientPublicKey = new Uint8Array(32).fill(0x86);
    const serverPublicKey = new Uint8Array(32).fill(0x87);
    const document = attestationDocument(challenge, clientPublicKey, serverPublicKey);
    const authenticateDocument = mock(async () => document);
    const parseLocalDocument = mock(async () => document);
    const requirePcr = mock(async () => ({
      hash: "41".repeat(48),
      validation: { isMatch: true, text: "test trust root" }
    }));
    const dependencies: TransportV2AttestationVerificationDependencies = {
      isLocalDevelopmentApiUrl: (url) => url.startsWith("http://localhost:"),
      authenticateDocument,
      parseLocalDocument,
      requireTrustedPcr0: requirePcr
    };

    await verifyTransportV2AttestationDocumentWithDependencies(
      "local-document",
      challenge,
      clientPublicKey,
      "http://localhost:3000",
      { environment: "development" },
      dependencies
    );
    expect(parseLocalDocument).toHaveBeenCalledTimes(1);
    expect(requirePcr).toHaveBeenCalledTimes(0);

    await verifyTransportV2AttestationDocumentWithDependencies(
      "signed-document",
      challenge,
      clientPublicKey,
      "https://localhost:3000",
      { environment: "development" },
      dependencies
    );
    expect(authenticateDocument).toHaveBeenCalledTimes(1);
    expect(requirePcr).toHaveBeenCalledTimes(1);
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
        expires_in_seconds: SESSION_LIFETIME_SECONDS
      })
    );
    await expect(
      TransportV2Client.establish(
        { apiUrl: "https://enclave.example.test", fetch: fetchMock as typeof fetch },
        dependencies
      )
    ).rejects.toThrow("untrusted PCR0");
  });

  test("bounds session establishment with an independent timeout", async () => {
    const keyPair = nacl.box.keyPair();
    const dependencies: TransportV2ClientDependencies = {
      randomBytes: () => new Uint8Array(32).fill(0x93),
      generateKeyPair: () => ({
        publicKey: new Uint8Array(keyPair.publicKey),
        secretKey: new Uint8Array(keyPair.secretKey)
      }),
      verifyDocument: async () => new Uint8Array(32).fill(1),
      establishmentTimeoutMs: 5
    };
    const fetchMock = mock(
      async (_input: string | URL | Request, init?: RequestInit): Promise<Response> =>
        new Promise((_resolve, reject) => {
          init?.signal?.addEventListener(
            "abort",
            () => reject(init.signal?.reason ?? new DOMException("aborted", "AbortError")),
            { once: true }
          );
        })
    );

    await expect(
      TransportV2Client.establish(
        { apiUrl: "https://enclave.example.test", fetch: fetchMock as typeof fetch },
        dependencies
      )
    ).rejects.toThrow("session establishment timed out");
    expect(fetchMock).toHaveBeenCalledTimes(1);
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
              expires_in_seconds: SESSION_LIFETIME_SECONDS + 1
            })
          ) as typeof fetch
        },
        dependencies
      )
    ).rejects.toThrow(/session response is invalid/i);
    expect(verifyDocument).not.toHaveBeenCalled();

    const keys = await vectorKeys();
    expect(
      () =>
        new TransportV2Session(
          keys,
          ROUTING_KEY,
          SESSION_LIFETIME_SECONDS,
          Date.now() - (SESSION_LIFETIME_SECONDS * 1000 - 29_000)
        )
    ).toThrow(/expired during establishment/i);
  });

  test("fails closed on an unauthenticated outer response without a V1 fallback", async () => {
    const keys = await vectorKeys();
    const session = new TransportV2Session(keys, ROUTING_KEY, SESSION_LIFETIME_SECONDS);
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

  test("an old server gets one credential-free V2 attempt and no downgrade", async () => {
    const requests: Array<{ url: string; init?: RequestInit }> = [];
    const fetchMock = mock(async (input: string | URL | Request, init?: RequestInit) => {
      requests.push({ url: input.toString(), init });
      return new Response("not found", {
        status: 404,
        headers: { "content-type": "text/plain" }
      });
    });

    await expect(
      TransportV2Client.establish({
        apiUrl: "https://old-server.example.test",
        fetch: fetchMock as typeof fetch
      })
    ).rejects.toThrow(/unauthenticated outer response/i);
    expect(requests).toHaveLength(1);
    expect(requests[0].url).toBe("https://old-server.example.test/v2/session");
    expect(requests[0].init?.method).toBe("POST");
    expect(requests[0].init?.credentials).toBe("omit");
    expect(Object.keys(requests[0].init?.headers as Record<string, string>)).toEqual([
      "content-type",
      "x-opensecret-routing-key"
    ]);
  });

  test("checks current authority after sealing and immediately before the outer fetch", async () => {
    const keys = await vectorKeys();
    const fetchMock = mock(async () => {
      throw new Error("outer fetch must not run");
    });
    const client = TransportV2Client.restore(
      { apiUrl: "https://enclave.example.test", fetch: fetchMock as typeof fetch },
      {
        version: 2,
        session_id: keys.sessionId,
        routing_key: ROUTING_KEY,
        request_key: encodeCanonicalBase64(keys.requestKey),
        response_key: encodeCanonicalBase64(keys.responseKey),
        expires_at_ms: Date.now() + 60_000
      }
    );

    let fenceCalls = 0;
    const request = client.request({ method: "GET", target: "/protected/user" }, undefined, () => {
      fenceCalls += 1;
      throw new Error("authority changed");
    });
    // Sealing is asynchronous; a fence moved before it would run synchronously.
    expect(fenceCalls).toBe(0);
    await expect(request).rejects.toThrow("authority changed");
    expect(fenceCalls).toBe(1);
    expect(fetchMock).toHaveBeenCalledTimes(0);
    client.dispose();
  });

  test("does not fetch when a session expires during asynchronous request sealing", async () => {
    const keys = await vectorKeys();
    let nowMs = 1_000_000;
    const expiresAtMs = nowMs + 1;
    const fetchMock = mock(async () => {
      throw new Error("outer fetch must not run");
    });
    const client = TransportV2Client.restore(
      { apiUrl: "https://enclave.example.test", fetch: fetchMock as typeof fetch },
      {
        version: 2,
        session_id: keys.sessionId,
        routing_key: ROUTING_KEY,
        request_key: encodeCanonicalBase64(keys.requestKey),
        response_key: encodeCanonicalBase64(keys.responseKey),
        expires_at_ms: expiresAtMs
      },
      () => nowMs
    );

    // The request reaches WebCrypto's asynchronous key-derivation boundary
    // before this synchronous clock advance. The post-seal lifetime fence must
    // reject the completed ciphertext without putting it on the wire.
    const request = client.request({ method: "GET", target: "/protected/user" });
    nowMs = expiresAtMs;

    await expect(request).rejects.toThrow(/session is expired/i);
    expect(fetchMock).toHaveBeenCalledTimes(0);
  });

  test("preserves an admitted response when a concurrent new send observes expiry", async () => {
    const keys = await vectorKeys();
    let nowMs = 2_000_000;
    const expiresAtMs = nowMs + 1_000;
    const responseReady = deferred<void>();
    const releaseResponse = deferred<void>();
    const fetchMock = mock(async (_input: string | URL | Request, init?: RequestInit) => {
      const requestId = new Uint8Array(init?.body as Uint8Array).slice(0, 16);
      const response = await framedResponse(keys, requestId, [
        responsePlaintext("start"),
        responsePlaintext("chunk", utf8("slow but valid")),
        responsePlaintext("end")
      ]);
      const wire = new Uint8Array(await response.arrayBuffer());
      const delayed = new Response(
        new ReadableStream<Uint8Array>({
          async pull(controller) {
            await releaseResponse.promise;
            controller.enqueue(wire);
            controller.close();
          }
        }),
        { status: 200, headers: { "content-type": "application/octet-stream" } }
      );
      responseReady.resolve();
      return delayed;
    });
    const client = TransportV2Client.restore(
      { apiUrl: "https://enclave.example.test", fetch: fetchMock as typeof fetch },
      {
        version: 2,
        session_id: keys.sessionId,
        routing_key: ROUTING_KEY,
        request_key: encodeCanonicalBase64(keys.requestKey),
        response_key: encodeCanonicalBase64(keys.responseKey),
        expires_at_ms: expiresAtMs
      },
      () => nowMs
    );

    const admitted = client.request({ method: "GET", target: "/protected/user" });
    await responseReady.promise;

    // A second request starts while the session is usable but finishes its
    // asynchronous seal after expiry. Rejecting it must not zero the keys that
    // the admitted first request still needs to open its response.
    const rejected = client.request({ method: "GET", target: "/protected/second" });
    nowMs = expiresAtMs;
    await expect(rejected).rejects.toThrow(/session is expired/i);
    expect(fetchMock).toHaveBeenCalledTimes(1);

    releaseResponse.resolve();

    const logical = await admitted;
    expect(await consume(logical.body)).toBe("slow but valid");
    expect(fetchMock).toHaveBeenCalledTimes(1);
    client.dispose();
  });

  test("rejects plaintext SSE even when the outer status is successful", async () => {
    const keys = await vectorKeys();
    const session = new TransportV2Session(keys, ROUTING_KEY, SESSION_LIFETIME_SECONDS);
    await expect(
      session.openResponse(
        new Response("data: plaintext\n\n", {
          status: 200,
          headers: { "content-type": "text/event-stream" }
        }),
        new Uint8Array(16).fill(0xa2)
      )
    ).rejects.toBeInstanceOf(TransportV2ProtocolError);
  });
});
