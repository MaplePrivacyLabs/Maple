# Maple Agent (GPUI prototype)

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

### Backend / frontend boundary

`app/src/backend.rs` owns the runtime: it is the only file that drives
`maple_agent`'s services, holding a private Tokio runtime and exposing an
async facade (`AgentBackend`) plus one event stream. UI modules import data
types from `maple_agent` (timeline items, session summaries) but talk to the
running agent through that facade only. This mirrors Maple's own edge-adapter
pattern, so a future process split replaces the facade without touching UI
code.

The runtime began as a copy of Research’s Tauri agent (now
`apps/maple-research/frontend/src-tauri/src`) with Tauri removed, and now runs
tasks on the pi-* crates instead of an embedded Goose. Research keeps its own
Goose, with an independent dependency graph.

## Features

- Sign in with email and password, or with GitHub, Google, or Apple OAuth.
  Packaged Dev uses hosted browser sign-in and an automatic loopback return;
  Prod and unpackaged builds retain the callback URL paste flow. Hosted Dev
  requires the matching Auth host and provider/backend callback configuration;
  building the client does not deploy those services.
  The session persists in `auth.json` (mode 0600) so the next launch and
  the `acp` mode skip sign-in. The window opens while the saved session is
  checked, and a check that cannot reach the server keeps the credentials
  for the next launch; only a refusal from the server signs the user out.
- Agent chat with streaming Markdown, tool calls, agent questions, image
  attachments (picker, paste, or drag and drop), a per-message Copy button,
  and a context-window indicator. Every tool call runs without asking for
  approval.
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
  Ctrl+Enter (Cmd+Enter) steers it into the current turn. Queued messages
  can be sent now, edited in the composer (the message keeps its place in
  the queue), or removed.
- Sidebar search filters tasks and projects by name; Escape clears it.
- Up and Down in an empty composer recall prompts sent in this window.
- Optional composer-only Vim editing provides Normal, Insert, and characterwise
  Visual modes, Unicode-aware motions and text objects, operators and counts,
  an unnamed register, undo/redo transactions, and structured dot repeat.
  Enable it under General settings; every other text field stays standard.
- Optional application Vim navigation moves a stable semantic selection through
  Chat sidebar and transcript rows, modal choices, and Settings controls. It is
  independent of composer Vim and leaves ordinary text fields unchanged.
- Projects (working directories) with pinned and recent roots, rename,
  open in the file manager, and remove. The home directory and the
  directory the app was launched from are trusted by default. Other
  projects that provide skills, prompt templates, or a `.maple/SYSTEM.md`
  ask once for a trust decision before those load.
- Sessions grouped by project, with rename, archive, and restore.
- Settings: General (web tools, appearance, tool call details, desktop
  notifications, tool call summaries, composer Vim, application Vim, and the
  speech voice and speed), Keyboard Shortcuts, System prompt, Integrations
  (detected built-ins and custom MCP servers), Usage (the plan meter from
  the billing API), and About.
- Dark and light themes; the default follows the system.
- Billing status from the Maple billing API.
- Desktop notifications when a task finishes or asks a question while the
  window is not focused.
- Release check on launch: a banner links to a newer GitHub release.
  Nothing is downloaded or installed by the app.
- Window size and maximized state persist between launches.

### Integrations preview

On macOS and Linux, Settings > Integrations can set up computer use inside
Maple itself. The embedded CUA runtime uses the pinned Cua Driver Rust SDK; it
does not need a separate daemon, executable, or MCP child process.

On macOS, setup reports and requests Accessibility and Screen Recording for
Maple's own app identity. Grants held by a separately installed CuaDriver app
do not transfer to Maple. On Linux the desktop portal asks for consent the
first time a task captures the screen or sends input. Under GNOME on Wayland
the `winrects@cua` GNOME Shell extension that ships with the SDK is required,
because Mutter exposes no window geometry or screen capture to an ordinary
client; Settings reports it as an unmet requirement until it is installed and
the session has been restarted once.

CUA keeps its native screenshot defaults. Every model receives full
accessibility text plus a bounded projection of exact structured grounding
data such as window IDs and element tokens. Vision models retain the canonical
image blocks; text-only models instead receive a CUA-specific description from
Maple's existing Gemma image helper, with raw screenshot blocks removed before
the primary-model request. Maple owns the task-scoped session lifecycle and
prevents models from mixing standalone CLI or other MCP session identities into
the embedded transport.

Enabling an integration sets a device-local default for new tasks. Existing
tasks keep their frozen integration choice and expose CUA as an independent
per-task switch in the composer. A task that never chose a backend adopts the
device default only when it can actually run it. Maple no longer looks for a
separately installed CuaDriver application: a task saved with its stdio entry
loses that entry on its next run, and a device setting that selected it reads
as not set up. Custom STDIO and Streamable HTTP MCP servers remain account
configuration that may roam between devices.

The embedded design, migration rules, privacy boundary, and preview limits are
documented in [`docs/embedded-cua.md`](docs/embedded-cua.md).

#### Custom MCP servers

Settings > Integrations also keeps custom MCP servers: a command Maple starts
(stdio) or a Streamable HTTP endpoint, each with environment variables,
headers, and a per-request timeout. A server switched on in Settings is on for
new tasks, and the composer's menu switches servers on or off for one task.
A task's servers start when it runs; its first prompt waits up to ten seconds
for them, and a server that connects later joins from the next prompt. Their
tools reach the model as `mcp__<server>__<tool>`, and their instructions join
the system prompt. A server that fails to connect is named once in a notice
and tried again at the task's next run. Changes in Settings apply from a
task's next run.

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
Normal mode and shows a small `NORMAL`, `INSERT`, or `VISUAL` badge; login,
search, rename, settings, prompt, question, and MCP fields keep their ordinary
editing behavior.

The preview includes `h/j/k/l`, `w/b/e`, `0/$`, `gg/G`, `i/a/I/A/o/O`,
`d/c/y` with motions or `iw`/`aw`, `dd/cc/yy`, counts, `v`, `x`, `p/P`,
`u`, `Ctrl-R`, and structured `.` repeat. Arrow keys also move in Normal and
Visual modes. Escape leaves Insert or Visual for Normal; Enter sends in Insert
or Normal, while Shift-Enter inserts a newline only in Insert.

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
`[a`/`]a`, `gi`, and `Ctrl-W h/j/k/l`. `Space s n` starts a task and `Space ,`
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
Per-binding changes are stored in `settings.json` under `shortcut_overrides`;
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
produces both profiles on protected `master`; macOS packages use Developer ID
signing and notarization, and Linux packages are portable AppImages.

The root also offers `just agent-check`, `just agent-build`, and
`just agent-dev`. Native development on macOS requires full Xcode at
`/Applications/Xcode.app`; Linux libraries come from Nix. Windows CI uses the
same Rust version resolved by the component’s pinned `flake.lock`, with
Cargo commands from this directory.

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
bundle is named Maple Agent Debug with identifier
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
maple-agent --version       Print the version.
```

### `maple-agent login`

Prompts for the account email (or takes `--email`) and the password, signs
in, and saves the session the same way the desktop app does. Use it on a
machine that never opens the window so `maple-agent acp` has a sign-in. The
password is always prompted for; there is no flag for it. OAuth sign-in is
desktop-only for now.

### `maple-agent acp`

Runs a standalone ACP agent over stdio for editors and ACP clients. It
reuses the sign-in saved by the desktop app and hosts its own runtime, so
the desktop app does not need to run. Logs go to the log file only; stdout
is the ACP channel. If no sign-in is saved, it exits with a message that
tells the user to sign in from the desktop app first.

### `maple-agent proxy`

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
| `proxy` | `maple-agent proxy`. |

```sh
cargo build --release -p maple-agent-app --no-default-features --features acp
cargo build --release -p maple-agent-app --no-default-features --features proxy
```

A mode that is compiled out exits with status 2 and a message that names
the missing feature.

## Configuration

Local unpackaged builds accept the environment settings below. Packaged Dev
and Prod bake their service endpoints, client ID, PCR trust, state namespace,
and update repository at compile time; service and repository overrides are
ignored. See [build profiles](docs/release-profiles.md).

| Variable | Purpose | Default |
| --- | --- | --- |
| `MAPLE_API_URL` | OpenSecret backend. Use `http://127.0.0.1:3000` for a local dev backend. | `https://enclave.trymaple.ai` |
| `MAPLE_BILLING_API_URL` | Maple billing API. | `https://billing.opensecret.cloud` |
| `MAPLE_CLIENT_ID` | OpenSecret client id (UUID). | Maple's id |
| `MAPLE_MODEL` | Model to select at start. | Runtime default |
| `MAPLE_CONTEXT_LIMIT` | Context window size in tokens, when the model catalog does not report one. | Catalog value |
| `MAPLE_SHELL` | The bash the agent's `bash` tool runs. | `/bin/bash`, else `bash` on PATH (Windows: Git Bash; without it, the `powershell` tool) |
| `MAPLE_UPDATE_REPO` | GitHub `owner/repo` containing stable `maple-agent-vX.Y.Z` releases. | `MaplePrivacyLabs/Maple` |
| `MAPLE_DISABLE_UPDATE_CHECK` | `1` turns the release check off. | unset |
| `RUST_LOG` | Log filter. | `info` |

### File locations

The roots follow the platform, the same way the Tauri app's
`app_config_dir` and `app_local_data_dir` do. `XDG_CONFIG_HOME` and
`XDG_DATA_HOME` override the base directories on every platform. The table
shows unpackaged builds; packaged Dev and Prod append `maple-agent-dev` and
`maple-agent-prod` respectively instead of `maple-agent`.

| Root | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Config | `~/.config/maple-agent/` | `~/Library/Application Support/maple-agent/` | `%APPDATA%\maple-agent\` |
| Local data | `~/.local/share/maple-agent/` | `~/Library/Application Support/maple-agent/` | `%LOCALAPPDATA%\maple-agent\` |

| Path | Content |
| --- | --- |
| `<config>/settings.json` | App settings. |
| `<config>/agent/accounts/<scope>/config.json` | Per-account agent configuration (default root, model, custom MCP servers, project trust). May roam between machines. |
| `<config>/agent/accounts/<scope>/AGENTS.md`, `skills/`, `prompts/`, `SYSTEM.md` | The account's own instructions, skills and prompt templates, as Pi's agent folder holds them, and a `SYSTEM.md` that replaces Pi's default prompt ahead of Maple's opening instructions. |
| `<local data>/auth.json` | Sign-in credentials (mode 0600). Device-local; never in a roaming profile. |
| `<local data>/agent/accounts/<scope>/integrations.json` | Per-account defaults for the integrations on this device. |
| `<local data>/agent/accounts/<scope>/sessions/` | One session file (JSONL) per task, and `tasks.db`, the task index (SQLite, WAL). |
| `<local data>/agent/accounts/<scope>/tool_summaries.db` | Model-written one-line summaries of tool calls (SQLite, WAL). |
| `<local data>/agent/accounts/<scope>/attachments/` | Image attachments. |
| `<local data>/agent/acp/accounts/<scope>/config.json` | ACP configuration. |
| `<local data>/logs/maple-agent.log` | Log file. Panics are logged here too. |

Releases before the package rename used `maple-gpui` for both roots. On its
first start an unpackaged build renames an existing `maple-gpui` directory to `maple-agent`
when the new one does not exist yet, so sign-in, settings, and history carry
over in place. Packaged profiles never adopt legacy or Research state.

`<scope>` is the SHA-256 of the account's user id. Small JSON files are
written atomically (temp file, sync, rename) with owner-only permissions.

These directories are separate from the Tauri app's directories. The two
apps must not share session storage.

## Tests

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Root `.github/workflows/agent-ci.yml` selects this component when Agent or
its shared Rust dependencies change. It builds and tests Linux, macOS, and
Windows; Linux additionally runs the feature-matrix lint/headless checks and
a release build. `just ci` is the full local format, lint, build and test gate;
`just release` separately validates the optimized binary. PR jobs have no
signing or publishing credentials. See the root agent guide for shared checks.

Two by-hand checks complement the automated ones: the
[scenario checklist](docs/scenario-checklist.md), one scenario per feature
with the results of each run, and the
[performance check](docs/performance-check.md), a quick release-build sanity
check.

## Update and release boundary

Prod and unpackaged builds accept stable `maple-agent-vX.Y.Z` releases. Dev
accepts only prereleases in `maple-agent-dev-vX.Y.Z`. Each selects the highest
semantic version across a bounded, complete scan of release pages. Research
`vX.Y.Z` releases, drafts, and the other channel are ignored. A failed or
incomplete scan produces no update banner. The app only links to a canonical
GitHub release page; it does not download or install binaries.

The [desktop build workflow](docs/desktop-builds.md) provides Actions artifacts
without creating releases or advancing update feeds. Future Agent releases
must set `make_latest: false` so Research’s global GitHub latest pointer remains
unchanged. Release publication requires its own authorization. Local
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
