"""Credential-free tests of query validation, SQL, bounds and error redaction."""

import contextlib
from datetime import datetime, timedelta
import io
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError, URLError

import query


NOW = datetime(2026, 10, 27, 12, 34, 56, tzinfo=query.UTC)
ENV = {
    "CLOUDFLARE_ACCOUNT_ID": "a" * 32,
    "DOWNLOAD_ANALYTICS_DATABASE_ID": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
    "CLOUDFLARE_API_TOKEN": "fixture-private-token-never-print",
}


def reply(rows):
    return json.dumps({"success": True, "result": [{"success": True, "results": rows}]}).encode()


def row():
    return {"bucket": "2026-10-27T00:00:00.000Z", "product": "agent", "version": "0.0.9",
            "platform": "macos", "artifact": "macos-dmg", "kind": "artifact", "status": 200,
            "method": "GET", "range": "none", "is_probe": 0, "events": 2, "expected_response_bytes": 42}


class QueryTests(unittest.TestCase):
    def test_utc_time_validation_and_window_bounds(self):
        for value in ("2026-10-27", "2026-10-27T12:00:00+00:00", "2026-10-27T12:00:00",
                      "2026-02-30T00:00:00Z", "2026-10-27T12:00:00.1Z", "2026-10-27T12:00:60Z"):
            with self.subTest(value=value), self.assertRaises(query.QueryError):
                query.timestamp(value)
        self.assertEqual(query.iso(query.timestamp("2026-10-27T12:00:00Z")), "2026-10-27T12:00:00.000Z")
        self.assertEqual(query.iso(query.timestamp("2026-10-27T12:00:00.123Z")), "2026-10-27T12:00:00.123Z")
        for argv in (
            ["--since", "2026-10-27T12:34:56Z"],
            ["--since", "2026-10-28T00:00:00Z"],
            ["--since", "2026-09-25T00:00:00Z"],
        ):
            with self.subTest(argv=argv), self.assertRaises(query.QueryError):
                query.query_spec(query.parse_args(argv), NOW)
        _, params, summary = query.query_spec(query.parse_args([]), NOW)
        self.assertEqual(params[:2], ["2026-10-20T12:34:56.000Z", "2026-10-27T12:34:56.000Z"])
        self.assertFalse(summary["include_probes"])

    def test_queries_are_fixed_selects_with_only_bound_user_filters(self):
        args = query.parse_args(["--product", "agent", "--version", "0.0.9", "--platform", "linux",
                                 "--artifact", "linux-bundle", "--kind", "artifact", "--method", "GET",
                                 "--range", "partial", "--status", "206", "--host", "updates.maple.ai"])
        sql, params, _ = query.query_spec(args, NOW)
        self.assertTrue(sql.startswith("SELECT "))
        self.assertNotIn(";", sql)
        self.assertIn("observed_at >= ? AND observed_at < ?", sql)
        self.assertIn("is_probe = 0", sql)
        self.assertIn("GROUP BY bucket, product, version, platform, artifact, kind, status, method, range, is_probe", sql)
        self.assertTrue(sql.endswith("LIMIT 10001"))
        self.assertEqual(sql.count("?"), len(params))
        self.assertIn("0.0.9", params)
        self.assertNotIn("0.0.9", sql)
        self.assertNotIn("updates.maple.ai", sql)
        for bad in (["--product", "private;DELETE"], ["--version", "1.2.3';DELETE"],
                    ["--bucket", "day;DROP"], ["--status", "199"], ["--status", "600"],
                    ["--version", "01.2.3"], ["--unexpected", "private-token"]):
            with self.subTest(bad=bad), self.assertRaises(query.QueryError):
                query.parse_args(bad)
        health_sql, _, _ = query.query_spec(query.parse_args(["--health", "--include-probes"]), NOW)
        self.assertTrue(health_sql.startswith("SELECT "))
        self.assertNotIn("is_probe = 0", health_sql)

    def test_exact_readonly_sql_executes_on_migration_and_separates_kinds_probes_ranges(self):
        db = sqlite3.connect(":memory:")
        db.row_factory = sqlite3.Row
        db.executescript((Path(__file__).parents[1] / "agent-updates/migrations/0001_download_events.sql").read_text())
        base = ["2026-10-27T12:00:00.000Z", "agent", "updates.maple.ai", "0.0.9", "macos", "macos-dmg", "artifact", "GET", 200, "none", "success", 42, "cli", 0]
        for index, changes in enumerate(({}, {7: "HEAD"}, {8: 206, 9: "partial"},
                                         {6: "alias", 8: 302, 10: "redirect"}, {13: 1})):
            values = base.copy()
            for position, value in changes.items():
                values[position] = value
            db.execute("INSERT INTO download_events VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)", [str(index), *values])
        sql, params, _ = query.query_spec(query.parse_args([]), NOW)
        rows = [dict(item) for item in db.execute(sql, params)]
        self.assertEqual(sum(item["events"] for item in rows), 4)
        self.assertEqual(len(rows), 4)
        self.assertEqual({item["kind"] for item in rows}, {"alias", "artifact"})
        hour_sql, hour_params, _ = query.query_spec(query.parse_args(["--bucket", "hour", "--include-probes"]), NOW)
        hour_rows = list(db.execute(hour_sql, hour_params))
        self.assertEqual({item["bucket"] for item in hour_rows}, {"2026-10-27T12:00:00.000Z"})
        self.assertEqual(sum(item["events"] for item in hour_rows), 5)
        health_sql, health_params, _ = query.query_spec(query.parse_args(["--health"]), NOW)
        health = dict(db.execute(health_sql, health_params).fetchone())
        self.assertEqual(health, {"latest_observed_at": base[0], "persisted_events": 4, "utc_day_events": 4})
        db.close()

    def test_environment_only_credentials_and_fixed_private_endpoint(self):
        calls = []
        def send(request):
            calls.append(request)
            return reply([row()])
        result = query.report(query.parse_args([]), ENV, NOW, send)
        request = calls[0]
        self.assertEqual(request.get_method(), "POST")
        self.assertEqual(request.full_url, f"https://api.cloudflare.com/client/v4/accounts/{'a' * 32}/d1/database/{ENV['DOWNLOAD_ANALYTICS_DATABASE_ID']}/query")
        self.assertEqual(request.get_header("Authorization"), "Bearer " + ENV["CLOUDFLARE_API_TOKEN"])
        body = json.loads(request.data)
        self.assertTrue(body["sql"].startswith("SELECT "))
        self.assertNotIn(ENV["CLOUDFLARE_API_TOKEN"], json.dumps(result))
        self.assertFalse(result["truncated"])
        self.assertEqual(result["rows"], [row()])
        for key in ENV:
            bad = {**ENV, key: "private-invalid-value\n"}
            with self.subTest(key=key), self.assertRaises(query.QueryError):
                query.credentials(bad)

    def test_database_defaults_to_exact_config_binding_with_jsonc(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.jsonc"
            path.write_text('''{ // private config comments not emitted
              "$schema": "https://example.test/schema,}",
              "d1_databases": [{"binding":"OTHER","database_id":"ignored"},
              {"binding":"DOWNLOAD_DB","database_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",},],
            }''')
            self.assertEqual(query.database_from_config(path), ENV["DOWNLOAD_ANALYTICS_DATABASE_ID"])
            with patch.object(query, "database_from_config", return_value=ENV["DOWNLOAD_ANALYTICS_DATABASE_ID"]):
                credentials = query.credentials({key: value for key, value in ENV.items() if key != "DOWNLOAD_ANALYTICS_DATABASE_ID"})
            self.assertEqual(credentials[1], ENV["DOWNLOAD_ANALYTICS_DATABASE_ID"])
            path.write_text('{"d1_databases": []}')
            with self.assertRaises(query.QueryError):
                query.database_from_config(path)

    def test_output_cap_is_explicit_not_silent(self):
        report = query.report(query.parse_args([]), ENV, NOW, lambda _: reply([row()] * 10001))
        self.assertTrue(report["truncated"])
        self.assertEqual(len(report["rows"]), 10000)

    def test_upstream_errors_malformed_responses_and_large_bodies_are_sanitized(self):
        def http_error(_):
            raise HTTPError("https://private.example/secret", 403, "private token", {}, io.BytesIO(b"secret private body"))
        def network_error(_):
            raise URLError("private token URL")
        cases = [(http_error, 20), (network_error, 24), (lambda _: b"private token", 22),
                 (lambda _: b"x" * (query.MAX_REPLY_BYTES + 1), 23),
                 (lambda _: json.dumps({"success": False, "errors": ["private token"]}).encode(), 21),
                 (lambda _: reply([{**row(), "private": "token"}]), 22)]
        for send, expected in cases:
            with self.subTest(expected=expected), self.assertRaises(query.QueryError) as caught:
                query.report(query.parse_args([]), ENV, NOW, send)
            self.assertEqual(caught.exception.code, expected)
            self.assertNotIn("private", str(caught.exception))

    def test_main_error_output_contains_numeric_codes_only(self):
        out, err = io.StringIO(), io.StringIO()
        with patch.object(query, "report", side_effect=query.QueryError(20, 403)):
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = query.main([], ENV)
        self.assertEqual(code, 1)
        self.assertEqual(out.getvalue(), "")
        self.assertEqual(json.loads(err.getvalue()), {"error_code": 20, "http_status": 403})
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = query.main(["--product", ENV["CLOUDFLARE_API_TOKEN"]], ENV)
        self.assertEqual(code, 1)
        self.assertEqual(json.loads(err.getvalue()), {"error_code": 2})
        self.assertNotIn(ENV["CLOUDFLARE_API_TOKEN"], err.getvalue())

    def test_private_api_transport_rejects_redirects(self):
        handler = query.NoRedirect()
        self.assertIsNone(handler.redirect_request(None, None, 302, "private", {}, "https://private.example"))


if __name__ == "__main__":
    unittest.main()
