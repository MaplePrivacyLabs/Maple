# Recovery Credential Architecture Plan

## Status

- Proposal for implementation and review.
- Scope: preserve the existing user seed during password recovery by using one pre-enrolled, user-held recovery code per user.
- Source baseline: repository `master` at `34bea4f`.
- Evidence: source inspection and the [Credential-Bound Recovery field guide](https://opensecret-recovery-field-guide.anthony599874.chatgpt.site/). No runtime, deployed Nitro, KMS-policy, SDK, or Maple behavior was verified for this plan.

## Decisions

The v1 design uses these agreed constraints:

1. Each user has at most one recovery code and one recovery-wrapped seed.
2. Recovery fields live on the existing `users` row rather than in a separate credential table.
3. The recovery code does not contain a public credential UUID. Email plus project identifies the user and therefore the single recovery envelope.
4. The server generates 256 random bits for each recovery code and encodes them in a versioned, checksummed format that is visibly distinct from BIP-39.
5. Enrollment activates the generated code immediately. The user is not required to re-enter or confirm it, matching the current treatment of the generated user seed.
6. Successful recovery proves the current code by opening the current recovery envelope, preserves the exact existing seed, installs the new password wrap, generates a replacement recovery code, and atomically overwrites the old recovery envelope.
7. The replacement recovery code is returned once in the encrypted success response and is immediately authoritative. The user is responsible for retaining it.
8. Recovery remains reset-scoped. It is not an ordinary login credential, JWT authentication method, or second factor for routine login.
9. The existing destructive reset remains an explicit fallback for users who did not enroll recovery or lost both their password and recovery code.
10. A malicious rollback of the entire database to an internally consistent historical snapshot is outside the v1 threat model. The generation counter protects against ordinary races and stale writes, not full-database rollback.

## Executive summary

OpenSecret currently stores a stable 12-word BIP-39 mnemonic in credential-bound AES-256-GCM seed envelopes. Password login verifies the Argon2 password verifier inside the enclave, computes an enclave-keyed authentication binding from the verified credential facts, and only issues tokens after a password-bound envelope opens. OAuth uses the same envelope pattern in a separate credential domain.

The existing forgotten-password flow cannot recreate the password authentication binding and therefore cannot open the old seed envelope. It intentionally generates a new seed, deletes old seed wraps and seed-key-encrypted data, disconnects OAuth, and installs a new password wrap. That behavior is safe but destructive.

Add one recovery envelope to the user record. The recovery code materially participates in deriving a recovery-specific AEAD key. The envelope is bound through canonical authenticated context to the project, user, recovery generation, format version, and wrapping version. PostgreSQL remains an untrusted carrier: a row is accepted only when trusted enclave code reconstructs the expected context and successfully opens the envelope.

## Goals

- Preserve the exact existing mnemonic bytes and all seed-key-encrypted user data after successful recovery.
- Continue treating PostgreSQL values as untrusted candidates rather than credential proof.
- Prevent cross-user, cross-project, cross-generation, and cross-version substitution.
- Keep recovery authority narrower than password or OAuth authentication.
- Support opt-in enrollment for existing users and post-registration enrollment for new users.
- Rotate the recovery code after every successful use by overwriting the single stored envelope atomically.
- Bound recovery code, ciphertext, and cryptographic work for every request.
- Avoid storing the recovery code, seed, derived keys, or plaintext-equivalent verifier in PostgreSQL, logs, analytics, URLs, email, or public AAD.
- Roll out additively for existing users and independently released clients.

## Non-goals

- Recover an account that did not enroll recovery before losing all existing credentials.
- Supporting multiple simultaneous recovery codes in v1.
- Confirm that the user copied or retained a newly displayed recovery code.
- Make PostgreSQL available, durable, deletion-resistant, or resistant to a complete historical snapshot rollback.
- Protect against compromise of an approved enclave image, the OpenSecret enclave root, the effective KMS policy, or the user's recovery code.
- Turn recovery into a normal login method or second factor.
- Backfill recovery envelopes in SQL or startup migrations. Existing rows cannot be rewrapped without access to the user-owned seed.
- Replace the existing email reset challenge in the first release.

## Current implementation trace

### Enclave root and encryption primitives

- `src/main.rs::get_or_create_enclave_key` and `resolve_enclave_key_material` load or create the 32-byte OpenSecret `enclave_key`. The persisted copy is KMS-encrypted; plaintext is held in `AppState`.
- `src/encrypt.rs` provides HKDF-SHA256 derivation, AES-256-GCM helpers, random 96-bit nonces, and `CanonicalBytes` type-and-length framing.
- `src/seed_wrapping.rs` derives purpose-specific MAC and seed-wrap keys from the enclave root. Current domains include password authentication, OAuth authentication, credential lookup, password reset codes, and seed wrapping.
- The current seed envelope is `nonce || ciphertext || tag`. Its AAD canonically includes the user UUID, project ID, credential kind, wrapping version, and authentication binding.

### Seed generation and key derivation

- `src/private_key.rs::generate_twelve_word_seed` obtains 16 enclave-random bytes and encodes them as a 12-word BIP-39 mnemonic. The mnemonic string bytes are the encrypted payload.
- `plaintext_user_seed_to_key` parses the mnemonic, calls BIP-39 with an empty passphrase, and derives the secp256k1 key through BIP-32. Optional BIP-85 derivation creates child mnemonics.
- Preserving the exact plaintext mnemonic preserves the user's derived identity and access to existing seed-key-encrypted data.

### Registration

- `POST /register` is a public-auth route carried over a live OpenSecret encrypted session.
- `AppState::register_user` creates an Argon2 PHC verifier, encrypts it under the enclave key, generates the mnemonic, and calls `create_user_with_password_seed_wrap`.
- User creation and the initial password seed wrap commit in one transaction. The new wrap is opened and compared with the intended seed before persistence.
- The route then performs normal password login and only returns tokens after password verification and seed unwrap succeed.

### Password login and tokens

- `POST /login` verifies the encrypted Argon2 verifier in `AppState::authenticate_user`.
- Only after password verification does `compute_password_auth_binding` bind project, user, identifier kind, normalized identifier, and the exact verifier under an enclave-derived HMAC key.
- `decrypt_seed_for_auth_context` loads candidate wraps for the authoritative user and credential kind and accepts only a candidate that opens under the reconstructed key and AAD.
- Access and refresh JWTs carry a signed v2 `AuthContext`. JWT middleware and `/refresh` require that the signed binding still opens an active seed wrap.

### Password change

- `POST /protected/change_password` requires a valid JWT, encrypted session, and current-password reauthentication.
- `AppState::update_user_password_and_seed_wrap` opens the existing seed through the signed current context, creates a new password verifier and wrap over the same seed, and returns a new authentication context.
- `PostgresConnection::update_user_password_and_seed_wrap` compare-and-swaps the old encrypted verifier, deletes old password wraps, and inserts the replacement in one transaction. This is the nearest existing pattern for preserving recovery.

### Forgotten-password reset

- `POST /password-reset/request` stores a client challenge digest and an enclave-keyed MAC of an emailed eight-character code. The challenge expires after 24 hours.
- `POST /password-reset/confirm` verifies the email code and separate client secret, but neither value can open the existing password seed wrap.
- `AppState::confirm_password_reset` therefore generates a new mnemonic and new password wrap.
- `PostgresConnection::complete_destructive_password_reset` locks the user, consumes reset requests, deletes seed wraps, OAuth connections, and seed-key-encrypted storage roots, updates the password, and inserts the new wrap atomically.

### Current test foundations

Existing tests already cover many required primitives and lifecycle properties:

- Seed-wrap round trips and ciphertext/AAD mutation rejection.
- Cross-user, cross-project, credential-kind, password-verifier, and OAuth-subject substitution.
- New-envelope open-and-compare before persistence.
- Password change preserving the seed and invalidating old authentication context.
- Destructive reset rotating the seed and deleting seed-key-encrypted data.
- Password-change/reset races and copied reset-row rejection.

Relevant files are `src/seed_wrapping.rs`, `src/crypto_property_tests.rs`, `src/aead_db_tamper_tests.rs`, and `src/security_invariants.rs`.

## Threat model and security claim

### Assumed attacker capabilities

The storage attacker may read, insert, copy, modify, relabel, reorder, replay, or delete individual PostgreSQL values and rows. The attacker may observe ordinary host-visible metadata and trigger public recovery endpoints. The design assumes a copied database is available for offline analysis.

The trusted computing base remains the approved enclave code, the plaintext OpenSecret enclave root available to that code, the effective attestation-aware KMS policy, cryptographic libraries, and credential-verification logic. The client must correctly attest the enclave and protect the displayed recovery code.

### Intended narrow claim

Given approved enclave code, the intended KMS policy, and an uncompromised enclave root, manipulating individual database values or moving a recovery envelope between users, projects, generations, or versions must not cause it to open or authorize an ordinary authenticated session.

### Explicit limitations

- Deletion remains denial of service.
- AEAD authenticates a formerly valid envelope but does not establish that it is the newest envelope.
- A complete rollback of the user row and related state to an internally consistent historical snapshot is outside the v1 threat model.
- The generation counter detects ordinary stale concurrent operations. It does not independently prevent a malicious full-database rollback because the counter and matching old envelope could be restored together.
- Possession of the recovery code plus the other required reset factors authorizes account recovery. Theft is severe even though the code is not the mnemonic itself.
- Local source and tests do not prove deployed PCRs, live KMS/IAM policy, artifact identity, network placement, or log retention.

## Recovery code format

Generate a 32-byte secret using `generate_random_enclave::<32>`. Encode it using a versioned, checksummed representation such as:

```text
OSRC1-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX
```

- `OSRC1` is a fixed format marker, not a cryptographic domain.
- The payload encodes all 256 random bits using Crockford Base32 or another reviewed non-BIP39 encoding.
- A versioned checksum detects transcription mistakes. It adds no entropy.
- Group encoded characters in fixed-width blocks for display and printing.
- Parsing accepts ASCII case-insensitively and ignores only ASCII hyphens and spaces. Reject all other characters, Unicode lookalikes, wrong lengths, unknown versions, and checksum failures before database work.
- Canonical output is uppercase with fixed grouping. Persist only decoded bytes transiently in enclave memory, never user-supplied text.

The representation must be visibly distinct from the lowercase, space-separated 12-word BIP-39 mnemonic. Do not use the BIP-39 word list, a mnemonic-compatible word count, or the label "seed phrase." Product copy should call it an "OpenSecret recovery code."

The server generates the value. User-chosen recovery phrases are out of scope. HKDF does not increase human-secret entropy, and Argon2 is unnecessary for a uniformly random 256-bit code.

### Secret handling

- Accept the recovery code only in an encrypted request body, never in paths, query strings, headers, emails, or telemetry.
- Return it only in an encrypted response immediately after enrollment or successful recovery.
- Redact complete request and response payloads at every logging layer.
- Use `zeroize`/`Zeroizing` for parsed secret bytes, derived recovery keys, and temporary formatted strings where practical.
- Apply strict request-string and decoded-length limits before derivation or database work.

## Cryptographic construction

### Separate recovery domains

Do not reuse password, OAuth, reset-code, lookup, authentication-binding, or general seed-wrap labels. Reserve and test exact ASCII labels before implementation:

```text
os.recovery-wrap-root.v1
os.recovery-wrap-aead-key.v1
os.recovery-wrap.v1
```

The recovery code is high entropy and does not need to become the existing `AuthBinding` or JWT `AuthMethod`:

```text
recovery_wrap_root = HKDF-SHA256(
    ikm = enclave_root,
    salt = none,
    info = "os.recovery-wrap-root.v1"
)

recovery_wrap_key = HKDF-SHA256(
    ikm = recovery_wrap_root,
    salt = recovery_secret_32_bytes,
    info = canonical(
        "os.recovery-wrap-aead-key.v1",
        project_id,
        user_uuid,
        recovery_generation,
        format_version,
        wrapping_version
    )
)

recovery_aad = canonical(
    "os.recovery-wrap.v1",
    project_id,
    user_uuid,
    recovery_generation,
    format_version,
    wrapping_version
)

envelope = AES-256-GCM.seal(
    recovery_wrap_key,
    exact_seed_bytes,
    recovery_aad,
    fresh_random_96_bit_nonce
)
```

The implementation may refine the HKDF input layout after cryptographic review, but it must preserve separate labels, canonical field boundaries, and the rule that both the enclave root and presented 256-bit secret are necessary. The raw recovery secret must not be stored in AAD or any database lookup/hash column.

### Generation and versions

These values serve different purposes:

- `format_version` defines recovery code parsing and display rules.
- `wrapping_version` defines the envelope algorithm, derivation, and canonical AAD fields.
- `recovery_generation` identifies the current user-specific instance. It increments whenever the recovery code is enrolled, rotated, revoked, or replaced after use.

Routine rotation increments generation while normally retaining the same format and wrapping versions. The generation must be at least `BIGINT`/`u64`, server-controlled, and included in key context and AAD. It is not an AEAD nonce; every encryption still receives a fresh random GCM nonce.

For v1, generation protects transactional compare-and-swap and stale-operation detection. Full consistent database rollback is explicitly outside the threat model.

### Envelope layout and ciphertext bounds

Follow the existing seed-wrap representation:

```text
12-byte nonce || ciphertext || 16-byte GCM tag
```

Do not add an embedded magic value or self-describing header in v1. Recovery-specific key derivation, trusted AAD, the dedicated user column, and `recovery_wrapping_version` provide the necessary format and domain separation while remaining consistent with current code.

Define a small maximum envelope size from the maximum canonical mnemonic length plus nonce and tag. A normal wrapped 12-word mnemonic is only a few hundred bytes. Reject oversized `recovery_seed_enc` values before copying or decrypting them. The SQL check and Rust constant must use the same reviewed bound.

## Persistence design

Add nullable recovery columns to `users` through a new timestamped Diesel migration:

```sql
ALTER TABLE users
    ADD COLUMN recovery_generation BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN recovery_format_version SMALLINT,
    ADD COLUMN recovery_wrapping_version SMALLINT,
    ADD COLUMN recovery_seed_enc BYTEA,
    ADD COLUMN recovery_enrolled_at TIMESTAMPTZ;
```

Add constraints equivalent to:

```sql
CHECK (recovery_generation >= 0)

CHECK (
    (recovery_seed_enc IS NULL
        AND recovery_format_version IS NULL
        AND recovery_wrapping_version IS NULL
        AND recovery_enrolled_at IS NULL)
    OR
    (recovery_seed_enc IS NOT NULL
        AND recovery_generation > 0
        AND recovery_format_version IS NOT NULL
        AND recovery_wrapping_version IS NOT NULL
        AND recovery_enrolled_at IS NOT NULL
        AND octet_length(recovery_seed_enc) BETWEEN <minimum> AND <reviewed maximum>)
)
```

No recovery secret, deterministic secret digest, credential UUID, pending state, or historical credential row is stored. Nullable envelope fields mean recovery is not enrolled. Revocation clears the envelope/version/timestamp fields and increments generation.

Update `src/models/users.rs`, `src/models/schema.rs`, database queries, and the encrypted-schema classification in `src/security_invariants.rs`. Destructive reset and account deletion must clear/remove recovery state.

### Why columns instead of a table

V1 supports exactly one recovery code, immediate activation, and replacement by overwrite. There is no pending credential, confirmation state, history, or independent multi-code revocation. Those constraints make recovery state a one-to-one property of the user, so columns are the smaller design.

Move to a dedicated table only if requirements later add multiple simultaneous codes, labels, per-code revocation, pending confirmation, recovery history, or independent expiry.

## API design

All routes use the existing attested encrypted session protocol. Recovery codes exist only inside decrypted request/response payloads. Paths and payloads are proposed contracts requiring SDK and Maple review.

### Authenticated status

```text
GET /protected/recovery-code
```

Authorization:

- User access JWT only; API-key context must never authorize recovery management.
- The JWT's signed `AuthContext` must still open an active password or OAuth seed wrap.

Encrypted response:

```json
{
  "enrolled": true,
  "enrolled_at": "RFC3339 timestamp or null"
}
```

Never return generation, code fragments, envelope bytes, or secret-equivalent metadata.

### Enroll or rotate

```text
POST /protected/recovery-code
```

Authorization:

- Valid user JWT and live encrypted session.
- Require recent step-up authentication. For password users, verify the current password. For OAuth users, require a fresh provider reauthentication or defer OAuth-only enrollment until such proof exists.
- Do not accept API keys.

Operation:

1. Open the exact existing seed through the signed current `AuthContext`.
2. Generate a new 256-bit recovery code and next generation.
3. Create the recovery envelope and immediately open it under reconstructed trusted context.
4. Compare the decrypted bytes exactly with the existing seed.
5. Lock the user row and revalidate the observed password/credential state and recovery generation.
6. Atomically overwrite the recovery columns with the new envelope, versions, generation, and timestamp.
7. Return the formatted code once. It is active immediately; no confirmation endpoint or pending state exists.

Encrypted response:

```json
{
  "recovery_code": "OSRC1-...",
  "enrolled_at": "RFC3339 timestamp"
}
```

If the response is lost after commit, the new code remains authoritative and cannot be retrieved. The authenticated user can enroll again while they still have ordinary account access.

### Revoke

```text
DELETE /protected/recovery-code
```

Require the same step-up policy as enrollment. Lock the user, verify the expected generation, increment it, clear the envelope/version/timestamp fields, and return generic success. Revocation must not alter password/OAuth seed wraps or user data.

### Recovery-preserving reset

```text
POST /password-reset/recovery/confirm
```

No JWT is required, but a live encrypted session is mandatory. Require:

- Project `client_id` and project-scoped email.
- Existing emailed reset code.
- Existing client reset challenge secret.
- Current recovery code.
- New password.

Request:

```json
{
  "email": "user@example.com",
  "alphanumeric_code": "...",
  "plaintext_secret": "...",
  "recovery_code": "OSRC1-...",
  "new_password": "...",
  "client_id": "uuid"
}
```

The email challenge proves access to the reset channel; the recovery code supplies the missing cryptographic authority to open the existing seed. Recovery alone does not issue an ordinary JWT.

Public failures collapse to one sanitized response, such as HTTP 400 with the existing `Bad Request` shape. Do not reveal whether the account exists, recovery is enrolled, the reset factor failed, or AEAD authentication failed.

On success, return new tokens and the replacement code in the encrypted response:

```json
{
  "message": "Password reset successful",
  "access_token": "...",
  "refresh_token": "...",
  "recovery_code": "OSRC1-..."
}
```

The replacement is immediately active and the prior code no longer works. The UI must clearly require the user to replace the old stored code with this new value. No re-entry confirmation is required in v1.

### Destructive fallback

Keep `POST /password-reset/confirm` destructive for backward compatibility. New clients must warn that it creates a new cryptographic identity and erases seed-key-encrypted data. Never silently fall back from failed recovery to destructive reset; require a separate user action and request.

## Preserving-reset transaction

Prepare cryptographic outputs before the transaction, but treat database state as provisional until revalidated under the user-row lock:

1. Resolve the user by email and project without exposing the result publicly.
2. Verify reset-code MAC, reset expiry, and client challenge digest.
3. Read the single recovery envelope, versions, and generation from the selected user row.
4. Strictly parse the submitted recovery code.
5. Open the recovery envelope with trusted project/user context and expected generation/version.
6. Validate the recovered bytes as the existing BIP-39 mnemonic. Never generate a seed on unwrap failure.
7. Create a new Argon2 verifier and password envelope over the exact recovered bytes. Open and byte-compare the password envelope.
8. Generate a new 256-bit recovery code, increment generation, and create a replacement recovery envelope over the same bytes. Open and byte-compare it.
9. Begin one PostgreSQL transaction and lock the user row `FOR UPDATE`.
10. Recheck the reset request is unused and unexpired, recovery envelope/version/generation still match the observed state, and encrypted password state has not changed.
11. Atomically consume the selected reset request and all other active reset requests.
12. Delete every password-kind seed wrap and insert exactly one new password wrap.
13. Update `users.password_enc` and overwrite all recovery columns with the replacement envelope, new generation, versions, and timestamp.
14. Apply the secondary-credential policy below.
15. Commit.
16. Verify the committed new password context opens the seed before issuing tokens. If this fails, return a generic internal error and raise a sanitized high-severity operational event; never run destructive fallback.
17. Return the new tokens and replacement recovery code in the encrypted response.

Any failure before commit leaves reset state, password, recovery envelope, seed wraps, and user data unchanged. A response loss after commit leaves the new password usable; after login, the user can rotate recovery again if the returned code was lost.

## Secondary credential policy

Recovery is an account-takeover-sensitive event. V1 should default to:

- Delete all OAuth connections and OAuth seed wraps during recovery, requiring later re-linking.
- Delete all old password seed wraps before inserting the replacement, invalidating old password access and refresh JWTs.
- Consume all password reset requests.
- Preserve all seed-key-encrypted user data because the seed is unchanged.
- Revoke all user API keys unless product review establishes a narrower safe policy. The current destructive reset preserves API keys, but preserving recovery should not automatically retain bearer credentials after possible account compromise.
- Leave transport sessions alone because they confer no identity and are not reliably associated with a user.

These choices must be user-visible and tested. If product chooses to preserve OAuth links or API keys, document the threat model and provide post-recovery account activity review.

## New-user and existing-user enrollment

### New password users

Do not add the recovery code directly to the existing `/register` response in the first implementation. Registration already commits the account and then performs login and email-verification work. Changing that contract would complicate compatibility and partial-success behavior.

Updated clients should prompt for recovery enrollment immediately after registration/login using `POST /protected/recovery-code`. The code becomes active when the enrollment transaction commits and is shown once.

### Existing password users

Enrollment is opt-in and lazy. A valid current JWT plus step-up password verification lets the enclave open the existing seed and create the recovery envelope. No SQL backfill is possible or required.

### OAuth-only and guest users

- OAuth-only enrollment requires a defined fresh-provider proof. Do not treat an old OAuth JWT as sufficient step-up without review. Launch password-user enrollment first if provider proof is not ready.
- Guest accounts have no email reset channel. They cannot use this forgotten-password flow until a separate reset gate is designed. Do not implicitly turn recovery into ordinary guest login.

## Abuse resistance and operational controls

The current login/reset routes have no source-visible per-account attempt limits. Recovery must add controls before release:

- Rate limit reset requests by project, normalized account key, session/IP risk signal, and bounded global capacity.
- Limit recovery attempts per reset request and user. Atomically exhaust or delay a reset challenge after a reviewed number of failures.
- Bound active reset requests and clean expired rows.
- Parse and checksum the recovery code before derivation, while preserving generic public outcomes.
- Perform at most one recovery AEAD open per request.
- Bound the code, ciphertext, and decoded metadata in SQL and Rust.
- Emit security events containing only allowlisted metadata such as event type, project-scoped opaque user identifier, outcome class, and timestamp. Never include emails, codes, ciphertext, seed bytes, verifier strings, headers, or request bodies.
- Alert on unusual reset and failure rates without creating a credential oracle.
- Send account notifications after enrollment, revocation, rotation, and successful recovery. Notification content must not include recovery material.

Because the code has 256 random bits, rate limiting primarily addresses abuse, enumeration, resource exhaustion, and compromised-code scenarios rather than weak entropy.

## Compatibility and rollout

Treat OpenSecret, released SDKs, and Maple as independently versioned protocol participants.

### Additive rollout order

1. Ship nullable user columns and server support behind a disabled feature flag. Existing rows remain unenrolled.
2. Add SDK models and encrypted methods for status, enrollment/rotation, revocation, and preserving reset.
3. Add client UX for one-time display, print/copy, destructive-fallback warning, recovery input, and replacement-code display.
4. Enable opt-in enrollment for password users in a test environment, then a small production cohort.
5. Enable preserving reset after enrollment telemetry, encrypted-client smoke tests, security review, and recovery transaction tests pass.
6. Prompt existing users after successful login and new users after registration/login.
7. Add OAuth-only and guest behavior only after their independent reset authority is designed.

### Compatibility matrix

- Old client/new server: unchanged registration and destructive reset continue to work; nullable recovery columns are ignored.
- New client/old server: feature discovery reports unsupported or receives 404; enrollment UI remains hidden and destructive fallback stays explicit.
- New client/new server, unenrolled account: authenticated status reports `enrolled: false`; unauthenticated recovery failure reveals nothing.
- New client/new server, enrolled account: preserving reset returns a replacement code.
- Server rollback after recovery columns exist: old server code ignores nullable unknown columns at the database model boundary only if generated schema/models are deployed compatibly; verify this migration ordering before rollout.

No OpenSecret SDK or Maple checkout is present in this repository. Their concrete decoders, secure display/storage APIs, and release versions remain unverified.

## Implementation sequence

### Phase 1: freeze the protocol

- Freeze code encoding, normalization, checksum, labels, canonical field order, versions, and fixed test vectors.
- Record the v1 full-database rollback exclusion explicitly in the security model.
- Decide OAuth and API-key revocation policy.
- Review recovery UX and one-time-display behavior with SDK and Maple owners.

### Phase 2: cryptographic module

- Add a focused recovery module rather than expanding JWT `AuthMethod`.
- Implement strict parse/format and recovery-specific envelope seal/open.
- Use typed wrappers for recovery secret, generation, format version, wrapping version, and envelope bytes.
- Open and byte-compare every newly created envelope before persistence.
- Add zeroization and redaction boundaries.

### Phase 3: migration and model

- Add a timestamped reversible migration, generated Diesel schema, user model fields, constraints, and transaction methods.
- Classify `users.recovery_seed_enc` in encrypted-schema/destructive-reset invariants.
- Extend destructive reset to clear recovery state.
- Define migration ordering for mixed old/new servers before deployment.

### Phase 4: authenticated enrollment

- Implement authenticated status, enroll/rotate, and revoke routes.
- Add password step-up; gate OAuth-only enrollment until fresh-provider proof exists.
- Use generation and password-state compare-and-swap under a user-row lock.
- Add account security notifications without sensitive content.

### Phase 5: preserving reset

- Add a new preserving transaction instead of modifying `complete_destructive_password_reset`.
- Reuse reset challenge verification without adding account-existence leaks.
- Preserve exact seed bytes and all seed-key data.
- Apply secondary-credential revocation policy.
- Atomically overwrite the recovery envelope and return the new code once.
- Never automatically call destructive reset after recovery failure.

### Phase 6: SDK and client rollout

- Add capability discovery and typed encrypted methods to each supported SDK.
- Implement secure one-time display and replacement UX.
- Test old-client/new-server and new-client/old-server combinations.
- Add privacy-safe operational dashboards for bounded outcomes and enrollment adoption.

## Test and validation plan

### Pure unit and property tests

- Fixed vectors for code parse/format/checksum and cryptographic derivation.
- Round trip for arbitrary seeds and recovery secrets.
- Reject changes to user, project, generation, format version, wrapping version, nonce, ciphertext, tag, root key, and recovery secret.
- Reject cross-domain use as password, OAuth, reset-code, lookup, or JWT binding material.
- Reject non-ASCII, Unicode lookalikes, invalid separators, unknown versions, wrong lengths, and checksum errors.
- Verify no accepted alternative spelling derives different bytes.
- Property-test canonical framing and bit-flip rejection.

### Database and concurrency tests

Use the disposable migrated PostgreSQL harness and upgrade-shaped rows:

- New and existing account enrollment preserve the exact seed/private key.
- Re-enrollment immediately replaces the old code; the old code fails and the new one succeeds.
- Revocation makes the old code unusable without altering ordinary seed wraps or user data.
- Copied recovery envelope fields fail across users and projects.
- Relabeled generations and versions fail.
- Unknown, malformed, wrong, or revoked codes cause no mutation.
- Successful preserving reset keeps every seed-key-encrypted record decryptable and preserves derived key identity.
- Successful preserving reset invalidates old password JWTs, consumes reset requests, applies OAuth/API-key policy, replaces the old recovery code, and returns a code that opens the committed envelope.
- Destructive reset still rotates the seed, deletes seed-key data, and clears recovery fields.
- Two concurrent recovery attempts have one winner.
- Recovery versus password change, enrollment, revoke, destructive reset, and account deletion has one documented winner and no ordinary stale resurrection.
- Inject failures before every transaction write and verify full rollback.
- Oversized envelope values are rejected before decryption.
- Existing pre-feature users and mixed-version envelopes behave as specified.

### API and encrypted-client tests

- Exercise every route through a pinned OpenSecret SDK over a live encrypted session.
- Verify success bodies remain encrypted and public errors are stable and sanitized.
- Verify unauthenticated outcomes do not disclose account or enrollment status.
- Verify API-key authentication cannot manage recovery.
- Verify logs contain no recovery code, seed, verifier, decrypted payload, raw headers, response payload, or ciphertext.
- Verify retries do not duplicate rotation, permit both old and new codes, or trigger destructive fallback.
- Test client interruption after enrollment commit and after recovery commit. Confirm subsequent password login can rotate recovery if the returned code was lost.

### Repository validation

Run the exact Rust CI gates through the pinned Nix environment:

```sh
OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-write-lock-file -c cargo fmt --all -- --check

OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-write-lock-file -c env RUSTFLAGS='-D warnings' \
  cargo clippy --locked --all-targets --all-features -- -D warnings

OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-write-lock-file -c env RUSTFLAGS='-D warnings' \
  cargo test --locked --all-features
```

Run the disposable database suite with latest migration rollback/upgrade coverage:

```sh
OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-write-lock-file -c bash \
  ./.agents/skills/validate-opensecret/scripts/disposable_db_tests.sh --redo-latest
```

Then run encrypted SDK and affected Maple flows. EIF/PCR comparison, KMS/IAM inspection, deployment, signing, and reference updates remain separate release evidence requiring explicit operator authorization.

## Review checklist

- Recovery is enrolled while an existing credential can open the seed.
- The recovery secret materially participates in AEAD key derivation.
- Database metadata is never accepted as proof of code possession.
- Recovery never creates ordinary JWT `AuthContext` or authorizes unrelated seed-backed operations.
- Trusted AAD is reconstructed from validated account state rather than copied blindly from storage.
- Every new recovery and password envelope is opened and byte-compared before persistence.
- No unwrap failure generates a replacement seed or invokes destructive reset.
- Successful recovery preserves exact mnemonic bytes and derived identity.
- Wrong, absent, revoked, malformed, oversized, or unsupported recovery state mutates nothing.
- Reset consumption, password replacement, password-wrap replacement, recovery overwrite, and secondary-credential revocation commit atomically.
- Generation compare-and-swap rejects ordinary stale operations.
- The full-database rollback exclusion is documented and not overstated.
- Account deletion and destructive reset clear recovery state.
- Sensitive values cannot reach logs, traces, analytics, URLs, email, or plaintext errors.
- Old/new server and client combinations have explicit behavior.
- Tests cover cryptographic negatives, persistence, concurrency, interruption, and encrypted client contracts.

## Open decisions before implementation

1. Is 256-bit Crockford Base32 with a checksum the preferred non-BIP39 representation, or should another encoding with the same entropy and strict normalization be used?
2. Will preserving recovery require the existing email reset challenge for all password users, or another independent factor?
3. Should OAuth links and API keys always be revoked after recovery? This plan recommends yes.
4. What fresh-provider proof enables enrollment for OAuth-only users?
5. Are guest users excluded from forgotten-password recovery, or will a separate reset gate be designed?
6. What exact rate limits, failure budgets, envelope-size bound, and reset expiry apply per deployment?
7. Which SDK and Maple versions will implement the new contract, and what capability-discovery mechanism will they use?

## Recommended decision

Proceed with one recovery envelope on `users`, 256 bits of enclave-generated entropy, immediate activation without re-entry confirmation, and a separate preserving-reset endpoint. On successful recovery, prove the current code by opening the existing envelope, preserve the exact seed, atomically replace password and recovery wraps, and return the new recovery code once. Keep destructive reset separate and explicitly accept complete consistent database rollback as outside the v1 threat model.
