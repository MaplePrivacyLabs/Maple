# Apple billing in the Maple application

The iOS app uses StoreKit for monthly Pro and Max subscriptions. The regular
application mounts `AppleBillingProvider`; the simulator-only StoreKit Lab is
a separate diagnostic entry point and is not part of this flow.

## Ownership and recovery

Each authenticated OpenSecret credential revision owns a fresh
`AppleBillingSession`. The app installs the transaction listener before
enumerating purchases, then submits signed transactions to billing. Only the
transaction ID acknowledged by billing can be finished. The selected plan may
still come from a Team or another provider.

Logout and account deletion synchronously suspend this work before awaiting
other account cleanup. A failed logout can start a new session after cleanup;
confirmed deletion retains the old account's suspension. A local one-second
identity check reattaches recovery after SDK refresh without issuing network
requests itself. Foreground, reconnect, and explicit retry also recover, with
failed confirmations retried at 15, 30, 60, 120, then 300 seconds. Billing HTTP
has a 30-second deadline, including its response body. Apple purchase and
restore authentication sheets are not timed out by that HTTP deadline.

Neither signed transactions nor billing bearer tokens are persisted by the new
client code. The shared billing service also fences token minting, responses,
checkout redirects, and portal opening against the account and credential
revision that started the operation.

## Pricing and management

- Native iOS mounts an Apple pricing page instead of the legacy checkout
  component. Product IDs use `cloud.opensecret.maple` or the separate
  `cloud.opensecret.maple.dev` app according to the compiled app variant.
- Prices come exclusively from StoreKit, and require the expected monthly
  subscription period. Missing products or invalid variants cannot start a
  purchase. Storefront changes refresh the catalog.
- `ios_iap_enabled` controls new purchases only. Missing flags fail closed.
  Restore, confirmation retry, and Apple management remain available.
- Existing Team/provider subscriptions block a conflicting new Apple purchase.
  An Apple subscription also directs other platforms to Apple management rather
  than starting another provider's checkout.
- Billing settings list every server-reported subscription, including one not
  selected for quota. Apple uses the native sheet on iOS and Apple's fixed
  subscription-management URL elsewhere. Stripe cancellation management remains
  available independently.
- Anonymous accounts may buy with Apple, but must preserve their Maple Account
  ID and password. Restoring a purchase does not recover a lost Maple login.
- The iOS application hides credit purchases, Team seat-purchase prompts,
  subscription-pass entry, and web discount promotions. Existing usage, API
  keys, Team membership, and provider subscription management remain available.

The US external-checkout link is a separate rollout and is not offered by this
change, even if its server flag is enabled. Questions about App Review's
treatment of existing Team access, credit-funded usage, and issued passes remain
product/review decisions; hiding purchase controls does not settle those issues.

## Validation boundaries

The billing, provider, catalog, and rendered-view tests exercise account
replacement, refresh, failed cleanup, retry, deadline, pending purchase,
conflict, and kill-switch behavior with synthetic inputs. They do not prove an
App Store purchase or Apple's server verification.

For normal-app simulator validation, leave `VITE_STOREKIT_EXPERIMENT` unset and
use the ordinary iOS build path. A combined Maple Dev build also needs the
separate Dev app configuration. Record the exact source revisions, native app
identity, compiled endpoints, and scenarios exercised.

Xcode-local StoreKit transactions must not be accepted by hosted billing.
Real sandbox purchase, restore, renewal, cancellation, refund, and notification
processing still need the App Store/TestFlight path. A successful simulator
build or a local acknowledgment fixture is not evidence of that integration.
