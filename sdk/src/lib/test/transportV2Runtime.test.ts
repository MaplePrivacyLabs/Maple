import { beforeEach, describe, expect, mock, test } from "bun:test";
import type { PcrConfig } from "../pcr";
import {
  deriveTransportV2SessionKeys,
  encryptTransportV2ResponseForTesting
} from "../transportV2/crypto";
import {
  TransportV2ProtocolError,
  concatBytes,
  uint32,
  utf8,
  type TransportV2Request
} from "../transportV2/protocol";
import {
  TransportV2Runtime,
  getTransportV2PublicAttestation,
  transportV2LogicalTarget,
  type TransportV2RuntimeDependencies
} from "../transportV2/runtime";
import { TransportV2Session, type SerializedTransportV2Session } from "../transportV2/session";
import { TransportV2Client } from "../transportV2/client";

const API_URL = "https://api.example.test/service";

interface Deferred<T> {
  promise: Promise<T>;
  resolve(value: T): void;
}

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  return {
    promise: new Promise<T>((fulfill) => {
      resolve = fulfill;
    }),
    resolve
  };
}

function logicalResponse(
  status = 200,
  chunks: readonly Uint8Array[] = [utf8("ok")],
  headers: { name: string; value: string }[] = []
) {
  return {
    requestId: new Uint8Array(16),
    status,
    headers,
    body: new ReadableStream<Uint8Array>({
      start(controller) {
        for (const chunk of chunks) controller.enqueue(new Uint8Array(chunk));
        controller.close();
      }
    })
  };
}

function serializedSession(sessionId: string): SerializedTransportV2Session {
  return {
    version: 2,
    session_id: sessionId,
    request_key: "ERERERERERERERERERERERERERERERERERERERERERE=",
    response_key: "IiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiI=",
    expires_at_ms: Date.now() + 60_000
  };
}

function fakeClient(
  sessionId: string,
  request: (
    value: TransportV2Request
  ) => ReturnType<typeof logicalResponse> | Promise<ReturnType<typeof logicalResponse>> = () =>
    logicalResponse()
): TransportV2Client {
  let disposed = false;
  const expiresAtMs = Date.now() + 60_000;
  return {
    get sessionId() {
      if (disposed) throw new Error("disposed");
      return sessionId;
    },
    get expiresAtMs() {
      return expiresAtMs;
    },
    isUsable() {
      return !disposed;
    },
    serializeSession() {
      return { ...serializedSession(sessionId), expires_at_ms: expiresAtMs };
    },
    request(value: TransportV2Request, _signal?: AbortSignal | null, beforeSend?: () => void) {
      beforeSend?.();
      return request(value);
    },
    dispose() {
      disposed = true;
    }
  } as unknown as TransportV2Client;
}

function responseRecord(
  kind: "start" | "chunk" | "end",
  value?: { status: number; headers: { name: string; value: string }[] } | Uint8Array
): Uint8Array {
  if (kind === "start") {
    return concatBytes(new Uint8Array([1]), utf8(JSON.stringify(value)));
  }
  if (kind === "chunk") return concatBytes(new Uint8Array([2]), value as Uint8Array);
  return new Uint8Array([3]);
}

async function encryptedFrames(
  keys: Awaited<ReturnType<typeof deriveTransportV2SessionKeys>>,
  requestId: Uint8Array,
  records: readonly Uint8Array[]
): Promise<Uint8Array[]> {
  const frames: Uint8Array[] = [];
  for (let index = 0; index < records.length; index += 1) {
    const ciphertext = await encryptTransportV2ResponseForTesting(
      keys,
      requestId,
      BigInt(index),
      records[index]
    );
    frames.push(concatBytes(uint32(ciphertext.byteLength), ciphertext));
  }
  return frames;
}

beforeEach(() => {
  globalThis.sessionStorage.clear();
});

describe("Transport V2 runtime", () => {
  test("keeps the logical path and query inside the exact attested API base", () => {
    expect(
      transportV2LogicalTarget(
        API_URL,
        "https://api.example.test/service/v1/items?tag=a%2Fb&tag=two"
      )
    ).toBe("/v1/items?tag=a%2Fb&tag=two");
    expect(transportV2LogicalTarget(`${API_URL}/`, "v1/items?q=hello+world")).toBe(
      "/v1/items?q=hello+world"
    );

    for (const requestUrl of [
      "https://other.example.test/service/v1/items",
      "https://api.example.test/service-other/v1/items",
      "https://api.example.test/v1/items",
      "https://api.example.test/service/v1/items#fragment",
      "https://user@api.example.test/service/v1/items"
    ]) {
      expect(() => transportV2LogicalTarget(API_URL, requestUrl)).toThrow(TransportV2ProtocolError);
    }
  });

  test("coalesces establishment for each canonical API and PCR policy", async () => {
    const pending = deferred<TransportV2Client>();
    const establish = mock(async () => pending.promise);
    const runtime = new TransportV2Runtime({
      establish,
      restore: () => {
        throw new Error("unexpected restore");
      }
    });
    const firstPolicy: PcrConfig = {
      remoteAttestation: false,
      pcr0Values: ["bbbb", "aaaa"]
    };
    const equivalentPolicy: PcrConfig = {
      pcr0Values: ["aaaa", "bbbb", "aaaa"],
      remoteAttestation: false
    };

    const first = runtime.request({
      apiUrl: `${API_URL}/`,
      pcrConfig: firstPolicy,
      request: { method: "GET", target: "/v1/one" }
    });
    const second = runtime.request({
      apiUrl: API_URL,
      pcrConfig: equivalentPolicy,
      request: { method: "GET", target: "/v1/two" }
    });
    expect(establish).toHaveBeenCalledTimes(1);
    pending.resolve(fakeClient("11111111111111111111111111111111"));
    await Promise.all([first, second]);
    expect(establish).toHaveBeenCalledTimes(1);

    await runtime.request({
      apiUrl: API_URL,
      pcrConfig: { environment: "development", remoteAttestation: false },
      request: { method: "GET", target: "/v1/three" }
    });
    expect(establish).toHaveBeenCalledTimes(2);
  });

  test("lets an aborted caller detach without cancelling the shared establishment", async () => {
    const pending = deferred<TransportV2Client>();
    const establish = mock(async () => pending.promise);
    const runtime = new TransportV2Runtime({
      establish,
      restore: () => {
        throw new Error("unexpected restore");
      }
    });
    const controller = new AbortController();
    const first = runtime.request({
      apiUrl: API_URL,
      signal: controller.signal,
      request: { method: "GET", target: "/v1/first" }
    });
    const second = runtime.request({
      apiUrl: API_URL,
      request: { method: "GET", target: "/v1/second" }
    });
    expect(establish).toHaveBeenCalledTimes(1);

    controller.abort(new Error("caller cancelled"));
    await expect(first).rejects.toThrow("caller cancelled");
    pending.resolve(fakeClient("11111111111111111111111111111111"));
    await expect(second).resolves.toBeDefined();
    expect(establish).toHaveBeenCalledTimes(1);
  });

  test("force-refresh retirement replaces only the selected attested scope", async () => {
    const disposed: string[] = [];
    let sequence = 0;
    const establish = mock(async () => {
      sequence += 1;
      const sessionId = sequence.toString(16).padStart(32, "0");
      const client = fakeClient(sessionId);
      const dispose = client.dispose.bind(client);
      client.dispose = () => {
        disposed.push(sessionId);
        dispose();
      };
      return client;
    });
    const runtime = new TransportV2Runtime({
      establish,
      restore: () => {
        throw new Error("unexpected restore");
      }
    });
    const policy: PcrConfig = { environment: "development", remoteAttestation: false };

    const initial = await runtime.sessionInfo(API_URL, policy);
    const reused = await runtime.sessionInfo(API_URL, policy);
    expect(reused.sessionId).toBe(initial.sessionId);
    expect(establish).toHaveBeenCalledTimes(1);

    runtime.clearScope(API_URL, policy);
    const refreshed = await runtime.sessionInfo(API_URL, policy);
    expect(refreshed.sessionId).not.toBe(initial.sessionId);
    expect(establish).toHaveBeenCalledTimes(2);
    expect(disposed).toEqual([initial.sessionId]);
  });

  test("force refresh cannot reuse or publish an in-progress establishment", async () => {
    const firstPending = deferred<TransportV2Client>();
    const secondPending = deferred<TransportV2Client>();
    const disposed: string[] = [];
    const firstClient = fakeClient("11111111111111111111111111111111");
    const firstDispose = firstClient.dispose.bind(firstClient);
    firstClient.dispose = () => {
      disposed.push("first");
      firstDispose();
    };
    const secondClient = fakeClient("22222222222222222222222222222222");
    const establish = mock(async () =>
      establish.mock.calls.length === 1 ? firstPending.promise : secondPending.promise
    );
    const runtime = new TransportV2Runtime({
      establish,
      restore: () => {
        throw new Error("unexpected restore");
      }
    });
    const policy: PcrConfig = { environment: "development", remoteAttestation: false };

    const startedBeforeRefresh = runtime.sessionInfo(API_URL, policy);
    expect(establish).toHaveBeenCalledTimes(1);
    runtime.clearScope(API_URL, policy);
    const forced = runtime.sessionInfo(API_URL, policy);
    expect(establish).toHaveBeenCalledTimes(2);

    firstPending.resolve(firstClient);
    secondPending.resolve(secondClient);
    await expect(startedBeforeRefresh).resolves.toMatchObject({
      sessionId: secondClient.sessionId
    });
    await expect(forced).resolves.toMatchObject({ sessionId: secondClient.sessionId });
    expect(disposed).toEqual(["first"]);
    expect(establish).toHaveBeenCalledTimes(2);
  });

  test("preserves the public attestation shape and explicit force-refresh scope", async () => {
    const clearScope = mock(() => {});
    const sessionInfo = mock(async () => ({
      protocolVersion: 2 as const,
      sessionId: "11111111111111111111111111111111",
      expiresAtUnixSeconds: 4_000_000_000
    }));
    const explicitPolicy: PcrConfig = {
      environment: "development",
      remoteAttestation: false,
      pcr0DevValues: ["11".repeat(48)]
    };

    const result = await getTransportV2PublicAttestation(
      { clearScope, sessionInfo },
      API_URL,
      { environment: "production" },
      true,
      "https://override.example.test/api/",
      explicitPolicy
    );
    explicitPolicy.environment = "production";
    explicitPolicy.pcr0DevValues![0] = "22".repeat(48);

    expect(result).toEqual({
      sessionKey: null,
      sessionId: "11111111111111111111111111111111"
    });
    expect(clearScope).toHaveBeenCalledTimes(1);
    expect(sessionInfo).toHaveBeenCalledTimes(1);
    expect(clearScope.mock.calls[0]).toEqual(sessionInfo.mock.calls[0]);
    expect(clearScope.mock.calls[0][0]).toBe("https://override.example.test/api/");
    expect(clearScope.mock.calls[0][1]).toMatchObject({
      environment: "development",
      remoteAttestation: false,
      pcr0DevValues: ["11".repeat(48)]
    });
  });

  test("never resends a failed or ambiguous operation and replaces its session only later", async () => {
    let applicationSends = 0;
    let firstDisposed = false;
    const first = fakeClient("11111111111111111111111111111111", async () => {
      applicationSends += 1;
      throw new Error("connection ended after send");
    });
    const originalDispose = first.dispose.bind(first);
    first.dispose = () => {
      firstDisposed = true;
      originalDispose();
    };
    const second = fakeClient("22222222222222222222222222222222", () => {
      applicationSends += 1;
      return logicalResponse();
    });
    const establish = mock(async () => (establish.mock.calls.length === 1 ? first : second));
    const runtime = new TransportV2Runtime({
      establish,
      restore: () => {
        throw new Error("unexpected restore");
      }
    });

    await expect(
      runtime.request({
        apiUrl: API_URL,
        request: { method: "POST", target: "/v1/mutation", body: utf8("once") }
      })
    ).rejects.toThrow("connection ended after send");
    expect(applicationSends).toBe(1);
    expect(establish).toHaveBeenCalledTimes(1);
    expect(firstDisposed).toBe(true);

    const later = await runtime.request({
      apiUrl: API_URL,
      request: { method: "GET", target: "/v1/later" }
    });
    expect(await later.response.text()).toBe("ok");
    expect(applicationSends).toBe(2);
    expect(establish).toHaveBeenCalledTimes(2);
  });

  test("forwards the final pre-send authority fence before dispatch", async () => {
    const dispatch = mock(async () => logicalResponse());
    const runtime = new TransportV2Runtime({
      establish: async () => fakeClient("11111111111111111111111111111111", dispatch),
      restore: () => {
        throw new Error("unexpected restore");
      }
    });

    await expect(
      runtime.request({
        apiUrl: API_URL,
        request: { method: "GET", target: "/protected/user" },
        beforeSend: () => {
          throw new Error("authority changed");
        }
      })
    ).rejects.toThrow("authority changed");
    expect(dispatch).toHaveBeenCalledTimes(0);
  });

  test("reconstructs authenticated incremental Start/Chunk/End bodies and bodyless 204s", async () => {
    const keys = await deriveTransportV2SessionKeys(new Uint8Array(32).fill(0x44), {
      challenge: new Uint8Array(32).fill(0x11),
      clientPublicKey: new Uint8Array(32).fill(0x22),
      serverPublicKey: new Uint8Array(32).fill(0x33)
    });
    const stored = new TransportV2Session(keys, 3900).serialize();
    const releaseBody = deferred<void>();
    let call = 0;
    const fetchMock = mock(async (_input: string | URL | Request, init?: RequestInit) => {
      call += 1;
      const outer = new Uint8Array(init?.body as Uint8Array);
      const requestId = outer.slice(0, 16);
      const records =
        call === 1
          ? [
              responseRecord("start", {
                status: 200,
                headers: [{ name: "content-type", value: "text/event-stream" }]
              }),
              responseRecord("chunk", utf8("hello ")),
              responseRecord("chunk", utf8("world")),
              responseRecord("end")
            ]
          : [responseRecord("start", { status: 204, headers: [] }), responseRecord("end")];
      const frames = await encryptedFrames(keys, requestId, records);
      let index = 0;
      return new Response(
        new ReadableStream<Uint8Array>({
          async pull(controller) {
            if (index === 0) {
              controller.enqueue(frames[index++]);
              return;
            }
            if (call === 1) await releaseBody.promise;
            while (index < frames.length) controller.enqueue(frames[index++]);
            controller.close();
          }
        }),
        { status: 200, headers: { "content-type": "application/octet-stream" } }
      );
    });
    const runtime = new TransportV2Runtime({
      establish: async (options) =>
        TransportV2Client.restore(
          { apiUrl: options.apiUrl, fetch: fetchMock as typeof fetch },
          stored
        ),
      restore: (options, state) => TransportV2Client.restore(options, state)
    });

    const streamed = await runtime.request({
      apiUrl: API_URL,
      request: { method: "POST", target: "/v1/chat/completions" }
    });
    expect(streamed.response.status).toBe(200);
    expect(streamed.response.headers.get("content-type")).toBe("text/event-stream");
    const bodyPromise = streamed.response.text();
    let completed = false;
    void bodyPromise.then(() => {
      completed = true;
    });
    await Promise.resolve();
    expect(completed).toBe(false);
    releaseBody.resolve();
    expect(await bodyPromise).toBe("hello world");

    const empty = await runtime.request({
      apiUrl: API_URL,
      request: { method: "DELETE", target: "/v1/item" }
    });
    expect(empty.response.status).toBe(204);
    expect(empty.response.body).toBeNull();
  });

  test("reconstructs ordered duplicate logical headers with Fetch append semantics", async () => {
    const runtime = new TransportV2Runtime({
      establish: async () =>
        fakeClient("11111111111111111111111111111111", () =>
          logicalResponse(
            200,
            [utf8("ok")],
            [
              { name: "x-repeated", value: "first" },
              { name: "x-repeated", value: "second" }
            ]
          )
        ),
      restore: () => {
        throw new Error("unexpected restore");
      }
    });

    const exchange = await runtime.request({
      apiUrl: API_URL,
      request: { method: "GET", target: "/v1/models" }
    });
    expect(exchange.response.headers.get("x-repeated")).toBe("first, second");
  });

  test("restores an exact OAuth continuation once and rejects mismatches", async () => {
    const initial = fakeClient("11111111111111111111111111111111");
    const restored = fakeClient("11111111111111111111111111111111");
    const restore = mock(() => restored);
    const runtime = new TransportV2Runtime({ establish: async () => initial, restore });

    const begin = await runtime.request({
      apiUrl: API_URL,
      pcrConfig: { remoteAttestation: false },
      request: { method: "POST", target: "/v1/oauth/github" }
    });
    begin.rememberOAuthContinuation("github", "state-one");
    await runtime.request({
      apiUrl: API_URL,
      pcrConfig: { remoteAttestation: false },
      oauthCallback: { provider: "github", state: "state-one" },
      request: { method: "POST", target: "/v1/oauth/github/callback" }
    });
    expect(restore).toHaveBeenCalledTimes(1);
    expect(restore.mock.calls[0][1]).toEqual(initial.serializeSession());
    await expect(
      runtime.request({
        apiUrl: API_URL,
        pcrConfig: { remoteAttestation: false },
        oauthCallback: { provider: "github", state: "state-one" },
        request: { method: "POST", target: "/v1/oauth/github/callback" }
      })
    ).rejects.toThrow(/continuation is missing/i);

    begin.rememberOAuthContinuation("github", "state-two");
    await expect(
      runtime.request({
        apiUrl: API_URL,
        pcrConfig: { remoteAttestation: false },
        oauthCallback: { provider: "github", state: "wrong-state" },
        request: { method: "POST", target: "/v1/oauth/github/callback" }
      })
    ).rejects.toThrow(/does not match/i);

    begin.rememberOAuthContinuation("github", "state-three");
    await expect(
      runtime.request({
        apiUrl: API_URL,
        pcrConfig: { environment: "development", remoteAttestation: false },
        oauthCallback: { provider: "github", state: "state-three" },
        request: { method: "POST", target: "/v1/oauth/github/callback" }
      })
    ).rejects.toThrow(/does not match/i);

    begin.rememberOAuthContinuation("github", "state-four");
    await expect(
      runtime.request({
        apiUrl: API_URL,
        pcrConfig: { remoteAttestation: false },
        oauthCallback: { provider: "google", state: "state-four" },
        request: { method: "POST", target: "/v1/oauth/google/callback" }
      })
    ).rejects.toThrow(/continuation is missing/i);
  });
});
