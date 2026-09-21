import { describe, expect, test } from "bun:test";
import { assertAuthBundleIsolation } from "../../auth-build-boundary";

const root = "/fixture/frontend/src";

describe("dedicated auth bundle boundary", () => {
  test("accepts the dedicated entry and explicitly shared auth dependencies", () => {
    expect(() =>
      assertAuthBundleIsolation(
        [
          `${root}/auth-site/main.tsx`,
          `${root}/components/HostedNativeSignInConfirmation.tsx`,
          `${root}/services/desktopOAuthTransport.ts`,
          "/fixture/node_modules/@mapleai/sdk/dist/index.js"
        ],
        root
      )
    ).not.toThrow();
  });

  test("rejects full app, legacy, chat, billing, and agent imports", () => {
    for (const module of [
      "App.tsx",
      "main.tsx",
      "routeTree.gen.ts",
      "routes/auth.$provider.callback.tsx",
      "legacy/LegacyDesktopOAuthApp.tsx",
      "components/AppleAuthProvider.tsx",
      "billing/billingService.ts",
      "services/agentService.ts",
      "components/Chat.tsx"
    ]) {
      expect(() => assertAuthBundleIsolation([`${root}/${module}`], root)).toThrow();
    }
    expect(() =>
      assertAuthBundleIsolation(["/fixture/node_modules/@opensecret/react-v1/dist/index.js"], root)
    ).toThrow();
  });
});
