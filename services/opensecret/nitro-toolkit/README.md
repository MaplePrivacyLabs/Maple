# Nitro Toolkit

A collection of host-side utilities for working with AWS Nitro Enclaves. These tools help manage credentials, logging, and networking for Nitro Enclaves.

Maintained directly in Maple under `services/opensecret/nitro-toolkit/`.
Changes belong in Maple pull requests alongside the OpenSecret backend.
This directory is not a Git submodule. The existing [MIT license](LICENSE)
and source history are retained; see the [import notes](../../../docs/nitro-toolkit-import.md).

## Components

### Credential Requester

A Python-based service that securely handles AWS credential management for Nitro Enclaves.

#### Features
- Retrieves AWS credentials using IMDSv2
- Handles SecretsManager requests
- Supports vsock communication with enclaves
- Multi-threaded request handling
- Automatic token refresh

#### Usage
```bash
# Build the Docker image from services/opensecret/nitro-toolkit
docker build -t credential-requester credential_requester

# Run the container
docker run -d --restart always \
  --name credential-requester \
  --device=/dev/vsock:/dev/vsock \
  -v /var/run/vsock:/var/run/vsock \
  --privileged \
  -e PORT=8003 \
  credential-requester:latest
```

### Logging

A CloudWatch logging solution specifically designed for Nitro Enclaves.

#### Features
- Forwards logs from enclaves to AWS CloudWatch
- Supports vsock communication
- Multi-threaded log processing
- Automatic retry mechanisms
- Configurable log groups and streams

#### Usage
```bash
# Build the Docker image from services/opensecret/nitro-toolkit
docker build -t enclave-logging logging

# Run the container
docker run -d --restart always \
  --name enclave-logging \
  --device=/dev/vsock:/dev/vsock \
  -v /var/run/vsock:/var/run/vsock \
  --privileged \
  -e VSOCK_PORT=8011 \
  -e LOG_GROUP=/aws/nitro-enclaves/my-enclave \
  -e LOG_STREAM=enclave-logs \
  -e AWS_REGION=us-east-2 \
  enclave-logging:latest
```

### Maintaining the host-side images

The credential requester and logger are separate containers on the parent,
not part of the application EIF. Their Dockerfiles pin the Python 3.13
Bookworm multi-architecture image by digest. They install only the complete,
hash-locked Python dependency closures in their respective `requirements.txt`
files; `requirements.in` lists the direct imports and the urllib3 security
floor. The previously installed `iproute2` package is not used by either
helper and is omitted from the refreshed images. The build removes pip and its
`ensurepip` bootstrap wheel after checking dependencies, since the running
helpers do not use them.

To refresh a lock, run this from the relevant helper directory, then review
the version and hash diff before building an image:

```sh
uv pip compile --python-version 3.13 \
  --python-platform aarch64-manylinux_2_36 \
  --only-binary :all: --generate-hashes \
  -o requirements.txt requirements.in
```

CI builds each image for Linux ARM64, runs synthetic, network-disabled
contract tests inside it, and scans the finished image for fixable high and
critical vulnerability matches. The tests check the credential JSON contract
and CloudWatch forwarding without contacting IMDS, AWS, or a VSOCK device. An
image build and offline test do not prove runtime behavior on a Nitro parent;
any later environment rollout needs its own review and validation.

### Traffic Forwarder

A Python utility for forwarding network traffic between Nitro Enclaves and external services.

#### Features
- Bidirectional traffic forwarding
- Support for both TCP and VSOCK protocols
- Automatic reconnection on failure
- Configurable endpoints
- Thread-safe operation

#### Usage
```python
# Forward traffic from local TCP to VSOCK
python traffic_forwarder.py <local_ip> <local_port> <remote_cid> <remote_port>

# Example: Forward from localhost:8080 to enclave CID 3 port 5000
python traffic_forwarder.py 127.0.0.1 8080 3 5000
```

#### Offline regression tests

From `services/opensecret/`, run the forwarder suite with the backend's pinned Python:

```sh
OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-update-lock-file '.?submodules=1' -c \
  python3 -B -m unittest discover -s nitro-toolkit -p test_traffic_forwarder.py -v
```

The tests use synthetic data and local sockets; no AWS credentials or VSOCK device
are required. The backend's `traffic-forwarder` Nix check runs the same suite in
existing backend CI. These tests do not establish deployed enclave behavior.

### VSOCK Helper

A utility for managing VSOCK communications with Nitro Enclaves.

#### Features
- Reliable VSOCK communication
- Automatic retry mechanism
- Configurable timeouts
- JSON request/response handling
- Detailed error reporting

#### Usage
```python
# Send a request to an enclave
python vsock_helper.py <cid> <port> <request>

# Example: Send a credentials request
python vsock_helper.py 3 8003 '{"request_type":"credentials","key_name":null}'
```

## Installation

1. Clone Maple and enter this directory:
```bash
git clone https://github.com/MaplePrivacyLabs/Maple.git
cd Maple/services/opensecret/nitro-toolkit
```

2. Build the credential requester and logging containers from their component directories. The traffic forwarder and VSOCK helper are standalone Python utilities. See the individual component sections for specific instructions.

## Requirements

- AWS Nitro Enclaves enabled instance
- Docker (for the credential requester and logging containers)
- Python 3.13 in the helper images; standalone utilities use the backend's pinned Python
- AWS CLI configured with appropriate permissions
- Proper IAM roles and policies configured for AWS services
