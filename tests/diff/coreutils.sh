# busybox text processing, compression and shell arithmetic
echo hello | sha256sum
seq 1 10000 | awk '{ s += $1 } END { print s }'
seq 1 2000 | sort -r | md5sum
printf 'b\na\nc\n' | sort
head -c 100000 /dev/zero | gzip -9 | wc -c
# /dev/zero and /dev/urandom fill the whole block in one read (not 64 KB)
dd if=/dev/zero of=/dev/null bs=4M count=2 2>&1 | head -2
dd if=/dev/urandom of=/dev/null bs=1M count=1 2>&1 | head -2
gzip -c /etc/passwd | gzip -d | md5sum
seq 1 3000 | bzip2 -9 | bzip2 -d | tail -1
printf '%x %o %e %s\n' 255 8 12345.678 "$(echo abc | tr a-c A-C)"
echo $((7 * 6)) $((1 << 40)) "$(expr 10 / 3)"
date -u -d @1000000000 '+%Y-%m-%d %H:%M:%S'
echo 'one two three' | sed 's/\(t[a-z]*\)/[\1]/g'
seq 1 20 | paste -sd+ | bc 2>/dev/null || seq 1 20 | awk '{ s += $1 } END { print s }'
