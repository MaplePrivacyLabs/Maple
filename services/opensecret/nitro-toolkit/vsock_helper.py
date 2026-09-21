import socket
import sys
import time
import json
import select
from contextlib import closing

def vsock_request(cid, port, request, max_retries=5, retry_delay=10, initial_delay=5):
    print(f"Waiting {initial_delay} seconds before first attempt...", file=sys.stderr)
    time.sleep(initial_delay)

    for attempt in range(max_retries):
        try:
            with closing(socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)) as sock:
                print(f"Attempt {attempt + 1}: Connecting to CID {cid}, port {port}...", file=sys.stderr)
                sock.settimeout(60)  # 60 seconds timeout
                connect_start = time.time()
                sock.connect((cid, port))
                connect_time = time.time() - connect_start
                print(f"Connected in {connect_time:.2f} seconds", file=sys.stderr)

                print("Sending request...", file=sys.stderr)
                sock.sendall(request.encode())
                
                print("Receiving response...", file=sys.stderr)
                response = b""
                chunk_count = 0
                while True:
                    ready = select.select([sock], [], [], 5)  # 5 second timeout
                    if ready[0]:
                        try:
                            chunk = sock.recv(4096)
                            chunk_count += 1
                            print(f"Received chunk {chunk_count} of length {len(chunk)}", file=sys.stderr)
                            if chunk:
                                response += chunk
                                print(f"Total response length after chunk {chunk_count}: {len(response)}", file=sys.stderr)
                            else:
                                print("Received empty chunk, ending reception", file=sys.stderr)
                                break
                        except socket.error as e:
                            print(f"Socket error while receiving: {e}", file=sys.stderr)
                            break
                    else:
                        print("Receive timeout, ending reception", file=sys.stderr)
                        break

                if response:
                    print(f"Received complete response of length {len(response)}", file=sys.stderr)
                    try:
                        decoded_response = response.decode()
                        print(f"Decoded response length: {len(decoded_response)}", file=sys.stderr)
                        return decoded_response
                    except UnicodeDecodeError as e:
                        print(f"Error decoding response: {e}", file=sys.stderr)
                        print(f"Raw response: {response}", file=sys.stderr)
                        raise
                else:
                    print("No response received, retrying...", file=sys.stderr)
                    continue

        except (OSError, socket.error) as e:
            print(f"Connection error on attempt {attempt + 1}: {str(e)}", file=sys.stderr)
        
        if attempt < max_retries - 1:
            print(f"Retrying in {retry_delay} seconds...", file=sys.stderr)
            time.sleep(retry_delay)
        else:
            print("Max retries reached. Returning error JSON.", file=sys.stderr)
            return json.dumps({"error": f"VSOCK connection failed after {max_retries} attempts"})

if __name__ == "__main__":
    if len(sys.argv) != 4:
        print("Usage: python vsock_helper.py <cid> <port> <request>", file=sys.stderr)
        sys.exit(1)
    
    cid = int(sys.argv[1])
    port = int(sys.argv[2])
    request = sys.argv[3]
    
    response = vsock_request(cid, port, request)
    
    # Print only the JSON response to stdout
    print(response)
