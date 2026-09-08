# Maple CPython worker

This crate owns task-lived CPython processes. It has no GPUI, Goose, account,
permission, or SDK dependency. The caller supplies an opaque task key, a packaged
interpreter, an immutable working directory and environment, and an owner lifetime.
The caller remains responsible for authorization before every operation.

## Execution and lifetime

`Runtime::bind` installs a task binding without starting Python. The first
`TaskHandle::execute` reserves one of four worker slots and starts the exact
packaged executable. A protocol handshake validates CPython 3.13.15, executable,
working directory, and protocol version before any cell executes. There is no
system-Python fallback, runtime download, interpreter probe, or automatic replay.

Each generation has one real `__main__` namespace, a continuously running asyncio
loop on the main thread, and one foreground cell at a time. Top-level `await` is
supported. A non-`None` final expression is displayed and retained in `_`; an
ordinary coroutine value is not automatically awaited. Exceptions preserve partial
assignments. Background tasks continue between cells; synchronous blocking calls
pause the loop. The runtime is ordinary native Python execution, not a sandbox.

`execute_guarded` invokes the caller's launch fence synchronously around admission
and spawn. The returned future never holds that fence. Dropping even an unpolled
execution future cancels the admitted unfinished call. Cancelling a completed
call's token does not retire its idle worker. Owner revocation always retires it.

`reset` ends the worker generation while preserving the logical task binding.
`retire` permanently fences that binding, including previously prepared handles.
Both fence admission before returning their cleanup future. Neither spawns a
replacement. A later permitted call starts fresh only after exact prior cleanup.
Starting, executing, idle, retiring, and cleanup-pending workers all count toward
the capacity limit. There is no eviction, idle expiry, or cell execution deadline.

Shutdown has one two-second cooperative grace window, followed by process-group
or Windows Job termination. Supervision owns child cleanup independently of the
request future. A five-second caller observation timeout does not release the slot
or abandon cleanup. Forced termination can skip Python cleanup handlers and lose
buffered output. Descendants that deliberately escape ordinary process containment
are outside this runtime's guarantee.

On Unix, supervision retains one process-wrap group wait. On Windows, a concrete
Job guard assigns the suspended child before resume, terminates the Job, and
observes its active-process count reaching zero. process-wrap 9.1.0's Job wait
accepts completion-port packets that do not prove all processes have exited, so it
is not used as the Windows cleanup proof. This implementation does not change the
existing shell tool's process handling.

## Protocol and output

Private non-inheritable descriptors carry four-byte big-endian length-prefixed
UTF-8 JSON. Frames are limited to 1 MiB before allocation. Messages carry a worker
generation and, where applicable, execution identity. The separate control reader
can request shutdown while Python is executing a cell.

User stdin returns EOF. Python text/binary streams produce attributed output;
raw file-descriptor, native, and subprocess writes are captured as unattributed
output. Invalid UTF-8 is replaced. Drain fences deliver already-emitted synchronous
output before the terminal frame; stdout/stderr have no total ordering guarantee.
Late output never modifies a completed outcome and is consumed once by a later
call. Retention is bounded independently of draining:

- Source: 256 KiB per cell; traceback history: 64 cells or 1 MiB.
- Output chunks: 16 KiB; transport queues: at most 256 KiB plus reserved controls.
- Foreground text: 64 KiB, reserving 16 KiB for the result or traceback.
- Background: 64 KiB with a bounded chunk count.

Outcomes report generation, execution ID, output, result/error, elapsed time,
dropped bytes, attributed background chunks, and state-loss notices. Custom Python
formatting methods retain native authority and may block or allocate; cancellation
and process termination cover a hung unfinished call, not a memory sandbox.

## Distribution and checks

Development and standalone archives use the hash-pinned Python Build Standalone
20260901 CPython 3.13.15 normal-GIL build. Nix packages retain locked nixpkgs'
CPython 3.13.15 closure. Both carry `runtime.json`, `worker.py`, and licenses. The
interpreter launches with `-I -B -u`: project imports are enabled only after worker
bootstrap imports, and bytecode writes cannot mutate signed runtime resources.
The bundle guarantees the standard library; it does not select project virtual
environments or provide a writable shared package installation.

This is a deliberate simplification from PR #2's optional-IPython proof of concept.
CPython is the only implementation. IPython magics, SDK/controller calls, RLM,
namespace serialization, and optional engines are absent. PR #2 remains historical
evidence of a broader possible system, not the implementation specification.

From the repository's Nix development shell:

```sh
just python-test
cargo test -p maple-code-mode --locked
just code-mode-smoke
```

Raw Cargo tests consume the prepared fixture and fail if it is missing. Explicit
Nix fixtures use `MAPLE_CODE_MODE_RUNTIME_MANIFEST`. `code-mode-smoke --manifest
PATH` exercises a relocated or signed package with the production resolver and
framed worker. Native Windows/Linux/Intel validation belongs to their CI runners;
a successful macOS ARM64 run is not evidence for those platforms.
