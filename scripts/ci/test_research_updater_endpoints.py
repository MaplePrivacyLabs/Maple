#!/usr/bin/env python3
"""Research's updater asks the hosted feed first, then this repository's latest release."""

import json
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
CONFIG = ROOT / "apps/maple-research/frontend/src-tauri/tauri.conf.json"


class UpdaterEndpoints(unittest.TestCase):
    def test_fallback_names_the_canonical_repository(self):
        # A former owner's name only works while GitHub redirects it.
        github = json.loads((ROOT / "repo.meta.json").read_text())["github"]
        endpoints = json.loads(CONFIG.read_text())["plugins"]["updater"]["endpoints"]
        self.assertEqual(endpoints, [
            "https://updates.trymaple.ai/latest.json",
            f"https://github.com/{github['owner']}/{github['repository']}"
            "/releases/latest/download/latest.json",
        ])


if __name__ == "__main__":
    unittest.main()
