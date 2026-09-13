#!/usr/bin/env bash
# Commit the approval files changed by the signing step to a review branch.
# Never writes master, creates a tag, or touches GitHub Releases.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "Usage: publish_opensecret_approval.sh dev|prod ARTIFACT_DIR" >&2
  exit 2
fi
case "$1" in
  dev) environment=Dev ;;
  prod) environment=Prod ;;
  *) echo "Expected dev or prod." >&2; exit 2 ;;
esac
mode=$1
: "${GITHUB_TOKEN:?Provide the job token}"
: "${GITHUB_REPOSITORY:?Provide the owner/name repository slug}"

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
handoff=$(cd -- "$2" && pwd -P)/handoff.json
cd "$repo_root"
source_sha=$(git rev-parse HEAD)
[[ "$(jq -r .source_sha "$handoff")" == "$source_sha" ]] || {
  echo "Handoff was built from a different commit than this checkout." >&2
  exit 1
}
[[ "$(jq -r .environment "$handoff")" == "$mode" ]] || {
  echo "Handoff was built for a different environment." >&2
  exit 1
}

snapshot="services/opensecret/pcr${environment}.json"
history="services/opensecret/pcr${environment}History.json"
changed=$(git status --porcelain --untracked-files=all -- services/opensecret)
if [[ -z "$changed" ]]; then
  echo "Approvals already cover this build; nothing to publish."
  exit 0
fi
while read -r _ path; do
  case "$path" in
    "$snapshot" | "$history") ;;
    *) echo "Refusing to publish an unexpected change: $path" >&2; exit 1 ;;
  esac
done <<< "$changed"

branch="opensecret/pcr-approval-${mode}-${source_sha:0:12}"
# Same mechanism actions/checkout uses; the token never appears in a URL.
authorization=$(printf 'x-access-token:%s' "$GITHUB_TOKEN" | base64 | tr -d '\n')
git_auth() {
  git -c "http.https://github.com/.extraheader=AUTHORIZATION: basic $authorization" "$@"
}
if git_auth ls-remote --exit-code --heads origin "refs/heads/$branch" >/dev/null 2>&1; then
  echo "Branch $branch already exists; merge or delete it before rerunning." >&2
  exit 1
fi

pcr0=$(jq -r .measurements.PCR0 "$handoff")
eif_sha256=$(jq -r .eif_sha256 "$handoff")
git add -- "$snapshot" "$history"
git -c user.name='github-actions[bot]' \
    -c user.email='41898282+github-actions[bot]@users.noreply.github.com' \
    commit --quiet \
    -m "Approve ${mode} OpenSecret EIF measurements from ${source_sha:0:12}" \
    -m "Source commit: ${source_sha}" \
    -m "PCR0: ${pcr0}" \
    -m "EIF SHA-256: ${eif_sha256}" \
    -m "Signed by the OpenSecret EIF release workflow run ${GITHUB_RUN_ID:-local}."
git_auth push --quiet origin "HEAD:refs/heads/$branch"

compare="https://github.com/${GITHUB_REPOSITORY}/compare/master...${branch}?expand=1"
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  {
    echo "## Approval branch pushed ($mode)"
    echo
    echo "Open the pull request: $compare"
    echo
    echo "After it merges, hand deployment the merge commit as \`OPENSECRET_SOURCE_REF\`,"
    echo "the artifact directory added to the Nix store as \`OPENSECRET_EIF_DIR\`, and"
    echo "\`OPENSECRET_EIF_SHA256=${eif_sha256}\`. Mirror the four PCR files to the legacy"
    echo "repository with the manual compatibility procedure."
  } >> "$GITHUB_STEP_SUMMARY"
fi
echo "Pushed $branch; open the pull request at $compare"
