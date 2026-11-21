/*
 * Sigstore's production trusted root, pinned at build time.
 *
 * This is the same file the release pipeline pins
 * (scripts/ci/opensecret-release/trusted_root.json); a test asserts the two
 * stay byte-identical. It is loaded on its own, apart from the policy code,
 * so a future trust-root refresh can replace the loader without touching
 * verification. There is deliberately no refresh at runtime today.
 */
import type { TrustedRoot } from "@freedomofpress/sigstore-browser";
import trustedRootJson from "./trustedRoot.json";

export function embeddedTrustedRoot(): TrustedRoot {
  return trustedRootJson as unknown as TrustedRoot;
}
