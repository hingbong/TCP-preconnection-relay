"""Sequential connection test for relay."""
import socket, time, os

HOST = os.environ.get('RELAY_HOST', '127.0.0.1')
PORT = int(os.environ.get('RELAY_PORT', '1234'))

for i in range(3):
    try:
        s=socket.socket(); s.settimeout(5)
        s.connect((HOST, PORT))
        s.sendall(b'ping')
        d=s.recv(1024)
        print(f'[{i}] OK:{d}', flush=True)
        s.close()
    except Exception as e:
        print(f'[{i}] ERR:{e}', flush=True)
    time.sleep(1.5)
print('DONE', flush=True)
