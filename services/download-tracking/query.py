#!/usr/bin/env python3
"""Private, read-only D1 reports. Credentials come only from the environment."""

from __future__ import annotations

import argparse
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import re
import sys
from typing import Callable, Mapping
from urllib.error import HTTPError, URLError
from urllib.request import HTTPRedirectHandler, Request, build_opener

MAX_ROWS = 10_000
MAX_REPLY_BYTES = 8 * 1024 * 1024
CONFIG = Path(__file__).resolve().parents[1] / "agent-updates" / "wrangler.jsonc"
UTC = timezone.utc
ARTIFACTS = (
    "macos-dmg", "macos-updater", "windows-installer", "linux-bundle",
    "linux-appimage", "linux-deb", "linux-rpm", "android-apk", "latest", "manifest",
)
FILTERS = {
    "product": ("agent", "research"),
    "platform": ("macos", "windows", "linux", "android", "all"),
    "artifact": ARTIFACTS,
    "kind": ("artifact", "alias", "metadata"),
    "method": ("GET", "HEAD"),
    "range": ("none", "requested", "partial", "unsatisfiable"),
    "host": ("updates.maple.ai", "updates.trymaple.ai"),
}
DIMENSIONS = ("product", "version", "platform", "artifact", "kind", "status", "method", "range", "is_probe")


class QueryError(Exception):
    def __init__(self, code: int, http_status: int | None = None):
        self.code = code
        self.http_status = http_status
        super().__init__(code)


class Parser(argparse.ArgumentParser):
    def error(self, _message: str) -> None:
        # Never echo invalid arguments, credentials, URLs, or upstream bodies.
        raise QueryError(2)


def timestamp(value: str) -> datetime:
    if not re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{3})?Z", value):
        raise QueryError(11)
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        raise QueryError(11) from None


def iso(value: datetime) -> str:
    return value.astimezone(UTC).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = Parser(description=__doc__)
    parser.add_argument("--since", help="inclusive UTC timestamp, e.g. 2026-10-27T00:00:00Z")
    parser.add_argument("--until", help="exclusive UTC timestamp; defaults to now")
    parser.add_argument("--bucket", choices=("hour", "day"), default="day")
    parser.add_argument("--include-probes", action="store_true", help="include self-reported probe requests")
    parser.add_argument("--health", action="store_true", help="latest persisted observation and UTC-day count within the selected window")
    for column, choices in FILTERS.items():
        parser.add_argument("--" + column, choices=choices)
    parser.add_argument("--version", help="exact stable SemVer")
    parser.add_argument("--status", type=int, help="exact HTTP status, 200 through 599")
    args = parser.parse_args(argv)
    if args.version is not None and (
        len(args.version) > 64 or not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", args.version)
    ):
        raise QueryError(12)
    if args.status is not None and not 200 <= args.status <= 599:
        raise QueryError(12)
    return args


def query_spec(args: argparse.Namespace, now: datetime) -> tuple[str, list[object], dict[str, object]]:
    now = now.astimezone(UTC).replace(microsecond=(now.microsecond // 1000) * 1000)
    until = timestamp(args.until) if args.until else now.astimezone(UTC)
    try:
        since = timestamp(args.since) if args.since else until - timedelta(days=7)
    except OverflowError:
        raise QueryError(11) from None
    if not timedelta(0) < until - since <= timedelta(days=31):
        raise QueryError(11)
    start, end = iso(since), iso(until)
    clauses = ["observed_at >= ?", "observed_at < ?"]
    params: list[object] = [start, end]
    for column in (*FILTERS, "version", "status"):
        value = getattr(args, column)
        if value is not None:
            clauses.append(f"{column} = ?")
            params.append(value)
    if not args.include_probes:
        clauses.append("is_probe = 0")
    where = " AND ".join(clauses)
    summary: dict[str, object] = {
        "since": start, "until": end, "bucket": args.bucket,
        "include_probes": args.include_probes,
    }
    if args.health:
        day_start = now.astimezone(UTC).replace(hour=0, minute=0, second=0, microsecond=0)
        sql = (
            "SELECT MAX(observed_at) AS latest_observed_at, COUNT(*) AS persisted_events, "
            "COALESCE(SUM(CASE WHEN observed_at >= ? THEN 1 ELSE 0 END), 0) AS utc_day_events "
            f"FROM download_events WHERE {where}"
        )
        params.insert(0, iso(day_start))
        summary["utc_day_since"] = iso(day_start)
    else:
        # Only this fixed choice enters SQL; every user filter is a bound value.
        bucket = "substr(observed_at, 1, 13) || ':00:00.000Z'" if args.bucket == "hour" else "substr(observed_at, 1, 10) || 'T00:00:00.000Z'"
        dimensions = ", ".join(DIMENSIONS)
        sql = (
            f"SELECT {bucket} AS bucket, {dimensions}, COUNT(*) AS events, "
            "COALESCE(SUM(response_length), 0) AS expected_response_bytes "
            f"FROM download_events WHERE {where} GROUP BY bucket, {dimensions} "
            f"ORDER BY bucket, {dimensions} LIMIT {MAX_ROWS + 1}"
        )
    return sql, params, summary


def database_from_config(path: Path = CONFIG) -> str:
    try:
        # Wrangler JSONC: preserve strings while removing comments and trailing commas.
        text = path.read_text(encoding="utf-8")
        tokens = r'"(?:\\.|[^"\\])*"|//[^\n]*|/\*[\s\S]*?\*/'
        text = re.sub(tokens, lambda m: m[0] if m[0].startswith('"') else "", text)
        text = re.sub(r'"(?:\\.|[^"\\])*"|,\s*(?=[}\]])', lambda m: m[0] if m[0].startswith('"') else "", text)
        config = json.loads(text)
        matches = [item["database_id"] for item in config["d1_databases"] if item.get("binding") == "DOWNLOAD_DB"]
        if len(matches) != 1:
            raise QueryError(10)
        return matches[0]
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        raise QueryError(10) from None


def credentials(environ: Mapping[str, str]) -> tuple[str, str, str]:
    account = environ.get("CLOUDFLARE_ACCOUNT_ID", "")
    database = environ.get("DOWNLOAD_ANALYTICS_DATABASE_ID") or database_from_config()
    token = environ.get("CLOUDFLARE_API_TOKEN", "")
    if (
        not re.fullmatch(r"[0-9a-f]{32}", account)
        or not isinstance(database, str)
        or not re.fullmatch(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", database)
        or not 1 <= len(token) <= 4096
        or not all(33 <= ord(char) <= 126 for char in token)
    ):
        raise QueryError(10)
    return account, database, token


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def transport(request: Request) -> bytes:
    with build_opener(NoRedirect).open(request, timeout=30) as response:
        data = response.read(MAX_REPLY_BYTES + 1)
        if len(data) > MAX_REPLY_BYTES:
            raise QueryError(23)
        return data


def execute(
    sql: str,
    params: list[object],
    environ: Mapping[str, str],
    send: Callable[[Request], bytes] = transport,
) -> list[dict[str, object]]:
    account, database, token = credentials(environ)
    request = Request(
        f"https://api.cloudflare.com/client/v4/accounts/{account}/d1/database/{database}/query",
        data=json.dumps({"sql": sql, "params": params}, separators=(",", ":")).encode(),
        headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"},
        method="POST",
    )
    try:
        data = send(request)
        if len(data) > MAX_REPLY_BYTES:
            raise QueryError(23)
        payload = json.loads(data)
        if not isinstance(payload, dict) or payload.get("success") is not True:
            raise QueryError(21)
        result = payload.get("result")
        if not isinstance(result, list) or len(result) != 1 or not isinstance(result[0], dict):
            raise QueryError(22)
        if result[0].get("success") is not True:
            raise QueryError(21)
        rows = result[0].get("results")
        if not isinstance(rows, list) or len(rows) > MAX_ROWS + 1 or not all(isinstance(row, dict) for row in rows):
            raise QueryError(22)
        return rows
    except HTTPError as error:
        # The upstream body/message/URL can be sensitive. Do not read or print it.
        error.close()
        raise QueryError(20, error.code) from None
    except (URLError, TimeoutError, OSError):
        raise QueryError(24) from None
    except (ValueError, TypeError, UnicodeError):
        raise QueryError(22) from None


def report(
    args: argparse.Namespace,
    environ: Mapping[str, str],
    now: datetime,
    send: Callable[[Request], bytes] = transport,
) -> dict[str, object]:
    sql, params, summary = query_spec(args, now)
    rows = execute(sql, params, environ, send)
    expected_keys = {"latest_observed_at", "persisted_events", "utc_day_events"} if args.health else {"bucket", *DIMENSIONS, "events", "expected_response_bytes"}
    for row in rows:
        if set(row) != expected_keys or any(
            not (value is None or (type(value) is int and value >= 0) or (type(value) is str and len(value) <= 64))
            for value in row.values()
        ):
            raise QueryError(22)
    summary["truncated"] = len(rows) > MAX_ROWS
    summary["rows"] = rows[:MAX_ROWS]
    return summary


def main(argv: list[str] | None = None, environ: Mapping[str, str] | None = None) -> int:
    try:
        result = report(parse_args(argv), os.environ if environ is None else environ, datetime.now(UTC))
        print(json.dumps(result, separators=(",", ":")))
        return 0
    except QueryError as error:
        failure = {"error_code": error.code}
        if error.http_status is not None:
            failure["http_status"] = error.http_status
        print(json.dumps(failure, separators=(",", ":")), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
