"""Offline contract tests for the host-side credential requester."""

import json
import unittest
from unittest import mock

import credential_requester as requester


class FakeConnection:
    def __init__(self, request):
        self.request = json.dumps(request).encode()
        self.sent = []
        self.closed = False

    def recv(self, _size):
        return self.request

    def send(self, data):
        self.sent.append(data)
        return len(data)

    def close(self):
        self.closed = True

    def response(self):
        return json.loads(b"".join(self.sent))


class CredentialRequesterTests(unittest.TestCase):
    def test_imdsv2_token_request(self):
        response = mock.Mock(text="synthetic-token")
        with mock.patch.object(requester.requests, "put", return_value=response) as put:
            self.assertEqual(requester.get_imdsv2_token(), "synthetic-token")

        put.assert_called_once_with(
            "http://169.254.169.254/latest/api/token",
            headers={"X-aws-ec2-metadata-token-ttl-seconds": "21600"},
        )
        response.raise_for_status.assert_called_once_with()

    def test_credentials_response_preserves_enclave_wire_shape(self):
        conn = FakeConnection({"request_type": "credentials"})
        credentials = {
            "AccessKeyId": "synthetic-access-key",
            "SecretAccessKey": "synthetic-secret-key",
            "Token": "synthetic-session-token",
        }
        with (
            mock.patch.object(requester, "get_imdsv2_token", return_value="synthetic-token"),
            mock.patch.object(requester.requests, "get", return_value=mock.Mock(text="test-role")) as get,
            mock.patch.object(requester, "get_credentials", return_value=credentials) as fetch,
            mock.patch.object(requester, "get_region", return_value="us-east-2"),
        ):
            requester.handle_client(conn, (3, 8003))

        get.assert_called_once_with(
            requester.IMDS_URL,
            headers={"X-aws-ec2-metadata-token": "synthetic-token"},
        )
        fetch.assert_called_once_with("test-role", "synthetic-token")
        self.assertEqual(
            conn.response(),
            {
                "response_type": "credentials",
                "response_value": {**credentials, "Region": "us-east-2"},
            },
        )
        self.assertTrue(conn.closed)

    def test_secrets_manager_response_uses_requested_name_and_region(self):
        conn = FakeConnection({"request_type": "SecretsManager", "key_name": "test-secret"})
        credentials = {
            "AccessKeyId": "synthetic-access-key",
            "SecretAccessKey": "synthetic-secret-key",
            "Token": "synthetic-session-token",
        }
        with (
            mock.patch.object(requester, "get_imdsv2_token", return_value="synthetic-token"),
            mock.patch.object(requester.requests, "get", return_value=mock.Mock(text="test-role")),
            mock.patch.object(requester, "get_credentials", return_value=credentials),
            mock.patch.object(requester, "get_region", return_value="us-east-2"),
            mock.patch.object(requester, "get_secret", return_value="synthetic-value") as get_secret,
        ):
            requester.handle_client(conn, (3, 8003))

        get_secret.assert_called_once_with(
            "test-secret", "us-east-2", "synthetic-access-key",
            "synthetic-secret-key", "synthetic-session-token",
        )
        self.assertEqual(
            conn.response(),
            {"response_type": "secret", "response_value": "synthetic-value"},
        )
        self.assertTrue(conn.closed)

    def test_missing_secret_name_fails_before_metadata_or_aws(self):
        conn = FakeConnection({"request_type": "SecretsManager"})
        with (
            mock.patch.object(requester, "get_imdsv2_token") as token,
            mock.patch.object(requester, "get_secret") as get_secret,
        ):
            requester.handle_client(conn, (3, 8003))

        token.assert_not_called()
        get_secret.assert_not_called()
        self.assertEqual(
            conn.response(),
            {"response_type": "error", "response_value": "Missing key_name for SecretsManager request"},
        )
        self.assertTrue(conn.closed)

    def test_secret_lookup_uses_session_credentials(self):
        client = mock.Mock()
        client.get_secret_value.return_value = {"SecretString": "synthetic-value"}
        session = mock.Mock()
        session.client.return_value = client
        with mock.patch.object(requester.boto3.session, "Session", return_value=session) as make_session:
            value = requester.get_secret(
                "test-secret", "us-east-2", "synthetic-access-key",
                "synthetic-secret-key", "synthetic-session-token",
            )

        self.assertEqual(value, "synthetic-value")
        make_session.assert_called_once_with(
            aws_access_key_id="synthetic-access-key",
            aws_secret_access_key="synthetic-secret-key",
            aws_session_token="synthetic-session-token",
        )
        session.client.assert_called_once_with(service_name="secretsmanager", region_name="us-east-2")
        client.get_secret_value.assert_called_once_with(SecretId="test-secret")


if __name__ == "__main__":
    unittest.main()
