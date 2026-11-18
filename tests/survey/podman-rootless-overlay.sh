# podman run by an ordinary user (rootless) with its overlay storage driver on Tuxport's overlayfs:
# the kernel's overlay mounted in the user's own user and mount namespaces with `userxattr`, the
# opaque directories and whiteouts that layer extraction and containers make as that user, an
# overlay volume (`:O`), diff, commit, a build that deletes what an earlier step made, volumes,
# and how long starting a container takes. Its storage root is its own, so podman-rootless.sh's
# vfs images stay. Uses the podman store:
#   scripts/survey -s target/survey/podman podman-rootless-overlay
# Native comparison:
# SURVEY_DOCKER_OPTS=--privileged scripts/survey -n tuxport-podman-native podman-rootless-overlay
# (there the storage is on a tmpfs: docker's overlay root cannot be an upper layer)
exec 2>&1
for p in podman crun shadow-uidmap attr; do
    apk info -e $p >/dev/null || apk add -q $p
done
id rl >/dev/null 2>&1 || adduser -D -u 1000 rl
grep -q '^rl:' /etc/subuid || echo rl:100000:65536 >>/etc/subuid
grep -q '^rl:' /etc/subgid || echo rl:100000:65536 >>/etc/subgid
mkdir -p /run/user/1000 && chown rl:rl /run/user/1000 && chmod 700 /run/user/1000
mkdir -p /etc/containers/containers.rootless.conf.d
[ -e /etc/containers/containers.rootless.conf.d/tuxport.conf ] ||
    printf '[containers]\nnetns = "host"\n' >/etc/containers/containers.rootless.conf.d/tuxport.conf
mkdir -p /home/rl/.config/containers
printf '[engine]\ncgroup_manager = "cgroupfs"\nevents_logger = "file"\n' >/home/rl/.config/containers/containers.conf
ROOT=/home/rl/.local/share/containers/overlay-survey
mkdir -p $ROOT
if grep -q ' / / .* - overlay ' /proc/self/mountinfo; then
    mountpoint -q $ROOT || mount -t tmpfs -o mode=700,uid=1000,gid=1000 tmpfs $ROOT
fi
chown -R rl:rl /home/rl/.config /home/rl/.local
P="podman --root $ROOT --runroot /run/user/1000/overlay-survey --storage-driver overlay"
# As the user, with its runtime directory; each step's output and exit status
as() { su rl -s /bin/sh -c "cd /home/rl; export XDG_RUNTIME_DIR=/run/user/1000 HOME=/home/rl; $1"; }
step() { echo; echo "== $1"; as "$2"; echo "exit=$?"; }

step reset "$P rm -af >/dev/null; $P rmi -af >/dev/null; echo ok"
step storage "$P info --format '{{.Host.Security.Rootless}} {{.Store.GraphDriverName}} native-diff={{index .Store.GraphStatus \"Native Overlay Diff\"}} d_type={{index .Store.GraphStatus \"Supports d_type\"}} metacopy={{index .Store.GraphStatus \"Using metacopy\"}}'"
step pull "$P pull -q docker.io/library/alpine:3.24 >/dev/null && echo pulled"
step "root filesystem" "$P run --rm --network host alpine:3.24 sh -c 'grep \" / / \" /proc/self/mountinfo | sed \"s/.* - //; s/lowerdir=[^,]*/lowerdir=…/; s/upperdir=[^,]*/upperdir=…/; s/workdir=[^,]*/workdir=…/\"'"
step "changes over the image" "$P run --rm --network host alpine:3.24 sh -c 'id; echo more >> /etc/motd && tail -1 /etc/motd; rm /etc/issue && ls /etc/issue 2>&1; rm -r /etc/apk/keys && mkdir /etc/apk/keys && ls -a /etc/apk/keys; mv /etc/apk /etc/apk2; echo \"mv of an image directory: \$?\"; chown 123:456 /etc/hosts && stat -c \"%u:%g\" /etc/hosts'"
step "diff and commit" "$P run --name c1 --network host alpine:3.24 sh -c 'echo x > /made; rm /etc/motd' >/dev/null; $P diff c1 | sort; $P commit -q c1 rdemo-ovl >/dev/null && $P run --rm --network host rdemo-ovl sh -c 'cat /made; ls /etc/motd 2>&1'; $P rm c1 >/dev/null"
step build "mkdir -p ctx-ovl && printf 'FROM alpine:3.24\nRUN echo one > /one && mkdir /d && echo f > /d/f\nRUN rm /one /d/f && rm /etc/motd\nCMD ls -a /d; ls /one /etc/motd 2>&1; true\n' > ctx-ovl/Containerfile && $P build -q --network host -t rbuild-ovl ctx-ovl >/dev/null && $P run --rm --network host rbuild-ovl"
step volume "mkdir -p vol-ovl && echo from-host > vol-ovl/a && $P run --rm --network host -v ./vol-ovl:/data alpine:3.24 sh -c 'cat /data/a; echo x > /data/b' && cat vol-ovl/b"
step "overlay volume" "mkdir -p ovol && echo host > ovol/a && echo keep > ovol/k && $P run --rm --network host -v ./ovol:/data:O alpine:3.24 sh -c 'cat /data/a; echo changed > /data/a; echo new > /data/b; rm /data/k; mkdir /data/d; ls /data; grep \" /data \" /proc/self/mountinfo | sed \"s/.* - //; s/lowerdir=[^,]*/lowerdir=…/; s/upperdir=[^,]*/upperdir=…/; s/workdir=[^,]*/workdir=…/\"'; echo \"host after: \$(cat ovol/a) \$(ls ovol | tr '\\n' ' ')\"; rm -rf ovol"
step "upper layer seen from outside" "$P run --name c2 --network host alpine:3.24 sh -c 'rm /etc/motd; rm -r /etc/apk/keys; mkdir /etc/apk/keys' >/dev/null; d=\$($P inspect --format '{{.GraphDriver.Data.UpperDir}}' c2); ls -ln \$d/etc/motd | cut -c1-10,30-40; $P unshare getfattr -n user.overlay.opaque --only-values \$d/etc/apk/keys 2>&1; echo; $P rm c2 >/dev/null"
step "start time" "for i in 1 2 3; do s=\$(cut -d' ' -f1 /proc/uptime); $P run --rm --network host alpine:3.24 true; e=\$(cut -d' ' -f1 /proc/uptime); echo \"run: \$(awk -v s=\$s -v e=\$e 'BEGIN { printf \"%.2f\", e - s }') s\"; done"
step cleanup "$P rm -af >/dev/null; $P rmi -af >/dev/null; echo ok"
