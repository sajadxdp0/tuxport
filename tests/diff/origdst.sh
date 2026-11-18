# docker-run-options: --privileged
# What transparent proxies need (docs/rounds/180-original-dst.md):
# - SO_ORIGINAL_DST, IP6T_SO_ORIGINAL_DST: each case sets up rules and connects in a new network
#   namespace (with lo up), and looks at what the accepting end, the connecting end and the
#   listening socket answer (optlen, address, error code). No rules, only an empty nat chain (no
#   connection tracking), only a ct filter rule (tracking on), nft's redirect and dnat,
#   iptables' REDIRECT, ip6 and inet tables, a dual-stack listener, IPv4-mapped addresses, a
#   client that closes before accept;
# - across namespaces: a bridge in the boot-time namespace and two "containers" A and B; B's
#   PREROUTING does REDIRECT, A's OUTPUT does DNAT, and the outer PREROUTING does REDIRECT for
#   what comes from the bridge; the mark of A's socket with SO_MARK is cleared on entering B
#   through the veth; A connecting to an external address also goes through the outer
#   PREROUTING: REDIRECTed to the outer proxy (`fib daddr type local` rules, which only match
#   local addresses, do not count), DNATed to B (no connection when the outer FORWARD rejects
#   it), UDP port 53 hijacked to the outer namespace (replies come from the original
#   destination); when a UDP server in the outer namespace bound to the wildcard address replies
#   to A, the source is the outer address facing A;
# - SO_MARK: reading and writing, length, no permission, UDP, accepted sockets inheriting the
#   listener's;
# - iptables owner and mark matches on the OUTPUT of nat and filter, nft's meta skuid and meta
#   mark;
# - a transparent proxy written in Python: asks SO_ORIGINAL_DST, connects to the original
#   destination with SO_MARK (the rules let marked ones through), and relays one request.
# The native side is a privileged container, where the rules really run in the kernel. The Python
# of every role imports the same set of modules first.
exec 2>&1
cd /tmp
cat > od.py <<'PY'
import errno, os, select, socket, struct, subprocess, sys, time

SO_ORIGINAL_DST = 80
SO_MARK = 36
V4, V6 = socket.AF_INET, socket.AF_INET6
socket.setdefaulttimeout(15)


def nft(script):
    script = script.replace("{ ", "{\n").replace("; ", "\n").replace(" }", "\n}")
    subprocess.run(["nft", "-f", "-"], input=script.encode(), check=True)


def sh(cmd):
    subprocess.run(cmd, shell=True, check=True)


def err(e):
    return errno.errorcode.get(e.errno, e.errno)


def fmt(b):
    fam = struct.unpack("<H", b[:2])[0] if len(b) >= 2 else None
    port = struct.unpack(">H", b[2:4])[0] if len(b) >= 4 else None
    if fam == 2 and len(b) >= 8:
        return f"{socket.inet_ntoa(b[4:8])}:{port} rest={b[8:16].hex()}"
    if fam == 10 and len(b) >= 28:
        flow, scope = struct.unpack(">I", b[4:8])[0], struct.unpack("<I", b[24:28])[0]
        return f"[{socket.inet_ntop(V6, b[8:24])}]:{port} flow={flow} scope={scope}"
    return b.hex()


def q(s, level=socket.SOL_IP, ln=16):
    """SO_ORIGINAL_DST: optlen (which Linux writes back as is) and the sockaddr written in."""
    try:
        b = s.getsockopt(level, SO_ORIGINAL_DST, ln)
        return f"optlen={len(b)} " + fmt(b[: 28 if level == socket.IPPROTO_IPV6 else 16])
    except OSError as e:
        return err(e)


def orig(s):
    try:
        b = s.getsockopt(socket.SOL_IP, SO_ORIGINAL_DST, 16)
        return f"{socket.inet_ntoa(b[4:8])}:{struct.unpack('>H', b[2:4])[0]}"
    except OSError as e:
        return err(e)


def listener(fam, addr, port, mark=None):
    l = socket.socket(fam, socket.SOCK_STREAM)
    l.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    if mark is not None:
        l.setsockopt(socket.SOL_SOCKET, SO_MARK, mark)
    l.bind((addr, port))
    l.listen()
    return l


def pair(lfam, laddr, lport, cfam, caddr, cport):
    l = listener(lfam, laddr, lport)
    c = socket.socket(cfam, socket.SOCK_STREAM)
    c.connect((caddr, cport))
    a, _ = l.accept()
    return l, c, a


def show(tag, l, c, a, v6=False):
    print(f"{tag}: accepted SOL_IP {q(a)}")
    print(f"{tag}: accepted SOL_IP optlen 64 {q(a, ln=64)}")
    print(f"{tag}: client SOL_IP {q(c)}")
    print(f"{tag}: listener SOL_IP {q(l)}")
    if v6:
        print(f"{tag}: accepted SOL_IPV6 {q(a, socket.IPPROTO_IPV6, 28)}")
        print(f"{tag}: accepted SOL_IPV6 optlen 64 {q(a, socket.IPPROTO_IPV6, 64)}")
        print(f"{tag}: client SOL_IPV6 {q(c, socket.IPPROTO_IPV6, 28)}")
        print(f"{tag}: accepted SOL_IPV6 optlen 27 {q(a, socket.IPPROTO_IPV6, 27)}")
    for x in (l, c, a):
        x.close()


def as_user(uid, fn):
    """Run fn in a child process as uid (with the same gid) and print its result."""
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(r)
        try:
            os.setgid(uid)
            os.setuid(uid)
            out = fn()
        except OSError as e:
            out = err(e)
        os.write(w, str(out).encode())
        os._exit(0)
    os.close(w)
    out = b""
    while chunk := os.read(r, 1000):
        out += chunk
    os.waitpid(pid, 0)
    os.close(r)
    return out.decode()


def where(port, uid=0, mark=None, ls=()):
    """Connect to 127.0.0.2:port and return which listener accepted it (one REDIRECTed to 1081
    reports the original destination address)."""

    def connect():
        c = socket.socket()
        if mark is not None:
            c.setsockopt(socket.SOL_SOCKET, SO_MARK, mark)
        c.settimeout(5)
        try:
            c.connect(("127.0.0.2", port))
        except OSError as e:
            return err(e)
        c.close()
        return "connected"

    res = as_user(uid, connect) if uid else connect()
    if res != "connected":
        return res
    r, _, _ = select.select(ls, [], [], 5)
    if not r:
        return "nobody accepted"
    a, _ = r[0].accept()
    me = a.getsockname()[1]
    out = f"redirected (orig {orig(a)})" if me == 1081 else f"direct to {me}"
    a.close()
    return out


def probe(sc):
    if sc == "none":
        show("no rules", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.1", 1081))
        u = socket.socket(V4, socket.SOCK_DGRAM)
        u.bind(("127.0.0.1", 0))
        u.connect(("127.0.0.1", 9))
        print("udp", q(u), q(u, ln=4))
        print("tcp optlen 8", q(socket.socket(), ln=8))
        print("AF_INET SOL_IPV6", q(socket.socket(), socket.IPPROTO_IPV6, 28))
    elif sc == "chain":
        nft("table ip t { chain out { type nat hook output priority -100; } }")
        show("empty nat chain", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.1", 1081))
    elif sc == "filterct":
        nft("table ip t { chain out { type filter hook output priority 0; ct state new accept } }")
        show("ip filter ct", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.1", 1081))
        show("ip filter ct, IPv6", *pair(V6, "::1", 1083, V6, "::1", 1083), v6=True)
    elif sc == "inetct":
        nft("table inet t { chain out { type filter hook output priority 0; ct state new accept } }")
        show("inet filter ct, IPv6", *pair(V6, "::1", 1083, V6, "::1", 1083), v6=True)
    elif sc == "redirect":
        nft(
            "table ip t { chain out { type nat hook output priority -100; "
            "ip daddr 127.0.0.2 tcp dport 2290 redirect to :1081; "
            "ip daddr 127.0.0.3 tcp dport 80 dnat to 127.0.0.1:1081; "
            "ip daddr 127.0.0.2 tcp dport 2291 redirect to :1082 } }"
        )
        show("redirected", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.2", 2290))
        show("dnat", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.3", 80))
        show("direct", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.1", 1081))
        show("dual-stack listener", *pair(V6, "::", 1082, V4, "127.0.0.2", 2291), v6=True)
        show("v6 client, mapped", *pair(V4, "127.0.0.1", 1081, V6, "::ffff:127.0.0.2", 2290), v6=True)
        show("v6 without an ip6 table", *pair(V6, "::1", 1083, V6, "::1", 1083), v6=True)
        l = listener(V4, "127.0.0.1", 1081)
        c = socket.create_connection(("127.0.0.2", 2290))
        c.sendall(b"x")
        c.close()
        time.sleep(0.2)
        a, _ = l.accept()
        print("client closed before accept:", q(a), a.recv(10))
        a.close()
        c = socket.create_connection(("127.0.0.2", 2290))
        a, _ = l.accept()
        c.close()
        time.sleep(0.2)
        print("client closed after accept:", q(a))
        a.close()
        l.close()
    elif sc == "iptables":
        sh("iptables -t nat -A OUTPUT -p tcp -d 127.0.0.2 --dport 2290 -j REDIRECT --to-ports 1081")
        show("iptables REDIRECT", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.2", 2290))
        show("iptables, direct", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.1", 1081))
    elif sc == "ip6":
        nft("table ip6 t { chain out { type nat hook output priority -100; ip6 daddr ::1 tcp dport 2290 redirect to :1083 } }")
        show("ip6 redirected", *pair(V6, "::1", 1083, V6, "::1", 2290), v6=True)
        show("IPv4 without an ip table", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.1", 1081))
    elif sc == "inet":
        nft("table inet t { chain out { type nat hook output priority -100; tcp dport 2290 redirect to :1084 } }")
        show("inet, IPv4", *pair(V6, "::", 1084, V4, "127.0.0.1", 2290), v6=True)
        show("inet, IPv6", *pair(V6, "::", 1084, V6, "::1", 2290), v6=True)


def marks():
    s = socket.socket()
    print("default", s.getsockopt(socket.SOL_SOCKET, SO_MARK))
    s.setsockopt(socket.SOL_SOCKET, SO_MARK, 5)
    print("set 5", s.getsockopt(socket.SOL_SOCKET, SO_MARK))
    print("optlen 2", s.getsockopt(socket.SOL_SOCKET, SO_MARK, 2).hex())
    s.setsockopt(socket.SOL_SOCKET, SO_MARK, -1)
    print("set -1", hex(s.getsockopt(socket.SOL_SOCKET, SO_MARK) & 0xFFFFFFFF))
    try:
        s.setsockopt(socket.SOL_SOCKET, SO_MARK, b"\x01\x00")
    except OSError as e:
        print("optlen 2 set", err(e))
    u = socket.socket(V4, socket.SOCK_DGRAM)
    u.setsockopt(socket.SOL_SOCKET, SO_MARK, 6)
    print("udp", u.getsockopt(socket.SOL_SOCKET, SO_MARK))

    def unprivileged():
        x = socket.socket()
        seen = x.getsockopt(socket.SOL_SOCKET, SO_MARK)
        try:
            x.setsockopt(socket.SOL_SOCKET, SO_MARK, 1)
            return f"read {seen}, set ok"
        except OSError as e:
            return f"read {seen}, set {err(e)}"

    print("nobody:", as_user(65534, unprivileged))
    l = listener(V4, "127.0.0.1", 1081, mark=9)
    c = socket.create_connection(("127.0.0.1", 1081))
    a, _ = l.accept()
    print("accepted from a listener with mark 9:", a.getsockopt(socket.SOL_SOCKET, SO_MARK),
          "client:", c.getsockopt(socket.SOL_SOCKET, SO_MARK))


def owner():
    sh("iptables -t nat -A OUTPUT -p tcp -d 127.0.0.2 --dport 2290 -m owner --uid-owner 65534 -j REDIRECT --to-ports 1081")
    sh("iptables -t nat -A OUTPUT -p tcp -d 127.0.0.2 --dport 2291 -m owner ! --uid-owner 0 -j REDIRECT --to-ports 1081")
    sh("iptables -t nat -A OUTPUT -p tcp -d 127.0.0.2 --dport 2292 -m owner --gid-owner 65534 -j REDIRECT --to-ports 1081")
    sh("iptables -t nat -A OUTPUT -p tcp -d 127.0.0.2 --dport 2293 -m mark --mark 7 -j RETURN")
    sh("iptables -t nat -A OUTPUT -p tcp -d 127.0.0.2 --dport 2293 -j REDIRECT --to-ports 1081")
    sh("iptables -t nat -A OUTPUT -p tcp -d 127.0.0.2 --dport 2294 -m mark --mark 0x10/0xf0 -j REDIRECT --to-ports 1081")
    sh("iptables -A OUTPUT -p tcp -d 127.0.0.2 --dport 2295 -m owner --uid-owner 65534 -j REJECT --reject-with tcp-reset")
    sh("iptables -A OUTPUT -p tcp -d 127.0.0.2 --dport 2296 -m mark ! --mark 3 -j REJECT --reject-with tcp-reset")
    nft("table ip n { chain out { type nat hook output priority -50; "
        "meta skuid 65534 ip daddr 127.0.0.2 tcp dport 2297 redirect to :1081; "
        "meta mark 0x100 return; ip daddr 127.0.0.2 tcp dport 2298 redirect to :1081 } }")
    ls = [listener(V4, "127.0.0.1", 1081)] + [listener(V4, "127.0.0.2", p) for p in range(2290, 2299)]
    for port, cases in [
        (2290, [(0, None), (65534, None)]),
        (2291, [(0, None), (65534, None)]),
        (2292, [(0, None), (65534, None)]),
        (2293, [(0, None), (0, 7), (0, 8)]),
        (2294, [(0, 0x13), (0, 0x23), (0, None)]),
        (2295, [(0, None), (65534, None)]),
        (2296, [(0, 3), (0, None)]),
        (2297, [(0, None), (65534, None)]),
        (2298, [(0, None), (0, 0x100), (0, 0x101)]),
    ]:
        for uid, mark in cases:
            m = "" if mark is None else f" mark {mark:#x}"
            print(f"{port} uid {uid}{m}: {where(port, uid, mark, ls)}")


def proxy():
    """A transparent proxy: accepts connections REDIRECTed to it, asks SO_ORIGINAL_DST, connects
    to the original destination with mark 0x100 (the rules let it through), and relays one
    request and its reply."""
    nft("table ip p { chain out { type nat hook output priority -100; "
        "meta mark 0x100 return; ip daddr 127.0.0.0/8 tcp dport { 2290, 2291 } redirect to :1081 } }")
    origin = [listener(V4, "127.0.0.2", 2290), listener(V4, "127.0.0.3", 2291)]
    px = listener(V4, "127.0.0.1", 1081)
    pid = os.fork()
    if pid == 0:
        for _ in range(2):
            a, _ = px.accept()
            dst = a.getsockopt(socket.SOL_IP, SO_ORIGINAL_DST, 16)
            ip, port = socket.inet_ntoa(dst[4:8]), struct.unpack(">H", dst[2:4])[0]
            up = socket.socket()
            up.setsockopt(socket.SOL_SOCKET, SO_MARK, 0x100)
            up.connect((ip, port))
            up.sendall(a.recv(100) + f" (via proxy to {ip}:{port})".encode())
            a.sendall(up.recv(200))
            a.close()
            up.close()
        os._exit(0)
    for ip, port in [("127.0.0.2", 2290), ("127.0.0.3", 2291)]:
        c = socket.create_connection((ip, port))
        c.sendall(f"hello {ip}:{port}".encode())
        r, _, _ = select.select(origin, [], [], 5)
        s, peer = r[0].accept()
        got = s.recv(200).decode()
        s.sendall(f"origin {s.getsockname()[0]}:{s.getsockname()[1]} got '{got}'".encode())
        s.close()
        print(c.recv(300).decode())
        c.close()
    os.waitpid(pid, 0)


def srv(ports, ready, n):
    ls = [listener(V4, "0.0.0.0", int(p)) for p in ports]
    open(ready, "w").close()
    for _ in range(int(n)):
        r, _, _ = select.select(ls, [], [], 15)
        if not r:
            print("server: nothing accepted")
            return
        a, _ = r[0].accept()
        name = a.recv(100).decode()
        me = a.getsockname()
        a.sendall(f"{name}: server {me[0]}:{me[1]} sees orig {orig(a)}".encode())
        a.close()


def cli(addr, port, name, mark=None):
    c = socket.socket()
    if mark is not None:
        c.setsockopt(socket.SOL_SOCKET, SO_MARK, int(mark))
    try:
        c.connect((addr, int(port)))
    except OSError as e:
        print(f"{name}: {err(e)}")
        return
    c.sendall(name.encode())
    print(c.recv(200).decode(), f"| client orig {orig(c)}")


def usrv(port, ready, n):
    s = socket.socket(V4, socket.SOCK_DGRAM)
    s.bind(("0.0.0.0", int(port)))
    open(ready, "w").close()
    for _ in range(int(n)):
        data, peer = s.recvfrom(200)
        s.sendto(f"{data.decode()}: udp server {port} sees {peer[0]}".encode(), peer)


def ucli(addr, port, name):
    s = socket.socket(V4, socket.SOCK_DGRAM)
    s.sendto(name.encode(), (addr, int(port)))
    data, peer = s.recvfrom(200)
    print(f"{data.decode()} | reply from {peer[0]}:{peer[1]}")
    c = socket.socket(V4, socket.SOCK_DGRAM)
    c.connect((addr, int(port)))
    c.send(f"{name}, connected".encode())
    data = c.recv(200).decode()
    print(f"{data} | peer {c.getpeername()[0]}:{c.getpeername()[1]}")


cmd = sys.argv[1]
if cmd == "probe":
    probe(sys.argv[2])
elif cmd == "marks":
    marks()
elif cmd == "owner":
    owner()
elif cmd == "proxy":
    proxy()
elif cmd == "srv":
    srv(sys.argv[2:-2], sys.argv[-2], sys.argv[-1])
elif cmd == "cli":
    cli(*sys.argv[2:])
elif cmd == "usrv":
    usrv(*sys.argv[2:])
elif cmd == "ucli":
    ucli(*sys.argv[2:])
PY
fresh() { unshare -n sh -c "ip link set lo up; python3 /tmp/od.py $*"; }

for sc in none chain filterct inetct redirect iptables ip6 inet; do
    echo "== SO_ORIGINAL_DST: $sc"
    fresh probe "$sc"
done
echo "== SO_MARK"
fresh marks
echo "== owner and mark matches"
fresh owner
echo "== a transparent proxy"
fresh proxy

echo "== across namespaces"
wait_for() { for i in $(seq 100); do [ -f "$1" ] && return; sleep 0.1; done; echo "timeout: $1"; }
# The outer namespace forwards what is DNATed to B
echo 1 > /proc/sys/net/ipv4/ip_forward
ip link add br0 address 02:00:00:00:50:01 type bridge
ip addr add 10.50.0.1/24 dev br0
ip link set br0 up
container() {
    unshare -n sleep 600 &
    eval "$1=$!"
    sleep 0.3
    ip link add "veth$1" type veth peer name eth0 netns "$(eval echo \$$1)"
    ip link set "veth$1" master br0
    ip link set "veth$1" up
    nsenter -t "$(eval echo \$$1)" -n sh -c "ip link set lo up; ip link set eth0 up; ip addr add $2/24 dev eth0; ip route add default via 10.50.0.1"
}
container A 10.50.0.2
container B 10.50.0.3
sleep 2
# B's PREROUTING: mark 7 is let through (the mark has already been cleared on coming in through
# the veth)
nsenter -t "$B" -n nft -f - <<'N'
table ip t {
  chain pre {
    type nat hook prerouting priority -100
    meta mark 7 return
    tcp dport 2290 redirect to :1081
  }
}
N
nsenter -t "$A" -n nft -f - <<'N'
table ip t {
  chain out {
    type nat hook output priority -100
    ip daddr 10.50.0.99 tcp dport 80 dnat to 10.50.0.3:1082
  }
}
N
# Outer namespace: 2290 from the bridge is REDIRECTed to the proxy at 1083; for 2292 only what is
# sent to a local address goes to 1083 (docker's and podman's rules for published ports restrict
# it this way), the rest to 1084; 2293 and 2294 are DNATed to B, and FORWARD rejects B's 1085 (B
# listens there); UDP port 53 is hijacked to 1053. A and B are in the same subnet, so what is
# DNATed to B needs masquerade for B's replies to pass through the outer namespace and be
# translated back (Tuxport does not execute POSTROUTING; the connections are at the socket layer
# anyway)
nft -f - <<'N'
table ip t {
  chain pre {
    type nat hook prerouting priority -100
    iif br0 tcp dport 2290 redirect to :1083
    fib daddr type local tcp dport 2292 redirect to :1083
    iif br0 tcp dport 2292 redirect to :1084
    iif br0 tcp dport 2293 dnat to 10.50.0.3:1082
    iif br0 tcp dport 2294 dnat to 10.50.0.3:1085
    iif br0 udp dport 53 redirect to :1053
  }
  chain post {
    type nat hook postrouting priority 100
    ip saddr 10.50.0.2 ip daddr 10.50.0.3 masquerade
  }
  chain fw {
    type filter hook forward priority 0
    iif br0 oif br0 tcp dport 1085 reject with tcp reset
  }
}
N
rm -f /tmp/ready-b /tmp/ready-b2 /tmp/ready-outer /tmp/ready-udp
nsenter -t "$B" -n python3 /tmp/od.py srv 1081 1082 /tmp/ready-b 5 &
b_srv=$!
# Everything connecting here should be rejected by the outer FORWARD: stop it once done
nsenter -t "$B" -n python3 /tmp/od.py srv 1085 /tmp/ready-b2 2 &
b_rejected=$!
python3 /tmp/od.py srv 1083 1084 /tmp/ready-outer 4 &
outer_srv=$!
wait_for /tmp/ready-b
wait_for /tmp/ready-b2
wait_for /tmp/ready-outer
nsenter -t "$A" -n python3 /tmp/od.py cli 10.50.0.3 2290 "A to B, PREROUTING redirect"
nsenter -t "$A" -n python3 /tmp/od.py cli 10.50.0.3 2290 "A to B with mark 7" 7
nsenter -t "$A" -n python3 /tmp/od.py cli 10.50.0.99 80 "A OUTPUT dnat to B"
python3 /tmp/od.py cli 10.50.0.3 2290 "outer to B"
nsenter -t "$A" -n python3 /tmp/od.py cli 10.50.0.1 2290 "A to the outer gateway"
nsenter -t "$A" -n python3 /tmp/od.py cli 192.0.2.10 2290 "A to an outside address"
nsenter -t "$A" -n python3 /tmp/od.py cli 10.50.0.1 2292 "A to the gateway, local-only rule"
nsenter -t "$A" -n python3 /tmp/od.py cli 192.0.2.10 2292 "A to an outside address, local-only rule"
nsenter -t "$A" -n python3 /tmp/od.py cli 192.0.2.10 2293 "A to an outside address, dnat to B"
nsenter -t "$A" -n python3 /tmp/od.py cli 192.0.2.10 2294 "A to an outside address, dnat to B, outer FORWARD rejects"
nsenter -t "$A" -n python3 /tmp/od.py cli 10.50.0.1 2294 "A to the gateway, dnat to B, outer FORWARD rejects"
kill "$b_rejected"
wait "$b_rejected" 2>/dev/null
wait "$b_srv" "$outer_srv"
python3 /tmp/od.py usrv 1053 /tmp/ready-udp 4 &
udp_srv=$!
wait_for /tmp/ready-udp
nsenter -t "$A" -n python3 /tmp/od.py ucli 10.50.0.1 1053 "A udp to the gateway"
nsenter -t "$A" -n python3 /tmp/od.py ucli 192.0.2.10 53 "A udp to an outside address"
wait "$udp_srv"
kill "$A" "$B"
wait "$A" "$B" 2>/dev/null
nft delete table ip t
ip link del br0
