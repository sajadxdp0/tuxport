# docker with containerd's default overlayfs snapshotter on Tuxport's overlayfs: no storage-driver
# in daemon.json, otherwise as docker.sh (no bridge, no iptables, host networking). dockerd runs
# with a data root, exec root and socket of its own, so docker.sh's images stay:
#   scripts/survey -s target/survey/docker docker-overlay
# Native comparison: scripts/survey -n <an alpine image with docker installed> docker-overlay,
# SURVEY_DOCKER_OPTS=--privileged (its data root is on a tmpfs there: docker's own overlay root
# cannot be an upper layer)
exec 2>&1
[ -x /usr/bin/dockerd ] || apk add -q docker
D=/var/lib/docker-overlay
if grep -q ' / / .* - overlay ' /proc/self/mountinfo; then
    mkdir -p $D && mount -t tmpfs tmpfs $D
fi
mkdir -p /etc/docker
echo '{"iptables":false,"ip6tables":false,"bridge":"none"}' > /etc/docker/daemon-overlay.json
step() { echo; echo "== $*"; }
S=unix:///run/docker-overlay.sock
HTTP_PROXY=${http_proxy:-} HTTPS_PROXY=${https_proxy:-} NO_PROXY=127.0.0.1,localhost \
    dockerd --config-file /etc/docker/daemon-overlay.json --data-root $D \
    --exec-root /run/docker-overlay -H $S -p /run/docker-overlay.pid >/tmp/dockerd-overlay.log 2>&1 &
d() { docker -H $S "$@"; }
for i in $(seq 60); do d info >/dev/null 2>&1 && break; sleep 1; done
step version; d version --format '{{.Client.Version}} {{.Server.Version}}'
step info; d info --format '{{.Driver}} {{.DriverStatus}}'
d image inspect alpine:3.24 >/dev/null 2>&1 || d pull -q alpine:3.24 >/dev/null
step root filesystem; d run --rm --network host alpine:3.24 sh -c 'grep " / / " /proc/self/mountinfo | sed "s/.* - //; s/lowerdir=[^,]*/lowerdir=…/; s/upperdir=[^,]*/upperdir=…/; s/workdir=[^,]*/workdir=…/"'
step changes over the image
d run --rm --network host alpine:3.24 sh -c '
    echo more >> /etc/motd && tail -1 /etc/motd
    rm /etc/issue && ls /etc/issue 2>&1
    rm -r /etc/apk/keys && mkdir /etc/apk/keys && ls -a /etc/apk/keys
    mv /etc/apk /etc/apk2 2>&1; echo "mv of an image directory: $?"
'
step diff and commit
d run --name c1 --network host alpine:3.24 sh -c 'echo x > /made; rm /etc/motd' >/dev/null
d diff c1 | sort
d commit c1 ddemo-ovl >/dev/null && d run --rm --network host ddemo-ovl sh -c 'cat /made; ls /etc/motd 2>&1'
d rm c1 >/dev/null
step build; mkdir -p /tmp/ctx-ovl
printf 'FROM alpine:3.24\nRUN echo one > /one && mkdir /d && echo f > /d/f\nRUN rm /one /d/f && rm /etc/motd\nCMD ls -a /d; ls /one /etc/motd 2>&1\n' > /tmp/ctx-ovl/Dockerfile
d build -q --network host -t dbuild-ovl /tmp/ctx-ovl >/dev/null && d run --rm --network host dbuild-ovl
rm -rf /tmp/ctx-ovl
step start time
for i in 1 2 3; do
    s=$(cut -d" " -f1 /proc/uptime); d run --rm --network host alpine:3.24 true; e=$(cut -d" " -f1 /proc/uptime)
    echo "run: $(awk -v s=$s -v e=$e 'BEGIN { printf "%.2f", e - s }') s"
done
d rmi -f ddemo-ovl dbuild-ovl >/dev/null 2>&1
step dockerd errors; grep -E 'level=(error|fatal)' /tmp/dockerd-overlay.log | sed 's/^time="[^"]*" //; s/ id=[0-9a-f]*//' | cut -c1-300 | sort -u | head -10
kill $(cat /run/docker-overlay.pid); sleep 3
step left mounted; grep ' - overlay ' /proc/self/mountinfo | grep -vc ' / / ' || true
