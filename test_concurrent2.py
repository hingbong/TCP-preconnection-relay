"""2-concurrent connection test for relay."""
import socket, time, threading, os

HOST = os.environ.get('RELAY_HOST', '127.0.0.1')
PORT = int(os.environ.get('RELAY_PORT', '1234'))
results = {}

def test(i):
    try:
        s = socket.socket(); s.settimeout(5)
        s.connect((HOST, PORT))
        s.sendall(b'ping')
        d = s.recv(1024)
        results[i] = f'OK:{d}'
        s.close()
    except Exception as e:
        results[i] = f'ERR:{e}'

t1 = threading.Thread(target=test, args=(1,))
t1.start()
time.sleep(0.02)
t2 = threading.Thread(target=test, args=(2,))
t2.start()
t1.join()
t2.join()
for k, v in sorted(results.items()):
    print(f'  [{k}] {v}')
print('DONE')
