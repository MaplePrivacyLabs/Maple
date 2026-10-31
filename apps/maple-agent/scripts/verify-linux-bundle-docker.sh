#!/usr/bin/env bash
# Start a Linux bundle in a stock Ubuntu container with no Nix and no network,
# and check its embedded build identity against the package's build-info.json.
# The container adds only the desktop libraries every Linux desktop provides;
# the bundle brings no glibc or other libraries of its own.
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
image="maple-agent-linux-smoke:$$"
trap 'rm -f "$portable"; docker image rm -f "$image" >/dev/null 2>&1 || true' EXIT
docker build --quiet --platform linux/amd64 --tag "$image" - >/dev/null <<'DOCKERFILE'
FROM ubuntu@sha256:f610ab94648195aa356059f5b41d6085c9d4d903c072430cdd1af7bdb646106b
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
    libasound2t64 libx11-6 libxcb1 libxi6 libxkbcommon0 libxkbcommon-x11-0 \
 && rm -rf /var/lib/apt/lists/*
DOCKERFILE
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
