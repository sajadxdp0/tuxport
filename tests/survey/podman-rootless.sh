# podman run by an ordinary user (rootless): the user's own user namespace through newuidmap and
# /etc/subuid, the vfs storage driver (from the system's storage.conf, as the Android app sets it),
# no systemd (cgroupfs, file events), pasta and slirp4netns networking (which need a TUN device:
# the default is the host network the Android app configures). Uses the podman store:
# scripts/survey -s target/survey/podman podman-rootless. Native comparison:
# SURVEY_DOCKER_OPTS=--privileged scripts/survey -n tuxport-podman-native podman-rootless
exec 2>&1
for p in podman crun shadow-uidmap passt slirp4netns libcap-utils; do
    apk info -e $p >/dev/null || apk add -q $p
done
id rl >/dev/null 2>&1 || adduser -D -u 1000 rl
grep -q '^rl:' /etc/subuid || echo rl:100000:65536 >>/etc/subuid
grep -q '^rl:' /etc/subgid || echo rl:100000:65536 >>/etc/subgid
mkdir -p /run/user/1000 && chown rl:rl /run/user/1000 && chmod 700 /run/user/1000
# What the Android app writes before every start (Rootfs.ensureContainersConfig): without a TUN
# device rootless containers default to the host's network (when missing), and without overlayfs
# podman's storage driver is vfs (written when missing, an overlay driver changed); the user has no
# storage.conf of its own and takes the system's driver
mkdir -p /etc/containers/containers.rootless.conf.d
[ -e /etc/containers/containers.rootless.conf.d/tuxport.conf ] ||
    printf '[containers]\nnetns = "host"\n' >/etc/containers/containers.rootless.conf.d/tuxport.conf
if [ -e /etc/containers/storage.conf ]; then
    sed -i 's/^driver = "overlay"/driver = "vfs"/' /etc/containers/storage.conf
else
    printf '[storage]\ndriver = "vfs"\n' >/etc/containers/storage.conf
fi
mkdir -p /home/rl/.config/containers
rm -f /home/rl/.config/containers/storage.conf
printf '[engine]\ncgroup_manager = "cgroupfs"\nevents_logger = "file"\n' >/home/rl/.config/containers/containers.conf
chown -R rl:rl /home/rl/.config
# As the user, with its runtime directory; each step's output and exit status
as() { su rl -s /bin/sh -c "cd /home/rl; export XDG_RUNTIME_DIR=/run/user/1000 HOME=/home/rl; $1"; }
step() { echo; echo "== $1"; as "$2"; echo "exit=$?"; }

step reset 'podman rm -af >/dev/null; podman rmi -f rdemo >/dev/null; podman system migrate >/dev/null; echo ok'
step info 'podman info --format "{{.Host.Security.Rootless}} {{.Host.CgroupManager}} {{.Host.CgroupsVersion}} {{.Host.NetworkBackend}} {{.Store.GraphDriverName}}"'
step "unshare: the user's namespace" 'podman unshare cat /proc/self/uid_map /proc/self/gid_map; podman unshare id'
step pull 'podman image exists alpine:3.24 || podman pull -q docker.io/library/alpine:3.24 >/dev/null; echo pulled'
step "run, host network" 'podman run --rm --network host alpine:3.24 sh -c "id; cat /proc/self/uid_map; echo hello"'
step "run, default network" 'podman run --rm alpine:3.24 sh -c "ip -o addr | cut -d\" \" -f2,7; cat /proc/sys/net/ipv4/ping_group_range"'
step "run, slirp4netns" 'podman run --rm --network slirp4netns alpine:3.24 sh -c "ip -o -4 addr | cut -d\" \" -f2,7"'
step "ping socket" 'podman run --rm alpine:3.24 ping -c 1 -W 2 127.0.0.1 | grep -c "bytes from"'
step "net sysctl" 'podman run --rm --sysctl net.ipv4.ip_unprivileged_port_start=80 alpine:3.24 cat /proc/sys/net/ipv4/ip_unprivileged_port_start'
step "keep-id" 'podman run --rm --network host --userns keep-id alpine:3.24 id'
step "volume" 'mkdir -p vol && echo from-host >vol/a && podman run --rm --network host -v ./vol:/data alpine:3.24 sh -c "cat /data/a; ls -ln /data/a; echo x >/data/b" && ls -ln vol/b | cut -d" " -f3,4'
step "port publish" 'podman run -d --name web -p 2390:2390 alpine:3.24 sh -c "exec nc -lk -p 2390 -e cat /etc/alpine-release" >/dev/null; sleep 3; nc 127.0.0.1 2390 </dev/null; podman rm -f web >/dev/null'
step "file capabilities in a build" 'mkdir -p ctx && printf "FROM alpine:3.24\nRUN apk add -q libcap-utils && cp /bin/busybox /bb && setcap cap_net_raw+ep /bb && getcap /bb\n" >ctx/Containerfile && podman build -q --network host -t rdemo ctx | tail -2 && podman run --rm --network host rdemo getcap /bb'
step "SysV IPC: root of the container removes another user's queue" 'podman run --rm --network host --cap-add SYS_ADMIN,IPC_OWNER alpine:3.24 sh -c "apk add -q util-linux-misc && su -s /bin/sh nobody -c \"ipcmk -Q -p 600\" | grep -o \"[0-9]*\" >/tmp/q && ipcrm -q \$(cat /tmp/q) && echo removed"'
step "network sysctls of the user's own network namespace" 'podman unshare unshare -n sh -c "echo 0 2147483647 >/proc/sys/net/ipv4/ping_group_range && cat /proc/sys/net/ipv4/ping_group_range; echo 1 >/proc/sys/net/ipv6/conf/default/accept_dad && echo dad-ok"'
step "build without capabilities" 'printf "FROM alpine:3.24\nRUN echo built >/built\nCMD cat /built\n" >ctx/Containerfile && podman build -q --network host -t rdemo ctx | tail -1 | cut -c1-12 >/dev/null && podman run --rm --network host rdemo'
step "exec, logs, stop" 'podman run -d --name svc --network host alpine:3.24 sh -c "trap \"echo got TERM; exit 3\" TERM; echo up; while :; do sleep 1; done" >/dev/null && sleep 2 && podman exec svc id -u && podman stop -t 5 svc >/dev/null; podman logs svc; podman rm svc >/dev/null'
step "ping, host network" 'podman run --rm --network host alpine:3.24 ping -c 1 -W 2 127.0.0.1 | grep -c "bytes from"'
step "memory limit" 'podman run --rm --network host --memory 64m alpine:3.24 true'
step "pids limit" 'podman run --rm --network host --pids-limit 50 alpine:3.24 true'
step "cgroup inside" 'podman run --rm --network host alpine:3.24 sh -c "cat /proc/self/cgroup; ls /sys/fs/cgroup | head -3"'
step "timing" 'for i in 1 2 3; do s=$(date +%s); podman run --rm --network host alpine:3.24 true; echo $(( $(date +%s) - s ))s; done'
step cleanup 'podman rm -af >/dev/null; podman rmi -f rdemo >/dev/null; echo done'
