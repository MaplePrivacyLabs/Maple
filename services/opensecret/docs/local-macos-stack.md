# Local macOS stack

This runbook covers OpenSecret with the native Continuum proxy and the
in-process Tinfoil Rust SDK. It is separate from Linux/Nitro deployment.
Run backend commands from `services/opensecret/` in the Maple monorepo. Preserve
generated environment files, ports, and process ownership when a workspace
manager already provides this stack; use that manager's lifecycle commands.

```text
Continuum proxy   http://127.0.0.1:8092
OpenSecret API    http://127.0.0.1:3000
Maple             VITE_OPEN_SECRET_API_URL=http://127.0.0.1:3000
```

Tinfoil discovery, attestation, TLS pinning, and requests happen inside the
OpenSecret process; there is no local Tinfoil sidecar or port.

## One-time setup

```sh
git -C ../.. submodule update --init --recursive -- services/opensecret/nitro-toolkit services/opensecret/privatemode-public
OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-write-lock-file '.?submodules=1' -c just build-local-proxies-macos
```

The component's `secretspec.toml` owns the local provider contract. Its `opensecret-local-dev` BWS
project stores `continuum_api_key`, `tinfoil_api_key`, and `kagi_api_key`.
The manifest maps those native names to the uppercase environment variables
used by the processes. BWS project IDs survive display-name changes.

The pinned shell includes SecretSpec 0.20 and BWS. On this VM the manifest
reuses the existing bootstrap token at Keychain service
`secretspec/opensecret-dev-observability/_provider/access_token`; it does not
copy the token, and the token is not injected into either service. On a new
machine, run `just local-secrets-login` in an interactive Nix shell and enter
a BWS machine token with read access to the selected project at the hidden
prompt. Do not repeat login on an already configured machine unless replacing
the token. macOS may request Keychain access for a new SecretSpec executable.

Check access without displaying values or prompting to create missing secrets:

```sh
OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-update-lock-file '.?submodules=1' -c just local-secrets-check
```

Secret retrieval happens only in these explicit commands and the run recipes;
entering the shell and building never retrieve credentials. Values are not
written to `.env` or `.local/secrets`. Workspace-generated JWT/enclave secrets,
database configuration and inter-service authentication remain workspace-owned.
Brave is not part of this local contract.

`just local-secrets-test` runs offline helper tests; `nix flake check` also
runs them without BWS access. The generated proxy binary is gitignored.

Enter `nix develop` once to prepare the local PostgreSQL state and create `.env`
when absent. Review an existing `.env` rather than replacing it, then run:

```sh
just diesel-migration-run-local
```

## Run

Use separate terminals.

Terminal 1:

```sh
OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-write-lock-file '.?submodules=1' -c just run-continuum-proxy-macos
```

Terminal 2:

```sh
nix develop --no-update-lock-file '.?submodules=1' -c just run-local-backend-macos
```

Local backend logs are line-buffered on stdout. Follow that terminal, or the
capturing process's log file (workspace-managed starts use `logs/opensecret.log`).

The proxy recipe resolves only Continuum; the backend recipe resolves only
Tinfoil and Kagi. Both select the committed manifest, provider, profile and
scope explicitly. Ambient BWS/SecretSpec routing and provider keys are cleared;
missing BWS values fail rather than falling back to old files or shell keys.
The backend recipe selects the loopback Continuum base. Any other custom
provider base is a credential boundary; derive
URL and header behavior from current source before supplying credentials.

In Maple, set `apps/maple-research/frontend/.env.local` (relative to the
monorepo root) to use this backend, preserving other existing configuration:

```dotenv
VITE_OPEN_SECRET_API_URL=http://127.0.0.1:3000
```

Follow the selected Maple revision's own `AGENTS.md` and development or
validation skill when present. Browser Research and native Agent Mode are
separate consumers; choose the path that exercises the changed contract.
