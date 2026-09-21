import socket
import requests
import json
import logging
import threading
import argparse
import boto3
from botocore.exceptions import ClientError

# IMDSv2 endpoint
IMDS_URL = "http://169.254.169.254/latest/meta-data/iam/security-credentials/"

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

class EnclaveRequest:
    def __init__(self, request_type, key_name=None):
        self.request_type = request_type
        self.key_name = key_name

class ParentResponse:
    def __init__(self, response_type, response_value):
        self.response_type = response_type
        self.response_value = response_value

def get_imdsv2_token():
    headers = {"X-aws-ec2-metadata-token-ttl-seconds": "21600"}
    try:
        response = requests.put("http://169.254.169.254/latest/api/token", headers=headers)
        response.raise_for_status()
        return response.text
    except requests.RequestException as e:
        logger.error(f"Failed to get IMDSv2 token: {e}")
        return None

def get_credentials(role_name, token):
    headers = {"X-aws-ec2-metadata-token": token}
    try:
        response = requests.get(f"{IMDS_URL}{role_name}", headers=headers)
        response.raise_for_status()
        return json.loads(response.text)
    except (requests.RequestException, json.JSONDecodeError) as e:
        logger.error(f"Failed to get credentials: {e}")
        return None

def get_secret(secret_name, region_name, aws_access_key_id, aws_secret_access_key, aws_session_token):
    session = boto3.session.Session(
        aws_access_key_id=aws_access_key_id,
        aws_secret_access_key=aws_secret_access_key,
        aws_session_token=aws_session_token
    )
    client = session.client(
        service_name='secretsmanager',
        region_name=region_name
    )

    try:
        get_secret_value_response = client.get_secret_value(SecretId=secret_name)
        return get_secret_value_response['SecretString']
    except ClientError as e:
        logger.error(f"Failed to get secret: {e}")
        return None

def get_region(token):
    headers = {"X-aws-ec2-metadata-token": token}
    try:
        response = requests.get("http://169.254.169.254/latest/meta-data/placement/region", headers=headers)
        response.raise_for_status()
        return response.text
    except requests.RequestException as e:
        logger.error(f"Failed to get region: {e}")
        return None

def handle_client(conn, addr):
    try:
        # Read request from client
        request_data = conn.recv(1024).decode()
        request = json.loads(request_data, object_hook=lambda d: EnclaveRequest(**d))

        if request.request_type == "credentials":
            # Get IMDSv2 token
            token = get_imdsv2_token()
            if not token:
                response = ParentResponse("error", "Failed to get IMDSv2 token")
                conn.send(json.dumps(response.__dict__).encode())
                return

            # Get IAM role name
            headers = {"X-aws-ec2-metadata-token": token}
            try:
                role_name = requests.get(IMDS_URL, headers=headers).text
            except requests.RequestException as e:
                logger.error(f"Failed to get IAM role name: {e}")
                response = ParentResponse("error", "Failed to get IAM role name")
                conn.send(json.dumps(response.__dict__).encode())
                return

            # Get credentials
            creds = get_credentials(role_name, token)
            if not creds:
                response = ParentResponse("error", "Failed to get credentials")
                conn.send(json.dumps(response.__dict__).encode())
                return

            # Get region
            region = get_region(token)
            if not region:
                response = ParentResponse("error", "Failed to get region")
                conn.send(json.dumps(response.__dict__).encode())
                return

            # Prepare response
            response_value = {
                "AccessKeyId": creds["AccessKeyId"],
                "SecretAccessKey": creds["SecretAccessKey"],
                "Token": creds["Token"],
                "Region": region
            }
            response = ParentResponse("credentials", response_value)
            conn.send(json.dumps(response.__dict__).encode())
        elif request.request_type == "SecretsManager":
            if not request.key_name:
                response = ParentResponse("error", "Missing key_name for SecretsManager request")
                conn.send(json.dumps(response.__dict__).encode())
                return

            # Get AWS credentials first
            token = get_imdsv2_token()
            if not token:
                response = ParentResponse("error", "Failed to get IMDSv2 token")
                conn.send(json.dumps(response.__dict__).encode())
                return

            headers = {"X-aws-ec2-metadata-token": token}
            try:
                role_name = requests.get(IMDS_URL, headers=headers).text
            except requests.RequestException as e:
                logger.error(f"Failed to get IAM role name: {e}")
                response = ParentResponse("error", "Failed to get IAM role name")
                conn.send(json.dumps(response.__dict__).encode())
                return

            creds = get_credentials(role_name, token)
            if not creds:
                response = ParentResponse("error", "Failed to get credentials")
                conn.send(json.dumps(response.__dict__).encode())
                return

            # Get region
            region = get_region(token)
            if not region:
                response = ParentResponse("error", "Failed to get region")
                conn.send(json.dumps(response.__dict__).encode())
                return

            # Now use these credentials to get the secret
            secret = get_secret(
                request.key_name,
                region,  # Use the dynamically fetched region
                creds["AccessKeyId"],
                creds["SecretAccessKey"],
                creds["Token"]
            )

            if secret:
                response = ParentResponse("secret", secret)
            else:
                response = ParentResponse("error", "Failed to retrieve secret")

            conn.send(json.dumps(response.__dict__).encode())
        else:
            response = ParentResponse("error", "Unknown request type")
            conn.send(json.dumps(response.__dict__).encode())
    except Exception as e:
        logger.error(f"Error handling client {addr}: {e}")
    finally:
        conn.close()

def main(port):
    # Create vsock socket
    s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)

    # CID_ANY allows listening for any CID
    cid = socket.VMADDR_CID_ANY

    try:
        s.bind((cid, port))
        s.listen()

        logger.info(f"Listening on port {port}")

        while True:
            conn, addr = s.accept()
            logger.info(f"Connected by {addr}")
            threading.Thread(target=handle_client, args=(conn, addr)).start()
    except Exception as e:
        logger.error(f"Error in main loop: {e}")
    finally:
        s.close()

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Credential requester for AWS IMDSv2")
    parser.add_argument("-p", "--port", type=int, default=5000,
                        help="Port number to listen on (default: 5000)")
    args = parser.parse_args()

    main(args.port)
