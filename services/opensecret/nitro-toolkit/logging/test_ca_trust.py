"""Offline trust-store check for the deployable parent-side logger image."""

import hashlib
from pathlib import Path
import re
import ssl
import unittest

import botocore.httpsession
import botocore.session
import certifi


EXPECTED_CERTIFI_VERSION = "2026.10.19"
EXPECTED_BUNDLE_SHA256 = "50f79f98dac192b20dbf73f35c56f0b0c557ab267ff5199d1117cc1180b70785"
EMUDHRA_G3_SHA256 = "86a1ecba089c4a8d3bbe2734c612ba341d813e043cf9e8a862cd5c57a36bbe6b"
# This root is present in the pinned Botocore 1.43.108 fallback bundle. An
# affected eMudhra intermediate was also cross-signed under Comodo AAA.
COMODO_AAA_SHA256 = "d7a7a0fb5d7e2731d771e9484ebcdef71d5f0c3e0a2948782bc83ee0ea699ef4"


class LoggerCATrustTests(unittest.TestCase):
    def test_installed_bundle_and_botocore_default_trust(self):
        bundle = Path(certifi.where())
        data = bundle.read_bytes()
        certs = re.findall(
            rb"-----BEGIN CERTIFICATE-----\s+.*?-----END CERTIFICATE-----",
            data,
            re.DOTALL,
        )
        fingerprints = {
            hashlib.sha256(ssl.PEM_cert_to_DER_cert(cert.decode("ascii"))).hexdigest()
            for cert in certs
        }

        self.assertEqual(certifi.__version__, EXPECTED_CERTIFI_VERSION)
        self.assertEqual(hashlib.sha256(data).hexdigest(), EXPECTED_BUNDLE_SHA256)
        self.assertEqual((len(certs), len(fingerprints)), (120, 120))
        self.assertNotIn(EMUDHRA_G3_SHA256, fingerprints)
        self.assertNotIn(COMODO_AAA_SHA256, fingerprints)

        session = botocore.session.get_session()
        self.assertIsNone(session.get_config_variable("ca_bundle"))
        client = session.create_client(
            "logs",
            region_name="us-east-2",
            aws_access_key_id="synthetic-access-key",
            aws_secret_access_key="synthetic-secret-key",
            aws_session_token="synthetic-session-token",
        )
        try:
            # Check the effective TLS verifier without sending a request or
            # reading AWS credentials.
            verify = client._endpoint.http_session._verify
            self.assertEqual(botocore.httpsession.get_cert_path(verify), str(bundle))
        finally:
            client.close()


if __name__ == "__main__":
    unittest.main()
