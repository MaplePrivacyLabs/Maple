#!/usr/bin/env bash
# Commit the approval files changed by the signing step to the review branch
# for this build commit and checkout. Runs that approve the same build from the
# same checkout share that branch, so one pull request carries both
# environments' approvals; a later run lands its commit on top of the earlier
# one. Never writes master, creates a tag, or touches GitHub Releases.
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
checkout_sha=$(git rev-parse HEAD)
# A signing workflow may approve an earlier verified master build from a newer
# master checkout; it names that build's commit. It must be a full commit that
# this checkout already contains.
source_sha=${EXPECTED_SOURCE_SHA:-$checkout_sha}
if [[ ! "$source_sha" =~ ^[0-9a-f]{40}$ ]] || ! git merge-base --is-ancestor "$source_sha" HEAD 2>/dev/null; then
  echo "Approval source must be a full commit contained in this checkout." >&2
  exit 1
fi
[[ "$(jq -r .source_sha "$handoff")" == "$source_sha" ]] || {
  echo "Handoff was built from a different commit than the selected build commit." >&2
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
# Approving from a newer checkout gives the branch its own base suffix, so it
# never replays unrelated source changes onto the build commit's branch.
if [[ "$source_sha" != "$checkout_sha" ]]; then
  branch+="-${checkout_sha:0:12}"
fi
# Same mechanism actions/checkout uses; the token never appears in a URL.
authorization=$(printf 'x-access-token:%s' "$GITHUB_TOKEN" | base64 | tr -d '\n')
git_auth() {
  git -c "http.https://github.com/.extraheader=AUTHORIZATION: basic $authorization" "$@"
}
git_bot() {
  git -c user.name='github-actions[bot]' \
      -c user.email='41898282+github-actions[bot]@users.noreply.github.com' "$@"
}

branch_approves_candidate() {
  local tip=$1
  local baseline=$2
  local handoff=$3
  local history=$4
  local approved
  approved=$(mktemp -d)
  mkdir -- "$approved/baseline"
  for file in pcrDev.json pcrProd.json pcrDevHistory.json pcrProdHistory.json; do
    git show "$tip:services/opensecret/$file" > "$approved/$file"
    git show "$baseline:services/opensecret/$file" > "$approved/baseline/$file"
  done
  # A pending branch may already approve this build. Verify its signatures and
  # preservation of this checkout's history before accepting it on a rerun.
  local valid=false
  if python3 services/opensecret/scripts/pcr_compatibility.py check "$approved" \
    --baseline-dir "$approved/baseline" > /dev/null; then
    valid=$(jq --slurpfile handoff "$handoff" \
      'any(.[]; .PCR0 == $handoff[0].measurements.PCR0 and
                 .PCR1 == $handoff[0].measurements.PCR1 and
                 .PCR2 == $handoff[0].measurements.PCR2)' "$approved/${history##*/}")
  fi
  rm -rf -- "$approved"
  [[ "$valid" == true ]]
}

pcr0=$(jq -r .measurements.PCR0 "$handoff")
eif_sha256=$(jq -r .eif_sha256 "$handoff")
git add -- "$snapshot" "$history"
git_bot commit --quiet \
    -m "Approve ${mode} OpenSecret EIF measurements from ${source_sha:0:12}" \
    -m "Source commit: ${source_sha}" \
    -m "PCR0: ${pcr0}" \
    -m "EIF SHA-256: ${eif_sha256}" \
    -m "Signed by the reviewer-gated ${GITHUB_WORKFLOW:-OpenSecret EIF release} workflow run ${GITHUB_RUN_ID:-local}."

# Land this commit on top of whatever the branch already holds for this build
# commit and checkout. A push loses the race only to the other environment's
# run, which touches different files, so refetch and replay once and try again.
approval_ready=false
reused=false
approval_tip=HEAD
for attempt in 1 2 3 4 5; do
  if git_auth fetch --quiet origin "refs/heads/$branch:refs/remotes/origin/$branch" 2>/dev/null; then
    tip=$(git rev-parse "refs/remotes/origin/$branch")
    git merge-base --is-ancestor "$checkout_sha" "$tip" || {
      echo "Branch $branch does not descend from $checkout_sha; inspect it before rerunning." >&2
      exit 1
    }
    own_approval=false
    while read -r path; do
      [[ -n "$path" ]] || continue
      case "$path" in
        "$snapshot" | "$history")
          own_approval=true
          ;;
        services/opensecret/pcrDev.json | services/opensecret/pcrDevHistory.json | \
        services/opensecret/pcrProd.json | services/opensecret/pcrProdHistory.json) ;;
        *) echo "Branch $branch changes more than approval files: $path" >&2; exit 1 ;;
      esac
    done <<< "$(git diff --name-only "$checkout_sha" "$tip")"
    if [[ "$own_approval" == true ]]; then
      if ! branch_approves_candidate "$tip" "$checkout_sha" "$handoff" "$history"; then
        echo "Branch $branch already carries the $mode approval but does not verify for this artifact." >&2
        exit 1
      fi
      approval_ready=true
      reused=true
      approval_tip=$tip
      break
    fi
    if [[ "$(git rev-parse HEAD~1)" != "$tip" ]]; then
      git_bot rebase --quiet "$tip" >/dev/null
    fi
  fi
  if git_auth push --quiet origin "HEAD:refs/heads/$branch" 2>/dev/null; then
    approval_ready=true
    break
  fi
  echo "Push of $branch was rejected (attempt $attempt); refetching." >&2
  sleep 2
done
[[ "$approval_ready" == true ]] || {
  echo "Could not push $branch after five attempts; the approval commit is at $(git rev-parse HEAD) in this checkout." >&2
  exit 1
}

updates_snapshot=false
if ! git diff --quiet "$checkout_sha" "$approval_tip" -- "$snapshot"; then
  updates_snapshot=true
fi
compare="https://github.com/${GITHUB_REPOSITORY}/compare/master...${branch}?expand=1"
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  {
    echo "## Approval branch ($mode)"
    echo
    echo "Open the pull request: $compare"
    echo
    echo "The branch is shared by every environment approved for this build commit from"
    echo "the same checkout; an existing pull request for it simply gains this approval."
    echo
    if [[ "$updates_snapshot" == true ]]; then
      echo "After it merges, hand deployment the merge commit as \`OPENSECRET_SOURCE_REF\`,"
      echo "the artifact directory added to the Nix store as \`OPENSECRET_EIF_DIR\`, and"
      echo "\`OPENSECRET_EIF_SHA256=${eif_sha256}\`."
    else
      echo "History-only approval: the current snapshot is unchanged."
    fi
  } >> "$GITHUB_STEP_SUMMARY"
fi
if [[ "$reused" == true ]]; then
  echo "Branch $branch already approves this artifact; keeping its existing history."
else
  echo "Pushed $branch; open the pull request at $compare"
fi
if [[ "$updates_snapshot" == false ]]; then
  echo "History-only approval: the current snapshot is unchanged."
fi
