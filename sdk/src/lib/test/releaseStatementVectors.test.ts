// Runs the shared release-statement vectors (test/fixtures/release-statement).
// The Rust SDK runs a byte-identical copy, so both give the same answer for
// the same statement, including one that is wrong in several ways at once.
import { describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import type { PcrEnvironment } from "../pcr";
import { checkReleaseStatement } from "../releaseEvidence/verification";

const VECTORS_PATH = resolve(
  import.meta.dir,
  "../../../test/fixtures/release-statement/vectors.json"
);

// The Rust SDK's tests pin the same value for its copy.
const VECTORS_SHA256 = "d1af0b289ccc98365f233387c2cb67fbe2eaa590d469d7c94b4b47ba5bd2b22f";

interface StatementVector {
  id: string;
  shows: string;
  payload: string;
  expected: { result: "accept" } | { result: "reject"; category: string };
}

interface VectorFile {
  expected_environment: PcrEnvironment;
  expected_measurements: { PCR0: string; PCR1: string; PCR2: string };
  vectors: StatementVector[];
}

const vectorBytes = readFileSync(VECTORS_PATH);
const vectorFile = JSON.parse(vectorBytes.toString("utf8")) as VectorFile;

const expectedRelease = {
  environment: vectorFile.expected_environment,
  pcr0: vectorFile.expected_measurements.PCR0,
  pcr1: vectorFile.expected_measurements.PCR1,
  pcr2: vectorFile.expected_measurements.PCR2
};

describe("release statement vectors", () => {
  test("are the copy the Rust SDK also runs", () => {
    const digest = createHash("sha256").update(vectorBytes).digest("hex");
    expect(digest).toBe(VECTORS_SHA256);
    expect(vectorFile.vectors.length).toBe(48);
  });

  for (const vector of vectorFile.vectors) {
    test(`${vector.id}: ${vector.shows}`, () => {
      const payload = new Uint8Array(Buffer.from(vector.payload, "base64"));
      const result = checkReleaseStatement(payload, expectedRelease);
      const expected = vector.expected;
      if (expected.result === "accept") {
        expect(result.ok).toBe(true);
        return;
      }
      expect(result).toEqual({ ok: false, category: expected.category });
    });
  }
});
