"""Simple single-connection smoke test for relay."""
import socket, time, os

HOST = os.environ.get('RELAY_HOST', '127.0.0.1')
PORT = int(os.environ.get('RELAY_PORT', '1234'))

s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.settimeout(5)
s.connect((HOST, PORT))
print('CONNECTED', flush=True)
s.sendall(b'hello')
time.sleep(0.1)
data = s.recv(1024)
print(f'RECEIVED: {data}', flush=True)
s.close()
print('DONE', flush=True)
