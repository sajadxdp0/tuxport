# After a power loss, checks the data directory $1 of workload.sh; standard input is the "synced v"
# lines printed before the power loss.
# f1..f20 must all exist, each with the complete content of some version; every s$v that was fsynced
# must be intact. Prints "ok" or the problems.
d=$1
cd "$d" || { echo "FAIL no dir"; exit 1; }
lines() { echo $(( ($1 * 7 + $2 * 13) % 40 * 50 + 50 )); }
bad=0
i=1
while [ $i -le 20 ]; do
    if [ ! -e f$i ]; then
        # May be missing when the first round had not finished writing
        [ -e f1 ] && { echo "FAIL f$i missing"; bad=1; }
    else
        first=$(head -n 1 f$i 2>&1)
        v=${first%% *}
        if [ "$first" != "$v $i" ]; then
            echo "FAIL f$i first line [$first] size $(wc -c < f$i 2>&1)"
            bad=1
        else
            n=$(wc -l < f$i 2>&1)
            u=$(sort -u f$i 2>&1 | wc -l)
            if [ "$n" != "$(lines $v $i)" ] || [ "$u" != 1 ]; then
                echo "FAIL f$i v$v lines $n want $(lines $v $i) distinct $u"
                bad=1
            fi
        fi
    fi
    i=$((i + 1))
done
while read -r _ v; do
    n=$(wc -l < s$v 2>&1)
    u=$(sort -u s$v 2>&1)
    if [ "$n" != 500 ] || [ "$u" != "s $v" ]; then
        echo "FAIL s$v fsynced but lines [$n]"
        bad=1
    fi
done
[ $bad -eq 0 ] && echo ok
