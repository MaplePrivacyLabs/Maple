# OAuth callback selection

OAuth callbacks are configured per project and provider. This additive API
contract lets browser clients on different origins finish sign-in on the
origin that started it. It does not change the default callback or require
existing clients to send a new field.

## Provider settings

The encrypted platform settings API at
`/platform/orgs/:org_id/projects/:project_id/settings/oauth` retains its
existing GET and PUT methods and organization/project authorization. Updating
settings still requires an organization owner or administrator.

Each Google, GitHub, or Apple settings object keeps `redirect_url` and accepts
an optional `additional_redirect_urls` array. For example, a Google settings
object can contain:

```json
{
  "client_id": "example-provider-client-id",
  "redirect_url": "https://app.example.com/auth/google/callback",
  "additional_redirect_urls": [
    "https://auth.example.com/auth/google/callback"
  ]
}
```

This is a nested provider object, not a complete PUT request. The surrounding
enabled flags and other provider settings retain their existing semantics.

- At most 16 additional URLs are accepted per provider. Each must satisfy
  the existing generic callback URL validation, including its length bound.
  The platform does not restrict this list to one application's hostnames.
- On PUT, omitting the new field or sending `null` preserves that provider's
  stored additional list. An explicit array replaces it; `[]` clears it.
- This preservation applies when the provider settings object is supplied.
  Omitting or clearing the entire provider object retains the existing
  whole-object PUT behavior; it is not a patch API for other fields.
- GET and the PUT response include `additional_redirect_urls` when a list is
  stored, including `[]`. An unset or null stored value omits the field from
  the response; both omission and `[]` mean no additional callbacks on read.
  Existing rows without the field remain readable; no SQL schema migration
  is required.
- URL-list preservation and the settings write are serialized per project,
  so an older writer that omits the field cannot overwrite a concurrently
  committed list with an earlier snapshot.

## Initiation and completion

The decrypted request body for `/auth/github`, `/auth/google`, and
`/auth/apple` accepts an optional `redirect_url` alongside the existing
`client_id`. The same contract applies through both
Transport V1 and Transport V2.

When absent or `null`, the provider's default `redirect_url` is used. When
present, the value must exactly match the default or one of that provider's
additional entries for the requested project. There is no wildcard, prefix,
or same-host matching. A rejected selection returns the existing bad-request
error before allocating OAuth state.

The chosen callback is recorded in the server-validated, one-use OAuth state.
The provider authorization request and token exchange use that same callback,
including Apple's token exchange. Changing the default or additional list
does not retarget an already-started flow. Removing a list entry stops new
flows from selecting it; it does not revoke a pending flow. Existing state
expiry, one-use checks, provider checks, and V2 session/PKCE/nonce bindings
continue to apply.

Clients must treat the returned `state` as opaque and return it unchanged.
The callback request does not accept a separate redirect override. Changing
the callback inside the returned state cannot change the server's stored
selection.

## Compatibility and adoption

Deploy backend support before a client selects a non-default callback.
Register each callback with its OAuth provider as well as in the project's
backend settings; these are separate requirements. Existing clients that
omit the request field continue to use the default.

An older backend ignores the new configuration and request fields and keeps
using its default callback. Therefore a client that depends on a non-default
callback must not remain active during a backend downgrade. Older backend
settings writes may also discard the additional list; preserve configuration
outside the downgraded writer and verify it before re-enabling consumers.
OAuth state is process-local, so restarting or replacing a backend can
invalidate pending sign-ins; the user must start a new attempt.

SDK publication, consumer upgrades, provider registration, and traffic changes
are separate from implementing this backend contract.
