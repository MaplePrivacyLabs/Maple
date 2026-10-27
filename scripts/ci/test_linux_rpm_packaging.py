#!/usr/bin/env python3
"""Exercise RPM staging identity and packaging failures without native builds."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class RpmPackagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.tauri = self.root / "tauri"
        self.deb = self.tauri / "target/release/bundle/deb/Maple.deb"
        self.deb.parent.mkdir(parents=True)
        self.deb.write_bytes(b"fixture\0__TAURI_BUNDLE_TYPE_VAR_DEB\0payload")
        (self.tauri / "tauri.conf.json").write_text('{"version":"1.2.3"}')
        self.rpm = self.tauri / "target/release/bundle/rpm/Maple-1.2.3-1.x86_64.rpm"
        self.bin = self.root / "bin"
        self.bin.mkdir()

    def run_shell(self, commands):
        env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                   FIXTURE_TAURI=str(self.tauri), SOURCE_DATE_EPOCH="1234567890")
        return subprocess.run(
            ["bash", "-c", 'source scripts/ci/_common.sh\n'
             'TAURI_DIR="$FIXTURE_TAURI"\n'
             'host_os() { echo linux; }\n'
             'rpm_arch() { echo x86_64; }\n' + commands],
            cwd=ROOT, env=env, capture_output=True, text=True,
        )

    def test_normalization_preserves_deb_and_marks_only_rpm_copy(self):
        original = self.deb.read_bytes()
        result = self.run_shell('''
normalize_deb_package() { :; }
extract_deb_payload() {
  mkdir -p "$2/usr/bin"
  cp "$1" "$2/usr/bin/maple"
}
build_rpm_package_from_payload() { cp "$2/usr/bin/maple" "$1"; }
normalize_linux_desktop_packages
''')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.deb.read_bytes(), original)
        self.assertEqual(self.rpm.read_bytes(), original.replace(b"VAR_DEB", b"VAR_RPM"))

    def test_invalid_staging_identity_never_builds_rpm(self):
        self.deb.write_bytes(b"missing bundle identity")
        result = self.run_shell('''
normalize_deb_package() { :; }
extract_deb_payload() {
  mkdir -p "$2/usr/bin"
  cp "$1" "$2/usr/bin/maple"
}
build_rpm_package_from_payload() { touch "$1"; }
normalize_linux_desktop_packages
''')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.rpm.exists())

    def test_failed_extraction_is_not_hidden_by_cleanup(self):
        result = self.run_shell('''
ar() { return 19; }
if extract_deb_payload "${TAURI_DIR}/target/release/bundle/deb/Maple.deb" "$TAURI_DIR/payload"; then
  exit 0
else
  exit "$?"
fi
''')
        self.assertEqual(result.returncode, 19)

    def test_appimage_extraction_excludes_privileged_xattrs(self):
        result = self.run_shell(r'''
appimage_squashfs_offset() { echo 123; }
unsquashfs() {
  [ "$1" = -xattrs-include ] && [ "$2" = '^user\.' ] || return 19
  [ "$3" = -d ] || return 19
  mkdir -p "$4"
  touch "$4/AppRun"
  chmod +x "$4/AppRun"
}
extract_appimage_tool fixture.AppImage "$TAURI_DIR/extracted"
''')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.tauri / "extracted/AppRun").exists())

    def fake_rpmbuild(self, exit_code):
        tool = self.bin / "rpmbuild"
        tool.write_text(f'''#!{shutil.which("bash")}
set -eu
cp "$2" "$FIXTURE_TAURI/maple.spec"
while [ "$#" -gt 0 ]; do
  if [ "$1" = --define ]; then
    case "$2" in "_topdir "*) topdir="${{2#_topdir }}" ;; esac
    shift
  fi
  shift
done
mkdir -p "$topdir/RPMS/x86_64"
printf 'partial RPM' > "$topdir/RPMS/x86_64/maple-1.2.3-1.x86_64.rpm"
exit {exit_code}
''')
        tool.chmod(0o755)

    def run_factory(self, extra=""):
        return self.run_shell(extra + '''
mkdir -p "$TAURI_DIR/payload/usr/bin" "$TAURI_DIR/target/release/bundle/rpm"
printf 'fixture' > "$TAURI_DIR/payload/usr/bin/maple"
if build_rpm_package_from_payload "$TAURI_DIR/target/release/bundle/rpm/Maple-1.2.3-1.x86_64.rpm" "$TAURI_DIR/payload"; then
  exit 0
else
  exit "$?"
fi
''')

    def test_failed_rpmbuild_cannot_publish_partial_output(self):
        self.fake_rpmbuild(19)
        result = self.run_factory()
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.rpm.exists())

    def run_dev_factory(self):
        config = {"version": "1.2.3", "productName": "Maple Research Dev", "mainBinaryName": "maple-research-dev",
                  "bundle": {"linux": {"rpm": {"depends": ["libwebkit2gtk-4.1.so.0()(64bit)"]}}}}
        (self.tauri / "dev.conf.json").write_text(json.dumps(config))
        return self.run_shell('''
mkdir -p "$TAURI_DIR/payload/usr/bin" "$TAURI_DIR/target/release/bundle/rpm"
printf fixture > "$TAURI_DIR/payload/usr/bin/maple-research-dev"
build_rpm_package_from_payload "$TAURI_DIR/target/release/bundle/rpm/Maple-1.2.3-1.x86_64.rpm" "$TAURI_DIR/payload" "$TAURI_DIR/dev.conf.json"
''')

    def test_dev_rpm_uses_its_own_identity_and_dependencies(self):
        self.fake_rpmbuild(0)
        result = self.run_dev_factory()
        self.assertEqual(result.returncode, 0, result.stderr)
        spec = (self.tauri / "maple.spec").read_text()
        self.assertIn("Name: maple-research-dev\n", spec)
        self.assertIn("Summary: Maple Research Dev\n", spec)
        self.assertIn("Requires: libwebkit2gtk-4.1.so.0()(64bit)\n", spec)
        self.assertNotIn("Name: maple\n", spec)
        self.assertNotIn("Requires: webkit2gtk4.1\n", spec)

    def test_default_rpm_does_not_gain_dev_requirements(self):
        self.fake_rpmbuild(0)
        result = self.run_factory()
        self.assertEqual(result.returncode, 0, result.stderr)
        spec = (self.tauri / "maple.spec").read_text()
        self.assertIn("Name: maple\n", spec)
        self.assertNotIn("Requires:", spec)

    @unittest.skipUnless(shutil.which("rpmbuild") and shutil.which("rpm"), "RPM tools are unavailable")
    def test_built_dev_rpm_declares_the_runtime_capability(self):
        result = self.run_dev_factory()
        self.assertEqual(result.returncode, 0, result.stderr)
        name = subprocess.check_output(["rpm", "-qp", "--queryformat", "%{NAME}", str(self.rpm)], text=True)
        requires = subprocess.check_output(["rpm", "-qp", "--requires", str(self.rpm)], text=True).splitlines()
        self.assertEqual(name, "maple-research-dev")
        self.assertIn("libwebkit2gtk-4.1.so.0()(64bit)", requires)
        self.assertNotIn("webkit2gtk4.1", requires)

    def test_rpm_preserves_configured_runtime_dependencies(self):
        config = json.loads((ROOT / "apps/maple-research/frontend/src-tauri/tauri.conf.json").read_text())
        self.assertIn("webkit2gtk4.1", config["bundle"]["linux"]["rpm"]["depends"])
        (self.tauri / "tauri.conf.json").write_text(json.dumps(config))
        self.fake_rpmbuild(0)
        result = self.run_factory()
        self.assertEqual(result.returncode, 0, result.stderr)
        spec = (self.tauri / "maple.spec").read_text()
        self.assertIn("Requires: webkit2gtk4.1\n", spec)

    def test_rpm_dependency_verification(self):
        for requires, expected in (("webkit2gtk4.1", 0), ("rpmlib(PayloadFilesHavePrefix) <= 4.0-1", 1), ("webkit2gtk4.0", 1)):
            with self.subTest(requires=requires):
                result = self.run_shell(f'''
rpm() {{ printf '%s\\n' '{requires}'; }}
verify_linux_rpm_package_dependencies fixture.rpm
''')
                self.assertEqual(result.returncode, expected, result.stderr)

    @unittest.skipUnless(shutil.which("rpmbuild") and shutil.which("rpm"), "RPM tools are unavailable")
    def test_built_rpm_declares_webkit_dependency(self):
        config = json.loads((ROOT / "apps/maple-research/frontend/src-tauri/tauri.conf.json").read_text())
        config["version"] = "1.2.3"
        (self.tauri / "tauri.conf.json").write_text(json.dumps(config))
        result = self.run_factory()
        self.assertEqual(result.returncode, 0, result.stderr)
        requires = subprocess.run(
            ["rpm", "-qp", "--requires", str(self.rpm)],
            capture_output=True, text=True, check=True,
        ).stdout.splitlines()
        self.assertIn("webkit2gtk4.1", requires)
        verified = self.run_shell(f'verify_linux_rpm_package_dependencies "{self.rpm}"')
        self.assertEqual(verified.returncode, 0, verified.stderr)

    def test_rpm_query_failure_is_rejected(self):
        result = self.run_shell('''
rpm() { return 19; }
verify_linux_rpm_package_dependencies fixture.rpm
''')
        self.assertNotEqual(result.returncode, 0)

    def test_failed_final_move_is_not_hidden_by_cleanup(self):
        self.fake_rpmbuild(0)
        result = self.run_factory('mv() { return 19; }\n')
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.rpm.exists())


if __name__ == "__main__":
    unittest.main()
