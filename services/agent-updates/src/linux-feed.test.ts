import { expect, test } from "bun:test";
import { encodeFixture, fixture, PAYLOAD } from "./fixtures";
import { encodeLinuxFixture, linuxFixture } from "./linux-fixtures";
import { parseEnvelope, parseManifest } from "./manifest";
import worker, { type Bucket } from "./worker";

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
      httpEtag: '"linux-fixture"',
      body: new Response(
        range ? bytes.slice(range.offset, range.offset + range.length) : bytes,
      ).body!,
    };
  }
  async head(key: string) {
    this.calls.push(`head:${key}`);
    const bytes = this.objects.get(key);
    return bytes ? { size: bytes.length, httpEtag: '"linux-fixture"' } : null;
  }
}

function setup() {
  const bucket = new MemoryBucket();
  const mac = fixture(true);
  const linux = linuxFixture();
  bucket.objects.set("stable/latest.json", mac.bytes);
  bucket.objects.set("stable/linux/latest.json", linux.bytes);
  bucket.objects.set("stable/releases/0.0.2/manifest.json", mac.bytes);
  bucket.objects.set("stable/releases/0.0.2/linux-manifest.json", linux.bytes);
  for (const manifest of [mac.manifest, linux.manifest])
    for (const artifact of Object.values(manifest.artifacts))
      bucket.objects.set(artifact.path.replace("/agent/", ""), PAYLOAD);
  const fetch = (path: string, init?: RequestInit) =>
    worker.fetch(new Request(`https://updates.example${path}`, init), {
      AGENT_RELEASES: bucket,
    });
  return { bucket, fetch, mac, linux };
}

test("macOS and Linux latest feeds return their own exact envelope and uncached HEAD", async () => {
  const { fetch, mac, linux, bucket } = setup();
  for (const [path, key, bytes] of [
    ["/agent/stable/latest.json", "stable/latest.json", mac.bytes],
    [
      "/agent/stable/linux/latest.json",
      "stable/linux/latest.json",
      linux.bytes,
    ],
  ] as const) {
    for (const method of ["GET", "HEAD"]) {
      bucket.calls = [];
      const result = await fetch(path, { method });
      expect(result.status).toBe(200);
      expect(result.headers.get("cache-control")).toBe("no-store");
      expect(result.headers.get("cdn-cache-control")).toBe("no-store");
      expect(result.headers.get("content-length")).toBe(String(bytes.length));
      expect(new Uint8Array(await result.arrayBuffer())).toEqual(
        method === "HEAD" ? new Uint8Array() : bytes,
      );
      expect(bucket.calls).toEqual([`get:${key}`]);
    }
  }
});

test("each download alias uses only its platform feed and remains uncached", async () => {
  const { fetch, bucket, mac, linux } = setup();
  for (const [alias, key, artifact] of [
    ["macos", "stable/latest.json", mac.manifest.downloads.macos],
    [
      "linux-appimage",
      "stable/latest.json",
      mac.manifest.downloads["linux-appimage"]!,
    ],
    [
      "linux",
      "stable/linux/latest.json",
      linux.manifest.downloads["linux-bundle"],
    ],
  ] as const) {
    for (const method of ["GET", "HEAD"]) {
      bucket.calls = [];
      const result = await fetch(`/download/agent/stable/${alias}`, { method });
      expect(result.status).toBe(302);
      expect(result.headers.get("location")).toBe(artifact.path);
      expect(result.headers.get("cache-control")).toBe("no-store");
      expect(result.headers.get("cdn-cache-control")).toBe("no-store");
      expect(bucket.calls).toEqual([`get:${key}`]);
    }
  }
  bucket.objects.delete("stable/linux/latest.json");
  expect((await fetch("/download/agent/stable/linux")).status).toBe(404);
  expect((await fetch("/download/agent/stable/linux-appimage")).status).toBe(
    302,
  );
  bucket.objects.delete("stable/latest.json");
  expect((await fetch("/download/agent/stable/linux-appimage")).status).toBe(
    404,
  );
});

test("latest and immutable metadata cannot cross formats or fall back to another feed", async () => {
  for (const [key, paths, wrong] of [
    [
      "stable/latest.json",
      [
        "/agent/stable/latest.json",
        "/download/agent/stable/macos",
        "/download/agent/stable/linux-appimage",
      ],
      linuxFixture().bytes,
    ],
    [
      "stable/linux/latest.json",
      ["/agent/stable/linux/latest.json", "/download/agent/stable/linux"],
      fixture().bytes,
    ],
    [
      "stable/releases/0.0.2/manifest.json",
      ["/agent/stable/releases/0.0.2/manifest.json"],
      linuxFixture().bytes,
    ],
    [
      "stable/releases/0.0.2/linux-manifest.json",
      ["/agent/stable/releases/0.0.2/linux-manifest.json"],
      fixture().bytes,
    ],
  ] as const) {
    const { fetch, bucket } = setup();
    bucket.objects.set(key, wrong);
    for (const path of paths) {
      bucket.calls = [];
      const result = await fetch(path);
      expect(result.status).toBe(503);
      expect(await result.text()).toBe("503\n");
      expect(result.headers.get("cdn-cache-control")).toBe("no-store");
      expect(bucket.calls).toEqual([`get:${key}`]);
    }
    bucket.objects.delete(key);
    for (const path of paths) expect((await fetch(path)).status).toBe(404);
  }
});

test("Linux immutable metadata is byte-exact and bound to the requested version", async () => {
  const { fetch, bucket, linux } = setup();
  for (const method of ["GET", "HEAD"]) {
    const result = await fetch(
      "/agent/stable/releases/0.0.2/linux-manifest.json",
      { method },
    );
    expect(result.status).toBe(200);
    expect(result.headers.get("cache-control")).toContain("immutable");
    expect(new Uint8Array(await result.arrayBuffer())).toEqual(
      method === "HEAD" ? new Uint8Array() : linux.bytes,
    );
  }
  bucket.objects.set("stable/releases/0.0.3/linux-manifest.json", linux.bytes);
  expect(
    (await fetch("/agent/stable/releases/0.0.3/linux-manifest.json")).status,
  ).toBe(503);
});

test("corrupt Linux discovery fails closed without consulting the macOS feed", async () => {
  const { fetch, bucket } = setup();
  bucket.objects.set("stable/linux/latest.json", new Uint8Array([0xff]));
  for (const path of [
    "/agent/stable/linux/latest.json",
    "/download/agent/stable/linux",
  ]) {
    bucket.calls = [];
    const result = await fetch(path);
    expect(result.status).toBe(503);
    expect(await result.text()).toBe("503\n");
    expect(result.headers.get("cache-control")).toBe("no-store");
    expect(bucket.calls).toEqual(["get:stable/linux/latest.json"]);
  }
  expect((await fetch("/agent/stable/latest.json")).status).toBe(200);
});

test("Linux artifact streams through its version-specific signed enumeration", async () => {
  const { fetch, bucket, linux } = setup();
  const path = linux.manifest.artifacts["linux-x86_64"].path;
  const full = await fetch(path);
  expect(full.status).toBe(200);
  expect(full.headers.get("content-type")).toBe("application/gzip");
  expect(full.headers.get("cache-control")).toContain("immutable");
  expect(new Uint8Array(await full.arrayBuffer())).toEqual(PAYLOAD);
  expect(bucket.calls).toEqual([
    "get:stable/releases/0.0.2/manifest.json",
    "get:stable/releases/0.0.2/linux-manifest.json",
    `head:${path.replace("/agent/", "")}`,
    `get:${path.replace("/agent/", "")}`,
  ]);
  const range = await fetch(path, { headers: { range: "bytes=2-6" } });
  expect(range.status).toBe(206);
  expect(range.headers.get("content-range")).toBe(
    `bytes 2-6/${PAYLOAD.length}`,
  );
  expect(new Uint8Array(await range.arrayBuffer())).toEqual(
    PAYLOAD.slice(2, 7),
  );
  expect(
    (await fetch(path, { headers: { range: "bytes=9999-" } })).status,
  ).toBe(416);
  expect(
    (await fetch(path, { headers: { "if-none-match": '"linux-fixture"' } }))
      .status,
  ).toBe(304);
  expect(
    (
      await fetch(path, {
        headers: { range: "bytes=2-6", "if-range": '"changed"' },
      })
    ).status,
  ).toBe(200);
  bucket.calls = [];
  const head = await fetch(path, {
    method: "HEAD",
    headers: { range: "bytes=2-6" },
  });
  expect(head.status).toBe(200);
  expect(head.headers.get("content-length")).toBe(String(PAYLOAD.length));
  expect(head.headers.get("content-range")).toBeNull();
  expect(await head.text()).toBe("");
  expect(bucket.calls).not.toContain(`get:${path.replace("/agent/", "")}`);
  bucket.objects.set(path.replace("/agent/", ""), new Uint8Array([1]));
  expect((await fetch(path)).status).toBe(503);
});

test("v1 artifact hits do not consult Linux; missing v1 permits Linux-only releases", async () => {
  const { fetch, bucket, mac, linux } = setup();
  bucket.objects.set(
    "stable/releases/0.0.2/linux-manifest.json",
    new Uint8Array([0xff]),
  );
  const macPath = mac.manifest.artifacts["darwin-aarch64"].path;
  expect((await fetch(macPath)).status).toBe(200);
  expect(bucket.calls).not.toContain(
    "get:stable/releases/0.0.2/linux-manifest.json",
  );
  bucket.objects.set("stable/releases/0.0.2/linux-manifest.json", linux.bytes);
  bucket.objects.delete("stable/releases/0.0.2/manifest.json");
  expect(
    (await fetch(linux.manifest.artifacts["linux-x86_64"].path)).status,
  ).toBe(200);
  expect((await fetch(macPath)).status).toBe(404);
});

test("Linux discovery cannot authorize a release without its complete immutable manifest", async () => {
  const { fetch, bucket, linux } = setup();
  const path = linux.manifest.artifacts["linux-x86_64"].path;
  bucket.objects.delete("stable/releases/0.0.2/linux-manifest.json");
  expect((await fetch("/agent/stable/linux/latest.json")).status).toBe(200);
  bucket.calls = [];
  expect((await fetch(path)).status).toBe(404);
  expect(bucket.calls).toEqual([
    "get:stable/releases/0.0.2/manifest.json",
    "get:stable/releases/0.0.2/linux-manifest.json",
    "get:stable/releases/0.0.2/windows-manifest.json",
    "get:stable/releases/0.0.2/windows-arm64-manifest.json",
  ]);
});

test("corrupt consulted release metadata never falls through to a healthy sibling", async () => {
  for (const bad of [new Uint8Array([0xff]), linuxFixture().bytes]) {
    const { fetch, bucket, linux } = setup();
    bucket.objects.set("stable/releases/0.0.2/manifest.json", bad);
    bucket.calls = [];
    expect(
      (await fetch(linux.manifest.artifacts["linux-x86_64"].path)).status,
    ).toBe(503);
    expect(bucket.calls).toEqual(["get:stable/releases/0.0.2/manifest.json"]);
  }
  const { fetch, bucket, linux } = setup();
  bucket.objects.set(
    "stable/releases/0.0.2/linux-manifest.json",
    new Uint8Array([0xff]),
  );
  expect(
    (await fetch(linux.manifest.artifacts["linux-x86_64"].path)).status,
  ).toBe(503);
  bucket.objects.set("stable/releases/0.0.3/manifest.json", fixture().bytes);
  bucket.objects.set("stable/releases/0.0.3/linux-manifest.json", linux.bytes);
  bucket.calls = [];
  expect(
    (
      await fetch(
        "/agent/stable/releases/0.0.3/Maple-Agent-linux-x86_64.tar.gz",
      )
    ).status,
  ).toBe(503);
  expect(bucket.calls).toEqual(["get:stable/releases/0.0.3/manifest.json"]);
});

test("Linux paths preserve private-object and strict route boundaries", async () => {
  const { fetch, bucket } = setup();
  for (const file of [
    "publication-manifest.json",
    "publication-metadata.json",
    "private.tar.gz",
    "metadata/linux-build-info.json",
  ]) {
    bucket.objects.set(`stable/releases/0.0.2/${file}`, PAYLOAD);
    bucket.calls = [];
    expect((await fetch(`/agent/stable/releases/0.0.2/${file}`)).status).toBe(
      404,
    );
    expect(bucket.calls.some((call) => call.endsWith(`/${file}`))).toBe(false);
  }
  for (const path of [
    "/agent/stable/linux/latest.json?x=y",
    "/agent/stable/linux//latest.json",
    "/agent/stable/linux%2flatest.json",
    "/download/agent/stable/linux-bundle",
  ]) {
    bucket.calls = [];
    expect((await fetch(path)).status).toBe(404);
    expect(bucket.calls).toEqual([]);
  }
  const post = await fetch("/agent/stable/linux/latest.json", {
    method: "POST",
  });
  expect(post.status).toBe(405);
  expect(post.headers.get("allow")).toBe("GET, HEAD");
});

test("v2 permits only the exact Linux tar bundle target and matching download", () => {
  expect(parseEnvelope(linuxFixture().bytes).manifest.format).toBe(
    "maple-agent-update-v2",
  );
  const mutations = [
    (v: any) => {
      v.format = "maple-agent-update-v3";
    },
    (v: any) => {
      v.extra = true;
    },
    (v: any) => {
      v.artifacts["darwin-aarch64"] = v.artifacts["linux-x86_64"];
    },
    (v: any) => {
      v.downloads.macos = v.downloads["linux-bundle"];
    },
    (v: any) => {
      v.downloads["linux-appimage"] = v.downloads["linux-bundle"];
    },
    (v: any) => {
      delete v.artifacts["linux-x86_64"];
    },
    (v: any) => {
      delete v.downloads["linux-bundle"];
    },
    (v: any) => {
      v.downloads["linux-bundle"].path += ".AppImage";
    },
    (v: any) => {
      v.artifacts["linux-x86_64"].path = v.downloads["linux-bundle"].path =
        "/agent/stable/releases/0.0.2/Agent.AppImage";
    },
    (v: any) => {
      v.downloads["linux-bundle"].path =
        "/agent/stable/releases/0.0.2/other.tar.gz";
    },
    (v: any) => {
      v.downloads["linux-bundle"].size += 1;
    },
    (v: any) => {
      v.downloads["linux-bundle"].sha256 = "d".repeat(64);
    },
    (v: any) => {
      v.artifacts["linux-x86_64"].extra = true;
    },
    (v: any) => {
      v.product = "research";
    },
    (v: any) => {
      v.artifacts["linux-x86_64"].path =
        "/agent/stable/releases/0.0.1/Agent.tar.gz";
    },
  ];
  for (const mutate of mutations) {
    const { manifest } = linuxFixture();
    mutate(manifest);
    expect(() => parseEnvelope(encodeLinuxFixture(manifest))).toThrow();
  }
});

test("historical 0.0.3 v1 bytes and optional AppImage remain accepted without reinterpretation", () => {
  const { manifest } = fixture(true);
  manifest.version = "0.0.3";
  for (const artifact of [
    ...Object.values(manifest.artifacts),
    ...Object.values(manifest.downloads),
  ])
    artifact.path = artifact.path.replace("/0.0.2/", "/0.0.3/");
  const bytes = encodeFixture(manifest);
  expect(parseEnvelope(bytes).manifest).toEqual(manifest);
  expect(parseManifest(manifest).format).toBe("maple-agent-update-v1");
  const linux = linuxFixture().manifest;
  (linux as any).format = "maple-agent-update-v1";
  expect(() => parseManifest(linux)).toThrow();
});
