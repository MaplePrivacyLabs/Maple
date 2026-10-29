import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

// Keep local validation independent of the owner-provisioned production bucket.
// Bundle and validate code using Wrangler's actual dry-run path.
// This checked-in config uses whole-line comments and trailing commas only.
// Unsupported JSONC syntax fails closed instead of weakening deployment guards.
const config: unknown = JSON.parse(
  (await Bun.file("wrangler.jsonc").text())
    .replace(/^\s*\/\/.*$/gm, "")
    .replace(/,(\s*[}\]])/g, "$1"),
);
if (!config || typeof config !== "object" || Array.isArray(config)) {
  throw new Error("invalid Worker configuration");
}
const workerConfig = config as Record<string, unknown>;
if (
  "route" in workerConfig ||
  "routes" in workerConfig ||
  "env" in workerConfig
) {
  throw new Error(
    "Agent dry run cannot manage routing or environment overrides",
  );
}
const directory = await mkdtemp(join(tmpdir(), "agent-updates-dry-run-"));
try {
  const path = join(directory, "wrangler.json");
  await Bun.write(
    path,
    JSON.stringify({
      ...workerConfig,
      main: resolve("src/index.ts"),
      r2_buckets: [
        {
          binding: "AGENT_RELEASES",
          bucket_name: "agent-updates-local-dry-run",
        },
      ],
      d1_databases: [
        {
          binding: "DOWNLOAD_DB",
          database_name: "downloads-local-dry-run",
          database_id: "00000000-0000-4000-8000-000000000001",
        },
      ],
      queues: {
        producers: [
          { binding: "DOWNLOAD_EVENTS", queue: "downloads-local-dry-run" },
        ],
      },
    }),
  );
  const result = Bun.spawn(
    [
      "node",
      resolve("node_modules/wrangler/bin/wrangler.js"),
      "deploy",
      "--dry-run",
      "--config",
      path,
      "--outdir",
      resolve(".wrangler/dry-run"),
    ],
    {
      stdout: "inherit",
      stderr: "inherit",
      env: { ...process.env, WRANGLER_SEND_METRICS: "false" },
    },
  );
  if ((await result.exited) !== 0) process.exitCode = 1;
} finally {
  await rm(directory, { recursive: true, force: true });
}
