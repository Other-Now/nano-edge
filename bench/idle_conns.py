"""Open N keep-alive connections, make one request on each, hold them idle, and report
the RSS growth of the given server processes per connection.

usage: idle_conns.py <port> <N> "<pid> [pid...]" [requests_per_conn=1]
"""
import resource
import socket
import sys
import time

port, n, pids = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3].split()
nreq = int(sys.argv[4]) if len(sys.argv) > 4 else 1
resource.setrlimit(resource.RLIMIT_NOFILE, (n + 100, resource.getrlimit(resource.RLIMIT_NOFILE)[1]))


def rss_kb():
    total = 0
    for p in pids:
        with open(f"/proc/{p}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    total += int(line.split()[1])
    return total


req = b"GET /obj/m HTTP/1.1\r\nHost: bench\r\n\r\n"
before = rss_kb()
socks = []
for _ in range(n):
    s = socket.create_connection(("127.0.0.1", port))
    s.sendall(req)
    got = b""
    while b"\r\n\r\n" not in got:
        got += s.recv(65536)
    head, _, body = got.partition(b"\r\n\r\n")
    cl = int([l.split(b":")[1] for l in head.split(b"\r\n") if l.lower().startswith(b"content-length")][0])
    while len(body) < cl:
        body += s.recv(65536)
    socks.append(s)
time.sleep(2)
after = rss_kb()
alive = 0
for s in socks:
    s.setblocking(False)
    try:
        alive += s.recv(1) != b""
    except BlockingIOError:
        alive += 1
print(f"RSS {before/1024:.1f} MiB -> {after/1024:.1f} MiB with {alive}/{n} connections open: {(after-before)*1024/n/1024:.2f} KiB per connection")
