import socket
import sys
import threading
import time
import logging
import signal
import os

# Set up logging
logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')

# Global flag for graceful shutdown
shutdown_flag = threading.Event()

def signal_handler(sig, frame):
    logging.info("Received shutdown signal")
    shutdown_flag.set()

signal.signal(signal.SIGINT, signal_handler)
signal.signal(signal.SIGTERM, signal_handler)

def forward(source, destination, connection_id, direction):
    """Forward data between sockets with proper cleanup"""
    try:
        source.settimeout(1.0)  # 1 second timeout for checking shutdown
        while not shutdown_flag.is_set():
            try:
                data = source.recv(1024)
                if not data:
                    logging.info(f"Connection {connection_id}: End of data stream ({direction})")
                    break
                destination.sendall(data)
            except socket.timeout:
                continue  # Check shutdown flag
            except OSError as e:
                # Check if it's a bad file descriptor or transport endpoint error
                if e.errno in (9, 107) or "Bad file descriptor" in str(e) or "Transport endpoint is not connected" in str(e):
                    # Socket was closed by the other thread, this is normal during shutdown
                    logging.debug(f"Connection {connection_id}: Socket closed by peer ({direction})")
                else:
                    logging.error(f"Connection {connection_id}: Forwarding error ({direction}): {e}")
                break
            except Exception as e:
                if not shutdown_flag.is_set():
                    logging.error(f"Connection {connection_id}: Forwarding error ({direction}): {e}")
                break
    except Exception as e:
        logging.error(f"Connection {connection_id}: Fatal error ({direction}): {e}")
    finally:
        # Only shutdown our reading side and their writing side
        try:
            if direction == "client->server":
                source.shutdown(socket.SHUT_RD)
                destination.shutdown(socket.SHUT_WR)
            else:  # server->client
                source.shutdown(socket.SHUT_RD)
                destination.shutdown(socket.SHUT_WR)
        except OSError:
            # Socket might already be closed, that's OK
            pass
        logging.info(f"Connection {connection_id}: Completed ({direction})")

def handle_connection(client_socket, client_addr, remote_cid, remote_port, connection_id):
    """Handle a single connection with proper resource management"""
    server_socket = None
    threads = []
    
    try:
        # Connect to VSOCK
        server_socket = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
        server_socket.settimeout(30)  # 30 second timeout for connection
        server_socket.connect((remote_cid, remote_port))
        logging.info(f"Connection {connection_id}: Connected to VSOCK {remote_cid}:{remote_port}")
        
        # Create forwarding threads
        outgoing_thread = threading.Thread(
            target=forward, 
            args=(client_socket, server_socket, connection_id, "client->server"),
            name=f"forward-{connection_id}-out"
        )
        incoming_thread = threading.Thread(
            target=forward, 
            args=(server_socket, client_socket, connection_id, "server->client"),
            name=f"forward-{connection_id}-in"
        )
        
        threads = [outgoing_thread, incoming_thread]
        
        # Start threads
        for thread in threads:
            thread.start()
        
        # Wait for threads to complete
        for thread in threads:
            thread.join()
            
    except Exception as e:
        logging.error(f"Connection {connection_id}: Failed to establish connection: {e}")
    finally:
        # Now that both forwarding threads are done, we can fully close the sockets
        for sock in [client_socket, server_socket]:
            if sock:
                try:
                    # Both directions should already be shutdown by the forwarding threads
                    sock.close()
                except OSError:
                    pass  # Already closed
        
        logging.info(f"Connection {connection_id}: Handler complete")

def server(local_ip, local_port, remote_cid, remote_port):
    """Main server with proper resource management and graceful shutdown"""
    dock_socket = None
    connection_counter = 0
    active_connections = []
    
    try:
        dock_socket = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        dock_socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        dock_socket.bind((local_ip, local_port))
        dock_socket.listen(5)
        dock_socket.settimeout(1.0)  # Check for shutdown every second
        logging.info(f"Listening on {local_ip}:{local_port}")
        
        while not shutdown_flag.is_set():
            try:
                client_socket, client_addr = dock_socket.accept()
                connection_counter += 1
                connection_id = f"{connection_counter}"
                logging.info(f"Connection {connection_id}: Accepted from {client_addr}")
                
                # Handle connection in a separate thread
                handler_thread = threading.Thread(
                    target=handle_connection,
                    args=(client_socket, client_addr, remote_cid, remote_port, connection_id),
                    name=f"handler-{connection_id}"
                )
                handler_thread.daemon = True  # Allow main thread to exit
                handler_thread.start()
                active_connections.append(handler_thread)
                
                # Clean up finished threads
                active_connections = [t for t in active_connections if t.is_alive()]
                
            except socket.timeout:
                continue  # Check shutdown flag
            except Exception as e:
                if not shutdown_flag.is_set():
                    logging.error(f"Server error: {e}")
                    
    except Exception as e:
        logging.error(f"Failed to start server: {e}")
    finally:
        if dock_socket:
            try:
                dock_socket.close()
            except:
                pass
        
        # Wait for active connections to finish
        logging.info("Waiting for active connections to close...")
        for thread in active_connections:
            thread.join(timeout=5)
        
        logging.info("Server shutdown complete")

def main(args):
    if len(args) < 4:
        logging.error("Usage: python traffic_forwarder.py <local_ip> <local_port> <remote_cid> <remote_port>")
        sys.exit(1)
        
    local_ip = str(args[0])
    local_port = int(args[1])
    remote_cid = int(args[2])
    remote_port = int(args[3])
    
    logging.info(f"Starting forwarder on {local_ip}:{local_port} to {remote_cid}:{remote_port}")
    
    # Run server (will block until shutdown)
    server(local_ip, local_port, remote_cid, remote_port)
    
    logging.info("Traffic forwarder exiting")

if __name__ == '__main__':
    main(sys.argv[1:])