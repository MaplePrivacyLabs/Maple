# Development environments and authentication

Choose the environment for the task before setup, implementation, or smoke
testing. Keep the chosen lane through API, client/project ID, auth origin,
account, billing/flags, and runtime configuration.

| Environment | Use | Required setup/evidence |
| --- | --- | --- |
| Local | Any backend change, heavy feature, or specific frontend/backend, backend-log, or billing interaction | Isolated linked services and migrated local state; selected API/project/client values; local account path; relevant client/runtime smoke. |
| Hosted Dev | Small frontend work where hosted behavior is sufficient | Explicit Dev API and trust policy, valid Dev account and supported login route; name integrations not exercised. |
| Prod | Intentional production validation or operations | Explicit scope and authority; production account/services and separate live evidence. |

An external workspace may own ignored environments, ports, generated account
fixtures, and services. Follow its configuration/lifecycle instructions and
consume its generated values; do not recreate them with standalone defaults.
Otherwise use the component's public setup guide. Do not silently switch to
hosted services when an isolated Local integration is required.

For a Local billing-dependent scenario, the stack owner must provide the local
billing API and matching inter-service authentication/account fixtures. The
standalone OpenSecret setup covers its backend/providers, not another service's
implementation. If that dependency is unavailable, report the billing boundary
blocked rather than substituting hosted billing and calling it Local proof.

## Verify the effective client configuration

Record public configuration only; never print environment dumps or credentials:

- Actual OpenSecret API origin and client/project ID.
- Trust/attestation policy and selected SDK source/version.
- Login method, browser auth origin, and native callback identity when used.
- Billing/flags API origins only when the changed behavior uses them.
- Exact app/build/runtime and disposable account class.

Inspect the values the running/building client consumes, not only a shell
variable or an example file. Different clients have separate configuration.
Research uses `apps/maple-research/frontend/.env.local` in its development
runtime; the GPUI Agent does not load dotenv. A new client must define its own
configuration adapter rather than assuming either contract applies.

A debug build or iOS simulator can still use hosted services. Research's PR
packaging scripts deliberately ignore local dotenv and compile hosted Dev
profiles. [Maple Dev TestFlight](ios-dev-testflight.md) is a signed Research
distribution channel with fixed hosted services. Neither establishes Local
backend integration. Use the configured development runtime for Local smoke,
and fixed PR packages for compile/package evidence.

## Local baseline authentication

Use disposable accounts through the supported encrypted password/signup/login
client path. If a workspace supplies account fixtures, use its instructions and
the account/project generated for that workspace. For standalone setup, use
the SDK's supported encrypted account APIs and the selected local project.
Do not bypass auth or fabricate session/token storage to make a UI test pass.

Confirm the account login against the actual local API before testing dependent
behavior. A backend health response proves liveness; successful provider-secret
resolution proves secret access. Neither proves client configuration, user
authentication, billing, or the changed feature. Preserve fixture scope and
local credential state; avoid broad Keychain/browser storage deletion.

Protected OpenSecret routes require an SDK or encrypted application client.
The SDK local mock-attestation exception is limited to supported loopback
origins; an iPhone using a Mac's LAN HTTP address is a different transport
problem. Start local mobile validation on a supported simulator path and
verify effective reachability/configuration. Physical-device networking or
an attestation-policy change requires a separate supported design; do not
disable verification to make it work.

## Hosted Dev and intentional OAuth checks

Use an account valid for the selected Dev API/project and a supported login
path. Verify account login before dependent UI validation; a production account
or retained session from another origin is not a Dev-account setup. Account
provisioning/access belongs to the environment owner. Ordinary small frontend
work can use the supported password flow without introducing a provider-login
rehearsal. When OAuth/native handoff is the changed boundary, explicitly select
its hosted environment and test the real provider/callback/native path.

### Research hosted Dev browser loop

If a workspace provides a hosted Dev launcher, use it. For a standalone browser
process, choose an agreed unused port and run this from the monorepo root:

```sh
MAPLE_BROWSER_PORT=5175 nix develop --no-update-lock-file .#ci -c bash -c '
  set -euo pipefail
  source scripts/ci/_common.sh
  use_pr_environment
  export MAPLE_IGNORE_VITE_ENV_FILES=1
  cd apps/maple-research/frontend
  bun --no-env-file run dev --host 127.0.0.1 --port "$MAPLE_BROWSER_PORT" --strictPort
'
```

Replace `5175` with that port and inspect its listener before launch; preserve
other processes and track this process through its own terminal/lifecycle.
`use_pr_environment` clears inherited `VITE_*` values and selects the checked-in
Dev profile: `https://enclave.secretgpt.ai`, development PCR trust, public
project `ba5a14b5-d915-47b1-b7b1-afda52bc5fc6`, and the Dev flags/billing APIs.
`bun --no-env-file` prevents Bun dotenv loading;
`MAPLE_IGNORE_VITE_ENV_FILES=1` selects the existing Vite bypass for frontend
dotenv. This does not edit generated `.env.local` or the workspace's Local
reservations. The package's `predev` script prepares locked dependencies.

Open `http://127.0.0.1:<port>/login` and use **email/password** for an account
valid in that Dev API/project. The password-login browser origin is this running
frontend; no hosted Auth site is needed for this loop. Verify the effective
configuration and account before testing the changed state. If Dev rejects the
account or browser origin, report that blocker rather than changing callbacks,
substituting a production account, or treating OAuth as a setup workaround.
This command is source-backed configuration guidance, not proof of live Dev
availability or successful account login.

For PR artifact browser smoke instead, use
[the fixed Dev web build and preview procedure](../.agents/skills/validate-maple/references/automated-checks.md#web-production-build).
That validates the produced artifact and is distinct from this interactive loop.

In current Research, native OAuth's API URL and browser auth origin are separate:
`startNativeOAuth` prepares against the configured API, while
`buildTransportV2DesktopAuthUrl` selects `/desktop-auth` from the app variant's
auth origin. The production variant uses `https://trymaple.ai`; Dev requires
`VITE_MAPLE_DEV_AUTH_ORIGIN` and its hosted handoff accepts only the fixed
development backend. Pointing `VITE_OPEN_SECRET_API_URL` at Local does **not**
localize native OAuth. The Dev auth-origin parser requires canonical HTTPS;
setting it to a loopback HTTP URL is not a supported local OAuth setup.

Research keeps its built-in auth. The independent
[Auth app](../apps/maple-auth/README.md) has its own SDK pin, configuration,
routes, and publisher. Follow source and the applicable component guide for
actual callback support rather than inferring it from a hostname.

Report local password fixtures, hosted Dev login, provider OAuth, native handoff,
and production behavior as separate evidence. No source/build/unit-test result
proves deployed provider configuration or a real login flow.
