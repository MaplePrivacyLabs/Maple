import type { PcrEnvironment } from "../pcr";

/*
 * Maple's release-approval public keys, one per environment, shipped with the
 * SDK. These are the only keys that can approve a release; nothing fetched
 * from the network, carried in a bundle or listed in a snapshot can add or
 * replace one.
 *
 * The Dev key is the signing pipeline's reviewed key; a test compares it with
 * scripts/ci/opensecret-release/dev-signing-key.pem. Production has no key
 * yet, so Production verification fails as "approval-key-not-configured" and
 * never falls back to the Dev key.
 */

const DEV_APPROVAL_KEY_PEM = `-----BEGIN PUBLIC KEY-----
MHYwEAYHKoZIzj0CAQYFK4EEACIDYgAE2IpJwjrA1Uuk4p8F3B48EFZ16K1vwRrj
s3YrvgrucBNiHrSxDFyg0aVSHeuWlRXamXRW9UWxol/c6m/cu1St8sjBm1nUvSK0
Bh96GUQzovL4yGvEYriCf3BY0m2+GtZv
-----END PUBLIC KEY-----
`;

/** The environment's approval key as SPKI PEM, or undefined when it has none. */
export function approvalKeyPem(environment: PcrEnvironment): string | undefined {
  switch (environment) {
    case "development":
      return DEV_APPROVAL_KEY_PEM;
    case "production":
      return undefined;
  }
}
