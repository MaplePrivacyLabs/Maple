import { describe, expect, test } from "bun:test";
import { fileURLToPath } from "node:url";

const frontendDirectory = fileURLToPath(new URL("../..", import.meta.url));

describe("hosted auth with the real SDK provider", () => {
  for (const scenario of ["cold-start", "cold-callback", "retained-start", "retained-callback"]) {
    test(`${scenario} waits for provider bootstrap`, () => {
      // Separate processes keep the real SDK's initial module-level API URL empty,
      // regardless of context mocks or SDK initialization in other test files.
      const result = Bun.spawnSync(
        [
          process.execPath,
          "--no-env-file",
          "--preload",
          "./src/lib/test/preload.ts",
          "--preload",
          "./src/lib/test/der-loader.ts",
          "./src/auth-site/fixtures/bootstrap.tsx",
          scenario
        ],
        {
          cwd: frontendDirectory,
          env: { PATH: process.env.PATH, LANG: "C.UTF-8" },
          stdout: "pipe",
          stderr: "pipe",
          timeout: 10_000
        }
      );
      expect(new TextDecoder().decode(result.stderr)).toBe("");
      expect(result.exitCode).toBe(0);
      expect(new TextDecoder().decode(result.stdout).trim()).toBe("bootstrap verified");
    }, 15_000);
  }
});
