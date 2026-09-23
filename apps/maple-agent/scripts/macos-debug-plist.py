#!/usr/bin/env python3
"""Configure a local debug bundle without copying the caller's secret environment."""

import os
from pathlib import Path
import plistlib
import re
import sys


PUBLIC_CONFIG = (
    "MAPLE_API_URL", "MAPLE_BILLING_API_URL", "MAPLE_CLIENT_ID",
    "MAPLE_PROXY_HOST", "MAPLE_PORT", "MAPLE_ENABLE_CORS",
)


def configure(plist: dict, environment: dict[str, str]) -> dict:
    bundle_id = environment.get("MAPLE_DEBUG_BUNDLE_ID") or "cloud.opensecret.maple.agent.dev"
    if not re.fullmatch(r"[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+", bundle_id):
        raise ValueError("MAPLE_DEBUG_BUNDLE_ID must be a dotted bundle identifier")
    result = {**plist, "CFBundleIdentifier": bundle_id}
    result.pop("LSEnvironment", None)
    # GUI launch does not inherit the shell used to package the app. A managed
    # identity must carry both state roots so it cannot reopen a legacy account.
    if bundle_id.startswith("cloud.opensecret.maple.agent.local."):
        roots = {key: environment.get(key, "") for key in ("XDG_CONFIG_HOME", "XDG_DATA_HOME")}
        if any(not value or not Path(value).is_absolute() for value in roots.values()):
            raise ValueError("Managed debug bundles require both absolute XDG state roots")
        result["LSEnvironment"] = {
            **{key: environment[key] for key in PUBLIC_CONFIG if key in environment},
            **roots,
            "MAPLE_DISABLE_UPDATE_CHECK": "1",
            # An empty key cannot spend an inherited saved proxy credential.
            "MAPLE_API_KEY": "",
        }
    return result


if __name__ == "__main__":
    path = Path(sys.argv[1])
    with path.open("rb") as source:
        configured = configure(plistlib.load(source), os.environ)
    with path.open("wb") as output:
        plistlib.dump(configured, output)
