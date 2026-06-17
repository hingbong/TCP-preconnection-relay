"""Stress test: 5 concurrent connections, 64KB each."""
import socket, time, threading, os

HOST = os.environ.get('RELAY_HOST', '127.0.0.1')
PORT = int(os.environ.get('RELAY_PORT', '1234'))

def worker(idx, results):
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(5)
        s.connect((HOST, PORT))
        payload = b'X' * 65536
        s.sendall(payload)
        received = b''
        while len(received) < len(payload):
            chunk = s.recv(65536)
            if not chunk:
                results[idx] = f"EOF early at {len(received)}/{len(payload)}"
                break
            received += chunk
        if len(received) == len(payload):
            results[idx] = f"OK ({len(received)} bytes)"
        s.close()
    except Exception as e:
        results[idx] = f"ERROR: {e}"

N = 5
results = [None] * N
ts = [threading.Thread(target=worker, args=(i, results)) for i in range(N)]
for t in ts: t.start(); time.sleep(0.05)
for t in ts: t.join()
for i, r in enumerate(results):
    print(f"  [{i}] {r}")
print("DONE", flush=True)
