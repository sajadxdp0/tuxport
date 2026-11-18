# podman's bridge networks (docs/rounds/169-container-network-survey.md): under netavark's
# firewall_driver = "none" and then "nftables" (podman passes the host's proxy environment variables
# into containers by default, so wget in containers always gets -Y off), looks at containers'
# addresses and routes in the default network and in a custom network, reaching the outside,
# containers reaching each other by IP, -p port publishing, resolving by container name
# (aardvark-dns), and the netlink, sysctl and nftables rules netavark uses. The store is shared with
# podman.sh:
#   scripts/survey -s target/survey/podman podman-bridge
# Native comparison:
# SURVEY_DOCKER_OPTS=--privileged scripts/survey -n tuxport-podman-native podman-bridge
exec 2>&1
command -v strace >/dev/null || apk add -q strace
command -v nft >/dev/null || apk add -q nftables
command -v iptables >/dev/null || apk add -q iptables
step() { echo; echo "== $*"; }
tidy() { sed -E 's/[0-9a-f]{12,64}/ID/g; s/veth[0-9a-f]+/vethX/g; s/podman[0-9]+/podmanN/g'; }
# The server is a loop of nc -l (Alpine's busybox has no httpd)
serve() { echo "while true; do printf \"HTTP/1.0 200 OK\\r\\n\\r\\n$1\\n\" | nc -l -p 80; done"; }
# Trace the child processes podman starts, such as netavark, too, into /tmp/pm.st. Containers run in
# the background (run -d) are not traced: strace -f follows the processes in the container and
# podman never returns; netavark's calls show up in the foreground run and network create
pm() { strace -f -qq -a 1 -o /tmp/pm.one -e trace=execve,socket,sendto,sendmsg,openat -s 300 podman "$@"; r=$?; cat /tmp/pm.one >> /tmp/pm.st; return $r; }

config() {
    mkdir -p /etc/containers
    # Only the network matters: neither side uses cgroups (natively, controllers cannot be enabled
    # when nested)
    printf '[containers]\ncgroups = "disabled"\n[network]\n%s\n' "$1" > /etc/containers/containers.conf
    sed -i 's/^driver = .*/driver = "vfs"/' /etc/containers/storage.conf 2>/dev/null
    : > /tmp/pm.st
    podman rm -af >/dev/null 2>&1
    podman network rm -f pnet >/dev/null 2>&1
    podman image exists alpine:3.24 || podman pull -q alpine:3.24 >/dev/null
}

probe() {
    gw=$(ip route | awk '/^default/ { print $3; exit }')
    step "$1: inside a container on the default network"
    pm run --rm -e GW="$gw" alpine:3.24 sh -c '
        ip addr show eth0 | grep -E "inet |mtu" | sed -E "s/^ +//; s/ brd.*//; s/ qdisc.*//"
        ip route; grep -v "^#" /etc/resolv.conf | grep .
        ping -c 1 -W 2 "$GW" >/dev/null 2>&1 && echo "ping the outer gateway: ok" || echo "ping the outer gateway: failed"' | tidy
    step "$1: bridge in the outer namespace"
    ip addr show podman0 2>&1 | grep -E 'inet |state' | sed -E 's/^ +//; s/ +(valid|preferred).*//' | tidy
    step "$1: container to container by IP"
    podman run -d --name srv alpine:3.24 sh -c "$(serve hello)" >/dev/null
    sleep 1
    ip=$(podman inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' srv)
    echo "srv at $ip" | sed -E 's/[0-9]+$/N/'
    pm run --rm alpine:3.24 wget -Y off -q -T 5 -O - "http://$ip/" 2>&1 | head -1
    step "$1: from the outer namespace to the container IP"
    wget -Y off -q -T 5 -O - "http://$ip/" 2>&1 | head -1
    podman rm -f srv >/dev/null
    step "$1: -p 8080:80"
    podman run -d --name pub -p 8080:80 alpine:3.24 sh -c "$(serve published)" >/dev/null
    sleep 1
    wget -Y off -q -T 5 -O - http://127.0.0.1:8080/ 2>&1 | head -1 | sed 's/^/via 127.0.0.1: /'
    hostip=$(ip route get 1.1.1.1 2>/dev/null | grep -oE 'src [0-9.]+' | cut -d' ' -f2)
    wget -Y off -q -T 5 -O - "http://$hostip:8080/" 2>&1 | head -1 | sed 's/^/via the outer address: /'
    echo "listeners on 8080: $(netstat -ltn 2>/dev/null | grep -c ':8080 ')"
    podman rm -f pub >/dev/null
    step "$1: user-defined network, by name"
    pm network create pnet >/dev/null
    podman run -d --name named --network pnet alpine:3.24 sh -c "$(serve by-name)" >/dev/null
    sleep 1
    pm run --rm --network pnet alpine:3.24 sh -c '
        grep -v "^#" /etc/resolv.conf | grep .
        nslookup named 2>&1 | grep -E "^Address|can.t" | tail -1 | sed -E "s/[0-9]+$/N/"
        wget -Y off -q -T 5 -O - http://named/ 2>&1 | head -1' | tidy
    echo "aardvark-dns processes: $(pidof aardvark-dns | wc -w)"
    netstat -lun 2>/dev/null | grep ':53 ' | sed -E 's/ +/ /g' | tidy
    pid=$(podman inspect -f '{{.State.Pid}}' named)
    step "$1: rules inside the container namespace"
    nsenter -t "$pid" -n nft list ruleset 2>&1 | grep -E 'table|dnat|snat' | head -6
    step "$1: chains in the outer namespace"
    nft list ruleset 2>&1 | grep -E '^table|chain' | tr -d '{' | tidy | head -24
    podman rm -f named >/dev/null
    podman network rm pnet >/dev/null
}

report() {
    step "$1: netlink messages"
    grep -oE 'nlmsg_type=RTM_[A-Z]+' /tmp/pm.st | sort | uniq -c | sort -rn | head -20
    step "$1: link kinds created"
    grep 'RTM_NEWLINK' /tmp/pm.st | grep -oE '"(veth|bridge|vxlan|macvlan|ipvlan|dummy)"' | sort | uniq -c
    step "$1: other netlink"
    grep -oE 'socket\(AF_NETLINK, [A-Z_|]+, NETLINK_[A-Z_]+' /tmp/pm.st | sort | uniq -c
    step "$1: sysctls written"
    grep -oE 'openat\([^)]*"/proc/sys/net/[^"]+", O_WRONLY' /tmp/pm.st | grep -oE '/proc/sys/[^"]+' |
        sed -E 's/veth[0-9a-f]+/vethX/; s/podman[0-9]+/podmanN/' | sort | uniq -c
    step "$1: programs executed"
    grep -oE 'execve\("[^"]+"' /tmp/pm.st | sort | uniq -c | sort -rn | head -20
}

config 'firewall_driver = "none"'
probe "firewall none"
report "firewall none"

# The default driver is iptables (when iptables is installed); try nftables too
config 'firewall_driver = "nftables"'
probe "firewall nftables"
report "firewall nftables"

config 'firewall_driver = "iptables"'
probe "firewall iptables"
report "firewall iptables"
podman rm -af >/dev/null 2>&1
