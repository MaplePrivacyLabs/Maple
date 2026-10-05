import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, test } from "bun:test";
import {
  assertPublishedBundle,
  prepareInstallerCatalog,
} from "./prepare-installers";
import { fixture } from "./test-fixtures";

// Wrangler runs under its supported Node runtime, while Bun owns the test and
// assertions. Port 0 and temporary assets keep this independent of local dev.
const server = `
  import { unstable_dev, unstable_readConfig } from "wrangler";
  const config = unstable_readConfig({ config: "wrangler.jsonc" }, { hideWarnings: true });
  const worker = await unstable_dev(config.main, {
    config: "wrangler.jsonc", assets: process.env.MAPLE_TEST_ASSETS,
    ip: "127.0.0.1", port: 0, inspectorPort: 0, local: true,
    persist: false, envFiles: [], logLevel: "error",
    experimental: { forceLocal: true, disableDevRegistry: true,
      disableExperimentalWarning: true, watch: false },
  });
  process.on("SIGTERM", async () => { await worker.stop(); process.exit(0); });
  process.send({ origin: "http://" + worker.address + ":" + worker.port });
`;

test("the configured Worker starts in workerd and serves its bundled metadata and redirects", async () => {
  const directory = await mkdtemp(join(tmpdir(), "maple-updates-runtime-"));
  let child: ReturnType<typeof Bun.spawn> | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const { release, latestBytes } = fixture();
    const catalog = prepareInstallerCatalog(release, latestBytes);
    await Bun.write(join(directory, "latest.json"), latestBytes);
    await Bun.write(
      join(directory, "installers.json"),
      JSON.stringify(catalog),
    );
    const ready = new Promise<string>((resolve, reject) => {
      timer = setTimeout(
        () => reject(new Error("workerd did not become ready")),
        20000,
      );
      child = Bun.spawn(["node", "--input-type=module", "-e", server], {
        env: {
          ...process.env,
          MAPLE_TEST_ASSETS: directory,
          WRANGLER_SEND_METRICS: "false",
        },
        serialization: "json",
        stdout: "ignore",
        stderr: "inherit",
        ipc(message: unknown) {
          if (
            typeof message === "object" &&
            message !== null &&
            "origin" in message &&
            typeof message.origin === "string"
          )
            resolve(message.origin);
        },
        onExit(_child, code) {
          reject(new Error(`workerd launcher exited with ${code}`));
        },
      });
    });
    const origin = await ready;
    clearTimeout(timer);
    await assertPublishedBundle(origin, catalog, latestBytes);
    expect((await fetch(`${origin}/installers.json`)).status).toBe(404);
  } finally {
    clearTimeout(timer);
    child?.kill("SIGTERM");
    if (child) await child.exited;
    await rm(directory, { recursive: true, force: true });
  }
}, 45000);
