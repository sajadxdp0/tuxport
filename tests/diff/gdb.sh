# gdb: breakpoints, single-stepping, inspecting variables, software and hardware watchpoints,
# hardware breakpoints, threads. Load addresses and pids differ between the two sides, so the
# outputs are compared after normalization
set -e
cd /tmp
cat > loop.c <<'C'
#include <stdio.h>

static int square(int x)
{
    int y = x * x;
    return y;
}

int counter;

int main(void)
{
    int total = 0;
    for (int i = 1; i <= 3; i++) {
        counter++;
        total += square(i);
    }
    printf("total %d\n", total);
    return 0;
}
C
cat > threads.c <<'C'
#include <pthread.h>
#include <stdio.h>

static int done;

static void *worker(void *arg)
{
    done = *(int *)arg;
    return 0;
}

int main(void)
{
    pthread_t t;
    int v = 7;
    pthread_create(&t, 0, worker, &v);
    pthread_join(t, 0);
    printf("done %d\n", done);
    return 0;
}
C
gcc -g -O0 -o loop loop.c
gcc -g -O0 -o threads threads.c -lpthread
# In info threads, keep only the number and name on the lines of threads other than the current
# one: which frame the other thread is stopped in depends on scheduling (the main thread may still
# be in clone, or already waiting in pthread_join). The aligning spaces after the name on the
# current thread's line vary with the number of digits of the LWP number; squeeze them into one
norm() {
    sed -E 's/0x[0-9a-f]+/ADDR/g; s/(process|LWP|Thread) [0-9]+/\1 N/g; s/^\[Inferior 1 \(process N\)/[Inferior 1/' |
        sed -E 's/^(  [0-9]+ +LWP N "[^"]*").*/\1/; s/^(\* [0-9]+ +LWP N "[^"]*") +/\1 /'
}
# Software watchpoints (compared after single-stepping each instruction)
gdb -batch -nx -ex 'set can-use-hw-watchpoints 0' -ex 'break square' -ex run -ex bt -ex 'print x' \
    -ex next -ex next -ex 'print y' -ex finish -ex 'watch counter' -ex continue -ex continue -ex delete \
    -ex stepi -ex 'x/2i $pc' -ex 'print $w0' -ex 'print $pc == &square + 4' -ex continue ./loop 2>&1 | norm
# square has run before (Tier 1 has already translated it), and the breakpoint is set afterwards
gdb -batch -nx -ex 'break main' -ex run -ex next -ex next -ex next -ex next -ex 'break square' -ex continue \
    -ex bt -ex 'delete 2' -ex continue ./loop 2>&1 | norm
# Do not print thread creation and exit: whether the program's last output or gdb's report of the
# thread exiting comes first is not fixed
gdb -batch -nx -ex 'set print thread-events off' -ex 'break worker' -ex run -ex 'info threads' \
    -ex 'print *(int *)arg' -ex finish -ex continue ./threads 2>&1 | norm
# Stop at system calls (PTRACE_SYSCALL)
gdb -batch -nx -ex 'catch syscall writev' -ex run -ex 'info registers x8' -ex continue -ex continue ./loop 2>&1 | norm
# Hardware watchpoints (write, two unaligned bytes, read, read-write) and hardware breakpoints,
# with another thread triggering a watchpoint. Tuxport's are checked only by the interpreter, and
# threads that have them run only in Tier 0. kill at the end: if it ran to completion, whether the
# program's last output or gdb's report of the thread exiting comes first is not fixed
cat > watch.c <<'C'
#include <pthread.h>
#include <stdio.h>

struct rec {
    char tag;
    short half;
    int value;
} rec;
volatile int counter;
int reads = 5;

static int work(int i)
{
    rec.half = i;
    return reads;
}

static void *thread(void *arg)
{
    (void)arg;
    counter += 100;
    return 0;
}

int main(void)
{
    int sum = 0;
    for (int i = 0; i < 3; i++) {
        counter += 2;
        sum += work(i);
    }
    pthread_t t;
    pthread_create(&t, 0, thread, 0);
    pthread_join(t, 0);
    printf("%d %d %d\n", sum, rec.half, counter);
    return 0;
}
C
gcc -g -O0 -o watch watch.c -lpthread
gdb -batch -nx -ex 'break main' -ex run -ex 'watch counter' -ex continue -ex continue -ex 'delete 2' \
    -ex 'watch rec.half' -ex continue -ex 'delete 3' -ex 'rwatch reads' -ex continue -ex 'delete 4' \
    -ex 'awatch rec.tag' -ex 'info watchpoints' -ex 'delete 5' -ex 'hbreak work' -ex continue -ex bt -ex 'delete 6' \
    -ex 'watch counter' -ex continue -ex 'info threads' -ex kill ./watch 2>&1 | norm
