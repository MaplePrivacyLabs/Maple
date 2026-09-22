#!/usr/bin/env python3
"""Regressions for GUI launch accidentally reopening legacy GPUI account state."""

import importlib.util
from pathlib import Path
import plistlib
import unittest


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "debug_plist", ROOT / "apps/maple-agent/scripts/macos-debug-plist.py"
)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class DebugPlistTests(unittest.TestCase):
    def test_managed_gui_preserves_state_and_only_allowlisted_configuration(self):
        env = {
            "MAPLE_DEBUG_BUNDLE_ID": "cloud.opensecret.maple.agent.local.workspace",
            "XDG_CONFIG_HOME": "/tmp/quoted ' workspace/config",
            "XDG_DATA_HOME": "/tmp/quoted ' workspace/data",
            "MAPLE_API_URL": "http://127.0.0.1:31360",
            "MAPLE_BILLING_API_URL": "http://127.0.0.1:36066",
            "MAPLE_API_KEY": "fixture-must-not-be-copied",
            "BWS_ACCESS_TOKEN": "fixture-must-not-be-copied",
        }
        result = plistlib.loads(plistlib.dumps(MODULE.configure({}, env)))
        launch = result["LSEnvironment"]
        self.assertEqual(launch["XDG_CONFIG_HOME"], env["XDG_CONFIG_HOME"])
        self.assertEqual(launch["XDG_DATA_HOME"], env["XDG_DATA_HOME"])
        self.assertEqual(launch["MAPLE_API_URL"], env["MAPLE_API_URL"])
        self.assertEqual(launch["MAPLE_BILLING_API_URL"], env["MAPLE_BILLING_API_URL"])
        self.assertEqual(launch["MAPLE_API_KEY"], "")
        self.assertEqual(launch["MAPLE_DISABLE_UPDATE_CHECK"], "1")
        self.assertNotIn("BWS_ACCESS_TOKEN", launch)

    def test_managed_identity_fails_closed_without_both_absolute_state_roots(self):
        for config, data in (("", "/tmp/data"), ("/tmp/config", ""), ("relative", "/tmp/data")):
            with self.subTest(config=config, data=data), self.assertRaises(ValueError):
                MODULE.configure({}, {
                    "MAPLE_DEBUG_BUNDLE_ID": "cloud.opensecret.maple.agent.local.workspace",
                    "XDG_CONFIG_HOME": config, "XDG_DATA_HOME": data,
                })

    def test_unmanaged_bundle_does_not_copy_shell_or_stale_environment(self):
        result = MODULE.configure({"LSEnvironment": {"OLD_KEY": "fixture"}}, {"XDG_CONFIG_HOME": "/tmp/config"})
        self.assertEqual(result["CFBundleIdentifier"], "cloud.opensecret.maple.agent.dev")
        self.assertNotIn("LSEnvironment", result)


if __name__ == "__main__":
    unittest.main()
