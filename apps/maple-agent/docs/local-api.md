# Local API

Settings → Local API controls an optional HTTP/1 endpoint on this computer.
The default is Off, bound only to `127.0.0.1`, on port 8080 (or the managed
workspace's `MAPLE_PORT` reservation). Change the port while stopped. The base
URL is `http://127.0.0.1:<port>/v1`.

Create a named Maple API key in Settings → API keys and put it in your AI
application's API-key field. Every inference request must supply
`Authorization: Bearer <key>`. The service saves no fallback key. The key's
account determines authorization and usage; it need not be the desktop's
signed-in account. OpenSecret remains authoritative for key validity, plans,
models and quotas. Local refers to the endpoint: inference uses Maple's
attested encrypted SDK transport to the configured backend.

The existing routes are GET `/v1/models`, POST `/v1/chat/completions`,
POST `/v1/embeddings`, POST `/v1/systemone`, and GET `/health` (also `/`).
Responses, audio and legacy `/v1/completions` are not exposed. Browser access
is disabled; requests carrying `Origin` or `Sec-Fetch-Site` are rejected.
Running means the socket is listening. It does not attest backend availability
or successful inference, and `/health` checks only local liveness.

## Ownership

The profile's local host owns the listener, all connections and active response
bodies. GPUI is a controller; it does not start a second runtime or an external
proxy process. Settings uses its local backend even when a task executes on a
paired remote computer. Proxy RPCs reject remote facades and are excluded from
the remote method allowlist. Local status invalidations never enter paired
remote event streams. No remote protocol change is involved.

The open local store persists `local_api.config` under the selected account.
It contains only the desired enabled state and port. Actual state, safe bind
errors and active-request counts are host snapshots with monotonically ordered
revisions; reconnect starts a new UI epoch. These preferences and API keys are
separate. The proxy's SDK transport never borrows the desktop login session.

Enabling requires a validated local account. A saved account ID alone cannot
autostart the endpoint. Enabled preferences resume after the profile's host
starts and its restore/unlock succeeds. This does not install an OS login
service. Reattaching a frontend to an identical running service leaves its
streams intact. Bind conflicts are errors, without automatic port changes or
process takeover.

## Lifetime and limits

An enabled listener retains the host after the window closes. Existing tray
and no-tray behavior applies; explicit Quit explains and stops the local API.
Logout/account transitions, destructive reset, draining store operations and
updates stop network admission before releasing account/store ownership.
Key-only password rewraps retain the existing non-draining lane.

Stop and status remain available through private local IPC while storage is
locked or maintaining. The settings switch persists Off. Emergency
`stop_local_api` suspends the current service without accessing locked
preferences; an intervening emergency Stop prevents a maintenance operation
from resuming it. Automatic account restore keeps it paused for this host's
lifetime; use Resume or restart the daemon to resume persisted On.

Generations fence pending authentication, configuration and bind work. Stop
invalidates pending starts before waiting for teardown. Requests never hold
account/store guards during model streams. Connection tasks are tracked and
cancelled, including incomplete headers, uploads, response setup and response
bodies. Shutdown bounds draining and closes remaining sockets. Local
cancellation cannot promise cancellation of provider work already accepted
upstream or reversal of charges.

There are at most 32 accepted HTTP connections and 8 active requests. Request
permits are acquired before body buffering and retained until body completion
or drop. Excess requests receive 503; excess connections close. The shared
engine retains its 50 MiB body limit. Headers and uploads have bounded waits;
the engine owns inference setup and SSE idle timeouts. Persistent provider-cache
identity is intentionally separate; the existing ephemeral behavior is used.

The existing `maple-agent proxy` command remains an independent foreground
mode with its documented host/CORS/key options. Only one runtime may own a
given port. Both modes use the same browser-policy helper. Builds without the
`proxy` feature omit the managed service and its UI; proxy/headless builds do
not depend on GPUI.

## Verification

Run the component's pinned `just ci`. Focused `cargo test -p maple-agent-app
local_api` covers real loopback connections, permit lifetimes, blocked clients,
stale starts, redundant restore, locked controls, account/store teardown and
rendered Settings ownership across distinct execution profiles. The UI fixtures
exercise real private IPC and listeners with synthetic account effects; they
are not a real Iroh pairing or live backend inference check. Validate those
separately using an isolated linked Local workspace and the exact built app.
