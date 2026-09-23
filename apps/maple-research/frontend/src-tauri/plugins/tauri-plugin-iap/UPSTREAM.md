# Vendored StoreKit plugin

Source: https://github.com/Choochmeque/tauri-plugin-iap
Revision: `cf4af3d57813883df7b406a966a4a25ebab7efb0` (`v0.10.0-rc.10`).
License: MIT; the upstream LICENSE is preserved verbatim.

This is a deliberately narrowed iOS-only fork. The original Cargo/build script,
Rust mobile registration and command glue, iOS Swift package/plugin, and default
permission declaration were copied from that revision and adapted here. Android,
macOS, Windows, JavaScript distribution tooling, and the upstream demo are not
vendored. Public command names and result shapes are Maple-owned; this is not a
drop-in replacement for the upstream JavaScript package.

Changes: the plugin never calls finish during purchase or event delivery;
explicit finishing follows exact server acknowledgement, while StoreKit owns
queue membership. Signed transaction recovery and events do
not query product metadata; IDs remain strings; purchases distinguish success,
pending and cancellation; explicit sync, storefront changes and management are
exposed. The backend must verify JWS and account ownership. Native StoreKit
verification does not authorize a Maple entitlement. Never log signed payloads.

`finish_transaction` is the sole StoreKit finish site. It must only be invoked
after the server acknowledges exactly that transaction ID. It requires a verified
StoreKit transaction with that exact ID: it searches unfinished transactions first,
then current entitlements, then transaction history. The history fallback is only
enumerated when the narrower sequences omit the ID, and supports earlier
subscription terms and idempotent finishing of known, already-finished IDs.
A matching unverified transaction returns `transaction_verification_failed`; an
ID absent from all three sequences returns `transaction_not_found`, never an
unproven success. No lookup depends on product metadata. The caller owns
account/session fencing and may not apply an old account's status
response after sign-out or account switch. iOS subscriptions management returns
`manage_subscriptions_unavailable` if no UIWindowScene is available; the caller
can then open Apple's fixed subscriptions URL.

Register the crate only under the app's iOS target. Off iOS it exports no commands
or native service; host tests only validate the transport contract.
