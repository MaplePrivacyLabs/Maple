import { afterEach, expect, setSystemTime, test } from "bun:test";
import { decode, encode } from "@stablelib/base64";
import * as cbor from "cbor2";
import { X509Certificate } from "@peculiar/x509";
import { authenticate, parseDocumentData, parseDocumentPayload } from "../attestation";
import { ATTESTATION_NOT_BEFORE_LEEWAY_MS, AttestationClockSkewError } from "../attestationClock";
import awsRootCertDer from "../../assets/aws_root.der";
import {
  NITRO_ATTESTATION_DOCUMENT_2024,
  NITRO_ATTESTATION_DOCUMENT_2024_MODULE_ID,
  NITRO_ATTESTATION_DOCUMENT_2024_NONCE,
  NITRO_ATTESTATION_DOCUMENT_2024_TIMESTAMP_MS
} from "./fixtures/nitroAttestationDocument2024";

const SECOND_MS = 1000;
const HOUR_MS = 60 * 60 * SECOND_MS;
const DAY_MS = 24 * HOUR_MS;
const signedAt = NITRO_ATTESTATION_DOCUMENT_2024_TIMESTAMP_MS;

async function leafValidity() {
  const parsed = await parseDocumentData(NITRO_ATTESTATION_DOCUMENT_2024);
  const payload = await parseDocumentPayload(parsed.payload);
  const leaf = new X509Certificate(payload.certificate);
  return { notBefore: leaf.notBefore.getTime(), notAfter: leaf.notAfter.getTime() };
}

function verifyFixture() {
  return authenticate(
    NITRO_ATTESTATION_DOCUMENT_2024,
    awsRootCertDer,
    NITRO_ATTESTATION_DOCUMENT_2024_NONCE
  );
}

async function failureAt(deviceTime: number): Promise<AttestationClockSkewError> {
  setSystemTime(new Date(deviceTime));
  const error = await verifyFixture().catch((caught: unknown) => caught);
  expect(error).toBeInstanceOf(AttestationClockSkewError);
  return error as AttestationClockSkewError;
}

afterEach(() => {
  setSystemTime();
});

test("the fixture's leaf certificate was issued 361 s before the enclave signed the document", async () => {
  const { notBefore, notAfter } = await leafValidity();
  expect(signedAt - notBefore).toBe(361 * SECOND_MS + 206);
  expect(notAfter - notBefore).toBe(3 * HOUR_MS + 3 * SECOND_MS);
});

test("a real document verifies when the device clock matches the enclave", async () => {
  setSystemTime(new Date(signedAt));
  const document = await verifyFixture();
  expect(document.module_id).toBe(NITRO_ATTESTATION_DOCUMENT_2024_MODULE_ID);
  expect(document.timestamp).toBe(signedAt);
});

test("a device clock slightly behind a freshly issued leaf certificate verifies within the leeway", async () => {
  // Used to fail with "Certificate is expired." for any clock behind notBefore.
  const { notBefore } = await leafValidity();
  for (const deviceTime of [
    notBefore - SECOND_MS,
    notBefore - 60 * SECOND_MS,
    notBefore - ATTESTATION_NOT_BEFORE_LEEWAY_MS
  ]) {
    setSystemTime(new Date(deviceTime));
    await expect(verifyFixture()).resolves.toBeTruthy();
  }
});

test("a device clock behind by more than the leeway is rejected as a clock problem", async () => {
  const { notBefore, notAfter } = await leafValidity();
  const deviceTime = notBefore - ATTESTATION_NOT_BEFORE_LEEWAY_MS - SECOND_MS;
  const error = await failureAt(deviceTime);
  expect(error.deviceClockLikelyWrong).toBe(true);
  expect(error.skewMs).toBe(deviceTime - signedAt);
  // The chain builder lists the leaf first, so the failing certificate is index 0.
  expect(error.certificateIndex).toBe(0);
  expect(error.notBefore.getTime()).toBe(notBefore);
  expect(error.notAfter.getTime()).toBe(notAfter);
  expect(error.message).toContain("about 11 minutes behind the secure enclave");
  expect(error.message).toContain("looks not yet valid");
  expect(error.message).toContain("date, time and time zone settings");
});

test("a fast device clock is accepted only while the leaf certificate is still valid", async () => {
  const { notAfter } = await leafValidity();
  setSystemTime(new Date(notAfter - SECOND_MS));
  await expect(verifyFixture()).resolves.toBeTruthy();

  const error = await failureAt(notAfter + SECOND_MS);
  expect(error.deviceClockLikelyWrong).toBe(true);
  expect(error.message).toContain("about 3 hours ahead of the secure enclave");
  expect(error.message).toContain("looks expired");
});

test("a device clock days off fails with a message naming the difference (App Store review device case)", async () => {
  const ahead = await failureAt(signedAt + 3 * DAY_MS);
  expect(ahead.message).toContain("about 3 days ahead of the secure enclave");
  expect(ahead.enclaveTime.getTime()).toBe(signedAt);

  const behind = await failureAt(signedAt - 3 * DAY_MS);
  expect(behind.message).toContain("about 3 days behind the secure enclave");
  expect(behind.message).toContain("looks not yet valid");
});

test("today's clock rejects the 2024 document", async () => {
  await expect(verifyFixture()).rejects.toBeInstanceOf(AttestationClockSkewError);
});

test("a tampered timestamp cannot influence the check because the signature fails first", async () => {
  const parsed = await parseDocumentData(NITRO_ATTESTATION_DOCUMENT_2024);
  const payload = cbor.decode(parsed.payload) as Map<string, unknown> | Record<string, unknown>;
  const setField = (key: string, value: unknown) => {
    if (payload instanceof Map) payload.set(key, value);
    else (payload as Record<string, unknown>)[key] = value;
  };
  setField("timestamp", Date.now());
  const raw = decode(NITRO_ATTESTATION_DOCUMENT_2024);
  const cose = cbor.decode(raw) as unknown[];
  cose[2] = cbor.encode(payload);
  const forged = encode(cbor.encode(cose));

  setSystemTime(new Date(signedAt));
  await expect(
    authenticate(forged, awsRootCertDer, NITRO_ATTESTATION_DOCUMENT_2024_NONCE)
  ).rejects.toThrow(/Signature verification failed/);
});
