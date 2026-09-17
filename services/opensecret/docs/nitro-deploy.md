# Deploy on Nitro

This operator runbook remains manual. Run repository-local build and `just`
commands from `services/opensecret/` in the Maple monorepo using its pinned Nix
flake. The root EIF approval workflow builds dev/prod and compares measurements
without updating approvals. The reviewer-gated
[EIF release workflow](#eif-release-workflow) is the only CI path that signs.
For legacy client compatibility, follow
[the PCR publication procedure](pcr-compatibility.md).

## Current operator entrypoints

Use [isolated signing](../secretspec/README.md) for the existing SDK-trusted key.
The private deployment automation provides native SecretSpec deployment
entrypoints, per-environment JSON config and separate rollout state. Current
blue/green and provision EIF paths accept a reviewed source commit, immutable
Nix output and SHA-256; they do not build or sign. Older low-level infrastructure
examples below are reference material, not a substitute for these approval,
credential-isolation and publication gates.

## EIF release workflow

`opensecret-eif-release.yml` (`OpenSecret EIF release`) produces a signed
approval from CI. It is manual only: dispatch it on `master` with the
environment (`dev` or `prod`), and leave `dry_run` checked to build, attest and
compare without signing.

1. The `build` job runs on the trusted ARM64 runner with FlakeHub cache, builds
   the EIF, copies `image.eif` and `pcr.json` into a workflow artifact named
   `opensecret-eif-<env>-<commit>` together with `SHA256SUMS` and
   `handoff.json`, attests the artifact, and reports whether the approved
   snapshot already covers the measurements.
2. With `dry_run` unchecked and new measurements, the `sign` job waits for
   approval of the protected `pcr-signing` environment. It downloads the
   attested artifact, resolves the existing signing key through Bitwarden's
   Secrets Manager action using that environment's read-only machine token, and
   runs the same `append-pcr-<env>` recipe operators use. The key reaches only
   the `node pcr_sign.js` child; the verifier checks the signature against the
   pinned public key before the history is rewritten atomically. Setup is in
   [`secretspec/README.md`](../secretspec/README.md).
3. The job commits the two changed approval files to
   `opensecret/pcr-approval-<commit>` and prints the compare link. That branch
   is shared by both environments' runs for the same source commit: when both
   measurement sets change, the second run lands its commit on top of the
   first, so one pull request carries both approvals and each environment's
   approval check compares against its updated file. Open the pull request
   yourself; the approval checks then rebuild and compare on that PR, and the
   root `pcr-compatibility` check verifies every signature.
   After merging, mirror the four files to the legacy repository with the
   manual [compatibility procedure](pcr-compatibility.md) and verify both
   public locations.

The workflow never creates a GitHub Release or tag: `/releases/latest` and the
updater `latest.json` belong to the Maple desktop application. It does not
deploy.

### Deployment handoff from a release run

Deployment verifies an immutable Nix store artifact. Download the run's
artifact, verify its provenance, add it to the store, and pass the merged
approval commit:

```sh
gh run download RUN_ID --name opensecret-eif-prod-COMMIT12 --dir eif-candidate
gh attestation verify eif-candidate/image.eif --repo MaplePrivacyLabs/Maple
eif_dir="$(nix store add-path --name opensecret-eif-prod eif-candidate)"
export OPENSECRET_SOURCE_REF=MERGED_APPROVAL_COMMIT_SHA \
  OPENSECRET_EIF_DIR="$eif_dir" \
  OPENSECRET_EIF_SHA256="$(grep ' image.eif$' eif-candidate/SHA256SUMS | cut -d' ' -f1)"
```

The source ref must be the merged commit whose `pcr<Env>.json` matches the
artifact's `pcr.json`; the deployment verifier rejects anything else.

## CI approval checks

`opensecret-eif.yml` compares generated measurements with the approved JSON
files on Linux ARM64. A PR runs these comparisons only if its own diff edits
`pcrDev.json`, `pcrProd.json`, `pcrDevHistory.json`, or `pcrProdHistory.json`
under `services/opensecret/`. Backend code changes alone do not require new
approvals to pass PR CI. The PR's checkout supplies both source and references.

Master compares on backend/TEE build-input, approved-PCR, and EIF-check tooling
changes. A mismatch intentionally leaves the distinct EIF/PCR approval check
red until an operator reviews and updates approvals. Unrelated client-only or
documentation changes skip it. Manual dispatch checks both environments.
If a PR's changed files cannot be determined, routing fails explicitly instead
of treating missing information as an approval edit.

Signed-history validation remains available through the lightweight
monorepo-root `pcr-compatibility` Nix check and the manual publication procedure.
There is no standalone backend dev-shell job for it. Neither a matching EIF nor
green CI verifies both public publication locations, live KMS policy, or the
running enclave, and neither authorizes deployment. Builds use normal Nix
cache semantics; this is measurement parity, not a forced independent rebuild.
The approval checks never update references or handle signing keys; only the
reviewer-gated release workflow above does, and it publishes a review branch
rather than writing to `master`.

### Binary caches and cold-run validation

Master push/manual runs and same-repository PR comparisons use the trusted EIF
job, which installs Determinate Nix and uses FlakeHub Cache with job-scoped
`id-token: write`. A PR qualifies only when its head repository's full name
equals `github.repository` and the successful selector reports an approval
JSON edit. This intentionally trusts same-repository PR code to write the
FlakeHub cache; it does not grant signing or deployment authority.
Trusted jobs use the organization-configured GitHub-hosted runner
`ubuntu-24.04-arm64-8core` (Ubuntu 24.04 ARM64, 8 CPU, 32 GB RAM) and allow
180 minutes for cold kernel builds. Its runner group must allow the public
Maple repository, with capacity for both dev/prod jobs. The existing
`ubuntu-latest-8-cores` runner is x86-64, not a substitute. Unprivileged jobs
retain the standard `ubuntu-24.04-arm` runner and their 90-minute limit.

The trusted job also explicitly enables the GitHub cache and `diff-store: true`,
so paths fetched from FlakeHub, not just locally built paths, populate that
cache. Master runs warm the default-branch cache. GitHub permits reads from the
default/base-branch cache, but PR writes are confined to the PR merge ref and
cannot populate master's GitHub cache, even for same-repository PRs. The cache
action's post step can run after an expected PCR mismatch; the comparison still
fails and approvals stay unchanged.

Fork PRs, PRs with missing head-repository metadata, and manual runs on other
refs have no OIDC permission. They use the pinned Magic Nix Cache action with
FlakeHub disabled and GitHub caching enabled.
Do not use `pull_request_target`, pass cache secrets to PRs, or change the
checkout to trusted master while claiming to check a PR's source.

FlakeHub access is repository/organization-scoped, and fork PRs cannot
authenticate to it. Restoring the action does not grant Maple access to
`OpenSecretCloud/opensecret` cache entries. A new identity or an empty/evicted
GitHub cache may need operator-approved access or a trusted cache-warming run.
Never assume the old cache's visibility transferred with the source import.

After a cache change, inspect fresh hosted ARM64 master and same-repository PR
runs for successful FlakeHub authentication, actual substitution of the expected
custom kernel store path, build duration, and the eventual measurement comparison.
Then verify that an unprivileged fork run can reuse the GitHub cache warmed by
master. A warm local store, a skipped PR EIF job, or passing workflow unit tests
does not prove this.
Diagnose missing cache access separately from PCR mismatch. A longer timeout
provides cold-build headroom, not proof of working cache access or reuse.
Preserve measured kernel/build inputs when fixing cache availability.

## Log into AWS CLI 

This should be after creating an IAM profile with admin access. 

For the first time: 
```
aws configure sso
```

For logging in to an existing profile (replace with your own):
```
aws sso login --profile AdministratorAccess-1111
```

## Create an SSH Keypair for logging into the machine:

Read up on the [docs](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/create-key-pairs.html). Either import existing or create a new one.

To ge the public key of an existing `.pem` file:

```
ssh-keygen -y -f ~/.ssh/your_ssh.pem 
```

## Create a container that is nitro compatible

Read up on the [docs](https://docs.aws.amazon.com/enclaves/latest/user/getting-started.html) if needed.

Use this ec2 command:


Replace `AWS_PROFILE` with your CLI access name. Ex. `AdministratorAccess-1111`
Replace `KEY_NAME` with the uploaded ssh key pair name. Ex. `opensecret-admin`

```
aws ec2 run-instances \
--image-id ami-067df2907035c28c2 \
--count 1 \
--instance-type m6g.2xlarge \
--enclave-options 'Enabled=true' \
--block-device-mappings "[{\"DeviceName\":\"/dev/xvda\",\"Ebs\":{\"VolumeSize\":20,\"DeleteOnTermination\":true}}]" \
--key-name $KEY_NAME \
--profile $AWS_PROFILE
```

These are the image types, ARM vs x86:

```
ami-05c3dc660cb6907f0 (64-bit (x86), uefi-preferred) - m6a.xlarge
ami-067df2907035c28c2 (64-bit (Arm), uefi) - m6g.xlarge
```

Log into the AWS console and get the IP address of the EC2 instance.

Add a new security group rule for allowing SSH access from IPv4 and IPv6. Also allow 80 and 443 while you are here.

## Basic server configurations after creation: 

Get the current IP address and ssh in:

```
ssh ec2-user@ec2-[your-ip].us-east-2.compute.amazonaws.com -i ~/.ssh/your_ssh_key.pem
```

Upgrade if needed:

```
/usr/bin/dnf check-release-update
```

## Install packages


Much of this comes from the nitro workshop: https://catalog.workshops.aws/nitro-enclaves/en-US/0-getting-started

Install nitro CLI things: 

```
sudo dnf install aws-nitro-enclaves-cli -y
```

```
sudo dnf install socat -y
```

```
sudo dnf install aws-nitro-enclaves-cli-devel -y
```

```
sudo usermod -aG ne ec2-user
```

```
sudo usermod -aG docker ec2-user
```

```
sudo systemctl enable --now docker
```

Verify:

```
nitro-cli --version
```

Configure nitro enclaves:

```
sudo vim /etc/nitro_enclaves/allocator.yaml
```

Basic recommendation:
```
# How much memory to allocate for enclaves (in MiB).
memory_mib: 21504
#
# How many CPUs to reserve for enclaves.
cpu_count: 6
```

Enable them:

```
sudo systemctl enable --now nitro-enclaves-allocator.service
```

If you need to reconfig and then restart:

```
sudo systemctl restart nitro-enclaves-allocator.service
```

## App deployment

When deploying the app in the Nitro enclave, make sure to set the `APP_MODE` to one of:
- `dev` for development environment
- `preview` for preview/staging environment
- `prod` for production environment

The application also supports a `custom` mode with `ENV_NAME`, but the current
flake does not export a custom EIF. Supporting one requires adding and reviewing
a named flake output. `ENV_NAME` is used to:
- Form the KMS key alias (`alias/open-secret-{env_name}-enclave`)
- Form the database URL secret name (`opensecret_{env_name}_database_url`)
- Form the Continuum proxy API key secret name (`continuum_proxy_{env_name}_api_key`)
- Form the Tinfoil API key secret name (`tinfoil_proxy_{env_name}_api_key`;
  the historical secret name is retained for deployment compatibility)

The named Nix outputs are the only supported OpenSecret application EIF build
path. The parent-instance credential requester and logging containers described
later in this guide are operational services, not EIF build inputs.

### Building and Deploying with Nix (Recommended)

The recommended way to build and deploy the enclave is using Nix, which provides reproducible builds:

1. Nix builds the fixed-source Nitro helper closure automatically. To inspect
   it independently on Linux ARM:
```bash
just build-nitro-bins
```

2. Build the EIF for your target environment:
```bash
# For development
nix build --no-update-lock-file '.?submodules=1#eif-dev'

# For production
nix build --no-update-lock-file '.?submodules=1#eif-prod'

# For preview
nix build --no-update-lock-file '.?submodules=1#eif-preview'
```

This will create a symlink `result` pointing to the built EIF file.

3. Copy the EIF to your AWS parent instance:
```bash
# For development
just scp-eif-to-aws-dev

# For production
just scp-eif-to-aws-prod

# For preview
just scp-eif-to-aws-preview
```

4. Deploy the EIF:
```bash
# For development
just deploy-dev-nix

# For production
just deploy-prod-nix

# For preview
just deploy-preview-nix
```

The deployment process will:
1. Build the EIF
2. Copy it to the AWS parent instance
3. Prompt you to review the PCR values
4. After confirmation, terminate any existing enclave
5. Run the new enclave
6. Restart the socat proxy

### PCR Value Management

The Nix build process generates PCR (Platform Configuration Register) values that are used by AWS KMS for attestation. You can:

1. Copy PCR values to a reference file:
```bash
just copy-pcr-dev    # For development
just copy-pcr-prod   # For production
just copy-pcr-preview # For preview
```

2. Verify PCR values match the reference:
```bash
just verify-pcr-dev    # For development
just verify-pcr-prod   # For production
just verify-pcr-preview # For preview
```

This ensures the build is reproducible and matches the expected configuration.

## Setup SSL

Install nginx:

```
sudo dnf install nginx -y
```

Install acm:

```
sudo dnf install aws-nitro-enclaves-acm -y
```


Follow instructions for configuring [nginx](https://docs.aws.amazon.com/enclaves/latest/user/nitro-enclave-refapp.html) in the enclave.

Make the proxy timeouts explicit in the nginx `location` block that forwards traffic to
the local socat listener. The nginx defaults include 60s upstream read/send timeouts, which
is too short for long non-streaming completions where the enclave does not send response
bytes until the model response is complete.

Example `location` block:

```nginx
location / {
    proxy_pass http://localhost:8080;
    proxy_http_version 1.1;

    proxy_set_header Host $host;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;

    proxy_connect_timeout 30s;
    proxy_send_timeout 300s;
    proxy_read_timeout 300s;
    send_timeout 300s;

    # Do not buffer SSE responses from streaming completion endpoints.
    proxy_buffering off;
}
```

If the existing config already has a `location /` with `proxy_pass` and forwarded headers,
the minimal required change is to add these directives inside that block:

```nginx
proxy_connect_timeout 30s;
proxy_send_timeout 300s;
proxy_read_timeout 300s;
send_timeout 300s;
proxy_buffering off;
```

After updating nginx:

```bash
sudo nginx -t
sudo systemctl reload nginx
```

`proxy_read_timeout` is an idle timeout between reads from the upstream server, not a total
request duration. For non-streaming completion requests, set it above the longest expected
time-to-first-byte from the enclave.

The public API hostname is also Cloudflare-proxied in production. Cloudflare has a separate
origin read timeout; if that expires, clients should see a Cloudflare 524 response. A
completion request that fails at roughly 60 seconds with HTTP 504 should be checked against
the origin nginx configuration first. For model runs that can exceed Cloudflare's current
proxied-request timeout, prefer streaming completions or use a Cloudflare configuration that
supports a longer proxy read timeout for the API hostname.

After following the instructions, reboot the machine. And then start up this enclave program again.


## Socat proxy for SSL
Create a socat proxy service so that HTTP program on port 8080 can talk to the enclave:

```
sudo vim /etc/systemd/system/socat-proxy.service
```

Put in this info:

```
[Unit]
Description=Socat Proxy for Nitro Enclave
After=network.target

[Service]
Type=simple
User=ec2-user
ExecStart=/bin/bash -c 'ENCLAVES=$(nitro-cli describe-enclaves); echo "Enclaves: $ENCLAVES"; ENCLAVE_CID=$(echo "$ENCLAVES" | jq -r '\''.[] | select(.EnclaveName == "opensecret") | .EnclaveCID'\''); echo "ENCLAVE_CID: $ENCLAVE_CID"; if [ -n "$ENCLAVE_CID" ]; then socat TCP-LISTEN:8080,reuseaddr,fork VSOCK-CONNECT:$ENCLAVE_CID:5000; else echo "Enclave not found" >&2; exit 1; fi'
Restart=always

[Install]
WantedBy=multi-user.target
```

Activate service:
```
sudo systemctl daemon-reload
sudo systemctl enable socat-proxy.service
sudo systemctl start socat-proxy.service
sudo systemctl status socat-proxy.service
```

Restart socat proxy anytime there is a change to the enclave program:
```
sudo systemctl restart socat-proxy.service
```

## Vsock DB proxy
Create a vsock proxy service so that enclave program can talk to the database:

First configure the endpoint into it's allowlist:

```
sudo vim /etc/nitro_enclaves/vsock-proxy.yaml
```

```
- {address: [YOUR-DB-ENDPOINT].us-east-2.aws.neon.tech, port: 5432}
```

Now create a service that spins this up automatically:

```
sudo vim /etc/systemd/system/vsock-db-proxy.service
```

```
[Unit]
Description=Vsock DB Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 30 8001 [YOUR-DB-ENDPOINT].us-east-2.aws.neon.tech 5432
Restart=always

[Install]
WantedBy=multi-user.target
```

Activate service:

```
sudo systemctl daemon-reload
sudo systemctl enable vsock-db-proxy.service
sudo systemctl start vsock-db-proxy.service
sudo systemctl status vsock-db-proxy.service
```

A restart of this should not be needed but if you need to
```
sudo systemctl restart vsock-db-proxy.service
```

Note: The vsock-proxy is configured with 30 workers to match the application's database connection pool size of 20, providing some headroom for concurrent connections.

## Vsock GitHub OAuth proxy
Create a vsock proxy service so that enclave program can talk to GitHub:

First configure the endpoints into their allowlist:

```
sudo vim /etc/nitro_enclaves/vsock-proxy.yaml
```

Add these lines:
```
- {address: github.com, port: 443}
- {address: api.github.com, port: 443}
```

Now create services that spin these up automatically:

```
sudo vim /etc/systemd/system/vsock-github-proxy.service
```

```
[Unit]
Description=Vsock GitHub Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8012 github.com 443
Restart=always

[Install]
WantedBy=multi-user.target
```

```
sudo vim /etc/systemd/system/vsock-github-api-proxy.service
```

```
[Unit]
Description=Vsock GitHub API Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8013 api.github.com 443
Restart=always

[Install]
WantedBy=multi-user.target
```

Activate services:

```
sudo systemctl daemon-reload
sudo systemctl enable vsock-github-proxy.service
sudo systemctl start vsock-github-proxy.service
sudo systemctl status vsock-github-proxy.service
sudo systemctl enable vsock-github-api-proxy.service
sudo systemctl start vsock-github-api-proxy.service
sudo systemctl status vsock-github-api-proxy.service
```

A restart of these should not be needed but if you need to:
```
sudo systemctl restart vsock-github-proxy.service
sudo systemctl restart vsock-github-api-proxy.service
```

## Vsock Google OAuth proxy
Create vsock proxy services so that enclave program can talk to Google OAuth:

First configure the endpoints into their allowlist:

```
sudo vim /etc/nitro_enclaves/vsock-proxy.yaml
```

Add these lines:
```
- {address: oauth2.googleapis.com, port: 443}
- {address: www.googleapis.com, port: 443}
```

Now create services that spin these up automatically:

```
sudo vim /etc/systemd/system/vsock-google-oauth-proxy.service
```

```
[Unit]
Description=Vsock Google OAuth Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8014 oauth2.googleapis.com 443
Restart=always

[Install]
WantedBy=multi-user.target
```

```
sudo vim /etc/systemd/system/vsock-google-api-proxy.service
```

```
[Unit]
Description=Vsock Google API Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8015 www.googleapis.com 443
Restart=always

[Install]
WantedBy=multi-user.target
```

Activate services:

```
sudo systemctl daemon-reload
sudo systemctl enable vsock-google-oauth-proxy.service
sudo systemctl start vsock-google-oauth-proxy.service
sudo systemctl status vsock-google-oauth-proxy.service
sudo systemctl enable vsock-google-api-proxy.service
sudo systemctl start vsock-google-api-proxy.service
sudo systemctl status vsock-google-api-proxy.service
```

A restart of these should not be needed but if you need to:
```
sudo systemctl restart vsock-google-oauth-proxy.service
sudo systemctl restart vsock-google-api-proxy.service
```

## Vsock Apple OAuth proxy
Create a vsock proxy service so that enclave program can talk to Apple OAuth:

First configure the endpoint into its allowlist:

```
sudo vim /etc/nitro_enclaves/vsock-proxy.yaml
```

Add this line:
```
- {address: appleid.apple.com, port: 443}
```

Now create a service that spins this up automatically:

```
sudo vim /etc/systemd/system/vsock-apple-proxy.service
```

```
[Unit]
Description=Vsock Apple OAuth Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8018 appleid.apple.com 443
Restart=always

[Install]
WantedBy=multi-user.target
```

Activate service:

```
sudo systemctl daemon-reload
sudo systemctl enable vsock-apple-proxy.service
sudo systemctl start vsock-apple-proxy.service
sudo systemctl status vsock-apple-proxy.service
```

A restart of this should not be needed but if you need to:
```
sudo systemctl restart vsock-apple-proxy.service
```

## Vsock Resend proxy
Create a vsock proxy service so that enclave program can talk to resend:

First configure the endpoint into it's allowlist:

```
sudo vim /etc/nitro_enclaves/vsock-proxy.yaml
```

```
- {address: api.resend.com, port: 443}
```

Now create a service that spins this up automatically:

```
sudo vim /etc/systemd/system/vsock-resend-proxy.service
```

```
[Unit]
Description=Vsock Resend Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8010 api.resend.com 443
Restart=always

[Install]
WantedBy=multi-user.target
```

Activate service:

```
sudo systemctl daemon-reload
sudo systemctl enable vsock-resend-proxy.service
sudo systemctl start vsock-resend-proxy.service
sudo systemctl status vsock-resend-proxy.service
```

A restart of this should not be needed but if you need to
```
sudo systemctl restart vsock-resend-proxy.service
```


## Vsock Continuum API proxy
Create a vsock proxy service so that the continuum-proxy can talk to the Continuum API:

First configure the endpoint into its allowlist:

```sh
sudo vim /etc/nitro_enclaves/vsock-proxy.yaml
```

Add these lines:
```
- {address: kdsintf.amd.com, port: 443}
- {address: secret.privatemode.ai, port: 443}
- {address: cdn.confidential.cloud, port: 443}
- {address: api.privatemode.ai, port: 443}
- {address: coordinator.privatemode.ai, port: 443}
- {address: api.trustedservices.intel.com, port: 443}
- {address: certificates.trustedservices.intel.com, port: 443}
```

Restart the nitro vsock proxy service:
```
sudo systemctl restart nitro-enclaves-vsock-proxy.service
```

#### Continuum API
Now create a service that spins this up automatically:

```
sudo vim /etc/systemd/system/vsock-continuum-proxy.service
```

Add the following content:
```
[Unit]
Description=Vsock Continuum API Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8004 api.privatemode.ai 443
Restart=always

[Install]
WantedBy=multi-user.target
```

#### Continuum CDN
```
sudo vim /etc/systemd/system/vsock-continuum-cdn.service
```

Add the following content:
```
[Unit]
Description=Vsock Continuum CDN Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8005 cdn.confidential.cloud 443
Restart=always

[Install]
WantedBy=multi-user.target
```

#### Continuum Secret Service
```
sudo vim /etc/systemd/system/vsock-continuum-secret.service
```

Add the following content:
```
[Unit]
Description=Vsock Continuum Secret Service Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8006 secret.privatemode.ai 443
Restart=always

[Install]
WantedBy=multi-user.target
```

#### Continuum Coordinator
```
sudo vim /etc/systemd/system/vsock-continuum-coordinator.service
```

Add the following content:
```
[Unit]
Description=Vsock Continuum Coordinator Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8007 coordinator.privatemode.ai 443
Restart=always

[Install]
WantedBy=multi-user.target
```

#### AMD KDS Interface
```
sudo vim /etc/systemd/system/vsock-amd-kds.service
```

Add the following content:
```
[Unit]
Description=Vsock AMD KDS Interface Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8008 kdsintf.amd.com 443
Restart=always

[Install]
WantedBy=multi-user.target
```

#### Intel PCS API
```
sudo vim /etc/systemd/system/vsock-intel-pcs-api.service
```

Add the following content:
```
[Unit]
Description=Vsock Intel PCS API Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8048 api.trustedservices.intel.com 443
Restart=always

[Install]
WantedBy=multi-user.target
```

#### Intel PCS Certificates
```
sudo vim /etc/systemd/system/vsock-intel-pcs-certs.service
```

Add the following content:
```
[Unit]
Description=Vsock Intel PCS Certificates Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8049 certificates.trustedservices.intel.com 443
Restart=always

[Install]
WantedBy=multi-user.target
```

Activate the services:

```
sudo systemctl daemon-reload
sudo systemctl enable vsock-continuum-proxy.service
sudo systemctl start vsock-continuum-proxy.service
sudo systemctl status vsock-continuum-proxy.service
sudo systemctl enable vsock-continuum-cdn.service
sudo systemctl start vsock-continuum-cdn.service
sudo systemctl status vsock-continuum-cdn.service
sudo systemctl enable vsock-continuum-secret.service
sudo systemctl start vsock-continuum-secret.service
sudo systemctl status vsock-continuum-secret.service
sudo systemctl enable vsock-continuum-coordinator.service
sudo systemctl start vsock-continuum-coordinator.service
sudo systemctl status vsock-continuum-coordinator.service
sudo systemctl enable vsock-amd-kds.service
sudo systemctl start vsock-amd-kds.service
sudo systemctl status vsock-amd-kds.service
sudo systemctl enable vsock-intel-pcs-api.service
sudo systemctl start vsock-intel-pcs-api.service
sudo systemctl status vsock-intel-pcs-api.service
sudo systemctl enable vsock-intel-pcs-certs.service
sudo systemctl start vsock-intel-pcs-certs.service
sudo systemctl status vsock-intel-pcs-certs.service
```

If you need to restart these services:
```
sudo systemctl restart vsock-continuum-proxy.service
sudo systemctl restart vsock-continuum-cdn.service
sudo systemctl restart vsock-continuum-secret.service
sudo systemctl restart vsock-continuum-coordinator.service
sudo systemctl restart vsock-amd-kds.service
sudo systemctl restart vsock-intel-pcs-api.service
sudo systemctl restart vsock-intel-pcs-certs.service
```

#### Vsock AWS SQS proxy
Create a vsock proxy service so that enclave program can talk to AWS SQS:

First configure the endpoint into its allowlist:

```sh
sudo vim /etc/nitro_enclaves/vsock-proxy.yaml
```

Add this line:
```
- {address: sqs.us-east-2.amazonaws.com, port: 443}
```

Now create a service that spins this up automatically:

```sh
sudo vim /etc/systemd/system/vsock-sqs-proxy.service
```

```
[Unit]
Description=Vsock AWS SQS Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8016 sqs.us-east-2.amazonaws.com 443
Restart=always

[Install]
WantedBy=multi-user.target
```

Activate the service:

```sh
sudo systemctl daemon-reload
sudo systemctl enable vsock-sqs-proxy.service
sudo systemctl start vsock-sqs-proxy.service
sudo systemctl status vsock-sqs-proxy.service
```

A restart should not be needed but if you need to:
```sh
sudo systemctl restart vsock-sqs-proxy.service
```

## Vsock Billing proxy
Create a vsock proxy service so that enclave program can talk to the billing service:

First configure the endpoints into their allowlist:

```sh
sudo vim /etc/nitro_enclaves/vsock-proxy.yaml
```

Add one of these lines depending on your environment:
```
- {address: billing-dev.opensecret.cloud, port: 443}  # for dev environment
- {address: billing.opensecret.cloud, port: 443}      # for prod environment
```

Now create a service that spins this up automatically:

```sh
sudo vim /etc/systemd/system/vsock-billing-proxy.service
```

```
[Unit]
Description=Vsock Billing Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8017 billing-dev.opensecret.cloud 443  # Change to billing.opensecret.cloud for prod
Restart=always

[Install]
WantedBy=multi-user.target
```

Activate the service:

```sh
sudo systemctl daemon-reload
sudo systemctl enable vsock-billing-proxy.service
sudo systemctl start vsock-billing-proxy.service
sudo systemctl status vsock-billing-proxy.service
```

A restart should not be needed but if you need to:
```sh
sudo systemctl restart vsock-billing-proxy.service
```

## Vsock os-flags proxy
Create a vsock proxy service so that enclave program can talk to the os-flags feature flag service:

First configure the endpoints into their allowlist:

```sh
sudo vim /etc/nitro_enclaves/vsock-proxy.yaml
```

Add one of these lines depending on your environment:
```
- {address: flags-dev.opensecret.cloud, port: 443}  # for dev/preview/custom environments
- {address: flags.opensecret.cloud, port: 443}      # for prod environment
```

Now create a service that spins this up automatically:

```sh
sudo vim /etc/systemd/system/vsock-os-flags-proxy.service
```

```
[Unit]
Description=Vsock os-flags Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8028 flags-dev.opensecret.cloud 443  # Change to flags.opensecret.cloud for prod
Restart=always

[Install]
WantedBy=multi-user.target
```

Activate the service:

```sh
sudo systemctl daemon-reload
sudo systemctl enable vsock-os-flags-proxy.service
sudo systemctl start vsock-os-flags-proxy.service
sudo systemctl status vsock-os-flags-proxy.service
```

A restart should not be needed but if you need to:
```sh
sudo systemctl restart vsock-os-flags-proxy.service
```

## Vsock Kagi Search proxy
Create a vsock proxy service so that enclave program can talk to the Kagi Search API:

First configure the endpoint into its allowlist:

```sh
sudo vim /etc/nitro_enclaves/vsock-proxy.yaml
```

Add this line:
```
- {address: kagi.com, port: 443}
```

Now create a service that spins this up automatically:

```sh
sudo vim /etc/systemd/system/vsock-kagi-proxy.service
```

```
[Unit]
Description=Vsock Kagi Search Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy 8026 kagi.com 443
Restart=always

[Install]
WantedBy=multi-user.target
```

Activate the service:

```sh
sudo systemctl daemon-reload
sudo systemctl enable vsock-kagi-proxy.service
sudo systemctl start vsock-kagi-proxy.service
sudo systemctl status vsock-kagi-proxy.service
```

A restart should not be needed but if you need to:
```sh
sudo systemctl restart vsock-kagi-proxy.service
```

## Vsock Tinfoil proxies
Create vsock proxy services so that the in-process Tinfoil Rust SDK can talk to Tinfoil services:

First configure the endpoints into their allowlist:

```sh
sudo vim /etc/nitro_enclaves/vsock-proxy.yaml
```

Add these lines:
```
- {address: github-proxy.tinfoil.sh, port: 443}
# Retained unchanged by this migration; tinfoil-rs v0.2.0 embeds its trust root.
- {address: tuf-repo-cdn.sigstore.dev, port: 443}
- {address: kds-proxy.tinfoil.sh, port: 443}
- {address: atc.tinfoil.sh, port: 443}
- {address: inference.tinfoil.sh, port: 443}
# New router naming (preferred)
- {address: router-0.tinfoil.sh, port: 443}
- {address: router-1.tinfoil.sh, port: 443}
- {address: router-2.tinfoil.sh, port: 443}
- {address: router-3.tinfoil.sh, port: 443}
- {address: router-4.tinfoil.sh, port: 443}
- {address: router-5.tinfoil.sh, port: 443}
# Additional router naming (retain for discovery and fallback)
- {address: router.inf4.tinfoil.sh, port: 443}
- {address: router.inf5.tinfoil.sh, port: 443}
- {address: router.inf6.tinfoil.sh, port: 443}
- {address: router.inf7.tinfoil.sh, port: 443}
- {address: router.inf8.tinfoil.sh, port: 443}
- {address: router.inf9.tinfoil.sh, port: 443}
- {address: router.inf10.tinfoil.sh, port: 443}
```

The TUF CDN entry and service below are retained to keep existing entrypoint
and host behavior unchanged during this minimal migration. The in-process SDK
does not use them.

Restart the nitro vsock proxy service:
```
sudo systemctl restart nitro-enclaves-vsock-proxy.service
```

### Migrating numbered routers from `.dev` to `.sh`

Replace the numbered `router-0` through `router-5` `.tinfoil.dev` destinations
with their `.tinfoil.sh` equivalents in both the parent-host allowlist and the
six systemd units below. Preserve the VSOCK ports (8042–8047), unit names, and
enclave loopback addresses. Keep `inference.tinfoil.sh` and the existing
`router.inf*.tinfoil.sh` entries and services, including `router.inf6.tinfoil.sh`.

The in-process SDK queries `https://atc.tinfoil.sh/routers?platform=snp`, tries
returned routers in order, and falls back to attesting `inference.tinfoil.sh`.
OpenSecret bounds each discovery attempt to 30 seconds and retries discovery
in the background if unavailable. A DNS/TCP/TLS connection failure triggers
rediscovery and one request retry; ambiguous sends and interrupted streams are
not replayed. Treat fallback as recovery, not a guarantee of error-free cutover.

For existing hosts, coordinate the update with the normal authorized rollout:

1. Inspect and back up each parent's actual allowlist and router unit files;
   confirm the running EIF and that the retained fallback proxy services work.
   Check the current ATC response rather than assuming its router list is fixed.
2. On a drained development parent, update the allowlist and numbered router
   unit destinations. Run `systemctl daemon-reload`, restart the affected proxy
   services, and verify their status. Preserve the fallback services throughout.
3. Deploy the development EIF containing the matching `entrypoint.sh` hostname
   mappings, built and approved beforehand through the normal
   [PCR compatibility procedure](pcr-compatibility.md). Check
   attested provider connectivity through the enclave and an encrypted client
   request; a process health check alone does not prove Tinfoil connectivity.
   Return the validated parent to traffic, then repeat on the other development
   parent.
4. Apply the validated parent configuration and approved production EIF to
   production one drained parent at a time. Verify provider connectivity, client
   behavior, and the expected PCR before returning each parent to traffic.

Parent configuration and EIF rollout are separate operations. The standard EIF
rollout does not install these outbound proxy unit changes, and updating this
document does not change running hosts. An old EIF still maps `.dev` names while
the updated proxies connect to `.sh` destinations; do not rely on those numbered
routes during that intermediate state. Retained fallback routes support the
transition. Keep backups and the prior approved EIF for coordinated rollback;
restoring an old `.dev` destination does not restore an endpoint retired upstream.

All Tinfoil `vsock-proxy` services below should use an elevated worker count. The AWS `vsock-proxy` default is 4 simultaneous connections, which is too low for long-lived streaming completions plus health checks. Start with `--num_workers 128` on production-sized instances, and keep `LimitNOFILE` high enough for the additional sockets.

#### Tinfoil GitHub Proxy
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-api-github-proxy.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil GitHub Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8019 github-proxy.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### TUF Repository CDN
```sh
sudo vim /etc/systemd/system/vsock-tuf-repo-cdn.service
```

Add the following content:
```ini
[Unit]
Description=Vsock TUF Repository CDN Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8020 tuf-repo-cdn.sigstore.dev 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil KDS Proxy
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-kds-proxy.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil KDS Proxy Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8022 kds-proxy.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil ATC
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-atc.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil ATC Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8021 atc.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil Inference
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-inference.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil Inference Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8041 inference.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil Router 0
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-router-0.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil Router 0 Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8042 router-0.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil Router 1
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-router-1.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil Router 1 Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8043 router-1.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil Router 2
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-router-2.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil Router 2 Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8044 router-2.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil Router 3
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-router-3.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil Router 3 Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8045 router-3.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil Router 4
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-router-4.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil Router 4 Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8046 router-4.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil Router 5
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-router-5.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil Router 5 Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8047 router-5.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

The following services use the `router.inf*.tinfoil.sh` naming. Keep them enabled; Tinfoil still advertises endpoints in this family, including `router.inf6.tinfoil.sh`.

#### Tinfoil Router Inf4 (retained)
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-router-inf4.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil Router Inf4 Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8034 router.inf4.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil Router Inf5 (retained)
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-router-inf5.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil Router Inf5 Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8035 router.inf5.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil Router Inf6 (retained)
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-router-inf6.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil Router Inf6 Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8036 router.inf6.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil Router Inf7 (retained)
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-router-inf7.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil Router Inf7 Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8037 router.inf7.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil Router Inf8 (retained)
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-router-inf8.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil Router Inf8 Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8038 router.inf8.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil Router Inf9 (retained)
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-router-inf9.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil Router Inf9 Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8039 router.inf9.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

#### Tinfoil Router Inf10 (retained)
```sh
sudo vim /etc/systemd/system/vsock-tinfoil-router-inf10.service
```

Add the following content:
```
[Unit]
Description=Vsock Tinfoil Router Inf10 Service
After=network.target

[Service]
User=root
ExecStart=/usr/bin/vsock-proxy --num_workers 128 8040 router.inf10.tinfoil.sh 443
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

Activate all the services (including the deprecated legacy router services during migration):

```sh
sudo systemctl daemon-reload
sudo systemctl enable vsock-tinfoil-api-github-proxy.service
sudo systemctl start vsock-tinfoil-api-github-proxy.service
sudo systemctl status vsock-tinfoil-api-github-proxy.service
sudo systemctl enable vsock-tuf-repo-cdn.service
sudo systemctl start vsock-tuf-repo-cdn.service
sudo systemctl status vsock-tuf-repo-cdn.service
sudo systemctl enable vsock-tinfoil-kds-proxy.service
sudo systemctl start vsock-tinfoil-kds-proxy.service
sudo systemctl status vsock-tinfoil-kds-proxy.service
sudo systemctl enable vsock-tinfoil-atc.service
sudo systemctl start vsock-tinfoil-atc.service
sudo systemctl status vsock-tinfoil-atc.service
sudo systemctl enable vsock-tinfoil-inference.service
sudo systemctl start vsock-tinfoil-inference.service
sudo systemctl status vsock-tinfoil-inference.service
sudo systemctl enable vsock-tinfoil-router-0.service
sudo systemctl start vsock-tinfoil-router-0.service
sudo systemctl status vsock-tinfoil-router-0.service
sudo systemctl enable vsock-tinfoil-router-1.service
sudo systemctl start vsock-tinfoil-router-1.service
sudo systemctl status vsock-tinfoil-router-1.service
sudo systemctl enable vsock-tinfoil-router-2.service
sudo systemctl start vsock-tinfoil-router-2.service
sudo systemctl status vsock-tinfoil-router-2.service
sudo systemctl enable vsock-tinfoil-router-3.service
sudo systemctl start vsock-tinfoil-router-3.service
sudo systemctl status vsock-tinfoil-router-3.service
sudo systemctl enable vsock-tinfoil-router-4.service
sudo systemctl start vsock-tinfoil-router-4.service
sudo systemctl status vsock-tinfoil-router-4.service
sudo systemctl enable vsock-tinfoil-router-5.service
sudo systemctl start vsock-tinfoil-router-5.service
sudo systemctl status vsock-tinfoil-router-5.service
sudo systemctl enable vsock-tinfoil-router-inf4.service
sudo systemctl start vsock-tinfoil-router-inf4.service
sudo systemctl status vsock-tinfoil-router-inf4.service
sudo systemctl enable vsock-tinfoil-router-inf5.service
sudo systemctl start vsock-tinfoil-router-inf5.service
sudo systemctl status vsock-tinfoil-router-inf5.service
sudo systemctl enable vsock-tinfoil-router-inf6.service
sudo systemctl start vsock-tinfoil-router-inf6.service
sudo systemctl status vsock-tinfoil-router-inf6.service
sudo systemctl enable vsock-tinfoil-router-inf7.service
sudo systemctl start vsock-tinfoil-router-inf7.service
sudo systemctl status vsock-tinfoil-router-inf7.service
sudo systemctl enable vsock-tinfoil-router-inf8.service
sudo systemctl start vsock-tinfoil-router-inf8.service
sudo systemctl status vsock-tinfoil-router-inf8.service
sudo systemctl enable vsock-tinfoil-router-inf9.service
sudo systemctl start vsock-tinfoil-router-inf9.service
sudo systemctl status vsock-tinfoil-router-inf9.service
sudo systemctl enable vsock-tinfoil-router-inf10.service
sudo systemctl start vsock-tinfoil-router-inf10.service
sudo systemctl status vsock-tinfoil-router-inf10.service
```

If you need to restart these services:
```sh
sudo systemctl restart vsock-tinfoil-api-github-proxy.service
sudo systemctl restart vsock-tuf-repo-cdn.service
sudo systemctl restart vsock-tinfoil-kds-proxy.service
sudo systemctl restart vsock-tinfoil-atc.service
sudo systemctl restart vsock-tinfoil-inference.service
sudo systemctl restart vsock-tinfoil-router-0.service
sudo systemctl restart vsock-tinfoil-router-1.service
sudo systemctl restart vsock-tinfoil-router-2.service
sudo systemctl restart vsock-tinfoil-router-3.service
sudo systemctl restart vsock-tinfoil-router-4.service
sudo systemctl restart vsock-tinfoil-router-5.service
sudo systemctl restart vsock-tinfoil-router-inf4.service
sudo systemctl restart vsock-tinfoil-router-inf5.service
sudo systemctl restart vsock-tinfoil-router-inf6.service
sudo systemctl restart vsock-tinfoil-router-inf7.service
sudo systemctl restart vsock-tinfoil-router-inf8.service
sudo systemctl restart vsock-tinfoil-router-inf9.service
sudo systemctl restart vsock-tinfoil-router-inf10.service
```

## KMS Key

You need to create an AWS KMS key that the enclave can encrypt/decrypt things to. Name it according to your environment:
- `open-secret-dev-enclave` for dev environment
- `open-secret-preview1-enclave` for preview environment
- `open-secret-prod-enclave` for prod environment
- `open-secret-{env_name}-enclave` for custom environments (replace `{env_name}` with your ENV_NAME)

Here is an example policy, replace with your values:

```json
{
    "Version": "2012-10-17",
    "Id": "key-consolepolicy-3",
    "Statement": [
        {
            "Sid": "Limited Root Account Permissions",
            "Effect": "Allow",
            "Principal": {
                "AWS": "arn::{ACCOUNT}:root"
            },
            "Action": [
                "kms:Create*",
                "kms:Describe*",
                "kms:Enable*",
                "kms:List*",
                "kms:Put*",
                "kms:Update*",
                "kms:Revoke*",
                "kms:Disable*",
                "kms:Get*",
                "kms:Delete*",
                "kms:TagResource",
                "kms:UntagResource",
                "kms:ScheduleKeyDeletion",
                "kms:CancelKeyDeletion"
            ],
            "Resource": "*"
        },
        {
            "Sid": "Enable decrypt from enclave",
            "Effect": "Allow",
            "Principal": {
                "AWS": "arn:aws:sts::{ACCOUNT}:assumed-role/acm-role/i-{INSTNANCE}"
            },
            "Action": [
                "kms:Decrypt",
                "kms:GenerateDataKey*"
            ],
            "Resource": "*",
            "Condition": {
                "StringEqualsIgnoreCase": {
                    "kms:RecipientAttestation:ImageSha384": "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
                }
            }
        }
        {
            "Sid": "Enable encrypt from instance",
            "Effect": "Allow",
            "Principal": {
                "AWS": "arn:aws:sts::{{ACCOUNT}}:assumed-role/acm-role/i-{INSTANCE}"
            },
            "Action": "kms:Encrypt",
            "Resource": "*"
        }
    ]
}
```

Add a policy to your EC2's IAM role with this info: 

```
{
	"Version": "2012-10-17",
	"Statement": [
		{
			"Effect": "Allow",
			"Action": [
				"kms:Decrypt",
				"kms:GenerateDataKey",
				"kms:GenerateDataKeyWithoutPlaintext",
				"kms:CreateAlias",
				"kms:CreateKey",
				"kms:DeleteAlias",
				"kms:Describe*",
				"kms:GenerateRandom",
				"kms:Get*",
				"kms:List*",
				"kms:TagResource",
				"kms:UntagResource"
			],
			"Resource": "*"
		}
	]
}
```

## Resend key

After the DB is initialized, we need to store the resend api key encrypted to the enclave KMS key.

```sh
echo -n "API_KEY" | base64 -w 0
```

Take that output and encrypt to the KMS key, from a machine that has encrypt access to the key:

```sh
aws kms encrypt --key-id "KEY_ARN" --plaintext "BASE64_KEY" --query CiphertextBlob --output text
```

Take that encrypted base64 and insert it into the `enclave_secrets` table with key as `resend_api_key` and value as the base64.

```sql
INSERT INTO enclave_secrets (key, value)
VALUES ('resend_api_key', decode('your_base64_string', 'base64'));
```

## Github oauth info

After the DB is initialized, we need to store the github secret key encrypted to the enclave KMS key.

### Github secret

```sh
echo -n "GITHUB_SECRET" | base64 -w 0
```

Take that output and encrypt to the KMS key, from a machine that has encrypt access to the key:

```sh
aws kms encrypt --key-id "KEY_ARN" --plaintext "BASE64_KEY" --query CiphertextBlob --output text
```

Take that encrypted base64 and insert it into the `enclave_secrets` table with key as `github_client_secret` and value as the base64.

```sql
INSERT INTO enclave_secrets (key, value)
VALUES ('github_client_secret', decode('your_base64_string', 'base64'));
```

### Github client id

```sh
echo -n "GITHUB_CLIENT_ID" | base64 -w 0
```

Take that output and encrypt to the KMS key, from a machine that has encrypt access to the key:

```sh
aws kms encrypt --key-id "KEY_ARN" --plaintext "BASE64_KEY" --query CiphertextBlob --output text
```

Take that encrypted base64 and insert it into the `enclave_secrets` table with key as `github_client_id` and value as the base64.

```sql
INSERT INTO enclave_secrets (key, value)
VALUES ('github_client_id', decode('your_base64_string', 'base64'));
```

## Google oauth info

After the DB is initialized, we need to store the google secret key encrypted to the enclave KMS key.

### Google secret

```sh
echo -n "GOOGLE_SECRET" | base64 -w 0
```

Take that output and encrypt to the KMS key, from a machine that has encrypt access to the key:

```sh
aws kms encrypt --key-id "KEY_ARN" --plaintext "BASE64_KEY" --query CiphertextBlob --output text
```

Take that encrypted base64 and insert it into the `enclave_secrets` table with key as `google_client_secret` and value as the base64.

```sql
INSERT INTO enclave_secrets (key, value)
VALUES ('google_client_secret', decode('your_base64_string', 'base64'));
```

### Google client id

```sh
echo -n "GOOGLE_CLIENT_ID" | base64 -w 0
```

Take that output and encrypt to the KMS key, from a machine that has encrypt access to the key:

```sh
aws kms encrypt --key-id "KEY_ARN" --plaintext "BASE64_KEY" --query CiphertextBlob --output text
```

Take that encrypted base64 and insert it into the `enclave_secrets` table with key as `google_client_id` and value as the base64.

```sql
INSERT INTO enclave_secrets (key, value)
VALUES ('google_client_id', decode('your_base64_string', 'base64'));
```

### SQS Queue URL

After the DB is initialized, we need to store the SQS queue URL encrypted to the enclave KMS key.

```sh
echo -n "SQS_QUEUE_URL" | base64 -w 0
```

Take that output and encrypt to the KMS key, from a machine that has encrypt access to the key:

```sh
aws kms encrypt --key-id "KEY_ARN" --plaintext "BASE64_URL" --query CiphertextBlob --output text
```

Take that encrypted base64 and insert it into the `enclave_secrets` table with key as `sqs_queue_ai_events_url` and value as the base64.

```sql
INSERT INTO enclave_secrets (key, value)
VALUES ('sqs_queue_ai_events_url', decode('your_base64_string', 'base64'));
```

#### SQS Permissions

Add this policy to your EC2's IAM role to allow SQS access:

```json
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Effect": "Allow",
            "Action": [
                "sqs:SendMessage",
                "sqs:GetQueueUrl"
            ],
            "Resource": [
                "arn:aws:sqs:us-east-2:YOUR_ACCOUNT_ID:ai-events*"
            ]
        }
    ]
}
```

Replace `YOUR_ACCOUNT_ID` with your AWS account ID and adjust the queue name pattern if needed.

#### Billing API Key

```sh
echo -n "BILLING_API_KEY" | base64 -w 0
```

Take that output and encrypt to the KMS key, from a machine that has encrypt access to the key:

```sh
aws kms encrypt --key-id "KEY_ARN" --plaintext "BASE64_KEY" --query CiphertextBlob --output text
```

Take that encrypted base64 and insert it into the `enclave_secrets` table with key as `billing_api_key` and value as the base64.

```sql
INSERT INTO enclave_secrets (key, value)
VALUES ('billing_api_key', decode('your_base64_string', 'base64'));
```

#### Billing Server URL

```sh
echo -n "BILLING_SERVER_URL" | base64 -w 0
```

Take that output and encrypt to the KMS key, from a machine that has encrypt access to the key:

```sh
aws kms encrypt --key-id "KEY_ARN" --plaintext "BASE64_KEY" --query CiphertextBlob --output text
```

Take that encrypted base64 and insert it into the `enclave_secrets` table with key as `billing_server_url` and value as the base64.

```sql
INSERT INTO enclave_secrets (key, value)
VALUES ('billing_server_url', decode('your_base64_string', 'base64'));
```

#### Kagi API Key

After the DB is initialized, we need to store the Kagi Search API key encrypted to the enclave KMS key.

```sh
echo -n "KAGI_API_KEY" | base64 -w 0
```

Take that output and encrypt to the KMS key, from a machine that has encrypt access to the key:

```sh
aws kms encrypt --key-id "KEY_ARN" --plaintext "BASE64_KEY" --query CiphertextBlob --output text
```

Take that encrypted base64 and insert it into the `enclave_secrets` table with key as `kagi_api_key` and value as the base64.

```sql
INSERT INTO enclave_secrets (key, value)
VALUES ('kagi_api_key', decode('your_base64_string', 'base64'));
```

#### os-flags API Key

After the DB is initialized, we need to store the os-flags API key encrypted to the enclave KMS key.

```sh
echo -n "OS_FLAGS_API_KEY" | base64 -w 0
```

Take that output and encrypt to the KMS key, from a machine that has encrypt access to the key:

```sh
aws kms encrypt --key-id "KEY_ARN" --plaintext "BASE64_KEY" --query CiphertextBlob --output text
```

Take that encrypted base64 and insert it into the `enclave_secrets` table with key as `os_flags_api_key` and value as the base64.

```sql
INSERT INTO enclave_secrets (key, value)
VALUES ('os_flags_api_key', decode('your_base64_string', 'base64'));
```

#### os-flags Base URL (optional)

If you want to override the default os-flags base URL, store it (encrypted) under the `os_flags_base_url` key. If omitted, the server will default to:
- `https://flags.opensecret.cloud` for prod
- `https://flags-dev.opensecret.cloud` for non-prod

```sh
echo -n "OS_FLAGS_BASE_URL" | base64 -w 0
```

Take that output and encrypt to the KMS key, from a machine that has encrypt access to the key:

```sh
aws kms encrypt --key-id "KEY_ARN" --plaintext "BASE64_URL" --query CiphertextBlob --output text
```

Take that encrypted base64 and insert it into the `enclave_secrets` table with key as `os_flags_base_url` and value as the base64.

```sql
INSERT INTO enclave_secrets (key, value)
VALUES ('os_flags_base_url', decode('your_base64_string', 'base64'));
```

## Secrets Manager

### Postgresql
Need to store the postgresql string encrypted to the enclave.

```sh
echo -n "DB_URL" | base64 -w 0
```

Take that output and encrypt to the KMS key, from a machine that has encrypt access to the key:

```sh
aws kms encrypt --key-id "KEY_ARN" --plaintext "BASE64_KEY" --query CiphertextBlob --output text
```

Take that value and insert into SecretsManager with the appropriate name:
- `opensecret_dev_database_url` for dev environment
- `opensecret_preview1_database_url` for preview environment
- `opensecret_prod_database_url` for prod environment

#### Continuum API Key
Need to store the continuum api string encrypted to the enclave.

```sh
echo -n "API_KEY" | base64 -w 0
```

Take that output and encrypt to the KMS key, from a machine that has encrypt access to the key:

```sh
aws kms encrypt --key-id "KEY_ARN" --plaintext "BASE64_KEY" --query CiphertextBlob --output text
```

Take that value and insert into SecretsManager with the appropriate name:
- `continuum_proxy_dev_api_key` for dev environment
- `continuum_proxy_preview1_api_key` for preview environment
- `continuum_proxy_prod_api_key` for prod environment

#### Tinfoil API Key

Store the Tinfoil API key encrypted to the enclave. The existing
`tinfoil_proxy_*` secret names remain unchanged for deployment compatibility,
but the backend now consumes the key directly through the in-process Rust SDK.

```sh
echo -n "TINFOIL_API_KEY" | base64 -w 0
```

Take that output and encrypt to the KMS key, from a machine that has encrypt access to the key:

```sh
aws kms encrypt --key-id "KEY_ARN" --plaintext "BASE64_KEY" --query CiphertextBlob --output text
```

Take that value and insert into SecretsManager with the appropriate name:
- `tinfoil_proxy_dev_api_key` for dev environment
- `tinfoil_proxy_preview1_api_key` for preview environment
- `tinfoil_proxy_prod_api_key` for prod environment

## Credential Requester

This setup will run the credential requester on port 8003 of the parent instance, making it available for the enclave to request aws credentials.

The ec2 role will need a new inline policy to request secrets from Secrets Manager. Add the appropriate ARNs for your environment:

```json
{
	"Version": "2012-10-17",
	"Statement": [
		{
			"Effect": "Allow",
			"Action": "secretsmanager:GetSecretValue",
			"Resource": [
				"arn:aws:secretsmanager:us-east-2:XXX:secret:continuum_proxy_dev_api_key-XXX",
				"arn:aws:secretsmanager:us-east-2:XXX:secret:opensecret_dev_database_url-XXX",
				"arn:aws:secretsmanager:us-east-2:XXX:secret:continuum_proxy_preview1_api_key-XXX",
				"arn:aws:secretsmanager:us-east-2:XXX:secret:opensecret_preview1_database_url-XXX",
				"arn:aws:secretsmanager:us-east-2:XXX:secret:continuum_proxy_prod_api_key-XXX",
				"arn:aws:secretsmanager:us-east-2:XXX:secret:opensecret_prod_database_url-XXX",
				"arn:aws:secretsmanager:us-east-2:XXX:secret:tinfoil_proxy_dev_api_key-XXX",
				"arn:aws:secretsmanager:us-east-2:XXX:secret:tinfoil_proxy_preview1_api_key-XXX",
				"arn:aws:secretsmanager:us-east-2:XXX:secret:tinfoil_proxy_prod_api_key-XXX"
			]
		}
	]
}
```

Replace with the correct ARNs for those keys.

Build the docker image.

```sh
cd nitro-toolkit/credential_requester
docker build -t credential-requester .
```

Store it for transfer to the parent:

```sh
rm credential-requester.tar && docker save -o credential-requester.tar credential-requester
```

Now SCP into the AWS Parent instance:

```sh
scp credential-requester.tar ec2-user@[aws-parent-instance-ip]:~/
```

Load the docker image and tag it:

```sh
ssh ec2-user@[aws-parent-instance-ip]
docker load -i credential-requester.tar
docker tag localhost/credential-requester:latest credential-requester:latest
```

Now run it:

```sh
docker run -d --restart always --name credential-requester --device=/dev/vsock:/dev/vsock -v /var/run/vsock:/var/run/vsock --privileged -e PORT=8003 credential-requester:latest
```

## Logging Setup

To set up logging from the enclave to CloudWatch:

1. Build the logging Docker image:

```sh
cd nitro-toolkit/logging
docker build -t enclave-logging .
```

2. Save the Docker image:

```sh
docker save -o enclave-logging.tar enclave-logging
```

3. SCP the Docker image to the AWS parent instance:

```sh
scp enclave-logging.tar ec2-user@[aws-parent-instance-ip]:~/
```

4. SSH into the AWS parent instance and load the Docker image:

```sh
ssh ec2-user@[aws-parent-instance-ip]
docker load -i enclave-logging.tar
docker tag localhost/enclave-logging:latest enclave-logging:latest
```

5. Run the logging container:

```sh
docker run -d --restart always --name enclave-logging \
  --device=/dev/vsock:/dev/vsock \
  -v /var/run/vsock:/var/run/vsock \
  --privileged \
  -e VSOCK_PORT=8011 \
  -e LOG_GROUP=/aws/nitro-enclaves/enclave-dev \
  -e LOG_STREAM=enclave-logs-dev \
  -e AWS_REGION=us-east-2 \
  enclave-logging:latest
```

Replace `enclave-dev` and `enclave-logs-dev` with appropriate names for your development environment. For preview, use `enclave-preview` and `enclave-logs-preview`. For production, use `enclave-prod` and `enclave-logs-prod`.

### Setting up CloudWatch in AWS Console

Before running the logging container, you need to set up the necessary permissions and log groups in AWS CloudWatch. Follow these steps:

1. Log in to the AWS Management Console.

2. Navigate to the IAM (Identity and Access Management) service.

3. In the left sidebar, click on "Roles".

4. Find and click on the IAM role associated with your EC2 instance running the Nitro Enclave.

5. Click the "Add permissions" button and choose "Attach policies".

6. Search for and attach the "CloudWatchLogsFullAccess" policy. Note: In a production environment, you should create a more restrictive custom policy.

7. Navigate to the CloudWatch service in the AWS Console.

8. In the left sidebar, under "Logs", click on "Log groups".

9. Click the "Create log group" button.

10. Enter the name of your log group (e.g., `/aws/nitro-enclaves/enclave-dev` for development or `/aws/nitro-enclaves/enclave-prod` for production).

11. Click "Create" to finalize the log group creation.

After completing these steps, your EC2 instance will have the necessary permissions to write logs to CloudWatch, and the log group will be ready to receive logs from your Nitro Enclave.

Remember to repeat steps 9-11 if you need separate log groups for different environments (e.g., development and production).

Once CloudWatch is set up and the logging container is running, you can view your enclave logs by:

1. Going to the CloudWatch service in the AWS Console.
2. Clicking on "Log groups" in the left sidebar.
3. Selecting your log group (e.g., `/aws/nitro-enclaves/enclave-dev`).
4. Clicking on the log stream (e.g., `enclave-logs-dev`) to view the logs.

This setup allows you to monitor your Nitro Enclave's logs in real-time through the AWS CloudWatch console.

## Disk Space Management

Monitor disk space regularly to prevent service disruptions. A full disk can cause various issues including failed HTTP requests.

### Check disk usage:
```bash
df -h
du -sh /* 2>/dev/null | sort -rh | head -20
```

### Configure Docker log rotation:
Docker container logs can grow very large. Configure log rotation to prevent disk space issues:

```bash
echo '{
  "log-driver": "json-file",
  "log-opts": {
    "max-size": "100m",
    "max-file": "3"
  }
}' | sudo tee /etc/docker/daemon.json

sudo systemctl restart docker
```

### Emergency cleanup commands:
If disk is full, use these commands to free up space:

```bash
# Clear large Docker logs
sudo find /var/lib/docker/containers -name "*-json.log" -size +100M -exec truncate -s 0 {} \;

# Clear systemd journal logs
sudo journalctl --vacuum-size=100M

# Clear old logs
sudo find /var/log -type f -name "*.log" -exec truncate -s 0 {} \;

# Docker cleanup
docker system prune -a -f
docker volume prune -f

# Clear tmp directories
sudo rm -rf /tmp/*
sudo rm -rf /var/tmp/*
```

### Regular maintenance:
Set up a cron job for regular cleanup:

```bash
sudo crontab -e
# Add this line for weekly cleanup:
0 2 * * 0 journalctl --vacuum-size=100M && docker system prune -f
```
