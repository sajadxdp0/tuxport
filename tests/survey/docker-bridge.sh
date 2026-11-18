# docker's bridge networks (docs/rounds/169-container-network-survey.md): starts dockerd with
# "iptables": false and then with the default (iptables on), and looks at containers' addresses and
# routes in the default bridge network and in a custom network, reaching the internet, containers
# reaching each other by IP, -p port publishing, resolving by container name (the custom network's
# built-in DNS), and the netlink, sysctl and iptables/nftables rules and docker-proxy that dockerd
# uses. The store is shared with docker.sh:
#   scripts/survey -s target/survey/docker docker-bridge
# Native comparison:
# SURVEY_DOCKER_OPTS=--privileged scripts/survey -n tuxport-docker-native docker-bridge
exec 2>&1
[ -x /usr/bin/dockerd ] || apk add -q docker
command -v strace >/dev/null || apk add -q strace
command -v nft >/dev/null || apk add -q nftables
step() { echo; echo "== $*"; }
# The server is a loop of nc -l (Alpine's busybox has no httpd). Container IDs, pids, random ports
# and the like are replaced by fixed spellings
tidy() { sed -E 's/[0-9a-f]{12,64}/ID/g; s/veth[0-9a-f]+/vethX/g'; }

start() {
    mkdir -p /etc/docker
    echo "$1" > /etc/docker/daemon.json
    HTTP_PROXY=${http_proxy:-} HTTPS_PROXY=${https_proxy:-} NO_PROXY=127.0.0.1,localhost \
        strace -f -qq -o /tmp/dockerd.st -e trace=execve,socket,sendto,sendmsg,openat -s 300 \
        dockerd >/tmp/dockerd.log 2>&1 &
    for i in $(seq 60); do docker info >/dev/null 2>&1 && break; sleep 1; done
    docker image inspect alpine:3.24 >/dev/null 2>&1 || docker pull -q alpine:3.24 >/dev/null
}

stop() {
    docker rm -f $(docker ps -aq) >/dev/null 2>&1
    docker network rm mynet >/dev/null 2>&1
    kill $(pidof dockerd) 2>/dev/null
    for i in $(seq 20); do pidof dockerd >/dev/null || break; sleep 1; done
    ip link del docker0 2>/dev/null
}

# What dockerd uses: netlink message types and kinds of network devices, the sysctls written, the
# programs executed
report() {
    step "$1: netlink messages"
    grep -oE 'nlmsg_type=RTM_[A-Z]+' /tmp/dockerd.st | sort | uniq -c | sort -rn | head -20
    step "$1: link kinds created"
    grep 'RTM_NEWLINK' /tmp/dockerd.st | grep -oE '"(veth|bridge|vxlan|macvlan|ipvlan|dummy)"' | sort | uniq -c
    step "$1: iptables commands (table, verb, chain, target)"
    grep -oE 'execve\("/usr/sbin/ip6?tables", \[[^]]*\]' /tmp/dockerd.st |
        awk -F'", "' '{ t = "filter"; v = ""; c = ""; j = ""
            for (i = 1; i <= NF; i++) {
                if ($i == "-t") t = $(i + 1)
                if ($i ~ /^-[ACDINIX]$/) { v = $i; c = $(i + 1) }
                if ($i == "-j") j = $(i + 1)
            }
            sub(/".*/, "", j); print (index($1, "ip6tables") ? "ip6" : "ip4"), t, v, c, j }' |
        sort | uniq -c | sort -rn | head -20
    step "$1: other netlink"
    grep -oE 'socket\(AF_NETLINK, [A-Z_|]+, NETLINK_[A-Z_]+' /tmp/dockerd.st | sort | uniq -c
    step "$1: sysctls written"
    grep -oE 'openat\([^)]*"/proc/sys/net/[^"]+", O_WRONLY' /tmp/dockerd.st | grep -oE '/proc/sys/[^"]+' | sed -E 's/veth[0-9a-f]+/vethX/' | sort | uniq -c
    step "$1: programs executed"
    grep -oE 'execve\("[^"]+"' /tmp/dockerd.st | sort | uniq -c
    step "$1: dockerd errors"
    grep -E 'level=(error|fatal)' /tmp/dockerd.log | sed 's/^time="[^"]*" //' | cut -c1-250 | sort -u | head -10
}

probe() {
    gw=$(ip route | awk '/^default/ { print $3; exit }')
    step "$1: docker0 and routes"
    ip addr show docker0 | grep -E 'inet |state' | sed -E 's/^ +//; s/ +(valid|preferred).*//' | tidy
    ip route | grep -E 'docker0' | tidy
    step "$1: default bridge, inside a container"
    docker run --rm -e GW="$gw" alpine:3.24 sh -c '
        ip addr show eth0 | grep -E "inet |mtu" | sed -E "s/^ +//; s/ brd.*//; s/ qdisc.*//"
        ip route; grep -v "^#" /etc/resolv.conf | grep .
        ping -c 1 -W 2 "$GW" >/dev/null 2>&1 && echo "ping the outer gateway: ok" || echo "ping the outer gateway: failed"' | tidy
    step "$1: container to container by IP"
    docker run -d --name srv alpine:3.24 sh -c 'while true; do printf "HTTP/1.0 200 OK\\r\\n\\r\\nhello\\n" | nc -l -p 80; done' >/dev/null
    sleep 1
    ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' srv)
    echo "srv at $ip" | sed -E 's/[0-9]+$/N/'
    docker run --rm alpine:3.24 wget -q -T 5 -O - "http://$ip/" 2>&1 | head -1
    step "$1: from the outer namespace to the container IP"
    wget -Y off -q -T 5 -O - "http://$ip/" 2>&1 | head -1
    docker rm -f srv >/dev/null
    step "$1: -p 8080:80"
    docker run -d --name pub -p 8080:80 alpine:3.24 sh -c 'while true; do printf "HTTP/1.0 200 OK\\r\\n\\r\\npublished\\n" | nc -l -p 80; done' >/dev/null
    sleep 1
    wget -Y off -q -T 5 -O - http://127.0.0.1:8080/ 2>&1 | head -1
    hostip=$(ip route get 1.1.1.1 2>/dev/null | grep -oE 'src [0-9.]+' | cut -d' ' -f2)
    wget -Y off -q -T 5 -O - "http://$hostip:8080/" 2>&1 | head -1 | sed 's/^/via the outer address: /'
    ps -o args | grep '[d]ocker-proxy' | tidy | sed -E 's/[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+/IP/g' | head -2
    docker rm -f pub >/dev/null
    step "$1: user-defined network, by name"
    docker network create mynet >/dev/null
    docker run -d --name named --network mynet alpine:3.24 sh -c 'while true; do printf "HTTP/1.0 200 OK\\r\\n\\r\\nby-name\\n" | nc -l -p 80; done' >/dev/null
    sleep 1
    docker run --rm --network mynet alpine:3.24 sh -c '
        grep -v "^#" /etc/resolv.conf | grep .
        nslookup named 2>&1 | grep -E "^Address|can.t" | tail -1 | sed -E "s/[0-9]+$/N/"
        wget -q -T 5 -O - http://named/ 2>&1 | head -1' | tidy
    pid=$(docker inspect -f '{{.State.Pid}}' named)
    step "$1: nat rules inside the container namespace"
    nsenter -t "$pid" -n nft list ruleset 2>&1 | grep -E 'table|DNAT|SNAT|127\.0\.0\.11' | sed -E 's/(port|sport|dport) [0-9]+/\1 N/g; s/packets [0-9]+ bytes [0-9]+/C/' | tidy | head -12
    step "$1: chains in the outer namespace"
    nft list ruleset 2>&1 | grep -E '^table|chain' | tr -d '{' | tidy | head -24
    step "$1: --sysctl"
    docker run --rm --sysctl net.ipv4.tcp_keepalive_time=600 --sysctl net.core.somaxconn=1024 alpine:3.24 \
        sh -c 'cat /proc/sys/net/ipv4/tcp_keepalive_time /proc/sys/net/core/somaxconn' 2>&1 | head -3
    step "$1: isolation between networks"
    docker network create other >/dev/null
    docker network create --internal inner >/dev/null
    ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' named)
    docker run --rm --network other alpine:3.24 wget -q -T 3 -O - "http://$ip/" 2>&1 | head -1 | sed 's/^/from another network: /; s/([0-9.]*)/(IP)/'
    docker run --rm --network inner alpine:3.24 sh -c "echo default routes: \$(ip route | grep -c default); wget -q -T 3 -O - http://$ip/ 2>&1 | head -1" | sed 's/^/from an internal network: /; s/([0-9.]*)/(IP)/'
    docker network rm other inner >/dev/null
    docker rm -f named >/dev/null
    docker network rm mynet >/dev/null
}

start '{"storage-driver":"vfs","iptables":false,"ip6tables":false}'
probe "iptables off"
report "iptables off"
stop

start '{"storage-driver":"vfs"}'
probe "iptables on"
report "iptables on"
stop
