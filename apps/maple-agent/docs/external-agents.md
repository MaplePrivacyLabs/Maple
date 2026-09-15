# External agents

A Maple task can hand work to an external coding agent that is installed on
the same computer. Codex is the first provider. The tool contract is generic
so another harness can be added without changing what the task sees.

## What the task sees

Five tools appear in a desktop task when Codex is enabled under
Settings > Integrations:

| Tool | Purpose |
| --- | --- |
| `list_agent_providers` | Which providers are installed, their version, and whether they are signed in. |
| `agent_start` | Start an agent on a self-contained briefing. Blocking by default; `background: true` returns at once. |
| `agent_send` | Give a started agent more instructions in the same thread. |
| `agent_status` | Read an agent's status, last message, changed files, and commands. |
| `agent_cancel` | Stop an agent's current turn and its process. The thread stays on disk; the next `agent_send` resumes it in a fresh process. |

Every tool takes `provider` (`"codex"`). `agent_start` also takes optional
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

The child runs in the project root, on the user's login PATH, with the same
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

## Transcript

The tool call's row streams the agent's text, its commands with exit
status, the files it changed, and its todo list. While a turn runs, the
agent also has a row above the composer with a Stop button. A background
turn that ends after the tool returned writes two notices into the session
history: one the transcript projects back onto the same row, so a reopened
task still shows what the agent did, and one plain line saying the agent
finished. Maple then tells the task that the agent finished, into the
running turn or the next one.

Stopping an agent, from its row or with `agent_cancel`, sends
`turn/interrupt`, waits briefly for Codex to confirm, then kills the
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
