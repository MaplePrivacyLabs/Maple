# Python scratchpad

Maple's built-in `python_code` tool runs bundled CPython 3.13.15 in the task's
working directory. It is available to Maple-owned desktop and ACP tasks through
their existing permission flow. Its display name is **Python**.

```json
{"code":"answer = 40\nanswer + 2"}
```

Variables, functions, imports, and `_` survive later tool calls and model turns.
Each task has its own interpreter. The final non-`None` expression is displayed
and stored in `_`. Top-level `await` runs on the same continuously active asyncio
loop, so async clients and background tasks can survive between cells. An ordinary
coroutine returned as the final expression is displayed without being awaited.
Use awaitable APIs instead of `asyncio.run()` inside this loop. Synchronous calls
such as `time.sleep()` or `subprocess.run()` pause background asyncio work until
they return.

The bundle provides the standard library. Project modules can be explicitly
imported from the task root; compatible dependency directories can be explicitly
added to `sys.path`. A shell-created virtual environment does not change the
retained interpreter, and the shared bundle is not a package-install destination.
The existing raw execution tools remain available for separate environments.

## Permissions and results

Python is explicitly ask-before in Maple's tool policy. Auto mode uses its
existing one-shot approval behavior; other modes and ACP use their existing
approval routing. Python source is not classified as read-only. The desktop
permission card shows complete multiline source and the reset flag in a scrollable
pane. Approval authorizes ordinary native Python filesystem, network, and process
access. Background work can continue after the foreground call completes.

Results include the worker generation and execution ID, captured stdout/stderr,
final value or traceback, omitted-byte counts, and state-loss notices. First use
identifies the exact interpreter and working directory. Expanded Python cards keep
their code and output available after a generated summary appears. Raw/native and
subprocess output is labelled unattributed; late Python output retains its original
execution ID and is consumed by a later call. Large outputs should be written to
files. Runtime retention limits do not impose a Python memory sandbox.

## Reset, Stop and task ownership

The model can reset its own scratchpad:

```json
{"code":"", "reset":true}
```

Empty code with reset releases the worker after cleanup without starting another.
Nonempty code with reset waits for cleanup, then starts a fresh generation. The
desktop task menu offers **Reset Python** for retained state, including settled
tasks. It resolves current state at click time and confirms success after cleanup.
Finish or Stop a running/preparing task before using that menu action. Externally
leased tasks are controlled through their owning ACP session.

Stop retires an unfinished admitted Python cell and its state. Cancelling a later
model step preserves an already completed cell's namespace and background work.
Reset ends retained background work explicitly. Ordinary Python exceptions keep
partial assignments and external effects; there is no rollback or replay.

Archive, project removal, deletion, owner replacement, and app/runtime shutdown
retire affected Python state. Archive and project removal update visibility without
waiting for process cleanup. Context compaction and settling a task preserve Python.
Restored/reopened tasks start fresh after prior cleanup; state is not serialized.

There are four worker slots per Maple service, including workers still starting
or cleaning up. Desktop and standalone ACP processes have separate services. No
task is automatically evicted. Capacity errors name only tasks the caller may see,
and explain how to free a slot. An owning ACP client can close its session or
connection; idle `session/cancel` does not reset Python.

## Implementation and delivery

The GPUI-free [runtime crate](../crates/maple-code-mode/README.md) owns framing,
bounded output, process supervision and capacity. Maple binds it to existing
account/task/context authority. Reconstructed developer clients share that task
binding. Goose-created subagents construct their own clients and have no Python
capability in this feature.

Package resolution and the existing bounded login-PATH probe run only for an
approved first execution, outside Maple lifecycle locks. Every admission rechecks
the installed context and run cancellation. The retained environment removes the
context's inherited scrub keys and the five named Buzz bridge values; ordinary
custom values and explicit context PATH remain supported. Windows environment
matching respects case-insensitive names. The existing shell environment behavior
is unchanged.

Standalone builds use hash-pinned Python Build Standalone 20260901 artifacts. Nix
retains locked nixpkgs' exact CPython closure. The app launches the packaged
executable with `-I -B -u`, without PATH fallback or runtime downloads. Development
preparation happens through `just python-prepare`; `just build`, `just test`, and
`just debug-app` prepare their required resources. Complete archives include the
interpreter, worker, manifest and upstream license notices.

CPython-only execution deliberately simplifies PR #2's optional-IPython PoC. That
historical full-system experiment remains preserved. This feature adds no Maple
Python SDK, controller, semantic projection, RLM, Settings toggle or shortcuts.
Future SDK/RLM work needs a separate reviewed scope; no unused engine abstraction
or host-RPC protocol is introduced in anticipation of it.
