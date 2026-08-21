# Recovery Credential Architecture Plan

## Status

- Proposal for implementation and review.
- Scope: preserve the existing user seed during password recovery by using a pre-enrolled, user-held recovery credential.
- Source baseline: repository `master` at `34bea4f`.
- Evidence: source inspection and the [Credential-Bound Recovery field guide](https://opensecret-recovery-field-guide.anthony599874.chatgpt.site/). No runtime, deployed Nitro, KMS-policy, SDK, or Maple behavior was verified for this plan.

## Executive summary

OpenSecret currently stores a stable 12-word BIP-39 mnemonic in credential-bound AES-256-GCM seed envelopes. Password login verifies the Argon2 password verifier inside the enclave, computes an enclave-keyed authentication binding from the verified credential facts, and only issues tokens after a password-bound envelope opens. OAuth uses the same envelope pattern in a separate credential domain.

The existing forgotten-password flow cannot recreate the password authentication binding and therefore cannot open the old seed envelope. It intentionally generates a new seed, deletes the old seed wraps and seed-key-encrypted data, disconnects OAuth, and installs a new password wrap. That behavior is safe but destructive.

Add a recovery credential as a separate, reset-scoped capability that protects another envelope containing the exact same seed. The recommended first version has these properties:

1. Generate 256 random bits inside the enclave and encode them as a versioned, checksummed string that is visibly not BIP-39.
2. Give each recovery credential a random public UUID for exact, bounded row selection. The UUID is routing metadata, not proof.
3. Derive the recovery envelope key from the enclave root and the presented recovery secret using recovery-specific HKDF domains. Bind the envelope to the project, user, credential UUID, format version, and server-controlled generation through canonical AAD.
4. Store recovery lifecycle state in a dedicated table rather than making recovery an ordinary JWT authentication method.
5. Use two-step enrollment so the credential becomes active only after the client proves it retained the displayed value.
6. Require both the existing email reset challenge and the recovery credential for a preserving reset. Keep recovery incapable of ordinary login or unrelated seed-backed operations.
7. Atomically preserve the existing seed, replace the password verifier and password wrap, consume the reset challenge, revoke the used recovery credential, and stage a replacement recovery credential.
8. Keep the existing destructive reset as an explicit fallback for accounts without an enrolled recovery credential or users who lost it.

Do not use a database-stored `u32` by itself as the recovery key salt or replay defense. A database attacker can roll that integer and its matching ciphertext back together. A generation counter is useful as authenticated lifecycle context, but rollback resistance requires an authoritative state source outside the attacker-controlled candidate or an explicit statement that whole-database rollback is outside the guarantee.

## Goals

- Preserve the exact existing mnemonic bytes and all seed-key-encrypted user data after successful recovery.
- Continue treating PostgreSQL rows as untrusted candidates rather than credential proof.
- Prevent cross-user, cross-project, cross-kind, cross-credential, and cross-generation substitution.
- Keep recovery authority narrower than ordinary password or OAuth authentication.
- Make recovery enrollment, confirmation, rotation, revocation, use, and destructive fallback explicit state transitions.
- Bound row selection and cryptographic work for every request.
- Avoid storing the recovery secret, a plaintext-equivalent verifier, the seed, or derived keys in PostgreSQL, logs, analytics, URLs, email, or public AAD.
- Roll out additively for existing users and independently released clients.

## Non-goals

- Recover an account that did not enroll recovery material before losing all existing credentials.
- Make PostgreSQL available, durable, or deletion-resistant.
- Make a previously valid envelope fresh using AEAD alone.
- Protect against compromise of an approved enclave image, the OpenSecret enclave root, the effective KMS policy, or the user's recovery credential.
- Turn the recovery credential into a second factor for normal login.
- Backfill recovery wraps in SQL or startup migrations. Existing rows cannot be rewrapped without access to the user-owned seed.
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
- User creation and the initial password seed wrap commit in one transaction. The new wrap is opened and compared with the intended seed before it is persisted.
- The route then performs normal password login and only returns tokens after password verification and seed unwrap succeed.

### Password login and tokens

- `POST /login` verifies the encrypted Argon2 verifier in `AppState::authenticate_user`.
- Only after password verification does `compute_password_auth_binding` bind project, user, identifier kind, normalized identifier, and the exact verifier under an enclave-derived HMAC key.
- `decrypt_seed_for_auth_context` loads candidate wraps for the authoritative user and credential kind and accepts only a candidate that opens under the reconstructed key and AAD.
- Access and refresh JWTs carry a signed v2 `AuthContext`. JWT middleware and `/refresh` require that the signed binding still opens an active seed wrap.

### Password change

- `POST /protected/change_password` requires a valid JWT, encrypted session, and current-password reauthentication.
- `AppState::update_user_password_and_seed_wrap` opens the existing seed through the signed current context, creates a new password verifier and wrap over the same seed, and returns a new authentication context.
- `PostgresConnection::update_user_password_and_seed_wrap` compare-and-swaps the old encrypted verifier, deletes old password wraps, and inserts the replacement in one transaction. This is the nearest existing pattern for preserving reset.

### Forgotten-password reset

- `POST /password-reset/request` stores a client challenge digest and an enclave-keyed MAC of an emailed eight-character code. The challenge expires after 24 hours.
- `POST /password-reset/confirm` verifies the email code and the separate client secret, but neither value can open the existing password seed wrap.
- `AppState::confirm_password_reset` therefore generates a new mnemonic and new password wrap.
- `PostgresConnection::complete_destructive_password_reset` locks the user, consumes all reset requests, deletes all seed wraps, OAuth connections, and seed-key-encrypted storage roots, updates the password, and inserts the new wrap atomically.

### Relevant persistence

- `user_seed_wrappings` supports only `password` and `oauth`, with uniqueness on `(user_id, credential_kind, credential_lookup_hash, wrapping_version)`.
- The principal unwrap path scans every wrap for a user and kind rather than selecting by `credential_lookup_hash`. Recovery must not copy this unbounded trial-decryption behavior.
- `security_invariants.rs` maintains the destructive-reset inventory of seed-key-encrypted tables. A preserving reset must deliberately avoid that deletion path.

## Threat model and security claims

### Assumed attacker capabilities

The storage attacker may read, insert, copy, modify, relabel, reorder, replay, or delete PostgreSQL rows. The attacker may also observe normal host-visible metadata and trigger public recovery endpoints. The design must assume a copied database is available for offline analysis.

The trusted computing base remains the approved enclave code, the plaintext OpenSecret enclave root available to that code, the effective attestation-aware KMS policy, cryptographic libraries, and credential-verification logic. The client must correctly attest the enclave and protect the displayed recovery credential.

### Intended narrow claim

Given approved enclave code, the intended KMS policy, and an uncompromised enclave root, database manipulation alone must not cause a recovery credential for one user, project, credential UUID, generation, or format to open another recovery envelope or authorize an ordinary authenticated session.

### Explicit limitations

- Deletion remains denial of service.
- AEAD authenticates a formerly valid envelope but does not establish that it is the latest active envelope.
- A full database rollback can restore mutually consistent old rows. PostgreSQL cannot be both the attacker-controlled state and the sole freshness oracle.
- Possession of the recovery credential plus the other required reset factors authorizes account recovery. Theft is therefore severe even though the credential is not the mnemonic itself.
- Local source and tests do not prove deployed PCRs, live KMS/IAM policy, artifact identity, network placement, or log retention.

## Recovery credential format

### Recommended v1 format

Generate a 32-byte secret using `generate_random_enclave::<32>`. Encode a self-describing binary payload using Crockford Base32:

```text
OSRC1-<credential-id>-<secret>-<checksum>
```

- `OSRC1` is a fixed recovery-format marker, not a cryptographic domain.
- `credential-id` encodes a random UUID generated by the enclave/application. It is public and selects one row.
- `secret` encodes all 256 random bits.
- `checksum` detects transcription mistakes over the marker, credential ID, and secret. Use a versioned checksum algorithm such as the first 40 bits of SHA-256; it adds no entropy.
- Group encoded characters in fixed-width blocks for display and printing.
- Parsing accepts ASCII case-insensitively and ignores only ASCII hyphens and spaces. Reject all other characters, Unicode lookalikes, wrong lengths, unknown versions, and checksum failures before database work.
- Canonical output is uppercase with fixed grouping. Persist only parsed bytes, never user-supplied text.

This is visibly distinct from the lowercase, space-separated 12-word BIP-39 mnemonic. Do not use the BIP-39 word list, a mnemonic-compatible word count, or labels such as "seed phrase." Product copy should call it an "OpenSecret recovery code" and state that it cannot replace the private-key mnemonic.

### Entropy rationale

The existing mnemonic contains 128 bits of entropy. The recovery credential should use 256 random bits because it is a long-lived bearer secret likely to be stored offline, printable, and entered rarely. This does not compensate for an implementation flaw, but it makes brute-force infeasible even if a database snapshot later becomes a verifier through another compromise.

The server must generate the value. User-chosen recovery phrases are out of scope. HKDF does not increase human-secret entropy, and Argon2 is not required to make a uniformly random 256-bit credential safe against guessing.

### Secret handling

- Accept the recovery value only in the encrypted request body, never in paths, query strings, headers, emails, or telemetry.
- Return it only in an encrypted response immediately after generation or rotation.
- Redact complete request payloads at every logging layer. Do not log parsed IDs until the threat and privacy impact is accepted; user UUID and recovery credential UUID together are linkable metadata.
- Use `zeroize`/`Zeroizing` for parsed secret bytes, derived recovery keys, and temporary formatted strings where practical. This improves memory hygiene but does not make copies impossible.
- Apply strict body, string, and decoded-length limits before KDF or database work.

## Cryptographic construction

### Separate recovery domains

Do not reuse password, OAuth, reset-code, lookup, authentication-binding, or general seed-wrap labels. Reserve and test exact ASCII labels before implementation. A concrete v1 set is:

```text
os.recovery-wrap-root.v1
os.recovery-wrap-aead-key.v1
os.recovery-wrap.v1
os.recovery-confirm-mac.v1
```

The recovery credential is high entropy and does not need to become the existing `AuthBinding` or JWT `AuthMethod`. Derive a recovery-specific AEAD key directly:

```text
recovery_wrap_root = HKDF-SHA256(
    ikm = enclave_root,
    salt = none,
    info = "os.recovery-wrap-root.v1"
)

credential_ikm = canonical(
    "os.recovery-credential.v1",
    recovery_secret_32_bytes
)

recovery_wrap_key = HKDF-SHA256(
    ikm = recovery_wrap_root,
    salt = SHA-256(credential_ikm),
    info = canonical(
        "os.recovery-wrap-aead-key.v1",
        project_id,
        user_uuid,
        credential_uuid,
        generation
    )
)

recovery_aad = canonical(
    "os.recovery-wrap.v1",
    project_id,
    user_uuid,
    credential_uuid,
    generation,
    format_version,
    wrapping_version,
    state = "pending" | "active"
)

envelope = AES-256-GCM.seal(
    recovery_wrap_key,
    exact_seed_bytes,
    recovery_aad,
    fresh_random_96_bit_nonce
)
```

The implementation may simplify the HKDF input layout after cryptographic review, but it must preserve separate labels, canonical field boundaries, and the rule that both the enclave root and presented 256-bit secret are necessary. The raw secret must not be stored in AAD, a lookup column, or any deterministic public digest.

### Exact row selection

Use the credential UUID from the formatted recovery code to select exactly one row already scoped by the project-derived user and expected state. The UUID is not authentication. A successful AEAD open under enclave-reconstructed context proves possession and envelope integrity.

This avoids scanning all recovery wraps, gives a fixed amount of cryptographic work, and permits generic handling of unknown IDs. Do not derive a public lookup hash directly from the secret; that would be unnecessary and could become an offline verifier if the format changes.

### Versioning

Store and authenticate distinct versions:

- `format_version`: parsing and display representation.
- `wrapping_version`: cryptographic envelope algorithm and canonical inputs.
- `generation`: lifecycle epoch for rotation and stale-operation detection.

The envelope bytes should also contain a magic value and cryptographic version rather than relying solely on mutable database columns. Authenticate all public envelope metadata. Reject a database-column/envelope-header mismatch before decrypting.

### About the proposed `u32` generation

A generation counter belongs in canonical key context and AAD, but it should be at least `BIGINT`/`u64` and server-controlled. It is not an AEAD nonce and must not replace the fresh random GCM nonce.

If the current generation and candidate envelope are both read from PostgreSQL, a storage attacker can replay an older matching pair. The implementation must choose and document one of these models:

1. **Recommended target:** keep the authoritative per-user recovery generation in a rollback-resistant service outside PostgreSQL, such as an enclave/KMS-bound monotonic state service, and authenticate its value into the envelope.
2. **Pragmatic v1:** use a locked `users.recovery_generation BIGINT` plus append-only recovery audit records in PostgreSQL, detect ordinary stale writes, and explicitly accept whole-database rollback as outside the v1 guarantee. Monitor for generation regression against an independent log when available.

Do not claim rollback resistance under model 2. A random new credential makes an old row unusable to a user who only possesses the newest code during normal operation, but database rollback can reactivate the old credential for an attacker who retained it.

## Persistence design

### Dedicated table

Add a timestamped Diesel migration for a dedicated lifecycle table:

```sql
CREATE TABLE user_recovery_credentials (
    id BIGSERIAL PRIMARY KEY,
    credential_uuid UUID NOT NULL UNIQUE,
    user_id UUID NOT NULL REFERENCES users(uuid) ON DELETE CASCADE,
    generation BIGINT NOT NULL CHECK (generation > 0),
    format_version SMALLINT NOT NULL,
    wrapping_version SMALLINT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'active', 'consumed', 'revoked')),
    seed_enc BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    confirmed_at TIMESTAMPTZ,
    consumed_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    expires_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK (octet_length(seed_enc) BETWEEN 29 AND 4096)
);
```

Add:

- A unique partial index enforcing at most one `active` recovery credential per user for v1.
- A unique partial index enforcing at most one `pending` recovery credential per user.
- An index on `(user_id, state)` for status and revocation.
- An `updated_at` trigger using the repository's current convention.
- An optional short expiration for `pending` rows, such as 15 minutes. Active credentials do not expire by default unless product policy explicitly requires it.
- `users.recovery_generation BIGINT NOT NULL DEFAULT 0` for the pragmatic v1 lifecycle epoch, or a reference to the chosen rollback-resistant authority.

Do not overload `user_seed_wrappings` in v1. Recovery has pending, confirmed, consumed, revoked, and rotation states that password/OAuth wraps do not have. Keeping the table separate also prevents accidental inclusion in `AuthMethod`, JWT validation, and generic candidate scanning.

### State invariants

- `pending`: a wrap exists and the code has been displayed, but recovery is not enabled.
- `active`: the client proved possession of the exact newly generated credential.
- `consumed`: the credential successfully participated in a preserving reset and cannot be used again.
- `revoked`: an authenticated user disabled or replaced it without use.
- Exactly one active credential is supported per user in v1.
- A pending credential never authorizes recovery.
- State transitions are one-way except that a new row may supersede an old row.
- Account deletion cascades all recovery rows.
- Destructive reset revokes/deletes all recovery rows.

Update `src/models/schema.rs`, add a typed model module, database methods with user-scoped queries, and the encrypted-schema classification in `src/security_invariants.rs`.

## API design

All routes use the existing attested encrypted session protocol. Recovery values exist only inside decrypted request/response payloads. The paths and names below are proposed contracts and should be reviewed with SDK and Maple owners before implementation.

### Authenticated status

```text
GET /protected/recovery-credential
```

Authorization:

- User access JWT only; API-key context must never authorize recovery management.
- The JWT's signed `AuthContext` must still open an active password or OAuth seed wrap.

Encrypted response:

```json
{
  "status": "not_enrolled | pending | active",
  "created_at": "RFC3339 timestamp or null",
  "confirmed_at": "RFC3339 timestamp or null"
}
```

Never return a credential UUID, generation, code fragment, or secret-equivalent status to unauthenticated callers.

### Begin enrollment or rotation

```text
POST /protected/recovery-credential/enroll
```

Authorization:

- Valid user JWT and live encrypted session.
- Require recent step-up authentication. For password users, verify the current password. For OAuth users, require a fresh provider reauthentication or defer OAuth-only enrollment until such proof is available.
- Do not accept API keys.

Operation:

1. Open the exact existing seed through the signed current `AuthContext`.
2. Generate credential UUID, 256-bit secret, next generation, and pending envelope.
3. Open the newly created envelope and compare the plaintext byte-for-byte with the existing seed.
4. Lock the user and current recovery state.
5. Revalidate the observed credential/password state or generation so a racing password change/reset cannot install a stale seed.
6. Revoke any prior pending row and insert the new pending row atomically. Do not revoke the active row yet during rotation.
7. Return the formatted recovery code once.

Encrypted response:

```json
{
  "recovery_code": "OSRC1-...",
  "confirmation_token": "short-lived opaque token",
  "expires_at": "RFC3339 timestamp"
}
```

The confirmation token should be an enclave-keyed MAC over user, project, credential UUID, generation, pending-row identity, and expiry. It is not a substitute for presenting the recovery code.

### Confirm enrollment

```text
POST /protected/recovery-credential/confirm
```

Request:

```json
{
  "recovery_code": "OSRC1-...",
  "confirmation_token": "..."
}
```

Operation:

1. Validate and parse the code under strict bounds.
2. Validate the confirmation token and load the exact pending row scoped to the JWT user/project.
3. Open the pending envelope with the presented secret.
4. Independently open the user's seed through the current signed `AuthContext` and compare exact bytes.
5. In one transaction, lock the user and rows, verify the expected generation and pending state, mark the prior active credential revoked, and mark this row active.
6. Return no recovery material.

If the client disappears after enrollment but before confirmation, the prior active credential remains active and the pending row expires. This is the server-side proof that the client retained the newly displayed value.

### Revoke

```text
DELETE /protected/recovery-credential
```

Require the same step-up policy as enrollment. Lock the user, increment the recovery generation, revoke active and pending rows, and return a generic success response. Revocation must not alter the password/OAuth seed wraps or user data.

### Recovery-preserving reset

```text
POST /password-reset/recovery/confirm
```

No JWT is required, but a live encrypted session is mandatory. Require all of:

- Project `client_id` and project-scoped email.
- Existing emailed reset code.
- Existing client reset challenge secret.
- Enrolled recovery code.
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

The email challenge proves access to the reset channel; the recovery credential supplies the missing cryptographic authority to open the existing seed. Recovery alone does not issue an ordinary JWT.

Public failures should collapse to one sanitized response, for example HTTP 400 with the existing `Bad Request` shape. Do not reveal whether the account exists, recovery is enrolled, the ID matched, the reset factor failed, or AEAD authentication failed. Apply response-time equalization only after measurement and cryptographic review; artificial sleeps can create denial-of-service capacity problems.

### Replacement recovery credential after use

A used recovery credential must be one-time and revoked. Generate a replacement inside the preserving-reset operation, but stage it as `pending`, not `active`. Return it with a short-lived confirmation token only after the transaction commits:

```json
{
  "message": "Password reset successful",
  "access_token": "...",
  "refresh_token": "...",
  "recovery_code": "OSRC1-...",
  "confirmation_token": "...",
  "expires_at": "RFC3339 timestamp"
}
```

The client must confirm the new code through the authenticated confirmation route. Until confirmation, recovery is not active. This avoids claiming the user saved material merely because it was sent once. If product chooses uninterrupted recovery coverage instead, it must accept that making the unseen replacement active may strand the user, while keeping the used credential active violates one-time semantics.

### Destructive fallback

Keep `POST /password-reset/confirm` destructive for backward compatibility. New clients must present an explicit warning that it creates a new cryptographic identity and erases seed-key-encrypted data. Do not silently fall back from a failed recovery attempt to destructive reset. Require a separate user confirmation and separate request.

## Preserving-reset transaction

Prepare cryptographic outputs before the database transaction, but treat all rows as provisional until revalidated under locks:

1. Resolve the user by email and project without exposing the result publicly.
2. Strictly parse the recovery code and select one active credential row by `(user_id, credential_uuid, state)`.
3. Verify reset-code MAC, reset expiry, and client challenge digest.
4. Open the recovery envelope with trusted project/user context, the parsed credential UUID and secret, stored version dispatched through reviewed code, and the expected authoritative generation.
5. Validate the recovered bytes as the existing BIP-39 mnemonic. Never generate a seed on unwrap failure.
6. Create a new Argon2 verifier and password envelope over the exact recovered bytes. Open and byte-compare the new envelope.
7. Create a new random replacement recovery credential and verified pending envelope over the same bytes.
8. Begin one PostgreSQL transaction.
9. Lock the user and selected recovery row `FOR UPDATE` in a documented global order.
10. Recheck the reset request is unused and unexpired, recovery row is active/unrevoked, versions are supported, generation is current, and the encrypted password state has not changed since it was observed.
11. Atomically consume the selected reset request and mark all other active reset requests consumed.
12. Mark the used recovery row consumed, increment the recovery generation, and insert the replacement pending row at the new generation.
13. Delete every password-kind seed wrap and insert exactly one new password wrap.
14. Update `users.password_enc`.
15. Apply the explicit secondary-credential policy below.
16. Commit.
17. Verify the committed new password context opens the seed before issuing tokens. If post-commit verification fails, return a generic internal error and raise a high-severity operational event without sensitive values; never run destructive fallback.

Any failure before commit leaves reset state, password, recovery credential, seed wraps, and user data unchanged.

## Secondary credential policy

Recovery is an account-takeover-sensitive event. V1 should use these defaults:

- Delete all OAuth connections and OAuth seed wraps during recovery. This prevents a potentially compromised linked identity from retaining access. Re-linking requires a later authenticated flow.
- Delete all password seed wraps before inserting the replacement. This invalidates old password access and refresh JWTs because their bindings no longer open.
- Revoke/consume all recovery rows except the newly pending replacement.
- Consume all password reset requests.
- Preserve all seed-key-encrypted user data because the seed is unchanged.
- Revoke all user API keys unless a product-level review establishes a narrower safe policy. The current destructive reset preserves API keys, but a preserving account recovery should not automatically retain bearer credentials after possible account compromise.
- Existing OpenSecret transport sessions may remain in cache, but they confer no identity. Consider explicit session eviction only if the session store can reliably associate sessions with users; do not add a false association.

These choices must be visible in user-facing copy and tested. If the product chooses to preserve OAuth or API keys, document the threat model and provide account activity review after recovery.

## New-user and existing-user enrollment

### New password users

Do not add the recovery code directly to the existing `/register` success contract in the first implementation. Registration already commits the account before subsequent email-verification work and then logs in. Mixing mandatory backup confirmation into that flow creates difficult partial-success semantics and breaks old clients.

Roll out an immediate post-registration enrollment prompt in updated clients using the protected two-step enrollment API. The server may later add an atomic registration protocol version that creates a pending recovery row with the user and password wrap, but it should be a separate versioned contract.

### Existing password users

Enrollment is opt-in and lazy. A valid current JWT plus step-up password verification lets the enclave open the existing seed and create the recovery envelope. No SQL backfill is possible or required.

### OAuth-only and guest users

- OAuth-only enrollment requires a defined fresh-provider proof. Do not treat an old OAuth JWT as sufficient step-up without review. If SDK/provider support is not ready, launch password-user enrollment first and report OAuth-only recovery as unsupported.
- Guest accounts have no email reset channel. Under the proposed two-factor preserving reset, guests cannot use forgotten-password recovery until the product defines a separate stable account identifier and reset gate. Do not weaken the design by letting the recovery credential become ordinary guest login implicitly.

## Abuse resistance and operational controls

The current login/reset routes have no source-visible per-account attempt limits. Recovery must add controls before release:

- Rate limit reset requests by project, normalized account key, session/IP risk signal, and bounded global capacity.
- Limit recovery-confirm attempts per reset request and per user. Atomically exhaust or delay the reset challenge after a reviewed number of failures.
- Bound active reset requests and expire/clean old rows.
- Parse and checksum the recovery code before row lookup, but use generic outcomes.
- Perform at most one recovery AEAD open per request.
- Bound ciphertext and metadata sizes in SQL and Rust.
- Emit security events containing only allowlisted metadata such as event type, project-scoped opaque user identifier, outcome class, and timestamp. Never include emails, secrets, codes, credential UUIDs, ciphertext, seed bytes, verifier strings, headers, or request bodies.
- Alert on unusual reset-request and failure rates without creating a credential oracle.
- Send account notifications after enrollment, revocation, and successful recovery. Notification content must not include recovery material.

Because the credential has 256 random bits, online rate limiting is primarily for abuse, account enumeration, resource exhaustion, and compromised-code scenarios rather than compensating for weak entropy.

## Compatibility and rollout

Treat OpenSecret, released SDKs, and Maple as independently versioned protocol participants.

### Additive rollout order

1. Ship schema support and server code behind a disabled feature flag. Old servers must tolerate no new client behavior; old clients see unchanged routes.
2. Add SDK models and methods for status, enroll, confirm, revoke, and preserving reset. The SDK owns attestation and encrypted transport; these are not plaintext `fetch`/`curl` contracts.
3. Add client UX for secure display, print/copy, confirmation re-entry, destructive-fallback warning, recovery input, replacement confirmation, and credential revocation.
4. Enable opt-in enrollment for password users in a test environment, then a small production cohort.
5. Enable preserving reset only after enrollment telemetry, encrypted-client smoke tests, security review, and recovery transaction tests pass.
6. Prompt existing users after successful login. Prompt new users after registration/login without blocking account creation until product explicitly chooses mandatory enrollment.
7. Add OAuth-only and guest behavior only after their independent reset authority is designed.

### Compatibility matrix

- Old client/new server: unchanged registration and destructive reset continue to work; new routes are additive.
- New client/old server: feature discovery reports unsupported or a 404; the client must hide enrollment and preserve the explicit destructive fallback.
- New client/new server, unenrolled account: status is `not_enrolled`; preserving reset is unavailable without revealing this on the unauthenticated endpoint.
- New client/new server, enrolled account: preserving reset and replacement confirmation are available.
- Server rollback after recovery rows exist: old server code must ignore the new table. Do not add `recovery` to generic `user_seed_wrappings` if old exhaustive matches could fail.

No OpenSecret SDK or Maple checkout is present in this repository. Their concrete request decoders, unknown-field behavior, secure-storage APIs, and release versions remain unverified and must be inspected before finalizing contracts.

## Implementation sequence

### Phase 1: protocol and threat-model decision record

- Freeze credential encoding, normalization, checksum, labels, canonical field order, envelope header, versions, and test vectors.
- Decide the rollback/freshness model explicitly. Do not advertise database rollback resistance if using PostgreSQL-only generation state.
- Decide OAuth, API-key, guest, and session behavior after recovery.
- Review recovery UX with SDK and Maple owners before endpoint names become public.

### Phase 2: cryptographic module

- Add a focused recovery module rather than expanding JWT `AuthMethod`.
- Implement strict parse/format, recovery-specific key derivation, envelope seal/open, and confirmation-token MAC.
- Use typed wrappers for credential UUID, recovery secret, generation, format version, wrapping version, and envelope bytes.
- Open and compare every newly created envelope before returning it for persistence.
- Add zeroization and redaction boundaries.

### Phase 3: migration and models

- Add the timestamped reversible migration, generated Diesel schema, model, scoped queries, and transaction methods.
- Add constraints and partial indexes for one active/one pending credential.
- Classify `seed_enc` in the encrypted-schema/destructive-reset invariants.
- Extend destructive reset and account deletion to remove recovery credentials.
- Add cleanup for expired pending rows.

### Phase 4: authenticated enrollment lifecycle

- Implement status, begin enrollment, confirm, rotate, and revoke routes.
- Add password step-up; gate OAuth-only enrollment until fresh-provider proof exists.
- Use generation/state compare-and-swap and row locks for concurrent enrollment, rotation, password change, recovery, and destructive reset.
- Add account security notifications without sensitive content.

### Phase 5: preserving reset

- Add a new transaction instead of modifying `complete_destructive_password_reset`.
- Reuse reset challenge verification but avoid duplicated account-existence leaks.
- Preserve exact seed bytes and all seed-key data.
- Apply secondary-credential revocation policy.
- Stage and return one replacement recovery credential; require authenticated confirmation.
- Never automatically call destructive reset after a recovery failure.

### Phase 6: SDK and client rollout

- Add capability discovery and typed encrypted methods to each supported SDK.
- Implement Maple/client secure display and lifecycle UX.
- Test old-client/new-server and new-client/old-server combinations.
- Add operator dashboards for bounded outcome classes and migration adoption, without credential material.

## Test and validation plan

### Pure unit and property tests

- Official fixed test vectors for format parse/format/checksum and cryptographic derivation.
- Round trip for arbitrary seeds and recovery secrets.
- Reject changes to user, project, credential UUID, generation, format version, wrapping version, state, envelope header, nonce, ciphertext, tag, root key, and recovery secret.
- Reject cross-domain use as password, OAuth, reset-code, lookup, or JWT binding material.
- Reject non-ASCII, Unicode lookalikes, invalid separators, unknown versions, wrong lengths, and checksum errors.
- Verify no accepted alternative spelling derives different bytes.
- Property-test canonical framing and bit-flip rejection.

### Database and concurrency tests

Use the disposable migrated PostgreSQL harness and add upgrade-shaped rows:

- New account and existing account enrollment preserve the exact seed/private key.
- Pending enrollment cannot recover; confirmation activates it.
- Rotation keeps the old active credential until the new one is confirmed.
- Revoke makes the old code unusable without altering the password wrap or user data.
- Copied recovery rows fail across users and projects.
- Relabeled IDs, generations, versions, and states fail.
- Unknown/malformed credentials cause no mutation.
- Successful preserving reset keeps every seed-key-encrypted record decryptable and preserves derived public/private key identity.
- Successful preserving reset invalidates old password JWTs, consumes reset requests, applies OAuth/API-key policy, consumes the old recovery code, and creates only one pending replacement.
- The replacement remains inactive until confirmation.
- Destructive reset still rotates the seed, deletes seed-key data, and removes recovery rows.
- Two concurrent recovery attempts have one winner.
- Recovery versus password change, enrollment, revoke, destructive reset, and account deletion have one documented winner and no stale resurrection.
- Inject failures before every transaction write and verify full rollback.
- Candidate selection remains one exact row with bounded work.
- Existing pre-feature users and mixed-version rows behave as specified.

### API and encrypted-client tests

- Exercise every route through a pinned OpenSecret SDK over a live encrypted session.
- Verify success bodies remain encrypted and public errors are stable and sanitized.
- Verify unauthenticated outcomes do not disclose account or enrollment status.
- Verify API-key authentication cannot manage recovery.
- Verify response and application logs contain no credential, seed, verifier, decrypted payload, raw headers, or ciphertext.
- Verify retry behavior does not duplicate enrollment, consume two credentials, or perform destructive fallback.
- Test client interruption after code display, after pending write, after preserving-reset commit, and before replacement confirmation.

### Repository validation

Run the exact current Rust CI gates through the pinned Nix environment:

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

Then run encrypted SDK and affected Maple flows. EIF/PCR comparison, KMS/IAM inspection, deployment, signing, and reference updates are separate release evidence and require explicit operator authorization.

## Review checklist

- Recovery proof is pre-enrolled while an existing credential can open the seed.
- Recovery secret materially participates in AEAD key derivation.
- Public credential UUID and database metadata are never accepted as proof.
- Recovery never creates ordinary JWT `AuthContext` and cannot call unrelated seed-backed operations.
- Every trusted AAD field is reconstructed from validated request/account state rather than copied blindly from the row.
- The new envelope is opened and byte-compared before persistence.
- No unwrap failure generates a replacement seed or invokes destructive reset.
- Recovery success preserves exact mnemonic bytes and derived identity.
- Wrong, absent, pending, consumed, expired, revoked, or unsupported credentials mutate nothing.
- Reset consumption, password replacement, password-wrap replacement, recovery consumption/rotation, and secondary-credential revocation commit atomically.
- Row selection and KDF work are bounded.
- Generation and rollback claims match the actual authoritative state source.
- Account deletion and destructive reset remove recovery rows.
- Sensitive values cannot reach logs, traces, analytics, URLs, email, or plaintext errors.
- Old/new server and client combinations have explicit behavior.
- Tests cover cryptographic negatives, persistence, concurrency, interruption, and encrypted client contracts.

## Open decisions before implementation

1. Which rollback-resistant authority, if any, will own the recovery generation? If none is available for v1, approve the explicit whole-database rollback limitation.
2. Is 256-bit Crockford Base32 with a checksum acceptable UX, or should the client use a different non-BIP39 encoding with the same entropy and strict normalization?
3. Will preserving recovery require the existing email reset challenge for all password users, or another independent factor?
4. Should OAuth links and API keys always be revoked after recovery? This plan recommends yes.
5. What fresh-provider proof enables enrollment for OAuth-only users?
6. Are guest users excluded from forgotten-password recovery, or will a separate reset gate be designed?
7. Is one active recovery credential sufficient for v1, or is independent multi-code revocation a launch requirement?
8. What exact rate limits, failure budgets, and pending-expiration periods apply per project and deployment?
9. Which SDK and Maple versions will implement the new contract, and what capability-discovery mechanism will they use?

## Recommended decision

Proceed with the dedicated, reset-scoped recovery credential design, one active credential per user, 256 bits of enclave-generated entropy, two-step enrollment, and a new preserving-reset endpoint. Keep destructive reset separate and explicit. Before implementation, resolve the rollback-authority decision and secondary-credential revocation policy; those two choices determine whether the lifecycle security claim is accurate.
