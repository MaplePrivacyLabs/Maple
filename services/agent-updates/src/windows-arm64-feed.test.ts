import { expect, test } from "bun:test";
import { fixture, PAYLOAD } from "./fixtures";
import { linuxFixture } from "./linux-fixtures";
import { parseEnvelope, parseManifest } from "./manifest";
import { windowsFixture } from "./windows-fixtures";
import {
  encodeWindowsArm64Fixture,
  windowsArm64Fixture,
} from "./windows-arm64-fixtures";
import worker, { type Bucket } from "./worker";

test("ARM64 v4 parser agrees with the shared Rust manifest and exact signed envelope fixture", async () => {
  const fixtures = new URL(
    "../../../apps/maple-agent/crates/maple-agent-updater/tests/fixtures/",
    import.meta.url,
  );
  const [manifestSource, envelopeSource] = await Promise.all([
    Bun.file(new URL("windows-arm64-v4-manifest.json", fixtures)).text(),
    Bun.file(new URL("signed-windows-arm64-v4-envelope.json", fixtures)).text(),
  ]);
  const expected = JSON.parse(manifestSource);
  const parsed = parseEnvelope(new TextEncoder().encode(envelopeSource));
  expect(parseManifest(expected)).toEqual(expected);
  expect(parsed.manifest).toEqual(expected);
  expect(parsed.envelope).toEqual(JSON.parse(envelopeSource));
  expect(parsed.envelope.manifest).toBe(manifestSource);
});

class MemoryBucket implements Bucket {
  objects = new Map<string, Uint8Array<ArrayBuffer>>();
  calls: string[] = [];
  async get(
    key: string,
    options?: { range: { offset: number; length: number } },
  ) {
    this.calls.push(`get:${key}`);
    const bytes = this.objects.get(key);
    if (!bytes) return null;
    const range = options?.range;
    return {
      size: bytes.length,
      httpEtag: '"arm64-fixture"',
      body: new Response(
        range ? bytes.slice(range.offset, range.offset + range.length) : bytes,
      ).body!,
    };
  }
  async head(key: string) {
    this.calls.push(`head:${key}`);
    const bytes = this.objects.get(key);
    return bytes ? { size: bytes.length, httpEtag: '"arm64-fixture"' } : null;
  }
}

function setup() {
  const bucket = new MemoryBucket();
  const arm64 = windowsArm64Fixture();
  const platforms = [
    ["manifest.json", fixture(true)],
    ["linux-manifest.json", linuxFixture()],
    ["windows-manifest.json", windowsFixture()],
    ["windows-arm64-manifest.json", arm64],
  ] as const;
  for (const [file, release] of platforms) {
    bucket.objects.set(`stable/releases/0.0.2/${file}`, release.bytes);
    for (const artifact of [
      ...Object.values(release.manifest.artifacts),
      ...Object.values(release.manifest.downloads),
    ])
      bucket.objects.set(artifact.path.replace("/agent/", ""), PAYLOAD);
  }
  bucket.objects.set("stable/windows-arm64/latest.json", arm64.bytes);
  bucket.objects.set("stable/windows/latest.json", windowsFixture().bytes);
  const fetch = (path: string, init?: RequestInit) =>
    worker.fetch(new Request(`https://updates.example${path}`, init), {
      AGENT_RELEASES: bucket,
    });
  return { bucket, fetch, arm64, platforms };
}

test("ARM64 v4 discovery and installer alias read only their exact uncached pointer", async () => {
  const { fetch, bucket, arm64 } = setup();
  for (const method of ["GET", "HEAD"]) {
    bucket.calls = [];
    const latest = await fetch("/agent/stable/windows-arm64/latest.json", {
      method,
    });
    expect(latest.status).toBe(200);
    expect(latest.headers.get("cache-control")).toBe("no-store");
    expect(latest.headers.get("cdn-cache-control")).toBe("no-store");
    expect(latest.headers.get("content-length")).toBe(
      String(arm64.bytes.length),
    );
    expect(new Uint8Array(await latest.arrayBuffer())).toEqual(
      method === "HEAD" ? new Uint8Array() : arm64.bytes,
    );
    expect(bucket.calls).toEqual(["get:stable/windows-arm64/latest.json"]);
    bucket.calls = [];
    const redirect = await fetch("/download/agent/stable/windows-arm64", {
      method,
    });
    expect(redirect.status).toBe(302);
    expect(redirect.headers.get("location")).toBe(
      arm64.manifest.downloads["windows-arm64"].path,
    );
    expect(redirect.headers.get("cache-control")).toBe("no-store");
    expect(redirect.headers.get("cdn-cache-control")).toBe("no-store");
    expect(bucket.calls).toEqual(["get:stable/windows-arm64/latest.json"]);
  }
  bucket.objects.delete("stable/windows-arm64/latest.json");
  expect((await fetch("/agent/stable/windows-arm64/latest.json")).status).toBe(
    404,
  );
  expect((await fetch("/download/agent/stable/windows-arm64")).status).toBe(
    404,
  );
  expect((await fetch("/agent/stable/windows/latest.json")).status).toBe(200);
});

test("ARM64 immutable metadata is byte exact and requested-version bound", async () => {
  const { fetch, bucket, arm64 } = setup();
  for (const method of ["GET", "HEAD"]) {
    bucket.calls = [];
    const result = await fetch(
      "/agent/stable/releases/0.0.2/windows-arm64-manifest.json",
      { method },
    );
    expect(result.status).toBe(200);
    expect(result.headers.get("cache-control")).toContain("immutable");
    expect(new Uint8Array(await result.arrayBuffer())).toEqual(
      method === "HEAD" ? new Uint8Array() : arm64.bytes,
    );
    expect(bucket.calls).toEqual([
      "get:stable/releases/0.0.2/windows-arm64-manifest.json",
    ]);
  }
  bucket.objects.set(
    "stable/releases/0.0.3/windows-arm64-manifest.json",
    arm64.bytes,
  );
  expect(
    (await fetch("/agent/stable/releases/0.0.3/windows-arm64-manifest.json"))
      .status,
  ).toBe(503);
});

test("all feed routes reject another architecture or format without falling back", async () => {
  const routes = [
    ["stable/latest.json", fixture()],
    ["stable/linux/latest.json", linuxFixture()],
    ["stable/windows/latest.json", windowsFixture()],
    ["stable/windows-arm64/latest.json", windowsArm64Fixture()],
    ["stable/releases/0.0.2/manifest.json", fixture()],
    ["stable/releases/0.0.2/linux-manifest.json", linuxFixture()],
    ["stable/releases/0.0.2/windows-manifest.json", windowsFixture()],
    [
      "stable/releases/0.0.2/windows-arm64-manifest.json",
      windowsArm64Fixture(),
    ],
  ] as const;
  for (const [key, expected] of routes) {
    for (const alternate of [
      fixture(),
      linuxFixture(),
      windowsFixture(),
      windowsArm64Fixture(),
    ]) {
      if (alternate.manifest.format === expected.manifest.format) continue;
      const { fetch, bucket } = setup();
      bucket.objects.set(key, alternate.bytes);
      bucket.calls = [];
      expect((await fetch(`/agent/${key}`)).status).toBe(503);
      expect(bucket.calls).toEqual([`get:${key}`]);
    }
  }
  for (const key of [
    "stable/windows-arm64/latest.json",
    "stable/releases/0.0.2/windows-arm64-manifest.json",
  ]) {
    const { fetch, bucket } = setup();
    bucket.objects.set(key, new Uint8Array([0xff]));
    expect((await fetch(`/agent/${key}`)).status).toBe(503);
  }
});

test("ARM64 payloads require immutable enumeration and retain HEAD, ranges and validators", async () => {
  const { fetch, bucket, arm64 } = setup();
  for (const artifact of [
    arm64.manifest.artifacts["windows-aarch64"],
    arm64.manifest.downloads["windows-arm64"],
  ]) {
    bucket.calls = [];
    const result = await fetch(artifact.path);
    expect(result.status).toBe(200);
    expect(new Uint8Array(await result.arrayBuffer())).toEqual(PAYLOAD);
    expect(bucket.calls).toEqual([
      "get:stable/releases/0.0.2/manifest.json",
      "get:stable/releases/0.0.2/linux-manifest.json",
      "get:stable/releases/0.0.2/windows-manifest.json",
      "get:stable/releases/0.0.2/windows-arm64-manifest.json",
      `head:${artifact.path.replace("/agent/", "")}`,
      `get:${artifact.path.replace("/agent/", "")}`,
    ]);
    const range = await fetch(artifact.path, {
      headers: { range: "bytes=2-6" },
    });
    expect(range.status).toBe(206);
    expect(range.headers.get("content-range")).toBe(
      `bytes 2-6/${PAYLOAD.length}`,
    );
    expect(new Uint8Array(await range.arrayBuffer())).toEqual(
      PAYLOAD.slice(2, 7),
    );
    expect(
      (await fetch(artifact.path, { headers: { range: "bytes=9999-" } }))
        .status,
    ).toBe(416);
    expect(
      (
        await fetch(artifact.path, {
          headers: { "if-none-match": '"arm64-fixture"' },
        })
      ).status,
    ).toBe(304);
    const head = await fetch(artifact.path, {
      method: "HEAD",
      headers: { range: "bytes=2-6" },
    });
    expect(head.status).toBe(200);
    expect(head.headers.get("content-length")).toBe(String(PAYLOAD.length));
    expect(head.headers.get("content-range")).toBeNull();
    expect(await head.text()).toBe("");
  }
  bucket.objects.delete("stable/releases/0.0.2/windows-arm64-manifest.json");
  expect((await fetch("/agent/stable/windows-arm64/latest.json")).status).toBe(
    200,
  );
  expect(
    (await fetch(arm64.manifest.artifacts["windows-aarch64"].path)).status,
  ).toBe(404);
});

test("earlier platform hits retain behavior and consulted corruption blocks ARM64 fallback", async () => {
  const { fetch, bucket, arm64, platforms } = setup();
  const armKey = "stable/releases/0.0.2/windows-arm64-manifest.json";
  bucket.objects.set(armKey, new Uint8Array([0xff]));
  for (const [, earlier] of platforms.slice(0, 3)) {
    const artifact = Object.values(earlier.manifest.artifacts)[0];
    bucket.calls = [];
    expect((await fetch(artifact.path)).status).toBe(200);
    expect(bucket.calls).not.toContain(`get:${armKey}`);
  }
  bucket.objects.set(armKey, arm64.bytes);
  for (const [file] of platforms.slice(0, 3))
    bucket.objects.delete(`stable/releases/0.0.2/${file}`);
  const path = arm64.manifest.downloads["windows-arm64"].path;
  expect((await fetch(path)).status).toBe(200);
  for (const [file] of platforms.slice(0, 3)) {
    const key = `stable/releases/0.0.2/${file}`;
    bucket.objects.set(key, new Uint8Array([0xff]));
    bucket.calls = [];
    expect((await fetch(path)).status).toBe(503);
    expect(bucket.calls).not.toContain(`get:${armKey}`);
    bucket.objects.delete(key);
  }
});

test("ARM64 routing keeps auxiliary objects private and rejects ambiguous paths and unsupported methods", async () => {
  const { fetch, bucket } = setup();
  for (const file of [
    "windows-arm64-build-info.json",
    "windows-arm64-SHA256SUMS",
    "private.exe",
    "private.tar.gz",
  ]) {
    bucket.objects.set(`stable/releases/0.0.2/${file}`, PAYLOAD);
    bucket.calls = [];
    expect((await fetch(`/agent/stable/releases/0.0.2/${file}`)).status).toBe(
      404,
    );
    expect(bucket.calls.some((call) => call.endsWith(`/${file}`))).toBe(false);
  }
  for (const path of [
    "/agent/stable/windows-arm64/latest.json?x=y",
    "/agent/stable/windows-arm64//latest.json",
    "/agent/stable/windows-arm64%2flatest.json",
    "/download/agent/stable/windows-aarch64",
  ]) {
    bucket.calls = [];
    expect((await fetch(path)).status).toBe(404);
    expect(bucket.calls).toEqual([]);
  }
  const result = await fetch("/agent/stable/windows-arm64/latest.json", {
    method: "POST",
  });
  expect(result.status).toBe(405);
  expect(result.headers.get("allow")).toBe("GET, HEAD");
});

test("v4 accepts only ARM64 bundle and installer while v1-v3 reject ARM fields including null", () => {
  const arm64 = windowsArm64Fixture();
  expect(parseEnvelope(arm64.bytes).manifest).toEqual(arm64.manifest);
  for (const mutate of [
    (v: any) => {
      v.extra = true;
    },
    (v: any) => {
      delete v.artifacts["windows-aarch64"];
    },
    (v: any) => {
      delete v.downloads["windows-arm64"];
    },
    (v: any) => {
      v.artifacts["windows-x86_64"] = v.artifacts["windows-aarch64"];
    },
    (v: any) => {
      v.downloads.windows = null;
    },
    (v: any) => {
      v.downloads["windows-arm64"].path += ".tar.gz";
    },
    (v: any) => {
      v.artifacts["windows-aarch64"].path += ".exe";
    },
    (v: any) => {
      v.downloads["windows-arm64"].path =
        "/agent/stable/releases/0.0.1/Maple-Agent.exe";
    },
    (v: any) => {
      v.artifacts["windows-aarch64"].size = 0;
    },
    (v: any) => {
      v.downloads["windows-arm64"].sha256 = "no";
    },
    (v: any) => {
      v.artifacts["windows-aarch64"].extra = true;
    },
    (v: any) => {
      v.product = "research";
    },
    (v: any) => {
      v.format = "maple-agent-update-v3";
    },
  ]) {
    const { manifest } = windowsArm64Fixture();
    mutate(manifest);
    expect(() => parseEnvelope(encodeWindowsArm64Fixture(manifest))).toThrow();
  }
  for (const existing of [fixture(true), linuxFixture(), windowsFixture()]) {
    expect(parseManifest(existing.manifest)).toEqual(existing.manifest);
    for (const [field, key] of [
      ["artifacts", "windows-aarch64"],
      ["downloads", "windows-arm64"],
    ] as const) {
      const manifest = structuredClone(existing.manifest);
      (manifest as any)[field][key] = null;
      expect(() => parseManifest(manifest)).toThrow();
    }
  }
});
