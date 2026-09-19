# External agents

A Maple task can hand work to an external coding agent that is installed on
the same computer. Supported providers are Codex (`codex`) and Claude Code
(`claude`). Both use the same delegation tools and task controls.

## What the task sees

Five tools appear when at least one external agent is selected in the
composer's Integrations menu. Settings > Integrations controls which providers
are available to select for this account on this device. Disabled providers
are hidden from the composer. Enabling a provider in Settings does not select
it for any task: each task starts with external agents off.
Choosing on or off in the composer persists that choice for that task.
Disabling a provider in Settings blocks saved task selections on the next run;
re-enabling it restores their availability without erasing those choices.
Changes take effect on its next run; stop an active run before changing
its selection. CUA and custom MCP servers have independent switches.

An integration enabled in Settings but unavailable on the device remains
visible in the composer and links to Settings for setup.
The runtime checks installation before enabling a provider and again when
launching it. External agents are desktop capabilities: an ACP caller cannot
acquire them by resuming a desktop task.

The tools are:

| Tool | Purpose |
| --- | --- |
| `list_agent_providers` | Selected providers, their installation status and version, and setup or sign-in guidance. |
| `agent_start` | Start an agent on a self-contained briefing. Blocking by default; `background: true` returns at once. |
| `agent_send` | Give a started agent more instructions in the same thread. |
| `agent_status` | Read an agent's status, last message, changed files, and commands. |
| `agent_cancel` | Stop an agent's current turn and its process. The thread stays on disk; the next `agent_send` resumes it in a fresh process. |

All tools except `list_agent_providers` take `provider` (`"codex"` or `"claude"`). `agent_start` also takes optional
`model`, `effort`, and `cwd`. `cwd` must be inside the project root.

A result has Paseo's shape: a status line, the agent ID and thread ID, the
files changed and commands run, the agent's last message inside an
`<agent-response>` block cut at 4,000 characters, and one line of guidance.

Starting an agent is always allowed. The agent's own actions are what get
gated, so the hand-off does not prompt twice.

## Skills

Enabling the integration installs three skills into
`<config>/agent/accounts/<scope>/goose/config/skills/`:

- `/handoff` writes a self-contained briefing and starts an agent in the
  background.
- `/committee` starts several agents on one question with different models
  or efforts, adds the task's own analysis, and compares.
- `/advisor` asks for read-only analysis with "do not edit files" appended.

Disabling removes the files Maple wrote and nothing else. A user's own skill
of the same name is left alone. The files are reconciled at every runtime
start so an upgrade that changed them takes effect.

## How Codex is driven

Maple runs `codex app-server` as a child process, one per Codex thread,
and speaks its JSON-RPC protocol over stdio. Codex uses the user's own
sign-in and `~/.codex` configuration, including its `sandbox_mode` and
`approval_policy`. Maple sends only the prompt, the working directory, and
one feature flag, `features.default_mode_request_user_input`, so Codex can
ask the user questions outside plan mode.

Maple's own permission mode does not change what Codex may do; Codex's
sandbox does that. The mode decides who answers when Codex asks:

| Maple mode | Codex asks to run a command or change a file |
| --- | --- |
| Read only | A Maple permission card. Allow sends `accept`; deny sends `decline`. |
| Allow all | Maple sends `accept` without asking. |

Stopping the agent sends `cancel`. Maple never grants `acceptForSession`.
A question from Codex, blocking in plan mode or asynchronous in the
default mode, opens Maple's question card and the answer goes back in
Codex's own shape; an asynchronous answer that arrives after the turn
ended starts a follow-up turn on the same thread.

The child runs in the requested project directory, on the user's login PATH, with the same
environment scrubbing as the shell tool, in its own process group or job
so teardown reaches every descendant. It is killed when the runtime stops,
on logout, and when its task is deleted. Threads are not ephemeral, so
`codex resume` works from a terminal afterwards.

On macOS, Settings, external-agent tools, and the task's shell tool share the
runtime's recovered interactive login-shell PATH. The shell tool can still
execute commands with bash; it must not substitute bash's startup PATH for
the user's login-shell search path. Maple does not change the process-global
PATH or the user's shell configuration.

The handshake reports the reserved client name `codex_app_server_daemon`,
the same non-originating name Paseo uses.

## How Claude Code is driven

Maple's native Rust transport is adapted from
[Goose's `ClaudeCodeProvider`](https://github.com/AnthonyRonning/goose/blob/785d655d110746147117d23690e09cc7023aa9dc/crates/goose/src/providers/claude_code.rs).
The control protocol types and permission exchange come from that implementation
of Claude's SDK protocol. The adapted source lives in `external_agents/claude.rs`
with its provenance.

Goose keeps its transport private inside its provider. Maple adapts that code
so its existing host retains control of process launch, the working directory,
scoped environment, cancellation, and descendant cleanup. It also bounds
protocol lines, sanitizes errors, projects activity, and supplies question
answers through `updatedInput`.

Install the Claude Code CLI and sign in with `claude auth login`. Detection
runs `claude --version`; it does not read credentials or claim to have verified
the account's sign-in. The CLI is the only additional runtime dependency.

Claude keeps its normal system prompt and configuration. Maple passes
`--permission-mode default` and `--permission-prompt-tool stdio`, never a
bypass-permissions flag. Claude's rules decide which actions need approval;
`can_use_tool` requests go to Maple's current permission mode. Allow all answers
those requests automatically, and Read only shows a one-shot permission card.
`AskUserQuestion` uses Maple's question card. Answers change only that call's
input; Maple never persists an allow rule in Claude's settings.

Each turn starts a contained Claude CLI process using `--session-id` initially
and `--resume` thereafter. This allows optional `model` and `effort` launch
arguments to change on each turn. The process runs in the requested project
directory with the shell tool's scoped environment and process containment.
Stop, task deletion, logout, and runtime shutdown reclaim Claude and its
descendants. A subsequent `agent_send` resumes the saved session. You can also
use `claude --resume` from a terminal.

Claude's text, Bash calls, successful Edit/Write/NotebookEdit calls, and
TodoWrite items feed the existing activity row. Other tools continue to run
under Claude's policy but do not yet have specialized activity summaries.
Command completion shows success or failure; tool-result messages do not
guarantee numeric exit codes. Protocol failures produce a generic error without
forwarding potentially sensitive exception text or CLI stderr.

## Transcript

The tool call's row streams the agent's text, its commands with exit
status, the files it changed, and its todo list. While a turn runs, the
agent also has a row above the composer with a Stop button. A background
turn that ends after the tool returned writes two notices into the session
history: one the transcript projects back onto the same row, so a reopened
task still shows what the agent did, and one plain line saying the agent
finished. Maple delivers a bounded result snapshot directly into the task's
context, in the running turn or a new turn it starts automatically if idle.
The task can use that result immediately; `agent_status` remains available for
inspecting the agent again, rather than being required after every completion.
A completion that arrives as a turn ends is carried into the next turn. If the
task cannot be resumed, the notice asks you to send a message instead. Stopping
the runtime or signing out prevents pending completions from starting work.

Built-in background subagents use the same delivery behavior. Maple reads their
completed result with `load(peek: true)`, retains it for later retrieval, and
injects up to 8,000 characters with an explicit truncation flag. The task can
call `load(source: task_id)` if it needs the remaining output. Completion
messages are hidden from the user's transcript and identify their contents as
delegated agent output. Tasks owned by external clients such as ACP retain the
result in history for their next turn; Maple does not start desktop runs for them.

Goose currently buffers approval requests from built-in background subagents
until a non-peek `load` attaches their permission flow. A subagent waiting for
approval therefore still needs `load` before it can finish; completion delivery
alone cannot unblock it.

Stopping an agent, from its row or with `agent_cancel`, sends
`turn/interrupt` (translated to Claude’s native `interrupt` control request), waits
briefly for confirmation, then kills the
process group. Codex does not always end a sandboxed command on interrupt,
so the kill is what guarantees nothing keeps running.

## Limits

- One task may run at most four external agents at once.
- Switching Maple's mode mid-turn changes who answers Codex's next
  request, not the sandbox Codex already runs under.
- Flatpak builds report external agents as unsupported.
- Codex 0.143 or newer is required.
- Maple cannot sign Codex in. The Integrations card says when a sign-in is
  missing; run `codex login` in a terminal.
- Windows compiles and runs the same code, with `codex.exe` preferred over
  the npm `codex.cmd` shim, but the desktop smoke has not been run there.

## Adding a provider

Register its stable ID, label, description, and Settings projection in
`EXTERNAL_AGENT_INTEGRATIONS` in `agent/integrations.rs`, then add discovery
to `IntegrationDetections` and a transport adapter under `agent/external_agents/`.
Settings gates, composer rows, and task choices use this catalog.
No provider-specific composer branch or database migration is needed.

Task overrides live in the versioned `maple_integrations` extension data,
keyed by provider ID. Missing entries mean disabled. A true entry grants access
only while Settings also enables that provider. CUA
keeps its existing `maple_cua` backend metadata so an old external driver
task cannot silently switch to the embedded backend.

The selector carries a typed `kind` alongside `name` and `displayName`.
MCP names and external provider IDs are separate domains; a custom MCP
server named `codex` or `claude` cannot toggle either provider. Older MCP requests
without `kind` continue to mean MCP.

Provider transport adapters still own their protocol, discovery, progress,
approvals, and cancellation. Extend `ExternalAgentRegistry` dispatch and
provider listing when adding an adapter. The developer client passes only
the task's selected provider IDs and rejects calls for any other provider.
Keep listing filtered by that same selection when more adapters are added.

`resumed_task_refreshes_external_tools_on_every_run` exercises the actual
run configuration and Goose tool cache with old persisted extension state,
warm reuse, cold restore, explicit overrides, and a desktop task leased to
ACP. Extend that regression with each adapter's admission behavior.

The `claude_native_*` tests exercise the Rust transport against a deterministic
fake CLI, without network requests. Both providers' fixtures re-execute the
Rust test binary through CLI shims on a private search PATH. They cover
streamed activity, permissions, questions, resumption,
provider/session isolation, errors, and process-group cancellation. Live
inference and platform packaging remain separate checks.
