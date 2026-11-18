# docker-run-options: --privileged
# Port forwarding (docs/rounds/173-port-forwarding.md): two bridges in the boot-time namespace
# (br0 10.60.0.1/24, br1 10.61.0.1/24), and new network namespaces A and B each attached to one
# like containers (10.60.0.2, 10.61.0.3, in different subnets: B's reply goes through the
# boot-time namespace, which is what lets NAT translate it back). The boot-time namespace has nat
# rules in PREROUTING (the way podman and docker publish ports), and A connects to ports published
# on the gateway address and the outer address, forwarded through the boot-time namespace to B:
# compare the peer A sees, the source of the reply, the source and local end B sees, iifname
# matching, redirect, the boot-time namespace connecting itself (not through PREROUTING), and a
# listener that holds the port and never accepts, as podman does. The native side is a
# privileged container, where packets really go through the bridges and nat.
#
# - The outer namespace connecting directly to 8080 on B, where nothing listens, is not compared:
#   in Tuxport the outer listener holding 8080 binds the wildcard address on the host, which also
#   covers B's loopback address on the host, so the connection succeeds (natively ECONNREFUSED).
# - The iifname rule uses an interface that does not exist: when Tuxport decides whether to
#   forward a connection from outside, it assumes the connection came in on the host's first
#   interface; if the rule holds for it, this outer listener is handed to the forwarding thread,
#   and A's connections are taken by it too (deferred).
# - Containers in the same subnet connecting to a published port through the gateway, and a
#   container connecting back to its own published port (hairpin), are not compared: natively
#   the reply does not go through the gateway, and without masquerade the connection fails
#   (netavark adds masquerade for this, Tuxport does not execute it).
exec 2>&1
cd /tmp
cat > srv.py <<'PY'
import socket, sys
# srv.py tcp|udp <address> <port> <count> <ready file>; exits after 15 seconds without a connection
socket.setdefaulttimeout(15)
kind, addr, port, n, ready = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
if kind == "tcp":
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind((addr, port))
    s.listen()
    open(ready, "w").close()
    for _ in range(n):
        c, peer = s.accept()
        data = c.recv(100).decode()
        me = c.getsockname()
        c.sendall(f"{data}: server sees {peer[0]} -> {me[0]}:{me[1]}".encode())
        c.close()
else:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind((addr, port))
    open(ready, "w").close()
    for _ in range(n):
        data, peer = s.recvfrom(100)
        s.sendto(f"{data.decode()}: server sees {peer[0]}".encode(), peer)
PY
cat > cli.py <<'PY'
import errno, socket, sys
# cli.py tcp|udp|udpconn <address> <port> <message>
kind, addr, port, msg = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
try:
    if kind == "tcp":
        c = socket.socket()
        c.settimeout(5)
        c.connect((addr, port))
        p = c.getpeername()
        print(f"{msg}: connected to {p[0]}:{p[1]}")
        c.sendall(msg.encode())
        print(c.recv(200).decode())
    else:
        c = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        c.settimeout(5)
        if kind == "udpconn":
            c.connect((addr, port))
            c.send(msg.encode())
        else:
            c.sendto(msg.encode(), (addr, port))
        data, peer = c.recvfrom(200)
        print(f"{data.decode()}; reply from {peer[0]}:{peer[1]}")
except OSError as e:
    print(f"{msg}: {errno.errorcode.get(e.errno, e.errno)}")
PY
wait_for() { for i in $(seq 100); do [ -f "$1" ] && return; sleep 0.1; done; echo "timeout: $1"; }
# Start servers in the background in the namespace of (pid); served waits for them to end
servers=""
serve() {
    pid=$1; shift; rm -f "/tmp/ready-$1-$3"
    nsenter -t "$pid" -n python3 /tmp/srv.py "$@" "/tmp/ready-$1-$3" &
    servers="$servers $!"
    wait_for "/tmp/ready-$1-$3"
}
served() { wait $servers; servers=""; }
# The outer address (the container's eth0) is replaced with HOST
host=$(ip -4 -o addr show eth0 | awk '{ print $4 }' | cut -d/ -f1)
tidy() { sed "s/$host/HOST/g"; }

echo 1 > /proc/sys/net/ipv4/ip_forward
for n in 0 1; do
    ip link add br$n address 02:00:00:00:6$n:01 type bridge
    ip addr add 10.6$n.0.1/24 dev br$n
    ip link set br$n up
done
# container <name> <bridge> <address> <gateway>
container() {
    unshare -n sleep 600 &
    eval "$1=$!"
    sleep 0.3
    ip link add "veth$1" type veth peer name eth0 netns "$(eval echo \$$1)"
    ip link set "veth$1" master "$2"
    ip link set "veth$1" up
    nsenter -t "$(eval echo \$$1)" -n sh -c "ip link set lo up; ip link set eth0 up; ip addr add $3/24 dev eth0; ip route add default via $4"
}
container A br0 10.60.0.2 10.60.0.1
container B br1 10.61.0.3 10.61.0.1
sleep 2
nft -f - <<'NFT'
table ip pf {
	chain pre {
		type nat hook prerouting priority dstnat; policy accept;
		fib daddr type local tcp dport 8080 dnat to 10.61.0.3:80
		fib daddr type local udp dport 5353 dnat to 10.61.0.3:53
		iifname "eth9" tcp dport 8081 dnat to 10.61.0.3:80
		tcp dport 8082 redirect to :9000
	}
}
NFT
echo "load: $?"
# Hold 8080 and never accept, as podman does
python3 -c '
import socket, time
s = socket.socket(); s.bind(("0.0.0.0", 8080)); s.listen(); open("/tmp/held", "w").close(); time.sleep(60)' &
holder=$!
wait_for /tmp/held

echo "== A -> the gateway and the outer address, TCP 8080"
serve "$B" tcp 0.0.0.0 80 2
nsenter -t "$A" -n python3 cli.py tcp 10.60.0.1 8080 a-to-gw | tidy
nsenter -t "$A" -n python3 cli.py tcp "$host" 8080 a-to-host | tidy
served
echo "== UDP 5353"
serve "$B" udp 0.0.0.0 53 2
nsenter -t "$A" -n python3 cli.py udp 10.60.0.1 5353 a-udp | tidy
nsenter -t "$A" -n python3 cli.py udpconn "$host" 5353 a-udpconn | tidy
served
echo "== iifname: from br0, not eth9: no DNAT"
serve $$ tcp 0.0.0.0 8081 1
nsenter -t "$A" -n python3 cli.py tcp 10.60.0.1 8081 a-8081 | tidy
served
echo "== redirect"
serve $$ tcp 0.0.0.0 9000 2
nsenter -t "$A" -n python3 cli.py tcp 10.60.0.1 8082 a-redirect | tidy
nsenter -t "$A" -n python3 cli.py tcp "$host" 8082 a-redirect-host | tidy
served
echo "== the outer namespace itself: OUTPUT only"
python3 cli.py tcp 10.60.0.1 8082 local-8082 | tidy
echo "== without the rules"
nft delete table ip pf
nsenter -t "$A" -n python3 cli.py tcp 10.60.0.1 8082 a-no-rules | tidy
kill "$holder" "$A" "$B"; wait 2>/dev/null
ip link del br0
ip link del br1
