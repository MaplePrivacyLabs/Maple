# Local host and ACP

Desktop and `maple-agent acp` attach to one on-demand process, started by the
same installed executable as `maple-agent host serve`. The host owns credentials,
account configuration, the fresh `store-v1/` database, Pi runs, tools and child
processes. Desktop owns presentation and native permission prompts. ACP projects
host operations and events onto the client protocol. Pi's generic session-store
seams do not depend on Maple or SQLite. See [local storage](local-store.md) for
scope, unlock, durability, backups and release qualification.

Project MCP defaults are also host-owned, account-scoped and device-local.
Desktop draft switches save through the host; new desktop and ACP tasks take
the same project defaults. Existing tasks keep their own choices. Reconnecting
desktop reloads the draft's defaults and preserves unsent text and local choices;
a first send waiting on that reload is not resumed after a disconnect.

## Lifetime and isolation

Opening an ACP editor can start the host without opening a window. Desktop joins
it later. A locked store answers local status/control requests; unlock through
desktop or `maple-agent unlock` before login or ACP operations. The terminal
command uses a hidden controlling-TTY prompt, never argv/environment variables.
ACP stdout remains protocol-only.

Attached clients keep the host alive. In plain mode, the last intentional client
detach normally starts a two-second replacement grace before work drains and the
host exits. Unexpected connection loss reserves thirty seconds for recovery.
Incoming remote access and an unlocked password store independently keep the
host resident without clients. Closing the desktop window then hides/backgrounds
Maple through the Dock/tray; a desktop without a usable tray offers Keep open or
Quit. Without either residency condition, window close saves UI settings and
detaches only the frontend; other attached clients continue under the normal
host lifetime policy. There is no automatic lock on idle, screen lock or window close.

Ordinary app-menu, keyboard, Dock and tray Quit use one shutdown coordinator.
It checks active work, confirms when needed, flushes state, stops the exact host
and waits for ownership release before completing Quit. An explicit Quit accepted
before a soft close commits takes priority. After frontend detach commits, that
frontend exit finishes without claiming host stop or locking. A failure keeps the
frontend open with a warning/retry, including when no main window was visible.
A failed save keeps pending source and ownership without presenting a cancelled
runtime as running. Retry saves the retained source before a fresh runtime can
accept work; restoring the retained host also restores remote serving.
`maple-agent host stop` likewise stops this profile and all its tasks, even with
clients attached; a Stop acknowledgement is not completed shutdown. Force Quit
of only the frontend can leave the host unlocked. See [remote access](remote-access.md)
for the controller role.

There is no login item or permanently installed service. On Linux, logout or SSH
exit follows the OS user-session process policy and may leave the host running;
Maple does not change logind policy. Quit or stop the exact host before logout
when locking is required. Remote listeners are opt-in, and a never-used remote
profile does not initialize networking.

One OS user, build channel and canonical `store-v1/` path identify the storage
owner. The store's authoritative `owner.lock` prevents two owners even if a
temporary IPC endpoint disappears. Canonical resource/config roots are also
checked when attaching. Backend URL, billing
URL, client identity, attestation environment, executable source version and
compiled features must match before a client joins. A mismatched client reports
an error; it does not terminate or replace the owner. Desktop shows a startup
error window instead of panicking, with a user-confirmed Stop-and-reopen action
for an incompatible owner. It warns that existing tasks and remote access stop.
Explicit `maple-agent host stop` can stop the
owner of the same storage namespace even when its build or backend settings
differ; this exception does not allow other commands. Hosts predating that stop
exception must be stopped using their matching build/configuration. Quit the old
build before switching versions. A replacement waits for a shutting-down host to
release its ownership lock before opening shared state. Rebuilds of uncommitted
source share a source version, so stop
the development host before rebuilding.

Unix uses an owner-only socket inside a private directory and checks peer UID.
Windows uses a local named pipe restricted to its owner and SYSTEM and verifies
the pipe owner. This is an OS-user trust boundary, not a sandbox against other
programs running as that user. No API token is placed in endpoint names or logs.
Signing out revokes existing runtime handles and ACP connections before another
account can sign in. Task commands validate their account at the host boundary.
Checking saved credentials and attaching ACP share a read lock with local operations;
slow credential validation does not block history, settings, or Stop. Actual account
switches remain exclusive so a request cannot cross accounts while it is admitted.

## Shared tasks and recovery

A new draft remains absent from task lists until the first accepted message.
Desktop and ACP can load the same task and observe its history and live stream.
Each task has one running loop. Desktop retains its queue/steer controls; ACP uses
the steering extension below. Cancel from either interface stops the shared run.
Filesystem paths and tools always refer to the host machine.

ACP `session/load` freezes canonical saved rows into a private, temporary
SQLCipher database with a fresh memory-only key. It does not copy source entries
or retain a permanent archive. The local capture holds one source read snapshot
while copying compact rows, then releases it before client I/O; concurrent writes
can grow the source WAL until capture ends. Capture has a cooperative 30-second
deadline and free-space admission, with at most four captures per host. Memory is
bounded by caches and one reconstructed row, rather than the conversation length.
Scratch is closed and its key wiped on completion, cancellation or account/store
drain. After a process crash, owned startup cleanup removes the ciphertext whose
memory-only key is gone. A stalled filesystem call can delay cancellation.

Shared updates are buffered before capture, then replayed in order after the
snapshot's revision cutoff. Each ACP session has a 1,024-event/4 MiB encoded
backlog, including its in-flight update; one blocked replay does not stop the
account event receiver or another session. A session that exceeds this budget,
whose history is replaced, or whose authorized source disappears must be loaded
again; its authoritative task continues. Each retained live projection also has
4,096 tracked map entries and 32 MiB of string state. Reaching that resource limit
requires a reload instead of silently forgetting identities or text prefixes;
saved history still replays one row at a time without a conversation-length cap. A running task whose live overlay exceeds
the existing bounded replay allowance must settle before it can be loaded. Saved
history is never pruned to satisfy these limits.

Snapshots include live partial rows with an event cutoff. Clients discard older
updates and keep later ones, rather than appending the same partial text twice.
The event stream is bounded. A desktop that falls behind or loses its socket
reconnects for up to fifteen seconds, without starting a replacement host. It shows
the connection state and blocks new sends until task history, active runs, questions,
and queues have been refreshed. Unsent composer text stays in place. A snapshot
also resets host-local revisions, so a restarted host cannot leave old partials
on screen. An account change discovered during recovery switches the screen before
new events are shown. Failed recovery asks the user to reopen Maple.

Commands are never replayed on reconnection. A host crash does not restart inference.
Stored history survives; active tools are not promised to survive a host crash.
An unrecovered last-client disconnect eventually drains work after the recovery
grace when neither incoming remote access nor an unlocked password store requires
resident operation.

Desktop sends carry unique IDs. Account-scoped receipts in the application
database record admission and its result, so a lost reply can be queried without
resending the prompt. A
crash during admission produces an explicit unknown outcome: inspect the task
before retrying. Receipts contain hashes and task/run IDs, not prompt text or
images. They are not a token event log. IPC frames retain a 160 MiB ceiling for
bounded history and control replies; oversized messages fail explicitly.
Attachment imports use staged paths and bounded byte streams, not inline IPC
payloads.

History pages can shorten an oversized display row without changing its saved
source. Desktop **Show full output** reconstructs a row only within its 32 MiB
view budget; ACP replay permits at most 64 MiB of serialized JSON for one row.
Exceeding either limit returns an explicit error rather than pruning history.
Authorized row and attachment range reads transfer at most 1 MiB of raw data
per call before base64 encoding. These fragment APIs do not promise arbitrary
full-row layout or replay in either interface. See [history display and replay
limits](local-store.md#history-display-and-replay-limits).

## ACP compatibility

ACP requires an existing sign-in. With no saved account, the command prints a
sign-in instruction to stderr and exits unsuccessfully; stdout remains protocol-only.

The adapter uses stable ACP protocol version 1. It supports initialization,
new/load/list/delete, prompts, cancellation, model and `thought_level` config
options, command discovery, images, tool/thought/text updates, usage, and negotiated
form elicitation. It does not advertise session modes or `session/close`: ACP close
means cancellation, while this host's pipe EOF means detach. A client may still
explicitly cancel before closing; Maple honors that cancellation.

`session/load` streams saved history in bounded pages. A complete row larger
than the 64 MiB reconstruction limit stops replay with an explicit error; it is
retained in the store. Long task history and an arbitrarily large individual
row are different interface constraints.

Stable ACP has no steer or queue method. Maple advertises
`_meta.steering.supported=true` and implements `_session/steering`, shared with the
Codex and Claude adapters. It accepts the same `sessionId` and `prompt` content as
`session/prompt` and injects at Pi's next steering opportunity. The result is
`{"outcome":"injected"}`. With `_meta.steering.idleBehavior="promptRequired"`, an
idle task returns `{"outcome":"promptRequired","reason":"noRunningTurn"}` without
consuming the message. Legacy callers omitting that option start a new turn and
receive `{"outcome":"startedNewTurn"}`. Ordinary overlapping `session/prompt`
requests return busy; clients should use the advertised extension.

Questions appear on capable attached interfaces; the first answer resolves the
host request. Without a capable responder, the tool asks for a normal message
instead of waiting indefinitely. Client filesystem and terminal callbacks are
not used. Host-configured MCP servers stay host-owned. ACP-supplied bindings are
transient, disabled on detach and require explicit reattachment. Conflicting
server names are rejected rather than silently replacing another client's tools.

### Paseo

Configure a provider extending `acp` with command `["/path/to/maple-agent", "acp"]`.
It must inherit the same profile environment as desktop. In a managed development
workspace use its launcher (or source its generated Agent environment before
executing the built binary). A different profile is intentionally a different host.

The source baseline inspected for this change is Paseo `b2ca7dcf` (2026-10-15).
Its generic ACP provider supports the normal create/resume, prompt, tool streaming,
model/thought-level and cancellation path. It does not call `_session/steering`
and rejects an active second turn itself. Live validation with Paseo 0.11.1 also
confirmed desktop-originated user and assistant messages reach an already attached,
idle Paseo client, which can then continue the task without reopening it. Stable
ACP v1 does not supply a separate running/idle lifecycle for turns started by
another client, so transcript delivery does not establish full desktop status parity.
ACP cannot retract already rendered history on compaction;
reload is the authoritative view after history replacement. No Paseo-specific
wire shims or separate task database are added.

## Maintenance boundary

Keep host commands private and versioned; ACP remains a standards-based projection.
Test ownership, admission, snapshot/stream reconciliation, account revocation and
resource cleanup where they cross boundaries. Native macOS permissions, Windows
named pipes and Linux sockets each require platform execution evidence. Password
creation/change and experimental salvage remain host-enforced debug-only pending
native lifecycle, real Dock Quit and low-end KDF qualification. Plain SQLite is
the release path. Experimental salvage keeps source only for temporary build/
crash recovery; confirmed destructive reset does not create a permanent archive.
See [remote pairing security](remote-security.md) for the
separate transport and native trial boundary.
