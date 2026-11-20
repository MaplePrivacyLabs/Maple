// Release-bundle verification against the published Dev release bundle, the
// reviewed Dev approval key and the pinned Sigstore trusted root.
import { describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { resolve } from "node:path";

import { hasDuplicateJsonMember } from "../releaseEvidence/duplicateJsonKey";
import { embeddedTrustedRoot } from "../releaseEvidence/trustedRoot";
import {
  checkPayloadType,
  checkReleaseStatement,
  verifyReleaseBundle,
  type ExpectedMeasurements
} from "../releaseEvidence/verification";

const SDK_ROOT = resolve(import.meta.dir, "../../..");
const REPO_ROOT = resolve(SDK_ROOT, "..");
const RELEASE_EVIDENCE = resolve(SDK_ROOT, "src/lib/releaseEvidence");
const PUBLISHED_BUNDLE_PATH = resolve(RELEASE_EVIDENCE, "published/dev-b422d5fc.sigstore.json");
// The reviewed copies the release pipeline signs and verifies with. The SDK
// lane stages both next to the SDK.
const REVIEWED_DEV_KEY_PATH = resolve(
  REPO_ROOT,
  "scripts/ci/opensecret-release/dev-signing-key.pem"
);
const REVIEWED_TRUSTED_ROOT_PATH = resolve(
  REPO_ROOT,
  "scripts/ci/opensecret-release/trusted_root.json"
);

const PUBLISHED_BUNDLE = readFileSync(PUBLISHED_BUNDLE_PATH, "utf8");
const DEV_KEY_PEM = readFileSync(REVIEWED_DEV_KEY_PATH, "utf8");

// The measurements the published Dev release statement approves.
const DEV_RELEASE: ExpectedMeasurements = {
  environment: "development",
  pcr0: "b422d5fcb25929a1b76f0eb8aa10c26a78c635a86a58039b48f22eb0dc4dc6f8ae6e646aa2fbbc57db756e6f06cad3dc",
  pcr1: "e83dc536d4cf82daf0e0821e7fdbddf25cd76090602cb67bef7c9d5ab2914417ee6a94bd0c9c1e7ed15a3c3c3bfcc0a3",
  pcr2: "57b7e86919f571beff7539cb8643baafe81ec9f2ffb3434d683268978675a664e0d043dead3367aba14a5d2814a4be97"
};
const DEV_LOG_INDEX = "207746380";

type Json = Record<string, any>;

function publishedBundle(): Json {
  return JSON.parse(PUBLISHED_BUNDLE);
}

function devStatement(): Json {
  const payload = publishedBundle().dsseEnvelope.payload as string;
  return JSON.parse(Buffer.from(payload, "base64").toString("utf8"));
}

// A mutated copy of the real signed statement, as payload bytes.
function devStatementWith(mutate: (statement: Json) => void): Uint8Array {
  const statement = devStatement();
  mutate(statement);
  return new TextEncoder().encode(JSON.stringify(statement));
}

function verifyPublished(text: string, expected: ExpectedMeasurements = DEV_RELEASE) {
  return verifyReleaseBundle(text, DEV_KEY_PEM, embeddedTrustedRoot(), expected);
}

describe("published Dev release bundle", () => {
  test("is the pinned published text", () => {
    const pinned = readFileSync(`${PUBLISHED_BUNDLE_PATH}.sha256`, "utf8").split(" ")[0];
    const bytes = readFileSync(PUBLISHED_BUNDLE_PATH);
    expect(bytes.byteLength).toBe(5517);
    expect(createHash("sha256").update(bytes).digest("hex")).toBe(pinned);
  });

  test("passes as published, with its repository, commit and verified log index", async () => {
    const result = await verifyPublished(PUBLISHED_BUNDLE);
    expect(result).toEqual({
      ok: true,
      source: {
        repository: "https://github.com/MaplePrivacyLabs/Maple",
        commit: "bcf6e84476be2cf08b575c5303f5a83d98adbf40"
      },
      logIndex: DEV_LOG_INDEX
    });
  });

  test("reports the inclusion proof's log index, not the entry's unverified one", async () => {
    const bundle = publishedBundle();
    bundle.verificationMaterial.tlogEntries[0].logIndex = "207716380";
    const result = await verifyPublished(JSON.stringify(bundle));
    expect(result).toMatchObject({ ok: true, logIndex: DEV_LOG_INDEX });
  });

  test("fails for a different PCR", async () => {
    const result = await verifyPublished(PUBLISHED_BUNDLE, {
      ...DEV_RELEASE,
      pcr2: "00".repeat(48)
    });
    expect(result).toEqual({ ok: false, category: "statement-measurement-mismatch" });
  });

  test("fails when production is expected", async () => {
    const result = await verifyPublished(PUBLISHED_BUNDLE, {
      ...DEV_RELEASE,
      environment: "production"
    });
    expect(result).toEqual({ ok: false, category: "statement-environment-mismatch" });
  });

  test("fails with a repeated member before the verifier sees it", async () => {
    const repeated = PUBLISHED_BUNDLE.replace(
      '"mediaType":',
      '"mediaType":"application/vnd.dev.sigstore.bundle.v0.3+json","mediaType":'
    );
    expect(repeated).not.toBe(PUBLISHED_BUNDLE);
    const result = await verifyPublished(repeated);
    expect(result).toEqual({ ok: false, category: "duplicate-json-key" });
  });

  test("fails without its DSSE envelope", async () => {
    const bundle = publishedBundle();
    delete bundle.dsseEnvelope;
    const result = await verifyPublished(JSON.stringify(bundle));
    expect(result).toEqual({ ok: false, category: "content-unsupported" });
  });

  test("fails above the byte cap before parsing", async () => {
    const padded = PUBLISHED_BUNDLE + " ".repeat(256 * 1024);
    const result = await verifyPublished(padded);
    expect(result).toEqual({ ok: false, category: "bundle-too-large" });
  });
});

describe("trust anchors", () => {
  test("the embedded trusted root is the reviewed copy, byte for byte", () => {
    const reviewed = readFileSync(REVIEWED_TRUSTED_ROOT_PATH);
    const embedded = readFileSync(resolve(RELEASE_EVIDENCE, "trustedRoot.json"));
    expect(embedded.equals(reviewed)).toBe(true);
  });
});

describe("duplicate member check", () => {
  test("finds a repeated member in a nested object", () => {
    expect(hasDuplicateJsonMember('{"a": {"b": 1, "b": 2}}')).toBe(true);
  });

  test("treats escaped spellings of a name as the same name", () => {
    expect(hasDuplicateJsonMember('{"a": 1, "\\u0061": 2}')).toBe(true);
  });

  test("ignores names and brackets inside string values", () => {
    expect(hasDuplicateJsonMember('{"a": "{\\"a\\": 1}", "b": "]"}')).toBe(false);
  });

  test("allows the same name in different objects", () => {
    expect(hasDuplicateJsonMember('{"a": {"b": 1}, "c": [{"b": 2}, {"b": 3}]}')).toBe(false);
  });

  test("finds no repeated member in the published bundle", () => {
    expect(hasDuplicateJsonMember(PUBLISHED_BUNDLE)).toBe(false);
  });

  test("throws for text that is not JSON", () => {
    expect(() => hasDuplicateJsonMember('{"a":')).toThrow(SyntaxError);
  });
});

describe("DSSE payload type", () => {
  test("accepts the in-toto statement type", () => {
    expect(checkPayloadType("application/vnd.in-toto+json")).toEqual({ ok: true });
  });

  test("reports any other type as statement-unsupported", () => {
    expect(checkPayloadType("application/vnd.dev.other+json")).toEqual({
      ok: false,
      category: "statement-unsupported"
    });
  });
});

describe("statement policy on the real Dev statement", () => {
  const check = (mutate: (statement: Json) => void) =>
    checkReleaseStatement(devStatementWith(mutate), DEV_RELEASE);

  test("accepts the statement as signed", () => {
    expect(check(() => {})).toEqual({
      ok: true,
      value: {
        repository: "https://github.com/MaplePrivacyLabs/Maple",
        commit: "bcf6e84476be2cf08b575c5303f5a83d98adbf40"
      }
    });
  });

  test("still passes when the repository and workflow run differ", () => {
    const result = check((statement) => {
      statement.predicate.source.repository = "https://github.com/example/renamed";
      statement.predicate.build.runId = "1";
    });
    expect(result).toMatchObject({ ok: true });
  });

  const failures: Array<[string, (statement: Json) => void, string]> = [
    ["a missing statement type", (s) => delete s._type, "statement-unsupported"],
    [
      "an older statement type",
      (s) => (s._type = "https://in-toto.io/Statement/v0.1"),
      "statement-unsupported"
    ],
    [
      "a different predicate type",
      (s) => (s.predicateType = "https://example.com/other"),
      "statement-unsupported"
    ],
    ["a missing subject", (s) => delete s.subject, "statement-malformed"],
    ["an empty subject list", (s) => (s.subject = []), "statement-malformed"],
    ["a subject without a digest", (s) => delete s.subject[0].digest, "statement-malformed"],
    [
      "a subject digest that is not SHA-256 hex",
      (s) => (s.subject[0].digest.sha256 = "garbage"),
      "statement-malformed"
    ],
    ["a missing predicate", (s) => delete s.predicate, "statement-malformed"],
    [
      "a production environment",
      (s) => (s.predicate.environment = "production"),
      "statement-environment-mismatch"
    ],
    ["a malformed PCR", (s) => (s.predicate.measurements.PCR0 = "zz"), "statement-malformed"],
    [
      "an uppercase PCR",
      (s) => (s.predicate.measurements.PCR1 = DEV_RELEASE.pcr1.toUpperCase()),
      "statement-malformed"
    ],
    [
      "a different PCR",
      (s) => (s.predicate.measurements.PCR2 = "00".repeat(48)),
      "statement-measurement-mismatch"
    ],
    ["a short commit", (s) => (s.predicate.source.commit = "bcf6e844"), "statement-malformed"],
    ["a missing repository", (s) => delete s.predicate.source.repository, "statement-malformed"]
  ];
  for (const [name, mutate, category] of failures) {
    test(`rejects ${name}`, () => {
      expect(check(mutate)).toEqual({ ok: false, category });
    });
  }

  test("rejects a statement that names a member twice", () => {
    const signed = JSON.stringify(devStatement());
    const repeated = signed.replace(
      '"environment":"development"',
      '"environment":"production","environment":"development"'
    );
    expect(repeated).not.toBe(signed);
    const result = checkReleaseStatement(new TextEncoder().encode(repeated), DEV_RELEASE);
    expect(result).toEqual({ ok: false, category: "duplicate-json-key" });
  });

  test("rejects a payload that is not an object", () => {
    const result = checkReleaseStatement(new TextEncoder().encode("[1,2,3]"), DEV_RELEASE);
    expect(result).toEqual({ ok: false, category: "statement-malformed" });
  });
});

describe("engine without native Ed25519", () => {
  // A child process hides native Ed25519 from WebCrypto, as WebKit before
  // iOS 17 does, and verifies the published bundle through the fallback.
  test("the published bundle still verifies", () => {
    const script = `
      const original = globalThis.crypto.subtle;
      function namesEd25519(prop, args) {
        // importKey takes the algorithm third; the other calls take it first.
        let algorithm = args[0];
        const isImportKey = prop === "importKey";
        if (isImportKey) {
          algorithm = args[2];
        }
        const algorithmIsName = typeof algorithm === "string";
        if (algorithmIsName) {
          return algorithm === "Ed25519";
        }
        return algorithm?.name === "Ed25519";
      }
      const withoutEd25519 = new Proxy(original, {
        get(target, prop) {
          const value = Reflect.get(target, prop);
          const isMethod = typeof value === "function";
          if (!isMethod) {
            return value;
          }
          return (...args) => {
            if (namesEd25519(prop, args)) {
              return Promise.reject(new DOMException("Unrecognized name.", "NotSupportedError"));
            }
            return value.apply(target, args);
          };
        }
      });
      Object.defineProperty(globalThis, "crypto", {
        value: new Proxy(globalThis.crypto, {
          get(target, prop) {
            const isSubtle = prop === "subtle";
            if (isSubtle) {
              return withoutEd25519;
            }
            const value = Reflect.get(target, prop);
            const isMethod = typeof value === "function";
            if (isMethod) {
              return value.bind(target);
            }
            return value;
          }
        }),
        configurable: true
      });
      const { readFileSync } = await import("node:fs");
      const { verifyReleaseBundle } = await import(${JSON.stringify(resolve(RELEASE_EVIDENCE, "verification.ts"))});
      const { embeddedTrustedRoot } = await import(${JSON.stringify(resolve(RELEASE_EVIDENCE, "trustedRoot.ts"))});
      const bundle = readFileSync(${JSON.stringify(PUBLISHED_BUNDLE_PATH)}, "utf8");
      const key = readFileSync(${JSON.stringify(REVIEWED_DEV_KEY_PATH)}, "utf8");
      const result = await verifyReleaseBundle(bundle, key, embeddedTrustedRoot(), ${JSON.stringify(DEV_RELEASE)});
      console.log(JSON.stringify(result));
    `;
    const child = spawnSync(process.execPath, ["-e", script], {
      cwd: SDK_ROOT,
      encoding: "utf8",
      timeout: 60000
    });
    expect(child.stderr).toBe("");
    expect(child.status).toBe(0);
    expect(JSON.parse(child.stdout)).toMatchObject({ ok: true, logIndex: DEV_LOG_INDEX });
  });
});
