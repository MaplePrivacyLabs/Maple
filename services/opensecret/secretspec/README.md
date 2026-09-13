# PCR signing, not local runtime or deployment

`pcr-signing.toml` declares the **existing** SDK-trusted signing key in the
`opensecret-pcr-signing` BWS project. Both dev and prod use that same key.
It signs PCR0 text only, not the environment. Separate histories are not
separate cryptographic authorities. A Sigstore transition needs its own client
compatibility plan.

The root `../secretspec.toml` remains the independent local-runtime contract.
AWS, Cloudflare, SSH and rollout state belong to the separate private
deployment automation, not this public repository.

## Operator setup, performed separately

Create a dedicated BWS machine account with read access only to the signing
project. Import the existing key as `signing_private_key` using the BWS UI.
Do not generate, rotate, print, or copy it into dotenv files. Do not use
`secretspec set`/bulk import for this migration: the current CLI-backed BWS
write path can place values in process arguments.

The manifest commits the `opensecret_pcr_signing` alias with the signing
project ID. Store the signing machine token once per operator machine:

```sh
just --no-dotenv pcr-signing-login
just --no-dotenv pcr-signing-check
```

Login uses a hidden prompt and stores the token in the OS keyring under this
alias. Do not reuse a local-runtime or deployment machine token, and do not
export `BWS_ACCESS_TOKEN` for signing: the signing recipes start from an empty
environment and the alias declares no environment fallback. `check --no-prompt`
does not create missing secrets. No shell hook resolves the signing key.

## Authorized signing

Enter the pinned component shell without ambient credentials or stateful hooks:

```sh
env -i HOME="$HOME" PATH="$PATH" \
  OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-update-lock-file '.?submodules=1'
```

On Linux, also pass `XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS` through
`env -i` so the keyring's secret-service bus stays reachable.

Use `just --no-dotenv` for **every operator recipe**, including builds.
The older global dotenv setting remains only for compatibility with local
development. Operator recipes chain through Just dependencies, so one
`--no-dotenv` covers the whole operation, and every command that builds or
touches credentials runs under `env -i` with a tool and keyring allowlist.
Those commands take `python3` from `PATH`; use the pinned shell, whose
interpreter includes `cryptography`.

1. Build with `just --no-dotenv build-eif-dev` (or `build-eif-prod`).
2. Record the full source commit, immutable `result` output, EIF SHA-256 and
   measurements. Review them before authorizing a snapshot/history update.
3. Deliberately copy the reviewed measurements into the environment's snapshot.
4. Run `just --no-dotenv append-pcr-dev` (or `append-pcr-prod`).
5. Run `just --no-dotenv check-pcr-compatibility` and the baseline/publishing
   checks in [the compatibility runbook](../docs/pcr-compatibility.md).

The append recipes validate existing files before lookup, skip already approved
measurements without retrieving a key, and use native SecretSpec only around
`node pcr_sign.js sign-pcr0`. The parent recipe never receives the resolved key.
The existing Python verifier accepts only the resulting public signature,
checks it against the pinned SDK key, and atomically appends the history.
Missing histories are errors, not invitations to start over. Wrong keys or
invalid signatures leave history unchanged.

`update-pcr-dev/prod/all` still combine building, copying and signing. They are
explicit approval operations, not normal development or deployment commands.
Prefer the separated review steps above. Serialize operators; atomic replacement
does not provide a distributed lock across repositories.

Build recipes clear the process environment and never resolve signing secrets.
This is environment hygiene, not a sandbox: a process under the same OS user
may still access that user's keyring and credential files.

## CI signing

The `OpenSecret EIF release` workflow signs with the same key and recipe behind
the protected `pcr-signing` GitHub environment (required reviewer, `master`
only). Its one-time setup, performed by an owner:

- A `pcr-signing-ci` BWS machine account with read access to only the signing
  project, and an access token with an expiry date.
- Environment secret `OPENSECRET_PCR_SIGNING_BWS_ACCESS_TOKEN` holding that
  token. GitHub never holds the key itself: a leaked token is revoked in
  Bitwarden, while a leaked key could not be rotated without a client update.
- Environment variable `OPENSECRET_PCR_SIGNING_KEY_ID`, the UUID of the
  `signing_private_key` item. It is an identifier, not a credential.

Bitwarden's Secrets Manager action resolves the key as a masked step output
that only the signing step receives. `scripts/ci_sign_pcr.sh` then runs the
`append-pcr-*` recipe with `--set pcr_signer 'node pcr_sign.js sign-pcr0'`, so
the key still reaches only the node signer and the verification and atomic
append are unchanged. Laptops keep the keyring alias; never use that override
locally. See [the Nitro runbook](../docs/nitro-deploy.md#eif-release-workflow).

## Deployment handoff

After approved PCR publication, hand the deployment operators the reviewed
source commit, immutable Nix output directory and EIF SHA-256. The deployment
side uses the read-only `scripts/pcr_compatibility.py artifact` command to
check a clean matching checkout, signed approval, measurements and bytes
before remote operations. It does not rebuild or sign. Source provenance and publication at both public
URLs remain operator-reviewed gates; the signature does not authenticate a
commit or the artifact's build provenance.
