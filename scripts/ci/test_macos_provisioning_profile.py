"""The Mac password profile and its restricted entitlements go only into signed builds."""

from pathlib import Path
import plistlib
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
COMMON = ROOT / "scripts/ci/_common.sh"
BASE_ENTITLEMENTS = ROOT / "apps/maple-research/frontend/src-tauri/Entitlements.plist"
RESTRICTED = {
    "com.apple.application-identifier",
    "com.apple.developer.team-identifier",
    "com.apple.developer.associated-domains",
}


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

    def test_base_entitlements_claim_no_restricted_entitlement(self) -> None:
        # Builds signed without the profile (local, ad-hoc, CI without the
        # secret) would be killed at launch if the base file claimed these.
        base = plistlib.loads(BASE_ENTITLEMENTS.read_bytes())
        self.assertFalse(RESTRICTED & base.keys())

    def test_signed_entitlements_add_the_app_id_and_password_domain(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "signed.plist"
            subprocess.run(
                ["bash", "-c", f'source {COMMON}\nwrite_macos_signed_entitlements "$1" "$2"', "_",
                 str(BASE_ENTITLEMENTS), str(out)],
                check=True,
            )
            base = plistlib.loads(BASE_ENTITLEMENTS.read_bytes())
            signed = plistlib.loads(out.read_bytes())
            self.assertEqual({key: signed[key] for key in base}, base)
            self.assertEqual(signed["com.apple.application-identifier"], "X773Y823TN.cloud.opensecret.maple")
            self.assertEqual(signed["com.apple.developer.team-identifier"], "X773Y823TN")
            self.assertEqual(signed["com.apple.developer.associated-domains"], ["webcredentials:trymaple.ai"])
            self.assertEqual(set(signed) - set(base), RESTRICTED)


if __name__ == "__main__":
    unittest.main()
