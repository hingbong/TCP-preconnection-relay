#!/usr/bin/env python3
"""50-concurrent stress test for TCP relay. Multiple rounds with latency stats."""
import socket, time, threading, os

HOST = os.environ.get('RELAY_HOST', '127.0.0.1')
PORT = int(os.environ.get('RELAY_PORT', '1234'))
ROUNDS = 5
CONCUR = 50
PAYLOAD = b'X' * 4096

stats = {'ok': 0, 'err': 0, 'bytes': 0, 'elapsed_ms': []}

def worker(results):
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(10)
        t0 = time.monotonic()
        s.connect((HOST, PORT))
        s.sendall(PAYLOAD)
        received = b''
        while len(received) < len(PAYLOAD):
            chunk = s.recv(65536)
            if not chunk:
                break
            received += chunk
        elapsed = (time.monotonic() - t0) * 1000
        s.close()
        if len(received) == len(PAYLOAD):
            results.append(('OK', elapsed, len(received)))
        else:
            results.append(('SHORT', elapsed, len(received)))
    except Exception as e:
        results.append(('ERR', 0, 0))

for rnd in range(ROUNDS):
    print(f'--- Round {rnd+1}/{ROUNDS} ({CONCUR} concurrent) ---', flush=True)
    results = []
    t0 = time.monotonic()
    ts = [threading.Thread(target=worker, args=(results,)) for _ in range(CONCUR)]
    for t in ts: t.start()
    for t in ts: t.join()
    total_ms = (time.monotonic() - t0) * 1000

    ok = sum(1 for r in results if r[0] == 'OK')
    err = sum(1 for r in results if r[0] != 'OK')
    latencies = [r[1] for r in results if r[0] == 'OK']
    lat_min = min(latencies) if latencies else 0
    lat_max = max(latencies) if latencies else 0
    lat_avg = sum(latencies) / len(latencies) if latencies else 0

    stats['ok'] += ok
    stats['err'] += err
    stats['bytes'] += sum(r[2] for r in results)
    stats['elapsed_ms'].extend(latencies)

    print(f'  OK={ok} ERR={err}  wall={total_ms:.0f}ms  '
          f'lat: min={lat_min:.0f}ms avg={lat_avg:.0f}ms max={lat_max:.0f}ms',
          flush=True)
    time.sleep(0.5)

print(f'\n=== TOTAL: OK={stats["ok"]} ERR={stats["err"]}  '
      f'{stats["bytes"]} bytes transferred ===', flush=True)
