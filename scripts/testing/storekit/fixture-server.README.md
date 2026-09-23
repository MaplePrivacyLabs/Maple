# Local StoreKit acknowledgement fixture

This standalone test process acknowledges local StoreKit transactions after
writing and syncing a metadata-only journal. It imports no billing code,
contacts no Apple or Maple service, and grants no Maple entitlement. Its
`payment_provider: "stripe"` response intentionally proves that transaction
acknowledgement does not depend on the selected subscription provider.

Run commands from the Maple repository root in its pinned shell:

```sh
nix develop --no-update-lock-file .#ci -c node --test scripts/testing/storekit/fixture-server.test.mjs
```

For an actual simulator run, choose a new absolute path in an existing test
output directory. Preserve that journal when restarting the server. The CLI
always binds `127.0.0.1:38863`; a port conflict fails startup.

```sh
nix develop --no-update-lock-file .#ci -c node scripts/testing/storekit/fixture-server.mjs \
  --local-fixture --bootstrap-only --journal /absolute/test-output/acknowledgements.jsonl
```

Bootstrap mode provides the fixture account token but refuses every transaction
submission with HTTP 503. Obtain the **public ES256 certificate** from a
native-verified StoreKit 2 test transaction using the native test harness, then
stop this exact process and restart with that operator-selected certificate:

```sh
nix develop --no-update-lock-file .#ci -c node scripts/testing/storekit/fixture-server.mjs \
  --local-fixture --certificate /absolute/test-output/storekit2-test.cer \
  --journal /absolute/test-output/acknowledgements.jsonl
```

`--bootstrap-only` and `--certificate` are mutually exclusive. A transaction
request cannot supply a trust anchor. Verification requires an exact match with
the pinned self-signed P-256 certificate, its validity window, and a valid ES256
JWS signature. The RSA `StoreKitTestCertificate.cer` shipped in Xcode's
`IDEStoreKitEditor` resources is a receipt certificate and is deliberately
rejected here. Neither Xcode's receipt certificate nor an arbitrary certificate
from a network request establishes trust in a StoreKit 2 JWS.

All requests require `X-Maple-StoreKit-Fixture: 1`. Native requests may omit
`Origin`; renderer requests must use exactly `tauri://localhost` or
`http://tauri.localhost`. Host must be the listener's `127.0.0.1` address.
JSON request bodies are limited to 64 KiB. The fixed fixture account token is
`11111111-1111-4111-8111-111111111111` and is not a credential for any real user.

| Method and path | Body | Result |
| --- | --- | --- |
| `GET /v1/maple/subscription/apple/account-token` | None | `{ "app_account_token": "11111111-1111-4111-8111-111111111111" }` |
| `POST /v1/maple/subscription/apple/transactions` | `{ "signed_transaction": "<JWS>" }` | `{ "acknowledged_transaction_id": "<exact string ID>", "payment_provider": "stripe", "fixture_only": true }` |
| `POST /__test__/mode` | `{ "mode": "unavailable" }` or `{ "mode": "ok" }` | Set a failure before acknowledgement |
| `GET /__test__/status` | None | Fixture mode and acknowledged transaction metadata |

Control routes additionally require `X-Maple-Fixture-Control: storekit-harness`
and **no Origin**, so the renderer cannot change test controls. These headers
separate test traffic and protect against ordinary browser-origin requests;
they do not authenticate same-user native processes.

The host-only status response includes `last_rejection`, initially `null`. A
failed transaction POST sets only its fixed `{ status, code }` diagnostic; an
unexpected internal failure uses `{ status: 500, code: "fixture_internal_error" }`.
A successful transaction acknowledgement clears it. Other routes do not change
it, and it is never persisted. No request values or raw error messages are kept.
An absent `appAccountToken` returns `409 missing_account_token` unless its exact
verified original transaction ID is already in this fixture's durable journal.
Null, invalid, or nonmatching values always return `409 wrong_account_token`,
even for a known original ID.

Only `environment: "Xcode"`, bundle `cloud.opensecret.maple`, and these product
IDs are accepted. Account ownership requires the fixture token or the existing
original transaction lineage described below:

- `cloud.opensecret.maple.pro.monthly`
- `cloud.opensecret.maple.max.monthly`
- `cloud.opensecret.maple.pro.yearly`

Transaction and original transaction IDs must be decimal strings and are never
converted to JavaScript numbers. The journal retains only those IDs, product ID,
and `Xcode` environment. It never retains JWS, signatures, account tokens, or
real entitlement state. Every new record is fsynced before acknowledgement;
startup also fsyncs the validated journal and its parent directory before
serving requests, including when the journal already exists.
Duplicates reuse the durable record, and conflicting identities fail. A damaged
or incomplete journal fails startup. Preserve it as failure evidence and use a
new run output directory; this process has no destructive reset route.

StoreKit management can omit the account token on later transactions. A first
acknowledgement for an original transaction ID must carry the fixture token;
later verified transactions with that exact original ID may omit it, including
product changes and after a fixture restart. The journal is trusted local test
state and must belong to the same test run. An unknown original ID is never
inferred from a product or transaction ID. The standalone verifier keeps token
validation strict unless explicitly given a known-lineage predicate. These
rules acknowledge local fixture transactions and do not grant real entitlements.

Synthetic cryptographic tests exercise the server boundary with ephemeral
test-only signing keys. They do not establish simulator integration, App Store
sandbox behavior, production certificate verification, authentication, or Maple
billing correctness.
