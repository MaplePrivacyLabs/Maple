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

The component's `secretspec.toml` declares required keys and process scopes.
It maps `continuum_api_key`, `tinfoil_api_key`, and `kagi_api_key` to the uppercase
environment variables used by the processes. Native SecretSpec commands in
Just handle resolution; no custom runtime helper is needed.

The pinned shell includes SecretSpec 0.20 and BWS. Configure the
`opensecret_local` alias once in your user-level SecretSpec configuration,
substituting your local-development BWS project ID:

```sh
secretspec config global provider add opensecret_local 'bws://YOUR_PROJECT_UUID' \
  --credential access_token=keyring
just local-secrets-login
```

The login command uses SecretSpec's hidden prompt to store the BWS machine
token in Keychain. Give the machine account read access to the intended project.
The project ID is an identifier, not a credential; authentication and BWS
permissions control access. Keep your provider URI and bootstrap binding in
user configuration so other contributors can use their own account or provider.

If this machine already has a BWS login, configure the alias's
`credentials.access_token` with a native Keychain reference instead of logging
in again. This reuses the token without copying it:

```toml
# ~/.config/secretspec/config.toml
[defaults.providers.opensecret_local]
uri = "bws://YOUR_PROJECT_UUID"
credentials = { access_token = { provider = "keyring", ref = { item = "EXISTING_KEYCHAIN_SERVICE" } } }
```

Preserve other aliases when editing this file. Do not replace a working token
unless deliberately rotating it; a shared reference also affects other tools
using that login. macOS may request Keychain access for a new SecretSpec
executable. See the official [provider alias](https://secretspec.dev/reference/cli/#config-global-provider-add)
and [BWS](https://secretspec.dev/providers/bws/) documentation.

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

The generated proxy binary is gitignored.

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

The proxy receives only Continuum; the backend receives Tinfoil and Kagi.
Both select the committed manifest, user-level provider alias, profile and scope
explicitly. The recipes remove inherited BWS bootstrap/config overrides and
use the pinned shell's `bws`; SecretSpec performs key resolution and scope
filtering. The inner Just invocation disables dotenv loading so it cannot
restore excluded keys. Missing provider values fail rather than falling back
to old files or shell keys.

Scopes minimize the child environment; they do not narrow machine-account
permissions. The BWS provider internally lists the selected project's secrets,
then selects the declared keys. The bootstrap token goes to the `bws` subprocess,
not to the application. Processes with access to the same Keychain login can
still access everything its machine account permits.
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
