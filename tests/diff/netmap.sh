# docker-run-options: --privileged
# Connections through virtual network devices (docs/rounds/171-netmap.md): a bridge
# (10.50.0.1/24) in the boot-time namespace, and two new network namespaces A and B that, like
# containers, each have a veth attached to it (10.50.0.2, 10.50.0.3, default route through
# 10.50.0.1). Compare whether TCP, UDP and ping get through between A and B, between the outer
# namespace and A, and between A and the outer gateway address, the local and peer addresses
# both ends see, low ports, /proc/net, and the errors for unreachable destinations. The native
# side is a privileged container, where packets really go through the bridge.
#
# - What leaves the container through the default route is not compared (natively it depends on
#   the host's forwarding and NAT). Only compare, for one UDP socket sending first to an address
#   in a namespace and then to the outer default gateway (outside the container, which never
#   answers), whether the sends get out and whether the one in the namespace answers (a socket
#   that calls sendto without binding first; it used to get bound to the loopback address when
#   the first destination was in a namespace).
# - An address absent from a directly connected subnet: natively it is EHOSTUNREACH only after
#   the ARP timeout (about 3 seconds), in Tuxport at once; after the peer's namespace is gone the
#   native ARP cache remains, and connecting to its address has to wait for the timeout, so this
#   item is not compared.
exec 2>&1
cd /tmp
cat > srv.py <<'PY'
import os, socket, sys
# srv.py tcp|udp <address> <port> <count> <ready file>; exits after 15 seconds without a connection
socket.setdefaulttimeout(15)
kind, addr, port, n, ready = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
fam = socket.AF_INET6 if ":" in addr else socket.AF_INET
if kind == "tcp":
    s = socket.socket(fam, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind((addr, port))
    s.listen()
    open(ready, "w").close()
    for _ in range(n):
        c, peer = s.accept()
        data = c.recv(100).decode()
        c.sendall(f"{data}: server sees {peer[0]} -> {c.getsockname()[0]}".encode())
        c.close()
else:
    s = socket.socket(fam, socket.SOCK_DGRAM)
    s.bind((addr, port))
    open(ready, "w").close()
    for _ in range(n):
        data, peer = s.recvfrom(100)
        s.sendto(f"{data.decode()}: server sees {peer[0]}".encode(), peer)
PY
cat > cli.py <<'PY'
import errno, socket, sys
# cli.py tcp|udp|udpbind|udpconn <address> <port> <message>: udpbind first binds the wildcard
# address and port 0, udpconn then also connects (busybox's nslookup does this)
kind, addr, port, msg = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
fam = socket.AF_INET6 if ":" in addr else socket.AF_INET
try:
    if kind == "tcp":
        c = socket.socket(fam, socket.SOCK_STREAM)
        c.settimeout(10)
        c.connect((addr, port))
        print(f"{msg}: client {c.getsockname()[0]} -> {c.getpeername()[0]}")
        c.sendall(msg.encode())
        print(c.recv(200).decode())
    else:
        c = socket.socket(fam, socket.SOCK_DGRAM)
        c.settimeout(10)
        if kind in ("udpbind", "udpconn"):
            c.bind(("0.0.0.0", 0))
        if kind == "udpconn":
            before = c.getsockname()[1]
            c.connect((addr, port))
            # The port does not change (Tuxport used to release the port the host assigned here)
            same = c.getsockname()[1] == before
            print(f"{msg}: connected {c.getsockname()[0]} -> {c.getpeername()[0]}, same port {same}")
            c.send(msg.encode())
        else:
            c.sendto(msg.encode(), (addr, port))
        data, peer = c.recvfrom(200)
        print(f"{data.decode()}; reply from {peer[0]}:{peer[1]}, client {c.getsockname()[0]}")
except OSError as e:
    print(f"{msg}: {errno.errorcode.get(e.errno, e.errno)}")
PY
cat > multi.py <<'PY'
import errno, select, socket, sys
# multi.py <name> <address:port>...: one unbound UDP socket sends to them in turn, then receives
# the replies
name, dests = sys.argv[1], sys.argv[2:]
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sends = []
for i, d in enumerate(dests):
    host, port = d.rsplit(":", 1)
    try:
        s.sendto(f"m{i}".encode(), (host, int(port)))
        sends.append("ok")
    except OSError as e:
        sends.append(errno.errorcode.get(e.errno, e.errno))
got = []
while select.select([s], [], [], 1)[0]:
    got.append(s.recvfrom(200)[0].decode().split(":")[0])
print(f"{name}: sends {sends}, answered {sorted(got)}")
PY
wait_for() { for i in $(seq 100); do [ -f "$1" ] && return; sleep 0.1; done; echo "timeout: $1"; }
# Start servers in the background in the namespace of (pid); served waits for them to end (wait
# cannot be called without arguments: the processes of A and B are children too)
servers=""
serve() {
    pid=$1; shift; rm -f "/tmp/ready-$1-$3"
    nsenter -t "$pid" -n python3 /tmp/srv.py "$@" "/tmp/ready-$1-$3" &
    servers="$servers $!"
    wait_for "/tmp/ready-$1-$3"
}
served() { wait $servers; servers=""; }

# Set the bridge's MAC as docker does: otherwise it takes the smallest of its ports' MACs, which
# changes when a port is deleted, and natively the ARP caches of other containers no longer match
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

echo "== A <-> B over TCP"
serve "$A" tcp 0.0.0.0 8080 3
nsenter -t "$B" -n python3 cli.py tcp 10.50.0.2 8080 b-to-a
echo "== outer -> A"
python3 cli.py tcp 10.50.0.2 8080 outer-to-a
echo "== A -> itself over its own address"
nsenter -t "$A" -n python3 cli.py tcp 10.50.0.2 8080 a-to-a
served

echo "== bound to the eth0 address, IPv6 wildcard"
serve "$A" tcp 10.50.0.2 8081 1
nsenter -t "$B" -n python3 cli.py tcp 10.50.0.2 8081 b-to-a-specific
serve "$A" tcp :: 8082 1
nsenter -t "$B" -n python3 cli.py tcp 10.50.0.2 8082 b-to-a-v6any
served

echo "== A -> the gateway"
serve $$ tcp 10.50.0.1 9090 1
nsenter -t "$A" -n python3 cli.py tcp 10.50.0.1 9090 a-to-gateway
serve $$ tcp 0.0.0.0 9091 1
nsenter -t "$A" -n python3 cli.py tcp 10.50.0.1 9091 a-to-gateway-any
served

echo "== UDP"
serve "$A" udp 10.50.0.2 5353 1
nsenter -t "$B" -n python3 cli.py udp 10.50.0.2 5353 b-to-a-udp
serve $$ udp 10.50.0.1 5354 1
nsenter -t "$B" -n python3 cli.py udp 10.50.0.1 5354 b-to-gateway-udp
serve "$A" udp 0.0.0.0 5355 2
nsenter -t "$B" -n python3 cli.py udpbind 10.50.0.2 5355 b-bound-any-to-a
nsenter -t "$B" -n python3 cli.py udpconn 10.50.0.2 5355 b-bound-any-connected-to-a
served

echo "== one UDP socket, inside and then outside the namespaces"
outside=$(ip -4 route show default | awk '{print $3; exit}')
serve $$ udp 0.0.0.0 5356 2
serve "$A" udp 127.0.0.1 5357 1
nsenter -t "$A" -n python3 multi.py gateway-then-outside 10.50.0.1:5356 "$outside:5356"
nsenter -t "$A" -n python3 multi.py outside-then-gateway "$outside:5356" 10.50.0.1:5356
nsenter -t "$A" -n python3 multi.py loopback-then-outside 127.0.0.1:5357 "$outside:5356"
served

echo "== low ports"
serve "$A" tcp 0.0.0.0 80 1
nsenter -t "$B" -n python3 cli.py tcp 10.50.0.2 80 b-to-a-80
serve "$A" udp 0.0.0.0 53 1
python3 cli.py udp 10.50.0.2 53 outer-to-a-53
served

echo "== ping"
nsenter -t "$A" -n ping -c 1 -W 3 10.50.0.3 | grep -E "bytes from|transmitted" | sed -E 's/time=[0-9.]+ ms/time=T/'
nsenter -t "$A" -n ping -c 1 -W 3 10.50.0.1 | grep -E "bytes from|transmitted" | sed -E 's/time=[0-9.]+ ms/time=T/'
ping -c 1 -W 3 10.50.0.3 | grep -E "bytes from|transmitted" | sed -E 's/time=[0-9.]+ ms/time=T/'

echo "== ping sockets (no CAP_NET_RAW)"
# In a new namespace ping_group_range allows nobody by default (natively)
nsenter -t "$A" -n sh -c 'echo "0 2147483647" > /proc/sys/net/ipv4/ping_group_range'
for dst in 10.50.0.1 10.50.0.3; do
    nsenter -t "$A" -n python3 -c "import os; os.setgid(65534); os.setuid(65534); os.execvp('ping', ['ping', '-c', '1', '-W', '3', '$dst'])" |
        grep -E "bytes from|transmitted" | sed -E 's/time=[0-9.]+ ms/time=T/'
done

echo "== /proc/net/tcp"
# Connections and listeners still open (Tuxport does not see the TIME_WAIT ones), ports other than
# 8083 replaced with PORT
serve "$A" tcp 0.0.0.0 8083 1
APID=$A nsenter -t "$B" -n python3 -c '
import os, socket, subprocess, time
c = socket.create_connection(("10.50.0.2", 8083))
time.sleep(0.5)
def show(who, text):
    rows = []
    for l in text.splitlines()[1:]:
        f = l.split()
        if f[3] not in ("01", "0A"):
            continue
        ends = [e if e.endswith(":1F93") else e.split(":")[0] + ":PORT" for e in f[1:3]]
        rows.append(" ".join([who, *ends, f[3]]))
    # Row order is not compared (Linux lists listeners first, Tuxport by the order of the fds)
    print("\n".join(sorted(rows)))
show("B", open("/proc/net/tcp").read())
a = subprocess.run(["nsenter", "-t", os.environ["APID"], "-n", "cat", "/proc/net/tcp"], capture_output=True, text=True)
show("A", a.stdout)
c.sendall(b"x")
c.recv(100)
'
served

echo "== unreachable"
nsenter -t "$A" -n python3 cli.py tcp 10.50.0.9 8080 no-such-host
nsenter -t "$A" -n python3 cli.py tcp 10.50.0.3 8999 nothing-listening
nsenter -t "$A" -n sh -c 'ip route del default; python3 /tmp/cli.py tcp 192.0.2.1 80 no-route; python3 /tmp/cli.py udp 10.60.0.1 53 no-route-udp'
nsenter -t "$A" -n python3 -c '
import socket, errno
s = socket.socket()
for a in ("10.50.0.3", "10.50.0.1", "10.50.0.2"):
    try:
        s = socket.socket(); s.bind((a, 0)); print("bind", a, "ok")
    except OSError as e:
        print("bind", a, errno.errorcode[e.errno])
'

echo "== after A is gone"
kill "$A"; wait "$A" 2>/dev/null
for i in $(seq 50); do ip link show vethA >/dev/null 2>&1 || break; sleep 0.1; done
ip -o link | grep -c vethA
serve $$ tcp 10.50.0.1 9092 1
nsenter -t "$B" -n python3 cli.py tcp 10.50.0.1 9092 b-to-gateway
served
kill "$B"; wait "$B" 2>/dev/null
ip link del br0
