import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, test } from "bun:test";
import { fixture, PAYLOAD } from "./fixtures";
import { linuxFixture } from "./linux-fixtures";
import { windowsFixture } from "./windows-fixtures";

// Miniflare is Wrangler's exact locked runtime dependency. Node hosts the
// harness; assertions make real requests through workerd and its R2 binding.
const server = `
 import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
 import { readFileSync } from 'node:fs';
 const mf = new Miniflare(convertV4MiniflareOptions({modules:true, name:'agent-updates-test', script:readFileSync(process.env.MAPLE_TEST_WORKER,'utf8'), compatibilityDate:'2026-08-24', r2Buckets:['AGENT_RELEASES'], port:0, inspectorPort:0}));
 process.on('SIGTERM', async()=>{await mf.dispose(); process.exit(0);});
 process.send({origin:(await mf.ready).origin});
`;
test("real workerd R2 binding serves separate feeds, immutable bytes, ranges, HEAD and redirects", async () => {
  const directory = await mkdtemp(join(tmpdir(), "agent-updates-workerd-"));
  let child: ReturnType<typeof Bun.spawn> | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const wrapper = join(directory, "entry.ts");
    await Bun.write(
      wrapper,
      `
      import worker from ${JSON.stringify(join(import.meta.dir, "index.ts"))};
      export default { async fetch(request,env) {
        if (new URL(request.url).pathname === '/__seed' && request.method === 'POST') {
          const {metadata,linuxMetadata,windowsMetadata,payload} = await request.json();
          const bytes = Uint8Array.from(metadata);
          await env.AGENT_RELEASES.put('stable/latest.json',bytes);
          await env.AGENT_RELEASES.put('stable/releases/0.0.2/manifest.json',bytes);
          await env.AGENT_RELEASES.put('stable/releases/0.0.2/Maple-Agent.app.tar.gz',Uint8Array.from(payload));
          await env.AGENT_RELEASES.put('stable/releases/0.0.2/Maple-Agent.dmg',Uint8Array.from(payload));
          await env.AGENT_RELEASES.put('stable/linux/latest.json',Uint8Array.from(linuxMetadata));
          await env.AGENT_RELEASES.put('stable/releases/0.0.2/linux-manifest.json',Uint8Array.from(linuxMetadata));
          await env.AGENT_RELEASES.put('stable/releases/0.0.2/Maple-Agent-linux-x86_64.tar.gz',Uint8Array.from(payload));
          await env.AGENT_RELEASES.put('stable/windows/latest.json',Uint8Array.from(windowsMetadata));
          await env.AGENT_RELEASES.put('stable/releases/0.0.2/windows-manifest.json',Uint8Array.from(windowsMetadata));
          await env.AGENT_RELEASES.put('stable/releases/0.0.2/Maple-Agent-windows-x86_64.tar.gz',Uint8Array.from(payload));
          await env.AGENT_RELEASES.put('stable/releases/0.0.2/Maple-Agent-windows-x86_64-setup.exe',Uint8Array.from(payload));
          await env.AGENT_RELEASES.put('stable/releases/0.0.2/publication-manifest.json',Uint8Array.from(payload));
          return new Response('seeded');
        }
        return worker.fetch(request,env);
      }};
    `,
    );
    const build = await Bun.build({
      entrypoints: [wrapper],
      outdir: directory,
      target: "browser",
      format: "esm",
    });
    if (!build.success) throw new Error("fixture Worker bundle failed");
    const origin = await new Promise<string>((resolve, reject) => {
      timer = setTimeout(
        () => reject(new Error("workerd startup timeout")),
        20000,
      );
      child = Bun.spawn(["node", "--input-type=module", "-e", server], {
        env: {
          ...process.env,
          MAPLE_TEST_WORKER: join(directory, "entry.js"),
          MAPLE_TEST_METADATA: Buffer.from(fixture().bytes).toString("base64"),
          MAPLE_TEST_PAYLOAD: Buffer.from(PAYLOAD).toString("base64"),
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
          reject(new Error(`workerd exited ${code}`));
        },
      });
    });
    clearTimeout(timer);
    const seeded = await fetch(`${origin}/__seed`, {
      method: "POST",
      body: JSON.stringify({
        metadata: Array.from(fixture().bytes),
        linuxMetadata: Array.from(linuxFixture().bytes),
        windowsMetadata: Array.from(windowsFixture().bytes),
        payload: Array.from(PAYLOAD),
      }),
    });
    expect(seeded.status).toBe(200);
    const latest = await fetch(`${origin}/agent/stable/latest.json`);
    expect(latest.status).toBe(200);
    expect(new Uint8Array(await latest.arrayBuffer())).toEqual(fixture().bytes);
    const redirect = await fetch(`${origin}/download/agent/stable/macos`, {
      redirect: "manual",
    });
    expect(redirect.status).toBe(302);
    expect(redirect.headers.get("location")).toBe(
      "/agent/stable/releases/0.0.2/Maple-Agent.dmg",
    );
    const artifact = `${origin}/agent/stable/releases/0.0.2/Maple-Agent.app.tar.gz`;
    const full = await fetch(artifact);
    expect(full.status).toBe(200);
    expect(new Uint8Array(await full.arrayBuffer())).toEqual(PAYLOAD);
    const range = await fetch(artifact, { headers: { range: "bytes=2-7" } });
    expect(range.status).toBe(206);
    expect(range.headers.get("content-range")).toBe(
      `bytes 2-7/${PAYLOAD.length}`,
    );
    expect(new Uint8Array(await range.arrayBuffer())).toEqual(
      PAYLOAD.slice(2, 8),
    );
    const head = await fetch(artifact, { method: "HEAD" });
    expect(head.status).toBe(200);
    expect(head.headers.get("content-length")).toBe(String(PAYLOAD.length));
    expect(await head.text()).toBe("");
    const linuxLatest = await fetch(`${origin}/agent/stable/linux/latest.json`);
    expect(linuxLatest.status).toBe(200);
    expect(linuxLatest.headers.get("cdn-cache-control")).toBe("no-store");
    expect(new Uint8Array(await linuxLatest.arrayBuffer())).toEqual(
      linuxFixture().bytes,
    );
    const linuxRedirect = await fetch(`${origin}/download/agent/stable/linux`, {
      redirect: "manual",
    });
    expect(linuxRedirect.status).toBe(302);
    expect(linuxRedirect.headers.get("cache-control")).toBe("no-store");
    expect(linuxRedirect.headers.get("location")).toBe(
      linuxFixture().manifest.artifacts["linux-x86_64"].path,
    );
    const linuxArtifact = `${origin}${linuxFixture().manifest.artifacts["linux-x86_64"].path}`;
    const linuxRange = await fetch(linuxArtifact, {
      headers: { range: "bytes=2-7" },
    });
    expect(linuxRange.status).toBe(206);
    expect(new Uint8Array(await linuxRange.arrayBuffer())).toEqual(
      PAYLOAD.slice(2, 8),
    );
    const linuxHead = await fetch(linuxArtifact, { method: "HEAD" });
    expect(linuxHead.status).toBe(200);
    expect(linuxHead.headers.get("content-length")).toBe(
      String(PAYLOAD.length),
    );
    expect(await linuxHead.text()).toBe("");
    const linuxManifest = await fetch(
      `${origin}/agent/stable/releases/0.0.2/linux-manifest.json`,
    );
    expect(linuxManifest.status).toBe(200);
    expect(new Uint8Array(await linuxManifest.arrayBuffer())).toEqual(
      linuxFixture().bytes,
    );
    const windows = windowsFixture();
    const windowsLatest = await fetch(
      `${origin}/agent/stable/windows/latest.json`,
    );
    expect(windowsLatest.status).toBe(200);
    expect(windowsLatest.headers.get("cdn-cache-control")).toBe("no-store");
    expect(new Uint8Array(await windowsLatest.arrayBuffer())).toEqual(
      windows.bytes,
    );
    const windowsRedirect = await fetch(
      `${origin}/download/agent/stable/windows`,
      { redirect: "manual" },
    );
    expect(windowsRedirect.status).toBe(302);
    expect(windowsRedirect.headers.get("location")).toBe(
      windows.manifest.downloads.windows.path,
    );
    expect(windowsRedirect.headers.get("cache-control")).toBe("no-store");
    const windowsManifest = await fetch(
      `${origin}/agent/stable/releases/0.0.2/windows-manifest.json`,
    );
    expect(windowsManifest.status).toBe(200);
    expect(new Uint8Array(await windowsManifest.arrayBuffer())).toEqual(
      windows.bytes,
    );
    for (const artifact of [
      windows.manifest.artifacts["windows-x86_64"],
      windows.manifest.downloads.windows,
    ]) {
      const full = await fetch(`${origin}${artifact.path}`);
      expect(full.status).toBe(200);
      expect(new Uint8Array(await full.arrayBuffer())).toEqual(PAYLOAD);
      const partial = await fetch(`${origin}${artifact.path}`, {
        headers: { range: "bytes=2-7" },
      });
      expect(partial.status).toBe(206);
      expect(new Uint8Array(await partial.arrayBuffer())).toEqual(
        PAYLOAD.slice(2, 8),
      );
      const head = await fetch(`${origin}${artifact.path}`, { method: "HEAD" });
      expect(head.status).toBe(200);
      expect(head.headers.get("content-length")).toBe(String(PAYLOAD.length));
      expect(await head.text()).toBe("");
    }
    expect(
      (
        await fetch(
          `${origin}/agent/stable/releases/0.0.2/publication-manifest.json`,
        )
      ).status,
    ).toBe(404);
    expect(
      (await fetch(`${origin}/download/agent/stable/linux-appimage`)).status,
    ).toBe(404);
    expect((await fetch(`${origin}/latest.json`)).status).toBe(404);
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
}, 45000);
