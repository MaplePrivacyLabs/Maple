#!/usr/bin/env bash
# Commit the approval files changed by the signing step to the review branch
# for this source commit. Both environments' runs share that branch, so one
# pull request carries every approval for the commit; a later run lands its
# commit on top of the earlier one. Never writes master, creates a tag, or
# touches GitHub Releases.
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

branch="opensecret/pcr-approval-${source_sha:0:12}"
# Same mechanism actions/checkout uses; the token never appears in a URL.
authorization=$(printf 'x-access-token:%s' "$GITHUB_TOKEN" | base64 | tr -d '\n')
git_auth() {
  git -c "http.https://github.com/.extraheader=AUTHORIZATION: basic $authorization" "$@"
}
git_bot() {
  git -c user.name='github-actions[bot]' \
      -c user.email='41898282+github-actions[bot]@users.noreply.github.com' "$@"
}

pcr0=$(jq -r .measurements.PCR0 "$handoff")
eif_sha256=$(jq -r .eif_sha256 "$handoff")
git add -- "$snapshot" "$history"
git_bot commit --quiet \
    -m "Approve ${mode} OpenSecret EIF measurements from ${source_sha:0:12}" \
    -m "Source commit: ${source_sha}" \
    -m "PCR0: ${pcr0}" \
    -m "EIF SHA-256: ${eif_sha256}" \
    -m "Signed by the OpenSecret EIF release workflow run ${GITHUB_RUN_ID:-local}."

# Land this commit on top of whatever the branch already holds for this source
# commit. A push loses the race only to the other environment's run, which
# touches different files, so refetch and replay once and try again.
pushed=0
for attempt in 1 2 3 4 5; do
  if git_auth fetch --quiet origin "refs/heads/$branch:refs/remotes/origin/$branch" 2>/dev/null; then
    tip=$(git rev-parse "refs/remotes/origin/$branch")
    git merge-base --is-ancestor "$source_sha" "$tip" || {
      echo "Branch $branch does not descend from $source_sha; inspect it before rerunning." >&2
      exit 1
    }
    while read -r path; do
      [[ -n "$path" ]] || continue
      case "$path" in
        "$snapshot" | "$history")
          echo "Branch $branch already carries the $mode approval; merge or delete it before rerunning." >&2
          exit 1 ;;
        services/opensecret/pcrDev.json | services/opensecret/pcrDevHistory.json | \
        services/opensecret/pcrProd.json | services/opensecret/pcrProdHistory.json) ;;
        *) echo "Branch $branch changes more than approval files: $path" >&2; exit 1 ;;
      esac
    done <<< "$(git diff --name-only "$source_sha" "$tip")"
    if [[ "$(git rev-parse HEAD~1)" != "$tip" ]]; then
      git_bot rebase --quiet "$tip" >/dev/null
    fi
  fi
  if git_auth push --quiet origin "HEAD:refs/heads/$branch" 2>/dev/null; then
    pushed=1
    break
  fi
  echo "Push of $branch was rejected (attempt $attempt); refetching." >&2
  sleep 2
done
[[ "$pushed" == 1 ]] || {
  echo "Could not push $branch after five attempts; the approval commit is at $(git rev-parse HEAD) in this checkout." >&2
  exit 1
}

compare="https://github.com/${GITHUB_REPOSITORY}/compare/master...${branch}?expand=1"
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  {
    echo "## Approval branch pushed ($mode)"
    echo
    echo "Open the pull request: $compare"
    echo
    echo "The branch is shared by every environment approved from this source commit;"
    echo "an existing pull request for it simply gains this approval."
    echo
    echo "After it merges, hand deployment the merge commit as \`OPENSECRET_SOURCE_REF\`,"
    echo "the artifact directory added to the Nix store as \`OPENSECRET_EIF_DIR\`, and"
    echo "\`OPENSECRET_EIF_SHA256=${eif_sha256}\`. Mirror the four PCR files to the legacy"
    echo "repository with the manual compatibility procedure."
  } >> "$GITHUB_STEP_SUMMARY"
fi
echo "Pushed $branch; open the pull request at $compare"
