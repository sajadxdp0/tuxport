# podman with its overlay storage driver on Tuxport's overlayfs (rootful): what containers/storage
# concludes from its probes, writes, deletions and renames inside containers over the image's
# layers, an overlay volume (`:O`, whose changes stay out of the host directory), diff and commit,
# a build whose later step deletes what an earlier one made, save and load, and how long starting
# a container takes. Host networking and no cgroups, as podman.sh. Its storage root is its own, so
# podman.sh's vfs images stay:
#   scripts/survey -s target/survey/podman podman-overlay
# Native comparison: scripts/survey -n <an alpine:3.24 image with podman installed> podman-overlay,
# SURVEY_DOCKER_OPTS=--privileged (its storage must not be on docker's own overlay root, so it is
# on a tmpfs there)
exec 2>&1
[ -x /usr/bin/podman ] || apk add -q podman
[ -x /usr/bin/crun ] || apk add -q crun
ROOT=/var/lib/containers/overlay-survey
# Natively the root filesystem is docker's overlay, which cannot be an upper layer
if grep -q ' / / .* - overlay ' /proc/self/mountinfo; then
    mkdir -p $ROOT && mount -t tmpfs tmpfs $ROOT
fi
P="podman --root $ROOT --runroot /run/containers/overlay-survey --storage-driver overlay"
P="$P --runtime crun --cgroup-manager cgroupfs"
R="--rm --network host --cgroups disabled"
$P rm -af >/dev/null 2>&1; $P rmi -af >/dev/null 2>&1
step() { echo; echo "== $*"; }
img() { $P image exists "$1" || $P pull -q "$1" >/dev/null; }

step version; podman --version
step storage; $P info --format '{{.Store.GraphDriverName}} native-diff={{index .Store.GraphStatus "Native Overlay Diff"}} d_type={{index .Store.GraphStatus "Supports d_type"}} metacopy={{index .Store.GraphStatus "Using metacopy"}} backing={{index .Store.GraphStatus "Backing Filesystem"}}'
img docker.io/library/alpine:3.24
step root filesystem; $P run $R alpine:3.24 sh -c 'grep " / / " /proc/self/mountinfo | sed "s/.* - //; s/lowerdir=[^,]*/lowerdir=…/; s/upperdir=[^,]*/upperdir=…/; s/workdir=[^,]*/workdir=…/"; stat -f -c %T /'
step changes over the image
$P run $R alpine:3.24 sh -c '
    echo more >> /etc/motd && tail -1 /etc/motd
    rm /etc/issue && ls /etc/issue 2>&1
    echo back > /etc/issue && cat /etc/issue
    rm -r /etc/apk/keys && mkdir /etc/apk/keys && ls -a /etc/apk/keys
    mv /etc/apk /etc/apk2 2>&1; echo "mv of an image directory: $?"; ls /etc/apk2 | head -3
    chmod 600 /etc/hosts && stat -c "%a" /etc/hosts
    mkdir /new && echo x > /new/f && mv /new /new2 && cat /new2/f
    ln /etc/passwd /passwd2 && stat -c "%h" /etc/passwd
'
step diff and commit
$P run --name c1 --network host --cgroups disabled alpine:3.24 sh -c 'echo x > /made; rm /etc/motd; mkdir /etc/apk/sub' >/dev/null
$P diff c1 | sort
$P commit -q c1 demo-ovl >/dev/null && $P run $R demo-ovl sh -c 'cat /made; ls /etc/motd 2>&1; ls -d /etc/apk/sub'
$P rm c1 >/dev/null
step overlay volume
mkdir -p /root/ovol && echo host > /root/ovol/a && echo keep > /root/ovol/k
$P run $R -v /root/ovol:/data:O alpine:3.24 sh -c 'cat /data/a; echo changed > /data/a; echo new > /data/b; rm /data/k; mkdir /data/d; ls /data; grep " /data " /proc/self/mountinfo | sed "s/.* - //; s/lowerdir=[^,]*/lowerdir=…/; s/upperdir=[^,]*/upperdir=…/; s/workdir=[^,]*/workdir=…/"'
echo "host after: $(cat /root/ovol/a) $(ls /root/ovol | tr '\n' ' ')"
rm -rf /root/ovol
step mount a container; id=$($P create --network host --cgroups disabled alpine:3.24 true)
m=$($P mount $id) && ls $m/etc/alpine-release && $P umount $id >/dev/null; $P rm $id >/dev/null
step build
mkdir -p /root/ctx-ovl
printf 'FROM alpine:3.24\nRUN echo one > /one && mkdir /d && echo f > /d/f\nRUN rm /one /d/f && rm /etc/motd\nRUN mkdir -p /d/g && echo two > /d/g/two\nCMD ls -R /d; ls /one /etc/motd 2>&1\n' > /root/ctx-ovl/Containerfile
$P build -q --network host -t demo-build /root/ctx-ovl >/dev/null && $P run $R demo-build
$P history --format '{{.CreatedBy}}' demo-build | head -4 | cut -c1-60
step save and load
$P save -q -o /root/demo-ovl.tar demo-build && $P rmi -f demo-build >/dev/null &&
    $P load -q -i /root/demo-ovl.tar | tail -1 && $P run $R demo-build | head -2
rm -f /root/demo-ovl.tar
step glibc image; img docker.io/library/debian:trixie-slim
$P run $R debian:trixie-slim sh -c '. /etc/os-release; echo $PRETTY_NAME; echo x > /usr/share/doc/x && rm -r /usr/share/doc && ls /usr/share/doc 2>&1'
step start time
for i in 1 2 3; do
    s=$(cut -d" " -f1 /proc/uptime); $P run $R alpine:3.24 true; e=$(cut -d" " -f1 /proc/uptime)
    echo "run: $(awk -v s=$s -v e=$e 'BEGIN { printf "%.2f", e - s }') s"
done
step layers; $P images --format '{{.Repository}}:{{.Tag}} {{.Size}}' | sort
$P rm -af >/dev/null 2>&1; $P rmi -af >/dev/null 2>&1
step left mounted; grep ' - overlay ' /proc/self/mountinfo | grep -vc ' / / ' || true
