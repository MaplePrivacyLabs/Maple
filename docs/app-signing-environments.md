# App signing environments

App signing and store credentials belong in GitHub **environment secrets**,
not repository or organization secrets available to arbitrary branch workflows.
Branch protection on `master` alone does not protect repository-level secrets.

| Environment | Credentials | Jobs |
| --- | --- | --- |
| `desktop-signing` | `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_ID`, `APPLE_ID_PASSWORD`, `APPLE_TEAM_ID`, `APPLE_PROVISIONING_PROFILE`, `KEYCHAIN_PASSWORD` | macOS/Linux master and release builds |
| `apple-signing` | `APPLE_API_ISSUER`, `APPLE_API_KEY`, `APPLE_API_PRIVATE_KEY`, `APPLE_TEAM_ID` | iOS master/release builds and TestFlight submission |
| `android-signing` | `ANDROID_KEYSTORE_BASE64`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD` | Android master and release builds |
| `windows-signing` | `AZURE_*` signing configuration and both `TAURI_SIGNING_PRIVATE_KEY*` secrets | Windows master and release builds |
| `zapstore-publishing` | `ZAPSTORE_SIGN_WITH` | Existing best-effort publisher after a successful release |

Configure each environment with **selected branches and tags**: branch `master`
and tags `v*`. The existing branch and release-tag rulesets must continue to
protect those refs. Do not add a branch wildcard or allow PR merge refs.
Zapstore runs its trusted workflow from `master` after the release completes.

Normal master builds and release builds remain automatic: these environments
do not add a required-reviewer prompt. Review happens before changes reach the
protected branch or a release tag is created. Unsigned PR builds keep their
existing behavior. Keep the `windows-signing` name unchanged because Azure's
OIDC trust uses that environment identity.

When migrating credentials, configure the environment restrictions and populate
all destination secrets first, update every consuming job, then remove the
repository-level copies. Preserve existing values and secret names. A failed
or incomplete migration must leave the working source credentials intact.
Rerunning an old workflow revision that predates these environment declarations
will no longer have signing access; use a current reviewed revision instead.

`nix flake check --no-update-lock-file` checks workflow syntax and the signing
job boundaries. The manual `Check app signing credentials` workflow checks credential availability
without signing, building, or publishing anything. Run it on `master`; a run from
a feature branch should be rejected by the environment policies.

Server-side environment policies and secret placement must also
be verified through GitHub; a source test cannot enforce repository settings.

This boundary prevents a new, unreviewed branch workflow from reading signing
credentials. It does not make code already admitted to a signing job harmless,
prevent an authorized person from publishing a release, or replace the separate
PCR-signing approval policy.

The shared iOS release script removes App Store Connect signing inputs from
child-process environments before dependency installation, the frontend build,
and the unsigned archive. It decodes an encoded private key only immediately
before the signed archive/export, into an owned temporary directory with mode
700 and a key file with mode 600. Success, failure, and handled signals remove
that directory; the key is already removed before artifact verification. A
caller-supplied `APPLE_API_KEY_PATH` remains caller-owned and is never modified
or deleted by the script.

This limits accidental inheritance and key-file lifetime within the script. It
does not isolate the signer from code running under the same runner account:
the invoking Nix environment and signed Tauri/native build are still trusted,
and an earlier child could remain running. A caller-supplied key already on disk
also remains accessible to that account. Separate signing infrastructure would
be required to establish that stronger boundary. Hermetic release-script tests
exercise canary environments and cleanup; they do not prove real Apple signing
or TestFlight upload.
