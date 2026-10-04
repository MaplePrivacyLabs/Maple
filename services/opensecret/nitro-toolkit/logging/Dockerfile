FROM docker.io/library/python:3.13.16-slim-bookworm@sha256:5024f48ba9441d4b13a95d3945abc6365538e3a31109833367a1923523c6efed

# The pinned base predates Bookworm's CVE-2026-103111 fix. Keep the digest
# stable while requiring the fixed libpcre2 package from bookworm-security.
RUN apt-get update \
    && apt-get install -y --no-install-recommends --only-upgrade libpcre2-8-0=10.42-1+deb12u2 \
    && test "$(dpkg-query -W -f='${Version}' libpcre2-8-0)" = '10.42-1+deb12u2' \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY requirements.txt .
RUN python -m pip install --no-cache-dir --only-binary=:all: --require-hashes -r requirements.txt \
    && python -m pip check \
    && python -m pip uninstall -y pip \
    && rm -r /usr/local/lib/python3.13/ensurepip

COPY cloudwatch_logger.py .

ENV VSOCK_PORT 8011
ENV LOG_GROUP /aws/nitro-enclaves/my-enclave
ENV LOG_STREAM enclave-logs
ENV AWS_REGION us-east-2

# Run Python in unbuffered mode
CMD ["python", "-u", "cloudwatch_logger.py"]
