# podman: runs containers inside Tuxport. The vfs storage driver (no overlayfs), host networking (no
# veth, iptables), no cgroups (read-only). The store must be created separately: scripts/survey -s
# target/survey/podman podman
# Native comparison: scripts/survey -n <an alpine:3.24 image with podman installed> podman,
# SURVEY_DOCKER_OPTS=--privileged (docs/rounds/157-container-survey.md)
exec 2>&1
[ -x /usr/bin/podman ] || apk add -q podman
# runc conflicts with crun, and installing runc replaces crun; without cgroups podman does not allow
# runc either
[ -x /usr/bin/crun ] || apk add -q crun
sed -i 's/^driver = .*/driver = "vfs"/' /etc/containers/storage.conf
P="podman --runtime crun --cgroup-manager cgroupfs"
R="--rm --network host --cgroups disabled"
# Containers, volumes and images left by the previous run
podman rm -af >/dev/null 2>&1; podman volume rm -af >/dev/null 2>&1; podman rmi -f demo demo2 >/dev/null 2>&1
rm -f /root/demo.tar
step() { echo; echo "== $*"; }
img() { $P image exists "$1" || $P pull -q "$1" >/dev/null; }

step version; podman --version; crun --version | head -1
img docker.io/library/alpine:3.24
step run; $P run $R alpine:3.24 sh -c 'echo hello; id; cat /etc/alpine-release; [ "$(hostname)" = "$(cat /etc/hostname)" ] && echo hostname-ok'
step exit code; $P run $R alpine:3.24 sh -c 'exit 7'; echo "exit=$?"
step pid 1; $P run $R alpine:3.24 sh -c 'echo $$; ps -o pid,comm'
step mounts; $P run $R alpine:3.24 sh -c 'cat /proc/self/mountinfo | cut -d" " -f5,9 | sort'
step caps; $P run $R alpine:3.24 sh -c 'grep Cap /proc/self/status; grep Seccomp /proc/self/status'
step env and workdir; $P run $R -e FOO=bar -w /tmp alpine:3.24 sh -c 'echo $FOO; pwd'
step volume; mkdir -p /root/vol && echo from-host > /root/vol/a
$P run $R -v /root/vol:/data alpine:3.24 sh -c 'cat /data/a; echo from-container > /data/b'; cat /root/vol/b
step named volume; $P volume create v1 >/dev/null
$P run $R -v v1:/v alpine:3.24 sh -c 'echo x > /v/x'; $P run $R -v v1:/v alpine:3.24 cat /v/x
step tmpfs; $P run $R --tmpfs /scratch alpine:3.24 sh -c 'df -h /scratch | tail -1; touch /scratch/t && echo ok'
step read-only; $P run $R --read-only alpine:3.24 sh -c 'touch /x 2>&1; echo exit=$?'
step user; $P run $R --user 1000:1000 alpine:3.24 id
step detached service
$P run -d --name web --network host --cgroups disabled alpine:3.24 sh -c 'echo started; exec nc -lk -p 2380 -e cat /etc/alpine-release' >/dev/null
sleep 2
$P ps --format '{{.Names}} {{.Status}}' | sed 's/Up .*/Up/'
nc 127.0.0.1 2380 </dev/null
$P exec web sh -c 'echo exec-ok'; $P exec web ps -o pid,comm
$P logs web
$P top web pid comm 2>&1 | head -3
$P stop -t 2 web >/dev/null; $P ps -a --format '{{.Names}} {{.Status}}' | sed 's/Exited (\([0-9]*\)).*/Exited \1/'
$P rm web >/dev/null
step signal; $P run -d --name sig --network host --cgroups disabled alpine:3.24 sh -c 'trap "echo got TERM; exit 3" TERM; while :; do sleep 1; done' >/dev/null
sleep 1; $P kill -s TERM sig >/dev/null; $P wait sig; $P logs sig; $P rm sig >/dev/null
step glibc image; img docker.io/library/debian:trixie-slim
$P run $R debian:trixie-slim sh -c '. /etc/os-release; echo $PRETTY_NAME; ldd --version | head -1'
step build
mkdir -p /root/ctx && printf 'FROM alpine:3.24\nRUN echo built > /built && apk --version\nCOPY a /a\nCMD cat /built /a\n' > /root/ctx/Containerfile
echo copied > /root/ctx/a
$P build -q --network host -t demo /root/ctx 2>&1 | tail -3
step build without cgroups
$P build -q --network host --runtime-flag cgroup-manager=disabled -t demo /root/ctx 2>&1 | tail -3
$P run $R demo
step build isolation chroot; $P build -q --no-cache --isolation chroot --network host -t demo2 /root/ctx 2>&1 | tail -3
step save and load; $P save -o /root/demo.tar demo 2>&1 | tail -1; $P rmi demo >/dev/null; $P load -q -i /root/demo.tar | tail -1
step userns; $P run $R --userns auto alpine:3.24 sh -c 'cat /proc/self/uid_map' 2>&1 | tail -3
$P run $R --uidmap 0:100000:65536 --gidmap 0:100000:65536 alpine:3.24 sh -c 'cat /proc/self/uid_map; id' 2>&1 | tail -3
step pid and ipc host; $P run $R --pid host --ipc host alpine:3.24 sh -c 'ps -o pid | wc -l' 2>&1 | tail -2
step port publish; $P run --rm --cgroups disabled -p 2381:80 alpine:3.24 true 2>&1 | tail -2
step pasta; $P run --rm --cgroups disabled --network pasta alpine:3.24 true 2>&1 | tail -2
step memory limit; $P run --rm --network host --memory 64m alpine:3.24 true 2>&1 | tail -2
step default network; $P run --rm --cgroups disabled alpine:3.24 true 2>&1 | tail -2
step timing
for i in 1 2 3; do
    ( time $P run $R alpine:3.24 true ) 2>&1 | grep -v '^user\|^sys'
done
step cleanup; $P system df 2>&1 | tail -4
