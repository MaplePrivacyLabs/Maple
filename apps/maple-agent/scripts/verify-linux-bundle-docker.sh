#!/usr/bin/env bash
# Start a Linux bundle in stock Ubuntu containers with no Nix and no network,
# and check its embedded build identity against the package's build-info.json.
# Install no extra libraries: the bundle must supply its application dependencies
# on both the oldest supported glibc and a newer distribution. Graphics drivers
# and a desktop session are separate from this CLI/host lifecycle check.
# Usage: verify-linux-bundle-docker.sh PROFILE PACKAGE_DIRECTORY
set -euo pipefail
profile="$1"
package="$(cd "$2" && pwd)"
scripts="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
shopt -s nullglob
archives=("$package"/*-linux-x86_64.tar.gz)
test "${#archives[@]}" -eq 1
test -d /etc/ssl/certs
portable="$(mktemp)"
trap 'rm -f "$portable"' EXIT
# Official Ubuntu amd64 image manifests, pinned independently of mutable tags.
for image in \
    ubuntu@sha256:c664f8f86ed5a386b0a340d981b8f81714e21a8b9c73f658c4bea56aa179d54a \
    ubuntu@sha256:78d4744ead1f5b19b04d0df558ab182a5300903900755e515b2ca4d10eb2044d; do
docker run --rm --platform linux/amd64 --user 65534:65534 \
  --network none --read-only --tmpfs /tmp:exec,mode=1777 \
  --volume "$package:/artifacts:ro" \
  --volume "$scripts/smoke-linux-bundle-host.sh:/checks/smoke-linux-bundle-host.sh:ro" \
  --volume /etc/ssl/certs:/etc/ssl/certs:ro \
  "$image" /bin/sh -ec '
    cd /tmp
    tar -xzpf /artifacts/*-linux-x86_64.tar.gz
    ./maple-agent*.app/AppRun --version >&2
    ./maple-agent*.app/AppRun --build-info
    MAPLE_AGENT_SMOKE_DIAGNOSTICS=1 /bin/sh /checks/smoke-linux-bundle-host.sh ./maple-agent*.app/AppRun >&2
  ' > "$portable"
python3 - "$profile" "$portable" "$package/build-info.json" <<'PY'
import json
import sys
actual = json.load(open(sys.argv[2]))
expected = json.load(open(sys.argv[3]))
if actual != expected or actual.get("profile") != sys.argv[1]:
    raise SystemExit("portable Linux bundle build profile differs from verified package")
print("Verified Agent launch and embedded profile without a Nix installation")
PY
done
