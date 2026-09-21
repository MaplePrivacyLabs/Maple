import path from "path";

const SHARED_AUTH_MODULES = new Set([
  "components/HostedNativeSignInConfirmation.tsx",
  "components/ui/button.tsx",
  "config/openSecretClientConfig.ts",
  "config/openSecretPcrEnvironment.ts",
  "services/appleOAuth.ts",
  "services/desktopOAuthTransport.ts",
  "services/oauthConfig.ts",
  "utils/utils.ts"
]);

/** Fail the dedicated build if a shared import pulls in the application or V1 SDK. */
export function assertAuthBundleIsolation(moduleIds: Iterable<string>, sourceRoot: string): void {
  const prefix = `${path.resolve(sourceRoot).replace(/\\/gu, "/")}/`;
  for (const moduleId of moduleIds) {
    const id = moduleId.replace(/\\/gu, "/").split("?")[0];
    if (id.includes("/@opensecret/") || id.includes("/@opensecret+")) {
      throw new Error("The dedicated auth build must not include the legacy SDK");
    }
    if (!id.startsWith(prefix)) continue;
    const relative = id.slice(prefix.length);
    if (!relative.startsWith("auth-site/") && !SHARED_AUTH_MODULES.has(relative)) {
      throw new Error(
        `The dedicated auth build imported an unapproved application module: ${relative}`
      );
    }
  }
}
