#!/usr/bin/env bash
# Reviewer-gated CI signing entrypoint. The workflow places the existing key in
# SIGNING_PRIVATE_KEY for this step only; the operator recipe below hands it to
# the node signer alone and verifies the result against the pinned public key.
# Never builds, publishes, or deploys.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "Usage: ci_sign_pcr.sh dev|prod ARTIFACT_DIR" >&2
  exit 2
fi
case "$1" in
  dev) snapshot=pcrDev.json ;;
  prod) snapshot=pcrProd.json ;;
  *) echo "Expected dev or prod." >&2; exit 2 ;;
esac
mode=$1
: "${SIGNING_PRIVATE_KEY:?Run this only from the gated signing step}"

component=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
artifact=$(cd -- "$2" && pwd -P)
cd "$component"

# The candidate must be byte-identical to what the build job attested.
(cd "$artifact" && sha256sum --check --strict --quiet SHA256SUMS)
if [[ -n "${EXPECTED_EIF_SHA256:-}" ]]; then
  [[ "$(sha256sum -- "$artifact/image.eif" | cut -d' ' -f1)" == "$EXPECTED_EIF_SHA256" ]] || {
    echo "EIF SHA-256 differs from the build job's handoff." >&2
    exit 1
  }
fi
[[ "$(jq -r .environment "$artifact/handoff.json")" == "$mode" ]] || {
  echo "Artifact was built for a different environment." >&2
  exit 1
}

# An older build may add history but must leave the current snapshot alone.
# Docs and approval edits do not make the authenticated build older.
source_sha=$(jq -er .source_sha "$artifact/handoff.json")
checkout_sha=$(git rev-parse HEAD)
if ! git merge-base --is-ancestor "$source_sha" "$checkout_sha"; then
  echo "Artifact source is outside this checkout's history." >&2
  exit 1
fi
eif_input_changes=$(git diff --name-only --no-renames -z "$source_sha" "$checkout_sha" |
  python3 ../../scripts/ci/opensecret_change_detection.py --eif-input-paths)
candidate_is_current=$(jq -r 'length == 0' <<< "$eif_input_changes")
if [[ "$candidate_is_current" == true ]]; then
  classification="Current build: the EIF inputs match this checkout; updating $snapshot."
else
  changed_paths=$(jq -r '.[] | @json' <<< "$eif_input_changes")
  classification=$(printf 'Older build: leaving %s unchanged; approving history only.\nEIF input differences:\n%s' \
    "$snapshot" "$changed_paths")
fi
printf '%s\n' "$classification"
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  printf '## Legacy approval (%s)\n\n%s\n\n' "$mode" "$classification" >> "$GITHUB_STEP_SUMMARY"
fi
# The recipe validates the files, skips already approved measurements before
# any signing, signs PCR0 only inside the node child, verifies the signature,
# and appends atomically. Everything else runs under its credential-free env.
history="${snapshot%.json}History.json"
just --no-dotenv --set pcr_signer 'node pcr_sign.js sign-pcr0' _append-pcr-file "$artifact/pcr.json" "$history"
if [[ "$candidate_is_current" == true ]]; then
  cp -- "$artifact/pcr.json" "./$snapshot"
fi
unset SIGNING_PRIVATE_KEY
python3 scripts/pcr_compatibility.py check .
