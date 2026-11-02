# Python scratchpad

Maple's built-in `python` tool runs bundled CPython 3.14 in the task's project
directory. Every task gets it in a build that bundles the runtime; the tool
is absent otherwise. Like every Maple tool, it runs without asking for
approval. It is a scratchpad for the model, not a user-facing feature: the
transcript shows its calls as ordinary tool rows titled "Python: <first line>".

```json
{"code":"answer = 40\nanswer + 2"}
```

Variables, functions, imports, and `_` survive later tool calls and model turns.
Each loaded task has its own interpreter. The final non-`None` expression is
displayed and stored in `_`. Top-level `await` runs on the same continuously
active asyncio loop, so async clients and background tasks can survive between
cells. An ordinary coroutine returned as the final expression is displayed
without being awaited. Use awaitable APIs instead of `asyncio.run()` inside
this loop. Synchronous calls such as `time.sleep()` or `subprocess.run()` pause
background asyncio work until they return; the tool's description steers
project commands, tests and long-running processes to the shell.

The tool declares Pi's sequential execution mode: a model response that calls
`python` runs its whole tool batch one call at a time, in the order the model
made the calls, so cells see each other's effects. Use `asyncio.gather` inside a
cell for concurrent async work.

The bundle provides the standard library. Project modules can be explicitly
imported from the task root; compatible dependency directories can be explicitly
added to `sys.path`. A shell-created virtual environment does not change the
retained interpreter.

## Dependencies and process pools

The Python Build Standalone bundle includes pip. Never install into the
interpreter's shared directory or an app bundle: this changes the packaged
runtime and can invalidate its signature. Install compatible wheels into an
explicit directory outside the runtime, then add that directory to this
interpreter's `sys.path`:

```python
import importlib
from pathlib import Path
import subprocess
import sys

# Choose a fresh directory in the task project, outside the application/runtime.
deps = (Path.cwd() / ".maple-python-deps" / "excel-v1").resolve()
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
use a separate environment for packages that require a build. Dependencies and
saved files persist on disk after a reset; imported Python objects do not. Reuse
a dependency directory only for the same interpreter version and platform.

Spawned process pools need picklable functions from importable `.py` modules.
Functions defined only in a scratchpad cell are not importable by spawned children
and can fail or hang a pool. Put the function in a module under the task root or
an explicitly added dependency directory, import it, and pass that imported
function to the pool.

## Results

Results include the worker generation and execution ID, captured stdout/stderr,
final value or traceback, omitted-byte counts, and state-loss notices. The first
call of each interpreter identifies the exact interpreter and working directory.
Raw/native and subprocess output is labelled unattributed; late Python output
retains its original execution ID and is consumed by a later call. Large outputs
should be written to files. Output limits do not impose a Python memory sandbox.

## Lifetime

- **Reset.** `{"code":"","reset":true}` discards the scratchpad without starting
  another; non-empty code with `reset` starts fresh before running it.
- **Stop.** Stop retires an unfinished cell and its state. It does not
  immediately interrupt synchronous Python or native calls: code can continue
  producing filesystem or other effects during the two-second retirement grace
  period, before forced termination. Cleanup handlers may be skipped and
  buffered output lost. Stopping after a cell finished keeps its namespace.
- **Capacity.** At most four interpreters run per Maple host. When a task needs
  one while all four exist, Maple discards the least recently used idle
  scratchpad of another task; that task's next call starts fresh and says so.
  If all four are running cells, the call returns an error instead.
- **Archive and unload.** Archiving a task, or archiving or removing its
  project, discards its scratchpad. Unloading a task (idle eviction of loaded
  tasks, deletion, project trust changes) or stopping Maple retires it.
- **Reloads.** State is never serialized. A task with history that is loaded
  again, after a restart or eviction, starts with a fresh interpreter, and its
  first result says that earlier variables, imports and background work are gone.

Errors keep partial assignments and external effects; there is no rollback or
replay. Settling a task and compacting its context keep the scratchpad.

## Implementation and delivery

The GPUI-free [runtime crate](../crates/maple-code-mode/README.md) owns framing,
bounded output, process supervision and capacity. Maple's `python` tool
(`crates/maple-agent/src/agent/tools/python.rs`) binds it to the task: one
interpreter per loaded task, started lazily on the first call with the shell
tool's environment (the login shell's PATH and `AGENT_SESSION_ID`).

Development builds prepare the hash-pinned Python Build Standalone runtime with
`just python-prepare` (`just build` and `just run` do this) into
`target/debug/runtime/python`, beside the binary. `just python-test` runs the
worker's protocol suite and `just code-mode-smoke` the native worker through the
app's package resolver. An executable without a bundled runtime offers no
`python` tool.
