/*
 * Verification of a release-approval bundle against an explicit approval key
 * and trusted root.
 *
 * verifyReleaseBundle runs the whole check: the byte cap, the duplicate-member
 * rule, the verifier's strict managed-key profile (DSSE signature, one Rekor v2
 * entry with its inclusion proof and checkpoint, one RFC 3161 timestamp), then
 * Maple's statement policy. Nothing here touches the network; callers
 * supply the bundle text and the trust inputs.
 */
import {
  ManagedKeyVerifier,
  STRICT_MANAGED_KEY_PROFILE,
  VerificationError,
  type TrustedRoot
} from "@freedomofpress/sigstore-browser";
import { installEd25519Fallback } from "@freedomofpress/crypto-browser/noble";

import type { PcrEnvironment } from "../pcr";
import { hasDuplicateJsonMember } from "./duplicateJsonKey";

// The verifier does not install the Ed25519 fallback itself; engines without
// native Ed25519 (WebKit before iOS 17) need it before any trusted root is
// loaded, or the Rekor checkpoint keys fail to import. On engines with native
// Ed25519 the fallback defers to the platform.
let ed25519Ready: Promise<unknown> | undefined;

function ensureEd25519Fallback(): Promise<unknown> {
  ed25519Ready ??= installEd25519Fallback();
  return ed25519Ready;
}

/** Roughly forty times the real bundle; nothing legitimate comes close. */
export const MAX_BUNDLE_BYTES = 256 * 1024;

export const IN_TOTO_PAYLOAD_TYPE = "application/vnd.in-toto+json";
export const IN_TOTO_STATEMENT_TYPE = "https://in-toto.io/Statement/v1";
export const RELEASE_STATEMENT_PREDICATE_TYPE = "https://maple.ai/attestations/enclave-release/v1";

const PCR_HEX = /^[0-9a-f]{96}$/;
const SHA256_HEX = /^[0-9a-f]{64}$/;
// The release statement names its source commit as a full Git commit hash.
const COMMIT_HEX = /^[0-9a-f]{40}$/;
const LOG_INDEX_DIGITS = /^[0-9]+$/;
// The build run is a GitHub Actions run ID, written as a decimal string.
const RUN_ID_DIGITS = /^[0-9]+$/;

export interface ExpectedMeasurements {
  environment: PcrEnvironment;
  pcr0: string;
  pcr1: string;
  pcr2: string;
}

/** Signed audit claims about where a release came from. Not policy inputs. */
export interface ReleaseSource {
  repository: string;
  commit: string;
  /** The workflow run that built the release, and which attempt of it. */
  buildRunId: string;
  buildRunAttempt: number;
}

export type Failure = { ok: false; category: string };

export type ReleaseVerification = { ok: true; source: ReleaseSource; logIndex: string } | Failure;

type Parsed<T> = { ok: true; value: T } | Failure;

type Json = Record<string, unknown>;

function isRecord(value: unknown): value is Json {
  const isObject = typeof value === "object" && value !== null;
  const isArray = Array.isArray(value);
  return isObject && !isArray;
}

function isNonEmptyString(value: unknown): value is string {
  const isString = typeof value === "string";
  return isString && value.length > 0;
}

function fail(category: string): Failure {
  return { ok: false, category };
}

/**
 * Parses JSON text that must not name any member twice. Text that is not
 * JSON and text with a repeated member get their own categories.
 */
function parseUnambiguousJson(text: string): Parsed<unknown> {
  let repeated: boolean;
  try {
    repeated = hasDuplicateJsonMember(text);
  } catch {
    return fail("json-syntax-error");
  }
  if (repeated) {
    return fail("duplicate-json-key");
  }
  return { ok: true, value: JSON.parse(text) };
}

function parseBundleText(text: string): Parsed<unknown> {
  const byteLength = new TextEncoder().encode(text).byteLength;
  const tooLarge = byteLength > MAX_BUNDLE_BYTES;
  if (tooLarge) {
    return fail("bundle-too-large");
  }
  return parseUnambiguousJson(text);
}

function readTlogEntries(bundle: unknown): unknown[] {
  if (!isRecord(bundle)) {
    return [];
  }
  const material = bundle.verificationMaterial;
  if (!isRecord(material)) {
    return [];
  }
  const entries = material.tlogEntries;
  if (!Array.isArray(entries)) {
    return [];
  }
  return entries;
}

/**
 * A log entry names its index twice: on the entry and again in its inclusion
 * proof. Verifiers differ in which of the two they prove inclusion at, so an
 * entry whose two indexes differ is rejected before verification. Every
 * client then gives the same answer for the same bundle, and the index that
 * is reported is the one that was proven. An entry without a proof is left
 * for the verifier to reject.
 */
function checkLogIndexesAgree(bundle: unknown): { ok: true } | Failure {
  for (const entry of readTlogEntries(bundle)) {
    if (!isRecord(entry)) {
      continue;
    }
    const proof = entry.inclusionProof;
    if (!isRecord(proof)) {
      continue;
    }
    const entryIndex = entry.logIndex;
    const proofIndex = proof.logIndex;
    const entryIndexIsNumber = typeof entryIndex === "string" && LOG_INDEX_DIGITS.test(entryIndex);
    const indexesAgree = entryIndexIsNumber && entryIndex === proofIndex;
    if (!indexesAgree) {
      return fail("tlog-index-mismatch");
    }
  }
  return { ok: true };
}

/** The index of the bundle's log entry, once checkLogIndexesAgree and the verifier have passed. */
function readVerifiedLogIndex(bundle: unknown): string {
  const entry: unknown = readTlogEntries(bundle)[0];
  if (!isRecord(entry)) {
    return "";
  }
  const logIndex = entry.logIndex;
  if (typeof logIndex !== "string") {
    return "";
  }
  return logIndex;
}

function verificationFailure(error: unknown): Failure {
  const hasReasonCode = error instanceof VerificationError && typeof error.code === "string";
  if (hasReasonCode) {
    return fail(error.code);
  }
  return fail("verifier-failed");
}

async function strictVerifier(trustedRoot: TrustedRoot): Promise<ManagedKeyVerifier> {
  await ensureEd25519Fallback();
  const verifier = new ManagedKeyVerifier(STRICT_MANAGED_KEY_PROFILE);
  await verifier.loadTrustedRoot(trustedRoot);
  return verifier;
}

/** The DSSE envelope must carry an in-toto statement. */
export function checkPayloadType(payloadType: string): { ok: true } | Failure {
  const supported = payloadType === IN_TOTO_PAYLOAD_TYPE;
  if (supported) {
    return { ok: true };
  }
  return fail("statement-unsupported");
}

// The subject digest is a signed claim the client cannot check, since it
// never has the EIF. Only its shape is checked here.
function subjectIsWellFormed(subject: unknown): boolean {
  const isArray = Array.isArray(subject);
  if (!isArray) {
    return false;
  }
  const isEmpty = subject.length === 0;
  if (isEmpty) {
    return false;
  }
  return subject.every(subjectEntryIsWellFormed);
}

function subjectEntryIsWellFormed(entry: unknown): boolean {
  if (!isRecord(entry)) {
    return false;
  }
  if (!isNonEmptyString(entry.name)) {
    return false;
  }
  const digest = entry.digest;
  if (!isRecord(digest)) {
    return false;
  }
  const sha256 = digest.sha256;
  const digestIsSha256 = typeof sha256 === "string" && SHA256_HEX.test(sha256);
  return digestIsSha256;
}

function parseStatement(payload: Uint8Array): Parsed<Json> {
  let text: string;
  try {
    // A leading byte order mark is kept, so JSON parsing rejects it as the
    // Rust SDK does, instead of being silently dropped.
    text = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(payload);
  } catch {
    return fail("statement-malformed");
  }
  const parsed = parseUnambiguousJson(text);
  if (!parsed.ok) {
    return parsed;
  }
  if (!isRecord(parsed.value)) {
    return fail("statement-malformed");
  }
  return { ok: true, value: parsed.value };
}

function checkStatementType(statement: Json): { ok: true } | Failure {
  const statementTypeSupported = statement._type === IN_TOTO_STATEMENT_TYPE;
  if (!statementTypeSupported) {
    return fail("statement-unsupported");
  }
  if (!subjectIsWellFormed(statement.subject)) {
    return fail("statement-malformed");
  }
  const predicateTypeSupported = statement.predicateType === RELEASE_STATEMENT_PREDICATE_TYPE;
  if (!predicateTypeSupported) {
    return fail("statement-unsupported");
  }
  return { ok: true };
}

// The signed PCRs must equal the tuple the enclave just authenticated with its
// Nitro document.
function checkMeasurements(
  measurements: unknown,
  expected: ExpectedMeasurements
): { ok: true } | Failure {
  if (!isRecord(measurements)) {
    return fail("statement-malformed");
  }
  const pcr0 = measurements.PCR0;
  const pcr1 = measurements.PCR1;
  const pcr2 = measurements.PCR2;
  const allWellFormed = [pcr0, pcr1, pcr2].every(
    (value) => typeof value === "string" && PCR_HEX.test(value)
  );
  if (!allWellFormed) {
    return fail("statement-malformed");
  }
  const pcr0Matches = pcr0 === expected.pcr0;
  const pcr1Matches = pcr1 === expected.pcr1;
  const pcr2Matches = pcr2 === expected.pcr2;
  const allMatch = pcr0Matches && pcr1Matches && pcr2Matches;
  if (!allMatch) {
    return fail("statement-measurement-mismatch");
  }
  return { ok: true };
}

// Repository, commit and build run are logged as audit data and never
// compared with pinned values; only their shape is required.
function readReleaseSource(source: unknown, build: unknown): Parsed<ReleaseSource> {
  if (!isRecord(source)) {
    return fail("statement-malformed");
  }
  if (!isRecord(build)) {
    return fail("statement-malformed");
  }
  const repository = source.repository;
  const commit = source.commit;
  const buildRunId = build.runId;
  const buildRunAttempt = build.runAttempt;
  const repositoryPresent = isNonEmptyString(repository);
  const commitIsHash = typeof commit === "string" && COMMIT_HEX.test(commit);
  const runIdIsNumber = typeof buildRunId === "string" && RUN_ID_DIGITS.test(buildRunId);
  // A whole number that every JSON reader holds exactly, so that 1, 1.0 and
  // 1e0 are judged alike here and in the Rust SDK.
  const attemptIsWholeNumber =
    typeof buildRunAttempt === "number" && Number.isSafeInteger(buildRunAttempt);
  const attemptIsCount = attemptIsWholeNumber && buildRunAttempt >= 1;
  const wellFormed = repositoryPresent && commitIsHash && runIdIsNumber && attemptIsCount;
  if (!wellFormed) {
    return fail("statement-malformed");
  }
  return { ok: true, value: { repository, commit, buildRunId, buildRunAttempt } };
}

/**
 * Maple's statement policy, applied to the payload of a verified envelope:
 * in-toto statement and predicate type, the signed environment, and all three
 * PCRs against the authenticated tuple.
 */
export function checkReleaseStatement(
  payload: Uint8Array,
  expected: ExpectedMeasurements
): Parsed<ReleaseSource> {
  const statement = parseStatement(payload);
  if (!statement.ok) {
    return statement;
  }
  const types = checkStatementType(statement.value);
  if (!types.ok) {
    return types;
  }
  const predicate = statement.value.predicate;
  if (!isRecord(predicate)) {
    return fail("statement-malformed");
  }
  const environmentMatches = predicate.environment === expected.environment;
  if (!environmentMatches) {
    return fail("statement-environment-mismatch");
  }
  const measurements = checkMeasurements(predicate.measurements, expected);
  if (!measurements.ok) {
    return measurements;
  }
  return readReleaseSource(predicate.source, predicate.build);
}

export type VerifiedEnvelope = {
  ok: true;
  payload: Uint8Array;
  payloadType: string;
  logIndex: string;
};

/**
 * The cryptographic layer alone: size, duplicate members and the strict
 * managed-key profile for a DSSE bundle. The shared corpus runs here, since
 * its vectors sign statements other than Maple's. Never throws.
 */
export async function verifyDsseBundle(
  bundleText: string,
  publicKeyPem: string,
  trustedRoot: TrustedRoot
): Promise<VerifiedEnvelope | Failure> {
  const parsed = parseBundleText(bundleText);
  if (!parsed.ok) {
    return parsed;
  }
  const indexes = checkLogIndexesAgree(parsed.value);
  if (!indexes.ok) {
    return indexes;
  }
  try {
    const verifier = await strictVerifier(trustedRoot);
    const envelope = await verifier.verifyDsse(bundleText, publicKeyPem);
    return {
      ok: true,
      payload: envelope.payload,
      payloadType: envelope.payloadType,
      logIndex: readVerifiedLogIndex(parsed.value)
    };
  } catch (error) {
    return verificationFailure(error);
  }
}

/**
 * Full verification of one release bundle: the cryptographic layer, then
 * Maple's statement policy. Never throws.
 */
export async function verifyReleaseBundle(
  bundleText: string,
  publicKeyPem: string,
  trustedRoot: TrustedRoot,
  expected: ExpectedMeasurements
): Promise<ReleaseVerification> {
  const envelope = await verifyDsseBundle(bundleText, publicKeyPem, trustedRoot);
  if (!envelope.ok) {
    return envelope;
  }
  const payloadType = checkPayloadType(envelope.payloadType);
  if (!payloadType.ok) {
    return payloadType;
  }
  const statement = checkReleaseStatement(envelope.payload, expected);
  if (!statement.ok) {
    return statement;
  }
  return { ok: true, source: statement.value, logIndex: envelope.logIndex };
}
