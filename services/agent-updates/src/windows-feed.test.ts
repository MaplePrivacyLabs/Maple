import { expect, test } from "bun:test";
import { fixture, PAYLOAD } from "./fixtures";
import { linuxFixture } from "./linux-fixtures";
import { parseEnvelope, parseManifest } from "./manifest";
import { encodeWindowsFixture, windowsFixture } from "./windows-fixtures";
import worker, { type Bucket } from "./worker";

test("Windows v3 parser agrees with the shared Rust manifest and signed envelope fixtures", async () => {
  const fixtures = new URL(
    "../../../apps/maple-agent/crates/maple-agent-updater/tests/fixtures/",
    import.meta.url,
  );
  const [manifestSource, envelopeSource] = await Promise.all([
    Bun.file(new URL("windows-v3-manifest.json", fixtures)).text(),
    Bun.file(new URL("signed-windows-v3-envelope.json", fixtures)).text(),
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
      httpEtag: '"windows-fixture"',
      body: new Response(
        range ? bytes.slice(range.offset, range.offset + range.length) : bytes,
      ).body!,
    };
  }
  async head(key: string) {
    this.calls.push(`head:${key}`);
    const bytes = this.objects.get(key);
    return bytes ? { size: bytes.length, httpEtag: '"windows-fixture"' } : null;
  }
}

function setup() {
  const bucket = new MemoryBucket();
  const mac = fixture(true);
  const linux = linuxFixture();
  const windows = windowsFixture();
  for (const [key, bytes] of [
    ["stable/latest.json", mac.bytes],
    ["stable/linux/latest.json", linux.bytes],
    ["stable/windows/latest.json", windows.bytes],
    ["stable/releases/0.0.2/manifest.json", mac.bytes],
    ["stable/releases/0.0.2/linux-manifest.json", linux.bytes],
    ["stable/releases/0.0.2/windows-manifest.json", windows.bytes],
  ] as const)
    bucket.objects.set(key, bytes);
  for (const manifest of [mac.manifest, linux.manifest, windows.manifest])
    for (const artifact of [
      ...Object.values(manifest.artifacts),
      ...Object.values(manifest.downloads),
    ])
      bucket.objects.set(artifact.path.replace("/agent/", ""), PAYLOAD);
  const fetch = (path: string, init?: RequestInit) =>
    worker.fetch(new Request(`https://updates.example${path}`, init), {
      AGENT_RELEASES: bucket,
    });
  return { bucket, fetch, mac, linux, windows };
}

test("Windows discovery and installer aliases read only their exact uncached v3 pointer", async () => {
  const { fetch, bucket, windows } = setup();
  for (const method of ["GET", "HEAD"]) {
    bucket.calls = [];
    const result = await fetch("/agent/stable/windows/latest.json", { method });
    expect(result.status).toBe(200);
    expect(result.headers.get("cache-control")).toBe("no-store");
    expect(result.headers.get("cdn-cache-control")).toBe("no-store");
    expect(result.headers.get("content-length")).toBe(
      String(windows.bytes.length),
    );
    expect(new Uint8Array(await result.arrayBuffer())).toEqual(
      method === "HEAD" ? new Uint8Array() : windows.bytes,
    );
    expect(bucket.calls).toEqual(["get:stable/windows/latest.json"]);
    bucket.calls = [];
    const redirect = await fetch("/download/agent/stable/windows", { method });
    expect(redirect.status).toBe(302);
    expect(redirect.headers.get("location")).toBe(
      windows.manifest.downloads.windows.path,
    );
    expect(redirect.headers.get("cache-control")).toBe("no-store");
    expect(redirect.headers.get("cdn-cache-control")).toBe("no-store");
    expect(bucket.calls).toEqual(["get:stable/windows/latest.json"]);
  }
  bucket.objects.delete("stable/windows/latest.json");
  expect((await fetch("/agent/stable/windows/latest.json")).status).toBe(404);
  expect((await fetch("/download/agent/stable/windows")).status).toBe(404);
  expect((await fetch("/agent/stable/latest.json")).status).toBe(200);
  expect((await fetch("/agent/stable/linux/latest.json")).status).toBe(200);
});

test("Windows immutable metadata is byte exact and bound to the v3 requested version", async () => {
  const { fetch, bucket, windows } = setup();
  for (const method of ["GET", "HEAD"]) {
    bucket.calls = [];
    const result = await fetch(
      "/agent/stable/releases/0.0.2/windows-manifest.json",
      { method },
    );
    expect(result.status).toBe(200);
    expect(result.headers.get("cache-control")).toContain("immutable");
    expect(new Uint8Array(await result.arrayBuffer())).toEqual(
      method === "HEAD" ? new Uint8Array() : windows.bytes,
    );
    expect(bucket.calls).toEqual([
      "get:stable/releases/0.0.2/windows-manifest.json",
    ]);
  }
  bucket.objects.set(
    "stable/releases/0.0.3/windows-manifest.json",
    windows.bytes,
  );
  expect(
    (await fetch("/agent/stable/releases/0.0.3/windows-manifest.json")).status,
  ).toBe(503);
});

test("Windows discovery and immutable metadata refuse cross-format substitutions and corruption", async () => {
  for (const key of [
    "stable/windows/latest.json",
    "stable/releases/0.0.2/windows-manifest.json",
  ]) {
    for (const bad of [
      fixture().bytes,
      linuxFixture().bytes,
      new Uint8Array([0xff]),
    ]) {
      const { fetch, bucket } = setup();
      bucket.objects.set(key, bad);
      const paths = key.endsWith("latest.json")
        ? [
            "/agent/stable/windows/latest.json",
            "/download/agent/stable/windows",
          ]
        : ["/agent/stable/releases/0.0.2/windows-manifest.json"];
      for (const path of paths) {
        bucket.calls = [];
        const result = await fetch(path);
        expect(result.status).toBe(503);
        expect(await result.text()).toBe("503\n");
        expect(bucket.calls).toEqual([`get:${key}`]);
      }
    }
  }
  for (const key of [
    "stable/latest.json",
    "stable/linux/latest.json",
    "stable/releases/0.0.2/manifest.json",
    "stable/releases/0.0.2/linux-manifest.json",
  ]) {
    const { fetch, bucket, windows } = setup();
    bucket.objects.set(key, windows.bytes);
    expect((await fetch(`/agent/${key}`)).status).toBe(503);
  }
});

test("Windows bundle and installer stream only from their version's immutable signed enumeration", async () => {
  const { fetch, bucket, windows } = setup();
  for (const artifact of [
    windows.manifest.artifacts["windows-x86_64"],
    windows.manifest.downloads.windows,
  ]) {
    bucket.calls = [];
    const result = await fetch(artifact.path);
    expect(result.status).toBe(200);
    expect(new Uint8Array(await result.arrayBuffer())).toEqual(PAYLOAD);
    expect(result.headers.get("cache-control")).toContain("immutable");
    expect(bucket.calls).toEqual([
      "get:stable/releases/0.0.2/manifest.json",
      "get:stable/releases/0.0.2/linux-manifest.json",
      "get:stable/releases/0.0.2/windows-manifest.json",
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
          headers: { "if-none-match": '"windows-fixture"' },
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
  const path = windows.manifest.artifacts["windows-x86_64"].path;
  bucket.objects.delete("stable/releases/0.0.2/windows-manifest.json");
  expect((await fetch("/agent/stable/windows/latest.json")).status).toBe(200);
  expect((await fetch(path)).status).toBe(404);
});

test("earlier platform hits do not consult Windows, while missing earlier manifests permit Windows-only releases", async () => {
  const { fetch, bucket, mac, linux, windows } = setup();
  bucket.objects.set(
    "stable/releases/0.0.2/windows-manifest.json",
    new Uint8Array([0xff]),
  );
  for (const path of [
    mac.manifest.artifacts["darwin-aarch64"].path,
    linux.manifest.artifacts["linux-x86_64"].path,
  ]) {
    bucket.calls = [];
    expect((await fetch(path)).status).toBe(200);
    expect(bucket.calls).not.toContain(
      "get:stable/releases/0.0.2/windows-manifest.json",
    );
  }
  bucket.objects.set(
    "stable/releases/0.0.2/windows-manifest.json",
    windows.bytes,
  );
  bucket.objects.delete("stable/releases/0.0.2/manifest.json");
  bucket.objects.delete("stable/releases/0.0.2/linux-manifest.json");
  expect(
    (await fetch(windows.manifest.artifacts["windows-x86_64"].path)).status,
  ).toBe(200);
  for (const key of [
    "stable/releases/0.0.2/manifest.json",
    "stable/releases/0.0.2/linux-manifest.json",
  ]) {
    bucket.objects.set(key, new Uint8Array([0xff]));
    bucket.calls = [];
    expect((await fetch(windows.manifest.downloads.windows.path)).status).toBe(
      503,
    );
    expect(bucket.calls).not.toContain(
      "get:stable/releases/0.0.2/windows-manifest.json",
    );
    bucket.objects.delete(key);
  }
});

test("Windows routing leaves auxiliary objects private and rejects ambiguous paths and unsupported methods", async () => {
  const { fetch, bucket } = setup();
  for (const file of [
    "windows-build-info.json",
    "windows-SHA256SUMS",
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
    "/agent/stable/windows/latest.json?x=y",
    "/agent/stable/windows//latest.json",
    "/agent/stable/windows%2flatest.json",
    "/download/agent/stable/windows-x86_64",
  ]) {
    bucket.calls = [];
    expect((await fetch(path)).status).toBe(404);
    expect(bucket.calls).toEqual([]);
  }
  const result = await fetch("/agent/stable/windows/latest.json", {
    method: "POST",
  });
  expect(result.status).toBe(405);
  expect(result.headers.get("allow")).toBe("GET, HEAD");
});

test("v3 permits only Windows bundle and installer targets without changing v1 or v2 acceptance", () => {
  expect(parseEnvelope(windowsFixture().bytes).manifest.format).toBe(
    "maple-agent-update-v3",
  );
  for (const mutate of [
    (v: any) => {
      v.extra = true;
    },
    (v: any) => {
      delete v.artifacts["windows-x86_64"];
    },
    (v: any) => {
      delete v.downloads.windows;
    },
    (v: any) => {
      v.artifacts["linux-x86_64"] = v.artifacts["windows-x86_64"];
    },
    (v: any) => {
      v.downloads["linux-bundle"] = v.artifacts["windows-x86_64"];
    },
    (v: any) => {
      v.downloads.macos = null;
    },
    (v: any) => {
      v.artifacts["windows-x86_64"].path += ".exe";
    },
    (v: any) => {
      v.downloads.windows.path += ".tar.gz";
    },
    (v: any) => {
      v.downloads.windows.path = "/agent/stable/releases/0.0.1/Maple-Agent.exe";
    },
    (v: any) => {
      v.artifacts["windows-x86_64"].size = 0;
    },
    (v: any) => {
      v.downloads.windows.sha256 = "no";
    },
    (v: any) => {
      v.artifacts["windows-x86_64"].extra = true;
    },
    (v: any) => {
      v.product = "research";
    },
    (v: any) => {
      v.format = "maple-agent-update-v2";
    },
  ]) {
    const { manifest } = windowsFixture();
    mutate(manifest);
    expect(() => parseEnvelope(encodeWindowsFixture(manifest))).toThrow();
  }
  for (const manifest of [fixture(true).manifest, linuxFixture().manifest]) {
    expect(parseManifest(manifest)).toEqual(manifest);
    (manifest as any).downloads.windows = null;
    expect(() => parseManifest(manifest)).toThrow();
  }
});
