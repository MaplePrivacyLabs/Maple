"""Focused worker checks against an explicitly prepared package interpreter.

Run through the repository environment, for example:
  nix develop -c target/debug/runtime/python/bin/python3.13 -I \
    crates/maple-code-mode/python/test_worker.py \
    --python /absolute/worktree/target/debug/runtime/python/bin/python3.13

There is deliberately no interpreter discovery, download, or missing-fixture
skip. Rust native-worker tests additionally cover host supervision and limits.
"""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import queue
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock


PYTHON = None
WORKER = Path(__file__).with_name("worker.py")


class NativeWorker:
    def __init__(self, root=None):
        self.temporary = tempfile.TemporaryDirectory(prefix="maple-python-ø ")
        self.root = str(Path(root or self.temporary.name).resolve())
        self.process = subprocess.Popen(
            [str(PYTHON), "-I", "-B", "-u", str(WORKER), "--generation", "7"],
            cwd=self.root,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=os.name != "nt",
        )
        self.messages = queue.Queue(maxsize=128)
        self.execution_id = 0
        self.bootstrap = bytearray()
        threading.Thread(target=self._reader, daemon=True).start()
        threading.Thread(target=self._stderr, daemon=True).start()
        self.ready = self.receive()
        if self.ready.get("type") != "ready":
            raise AssertionError(self.ready)

    def _stderr(self):
        while chunk := self.process.stderr.read(4096):
            available = max(0, 16384 - len(self.bootstrap))
            self.bootstrap.extend(chunk[:available])

    def _reader(self):
        try:
            while True:
                header = self.process.stdout.read(4)
                if not header:
                    break
                if len(header) != 4:
                    raise AssertionError("partial frame header")
                length = struct.unpack(">I", header)[0]
                if not 0 < length <= 1024 * 1024:
                    raise AssertionError("oversized worker frame")
                data = self.process.stdout.read(length)
                if len(data) != length:
                    raise AssertionError("partial frame payload")
                message = json.loads(data.decode("utf-8", "strict"))
                if message.get("generation") != 7:
                    raise AssertionError("wrong worker generation")
                for name in ("text", "traceback"):
                    if name in message and len(message[name].encode("utf-8")) > 16384:
                        raise AssertionError("oversized worker text")
                self.messages.put(message)
        except BaseException as error:
            self.messages.put(error)
        finally:
            self.messages.put({"type": "eof"})

    def receive(self, timeout=5):
        try:
            value = self.messages.get(timeout=timeout)
        except queue.Empty:
            raise AssertionError(
                f"worker receive timeout; exit={self.process.poll()}; "
                f"bootstrap={bytes(self.bootstrap)!r}"
            ) from None
        if isinstance(value, BaseException):
            raise value
        return value

    def send(self, message):
        self.send_bytes(json.dumps(message, ensure_ascii=True).encode("utf-8"))

    def send_bytes(self, data):
        self.process.stdin.write(struct.pack(">I", len(data)) + data)
        self.process.stdin.flush()

    def begin(self, code):
        self.execution_id += 1
        self.send({
            "type": "execute", "generation": 7,
            "execution_id": self.execution_id, "code": code,
        })
        return self.execution_id

    def collect(self, execution_id=None):
        execution_id = execution_id or self.execution_id
        retained = []
        retained_bytes = 0
        output_bytes = 0
        while True:
            message = self.receive()
            if message["type"] == "output":
                size = len(message["text"].encode("utf-8"))
                output_bytes += size
                if retained_bytes + size <= 128 * 1024:
                    retained.append(message)
                    retained_bytes += size
            else:
                retained.append(message)
            if message["type"] == "done":
                if message["execution_id"] != execution_id:
                    raise AssertionError("wrong terminal execution")
                return retained, output_bytes
            if message["type"] in ("fatal", "eof"):
                raise AssertionError(message)

    def execute(self, code):
        return self.collect(self.begin(code))[0]

    def shutdown(self):
        if self.process.poll() is None:
            try:
                self.send({"type": "shutdown", "generation": 7})
            except (BrokenPipeError, OSError, ValueError):
                pass
        deadline = time.monotonic() + 4
        while self.process.poll() is None and time.monotonic() < deadline:
            try:
                self.messages.get(timeout=0.05)
            except queue.Empty:
                pass
        if self.process.poll() is None:
            if os.name != "nt":
                os.killpg(self.process.pid, signal.SIGKILL)
            else:
                self.process.kill()
        self.process.wait(timeout=3)
        for handle in (self.process.stdin, self.process.stdout, self.process.stderr):
            handle.close()
        self.temporary.cleanup()


def result(messages):
    return next((item["text"] for item in messages if item["type"] == "result"), None)


def output(messages, stream=None):
    return "".join(
        item["text"] for item in messages
        if item["type"] == "output" and (stream is None or item["stream"] == stream)
    )


class PackagedWorkerTests(unittest.TestCase):
    def setUp(self):
        self.worker = NativeWorker()
        self.addCleanup(self.worker.shutdown)

    def test_exact_identity_real_main_state_and_eof_stdin(self):
        self.assertEqual(self.worker.ready["version"], "3.13.15")
        self.assertEqual(self.worker.ready["implementation"], "cpython")
        self.assertEqual(Path(self.worker.ready["executable"]).resolve(), PYTHON.resolve())
        self.assertEqual(self.worker.ready["cwd"], self.worker.root)
        self.assertEqual(result(self.worker.execute("values = [2, 3, 5]\nsum(values)")), "10")
        code = "import __main__, sys\n(values is __main__.values, _, sys.stdin.read(), __name__)"
        self.assertEqual(result(self.worker.execute(code)), "(True, 10, '', '__main__')")
        self.assertEqual(result(self.worker.execute("def saved(): return values\nsaved()")), "[2, 3, 5]")

    def test_top_level_await_loop_reuse_and_continuous_background_progress(self):
        code = """import asyncio
ticks = []
original_loop = asyncio.get_running_loop()
async def ticking():
    while True:
        ticks.append(1)
        await asyncio.sleep(0.005)
ticker = asyncio.create_task(ticking())
await asyncio.sleep(0.01)
len(ticks)
"""
        before = int(result(self.worker.execute(code)))
        time.sleep(0.06)
        messages = self.worker.execute("(len(ticks) > " + str(before) + ", asyncio.get_running_loop() is original_loop)")
        self.assertEqual(result(messages), "(True, True)")
        self.assertEqual(result(self.worker.execute("import time\nn = len(ticks)\ntime.sleep(0.04)\nlen(ticks) == n")), "True")

    def test_final_expression_is_not_automatically_awaited(self):
        messages = self.worker.execute("async def value(): return 42\nvalue()")
        self.assertIn("coroutine object value", result(messages))
        self.assertEqual(result(self.worker.execute("await _")), "42")

    def test_python_raw_subprocess_and_binary_outputs_drain_before_done(self):
        code = """import os, sys, subprocess
print('python-text')
sys.stdout.buffer.write(b'binary-\\xff-\\xf0\\x9f\\x98\\x80')
os.write(1, b'raw-output')
os.write(2, b'raw-error')
subprocess.run([sys.executable, '-I', '-c', "print('child-output')"], check=True)
"""
        messages = self.worker.execute(code)
        text = output(messages)
        for expected in ("python-text", "binary-�-😀", "raw-output", "raw-error", "child-output"):
            self.assertIn(expected, text)
        for item in messages:
            if item["type"] == "output" and any(fragment in item["text"] for fragment in ("raw-output", "raw-error", "child-output")):
                self.assertIsNone(item["execution_id"])
        self.assertEqual(messages[-1]["type"], "done")

    def test_background_output_and_unhandled_exception_keep_origin(self):
        code = """import asyncio
async def later():
    await asyncio.sleep(0.04)
    print('from-first-cell')
    raise ValueError('background-failure')
asyncio.create_task(later())
None
"""
        self.worker.execute(code)
        time.sleep(0.08)
        messages = self.worker.execute("42")
        self.assertIn("from-first-cell", output(messages))
        self.assertIn("background-failure", output(messages))
        for item in messages:
            if item["type"] == "output":
                self.assertEqual(item["execution_id"], 1)

    def test_binary_utf8_is_incremental_and_incomplete_bytes_flush_before_done(self):
        messages = self.worker.execute("import sys\nsys.stdout.buffer.write(b'\\xf0\\x9f')\nsys.stdout.buffer.write(b'\\x98\\x80')\nsys.stdout.buffer.write(b'\\xf0')\nNone")
        self.assertEqual(output(messages), "😀�")
        self.assertTrue(all(item["execution_id"] == 1 for item in messages if item["type"] == "output"))

    def test_errors_preserve_partial_state_and_traceback_source(self):
        messages = self.worker.execute("def fail():\n    raise ValueError('example')\nretained = 17\nfail()")
        self.assertEqual(messages[-1]["status"], "error")
        error = next(item["traceback"] for item in messages if item["type"] == "error")
        self.assertIn("<maple-python-7-cell-1>", error)
        self.assertIn("raise ValueError('example')", error)
        self.assertIn('line 4, in <module>', error)
        self.assertIn('line 2, in fail', error)
        self.assertNotIn(str(WORKER), error)
        self.assertEqual(result(self.worker.execute("retained")), "17")
        syntax = self.worker.execute("def broken(")
        error = next(item["traceback"] for item in syntax if item["type"] == "error")
        self.assertIn('File "<maple-python-7-cell-3>", line 1', error)
        self.assertIn("    def broken(\n              ^\n", error)
        self.assertIn("SyntaxError: '(' was never closed\n", error)
        self.assertNotIn(str(WORKER), error)

    def test_file_errors_report_both_paths_in_the_message(self):
        missing = str(Path(self.worker.root, "missing-ø.xlsx"))
        destination = str(Path(self.worker.root, "result.xlsx"))
        self.worker.execute(f"missing = {missing!r}\ndestination = {destination!r}")
        for code in ("open(missing)", "import os\nos.rename(missing, destination)"):
            messages = self.worker.execute(code)
            error = next(item["traceback"] for item in messages if item["type"] == "error")
            message = error.splitlines()[-1]
            self.assertIn("FileNotFoundError: [Errno 2]", message)
            self.assertIn(repr(missing), message)
            if "rename" in code:
                self.assertIn(" -> " + repr(destination), message)

    def test_tracebacks_retain_dependency_frames(self):
        messages = self.worker.execute("import json\njson.loads('not-json')")
        error = next(item["traceback"] for item in messages if item["type"] == "error")
        self.assertIn("<maple-python-7-cell-1>", error)
        self.assertIn("decoder.py", error)
        self.assertIn("JSONDecodeError", error)
        self.assertNotIn(str(WORKER), error)

    def test_output_flood_is_bounded_and_terminal_survives_backpressure(self):
        self.worker.begin("import sys\nsys.stdout.write('x' * (16 * 1024 * 1024))\nNone")
        time.sleep(0.1)
        messages, _ = self.worker.collect()
        self.assertEqual(messages[-1]["status"], "ok")
        self.assertGreater(messages[-1]["dropped_stdout_bytes"], 0)
        later = self.worker.execute("42")
        self.assertGreaterEqual(later[-1]["dropped_stdout_bytes"], messages[-1]["dropped_stdout_bytes"])

    def test_large_representations_and_tracebacks_are_bounded(self):
        messages = self.worker.execute("[['x' * 100000] * 1000] * 1000")
        self.assertLessEqual(len(result(messages).encode("utf-8")), 16384)
        self.assertIn("...", result(messages))
        messages = self.worker.execute("raise ValueError('bad' * 100000)")
        error = next(item["traceback"] for item in messages if item["type"] == "error")
        self.assertLessEqual(len(error.encode("utf-8")), 16384)
        self.assertEqual(messages[-1]["status"], "error")

    def test_project_import_and_two_workers_are_isolated(self):
        Path(self.worker.root, "task_module.py").write_text("ANSWER = 93\n")
        self.assertEqual(result(self.worker.execute("import task_module\nprivate = 7\ntask_module.ANSWER")), "93")
        other = NativeWorker()
        self.addCleanup(other.shutdown)
        self.assertEqual(result(other.execute("'private' in globals()")), "False")

    def test_shutdown_cancels_active_cell_and_runs_finally(self):
        self.worker.begin("import asyncio\ntry:\n    await asyncio.sleep(100)\nfinally:\n    print('cooperative-finally')")
        time.sleep(0.04)
        self.worker.send({"type": "shutdown", "generation": 7})
        messages = self.worker.collect()[0]
        self.assertEqual(messages[-1]["status"], "cancelled")
        self.assertIn("cooperative-finally", output(messages))
        self.worker.process.wait(timeout=3)

    @unittest.skipIf(os.name == "nt", "Unix EOF watchdog is a Unix-specific guarantee")
    def test_control_eof_exits_even_with_blocking_foreground(self):
        self.worker.begin("while True: pass")
        time.sleep(0.04)
        self.worker.process.stdin.close()
        self.worker.process.wait(timeout=4)

    @unittest.skipUnless(os.name == "nt", "Win32 standard handles require native Windows")
    def test_win32_standard_handle_output(self):
        code = """import ctypes
k = ctypes.WinDLL('kernel32', use_last_error=True)
k.GetStdHandle.argtypes = [ctypes.c_uint32]
k.GetStdHandle.restype = ctypes.c_void_p
k.WriteFile.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint32), ctypes.c_void_p]
n = ctypes.c_uint32()
data = ctypes.create_string_buffer(b'win32-output')
assert k.WriteFile(k.GetStdHandle(-11 & 0xffffffff), data, 12, ctypes.byref(n), None)
"""
        messages = self.worker.execute(code)
        self.assertIn("win32-output", output(messages))
        self.assertTrue(all(item["execution_id"] is None for item in messages if item["type"] == "output"))


class ProtocolTests(unittest.TestCase):
    def assert_fatal(self, payload=None, prefix=None):
        worker = NativeWorker()
        self.addCleanup(worker.shutdown)
        if prefix is not None:
            worker.process.stdin.write(prefix)
            worker.process.stdin.flush()
        else:
            worker.send_bytes(payload)
        self.assertEqual(worker.receive()["type"], "fatal")

    def test_oversized_length_rejected_without_payload(self):
        self.assert_fatal(prefix=struct.pack(">I", 1024 * 1024 + 1))

    def test_invalid_utf8_rejected(self):
        self.assert_fatal(payload=b"\xff")

    def test_unknown_direction_wrong_generation_bool_id_and_duplicate_fields(self):
        for payload in (
            b'{"type":"done","generation":7}',
            b'{"type":"shutdown","generation":8}',
            b'{"type":"execute","generation":7,"execution_id":true,"code":"42"}',
            b'{"type":"shutdown","generation":7,"generation":7}',
            b'{"type":"execute","generation":7,"execution_id":1,"code":"\\ud800"}',
        ):
            with self.subTest(payload=payload):
                self.assert_fatal(payload=payload)

    def test_source_limit_and_nonincreasing_execution_identity(self):
        self.assert_fatal(payload=json.dumps({
            "type": "execute", "generation": 7,
            "execution_id": 1, "code": "x" * (256 * 1024 + 1),
        }).encode())
        worker = NativeWorker()
        self.addCleanup(worker.shutdown)
        worker.execute("42")
        worker.send({"type": "execute", "generation": 7, "execution_id": 1, "code": "0"})
        self.assertEqual(worker.receive()["type"], "fatal")

    def test_shutdown_immediately_after_admission_still_settles_once(self):
        worker = NativeWorker()
        self.addCleanup(worker.shutdown)
        worker.begin("import asyncio\nawait asyncio.sleep(100)")
        worker.send({"type": "shutdown", "generation": 7})
        messages = worker.collect()[0]
        self.assertEqual(messages[-1]["status"], "cancelled")
        worker.process.wait(timeout=3)
        self.assertEqual(worker.receive()["type"], "eof")


class BootstrapUnitTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location("maple_worker_test", WORKER)
        cls.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.module)

    def test_output_coalescing_preserves_attribution_controls_and_frame_limit(self):
        with mock.patch.object(threading.Thread, "start"):
            transport = self.module._Transport(-1, 7, lambda: self.fail("broken transport"))
        transport.output("stdout", "first", 1)
        transport.output("stdout", " second", 1)
        transport.output("stderr", "error", 1)
        transport.output("stdout", "next cell", 2)
        transport.control({"type": "done", "execution_id": 2})
        transport.output("stdout", "late", 2)
        transport.output("stdout", "ø" * (self.module.MAX_OUTPUT_BYTES // 2), 2)
        messages = [entry[0] for entry in transport.queue]
        self.assertEqual([message["type"] for message in messages],
                         ["output", "output", "output", "done", "output", "output"])
        self.assertEqual([message.get("text") for message in messages[:5]],
                         ["first second", "error", "next cell", None, "late"])
        self.assertEqual([message.get("execution_id") for message in messages],
                         [1, 1, 2, 2, 2, 2])
        self.assertTrue(all(len(message.get("text", "").encode("utf-8")) <=
                            self.module.MAX_OUTPUT_BYTES for message in messages))

    def test_coalesced_output_still_bounds_queue_and_reserves_controls(self):
        with mock.patch.object(threading.Thread, "start"):
            transport = self.module._Transport(-1, 7, lambda: self.fail("broken transport"))
        for _ in range(40000):
            transport.output("stdout", "12345678", 1)
        self.assertLessEqual(transport.output_bytes, self.module.MAX_OUTPUT_QUEUE_BYTES)
        retained = sum(len(entry[0]["text"]) for entry in transport.queue)
        self.assertEqual(retained + transport.counts()["stdout"], 320000)
        self.assertGreater(transport.counts()["stdout"], 0)
        self.assertTrue(transport.control({"type": "done", "execution_id": 1}))
        self.assertEqual(transport.queue[-1][0]["type"], "done")

    def test_exception_fields_are_bounded_without_custom_str(self):
        class FileError(OSError):
            def __str__(self):
                raise AssertionError("exception __str__ must not run")

        error = FileError(2, "missing", "source.xlsx", None, "result.xlsx")
        formatted = self.module._bounded_traceback(error)
        self.assertIn("FileError: [Errno 2] missing: 'source.xlsx' -> 'result.xlsx'", formatted)
        for error in (
            FileError(2, "missing", "ø" * 100000),
            FileError(2, "ø" * 100000, "source.xlsx"),
            SyntaxError("ø" * 100000, ("source.py", 1, 100000, "ø" * 100000)),
        ):
            formatted = self.module._bounded_traceback(error)
            self.assertLessEqual(len(formatted.encode("utf-8")), self.module.MAX_VALUE_BYTES)
            self.assertIn("...", formatted)

    def test_traceback_frame_and_chain_limits_remain_bounded(self):
        previous = None
        for index in range(6):
            error = ValueError(f"cause-{index}")
            error.__cause__ = previous
            previous = error
        formatted = self.module._bounded_traceback(error)
        self.assertNotIn("cause-0", formatted)
        self.assertNotIn("cause-1", formatted)
        self.assertEqual(formatted.count("ValueError: cause-"), 4)

        def recurse(depth):
            if depth:
                return recurse(depth - 1)
            raise ValueError("deep")

        try:
            recurse(40)
        except ValueError as error:
            formatted = self.module._bounded_traceback(error)
        self.assertEqual(formatted.count('  File "'), 32)
        self.assertIn("additional frames omitted", formatted)

    def test_source_cache_enforces_both_bounds(self):
        cache = self.module._SourceCache()
        for index in range(70):
            cache.remember(f"<test-cell-{index}>", "x\n", 2)
        self.assertEqual(len(cache.entries), 64)
        self.assertNotIn("<test-cell-0>", self.module.linecache.cache)
        for index in range(6):
            cache.remember(f"<large-cell-{index}>", "x" * (256 * 1024), 256 * 1024)
        self.assertLessEqual(cache.bytes, 1024 * 1024)
        self.assertEqual(len(cache.entries), 4)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python", type=Path, required=True)
    args, remaining = parser.parse_known_args()
    if not args.python.is_absolute() or not args.python.is_file():
        parser.error("--python must name an existing absolute package interpreter; run just python-prepare")
    PYTHON = args.python
    unittest.main(argv=[sys.argv[0], *remaining])
