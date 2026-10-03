"""Offline contract tests for the host-side CloudWatch forwarder."""

import os
import unittest
from unittest import mock

from botocore.exceptions import ClientError

import cloudwatch_logger as logger


class FakeConnection:
    def __init__(self, chunks):
        self.chunks = iter(chunks)
        self.closed = False

    def recv(self, _size):
        return next(self.chunks)

    def close(self):
        self.closed = True


class CloudWatchLoggerTests(unittest.TestCase):
    def test_client_uses_configured_region(self):
        with (
            mock.patch.dict(os.environ, {"AWS_REGION": "us-west-2"}),
            mock.patch.object(logger.boto3, "client") as make_client,
        ):
            self.assertIs(logger.create_cloudwatch_client(), make_client.return_value)

        make_client.assert_called_once_with("logs", region_name="us-west-2")

    def test_existing_group_and_stream_are_accepted(self):
        cloudwatch = mock.Mock()
        cloudwatch.create_log_group.side_effect = ClientError(
            {"Error": {"Code": "ResourceAlreadyExistsException", "Message": "exists"}},
            "CreateLogGroup",
        )
        cloudwatch.create_log_stream.side_effect = ClientError(
            {"Error": {"Code": "ResourceAlreadyExistsException", "Message": "exists"}},
            "CreateLogStream",
        )
        with mock.patch.dict(
            os.environ, {"LOG_GROUP": "/test/enclave", "LOG_STREAM": "test-stream"}
        ):
            names = logger.setup_log_group_and_stream(cloudwatch)

        self.assertEqual(names, ("/test/enclave", "test-stream"))
        cloudwatch.create_log_group.assert_called_once_with(logGroupName="/test/enclave")
        cloudwatch.create_log_stream.assert_called_once_with(
            logGroupName="/test/enclave", logStreamName="test-stream"
        )

    def test_split_utf8_is_forwarded_without_corruption(self):
        conn = FakeConnection([b"before \xf0\x9f", b"\x98\x80 after", b""])
        cloudwatch = mock.Mock()
        with mock.patch.object(logger.time, "time", return_value=1234.5):
            logger.handle_client(conn, (3, 8011), cloudwatch, "/test/enclave", "test-stream")

        events = [call.kwargs["logEvents"][0] for call in cloudwatch.put_log_events.call_args_list]
        self.assertEqual("".join(event["message"] for event in events), "before 😀 after")
        self.assertTrue(all(event["timestamp"] == 1234500 for event in events))
        self.assertTrue(
            all(call.kwargs["logGroupName"] == "/test/enclave" for call in cloudwatch.put_log_events.call_args_list)
        )
        self.assertTrue(conn.closed)


if __name__ == "__main__":
    unittest.main()
