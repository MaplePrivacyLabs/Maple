import { beforeEach, describe, expect, mock, test } from "bun:test";
import type { PcrConfig } from "../pcr";
import { TransportV2Client, type TransportV2ClientOptions } from "../transportV2/client";
import {
  deriveTransportV2SessionKeys,
  encryptTransportV2Request,
  encryptTransportV2ResponseForTesting,
  type TransportV2SessionKeys
} from "../transportV2/crypto";
import {
  MAX_REQUEST_BODY_BYTES,
  TransportV2ProtocolError,
  concatBytes,
  encodeCanonicalBase64,
  encodeRequestEnvelope,
  uint32,
  utf8,
  type TransportV2Request
} from "../transportV2/protocol";
import { TransportV2Runtime, type TransportV2RuntimeRequest } from "../transportV2/runtime";
import {
  TransportV2Session,
  TransportV2UntrustedRecoveryHint,
  type TransportV2RecoveryCode
} from "../transportV2/session";

const API_URL = "https://api.example.test/service";
const CONTRACT = "x-opensecret-error-contract";
const CODE = "x-opensecret-error-code";
const CODES: TransportV2RecoveryCode[] = ["session_not_found", "request_decryption_failed"];
const ROUTING_KEY = encodeCanonicalBase64(new Uint8Array(32).fill(0x11));

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((fulfill) => {
    resolve = fulfill;
  });
  return { promise, resolve };
}

function hint(code: TransportV2RecoveryCode): Response {
  return new Response("transport request rejected", {
    status: 400,
    headers: { [CONTRACT]: "1", [CODE]: code }
  });
}

interface Attempt {
  index: number;
  session: number;
  keys: TransportV2SessionKeys;
  url: string;
  init: RequestInit;
  wire: Uint8Array;
  requestId: Uint8Array;
}

async function responseFor(
  attempt: Attempt,
  options: {
    status?: number;
    logicalHeaders?: { name: string; value: string }[];
    truncate?: boolean;
    corrupt?: boolean;
  } = {}
): Promise<Response> {
  const records = [
    concatBytes(
      new Uint8Array([1]),
      utf8(JSON.stringify({ status: options.status ?? 200, headers: options.logicalHeaders ?? [] }))
    ),
    concatBytes(new Uint8Array([2]), utf8("ok"))
  ];
  if (!options.truncate) records.push(new Uint8Array([3]));
  const frames = await Promise.all(
    records.map(async (record, sequence) => {
      const ciphertext = await encryptTransportV2ResponseForTesting(
        attempt.keys,
        attempt.requestId,
        BigInt(sequence),
        record
      );
      return concatBytes(uint32(ciphertext.byteLength), ciphertext);
    })
  );
  if (options.corrupt) frames[0][frames[0].length - 1] ^= 1;
  return new Response(concatBytes(...frames), {
    status: 200,
    headers: {
      "content-type": "application/octet-stream",
      ...(options.corrupt ? { [CONTRACT]: "1", [CODE]: "request_decryption_failed" } : {})
    }
  });
}

async function harness(
  respond: (attempt: Attempt) => Response | Promise<Response>,
  beforeEstablish?: (index: number, options: TransportV2ClientOptions) => Promise<void>
) {
  const keys = await Promise.all(
    [1, 2, 3].map((value) =>
      deriveTransportV2SessionKeys(new Uint8Array(32).fill(0x40 + value), {
        challenge: new Uint8Array(32).fill(0x11),
        clientPublicKey: new Uint8Array(32).fill(0x22),
        serverPublicKey: new Uint8Array(32).fill(0x33)
      })
    )
  );
  const stored = keys.map((key) => new TransportV2Session(key, ROUTING_KEY, 3600).serialize());
  const attempts: Attempt[] = [];
  const restore = (apiUrl: string, session: number) =>
    TransportV2Client.restore(
      {
        apiUrl,
        fetch: (async (url, init) => {
          const wire = new Uint8Array(init!.body as Uint8Array);
          const attempt: Attempt = {
            index: attempts.length,
            session,
            keys: keys[session],
            url: url.toString(),
            init: init!,
            wire,
            requestId: wire.slice(0, 16)
          };
          attempts.push(attempt);
          return respond(attempt);
        }) as typeof fetch
      },
      stored[session]
    );
  const establish = mock(async (options: TransportV2ClientOptions) => {
    const index = establish.mock.calls.length - 1;
    await beforeEstablish?.(index, options);
    return restore(options.apiUrl, index);
  });
  const runtime = new TransportV2Runtime({
    establish,
    restore: (options, state) =>
      restore(
        options.apiUrl,
        stored.findIndex((entry) => entry.session_id === state.session_id)
      )
  });
  return { runtime, establish, attempts, keys };
}

beforeEach(() => globalThis.sessionStorage.clear());

describe("Transport V2 bounded session repair", () => {
  for (const code of CODES) {
    test(`repairs ${code} once with fresh transport keys and the exact logical request`, async () => {
      const fixture = await harness((attempt) =>
        attempt.index === 0 ? hint(code) : responseFor(attempt)
      );
      const request: TransportV2Request = {
        method: "POST",
        target: "/v1/mutation?tag=a%2Fb&tag=two",
        credential: { kind: "bearer", value: "fixture-bearer" },
        cacheNamespaceRoot: new Uint8Array(32).fill(0x51),
        headers: [
          { name: "x-repeated", value: "first" },
          { name: "x-repeated", value: "second" }
        ],
        body: utf8("exact fixture body")
      };
      const expected = encodeRequestEnvelope(request);
      const result = await fixture.runtime.request({ apiUrl: API_URL, request });
      expect(await result.response.text()).toBe("ok");
      expect(fixture.establish).toHaveBeenCalledTimes(2);
      expect(fixture.attempts).toHaveLength(2);
      expect(fixture.attempts[0].keys.sessionId).not.toBe(fixture.attempts[1].keys.sessionId);
      expect(fixture.attempts[0].requestId).not.toEqual(fixture.attempts[1].requestId);
      for (const attempt of fixture.attempts) {
        expect(attempt.wire).toEqual(
          await encryptTransportV2Request(attempt.keys, attempt.requestId, expected)
        );
        expect(attempt.url).toBe(`${API_URL}/v2/request`);
        expect(attempt.init.credentials).toBe("omit");
        expect(attempt.init.redirect).toBe("error");
        expect([...new Headers(attempt.init.headers).keys()].sort()).toEqual([
          "content-type",
          "x-opensecret-routing-key",
          "x-session-id"
        ]);
      }
      expect(request.body).toEqual(utf8("exact fixture body"));
      expect(request.cacheNamespaceRoot).toEqual(new Uint8Array(32).fill(0x51));
    });
  }

  for (const second of CODES) {
    test(`surfaces a second ${second} after the single shared repair budget`, async () => {
      const fixture = await harness((attempt) =>
        hint(attempt.index === 0 ? "session_not_found" : second)
      );
      await expect(
        fixture.runtime.request({
          apiUrl: API_URL,
          request: { method: "POST", target: "/v1/item" }
        })
      ).rejects.toMatchObject({ name: "TransportV2UntrustedRecoveryHint", code: second });
      expect(fixture.attempts).toHaveLength(2);
      expect(fixture.establish).toHaveBeenCalledTimes(2);
    });
  }

  for (const body of [undefined, new Uint8Array()]) {
    test(`preserves a ${body === undefined ? "missing" : "present empty"} body during repair`, async () => {
      const fixture = await harness((attempt) =>
        attempt.index === 0 ? hint("session_not_found") : responseFor(attempt)
      );
      const request = { method: "POST", target: "/v1/item", body };
      const expected = encodeRequestEnvelope(request);
      const result = await fixture.runtime.request({ apiUrl: API_URL, request });
      expect(await result.response.text()).toBe("ok");
      for (const attempt of fixture.attempts) {
        expect(attempt.wire).toEqual(
          await encryptTransportV2Request(attempt.keys, attempt.requestId, expected)
        );
      }
    });
  }

  test("snapshots body, root, ordered headers, credential, policy, URL and fence before awaiting", async () => {
    const ready = deferred<void>();
    const gate = deferred<void>();
    const policy: PcrConfig = {
      environment: "development",
      remoteAttestation: false,
      pcr0DevValues: ["11".repeat(48)]
    };
    const request: TransportV2Request = {
      method: "POST",
      target: "/v1/original?exact=%2f",
      credential: { kind: "api_key", value: "original-key" },
      cacheNamespaceRoot: new Uint8Array(32).fill(0x55),
      headers: [{ name: "x-original", value: "original" }],
      body: utf8("original")
    };
    const expected = encodeRequestEnvelope(request);
    const fence = mock(() => {});
    const input: TransportV2RuntimeRequest = {
      apiUrl: API_URL,
      pcrConfig: policy,
      request,
      beforeSend: fence
    };
    const fixture = await harness(
      (attempt) => {
        request.body!.fill(0x66);
        return attempt.index === 0 ? hint("session_not_found") : responseFor(attempt);
      },
      async (index) => {
        if (index === 0) {
          ready.resolve();
          await gate.promise;
        }
      }
    );
    const pending = fixture.runtime.request(input);
    await ready.promise;
    request.method = "DELETE";
    request.target = "/v1/changed";
    request.credential!.value = "changed-key";
    request.cacheNamespaceRoot!.fill(0x66);
    request.headers![0].value = "changed";
    request.body!.fill(0x66);
    policy.environment = "production";
    policy.pcr0DevValues![0] = "22".repeat(48);
    input.apiUrl = "https://different.example.test";
    input.beforeSend = () => {
      throw new Error("replacement fence must not run");
    };
    gate.resolve();
    expect(await (await pending).response.text()).toBe("ok");
    expect(fence).toHaveBeenCalledTimes(2);
    for (const attempt of fixture.attempts) {
      expect(attempt.wire).toEqual(
        await encryptTransportV2Request(attempt.keys, attempt.requestId, expected)
      );
    }
    expect(fixture.establish.mock.calls[0][0]).toEqual(fixture.establish.mock.calls[1][0]);
    expect(fixture.establish.mock.calls[1][0].pcrConfig).toMatchObject({
      environment: "development",
      pcr0DevValues: ["11".repeat(48)]
    });
  });

  test("coalesces repair and reuses a replacement established by another stale caller", async () => {
    const firstHint = deferred<Response>();
    const secondHint = deferred<Response>();
    const sentBoth = deferred<void>();
    const fixture = await harness((attempt) => {
      if (attempt.index === 0) return firstHint.promise;
      if (attempt.index === 1) {
        sentBoth.resolve();
        return secondHint.promise;
      }
      return responseFor(attempt);
    });
    const input = { apiUrl: API_URL, request: { method: "POST", target: "/v1/item" } };
    const first = fixture.runtime.request(input);
    const second = fixture.runtime.request(input);
    await sentBoth.promise;
    firstHint.resolve(hint("session_not_found"));
    expect(await (await first).response.text()).toBe("ok");
    secondHint.resolve(hint("request_decryption_failed"));
    expect(await (await second).response.text()).toBe("ok");
    expect(fixture.establish).toHaveBeenCalledTimes(2);
    expect(fixture.attempts.map((attempt) => attempt.session)).toEqual([0, 0, 1, 1]);
  });

  for (const interruption of ["account", "cancel"] as const) {
    test(`a ${interruption} change during repair prevents the second send`, async () => {
      const repairStarted = deferred<void>();
      const gate = deferred<void>();
      const controller = new AbortController();
      let accountCurrent = true;
      const fixture = await harness(
        () => hint("session_not_found"),
        async (index) => {
          if (index === 1) {
            repairStarted.resolve();
            await gate.promise;
          }
        }
      );
      const pending = fixture.runtime.request({
        apiUrl: API_URL,
        request: {
          method: "POST",
          target: "/v1/item",
          credential: { kind: "bearer", value: "old-account" }
        },
        signal: controller.signal,
        beforeSend: () => {
          if (!accountCurrent) throw new Error("authority changed");
        }
      });
      await repairStarted.promise;
      accountCurrent = interruption !== "account";
      if (interruption === "cancel") controller.abort(new Error("caller cancelled"));
      gate.resolve();
      await expect(pending).rejects.toThrow(
        interruption === "cancel" ? "caller cancelled" : "authority changed"
      );
      expect(fixture.attempts).toHaveLength(1);
    });
  }

  test("cancellation after the hint does not start a replacement handshake", async () => {
    const controller = new AbortController();
    const fixture = await harness(() => {
      controller.abort(new Error("caller cancelled"));
      return hint("session_not_found");
    });
    await expect(
      fixture.runtime.request({
        apiUrl: API_URL,
        signal: controller.signal,
        request: { method: "POST", target: "/v1/item" }
      })
    ).rejects.toThrow("caller cancelled");
    expect(fixture.establish).toHaveBeenCalledTimes(1);
    expect(fixture.attempts).toHaveLength(1);
  });

  for (const failure of [
    "network",
    "outer400",
    "outer503",
    "response-auth",
    "framing",
    "partial-stream"
  ] as const) {
    test(`${failure} never repairs or resends the operation`, async () => {
      const fixture = await harness((attempt) => {
        if (failure === "network") throw new Error("connection lost after send");
        if (failure === "outer400" || failure === "outer503")
          return new Response("rejected", { status: failure === "outer400" ? 400 : 503 });
        if (failure === "framing")
          return new Response(new Uint8Array([0, 0]), {
            headers: { "content-type": "application/octet-stream" }
          });
        return responseFor(attempt, {
          corrupt: failure === "response-auth",
          truncate: failure === "partial-stream"
        });
      });
      await expect(
        (async () => {
          const result = await fixture.runtime.request({
            apiUrl: API_URL,
            request: { method: "POST", target: "/v1/item" }
          });
          await result.response.text();
        })()
      ).rejects.toBeInstanceOf(Error);
      expect(fixture.attempts).toHaveLength(1);
      expect(fixture.establish).toHaveBeenCalledTimes(1);
    });
  }

  test("an authenticated logical error with matching headers is returned without repair", async () => {
    const fixture = await harness((attempt) =>
      responseFor(attempt, {
        status: 400,
        logicalHeaders: [
          { name: CONTRACT, value: "1" },
          { name: CODE, value: "session_not_found" }
        ]
      })
    );
    const result = await fixture.runtime.request({
      apiUrl: API_URL,
      request: { method: "POST", target: "/v1/item" }
    });
    expect(result.response.status).toBe(400);
    expect(await result.response.text()).toBe("ok");
    expect(fixture.attempts).toHaveLength(1);
  });

  test("even a recovery-shaped session-establishment failure is never retried", async () => {
    const fixture = await harness(
      (attempt) => responseFor(attempt),
      async () => {
        throw new TransportV2UntrustedRecoveryHint("session_not_found");
      }
    );
    await expect(
      fixture.runtime.request({ apiUrl: API_URL, request: { method: "POST", target: "/v1/item" } })
    ).rejects.toBeInstanceOf(TransportV2UntrustedRecoveryHint);
    expect(fixture.establish).toHaveBeenCalledTimes(1);
    expect(fixture.attempts).toHaveLength(0);
  });

  test("OAuth callback continuations require restart after a recovery hint", async () => {
    const fixture = await harness((attempt) =>
      attempt.index === 0 ? responseFor(attempt) : hint("session_not_found")
    );
    const begin = await fixture.runtime.request({
      apiUrl: API_URL,
      request: { method: "POST", target: "/v1/oauth/github" }
    });
    await begin.response.text();
    begin.rememberOAuthContinuation("github", "fixture-state");
    await expect(
      fixture.runtime.request({
        apiUrl: API_URL,
        oauthCallback: { provider: "github", state: "fixture-state" },
        request: { method: "POST", target: "/v1/oauth/github/callback" }
      })
    ).rejects.toBeInstanceOf(TransportV2UntrustedRecoveryHint);
    expect(fixture.attempts).toHaveLength(2);
    expect(fixture.establish).toHaveBeenCalledTimes(1);
    expect(globalThis.sessionStorage.length).toBe(0);
  });

  for (const target of [
    "/auth/github/callback",
    "/auth/google/callback",
    "/auth/apple/callback",
    "/auth/native-handoff/redeem"
  ].flatMap((path) => [path, `${path}?fixture=1`])) {
    test(`never rebuilds a session-bound OAuth operation at ${target}`, async () => {
      const fixture = await harness(() => hint("request_decryption_failed"));
      await expect(
        fixture.runtime.request({ apiUrl: API_URL, request: { method: "POST", target } })
      ).rejects.toBeInstanceOf(TransportV2UntrustedRecoveryHint);
      expect(fixture.attempts).toHaveLength(1);
      expect(fixture.establish).toHaveBeenCalledTimes(1);
    });
  }
});

describe("Transport V2 recovery snapshot bounds", () => {
  test("rejects an oversized body or metadata before copying or establishing", async () => {
    const fixture = await harness((attempt) => responseFor(attempt));
    for (const request of [
      { method: "POST", target: "/v1/item", body: new Uint8Array(MAX_REQUEST_BODY_BYTES + 1) },
      { method: "POST", target: "/v1/item", cacheNamespaceRoot: new Uint8Array(33) },
      {
        method: "POST",
        target: "/v1/item",
        headers: Array.from({ length: 65 }, () => ({ name: "x-test", value: "one" }))
      }
    ]) {
      await expect(fixture.runtime.request({ apiUrl: API_URL, request })).rejects.toBeInstanceOf(
        TransportV2ProtocolError
      );
    }
    expect(fixture.establish).toHaveBeenCalledTimes(0);
    expect(fixture.attempts).toHaveLength(0);
  });

  test("native Apple sign-in keeps ordinary one-shot repair", async () => {
    const fixture = await harness((attempt) =>
      attempt.index === 0 ? hint("session_not_found") : responseFor(attempt)
    );
    const result = await fixture.runtime.request({
      apiUrl: API_URL,
      request: { method: "POST", target: "/auth/apple/native" }
    });
    expect(await result.response.text()).toBe("ok");
    expect(fixture.attempts).toHaveLength(2);
  });
});

describe("Transport V2 untrusted outer recovery classifier", () => {
  const cases: {
    name: string;
    status?: number;
    headers?: [string, string][];
    redirected?: boolean;
    eligible?: boolean;
  }[] = [
    ...CODES.map((code) => ({
      name: code,
      headers: [
        [CONTRACT, "1"],
        [CODE, code]
      ] as [string, string][],
      eligible: true
    })),
    { name: "headerless 400" },
    { name: "missing code", headers: [[CONTRACT, "1"]] },
    { name: "code without contract", headers: [[CODE, "session_not_found"]] },
    {
      name: "future contract",
      headers: [
        [CONTRACT, "2"],
        [CODE, "session_not_found"]
      ]
    },
    {
      name: "future code",
      headers: [
        [CONTRACT, "1"],
        [CODE, "future_recovery"]
      ]
    },
    {
      name: "duplicate contract",
      headers: [
        [CONTRACT, "1"],
        [CONTRACT, "1"],
        [CODE, "session_not_found"]
      ]
    },
    {
      name: "duplicate code",
      headers: [
        [CONTRACT, "1"],
        [CODE, "session_not_found"],
        [CODE, "session_not_found"]
      ]
    },
    {
      name: "mixed codes",
      headers: [
        [CONTRACT, "1"],
        [CODE, "session_not_found"],
        [CODE, "request_decryption_failed"]
      ]
    },
    {
      name: "wrong status",
      status: 503,
      headers: [
        [CONTRACT, "1"],
        [CODE, "session_not_found"]
      ]
    },
    {
      name: "redirected",
      redirected: true,
      headers: [
        [CONTRACT, "1"],
        [CODE, "session_not_found"]
      ]
    },
    {
      name: "replay rejection",
      headers: [
        [CONTRACT, "1"],
        [CODE, "duplicate_request"]
      ]
    },
    {
      name: "case mismatch",
      headers: [
        [CONTRACT, "1"],
        [CODE, "Session_Not_Found"]
      ]
    }
  ];
  for (const entry of cases) {
    test(entry.name, async () => {
      const fixture = await harness((attempt) => responseFor(attempt));
      const session = new TransportV2Session(fixture.keys[0], ROUTING_KEY, 3600);
      const cancelled = mock(() => {});
      const response = new Response(new ReadableStream({ cancel: cancelled }), {
        status: entry.status ?? 400,
        headers: entry.headers
      });
      if (entry.redirected) Object.defineProperty(response, "redirected", { value: true });
      let caught: unknown;
      try {
        await session.openResponse(response, new Uint8Array(16));
      } catch (error) {
        caught = error;
      }
      expect(caught).toBeInstanceOf(TransportV2ProtocolError);
      expect(caught instanceof TransportV2UntrustedRecoveryHint).toBe(entry.eligible ?? false);
      expect(cancelled).toHaveBeenCalledTimes(1);
    });
  }
});
