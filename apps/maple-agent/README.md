# Maple Work (GPUI prototype)

Maple’s desktop-v2 prototype lives in `apps/maple-agent/` in this monorepo.
Its Cargo package is `maple-agent-app` and its executable is `maple-agent`. It rebuilds Maple Agent Mode in
[gpui](https://crates.io/crates/gpui) on top of the Maple agent runtime
ported from the Tauri app.

The same binary also runs as an Agent Client Protocol (ACP) agent and as an
OpenAI-compatible proxy. See "Command line" below.

## Layout

```
app/                  The maple-agent binary. Owns the window, login, chat,
                      settings, notifications, and the backend adapter.
crates/maple-agent/   Maple's transport-neutral agent runtime. Runs tasks on
                      the pi-* crates, and owns the Maple provider over the
                      Maple Rust SDK, account-scoped task storage, Maple's
                      tools, and the ACP server.
crates/maple-billing/ HTTP client for the Maple billing API.
crates/pi-ai/         Model messages, streaming events and an OpenAI-compatible
                      provider, after Pi's pi-ai package.
crates/pi-agent-core/ The agent loop with tools, hooks and message queues, after
                      Pi's pi-agent-core package.
crates/pi-coding-agent/
                      Sessions as an append-only tree, compaction, skills and
                      prompt templates, the extension API, the built-in tools
                      and the agent session, after Pi's coding-agent core. The
                      pi-* crates have no Maple dependencies.
docs/                 Theme spec measured from the Tauri app.
scripts/              One maintainer helper: screenshot.py takes a desktop
                      screenshot through the xdg portal on GNOME Wayland.
                      Nothing in the build or the app uses it.
```

The Pi crates' embedding adaptations and upstream review process are recorded
in [crates/UPSTREAM.md](crates/UPSTREAM.md).

### Backend / frontend boundary

`app/src/backend.rs` is the asynchronous client facade. `app/src/host.rs`
starts or joins one OS-local host; `backend/local.rs` owns account credentials,
local persistence, and the Maple runtime inside that host. GPUI owns presentation
and native permission prompts. The ACP adapter forwards stdio to the same host,
which projects shared task events onto ACP. Pi remains the agent loop; it has no
knowledge of frontends or IPC. See [local host](docs/local-host.md) and
[local storage, unlock passwords and backups](docs/local-store.md).

This version starts fresh in `store-v1/`. Previous application data is left
untouched, with no import, fallback reads or cleanup. General Markdown
instructions, skills and prompt resources retain their file-loading behavior.
Password-setting controls remain debug-only pending native release qualification.

The runtime began as a copy of Research’s Tauri agent with Tauri removed, and
now runs tasks on the pi-* crates instead of an embedded Goose. Research has no
agent runtime; its desktop Agent page points users to Maple Work.

## Features

- Sign in with email and password, or with GitHub, Google, or Apple OAuth.
  Packaged Dev and Prod use hosted browser sign-in and an automatic loopback
  return; unpackaged builds retain the callback URL paste flow. Hosted sign-in
  requires the matching Auth host and provider/backend callback configuration;
  building the client does not deploy those services.
  The session persists in the private local store so the next unlocked launch
  and the `acp` mode skip sign-in. The window opens while the saved session is
  checked, and a check that cannot reach the server keeps the credentials
  for the next launch; only a refusal from the server signs the user out.
- Agent chat with streaming Markdown, tool calls, agent questions, image
  attachments (picker, paste, or drag and drop), a per-message Copy button,
  and a context-window indicator. Every tool call runs without asking for
  approval.
- A task can switch models between messages: its model menu offers every
  model, a pick applies from the next message, and the transcript notes the
  switch. When the new model's context window is smaller, the model that
  wrote the history summarizes it before the next message if it no longer
  fits, as `crates/maple-agent/src/agent/switch_compaction.rs` describes.
- Shell tool calls start in the task's saved folder on the execution computer.
  A `cd` inside a command does not affect later calls. Repository searches
  should name the intended checkout/worktree with explicit absolute paths.
  Maple supplies a 30-minute default deadline to allow slow builds, and the
  call can request a longer
  `timeout` explicitly. Search guidance asks for a short timeout (normally
  30 seconds) and a narrower scope if it expires; that shorter limit applies
  only when the call requests it.
  Quiet commands report their starting folder, elapsed time, time since output and
  deadline every five seconds, including on collapsed tool cards. Stop and
  deadlines use Pi's process-group cleanup on Unix (process-tree cleanup on Windows).
  Maple bounds the whole operation, including setup and stdin delivery, and allows
  up to one second for cooperative cancellation before dropping a stuck operation.
  The execution policy and progress reporting live in Maple; the Pi crates are unchanged.
  These are execution safeguards, not a filesystem sandbox: repository searches
  must still choose the correct folder and bounded scope, and deliberately
  detached processes can escape a Unix process group. `head` bounds output,
  not runtime.
- Slash commands in the composer: `/btw` asks a side question the task
  never sees, plus `/compact`, `/new`, `/pin`, `/web`, `/model`, and
  `/help`. Skills and prompt templates appear in the same list.
- Instructions and skills, as in Pi: `AGENTS.md` (or `CLAUDE.md`) files
  from the project folder and the folders above it, `~/.agents/AGENTS.md`,
  and the account's own. Skills come from the account, `~/.agents/skills`,
  `~/.claude/skills` and `~/.config/agents/skills`, and in a trusted
  project from `.maple/skills`, `.agents/skills`, `.claude/skills` and
  `.goose/skills`; the model sees each skill's description and reads the
  skill when a task needs it. Prompt templates come from the account's
  `prompts` folder and a trusted project's `.maple/prompts`.
- The task's latest todo list stays pinned above the composer.
- External agents: a task can hand work to Codex or Claude Code installed on
  this computer with the `agent_start`, `agent_send`, `agent_status`,
  `agent_cancel`, and `list_agent_providers` tools, once the provider is enabled
  under Settings > Integrations. Each agent runs in the project with its own
  account, context, and sandbox settings; Maple accepts its approval
  requests, and its questions still come to you through Maple's question
  card. Its progress streams into the tool call's row, and its row above
  the composer shows how long it has worked, with a Stop button. A
  background agent keeps its row after the turn ends, and Maple tells the
  task when it finishes, with a bounded result the model reads after the
  running turn or in a turn Maple starts. Three skills, `/handoff`, `/committee`,
  and `/advisor`, teach the task when and how to delegate. See
  [`docs/external-agents.md`](docs/external-agents.md).
- Voice: dictate a message with the microphone button, and read any
  message aloud. Both use Maple's speech models; the voice and speed
  are settings.
- The composer spell checks as you type; right-click a word for
  suggestions or to add it to your dictionary.
- Message queue: Enter during a run queues the message for the next turn,
  Ctrl+Enter (Cmd+Enter) steers it into the current turn. The composer has
  one button. It is Send, and it becomes Stop while a run is active and the
  field is empty. Typing or attaching an image turns it back into Send, and
  that click queues. Queued messages can be sent now, edited in the composer
  (the message keeps its place in the queue), or removed.
- Sidebar search filters tasks and projects by name; Escape clears it.
- Up and Down in an empty composer recall prompts sent in this window.
- Optional composer-only Vim editing provides Normal, Insert, and characterwise
  and linewise Visual modes, Unicode-aware motions and text objects, operators
  and counts, an unnamed register, undo/redo transactions, and structured dot
  repeat. Enable it under General settings; every other text field stays
  standard.
- Optional application Vim navigation moves a stable semantic selection through
  Chat sidebar and transcript rows, modal choices, and Settings controls. It is
  independent of composer Vim and leaves ordinary text fields unchanged.
- Projects (working directories) with pinned and recent roots, rename,
  open in the file manager, and remove. The home directory and the
  directory the app was launched from are trusted by default. Other
  projects that provide skills, prompt templates, or a `.maple/SYSTEM.md`
  ask once for a trust decision before those load.
  Project display names belong to the execution computer's daemon and account.
  Renaming from its desktop or a paired controller updates connected views and
  persists across restarts without changing the folder path. The owner supplies
  the default workspace label too. Personal pins remain desktop preferences.
- Sidebar organization by Project, Connection, Status, or In one list, with
  Last updated or Manual order for tasks. Connection becomes available after
  another computer is added, including while it is offline. Project views keep
  known empty projects visible. Project and connection order stays stable;
  dragging rearranges the sidebar without changing a task's project, computer
  or status. Dragging a task silently selects Manual order, which persists
  across grouping changes and restarts; new tasks appear at the top. Pinned
  tasks sit above the grouping header; their own drag order does not change
  the position they return to when unpinned. Sidebar menus open toward the
  main pane so the list stays visible. Tasks also support rename, archive,
  and restore. Unknown or malformed View preferences fall back to defaults.
- Settings: General (web tools, appearance, tool call details, desktop
  notifications, tool call summaries, composer Vim, application Vim, and the
  speech voice and speed), Keyboard Shortcuts, Additional instructions, Integrations
  (detected built-ins and custom MCP servers), Usage (the plan meter from
  the billing API), and About.
- Dark and light themes; the default follows the system.
  On Linux, the protected GUI starts a portal-only child of the same executable
  (`--internal-theme-watch`). The child receives only desktop-bus environment,
  never opens Maple state, disables core dumps, and exits when its parent pipe
  closes. Only the child remains debugger-accessible for portal identification.
  Setup has a two-second background deadline; unavailable portals do not block
  launch. Explicit Dark/Light settings override system updates.
- Billing status from the Maple billing API.
- Desktop notifications when a task finishes or asks a question while the
  window is not focused.
- Signed updates for packaged Prod: background download and Restart to Update;
  read-only installations offer a verified manual download.
  Nothing is downloaded or installed by the app.
- Window size and maximized state persist between launches.

### Integrations preview

On macOS, Linux and Windows, Settings > Integrations can set up computer use inside
Maple itself. The embedded CUA runtime uses the pinned Cua Driver Rust SDK; it
does not need a separate daemon, executable, or MCP child process.

On macOS, setup reports and requests Accessibility and Screen Recording for
Maple's own app identity. Grants held by a separately installed CuaDriver app
do not transfer to Maple. On Linux the desktop portal asks for consent the
first time a task captures the screen or sends input. Under GNOME on Wayland
the `winrects@cua` GNOME Shell extension that ships with the SDK is required,
because Mutter exposes no window geometry or screen capture to an ordinary
client; Settings reports it as an unmet requirement until it is installed and
the session has been restarted once. Windows asks for no permission at all, so
enabling computer use in Settings is the only consent step there.

A desktop task with CUA on gets its tools as `cua-driver__<tool>`, bound
anew at each run. CUA keeps its native screenshot defaults. Every model
receives full accessibility text plus a bounded projection of exact structured
grounding data such as window IDs and element tokens. Vision models retain the
image blocks, fitted like every tool image; text-only models instead receive a
CUA-specific description from Maple's existing Gemma image helper, with raw
screenshot blocks removed before the primary-model request. Maple owns the
task-scoped session lifecycle and prevents models from mixing standalone CLI or
other MCP session identities into the embedded transport.

Enabling an integration sets a device-local default for new tasks. A new task
records its own CUA choice once CUA is set up on the device, and keeps it as an
independent per-task switch in the composer; a task with no choice has it off.
Maple no longer looks for a separately installed CuaDriver application: a
custom server under one of CUA's names never runs for a task, and a device
setting that selected the external backend reads as not set up. Custom STDIO
and Streamable HTTP MCP servers are account-scoped configuration in this
computer's local store. Their definitions and secrets do not automatically roam.

The embedded design, migration rules, privacy boundary, and preview limits are
documented in [`docs/embedded-cua.md`](docs/embedded-cua.md).

#### Custom MCP servers

Settings > Integrations also keeps custom MCP servers: a command Maple starts
(stdio) or a Streamable HTTP endpoint, each with environment variables,
headers, and a per-request timeout. New tasks, including those created over
ACP, inherit each server's Settings switch unless their project has a saved
choice. Switching a custom MCP server in a new-task draft saves that project's
choice on this machine; switching one in an existing task changes only that task.
Switching projects on a draft
reloads the target project's choices. Off servers remain in the menu.
A task's servers start when it runs; its first prompt waits up to ten seconds
for them, and a server that connects later joins from the next prompt. Their
tools reach the model as `mcp__<server>__<tool>`, and their instructions join
the system prompt. A server that fails to connect is named once in a notice
and tried again at the task's next run. Server definition edits in Settings
apply from a task's next run.

A stdio server runs in the task's folder with the login shell's PATH, in its
own process group; it is stopped by closing its input, then SIGTERM, then
SIGKILL. For an HTTP server, `$NAME` and `${NAME}` in the URL and in header
values are filled in from the server's environment variables, and redirects
are not followed. Sign-in to servers (OAuth), and MCP resources and prompts,
are not supported yet.

#### Claude Code

Settings > Integrations lists Claude Code (`claude`) alongside Codex, with the
same per-task selection, streamed activity, question cards, and Stop control.
Install the Claude Code CLI on the app's PATH and sign in using
`claude auth login`. Maple uses a Rust transport adapted from Goose's Claude
Code provider. The CLI is the only external runtime dependency. The integration
is off by default. Enable it in Settings to show it in the composer, then
select it for the tasks that should use it. See
[external agents](docs/external-agents.md#how-claude-code-is-driven).

#### Codex

Settings > Integrations also lists the Codex CLI when `codex` is on the PATH
(the login shell's PATH on macOS). The card shows the installed version and
whether Codex is signed in; Maple never runs Codex's sign-in itself. The
toggle is off by default. Enabling it makes Codex available in the composer
alongside CUA and custom MCP servers. Each task must select Codex explicitly;
that choice survives relaunches but only applies while Settings enables Codex.
Selecting it gives that task the external-agent tools. Enabling it in Settings
installs the `handoff`, `committee`, and `advisor` skills into the
account's skills folder; disabling removes only the files Maple
wrote. Codex needs version 0.143 or newer. See
[`docs/external-agents.md`](docs/external-agents.md).

### Composer Vim preview

Turn on **Vim mode in composer** in General settings. The composer opens in
Normal mode and shows a small `NORMAL`, `INSERT`, `VISUAL`, or `VISUAL LINE`
badge; login, search, rename, settings, prompt, question, and MCP fields keep
their ordinary editing behavior.

- Motions: `h/j/k/l`, `w/b/e`, `W/B/E`, `ge/gE`, `0/^/$`, `gg/G`, `{`/`}`,
  `%`, and `f/F/t/T` with `;` and `,`.
- Operators: `d`, `c`, `y`, `g~`, `gu`, and `gU` with any motion or text
  object, doubled for lines (`dd`, `cc`, `yy`, `g~~`, `guu`, `gUU`).
- Text objects after `i` or `a`: `w`, `W`, `p`, quotes (`"`, `'`, `` ` ``),
  and brackets (`(`/`b`, `[`, `{`/`B`, `<`).
- Edits: `i/a/I/A/o/O`, `x/X`, `s/S`, `C/D/Y`, `r`, `~`, `J`, `p/P`, `u`,
  `Ctrl-R`, and structured `.` repeat, all with counts.
- Visual: `v` and `V`, `o` to swap ends, and the operators above plus `r`,
  `~`, `u`, `U`, and `J`. The capital `S/C/D/X/Y` act on whole lines.
- Insert: `Ctrl-W` deletes a word, `Ctrl-U` deletes to the indentation, and
  `Ctrl-H` deletes a character.

Arrow keys also move in Normal and Visual modes. Escape leaves Insert or
Visual for Normal and cancels a pending `f`, `r`, or text object; Enter sends
in Insert or Normal, while Shift-Enter inserts a newline only in Insert.

Behavior follows Neovim. `app/src/ui/text_input/vim_neovim_cases.json` holds
key sequences with the result Neovim produced for each, and a test replays
them through the composer's real keymap. Add a case and run `just vim-cases`
to record it with the Neovim version pinned by this component's flake.

Composer Vim uses the same customizable shortcut catalog as the rest of the
app. It can be enabled with or without application Vim.

### Application Vim preview

Turn on **Vim navigation across the app** in General settings. Application Vim
owns a stable semantic selection while ordinary inputs retain normal text
editing. Chat remembers transcript selection per task, follows streaming only
while the selection is pinned to the newest row, and resolves sidebar projects
and tasks by stable IDs rather than virtual-list indices. Direct clicks update
the same selection state.

The preview includes `j/k`, `gg/G`, counts, `Enter`, `h/l`, `y`, `/`, `ga`,
`[a`/`]a`, `gi`, and `Ctrl-W h/j/k/l`. `Ctrl-W j` from the composer reaches its
button row, where `h/l` (or `j/k`) and `gg/G` move between the model, thinking,
integrations, web, attachment, and dictation controls and `Enter` presses one; a menu
opens on its current choice. `Space s n` starts a task and `Space ,`
opens Settings. The Settings page itself is navigable, including its shortcut
search and editable binding rows. Annotation motions are registered and report
that no annotation source is available in the current app. A root action
palette and which-key display remain follow-up work rather than hidden partial
implementations.

Closing Settings explicitly restores Chat focus because the two screens are
separately mounted. Application Vim returns to its semantic focus proxy;
Standard mode returns to the composer. The Standard-mode handoff is intentional
cross-screen behavior rather than an opt-in Vim side effect.

### Keyboard shortcuts preview

On macOS, Command-Left/Right moves to the start/end of the visible wrapped
line, and Command-Backspace/Delete deletes to that edge. Option-Up/Down moves
between paragraph boundaries (explicit newlines); Command-Up/Down moves to
the beginning/end of the text field. Add Shift to these arrow shortcuts to
select text. Shift-Up/Down selects by visible row, retaining the horizontal
cursor position across shorter rows. These shortcuts work in ordinary fields
and composer Vim Insert mode; Vim Normal and Visual keep their own motions.
Previous and next task are Command-Option-Up/Down on macOS, and Alt-Up/Down
on Linux and Windows.

Open **Keyboard Shortcuts** in Settings to search, record, disable, or reset
the bindings Maple ships in this preview. Recording accepts sequences of up to
four strokes; Enter saves, Backspace removes the latest stroke, and Escape
cancels. Exact and prefix collisions are shown before saving, with an explicit
choice to replace the other bindings or keep compatible chords.

The catalog covers the existing GPUI actions plus the typed composer and
application Vim commands. It does not expose raw input behavior such as
composer send, Shift-Enter, slash completion, or login field traversal.
Per-binding changes are stored in the local store under `shortcut_overrides`;
missing entries keep their shipped key and `null` disables that exact binding
slot. Maple validates the complete candidate map before replacing the live one,
so a malformed override cannot leave ordinary text editing half-installed.

## Build and run

Use this component’s pinned Nix environment on macOS or Linux. Commands in
this README run from the component directory unless stated otherwise:

```sh
cd apps/maple-agent              # from the Maple repository root
nix develop --no-update-lock-file
just build                      # debug binary in target/debug
just run                        # desktop app
just release                    # local release build only
```

For hosted Dev and Prod packages, see [desktop builds](docs/desktop-builds.md)
and [build profiles](docs/release-profiles.md). The desktop build workflow
builds signed Prod packages on `master` pushes that change Agent inputs, Dev
packages nightly, and both on a manual dispatch from `master`; macOS packages
use Developer ID signing and notarization, and Linux packages are portable
user-local tar bundles.

The root also offers `just agent-check`, `just agent-build`, and
`just agent-dev`. Native development on macOS requires full Xcode at
`/Applications/Xcode.app`; Linux libraries come from Nix. Windows has no Nix:
`.github/actions/setup-agent-windows` pins the Rust version that the
component's `flake.lock` resolves, and the `agent-lint` lane fails if the two
differ. Cargo commands run from this directory. The statically linked
SQLCipher/OpenSSL build needs Perl; Nix supplies it on macOS/Linux. Windows
needs native Windows Perl (for example Strawberry Perl), rather than MSYS
Perl. NASM enables the provider's assembly path when available; its build
supports a no-assembly fallback.

For an OpenSecret managed workspace, run its `bin/maple-agent` launcher
instead of launching the binary directly. It sources private
`env/maple-agent.sh`, selects the workspace’s local or hosted backend and
billing configuration, and isolates config/data under
`state/maple-agent/{config,data}`. The launcher clears inherited API keys and
disables update discovery. Agent and the standalone proxy share one reserved
proxy port: choose one process to own it.

On macOS, use `just debug-app` when testing features that depend on privacy
permissions. It stages the debug binary in a stable, development-only `.app`
identity, discovers and embeds any Swift compatibility libraries required by
native dependencies, signs nested code before sealing the bundle, and prints
the exact bundle path to launch. This requires a full Xcode toolchain but does
not require a Developer ID or produce a release artifact. The default ad hoc
bundle displays Maple Work Debug and keeps the filename `Maple Agent Debug.app`
with identifier
`cloud.opensecret.maple.agent.debug`, separate from both packaged profiles. Its
identity changes when Maple is rebuilt, so macOS may require the development
app's privacy grants again. Set `MAPLE_DEBUG_CODESIGN_IDENTITY` to the name or
SHA-1 hash of an Apple Development identity in the local keychain when more
stable grants across rebuilds are useful. `MAPLE_DEBUG_BUNDLE_ID` can select
a dotted development bundle identifier; the managed Agent environment sets
a unique workspace identity. Source that environment before `just debug-app`
and launch the exact generated bundle. Stop only the process you started.
For the managed Agent identity, packaging records only the workspace's public
service configuration and both XDG roots in the local bundle's `LSEnvironment`.
This preserves isolation when Finder or a GUI driver launches the bundle without
the shell environment. Update checks are disabled and inherited proxy keys are
cleared. No other environment values or credentials are copied into the bundle.

### Nix

The component lockfile pins Rust and platform dependencies. Linux exposes a
pure release package and a development shell:

```sh
nix build --no-update-lock-file
nix develop --no-update-lock-file
```

On Apple Silicon macOS, use the Nix development shell with full Xcode at
`/Applications/Xcode.app`. If Xcode lacks its optional Metal compiler, install
that component before building:

```sh
xcodebuild -downloadComponent MetalToolchain
nix develop --no-update-lock-file -c just build
nix develop --no-update-lock-file -c just debug-app
```

A pure Darwin package is not exposed: the pinned Nix Swift/SDK combination
cannot build the CUA bridges with the SDK required for recording. The supported
macOS path uses Xcode's Swift and Metal toolchains and is also used by CI.
Linux builds use Nix-provided ALSA, font, keyboard, Wayland and Vulkan libraries.
Release bundles link against glibc 2.31 with `cargo zigbuild` and run on the
host's own libraries; see [desktop builds](docs/desktop-builds.md).

### Shared Rust build cache

Local Nix shells and `just` recipes use Cargo's separate build directory
(`CARGO_BUILD_BUILD_DIR` / `build.build-dir`) to share Rust intermediate
artifacts across Agent checkouts and git worktrees. Final artifacts
remain in the current checkout under `target/`, so `just run`,
`just dist`, and debugger paths do not change.

The default cache is separated by rustc host triple and compiler version:

```text
$HOME/.cache/cargo-build/maple-agent/<host-triple>/rust-<version>
```

An existing `CARGO_BUILD_BUILD_DIR` takes precedence. To temporarily restore
Cargo's traditional checkout-local layout, set
`MAPLE_DISABLE_SHARED_CARGO_BUILD_DIR=1`. CI does not enable the local
shared cache automatically.

Raw `cargo clean` removes both the checkout's target directory and the
configured shared build directory. To clean only the current checkout
without invalidating other Agent worktrees, run:

```bash
just clean-local
```

Release builds use fat LTO and one codegen unit. Use a release build for any
performance check; the dev profile is `opt-level = 1`.

The pinned shell includes `just`. Run `just` to list component recipes:

```sh
just ci          # all the checks that CI runs
just debug-app   # stable macOS debug app for privacy-permission testing
just release     # release binary in target/release
just dist        # release binary copied to dist/ with a SHA-256
just run         # debug build with debug logs
just clean-local # this checkout's Cargo artifacts only (keeps the shared cache)
```

## Command line

```
maple-agent                 Open the desktop app.
maple-agent acp             Serve the Agent Client Protocol on stdio.
maple-agent proxy [FLAGS]   Serve an OpenAI-compatible HTTP endpoint.
maple-agent login           Sign in with email and password from a terminal.
maple-agent unlock          Unlock the local store using a hidden TTY prompt.
maple-agent host stop       Stop this profile's host and its running tasks.
maple-agent --version       Print the version.
```

### `maple-agent login`

Prompts for the account email (or takes `--email`) and the password, signs
in, and saves the session the same way the desktop app does. An anonymous
account signs in with `--account-id <ID>` instead of an email. Use it on a
machine that never opens the window so `maple-agent acp` has a sign-in. The
password is always prompted for; there is no flag for it. OAuth sign-in is
desktop-only for now.

A password-protected store must be unlocked first. Login uses the Maple account
password; it does not unlock local storage.

### `maple-agent unlock`

Prompts for the local unlock password through the controlling terminal. No
password argument or environment variable is accepted. Unlock here or in the
desktop app before starting ACP against a locked store. See
[local storage](docs/local-store.md) for password scope, Close/Quit and recovery.

### `maple-agent acp`

Connects editors and ACP clients to the shared local host over stdio. It
can start the host without opening a window; desktop later joins that host.
Both interfaces use the same sign-in, tasks, tools, history, and one active run
per task. Logs go to the profile log; stdout contains only ACP frames.
Unlock through desktop or `maple-agent unlock`, then sign in through desktop or
`maple-agent login` if needed. ACP never places an unlock prompt on its protocol
stream.

`session/new` creates a draft in the client's working directory. Account MCP
servers use the Settings defaults with that project's saved choices applied.
Its first accepted prompt makes it visible in desktop and ACP lists. `session/load`
replays a task's history, including the partial answer of a running task.
Closing the ACP pipe detaches the client. Explicit `session/cancel` stops shared
work; the optional `session/close` capability is not advertised because it means
cancel in ACP. Some clients send cancel themselves when closing a turn.

Model and supported thinking levels use standard configuration options. Maple
also supports the `_session/steering` extension used by the Codex and Claude ACP
adapters: it injects into an active run at Pi's next steering opportunity.
Ordinary overlapping `session/prompt` calls retain ACP's busy response; clients
should use the advertised steering extension. Questions use negotiated form
elicitation or a connected desktop; with neither, the tool asks the agent to use
plain text. Project trust remains an explicit choice; tools do not ask for
individual approval and there are no session modes.

Client-supplied HTTP MCP bindings are temporary and become unavailable on
disconnect. Conflicting server names are refused. Transient stdio MCP injection
remains disabled; configure local stdio servers in Maple. Tools execute on the
host's filesystem with its normal login-shell PATH. See [lifecycle and client
compatibility](docs/local-host.md) before configuring an ACP launcher.

### `maple-agent proxy`

The desktop also offers **Settings → Local API**. It starts this computer's
loopback endpoint in the shared local host, including while a remote computer
is selected. Create a key under API keys, enable Local API, then configure your
client with the displayed `/v1` base URL and that key. See
[Local API ownership and lifecycle](docs/local-api.md).

The command below remains an independent foreground server. It cannot share a
port with an enabled desktop listener; an occupied port is reported without
stopping or adopting the other server.

```
--host HOST     bind address (default 127.0.0.1, env MAPLE_PROXY_HOST)
--port PORT     bind port (default 8080, env MAPLE_PORT)
--api-key KEY   Maple API key for requests without an Authorization header
                (env MAPLE_API_KEY); not allowed together with --cors
--cors          allow browser origins; every request must then carry its
                own key (env MAPLE_ENABLE_CORS=1)
```

Without `--cors`, the proxy rejects requests that carry browser-only headers
(`Origin`, `Sec-Fetch-Site`) so a web page cannot spend a saved key through
loopback. With `--cors`, a default key is refused for the same reason.

## Build features

The default build has every mode. Cargo features turn modes off, so a
server or CI machine can build the `acp` or `proxy` mode without the gpui
window and its display libraries:

| Feature | What it adds |
| --- | --- |
| `desktop` | The gpui window. Without it the binary is headless. |
| `acp` | `maple-agent acp` and `maple_agent::acp`. |
| `proxy` | `maple-agent proxy` and the local host's optional API service. |

```sh
cargo build --release -p maple-agent-app --no-default-features --features acp
cargo build --release -p maple-agent-app --no-default-features --features proxy
```

A mode that is compiled out exits with status 2 and a message that names
the missing feature.

## Configuration

Local unpackaged builds accept the environment settings below. Packaged Dev
and Prod bake their service endpoints, client ID, PCR trust, state namespace,
and updater trust at compile time; service overrides are
ignored. See [build profiles](docs/release-profiles.md).

| Variable | Purpose | Default |
| --- | --- | --- |
| `MAPLE_API_URL` | OpenSecret backend. Use `http://127.0.0.1:3000` for a local dev backend. | `https://enclave.trymaple.ai` |
| `MAPLE_BILLING_API_URL` | Maple billing API. | `https://billing.opensecret.cloud` |
| `MAPLE_CLIENT_ID` | OpenSecret client id (UUID). | Maple's id |
| `MAPLE_MODEL` | Model to select at start. | Runtime default |
| `MAPLE_CONTEXT_LIMIT` | Context window in tokens, used instead of the model catalog's for compaction and the context ring. | Catalog value |
| `MAPLE_SHELL` | The bash the agent's `bash` tool runs. | `/bin/bash`, else `bash` on PATH (Windows: Git Bash; without it, the `powershell` tool) |
| `MAPLE_DISABLE_UPDATE_CHECK` | `1` turns Agent update checks off. | unset |
| `RUST_LOG` | Log filter. | `info` |

### File locations

The application database lives in `<local data>/store-v1/`. An absolute
`XDG_DATA_HOME` overrides the data-directory base on every platform;
`XDG_CONFIG_HOME` selects instruction/configuration resources. The table shows
unpackaged builds. Packaged Dev and Prod use `maple-agent-dev` and
`maple-agent-prod` instead of `maple-agent`, with separate data and passwords.

| Root | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Config/resources | `~/.config/maple-agent/` | `~/Library/Application Support/maple-agent/` | `%APPDATA%\maple-agent\` |
| Local data | `~/.local/share/maple-agent/` | `~/Library/Application Support/maple-agent/` | `%LOCALAPPDATA%\maple-agent\` |

| Path | Content |
| --- | --- |
| `<local data>/store-v1/db/maple.db` | Account-scoped tasks, full source history, derived timeline, attachments, settings, credentials, MCP secrets, receipts and remote identities/grants. |
| `<local data>/store-v1/db/maple.db-wal`, `maple.db-shm` | SQLite WAL and coordination files, when present. |
| `<local data>/store-v1/db/keys.json`, `keys.backup.json` | Matching wrapped data-key copies in password mode. |
| `<local data>/store-v1/db/generation.json` | Current store generation identity. |
| `<local data>/store-v1/owner.lock` | Authoritative lifetime ownership lock. |
| `<local data>/store-v1/logs/maple-agent.log` | Plaintext diagnostics, including panic logging. |
| `<local data>/store-v1/resources/` | Newly generated delegation instruction Markdown, skills and prompts. |
| `<config>/agent/accounts/<scope>/AGENTS.md`, `skills/`, `prompts/`, `SYSTEM.md` | General account instruction resources; project/workspace instructions retain their existing lookup behavior. |

`<scope>` is the SHA-256 of the account's user id. General instruction files
remain outside database encryption and erasure. Other app state uses the fresh
store only: previous JSON, JSONL, databases, attachments, credentials and remote
identity files are neither read nor migrated, renamed or deleted. The previous
`maple-gpui` directory is not adopted. The Research/Tauri app's state is separate.

Use a local filesystem and keep the store private to the OS user. Plain mode
uses OS permissions; password mode additionally encrypts the current database
and WAL. Logs, instruction files and earlier copies remain outside that
protection. For stopped-host backup/restore steps, erase semantics, supported
filesystem boundaries and release qualification, see [local storage](docs/local-store.md).

## Tests

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

On a Linux desktop with a live Settings portal, run the opt-in helper lifecycle
regression from the same component shell:

```sh
cargo test --locked -p maple-agent-app --test system_theme_helper -- --ignored
```

It launches the real executable in helper mode and verifies theme output,
parent-pipe shutdown, and absence of application-state writes. It does not
change the desktop theme. Parser, environment-scrubbing and palette precedence
tests run in the ordinary app test suite.

The root `ci.yml` runs this component's lanes (`scripts/ci/lanes.py`) when
Agent or its shared Rust dependencies change; [CI](../../docs/ci.md) lists the
tiers and labels. Root `just lane <id>` runs one lane as CI does. `just ci` is
the full local format, lint, build and test gate; `just release` separately
validates the optimized binary. PR jobs have no signing or publishing
credentials. See the root agent guide for shared checks.

Two by-hand checks complement the automated ones: the
[scenario checklist](docs/scenario-checklist.md), one scenario per feature
with the results of each run, and the
[performance check](docs/performance-check.md), a quick release-build sanity
check.

## Update and release boundary

Packaged Prod updates support macOS, Linux and Windows through separate signed
platform feeds. Windows also provides a signed per-user manual installer;
see [desktop builds](docs/desktop-builds.md).

Packaged Prod builds use the independent signed Agent update feed. Dev and
unpackaged/MDE builds do not check a production updater. The Prod profile pins
the update URL and reviewed primary/backup public keys; inherited environment
variables cannot replace them. Packaged Prod builds check the compiled platform
feed; publication is a separate protected operation. Invitations require the
signed installed-update rehearsal for each supported platform.
An empty-key profile still reports that updates are not configured and makes no
update requests.

Updates authenticate product, channel, version, target, byte size and SHA-256
before staging. Automatic downloads default on; installation requires the
explicit Restart to Update action, fresh feed revalidation and profile-host
shutdown. Read-only installations offer a manual download. Linux x86_64 uses
a user-local directory bundle, with atomic directory exchange and relaunch
through its AppRun entrypoint. Its packaged update gate must be independently
validated. See [updater development and activation](docs/updates.md).

The [desktop build workflow](docs/desktop-builds.md) provides Actions artifacts
without creating releases or advancing update feeds. Agent publication uses
its own R2 bucket and workflow, with no GitHub Release or tag creation and no
Research Worker/feed changes. Production activation and release publication
require their own authorization. Local
`just release` and `just dist` only build local files.

## Shared dependencies and provenance

The workspace manifest and lockfile independently select a published
`maple-sdk` version; the former SDK fork patch is removed. The component still
consumes `maple-proxy` at `../../proxy`. Local SDK links to `../../sdk/rust`
remain supported for development under the
[SDK consumer version policy](../../docs/sdk-publishing.md#consumer-version-policy).
Keep Agent and its embedded proxy on one resolved SDK source/version. The Nix
source fileset includes the local SDK source and attestation assets so local
links work too; a registry-pinned build uses its locked registry package.

See [import provenance and follow-up work](../../docs/maple-agent-import.md).
This component preserves the original GPUI history; its old nested workflows
are replaced by root monorepo CI.

## License

MIT. See `LICENSE`.

## Desktop remote access

See [remote access](docs/remote-access.md) for pairing, host selection, recovery,
local-only account controls and the separate infrastructure/native trial gates.
Remote access is off by default; the initial development configuration has no
implicit public-relay fallback.

### Feedback

The quiet **Send feedback** button at the bottom of the expanded sidebar opens
an anonymous form. It sends only the chosen category, message, app version,
platform and build profile to the shared `/v1/feedback` service. Dev and Prod
use the same endpoint. No account, conversation, logs or attachments are added.
The Rust client is owned by this product (`app/src/feedback.rs`); it has no
submission credential. Failed submissions retain the draft, and an unchanged
retry reuses its random submission ID to avoid duplicate rows. Shift+Enter adds
a line; Escape or Cancel closes the form when no submission is in flight.
Feedback is conventional Cloudflare-hosted storage; avoid sensitive content.
