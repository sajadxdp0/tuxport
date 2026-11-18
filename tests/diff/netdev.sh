# docker-run-options: --privileged
# Virtual network devices (docs/rounds/170-netdev.md): creating, deleting and changing bridge,
# veth and dummy devices, addresses, routes, carrier and state, interface sysctls, a veth with one
# end in another network namespace, and a bridge stacked on the host interfaces in the boot-time
# namespace. The native side is a privileged container (it has to write /proc/sys).
#
# - MACs and the link-local addresses generated from them are random, and are replaced with MAC
#   and LL. DAD is turned off first in new namespaces (default/accept_dad), so native IPv6
#   addresses are never tentative for a while.
# - Error codes are checked with busybox's ip (it does not ask for extack messages); of the
#   extack messages iproute2 prints, only those that both 6.1 and 6.8 have are compared.
# - Natively a bridge learns that a port's carrier changed only through linkwatch (up to 1 second
#   late), so after bringing a port up wait a moment before looking.
# - Differences between 6.1 and 6.8 not compared here: renaming an interface that is up (EBUSY on
#   6.1, allowed from 6.2 on); the operstate after a veth is brought down while its peer is in
#   another namespace (6.8 looks at the peer's carrier). Moving lo into the namespace it is in:
#   EINVAL on 6.1 and 6.8, while newer kernels (CI's) do nothing, so it is moved to another
#   namespace (EINVAL everywhere).
# - Native IPv4 sysctls in a new namespace copy the host's (Ubuntu has ip_forward and rp_filter
#   on), so only the entries that are at their defaults on both sides are compared.
exec 2>&1
norm() {
    sed -E 's/([0-9a-f]{2}:){5}[0-9a-f]{2}/MAC/g; s/fe80::[0-9a-f:]+/fe80::LL/g; s/8000\.[0-9A-F:]{17}/8000.MAC/g'
}

unshare -n sh -c '
norm() { sed -E "s/([0-9a-f]{2}:){5}[0-9a-f]{2}/MAC/g; s/fe80::[0-9a-f:]+/fe80::LL/g"; }
# Local routes of link-local addresses sort by the (random) address: move them to the end, sorted
llsort() { norm | awk "/^local fe80::LL/ { print > \"/tmp/ll\"; next } { print }"; sort /tmp/ll 2>/dev/null; rm -f /tmp/ll; }
echo 0 > /proc/sys/net/ipv6/conf/default/accept_dad

echo "== create"
ip link add br0 type bridge
ip link add v0 type veth peer name v1
ip link add d0 type dummy
ip link add type veth
ip link show | norm
for d in br0 v0 d0; do echo "$d kind: $(ip -d -o link show $d | grep -oE " (bridge|veth|dummy) " | head -1)"; done
cat /proc/net/dev | cut -d: -f1 | tail -n +3 | tr -d " " | sort | tr "\n" " "; echo

echo "== master"
ip link set v0 master br0
ip link show v0 | norm
ip -d -o link show v0 | grep -oE "bridge_slave state [a-z]+"
ip -o link show master br0 | cut -d: -f2

echo "== up"
ip link set br0 up; ip link show br0 | norm
ip link set v0 up; ip link show v0 | norm; ip link show br0 | norm
ip link set v1 up; sleep 1.5; ip link show v0 | norm; ip link show v1 | norm; ip link show br0 | norm
ip -d -o link show v0 | grep -oE "bridge_slave state [a-z]+"
ip link set d0 up; ip link show d0 | norm
ip link set dev v1 down; ip link show v0 | norm; ip link show v1 | norm; ip link set dev v1 up

echo "== addresses"
ip addr add 10.1.0.1/24 dev br0
ip addr add 10.1.0.2/24 dev br0
busybox ip addr add 10.1.0.1/24 dev br0
ip addr add 10.2.0.1/16 brd + dev br0
ip -6 addr add fd00::1/64 dev br0 nodad
ip -6 addr add fd00::3/64 dev br0 nodad
ip addr show br0 | norm
ip -6 addr add fd00::2/64 dev v1 nodad
ip addr show v1 | norm
busybox ip addr show v0 | norm
ip -4 -o addr | cut -d" " -f2-4

echo "== routes"
ip route show table all | llsort
ip -6 route show table all | llsort

echo "== route add, get, del"
ip route add default via 10.1.0.254
ip route add 10.9.0.0/16 via 10.1.0.254 metric 50
ip route add 10.8.0.0/16 via 192.168.99.1
busybox ip route add 10.8.0.0/16 via 192.168.99.1
ip route add 10.7.0.0/16 dev v0
ip link set v0 down; ip route add 10.7.0.0/16 dev v0; ip link set v0 up
busybox ip route add default via 10.1.0.254
ip route add 10.6.0.0/16 dev d0 proto static metric 7
ip route add 10.5.1.1/16 dev d0
ip route append 10.6.0.0/16 dev br0 proto static metric 7
ip route add 10.4.0.0/16 dev br0 src 10.1.0.2
ip route add 10.3.0.0/16 dev br0 src 10.99.0.1
ip -6 route add fd01::/64 via fd00::9 dev br0
ip -6 route add fd02::/64 dev v1 metric 5
ip route
ip -6 route | norm
ip route get 10.9.1.1
ip route get 10.1.0.77
ip route get 10.4.0.1
ip route get 10.1.0.2
ip -6 route get fd01::5 | norm
busybox ip route get 192.0.2.1
ip route del 10.9.0.0/16
busybox ip route del 10.9.0.0/16
ip route del 10.6.0.0/16 dev br0
ip -6 route del fd02::/64
ip route
echo "-- d0 down"
ip link set d0 down; ip route; ip -4 route show table local | grep d0
ip link set d0 up; ip route

echo "== rename, mtu, mac"
ip link set dev v1 down; ip link set v1 name eth5; ip link show eth5 | norm
ip addr show eth5 | norm
busybox ip link set eth5 name br0
ip link set eth5 mtu 60
ip link set eth5 mtu 65536
ip link set eth5 address 01:00:00:00:00:01
ip link set eth5 address 02:00:00:00:00:01; ip link show eth5
ip link set eth5 up
ip link set v0 mtu 1400; ip link show br0 | norm
ip link set v0 mtu 1500; ip link show br0 | norm
ip link set br0 mtu 1400; ip link set v0 mtu 1300; ip link show br0 | norm

echo "== errors"
busybox ip link add br0 type bridge
ip link add x0 type nosuchkind
busybox ip link set lo master br0
ip link set br0 master br0
ip link add br1 type bridge; ip link set br1 master br0; ip link del br1
busybox ip link del lo
ip link del nosuch
busybox ip link set br0 netns 1
busybox ip link set lo netns 1
busybox ip addr del 10.1.0.99/24 dev br0
ip link add "$(printf "a%.0s" $(seq 16))" type dummy

echo "== sysctl"
ls /proc/sys/net/ipv4/conf | tr "\n" " "; echo
ls /proc/sys/net/ipv6/conf | tr "\n" " "; echo
for f in arp_notify route_localnet arp_ignore accept_local promote_secondaries; do echo "$f $(cat /proc/sys/net/ipv4/conf/br0/$f)"; done
for f in disable_ipv6 accept_ra autoconf forwarding mtu addr_gen_mode keep_addr_on_down use_tempaddr hop_limit; do echo "$f $(cat /proc/sys/net/ipv6/conf/br0/$f)"; done
cat /proc/sys/net/ipv6/conf/br0/stable_secret
echo 1 > /proc/sys/net/ipv4/conf/br0/route_localnet; cat /proc/sys/net/ipv4/conf/br0/route_localnet
echo x > /proc/sys/net/ipv4/conf/br0/route_localnet
echo 1 > /proc/sys/net/ipv4/ip_forward; cat /proc/sys/net/ipv4/ip_forward /proc/sys/net/ipv4/conf/br0/forwarding /proc/sys/net/ipv4/conf/default/forwarding
echo 1 > /proc/sys/net/ipv6/conf/br0/disable_ipv6; ip -6 addr show br0; ip -6 route show dev br0
echo 0 > /proc/sys/net/ipv6/conf/br0/disable_ipv6; ip -6 addr show br0 | norm
echo 1 > /proc/sys/net/ipv6/conf/d0/disable_ipv6; busybox ip -6 addr add fd09::1/64 dev d0
echo 9000 > /proc/sys/net/ipv6/conf/br0/mtu
echo 1280 > /proc/sys/net/ipv6/conf/br0/mtu; cat /proc/sys/net/ipv6/conf/br0/mtu
echo 4 > /proc/sys/net/ipv6/conf/br0/addr_gen_mode
cat /proc/sys/net/core/somaxconn
cat /proc/sys/net/ipv4/ping_group_range /proc/sys/net/ipv4/ip_unprivileged_port_start /proc/sys/net/ipv4/ip_local_port_range
echo "1000 60999" > /proc/sys/net/ipv4/ip_local_port_range
echo 40000 > /proc/sys/net/ipv4/ip_unprivileged_port_start
binds() { python3 -c "
import os, socket
os.setuid(65534)
for p in (80, 1500):
    s = socket.socket()
    try:
        s.bind((\"127.0.0.1\", p)); print(p, \"ok\")
    except OSError as e:
        print(p, e.errno)
"; }
ip link set lo up
echo 2000 > /proc/sys/net/ipv4/ip_unprivileged_port_start; binds
echo 0 > /proc/sys/net/ipv4/ip_unprivileged_port_start; binds
echo "1000 60999" > /proc/sys/net/ipv4/ip_local_port_range; cat /proc/sys/net/ipv4/ip_local_port_range
echo "0 2147483647" > /proc/sys/net/ipv4/ping_group_range; cat /proc/sys/net/ipv4/ping_group_range
echo "5 2" > /proc/sys/net/ipv4/ping_group_range; cat /proc/sys/net/ipv4/ping_group_range

echo "== down drops IPv6"
ip link set br0 down; ip addr show br0 | norm; ip route show table all dev br0 | norm
ip link set br0 up; sleep 1.5; ip addr show br0 | norm

echo "== another namespace"
unshare -n sleep 60 &
pid=$!
sleep 0.5
ip link add v2 type veth peer name eth0 netns $pid
ip link show v2 | norm
nsenter -t $pid -n ip link show | norm
nsenter -t $pid -n ip link set eth0 up
ip link set v2 up; ip link show v2 | norm
nsenter -t $pid -n sh -c "ip addr add 10.5.0.2/24 dev eth0; ip route add default via 10.5.0.1; ip route; ip -o link show eth0" | norm
ip link add v3 type veth peer name v4
ip link set v4 netns $pid
nsenter -t $pid -n ip -o link show v4 | norm
ip link add eth0 type dummy
busybox ip link set eth0 netns $pid
ip link set eth0 netns $pid name eth9 2>&1 || true
ip link del eth0
nsenter -t $pid -n ip -o link | cut -d: -f2 | tr "\n" " "; echo
kill $pid; wait $pid 2>/dev/null
for i in $(seq 50); do ip link show v2 >/dev/null 2>&1 || break; sleep 0.1; done
ip -o link | cut -d: -f2 | tr "\n" " "; echo

echo "== delete"
ip link del v0
ip -o link | cut -d: -f2 | tr "\n" " "; echo
ls /proc/sys/net/ipv4/conf | tr "\n" " "; echo
ip route show table all dev br0 | llsort
ip link show br0 | grep -o "link/ether [0-9a-f:]*"
ip link del veth0; ip link show veth1
'

echo "== the initial namespace"
ip link add br9 type bridge
ip addr add 10.77.0.1/24 dev br9
ip link set br9 up
ip link show br9 | norm
ip addr show br9 | grep "inet "
ip route | grep br9
ip route get 10.77.0.5
cat /sys/class/net/br9/operstate /sys/class/net/br9/type
python3 -c 'import socket; print([n for _, n in socket.if_nameindex() if n == "br9"])'
grep -c br9 /proc/net/dev
ls /proc/sys/net/ipv4/conf/ | grep -c br9
ip link add v8 type veth peer name v9
ip link set v8 master br9
cat /sys/class/net/br9/bridge/stp_state /sys/class/net/v8/brport/state
ip link set v8 up; ip link set v9 up; sleep 1.5
ip link show br9 | norm
ls /sys/class/net/br9/brif; readlink /sys/class/net/br9/brif/v8
cat /sys/class/net/br9/brif/v8/state /sys/class/net/br9/bridge/forward_delay /sys/class/net/v9/iflink /sys/class/net/v8/operstate
ls /sys/class/net/v9 | grep -c brport
busybox ip -o link | grep -E "br9|v8|v9" | cut -d: -f2 | tr "\n" " "; echo
ip link del v8
ip link del br9
ip link show br9
ls /sys/class/net | grep -c br9

# Dumps of many interfaces come in datagrams that fit a page: Go's syscall.NetlinkRIB (behind
# net.Interfaces and net.InterfaceAddrs) reads one page per datagram and fails on a truncated one
echo "== many interfaces"
mkdir -p /tmp/diff-netdev-go
cd /tmp/diff-netdev-go
cat > main.go <<'GO'
package main

import (
	"fmt"
	"net"
)

func main() {
	ifs, err := net.Interfaces()
	fmt.Println("interfaces:", len(ifs), err)
	addrs, err := net.InterfaceAddrs()
	fmt.Println("addresses:", len(addrs), err)
}
GO
printf 'module diffnetdev\n\ngo 1.22\n' > go.mod
go build -o ifaces .
unshare -n sh -c '
for i in $(seq 1 16); do
    ip link add d$i type dummy
    ip addr add 10.9.$i.1/24 dev d$i
    ip link set d$i up
done
./ifaces
'
cd /
rm -rf /tmp/diff-netdev-go
