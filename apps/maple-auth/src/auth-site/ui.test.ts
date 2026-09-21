import { describe, expect, test } from "bun:test";
import { fileURLToPath } from "node:url";

const appDirectory = fileURLToPath(new URL("../..", import.meta.url));

describe("hosted auth UI with the real SDK context", () => {
  for (const [fixture, cases] of [
    ["AuthSite", 23],
    ["HostedAppleSignIn", 9]
  ] as const) {
    test(`${fixture} runs every case without shared module mocks`, () => {
      // Keep global DOM and storage fixtures isolated while making every real-context
      // case mandatory in the default test suite.
      const result = Bun.spawnSync(
        [process.execPath, "--no-env-file", "test", `./src/auth-site/fixtures/${fixture}.case.tsx`],
        {
          cwd: appDirectory,
          env: { PATH: process.env.PATH, LANG: "C.UTF-8", NO_COLOR: "1" },
          stdout: "pipe",
          stderr: "pipe",
          timeout: 10_000
        }
      );
      const output =
        new TextDecoder().decode(result.stdout) + new TextDecoder().decode(result.stderr);
      if (result.exitCode !== 0) throw new Error(`${fixture} child tests failed:\n${output}`);
      // A missing or undiscovered fixture must fail, even if Bun exits successfully.
      expect(output).toMatch(new RegExp(`\\b${cases} pass\\b`));
      expect(output).toMatch(/\b0 fail\b/u);
      expect(output).toMatch(new RegExp(`Ran ${cases} tests across 1 file`));
    }, 15_000);
  }
});
