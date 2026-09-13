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

cp -- "$artifact/pcr.json" "./$snapshot"
# The recipe validates the files, skips already approved measurements before
# any signing, signs PCR0 only inside the node child, verifies the signature,
# and appends atomically. Everything else runs under its credential-free env.
just --no-dotenv --set pcr_signer 'node pcr_sign.js sign-pcr0' "append-pcr-$mode"
unset SIGNING_PRIVATE_KEY
python3 scripts/pcr_compatibility.py check .
