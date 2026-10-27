#!/usr/bin/env bash
# ci.yml's os-helpers lane: build OpenSecret's parent-side helper images for
# Linux ARM64 and run their tests offline, without publishing them (ci.yml then
# scans each image). HELPER picks one image; the default is both.
set -euo pipefail
cd "$(dirname "$0")/../../services/opensecret"

for helper in ${HELPER:-credential-requester enclave-logging}; do
  case "$helper" in
    credential-requester) CONTEXT=nitro-toolkit/credential_requester ;;
    enclave-logging) CONTEXT=nitro-toolkit/logging ;;
    *) echo "Unknown helper image: $helper" >&2; exit 2 ;;
  esac
  IMAGE="$helper:ci"

  # Build Linux ARM64 image
  docker build --platform linux/arm64 --tag "$IMAGE" "$CONTEXT"

  # Check image architecture and runtime packages
  test "$(docker image inspect --format '{{.Os}}/{{.Architecture}}' "$IMAGE")" = linux/arm64
  docker run --rm --network none --entrypoint python "$IMAGE" -c \
    'import boto3, botocore, urllib3, importlib.util; assert importlib.util.find_spec("pip") is None; assert importlib.util.find_spec("ensurepip") is None'

  # Run offline helper tests inside the image
  docker run --rm --network none \
    --mount "type=bind,src=$PWD/$CONTEXT,dst=/tests,readonly" \
    --entrypoint python "$IMAGE" \
    -B -m unittest discover -s /tests -p 'test_*.py' -v
done
