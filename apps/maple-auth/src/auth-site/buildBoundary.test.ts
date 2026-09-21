import { describe, expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { assertAuthBundleIsolation } from "../../auth-build-boundary";

const root = "/fixture/apps/maple-auth";

describe("standalone auth bundle boundary", () => {
  test("accepts its own source, registry dependencies and generated helpers", () => {
    expect(() =>
      assertAuthBundleIsolation(
        [
          `${root}/index.html`,
          `${root}/src/auth-site/main.tsx`,
          `${root}/src/components/HostedNativeSignInConfirmation.tsx`,
          `${root}/node_modules/@mapleai/sdk/dist/index.js`,
          `\0${root}/node_modules/react/index.js?commonjs-proxy`,
          "\0vite/modulepreload-polyfill.js",
          "\0commonjsHelpers.js"
        ],
        root
      )
    ).not.toThrow();
  });

  test("rejects sibling applications, local SDK source and parent node_modules", () => {
    for (const module of [
      "/fixture/apps/maple-research/frontend/src/components/ui/button.tsx",
      "/fixture/apps/maple-research/frontend/src/auth-site/main.tsx",
      "/fixture/apps/maple-agent/src/main.tsx",
      "/fixture/sdk/dist/index.js",
      "/fixture/node_modules/@mapleai/sdk/dist/index.js",
      `${root}/../maple-research/frontend/src/main.tsx`,
      `${root}-other/src/main.tsx`,
      "\0/fixture/sdk/dist/index.js?commonjs-proxy"
    ]) {
      expect(() => assertAuthBundleIsolation([module], root)).toThrow("outside its application");
    }
  });

  test("rejects the legacy SDK even if installed inside the app", () => {
    for (const module of [
      `${root}/node_modules/@opensecret/react-v1/dist/index.js`,
      `${root}/node_modules/.bun/@opensecret+react@3.4.1/node_modules/index.js`
    ]) {
      expect(() => assertAuthBundleIsolation([module], root)).toThrow("legacy SDK");
    }
  });

  test("rejects a local link hidden inside its node_modules", () => {
    const directory = mkdtempSync(path.join(tmpdir(), "maple-auth-boundary-"));
    try {
      const app = path.join(directory, "app");
      const sdk = path.join(directory, "sdk");
      mkdirSync(path.join(app, "node_modules", "@mapleai"), { recursive: true });
      mkdirSync(sdk);
      writeFileSync(path.join(sdk, "index.js"), "export {};\n");
      symlinkSync(sdk, path.join(app, "node_modules", "@mapleai", "sdk"));
      expect(() =>
        assertAuthBundleIsolation(
          [path.join(app, "node_modules", "@mapleai", "sdk", "index.js")],
          app
        )
      ).toThrow("outside its application");
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });
});
