# Desktop remote access

Remote access is off by default. The local `maple-agent host serve` daemon owns
its device identity, pairing records and Iroh endpoint. Desktop is a local IPC
client; selecting another computer opens a second, host-scoped IPC connection
through that daemon. The selected computer owns the tasks, workspace, tools,
MCP servers, computer use and send receipts. Its desktop and local ACP clients
continue to share those tasks. ACP is not exposed over Iroh.

## Using it

Open **Settings → Remote computers**. Under **This computer**, enable remote
access on the computer that will run tasks, then choose **Show pairing code**.
On the controller, use **Connect to another computer → Find computers**, choose
**Enter code** beside that computer, and enter its code before choosing
**Pair computer**. Code entry appears only for the selected computer; canceling,
refreshing discovery, or selecting a different computer clears it. Codes are
shown in eight individual boxes grouped as `ABCD-EFGH`. The single underlying
field supports typing, selection and paste with or without the dash. Filling it
does not submit a pairing attempt automatically. Both must
be signed into the same Maple account and deployment/channel. Account login
alone does not grant access. The code expires in five minutes and permits one
attempt. A failed attempt requires a new, locally opened window. Leaving Settings
keeps the current window alive; returning shows the same code and remaining time,
with Cancel available. Navigation never renews its deadline or attempt.

The sidebar's **View options** menu groups tasks by **Project**, **Connection**,
**Status**, or **In one list**. Connection becomes available after another
computer is added, including while it is offline; enabling remote access on
this computer alone does not add that choice. Project and Connection views keep
known empty projects visible. A globe marks remote projects. Selecting a task or
project also selects its execution computer; select a **This Computer** project
to return to local work.

**Sort tasks by** offers **Last updated** and **Manual order**, independently of
grouping. Project and connection positions stay stable in either mode and can
be rearranged by dragging. Dragging a task silently selects Manual order; this
changes only presentation order, preserving its project, execution computer and
status. The manual task order carries across grouping changes and restarts on
the viewing desktop. New tasks appear at the top; activity on existing tasks
does not reshuffle their manual order. These preferences belong to the viewing
desktop's signed-in account.

Project display names belong to the execution computer and the signed-in account.
**Rename project** in a sidebar project's menu updates that one name from
either its desktop or a paired controller. Every computer's folders have this
action, so renaming does not switch the current conversation or draft. Every
connected desktop receives the change, including computers whose projects are
visible only in the sidebar. Renaming back to the owner's folder
label, or **Maple Workspace** for its actual default workspace, clears the custom
name.
Renaming never changes the folder path, session working directories, or trust.
Names and the default-workspace identity arrive in initial/reconnect snapshots,
so controllers do not guess them from another computer's path syntax. Earlier app data is never imported, including project names and controller-only
project nicknames. New project names are stored only in this version's database. Task pins and computer nicknames remain viewer-local.

The computer name defaults to its OS hostname. In **Remote computers → Computer
name → Rename**, set the name advertised by this computer for this account and
profile. This works with remote access off and does not change the system
hostname. Other paired computers learn changes when refreshing discovery or
reconnecting; the name is not broadcast over active task streams. **Nickname**
on a paired row overrides its label on the viewing desktop only. Neither name
changes device identity or pairing authority.

Identity keys, grants, computer names and nicknames live in this computer's
account-scoped `store-v1/` records. They share password protection with the rest
of the current database. Erasing this version's local data leaves a stale peer
on other computers; remove it from **Remote computers** there. Earlier app data
is not imported. See [local storage](local-store.md) for scope and backup limits.

Each computer loads independently. The desktop first reads project/task
summaries and opens a transcript only when that computer is used. Cached groups
stay visible while switching computers or refreshing the same connection, and
during a disconnect alongside the connection status. Runs and task
updates arrive through the existing event stream; a small folder-list refresh
every thirty seconds also discovers empty projects selected by another desktop.
This is an in-memory navigation cache, not offline transcript replication. Search,
sidebar position and each computer's composer remain intact when switching.

With another computer added, the sidebar's **New project** button first asks
which computer should own the project, independently of the task being viewed.
Choose **This Computer** for the operating system's local folder picker, or a
remote computer for its built-in folder browser. Without another computer added,
the button opens the local picker directly. The remote browser starts at that
computer's home.

In Connection view, hover a computer heading and choose its **New project**
folder-plus button to open that computer's folder selector directly.
Browse folders, use the left arrow to go to the parent, enter an absolute path,
or create one new folder, then choose **Use folder**. The built-in browser also
serves as a fallback if the local platform picker is unavailable.
Browsing does not change the current project or its trust; creation happens only
when requested, and selecting a folder uses the existing project trust flow.
The execution daemon resolves paths and performs filesystem operations. Folder
lists are paged and bounded, and hidden folders can be shown explicitly. This is
a folder picker, not a filesystem sandbox for an already paired controller.
Files picked on the controller are explicit uploads to the execution computer.
Remote control frames remain limited to 16 MiB, including encoding overhead;
file uploads use separate bounded raw-byte streams and do not count against a
send's control frame.
Microphone, playback, billing and account operations stay on the controller.
The Integrations pane manages the selected execution computer and labels it;
account and application preference panes still use this computer. Native
permission setup must be performed on the execution computer.

Closing a window with remote access enabled or an unlocked password store keeps the application available
(macOS Dock; Windows/Linux tray Open/Quit). On a desktop without a usable tray,
Close offers Keep open or Quit Maple, explaining that Quit stops remote access.
Ordinary Quit stops that computer's local daemon, drains its work and waits for
ownership release. A shutdown failure keeps the frontend open with a warning
and retry. Closing a controller view does not cancel work
on another computer. Revocation and disabling access take effect locally even
when discovery is unavailable; re-enabling does not restore revoked grants.

CLI equivalents: `remote status`, `remote enable`, `remote disable`,
`remote discover`, `remote begin-pairing`, `remote cancel-pairing`,
`remote pair <endpoint>` (hidden code prompt), and `remote revoke <endpoint>`.
These administer only the local computer. Never put a pairing code in argv or logs.

A password-protected execution host starts locked and does not load remote
credentials or its identity until a local unlock succeeds. Unlock in its desktop
app or with `maple-agent unlock` using a controlling terminal. Controllers cannot
unlock, change passwords or erase a remote computer. Login alone does not unlock
the local store. Window close and OS screen lock do not lock an unlocked host;
Quit or `maple-agent host stop` completes local shutdown.

## Experimental remote updates

In **Settings → Remote computers**, each paired execution computer can report
its running application version and update status. **Refresh update status**
requests a new check by that computer's running desktop. Opening the view or
reading status in the background only reads its last reported updater state.
The explicit check requires a compatible peer with remote-refresh support;
older peers need a local check. Automatic downloads still follow the execution
computer's saved preference. Neither action installs an update or checks the
controller's installation. A failed check retains the last reported values as
history and cannot make a cached offer eligible for installation.
When available, the row shows the offered version and whether its verified
download is ready. Numeric package build numbers may be absent from running
executable metadata; the UI does not infer them from the protocol version.
Status reads remain available while a confirmed pairing from an older version
is recovered. If confirmation cannot be recovered, ordinary access is preserved
but remote installation requires confirming or pairing that computer again.

Installation requires the remote computer's running desktop updater and its
exact prepared, signed candidate. A daemon without its desktop reports that
installation is unavailable. Password-protected computers cannot install
remotely, including while unlocked. Update those computers locally; this feature
does not send passwords, transfer unlock keys or add an unlock handoff.

The controller does not need to run or download the execution computer's target
release first. Different reported application versions show a nonblocking warning;
they do not disable remote installation. The confirmation also names the target
and client versions when the update would leave them different. Version equality
does not prove protocol compatibility, and unknown versions do not imply a match.
The execution computer still binds its exact prepared, signed platform document
and checks its current authority and installation state. The controller's own
update and restart always remain manual, since it may be connected to other
computers.

The experimental confirmation names the execution computer and target version.
It warns that updating stops its work and remote access, that reconnecting can
fail and require direct access for repair, and that a later manual controller
update could affect compatibility with other computers. Continue only with
another way to access the execution computer.

A remote install request carries a stable operation ID and the exact candidate,
never a download URL, signing key or executable path. The host persists admission
and acknowledges the request before its desktop can consume it. A lost response
is uncertain: choose **Refresh update status**; the controller never automatically
resends installation. Interrupted host receipts are not automatically resumed
after a restart. The restarted host records success only when it observes the
requested version and source running, checking the package build when known.
That recorded outcome survives later updates. A changed or withdrawn offer needs
a fresh confirmation.
An explicit refusal before acceptance is shown as a declined request, rather
than an unconfirmed installation. Check the remote's status again before
reviewing another request. A transport failure that could have followed sending
the request remains uncertain and retains the original operation ID, even when
no receipt is visible yet.

A refusal before shutdown leaves the same verified download ready to try again;
it does not require quitting a healthy execution host. Uncertain shutdowns still
require checking that computer. If reporting a failed attempt is interrupted,
the execution desktop retries only the completion report, never installation.
Running-version visibility remains available when its update journal needs
attention, while remote installation stays blocked.

The journal currently retains at most 512 operation receipts per profile. Safe
retention beyond that limit is a follow-up: old operation identities must not
be forgotten while their target remains installable. Reaching the cap requires
updating that computer locally.

## Transport and recovery

Iroh 1.3.0 supplies authenticated QUIC, direct connectivity and relay fallback.
Maple explicitly clears public address lookup and builds Iroh without its
port-mapping feature. The Minimal preset alone is not the port-mapping policy.
There is no second encryption tunnel or custom routing layer. The remote
protocol keeps the `maple-agent/remote/1` ALPN family and currently advertises
wire version 7 with minimum version 7, independent of application builds. Its
shared project names, bounded navigation/history pages, stable row identities,
attachment ranges, and per-connection task subscriptions remain unchanged.
Project-change events carry only changed metadata; snapshots and project pages
provide authoritative names, revisions, and default-workspace flags.
Version 6 and earlier peers receive an explicit version refusal. Bounded frames
and an explicit task-operation allowlist apply.
The viewing desktop applies sidebar grouping and manual ordering to summaries
received through the existing navigation and project APIs. These preferences
stay on that desktop; no ordering parameters, commands, or capabilities are
added to the remote protocol.
Account operations and arbitrary whole-host administration cannot be forwarded.
The narrow experimental update capability uses the host's own signed updater;
it does not expose the ordinary host-stop command.

Optional discovery reports protocol version `7.1.0` and explicit capabilities
over separate request streams. The acknowledgement shape and ALPN remain
readable across the required version transition. Bounded folder browsing and single-folder creation require
the `project_folders_v1` capability. The controller checks support before sending
either folder command. A compatible host without that capability keeps ordinary
project/task access and its existing remote-update support; only the folder
browser reports that support is unavailable or an update is needed.

Since wire 7, task sends require attachment references. Images, ordinary files and archives
are copied into the execution daemon's session-owned plaintext file store;
SQLite contains metadata. Local imports use a controller-local staged path.
Remote imports send bounded metadata followed by raw bytes on a dedicated
low-priority stream, with admission before the body and a durable upload receipt
after finalization. The remote daemon never interprets a controller's local path.
The existing identified `send_message` then carries attachment IDs. An upload
retry cannot replay a prompt. Each authenticated connection has two bulk slots
separate from ordinary requests, range reads and urgent replies; generic files
are limited to 30 MiB each, while
composer images retain their 10 MiB limit. See [local storage](local-store.md).

Both computers must use wire 7 for remote access, including remote update
management. Upgrade the execution computer first while the old pair is still
compatible, then update the controller locally. A wire-7 controller stops
automatic reconnection after an explicit version refusal and shows **Update
required**, without treating ordinary offline errors as version mismatches.
Released wire-6 controllers keep their shipped behavior during the gap: cold
computer discovery can retry, and Settings can report the peer as unreachable.
Existing pairing records are retained. A disconnect alone does not confirm an
installation; check the running identity and retained receipt after reconnecting.
If the controller was updated first, update the execution computer directly,
then select that computer again to reconnect.

A bounded capability probe can fail without ending ordinary task requests.
Responses tolerate future optional fields and capability names. Experimental
remote installation continues to require its separate `remote_update_install_v1`
capability and the same signed-candidate, account, grant and updater checks.
Only validated folder support is cached, for the lifetime of that connection;
reconnecting checks it again. Failed discovery remains retryable rather than
being remembered as an unsupported host. Folder dispatch rechecks account and
grant authority after discovery, including when support was cached. Update
offers and installation readiness are always read afresh.
Input-only tool summaries require `tool_summary_start_v1`; without it, the
controller summarizes that call from its output once it finishes, as earlier
versions did, and the next call checks support again.

Four pre-admission handshakes and eight paired control connections have capacity
separate from local IPC. Each control connection bounds ordinary calls and keeps
separate capacity for cancellation/questions. Each request/reply uses an
independent bidirectional QUIC stream; the authenticated initial stream carries
ordered events. A bounded header admits each request before its attachment body
is read. Header parsing has an overall queue-and-read deadline. Stop and question
answers have reserved capacity and higher native stream
priority, so an incomplete upload or unread image reply cannot hold their dispatch
loop. Streams still share the connection's bandwidth; this is not a latency
guarantee on a saturated or unavailable network. Transfers use bounded frames and
inactivity deadlines. Uploads also have a ten-minute total deadline. Image
readback uses base64 on the remote wire.
A request failure or oversized reply returns a request error, while authority loss
or incompatible framing ends the connection. A failed connection recovers from an
authoritative snapshot. Transport loss does not cancel an accepted mutation on
the host or cause an automatic prompt replay; explicit host shutdown still stops
its work. A stable send identity
crosses both IPC and Iroh; only the execution daemon records its admission and
result, account-bound in its application database. An uncertain send keeps its original
identity, text and draft attachments in the desktop view and queries the execution
host's receipt after reconnecting. It never replays the command or overwrites a
newer draft. If the execution daemon restarts before recording a send's result,
it marks that receipt interrupted, preserving the identity against replay. The
message may already have taken effect: the desktop says so, retains/restores its
draft without overwriting newer input, and lets the user continue the task. A
pending receipt from a live daemon still blocks another send until resolved.
This is in-memory draft recovery, not persistence across desktop crashes.

A remote-enabled daemon retries transient startup failures with bounded backoff.
Local settings show whether networking is ready and retain actionable configuration
errors. Disable and revoke invalidate initialization already in flight; their local
effect does not wait for discovery publication.

After an abnormal exit, an older local daemon can still own this profile when an
updated application starts. The startup error offers a user-confirmed
Stop-and-reopen action; it stops the old host's tasks and remote access before
starting the new build. There is no automatic takeover.

The controller retries transient connection loss with bounded exponential delay
while the view remains attached. Known revocation and incompatible/unknown wire
messages terminate recovery. Recent validated address hints can be reused during
a discovery outage, but a daemon restart or new relay admission can still require
OpenSecret. No offline history replica or migration of workspaces is provided.

## Configuration and release boundary

Packaged builds default to Maple-operated relays for their environment:

| Profile | Relay URLs |
| --- | --- |
| Dev | `https://relay-dev.maple.ai/` |
| Prod | `https://relay-use1.maple.ai/`, `https://relay-usw2.maple.ai/` |

Iroh retains normal direct/relay path selection. There is no automatic fallback
to public relays or to the other environment's fleet. OpenSecret must be deployed
with remote enrollment support and its database migration in the matching
environment; each relay also needs its matching admission callback and service
credential. That credential stays between the relay and OpenSecret, never in
the client. Remote access remains off on a fresh profile; these defaults do not
start networking by themselves. Relay selection does not enable public discovery
or replace Maple account admission, pairing or local grants.

`MAPLE_REMOTE_RELAYS` overrides that default with an explicit comma-separated HTTPS
relay list. As with other environment settings, unset or blank means use the
profile default. An invalid nonempty override fails with a configuration error
rather than falling back to the profile default or public relays.
The default relay map is not a peer-relay allowlist: Iroh can also contact a
relay advertised in an authenticated peer's address hints.

Unpackaged validation builds additionally accept `MAPLE_REMOTE_RELAYS=public-test`.
Set `MAPLE_REMOTE_RELAY_ONLY=1` with either `public-test` or an explicit relay URL
list to remove IP transports and force relay traffic, even between two processes
on one machine. Packaged Dev and Prod ignore this validation flag and keep normal
path selection. `MAPLE_REMOTE_RELAYS=none` is available for direct-only development
and ignores the relay-only flag. These shortcuts remain unpackaged-only; an
unpackaged build without explicit relay configuration still reports an error.
The execution and controller profiles need separate XDG config/data roots and
proxy ports when testing on one machine. Do not reuse installed production state.

The encryption/enrollment contract and dependency qualification are in
[remote-security.md](remote-security.md). The backend relay callback and stock
relay deployment inputs are documented in
[OpenSecret remote enrollment](../../../services/opensecret/docs/remote-enrollment.md).
Infrastructure deployment, signed/native-permission validation, Windows/Linux
tray testing and two real machines remain distinct release gates.

See [implementation validation](remote-validation.md) for observed behavior and remaining trial gates.
