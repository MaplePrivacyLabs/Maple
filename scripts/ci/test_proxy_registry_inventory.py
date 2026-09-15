#!/usr/bin/env python3
"""Regression coverage for an empty namespace versus unavailable package access."""

import contextlib
import copy
import io
import json
import os
import sys
import unittest
from unittest.mock import patch
from urllib.error import HTTPError, URLError

import proxy_registry_inventory as inventory

TOKEN = "FAKE_WORKFLOW_TOKEN_NOT_A_CREDENTIAL"
REGISTRY_TOKEN = "FAKE_ANONYMOUS_PULL_TOKEN"
PACKAGE = {
    "name": inventory.PACKAGE,
    "package_type": "container",
    "visibility": "public",
    "owner": {"id": inventory.OWNER_ID, "login": inventory.OWNER},
    "repository": {"id": inventory.REPOSITORY_ID, "full_name": inventory.REPOSITORY},
}


class FakeGet:
    def __init__(self, *responses):
        self.responses = list(responses)
        self.calls = []

    def __call__(self, url, token=None, *, api_version=None):
        self.calls.append((url, token) if api_version is None else (url, token, api_version))
        if not self.responses:
            raise AssertionError("Unexpected extra request")
        result = self.responses.pop(0)
        if isinstance(result, Exception):
            raise result
        return result


class ProxyRegistryInventoryTests(unittest.TestCase):
    def public_responses(self, tags=None):
        return (
            inventory.Response(200, copy.deepcopy(PACKAGE)),
            inventory.Response(200, {"token": REGISTRY_TOKEN}),
            inventory.Response(200, {"name": inventory.IMAGE, "tags": ["0.3.4", "latest"] if tags is None else tags}),
        )

    def test_missing_package_requires_authenticated_metadata_and_listing(self):
        get = FakeGet(inventory.Response(404), inventory.Response(200, []))
        self.assertEqual(inventory.inventory(TOKEN, get=get), {"name": inventory.IMAGE, "tags": []})
        self.assertEqual(len(get.calls), 2)
        self.assertTrue(all(url.startswith(inventory.API + "/") and token == TOKEN for url, token in get.calls))

    def test_metadata_auth_network_and_service_errors_are_not_empty(self):
        for status in (301, 400, 401, 403, 429, 500, 502, 503):
            with self.subTest(status=status):
                get = FakeGet(inventory.Response(status))
                with self.assertRaises(inventory.InventoryError):
                    inventory.inventory(TOKEN, get=get)
                self.assertEqual(len(get.calls), 1)
        with self.assertRaises(inventory.InventoryError):
            inventory.inventory(TOKEN, get=FakeGet(inventory.InventoryError("Package inventory request failed")))

    def test_metadata_404_does_not_mask_unauthorized_or_failed_listing(self):
        for status in (301, 401, 403, 404, 429, 500):
            with self.subTest(status=status), self.assertRaises(inventory.InventoryError):
                inventory.inventory(TOKEN, get=FakeGet(inventory.Response(404), inventory.Response(status)))
        for data in ({}, None, [{"name": inventory.PACKAGE}], [{"name": "MAPLE-PROXY"}], ["invalid"], [{}]):
            with self.subTest(data=data), self.assertRaises(inventory.InventoryError):
                inventory.inventory(TOKEN, get=FakeGet(inventory.Response(404), inventory.Response(200, data)))

    def test_listing_failures_report_only_status_or_json_shape(self):
        for response, expected in (
            (inventory.Response(403), "HTTP 403"),
            (inventory.Response(404), "HTTP 404"),
            (inventory.Response(429), "HTTP 429"),
            (inventory.Response(500), "HTTP 500"),
            (inventory.Response(200, {"message": TOKEN}), "JSON object, expected array"),
            (inventory.Response(200, TOKEN), "JSON string, expected array"),
        ):
            with self.subTest(expected=expected), self.assertRaisesRegex(inventory.InventoryError, expected) as result:
                inventory.inventory(TOKEN, get=FakeGet(inventory.Response(404), response))
            self.assertNotIn(TOKEN, str(result.exception))

    def test_diagnostics_probe_only_fixed_endpoints_under_both_api_versions(self):
        get = FakeGet(
            inventory.Response(404), inventory.Response(403),
            inventory.Response(404), inventory.Response(200, []),
        )
        result = inventory.diagnose(TOKEN, get=get)
        expected_calls = [
            (url, TOKEN, version)
            for version in ("2022-11-28", "2026-03-10")
            for url in (
                inventory.PACKAGE_URL,
                f"{inventory.API}/orgs/{inventory.OWNER}/packages?package_type=container&per_page=100&page=1",
            )
        ]
        self.assertEqual(get.calls, expected_calls)
        self.assertTrue(result["diagnostic_only"])
        self.assertEqual([probe["http_status"] for probe in result["probes"]], [404, 403, 404, 200])
        self.assertEqual([probe["json_type"] for probe in result["probes"]], ["not-read", "not-read", "not-read", "array"])
        self.assertNotIn("tags", result)
        self.assertNotIn(TOKEN, json.dumps(result))

    def test_diagnostics_hide_values_headers_and_exception_text_and_continue(self):
        get = FakeGet(
            inventory.Response(200, {"message": TOKEN}, TOKEN),
            inventory.InventoryError(TOKEN),
            inventory.InventoryError(TOKEN, status=200, category="invalid-json"),
            inventory.Response(200, TOKEN, TOKEN),
        )
        result = inventory.diagnose(TOKEN, get=get)
        self.assertEqual(len(get.calls), 4)
        self.assertEqual(result["probes"][0]["json_type"], "object")
        self.assertEqual(result["probes"][1]["error_category"], "request-failed")
        self.assertIsNone(result["probes"][1]["http_status"])
        self.assertEqual(result["probes"][2]["http_status"], 200)
        self.assertEqual(result["probes"][2]["error_category"], "invalid-json")
        self.assertEqual(result["probes"][3]["json_type"], "string")
        self.assertNotIn(TOKEN, json.dumps(result))

    def test_diagnostics_require_a_token_before_any_request(self):
        get = FakeGet()
        with self.assertRaises(inventory.InventoryError):
            inventory.diagnose("", get=get)
        self.assertFalse(get.calls)

    def test_bootstrap_admits_only_digest_preparation_without_an_inventory(self):
        for previous in ("0.3.3", "0.4.0"):
            get = FakeGet(inventory.Response(404))
            result = inventory.prepare_bootstrap(TOKEN, "0.4.0", previous, "0.3.3", get=get)
            self.assertEqual(result, {
                "name": inventory.IMAGE, "proxy_version": "0.4.0",
                "bootstrap_only": True, "metadata_status": 404,
            })
            self.assertNotIn("tags", result)
            self.assertNotIn("publish", result)
            self.assertEqual(get.calls, [(inventory.PACKAGE_URL, TOKEN)])

    def test_bootstrap_rejects_missing_invalid_rollback_and_baseline_versions(self):
        invalid = (
            ("", "0.3.3", "0.3.3"), ("0.4.0", "", "0.3.3"),
            ("0.4.0", "0.3.3", ""), ("v0.4.0", "0.3.3", "0.3.3"),
            ("0.04.0", "0.3.3", "0.3.3"), ("0.4.0-beta", "0.3.3", "0.3.3"),
            ("0.4.0\n", "0.3.3", "0.3.3"), (TOKEN, "0.3.3", "0.3.3"),
            ("0.4.0", "0.5.0", "0.3.3"), ("0.3.3", "0.3.3", "0.3.3"),
            ("9" * 129 + ".0.0", "0.3.3", "0.3.3"),
        )
        for versions in invalid:
            get = FakeGet()
            with self.subTest(versions=versions), self.assertRaises(inventory.InventoryError) as result:
                inventory.prepare_bootstrap(TOKEN, *versions, get=get)
            self.assertFalse(get.calls)
            self.assertNotIn(TOKEN, str(result.exception))

    def test_bootstrap_denies_readable_package_and_other_metadata_failures(self):
        for status in (200, 301, 400, 401, 403, 429, 500, 503):
            get = FakeGet(inventory.Response(status, PACKAGE))
            with self.subTest(status=status), self.assertRaises(inventory.InventoryError):
                inventory.prepare_bootstrap(TOKEN, "0.4.0", "0.3.3", "0.3.3", get=get)
            self.assertEqual(len(get.calls), 1)
        with self.assertRaises(inventory.InventoryError):
            inventory.prepare_bootstrap(TOKEN, "0.4.0", "0.3.3", "0.3.3", get=FakeGet(inventory.InventoryError("request failed")))
        get = FakeGet()
        with self.assertRaises(inventory.InventoryError):
            inventory.prepare_bootstrap("", "0.4.0", "0.3.3", "0.3.3", get=get)
        self.assertFalse(get.calls)

    def test_missing_package_checks_all_readable_package_pages(self):
        get = FakeGet(
            inventory.Response(404),
            inventory.Response(200, [{"name": "other"}], '<https://attacker.invalid/>; rel="next"'),
            inventory.Response(200, [{"name": inventory.PACKAGE}]),
        )
        with self.assertRaises(inventory.InventoryError):
            inventory.inventory(TOKEN, get=get)
        self.assertEqual(get.calls[-1], (f"{inventory.API}/orgs/{inventory.OWNER}/packages?package_type=container&per_page=100&page=2", TOKEN))

    def test_malformed_or_unbounded_package_pagination_fails(self):
        for link in ('invalid', '<https://api.github.com/>; rel="unknown"', 'secret\n::error::injected'):
            with self.subTest(link=link), self.assertRaises(inventory.InventoryError):
                inventory.inventory(TOKEN, get=FakeGet(inventory.Response(404), inventory.Response(200, [], link)))
        responses = [inventory.Response(404)] + [inventory.Response(200, [], '<https://api.github.com/>; rel="next"')] * 100
        with self.assertRaisesRegex(inventory.InventoryError, "pagination limit"):
            inventory.inventory(TOKEN, get=FakeGet(*responses))

    def test_public_package_keeps_real_tags_and_uses_anonymous_registry_access(self):
        get = FakeGet(*self.public_responses())
        self.assertEqual(inventory.inventory(TOKEN, get=get), {"name": inventory.IMAGE, "tags": ["0.3.4", "latest"]})
        self.assertEqual(get.calls[0], (inventory.PACKAGE_URL, TOKEN))
        self.assertEqual(get.calls[1], (f"{inventory.REGISTRY}/token?scope=repository:{inventory.IMAGE}:pull", None))
        self.assertEqual(get.calls[2], (f"{inventory.REGISTRY}/v2/{inventory.IMAGE}/tags/list?n=10000", REGISTRY_TOKEN))

    def test_private_package_requires_operator_visibility_change(self):
        for visibility in ("private", "internal", None):
            with self.subTest(visibility=visibility):
                package = copy.deepcopy(PACKAGE)
                package["visibility"] = visibility
                get = FakeGet(inventory.Response(200, package))
                with self.assertRaisesRegex(inventory.InventoryError, "must be made public"):
                    inventory.inventory(TOKEN, get=get)
                self.assertEqual(len(get.calls), 1)

    def test_existing_package_must_belong_to_expected_owner_and_repository(self):
        for field, value in (
            ("name", "other"), ("package_type", "npm"),
            ("owner", {"id": 1, "login": inventory.OWNER}),
            ("owner", {"id": inventory.OWNER_ID, "login": "Other"}),
            ("repository", {"id": 1, "full_name": inventory.REPOSITORY}),
            ("repository", {"id": inventory.REPOSITORY_ID, "full_name": "Other/Maple"}),
            ("repository", None), ("repository", "invalid"),
        ):
            with self.subTest(field=field, value=value):
                package = copy.deepcopy(PACKAGE)
                package[field] = value
                with self.assertRaises(inventory.InventoryError):
                    inventory.inventory(TOKEN, get=FakeGet(inventory.Response(200, package)))

    def test_final_visibility_gate_requires_an_existing_public_package(self):
        with self.assertRaises(inventory.InventoryError):
            inventory.inventory(TOKEN, public_only=True, get=FakeGet(inventory.Response(404)))
        get = FakeGet(inventory.Response(200, PACKAGE))
        self.assertEqual(inventory.inventory(TOKEN, public_only=True, get=get), {"name": inventory.IMAGE, "visibility": "public"})
        self.assertEqual(len(get.calls), 1)

    def test_anonymous_auth_and_tags_failures_never_become_an_empty_inventory(self):
        for status in (301, 401, 403, 404, 429, 500):
            for index in (1, 2):
                with self.subTest(status=status, request=index):
                    responses = list(self.public_responses())[:index] + [inventory.Response(status)]
                    with self.assertRaises(inventory.InventoryError):
                        inventory.inventory(TOKEN, get=FakeGet(*responses))

    def test_invalid_registry_tokens_and_inventory_cannot_inject_outputs(self):
        for token in (None, "", "bad\n::error::injected", "x" * 16385):
            with self.subTest(token=token), self.assertRaises(inventory.InventoryError):
                inventory.inventory(TOKEN, get=FakeGet(inventory.Response(200, PACKAGE), inventory.Response(200, {"token": token})))
        for tags in (None, {}, ["bad\n::error::injected"], ["0.3.4", "0.3.4"], [1], ["/bad"], ["a" * 129]):
            with self.subTest(tags=tags):
                responses = list(self.public_responses())
                responses[2] = inventory.Response(200, {"name": inventory.IMAGE, "tags": tags})
                with self.assertRaises(inventory.InventoryError):
                    inventory.inventory(TOKEN, get=FakeGet(*responses))

    def test_partial_registry_inventory_is_rejected(self):
        responses = list(self.public_responses())
        responses[2] = inventory.Response(200, {"name": inventory.IMAGE, "tags": ["0.3.4"]}, 'next; rel="next"')
        with self.assertRaises(inventory.InventoryError):
            inventory.inventory(TOKEN, get=FakeGet(*responses))

    def test_transport_errors_are_sanitized_and_http_status_is_preserved(self):
        for error in (URLError(TOKEN), OSError(TOKEN)):
            with self.subTest(error=type(error).__name__), patch.object(inventory, "build_opener") as opener:
                opener.return_value.open.side_effect = error
                with self.assertRaises(inventory.InventoryError) as result:
                    inventory.get_json(inventory.PACKAGE_URL, TOKEN)
                self.assertNotIn(TOKEN, str(result.exception))
        with patch.object(inventory, "build_opener") as opener:
            opener.return_value.open.side_effect = HTTPError(inventory.PACKAGE_URL, 403, TOKEN, {}, io.BytesIO(TOKEN.encode()))
            self.assertEqual(inventory.get_json(inventory.PACKAGE_URL, TOKEN), inventory.Response(403))

    def test_api_version_comparison_does_not_change_the_default(self):
        for version in (None, "2026-03-10"):
            response = io.BytesIO(b"[]")
            response.status = 200
            response.headers = {}
            with self.subTest(version=version), patch.object(inventory, "build_opener") as opener:
                opener.return_value.open.return_value = response
                options = {} if version is None else {"api_version": version}
                inventory.get_json(inventory.PACKAGE_URL, TOKEN, **options)
                request = opener.return_value.open.call_args.args[0]
                self.assertEqual(request.get_header("X-github-api-version"), version or "2022-11-28")
                self.assertEqual(request.get_method(), "GET")

    def test_invalid_or_oversized_response_is_sanitized(self):
        for payload, limit in ((TOKEN.encode(), 4096), (b" " * 10, 4)):
            response = io.BytesIO(payload)
            response.status = 200
            response.headers = {}
            with self.subTest(limit=limit), patch.object(inventory, "MAX_JSON_BYTES", limit), patch.object(inventory, "build_opener") as opener:
                opener.return_value.open.return_value = response
                with self.assertRaises(inventory.InventoryError) as result:
                    inventory.get_json(inventory.PACKAGE_URL, TOKEN)
                self.assertNotIn(TOKEN, str(result.exception))
                self.assertEqual(result.exception.status, 200)
                self.assertEqual(result.exception.category, "response-too-large" if limit == 4 else "invalid-json")

    def test_http_redirects_never_forward_the_workflow_token(self):
        handler = inventory.NoRedirect()
        self.assertIsNone(handler.redirect_request(None, None, 302, None, None, "https://attacker.invalid"))

    def test_wrong_workflow_repository_fails_before_any_request(self):
        environment = {
            "GITHUB_REPOSITORY": inventory.REPOSITORY,
            "GITHUB_REPOSITORY_ID": str(inventory.REPOSITORY_ID),
            "GITHUB_REPOSITORY_OWNER_ID": str(inventory.OWNER_ID),
            "IMAGE_NAME": inventory.IMAGE,
            "REGISTRY": "ghcr.io",
            "GH_TOKEN": TOKEN,
        }
        for key in environment.keys() - {"GH_TOKEN"}:
            invalid = dict(environment, **{key: "invalid"})
            captured = io.StringIO()
            with self.subTest(key=key), patch.dict(os.environ, invalid, clear=True), patch.object(sys, "argv", ["inventory"]), patch.object(inventory, "inventory") as operation, contextlib.redirect_stderr(captured):
                self.assertEqual(inventory.main(), 1)
                operation.assert_not_called()
                self.assertNotIn(TOKEN, captured.getvalue())

    def test_diagnostic_cli_is_separate_from_admission_and_checks_repository(self):
        environment = {
            "GITHUB_REPOSITORY": inventory.REPOSITORY,
            "GITHUB_REPOSITORY_ID": str(inventory.REPOSITORY_ID),
            "GITHUB_REPOSITORY_OWNER_ID": str(inventory.OWNER_ID),
            "IMAGE_NAME": inventory.IMAGE,
            "REGISTRY": "ghcr.io",
            "GH_TOKEN": TOKEN,
        }
        for valid in (True, False):
            settings = dict(environment)
            if not valid:
                settings["GITHUB_REPOSITORY_ID"] = "0"
            captured = io.StringIO()
            with self.subTest(valid=valid), patch.dict(os.environ, settings, clear=True), patch.object(sys, "argv", ["inventory", "--diagnose"]), patch.object(inventory, "diagnose", return_value={"diagnostic_only": True}) as diagnose, patch.object(inventory, "inventory") as admission, contextlib.redirect_stdout(captured), contextlib.redirect_stderr(captured):
                self.assertEqual(inventory.main(), 0 if valid else 1)
                admission.assert_not_called()
                if valid:
                    diagnose.assert_called_once_with(TOKEN)
                else:
                    diagnose.assert_not_called()
                self.assertNotIn(TOKEN, captured.getvalue())

    def test_bootstrap_cli_requires_canonical_manual_master_and_isolates_admission(self):
        environment = {
            "GITHUB_REPOSITORY": inventory.REPOSITORY,
            "GITHUB_REPOSITORY_ID": str(inventory.REPOSITORY_ID),
            "GITHUB_REPOSITORY_OWNER_ID": str(inventory.OWNER_ID),
            "IMAGE_NAME": inventory.IMAGE,
            "REGISTRY": "ghcr.io",
            "GH_TOKEN": TOKEN,
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_REF": "refs/heads/master",
        }
        overrides = (
            {}, {"GITHUB_EVENT_NAME": "workflow_run"}, {"GITHUB_EVENT_NAME": "pull_request"},
            {"GITHUB_REF": "refs/heads/feature"}, {"GITHUB_REF": "refs/tags/v3.4.0"},
            {"GITHUB_REPOSITORY_ID": "0"}, {"GITHUB_REPOSITORY_OWNER_ID": "0"},
            {"GITHUB_REPOSITORY": "other/Maple"}, {"IMAGE_NAME": "other/maple-proxy"},
            {"REGISTRY": "other.invalid"},
        )
        for override in overrides:
            captured = io.StringIO()
            with self.subTest(override=override), patch.dict(os.environ, dict(environment, **override), clear=True), patch.object(sys, "argv", ["inventory", "--prepare-bootstrap", "0.4.0", "0.3.3", "0.3.3"]), patch.object(inventory, "prepare_bootstrap", return_value={"bootstrap_only": True}) as bootstrap, patch.object(inventory, "inventory") as admission, patch.object(inventory, "diagnose") as diagnose, contextlib.redirect_stdout(captured), contextlib.redirect_stderr(captured):
                self.assertEqual(inventory.main(), 1 if override else 0)
                admission.assert_not_called()
                diagnose.assert_not_called()
                if override:
                    bootstrap.assert_not_called()
                else:
                    bootstrap.assert_called_once_with(TOKEN, "0.4.0", "0.3.3", "0.3.3")
                self.assertNotIn(TOKEN, captured.getvalue())

    def test_bootstrap_cli_cannot_combine_modes(self):
        for mode in ("--diagnose", "--require-public"):
            with self.subTest(mode=mode), patch.object(sys, "argv", ["inventory", "--prepare-bootstrap", "0.4.0", "0.3.3", "0.3.3", mode]), patch.object(inventory, "prepare_bootstrap") as bootstrap, contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as result:
                inventory.main()
            self.assertEqual(result.exception.code, 2)
            bootstrap.assert_not_called()


if __name__ == "__main__":
    unittest.main()
