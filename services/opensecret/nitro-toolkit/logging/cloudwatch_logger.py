import socket
import boto3
import time
import os
import json
import sys
import codecs
import threading
import logging
from botocore.exceptions import ClientError

# Force unbuffered output
sys.stdout.reconfigure(line_buffering=True)
sys.stderr.reconfigure(line_buffering=True)

# Set up logging - WARNING level by default to reduce disk usage
# Set LOG_LEVEL env var to 'INFO' or 'DEBUG' for more verbose logging
log_level = os.environ.get('LOG_LEVEL', 'WARNING').upper()
logging.basicConfig(level=getattr(logging, log_level), format='[%(asctime)s] %(levelname)s: %(message)s')
logger = logging.getLogger(__name__)

def create_cloudwatch_client():
    region = os.environ.get('AWS_REGION', 'us-east-2')
    return boto3.client('logs', region_name=region)

def setup_log_group_and_stream(cloudwatch):
    log_group = os.environ.get('LOG_GROUP', '/aws/nitro-enclaves/enclave')
    log_stream = os.environ.get('LOG_STREAM', 'enclave-logs')

    try:
        cloudwatch.create_log_group(logGroupName=log_group)
    except ClientError as e:
        if e.response['Error']['Code'] != 'ResourceAlreadyExistsException':
            logger.error(f"Error creating log group: {e}")
            raise

    try:
        cloudwatch.create_log_stream(logGroupName=log_group, logStreamName=log_stream)
    except ClientError as e:
        if e.response['Error']['Code'] != 'ResourceAlreadyExistsException':
            logger.error(f"Error creating log stream: {e}")
            raise

    return log_group, log_stream

def handle_client(conn, addr, cloudwatch, log_group, log_stream):
    logger.debug(f"Connected by {addr}")
    decoder = codecs.getincrementaldecoder('utf-8')(errors='replace')
    try:
        while True:
            data = conn.recv(1024)
            if not data:
                break
            message = decoder.decode(data)
            if not message:
                continue
            logger.debug("Forwarding log to CloudWatch")
            try:
                cloudwatch.put_log_events(
                    logGroupName=log_group,
                    logStreamName=log_stream,
                    logEvents=[{
                        'timestamp': int(time.time() * 1000),
                        'message': message
                    }]
                )
            except Exception as e:
                logger.error(f"Error sending log to CloudWatch: {e}")

        remaining = decoder.decode(b'', final=True)
        if remaining:
            logger.debug("Forwarding log to CloudWatch")
            try:
                cloudwatch.put_log_events(
                    logGroupName=log_group,
                    logStreamName=log_stream,
                    logEvents=[{
                        'timestamp': int(time.time() * 1000),
                        'message': remaining
                    }]
                )
            except Exception as e:
                logger.error(f"Error sending log to CloudWatch: {e}")
    except Exception as e:
        logger.error(f"Error handling connection: {e}")
    finally:
        conn.close()
        logger.debug(f"Connection closed for {addr}")

def socket_to_cloudwatch(port):
    cloudwatch = create_cloudwatch_client()
    log_group, log_stream = setup_log_group_and_stream(cloudwatch)

    sock = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    cid = socket.VMADDR_CID_ANY
    sock.bind((cid, port))
    sock.listen()

    logger.info(f"Listening for logs on port {port}")

    while True:
        try:
            conn, addr = sock.accept()
            threading.Thread(target=handle_client, args=(conn, addr, cloudwatch, log_group, log_stream)).start()
        except Exception as e:
            logger.error(f"Error accepting connection: {e}")
            time.sleep(1)  # Add a small delay before retrying

if __name__ == "__main__":
    port = int(os.environ.get('VSOCK_PORT', 8011))
    socket_to_cloudwatch(port)
