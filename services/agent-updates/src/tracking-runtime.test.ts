import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, test } from "bun:test";
import { prepareInstallerCatalog } from "../../updates/src/prepare-installers";
import { fixture as researchFixture } from "../../updates/src/test-fixtures";
import { fixture, PAYLOAD } from "./fixtures";

// Use Wrangler's locked Miniflare/workerd dependency. All storage, queues and
// fixture-only control routes below live in one disposable local runtime.
const server = `
  import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
  import { readFileSync } from 'node:fs';
  const mf = new Miniflare(convertV4MiniflareOptions({
    modules: true, name: 'download-tracking-test',
    script: readFileSync(process.env.MAPLE_TEST_WORKER, 'utf8'),
    compatibilityDate: '2026-08-24', port: 0, inspectorPort: 0,
    r2Buckets: ['AGENT_RELEASES'], d1Databases: ['DOWNLOAD_DB'],
    queueProducers: { DOWNLOAD_EVENTS: 'download-events-test' },
    queueConsumers: { 'download-events-test': {
      maxBatchSize: 1, maxBatchTimeout: 0, retryDelay: 1, maxRetries: 5,
    } },
  }));
  process.on('SIGTERM', async () => { await mf.dispose(); process.exit(0); });
  process.send({ origin: (await mf.ready).origin });
`;

async function eventually<T>(
  read: () => Promise<T>,
  accept: (value: T) => boolean,
): Promise<T> {
  const deadline = Date.now() + 15000;
  let last: T;
  do {
    last = await read();
    if (accept(last)) return last;
    await Bun.sleep(100);
  } while (Date.now() < deadline);
  throw new Error(
    `tracking runtime condition timed out: ${JSON.stringify(last)}`,
  );
}

test("real workerd queues persist Agent and Research request events once and retry a D1 outage", async () => {
  const directory = await mkdtemp(join(tmpdir(), "download-tracking-workerd-"));
  let child: ReturnType<typeof Bun.spawn> | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const wrapper = join(directory, "entry.ts");
    await Bun.write(
      wrapper,
      `
      import agent from ${JSON.stringify(join(import.meta.dir, "index.ts"))};
      import research from ${JSON.stringify(join(import.meta.dir, "../../updates/src/index.ts"))};
      import { trackResponse } from ${JSON.stringify(join(import.meta.dir, "../../download-tracking/tracking.ts"))};
      export default {
        async queue(batch, env, ctx) {
          await agent.queue(batch, env, ctx);
          await env.DOWNLOAD_DB.prepare('INSERT INTO test_deliveries VALUES (?)').bind(batch.messages.length).run();
        },
        async fetch(request, env, ctx) {
          const path = new URL(request.url).pathname;
          if (path === '/__seed' && request.method === 'POST') {
            const { metadata, payload, latest, catalog, migration } = await request.json();
            const statements = migration.split(';').filter(statement => statement.trim());
            await env.DOWNLOAD_DB.batch(statements.map(statement => env.DOWNLOAD_DB.prepare(statement)));
            await env.DOWNLOAD_DB.prepare('CREATE TABLE test_deliveries (message_count INTEGER NOT NULL)').run();
            await env.AGENT_RELEASES.put('stable/latest.json', Uint8Array.from(metadata));
            await env.AGENT_RELEASES.put('stable/releases/0.0.2/manifest.json', Uint8Array.from(metadata));
            await env.AGENT_RELEASES.put('stable/releases/0.0.2/Maple-Agent.app.tar.gz', Uint8Array.from(payload));
            await env.AGENT_RELEASES.put('stable/releases/0.0.2/Maple-Agent.dmg', Uint8Array.from(payload));
            await env.AGENT_RELEASES.put('test/latest.json', Uint8Array.from(latest));
            await env.AGENT_RELEASES.put('test/installers.json', JSON.stringify(catalog));
            return new Response('seeded');
          }
          if (path === '/__upgrade' && request.method === 'POST') {
            const { migration, fail } = await request.json();
            const statements = migration.split(';').filter(statement => statement.trim());
            if (fail) statements.push('SELECT * FROM injected_migration_failure');
            try {
              await env.DOWNLOAD_DB.batch(statements.map(statement => env.DOWNLOAD_DB.prepare(statement)));
              return new Response('upgraded');
            } catch {
              return new Response('migration rolled back', { status: 503 });
            }
          }
          if (path === '/__schema') {
            const schema = await env.DOWNLOAD_DB.prepare("SELECT type, name, sql FROM sqlite_master WHERE name LIKE 'download_events%' ORDER BY type, name").all();
            return Response.json(schema.results);
          }
          if (path === '/__windows' && request.method === 'POST') {
            const payload = Uint8Array.from((await request.json()).payload);
            const response = new Response(payload, { headers: { 'content-length': String(payload.length) } });
            trackResponse(new Request('https://updates.maple.ai/agent/stable/releases/0.0.10/maple-agent-prod-0.0.10-abcdef0-windows-x86_64.tar.gz', {
              headers: { 'x-maple-download-probe': '1' },
            }), response, env, ctx, 'agent');
            return response;
          }
          if (path === '/__rows') {
            const rows = await env.DOWNLOAD_DB.prepare('SELECT * FROM download_events ORDER BY observed_at, event_id').all();
            const delivered = await env.DOWNLOAD_DB.prepare('SELECT COALESCE(SUM(message_count), 0) AS count FROM test_deliveries').first();
            return Response.json({ rows: rows.results, deliveries: delivered.count });
          }
          if (path === '/__replay' && request.method === 'POST') {
            const event = await env.DOWNLOAD_DB.prepare('SELECT * FROM download_events LIMIT 1').first();
            await env.DOWNLOAD_EVENTS.send({ schema_version: 1, ...event });
            await env.DOWNLOAD_EVENTS.send({ schema_version: 1, ...event });
            return new Response('replayed');
          }
          if (path === '/__outage' && request.method === 'POST') {
            await env.DOWNLOAD_DB.prepare('ALTER TABLE download_events RENAME TO test_unavailable_events').run();
            return new Response('unavailable');
          }
          if (path === '/__attempts') {
            const row = await env.DOWNLOAD_DB.prepare('SELECT COALESCE(SUM(message_count), 0) AS count FROM test_deliveries').first();
            return Response.json(row);
          }
          if (path === '/__recover' && request.method === 'POST') {
            await env.DOWNLOAD_DB.prepare('ALTER TABLE test_unavailable_events RENAME TO download_events').run();
            return new Response('recovered');
          }
          const canonical = new Request('https://updates.maple.ai' + path + new URL(request.url).search, request);
          if (path.startsWith('/agent/') || path.startsWith('/download/agent/')) {
            return agent.fetch(canonical, env, ctx);
          }
          return research.fetch(canonical, {
            DOWNLOAD_EVENTS: env.DOWNLOAD_EVENTS,
            ASSETS: { async fetch(assetRequest) {
              const object = await env.AGENT_RELEASES.get('test' + new URL(assetRequest.url).pathname);
              return object ? new Response(object.body) : new Response(null, { status: 404 });
            } },
          }, ctx);
        },
      };
      `,
    );
    const build = await Bun.build({
      entrypoints: [wrapper],
      outdir: directory,
      target: "browser",
      format: "esm",
    });
    if (!build.success) throw new Error("tracking fixture bundle failed");
    const origin = await new Promise<string>((resolve, reject) => {
      timer = setTimeout(
        () => reject(new Error("tracking workerd startup timeout")),
        20000,
      );
      child = Bun.spawn(["node", "--input-type=module", "-e", server], {
        env: {
          ...process.env,
          MAPLE_TEST_WORKER: join(directory, "entry.js"),
        },
        stdout: "ignore",
        stderr: "inherit",
        serialization: "json",
        ipc(value: unknown) {
          if (
            value &&
            typeof value === "object" &&
            "origin" in value &&
            typeof value.origin === "string"
          )
            resolve(value.origin);
        },
        onExit(_child, code) {
          reject(new Error(`tracking workerd exited ${code}`));
        },
      });
    });
    clearTimeout(timer);
    const { release, latestBytes } = researchFixture();
    expect(
      (
        await fetch(`${origin}/__seed`, {
          method: "POST",
          body: JSON.stringify({
            migration: await Bun.file(
              join(import.meta.dir, "../migrations/0001_download_events.sql"),
            ).text(),
            metadata: Array.from(fixture().bytes),
            payload: Array.from(PAYLOAD),
            latest: Array.from(latestBytes),
            catalog: prepareInstallerCatalog(release, latestBytes),
          }),
        })
      ).status,
    ).toBe(200);
    const artifact = `${origin}/agent/stable/releases/0.0.2/Maple-Agent.app.tar.gz`;
    const full = await fetch(artifact, {
      headers: {
        authorization: "Bearer private-must-not-be-stored",
        "x-forwarded-for": "203.0.113.10",
        "user-agent": "private-ua-must-not-be-stored",
      },
    });
    expect(full.status).toBe(200);
    expect(new Uint8Array(await full.arrayBuffer())).toEqual(PAYLOAD);
    const partial = await fetch(artifact, {
      headers: { range: "bytes=2-7", "x-maple-download-probe": "1" },
    });
    expect(partial.status).toBe(206);
    expect(new Uint8Array(await partial.arrayBuffer())).toEqual(
      PAYLOAD.slice(2, 8),
    );
    expect((await fetch(artifact, { method: "HEAD" })).status).toBe(200);
    expect(
      (
        await fetch(`${origin}/download/agent/stable/macos`, {
          redirect: "manual",
        })
      ).status,
    ).toBe(302);
    expect((await fetch(`${origin}/agent/stable/latest.json`)).status).toBe(
      200,
    );
    expect(
      (
        await fetch(
          `${origin}/download/research/stable/macos?utm_source=private-query-must-not-be-stored`,
          {
            redirect: "manual",
          },
        )
      ).status,
    ).toBe(302);
    expect((await fetch(`${origin}/latest.json`)).status).toBe(200);
    type Snapshot = {
      rows: Array<Record<string, unknown>>;
      deliveries: number;
    };
    const snapshot = () =>
      fetch(`${origin}/__rows`).then(
        (response) => response.json() as Promise<Snapshot>,
      );
    const first = await eventually(
      snapshot,
      (value) => value.rows.length === 7 && value.deliveries >= 7,
    );
    expect(new Set(first.rows.map((row) => row.event_id)).size).toBe(7);
    expect(first.rows.filter((row) => row.product === "research")).toHaveLength(
      2,
    );
    expect(first.rows.filter((row) => row.kind === "artifact")).toHaveLength(3);
    expect(first.rows.filter((row) => row.is_probe === 1)).toHaveLength(1);
    expect(JSON.stringify(first.rows)).not.toContain("private-");
    expect(JSON.stringify(first.rows)).not.toContain("203.0.113.10");
    expect(
      first.rows.every(
        (row) =>
          typeof row.observed_at === "string" &&
          Number.isFinite(Date.parse(row.observed_at)),
      ),
    ).toBe(true);

    const upgrade = await Bun.file(
      join(import.meta.dir, "../migrations/0002_windows_download_events.sql"),
    ).text();
    const schemaBefore = await fetch(`${origin}/__schema`).then((response) =>
      response.json(),
    );
    expect(
      (
        await fetch(`${origin}/__upgrade`, {
          method: "POST",
          body: JSON.stringify({ migration: upgrade, fail: true }),
        })
      ).status,
    ).toBe(503);
    expect((await snapshot()).rows).toEqual(first.rows);
    expect(
      await fetch(`${origin}/__schema`).then((response) => response.json()),
    ).toEqual(schemaBefore);
    expect(
      (
        await fetch(`${origin}/__upgrade`, {
          method: "POST",
          body: JSON.stringify({ migration: upgrade, fail: false }),
        })
      ).status,
    ).toBe(200);
    expect((await snapshot()).rows).toEqual(first.rows);
    const schemaAfter = (await fetch(`${origin}/__schema`).then((response) =>
      response.json(),
    )) as Array<{ type: string; name: string; sql: string }>;
    expect(
      schemaAfter.filter((row) => row.type === "index").map((row) => row.name),
    ).toEqual(["download_events_observed_at", "download_events_trends"]);
    expect(schemaAfter.find((row) => row.type === "table")?.sql).toContain(
      "'windows-bundle'",
    );
    const windows = await fetch(`${origin}/__windows`, {
      method: "POST",
      body: JSON.stringify({ payload: Array.from(PAYLOAD) }),
    });
    expect(new Uint8Array(await windows.arrayBuffer())).toEqual(PAYLOAD);
    const withWindows = await eventually(
      snapshot,
      (value) => value.rows.length === 8,
    );
    expect(
      withWindows.rows.filter((row) => row.platform === "windows"),
    ).toHaveLength(1);
    expect(
      withWindows.rows.find((row) => row.platform === "windows"),
    ).toMatchObject({
      artifact: "windows-bundle",
      kind: "artifact",
      is_probe: 1,
    });

    expect((await fetch(`${origin}/__replay`, { method: "POST" })).status).toBe(
      200,
    );
    const replayed = await eventually(
      snapshot,
      (value) => value.deliveries >= withWindows.deliveries + 2,
    );
    expect(replayed.rows).toEqual(withWindows.rows);

    expect((await fetch(`${origin}/__outage`, { method: "POST" })).status).toBe(
      200,
    );
    const duringOutage = await fetch(artifact);
    expect(duringOutage.status).toBe(200);
    expect(new Uint8Array(await duringOutage.arrayBuffer())).toEqual(PAYLOAD);
    await eventually(
      () =>
        fetch(`${origin}/__attempts`).then(
          (response) => response.json() as Promise<{ count: number }>,
        ),
      (value) => value.count > replayed.deliveries,
    );
    expect(
      (await fetch(`${origin}/__recover`, { method: "POST" })).status,
    ).toBe(200);
    const recovered = await eventually(
      snapshot,
      (value) => value.rows.length === 9,
    );
    expect(
      recovered.rows.filter((row) => row.kind === "artifact"),
    ).toHaveLength(5);
  } finally {
    clearTimeout(timer);
    child?.kill("SIGTERM");
    if (child) {
      const forceStop = setTimeout(() => child?.kill("SIGKILL"), 3000);
      await child.exited;
      clearTimeout(forceStop);
    }
    await rm(directory, { recursive: true, force: true });
  }
}, 60000);
