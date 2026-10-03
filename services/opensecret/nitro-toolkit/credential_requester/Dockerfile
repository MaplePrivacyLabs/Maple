FROM docker.io/library/python:3.13.16-slim-bookworm@sha256:5024f48ba9441d4b13a95d3945abc6365538e3a31109833367a1923523c6efed

WORKDIR /app

COPY requirements.txt .
RUN python -m pip install --no-cache-dir --only-binary=:all: --require-hashes -r requirements.txt \
    && python -m pip check \
    && python -m pip uninstall -y pip \
    && rm -r /usr/local/lib/python3.13/ensurepip

COPY credential_requester.py .

ENV PORT 8003
CMD python credential_requester.py --port $PORT
