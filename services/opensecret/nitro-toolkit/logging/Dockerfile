FROM python:3.9-slim

WORKDIR /app

RUN apt-get update && apt-get install -y iproute2 && rm -rf /var/lib/apt/lists/*

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY cloudwatch_logger.py .

ENV VSOCK_PORT 8011
ENV LOG_GROUP /aws/nitro-enclaves/my-enclave
ENV LOG_STREAM enclave-logs
ENV AWS_REGION us-east-2

# Run Python in unbuffered mode
CMD ["python", "-u", "cloudwatch_logger.py"]
