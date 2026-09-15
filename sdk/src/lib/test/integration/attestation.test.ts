import { expect, test } from "bun:test";
import {
  createSigStructure,
  isLocalDevelopmentApiUrl,
  parseDocumentData,
  parseDocumentPayload
} from "../../attestation";
import { encode } from "@stablelib/base64";
import {
  NITRO_ATTESTATION_DOCUMENT_2024 as HARDCODED_TEST_ATTESTATION_DOCUMENT,
  NITRO_ATTESTATION_DOCUMENT_2024_MODULE_ID as EXPECTED_MODULE_ID
} from "../fixtures/nitroAttestationDocument2024";

const EXPECTED_SIGNATURE_STRUCTURE_DIGEST =
  "4OIYuQwzjYJFBjHw0eI4cTKT3mUCMNo0yqgPmPGOFCnoFGes3/qjUhXHbxe/HREv";

test("Decode document data", async () => {
  const parsedDocument = await parseDocumentData(HARDCODED_TEST_ATTESTATION_DOCUMENT);
  const parsedPayload = await parseDocumentPayload(parsedDocument.payload);

  expect(parsedPayload.module_id).toBe(EXPECTED_MODULE_ID);
});

test("Makes CoseSign1 bytes correctly", async () => {
  const parsedDocument = await parseDocumentData(HARDCODED_TEST_ATTESTATION_DOCUMENT);

  const coseSign1 = await createSigStructure(parsedDocument.protected, parsedDocument.payload);

  // crypto.subtle isn't available in node so we have to use bun to test this file
  const hash = await crypto.subtle.digest("SHA-384", coseSign1);

  expect(encode(new Uint8Array(hash))).toBe(EXPECTED_SIGNATURE_STRUCTURE_DIGEST);
});

test("Recognizes local development API URLs independent of port", () => {
  const localApiUrls = [
    "http://127.0.0.1:31110",
    "http://localhost:31110/",
    "http://0.0.0.0:31110",
    "http://[::1]:31110"
  ];

  for (const apiUrl of localApiUrls) {
    expect(isLocalDevelopmentApiUrl(apiUrl)).toBe(true);
  }
});

test("Does not recognize production or invalid API URLs as local development URLs", () => {
  const nonLocalApiUrls = [
    "https://api.opensecret.cloud",
    "https://localhost:31110",
    "http://api.opensecret.cloud",
    "localhost:31110",
    "not a url"
  ];

  for (const apiUrl of nonLocalApiUrls) {
    expect(isLocalDevelopmentApiUrl(apiUrl)).toBe(false);
  }
});
