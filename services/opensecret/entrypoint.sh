#!/bin/bash

set -e

# Function for logging
log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $1"
}

# BEGIN ENCLAVE_ENTROPY_PREFLIGHT
# Fail closed before any application or KMS work if the enclave entropy path is
# not the one we expect. Positional overrides exist only so this function can
# be exercised against fixtures; production calls it with the defaults.
verify_enclave_entropy_readiness() {
    local nsm_device="${1:-/dev/nsm}"
    local rng_current_path="${2:-/sys/class/misc/hw_random/rng_current}"
    local python_bin="${3:-python3}"
    local rng_current

    if [ ! -c "$nsm_device" ]; then
        log "ERROR: NSM device is missing or is not a character device: $nsm_device"
        return 1
    fi

    if [ ! -r "$rng_current_path" ]; then
        log "ERROR: Hardware RNG selection is not readable: $rng_current_path"
        return 1
    fi

    if ! IFS= read -r rng_current < "$rng_current_path"; then
        log "ERROR: Failed to read hardware RNG selection: $rng_current_path"
        return 1
    fi

    if [ "$rng_current" != "nsm-hwrng" ]; then
        log "ERROR: Unexpected hardware RNG selected: ${rng_current:-<empty>} (expected nsm-hwrng)"
        return 1
    fi

    # A flags value of zero selects the urandom source and blocks until the
    # kernel CRNG is initialized. Bound the admission check to 30 seconds so
    # a broken entropy path exits instead of hanging before readiness forever.
    if ! "$python_bin" -c '
import os
import signal
import sys

if not hasattr(os, "getrandom"):
    sys.exit("os.getrandom is unavailable")

def crng_timeout(_signum, _frame):
    raise TimeoutError("getrandom readiness check timed out")

signal.signal(signal.SIGALRM, crng_timeout)
signal.alarm(30)
try:
    sample = os.getrandom(1, 0)
except (OSError, TimeoutError) as error:
    sys.exit(f"getrandom failed: {error}")
finally:
    signal.alarm(0)

if len(sample) != 1:
    sys.exit(f"getrandom returned {len(sample)} bytes; expected 1")
'; then
        log "ERROR: Kernel CRNG readiness check failed"
        return 1
    fi

    log "Enclave entropy readiness checks passed (NSM present, nsm-hwrng selected, CRNG initialized)"
}

verify_enclave_entropy_for_mode() {
    local app_mode="$1"
    shift

    if [ "$app_mode" = "local" ]; then
        return 0
    fi

    verify_enclave_entropy_readiness "$@"
}
# END ENCLAVE_ENTROPY_PREFLIGHT

# Restart-on-exit wrapper for long-running helpers started from this script.
run_forever() {
    local name="$1"
    shift

    local attempt=0
    local backoff_s=1
    local max_backoff_s=30

    set +e
    while true; do
        attempt=$((attempt + 1))

        "$@" &
        local pid=$!

        log "MONITOR started=${name} pid=${pid} attempt=${attempt}"

        wait "$pid"
        local ec=$?

        log "MONITOR restarting=${name} reason=exit_code code=${ec} old_pid=${pid} attempt=${attempt} backoff_s=${backoff_s}"
        sleep "$backoff_s"

        if [ "$backoff_s" -lt "$max_backoff_s" ]; then
            backoff_s=$((backoff_s * 2))
            if [ "$backoff_s" -gt "$max_backoff_s" ]; then
                backoff_s="$max_backoff_s"
            fi
        fi
    done
}

# Forward stdout/stderr to parent logging over VSOCK with auto-reconnect.
# If the VSOCK path is down, drain+drop logs to avoid blocking the enclave.
log_forwarder() {
    set +e
    local backoff_s=1
    local max_backoff_s=30

    while true; do
        local start_ts
        start_ts="$(date +%s)"

        socat - VSOCK-CONNECT:3:8011

        local end_ts
        end_ts="$(date +%s)"
        local runtime_s=$((end_ts - start_ts))
        if [ "$runtime_s" -ge 10 ]; then
            backoff_s=1
        fi

        timeout "$backoff_s" cat >/dev/null || true
        if [ "$backoff_s" -lt "$max_backoff_s" ]; then
            backoff_s=$((backoff_s * 2))
            if [ "$backoff_s" -gt "$max_backoff_s" ]; then
                backoff_s="$max_backoff_s"
            fi
        fi
    done
}

log "Starting entrypoint script"

# Read and set APP_MODE from file
log "Reading /app/APP_MODE"
if [ -f /app/APP_MODE ]; then
    APP_MODE=""
    APP_MODE="$(cat /app/APP_MODE)" || { log "Failed to read /app/APP_MODE"; exit 1; }
    export APP_MODE
    log "Set APP_MODE=$APP_MODE from /app/APP_MODE"
else
    log "ERROR: /app/APP_MODE is missing. Please ensure the file exists and contains a valid mode (dev/preview/prod/custom)"
    exit 1
fi

log "Starting entrypoint script"
log "APP_MODE=$APP_MODE"
log "Kernel version: $(uname -r)"

# Local development does not run inside a Nitro Enclave and has neither NSM nor
# nsm-hwrng. All deployable enclave modes must pass before startup continues.
verify_enclave_entropy_for_mode "$APP_MODE" || exit 1

# Start VSOCK log forwarding only after the enclave entropy path is known good.
# Until this point, diagnostics remain on the enclave console.
log "Starting log exports"
exec > >(log_forwarder) 2>&1

# Increase file descriptor limits to prevent socket exhaustion
ulimit -n 65536
log "File descriptor limit set to: $(ulimit -n)"

# Configure loopback interface
log "Configuring loopback interface"
ip addr add 127.0.0.1/8 dev lo
ip link set dev lo up

# Function to send request and receive response via VSOCK
vsock_request() {
    local cid=$1
    local port=$2
    local request=$3

    response=$(python3 /app/vsock_helper.py "$cid" "$port" "$request")
    
    # Check if the response contains an error
    if echo "$response" | jq -e 'has("error")' > /dev/null 2>&1; then
        log "VSOCK request failed"
        return 1
    fi

    echo "$response"
}

# Function to get AWS credentials
get_aws_credentials() {
    local cid=3
    local port=8003
    local request='{"request_type":"credentials","key_name":null}'

    vsock_request $cid $port "$request"
}

# Function to get secret from Secrets Manager
get_database_url_secret() {
    local cid=3
    local port=8003

    # Determine the correct secret name based on APP_MODE
    local secret_name
    if [ "$APP_MODE" = "prod" ]; then
        secret_name="opensecret_prod_database_url"
    elif [ "$APP_MODE" = "preview" ]; then
        secret_name="opensecret_preview1_database_url"
    elif [ "$APP_MODE" = "custom" ]; then
        if [ -z "$ENV_NAME" ]; then
            log "Error: ENV_NAME must be set when using custom mode"
            exit 1
        fi
        secret_name="opensecret_${ENV_NAME}_database_url"
    else
        secret_name="opensecret_dev_database_url"
    fi

    local request="{\"request_type\":\"SecretsManager\",\"key_name\":\"$secret_name\"}"

    vsock_request $cid $port "$request"
}

# Function to get secret from Secrets Manager
get_continuum_proxy_api_key_secret() {
    local cid=3
    local port=8003

    # Determine the correct secret name based on APP_MODE
    local secret_name
    if [ "$APP_MODE" = "prod" ]; then
        secret_name="continuum_proxy_prod_api_key"
    elif [ "$APP_MODE" = "preview" ]; then
        secret_name="continuum_proxy_preview1_api_key"
    elif [ "$APP_MODE" = "custom" ]; then
        if [ -z "$ENV_NAME" ]; then
            log "Error: ENV_NAME must be set when using custom mode"
            exit 1
        fi
        secret_name="continuum_proxy_${ENV_NAME}_api_key"
    else
        secret_name="continuum_proxy_dev_api_key"
    fi

    local request="{\"request_type\":\"SecretsManager\",\"key_name\":\"$secret_name\"}"

    vsock_request $cid $port "$request"
}

# Function to get tinfoil proxy API key from Secrets Manager
get_tinfoil_proxy_api_key_secret() {
    local cid=3
    local port=8003

    # Determine the correct secret name based on APP_MODE
    local secret_name
    if [ "$APP_MODE" = "prod" ]; then
        secret_name="tinfoil_proxy_prod_api_key"
    elif [ "$APP_MODE" = "preview" ]; then
        secret_name="tinfoil_proxy_preview1_api_key"
    elif [ "$APP_MODE" = "custom" ]; then
        if [ -z "$ENV_NAME" ]; then
            log "Error: ENV_NAME must be set when using custom mode"
            exit 1
        fi
        secret_name="tinfoil_proxy_${ENV_NAME}_api_key"
    else
        secret_name="tinfoil_proxy_dev_api_key"
    fi

    local request="{\"request_type\":\"SecretsManager\",\"key_name\":\"$secret_name\"}"

    vsock_request $cid $port "$request"
}

# Get AWS credentials
aws_creds=$(get_aws_credentials)
if [ -z "$aws_creds" ]; then
    log "Error: Failed to get AWS credentials"
    exit 1
fi

# Add error checking for jq parsing
if ! access_key_id=$(echo "$aws_creds" | jq -r '.response_value.AccessKeyId' 2>/dev/null); then
    log "Error: Failed to parse AccessKeyId from AWS credentials"
    exit 1
fi
if ! secret_access_key=$(echo "$aws_creds" | jq -r '.response_value.SecretAccessKey' 2>/dev/null); then
    log "Error: Failed to parse SecretAccessKey from AWS credentials"
    exit 1
fi
if ! session_token=$(echo "$aws_creds" | jq -r '.response_value.Token' 2>/dev/null); then
    log "Error: Failed to parse Token from AWS credentials"
    exit 1
fi
if ! region=$(echo "$aws_creds" | jq -r '.response_value.Region' 2>/dev/null); then
    log "Error: Failed to parse Region from AWS credentials"
    exit 1
fi

# Get encrypted database URL from Secrets Manager
secret_response=$(get_database_url_secret)

# Extract the database_url value from the JSON structure
if ! encrypted_db_url=$(echo "$secret_response" | jq -r '.response_value | fromjson | .database_url' 2>/dev/null); then
    log "Error: Failed to get encrypted database URL"
    exit 1
fi
if [ -z "$encrypted_db_url" ]; then
    log "Error: Failed to get encrypted database URL"
    exit 1
fi

# Decrypt the database URL using kmstool_enclave_cli
decryption_output=$(kmstool_enclave_cli decrypt \
    --region "$region" \
    --proxy-port 8000 \
    --aws-access-key-id "$access_key_id" \
    --aws-secret-access-key "$secret_access_key" \
    --aws-session-token "$session_token" \
    --ciphertext "$encrypted_db_url" 2>&1)

decrypted_db_url=$(echo "$decryption_output" | sed -n 's/PLAINTEXT: //p')

if [ -z "$decrypted_db_url" ]; then
    log "Error: Failed to decrypt database URL"
    exit 1
fi

# Decode the base64 decrypted URL
if ! decoded_db_url=$(echo "$decrypted_db_url" | base64 -d 2>/dev/null); then
    log "Error: Failed to decode base64 database URL"
    exit 1
fi

if [ -z "$decoded_db_url" ]; then
    log "Error: Failed to decode base64 database URL"
    exit 1
fi

# Extract the hostname from the decoded DATABASE_URL and add it to /etc/hosts
DB_HOSTNAME=$(echo "$decoded_db_url" | sed -n 's/.*@\([^/]*\).*/\1/p')
if [ -z "$DB_HOSTNAME" ]; then
    log "Error: Failed to extract DB_HOSTNAME from decoded URL"
    exit 1
fi

echo "127.0.0.1 $DB_HOSTNAME" >> /etc/hosts

# Add OpenAI API hostname to /etc/hosts
echo "127.0.0.1 api.openai.com" >> /etc/hosts
log "Added api.openai.com to /etc/hosts"

# Add Resend API hostname to /etc/hosts
echo "127.0.0.8 api.resend.com" >> /etc/hosts
log "Added api.resend.com to /etc/hosts"

# Add continuum hostnames to /etc/hosts
echo "127.0.0.2 api.privatemode.ai" >> /etc/hosts
echo "127.0.0.3 cdn.confidential.cloud" >> /etc/hosts
echo "127.0.0.4 secret.privatemode.ai" >> /etc/hosts
echo "127.0.0.5 coordinator.privatemode.ai" >> /etc/hosts
echo "127.0.0.6 kdsintf.amd.com" >> /etc/hosts
echo "127.0.0.40 api.trustedservices.intel.com" >> /etc/hosts
echo "127.0.0.41 certificates.trustedservices.intel.com" >> /etc/hosts

log "Added privatemode.ai, confidential.cloud, AMD, and Intel attestation domains to /etc/hosts"

# Add GitHub OAuth hostnames to /etc/hosts
echo "127.0.0.9 github.com" >> /etc/hosts
echo "127.0.0.10 api.github.com" >> /etc/hosts
log "Added GitHub OAuth domains to /etc/hosts"

# Add Google OAuth hostnames to /etc/hosts
echo "127.0.0.11 oauth2.googleapis.com" >> /etc/hosts
echo "127.0.0.12 www.googleapis.com" >> /etc/hosts
log "Added Google OAuth domains to /etc/hosts"

# Add Apple OAuth hostname to /etc/hosts
echo "127.0.0.15 appleid.apple.com" >> /etc/hosts
log "Added Apple OAuth domain to /etc/hosts"

# Add AWS SQS hostname to /etc/hosts
echo "127.0.0.13 sqs.us-east-2.amazonaws.com" >> /etc/hosts
log "Added AWS SQS domain to /etc/hosts"

# Add billing hostname to /etc/hosts based on APP_MODE
if [ "$APP_MODE" = "prod" ]; then
    echo "127.0.0.14 billing.opensecret.cloud" >> /etc/hosts
    log "Added production billing domain to /etc/hosts"
else
    echo "127.0.0.14 billing-dev.opensecret.cloud" >> /etc/hosts
    log "Added development billing domain to /etc/hosts"
fi

# Add os-flags hostname to /etc/hosts based on APP_MODE
if [ "$APP_MODE" = "prod" ]; then
    echo "127.0.0.18 flags.opensecret.cloud" >> /etc/hosts
    log "Added production os-flags domain to /etc/hosts"
else
    echo "127.0.0.18 flags-dev.opensecret.cloud" >> /etc/hosts
    log "Added development os-flags domain to /etc/hosts"
fi

# Add Tinfoil proxy hostnames to /etc/hosts
echo "127.0.0.16 github-proxy.tinfoil.sh" >> /etc/hosts
echo "127.0.0.17 tuf-repo-cdn.sigstore.dev" >> /etc/hosts
echo "127.0.0.19 kds-proxy.tinfoil.sh" >> /etc/hosts
echo "127.0.0.25 atc.tinfoil.sh" >> /etc/hosts
echo "127.0.0.33 inference.tinfoil.sh" >> /etc/hosts
# New Tinfoil router domains
echo "127.0.0.34 router-0.tinfoil.sh" >> /etc/hosts
echo "127.0.0.35 router-1.tinfoil.sh" >> /etc/hosts
echo "127.0.0.36 router-2.tinfoil.sh" >> /etc/hosts
echo "127.0.0.37 router-3.tinfoil.sh" >> /etc/hosts
echo "127.0.0.38 router-4.tinfoil.sh" >> /etc/hosts
echo "127.0.0.39 router-5.tinfoil.sh" >> /etc/hosts
# Legacy Tinfoil router domains (retained for discovery and fallback)
echo "127.0.0.26 router.inf4.tinfoil.sh" >> /etc/hosts
echo "127.0.0.27 router.inf5.tinfoil.sh" >> /etc/hosts
echo "127.0.0.28 router.inf6.tinfoil.sh" >> /etc/hosts
echo "127.0.0.29 router.inf7.tinfoil.sh" >> /etc/hosts
echo "127.0.0.30 router.inf8.tinfoil.sh" >> /etc/hosts
echo "127.0.0.31 router.inf9.tinfoil.sh" >> /etc/hosts
echo "127.0.0.32 router.inf10.tinfoil.sh" >> /etc/hosts
log "Added Tinfoil proxy domains (including legacy router domains) to /etc/hosts"

# Add Kagi Search hostname to /etc/hosts
echo "127.0.0.23 kagi.com" >> /etc/hosts
log "Added Kagi Search domain to /etc/hosts"

touch /app/libnsm.so
log "Created /app/libnsm.so"

# Print network information for debugging
log "Network configuration:"
ip addr show
ip route

# Start the traffic forwarder for the database in the background
log "Starting database traffic forwarder"
run_forever tf_db python3 /app/traffic_forwarder.py 127.0.0.1 5432 3 8001 &

# Start the traffic forwarder for OpenAI API in the background
log "Starting OpenAI API traffic forwarder"
run_forever tf_openai_api python3 /app/traffic_forwarder.py 127.0.0.1 443 3 8002 &

# Start the traffic forwarder for Resend API in the background
log "Starting Resend API traffic forwarder"
run_forever tf_resend_api python3 /app/traffic_forwarder.py 127.0.0.8 443 3 8010 &

# Start the traffic forwarder for Continuum API in the background
log "Starting Continuum API traffic forwarder"
run_forever tf_continuum_api python3 /app/traffic_forwarder.py 127.0.0.2 443 3 8004 &

# Start the traffic forwarder for Continuum CDN in the background
log "Starting Continuum CDN traffic forwarder"
run_forever tf_continuum_cdn python3 /app/traffic_forwarder.py 127.0.0.3 443 3 8005 &

# Start the traffic forwarder for Continuum Secret Service in the background
log "Starting Continuum Secret Service traffic forwarder"
run_forever tf_continuum_secret python3 /app/traffic_forwarder.py 127.0.0.4 443 3 8006 &

# Start the traffic forwarder for Continuum Coordinator in the background
log "Starting Continuum Coordinator traffic forwarder"
run_forever tf_continuum_coordinator python3 /app/traffic_forwarder.py 127.0.0.5 443 3 8007 &

# Start the traffic forwarder for AMD KDS Interface in the background
log "Starting AMD KDS Interface traffic forwarder"
run_forever tf_amd_kds python3 /app/traffic_forwarder.py 127.0.0.6 443 3 8008 &

# Start the traffic forwarder for Intel PCS API in the background
log "Starting Intel PCS API traffic forwarder"
run_forever tf_intel_pcs_api python3 /app/traffic_forwarder.py 127.0.0.40 443 3 8048 &

# Start the traffic forwarder for Intel PCS certificates in the background
log "Starting Intel PCS certificates traffic forwarder"
run_forever tf_intel_pcs_certs python3 /app/traffic_forwarder.py 127.0.0.41 443 3 8049 &

# Start the traffic forwarder for GitHub in the background
log "Starting GitHub traffic forwarder"
run_forever tf_github python3 /app/traffic_forwarder.py 127.0.0.9 443 3 8012 &

# Start the traffic forwarder for GitHub API in the background
log "Starting GitHub API traffic forwarder"
run_forever tf_github_api python3 /app/traffic_forwarder.py 127.0.0.10 443 3 8013 &

# Start the traffic forwarder for Google OAuth in the background
log "Starting Google OAuth traffic forwarder"
run_forever tf_google_oauth python3 /app/traffic_forwarder.py 127.0.0.11 443 3 8014 &

# Start the traffic forwarder for Google APIs in the background
log "Starting Google APIs traffic forwarder"
run_forever tf_google_api python3 /app/traffic_forwarder.py 127.0.0.12 443 3 8015 &

# Start the traffic forwarder for AWS SQS in the background
log "Starting AWS SQS traffic forwarder"
run_forever tf_aws_sqs python3 /app/traffic_forwarder.py 127.0.0.13 443 3 8016 &

# Start the traffic forwarder for billing service in the background
log "Starting billing service traffic forwarder"
run_forever tf_billing python3 /app/traffic_forwarder.py 127.0.0.14 443 3 8017 &

# Start the traffic forwarder for os-flags in the background
log "Starting os-flags traffic forwarder"
run_forever tf_os_flags python3 /app/traffic_forwarder.py 127.0.0.18 443 3 8028 &

# Start the traffic forwarder for Apple OAuth in the background
log "Starting Apple OAuth traffic forwarder"
run_forever tf_apple_oauth python3 /app/traffic_forwarder.py 127.0.0.15 443 3 8018 &

# Start the traffic forwarders for Tinfoil proxy in the background
log "Starting Tinfoil GitHub proxy traffic forwarder"
run_forever tf_tinfoil_github_proxy python3 /app/traffic_forwarder.py 127.0.0.16 443 3 8019 &

log "Starting TUF Repository CDN traffic forwarder"
run_forever tf_tuf_repo_cdn python3 /app/traffic_forwarder.py 127.0.0.17 443 3 8020 &

log "Starting Tinfoil KDS proxy traffic forwarder"
run_forever tf_tinfoil_kds_proxy python3 /app/traffic_forwarder.py 127.0.0.19 443 3 8022 &

log "Starting Tinfoil ATC traffic forwarder"
run_forever tf_tinfoil_atc python3 /app/traffic_forwarder.py 127.0.0.25 443 3 8021 &

log "Starting Tinfoil Inference traffic forwarder"
run_forever tf_tinfoil_inference python3 /app/traffic_forwarder.py 127.0.0.33 443 3 8041 &

log "Starting Tinfoil Router 0 traffic forwarder"
run_forever tf_tinfoil_router_0 python3 /app/traffic_forwarder.py 127.0.0.34 443 3 8042 &

log "Starting Tinfoil Router 1 traffic forwarder"
run_forever tf_tinfoil_router_1 python3 /app/traffic_forwarder.py 127.0.0.35 443 3 8043 &

log "Starting Tinfoil Router 2 traffic forwarder"
run_forever tf_tinfoil_router_2 python3 /app/traffic_forwarder.py 127.0.0.36 443 3 8044 &

log "Starting Tinfoil Router 3 traffic forwarder"
run_forever tf_tinfoil_router_3 python3 /app/traffic_forwarder.py 127.0.0.37 443 3 8045 &

log "Starting Tinfoil Router 4 traffic forwarder"
run_forever tf_tinfoil_router_4 python3 /app/traffic_forwarder.py 127.0.0.38 443 3 8046 &

log "Starting Tinfoil Router 5 traffic forwarder"
run_forever tf_tinfoil_router_5 python3 /app/traffic_forwarder.py 127.0.0.39 443 3 8047 &

# Legacy Tinfoil router endpoints (retained for discovery and fallback)
log "Starting Tinfoil Router Inf4 traffic forwarder"
run_forever tf_tinfoil_router_inf4 python3 /app/traffic_forwarder.py 127.0.0.26 443 3 8034 &

log "Starting Tinfoil Router Inf5 traffic forwarder"
run_forever tf_tinfoil_router_inf5 python3 /app/traffic_forwarder.py 127.0.0.27 443 3 8035 &

log "Starting Tinfoil Router Inf6 traffic forwarder"
run_forever tf_tinfoil_router_inf6 python3 /app/traffic_forwarder.py 127.0.0.28 443 3 8036 &

log "Starting Tinfoil Router Inf7 traffic forwarder"
run_forever tf_tinfoil_router_inf7 python3 /app/traffic_forwarder.py 127.0.0.29 443 3 8037 &

log "Starting Tinfoil Router Inf8 traffic forwarder"
run_forever tf_tinfoil_router_inf8 python3 /app/traffic_forwarder.py 127.0.0.30 443 3 8038 &

log "Starting Tinfoil Router Inf9 traffic forwarder"
run_forever tf_tinfoil_router_inf9 python3 /app/traffic_forwarder.py 127.0.0.31 443 3 8039 &

log "Starting Tinfoil Router Inf10 traffic forwarder"
run_forever tf_tinfoil_router_inf10 python3 /app/traffic_forwarder.py 127.0.0.32 443 3 8040 &

# Start the traffic forwarder for Kagi Search in the background
log "Starting Kagi Search traffic forwarder"
run_forever tf_kagi_search python3 /app/traffic_forwarder.py 127.0.0.23 443 3 8026 &

# Wait for the forwarders to start
log "Waiting for forwarders to start"
sleep 5

# Test the connection to PostgreSQL
log "Testing connection to PostgreSQL:"
if timeout 5 bash -c '</dev/tcp/127.0.0.1/5432'; then
    log "PostgreSQL connection successful"
else
    log "PostgreSQL connection failed"
fi

# Test the connection to OpenAI API (Note: This will only test if the port is open)
log "Testing connection to OpenAI API:"
if timeout 5 bash -c '</dev/tcp/127.0.0.1/443'; then
    log "OpenAI API connection successful"
else
    log "OpenAI API connection failed"
fi

# Test the connection to Continuum API
log "Testing connection to Continuum API:"
if timeout 5 bash -c '</dev/tcp/127.0.0.2/443'; then
    log "Continuum API connection successful"
else
    log "Continuum API connection failed"
fi

log "Testing connection to Continuum CDN:"
if timeout 5 bash -c '</dev/tcp/127.0.0.3/443'; then
    log "Continuum CDN connection successful"
else
    log "Continuum CDN connection failed"
fi

log "Testing connection to Continuum Secret Service:"
if timeout 5 bash -c '</dev/tcp/127.0.0.4/443'; then
    log "Continuum Secret Service connection successful"
else
    log "Continuum Secret Service connection failed"
fi

log "Testing connection to Continuum Coordinator:"
if timeout 5 bash -c '</dev/tcp/127.0.0.5/443'; then
    log "Continuum Coordinator connection successful"
else
    log "Continuum Coordinator connection failed"
fi

log "Testing connection to AMD KDS Interface:"
if timeout 5 bash -c '</dev/tcp/127.0.0.6/443'; then
    log "AMD KDS Interface connection successful"
else
    log "AMD KDS Interface connection failed"
fi

log "Testing connection to Intel PCS API:"
if timeout 5 bash -c '</dev/tcp/127.0.0.40/443'; then
    log "Intel PCS API connection successful"
else
    log "Intel PCS API connection failed"
fi

log "Testing connection to Intel PCS certificates:"
if timeout 5 bash -c '</dev/tcp/127.0.0.41/443'; then
    log "Intel PCS certificates connection successful"
else
    log "Intel PCS certificates connection failed"
fi

# Test the connection to GitHub
log "Testing connection to GitHub:"
if timeout 5 bash -c '</dev/tcp/127.0.0.9/443'; then
    log "GitHub connection successful"
else
    log "GitHub connection failed"
fi

# Test the connection to GitHub API
log "Testing connection to GitHub API:"
if timeout 5 bash -c '</dev/tcp/127.0.0.10/443'; then
    log "GitHub API connection successful"
else
    log "GitHub API connection failed"
fi

# Test the connection to Google OAuth
log "Testing connection to Google OAuth:"
if timeout 5 bash -c '</dev/tcp/127.0.0.11/443'; then
    log "Google OAuth connection successful"
else
    log "Google OAuth connection failed"
fi

# Test the connection to Google APIs
log "Testing connection to Google APIs:"
if timeout 5 bash -c '</dev/tcp/127.0.0.12/443'; then
    log "Google APIs connection successful"
else
    log "Google APIs connection failed"
fi

# Test the connection to AWS SQS
log "Testing connection to AWS SQS:"
if timeout 5 bash -c '</dev/tcp/127.0.0.13/443'; then
    log "AWS SQS connection successful"
else
    log "AWS SQS connection failed"
fi

# Test the connection to billing service
log "Testing connection to billing service:"
if timeout 5 bash -c '</dev/tcp/127.0.0.14/443'; then
    log "Billing service connection successful"
else
    log "Billing service connection failed"
fi

# Test the connection to os-flags
log "Testing connection to os-flags:"
if timeout 5 bash -c '</dev/tcp/127.0.0.18/443'; then
    log "os-flags connection successful"
else
    log "os-flags connection failed"
fi

# Test the connection to Apple OAuth
log "Testing connection to Apple OAuth:"
if timeout 5 bash -c '</dev/tcp/127.0.0.15/443'; then
    log "Apple OAuth connection successful"
else
    log "Apple OAuth connection failed"
fi

# Test the connections to Tinfoil proxy services
log "Testing connection to Tinfoil GitHub proxy:"
if timeout 5 bash -c '</dev/tcp/127.0.0.16/443'; then
    log "Tinfoil GitHub proxy connection successful"
else
    log "Tinfoil GitHub proxy connection failed"
fi

log "Testing connection to TUF Repository CDN:"
if timeout 5 bash -c '</dev/tcp/127.0.0.17/443'; then
    log "TUF Repository CDN connection successful"
else
    log "TUF Repository CDN connection failed"
fi

log "Testing connection to Tinfoil KDS proxy:"
if timeout 5 bash -c '</dev/tcp/127.0.0.19/443'; then
    log "Tinfoil KDS proxy connection successful"
else
    log "Tinfoil KDS proxy connection failed"
fi

log "Testing connection to Tinfoil ATC:"
if timeout 5 bash -c '</dev/tcp/127.0.0.25/443'; then
    log "Tinfoil ATC connection successful"
else
    log "Tinfoil ATC connection failed"
fi

log "Testing connection to Tinfoil Inference:"
if timeout 5 bash -c '</dev/tcp/127.0.0.33/443'; then
    log "Tinfoil Inference connection successful"
else
    log "Tinfoil Inference connection failed"
fi

log "Testing connection to Tinfoil Router 0:"
if timeout 5 bash -c '</dev/tcp/127.0.0.34/443'; then
    log "Tinfoil Router 0 connection successful"
else
    log "Tinfoil Router 0 connection failed"
fi

log "Testing connection to Tinfoil Router 1:"
if timeout 5 bash -c '</dev/tcp/127.0.0.35/443'; then
    log "Tinfoil Router 1 connection successful"
else
    log "Tinfoil Router 1 connection failed"
fi

log "Testing connection to Tinfoil Router 2:"
if timeout 5 bash -c '</dev/tcp/127.0.0.36/443'; then
    log "Tinfoil Router 2 connection successful"
else
    log "Tinfoil Router 2 connection failed"
fi

log "Testing connection to Tinfoil Router 3:"
if timeout 5 bash -c '</dev/tcp/127.0.0.37/443'; then
    log "Tinfoil Router 3 connection successful"
else
    log "Tinfoil Router 3 connection failed"
fi

log "Testing connection to Tinfoil Router 4:"
if timeout 5 bash -c '</dev/tcp/127.0.0.38/443'; then
    log "Tinfoil Router 4 connection successful"
else
    log "Tinfoil Router 4 connection failed"
fi

log "Testing connection to Tinfoil Router 5:"
if timeout 5 bash -c '</dev/tcp/127.0.0.39/443'; then
    log "Tinfoil Router 5 connection successful"
else
    log "Tinfoil Router 5 connection failed"
fi

# Legacy Tinfoil router endpoints (retained for discovery and fallback)
log "Testing connection to Tinfoil Router Inf4:"
if timeout 5 bash -c '</dev/tcp/127.0.0.26/443'; then
    log "Tinfoil Router Inf4 connection successful"
else
    log "Tinfoil Router Inf4 connection failed"
fi

log "Testing connection to Tinfoil Router Inf5:"
if timeout 5 bash -c '</dev/tcp/127.0.0.27/443'; then
    log "Tinfoil Router Inf5 connection successful"
else
    log "Tinfoil Router Inf5 connection failed"
fi

log "Testing connection to Tinfoil Router Inf6:"
if timeout 5 bash -c '</dev/tcp/127.0.0.28/443'; then
    log "Tinfoil Router Inf6 connection successful"
else
    log "Tinfoil Router Inf6 connection failed"
fi

log "Testing connection to Tinfoil Router Inf7:"
if timeout 5 bash -c '</dev/tcp/127.0.0.29/443'; then
    log "Tinfoil Router Inf7 connection successful"
else
    log "Tinfoil Router Inf7 connection failed"
fi

log "Testing connection to Tinfoil Router Inf8:"
if timeout 5 bash -c '</dev/tcp/127.0.0.30/443'; then
    log "Tinfoil Router Inf8 connection successful"
else
    log "Tinfoil Router Inf8 connection failed"
fi

log "Testing connection to Tinfoil Router Inf9:"
if timeout 5 bash -c '</dev/tcp/127.0.0.31/443'; then
    log "Tinfoil Router Inf9 connection successful"
else
    log "Tinfoil Router Inf9 connection failed"
fi

log "Testing connection to Tinfoil Router Inf10:"
if timeout 5 bash -c '</dev/tcp/127.0.0.32/443'; then
    log "Tinfoil Router Inf10 connection successful"
else
    log "Tinfoil Router Inf10 connection failed"
fi

# Test the connection to Kagi Search
log "Testing connection to Kagi Search:"
if timeout 5 bash -c '</dev/tcp/127.0.0.23/443'; then
    log "Kagi Search connection successful"
else
    log "Kagi Search connection failed"
fi

# Start the continuum-proxy if we're in AWS Nitro mode
if [ "$APP_MODE" != "local" ]; then
    # Get Continuum Proxy API key from Secrets Manager
    continuum_proxy_api_key_response=$(get_continuum_proxy_api_key_secret)

    # Check if the response is an error
    if echo "$continuum_proxy_api_key_response" | jq -e '.response_type == "error"' > /dev/null 2>&1; then
        log "Error: Failed to get Continuum Proxy API key"
        exit 1
    fi

    # Extract the encrypted API key value from the JSON structure
    if ! continuum_proxy_api_key_encrypted=$(echo "$continuum_proxy_api_key_response" | jq -r '.response_value | fromjson | .api_key' 2>/dev/null); then
        log "Error: Failed to extract Continuum Proxy API key from the response"
        exit 1
    fi
    if [ -z "$continuum_proxy_api_key_encrypted" ]; then
        log "Error: Failed to extract Continuum Proxy API key from the response"
        exit 1
    fi

    # Decrypt the API key using kmstool_enclave_cli
    decryption_output=$(kmstool_enclave_cli decrypt \
        --region "$region" \
        --proxy-port 8000 \
        --aws-access-key-id "$access_key_id" \
        --aws-secret-access-key "$secret_access_key" \
        --aws-session-token "$session_token" \
        --ciphertext "$continuum_proxy_api_key_encrypted" 2>&1)

    decrypted_api_key=$(echo "$decryption_output" | sed -n 's/PLAINTEXT: //p')

    if [ -z "$decrypted_api_key" ]; then
        log "Error: Failed to decrypt Continuum Proxy API key"
        exit 1
    fi

    # Base64 decode the decrypted API key
    if ! continuum_proxy_api_key=$(echo "$decrypted_api_key" | base64 -d 2>/dev/null); then
        log "Error: Failed to base64 decode Continuum Proxy API key"
        exit 1
    fi

    if [ -z "$continuum_proxy_api_key" ]; then
        log "Error: Failed to base64 decode Continuum Proxy API key"
        exit 1
    fi

    log "Starting continuum-proxy on port 8092 with user-scoped salts and per-request fallback"
    run_forever continuum_proxy /app/continuum-proxy --port 8092 --apiKey "$continuum_proxy_api_key" &

    # Wait for the proxy to start
    sleep 5

    # Set OPENAI_API_BASE to point to the local proxy
    export OPENAI_API_BASE="http://127.0.0.1:8092"
    
    # Get Tinfoil Proxy API key from Secrets Manager
    tinfoil_proxy_api_key_response=$(get_tinfoil_proxy_api_key_secret)

    # Check if the response is an error
    if echo "$tinfoil_proxy_api_key_response" | jq -e '.response_type == "error"' > /dev/null 2>&1; then
        log "Error: Failed to get Tinfoil Proxy API key"
        exit 1
    fi

    # Extract the encrypted API key value from the JSON structure
    if ! tinfoil_proxy_api_key_encrypted=$(echo "$tinfoil_proxy_api_key_response" | jq -r '.response_value | fromjson | .api_key' 2>/dev/null); then
        log "Error: Failed to extract Tinfoil Proxy API key from the response"
        exit 1
    fi
    if [ -z "$tinfoil_proxy_api_key_encrypted" ]; then
        log "Error: Failed to extract Tinfoil Proxy API key from the response"
        exit 1
    fi

    # Decrypt the API key using kmstool_enclave_cli
    decryption_output=$(kmstool_enclave_cli decrypt \
        --region "$region" \
        --proxy-port 8000 \
        --aws-access-key-id "$access_key_id" \
        --aws-secret-access-key "$secret_access_key" \
        --aws-session-token "$session_token" \
        --ciphertext "$tinfoil_proxy_api_key_encrypted" 2>&1)

    decrypted_api_key=$(echo "$decryption_output" | sed -n 's/PLAINTEXT: //p')

    if [ -z "$decrypted_api_key" ]; then
        log "Error: Failed to decrypt Tinfoil Proxy API key"
        exit 1
    fi

    # Base64 decode the decrypted API key
    if ! tinfoil_proxy_api_key=$(echo "$decrypted_api_key" | base64 -d 2>/dev/null); then
        log "Error: Failed to base64 decode Tinfoil Proxy API key"
        exit 1
    fi

    if [ -z "$tinfoil_proxy_api_key" ]; then
        log "Error: Failed to base64 decode Tinfoil Proxy API key"
        exit 1
    fi

else
    # For local mode, use the default OpenAI API base or the one set in the environment
    export OPENAI_API_BASE=${OPENAI_API_BASE:-"https://api.openai.com"}
    # No tinfoil proxy in local mode
    tinfoil_proxy_api_key="${TINFOIL_API_KEY:-}"
fi

# Start the opensecret
log "Starting opensecret..."
RUST_LOG_STYLE=never RUST_LOG="${RUST_LOG:-debug,hyper=info,aws_smithy_runtime=info,aws_smithy_runtime_api=info,aws_sdk_sqs=info,aws_config=info}" APP_MODE="$APP_MODE" OPENAI_API_BASE="$OPENAI_API_BASE" TINFOIL_API_KEY="$tinfoil_proxy_api_key" /app/opensecret &

# Wait for the opensecret to start
log "Waiting for opensecret to start"
sleep 5

# Start socat to forward from vsock to the opensecret
log "Starting socat..."
socat VSOCK-LISTEN:5000,reuseaddr,fork TCP:0.0.0.0:3000
