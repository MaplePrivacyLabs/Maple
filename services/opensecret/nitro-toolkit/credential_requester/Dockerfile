FROM docker.io/library/python:3.9-slim

WORKDIR /app

RUN apt-get update && apt-get install -y iproute2 && rm -rf /var/lib/apt/lists/*

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY credential_requester.py .

ENV PORT 8003
CMD python credential_requester.py --port $PORT
