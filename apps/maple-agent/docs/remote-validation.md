# Phase 2 implementation validation

Local validation on 2026-10-19 used macOS 27.0.1 / Apple Silicon and the
`agent-remote-p2` managed Local environment. The controller and execution daemon
had independent config/data roots, endpoint keys, proxy ports and debug bundle
identities. The installed production Agent, other development apps and Paseo
were not reconfigured. Test accounts and credentials remained inside the Local
MDE; no production OpenSecret account or backend was used. Live model calls used
the Local backend's configured provider.

## Observed behavior

- Two isolated daemons registered/discovered through the Local OpenSecret's real
  Transport V2 routes, then paired with CPace through public Iroh relays. IP
  transports were explicitly disabled. The focused transport test additionally
  asserted a relay path and pinned peer identities. A separate loopback UDP test
  covered a direct path; this is not evidence of real LAN/NAT traversal.
- Remote task lists matched the execution daemon. Account/password/whole-host
  controls were refused by the remote operation allowlist.
- GLM 5.3 Flash executed Bash on the execution daemon. The controller wrote its
  send request without reading the acknowledgement, then restarted during the
  turn. Recovery queried the execution daemon's original receipt and snapshot.
  The tool's proof file contained exactly one line: the prompt was not replayed.
- A wrong proof consumed the execution window even for a different admitted
  controller. A locally cancelled window could not be used. Disabling remote
  access refused control; re-enabling retained an existing grant. Signing the
  execution account out denied reconnect without signing out the controller.
  Same-account sign-in retained endpoint identity and pairing.
- The native controller opened the execution task, sent a real Bash turn and
  displayed the resulting text/tool summary. Local ACP loaded that same task,
  streamed a continuation, and both desktops displayed it. No ACP-over-Iroh
  adapter or Paseo configuration was involved.
- Remote desktop Stop terminated the Bash process and its recorded child.
  Closing the execution window preserved remote task access. Explicit Quit
  stopped its daemon. The isolated debug bundles preserve their public relay
  settings for a Dock/application relaunch.

Native testing found and corrected a desktop-startup request burst hitting the
initial remote capacity limit, initial host-selector theme/traffic-light spacing,
discovery publishing an empty address before the relay became ready, and the
Integrations settings pane retaining the controller backend after selecting a
remote task. Account/preferences remain local while Integrations uses the
selected execution backend. A disabled execution-only MCP fixture was visible
in both the remote task menu and Integrations pane; the controller configuration
remained empty. Remote capacity remains bounded and separate from local controls, with reserved slots
for cancellation and question answers.

## Computer and project navigation follow-up

The top-level computer toolbar has been replaced by computer/folder groups in the
normal sidebar and project menus. Remote setup lives in Settings. Three isolated
stores on this Mac (local plus two remote execution computers) used the same
folder paths and completed separate GLM Flash tasks through public Iroh relays.
The native desktop showed their independent titles and groups; switching between
computers preserved an unsent composer. Renaming a computer updated its local
label. Taking the third execution daemon offline retained its cached task and
folder rows; restoring the endpoint recovered its conversation without resending
the prompt. This is still same-machine development evidence.

Navigation regressions cover identical task IDs and folder paths on different
computers, keyboard destinations, search across computer/project names, summary
updates without transcript loading, a thousand streamed chunks during catalog
recovery, background unread state and superseded navigation during startup.
The full component checks below were rerun after this navigation change.

## Independent request streams

The pre-review audit found that reserved request slots did not prevent a large
transfer from blocking control messages on the original single ordered stream.
The initial wire version 2 used one bounded QUIC stream per request/reply and a separate
ordered event stream, retaining the existing authentication, receipts and recovery.

The new transport tests use actual Iroh endpoints with the production flow-control
configuration. They leave an upload incomplete and an image reply larger than its
receive window unread, fill ordinary admission capacity, and verify that Stop and
question answers still complete. Finishing the upload admits the original send
once. Additional coverage closes an incomplete transfer before admission, rejects
oversized urgent bodies before transmission, and refuses a mismatched reply ID.
These are transport tests, not evidence of latency across a real slow network.

A fresh isolated headless build of this correction also repeated the Local
registration, pairing and control flow with IP transports disabled. GLM 5.3 Flash
completed its tool turn after the controller restarted with an unread send
acknowledgement; the original receipt and exactly one proof-file write were
verified. Revocation refused reconnect. All test-owned daemons were stopped, and
the user's existing desktop builds remained untouched.

## October 20 review follow-up

The branch was rebased onto `339d306e`, including the merged Windows ACP
streaming-test correction (#1174). The focused fixes keep the existing
daemon/pairing architecture; that revision used protocol version 3.

Full Agent `just ci` passed again on macOS: all four lint configurations,
workspace build/tests, 271 runtime tests / 2 ignored, 501 desktop tests /
1 ignored and 92 headless tests / 1 ignored. The new regressions cover image
readback, inactivity deadlines, Hello parsing/refusal framing, wire envelopes,
uncertain draft retention, disabling while initialization waits on credentials,
and Stop with a lost acknowledgement. The initialization test does not itself
prove successful retry or a late valid endpoint install; the Local probes below
provide the separate runtime evidence.
These local results do not establish the rebased head's hosted CI status.

Fresh disposable Local profiles repeated the following through public Iroh relays,
with direct IP transports disabled and the user's apps left running:

- Reattaching the local view retrieved the same pairing code/window/deadline.
  A wrong proof consumed its single attempt; another controller received a clear
  closed-window refusal. A new window paired successfully. This check also found
  a stale startup advertisement overwriting a new pairing window; registration
  publication is now serialized without delaying local Cancel or Disable.
- A 5,881,918-byte PNG (23,524,264 bytes in the old JSON-array representation)
  returned intact, followed by another task request on the same connection.
  The automated transport regression additionally covers a 10 MiB reply and an
  oversized reply error followed by a successful control request.
- GLM 5.3 Flash completed its Bash turn after the controller restarted with an
  unread acknowledgement. The original receipt was recovered and the tool wrote
  its proof file exactly once. Disable/re-enable preserved the existing grant;
  revocation denied reconnect.
- A temporary local forwarding proxy made startup unavailable, then recovered:
  an already-enabled host retried automatically without another user action.
  Gating initialization while issuing Disable proved that its local effect did
  not wait on the service and that late completion could not install the endpoint.
- The isolated debug macOS GUI opened and rendered, then its own host was stopped.
  This does not establish the Linux/Windows tray UX or signed macOS permissions.

The disposable PostgreSQL suite passed 22 AEAD/database, 2 OAuth and 8 callback
tests, including stale window reclamation under forged expiry hints and a
concurrent replacement that compare-and-delete must preserve. All-feature
OpenSecret Clippy and unit tests passed (865 passed / 33 ignored). The guarded DB
run used `PGGSSENCMODE=disable` only for its temporary loopback database after a
macOS Kerberos-library teardown abort; no application authentication setting or
test was disabled.

## Final focused follow-up, October 20

The next change closes the remaining five trial issues without changing the
pairing or daemon architecture. Wire version 4 adds an explicit interrupted-send
receipt; mixed builds receive an update-required refusal. On startup, only the
new exclusive owner marks unfinished receipts interrupted. Those identities stay
reserved, the UI warns that effects may exist, and the task can continue without
replaying or overwriting a newer draft. Quit checks ownership before treating a
missing endpoint as stopped, and failed Stop offers a visible Quit anyway choice.
Settings re-entry establishes a baseline before detecting a newly paired computer;
mid-handshake EOF remains a transient disconnect.

Full Agent `just ci` passed on macOS: four lint configurations, 271 runtime tests /
2 ignored, 507 desktop tests / 1 ignored, and 94 headless tests / 1 ignored, plus
the workspace suites. Focused regressions exercise startup receipt classification
and account isolation, the send gate/draft preservation, truncated acknowledgement
versus invalid frames/revocation, Settings re-entry with an existing controller,
absent versus unreachable host ownership, and the tray Quit confirmation with no
main window. Wire fixtures cover selected forwarded events, attachment send
parameters, session summaries and receipt outcomes; they are not exhaustive
protocol conformance or mixed-build interoperability evidence.

Fresh disposable Local profiles repeated one-attempt pairing/refusals, unchanged
pairing-window reattachment, 5,881,918-byte image readback plus a subsequent task
request, disable/re-enable, and revoke over forced public Iroh relays. With the
execution daemon stopped, a synthetic unfinished row was inserted only into its
disposable receipt database. Restart classified it as `Interrupted` and remote
task access recovered. This is a deterministic crash-state fixture, not a claim
that a real crash was timed inside the admission transaction. Separately, a live
GLM 5.3 Flash/Bash turn survived controller restart with an unread acknowledgement;
the original execution receipt and exactly one proof-file write were verified.
All owned probe processes were stopped; user applications were untouched. These
probes used the changed source atop `ca4b2bbf` before commit, not a production
package or a second physical machine.

## Computer names, October 20

Two fresh Local profiles verified OS-derived defaults, renaming while remote
access is off without starting networking, and discovery of both names during
public-relay pairing. Subsequent discovery refreshes propagated each rename
without changing endpoint identity, grants or direction. Remote task access and
both names survived restarting both daemons. A remote request to rename the
execution computer was refused; naming is local administration. This uses the
existing discovery metadata and leaves Iroh wire version 4 unchanged.

## Initial October 19 automated checks

- Full Agent `just ci`: all four Clippy configurations, workspace build/tests and
  headless tests. Runtime: 271 passed / 2 ignored; desktop app: 494 passed /
  1 ignored; headless app: 86 passed / 1 ignored. The ignored app test contacts
  public relays and is run explicitly for this validation, not silently counted
  as ordinary offline CI coverage. Existing Pi/runtime workspace suites pass too.
- OpenSecret all-feature Clippy and unit tests: 817 passed / 32 ignored.
  Disposable PostgreSQL checks explicitly ran 21 AEAD tamper tests, 2 OAuth and
  8 callback binding tests, including valid foreign-row/lookup-column swaps,
  repeated-window expiry and relay registration/key rejection. Migration down/up passed.
- Rust SDK format, all-feature Clippy, 119 unit tests / 1 ignored, and documentation
  with warnings denied passed. Client/service registration fixture bytes match.
- Root and Agent Nix flake checks/evaluation passed on the current platform.
- The backend's configured advisory and incident-ban checks passed. Its policy
  does not configure a license allowlist; an exploratory unrestricted
  `cargo deny check` rejects ordinary MIT/Apache dependencies for that reason.
  No security policy was changed to suppress a finding.
- All eleven tests/vectors at the pinned upstream CPace revision passed. See
  [qualification and evidence limits](remote-security.md); this is not a claim
  that the complete Maple implementation received an external security audit.

At the audit baseline `3bd0c4a3`, GitHub Actions ran actual Linux, macOS and Windows
Agent tests, unsigned Dev/Prod macOS packaging and downloaded Linux artifact
verification successfully. Signed macOS jobs intentionally skipped. Those results
do not validate subsequent transport edits; those have the local checks above
and require their own hosted CI results.

## Open implementation findings

These remain review/rollout work, rather than newly accepted design exceptions:

- Disabling incoming remote access closes control connections but leaves an
  initialized networking endpoint alive until account/daemon teardown. Closing
  unused networking must preserve any outgoing controller role.
- User/project deletion cascades to enrollment records; destructive password
  reset does not yet remove them. Expired device/admission rows are removed on
  that account's later writes. Registration now reclaims other endpoints' window
  tombstones using authenticated expiry after 24 hours, preserving the current
  endpoint's repeated-W guard. Reset cleanup and cleanup for accounts that never
  write again remain hosted-rollout work.
- Pre-admission has a global connection cap and deadline. A separate per-peer
  validation bound remains hosted-rollout work.
- Quit/sign-out close local authority but do not yet unpublish discovery
  best-effort. Listings can remain stale until lease expiry; they confer no grant.
- Enrollment has row caps, leases and payload bounds, but no dedicated
  publish/list/admission request-rate enforcement yet. Row caps do not bound
  request traffic.

Cancel/disable is authoritative on the device even if discovery briefly retains
stale pairing readiness until refresh/expiry. That is a UI freshness limitation;
it does not permit another code attempt or grant control.

## Remaining trial/release evidence

Public relay validation does not exercise the prepared Maple relay admission
callback in a deployed relay. The callback has unit/disposable-DB coverage.
Packaged Dev/Prod default to their respective Maple-operated fleets. Before a
client rollout, validate each environment's deployed admission callback, relay
TLS/credential configuration and actual forwarding; a healthy HTTP endpoint or
an unknown-endpoint refusal alone does not prove registered admission. Also
exercise the two Prod relays together and relay-loss recovery. These are separate
from the Local public-relay evidence recorded here. The matching enclave
deployment and migration are required. The validation recorded here did not
deploy a service or production release.

Windows/Linux interactive status-tray behavior still needs platform testing;
builds and automated tests are covered by the baseline CI above. Two physical machines behind independent networks,
sleep/wake recovery, and signed macOS permissions/CUA remain distinct trial gates.
This same-machine debug exercise does not establish those results. A VM bridged
address did not establish the direct test path; the explicit loopback and forced
public-relay paths are the evidence reported above.

## Experimental remote updater coverage

The optional management extension has regression coverage in the existing
Agent native and headless suites:

- Frozen wire-6 Hello/ACK/ALPN fixtures, a real Iroh legacy-method refusal
  followed by an ordinary request on the same connection, and bounded probes
  before the first reply byte.
- Real Iroh successful install-reply delivery, rejected/reset replies,
  missing FIN and trailing data. The client consumes the complete reply stream
  before the host can release an installation to its desktop.
- Durable acknowledgement/claim ordering, failed journal writes, replay refusal,
  receipt and lease expiry, account/peer/grant/store scope, and unsafe journal
  files or symlinked parent directories.
- Journal-backed startup success reconciliation and a later local upgrade;
  mismatched version/source/known build and unclaimed requests never become
  success or executable work. Status-only reconciliation is persisted.
- Completion-report write/acknowledgement loss through the production desktop
  polling bridge with an injected local-RPC fixture: heartbeats continue, the
  same report is retried, and installation is not repeated. Account, lease and
  retirement changes discard old reports.
- Framed local-IPC refusal versus missing, malformed or timed-out authorization,
  accepted-but-incomplete shutdown, startup locking and ownership retention.
  Controller regressions keep the same prepared candidate ready only after a
  known safe refusal; uncertain shutdown and changed candidates remain blocked.
- A real password-wrapped SQLCipher store reopened through authenticated unlock:
  version/offer visibility remains available, while install/stop admission is
  denied. Refused remote stops leave unrelated maintenance and OAuth work alive.
- Native GPUI consent/focus, stale account/grant/target replies, multiple
  computers, cross-platform release identity, uncertain outcomes and retention
  of the controller's manual updater choice.
- Historical outcomes remain distinct from the currently running version;
  unavailable update journals preserve version visibility without install
  authority, and current computers have no contradictory download instruction.

These checks do not constitute a signed packaged cross-computer update
rehearsal. Qualify that separately with two signed versions on macOS and Linux:
update only the named execution computer, verify its actual new identity and
login/settings after reconnect, and keep the controller and a second remote
computer unchanged. Exercise interrupted connectivity and background desktop
operation, and confirm password-protected hosts refuse the direct request as
well as disabling the UI. Future-release protocol compatibility is not inferred
from equal application versions or from the controller having an update ready.
