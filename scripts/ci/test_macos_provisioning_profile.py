"""The Mac password profile is embedded only into the Maple app bundle."""

import os
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
COMMON = ROOT / "scripts/ci/_common.sh"
WRAPPER = ROOT / "scripts/ci/macos-codesign-wrapper/codesign"
CANARY = "CANARY-PROFILE-SECRET"


def write_app(root: Path, bundle_id: str) -> Path:
    app = root / f"{bundle_id}.app"
    contents = app / "Contents"
    contents.mkdir(parents=True)
    (contents / "Info.plist").write_text(
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        "<plist><dict><key>CFBundleIdentifier</key>"
        f"<string>{bundle_id}</string></dict></plist>\n",
        encoding="utf-8",
    )
    return app


class MacosProvisioningProfileTests(unittest.TestCase):
    def test_plist_check_requires_the_maple_password_association(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            good = Path(tmp) / "good.plist"
            bad = Path(tmp) / "bad.plist"
            good.write_text(
                "com.apple.developer.associated-domains webcredentials:trymaple.ai "
                "X773Y823TN.cloud.opensecret.maple",
                encoding="utf-8",
            )
            bad.write_text("webcredentials:example.com OTHER.bundle", encoding="utf-8")
            script = (
                f"source {COMMON}\n"
                f"macos_profile_plist_allows_maple_passwords {good}\n"
                f"if macos_profile_plist_allows_maple_passwords {bad}; then exit 1; fi\n"
            )
            subprocess.run(["bash", "-c", script], check=True)

    def test_wrapper_embeds_the_profile_only_in_the_maple_bundle(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            maple = write_app(root, "cloud.opensecret.maple")
            other = write_app(root, "cloud.opensecret.maple.agent")
            profile = root / "profile.bin"
            profile.write_text(CANARY, encoding="utf-8")
            fake = root / "codesign"
            fake.write_text("#!/bin/sh\nprintf '%s\\n' \"$@\"\n", encoding="utf-8")
            fake.chmod(fake.stat().st_mode | stat.S_IEXEC)
            env = os.environ.copy()
            env["MAPLE_MACOS_PROVISIONING_PROFILE"] = str(profile)
            env["MAPLE_REAL_CODESIGN"] = str(fake)
            result = subprocess.run(
                [str(WRAPPER), "--force", "--sign", "identity", str(maple), str(other)],
                check=True,
                capture_output=True,
                text=True,
                env=env,
            )
            embedded = (maple / "Contents/embedded.provisionprofile").read_text(encoding="utf-8")
            self.assertEqual(embedded, CANARY)
            self.assertFalse((other / "Contents/embedded.provisionprofile").exists())
            self.assertNotIn(CANARY, result.stdout)
            self.assertNotIn(CANARY, result.stderr)
            self.assertIn(str(maple), result.stdout)


if __name__ == "__main__":
    unittest.main()
