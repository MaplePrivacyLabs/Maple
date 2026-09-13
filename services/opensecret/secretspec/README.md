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

Preserve existing aliases when adding this user-level alias:

```sh
secretspec config global provider add opensecret_pcr_signing \
  'bws://SIGNING_PROJECT_UUID' --credential access_token=keyring
just --no-dotenv pcr-signing-login
just --no-dotenv pcr-signing-check
```

Login uses a hidden prompt and a separate keyring credential binding. Do not
reuse a local-runtime or deployment machine token. Project UUID and keyring
bindings stay in user configuration, not this repository. `check --no-prompt`
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

## Deployment handoff

After approved PCR publication, hand the deployment operators the reviewed
source commit, immutable Nix output directory and EIF SHA-256. The deployment
side uses the read-only `scripts/pcr_compatibility.py artifact` command to
check a clean matching checkout, signed approval, measurements and bytes
before remote operations. It does not rebuild or sign. Source provenance and publication at both public
URLs remain operator-reviewed gates; the signature does not authenticate a
commit or the artifact's build provenance.
