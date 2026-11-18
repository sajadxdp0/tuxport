# Debian (glibc): lldb 19. lldb drives the lldb-server it starts over a socket, and the latter
# manages the client program with ptrace: breakpoints in threads, single-stepping, registers,
# hardware watchpoints, expression evaluation (JIT-compiled code written into the client program's
# memory and executed there), attaching to a running process, reading core files
export DEBIAN_FRONTEND=noninteractive
[ -s /etc/resolv.conf ] || echo 'nameserver 127.0.0.53' > /etc/resolv.conf
if ! command -v lldb >/dev/null || ! command -v gcc >/dev/null; then
  apt-get update -q >/dev/null 2>&1
  apt-get install -y -q --no-install-recommends lldb gcc libc6-dev >/tmp/apt-lldb.log 2>&1
  echo "apt-get install: $?"
fi
lldb --version | head -1
cd /tmp; rm -rf ll; mkdir ll; cd ll
cat > t.c <<'C'
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
long sum[4];
long counter;
static int square(int v) { return v * v; }
static void *f(void *a) {
    long i = (long)a;
    for (long k = 0; k < 1000; k++) sum[i] += square(k % 7);
    return 0;
}
int main(int argc, char **argv) {
    if (argc > 1 && argv[1][0] == 'w') { for (;;) { counter++; usleep(1000); } }
    if (argc > 1 && argv[1][0] == 'c') { counter = 42; raise(SIGSEGV); }
    pthread_t t[4];
    for (long i = 0; i < 4; i++) pthread_create(&t[i], 0, f, (void *)i);
    for (int i = 0; i < 4; i++) pthread_join(t[i], 0);
    counter = sum[0] + sum[1] + sum[2] + sum[3];
    printf("counter=%ld\n", counter);
    return 0;
}
C
gcc -O0 -g -pthread -o t t.c
# The addresses and thread numbers lldb prints differ every time; drop the addresses and look only
# at functions and source locations
strip_addr() { sed -E 's/0x[0-9a-f]+/ADDR/g; s/tid = [0-9]+/tid = N/; s/process [0-9]+/process N/'; }
s=$(date +%s)
echo "== breakpoint in thread"
lldb -b -o 'b square' -o run -o 'bt 3' -o 'p v' -o 'thread list' -o 'register read pc' -o 'next' -o 'frame variable' \
    -o 'breakpoint delete 1' -o continue ./t >b.log 2>&1; echo "exit $? ($(( $(date +%s) - s ))s)"
strip_addr < b.log | grep -E 'stop reason|frame #|^\(int\)|counter=|exited with status|thread #' | sed 's/^ *\* *//' | sort -u | head -20
echo "== step and expression"
lldb -b -o 'b main' -o run -o 'expr square(9)' -o 'expr (int)getpid() > 1' -o 'expr counter = 5' -o 'p counter' -o 'kill' ./t >e.log 2>&1; echo "exit $?"
grep -E '^\((int|long|bool)\)' e.log
echo "== watchpoint"
lldb -b -o 'b main' -o run -o 'watchpoint set variable counter' -o continue -o 'bt 1' -o 'watchpoint delete' -o continue ./t >w.log 2>&1; echo "exit $?"
strip_addr < w.log | grep -E 'Watchpoint created|stop reason|old value|new value|frame #0|exited with status' | sed 's/^ *\* *//'
echo "== attach"
./t w & p=$!; sleep 1
lldb -b -p $p -o 'bt 2' -o 'p counter > 0' -o detach >a.log 2>&1; echo "exit $?"
strip_addr < a.log | grep -E 'stopped|frame #|^\(bool\)|Detaching|detached' | sed 's/^ *\* *//' | head -6
kill -0 $p && echo "still running after detach"; kill $p; wait $p 2>/dev/null
echo "== core"
rm -f core*; (ulimit -c unlimited; ./t c); ls core* >/dev/null 2>&1 && echo "core written"
lldb -b -c "$(ls core* | head -1)" ./t -o 'bt 3' -o 'p counter' >c.log 2>&1; echo "exit $?"
strip_addr < c.log | grep -E 'stop reason|frame #|^\(long\)' | sed 's/^ *\* *//'
