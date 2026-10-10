# Embedded CUA developer preview

Maple can expose the Cua Driver tool catalog to its embedded Goose agent
without launching a Cua Driver daemon or an MCP child process. The macOS and
Linux builds pin Cua's `cua-driver-sdk` source at the trycua/cua commit that
landed Xcode 26.5 linker compatibility, self-process window restoration on the
AppKit main queue, and the ashpd `async-io` backend (`trycua/cua#3687`) and
create the native runtime inside the Maple process. Self-process restoration
avoids an `invoke_menu` crash that only exists when CUA is embedded inside the
application whose window it restores. The Linux backend selection matches
GPUI's `async-io` stack so the two compile together.

Which platforms host the runtime is decided in one place, by the `embedded_cua`
configuration flag that `crates/maple-agent/build.rs` sets. Adding a platform
is a change there plus a dependency entry, not an edit in each module.

This is an explicitly enabled developer preview. It is not an installer, a
general integration marketplace, or a sandbox for computer-use actions.

## Architecture

There is one lazily-created CUA runtime for the Maple process. Each enabled
desktop task receives its own standard-mode trusted CUA session and a native
Goose MCP client. Its public and transport identities are stable, opaque hashes
of the account scope and Maple task ID, so replacing the ephemeral client at a
run boundary reconnects to the same isolated lifecycle without colliding across
accounts. Maple starts or revives that lifecycle before exposing tools; the
model cannot name, end, or enumerate sessions. The client uses the SDK's
canonical `list_tools_json` and `call_tool` surfaces, so Maple does not maintain
a second copy of CUA's tool definitions or run an in-process network protocol.

Goose still owns tool discovery, namespacing, and dispatch. Every CUA call
runs without asking for approval, like every other tool call in a Maple
task; CUA's published schemas and annotations reach the model unaltered.

CUA's action schemas and screenshot defaults remain canonical. Maple adapts the
general-purpose catalog only at the bound-session boundary: it hides CUA's six
lifecycle tools and removes caller-controlled `session` and reserved arguments
before the SDK injects Maple's trusted identity. Embedded-specific instructions
tell the model to use this tool surface directly, never mix CLI or other MCP
tokens into it, observe before acting, and treat controlled-app content as
untrusted data.

Every primary model receives the original accessibility text and a bounded,
image-free projection of CUA's structured grounding fields. This is necessary
because Goose's current OpenAI formatter otherwise preserves that field without
showing its exact window IDs, element tokens, coordinate frames, and refusal
details to the primary model. Vision-capable models also retain the canonical
raw image blocks, but a request carries only the newest few tool-produced
images as pixels and replaces older ones with a short marker; the stored
transcript keeps every screenshot, so this bounds what one request uploads
rather than what the task remembers. For a text-only primary model, Maple
instead sends each returned screenshot through the same fixed, tool-free Gemma
perception helper used by `read_image`, removes the raw image blocks, and
appends a factual CUA-specific description. The original non-image content,
structured metadata, protocol metadata, and error state remain intact. The
helper is instructed to report controls, state, layout,
coordinate-space-aware approximate positions, and discrepancies from the
accessibility tree without selecting actions or inventing element identifiers.
Helper usage is recorded outside the primary context ledger.

The native client is ephemeral. Goose must not serialize a Rust object as an
ordinary MCP transport, so its extension snapshot excludes the client. Maple
stores only versioned logical task metadata: whether CUA is enabled for the
task, and the embedded backend it uses. On cold load or a new
turn Maple recreates its trusted connection and rejoins the stable account/task
CUA lifecycle.

Embedded CUA is attached only while a user task is controlled by the desktop
app. ACP and other headless surfaces do not inherit the host's interactive
desktop authority.

## Setup and existing installations

What a platform needs before the runtime can start is described by one list of
permissions with their grant state. An empty list means the platform needs no
grant up front, so both the readiness rule and the "what is still missing"
answer come from that one value.

On macOS the list holds Accessibility and Screen Recording. Settings >
Integrations reports Maple's own status for each. **Set up CUA** invokes Cua's
in-process macOS permission helpers on the UI thread only after a direct user
action. If macOS does not raise a prompt, Maple opens the next missing Privacy
& Security pane, Accessibility first and then Screen Recording. Those grants
belong to Maple's app identity; permissions previously granted to
`CuaDriver.app`, Codex, a terminal, or any other host do not transfer.

On Linux the desktop portal asks for consent when a task first takes a
screenshot or sends input, and the compositor remembers that choice, so most
sessions need no setup step and the card offers **Enable** directly.

GNOME is the exception, and the requirement list says so. Mutter advertises
none of the protocols an ordinary client would use for this: not the wlroots
family, not `ext-foreign-toplevel-list-v1`, and not `ext-image-copy-capture-v1`.
Window geometry, window activation, and screen capture therefore all go through
the `winrects@cua` GNOME Shell extension that ships with the SDK. Without it
the SDK falls back to X11, finds no windows, and fails capture inside
`XGetImage`. Maple checks whether the extension owns `org.cua.WinRects` on the
session bus and reports it as an unmet requirement until it does, because
installing it is not enough: GNOME loads extensions only when the session
starts, so the user has to log out and back in once.

The extension is embedded in the binary, so **Set up** writes it into
`gnome-shell/extensions` and adds it to `enabled-extensions` without a source
checkout or a download. That matters because Maple is distributed as a bare
executable. Writing the enabled set reads the current value first and refuses
to touch anything it cannot parse back exactly, since overwriting that key
wrongly would disable every extension the user has.

The card distinguishes the two states, because telling somebody to install
something they already installed is how they conclude the feature is broken.
Before an install it offers Set up. Afterwards the catalog reports that no
setup action remains, the button goes away, and the card asks for the session
restart instead. The interface reads that from the projection rather than
deciding it, so the two cannot disagree.

The vendored copy lives in `crates/maple-agent/resources/gnome-helper/` and
moves with the SDK pin, because the driver and the extension negotiate an API
version. `UPSTREAM_SOURCE_REVISION` records where it came from.

Maple also opts into the SDK's native-Wayland backend on any Wayland session.
The SDK keeps that backend behind `CUA_DRIVER_RS_ENABLE_WAYLAND` and otherwise
routes enumeration, capture, and input through X11. GNOME and KDE still export
`DISPLAY` for Xwayland on a native Wayland session, so the X11 path looks
viable and fails silently. Maple sets the variable as the first statement of
`main`, where the process is still single-threaded, and an explicit value from
the user always wins.

Per-window PipeWire capture stays off because it needs PipeWire headers at
build time; full-screen portal screenshots and the GNOME helper's stage capture
do not.

### Driving a browser on Linux

CUA's `browser_*` tools need an owned Chrome DevTools Protocol endpoint. A
browser the user already had open was not started with remote debugging, so
`browser_prepare` reports `browser_requires_setup`, and taking that endpoint
would mean relaunching the browser and discarding the session the user asked
the agent to work in.

The supported path is the ordinary desktop one: `get_window_state` returns the
page's accessibility tree together with a window screenshot, and `click`,
`type_text`, and `press_key` act on it. This was verified against a snap
Firefox on GNOME Wayland, where one window yielded 739 elements including the
document, its headings and links, and each element's available actions. Input
is delivered through the portal's RemoteDesktop session and libei, so the first
action in a session raises a consent prompt. Background delivery is unavailable
on GNOME, so actions use `delivery_mode: "foreground"`, which activates the
target window and restores the previous one afterwards.

Maple no longer detects or runs a separately installed `CuaDriver.app`, so no
foreign binary is ever executed. A task saved by the first integrations preview
with that driver's stdio entry loses the entry before any agent is built for
it, and a device setting that selected the external backend is ignored on
load; both read as "not set up" until the user enables built-in CUA. Nothing
is migrated or rewritten beyond that.

Enabling the card changes the default for newly-created tasks. Existing tasks
retain an independent switch in the composer. Changing tools while a task is
running or leased to another agent surface remains disallowed.

## macOS development bundle

Raw command-line binaries do not provide the stable application identity that
macOS privacy controls require. On macOS, stage the debug executable in the
repository's fixed development bundle with:

```sh
just debug-app
```

The command prints the exact `.app` path. Launch that bundle with `open` and
keep the same path and bundle identifier when validating permission changes.
The final Maple binary carries an app-relative `Contents/Frameworks` lookup
path. The staging script uses Xcode's `swift-stdlib-tool` to scan that binary,
embeds the complete set of required Swift compatibility libraries, signs the
nested libraries, and then seals the complete bundle. This keeps Swift runtime
packaging correct when native dependencies change instead of maintaining a
hard-coded library list.

By default the script uses a local ad hoc signature, so it needs no Developer
ID and does not produce a release artifact. Because rebuilding changes the ad
hoc identity, macOS may require those grants again after the executable
changes. Developers with an Apple Development identity in their keychain can
set `MAPLE_DEBUG_CODESIGN_IDENTITY` to its name or SHA-1 hash for a more stable
development identity. Release signing, hardened runtime, and notarization
remain separate distribution concerns.

## Preview limits

- macOS and Linux host the runtime. Windows keeps the integration hidden until
  its packaging and permission story are implemented, even though the SDK
  carries a Windows backend.
- On Linux the preview is validated against GNOME on Wayland. Other
  compositors and X11 sessions are supported by the SDK but are not part of
  Maple's own testing yet.
- CUA's Rust packages are not published to crates.io and its portable tool
  contract is still marked experimental, so Maple pins an exact source commit.
  The pin currently includes narrow Xcode 26.5 linker and embedded-host
  main-thread fixes that should move back to an upstream release once
  available.
- Direct embedding is not process isolation. A CUA crash or native defect can
  affect Maple, and nothing in Maple provides OS-level containment.
- The CUA cursor overlay needs a main-thread AppKit host adapter and is not
  wired in this preview.
- Cancellation is best effort after an operating-system input event has been
  delivered; an already-completed external side effect cannot be rolled back.
- Text-only models receive mediated visual perception rather than native
  vision. This preserves CUA's visual fallback for canvas and weak-accessibility
  applications, but it adds one bounded helper request per returned screenshot
  and may be less precise than a vision-capable primary model on difficult
  pixel-only tasks. Models can still request CUA's tree-only fast path when a
  screenshot is unnecessary.
- Production distribution still needs a signed/notarized `.app` pipeline with
  a stable production bundle identifier and the same privacy usage strings.
