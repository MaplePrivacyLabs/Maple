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

Python calls submitted together execute one at a time in submission order, after
those Python calls' permissions are resolved. Denied calls do not hold a place in the
queue. At most 32 approved Python calls are admitted per batch, including the
active call; excess calls return a tool error without running. Reset is part of
that order, so later cells see the new namespace. Stop discards waiting calls.
Use `asyncio.gather` inside a cell for concurrent async work. Other tools remain
concurrent and provide no ordering guarantee relative to Python.

The bundle provides the standard library. Project modules can be explicitly
imported from the task root; compatible dependency directories can be explicitly
added to `sys.path`. A shell-created virtual environment does not change the
retained interpreter. The existing raw execution tools remain available for
separate environments.

## Dependencies and process pools

The portable Python Build Standalone bundle includes pip; the Nix runtime does
not guarantee pip. Check availability with `importlib.util.find_spec("pip")`.
Never install into the interpreter's shared directory or an app bundle: this
changes the packaged runtime and can invalidate its signature. When pip is
available, install compatible wheels into an explicit directory outside the
runtime, then add that directory to this interpreter's `sys.path`:

```python
import importlib
import importlib.util
from pathlib import Path
import subprocess
import sys

# Choose a fresh directory in the task project, outside the application/runtime.
deps = (Path.cwd() / ".maple-python-deps" / "excel-v1").resolve()
if importlib.util.find_spec("pip") is None:
    raise RuntimeError("pip is unavailable; use a separate Python environment")
subprocess.run([
    sys.executable, "-I", "-B", "-m", "pip", "--isolated",
    "--disable-pip-version-check", "install", "--no-cache-dir",
    "--only-binary=:all:", "--target", str(deps), "openpyxl",
], check=True)
sys.path.insert(0, str(deps))
importlib.invalidate_caches()
import openpyxl
```

Keep `-I -B` on the child interpreter: the worker's flags are not automatically
inherited by `subprocess`, and import bytecode caches can otherwise modify the
runtime even with `--target`. pip's `--isolated` ignores user configuration and
environment options. `--only-binary=:all:` fails if a compatible wheel is missing;
use a separate environment for packages that require a build. Use reviewed pinned
versions for repeatable work. Dependencies and saved files persist on disk after
Reset; imported Python objects do not. Reuse a dependency directory only for the
same interpreter version and platform. To change an already imported dependency,
install into a fresh directory and reset before importing it. Do not bootstrap
pip into the runtime if it is unavailable.

Spawned process pools need picklable functions from importable `.py` modules.
Functions defined only in a scratchpad cell are not importable by spawned children
and can fail or hang a pool. Put the function in a module under the task root or
an explicitly added dependency directory, import it, and pass that imported
function to the pool. Alternatively run a separate script with its own environment
through the existing execution tools. Pool operations that block also pause this
worker's asyncio loop; use an async API or `await asyncio.to_thread(...)` when
background asyncio work must keep progressing.

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
Stop does not immediately interrupt synchronous Python or native calls: code can
continue producing filesystem or other effects during the retirement grace
period, before forced termination. Cleanup handlers may be skipped and buffered
output lost. Do not treat Stop as undoing writes or as an immediate effect fence.

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

An opt-in scheduler in the pinned Goose fork orders the supported `python_code`
and `developer__python_code` names together. It schedules cold tool streams after
permission decisions in both Goose loop implementations; waiting calls do not
prepare Python, resolve PATH or start workers. This is a bounded batch scheduler,
not a durable job queue or an RLM child scheduler. Direct adapter calls and
unsupported recovered tool-name spellings retain the runtime's busy guard.

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

After preparation, the optional offline dependency check runs from the component's
Nix shell with `python3 -I -B scripts/check-python-dependencies.py --runtime
target/debug/runtime/python`. It copies the runtime, installs a synthetic wheel
outside it, exercises retained imports and importable-module process pools, and
checks that the packaged tree is unchanged. On macOS, use `--app` with an existing
signed app path instead of `--runtime` to verify its signature before and after
the same check. This does not download packages or build an app.

CPython-only execution deliberately simplifies PR #2's optional-IPython PoC. That
historical full-system experiment remains preserved. This feature adds no Maple
Python SDK, controller, semantic projection, RLM, Settings toggle or shortcuts.
Future SDK/RLM work needs a separate reviewed scope; no unused engine abstraction
or host-RPC protocol is introduced in anticipation of it.
