#!/usr/bin/env python3
"""Focused checks for profile mixups, archive identity and state restoration."""

import importlib.util
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import tomllib
import unittest
import zipfile


SCRIPT_DIR = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("ios_build_profile", SCRIPT_DIR / "ios-build-profile.py")
PROFILE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROFILE)
CANONICAL_SPEC = importlib.util.spec_from_file_location("canonical_ios_app", SCRIPT_DIR / "canonical-ios-app-hash.py")
CANONICAL = importlib.util.module_from_spec(CANONICAL_SPEC)
CANONICAL_SPEC.loader.exec_module(CANONICAL)
SOURCE_SHA = "a" * 40
FRONTEND_SHA = "b" * 64
BUILD_NUMBER = "42.1"
AUTH_ORIGIN = "https://dev-auth.example.test"


def environment_for(variant):
    environment = dict(PROFILE.PROFILES[variant]["environment"])
    if variant == "dev":
        environment["VITE_MAPLE_DEV_AUTH_ORIGIN"] = AUTH_ORIGIN
    return environment


def make_profile(variant="dev"):
    return PROFILE.make_profile(variant, SOURCE_SHA, BUILD_NUMBER, FRONTEND_SHA,
                                environment_for(variant))


def make_info(variant="dev"):
    profile = PROFILE.PROFILES[variant]
    return {
        "CFBundleIdentifier": profile["bundle_identifier"],
        "CFBundleDisplayName": profile["display_name"],
        "CFBundleVersion": BUILD_NUMBER,
        "CFBundleShortVersionString": "3.4.2",
        "CFBundleURLTypes": [{"CFBundleURLSchemes": [profile["bundle_identifier"]]}],
    }


def reconstructed_dev_plist():
    # Public metadata reconstructed from the local Dev simulator bundle. With
    # device platform and existing normalization, its hash exactly matches the
    # unsigned plist logged by CI run 35833889297. The one-key export mutation
    # below reproduces that run's exported plist hash; no failed IPA was retained.
    return {
        "CFBundleDevelopmentRegion": "en",
        "CFBundleDisplayName": "Maple Dev",
        "CFBundleExecutable": "Maple Dev",
        "CFBundleIcons": {"CFBundlePrimaryIcon": {
            "CFBundleIconFiles": ["AppIcon60x60"], "CFBundleIconName": "AppIcon"}},
        "CFBundleIcons~ipad": {"CFBundlePrimaryIcon": {
            "CFBundleIconFiles": ["AppIcon60x60", "AppIcon76x76"], "CFBundleIconName": "AppIcon"}},
        "CFBundleIdentifier": "cloud.opensecret.maple.dev",
        "CFBundleInfoDictionaryVersion": "6.0",
        "CFBundleName": "Maple Dev",
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": "3.4.2",
        "CFBundleSupportedPlatforms": ["iPhoneOS"],
        "CFBundleURLTypes": [{"CFBundleURLName": "cloud.opensecret.maple.dev",
                              "CFBundleURLSchemes": ["cloud.opensecret.maple.dev"]}],
        "ITSAppUsesNonExemptEncryption": False,
        "LSApplicationQueriesSchemes": ["https", "http"],
        "LSRequiresIPhoneOS": True,
        "MinimumOSVersion": "16.0",
        "NSCameraUsageDescription": "Maple needs access to your camera to take photos for your AI conversations.",
        "NSMicrophoneUsageDescription": "Maple needs access to your microphone to record voice messages for your AI conversations.",
        "NSPhotoLibraryUsageDescription": "Maple needs access to your photo library to upload images to your AI conversations.",
        "UIBackgroundModes": ["audio"],
        "UIDeviceFamily": [1, 2],
        "UILaunchStoryboardName": "LaunchScreen",
        "UIRequiredDeviceCapabilities": ["arm64", "metal"],
        "UISupportedInterfaceOrientations": ["UIInterfaceOrientationPortrait",
                                             "UIInterfaceOrientationLandscapeLeft",
                                             "UIInterfaceOrientationLandscapeRight"],
        "UISupportedInterfaceOrientations~ipad": ["UIInterfaceOrientationPortrait",
                                                  "UIInterfaceOrientationPortraitUpsideDown",
                                                  "UIInterfaceOrientationLandscapeLeft",
                                                  "UIInterfaceOrientationLandscapeRight"],
    }


class CanonicalPlistTests(unittest.TestCase):
    def canonical(self, info):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "Info.plist"
            path.write_bytes(plistlib.dumps(info, fmt=plistlib.FMT_BINARY))
            return CANONICAL.canonical_info_plist(path)

    def test_reconstructed_internal_export_matches_both_original_failure_hashes(self):
        unsigned = reconstructed_dev_plist()
        exported = dict(unsigned, TFInternalTestingOnly=True)
        for info, digest in (
            (unsigned, "99c054b97ae67b26a4df9f7302a7ba16921fd0b13c3d1c3d09660e7de031a10e"),
            (exported, "f76a0037b05f1a3e29ae65da68a26d72d4bd472ca8a184e910a81e260b5260f3"),
        ):
            raw = plistlib.dumps(info, fmt=plistlib.FMT_XML, sort_keys=True)
            self.assertEqual(hashlib.sha256(raw).hexdigest(), digest)
        self.assertEqual(self.canonical(unsigned), self.canonical(exported))

    def test_false_and_malformed_markers_remain_part_of_the_comparison(self):
        info = reconstructed_dev_plist()
        baseline = self.canonical(info)
        for marker in (False, 0, 1, "true", "false", [], {}):
            with self.subTest(marker=marker):
                self.assertNotEqual(baseline, self.canonical(dict(info, TFInternalTestingOnly=marker)))

    def test_non_device_platforms_and_malformed_platform_values_preserve_marker(self):
        for platforms in (["iPhoneSimulator"], ["MacOSX"], [], "iPhoneOS"):
            with self.subTest(platforms=platforms):
                info = dict(reconstructed_dev_plist(), CFBundleSupportedPlatforms=platforms)
                self.assertNotEqual(self.canonical(info), self.canonical(dict(info, TFInternalTestingOnly=True)))

    def test_other_metadata_changes_still_fail_comparison(self):
        info = reconstructed_dev_plist()
        for key, value in (("CFBundleIdentifier", "cloud.opensecret.maple"),
                           ("CFBundleDisplayName", "Maple"),
                           ("CFBundleURLTypes", [{"CFBundleURLSchemes": ["unexpected"]}]),
                           ("NSCameraUsageDescription", "Changed")):
            with self.subTest(key=key):
                changed = dict(info, TFInternalTestingOnly=True, **{key: value})
                self.assertNotEqual(self.canonical(info), self.canonical(changed))


class ProfileTests(unittest.TestCase):
    def test_native_build_receives_variant_and_pcr_after_xcode_filters_environment(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "apps/maple-research/frontend/src-tauri"
            script = root / "scripts/setup-ios-cargo-config.sh"
            script.parent.mkdir(parents=True)
            shutil.copy2(PROFILE.TAURI_ROOT / "scripts/setup-ios-cargo-config.sh", script)
            library = root / "onnxruntime-ios/onnxruntime.xcframework/ios-arm64/libonnxruntime.a"
            library.parent.mkdir(parents=True)
            library.touch()
            for variant, pcr in (("production", "production"),
                                 ("production", "development"), ("dev", "development")):
                with self.subTest(variant=variant, pcr=pcr):
                    environment = dict(os.environ, MAPLE_IOS_VARIANT=variant,
                                       VITE_MAPLE_APP_VARIANT=variant,
                                       VITE_OPEN_SECRET_PCR_ENVIRONMENT=pcr)
                    subprocess.run(["bash", str(script)], env=environment,
                                   capture_output=True, text=True, check=True)
                    native_env = tomllib.loads((root / ".cargo/config.toml").read_text())["env"]
                    for key in ("MAPLE_IOS_VARIANT", "VITE_MAPLE_APP_VARIANT",
                                "VITE_OPEN_SECRET_PCR_ENVIRONMENT"):
                        self.assertEqual(native_env[key], {"value": environment[key], "force": True})
            environment["VITE_OPEN_SECRET_PCR_ENVIRONMENT"] = "production"
            result = subprocess.run(["bash", str(script)], env=environment,
                                    capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("requires the development PCR", result.stderr)

    def test_shell_selects_complete_profile_and_removes_inherited_values(self):
        for variant in ("dev", "production"):
            with self.subTest(variant=variant):
                environment = dict(os.environ, MAPLE_IOS_VARIANT=variant,
                                   VITE_OPEN_SECRET_API_URL="https://wrong.invalid",
                                   VITE_MAPLE_APP_VARIANT="wrong", VITE_UNRELATED_SECRET="canary",
                                   MAPLE_IOS_DEV_AUTH_ORIGIN=AUTH_ORIGIN)
                script = '''source scripts/ci/_common.sh
source scripts/ci/ios-variant.sh
configure_ios_variant
python3 -c 'import os,json; print(json.dumps({k:v for k,v in os.environ.items() if k.startswith("VITE_")}))'
'''
                result = subprocess.run(["bash", "-c", script], cwd=SCRIPT_DIR.parents[1],
                                        env=environment, capture_output=True, text=True, check=True)
                self.assertEqual(json.loads(result.stdout), environment_for(variant))

    def test_unknown_variant_fails_before_build_or_secret_handling(self):
        environment = dict(os.environ, MAPLE_IOS_VARIANT="staging")
        result = subprocess.run(["bash", str(SCRIPT_DIR / "ios-release.sh")],
                                env=environment, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be production or dev", result.stderr)
        self.assertNotIn("git-commit", result.stdout)

    def test_default_variant_is_production(self):
        environment = dict(os.environ)
        environment.pop("MAPLE_IOS_VARIANT", None)
        result = subprocess.run([
            "bash", "-c", "source scripts/ci/_common.sh; source scripts/ci/ios-variant.sh; "
            "configure_ios_variant; printf '%s' \"$MAPLE_IOS_VARIANT\"",
        ], cwd=SCRIPT_DIR.parents[1], env=environment, capture_output=True, text=True, check=True)
        self.assertEqual(result.stdout, "production")

    def test_mixed_backend_or_pcr_environment_rejected(self):
        for key in environment_for("dev"):
            with self.subTest(key=key):
                environment = dict(environment_for("dev"), **{key: "wrong"})
                error_key = "MAPLE_IOS_DEV_AUTH_ORIGIN" if key == "VITE_MAPLE_DEV_AUTH_ORIGIN" else key
                with self.assertRaisesRegex(ValueError, error_key):
                    PROFILE.make_profile("dev", SOURCE_SHA, BUILD_NUMBER, FRONTEND_SHA, environment)

    def test_auth_origin_rejects_missing_noncanonical_or_credential_bearing_values(self):
        self.assertEqual(PROFILE.validate_auth_origin(AUTH_ORIGIN), AUTH_ORIGIN)
        for invalid in (None, "", "http://dev.example.test", "https://dev.example.test/",
                        "https://dev.example.test/path", "https://user:password@dev.example.test",
                        "https://dev.example.test:443", "https://dev.example.test:8443",
                        "https://dev.example.test?x=1", "https://dev.example.test#fragment",
                        "https://DEV.example.test"):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                PROFILE.validate_auth_origin(invalid)
        with self.assertRaisesRegex(ValueError, "profile"):
            PROFILE.verify_bundle(make_info(), make_profile(), "dev", SOURCE_SHA, BUILD_NUMBER,
                                  auth_origin="https://other-dev.example.test")

    def test_build_number_constraints(self):
        for valid in ("1", "3.4.2", "9999.99.99", BUILD_NUMBER):
            self.assertEqual(PROFILE.validate_build_number(valid), valid)
        for invalid in ("", "0", "10000.1", "1.100", "3.4.2.10", "1.1beta", "-1"):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                PROFILE.validate_build_number(invalid)

    def test_each_app_and_ipa_verified_from_actual_contents(self):
        for variant in PROFILE.PROFILES:
            with self.subTest(variant=variant), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                app = root / "Renamed.app"
                (app / "assets").mkdir(parents=True)
                (app / "Info.plist").write_bytes(plistlib.dumps(make_info(variant), fmt=plistlib.FMT_BINARY))
                (app / PROFILE.RESOURCE_PATH).write_text(json.dumps(make_profile(variant)))
                result = PROFILE.verify_app(app, variant, SOURCE_SHA, BUILD_NUMBER, auth_origin=AUTH_ORIGIN)
                self.assertEqual(result["bundle_identifier"], PROFILE.PROFILES[variant]["bundle_identifier"])
                # Xcode adds the restriction only while exporting the archive.
                if variant == "dev":
                    (app / "Info.plist").write_bytes(plistlib.dumps(
                        dict(make_info(variant), TFInternalTestingOnly=True)))
                ipa = root / "misleading-filename.ipa"
                with zipfile.ZipFile(ipa, "w") as archive:
                    for path in app.rglob("*"):
                        if path.is_file():
                            archive.write(path, f"Payload/Anything.app/{path.relative_to(app)}")
                report = PROFILE.verify_ipa(ipa, variant, SOURCE_SHA, BUILD_NUMBER, auth_origin=AUTH_ORIGIN)
                self.assertRegex(report["ipa_sha256"], r"^[0-9a-f]{64}$")

    def test_dev_ipa_requires_exact_boolean_internal_testing_marker(self):
        with tempfile.TemporaryDirectory() as temp:
            ipa = Path(temp) / "exported.ipa"
            for marker in (None, False, 0, 1, "true", "false", [], {}, True):
                with self.subTest(marker=marker):
                    info = make_info()
                    if marker is not None:
                        info["TFInternalTestingOnly"] = marker
                    with zipfile.ZipFile(ipa, "w") as archive:
                        archive.writestr("Payload/Maple.app/Info.plist", plistlib.dumps(info))
                        archive.writestr(f"Payload/Maple.app/{PROFILE.RESOURCE_PATH}",
                                         json.dumps(make_profile()))
                    if marker is True:
                        PROFILE.verify_ipa(ipa, "dev", SOURCE_SHA, BUILD_NUMBER, auth_origin=AUTH_ORIGIN)
                    else:
                        with self.assertRaisesRegex(ValueError, "internal TestFlight"):
                            PROFILE.verify_ipa(ipa, "dev", SOURCE_SHA, BUILD_NUMBER, auth_origin=AUTH_ORIGIN)

    def test_production_ipa_does_not_require_internal_testing_marker(self):
        with tempfile.TemporaryDirectory() as temp:
            ipa = Path(temp) / "production.ipa"
            for marker in (None, False):
                with self.subTest(marker=marker):
                    info = make_info("production")
                    if marker is not None:
                        info["TFInternalTestingOnly"] = marker
                    with zipfile.ZipFile(ipa, "w") as archive:
                        archive.writestr("Payload/Maple.app/Info.plist", plistlib.dumps(info))
                        archive.writestr(f"Payload/Maple.app/{PROFILE.RESOURCE_PATH}",
                                         json.dumps(make_profile("production")))
                    PROFILE.verify_ipa(ipa, "production", SOURCE_SHA, BUILD_NUMBER)

    def test_cross_variant_identity_source_version_and_profile_mixups_fail(self):
        for variant in PROFILE.PROFILES:
            other = "production" if variant == "dev" else "dev"
            for field, value in (("CFBundleIdentifier", PROFILE.PROFILES[other]["bundle_identifier"]),
                                 ("CFBundleDisplayName", PROFILE.PROFILES[other]["display_name"]),
                                 ("CFBundleVersion", "41.1")):
                with self.subTest(variant=variant, field=field), self.assertRaises(ValueError):
                    PROFILE.verify_bundle(dict(make_info(variant), **{field: value}),
                                          make_profile(variant), variant, SOURCE_SHA, BUILD_NUMBER, auth_origin=AUTH_ORIGIN)
            with self.assertRaises(ValueError):
                PROFILE.verify_bundle(make_info(variant), make_profile(other), variant, SOURCE_SHA, BUILD_NUMBER, auth_origin=AUTH_ORIGIN)
            with self.assertRaises(ValueError):
                PROFILE.verify_bundle(make_info(variant), make_profile(variant), variant, "c" * 40, BUILD_NUMBER, auth_origin=AUTH_ORIGIN)

    def test_other_app_scheme_rejected_even_when_own_scheme_present(self):
        info = make_info()
        info["CFBundleURLTypes"][0]["CFBundleURLSchemes"].append("cloud.opensecret.maple")
        with self.assertRaisesRegex(ValueError, "other variant"):
            PROFILE.verify_bundle(info, make_profile(), "dev", SOURCE_SHA, BUILD_NUMBER, auth_origin=AUTH_ORIGIN)

    def test_production_export_managed_number_is_preserved_but_dev_remains_exact(self):
        with tempfile.TemporaryDirectory() as temp:
            ipa = Path(temp) / "exported.ipa"
            for variant in PROFILE.PROFILES:
                info = dict(make_info(variant), CFBundleVersion="43")
                with zipfile.ZipFile(ipa, "w") as archive:
                    archive.writestr("Payload/Maple.app/Info.plist", plistlib.dumps(info))
                    archive.writestr(f"Payload/Maple.app/{PROFILE.RESOURCE_PATH}",
                                     json.dumps(make_profile(variant)))
                if variant == "production":
                    report = PROFILE.verify_ipa(ipa, variant, SOURCE_SHA, BUILD_NUMBER, auth_origin=AUTH_ORIGIN)
                    self.assertEqual(report["build_number"], "43")
                    self.assertEqual(report["profile"]["build_number"], BUILD_NUMBER)
                else:
                    with self.assertRaisesRegex(ValueError, "build number"):
                        PROFILE.verify_ipa(ipa, variant, SOURCE_SHA, BUILD_NUMBER, auth_origin=AUTH_ORIGIN)

    def test_missing_or_multiple_ipa_bundles_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            ipa = Path(temp) / "bad.ipa"
            for paths in ([], ["Payload/One.app/Info.plist", "Payload/Two.app/Info.plist"]):
                with self.subTest(paths=paths):
                    with zipfile.ZipFile(ipa, "w") as archive:
                        for path in paths:
                            archive.writestr(path, plistlib.dumps(make_info()))
                    with self.assertRaisesRegex(ValueError, "exactly one application"):
                        PROFILE.verify_ipa(ipa, "dev", SOURCE_SHA, BUILD_NUMBER, auth_origin=AUTH_ORIGIN)

    def test_snapshot_restores_existing_generated_state_and_removes_new_files(self):
        with tempfile.TemporaryDirectory() as temp:
            root, state = Path(temp) / "project", Path(temp) / "state"
            state.mkdir()
            original = PROFILE.STATE_PATHS[:3]
            for index, relative in enumerate(original):
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(f"original-{index}".encode())
            PROFILE.snapshot(root, state)
            for relative in PROFILE.STATE_PATHS:
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("generated-change")
            PROFILE.restore(root, state)
            for index, relative in enumerate(original):
                self.assertEqual((root / relative).read_bytes(), f"original-{index}".encode())
            for relative in PROFILE.STATE_PATHS[3:]:
                self.assertFalse((root / relative).exists())

    def test_dev_preparation_is_internal_only_and_preserves_originals_after_failure(self):
        with tempfile.TemporaryDirectory() as temp:
            root, state = Path(temp) / "project", Path(temp) / "state"
            state.mkdir()
            for relative, data in {
                "gen/apple/maple_iOS/Info.plist": make_info("production"),
                "gen/apple/maple_iOS/maple_iOS.entitlements": {
                    "com.apple.developer.associated-domains": ["applinks:trymaple.ai"],
                    "com.apple.developer.applesignin": ["Default"],
                },
                "gen/apple/ExportOptions.plist": {"method": "debugging"},
            }.items():
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(plistlib.dumps(data))
            PROFILE.snapshot(root, state)
            # Simulates a failed build setup (missing generated icons).
            with self.assertRaisesRegex(ValueError, "missing generated"):
                PROFILE.prepare_dev(root)
            export_path = root / "gen/apple/ExportOptions.plist"
            options = plistlib.loads(export_path.read_bytes())
            self.assertTrue(options["testFlightInternalTestingOnly"])
            self.assertFalse(options["manageAppVersionAndBuildNumber"])
            entitlements = plistlib.loads((root / PROFILE.STATE_PATHS[1]).read_bytes())
            self.assertNotIn("com.apple.developer.associated-domains", entitlements)
            self.assertEqual(entitlements["com.apple.developer.applesignin"], ["Default"])
            PROFILE.restore(root, state)
            self.assertEqual(plistlib.loads(export_path.read_bytes()), {"method": "debugging"})
            self.assertEqual(plistlib.loads((root / PROFILE.STATE_PATHS[0]).read_bytes()), make_info("production"))

    def test_overlay_keeps_production_identity_and_selects_only_dev_scheme(self):
        with tempfile.TemporaryDirectory() as temp:
            state = Path(temp)
            for variant in PROFILE.PROFILES:
                with self.subTest(variant=variant):
                    PROFILE.build_config(PROFILE.TAURI_ROOT, state, make_profile(variant))
                    config = json.loads((state / "tauri-build-config.json").read_text())
                    self.assertIsNone(config["build"]["beforeBuildCommand"])
                    self.assertEqual(config["bundle"]["iOS"]["bundleVersion"], BUILD_NUMBER)
                    if variant == "production":
                        self.assertNotIn("identifier", config)
                        self.assertNotIn("productName", config)
                        self.assertNotIn("plugins", config)
                    else:
                        self.assertEqual(config["identifier"], "cloud.opensecret.maple.dev")
                        self.assertEqual(config["productName"], "Maple Dev")
                        self.assertEqual(config["plugins"]["deep-link"]["mobile"], [])
                        self.assertEqual(config["plugins"]["deep-link"]["desktop"]["schemes"],
                                         ["cloud.opensecret.maple.dev"])


if __name__ == "__main__":
    unittest.main()
