# External agents

A Maple task can hand work to an external coding agent that is installed on
the same computer. Supported providers are Codex (`codex`), Claude Code
(`claude`), Grok Build (`grok`) and Antigravity (`antigravity`, its command
is `agy`). All use the same
delegation tools and task controls.

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
launching it. External agents work in the desktop app's tasks; a task
started over ACP does not get them.

The tools are:

| Tool | Purpose |
| --- | --- |
| `list_agent_providers` | Selected providers, their installation status and version, and setup or sign-in guidance. |
| `agent_start` | Start an agent on a self-contained briefing. Blocking by default; `background: true` returns at once. |
| `agent_send` | Give a started agent more instructions in the same thread. |
| `agent_status` | Read an agent's status, last message, changed files, and commands. |
| `agent_cancel` | Stop an agent's current turn and its process. The thread stays on disk; the next `agent_send` resumes it in a fresh process. |

All tools except `list_agent_providers` take `provider` (`"codex"`,
`"claude"`, `"grok"` or `"antigravity"`). `agent_start` also takes optional
`model`, `effort`, and `cwd`. `cwd` must be inside the project root.

A result has Paseo's shape: a status line, the agent ID and thread ID, the
files changed and commands run, the agent's last message inside an
`<agent-response>` block cut at 4,000 characters, and one line of guidance.

Starting an agent is an ordinary tool call, judged by the task's
[permission mode](permissions.md). Codex and Claude Code keep their own
sandbox and approval settings, and Maple accepts the approval requests they
send back. An ACP agent runs in its own mode matching the task's, and what it
asks to approve goes through the task's permission gate (see
[Approvals](#approvals)).

## Skills

Enabling the integration installs three skills into
`<config>/agent/accounts/<scope>/skills/`:

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
ask questions outside plan mode.

Maple does not change what Codex may do; Codex's sandbox does that. When
Codex asks to run a command or change a file, Maple sends `accept` without
asking. A request that arrives after the agent or its turn has ended gets
`cancel`, and so does stopping the agent. Maple never grants
`acceptForSession`.
A question from Codex, blocking in plan mode or asynchronous in the
default mode, goes to the model that called the agent (see
[Questions](#questions)), and the answer goes back in Codex's own shape.

The child runs in the requested project directory, on the user's login PATH, with the
task's id as the shell tool has it, in its own process group (on
Windows, its process tree) so teardown reaches every descendant. It is killed when the runtime stops,
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
runs `claude --version` and `claude auth status --json`. The card shows the
CLI's sign-in status or login instructions; an unavailable, malformed, or
timed-out status check is reported as unknown. Maple reads only the sign-in
boolean, without retaining account details or reading credential files.
As with Codex, sign-in status does not gate enabling the integration.
The CLI is the only additional runtime dependency.

Claude keeps its normal system prompt and configuration. Maple passes
`--permission-mode default` and `--permission-prompt-tool stdio`, never a
bypass-permissions flag. Claude's rules decide which actions need approval;
Maple answers every `can_use_tool` request with allow, except after Stop,
when it declines. `AskUserQuestion` goes to the model that called the agent
(see [Questions](#questions)), including multiple choices when Claude sets
`multiSelect`. Answers change only that call's input; Maple never persists an
allow rule in Claude's settings.

Each turn starts a contained Claude CLI process using `--session-id` initially
and `--resume` after the CLI confirms the session with `system/init` or a
successful result. A failure before confirmation leaves the agent retryable
with a fresh session. This allows optional `model` and `effort` launch
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

The `claude_native_*` runtime tests use a Rust CLI fixture, including a Rust
sleeping child for descendant cleanup; they need no installed `sleep` program
or Claude account.

## How ACP agents are driven

Grok Build and Antigravity speak the
[Agent Client Protocol](https://agentclientprotocol.com) over stdio, and Maple
drives them the way Paseo does, as a client at protocol version 1
(`external_agents/acp_client.rs`). Their catalog entries say how each starts
(`grok agent stdio`, `agy acp --disable-telemetry`), what goes in
`initialize._meta` and `session/cancel._meta`, and how long start-up may
take.

The user installs the CLI, signs in and configures it beforehand, as for
Codex and Claude Code. Maple never calls `authenticate`, never writes the
agent's configuration, and reads none of its files. The one thing it sets is
each entry's `environment`, which only switches off telemetry and
self-updates for the process it starts (`GROK_TELEMETRY_ENABLED=false`,
`GROK_TELEMETRY_TRACE_UPLOAD=false`, `GROK_DISABLE_AUTOUPDATER=1`,
`AGY_CLI_DISABLE_AUTO_UPDATE=true`). Settings cannot tell whether these agents
are signed in without starting them; a signed-out agent's first call returns
the entry's sign-in hint.

- `initialize` offers no file system and no terminal, so the agent uses its
  own tools under its own configuration, and offers form elicitation.
- `session/new` opens a session in the folder, with no MCP servers. A later
  process reopens it with `session/resume`, or `session/load` with the
  replay dropped.
- A turn's `model` and `effort` are set with `session/set_config_option`,
  the model first; a value the agent did not offer is refused before the
  prompt, with the ones it offers.
- The prompt goes as one text block, and its reply ends the turn:
  `end_turn` completes, `cancelled` cancels, an error or an unusual stop
  reason fails.
- Session updates are read leniently and merged per tool call: `execute`
  calls are commands, finished `edit`, `delete` and `move` calls are file
  changes, and `plan` entries are the to-do list.
- A permission request is judged by the task's gate (see
  [Approvals](#approvals)): allowed with its `allow_once` option, refused
  with `reject_once`. Options that outlive the turn are never chosen; with
  neither option, the request is cancelled. However many ways to allow a
  request offers, it is an approval for the gate, never a question for the
  calling model.
- A form elicitation goes to the caller as questions (see
  [Questions](#questions)). Grok asks only through its own
  `_x.ai/ask_user_question`, which Maple refuses like every request it does
  not know (`-32601`); Grok then writes the question in its reply.
- Stop sends `session/cancel`, answers whatever the agent still waits on as
  cancelled, and ends the process; the next `agent_send` resumes the session.

### Approvals

Each ACP entry's `acp.modes` names the agent's own mode for each of the
task's, following Paseo:

| Agent | Read only | Auto | Full access |
| --- | --- | --- | --- |
| Grok Build | `--permission-mode default` | `--permission-mode auto` | `--always-approve` |
| Antigravity | `default` | `auto` | `auto` |

Grok takes its mode only as launch arguments, before `agent stdio`; when the
task's mode changes, the next turn starts a new process that resumes the
session. Antigravity's is the session's `mode` option, set before each
prompt, even when it reads as set already, and Maple follows the mode and
options the agent reports. A request from the agent to change its own mode
is refused and listed under "Not allowed" for the calling model.

What the agent still asks about goes to the task's gate, as the task's own
calls do, with the tool named `<agent> <kind>` (such as `grok edit`) and the
request's input; the agent's title for the action never reaches the
classifier.

- **Full access** allows it once.
- **Auto** puts it to the user: the agent's own auto review already let
  through what it judged safe. The classifier only names the reason on the
  card.
- **Read only** runs what the classifier finds clearly harmless and puts
  the rest to the user.

While the card waits, the agent's status says what it waits on. A refusal
answers `reject_once`, and the agent's activity lists it under "Not
allowed", with the user's note, so the model that called the agent knows. A
request with no connected interface to ask is refused the same way. Agents
without modes, Codex and Claude Code among them, have every request accepted
once.

## Questions

An agent's questions are for the model that started it. That model may answer
them itself, ask the user first with Maple's own question tool, or decline;
the agent's question never opens the user's question card.

- A blocking `agent_start` or `agent_send` returns as soon as the agent asks,
  with status `waiting` and each question's ID, text, and option labels. The
  transcript row shows the question.
- A background agent's question reaches the model as a message the user does
  not see, as a background result does.
- While a question holds the turn, `agent_send` answers it. Its prompt answers
  a single question; `answers` maps question IDs to answers when there are
  several. An answer that names an option's label, in any case, chooses it; a
  comma-separated list of labels chooses several where the question allows
  it. Anything else is sent as the answer's own words. The call then waits for
  the turn's end or the next question, or returns at once with
  `background: true`.
- `agent_cancel` and Stop decline the question and stop the turn.
- Codex's asynchronous questions do not hold the turn. The turn's result lists
  them, and the next `agent_send` answers them as the next turn's input.

## Transcript

The tool call's row streams the agent's text, its commands with exit
status, the files it changed, and its todo list. A task reopened while a
turn runs shows the row as the agent left it. While a turn runs, the
agent also has a row above the composer with a Stop button. A background
turn that ends after the tool returned writes two entries into the task's
session: one the transcript projects back onto the same row, so a reopened
task still shows what the agent did, and a notice saying the agent
finished. Maple hands the model a bounded result snapshot as a message the
user does not see: behind the task's running turn, as a turn of its own, or
in a run it starts when the task is idle. The task can use that result
immediately; `agent_status` remains available for inspecting the agent
again, rather than being required after every completion. If the user stops
the run before the result went, the result stays in the task's history,
where the model reads it with the next message. If the task cannot be
resumed, a second notice asks you to send a message instead. Deleting the
task ends its agents without a report.

## Restarts

An agent's process ends when Maple stops, but its session lives on with its
vendor, and the task keeps a record of each agent: an entry in its session
that the model and the transcript never see, written when a turn starts,
when the agent's session is confirmed, and when the turn ends. It holds the
agent's ID, provider, session ID, folder and last activity, and goes with
the task, on whichever computer runs it. As Paseo does with its agents:

- `agent_send` or `agent_status` for an agent this run of Maple has not
  seen opens it again from its latest record on the task's current branch.
  Nothing starts until its next turn, which resumes its session in the
  task's current mode. An agent whose session was never confirmed cannot be
  resumed.
- New agents are numbered past the ones the task has recorded.
- When a task next runs after a restart (a message, or anything else that
  loads it to run), an agent its records leave working or waiting did not
  finish its turn. Maple records it as cancelled, shows the same notice as a
  stopped turn ("External agent grok-1 (Grok Build) was interrupted."), and
  gives the model one message it reads with that run, naming each such
  agent. One that never had a session is named as one to start again.
  Nothing resumes on its own: the model decides whether to continue.
- A question or approval the agent was waiting on went with its process.

Stopping an agent, from its row or with `agent_cancel`, sends
`turn/interrupt` (translated to Claude’s native `interrupt` control request), waits
briefly for confirmation, then kills the
process group. Codex does not always end a sandboxed command on interrupt,
so the kill is what guarantees nothing keeps running. Stopping Maple or
deleting the task stops each working agent the same way before killing it:
Grok runs each command in a process group of its own, which the kill does
not reach, so Grok has to end the command itself.

## Limits

- One task may start at most four external agents in one run of Maple. An
  agent whose command could not start does not count; the others last until
  the task is deleted or the runtime stops. Agents opened again from their
  records after a restart count too, but are never refused.
- Flatpak builds report external agents as unsupported.
- Codex 0.143 or newer is required.
- Maple cannot sign Codex in. The Integrations card says when a sign-in is
  missing; run `codex login` in a terminal.
- Windows compiles and runs the same code, with `codex.exe` preferred over
  the npm `codex.cmd` shim, but the desktop smoke has not been run there.
- A command Grok moves to the background is reported done, and its turn
  ends, while the command still runs. When the command finishes, Grok starts
  a turn of its own: its text is added to the agent's activity as a separate
  message, which `agent_status` shows, but the caller is not told. Paseo
  behaves the same way. Antigravity keeps its turn open until its background
  commands end.
- A refused request ends Grok's whole turn, which reports as cancelled; the
  result still lists what was not allowed.

## Adding a provider

Providers are data: one entry in `agent/external_agents/catalog.json` gives
the stable ID, name, Settings description and icon, the command to find,
the oldest version Maple accepts, which sign-in probe to run, the transport,
and every sentence Maple says about the provider. Settings cards, detection,
`list_agent_providers`, the tool descriptions' examples, composer rows and
task choices all read the catalog. A difference between providers is a
catalog key or a named behaviour a key selects (`signInProbe`, `transport`);
no code tests a provider ID. No provider-specific composer branch or database
migration is needed. The app keeps each provider's translated card copy by ID
and falls back to the catalog's English.

A task's choices live with the task in the task index, as the
`externalAgents` list of provider IDs in its settings. A provider missing
from it is off; a listed one is on only while Settings also enables it.

The selector carries a typed `kind` alongside `name` and `displayName`.
MCP names and external provider IDs are separate domains; a custom MCP
server named `codex` or `claude` cannot toggle either provider. Older MCP requests
without `kind` continue to mean MCP.

A transport owns its protocol: it turns its wire into the host's terms in
`external_agents/driver.rs` (events, items, requests and replies) and serves
the client operations in `app_server.rs` (initialize, open a thread, start
and interrupt a turn, reply). A new protocol is a `Transport` variant in the
catalog module and its arm of `AgentClient`; the host does not change. An
agent that speaks ACP needs no new transport: its entry sets `transport` to
`acp` and gives its `acp` settings and off switches. Every desktop task's Pi session has
the five tools; each run declares them to the model only while the task may
use a provider, and the tools reject calls for any other provider. Keep
listing filtered by that same selection when more adapters are added.

The `external_agents::tests::end_to_end` tests run the runtime with a
scripted model and the fake Codex: which tasks declare the tools, a
background result reaching the model and the call's row, and agents ending
with their task. Extend them with each adapter's admission behavior.

The `claude_native_*` tests exercise the Rust transport against a deterministic
fake CLI, without network requests. `external_agents::tests::acp` drives a fake
ACP agent the same way, and `acp_client::tests` read the message shapes Grok
Build 1.0.92 and Antigravity 1.5.17 sent in recorded runs. The fixtures re-execute the
Rust test binary through CLI shims on a private search PATH. They cover
streamed activity, accepted approvals, questions, resumption,
provider/session isolation, errors, and process-group cancellation. Live
inference and platform packaging remain separate checks: the ignored
`external_agents::tests::acp_live` drives the installed ACP agents with real
prompts on their own accounts when run by hand with
`MAPLE_LIVE_ACP=grok,antigravity` and `--ignored --nocapture`:

- an edit and a command in read only with every card allowed, a question in
  full access, Stop, and resume in auto;
- Stop during a long command, which must leave no command running;
- a command left running in the background, printing what the caller saw;
- an edit denied with a note, which must not happen and must reach the
  caller;
- both agents at once, and an unknown model refused with the agent's list.
