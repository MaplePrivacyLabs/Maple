"""Offline auth provenance, static artifact, and destination boundary regressions."""

import copy
import hashlib
import io
import json
import os
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import Mock, patch
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pages_artifact as artifact
import pages_auth_deploy as auth
import pages_deploy as pages

SHA, OLD_SHA, OTHER_SHA = "a" * 40, "b" * 40, "c" * 40


def write_archive(path, extra=None):
    entries = {"index.html": b"<html>Auth</html>", "assets/auth.js": b"export {};"}
    entries.update(extra or {})
    with tarfile.open(path, "w:gz") as archive:
        for name, content in entries.items():
            entry = tarfile.TarInfo(name)
            entry.size = len(content)
            archive.addfile(entry, io.BytesIO(content))
    return hashlib.sha256(path.read_bytes()).hexdigest()


class MemoryAPI:
    def __init__(self, values):
        self.values = values

    def json(self, path, method="GET", data=None):
        if method != "GET":
            raise AssertionError("Selection cannot write remote state")
        return copy.deepcopy(self.values[path])


class AuthProvenanceTests(unittest.TestCase):
    environment = "production"

    def setUp(self):
        self.config = auth.TARGETS[self.environment]
        self.root = "/repos/OpenSecretCloud/Maple"
        self.run = {"id": 100, "workflow_id": 10, "path": f".github/workflows/{self.config.build_workflow}",
                    "event": "workflow_dispatch", "status": "completed", "conclusion": "success",
                    "run_attempt": 2, "head_sha": SHA, "head_branch": "master",
                    "repository": {"id": auth.REPOSITORY_ID}, "head_repository": {"id": auth.REPOSITORY_ID}}
        self.asset = {"id": 300, "name": f"{self.config.artifact_prefix}-100-2", "expired": False,
                      "digest": "sha256:" + "d" * 64, "size_in_bytes": 1000}
        self.values = {self.root: {"id": auth.REPOSITORY_ID, "default_branch": "master"},
                       self.root + "/actions/runs/100": self.run,
                       self.root + f"/actions/workflows/{self.config.build_workflow}": {"id": 10},
                       self.root + "/git/ref/heads/master": {"object": {"sha": SHA}},
                       self.root + f"/git/ref/heads/{self.config.destination.production_branch}": {"object": {"sha": OLD_SHA}},
                       self.root + f"/compare/{OLD_SHA}...{SHA}": {"status": "ahead"},
                       self.root + "/actions/runs/100/artifacts?per_page=100":
                       {"total_count": 1, "artifacts": [self.asset]}}
        self.gh = pages.GitHub(MemoryAPI(self.values), "OpenSecretCloud/Maple", auth.REPOSITORY_ID)
        self.event = {"inputs": {"build_run_id": "100", "build_run_attempt": "2"}}

    def select(self):
        return auth.select_plan(self.gh, self.event, environment=self.environment)

    def test_exact_successful_manual_master_build(self):
        plan = self.select()
        self.assertEqual((plan["profile"], plan["branch"], plan["sha"]),
                         (self.config.profile, self.config.destination.production_branch, SHA))
        self.assertEqual((plan["run_id"], plan["run_attempt"], plan["artifact_id"]), (100, 2, 300))

    def test_wrong_workflow_event_status_attempt_and_branch(self):
        for key, value in (("workflow_id", 12), ("path", ".github/workflows/release.yml"),
                           ("event", "release"), ("conclusion", "failure"), ("status", "in_progress"),
                           ("run_attempt", 3), ("head_branch", "feature"), ("id", 200)):
            with self.subTest(key=key):
                old = self.run[key]
                self.run[key] = value
                with self.assertRaises(pages.Rejected):
                    self.select()
                self.run[key] = old

    def test_foreign_repository_and_fork_source(self):
        for key in ("repository", "head_repository"):
            with self.subTest(key=key):
                self.run[key]["id"] = 99
                with self.assertRaises(pages.Rejected):
                    self.select()
                self.run[key]["id"] = auth.REPOSITORY_ID
        self.gh.repository_id = 99
        with self.assertRaises(pages.Rejected):
            self.select()

    def test_source_and_ref_must_be_current_and_forward(self):
        self.values[self.root + "/git/ref/heads/master"]["object"]["sha"] = OTHER_SHA
        with self.assertRaises(pages.Superseded):
            self.select()
        self.values[self.root + "/git/ref/heads/master"]["object"]["sha"] = SHA
        self.values[self.root + f"/compare/{OLD_SHA}...{SHA}"]["status"] = "behind"
        with self.assertRaises(pages.Rejected):
            self.select()
        self.values[self.root + f"/git/ref/heads/{self.config.destination.production_branch}"]["object"]["sha"] = SHA
        self.assertEqual(self.select()["previous_sha"], SHA)

    def test_missing_production_ref_is_not_created(self):
        del self.values[self.root + f"/git/ref/heads/{self.config.destination.production_branch}"]
        with self.assertRaises(KeyError):
            self.select()

    def test_artifact_must_match_attempt_and_be_single_unexpired_signed(self):
        for key, value in (("name", f"{self.config.artifact_prefix}-100-1"), ("expired", True),
                           ("digest", None), ("size_in_bytes", 0), ("size_in_bytes", True)):
            with self.subTest(key=key, value=value):
                old = self.asset[key]
                self.asset[key] = value
                with self.assertRaises(pages.Rejected):
                    self.select()
                self.asset[key] = old
        self.values[self.root + "/actions/runs/100/artifacts?per_page=100"]["artifacts"].append(self.asset)
        with self.assertRaises(pages.Rejected):
            self.select()

    def test_manual_input_is_not_a_source_or_destination_override(self):
        for value in ("0", "01", "1.0", "100; echo unsafe", 100, True, "1" * 21):
            with self.subTest(value=value), self.assertRaises(pages.Rejected):
                auth.input_number(value)
        self.event["inputs"]["project"] = "maple"
        with self.assertRaises(pages.Rejected):
            self.select()
        del self.event["inputs"]["project"]
        self.event["workflow_run"] = self.run
        with self.assertRaises(pages.Rejected):
            self.select()

    def test_activation_and_protected_dispatch_required(self):
        environment = {self.config.enabled_variable: "true", "GITHUB_REF": "refs/heads/master",
                       "GITHUB_EVENT_NAME": "workflow_dispatch"}
        with patch.dict(os.environ, environment, clear=True):
            auth.require_publisher_environment(self.environment)
        for key, value in ((self.config.enabled_variable, "TRUE"),
                           (self.config.enabled_variable, "false"),
                           (self.config.enabled_variable, ""),
                           ("GITHUB_REF", "refs/heads/feature"), ("GITHUB_EVENT_NAME", "workflow_run")):
            with self.subTest(key=key, value=value), patch.dict(os.environ, {**environment, key: value}, clear=True):
                with self.assertRaises(pages.Rejected):
                    auth.require_publisher_environment(self.environment)


    def test_other_environment_cannot_supply_workflow_or_artifact(self):
        other = auth.TARGETS["development" if self.environment == "production" else "production"]
        self.values[self.root + f"/actions/workflows/{other.build_workflow}"] = {"id": 11}
        self.run.update(workflow_id=11, path=f".github/workflows/{other.build_workflow}")
        with self.assertRaises(pages.Rejected):
            self.select()
        self.run.update(workflow_id=10, path=f".github/workflows/{self.config.build_workflow}")
        self.asset["name"] = f"{other.artifact_prefix}-100-2"
        with self.assertRaises(pages.Rejected):
            self.select()

    def test_only_selected_environment_flag_enables_publication(self):
        other = auth.TARGETS["development" if self.environment == "production" else "production"]
        base = {"GITHUB_REF": "refs/heads/master", "GITHUB_EVENT_NAME": "workflow_dispatch"}
        for value in (None, "", "false", "TRUE", "1"):
            environment = {**base, other.enabled_variable: "true"}
            if value is not None:
                environment[self.config.enabled_variable] = value
            with self.subTest(value=value), patch.dict(os.environ, environment, clear=True):
                with self.assertRaises(pages.Rejected):
                    auth.require_publisher_environment(self.environment)

    def test_preparation_uses_selected_manifest_profile(self):
        with tempfile.TemporaryDirectory() as directory:
            state = Path(directory) / "state"
            with patch.object(pages, "download_build_artifact", return_value="d" * 64) as download, \
                    patch.object(auth, "extract_static", return_value={}):
                plan = auth.prepare(self.gh, self.event, state, environment=self.environment)
            self.assertEqual(plan["auth_environment"], self.environment)
            self.assertEqual(download.call_args.kwargs["expected_profile"], self.config.profile)

    def test_prepared_other_environment_cannot_deploy(self):
        plan = self.select()
        plan["auth_environment"] = "development" if self.environment == "production" else "production"
        with tempfile.TemporaryDirectory() as directory:
            state = Path(directory)
            (state / "plan.json").write_text(json.dumps({"selection": plan}))
            with patch.object(pages, "API") as api, patch.object(pages, "run_wrangler") as upload:
                with self.assertRaises(pages.Superseded):
                    pages.deploy(self.gh, self.event, state, destination=self.config.destination,
                                 selector=lambda gh, event, target: auth.select_plan(
                                     gh, event, target, environment=self.environment))
                api.assert_not_called()
                upload.assert_not_called()


class AuthDevelopmentProvenanceTests(AuthProvenanceTests):
    environment = "development"


class AuthArtifactTests(unittest.TestCase):
    def test_auth_zip_requires_matching_archive_and_manifest_profile(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "auth.tar.gz"
            write_archive(archive)
            for expected in ("auth-release", "auth-dev"):
                for archive_name in (artifact.AUTH_ARCHIVE_NAME, artifact.ARCHIVE_NAME):
                    for profile in ("auth-release", "auth-dev", "pr", "release"):
                        with self.subTest(expected=expected, name=archive_name, profile=profile):
                            zipped = root / "artifact.zip"
                            with zipfile.ZipFile(zipped, "w") as output:
                                output.write(archive, archive_name)
                                output.writestr(artifact.MANIFEST_NAME, json.dumps(
                                    artifact.pack_manifest(archive, profile, SHA, 100, 2)))
                            destination = root / (expected + profile + archive_name)
                            if archive_name == artifact.AUTH_ARCHIVE_NAME and profile == expected:
                                result = artifact.read_preview_zip(zipped, SHA, 100, 2, destination,
                                    archive_name=artifact.AUTH_ARCHIVE_NAME, expected_profile=expected)
                                self.assertEqual(result["profile"], expected)
                            else:
                                with self.assertRaises(artifact.ArtifactError):
                                    artifact.read_preview_zip(zipped, SHA, 100, 2, destination,
                                        archive_name=artifact.AUTH_ARCHIVE_NAME, expected_profile=expected)

    def test_producer_cannot_supply_header_or_worker_configuration(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("_headers", "_redirects", "_worker.js", "functions/index.js"):
                with self.subTest(name=name):
                    archive = root / "auth.tar.gz"
                    digest = write_archive(archive, {name: b"untrusted"})
                    with self.assertRaises(artifact.ArtifactError):
                        artifact.extract_static(archive, root / name.replace("/", "-"), digest)


class AuthDestinationTests(unittest.TestCase):
    environment = "production"

    def setUp(self):
        self.config = auth.TARGETS[self.environment]
        self.destination = self.config.destination

    def test_exact_trusted_headers_and_app_unchanged(self):
        expected = ("/*\n  Cache-Control: no-store, max-age=0\n  X-Robots-Tag: noindex, nofollow\n"
                    "  Referrer-Policy: no-referrer\n  X-Frame-Options: DENY\n"
                    "  Content-Security-Policy: frame-ancestors 'none'\n")
        with tempfile.TemporaryDirectory() as directory:
            assets = Path(directory)
            pages.add_trusted_headers(assets, pages.APP_DESTINATION)
            self.assertEqual(list(assets.iterdir()), [])
            pages.add_trusted_headers(assets, self.destination)
            self.assertEqual((assets / "_headers").read_bytes(), expected.encode())
            with self.assertRaises(FileExistsError):
                pages.add_trusted_headers(assets, self.destination)

    def test_cloudflare_auth_identity_and_disabled_native_builds(self):
        project = {"name": self.destination.project, "subdomain": self.destination.subdomain,
                   "production_branch": self.destination.production_branch,
                   "source": {"config": {"production_deployments_enabled": False}}}
        cf = Mock()
        cf.json.return_value = {"success": True, "result": project}
        pages.cloudflare_project(cf, "a" * 32, "production", self.destination)
        self.assertIn(f"/projects/{self.destination.project}", cf.json.call_args.args[0])
        with self.assertRaises(pages.Rejected):
            pages.cloudflare_project(cf, "a" * 32, "production")
        project["source"]["config"]["production_deployments_enabled"] = True
        with self.assertRaises(pages.Rejected):
            pages.cloudflare_project(cf, "a" * 32, "production", self.destination)

    def test_other_auth_project_is_rejected(self):
        other = auth.TARGETS["development" if self.environment == "production" else "production"].destination
        project = {"name": other.project, "subdomain": other.subdomain,
                   "production_branch": other.production_branch, "source": None}
        cf = Mock()
        cf.json.return_value = {"success": True, "result": project}
        with self.assertRaises(pages.Rejected):
            pages.cloudflare_project(cf, "a" * 32, "production", self.destination)

    def test_report_uses_auth_environment_and_public_url(self):
        gh = Mock()
        gh.write.return_value = {"id": 9}
        pages.report(gh, {"target": "production", "sha": SHA}, {"url": f"https://12345678.{self.destination.subdomain}"}, self.destination)
        self.assertEqual(gh.write.call_args_list[0].args[1]["environment"], self.destination.environment)
        self.assertEqual(gh.write.call_args_list[0].args[1]["production_environment"], self.environment == "production")
        self.assertEqual(gh.write.call_args_list[1].args[1]["environment_url"], self.destination.public_url)

    def test_auth_direct_upload_source_policy_is_fail_closed(self):
        project = {"name": self.destination.project, "subdomain": self.destination.subdomain,
                   "production_branch": self.destination.production_branch}
        cf = Mock()
        cf.json.return_value = {"success": True, "result": project}
        pages.cloudflare_project(cf, "a" * 32, "production", self.destination)
        project["source"] = None
        pages.cloudflare_project(cf, "a" * 32, "production", self.destination)
        for source in ({}, [], "github", False, {"config": None}, {"config": {}},
                       {"config": {"production_deployments_enabled": True}},
                       {"config": {"production_deployments_enabled": 0}}):
            with self.subTest(source=source):
                project["source"] = source
                with self.assertRaises(pages.Rejected):
                    pages.cloudflare_project(cf, "a" * 32, "production", self.destination)
        project.update(name="maple", subdomain="maple-ca8.pages.dev", production_branch="pages-production")
        for source in (None, {}):
            project["source"] = source
            with self.assertRaises(pages.Rejected):
                pages.cloudflare_project(cf, "a" * 32, "production")

    def test_deploy_reextracts_adds_headers_and_advances_only_auth_ref(self):
        with tempfile.TemporaryDirectory() as directory:
            state = Path(directory) / "state"
            state.mkdir()
            digest = write_archive(state / "web.tar.gz")
            files = artifact.extract_static(state / "web.tar.gz", state / "assets", digest)
            # Prepared assets are not consumed by the credential-bearing upload.
            (state / "assets/index.html").write_text("tampered prepared file")
            plan = {"target": "production", "profile": self.config.profile, "sha": SHA,
                    "previous_sha": OLD_SHA, "branch": self.destination.production_branch}
            (state / "plan.json").write_text(json.dumps({"selection": plan, "archive_digest": digest, "files": files}))
            gh = Mock()
            gh.write.side_effect = [{"object": {"sha": SHA}}, {"id": 9}, {}]
            selector = Mock(return_value=plan)

            def upload(selected, assets, account, token, workdir, destination):
                self.assertEqual(destination, self.destination)
                self.assertEqual((assets / "index.html").read_bytes(), b"<html>Auth</html>")
                self.assertEqual((assets / "_headers").read_text(), auth.AUTH_HEADERS)
                return {"url": f"https://12345678.{self.destination.subdomain}", "deployment_id": "a" * 36}

            with patch.dict(os.environ, {"CLOUDFLARE_ACCOUNT_ID": "a" * 32, "CLOUDFLARE_API_TOKEN": "synthetic"}, clear=True), \
                    patch.object(pages, "API"), patch.object(pages, "cloudflare_project"), \
                    patch.object(pages, "run_wrangler", side_effect=upload), patch.object(pages, "verify_deployment"):
                pages.deploy(gh, {}, state, destination=self.destination, selector=selector)
            self.assertEqual(selector.call_count, 2)
            update = gh.write.call_args_list[0]
            self.assertEqual(update.args, (f"/git/refs/heads/{self.destination.production_branch}", {"sha": SHA, "force": False}, "PATCH"))


class AuthDevelopmentDestinationTests(AuthDestinationTests):
    environment = "development"


if __name__ == "__main__":
    unittest.main()
