// The report-only observation: bundle lookup, cache and embedded bundles, and
// how the Transport V2 session starts it.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import nacl from "tweetnacl";

import type { AttestationDocument } from "../attestation";
import { approvalKeyPem } from "../releaseEvidence/approvalKeys";
import { EMBEDDED_BUNDLES, findEmbeddedBundle } from "../releaseEvidence/embedded";
import { fetchReleaseBundle, type FetchBundle } from "../releaseEvidence/fetchBundle";
import {
  getLastReleaseEvidenceResult,
  observeReleaseEvidence,
  resetReleaseEvidenceState,
  type ReleaseEvidenceObservation
} from "../releaseEvidence/observe";
import {
  TransportV2Client,
  verifyTransportV2AttestationDocumentWithDependencies,
  type TransportV2AttestationVerificationDependencies,
  type TransportV2ClientDependencies
} from "../transportV2/client";
import { attestationUserData, deriveTransportV2SessionKeys } from "../transportV2/crypto";

const SDK_ROOT = resolve(import.meta.dir, "../../..");
const REPO_ROOT = resolve(SDK_ROOT, "..");
const PUBLISHED_BUNDLE_PATH = resolve(
  SDK_ROOT,
  "src/lib/releaseEvidence/published/dev-b422d5fc.sigstore.json"
);
// The reviewed copy the release pipeline signs with. The SDK lane stages it
// next to the SDK.
const REVIEWED_DEV_KEY_PATH = resolve(
  REPO_ROOT,
  "scripts/ci/opensecret-release/dev-signing-key.pem"
);
const PUBLISHED_BUNDLE = readFileSync(PUBLISHED_BUNDLE_PATH, "utf8");

// The measurements the published Dev release statement approves.
const DEV_RELEASE: ReleaseEvidenceObservation = {
  environment: "development",
  pcr0: "b422d5fcb25929a1b76f0eb8aa10c26a78c635a86a58039b48f22eb0dc4dc6f8ae6e646aa2fbbc57db756e6f06cad3dc",
  pcr1: "e83dc536d4cf82daf0e0821e7fdbddf25cd76090602cb67bef7c9d5ab2914417ee6a94bd0c9c1e7ed15a3c3c3bfcc0a3",
  pcr2: "57b7e86919f571beff7539cb8643baafe81ec9f2ffb3434d683268978675a664e0d043dead3367aba14a5d2814a4be97"
};
// A measurement no bundle approves, so lookups for it miss the embedded bundles.
const UNKNOWN_RELEASE: ReleaseEvidenceObservation = { ...DEV_RELEASE, pcr0: "cd".repeat(48) };

const REMOTE_API_URL = "https://enclave.example.test";
const LOOPBACK_API_URL = "http://127.0.0.1:3000";

function respondWith(body: BodyInit | null, init: ResponseInit): FetchBundle {
  return async () => new Response(body, init);
}

function countingFetch(respond: () => Response): { fetchImpl: FetchBundle; calls: () => number } {
  let calls = 0;
  const fetchImpl: FetchBundle = async () => {
    calls += 1;
    return respond();
  };
  return { fetchImpl, calls: () => calls };
}

const networkForbidden: FetchBundle = async () => {
  throw new Error("this observation must not use the network");
};

// The session starts the observation without waiting; tests that check its
// outcome poll until it lands, bounded so a stuck observer fails the test.
const POLL_ATTEMPTS = 500;
const POLL_INTERVAL_MS = 2;

async function waitFor(read: () => unknown): Promise<void> {
  for (let attempt = 0; attempt < POLL_ATTEMPTS; attempt += 1) {
    const landed = read() !== undefined;
    if (landed) {
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, POLL_INTERVAL_MS));
  }
}

beforeEach(() => {
  resetReleaseEvidenceState();
});

describe("approval keys", () => {
  test("the Dev key is the reviewed signing key", () => {
    expect(approvalKeyPem("development")).toBe(readFileSync(REVIEWED_DEV_KEY_PATH, "utf8"));
  });

  test("production has no key and never falls back to Dev", () => {
    expect(approvalKeyPem("production")).toBeUndefined();
  });
});

describe("embedded bundles", () => {
  test("hold the published Dev bundle text exactly, matching its pin", () => {
    const entry = findEmbeddedBundle("development", DEV_RELEASE.pcr0, EMBEDDED_BUNDLES);
    expect(entry?.bundleText).toBe(PUBLISHED_BUNDLE);
    const pinned = readFileSync(`${PUBLISHED_BUNDLE_PATH}.sha256`, "utf8").split(" ")[0];
    const embeddedHash = createHash("sha256")
      .update(entry?.bundleText ?? "")
      .digest("hex");
    expect(embeddedHash).toBe(pinned);
  });

  test("are keyed by environment as well as PCR0", () => {
    expect(findEmbeddedBundle("production", DEV_RELEASE.pcr0, EMBEDDED_BUNDLES)).toBeUndefined();
  });
});

describe("observeReleaseEvidence", () => {
  test("verifies the embedded bundle without the network", async () => {
    const result = await observeReleaseEvidence(DEV_RELEASE, { fetchImpl: networkForbidden });
    expect(result).toMatchObject({
      outcome: "pass",
      source: "embedded",
      identity: { logIndex: "207746380" }
    });
  });

  test("reports approval-key-not-configured for production and does not fetch", async () => {
    const lookup = countingFetch(() => new Response(PUBLISHED_BUNDLE, { status: 200 }));
    const result = await observeReleaseEvidence(
      { ...DEV_RELEASE, environment: "production" },
      { fetchImpl: lookup.fetchImpl }
    );
    expect(result).toMatchObject({ outcome: "fail", category: "approval-key-not-configured" });
    expect(lookup.calls()).toBe(0);
  });

  test("reports a missing or short PCR as statement-measurement-mismatch", async () => {
    const missing = await observeReleaseEvidence({ ...DEV_RELEASE, pcr2: "" });
    const short = await observeReleaseEvidence({ ...DEV_RELEASE, pcr1: "ab" });
    expect(missing).toMatchObject({ outcome: "fail", category: "statement-measurement-mismatch" });
    expect(short).toMatchObject({ outcome: "fail", category: "statement-measurement-mismatch" });
  });

  test("fetches, verifies and caches a bundle missing from the embedded set", async () => {
    const lookup = countingFetch(() => new Response(PUBLISHED_BUNDLE, { status: 200 }));
    const first = await observeReleaseEvidence(DEV_RELEASE, {
      fetchImpl: lookup.fetchImpl,
      embeddedBundles: []
    });
    const second = await observeReleaseEvidence(DEV_RELEASE, {
      fetchImpl: networkForbidden,
      embeddedBundles: []
    });
    expect(first).toMatchObject({ outcome: "pass", source: "fetched" });
    expect(second).toMatchObject({ outcome: "pass", source: "cache" });
    expect(lookup.calls()).toBe(1);
  });

  test("a fetched bundle for other measurements fails closed", async () => {
    const result = await observeReleaseEvidence(UNKNOWN_RELEASE, {
      fetchImpl: respondWith(PUBLISHED_BUNDLE, { status: 200 })
    });
    expect(result).toMatchObject({
      outcome: "fail",
      category: "statement-measurement-mismatch",
      source: "fetched"
    });
  });

  test("a 404 is reported and never cached", async () => {
    const lookup = countingFetch(() => new Response("not found", { status: 404 }));
    const first = await observeReleaseEvidence(UNKNOWN_RELEASE, { fetchImpl: lookup.fetchImpl });
    const second = await observeReleaseEvidence(UNKNOWN_RELEASE, { fetchImpl: lookup.fetchImpl });
    expect(first).toMatchObject({ outcome: "fail", category: "bundle-not-found" });
    expect(second).toMatchObject({ outcome: "fail", category: "bundle-not-found" });
    expect(lookup.calls()).toBe(2);
  });

  test("a server error is bundle-fetch-failed", async () => {
    const result = await observeReleaseEvidence(UNKNOWN_RELEASE, {
      fetchImpl: respondWith("boom", { status: 500 })
    });
    expect(result).toMatchObject({ outcome: "fail", category: "bundle-fetch-failed" });
  });

  test("a network error is bundle-fetch-failed", async () => {
    const failing: FetchBundle = async () => {
      throw new TypeError("fetch failed");
    };
    const result = await observeReleaseEvidence(UNKNOWN_RELEASE, { fetchImpl: failing });
    expect(result).toMatchObject({ outcome: "fail", category: "bundle-fetch-failed" });
  });

  test("an oversized body is bundle-too-large", async () => {
    const oversized = new Uint8Array(300 * 1024);
    const result = await observeReleaseEvidence(UNKNOWN_RELEASE, {
      fetchImpl: respondWith(oversized, { status: 200 })
    });
    expect(result).toMatchObject({ outcome: "fail", category: "bundle-too-large" });
  });

  test("a cached pass is not reused for a different PCR tuple", async () => {
    const first = await observeReleaseEvidence(DEV_RELEASE, { fetchImpl: networkForbidden });
    expect(first.outcome).toBe("pass");
    const otherPcr1 = { ...DEV_RELEASE, pcr1: "ef".repeat(48) };
    const result = await observeReleaseEvidence(otherPcr1, { fetchImpl: networkForbidden });
    expect(result).toMatchObject({ outcome: "fail", category: "statement-measurement-mismatch" });
  });

  describe("on a host without a global fetch", () => {
    const originalFetch = globalThis.fetch;
    beforeEach(() => {
      Reflect.deleteProperty(globalThis, "fetch");
    });
    afterEach(() => {
      globalThis.fetch = originalFetch;
    });

    test("still verifies the embedded bundle", async () => {
      const result = await observeReleaseEvidence(DEV_RELEASE);
      expect(result).toMatchObject({ outcome: "pass", source: "embedded" });
    });

    test("reports a lookup as bundle-fetch-failed", async () => {
      const result = await observeReleaseEvidence(UNKNOWN_RELEASE);
      expect(result).toMatchObject({ outcome: "fail", category: "bundle-fetch-failed" });
    });
  });
});

describe("fetchReleaseBundle", () => {
  test("stops at its deadline as bundle-timeout", async () => {
    const hanging: FetchBundle = (_url, init) =>
      new Promise<Response>((_resolve, reject) => {
        init.signal?.addEventListener("abort", () => {
          reject(new DOMException("aborted", "AbortError"));
        });
      });
    const result = await fetchReleaseBundle(
      "https://example.invalid",
      UNKNOWN_RELEASE.pcr0,
      hanging,
      20
    );
    expect(result).toEqual({ ok: false, category: "bundle-timeout" });
  });
});

function pcrMap(observation: ReleaseEvidenceObservation): Map<number, Uint8Array> {
  return new Map([
    [0, Uint8Array.from(Buffer.from(observation.pcr0, "hex"))],
    [1, Uint8Array.from(Buffer.from(observation.pcr1, "hex"))],
    [2, Uint8Array.from(Buffer.from(observation.pcr2, "hex"))]
  ]);
}

function authenticatedDocument(
  challenge: Uint8Array,
  clientPublicKey: Uint8Array,
  serverPublicKey: Uint8Array
): AttestationDocument {
  return {
    module_id: "release-evidence-test",
    digest: "SHA384",
    timestamp: 1760000000,
    pcrs: pcrMap(DEV_RELEASE),
    certificate: new Uint8Array([1]),
    cabundle: [new Uint8Array([2])],
    public_key: new Uint8Array(serverPublicKey),
    user_data: attestationUserData(clientPublicKey),
    nonce: new Uint8Array(challenge)
  };
}

// The Nitro and PCR checks are stubbed as passing, as in the existing ordering
// tests; the observer is whatever the test supplies.
function sessionDependencies(
  document: AttestationDocument,
  observer: TransportV2AttestationVerificationDependencies["observeReleaseEvidence"]
): TransportV2AttestationVerificationDependencies {
  return {
    isLocalDevelopmentApiUrl: (url) => url === LOOPBACK_API_URL,
    authenticateDocument: async () => document,
    parseLocalDocument: async () => document,
    requireTrustedPcr0: async () => ({
      hash: DEV_RELEASE.pcr0,
      validation: { isMatch: true, text: "test trust root" }
    }),
    observeReleaseEvidence: observer
  };
}

describe("session wiring", () => {
  const challenge = new Uint8Array(32).fill(0x61);
  const clientPublicKey = new Uint8Array(32).fill(0x62);
  const serverPublicKey = new Uint8Array(32).fill(0x63);
  const document = authenticatedDocument(challenge, clientPublicKey, serverPublicKey);

  function verify(
    apiUrl: string,
    environment: "development" | "production",
    dependencies: TransportV2AttestationVerificationDependencies
  ): Promise<Uint8Array> {
    return verifyTransportV2AttestationDocumentWithDependencies(
      "document",
      challenge,
      clientPublicKey,
      apiUrl,
      { environment },
      dependencies
    );
  }

  test("an authenticated remote enclave is observed with its PCR tuple", async () => {
    let observed: ReleaseEvidenceObservation | undefined;
    const dependencies = sessionDependencies(document, async (observation) => {
      observed = observation;
    });
    await verify(REMOTE_API_URL, "development", dependencies);
    await waitFor(() => observed);
    expect(observed).toEqual(DEV_RELEASE);
  });

  test("the SDK's observer logs a pass for an authenticated Dev enclave", async () => {
    const dependencies = sessionDependencies(document, (observation) =>
      observeReleaseEvidence(observation, { fetchImpl: networkForbidden })
    );
    await verify(REMOTE_API_URL, "development", dependencies);
    await waitFor(() => getLastReleaseEvidenceResult());
    expect(getLastReleaseEvidenceResult()).toMatchObject({ outcome: "pass", source: "embedded" });
  });

  test("a production session reports approval-key-not-configured", async () => {
    const dependencies = sessionDependencies(document, (observation) =>
      observeReleaseEvidence(observation, { fetchImpl: networkForbidden })
    );
    await verify(REMOTE_API_URL, "production", dependencies);
    await waitFor(() => getLastReleaseEvidenceResult());
    expect(getLastReleaseEvidenceResult()).toMatchObject({
      outcome: "fail",
      environment: "production",
      category: "approval-key-not-configured"
    });
  });

  test("a loopback development server is never observed", async () => {
    let observed = false;
    const dependencies = sessionDependencies(document, async () => {
      observed = true;
    });
    const returned = await verify(LOOPBACK_API_URL, "development", dependencies);
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(returned).toEqual(serverPublicKey);
    expect(observed).toBe(false);
  });

  test("the observer is not called when the PCR gate rejects", async () => {
    let observed = false;
    const dependencies = {
      ...sessionDependencies(document, async () => {
        observed = true;
      }),
      requireTrustedPcr0: async () => {
        throw new Error("untrusted PCR0");
      }
    };
    await expect(verify(REMOTE_API_URL, "development", dependencies)).rejects.toThrow(
      "untrusted PCR0"
    );
    expect(observed).toBe(false);
  });

  describe("an observer that fails", () => {
    const unhandled: unknown[] = [];
    const recordUnhandled = (reason: unknown) => {
      unhandled.push(reason);
    };
    beforeEach(() => {
      unhandled.length = 0;
      process.on("unhandledRejection", recordUnhandled);
    });
    afterEach(() => {
      process.off("unhandledRejection", recordUnhandled);
    });

    const failingObservers: Array<[string, () => Promise<unknown>]> = [
      [
        "by throwing",
        () => {
          throw new Error("observer threw");
        }
      ],
      ["by rejecting", async () => Promise.reject(new Error("observer rejected"))]
    ];
    for (const [how, observer] of failingObservers) {
      test(`${how} leaves the session accepted with no unhandled rejection`, async () => {
        const returned = await verify(
          REMOTE_API_URL,
          "development",
          sessionDependencies(document, observer)
        );
        await new Promise((resolve) => setTimeout(resolve, 20));
        expect(returned).toEqual(serverPublicKey);
        expect(unhandled).toEqual([]);
      });
    }
  });

  test("a slow observer cannot hold the establishment deadline", async () => {
    const clientKeys = nacl.box.keyPair();
    const serverKeys = nacl.box.keyPair();
    const sessionDocument = authenticatedDocument(
      challenge,
      clientKeys.publicKey,
      serverKeys.publicKey
    );
    const shared = nacl.scalarMult(serverKeys.secretKey, clientKeys.publicKey);
    const keys = await deriveTransportV2SessionKeys(shared, {
      challenge,
      clientPublicKey: clientKeys.publicKey,
      serverPublicKey: serverKeys.publicKey
    });
    const establishmentDeadlineMs = 300;
    const observerDelayMs = 400;
    let observerFinished = false;
    const slowObserver = async () => {
      await new Promise((resolve) => setTimeout(resolve, observerDelayMs));
      observerFinished = true;
    };
    const dependencies: TransportV2ClientDependencies = {
      randomBytes: () => new Uint8Array(challenge),
      generateKeyPair: () => ({
        publicKey: new Uint8Array(clientKeys.publicKey),
        secretKey: new Uint8Array(clientKeys.secretKey)
      }),
      establishmentTimeoutMs: establishmentDeadlineMs,
      verifyDocument: (encoded, sessionChallenge, clientKey, url, policy) =>
        verifyTransportV2AttestationDocumentWithDependencies(
          encoded,
          sessionChallenge,
          clientKey,
          url,
          policy,
          sessionDependencies(sessionDocument, slowObserver)
        )
    };
    const sessionResponse = (async () =>
      new Response(
        JSON.stringify({
          version: 2,
          session_id: keys.sessionId,
          attestation_document: "document",
          expires_in_seconds: 3600
        }),
        { status: 200, headers: { "content-type": "application/json" } }
      )) as unknown as typeof fetch;
    const started = Date.now();
    const client = await TransportV2Client.establish(
      {
        apiUrl: REMOTE_API_URL,
        pcrConfig: { environment: "development" },
        fetch: sessionResponse
      },
      dependencies
    );
    const elapsed = Date.now() - started;
    expect(elapsed).toBeLessThan(establishmentDeadlineMs);
    expect(observerFinished).toBe(false);
    expect(client.isUsable()).toBe(true);
    client.dispose();
    await new Promise((resolve) => setTimeout(resolve, observerDelayMs + 100));
    expect(observerFinished).toBe(true);
  });
});
