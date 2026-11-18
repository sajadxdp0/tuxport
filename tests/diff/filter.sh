# docker-run-options: --privileged
# Filter chains (docs/rounds/175-filter.md): two bridges in the boot-time namespace
# (br0 10.80.0.1/24, br1 10.81.0.1/24), A attached to br0 and B to br1, routed through the outer
# namespace. With filter rules (nftables and iptables-nft ones) on the outer FORWARD and INPUT,
# A's OUTPUT and B's INPUT, compare the results of A connecting to B and to the outer namespace:
# dropped ones time out, rejected ones get ECONNREFUSED, ones dropped on OUTPUT get EPERM, and
# replies on established connections go through as usual.
exec 2>&1
cd /tmp
cat > srv.py <<'PY'
import socket, sys
socket.setdefaulttimeout(8)
kind, addr, port, n, ready = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
if kind == "tcp":
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind((addr, port)); s.listen()
    open(ready, "w").close()
    for _ in range(n):
        try:
            c, peer = s.accept()
        except OSError:
            break
        c.sendall(f"{c.recv(100).decode()}: server sees {peer[0]}".encode()); c.close()
else:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind((addr, port))
    open(ready, "w").close()
    for _ in range(n):
        try:
            data, peer = s.recvfrom(100)
        except OSError:
            break
        s.sendto(f"{data.decode()}: server sees {peer[0]}".encode(), peer)
PY
cat > cli.py <<'PY'
import errno, socket, sys, time
kind, addr, port, msg = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
start = time.monotonic()
try:
    if kind == "tcp":
        c = socket.socket(); c.settimeout(2)
        c.connect((addr, port)); c.sendall(msg.encode()); print(c.recv(200).decode())
    else:
        c = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); c.settimeout(2)
        c.sendto(msg.encode(), (addr, port)); data, peer = c.recvfrom(200)
        print(f"{data.decode()}; reply from {peer[0]}:{peer[1]}")
except TimeoutError:
    # A dropped one has to wait for the timeout (2 seconds, set here), it does not fail at once
    waited = "after waiting" if time.monotonic() - start >= 1.5 else "at once"
    print(f"{msg}: timed out {waited}")
except OSError as e:
    print(f"{msg}: {errno.errorcode.get(e.errno, e.errno)}")
PY
wait_for() { for i in $(seq 100); do [ -f "$1" ] && return; sleep 0.1; done; echo "timeout: $1"; }
servers=""
serve() {
    pid=$1; shift; rm -f "/tmp/ready-$1-$3"
    nsenter -t "$pid" -n python3 /tmp/srv.py "$@" "/tmp/ready-$1-$3" &
    servers="$servers $!"
    wait_for "/tmp/ready-$1-$3"
}
served() { kill $servers 2>/dev/null; wait $servers 2>/dev/null; servers=""; }
echo 1 > /proc/sys/net/ipv4/ip_forward
for n in 0 1; do
    ip link add br$n address 02:00:00:00:8$n:01 type bridge
    ip addr add 10.8$n.0.1/24 dev br$n
    ip link set br$n up
done
container() {
    unshare -n sleep 600 &
    eval "$1=$!"
    sleep 0.3
    ip link add "veth$1" type veth peer name eth0 netns "$(eval echo \$$1)"
    ip link set "veth$1" master "$2"
    ip link set "veth$1" up
    nsenter -t "$(eval echo \$$1)" -n sh -c "ip link set lo up; ip link set eth0 up; ip addr add $3/24 dev eth0; ip route add default via $4"
}
container A br0 10.80.0.2 10.80.0.1
container B br1 10.81.0.3 10.81.0.1
sleep 2
nft -f - <<'NFT'
table inet f {
	chain FORWARD {
		type filter hook forward priority filter; policy accept;
		ct state established,related accept
		iifname "br0" oifname "br1" tcp dport 7001 drop
		iifname "br0" oifname "br1" tcp dport 7002 reject with tcp reset
		iifname "br0" oifname "br1" udp dport 7003 drop
		iifname "br0" oifname "br1" tcp dport 7008 reject
	}
	chain INPUT {
		type filter hook input priority filter; policy accept;
		iifname "br0" tcp dport 7004 drop
	}
}
NFT
echo "outer rules: $?"
# New B -> A connections are dropped by the iptables-nft rules; established ones (the replies to
# A -> B) are let through
iptables -A FORWARD -i br1 -o br0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
iptables -A FORWARD -i br1 -o br0 -j DROP
echo "outer iptables rules: $?"
nsenter -t "$B" -n nft -f - <<'NFT'
table ip b {
	chain INPUT {
		type filter hook input priority filter; policy accept;
		tcp dport 7005 reject
	}
}
NFT
echo "B rules: $?"
nsenter -t "$A" -n sh -c 'iptables -A OUTPUT -d 10.81.0.3 -p tcp --dport 7006 -j DROP; iptables -A OUTPUT -d 10.81.0.3 -p tcp --dport 7007 -j REJECT; iptables -A OUTPUT -d 10.81.0.3 -p udp --dport 7011 -j DROP; echo "A rules: $?"'

echo "== A -> B"
for p in 7000 7001 7002 7005 7006 7007 7008; do serve "$B" tcp 0.0.0.0 $p 1; done
for p in 7000 7001 7002 7005 7006 7007 7008; do nsenter -t "$A" -n python3 cli.py tcp 10.81.0.3 $p a-to-b-$p; done
# Non-blocking connect: a dropped one stays in progress
nsenter -t "$A" -n python3 -c '
import errno, select, socket
s = socket.socket(); s.setblocking(False)
print("nonblocking connect to a dropped port:", errno.errorcode.get(s.connect_ex(("10.81.0.3", 7001))))
print("writable within 1s:", bool(select.select([], [s], [], 1)[1]))'
served
echo "== UDP"
serve "$B" udp 0.0.0.0 7000 1
serve "$B" udp 0.0.0.0 7003 1
nsenter -t "$A" -n python3 cli.py udp 10.81.0.3 7000 a-to-b-udp-7000
nsenter -t "$A" -n python3 cli.py udp 10.81.0.3 7003 a-to-b-udp-7003
nsenter -t "$A" -n python3 cli.py udp 10.81.0.3 7011 a-to-b-udp-7011
served
echo "== B -> A is dropped, A -> B and its replies are not"
serve "$A" tcp 0.0.0.0 7010 1
serve "$A" udp 0.0.0.0 7012 1
nsenter -t "$B" -n python3 cli.py tcp 10.80.0.2 7010 b-to-a
nsenter -t "$B" -n python3 cli.py udp 10.80.0.2 7012 b-to-a-udp
served
echo "== A -> the outer namespace"
serve $$ tcp 0.0.0.0 7004 1
serve $$ tcp 0.0.0.0 7009 1
nsenter -t "$A" -n python3 cli.py tcp 10.80.0.1 7004 a-to-outer-7004
nsenter -t "$A" -n python3 cli.py tcp 10.80.0.1 7009 a-to-outer-7009
served
echo "== the outer namespace itself (OUTPUT, not FORWARD)"
serve "$B" tcp 0.0.0.0 7001 1
python3 cli.py tcp 10.81.0.3 7001 outer-to-b-7001
served
kill "$A" "$B"; wait 2>/dev/null
ip link del br0; ip link del br1
