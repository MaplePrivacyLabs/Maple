import { describe, expect, test } from "bun:test";
import { fileURLToPath } from "node:url";

const appDirectory = fileURLToPath(new URL("../..", import.meta.url));
const cases = 16;

describe("Agent return UI with the real SDK context", () => {
  for (const environment of ["production", "development"]) {
    test(`${environment} runs every Agent return case in isolation`, () => {
      const result = Bun.spawnSync(
        [
          process.execPath,
          "--no-env-file",
          "test",
          "./src/auth-site/fixtures/AgentReturn.case.tsx"
        ],
        {
          cwd: appDirectory,
          env: {
            PATH: process.env.PATH,
            LANG: "C.UTF-8",
            NO_COLOR: "1",
            VITE_AUTH_ENVIRONMENT: environment
          },
          stdout: "pipe",
          stderr: "pipe",
          timeout: 10_000
        }
      );
      const output =
        new TextDecoder().decode(result.stdout) + new TextDecoder().decode(result.stderr);
      if (result.exitCode !== 0) throw new Error(`Agent return child tests failed:\n${output}`);
      expect(output).toMatch(new RegExp(`\\b${cases} pass\\b`));
      expect(output).toMatch(/\b0 fail\b/u);
      expect(output).toMatch(new RegExp(`Ran ${cases} tests across 1 file`));
    }, 15_000);
  }
});
