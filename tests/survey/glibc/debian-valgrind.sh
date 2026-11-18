# Debian (glibc): valgrind's memcheck (out-of-bounds reads, uninitialised values, leaks), helgrind
# and drd (data races), massif, callgrind, following child processes. valgrind is itself a binary
# translator: it reads /proc/self/maps to check against the mappings it has recorded, writes the
# translated code into memory it mapped and executes it there, and schedules the client program's
# threads with its own scheduler
export DEBIAN_FRONTEND=noninteractive
[ -s /etc/resolv.conf ] || echo 'nameserver 127.0.0.53' > /etc/resolv.conf
if ! command -v valgrind >/dev/null || ! command -v gcc >/dev/null; then
  apt-get update -q >/dev/null 2>&1
  apt-get install -y -q --no-install-recommends valgrind libc6-dbg gcc libc6-dev >/tmp/apt-vg.log 2>&1
  echo "apt-get install: $?"
fi
valgrind --version
cd /tmp; rm -rf vg; mkdir vg; cd vg
cat > m.c <<'C'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int main(void) {
    int *a = malloc(10 * sizeof(int));
    for (int i = 0; i < 10; i++) a[i] = i;
    int s = a[10];
    int *u = malloc(sizeof(int));
    if (*u > 0) s++;
    char *leak = malloc(40);
    strcpy(leak, "leak");
    leak = 0;
    free(a);
    free(u);
    printf("done %d\n", s != 12345);
    return 0;
}
C
cat > r.c <<'C'
#include <pthread.h>
#include <stdio.h>
static int x;
static void *f(void *a) { for (int i = 0; i < 1000; i++) x++; return a; }
int main(void) {
    pthread_t t[2];
    for (int i = 0; i < 2; i++) pthread_create(&t[i], 0, f, 0);
    for (int i = 0; i < 2; i++) pthread_join(t[i], 0);
    printf("x>0 %d\n", x > 0);
    return 0;
}
C
gcc -O0 -g -o m m.c && gcc -O0 -g -pthread -o r r.c
# Addresses and pids in valgrind's reports differ every time; look only at the descriptions and
# source locations
filt() { sed -n 's/^==[0-9]*== //p' | grep -E "$1" | sed 's/0x[0-9A-F]*: //'; }
s=$(date +%s)
echo "== memcheck"; valgrind --leak-check=full --error-exitcode=9 ./m 2>m.log; echo "exit $? ($(( $(date +%s) - s ))s)"
filt 'Invalid read|uninitialised|definitely lost:|at .*main|ERROR SUMMARY' < m.log
echo "== helgrind"; valgrind --tool=helgrind ./r 2>h.log; echo "exit $?"
filt 'Possible data race|at .*: f \(|ERROR SUMMARY' < h.log | sort -u
echo "== drd"; valgrind --tool=drd ./r 2>d.log; echo "exit $?"
filt 'Conflicting|ERROR SUMMARY' < d.log | sort -u
echo "== massif"; valgrind --tool=massif --massif-out-file=massif.out ./m >/dev/null 2>&1; echo "exit $?"
ms_print massif.out | grep -E '^ *[0-9]+ +[0-9,]+ +[0-9,]+' | head -3 | wc -l
echo "== callgrind"; valgrind --tool=callgrind --callgrind-out-file=cg.out ./r >/dev/null 2>&1; echo "exit $?"
callgrind_annotate cg.out 2>/dev/null | grep -cE ' f \[| f$|r\.c:f'
echo "== children"; valgrind -q --trace-children=yes --error-exitcode=9 sh -c 'ls / | wc -l; true' 2>c.log; echo "exit $?"
echo "children errors: $(grep -c 'Invalid' c.log)"
s=$(date +%s)
echo "== python"; valgrind -q python3 -c 'import json; print(json.dumps({"a": [1, 2]}))' 2>p.log; echo "exit $? ($(( $(date +%s) - s ))s)"
echo "python errors: $(grep -c '^==' p.log)"
