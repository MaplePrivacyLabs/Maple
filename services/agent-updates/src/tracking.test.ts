import { afterAll, afterEach, expect, spyOn, test } from "bun:test";
import { Database } from "bun:sqlite";
import {
  consumeEvents,
  isDownloadEvent,
  trackResponse,
  type DownloadEvent,
  type EventDatabase,
  type EventMessage,
  type Product,
} from "../../download-tracking/tracking";

const ORIGIN = "https://updates.maple.ai";
const ARTIFACT =
  "/agent/stable/releases/0.0.9/maple-agent-prod-0.0.9-d784804-macos-aarch64.dmg";
const logs = spyOn(console, "error").mockImplementation(() => {});
afterEach(() => logs.mockClear());
afterAll(() => logs.mockRestore());

function capture(
  path = ARTIFACT,
  response = new Response("payload", { headers: { "content-length": "7" } }),
  init?: RequestInit,
  product: Product = "agent",
) {
  const events: DownloadEvent[] = [];
  const pending: Promise<unknown>[] = [];
  trackResponse(
    new Request(new URL(path, ORIGIN), init),
    response,
    {
      DOWNLOAD_EVENTS: {
        async send(event) {
          events.push(event);
        },
      },
    },
    {
      waitUntil(promise) {
        pending.push(promise);
      },
    },
    product,
  );
  return { events, pending, response, done: () => Promise.all(pending) };
}

async function event(): Promise<DownloadEvent> {
  const observation = capture();
  await observation.done();
  return observation.events[0];
}

function message(
  body: unknown,
): EventMessage & { acks: number; retries: number } {
  return {
    body,
    acks: 0,
    retries: 0,
    ack() {
      this.acks++;
    },
    retry() {
      this.retries++;
    },
  };
}

async function migration(name: string): Promise<string> {
  return Bun.file(new URL(`../migrations/${name}`, import.meta.url)).text();
}

async function database() {
  const sqlite = new Database(":memory:");
  sqlite.exec(await migration("0001_download_events.sql"));
  const upgrade = await migration("0002_windows_download_events.sql");
  sqlite.transaction(() => sqlite.exec(upgrade))();
  const db: EventDatabase = {
    prepare(sql) {
      return {
        bind(...values) {
          return {
            async run() {
              sqlite.prepare(sql).run(...values);
              return { success: true };
            },
          };
        },
      };
    },
  };
  return { sqlite, db };
}

test("Windows routes distinguish v3 metadata, NSIS redirects and update bundles", async () => {
  const prefix = "/agent/stable/releases/0.0.10/";
  const stem = "maple-agent-prod-0.0.10-abcdef0-windows-x86_64";
  for (const [file, kind] of [
    [`${stem}.tar.gz`, "windows-bundle"],
    [`${stem}-setup.exe`, "windows-installer"],
  ] as const) {
    const observation = capture(prefix + file);
    await observation.done();
    expect(observation.events[0]).toMatchObject({
      product: "agent",
      platform: "windows",
      artifact: kind,
      kind: "artifact",
      version: "0.0.10",
    });
    expect(isDownloadEvent(observation.events[0])).toBe(true);
    expect(
      isDownloadEvent({ ...observation.events[0], platform: "linux" }),
    ).toBe(false);
    expect(
      isDownloadEvent({ ...observation.events[0], product: "research" }),
    ).toBe(false);
    if (kind === "windows-bundle")
      expect(isDownloadEvent({ ...observation.events[0], kind: "alias" })).toBe(
        false,
      );
  }
  const alias = capture(
    "/download/agent/stable/windows",
    new Response(null, {
      status: 302,
      headers: { location: prefix + stem + "-setup.exe" },
    }),
  );
  await alias.done();
  expect(alias.events[0]).toMatchObject({
    kind: "alias",
    platform: "windows",
    artifact: "windows-installer",
    version: "0.0.10",
  });
  expect(isDownloadEvent(alias.events[0])).toBe(true);
  for (const file of [
    "agent-windows-arm64.tar.gz",
    "agent-windows-x86.tar.gz",
    "agent-Windows-x86_64.tar.gz",
    "agent-windows-x86_64-dev.tar.gz",
    "agent-windows-x86_64.exe",
    "agent-windows-x86_64.app.tar.gz",
  ]) {
    const observation = capture(prefix + file);
    await observation.done();
    expect(observation.events).toHaveLength(0);
  }
});

test("ARM64 requests use existing Windows ledger categories without retaining architecture-specific paths", async () => {
  const prefix = "/agent/stable/releases/0.0.16/";
  const stem = "maple-agent-prod-0.0.16-abcdef0-windows-aarch64";
  for (const [path, artifact, kind, version, response] of [
    [
      prefix + stem + ".tar.gz",
      "windows-bundle",
      "artifact",
      "0.0.16",
      new Response("payload"),
    ],
    [
      prefix + stem + "-setup.exe",
      "windows-installer",
      "artifact",
      "0.0.16",
      new Response("payload"),
    ],
    [
      "/agent/stable/windows-arm64/latest.json",
      "latest",
      "metadata",
      null,
      new Response("unparsed"),
    ],
    [
      prefix + "windows-arm64-manifest.json",
      "manifest",
      "metadata",
      "0.0.16",
      new Response("unparsed"),
    ],
    [
      "/download/agent/stable/windows-arm64",
      "windows-installer",
      "alias",
      "0.0.16",
      new Response(null, {
        status: 302,
        headers: { location: prefix + stem + "-setup.exe" },
      }),
    ],
  ] as const) {
    const observation = capture(path, response);
    await observation.done();
    expect(observation.events).toHaveLength(1);
    expect(observation.events[0]).toMatchObject({
      product: "agent",
      platform: "windows",
      artifact,
      kind,
      version,
    });
    expect(isDownloadEvent(observation.events[0])).toBe(true);
    expect(JSON.stringify(observation.events[0])).not.toContain("aarch64");
    expect(observation.response.bodyUsed).toBe(false);
  }
  const { sqlite, db } = await database();
  try {
    const observation = capture(prefix + stem + ".tar.gz");
    await observation.done();
    const item = message(observation.events[0]);
    await consumeEvents({ messages: [item] }, { DOWNLOAD_DB: db });
    expect(item).toMatchObject({ acks: 1, retries: 0 });
    expect(
      sqlite
        .query("SELECT platform, artifact, version FROM download_events")
        .get(),
    ).toEqual({
      platform: "windows",
      artifact: "windows-bundle",
      version: "0.0.16",
    });
  } finally {
    sqlite.close();
  }
});

test("captures original UTC timestamp and random observation ID without reading/replacing bytes or headers", async () => {
  const bytes = new Uint8Array([0, 255, 1, 128]);
  const response = new Response(bytes, {
    headers: {
      "content-length": "4",
      etag: '"exact"',
      "content-type": "application/octet-stream",
      "cache-control": "public, max-age=31536000, immutable",
    },
  });
  const body = response.body;
  const headers = [...response.headers];
  const before = Date.now();
  const observation = capture(ARTIFACT, response);
  expect(observation.events).toHaveLength(0);
  expect(response.body).toBe(body);
  expect(response.bodyUsed).toBe(false);
  expect([...response.headers]).toEqual(headers);
  await observation.done();
  const [captured] = observation.events;
  expect(isDownloadEvent(captured)).toBe(true);
  expect(Date.parse(captured.observed_at)).toBeGreaterThanOrEqual(before);
  expect(captured).toMatchObject({
    product: "agent",
    host: "updates.maple.ai",
    version: "0.0.9",
    platform: "macos",
    artifact: "macos-dmg",
    kind: "artifact",
    method: "GET",
    status: 200,
    range: "none",
    result: "success",
    response_length: 4,
    is_probe: 0,
  });
  expect(response.bodyUsed).toBe(false);
  expect(new Uint8Array(await response.arrayBuffer())).toEqual(bytes);
  const second = await event();
  expect(second.event_id).not.toBe(captured.event_id);
});

test("nonblocking queue outage retries with the same ID and only sanitized errors", async () => {
  const pending: Promise<unknown>[] = [];
  const sent: DownloadEvent[] = [];
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const response = new Response("still delivered", {
    headers: { "content-length": "15" },
  });
  trackResponse(
    new Request(ORIGIN + ARTIFACT),
    response,
    {
      DOWNLOAD_EVENTS: {
        async send(item) {
          sent.push(item);
          await gate;
          throw new Error("private SQL URL token");
        },
      },
    },
    {
      waitUntil(promise) {
        pending.push(promise);
      },
    },
    "agent",
  );
  expect(await response.text()).toBe("still delivered");
  release();
  await Promise.all(pending);
  expect(sent).toHaveLength(2);
  expect(sent[0].event_id).toBe(sent[1].event_id);
  expect(sent[0].observed_at).toBe(sent[1].observed_at);
  expect(logs.mock.calls).toEqual(
    Array(2).fill([
      '{"service":"download-tracking","code":"queue_send_failed"}',
    ]),
  );
});

test("missing binding or failed scheduling never affects availability", async () => {
  const response = new Response("same");
  const request = new Request(ORIGIN + ARTIFACT);
  const pending: Promise<unknown>[] = [];
  expect(() =>
    trackResponse(
      request,
      response,
      {},
      {
        waitUntil() {
          throw new Error("unused");
        },
      },
      "agent",
    ),
  ).not.toThrow();
  expect(() =>
    trackResponse(
      request,
      response,
      { DOWNLOAD_EVENTS: { async send() {} } },
      {
        waitUntil(promise) {
          pending.push(promise);
          throw new Error("private scheduler error");
        },
      },
      "agent",
    ),
  ).not.toThrow();
  await Promise.all(pending);
  expect(await response.text()).toBe("same");
  expect(logs.mock.calls).toEqual([
    ['{"service":"download-tracking","code":"queue_binding_missing"}'],
    ['{"service":"download-tracking","code":"capture_failed"}'],
  ]);
});

test("missing binding logs only for classified production routes", () => {
  const context = {
    waitUntil() {
      throw new Error("must not schedule");
    },
  };
  for (const url of [
    ORIGIN + "/private",
    "https://attacker.example" + ARTIFACT,
  ])
    trackResponse(new Request(url), new Response(null), {}, context, "agent");
  expect(logs.mock.calls).toHaveLength(0);
});

test("alias redirect intent and follow-up payload are separate kinds", async () => {
  const alias = capture(
    "/download/agent/stable/macos",
    new Response(null, {
      status: 302,
      headers: { location: ARTIFACT },
    }),
  );
  await alias.done();
  expect(alias.events[0]).toMatchObject({
    kind: "alias",
    version: "0.0.9",
    artifact: "macos-dmg",
    result: "redirect",
  });
  expect(isDownloadEvent(alias.events[0])).toBe(true);
  expect((await event()).kind).toBe("artifact");
});

test("Research aliases record only validated GitHub redirect versions", async () => {
  const cases = [
    ["macos", "Maple_3.4.5_universal.dmg", "macos-dmg", "macos"],
    ["windows", "Maple_3.4.5_x64-setup.exe", "windows-installer", "windows"],
    ["linux-appimage", "Maple_3.4.5_amd64.AppImage", "linux-appimage", "linux"],
    ["linux-deb", "Maple_3.4.5_amd64.deb", "linux-deb", "linux"],
    ["linux-rpm", "Maple-3.4.5-1.x86_64.rpm", "linux-rpm", "linux"],
    ["android", "app-universal-release.apk", "android-apk", "android"],
  ];
  for (const [key, file, artifact, platform] of cases) {
    const observation = capture(
      `/download/research/stable/${key}`,
      new Response(null, {
        status: 302,
        headers: {
          location: `https://github.com/MaplePrivacyLabs/Maple/releases/download/v3.4.5/${file}`,
        },
      }),
      undefined,
      "research",
    );
    await observation.done();
    expect(observation.events[0]).toMatchObject({
      kind: "alias",
      version: "3.4.5",
      product: "research",
      artifact,
      platform,
    });
    expect(isDownloadEvent(observation.events[0])).toBe(true);
  }
  const untrusted = capture(
    "/download/research/stable/macos",
    new Response(null, {
      status: 302,
      headers: { location: "https://untrusted.example/private?token=secret" },
    }),
    undefined,
    "research",
  );
  await untrusted.done();
  expect(untrusted.events[0].version).toBeNull();
  expect(JSON.stringify(untrusted.events)).not.toContain("secret");
});

test("Research marketing query requests are counted without retaining query values", async () => {
  for (const path of ["/latest.json", "/download/research/stable/macos"]) {
    const observation = capture(
      path + "?utm_source=private-query&token=not-for-storage",
      new Response(null),
      undefined,
      "research",
    );
    await observation.done();
    expect(observation.events).toHaveLength(1);
    expect(isDownloadEvent(observation.events[0])).toBe(true);
    const serialized = JSON.stringify(observation.events[0]);
    expect(serialized).not.toContain("private-query");
    expect(serialized).not.toContain("not-for-storage");
  }
});

test("HEAD, ranges, validators, errors and self-reported probes remain distinct", async () => {
  for (const [status, range, method, expectedRange, result] of [
    [200, null, "HEAD", "none", "success"],
    [206, "bytes=0-1", "GET", "partial", "success"],
    [416, "bytes=999-", "GET", "unsatisfiable", "client_error"],
    [200, "bytes=0-1", "GET", "requested", "success"],
    [304, null, "GET", "none", "not_modified"],
    [404, null, "GET", "none", "client_error"],
    [503, null, "GET", "none", "server_error"],
  ] as const) {
    const observation = capture(ARTIFACT, new Response(null, { status }), {
      method,
      headers: {
        ...(range ? { range } : {}),
        "x-maple-download-probe": "1",
        "user-agent": "Maple-Agent-Updater/1",
      },
    });
    await observation.done();
    expect(observation.events[0]).toMatchObject({
      method,
      status,
      range: expectedRange,
      result,
      is_probe: 1,
      client_kind: "maple",
    });
    expect(isDownloadEvent(observation.events[0])).toBe(true);
  }
});

test("metadata capture never parses or rewrites bodies and needs no version assertion", async () => {
  for (const [path, product, artifact, version, platform] of [
    ["/latest.json", "research", "latest", null, "all"],
    ["/agent/stable/latest.json", "agent", "latest", null, "macos"],
    ["/agent/stable/linux/latest.json", "agent", "latest", null, "linux"],
    ["/agent/stable/windows/latest.json", "agent", "latest", null, "windows"],
    [
      "/agent/stable/windows-arm64/latest.json",
      "agent",
      "latest",
      null,
      "windows",
    ],
    [
      "/agent/stable/releases/0.0.9/manifest.json",
      "agent",
      "manifest",
      "0.0.9",
      "macos",
    ],
    [
      "/agent/stable/releases/0.0.9/linux-manifest.json",
      "agent",
      "manifest",
      "0.0.9",
      "linux",
    ],
    [
      "/agent/stable/releases/0.0.10/windows-manifest.json",
      "agent",
      "manifest",
      "0.0.10",
      "windows",
    ],
  ] as const) {
    const observation = capture(
      path,
      new Response("unparsed original bytes"),
      undefined,
      product,
    );
    await observation.done();
    expect(observation.events[0]).toMatchObject({
      kind: "metadata",
      artifact,
      version,
      platform,
    });
    expect(isDownloadEvent(observation.events[0])).toBe(true);
    expect(observation.response.bodyUsed).toBe(false);
    expect(await observation.response.text()).toBe("unparsed original bytes");
  }
});

test("privacy allowlist omits arbitrary routes, query strings, methods, hosts and private request fields", async () => {
  for (const path of [
    "/private?token=secret",
    ARTIFACT + "?token=secret",
    "/installers.json",
    "/download/agent/stable/constructor",
    "/download/agent/stable/__proto__",
    "/agent/stable/releases/0.0.9/private.txt",
    "/agent/stable/releases/not-a-version/app.dmg",
    "https://attacker.example" + ARTIFACT,
    "http://updates.maple.ai" + ARTIFACT,
    "/agent/stable/releases/0.0.9/app%2Fdmg",
    "/download/agent/stable/macos/extra",
  ]) {
    const observation = capture(path);
    await observation.done();
    expect(observation.events).toHaveLength(0);
  }
  const post = capture(ARTIFACT, new Response(null, { status: 405 }), {
    method: "POST",
  });
  await post.done();
  expect(post.events).toHaveLength(0);
  const observation = capture(ARTIFACT, new Response(null), {
    headers: {
      authorization: "Bearer private",
      cookie: "private",
      "cf-connecting-ip": "192.0.2.1",
      "user-agent": "curl/8 private custom identifier",
      "x-maple-download-probe": "false",
    },
  });
  await observation.done();
  expect(observation.events[0]).toMatchObject({
    client_kind: "cli",
    is_probe: 0,
  });
  const serialized = JSON.stringify(observation.events[0]);
  for (const text of ["private", "192.0.2.1", ARTIFACT, "curl/8"])
    expect(serialized).not.toContain(text);
});

test("content length is bounded expected response size, never transferred bytes", async () => {
  for (const length of [
    "-1",
    "1.5",
    "9007199254740992",
    "4294967297",
    "private",
  ]) {
    const observation = capture(
      ARTIFACT,
      new Response(null, { headers: { "content-length": length } }),
    );
    await observation.done();
    expect(observation.events[0].response_length).toBeNull();
  }
});

test("D1 commits original timestamps and deduplicates ambiguous/redelivered queue writes", async () => {
  const { sqlite, db } = await database();
  try {
    const body = await event();
    body.observed_at = "2026-10-27T12:00:00.000Z";
    const first = message(body);
    const duplicate = message(structuredClone(body));
    await consumeEvents({ messages: [first, duplicate] }, { DOWNLOAD_DB: db });
    expect(first).toMatchObject({ acks: 1, retries: 0 });
    expect(duplicate).toMatchObject({ acks: 1, retries: 0 });
    expect(
      sqlite.query("SELECT COUNT(*) AS count FROM download_events").get(),
    ).toEqual({ count: 1 });
    expect(
      sqlite.query("SELECT observed_at, version FROM download_events").get(),
    ).toEqual({ observed_at: body.observed_at, version: "0.0.9" });
  } finally {
    sqlite.close();
  }
});

test("SQL outage and unsuccessful D1 result retry without acknowledging or leaking exceptions", async () => {
  for (const run of [
    async () => {
      throw new Error("private database secret");
    },
    async () => ({ success: false }),
  ]) {
    const item = message(await event());
    await consumeEvents(
      { messages: [item] },
      {
        DOWNLOAD_DB: {
          prepare() {
            return {
              bind() {
                return { run };
              },
            };
          },
        },
      },
    );
    expect(item).toMatchObject({ acks: 0, retries: 1 });
  }
  expect(logs.mock.calls).toEqual(
    Array(2).fill([
      '{"service":"download-tracking","code":"database_write_failed"}',
    ]),
  );
});

test("invalid or poison events retry for configured DLQ, never insert or log their content", async () => {
  const good = await event();
  const bad: unknown[] = [
    null,
    "private",
    { ...good, ip: "192.0.2.1" },
    { ...good, event_id: "private" },
    { ...good, observed_at: "2026-02-30T00:00:00.000Z" },
    { ...good, host: "private.example" },
    { ...good, version: "x".repeat(1000) },
    { ...good, product: "research" },
    { ...good, status: 503 },
    { ...good, response_length: -1 },
    { ...good, kind: "metadata" },
    { ...good, platform: "windows" },
    { ...good, is_probe: "1" },
  ];
  const items = bad.map(message);
  let writes = 0;
  await consumeEvents(
    { messages: items },
    {
      DOWNLOAD_DB: {
        prepare() {
          writes++;
          throw new Error("must not prepare");
        },
      },
    },
  );
  expect(writes).toBe(0);
  for (const item of items) expect(item).toMatchObject({ acks: 0, retries: 1 });
  expect(logs.mock.calls).toEqual(
    bad.map(() => ['{"service":"download-tracking","code":"invalid_event"}']),
  );
});
