# docker-run-options: --privileged
# Mount and unmount events propagate (fs/pnode.c): a mount under a shared mount is copied to its
# peers and slaves, not back from a slave; unmounting takes the copies down, refused while a copy
# is in use; a copy goes under a mount already on that place, which moves back when the copy goes;
# MS_MOVE into a shared mount propagates; peers and slaves across mount namespaces, and copies
# locked below their root in a namespace owned by a new user namespace. Lines: mount point under
# /tmp/pg, source, propagation (groups numbered in order of appearance), sorted by mount point
set -e
cd /tmp
rm -rf pg
mkdir pg
mount -t tmpfs pg pg
mount --make-private pg
cd pg
show() {
    awk '$5 ~ "^/tmp/pg/" {
        t = ""
        for (i = 7; $i != "-"; i++)
            if ($i ~ /^(shared|master):/)
                t = t " " $i
        sub("^/tmp/pg/", "", $5)
        print $5, $(i + 2) t
    }' /proc/self/mountinfo | sort | awk '{
        for (i = 3; i <= NF; i++) {
            split($i, kv, ":")
            if (!(kv[2] in g))
                g[kv[2]] = ++n
            $i = kv[1] ":" g[kv[2]]
        }
        print "  " $0
    }'
}
mkdir a b c s
mount --bind a a
mount --make-shared a
mount --bind a b
mount --bind a c
mount --make-slave c
echo "a and b peers, c a slave"
show

echo "a mount in a reaches b and c"
mkdir a/x
mount -t tmpfs x a/x
show
echo "one in the slave stays there"
mkdir c/y
mount -t tmpfs y c/y
show
echo "one in b reaches a and c"
mkdir b/w
mount -t tmpfs w b/w
show

echo "unmounting in the slave: only there"
umount c/y
show
echo "unmounting in b: in a and c too"
umount b/x
show

echo "a copy in use"
mkdir a/v
mount -t tmpfs v a/v
(cd c/v && exec sleep 3) &
sleep 0.5
umount a/v 2>&1 | sed 's/.*: //'
kill $!
wait $! 2>/dev/null || true
umount a/v
show

echo "a copy goes under a mount already there"
mkdir a/z
mount -t tmpfs mine c/z
touch c/z/mine
mount -t tmpfs theirs a/z
touch a/z/theirs
show
echo "  c/z holds: $(ls c/z)"
echo "unmounting it: the one above moves back"
umount a/z
show
echo "  c/z holds: $(ls c/z)"
umount c/z

echo "a move into a shared mount"
mkdir s/m a/moved
mount -t tmpfs moved s/m
mount --move s/m a/moved
show

echo "a peer in another mount namespace"
mkdir a/n
unshare -m --propagation unchanged sh -c 'mount -t tmpfs n /tmp/pg/a/n'
show
echo "a slave namespace: sees new mounts, sends none"
mkdir a/k a/j
mkfifo go
unshare -m --propagation slave sh -c '
    mount -t tmpfs k /tmp/pg/a/k
    read x < /tmp/pg/go
    grep -c " /tmp/pg/a/j " /proc/self/mountinfo' &
sleep 0.5
mount -t tmpfs j a/j
echo go > go
wait $!
show
echo "copies in a new user namespace are locked, but for their root"
mkdir -p s/t a/u
mount -t tmpfs t s/t
mkdir s/t/in
mount -t tmpfs in s/t/in
unshare -U -r -m --propagation unchanged sh -c '
    read x < /tmp/pg/go
    for m in /tmp/pg/a/u/in /tmp/pg/a/u; do
        umount $m 2>&1 | sed "s/.*: /  /"
        echo "  $m: $(grep -c " $m " /proc/self/mountinfo)"
    done' &
sleep 0.5
mount --rbind s/t a/u
echo go > go
wait $!

cd /tmp
umount -l /tmp/pg
rmdir pg
