#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd -P)"
classifier="${script_dir}/classify-app-release.sh"
temp_root="$(mktemp -d)"
trap 'rm -rf "${temp_root}"' EXIT HUP INT TERM

passed=0

pass() {
  passed=$((passed + 1))
  printf 'ok %d - %s\n' "${passed}" "$1"
}

fail() {
  echo "not ok - $*" >&2
  exit 1
}

expect_failure() {
  local label="$1"
  shift

  if output="$("$@" 2>&1)"; then
    echo "Unexpected success output:" >&2
    printf '%s\n' "${output}" >&2
    fail "${label}"
  fi
  pass "${label}"
}

make_fixture() {
  local directory="$1"
  local app_version="$2"
  local release_tag="$3"
  local cargo_version="${4:-${app_version}}"

  mkdir -p "${directory}/apps/maple-research/frontend/src-tauri"
  printf '{"version":"%s"}\n' "${app_version}" > "${directory}/apps/maple-research/frontend/package.json"
  printf '{"version":"%s"}\n' "${app_version}" > "${directory}/apps/maple-research/frontend/src-tauri/tauri.conf.json"
  printf '[package]\nname = "maple-fixture"\nversion = "%s"\n\n[dependencies]\n' "${cargo_version}" \
    > "${directory}/apps/maple-research/frontend/src-tauri/Cargo.toml"
  git -C "${directory}" init -q -b master
  git -C "${directory}" config user.name "Maple release gate tests"
  git -C "${directory}" config user.email "release-gates@example.invalid"
  git -C "${directory}" add \
    apps/maple-research/frontend/package.json \
    apps/maple-research/frontend/src-tauri/Cargo.toml \
    apps/maple-research/frontend/src-tauri/tauri.conf.json
  git -C "${directory}" commit -q -m "fixture"
  git -C "${directory}" tag -a "${release_tag}" -m "${release_tag}"
}

run_classifier() {
  local fixture="$1"
  local tag="$2"
  local sha="$3"
  local draft="$4"
  local prerelease="$5"
  local ref="$6"
  local protected_ref="$7"

  bash "${classifier}" \
    --repo-root "${fixture}" \
    --tag "${tag}" \
    --sha "${sha}" \
    --draft "${draft}" \
    --prerelease "${prerelease}" \
    --ref "${ref}" \
    --protected-ref "${protected_ref}"
}

fixture="${temp_root}/valid"
make_fixture "${fixture}" "1.2.3" "v1.2.3"
sha="$(git -C "${fixture}" rev-parse HEAD)"

expected_stable="$(cat <<EOF
tag=v1.2.3
release_sha=${sha}
version=1.2.3
prerelease=false
stable=true
EOF
)"
actual="$(run_classifier "${fixture}" "v1.2.3" "${sha}" false false refs/tags/v1.2.3 refs/heads/master)"
[ "${actual}" = "${expected_stable}" ] || fail "stable release output did not match"
pass "accepts a stable app release"

expected_prerelease="$(cat <<EOF
tag=v1.2.3
release_sha=${sha}
version=1.2.3
prerelease=true
stable=false
EOF
)"
actual="$(run_classifier "${fixture}" "v1.2.3" "${sha}" false true refs/tags/v1.2.3 refs/heads/master)"
[ "${actual}" = "${expected_prerelease}" ] || fail "GitHub prerelease output did not match"
pass "accepts an exact app tag marked as a GitHub prerelease"

for malformed_tag in 1.2.3 v1.2 v1.2.3-beta.1 v1.2.3+build v01.2.3 V1.2.3; do
  expect_failure "rejects malformed tag ${malformed_tag}" \
    run_classifier "${fixture}" "${malformed_tag}" "${sha}" false false "refs/tags/${malformed_tag}" refs/heads/master
done

uppercase_sha="$(printf '%s' "${sha}" | tr '[:lower:]' '[:upper:]')"
if [ "${uppercase_sha}" = "${sha}" ]; then
  uppercase_sha="A${sha:1}"
fi
expect_failure "rejects an uppercase SHA" \
  run_classifier "${fixture}" v1.2.3 "${uppercase_sha}" false false refs/tags/v1.2.3 refs/heads/master
expect_failure "rejects a short SHA" \
  run_classifier "${fixture}" v1.2.3 "${sha:0:12}" false false refs/tags/v1.2.3 refs/heads/master
expect_failure "rejects draft=true" \
  run_classifier "${fixture}" v1.2.3 "${sha}" true false refs/tags/v1.2.3 refs/heads/master
expect_failure "rejects a malformed draft boolean" \
  run_classifier "${fixture}" v1.2.3 "${sha}" no false refs/tags/v1.2.3 refs/heads/master
expect_failure "rejects a malformed prerelease boolean" \
  run_classifier "${fixture}" v1.2.3 "${sha}" false no refs/tags/v1.2.3 refs/heads/master
expect_failure "rejects a release ref that does not match the tag" \
  run_classifier "${fixture}" v1.2.3 "${sha}" false false refs/tags/v9.9.9 refs/heads/master
expect_failure "rejects a malformed protected ref" \
  run_classifier "${fixture}" v1.2.3 "${sha}" false false refs/tags/v1.2.3 'master^{commit}'

printf 'new head\n' > "${fixture}/README.md"
git -C "${fixture}" add README.md
git -C "${fixture}" commit -q -m "move head"
new_head_sha="$(git -C "${fixture}" rev-parse HEAD)"
expect_failure "rejects a tag that does not peel to the release SHA" \
  run_classifier "${fixture}" v1.2.3 "${new_head_sha}" false false refs/tags/v1.2.3 refs/heads/master
expect_failure "rejects a checked-out HEAD that does not match the release SHA" \
  run_classifier "${fixture}" v1.2.3 "${sha}" false false refs/tags/v1.2.3 refs/heads/master

non_ancestor_fixture="${temp_root}/non-ancestor"
make_fixture "${non_ancestor_fixture}" "1.2.3" "v1.2.3"
tree="$(git -C "${non_ancestor_fixture}" rev-parse 'HEAD^{tree}')"
side_sha="$(printf 'unrelated release\n' | git -C "${non_ancestor_fixture}" commit-tree "${tree}")"
git -C "${non_ancestor_fixture}" update-ref refs/tags/v1.2.3 "${side_sha}"
git -C "${non_ancestor_fixture}" checkout -q --detach "${side_sha}"
expect_failure "rejects a release SHA outside the protected branch" \
  run_classifier "${non_ancestor_fixture}" v1.2.3 "${side_sha}" false false refs/tags/v1.2.3 refs/heads/master

version_fixture="${temp_root}/version-mismatch"
make_fixture "${version_fixture}" "9.9.9" "v1.2.3"
version_sha="$(git -C "${version_fixture}" rev-parse HEAD)"
expect_failure "rejects an app manifest version mismatch" \
  run_classifier "${version_fixture}" v1.2.3 "${version_sha}" false false refs/tags/v1.2.3 refs/heads/master

cargo_fixture="${temp_root}/cargo-version-mismatch"
make_fixture "${cargo_fixture}" "1.2.3" "v1.2.3" "9.9.9"
cargo_sha="$(git -C "${cargo_fixture}" rev-parse HEAD)"
expect_failure "rejects a Cargo package version mismatch" \
  run_classifier "${cargo_fixture}" v1.2.3 "${cargo_sha}" false false refs/tags/v1.2.3 refs/heads/master

expect_failure "rejects a missing required argument" \
  bash "${classifier}" --repo-root "${fixture}"
expect_failure "rejects an unknown argument" \
  bash "${classifier}" --unknown value

release_json="${temp_root}/release.json"
pages_production_json="${temp_root}/pages-production.json"
proxy_rust_json="${temp_root}/proxy-rust.json"
proxy_publish_json="${temp_root}/proxy-publish.json"
zapstore_json="${temp_root}/zapstore-publish.json"
yq -o=json '.' "${repo_root}/.github/workflows/release.yml" > "${release_json}"
yq -o=json '.' "${repo_root}/.github/workflows/pages-production.yml" > "${pages_production_json}"
yq -o=json '.' "${repo_root}/.github/workflows/proxy-rust.yml" > "${proxy_rust_json}"
yq -o=json '.' "${repo_root}/.github/workflows/proxy-publish.yml" > "${proxy_publish_json}"
yq -o=json '.' "${repo_root}/.github/workflows/zapstore-publish.yml" > "${zapstore_json}"

if rg -n --glob '*.yml' --glob '*.yaml' \
  'gh[[:space:]]+release[[:space:]]+create|softprops/action-gh-release' \
  "${repo_root}/.github/workflows"; then
  fail "repository workflows must not create a second GitHub Release"
fi
pass "repository workflows preserve one Maple GitHub Release object"

eif_release="${repo_root}/.github/workflows/opensecret-eif-release.yml"
[ "$(yq -o=json -I=0 '.on | keys' "${eif_release}")" = '["workflow_dispatch"]' ] || \
  fail "OpenSecret EIF release must be manual dispatch only"
if rg -n 'refs/tags|git[[:space:]]+tag|gh[[:space:]]+release|latest\.json' "${eif_release}"; then
  fail "OpenSecret EIF release must never create a tag or Release or touch updater metadata"
fi
for workflow in "${repo_root}"/.github/workflows/*.yml; do
  [ "$(basename "${workflow}")" = release.yml ] && continue
  if [ "$(yq '.on | has("release")' "${workflow}")" = "true" ]; then
    fail "only release.yml may run on GitHub Release events: ${workflow}"
  fi
done
pass "OpenSecret EIF release cannot move /releases/latest or updater metadata"

[ -x "${repo_root}/scripts/ci/inspect-proxy-container-manifest.sh" ] || \
  fail "proxy container inspector must be executable"
if rg -n 'ghcr-(push|build-push|login)' "${repo_root}/proxy/justfile"; then
  fail "local proxy recipes must not retain a GHCR write path"
fi
pass "proxy container publication has one GitHub Actions writer"

python3 - "${release_json}" "${pages_production_json}" "${proxy_rust_json}" "${proxy_publish_json}" "${zapstore_json}" <<'PY'
import json
import re
import sys


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def needs(job):
    value = job.get("needs", [])
    return [value] if isinstance(value, str) else value


def secret_names(value):
    found = set()
    if isinstance(value, dict):
        for child in value.values():
            found.update(secret_names(child))
    elif isinstance(value, list):
        for child in value:
            found.update(secret_names(child))
    elif isinstance(value, str):
        dot_pattern = r"secrets\.([A-Za-z0-9_]+)"
        bracket_pattern = r"secrets\[['\"]([A-Za-z0-9_]+)['\"]\]"
        found.update(re.findall(dot_pattern, value))
        found.update(re.findall(bracket_pattern, value))
        remaining = re.sub(dot_pattern, "", value)
        remaining = re.sub(bracket_pattern, "", remaining)
        if re.search(r"\bsecrets\b", remaining):
            found.add("*")
    return found


with open(sys.argv[5], encoding="utf-8") as handle:
    zapstore = json.load(handle)
zapstore_steps = zapstore["jobs"]["publish"]["steps"]
checkout = next(step for step in zapstore_steps if str(step.get("uses", "")).startswith("actions/checkout@"))
check(
    checkout["with"]["sparse-checkout"].splitlines() == [
        "apps/maple-research/zapstore.yaml",
        "apps/maple-research/frontend/src-tauri/icons/icon.png",
    ],
    "Zapstore must checkout the Research config and its relative icon",
)
download = next(step for step in zapstore_steps if step.get("name") == "Download APK from release")
check("--dir apps/maple-research" in download["run"],
      "Zapstore APK must be downloaded beside the config for config-relative release_source")
publish = next(step for step in zapstore_steps if step.get("name") == "Publish to Zapstore")
check("apps/maple-research/zapstore.yaml 2>&1" in publish["run"],
      "Zapstore must publish with the relocated config")

with open(sys.argv[1], encoding="utf-8") as handle:
    release = json.load(handle)

with open(sys.argv[2], encoding="utf-8") as handle:
    pages_production = json.load(handle)

with open(sys.argv[3], encoding="utf-8") as handle:
    proxy_rust = json.load(handle)

with open(sys.argv[4], encoding="utf-8") as handle:
    proxy_container_publish = json.load(handle)

release_jobs = release["jobs"]
classifier_id = "classify-app-release"
check(classifier_id in release_jobs, "Release workflow must have classify-app-release job")
classifier = release_jobs[classifier_id]
classifier_permissions = classifier.get("permissions", {})
check(
    classifier_permissions == {"contents": "read"},
    "Release classifier must have only contents: read permission",
)
check(not secret_names(classifier), "Release classifier must not receive secrets")
for output in ("tag", "release_sha", "version", "prerelease", "stable"):
    check(output in classifier.get("outputs", {}), f"Release classifier must emit {output}")

android_attestation_steps = [
    step
    for step in release_jobs.get("build-android", {}).get("steps", [])
    if step.get("name") == "Attest Android release artifacts"
]
check(len(android_attestation_steps) == 1, "Android release artifacts must be attested exactly once")
check(
    android_attestation_steps[0].get("continue-on-error") is True,
    "Android attestation must remain best effort",
)

classifier_checkouts = [
    step
    for step in classifier.get("steps", [])
    if str(step.get("uses", "")).startswith("actions/checkout@")
]
check(len(classifier_checkouts) == 1, "Release classifier must have exactly one checkout")
classifier_checkout = classifier_checkouts[0].get("with", {})
check(classifier_checkout.get("ref") == "${{ github.sha }}", "Release classifier must checkout github.sha")
check(classifier_checkout.get("fetch-depth") in (0, "0"), "Release classifier must fetch complete history")
check(classifier_checkout.get("persist-credentials") is False, "Release classifier must not persist credentials")

release_ref = "${{ needs.classify-app-release.outputs.release_sha }}"
for job_id, job in release_jobs.items():
    if job_id == classifier_id:
        continue
    check(classifier_id in needs(job), f"Release job {job_id} must directly need {classifier_id}")
    for step in job.get("steps", []):
        if str(step.get("uses", "")).startswith("actions/checkout@"):
            checkout = step.get("with", {})
            check(checkout.get("ref") == release_ref, f"Release checkout in {job_id} must pin classifier SHA")
            check(checkout.get("persist-credentials") is False, f"Release checkout in {job_id} must not persist credentials")

proxy_assets = {
    "maple-proxy-linux-aarch64.tar.gz",
    "maple-proxy-linux-x86_64.tar.gz",
    "maple-proxy-macos-aarch64.tar.gz",
    "maple-proxy-windows-x86_64.zip",
}
for job_id in (
    "build-proxy",
    "publish-proxy-release-artifacts",
    "verify-proxy-release-artifacts",
):
    check(job_id in release_jobs, f"Release workflow must have {job_id} job")

proxy_build = release_jobs["build-proxy"]
proxy_matrix = proxy_build.get("strategy", {}).get("matrix", {}).get("include", [])
check(
    {entry.get("archive") for entry in proxy_matrix} == proxy_assets,
    "Proxy release matrix must build the four stable native asset names",
)
check(
    len(proxy_matrix) == len(proxy_assets),
    "Proxy release matrix must contain each native asset exactly once",
)
check(
    proxy_build.get("permissions") == {"contents": "read"},
    "Proxy native builds must have only contents: read permission",
)

proxy_rehearsal = proxy_rust.get("jobs", {}).get("proxy-native-release", {})
proxy_rehearsal_matrix = (
    proxy_rehearsal.get("strategy", {}).get("matrix", {}).get("include", [])
)
check(
    proxy_rehearsal_matrix == proxy_matrix,
    "Proxy PR rehearsal and release matrices must remain identical",
)
proxy_rehearsal_runs = "\n".join(
    str(step.get("run", "")) for step in proxy_rehearsal.get("steps", [])
)
check(
    'proxy-release.sh proxy-release-rehearsal "${{ matrix.archive }}"'
    in proxy_rehearsal_runs,
    "Proxy PR rehearsal must run the release packaging script",
)

proxy_publish = release_jobs["publish-proxy-release-artifacts"]
check(
    set(needs(proxy_publish)) == {classifier_id, "build-proxy"},
    "Proxy publisher must wait for classification and every native proxy build",
)
check(
    proxy_publish.get("permissions")
    == {
        "contents": "write",
        "id-token": "write",
        "attestations": "write",
        "artifact-metadata": "write",
    },
    "Proxy publisher must have only release-upload and attestation permissions",
)
proxy_attest_steps = [
    step
    for step in proxy_publish.get("steps", [])
    if step.get("name") == "Attest proxy release assets"
]
check(len(proxy_attest_steps) == 1, "Proxy release assets must be attested exactly once")
check(
    proxy_attest_steps[0].get("continue-on-error") is not True,
    "Proxy release attestation must fail closed",
)
proxy_upload_steps = [
    step
    for step in proxy_publish.get("steps", [])
    if step.get("name") == "Upload proxy assets to the Maple release"
]
check(len(proxy_upload_steps) == 1, "Proxy assets must upload exactly once")
proxy_upload_run = str(proxy_upload_steps[0].get("run", ""))
check(
    'gh release upload "${RELEASE_TAG}"' in proxy_upload_run,
    "Proxy publisher must upload to the classified Maple release tag",
)
for asset in proxy_assets | {"maple-proxy-release-final.sha256"}:
    check(asset in proxy_upload_run, f"Proxy publisher must upload {asset}")

proxy_verify = release_jobs["verify-proxy-release-artifacts"]
check(
    set(needs(proxy_verify))
    == {classifier_id, "publish-proxy-release-artifacts"},
    "Published proxy verification must wait for the proxy publisher",
)

latest_needs = set(needs(release_jobs["update-latest-json"]))
check(
    not latest_needs.intersection(
        {"build-proxy", "publish-proxy-release-artifacts", "verify-proxy-release-artifacts"}
    ),
    "latest.json publication must remain independent of proxy release jobs",
)

aggregate = release_jobs["verify-release-artifacts"]
check(
    "verify-proxy-release-artifacts" in needs(aggregate),
    "Aggregate release verification must wait for published proxy verification",
)
aggregate_runs = "\n".join(str(step.get("run", "")) for step in aggregate.get("steps", []))
check(
    "verify-release-artifacts.sh artifacts macos windows ios web latest-json proxy"
    in aggregate_runs,
    "Aggregate release verification must include proxy assets",
)

check(
    pages_production.get("permissions") == {"contents": "read"},
    "Pages production workflow must default to contents: read",
)
pages_on = pages_production.get("on", {})
check("workflow_dispatch" in pages_on, "Pages production workflow must support manual retry")
pages_workflow_run = pages_on.get("workflow_run", {})
check(
    pages_workflow_run.get("workflows") == ["Release"]
    and pages_workflow_run.get("types") == ["completed"],
    "Pages production workflow must follow completed Release workflows",
)
check(
    pages_production.get("concurrency")
    == {"group": "pages-production", "cancel-in-progress": False},
    "Pages production workflow must serialize promotions without cancellation",
)

pages_jobs = pages_production.get("jobs", {})
check(set(pages_jobs) == {"promote"}, "Pages production workflow must have one promotion job")
pages_job = pages_jobs["promote"]
check(
    pages_job.get("permissions") == {"contents": "write"},
    "Pages production promotion must have only contents: write permission",
)
check(not secret_names(pages_production), "Pages production workflow must not receive secrets")
check("environment" not in pages_job, "Pages production promotion must not require a secret-bearing environment")

pages_if = str(pages_job.get("if", ""))
for required_gate in (
    "vars.MAPLE_PAGES_PRODUCTION_ENABLED != 'true'",
    "workflow_run.conclusion == 'success'",
    "workflow_run.event == 'release'",
    "workflow_run.path == '.github/workflows/release.yml'",
    "workflow_run.head_repository.full_name == github.repository",
):
    check(required_gate in pages_if, f"Pages production job is missing gate: {required_gate}")

pages_steps = pages_job.get("steps", [])
check(len(pages_steps) == 1, "Pages production promotion must have one non-checkout step")
pages_step = pages_steps[0]
check("uses" not in pages_step, "Pages production promotion must not run a third-party action")
pages_env = pages_step.get("env", {})
check(
    pages_env.get("PAGES_PRODUCTION_BRANCH") == "pages-production",
    "Pages production branch must be hardcoded",
)
check(pages_env.get("GH_TOKEN") == "${{ github.token }}", "Pages promotion must use github.token")

pages_run = str(pages_step.get("run", ""))
for required_control in (
    "repos/${REPOSITORY}/releases/latest",
    "repos/${REPOSITORY}/compare/${release_sha}...master",
    "repos/${REPOSITORY}/compare/${current_sha}...${release_sha}",
    "repos/${REPOSITORY}/git/refs/heads/${PAGES_PRODUCTION_BRANCH}",
    '-F force=false',
):
    check(required_control in pages_run, f"Pages production step is missing control: {required_control}")

check(
    proxy_container_publish.get("permissions") == {"contents": "read"},
    "Proxy container publisher must default to contents: read",
)
check(
    not secret_names(proxy_container_publish),
    "Proxy container publisher must use no repository or environment secrets",
)
proxy_publish_on = proxy_container_publish.get("on", {})
check("workflow_dispatch" in proxy_publish_on, "Proxy container publisher must support manual retry")
diagnostic_input = proxy_publish_on.get("workflow_dispatch", {}).get("inputs", {}).get("diagnostics_only", {})
check(diagnostic_input.get("type") == "boolean" and diagnostic_input.get("default") is False,
      "Proxy diagnostics must be an explicit boolean opt-in")
proxy_publish_workflow_run = proxy_publish_on.get("workflow_run", {})
check(
    proxy_publish_workflow_run.get("workflows") == ["Release"]
    and proxy_publish_workflow_run.get("types") == ["completed"],
    "Proxy container publisher must follow completed Release workflows",
)
check(
    proxy_container_publish.get("concurrency")
    == {"group": "proxy-publishing", "queue": "max", "cancel-in-progress": False},
    "Proxy container publication must serialize without dropping pending releases",
)
check(
    proxy_container_publish.get("env")
    == {
        "REGISTRY": "ghcr.io",
        "IMAGE_NAME": "mapleprivacylabs/maple-proxy",
        "UNBACKFILLED_PROXY_BASELINE": "0.3.3",
    },
    "Proxy container publisher must use the canonical MaplePrivacyLabs GHCR package",
)

container_jobs = proxy_container_publish.get("jobs", {})
check(
    set(container_jobs) == {"diagnose", "prepare", "build", "publish", "finalize"},
    "Proxy container publisher must separate validation, builds, exact publication, and alias reconciliation",
)
diagnose = container_jobs["diagnose"]
check(diagnose.get("permissions") == {"contents": "read", "packages": "read"},
      "Proxy diagnostics must have read-only permissions")
check(not needs(diagnose), "Proxy diagnostics must not depend on publication jobs")
for required_gate in (
    "github.repository == 'MaplePrivacyLabs/Maple'",
    "github.repository_id == '923138240'",
    "github.repository_owner_id == '322649754'",
    "github.ref_type == 'branch'",
    "github.event_name == 'workflow_dispatch' && inputs.diagnostics_only",
):
    check(required_gate in str(diagnose.get("if", "")), f"Proxy diagnostics is missing gate: {required_gate}")
diagnostic_runs = [step.get("run") for step in diagnose.get("steps", []) if "run" in step]
check(diagnostic_runs == ["python3 -I scripts/ci/proxy_registry_inventory.py --diagnose"],
      "Proxy diagnostics must only inspect package API access")
for job_name, job in container_jobs.items():
    check("diagnose" not in needs(job), f"Publication job {job_name} must not depend on diagnostics")
prepare = container_jobs["prepare"]
prepare_if = str(prepare.get("if", ""))
for required_gate in (
    "github.repository == 'MaplePrivacyLabs/Maple'",
    "github.repository_id == '923138240'",
    "github.repository_owner_id == '322649754'",
    "github.ref == 'refs/heads/master'",
    "!(github.event_name == 'workflow_dispatch' && inputs.diagnostics_only)",
    "workflow_run.conclusion == 'success'",
    "workflow_run.event == 'release'",
    "workflow_run.path == '.github/workflows/release.yml'",
    "workflow_run.head_repository.full_name == github.repository",
):
    check(required_gate in prepare_if, f"Proxy container publisher is missing gate: {required_gate}")
check(
    prepare.get("permissions") == {"contents": "read", "packages": "read"},
    "Proxy preparation must read package metadata without package write permission",
)
prepare_runs = "\n".join(str(step.get("run", "")) for step in prepare.get("steps", []))
for required_control in (
    "repos/${REPOSITORY}/releases/latest",
    "repos/${REPOSITORY}/releases?per_page=100",
    'git show "${sha}:proxy/Cargo.toml"',
    "compare/${release_sha}...master",
    "proxy container runtime inputs changed without a proxy version bump",
    '"${UNBACKFILLED_PROXY_BASELINE}"',
    "plan-proxy-container-publish.sh",
    "python3 -I scripts/ci/proxy_runtime_diff.py",
    "python3 -I scripts/ci/proxy_registry_inventory.py",
):
    check(required_control in prepare_runs, f"Proxy publication plan is missing control: {required_control}")
prepare_checkouts = [
    step for step in prepare.get("steps", []) if str(step.get("uses", "")).startswith("actions/checkout@")
]
check(len(prepare_checkouts) == 1, "Proxy plan must have one trusted checkout")
check(
    prepare_checkouts[0].get("with", {}).get("fetch-depth") == 0,
    "Proxy plan must fetch history for release and runtime-input comparisons",
)

container_build = container_jobs["build"]
check(needs(container_build) == ["prepare"], "Proxy container builds must need the validated plan")
check(
    container_build.get("if") == "needs.prepare.outputs.publish == 'true'",
    "Proxy container builds must skip unchanged versions",
)
check(
    container_build.get("permissions") == {"contents": "read", "packages": "write"},
    "Proxy container builds must have only contents read and packages write",
)
container_matrix = container_build.get("strategy", {}).get("matrix", {}).get("include", [])
check(
    {entry.get("platform") for entry in container_matrix} == {"linux/amd64", "linux/arm64"},
    "Proxy container publisher must build native AMD64 and ARM64 images",
)
for step in container_build.get("steps", []):
    if str(step.get("uses", "")).startswith("actions/checkout@"):
        checkout = step.get("with", {})
        check(
            checkout.get("ref") == "${{ needs.prepare.outputs.release_sha }}",
            "Proxy container builds must checkout the validated release SHA",
        )
        check(
            checkout.get("persist-credentials") is False,
            "Proxy container checkout must not persist credentials",
        )
build_steps = container_build.get("steps", [])
build_push = [step for step in build_steps if step.get("name") == "Build and push platform image by digest"]
check(len(build_push) == 1, "Proxy container platforms must push exactly once by digest")
build_with = build_push[0].get("with", {})
check(build_with.get("file") == "proxy/Dockerfile", "Proxy container publisher must use proxy/Dockerfile")
check(
    "push-by-digest=true" in str(build_with.get("outputs", "")),
    "Proxy container platforms must publish only by digest before the manifest",
)
check(
    build_with.get("provenance") == "mode=max" and build_with.get("sbom") is False,
    "Proxy container platforms must publish max-mode provenance without an SBOM",
)

container_publish = container_jobs["publish"]
check(
    set(needs(container_publish)) == {"prepare", "build"},
    "Proxy manifest publisher must wait for the plan and both platform builds",
)
check(
    container_publish.get("if") == "needs.prepare.outputs.publish == 'true'",
    "Proxy exact manifest publisher must run only for a required build",
)
check(
    container_publish.get("permissions") == {"contents": "read", "packages": "write"},
    "Proxy manifest publisher must have only contents read and packages write",
)
publish_runs = "\n".join(str(step.get("run", "")) for step in container_publish.get("steps", []))
check(
    '--tag "${image}:${PROXY_VERSION}"' in publish_runs,
    "Proxy exact manifest publisher must tag the immutable version",
)
for forbidden_alias in ("PROXY_MINOR", "PROXY_MAJOR", 'image}:latest'):
    check(
        forbidden_alias not in publish_runs,
        f"Proxy exact manifest publisher must not update mutable alias {forbidden_alias}",
    )

container_finalize = container_jobs["finalize"]
check(
    set(needs(container_finalize)) == {"prepare", "build", "publish"},
    "Proxy finalizer must observe both the optional build and exact publication",
)
finalize_if = str(container_finalize.get("if", ""))
for required_gate in (
    "always()",
    "needs.prepare.outputs.publish == 'true'",
    "needs.prepare.outputs.reconcile == 'true'",
    "needs.publish.result == 'success'",
):
    check(required_gate in finalize_if, f"Proxy finalizer is missing gate: {required_gate}")
check(
    container_finalize.get("permissions") == {"contents": "read", "packages": "write"},
    "Proxy finalizer must have only contents read and packages write",
)
finalize_runs = "\n".join(str(step.get("run", "")) for step in container_finalize.get("steps", []))
for required_control in (
    "inspect-proxy-container-manifest.sh",
    "python3 -I scripts/ci/proxy_registry_inventory.py --require-public",
    "published exact tag does not match the current proxy runtime inputs",
    "python3 -I scripts/ci/proxy_runtime_diff.py",
    'for tag in "${PROXY_MINOR}" "${PROXY_MAJOR}" latest',
    "docker buildx imagetools create",
    "per-platform SLSA provenance",
):
    check(required_control in finalize_runs, f"Proxy finalizer is missing control: {required_control}")
check(
    any(
        step.get("if") == "needs.prepare.outputs.publish == 'true'"
        and str(step.get("uses", "")).startswith("actions/download-artifact@")
        for step in container_finalize.get("steps", [])
    ),
    "Proxy finalizer must compare a new exact manifest with both build outputs",
)

PY
pass "workflow release-gate topology is fail closed with isolated downstream publishers"

bash "${script_dir}/test-windows-runtime-manifest.sh" >/dev/null
pass "Windows runtime proofs accept coherent retained and relocated paths"

bash "${script_dir}/test-proxy-release-artifacts.sh" >/dev/null
pass "proxy release artifact verifier accepts only the complete native asset set"

bash "${script_dir}/test-plan-proxy-container-publish.sh" >/dev/null
pass "proxy container publish planner preserves immutable versions and recovery"

python3 "${script_dir}/test_proxy_registry_inventory.py" >/dev/null
pass "proxy registry inventory distinguishes initial creation from failed access"

python3 "${script_dir}/test_proxy_runtime_diff.py" >/dev/null
pass "proxy runtime comparisons follow the SDK selected at both revisions"

printf '1..%d\n' "${passed}"
