# The workload of the power-loss test (the guest's or native busybox sh): $1 is the data directory.
# Each round v replaces f1..f20 with version v by "write a temporary file, rename it over the old
# one" (this is how apk installs files);
# every 5 rounds it writes an s$v, fsyncs it and the directory, prints "synced v" (it must be intact
# after a power loss), and pauses for a moment.
d=$1
mkdir -p "$d"
cd "$d" || exit 1
lines() { echo $(( ($1 * 7 + $2 * 13) % 40 * 50 + 50 )); }
v=0
while :; do
    v=$((v + 1))
    i=1
    while [ $i -le 20 ]; do
        yes "$v $i" | head -n "$(lines $v $i)" > f$i.tmp
        mv f$i.tmp f$i
        i=$((i + 1))
    done
    if [ $((v % 5)) -eq 0 ]; then
        yes "s $v" | head -n 500 > s$v
        fsync s$v . && echo "synced $v"
        # Pause: Tuxport's background thread finishes the checkpoint once writes stop, and the WAL is
        # reused from the start afterwards
        sleep 0.3
    fi
done
