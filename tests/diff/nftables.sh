# docker-run-options: --privileged
# A subset of nftables (docs/rounds/172-nftables.md): in a new network namespace
# - build with iptables-nft the built-in DNS rules docker writes into containers (-N, -C, -I,
#   DNAT, SNAT), and look at -S, nft list ruleset, and UDP and TCP sent to 127.0.0.11:53 being
#   DNATed to the port the service really listens on: the peer the client sees, the source of the
#   reply, and the source and local end the server sees;
# - load with nft -f a rule set like netavark's (inet table, anonymous sets, named sets, fib,
#   dnat, redirect), and look at the listing with handles, port forwarding, and the state after
#   deleting rules;
# - error cases: jumping to a missing chain, loops, deleting a referenced chain, an already
#   existing chain, no privilege;
# - flush ruleset, of one family and of all, also as the first line of a file loaded with nft -f.
# The native side is a privileged container, where the rules really run in the kernel. Counters
# (always 0 in Tuxport) are all removed.
exec 2>&1
cd /tmp
cat > srv.py <<'PY'
import socket, sys
# srv.py tcp|udp <address> <port> <count> <ready file>
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
import errno, os, socket, sys, time
# cli.py tcp|udp|udpconn <address> <port> <message> [<file>]: when a file is given, after sending
# over UDP wait until it appears (the server has exited) before reading the reply
kind, addr, port, msg = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
late = sys.argv[5] if len(sys.argv) > 5 else None
try:
    if kind == "tcp":
        c = socket.socket()
        c.settimeout(10)
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
            p = c.getpeername()
            print(f"{msg}: connected to {p[0]}:{p[1]}")
            c.send(msg.encode())
        else:
            c.sendto(msg.encode(), (addr, port))
        for _ in range(100 if late else 0):
            if os.path.exists(late):
                break
            time.sleep(0.1)
        data, peer = c.recvfrom(200)
        print(f"{data.decode()}; reply from {peer[0]}:{peer[1]}")
except OSError as e:
    print(f"{msg}: {errno.errorcode.get(e.errno, e.errno)}")
PY
# Functions shared by the two unshare -n shells
cat > /tmp/fns.sh <<'SH'
wait_for() { for i in $(seq 100); do [ -f "$1" ] && return; sleep 0.1; done; echo "timeout: $1"; }
servers=""
serve() {
    rm -f "/tmp/ready-$3"
    python3 /tmp/srv.py "$@" "/tmp/ready-$3" &
    servers="$servers $!"
    wait_for "/tmp/ready-$3"
}
served() { wait $servers; servers=""; }
# Natively table handles do not always start at 1 (within one namespace they come from a counter,
# see docs/rounds/172-nftables.md), so they are not compared
nocount() { sed -E 's/counter packets [0-9]+ bytes [0-9]+/counter/; s/^(table .*) # handle [0-9]+$/\1/'; }
SH

unshare -n sh -c '
. /tmp/fns.sh
ip link set lo up
echo "== docker embedded DNS rules (iptables-nft)"
T() { "$@" >/tmp/out 2>&1; c=$?; sed "s/^/  /" /tmp/out; echo "[$c] $*" | sed "s/ --wait//"; }
T iptables --wait -t nat -N DOCKER_OUTPUT
T iptables --wait -t nat -C OUTPUT -d 127.0.0.11 -j DOCKER_OUTPUT
T iptables --wait -t nat -I OUTPUT -d 127.0.0.11 -j DOCKER_OUTPUT
T iptables --wait -t nat -I DOCKER_OUTPUT -d 127.0.0.11 -p udp --dport 53 -j DNAT --to-destination 127.0.0.11:40001
T iptables --wait -t nat -I DOCKER_OUTPUT -d 127.0.0.11 -p tcp --dport 53 -j DNAT --to-destination 127.0.0.11:40002
T iptables --wait -t nat -N DOCKER_POSTROUTING
T iptables --wait -t nat -I POSTROUTING -d 127.0.0.11 -j DOCKER_POSTROUTING
T iptables --wait -t nat -I DOCKER_POSTROUTING -s 127.0.0.11 -p udp --sport 40001 -j SNAT --to-source :53
T iptables --wait -t nat -C OUTPUT -d 127.0.0.11 -j DOCKER_OUTPUT
T iptables --wait -t nat -N DOCKER_OUTPUT
T iptables --wait -t nat -A NOPE -j ACCEPT
T iptables --wait -t nat -X DOCKER_OUTPUT
iptables -t nat -S
nft list ruleset | nocount

echo "== DNAT of 127.0.0.11:53"
serve udp 127.0.0.11 40001 2
python3 /tmp/cli.py udp 127.0.0.11 53 udp-query
python3 /tmp/cli.py udpconn 127.0.0.11 53 udp-connected
serve tcp 127.0.0.11 40002 1
python3 /tmp/cli.py tcp 127.0.0.11 53 tcp-query
served
echo "== after deleting the DNAT rules"
iptables -t nat -D DOCKER_OUTPUT -d 127.0.0.11 -p tcp --dport 53 -j DNAT --to-destination 127.0.0.11:40002; echo "[$?] -D"
iptables -t nat -F DOCKER_OUTPUT; echo "[$?] -F"
python3 /tmp/cli.py tcp 127.0.0.11 53 tcp-no-dnat
iptables -t nat -S DOCKER_OUTPUT
'

cat > /tmp/nv.nft <<'NFT'
table inet nv {
	set protos {
		type inet_proto
		elements = { tcp, udp }
	}

	chain OUTPUT {
		type nat hook output priority dstnat; policy accept;
		fib daddr type local jump HOSTPORT
	}

	chain HOSTPORT {
		meta l4proto { tcp, udp } th dport 8080 jump DNAT8080
		meta l4proto @protos th dport 8081 redirect to :9001
		tcp dport 8082 counter accept
	}

	chain DNAT8080 {
		ip daddr 10.70.0.1 meta l4proto tcp dnat ip to 10.70.0.1:9000
		ip daddr 10.70.0.1 meta l4proto udp dnat ip to 10.70.0.1:9002
	}
}
NFT

unshare -n sh -c '
. /tmp/fns.sh
ip link set lo up
ip link add dummy0 type dummy
ip addr add 10.70.0.1/24 dev dummy0
ip link set dummy0 up
echo "== nft -f (netavark style)"
nft -f /tmp/nv.nft; echo "load: $?"
nft -a list ruleset | nocount
echo "== port forwarding"
serve tcp 10.70.0.1 9000 1
python3 /tmp/cli.py tcp 10.70.0.1 8080 tcp-8080
serve udp 10.70.0.1 9002 1
python3 /tmp/cli.py udp 10.70.0.1 8080 udp-8080
serve tcp 127.0.0.1 9001 1
python3 /tmp/cli.py tcp 127.0.0.1 8081 redirect-8081
served
# The server exits as soon as it has replied, and the client reads the reply only afterwards: the
# source is still 8080
rm -f /tmp/exited
serve udp 10.70.0.1 9002 1
python3 /tmp/cli.py udp 10.70.0.1 8080 udp-8080-late /tmp/exited &
late=$!
served; touch /tmp/exited; wait $late
python3 /tmp/cli.py tcp 10.70.0.1 8082 accepted-8082
python3 /tmp/cli.py tcp 10.1.2.3 8080 not-local
echo "== errors"
nft add rule inet nv HOSTPORT jump NOPE; echo "[$?]"
nft add rule inet nv DNAT8080 jump HOSTPORT; echo "[$?]"
nft add rule inet nv HOSTPORT jump OUTPUT; echo "[$?]"
nft delete chain inet nv DNAT8080; echo "[$?]"
nft add chain inet nv OUTPUT { type filter hook input priority 0 \; }; echo "[$?]"
nft delete set inet nv protos; echo "[$?]"
nft add table inet nv; echo "[$?] add an existing table"
nft create table inet nv; echo "[$?] create an existing table"
echo "== delete a rule, flush, delete"
h=$(nft -a list chain inet nv HOSTPORT | sed -n "s/.*tcp dport 8082 .*# handle //p")
nft delete rule inet nv HOSTPORT handle "$h"; echo "[$?] delete rule $h"
nft flush chain inet nv DNAT8080; echo "[$?] flush"
python3 /tmp/cli.py tcp 10.70.0.1 8080 after-flush
nft -a list table inet nv | nocount
nft delete table inet nv; echo "[$?] delete table"
nft list ruleset; echo "[$?] empty"
nft -f /tmp/nv.nft; echo "reload: $?"
nft -a list chain inet nv OUTPUT | nocount
echo "== flush ruleset"
nft add table ip t4; nft add chain ip t4 c; nft add table ip6 t6; nft add table ip t4b
nft flush ruleset ip; echo "[$?] flush ruleset ip"
nft list tables
nft flush ruleset; echo "[$?] flush ruleset"
nft list ruleset; echo "[$?] empty"
nft flush ruleset; echo "[$?] flush an empty ruleset"
printf "flush ruleset\ntable ip conf {\n  chain c {\n    type filter hook output priority 0; policy accept;\n  }\n}\n" > /tmp/conf.nft
nft -f /tmp/conf.nft; echo "[$?] load a file starting with flush ruleset"
nft -f /tmp/conf.nft; echo "[$?] load it again"
nft list tables
nft flush ruleset
echo "== without CAP_NET_ADMIN"
python3 -c "import os; os.setgid(65534); os.setuid(65534); os.execvp(\"nft\", [\"nft\", \"list\", \"ruleset\"])"; echo "[$?]"
python3 -c "import os; os.setgid(65534); os.setuid(65534); os.execvp(\"iptables\", [\"iptables\", \"-t\", \"nat\", \"-S\"])"; echo "[$?]"
'
