#!/usr/bin/env bash
# Build one EIF candidate into a directory of regular files and report whether
# the approved measurements already cover it. Never copies approvals, signs,
# or publishes; the reviewer-gated signing job consumes this output.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "Usage: build_opensecret_eif.sh dev|prod OUTPUT_DIR" >&2
  exit 2
fi
case "$1" in
  dev) reference=pcrDev.json ;;
  prod) reference=pcrProd.json ;;
  *) echo "Expected dev or prod." >&2; exit 2 ;;
esac
mode=$1

if [[ "$(uname -s)" != Linux || "$(uname -m)" != aarch64 ]]; then
  echo "EIF release builds require a Linux ARM64 runner." >&2
  exit 1
fi

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
mkdir -p -- "$2"
output_dir=$(cd -- "$2" && pwd -P)
if [[ -n "$(ls -A -- "$output_dir")" ]]; then
  echo "Output directory must be empty: $output_dir" >&2
  exit 1
fi
cd "$repo_root/services/opensecret"
if [[ ! -f "$reference" || ! -s "$reference" || -L "$reference" ]]; then
  echo "Missing regular approved measurement file: $reference" >&2
  exit 1
fi

# Direct Nix invocation avoids just's dotenv loader and development shell hooks.
# A fresh temporary link never replaces an operator's existing result symlink.
link_dir=$(mktemp -d "${TMPDIR:-/tmp}/opensecret-eif-release-${mode}.XXXXXX")
trap 'rm -rf -- "$link_dir"' EXIT
nix build --no-update-lock-file --print-build-logs \
  --out-link "$link_dir/result" ".?submodules=1#eif-$mode"

for name in image.eif pcr.json; do
  if [[ ! -f "$link_dir/result/$name" || ! -s "$link_dir/result/$name" ]]; then
    echo "EIF build did not produce $name." >&2
    exit 1
  fi
  cp -- "$link_dir/result/$name" "$output_dir/$name"
  chmod 0644 -- "$output_dir/$name"
done

jq -e '
  type == "object" and (.HashAlgorithm | type == "string") and
  all(.PCR0, .PCR1, .PCR2; type == "string" and test("^[0-9a-f]{96}$") and . != ("0" * 96))
' "$output_dir/pcr.json" >/dev/null || {
  echo "EIF build produced malformed measurements." >&2
  exit 1
}

(cd "$output_dir" && sha256sum image.eif pcr.json > SHA256SUMS)
eif_sha256=$(sha256sum -- "$output_dir/image.eif" | cut -d' ' -f1)
pcr0=$(jq -r .PCR0 "$output_dir/pcr.json")
if cmp -s -- "$reference" "$output_dir/pcr.json"; then
  approval_needed=false
else
  approval_needed=true
fi
source_sha=${GITHUB_SHA:-$(git rev-parse HEAD)}
artifact="opensecret-eif-${mode}-${source_sha:0:12}"
jq -n --arg environment "$mode" --arg source_sha "$source_sha" --arg eif_sha256 "$eif_sha256" \
  --argjson approval_needed "$approval_needed" --slurpfile measurements "$output_dir/pcr.json" \
  '{environment: $environment, source_sha: $source_sha, measurements: $measurements[0],
    eif_sha256: $eif_sha256, approval_needed: $approval_needed}' > "$output_dir/handoff.json"

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  printf 'approval_needed=%s\npcr0=%s\neif_sha256=%s\nartifact=%s\n' \
    "$approval_needed" "$pcr0" "$eif_sha256" "$artifact" >> "$GITHUB_OUTPUT"
fi
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  {
    echo "## OpenSecret EIF candidate ($mode)"
    echo
    echo "| Field | Value |"
    echo "| --- | --- |"
    echo "| Source commit | \`$source_sha\` |"
    echo "| PCR0 | \`$pcr0\` |"
    echo "| EIF SHA-256 | \`$eif_sha256\` |"
    echo "| Artifact | \`$artifact\` |"
    echo "| Approval needed | $approval_needed |"
  } >> "$GITHUB_STEP_SUMMARY"
fi
echo "EIF candidate ($mode): PCR0 $pcr0, SHA-256 $eif_sha256, approval needed: $approval_needed."
