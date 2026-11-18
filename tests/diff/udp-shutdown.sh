# A UDP socket shut down for reading (Linux 6.1's inet_shutdown and __skb_wait_for_more_packets):
# shutdown on an unconnected socket fails with ENOTCONN but still takes effect and wakes a thread
# blocked in recvfrom, which returns nothing (programs stop their receiving threads this way); a
# later blocking receive with nothing queued returns 0 at once, even with SO_RCVTIMEO, while a
# nonblocking one is EAGAIN; datagrams queued before the shutdown, or arriving after it, are still
# read first; shutting down only the writing side leaves receives blocking.
set -e
cd /tmp
python3 - <<'PY'
import errno, select, socket, struct, threading, time

def show(*a):
    print(*a, flush=True)

def err(f):
    try:
        return f()
    except OSError as e:
        return errno.errorcode.get(e.errno, e.errno)

def udp():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind(("127.0.0.1", 0))
    return s

def mask(s):
    p = select.poll()
    p.register(s, select.POLLIN | select.POLLOUT | select.POLLRDHUP)
    r = p.poll(0)
    return sorted(n for n in ("POLLIN", "POLLOUT", "POLLRDHUP", "POLLHUP", "POLLERR")
                  if r and r[0][1] & getattr(select, n))

def timed(f):
    """Runs a receive that may block (forever, if broken) in a daemon thread."""
    out = []
    def run():
        t = time.monotonic()
        r = err(f)
        out.append((r, "fast" if time.monotonic() - t < 1 else "slow"))
    t = threading.Thread(target=run, daemon=True)
    t.start()
    t.join(5)
    return out[0] if out else "still waiting"

show("== a thread blocked in recvfrom")
s = udp()
got = []
t = threading.Thread(target=lambda: got.append(err(lambda: s.recvfrom(100))), daemon=True)
t.start()
time.sleep(0.3)
show("shutdown", err(lambda: s.shutdown(socket.SHUT_RDWR)))
t.join(5)
show("woken", not t.is_alive(), got)
show("again", timed(lambda: s.recvfrom(100)))
show("nonblocking", err(lambda: s.recv(100, socket.MSG_DONTWAIT)))
show("poll", mask(s))
s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVTIMEO, struct.pack("ll", 3, 0))
show("with a timeout", timed(lambda: s.recv(100)))
s.close()

show("== connected, data queued around the shutdown")
a, b = udp(), udp()
a.connect(b.getsockname())
b.connect(a.getsockname())
a.send(b"before")
show("shutdown", err(lambda: b.shutdown(socket.SHUT_RD)))
a.send(b"after")
time.sleep(0.1)
show("poll", mask(b))
show("recv", b.recv(100), b.recv(100), timed(lambda: b.recv(100)))
a.close()
b.close()

show("== only the writing side")
s = udp()
show("shutdown", err(lambda: s.shutdown(socket.SHUT_WR)))
s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVTIMEO, struct.pack("ll", 0, 300000))
show("recv", err(lambda: s.recv(100)))
show("poll", mask(s))
s.close()
PY
