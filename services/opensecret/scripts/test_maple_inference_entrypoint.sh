#!/usr/bin/env bash
# Exercise optional credentials and instance egress mappings without AWS, VSOCK
# or real secrets. Never run the complete enclave entrypoint on the host.
set -euo pipefail

component_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
entrypoint_path="${ENTRYPOINT_UNDER_TEST:-$component_root/entrypoint.sh}"
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
extract_block() {
    local name="$1" destination="$2"
    if [ "$(grep -Fxc "# BEGIN $name" "$entrypoint_path")" != 1 ] || \
        [ "$(grep -Fxc "# END $name" "$entrypoint_path")" != 1 ]; then
        echo "Maple inference block markers are missing or duplicated: $name" >&2
        exit 1
    fi
    sed -n "/^# BEGIN $name$/,/^# END $name$/p" "$entrypoint_path" > "$destination"
    if [ "$(head -n 1 "$destination")" != "# BEGIN $name" ] || \
        [ "$(tail -n 1 "$destination")" != "# END $name" ]; then
        echo "Maple inference block markers are out of order: $name" >&2
        exit 1
    fi
}
extract_block OPTIONAL_MAPLE_INFERENCE_CREDENTIAL "$work_dir/credential.sh"
# shellcheck source=/dev/null
source "$work_dir/credential.sh"

log() { printf '%s\n' "$*" >> "$work_dir/log"; }
vsock_request() {
    [ "$1" = 3 ] && [ "$2" = 8003 ]
    printf '%s' "$3" > "$work_dir/request"
    if [ "$fixture" = missing ]; then return 1; fi
    printf '%s' "$fixture_response"
}
python3() {
    [ "$1" = /app/vsock_helper.py ]
    shift
    vsock_request "$@"
}
timeout() {
    [ "$1" = -k ] && [ "$2" = 2 ] && [ "$3" = 15 ]
    shift 3
    if [ "$fixture" = timeout ]; then return 124; fi
    "$@"
}
kmstool_enclave_cli() {
    if [ "$fixture" = decrypt_failure ]; then return 1; fi
    printf 'PLAINTEXT: %s\n' "$encoded"
}

region=synthetic-region
access_key_id=synthetic-access
secret_access_key=synthetic-secret
session_token=synthetic-session
TINFOIL_API_KEY=synthetic-shared-key
fixture_response='{"response_type":"secret","response_value":"{\"api_key\":\"synthetic-ciphertext\"}"}'
encoded="$(printf '%s' synthetic-dedicated-key | base64 | tr -d '\n')"
fixture=success

for mode in prod preview dev custom; do
    APP_MODE="$mode" ENV_NAME=synthetic-preview
    get_maple_inference_api_key_secret >/dev/null
    case "$mode" in
        prod) expected=maple_inference_prod_api_key ;;
        preview) expected=maple_inference_preview1_api_key ;;
        dev) expected=maple_inference_dev_api_key ;;
        custom) expected=maple_inference_synthetic-preview_api_key ;;
    esac
    [ "$(jq -r .key_name "$work_dir/request")" = "$expected" ]
done
APP_MODE=custom ENV_NAME=""
if get_maple_inference_api_key_secret >/dev/null; then
    echo "custom mode accepted an absent environment name" >&2
    exit 1
fi

APP_MODE=dev ENV_NAME=""
for fixture in missing invalid_response timeout decrypt_failure invalid_base64 empty success; do
    # A host-inherited or shared key must never survive a failed optional load.
    MAPLE_INFERENCE_API_KEY=synthetic-host-inherited-key
    fixture_response='{"response_type":"secret","response_value":"{\"api_key\":\"synthetic-ciphertext\"}"}'
    encoded="$(printf '%s' synthetic-dedicated-key | base64 | tr -d '\n')"
    case "$fixture" in
        invalid_response) fixture_response='{"response_type":"error","response_value":"missing"}' ;;
        invalid_base64) encoded='not-base64!' ;;
        empty) encoded='' ;;
    esac
    load_optional_maple_inference_api_key
    if [ "$fixture" = success ]; then
        [ "$MAPLE_INFERENCE_API_KEY" = synthetic-dedicated-key ]
    else
        [ -z "$MAPLE_INFERENCE_API_KEY" ]
    fi
    [ "$TINFOIL_API_KEY" = synthetic-shared-key ]
done

if grep -Eq 'synthetic-(shared|dedicated|host-inherited)-key|synthetic-ciphertext' "$work_dir/log"; then
    echo "optional credential helper logged credential material" >&2
    exit 1
fi
echo "optional Maple inference credential tests passed"

extract_block MAPLE_INFERENCE_HOSTS "$work_dir/hosts-block.sh"
extract_block MAPLE_INFERENCE_FORWARDERS "$work_dir/forwarders-block.sh"

# Inspect host writes without executing redirections, so even a malformed
# fixture cannot touch the system host file or another real output path.
[ "$(sed '/^#/d; /^[[:space:]]*$/d' "$work_dir/hosts-block.sh")" = \
    $'echo "127.0.0.42 i01.tfinf.maple.ai" >> /etc/hosts\necho "127.0.0.43 i02.tfinf.maple.ai" >> /etc/hosts\necho "127.0.0.44 i04.tfinf.maple.ai" >> /etc/hosts' ]

# Stub the supervisor, so the extracted launch block only records arguments.
# Separate files preserve deterministic assertions even though launches use &.
run_forever() {
    local name="$1"
    shift
    case "$name" in
        tf_maple_inference_i01|tf_maple_inference_i02|tf_maple_inference_i04) ;;
        *) return 1 ;;
    esac
    printf '%s\n' "$*" >> "$work_dir/$name"
}
# shellcheck source=/dev/null
source "$work_dir/forwarders-block.sh"
wait
[ "$(cat "$work_dir/tf_maple_inference_i01")" = 'python3 /app/traffic_forwarder.py 127.0.0.42 443 3 8050' ]
[ "$(cat "$work_dir/tf_maple_inference_i02")" = 'python3 /app/traffic_forwarder.py 127.0.0.43 443 3 8051' ]
[ "$(cat "$work_dir/tf_maple_inference_i04")" = 'python3 /app/traffic_forwarder.py 127.0.0.44 443 3 8052' ]

# The complete source must not add aliases or duplicate launch entries outside
# the isolated blocks that would override or collide with the tested mappings.
[ "$(grep -Ec '^[[:space:]]*echo .*\.tfinf\.maple\.ai' "$entrypoint_path")" = 3 ]
[ "$(grep -Ec '^[[:space:]]*run_forever tf_maple_inference_' "$entrypoint_path")" = 3 ]
# Nor may any other host entry or forwarder reuse an instance's loopback
# address or VSOCK port.
for address in 127.0.0.42 127.0.0.43 127.0.0.44; do
    [ "$(grep -Fc "echo \"$address " "$entrypoint_path")" = 1 ]
    [ "$(grep -Fc "traffic_forwarder.py $address " "$entrypoint_path")" = 1 ]
done
for port in 8050 8051 8052; do
    [ "$(grep -Ec "traffic_forwarder\.py .* 3 $port( |\$)" "$entrypoint_path")" = 1 ]
done
echo "Maple inference i01/i02/i04 egress mapping tests passed"
