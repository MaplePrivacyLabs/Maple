// Runs the shared managed-key corpus (test/fixtures/managed-key) through the
// verifier configuration release approvals use. The corpus is a byte-for-byte
// copy of the shared test vectors; see its README and PROFILE.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

import { verifyArtifactBundle, verifyDsseBundle } from "../releaseEvidence/verification";

const FIXTURES = resolve(import.meta.dir, "../../../test/fixtures/managed-key");

interface CorpusVector {
  id: string;
  bundle: string;
  key: string;
  artifact: string;
  trusted_root: string;
  parent: string | null;
  expected: {
    profile: { result: "accept" | "reject"; code?: string };
  };
}

const manifest = JSON.parse(readFileSync(join(FIXTURES, "manifest.json"), "utf8")) as {
  vectors: CorpusVector[];
};

// Artifact-signing vectors carry a messageSignature; the rest carry a DSSE
// envelope. Read structurally, since some vectors name a member twice on purpose.
function signsArtifact(bundleText: string): boolean {
  return /"messageSignature"\s*:/.test(bundleText);
}

async function runVector(vector: CorpusVector) {
  const bundleText = readFileSync(join(FIXTURES, vector.bundle), "utf8");
  const key = readFileSync(join(FIXTURES, vector.key), "utf8");
  const root = JSON.parse(readFileSync(join(FIXTURES, vector.trusted_root), "utf8"));
  if (signsArtifact(bundleText)) {
    const artifact = new Uint8Array(readFileSync(join(FIXTURES, vector.artifact)));
    return verifyArtifactBundle(bundleText, key, root, artifact);
  }
  return verifyDsseBundle(bundleText, key, root);
}

function vectorNamed(id: string): CorpusVector {
  const vector = manifest.vectors.find((candidate) => {
    const isNamed = candidate.id === id;
    return isNamed;
  });
  expect(vector).toBeDefined();
  return vector as CorpusVector;
}

describe("managed-key corpus", () => {
  test("covers every vector in the manifest", () => {
    expect(manifest.vectors.length).toBe(56);
  });

  for (const vector of manifest.vectors) {
    test(vector.id, async () => {
      const result = await runVector(vector);
      const expected = vector.expected.profile;
      const expectAccept = expected.result === "accept";
      if (expectAccept) {
        expect(result.ok).toBe(true);
        return;
      }
      expect(result).toEqual({ ok: false, category: expected.code });
    });
  }

  // A rejected vector is one change away from its parent, so the parent must pass.
  for (const vector of manifest.vectors) {
    const parentId = vector.parent;
    const hasParent = parentId !== null;
    if (!hasParent) {
      continue;
    }
    test(`${vector.id}: parent ${parentId} is accepted`, async () => {
      const result = await runVector(vectorNamed(parentId));
      expect(result.ok).toBe(true);
    });
  }
});
