# docker run by an ordinary user (rootless: dockerd in RootlessKit) with containerd's
# overlayfs snapshotter on Tuxport's overlayfs, which containerd mounts with `userxattr` in the
# user's namespaces and extracts layers through. RootlessKit's host network (slirp4netns and
# pasta need a TUN device Tuxport lacks, and dockerd-rootless itself refuses the host network), no
# bridge, no iptables. Uses the docker store:
#   scripts/survey -s target/survey/docker docker-rootless-overlay
# Native comparison: SURVEY_DOCKER_OPTS=--privileged scripts/survey -n tuxport-docker-native
# docker-rootless-overlay (there the data root is on a tmpfs: docker's overlay root cannot be an
# upper layer)
exec 2>&1
for p in docker docker-rootless-extras rootlesskit shadow-uidmap; do
    apk info -e $p >/dev/null || apk add -q $p
done
id rd >/dev/null 2>&1 || adduser -D -u 1001 rd
grep -q '^rd:' /etc/subuid || echo rd:200000:65536 >>/etc/subuid
grep -q '^rd:' /etc/subgid || echo rd:200000:65536 >>/etc/subgid
mkdir -p /run/user/1001 && chown rd:rd /run/user/1001 && chmod 700 /run/user/1001
D=/home/rd/.local/share/docker
mkdir -p $D
if grep -q ' / / .* - overlay ' /proc/self/mountinfo; then
    mountpoint -q $D || mount -t tmpfs -o mode=700,uid=1001,gid=1001 tmpfs $D
fi
chown -R rd:rd /home/rd/.local
# As the user, with its runtime directory
as() { su rd -s /bin/sh -c "cd /home/rd; export XDG_RUNTIME_DIR=/run/user/1001 HOME=/home/rd DOCKER_HOST=unix:///run/user/1001/docker.sock; $1"; }
step() { echo; echo "== $1"; as "$2"; echo "exit=$?"; }
# dockerd-rootless refuses RootlessKit's host network, and its child half enters RootlessKit's
# network namespace, which the host network has none of: RootlessKit is started as it would start
# it, and runs dockerd itself (rootless because ROOTLESSKIT_STATE_DIR is set). Containers use no
# network: with the host's, dockerd would have to enter the initial network namespace, which a
# user namespace's root may not (EPERM natively too), so rootless docker needs RootlessKit's own
# network namespace and with it TUN
as 'rootlesskit --state-dir=/run/user/1001/dockerd-rootless --net=host --port-driver=none \
    --copy-up=/etc --copy-up=/run --propagation=rslave \
    dockerd --iptables=false --ip6tables=false --bridge=none >/tmp/dockerd-rootless.log 2>&1 &'
for i in $(seq 90); do as 'docker info >/dev/null 2>&1' && break; sleep 1; done

step info "docker info --format '{{.Driver}} {{.DriverStatus}} {{.SecurityOptions}}'"
step pull "docker image inspect alpine:3.24 >/dev/null 2>&1 || docker pull -q alpine:3.24 >/dev/null; echo pulled"
step "root filesystem" "docker run --rm --network none alpine:3.24 sh -c 'id; grep \" / / \" /proc/self/mountinfo | sed \"s/.* - //; s/lowerdir=[^,]*/lowerdir=…/; s/upperdir=[^,]*/upperdir=…/; s/workdir=[^,]*/workdir=…/\"'"
step "changes over the image" "docker run --rm --network none alpine:3.24 sh -c 'echo more >> /etc/motd && tail -1 /etc/motd; rm /etc/issue && ls /etc/issue 2>&1; rm -r /etc/apk/keys && mkdir /etc/apk/keys && ls -a /etc/apk/keys; mv /etc/apk /etc/apk2; echo \"mv of an image directory: \$?\"'"
step "diff and commit" "docker run --name c1 --network none alpine:3.24 sh -c 'echo x > /made; rm /etc/motd' >/dev/null; docker diff c1 | sort; docker commit c1 rddemo-ovl >/dev/null && docker run --rm --network none rddemo-ovl sh -c 'cat /made; ls /etc/motd 2>&1'; docker rm c1 >/dev/null"
step build "mkdir -p ctx-ovl && printf 'FROM alpine:3.24\nRUN echo one > /one && mkdir /d && echo f > /d/f\nRUN rm /one /d/f && rm /etc/motd\nCMD ls -a /d; ls /one /etc/motd 2>&1; true\n' > ctx-ovl/Dockerfile && docker build -q --network none -t rdbuild-ovl ctx-ovl >/dev/null && docker run --rm --network none rdbuild-ovl"
step "start time" "for i in 1 2 3; do s=\$(cut -d' ' -f1 /proc/uptime); docker run --rm --network none alpine:3.24 true; e=\$(cut -d' ' -f1 /proc/uptime); echo \"run: \$(awk -v s=\$s -v e=\$e 'BEGIN { printf \"%.2f\", e - s }') s\"; done"
step cleanup "docker rmi -f rddemo-ovl rdbuild-ovl >/dev/null 2>&1; docker system prune -af >/dev/null 2>&1; echo ok"
echo; echo "== dockerd errors"
grep -E 'level=(error|fatal)|\[rootlesskit' /tmp/dockerd-rootless.log | sed 's/^time="[^"]*" //; s/ id=[0-9a-f]*//' | cut -c1-200 | sort -u | head -12
pkill rootlesskit; sleep 3
