"""Offline byte-preservation regressions for the TCP/VSOCK forwarder."""

from collections import deque
import socket
import threading
import time
import unittest
from unittest import mock

import traffic_forwarder as forwarder


class ScriptedSocket:
    def __init__(self, reads=(), writes=()):
        self.reads = deque(reads)
        self.writes = deque(writes)
        self.sent = bytearray()
        self.send_arguments = []
        self.read_count = 0
        self.before_read = None

    def settimeout(self, timeout):
        pass

    def recv(self, size):
        if self.before_read:
            self.before_read(self.read_count)
        self.read_count += 1
        result = self.reads.popleft() if self.reads else b""
        if isinstance(result, BaseException):
            raise result
        return result

    def send(self, data):
        self.send_arguments.append(bytes(data))
        result = self.writes.popleft() if self.writes else len(data)
        if callable(result):
            result = result()
        if isinstance(result, BaseException):
            raise result
        count = min(result, len(data))
        self.sent.extend(data[:count])
        return count

    def sendall(self, data):
        # Model sendall's hidden partial progress for baseline regression runs.
        pending = memoryview(data)
        while pending:
            count = self.send(pending)
            if not count:
                raise ConnectionError("No progress")
            pending = pending[count:]

    def shutdown(self, how):
        pass


class ObservedSocket:
    def __init__(self, sock):
        self.sock = sock
        self.write_timed_out = threading.Event()
        self.release_reader = threading.Event()

    def __getattr__(self, name):
        return getattr(self.sock, name)

    def write(self, operation, data):
        try:
            return operation(data)
        except socket.timeout:
            self.write_timed_out.set()
            if not self.release_reader.wait(3):
                raise RuntimeError("Test reader was not released")
            raise

    def send(self, data):
        return self.write(self.sock.send, data)

    def sendall(self, data):
        return self.write(self.sock.sendall, data)


class ForwardingTests(unittest.TestCase):
    def setUp(self):
        forwarder.shutdown_flag.clear()
        self.sockets = []
        self.workers = []
        self.worker_errors = []
        self.release_events = []

    def tearDown(self):
        forwarder.shutdown_flag.set()
        for event in self.release_events:
            event.set()
        for sock in self.sockets:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        for worker in self.workers:
            worker.join(3)
        for sock in self.sockets:
            sock.close()
        forwarder.shutdown_flag.clear()
        self.assertFalse(any(worker.is_alive() for worker in self.workers))
        self.assertEqual(self.worker_errors, [])

    def start_worker(self, target, *args):
        def run():
            try:
                target(*args)
            except BaseException as error:
                self.worker_errors.append(error)
        worker = threading.Thread(target=run, daemon=True)
        self.workers.append(worker)
        worker.start()
        return worker

    def test_short_writes_and_repeated_timeouts_preserve_suffix_before_next_read(self):
        source = ScriptedSocket(reads=[b"abcdef", b"NEXT", b""])
        destination = ScriptedSocket(writes=[2, socket.timeout(), socket.timeout(), 1, 3])
        observed_at_read = []
        source.before_read = lambda _: observed_at_read.append(bytes(destination.sent))

        forwarder.forward(source, destination, "synthetic", "client->server")

        self.assertEqual(destination.sent, b"abcdefNEXT")
        self.assertEqual(observed_at_read, [b"", b"abcdef", b"abcdefNEXT"])
        self.assertEqual(destination.send_arguments,
                         [b"abcdef", b"cdef", b"cdef", b"cdef", b"def", b"NEXT"])

    def test_receive_timeout_still_retries(self):
        source = ScriptedSocket(reads=[socket.timeout(), b"after idle", b""])
        destination = ScriptedSocket()
        forwarder.forward(source, destination, "synthetic", "client->server")
        self.assertEqual(destination.sent, b"after idle")

    def test_shutdown_during_blocked_write_exits_without_reading_next_chunk(self):
        entered = threading.Event()
        release = threading.Event()
        self.release_events.append(release)

        def blocked_write():
            entered.set()
            if not release.wait(3):
                raise RuntimeError("Test write was not released")
            return socket.timeout()

        source = ScriptedSocket(reads=[b"pending", b"unread", b""])
        destination = ScriptedSocket(writes=[blocked_write])
        worker = self.start_worker(forwarder.forward, source, destination,
                                   "synthetic", "client->server")
        self.assertTrue(entered.wait(2), "Writer never reached the blocked send")
        forwarder.shutdown_flag.set()
        release.set()
        worker.join(2)
        self.assertFalse(worker.is_alive())
        self.assertEqual(source.read_count, 1)
        self.assertEqual(destination.send_arguments, [b"pending"])
        self.assertEqual(destination.sent, b"")

    def test_zero_byte_send_exits_without_consuming_next_chunk(self):
        source = ScriptedSocket(reads=[b"pending", b"unread", b""])
        destination = ScriptedSocket(writes=[0])
        with self.assertLogs(level="ERROR"):
            forwarder.forward(source, destination, "synthetic", "client->server")
        self.assertEqual(source.read_count, 1)
        self.assertEqual(destination.send_arguments, [b"pending"])

    def test_both_timeouts_are_set_before_first_worker_starts(self):
        client, server = mock.Mock(), mock.Mock()
        workers = [mock.Mock(), mock.Mock()]
        observed_at_start = []
        for worker in workers:
            worker.start.side_effect = lambda: observed_at_start.append(
                (client.settimeout.call_args, server.settimeout.call_args,
                 server.connect.call_args))

        with mock.patch.object(forwarder.socket, "AF_VSOCK", 40, create=True), \
                mock.patch.object(forwarder.socket, "socket", return_value=server), \
                mock.patch.object(forwarder.threading, "Thread", side_effect=workers):
            forwarder.handle_connection(client, ("127.0.0.1", 1234), 16, 8000, "synthetic")

        self.assertEqual(observed_at_start,
                         [(mock.call(1.0), mock.call(1.0), mock.call((16, 8000)))] * 2)
        self.assertEqual(server.settimeout.call_args_list, [mock.call(30), mock.call(1.0)])
        client.settimeout.assert_called_once_with(1.0)

    def test_real_backpressure_preserves_exact_bytes_after_observed_timeout(self):
        producer, source = socket.socketpair()
        outgoing, consumer = socket.socketpair()
        self.sockets.extend((producer, source, outgoing, consumer))
        for sock in self.sockets:
            sock.settimeout(2)
        outgoing.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
        outgoing.settimeout(0.1)
        consumer.settimeout(0.1)
        destination = ObservedSocket(outgoing)
        self.release_events.append(destination.release_reader)
        payload = bytes(range(256)) * 256

        def produce():
            producer.sendall(payload)
            producer.shutdown(socket.SHUT_WR)

        writer = self.start_worker(produce)
        relay = self.start_worker(forwarder.forward, source, destination,
                                  "synthetic", "client->server")
        self.assertTrue(destination.write_timed_out.wait(2), "No write timeout observed")
        destination.release_reader.set()
        received = bytearray()
        deadline = time.monotonic() + 5
        while len(received) < len(payload) and time.monotonic() < deadline:
            try:
                chunk = consumer.recv(8192)
            except socket.timeout:
                if not relay.is_alive() and not writer.is_alive():
                    break
                continue
            if not chunk:
                break
            received.extend(chunk)
        # Read exact bytes rather than relying on the separately scoped EOF policy.
        self.assertEqual(len(received), len(payload))
        self.assertEqual(received, payload)
        writer.join(2)
        relay.join(2)
        self.assertFalse(writer.is_alive() or relay.is_alive())


if __name__ == "__main__":
    unittest.main()
