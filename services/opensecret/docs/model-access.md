# Model discovery and access

OpenSecret owns model authorization. Catalog visibility, subscription eligibility,
experimental grants and usage admission are separate decisions.

`GET /v1/models` and `GET /v1/models/catalog` retain their existing schemas and
feature-flag visibility. The uncensored DeepSeek entry remains hidden unless
`uncensored_deepseek` evaluates true. Its V1 access label remains `pro` because
older clients do not understand higher model tiers.

`GET /v2/models/catalog` is an authenticated, encrypted catalog that includes all
listed models, including uncensored DeepSeek. Each model and alias has an `access`
object with `allowed`, `eligible_plans` and `reason`. `allowed` describes model
entitlement rather than quota. The catalog API version is independent of the
encrypted transport version.

For uncensored DeepSeek, an explicit true `uncensored_deepseek` evaluation grants
model access on any plan. Otherwise Max or Teams is required. A false, absent or
unavailable flag falls through to subscription eligibility; it is not a kill
switch for eligible subscribers. Other models retain their existing policy.
Authentication, guest restrictions, usage, feature and context limits still apply.

Chat Completions and Responses capture this access policy from authenticated
billing/flag results before provider work. The immutable snapshot follows the
logical request through routing, preparation and provider execution. Selecting a
model or calling an older inference endpoint never grants permission.

Billing's V2 usage response retains separate API/frontend admission decisions and
adds the selected plan from the same usage entitlement resolution, including
Apple, Team and Stripe-cache paths. API-credit balances do not confer a plan.
Older billing responses retain ordinary Free/Paid behavior but cannot grant
Max/Teams-only access without the experimental flag.

Premium model denials retain HTTP 403, the existing flat error fields and
`model_not_available_on_plan`, with additive nested `model` and `eligible_plans`
metadata for an updated client. Quota denials remain a separate error contract.

Research opts into the inclusive catalog. It falls back to V1 only when a catalog
endpoint is absent (404/405), handles server access decisions rather than guessing
from the account's display name, and presents the required plans in its upgrade
dialog. Catalog responses are scoped to the current account, refreshed after plan
changes and when opening the selector, and ignored after disposal. Persisted model
metadata never restores an affirmative entitlement grant. Other frontends can
continue using their existing V1 discovery contract.
