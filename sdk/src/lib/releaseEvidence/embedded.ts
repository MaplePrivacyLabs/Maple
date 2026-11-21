import type { PcrEnvironment } from "../pcr";
import devB422d5fc from "./published/dev-b422d5fc.sigstore.json?raw";

/*
 * Release bundles shipped with the SDK, so a session can verify its release
 * without network access. Each file under published/ is the exact text the
 * bundle host served, kept out of the formatter, with its SHA-256 in the
 * .sha256 file beside it; tests check both. A measurement that is not listed
 * here is still verified through a runtime fetch.
 */
export interface EmbeddedBundle {
  environment: PcrEnvironment;
  /** Lowercase hex PCR0, 96 characters. */
  pcr0: string;
  /** The published bundle text, verified as it is. */
  bundleText: string;
}

export const EMBEDDED_BUNDLES: readonly EmbeddedBundle[] = [
  {
    environment: "development",
    pcr0: "b422d5fcb25929a1b76f0eb8aa10c26a78c635a86a58039b48f22eb0dc4dc6f8ae6e646aa2fbbc57db756e6f06cad3dc",
    bundleText: devB422d5fc
  }
];

export function findEmbeddedBundle(
  environment: PcrEnvironment,
  pcr0: string,
  bundles: readonly EmbeddedBundle[]
): EmbeddedBundle | undefined {
  return bundles.find((entry) => {
    const sameEnvironment = entry.environment === environment;
    const samePcr0 = entry.pcr0 === pcr0;
    return sameEnvironment && samePcr0;
  });
}
