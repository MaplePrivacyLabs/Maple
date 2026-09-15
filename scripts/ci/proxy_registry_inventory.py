#!/usr/bin/env python3
"""Read the canonical proxy package inventory without masking registry failures.

Only GitHub's authenticated package metadata can admit an initial package. A
registry 401, 403, 404, timeout or malformed response is never an empty registry.
The workflow supplies a repository-scoped GITHUB_TOKEN with packages: read.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
import os
import re
import sys
from urllib.error import HTTPError, URLError
from urllib.request import HTTPRedirectHandler, Request, build_opener

OWNER = "MaplePrivacyLabs"
OWNER_ID = 322649754
REPOSITORY = "MaplePrivacyLabs/Maple"
REPOSITORY_ID = 923138240
PACKAGE = "maple-proxy"
IMAGE = "mapleprivacylabs/maple-proxy"
API = "https://api.github.com"
API_VERSION = "2022-11-28"
DIAGNOSTIC_API_VERSIONS = (API_VERSION, "2026-03-10")
REGISTRY = "https://ghcr.io"
PACKAGE_URL = f"{API}/orgs/{OWNER}/packages/container/{PACKAGE}"
MAX_JSON_BYTES = 4 * 1024 * 1024


class InventoryError(ValueError):
    """Fixed-category error, with no remote body or credential interpolation."""

    def __init__(self, message: str, *, status: int | None = None, category: str = "request-failed"):
        super().__init__(message)
        self.status = status
        self.category = category


@dataclass(frozen=True)
class Response:
    status: int
    data: object = None
    link: str | None = None


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def get_json(url: str, token: str | None = None, *, api_version: str = API_VERSION) -> Response:
    headers = {"Accept": "application/vnd.github+json", "User-Agent": "Maple-proxy-package-inventory"}
    if url.startswith(API + "/"):
        headers["X-GitHub-Api-Version"] = api_version
    if token:
        headers["Authorization"] = "Bearer " + token
    try:
        with build_opener(NoRedirect).open(Request(url, headers=headers), timeout=30) as response:
            body = response.read(MAX_JSON_BYTES + 1)
            if len(body) > MAX_JSON_BYTES:
                raise InventoryError(
                    "Package inventory response exceeds size limit",
                    status=response.status, category="response-too-large",
                )
            try:
                data = json.loads(body)
            except ValueError:
                raise InventoryError(
                    "Package inventory response is not valid JSON",
                    status=response.status, category="invalid-json",
                ) from None
            return Response(response.status, data, response.headers.get("Link"))
    except HTTPError as error:
        status = error.code
        error.close()
        return Response(status)
    except InventoryError:
        raise
    except (OSError, URLError, ValueError):
        raise InventoryError("Package inventory request failed") from None


def json_type(data: object) -> str:
    """Describe only the shape, never a response value or a Python type name."""
    if data is None:
        return "null"
    for expected, name in ((dict, "object"), (list, "array"), (str, "string"), (bool, "boolean"), (int, "number"), (float, "number")):
        if isinstance(data, expected):
            return name
    return "invalid"


def diagnose(token: str, *, get=get_json) -> dict:
    """Probe fixed metadata endpoints without admitting or publishing a package.

    Every result is diagnostic evidence only. HTTP errors deliberately retain no
    response body or headers; malformed success bodies expose only their category.
    The second API version is a comparison, not a change to publication defaults.
    """
    if not token:
        raise InventoryError("Package diagnostics require the workflow token")
    endpoints = (
        ("package-metadata", PACKAGE_URL),
        ("organization-packages", f"{API}/orgs/{OWNER}/packages?package_type=container&per_page=100&page=1"),
    )
    results = []
    for version in DIAGNOSTIC_API_VERSIONS:
        for name, url in endpoints:
            result = {"endpoint": name, "api_version": version}
            try:
                response = get(url, token, api_version=version)
                result.update({
                    "http_status": response.status,
                    "json_type": json_type(response.data) if response.status == 200 else "not-read",
                })
            except InventoryError as error:
                # Neither exception text nor arbitrary response content belongs in logs.
                category = error.category if error.category in {"invalid-json", "response-too-large"} else "request-failed"
                result.update({"http_status": error.status, "error_category": category})
            results.append(result)
    return {"diagnostic_only": True, "probes": results}


def prepare_bootstrap(token: str, version: str, previous_version: str, baseline: str, *, get=get_json) -> dict:
    """Authorize only additive digest pushes, never infer an empty inventory.

    A metadata 404 can also hide a package this token cannot read. The explicit
    bootstrap operation creates no version or alias tags; registry permissions
    remain the final authority for the digest pushes. Ordinary publication must
    subsequently pass the complete canonical public-package inventory checks.
    """
    if not token:
        raise InventoryError("Proxy digest bootstrap requires the workflow token")
    parsed = []
    for value in (version, previous_version, baseline):
        if not isinstance(value, str) or len(value) > 128 or not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value):
            raise InventoryError("Proxy digest bootstrap requires three canonical X.Y.Z versions")
        parsed.append(tuple(int(component) for component in value.split(".")))
    if parsed[0] < parsed[1]:
        raise InventoryError("Proxy digest bootstrap cannot roll back the previous release version")
    if version == baseline:
        raise InventoryError("Proxy digest bootstrap cannot publish the unbackfilled baseline")
    response = get(PACKAGE_URL, token)
    if response.status != 404:
        raise InventoryError(f"Proxy digest bootstrap requires unavailable package metadata (HTTP {response.status})")
    return {"name": IMAGE, "proxy_version": version, "bootstrap_only": True, "metadata_status": 404}


def require_public(metadata: object) -> None:
    if not isinstance(metadata, dict):
        raise InventoryError("Invalid package metadata")
    owner = metadata.get("owner") or {}
    repository = metadata.get("repository") or {}
    if (
        metadata.get("name") != PACKAGE
        or metadata.get("package_type") != "container"
        or not isinstance(owner, dict)
        or owner.get("id") != OWNER_ID
        or owner.get("login") != OWNER
        or not isinstance(repository, dict)
        or repository.get("id") != REPOSITORY_ID
        or repository.get("full_name") != REPOSITORY
    ):
        raise InventoryError("Package is not linked to the canonical Maple repository")
    if metadata.get("visibility") != "public":
        raise InventoryError(
            "The new proxy package must be made public in GitHub package settings; "
            "then rerun Publish proxy container to verify and reconcile its aliases"
        )


def confirm_missing(token: str, get=get_json) -> None:
    # GitHub can mask unavailable resources as 404. Also require successful
    # authenticated package enumeration; a missing packages permission must not
    # authorize treating an inventory failure as an empty package. This lists
    # packages readable by this repository token, not unrelated private packages.
    # GitHub denies pushes to an existing package not connected to this repository.
    for page in range(1, 101):
        response = get(f"{API}/orgs/{OWNER}/packages?package_type=container&per_page=100&page={page}", token)
        if response.status != 200:
            raise InventoryError(f"Cannot confirm initial package: authenticated package listing returned HTTP {response.status}")
        if not isinstance(response.data, list):
            raise InventoryError(f"Cannot confirm initial package: authenticated package listing returned JSON {json_type(response.data)}, expected array")
        if len(response.data) > 100:
            raise InventoryError("Invalid package listing length")
        for package in response.data:
            if not isinstance(package, dict) or not isinstance(package.get("name"), str):
                raise InventoryError("Invalid package listing entry")
            if package["name"].casefold() == PACKAGE:
                raise InventoryError("Package metadata and package listing disagree")
        if not response.link:
            return
        # Construct the next fixed-origin URL ourselves; never follow a Link URL
        # carrying an API credential. Fail on unexpected pagination metadata.
        link_pattern = r'<[^>\r\n]+>;\s*rel="(?:next|prev|first|last)"'
        if not re.fullmatch(link_pattern + r'(?:,\s*' + link_pattern + r')*', response.link):
            raise InventoryError("Invalid package listing pagination")
        if 'rel="next"' not in response.link:
            return
    raise InventoryError("Package listing exceeds pagination limit")


def inventory(token: str, *, public_only: bool = False, get=get_json) -> dict:
    if not token:
        raise InventoryError("Package inventory requires the workflow token")
    response = get(PACKAGE_URL, token)
    if response.status == 404 and not public_only:
        confirm_missing(token, get)
        return {"name": IMAGE, "tags": []}
    if response.status != 200:
        raise InventoryError(f"Authenticated proxy package metadata is unavailable (HTTP {response.status})")
    require_public(response.data)
    if public_only:
        return {"name": IMAGE, "visibility": "public"}

    # Public access is part of the contract. Do not use the workflow credential
    # to hide a public-pull regression; this registry token is anonymous/pull-only.
    response = get(f"{REGISTRY}/token?scope=repository:{IMAGE}:pull")
    if response.status != 200 or not isinstance(response.data, dict):
        raise InventoryError("Cannot obtain anonymous proxy registry token")
    registry_token = response.data.get("token")
    if not isinstance(registry_token, str) or not re.fullmatch(r"[A-Za-z0-9._~+/=-]{1,16384}", registry_token):
        raise InventoryError("Invalid anonymous proxy registry token")
    response = get(f"{REGISTRY}/v2/{IMAGE}/tags/list?n=10000", registry_token)
    if response.status != 200 or not isinstance(response.data, dict) or response.link:
        raise InventoryError("Public proxy tag inventory is unavailable or incomplete")
    tags = response.data.get("tags")
    if (
        response.data.get("name") != IMAGE
        or not isinstance(tags, list)
        or len(tags) > 10000
        or any(not isinstance(tag, str) or not re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}", tag) for tag in tags)
        or len(set(tags)) != len(tags)
    ):
        raise InventoryError("Invalid public proxy tag inventory")
    return {"name": IMAGE, "tags": tags}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--require-public", action="store_true")
    mode.add_argument("--diagnose", action="store_true", help="report fixed read-only metadata probes; does not validate publication")
    mode.add_argument("--prepare-bootstrap", nargs=3, metavar=("VERSION", "PREVIOUS_VERSION", "BASELINE"), help="prepare additive digest-only bootstrap; never admits version or alias tags")
    args = parser.parse_args()
    try:
        if (
            os.environ.get("GITHUB_REPOSITORY") != REPOSITORY
            or os.environ.get("GITHUB_REPOSITORY_ID") != str(REPOSITORY_ID)
            or os.environ.get("GITHUB_REPOSITORY_OWNER_ID") != str(OWNER_ID)
            or os.environ.get("IMAGE_NAME") != IMAGE
            or os.environ.get("REGISTRY") != "ghcr.io"
        ):
            raise InventoryError("Proxy package inventory must run for the canonical repository and image")
        token = os.environ.get("GH_TOKEN", "")
        if args.prepare_bootstrap:
            if os.environ.get("GITHUB_EVENT_NAME") != "workflow_dispatch" or os.environ.get("GITHUB_REF") != "refs/heads/master":
                raise InventoryError("Proxy digest bootstrap requires manual dispatch from master")
            result = prepare_bootstrap(token, *args.prepare_bootstrap)
        else:
            result = diagnose(token) if args.diagnose else inventory(token, public_only=args.require_public)
        print(json.dumps(result, sort_keys=True))
    except InventoryError as error:
        print(f"Proxy package inventory failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
