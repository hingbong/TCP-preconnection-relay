#!/usr/bin/env python3
"""Multi-threaded TCP echo server for relay stress testing.

Usage:
    python3 echo_server.py [host] [port]
    # defaults: 127.0.0.1:8443
"""
import socket, threading, sys, os

HOST = sys.argv[1] if len(sys.argv) > 1 else os.environ.get('ECHO_HOST', '127.0.0.1')
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else int(os.environ.get('ECHO_PORT', '8443'))

def handle(c, addr):
    try:
        c.settimeout(30)
        while True:
            data = c.recv(65536)
            if not data:
                break
            c.sendall(data)
    except Exception as e:
        print(f'[{addr}] ERROR: {e}', flush=True)
    finally:
        c.close()

s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind((HOST, PORT))
s.listen(128)
print(f'ECHO server listening on {HOST}:{PORT}', flush=True)

while True:
    c, addr = s.accept()
    threading.Thread(target=handle, args=(c, addr), daemon=True).start()
