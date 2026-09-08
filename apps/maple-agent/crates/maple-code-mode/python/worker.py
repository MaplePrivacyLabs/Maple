"""Maple's private, packaged CPython worker. This is not a Python sandbox.

The host launches this file with the package interpreter and ``-I -B -u``. Import
all bootstrap dependencies before making the task directory importable.
"""

import ast
import asyncio
import builtins
import codecs
import collections
import contextvars
import inspect
import io
import json
import linecache
import os
import platform
import signal
import struct
import sys
import threading
import time
import types

if os.name == "nt":
    import ctypes
    import msvcrt


PROTOCOL_VERSION = 1
MAX_FRAME_BYTES = 1024 * 1024
MAX_SOURCE_BYTES = 256 * 1024
MAX_OUTPUT_BYTES = 16 * 1024
MAX_OUTPUT_QUEUE_BYTES = 256 * 1024
MAX_CONTROL_SLOTS = 8
MAX_VALUE_BYTES = 16 * 1024
MAX_SOURCE_CELLS = 64
MAX_SOURCE_CACHE_BYTES = 1024 * 1024
MAX_U64 = (1 << 64) - 1
_EXECUTION_ID = contextvars.ContextVar("maple_execution_id", default=None)


def _valid_id(value):
    return type(value) is int and 0 < value <= MAX_U64


def _utf8_size_within(text, limit, errors="replace"):
    """Check user strings without first allocating an arbitrarily large encode."""
    size = 0
    for offset in range(0, len(text), 4096):
        size += len(text[offset : offset + 4096].encode("utf-8", errors))
        if size > limit:
            return None
    return size


class _TextBudget:
    def __init__(self, limit=MAX_VALUE_BYTES):
        self.remaining = limit
        self.parts = []
        self.truncated = False

    def add(self, text):
        if not self.remaining:
            self.truncated = True
            return
        # Slicing by characters first bounds the temporary encoding as well.
        data = text[: self.remaining].encode("utf-8", "replace")
        if len(data) > self.remaining or len(text) > self.remaining:
            self.truncated = True
        retained = data[: self.remaining].decode("utf-8", "ignore")
        self.parts.append(retained)
        self.remaining -= len(retained.encode("utf-8"))

    def finish(self):
        text = "".join(self.parts)
        if self.truncated:
            data = text.encode("utf-8")
            return data[: max(0, len(data) - 3)].decode("utf-8", "ignore") + "..."
        return text


def _bounded_repr(value, limit=MAX_VALUE_BYTES):
    """Bound ordinary containers cumulatively, including temporary strings.

    Custom repr methods still have native Python authority and can allocate or
    block; the host's unfinished-call cancellation also covers this stage.
    """
    out = _TextBudget(limit)
    seen = set()

    def visit(item, depth):
        if not out.remaining:
            out.truncated = True
            return
        kind = type(item)
        if kind is str or kind is bytes or kind is bytearray:
            maximum = min(len(item), max(1, out.remaining // 4))
            out.add(repr(item[:maximum]))
            if maximum < len(item):
                out.add("...")
            return
        if kind not in (list, tuple, dict, set, frozenset):
            out.add(repr(item))
            return
        if id(item) in seen or depth >= 6:
            out.add("...")
            return
        seen.add(id(item))
        opening, closing = {
            list: ("[", "]"),
            tuple: ("(", ")"),
            dict: ("{", "}"),
            set: ("{", "}") if item else ("set(", ")"),
            frozenset: ("frozenset({", "})") if item else ("frozenset(", ")"),
        }[kind]
        out.add(opening)
        iterator = iter(item.items()) if kind is dict else iter(item)
        for index, child in enumerate(iterator):
            if index:
                out.add(", ")
            if index >= 128 or not out.remaining:
                out.add("...")
                break
            if kind is dict:
                visit(child[0], depth + 1)
                out.add(": ")
                visit(child[1], depth + 1)
            else:
                visit(child, depth + 1)
        if kind is tuple and len(item) == 1:
            out.add(",")
        out.add(closing)
        seen.remove(id(item))

    visit(value, 0)
    return out.finish()


def _bounded_traceback(error):
    """Format at most 32 frames and four causes, never frame locals.

    Avoid TracebackException's eager conversion of every exception argument and
    source line. Large ordinary arguments use the same cumulative repr budget.
    """
    out = _TextBudget()
    chain = []
    seen = set()
    current = error
    while current is not None and len(chain) < 4 and id(current) not in seen:
        seen.add(id(current))
        chain.append(current)
        current = current.__cause__ or (
            None if current.__suppress_context__ else current.__context__
        )
    for chain_index, item in enumerate(reversed(chain)):
        if chain_index:
            out.add("\nDuring handling of the above exception:\n\n")
        out.add("Traceback (most recent call last):\n")
        tb = item.__traceback__
        frames = 0
        while tb is not None and frames < 32 and out.remaining:
            code = tb.tb_frame.f_code
            out.add('  File "')
            out.add(code.co_filename[:512])
            out.add('", line ' + str(tb.tb_lineno) + ", in ")
            out.add(code.co_name[:256] + "\n")
            # Only our bounded source cache is consulted; never read an
            # arbitrary source file while formatting a traceback.
            cached = linecache.cache.get(code.co_filename)
            if cached and type(cached) is tuple and len(cached) == 4:
                lines = cached[2]
                if type(lines) is list and 0 < tb.tb_lineno <= len(lines):
                    source = lines[tb.tb_lineno - 1]
                    if type(source) is str:
                        out.add("    " + source[:512].strip() + "\n")
            tb = tb.tb_next
            frames += 1
        if tb is not None:
            out.add("  ... additional frames omitted ...\n")
        if isinstance(item, SyntaxError):
            out.add("  File " + str(item.filename)[:512] + ", line ")
            out.add(str(item.lineno) + "\n")
            if type(item.text) is str:
                out.add("    " + item.text[:512].strip() + "\n")
        out.add(type(item).__name__[:256] + ": ")
        arguments = item.args
        if len(arguments) == 1 and type(arguments[0]) is str:
            out.add(arguments[0])
        else:
            out.add(_bounded_repr(arguments, max(1, out.remaining)))
        out.add("\n")
    return out.finish()


class _Transport:
    """One writer, bounded payload retention, and separately reserved controls.

    FIFO order is intentional: terminal messages cannot overtake accepted
    output. At most 256 KiB plus eight small controls can precede a shutdown
    result. The host continues draining even when its retained text is full.
    """

    def __init__(self, fd, generation, broken):
        self.fd = fd
        self.generation = generation
        self.broken = broken
        self.condition = threading.Condition()
        self.queue = collections.deque()
        self.output_bytes = 0
        self.control_slots = 0
        self.dropped = {"stdout": 0, "stderr": 0}
        self.failed = False
        self.thread = threading.Thread(target=self._write_loop, daemon=True)
        self.thread.start()

    def output(self, stream, text, execution_id, original_bytes=None):
        size = _utf8_size_within(text, MAX_OUTPUT_BYTES)
        if size is None:
            raise RuntimeError("internal output chunk exceeds limit")
        original_bytes = size if original_bytes is None else original_bytes
        # The fixed charge bounds Python-object overhead and tiny-write count.
        cost = size + 256
        with self.condition:
            if self.failed or self.output_bytes + cost > MAX_OUTPUT_QUEUE_BYTES:
                self.dropped[stream] = min(
                    MAX_U64, self.dropped[stream] + original_bytes
                )
                return
            message = {
                "type": "output",
                "generation": self.generation,
                "execution_id": execution_id,
                "stream": stream,
                "text": text,
            }
            self.queue.append((message, cost, None))
            self.output_bytes += cost
            self.condition.notify()

    def control(self, message, delivered=None):
        message = dict(message, generation=self.generation)
        overflow = False
        with self.condition:
            if self.failed:
                return False
            if self.control_slots >= MAX_CONTROL_SLOTS:
                # A protocol bug must not turn reserved control capacity into
                # an unbounded queue or block the control-reader thread.
                self.failed = True
                overflow = True
            else:
                self.queue.append((message, 0, delivered))
                self.control_slots += 1
                self.condition.notify()
        if overflow:
            self.broken()
            return False
        return True

    def counts(self):
        with self.condition:
            return dict(self.dropped)

    def _write_loop(self):
        try:
            while True:
                with self.condition:
                    while not self.queue:
                        self.condition.wait()
                    message, cost, delivered = self.queue.popleft()
                # Every string is bounded before it reaches this thread. JSON
                # escaping can expand a single bounded chunk, never a cell's
                # whole output. No encoded frames accumulate in another queue.
                encoded = json.dumps(
                    message, ensure_ascii=True, separators=(",", ":"), allow_nan=False
                ).encode("utf-8")
                if len(encoded) > MAX_FRAME_BYTES:
                    raise RuntimeError("internal control frame exceeds limit")
                packet = struct.pack(">I", len(encoded)) + encoded
                view = memoryview(packet)
                while view:
                    written = os.write(self.fd, view)
                    if written <= 0:
                        raise BrokenPipeError()
                    view = view[written:]
                with self.condition:
                    if cost:
                        self.output_bytes -= cost
                    else:
                        self.control_slots -= 1
                if delivered is not None:
                    delivered.set()
        except BaseException:
            with self.condition:
                self.failed = True
                self.queue.clear()
                self.output_bytes = 0
                self.control_slots = 0
            self.broken()


class _BinaryOutput(io.RawIOBase):
    def __init__(self, transport, stream, descriptor):
        self.transport = transport
        self.stream = stream
        self.descriptor = descriptor
        self.lock = threading.RLock()
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.carried = 0
        self.execution_id = None

    def writable(self):
        return True

    def fileno(self):
        return self.descriptor

    def write(self, data):
        view = memoryview(data).cast("B")
        execution_id = _EXECUTION_ID.get()
        for offset in range(0, len(view), 4096):
            chunk = view[offset : offset + 4096]
            with self.lock:
                if execution_id != self.execution_id:
                    self.flush()
                    self.execution_id = execution_id
                text = self.decoder.decode(chunk)
                pending = len(self.decoder.getstate()[0])
                consumed = len(chunk) + self.carried - pending
                self.carried = pending
                if text:
                    self.transport.output(self.stream, text, execution_id, consumed)
        return len(view)

    def flush(self):
        with self.lock:
            text = self.decoder.decode(b"", final=True)
            if text:
                self.transport.output(
                    self.stream, text, self.execution_id, self.carried
                )
            self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
            self.carried = 0


class _TextOutput(io.TextIOBase):
    def __init__(self, transport, stream, descriptor):
        self.transport = transport
        self.stream = stream
        self.descriptor = descriptor
        self.buffer = _BinaryOutput(transport, stream, descriptor)

    @property
    def encoding(self):
        return "utf-8"

    @property
    def errors(self):
        return "replace"

    def writable(self):
        return True

    def fileno(self):
        return self.descriptor

    def write(self, text):
        if not isinstance(text, str):
            raise TypeError("write() argument must be str")
        self.buffer.flush()
        for offset in range(0, len(text), 4096):
            chunk = text[offset : offset + 4096].encode("utf-8", "replace")
            self.transport.output(
                self.stream, chunk.decode("utf-8"), _EXECUTION_ID.get()
            )
        return len(text)

    def flush(self):
        # At most three bytes of an incomplete binary UTF-8 character remain.
        # Flush them with replacement before this stream's drain fence.
        self.buffer.flush()


class _RawCapture:
    def __init__(self, reader, writer, transport, stream):
        self.reader = reader
        self.writer = writer
        self.transport = transport
        self.stream = stream
        self.marker = b"\x00maple-drain:" + os.urandom(24) + b"\x00"
        self.pending_lock = threading.Lock()
        self.fence_lock = threading.Lock()
        self.pending = None
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.carried = 0
        threading.Thread(target=self._read_loop, daemon=True).start()

    def _emit(self, data, final=False):
        text = self.decoder.decode(data, final=final)
        pending = len(self.decoder.getstate()[0])
        consumed = len(data) + self.carried - pending
        self.carried = pending
        if text:
            self.transport.output(self.stream, text, None, consumed)
        if final:
            self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def _read_loop(self):
        carry = b""
        try:
            while True:
                data = os.read(self.reader, 4096)
                if not data:
                    self._emit(carry, final=True)
                    return
                data = carry + data
                while True:
                    index = data.find(self.marker)
                    if index >= 0:
                        self._emit(data[:index], final=True)
                        with self.pending_lock:
                            if self.pending is not None:
                                self.pending.set()
                        data = data[index + len(self.marker) :]
                        continue
                    # Retain only an actual marker-prefix suffix. Ordinary
                    # output is delivered immediately even between cells.
                    suffix = min(len(data), len(self.marker) - 1)
                    while suffix and not data.endswith(self.marker[:suffix]):
                        suffix -= 1
                    if suffix:
                        self._emit(data[:-suffix])
                        carry = data[-suffix:]
                    else:
                        self._emit(data)
                        carry = b""
                    break
        except BaseException:
            self.transport.broken()

    async def fence(self):
        # One bounded operation per capture stream; daemon helpers cannot keep
        # interpreter exit alive. The main loop never performs a blocking pipe
        # write, so shutdown can still cancel a cell during drain.
        loop = asyncio.get_running_loop()
        completed = loop.create_future()

        def finish(error):
            if not completed.done():
                if error is None:
                    completed.set_result(None)
                else:
                    completed.set_exception(error)

        def run():
            try:
                with self.fence_lock:
                    event = threading.Event()
                    with self.pending_lock:
                        self.pending = event
                    view = memoryview(self.marker)
                    while view:
                        size = os.write(self.writer, view)
                        if size <= 0:
                            raise BrokenPipeError()
                        view = view[size:]
                    event.wait()
                    with self.pending_lock:
                        self.pending = None
                error = None
            except BaseException as caught:
                error = caught
            try:
                loop.call_soon_threadsafe(finish, error)
            except RuntimeError:
                pass

        threading.Thread(target=run, daemon=True).start()
        await completed


class _SourceCache:
    def __init__(self):
        self.entries = collections.OrderedDict()
        self.bytes = 0

    def remember(self, filename, source, size):
        linecache.cache[filename] = (
            size,
            None,
            source.splitlines(keepends=True),
            filename,
        )
        self.entries[filename] = size
        self.bytes += size
        while len(self.entries) > MAX_SOURCE_CELLS or self.bytes > MAX_SOURCE_CACHE_BYTES:
            oldest, old_size = self.entries.popitem(last=False)
            self.bytes -= old_size
            linecache.cache.pop(oldest, None)


def _read_exact(fd, count, allow_eof=False):
    data = bytearray()
    while len(data) < count:
        chunk = os.read(fd, min(65536, count - len(data)))
        if not chunk:
            if not data and allow_eof:
                return None
            raise ValueError("truncated control frame")
        data.extend(chunk)
    return data


def _unique_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError("duplicate control field")
        value[key] = item
    return value


def _invalid_constant(_value):
    raise ValueError("invalid JSON constant")


class _Worker:
    def __init__(self, generation, control_fd, output_fd):
        self.generation = generation
        self.control_fd = control_fd
        self.loop = asyncio.new_event_loop()
        asyncio.set_event_loop(self.loop)
        self.admission = threading.RLock()
        self.active_id = None
        self.last_id = 0
        self.last_done_id = 0
        self.retiring = False
        self.foreground = None
        self.shutdown_task = None
        self.watchdog_started = False
        self.tasks = set()
        self.source_cache = _SourceCache()
        self.transport = _Transport(output_fd, generation, self.owner_gone)
        self.captures = []
        self.python_streams = []
        module = types.ModuleType("__main__")
        module.__dict__.update(
            __builtins__=builtins,
            __package__=None,
            __spec__=None,
            __loader__=None,
        )
        self.namespace = module.__dict__
        sys.modules["__main__"] = module
        self.loop.set_task_factory(self._task_factory)
        self.loop.set_exception_handler(self._background_exception)

    def _task_factory(self, loop, coroutine, context=None, **kwargs):
        task = asyncio.Task(coroutine, loop=loop, context=context, **kwargs)
        task._maple_execution_id = (
            context.get(_EXECUTION_ID) if context is not None else _EXECUTION_ID.get()
        )
        self.tasks.add(task)
        task.add_done_callback(self.tasks.discard, context=contextvars.Context())
        return task

    def _background_exception(self, _loop, context):
        task = context.get("task") or context.get("future")
        execution_id = getattr(task, "_maple_execution_id", _EXECUTION_ID.get())
        error = context.get("exception")
        try:
            if error is not None:
                text = "Unhandled background exception:\n" + _bounded_traceback(error)
            else:
                text = "Background asyncio error: " + str(context.get("message", ""))[:512]
        except BaseException:
            text = "Unhandled background exception (formatting failed)\n"
        # Include the prefix inside the chunk bound and preserve attribution.
        for offset in range(0, len(text), 4096):
            self.transport.output("stderr", text[offset : offset + 4096], execution_id)

    def install_capture(self):
        null = os.open(os.devnull, os.O_RDONLY)
        os.dup2(null, 0)
        os.close(null)
        for descriptor, name in ((1, "stdout"), (2, "stderr")):
            reader, writer = os.pipe()
            os.set_inheritable(reader, False)
            os.set_inheritable(writer, False)
            os.dup2(writer, descriptor, inheritable=True)
            # Keep the private marker writer independent of user close/dup2.
            self.captures.append(_RawCapture(reader, writer, self.transport, name))
        if os.name == "nt":
            kernel = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel.SetStdHandle.argtypes = [ctypes.c_uint32, ctypes.c_void_p]
            kernel.SetStdHandle.restype = ctypes.c_int
            for descriptor, identifier in ((0, -10), (1, -11), (2, -12)):
                msvcrt.setmode(descriptor, os.O_BINARY)
                handle = msvcrt.get_osfhandle(descriptor)
                if not kernel.SetStdHandle(identifier & 0xFFFFFFFF, handle):
                    raise ctypes.WinError(ctypes.get_last_error())
        sys.stdin = sys.__stdin__ = io.TextIOWrapper(
            io.FileIO(0, "r", closefd=False), encoding="utf-8", errors="replace"
        )
        sys.stdout = sys.__stdout__ = _TextOutput(self.transport, "stdout", 1)
        sys.stderr = sys.__stderr__ = _TextOutput(self.transport, "stderr", 2)
        self.python_streams = [sys.stdout, sys.stderr]

    def owner_gone(self):
        with self.admission:
            self.retiring = True
            if not self.watchdog_started:
                self.watchdog_started = True

                def watchdog():
                    time.sleep(2)
                    # The group assertion avoids ever targeting a caller's
                    # group when a developer invokes the script directly.
                    if os.name != "nt" and os.getpgrp() == os.getpid():
                        try:
                            os.killpg(os.getpgrp(), signal.SIGKILL)
                        except OSError:
                            pass
                    os._exit(1)

                threading.Thread(target=watchdog, daemon=True).start()
        try:
            self.loop.call_soon_threadsafe(self._schedule_shutdown)
        except RuntimeError:
            pass

    def _fatal(self, message):
        self.transport.control({"type": "fatal", "message": message[:1024]})
        with self.admission:
            self.retiring = True
        self.loop.call_soon_threadsafe(self._schedule_shutdown)

    def _control_loop(self):
        try:
            while True:
                prefix = _read_exact(self.control_fd, 4, allow_eof=True)
                if prefix is None:
                    self.owner_gone()
                    return
                size = struct.unpack(">I", prefix)[0]
                if not 0 < size <= MAX_FRAME_BYTES:
                    raise ValueError("control frame length outside limit")
                encoded = _read_exact(self.control_fd, size)
                message = json.loads(
                    encoded.decode("utf-8", "strict"),
                    object_pairs_hook=_unique_object,
                    parse_constant=_invalid_constant,
                )
                if type(message) is not dict:
                    raise ValueError("control message must be an object")
                if not _valid_id(message.get("generation")) or message["generation"] != self.generation:
                    raise ValueError("control generation mismatch")
                kind = message.get("type")
                if kind == "shutdown":
                    if set(message) != {"type", "generation"}:
                        raise ValueError("invalid shutdown fields")
                    with self.admission:
                        if self.retiring:
                            raise ValueError("duplicate shutdown")
                        self.retiring = True
                    self.loop.call_soon_threadsafe(self._schedule_shutdown)
                    # Continue watching EOF independently of the loop, but
                    # reject every subsequent control frame.
                    continue
                if kind != "execute" or set(message) != {
                    "type", "generation", "execution_id", "code"
                }:
                    raise ValueError("invalid control direction or fields")
                execution_id = message["execution_id"]
                source = message["code"]
                if not _valid_id(execution_id) or type(source) is not str:
                    raise ValueError("invalid execution identity or source type")
                source_size = _utf8_size_within(source, MAX_SOURCE_BYTES, errors="strict")
                if source_size is None:
                    raise ValueError("cell source exceeds limit")
                with self.admission:
                    if self.retiring or self.active_id is not None:
                        raise ValueError("execution admitted while busy or retiring")
                    if execution_id <= self.last_id:
                        raise ValueError("execution identity must increase")
                    self.last_id = execution_id
                    self.active_id = execution_id
                self.loop.call_soon_threadsafe(
                    self._start_cell, execution_id, source, source_size
                )
        except BaseException as error:
            # Never echo malformed input, arbitrary exception repr, or source.
            message = str(error) if type(error) is ValueError else "invalid control frame"
            self._fatal(message)

    def _start_cell(self, execution_id, source, source_size):
        context = contextvars.copy_context()
        context.run(_EXECUTION_ID.set, execution_id)
        self.foreground = self.loop.create_task(
            self._cell(execution_id, source, source_size), context=context
        )
        self.foreground.add_done_callback(
            lambda task: self._cell_finished(task, execution_id),
            context=contextvars.Context(),
        )

    def _cell_finished(self, task, execution_id):
        # Cancellation can win before the coroutine executes its first line.
        # Keep the same exactly-once terminal path for that admission race.
        with self.admission:
            if execution_id <= self.last_done_id:
                return
        status = "cancelled" if task.cancelled() else "error"
        if not task.cancelled():
            task.exception()
        self._finish_cell(execution_id, status, 0)

    def _finish_cell(self, execution_id, status, elapsed_ms):
        dropped = self.transport.counts()
        with self.admission:
            if execution_id <= self.last_done_id:
                return
            self.last_done_id = execution_id
            self.active_id = None
            self.transport.control({
                "type": "done", "execution_id": execution_id, "status": status,
                "elapsed_ms": elapsed_ms,
                "dropped_stdout_bytes": dropped["stdout"],
                "dropped_stderr_bytes": dropped["stderr"],
            })

    async def _drain(self):
        for stream in self.python_streams:
            stream.flush()
        await asyncio.gather(*(capture.fence() for capture in self.captures))

    async def _cell(self, execution_id, source, source_size):
        started = time.monotonic_ns()
        status = "ok"
        filename = f"<maple-python-{self.generation}-cell-{execution_id}>"
        result_name = "__maple_cell_value_" + os.urandom(12).hex()
        try:
            if self.retiring:
                raise asyncio.CancelledError()
            self.source_cache.remember(filename, source, source_size)
            tree = compile(
                source, filename, "exec",
                flags=ast.PyCF_ONLY_AST | ast.PyCF_ALLOW_TOP_LEVEL_AWAIT,
                dont_inherit=True,
            )
            has_value = bool(tree.body and isinstance(tree.body[-1], ast.Expr))
            if has_value:
                expression = tree.body[-1]
                assignment = ast.Assign(
                    targets=[ast.Name(id=result_name, ctx=ast.Store())],
                    value=expression.value,
                )
                tree.body[-1] = ast.copy_location(assignment, expression)
                ast.fix_missing_locations(tree)
            code = compile(
                tree, filename, "exec",
                flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT, dont_inherit=True,
            )
            value = eval(code, self.namespace, self.namespace)
            if code.co_flags & inspect.CO_COROUTINE:
                await value
            if has_value:
                value = self.namespace.pop(result_name, None)
                if value is not None:
                    self.namespace["_"] = value
                    self.transport.control({
                        "type": "result", "execution_id": execution_id,
                        "text": _bounded_repr(value),
                    })
        except asyncio.CancelledError:
            status = "cancelled"
        except BaseException as error:
            status = "error"
            try:
                formatted = _bounded_traceback(error)
            except BaseException:
                formatted = "Python exception (traceback formatting failed)"
            self.transport.control({
                "type": "error", "execution_id": execution_id,
                "traceback": formatted,
            })
        finally:
            self.namespace.pop(result_name, None)
        try:
            await self._drain()
        except asyncio.CancelledError:
            status = "cancelled"
        except BaseException:
            status = "error"
            self._fatal("native output drain failed")
        elapsed_ms = min(MAX_U64, max(0, (time.monotonic_ns() - started) // 1_000_000))
        self._finish_cell(execution_id, status, elapsed_ms)

    def _schedule_shutdown(self):
        if self.shutdown_task is None:
            self.shutdown_task = self.loop.create_task(self._shutdown())

    async def _shutdown(self):
        current = asyncio.current_task()
        pending = [task for task in self.tasks if task is not current and not task.done()]
        for task in pending:
            task.cancel()
        if pending:
            # The host owns the single two-second grace. This shorter bounded
            # attempt leaves time for drains and normal process exit handlers.
            await asyncio.wait(pending, timeout=0.75)
        async_generators = self.loop.create_task(self.loop.shutdown_asyncgens())
        await asyncio.wait([async_generators], timeout=0.25)
        if not async_generators.done():
            async_generators.cancel()
        try:
            await asyncio.wait_for(self._drain(), timeout=0.25)
        except BaseException:
            pass
        # A delivery fence uses an existing message rather than extending the
        # wire protocol. If there is no queued control, all accepted output can
        # still be observed through this local transport barrier.
        await self._flush_transport()
        self.loop.stop()

    async def _flush_transport(self):
        deadline = time.monotonic() + 0.25
        while time.monotonic() < deadline:
            with self.transport.condition:
                if not self.transport.output_bytes and not self.transport.control_slots:
                    return
            await asyncio.sleep(0.005)

    def run(self):
        self.install_capture()
        root = os.getcwd()
        if _utf8_size_within(root, MAX_VALUE_BYTES) is None or _utf8_size_within(sys.executable, MAX_VALUE_BYTES) is None:
            raise RuntimeError("runtime identity exceeds metadata limit")
        # -I intentionally keeps user/site environment discovery disabled; only
        # the immutable launch directory is made available for project imports.
        sys.path.insert(0, root)
        ready = threading.Event()
        self.transport.control({
            "type": "ready", "protocol_version": PROTOCOL_VERSION,
            "implementation": sys.implementation.name,
            "version": platform.python_version(), "executable": sys.executable,
            "cwd": root,
        }, delivered=ready)
        ready.wait()
        threading.Thread(target=self._control_loop, daemon=True).start()
        try:
            self.loop.run_forever()
        finally:
            self.loop.close()


def main():
    if len(sys.argv) != 3 or sys.argv[1] != "--generation":
        raise SystemExit("usage: worker.py --generation POSITIVE_U64")
    try:
        generation = int(sys.argv[2])
    except ValueError:
        raise SystemExit("generation must be a positive u64") from None
    if not _valid_id(generation):
        raise SystemExit("generation must be a positive u64")
    control_fd = os.dup(0)
    output_fd = os.dup(1)
    # Windows duplicates of standard streams have a documented inheritance
    # exception. Set these explicitly on both platforms before any user code.
    os.set_inheritable(control_fd, False)
    os.set_inheritable(output_fd, False)
    if os.name == "nt":
        msvcrt.setmode(control_fd, os.O_BINARY)
        msvcrt.setmode(output_fd, os.O_BINARY)
    _Worker(generation, control_fd, output_fd).run()


if __name__ == "__main__":
    main()
