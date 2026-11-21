/*
 * Report-only observation of release-approval evidence for a session.
 *
 * Runs after the Nitro document and PCR policy have accepted a remote enclave.
 * It finds the bundle for the authenticated measurements (verified cache,
 * embedded bundles, then a bounded fetch from the environment's fixed host),
 * verifies it with the environment's approval key, logs the result and keeps
 * it for tests and a later enforcement gate. Nothing here can change whether
 * the session is accepted.
 */
import type { PcrEnvironment } from "../pcr";
import { approvalKeyPem } from "./approvalKeys";
import { cacheKey, clearVerificationCache, getCachedPass, storePass } from "./cache";
import { EMBEDDED_BUNDLES, findEmbeddedBundle, type EmbeddedBundle } from "./embedded";
import { BUNDLE_HOSTS, fetchReleaseBundle, type FetchBundle } from "./fetchBundle";
import { embeddedTrustedRoot } from "./trustedRoot";
import { verifyReleaseBundle, type ExpectedMeasurements } from "./verification";
import type { ReleaseEvidenceResult, ReleaseEvidenceSource } from "./types";

/** The environment and authenticated PCRs as lowercase hex; empty when absent. */
export type ReleaseEvidenceObservation = ExpectedMeasurements;

/** Test seams; production callers use the defaults. */
export interface ReleaseEvidenceOptions {
  fetchImpl?: FetchBundle;
  embeddedBundles?: readonly EmbeddedBundle[];
}

// Nitro PCRs are SHA-384 values: 96 lowercase hex characters.
const PCR_HEX = /^[0-9a-f]{96}$/;

let lastResult: ReleaseEvidenceResult | undefined;

/** The most recent observation in this process. Diagnostic only. */
export function getLastReleaseEvidenceResult(): ReleaseEvidenceResult | undefined {
  return lastResult;
}

/** Test hook: forget the cached passes and the last result. */
export function resetReleaseEvidenceState(): void {
  clearVerificationCache();
  lastResult = undefined;
}

function hexOrEmpty(bytes: Uint8Array | undefined): string {
  const absent = bytes === undefined;
  if (absent) {
    return "";
  }
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

/** The observation input for an authenticated document's PCR map. */
export function releaseEvidenceObservationFromPcrs(
  pcrs: Map<number, Uint8Array>,
  environment: PcrEnvironment
): ReleaseEvidenceObservation {
  return {
    environment,
    pcr0: hexOrEmpty(pcrs.get(0)),
    pcr1: hexOrEmpty(pcrs.get(1)),
    pcr2: hexOrEmpty(pcrs.get(2))
  };
}

function logResult(result: ReleaseEvidenceResult): void {
  const passed = result.outcome === "pass";
  if (passed) {
    // Public identity only: no payload, signature, subject digest or session data.
    const identity = result.identity;
    console.info(
      `release evidence: pass environment=${result.environment} pcr0=${result.pcr0} ` +
        `source=${result.source} repository=${identity.repository} ` +
        `commit=${identity.commit} logIndex=${identity.logIndex}`
    );
    return;
  }
  let sourceField = "";
  if (result.source) {
    sourceField = ` source=${result.source}`;
  }
  console.warn(
    `release evidence: fail environment=${result.environment} pcr0=${result.pcr0} ` +
      `category=${result.category}${sourceField}`
  );
}

function remember(result: ReleaseEvidenceResult): ReleaseEvidenceResult {
  lastResult = result;
  logResult(result);
  return result;
}

function failure(
  input: ReleaseEvidenceObservation,
  category: string,
  source?: ReleaseEvidenceSource
): ReleaseEvidenceResult {
  return remember({
    outcome: "fail",
    environment: input.environment,
    pcr0: input.pcr0,
    category,
    source
  });
}

function callGlobalFetch(url: string, init: RequestInit): Promise<Response> {
  return globalThis.fetch(url, init);
}

function tupleIsWellFormed(input: ReleaseEvidenceObservation): boolean {
  const pcrs = [input.pcr0, input.pcr1, input.pcr2];
  return pcrs.every((pcr) => PCR_HEX.test(pcr));
}

async function verifyAndRemember(
  input: ReleaseEvidenceObservation,
  bundleText: string,
  source: ReleaseEvidenceSource,
  publicKeyPem: string
): Promise<ReleaseEvidenceResult> {
  const result = await verifyReleaseBundle(bundleText, publicKeyPem, embeddedTrustedRoot(), input);
  if (!result.ok) {
    return failure(input, result.category, source);
  }
  const pass: ReleaseEvidenceResult = {
    outcome: "pass",
    environment: input.environment,
    pcr0: input.pcr0,
    source,
    identity: {
      environment: input.environment,
      pcr0: input.pcr0,
      repository: result.source.repository,
      commit: result.source.commit,
      logIndex: result.logIndex
    }
  };
  storePass(cacheKey(input.environment, input.pcr0, input.pcr1, input.pcr2), pass);
  return remember(pass);
}

async function resolveAndVerify(
  input: ReleaseEvidenceObservation,
  options: ReleaseEvidenceOptions
): Promise<ReleaseEvidenceResult> {
  if (!tupleIsWellFormed(input)) {
    return failure(input, "statement-measurement-mismatch");
  }
  const publicKeyPem = approvalKeyPem(input.environment);
  const keyConfigured = publicKeyPem !== undefined;
  if (!keyConfigured) {
    return failure(input, "approval-key-not-configured");
  }

  const cached = getCachedPass(cacheKey(input.environment, input.pcr0, input.pcr1, input.pcr2));
  const hasCachedPass = cached !== undefined;
  if (hasCachedPass) {
    return remember({ ...cached, source: "cache" });
  }

  const bundles = options.embeddedBundles ?? EMBEDDED_BUNDLES;
  const embedded = findEmbeddedBundle(input.environment, input.pcr0, bundles);
  const isEmbedded = embedded !== undefined;
  if (isEmbedded) {
    return verifyAndRemember(input, embedded.bundleText, "embedded", publicKeyPem);
  }

  // The global fetch is looked up only when the request is made, inside
  // fetchReleaseBundle's own try, so a host without one still gets a logged
  // bundle-fetch-failed.
  const fetchImpl = options.fetchImpl ?? callGlobalFetch;
  const fetched = await fetchReleaseBundle(BUNDLE_HOSTS[input.environment], input.pcr0, fetchImpl);
  if (!fetched.ok) {
    return failure(input, fetched.category, "fetched");
  }
  return verifyAndRemember(input, fetched.text, "fetched", publicKeyPem);
}

/**
 * Observe one session's release evidence and log the outcome. A failure in
 * any step becomes a logged result; the returned promise rejects only if
 * logging itself throws, and the session hook drops that too.
 */
export async function observeReleaseEvidence(
  input: ReleaseEvidenceObservation,
  options: ReleaseEvidenceOptions = {}
): Promise<ReleaseEvidenceResult> {
  try {
    return await resolveAndVerify(input, options);
  } catch {
    return failure(input, "verifier-failed");
  }
}
