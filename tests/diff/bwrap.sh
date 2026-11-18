# docker-run-options: --cap-add SYS_ADMIN --cap-add NET_ADMIN --security-opt apparmor=unconfined --security-opt systempaths=unconfined
# bubblewrap: a few common uses, including the arguments Codex's sandbox actually passes
# (docs/rounds/123-bwrap.md). The native side is a docker container with CAP_SYS_ADMIN that does
# not mask the paths under /proc (with them masked, Linux does not allow mounting a new procfs in
# a user namespace). Supplementary groups, and pids when not in a new pid namespace, differ
# between the two sides and are not compared; for mounts only the few mount points bwrap creates
# itself and whether they are read-only are compared (filesystem types and mounts brought in from
# outside differ between the two sides)
cd /tmp
rm -rf ws
mkdir -p ws

echo "== plain"
bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp sh -c '
    echo "uid $(id -u) gid $(id -g)"
    touch /x 2>&1; touch /tmp/x && echo "tmp writable"
    ls /dev | tr "\n" " "; echo
' 2>&1

echo "== mounts"
bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --ro-bind /etc/passwd /tmp/passwd \
    --bind /tmp/ws /tmp/ws awk '{print $5, ($6 ~ /^ro/) ? "ro" : "rw"}' /proc/self/mountinfo 2>&1 |
    grep -E "^/(tmp|tmp/passwd|tmp/ws|dev/null|dev/pts|proc) " | sort

echo "== unshare-all"
# The processes in the new pid namespace are counted by the shell's own glob, which starts no
# process: through a pipe (ls /proc | grep -c) grep was counted only when it had started before ls
# read /proc, so the count was 3 or 4 on either side
bwrap --unshare-all --hostname box --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp sh -c '
    echo "uid $(id -u) gid $(id -g) pid $$"; hostname
    set -- /proc/[0-9]*; echo "$# processes"
    ip -o addr | cut -d" " -f2-4
    wget -q -T 2 -O- http://10.0.0.1/ 2>&1 | head -1
' 2>&1

echo "== codex"
bwrap --as-pid-1 --new-session --die-with-parent --tmpfs / --dev /dev --ro-bind /bin /bin \
    --ro-bind /etc /etc --ro-bind /lib /lib --ro-bind /sbin /sbin --ro-bind /usr /usr \
    --bind /tmp/ws /tmp/ws --chdir /tmp/ws \
    --unshare-user --unshare-pid --unshare-ipc --unshare-net --proc /proc --cap-drop ALL \
    -- /bin/sh -c '
    echo "pid $$ in $(pwd), uid $(id -u) gid $(id -g)"
    grep -E "^Cap(Eff|Prm|Bnd)" /proc/self/status
    ls / | tr "\n" " "; echo
    touch /etc/x 2>&1; echo work > /tmp/ws/out && echo "workspace writable"
    ip -o link | cut -d: -f2
' 2>&1
echo "outside sees: $(cat ws/out)"

echo "== uid"
bwrap --unshare-user --uid 1000 --gid 1000 --ro-bind / / --proc /proc sh -c 'echo "uid $(id -u) gid $(id -g)"; grep -E "^Cap(Eff|Prm)" /proc/self/status' 2>&1

echo "== errors"
bwrap --ro-bind /nonexistent /x true 2>&1
bwrap --ro-bind / / --chdir /nonexistent true 2>&1
echo "exit $?"
