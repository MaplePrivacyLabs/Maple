# Remote development

Status: built. This document describes the feature as it is in the code.
What the plan promised and the code does not do is under "Not built"; the
enclave relay is under "Later: relay". When code and this document
disagree, the code wins; update this document in the same change.

A Maple host runs the agent runtime and serves it over the network. A Maple
client is the desktop app, which drives its own local host and any number
of remote hosts. Connections are direct, over a LAN or a Tailscale network.
A later release adds a blind relay through the OpenSecret enclave without
changing the protocol above the transport.

## Goal and scope

Full parity: a client connected to a remote host gets what the local window
has, through the same `HostBackend` trait the window uses. The host owns
the runtime, the filesystem, the git checkout, the SQLite stores, the
integrations, and project trust. The client owns sign-in, billing, audio,
notifications, and its own display settings.

Out of scope: the relay, mDNS, a directory browser beyond suggestions,
remote sign-in, cached task lists for offline hosts, terminals, CUA over the
wire, running the client with the local runtime disabled, and connection
probing with automatic switching.

## Decisions

These are the answers given during planning, kept as recorded. Where the
build differs in detail, the sections below say what the code does.

- Full parity. A client connected to a remote host gets everything the local
  window has: queue, steering, permissions, questions, integrations, Codex and
  Claude Code delegation, context usage, tool summaries, and host settings.
  ACP is not the remote protocol. It stays a narrower surface for editors.
- Host forms. A new `serve` subcommand behind a `serve` cargo feature, default
  on, headless compatible. The desktop app can also serve, behind an "Allow
  remote connections" setting that is off by default. Linux and macOS hosts
  both matter.
- Host credentials. The host signs in on its own with `maple-agent login` and
  holds its own `auth.json`. There is no remote sign-in. When the host's
  refresh token is rejected, clients see "host needs sign-in" and nothing
  more.
- Pairing. A high-entropy single-use code shown on the host and typed on the
  client. The code is the only proof. No account proof. The host records the
  client's claimed user id for display only.
- Encryption. End-to-end between device keys with Noise, so a relay sees only
  ciphertext. The relay is not built in this release.
- Client model. One sidebar merges sessions from the local host and every
  connected remote host, with a host filter. New tasks go to the host chosen
  in the sidebar.
- Fan-out. Every connected client sees every event. Permission prompts and
  questions go to all clients. The first answer wins.
- Discovery. Manual address entry. No mDNS. The desktop app shows its listen
  addresses and pairing code in Settings.
- Project roots. Text entry plus the host's recent roots plus host-side
  directory suggestions. No native folder picker on any host, so local
  and remote selection are the same dialog.
- Session defaults are per host. Default permission mode, default web,
  harness instructions, and default model move from app settings into the
  host's per-account config.
- Local window. The local window calls the runtime in process and never loops
  through the protocol.
- Delivery. Host-assigned sequences, bounded snapshots, paged catch-up.
  A client must never miss a message.
- Slow clients. The host closes a socket whose outbound queue would cross the
  limit. It never blocks the run and never touches other clients.
- Reserved. A generic binary stream channel type in the framing so a PTY can
  be added later without a protocol change. CUA stays host-local.

## Glossary

Use these words for these concepts in this document and in new UI copy.
The transport code says "peer" for the other end of a connection, and UI
copy says "this machine" for the computer the app runs on; neither names a
host or a device.

| Term | Meaning |
| --- | --- |
| Host | A process that runs the agent runtime and accepts client connections. Every app instance is its own local host. Identified by its static Noise public key; the local host's id is `local`. |
| Client | The desktop app acting as a consumer of a host. |
| Device | A client identity, one static Noise key pair. One person may have several devices. |
| Connection | One way to reach a host. Today the only kind is `direct`, an address. A host has one or more connections. |
| Pairing | The one-time exchange that gives a device and a host each other's static key. |
| Session | A Maple task with its timeline, owned by exactly one host. The UI says "task". |
| Generation | A UUID a host mints when its process starts. Every event sequence is scoped to it. |

## Architecture

### The backend seam

`app/src/backend.rs` keeps the account-level concerns that never go over
the wire: sign-in and OAuth, billing, audio transcription and speech, update
checks, opening URLs, and desktop notifications. These use the client's own
OpenSecret session.

Everything a client drives on a host is the `HostBackend` trait in
`crates/maple-agent/src/host/mod.rs`, with two implementations:

- `LocalHostBackend` (`crates/maple-agent/src/host/local/`) wraps
  `AgentRuntimeHandle` in process and owns the host-side pieces the UI must
  not reach into: the filesystem, the git dir, and the account's SQLite
  stores. `AgentBackend::local_host` hands out one per account.
- `RemoteHostBackend` (`crates/maple-remote/src/client.rs`) speaks the wire
  to a `HostServer`. One instance is one connection; when the connection
  ends the instance is dead and its owner reconnects with a fresh one.

Hosts push `HostEvent`s: `Service` (a runtime event), `ProjectBranch`, and
`Resync`. `HostEventHub` fans one host's events out to every subscriber; it
is the runtime's event sink for the local host, and `HostServer` subscribes
to it for every connection. `LocalHostBackend` and `HostServer` are
siblings over the same runtime handle, not layers.

`HostServer` (`crates/maple-remote/src/server.rs`) serves any number of
connections. Requests are dispatched by the domain prefix of the method to
one controller each: `host`, `project`, `session`, `run`, `model`,
`integration`. A controller is a plain `match` over its domain's request
enum.

There is no router type. The chat screen (`app/src/ui/chat/hosts.rs`) keeps
one `ChatHost` entry per known host, maps every task id to the host that
owns it (`session_hosts`), and points its single `host` handle at the host
new tasks target. A call about a task goes to `backend_for(session_id)`;
every other call goes to the target. The local host is named
"This computer".

### What moved to the host

Four places in the UI used to touch the host filesystem directly. Each is
now a host method or a pushed event.

| Before | Now |
| --- | --- |
| Native folder picker returned a local path; `is_dir()` ran in the UI process. | `HostBackend::select_project_root(path)` registers the root on the host: a leading `~` expands against the host's home directory (a typed path arrives as written on the client), the path is canonicalized, and a non-directory is an error. `suggest_directories(query)` answers from the host: an empty query lists the home directory, `~` means home, hidden directories appear only for a dot prefix, at most 50 entries. Recent roots come from the host. |
| Git branch read and `notify` watcher ran in the UI process. | The host owns one watcher per root, shared and reference-counted across the clients that asked for it, and pushes `HostEvent::ProjectBranch` when a watch starts and whenever `HEAD` changes. Access-only filesystem events are dropped; there is no other rate limit. |
| UI opened `sessions.db` every second for the context ring. | The client polls `HostBackend::context_usage(session, model)`: while a run is active on the task on screen, a poller ticks every 5 s and re-reads only when the task's timeline changed since the last tick. Nothing is pushed. |
| UI opened `tool_summaries.db` read-write. | `HostBackend::tool_summaries(session)` and `store_tool_summary`. |

Image attachments a task already holds are read with
`read_image_attachment` and travel on a binary stream from the host.
Images the user attaches to a new message travel on a binary stream from
the client ahead of `run.send`, which names them by upload id (see
"Streams and credit"). Neither direction is bounded by the control
frame limit.

### Local-only capabilities

Integration setup runs where the window is. `RemoteHostBackend::
setup_integration` answers "set up integrations on the host itself" without
a wire call; there is no `integration.setup` method. Desktop notifications
are the client's: it raises them for the tasks it shows when notifications
are on and the window is not focused, and nothing in that path asks which
host owns the task. CUA stays host-local; the client learns nothing about a
host's CUA status.

### Integrations on a remote host

Claude Code, Codex, custom MCP servers, the shell tool, and project trust
run where the runtime runs. A remote client sees their output and answers
their permission cards. The enabled toggles and the MCP server list are
host-side (`integration.list`, `integration.set_enabled`,
`integration.list_mcp`, `integration.save_mcp`), edited from the client
through the host selector that Settings shows on host-scoped sections once
more than one host is connected.

A session a remote client creates goes through the host's
`create_session`, the same call the window makes, so it is a desktop
session like the window's. Sessions created by ACP callers stay out of
every client's task list.

## Protocol

The protocol lives in `crates/maple-remote`. Bottom up: `carrier`, `noise`,
`listen`, `dial`, `net`; `keys`, `pairing`, `devices`; `hosts`, `manager`;
`frame`, `rpc`, `streams`, `outbound`; `wire`; `server`; `client`. It
depends on `crates/maple-agent` for domain types and never on `app`.

### Carrier and framing

A `Carrier` is a struct of two boxed halves, a `FrameSink` and a
`FrameStream`. `carrier::in_process_pair` connects two in one process for
tests. The network carrier is a plain `ws://` WebSocket with Noise inside:
`listen::serve_listener` accepts connections for one `HostServer`,
`dial::connect_direct` opens one for a client, and `net` holds what both
share. There is no connector trait; the relay is a second dial function
later.

Every WebSocket binary message is exactly one Noise transport message, and
both roles cap WebSocket messages at 65535 bytes, the Noise maximum. A
frame is cut into pieces of at most 65535 - 16 - 1 bytes; each piece
carries one continuation byte (`1` more follows, `0` last) before the frame
bytes. A peer that reassembles past the largest control frame plus its
header is cut off.

The plaintext of a frame is:

```
[channel: u16 BE][kind: u8][payload]
```

Channel 0 is control and carries one JSON-RPC message per `Data` frame.
Channels 1 and up are binary streams. Kinds are `Open` (0), `Data` (1),
`Close` (2), and `Credit` (3). The control frame limit is 4 MiB; anything
larger belongs on a stream or must be paged. A stream data frame is at most
256 KiB. A short header, an unknown kind, or an oversized payload closes
the connection.

Every frame a side sends goes through one byte-bounded outbound queue,
64 MiB by default. A frame that would push the count past the limit is
refused and marks the connection for closing: the peer has stopped
draining, and the host never waits on a client. Each connection's
forwarder serializes its own copy of a broadcast; nothing is serialized
once and shared.

### Control channel and methods

Control messages are JSON-RPC 2.0 with numeric ids. Requests get exactly
one response. The `event` notification carries host events from host to
client. The keepalive is the `host.ping` request. The host answers requests
concurrently, each on its own task, so a slow call never delays the ping.

Error codes: the JSON-RPC reserved `INVALID_REQUEST`, `METHOD_NOT_FOUND`,
and `INVALID_PARAMS`, plus `HANDSHAKE_REFUSED` (-32000, the connection
closes after the answer), `NOT_READY` (-32001, `host.hello` has not been
sent), and `HOST_ERROR` (-32002, the host's `HostBackend` returned an
error; the message is the user-facing text). There is no parse error: a
control frame the host cannot decode closes the connection, because
nothing in it can be trusted to carry an id. The client drops an
undecodable control frame and logs it.

Methods are namespaced by domain and mirror `HostBackend`. The request
enums in `crates/maple-remote/src/wire.rs` are the source of truth; each is
a serde enum tagged by `method` with the variant's fields as camelCase
`params`, and the `*_METHODS` constants list every name (a test checks
them against the enums):

```
host.hello, host.ping, host.bootstrap, host.start_runtime,
host.stop_runtime, host.session_defaults, host.set_session_defaults,
host.save_default_model, host.usage_summary, host.context_usage,
host.tool_summaries, host.store_tool_summary
project.recent_roots, project.select_root, project.remove_root,
project.suggest_directories, project.watch, project.unwatch,
project.trust, project.set_trust
session.list, session.create, session.load, session.timeline,
session.rename, session.set_state, session.delete, session.compact,
session.subagents,
session.cancel_external_agent, session.set_permission_mode,
session.set_web_enabled, session.read_attachment
run.send, run.cancel, run.cancel_queued, run.begin_queued_edit,
run.end_queued_edit, run.answer_question, run.permission_respond,
run.ask_side_question, run.summarize_tool_call, run.summarize_thinking
model.list, model.supports_vision, model.slash_commands,
model.resolve_slash_command
integration.list_session_mcp, integration.set_session_mcp,
integration.list_mcp, integration.save_mcp, integration.list,
integration.set_enabled
```

There are no response enums. A response is the `HostBackend` return type
serialized, with four exceptions: `host.bootstrap` answers a
`BootstrapSnapshot` (the bootstrap with the newest task's timeline
stripped, plus `latestTimelineLen`), `session.load` answers a
`SessionSnapshot` (the detail with an empty timeline, plus `timelineLen`),
`session.timeline` answers a `TimelinePage` (`items`, `hasMore`), and
`session.read_attachment` answers an `AttachmentHandle` (`stream`, `len`).
`host.ping` answers `{}`. There are no `device.*` methods.

Compatibility rules for everything in `wire`:

- Schemas are append-only. New params are `Option` with a serde default;
  unknown fields are ignored on both sides; a field that stops being sent
  stays accepted. Enums are not `#[non_exhaustive]`.
- Every compatibility shim carries a dated tag:
  `// COMPAT(name): added in vX.Y, remove after YYYY-MM-DD once host floor >= vX.Y.`
  `rg 'COMPAT\('` is the cleanup backlog.
- `protocol` is a tripwire, bumped only for a change no feature flag can
  express. Real evolution goes through the `features` bags in the
  handshake. Today both sides send the same table and neither gates a
  call on the other's flags; the client checks only the protocol version.

`AgentServiceEvent`, `AgentRunEvent`, and the request and response types
in `crates/maple-agent/src/agent/types.rs` carry `Serialize` and
`Deserialize`. That is the wire contract; review those types with that in
mind.

### Streams and credit

Either side sends streams. The side that sends the bytes opens a stream
with an `Open` frame on a free channel: clients open odd channels, hosts
even ones, so the two directions never collide. The `Open` payload is
JSON: `purpose` (`attachment` or `upload`), `requestId` (for an
attachment, the JSON-RPC request the stream answers), `uploadId` and
`mime` (for an upload), and `len`, required for uploads. The receiver
starts the sender with 16 frames of credit and grants 8 more each time
it has consumed 8, so one slow transfer never fills the connection's
outbound queue. The sender ends the stream with an empty `Close`, or
early with `Close { "error": "..." }`; a sender dropped mid-stream sends
`Close { "error": "the sender gave up" }` so the peer frees the slot. The
receiver answers every stream with its own `Close`: empty to acknowledge
it, or with an error to refuse it at the `Open`, mid-stream, or when it
ended short of `len`. The receiver collects the bytes whole and
preallocates at most 64 MiB on the announced length.

An attachment read works like this: the host reads the bytes, opens the
stream with the request id, then answers the RPC with the channel and
length. The open frame precedes the answer on the same ordered carrier, so
the client pairs the collector with the answer by request id and waits for
it. A failed read is an RPC error before any stream opens. When the RPC
fails or the wait runs out, the client abandons the request and drops any
collector opened for it, so late frames are discarded.

#### Uploads

An image the user attaches to a message travels before the message. For
each attachment the client mints an upload id, opens a stream with
`purpose: "upload"`, the id, the mime type, and the byte length, sends
the bytes under credit control, closes, and waits for the host's
acknowledging `Close`. It then sends `run.send` with `{ request,
uploads: [{ uploadId, name }] }`; the request's own `attachments` must be
empty on the wire. The host rebuilds each image from the stored bytes
and the mime type before calling the runtime. A request that names an
unknown or incomplete id is `INVALID_PARAMS`, and a request naming
several ids consumes all of them or none.

The host keeps at most 4 uploads in flight and 16 completed but not yet
referenced per connection, dropping the oldest, and at most 10 MiB each;
the client refuses larger images before opening a stream. Upload ids and
mime types are short printable ASCII, and a mime type may not contain
`,` or `;`, so it cannot alter the rebuilt data URL. Every upload dies
with its connection. Both sides advertise `uploadStreams`; a host without
it makes the client refuse attachments with "update the host", and a
client without it that sends inline images is refused with "update the
client".

### Sequence and resync

`HostServer` subscribes a connection to the host's events before it
answers the hello, so nothing is lost between the two; the forwarder holds
events until the client is ready. Every event on a connection carries one
monotonic `seq`, starting after the `seq` in the host's hello (0). The
client accepts `seq == expected` and treats anything else as a gap: it
publishes `HostEvent::Resync`, then the event. On `Resync` the UI re-reads
the task list and reloads the task on screen. A reconnect is a new
connection and a new `RemoteHostBackend`, and always resyncs.

The host keeps no event log and no per-entity cursors. `session.load`
builds a snapshot, keeps it for the connection (at most 8, least recently
paged first), and `session.timeline` pages it by item count and by bytes
(200 items or 1 MiB per page, at least one item always fits) until
`hasMore` is false. A page for a task the connection never loaded loads it
first. The bootstrap's newest task is paged the same way. Live events
emitted while a snapshot loads are also in the snapshot; applying them
again is idempotent because timeline items are keyed by id.

Each socket is independent: there is no logical client session across
sockets, and nothing survives a dropped socket. Permission answers go
straight to the runtime, which removes the pending request on the first
answer; a second answer to the same request fails as a host error ("No
pending Agent Mode permission request found"). Queue edits and steering
are last write wins with the host authoritative.

### Handshakes

Two Noise handshakes, both with the `snow` crate's default resolver:
pairing runs `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s` with the one-time code
as the pre-shared key, and every later connection runs
`Noise_IK_25519_ChaChaPoly_BLAKE2s` with the host's pinned static key. The
first byte of the client's first message names the handshake (`1` pair,
`2` session) and selects the prologue (`maple-remote-v1/pair` or
`maple-remote-v1/session`), so a relay cannot swap one for the other. In
the pairing pattern the client sends the last handshake message, so the
host sends one empty transport message once it has spent the code and
recorded the device; the client trusts nothing until it decrypts it. In
the session pattern the client's static key arrives in the first message,
and the host refuses an unpaired key before answering anything. The client
refuses a host whose static key does not match the pinned key.

After the Noise handshake the client sends `host.hello` as its first
request, and the host answers with its own hello:

```json
{
  "protocol": 1,
  "appVersion": "0.1.0",
  "build": "63bcff5c",
  "pcrEnvironment": "Production",
  "features": { "timelinePaging": true, "attachmentStreams": true,
                "uploadStreams": true, "ping": true },
  "device": { "publicKey": "...", "name": "bens-laptop", "userId": "..." }
}
```

```json
{
  "protocol": 1,
  "appVersion": "0.1.0",
  "build": "63bcff5c",
  "pcrEnvironment": "Production",
  "generation": "uuid",
  "seq": 0,
  "features": { "timelinePaging": true, "attachmentStreams": true,
                "uploadStreams": true, "ping": true },
  "host": { "id": "<host public key>", "name": "workstation", "userId": "..." }
}
```

`build` is the git revision `build.rs` baked into the binary (`abc1234`,
or `abc1234-dirty`), so two builds of one package version can be told
apart; it is optional and absent from a build outside a git checkout or
from an older peer. The host logs the client's name, key, version, and
build on connect ("client bens-laptop (...) connected running maple-agent
0.1.0 (63bcff5c)"); the client keeps the host's for the Settings screen.

The host refuses the hello with `HANDSHAKE_REFUSED` and closes when the
protocol differs, when `pcrEnvironment` differs (two binaries built for
different enclaves cannot share a backend), or when the hello names a
different device key than the handshake proved. Any other method before
the hello gets `NOT_READY`; a second hello gets `INVALID_REQUEST`. The
device name is cleaned before it reaches a log or the device list: control
characters removed, at most 64 characters. `HostServerConfig::
on_client_hello` runs once per connection after the hello is accepted; the
app's hook records the device's claimed name, user id, and last-seen time.

### Liveness

Four budgets. None is inferred from another.

| Budget | Value |
| --- | --- |
| Connect | 15 s: the host gives the WebSocket and Noise handshakes one deadline; the client gives the WebSocket connect, the Noise handshake, and the hello answer 15 s each |
| Application ping | Client sends `host.ping` every 10 s with a 15 s timeout; 2 consecutive misses close the connection |
| Host lease | 45 s, running from the moment the connection is created, renewed by any inbound frame, checked every 10 s, close on expiry |
| RPC | 60 s default; 90 s for `host.start_runtime`. A timeout is an operation failure, never proof the socket is dead |

A closing host connection waits up to 2 s for its queued frames (a
refusal, an error answer) to reach the peer before the writer is
abandoned, then releases every project watch the connection placed.

Reconnect uses full-jitter exponential backoff: an exponential delay from
1 s to a 30 s cap, jittered between half and all of it, reset on a
successful connection. There is no foreground probe.

### Pairing

1. On the host, `maple-agent serve pair` or the desktop app's "Generate
   pairing code" button writes a pending pairing record to
   `<local data>/remote/accounts/<scope>/pending_pairing.json` at mode
   0600 and shows the code. The running listener reads the file on every
   incoming connection, so a running host needs no restart. A record is
   valid for 5 minutes and is spent by the first pairing that completes;
   an expired record is removed when it is next read.
2. The code is 80 bits of randomness shown as sixteen Crockford base32
   characters in groups of four (`XXXX-XXXX-XXXX-XXXX`). Parsing accepts
   any case, ignores dashes and spaces, and maps the usual confusables.
   The pre-shared key is SHA-256 over a domain tag and the code.
3. The client dials the address with the code and runs the pairing
   handshake. Both sides learn each other's static key in it.
4. Before confirming, the host spends the code (a code already spent or
   replaced refuses the pairing, so of two devices racing on one code
   exactly one succeeds) and records the device in
   `<local data>/remote/accounts/<scope>/devices.json`: public key, the
   name "new device" until the hello names it, claimed user id, paired
   at, last seen. The client saves the host in its per-account
   `hosts.json`: public key, name (the host's announced name unless the
   user gave one), the address, paired at. Pairing again with a known key
   merges into the saved record.
5. Failed pairing handshakes are counted per source address: 5 in 10
   minutes lock the address out of pairing until the window passes, with
   at most 1024 addresses tracked (past that the address with the oldest
   failure is forgotten). Only pairing-mode failures count, so a revoked
   device that keeps retrying a session handshake cannot lock its address
   out of pairing again. A locked-out address is offered no pre-shared
   key; its session handshakes still work. Failures never delete the
   pending record.

Hosting serves one account's runtime, so devices and codes belong to the
account that is hosting: a device paired while account A was signed in is
not admitted by a host of account B, and a code published for A never
pairs a device into B. `serve pair` and `serve devices` resolve the account
from the saved sign-in and refuse to run without one. The host key and the
lock stay per machine.

Revocation is an edit to the device file. The listener checks every 10 s
whether each connected device is still paired and drops a revoked device's
connection at the next check. A revoked device's next dial is refused in
the handshake.

### Devices and hosts stores

Keys are X25519 pairs generated on first use and stored as
`{ "private": "...", "public": "..." }` (base64url, no padding) at mode
0600. Keys never appear in logs; `Debug` on a key shows only the public
half, and `Debug` on a code or pending record hides the code.

The client's `hosts.json` is one file per account:

```json
{
  "hosts": [
    {
      "id": "<host public key>",
      "name": "workstation",
      "connections": [
        { "kind": "direct", "address": "100.64.0.7:7130" },
        { "kind": "direct", "address": "192.168.1.20:7130" }
      ],
      "pairedAtMs": 0,
      "lastSeenVersion": "0.1.0",
      "lastSeenBuild": "63bcff5c"
    }
  ]
}
```

Adding a connection whose host presents an already-known public key merges
into that host. `lastSeenVersion` and `lastSeenBuild` are what the host
announced at its most recent hello and are rewritten on every connect;
both are optional, so a file from before they existed still loads. Loading
salvages per entry: a malformed connection is dropped, not the host, and a
malformed host is dropped, not the file.

The host's `devices.json` is `{ "devices": [ { "publicKey", "name",
"userId", "pairedAtMs", "lastSeenMs" } ] }`. Revoking by name is refused
when several devices share it; revoke by key. The pending record is
`{ "code", "createdMs", "expiresMs" }`.

## Host role

### The serve command

```
maple-agent serve                     Listen for paired clients.
maple-agent serve pair                Publish a one-time pairing code.
maple-agent serve devices list        Paired devices.
maple-agent serve devices revoke DEV  Forget a device by key or name.

--listen ADDR:PORT   bind address (default 0.0.0.0:7130, env MAPLE_SERVE_LISTEN)
--name NAME          host name clients show (default: hostname, env MAPLE_SERVE_NAME)
```

`serve` binds every interface by default, because pairing is the gate;
give one address (a Tailscale IP) to narrow it. The default port is 7130,
not 8080, which the proxy mode uses. A port that cannot be bound is an
error; nothing else is tried. The host name comes from `HOSTNAME` in the
environment, else `gethostname`, else "maple".

`serve` requires a saved sign-in and exits with a message otherwise. A
saved sign-in the server rejects exits with a message to run `login`
again. A server that cannot be reached at start does not: the host serves
with the saved sign-in, requests fail until it goes through, and the
sign-in is retried behind them with growing pauses (5 s, doubling to
5 minutes), so a unit that starts before the network recovers on its own.
Session defaults an older version kept in `settings.json` are adopted into
the account config by every mode that binds an account, including the
window after a sign-in.

`serve pair` prints the code on stdout and guidance on stderr, including
the running host's name and address when one runs; it learns that from
`serve.json`, read only while the hosting lock is held, so a crashed host's
leftover state is ignored. `serve devices revoke` accepts a public key or
a name.

`serve` runs under systemd: it stops on SIGTERM as well as Ctrl-C, and when
`NOTIFY_SOCKET` is set (`Type=notify`) it sends `READY=1` once the port is
bound and `STOPPING=1` on the way out. Stopping waits for the listener and
its connections to end before releasing the lock, so a restart right after
can bind. A user unit:

```ini
[Unit]
Description=Maple host
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
NotifyAccess=main
ExecStart=%h/.local/bin/maple-agent serve --listen 100.64.0.7:7130
Restart=on-failure
RestartSec=5
TimeoutStopSec=15

[Install]
WantedBy=default.target
```

Run `maple-agent login` once as that user first, then
`systemctl --user enable --now maple-serve`; `loginctl enable-linger` keeps
it up after logout.

### Desktop hosting

`app/src/remote/` holds both roles: `host.rs` the host role, `client.rs`
the connection manager for saved hosts, and `mod.rs` the files both keep
under `<local data>/remote/`. `Hosting::start` takes the data-root lock,
loads the host key, binds, writes `serve.json`, and serves the account's
local host on the backend runtime. The `serve` command runs it in the
foreground; the window runs it behind the "Allow remote connections"
setting, off by default. The command and the window share the host key,
the lock, and, for one account, the device list and the pending code, so
only one of them serves at a time; the other reports who holds the root.
The lock is a file lock, so a crashed host leaves nothing that blocks the
next start.

The window's host role (`HostingController`) has four states: off,
starting, listening, and failed. Starting runs on the backend runtime,
never the UI thread; a stop that arrives meanwhile wins. The setting
persists as on only once the host listens, so a start that failed (the
port taken, another host on the root) does not come back at the next
launch; the failure shows in place. Turning the setting off stops hosting
at once. At launch, hosting starts when the setting is on.

### Files

| Path | Owner | Contents |
| --- | --- | --- |
| `<local data>/remote/host_key.json` | host | This machine's static Noise key as a host, 0600 |
| `<local data>/remote/device_key.json` | client | This machine's static Noise key as a client device, 0600 |
| `<local data>/remote/serve.lock`, `serve.json` | host | The running host's lock and its listen address, name, and key |
| `<local data>/remote/accounts/<scope>/devices.json` | host | Devices paired into this account on this host |
| `<local data>/remote/accounts/<scope>/pending_pairing.json` | host | The pairing code published for this account, until used or expired, 0600 |
| `<local data>/agent/accounts/<scope>/hosts.json` | client | Hosts this account paired with: key, name, addresses |
| `<config>/agent/accounts/<scope>/config.json` | host | Existing `AgentConfig`, including session defaults |
| `<config>/settings.json` | client | Client settings, including `allow_remote_connections`, `last_task_host`, and per-host UI state under `hosts` |

`<scope>` is the SHA-256 of the account's user id. The `remote/`
directories are created owner-only.

## Client role

### Connection manager

`maple_remote::manager::HostManager` runs one connector task per saved
host on the backend runtime. A connector dials the host's connections in
order and uses the first that completes a handshake, hands the UI a
connected `RemoteHostBackend`, forwards the host's events, and reconnects
with the backoff above when the connection ends. Pairing dials with the
code, saves the host, and starts its connector on the connection the
pairing opened. Everything the UI needs arrives as `HostManagerEvent`s on
one channel: `Status { host, name, status, backend }` (`backend` is
present exactly when the status is `Online`), `Event { host, event }`, and
`HostsChanged(saved hosts)`. The desktop shell pumps that channel into the
chat screen in batches of up to 256, like the local host's events.

The status states are `Connecting`, `Online`, and `Offline { reason }`.
Removing a host cancels its connector and reports
`Offline { reason: "removed" }`. A replaced or removed connector says
nothing more once it is superseded. The manager also answers
`is_online(id)` and `host_version(id)` (the version and build the live
connection's hello announced, `None` while offline) for the settings
screen, and writes that version to the saved host on every successful
hello. `rename` exists on it and the store, but no UI calls it.

### Client settings

`<config>/settings.json` stays client-only: theme, fonts, vim modes,
shortcut overrides, notifications, reduce motion, window state, TTS voice
and speed, and the tool details and tool summaries display defaults. It
also holds `allow_remote_connections` (default false), `last_task_host`
(the remote host the last new task was created on; absent when it was the
local host), and `hosts`, a map from host id to `HostUiState`: pinned
tasks, settled and unsettled tasks, and project display names keyed by
path on that host.

Per host, in the host's per-account `AgentConfig`: default permission
mode, default web enabled, harness instructions, and the default model.
`HostSessionDefaults` carries all four; `set_session_defaults` writes the
first three and leaves `default_model` alone, so a stale settings snapshot
cannot put an old model back; the chat screen saves the model through
`save_default_model`. Values an older app kept in `settings.json` migrate
once into the local host's config; values the config already holds win.

### Sidebar and tasks

The sidebar merges tasks across hosts, one row per task, sorted as before.
`HostBootstrap` reads a host's saved project root, task list, recent
roots, newest task, and session defaults in one call; a remote host is
read that way when it connects, and its runtime is started. When more than
one host is known, each row shows the host name after the project name
(`project · host`), and the project switcher menu gains a host block above
the project rows: every host, then "All hosts". Offline hosts stay listed,
grayed, so a host that dropped is still visible; their tasks leave the
list until the host is back (the task on screen stays readable), and a
filter on an offline host is refused with a notice. New tasks go to the
host the filter names, else the selected task's host, else the local host.
"New Task" shows an empty draft and creates nothing; the task is created
on the target host when the first message is sent, carrying the draft's
mode, model, web access, and integration toggles, so the host or project
can still change before then and a draft that sends nothing leaves no
row behind.

When a host drops after having been online, a notice names it once; the
reconnect attempts that follow report nothing more until it is back. An
answer from a connection that has since dropped or been replaced is
stale and is discarded.

### Project selection

Choosing a project is one dialog for every host (`app/src/ui/chat/
picker.rs`): a search box over the target host's recent projects and its
directory suggestions, and an "Open this path" row when the text starts
with `/` or `~`. Arrows, Enter, and Escape drive it. There is no native
folder picker on any host.

### Host chip and restore

With a task open, the header names the host that task runs on as a
badge with a status dot; a task never moves, so there is nothing to
switch. On the new-task screen the same place holds a chip that names
the host new tasks run on and switches it from a dropdown; the sidebar
filter and the selected task move the target too. Both appear only when
more than one host is known.
Switching the target adopts that host's project root, recent roots, and
session defaults; the draft on screen follows, since no task exists until
its first message is sent.

The host the last new task ran on is saved in the client settings and is
the target again at the next launch: startup holds the local auto-select
until that host connects, then makes it the target if nothing was chosen
meanwhile, shows its saved project, and opens its latest task with its
stored tool summaries. A remembered host that reports offline, is no
longer saved, or fails its bootstrap releases startup to the local
auto-select.

### Settings

Settings has one Hosts pane for both roles. For the client role it pairs
(address, code, optional name) and lists the saved hosts with their
connection state and a remove action; there is no rename UI. Each row
shows the version and build the host announced, as `0.1.0 (63bcff5c)`
while online and `last seen 0.1.0 (63bcff5c)` while offline, and a line
comparing it with this app: "Behind this app; update the host" when the
host's version is lower, "Different build from this app" when only the
build differs, "Newer than this app; update this app" when it is higher,
and "Update available: <version>" when the update check found a release
newer than the host. The rows are computed when the list or a host's
state changes; while the pane is shown it polls the manager once a
second and re-renders only when a row changed. For the host role it
shows the "Allow remote connections" toggle and its state:
"Not listening", "Starting", the bound socket, or, when the host binds
every interface, the port with a note to use the machine's LAN or
Tailscale address. "Generate pairing code" works only while listening; the
code stays on screen until the host consumed it (the device list is then
re-read) or it expired. Below that, the paired devices with a revoke
action. Device and pairing files are read and written off the UI thread.

Host-scoped sections (session defaults, system prompt, integrations, MCP
servers, usage) get a "Host" selector once more than one host is
connected; choosing a host re-reads everything the section shows from
that host.

## Authority and trust

A paired device has the same reach as the desktop window on that host.
Project trust is enforced host-side. Sessions created by ACP callers stay
hidden from clients. The host records the claimed user id for display
only; the pairing code is the whole proof, and the pinned static keys are
the identity afterwards.

The host never logs access or refresh tokens, plaintext prompts, or
credential-bearing environments, per the repository security rules.
Pairing codes and private keys are never logged.

## Limits and constants

| Constant | Value | Where |
| --- | --- | --- |
| Control frame | 4 MiB | `frame::MAX_CONTROL_FRAME_BYTES` |
| Stream data frame | 256 KiB | `frame::MAX_STREAM_FRAME_BYTES` |
| WebSocket message | 65535 bytes | `net::MAX_WEBSOCKET_MESSAGE_BYTES` |
| Outbound queue | 64 MiB | `outbound::DEFAULT_MAX_OUTBOUND_BYTES` |
| Stream credit | 16 initial, 8 refill | `streams::INITIAL_CREDIT`, `CREDIT_REFILL` |
| Kept snapshots per connection | 8 | `server::MAX_KEPT_SNAPSHOTS` |
| Watched roots per connection | 64 | `server::MAX_WATCHED_ROOTS` |
| Timeline page | 200 items, 1 MiB | `HostServerConfig` |
| Close flush | 2 s | `server::CLOSE_FLUSH_TIMEOUT` |
| Lease | 45 s, checked every 10 s | `HostServerConfig` |
| Handshake / connect | 15 s | `net::HANDSHAKE_TIMEOUT`, `ClientConfig::connect_timeout` |
| Ping | every 10 s, 15 s timeout, 2 misses | `ClientConfig` |
| RPC | 60 s; 90 s for runtime start | `ClientConfig` |
| Backoff | 1 s to 30 s, full jitter | `manager` |
| Pairing code | 16 chars, 80 bits, 5 min | `pairing` |
| Pairing limiter | 5 failures per 10 min, 1024 addresses | `pairing::PairingLimiter` |
| Revocation check | every 10 s | `listen` |
| Device name | 64 chars, no control characters | `devices::MAX_DEVICE_NAME_CHARS` |
| Directory suggestions | 50 | `directories::SUGGESTION_LIMIT` |
| Context usage poll | every 5 s while a run is active | `ui/chat/mod.rs` |

## Testing

Unit tests sit beside each module in `crates/maple-remote/src/`: frame
round trips and refusals, the WebSocket cap, Noise reassembly limits,
outbound overflow and oversized frames, stream credit and abandonment,
pairing codes and the limiter, the device and hosts stores, request
decoding and the method lists, and manager removal and backoff.

Integration tests in `crates/maple-remote/tests/` run a `HostServer` over
a scripted `FakeHost`:

- `loopback.rs`, over the in-process carrier: handshake refusal for a
  different environment and protocol; snapshots page completely and
  calls round-trip; events arrive in order through the hub; attachments
  stream whole and a missing one is an error; a sequence gap publishes a
  resync before the event; a client that stops draining is closed without
  blocking the host; a quiet peer loses its lease and a pinging client
  keeps it; a second hello is refused and the hook runs once; integration
  setup is not a wire method; watches are capped and released when the
  connection ends; requests before the handshake and unknown methods are
  refused; a large attachment streams to the host ahead of the send; an
  oversized upload is refused and the connection stays usable; a send
  naming an unknown upload or carrying inline images is invalid params;
  uploads die with their connection.
- `transport.rs`, over a real listener with Noise: a device pairs,
  reconnects, and is refused once revoked (on its next dial); repeated
  wrong codes lock the address out; one code pairs exactly one of two
  racing devices; a revoked device reconnecting does not lock out pairing
  again; large frames cross the Noise carrier in pieces; an oversized
  WebSocket message is refused by both roles.
- `manager.rs`, the connection manager over the same listener: pairing
  records the host's version and build, `host_version` answers them while
  online and `None` once the host is gone, and the saved host keeps them.

Not covered: two clients answering one permission prompt, a revoked
device's live connection being dropped mid-session, and the desktop
app's host and client roles end to end.

## Not built

The plan promised these; the code does not do them.

- A pending pairing record deleted after a failure window. Failures only
  feed the limiter.
- Host-level state cursors: generation-scoped entity sequences,
  `changes`/`removals`/`snapshot` answers, and tombstones. The client
  re-reads everything on `Resync` and on reconnect.
- Per-session epochs and a bounded first resume with `has_older`.
- `device.*` RPC methods.
- `#[non_exhaustive]` wire enums with an `Unknown` fallback.
- General feature gating. The client checks the protocol version and
  the `uploadStreams` feature; nothing else is gated.
- Logical client sessions surviving 90 s across sockets, the permission
  in-flight guard, and "answered on another device". Each socket is
  independent; a second answer is a host error.
- A broadcast serialized once and filtered per socket. Each connection's
  forwarder serializes independently.
- A foreground probe with a 3 s deadline that bypasses backoff.
- A lease claimed by the first ping. The lease runs from connection
  creation.
- A default port used "unless taken". A taken port is a hard error.
- Integration setup edited from the client. Setup is refused remotely and
  done on the host.
- The client showing the host's CUA status.
- Pinned roots re-keyed by host, and host-side tool details and tool
  summaries display defaults. Those display defaults stay client-side.
- Streams opened by a control message. A stream opens with an `Open`
  frame on its channel, and the RPC answer names the channel.
- A host rename UI. The store and manager can rename; nothing calls them.
- Loopback tests for epoch change, generation change, a permission race,
  and revoke mid-connection.
- Compatibility tests that round-trip every wire type with unknown
  fields; only the hello is tested that way.

## Later: relay

The relay lives inside the OpenSecret enclave and sees only ciphertext.
Design constraints to honor when it arrives, so nothing above changes:

- The relay uses `wss://` to the enclave with the same Noise inside. The
  enclave ingress is Cloudflare, nginx, socat, then axum, with a 300 s
  idle timeout; the 10 s application ping already satisfies it, so the
  ingress needs only an nginx upgrade block.
- One persistent host-to-relay connection carrying a mux with explicit
  per-stream flow control. The relay buffers nothing and never drops a
  frame on the host's behalf. No dial-back-per-client topology.
- Rendezvous by device public key. The host publishes reachability through
  the user's encrypted KV store.
- A second dial function beside `connect_direct`, and a `relay` variant of
  `HostConnection`. Connection probing with first-available activation and
  hysteresis lands here.
- The handshake's mode byte doubles as the Noise prologue, so a relay
  cannot swap the pairing and session handshakes.
