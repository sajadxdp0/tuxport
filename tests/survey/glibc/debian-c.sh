# Debian (glibc): gcc/g++ compiling a multithreaded program, gdb, strace, locale, NSS, iconv
cd /tmp; rm -rf ct; mkdir ct; cd ct
cat > t.c <<'C'
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/wait.h>
#include <sched.h>
static long sum[4];
static void *f(void *a) { long i = (long)a; for (long k = 0; k < 3000000; k++) sum[i] += k % 7; return 0; }
int main(void) {
    pthread_t t[4];
    for (long i = 0; i < 4; i++) pthread_create(&t[i], 0, f, (void *)i);
    for (int i = 0; i < 4; i++) pthread_join(t[i], 0);
    pid_t p = fork(); if (!p) _exit(7); int st; waitpid(p, &st, 0);
    printf("sum=%ld child=%d cpu=%d nprocs=%ld\n", sum[0] + sum[1] + sum[2] + sum[3], WEXITSTATUS(st), sched_getcpu(), sysconf(_SC_NPROCESSORS_ONLN));
    return 0;
}
C
gcc -O2 -g -pthread -o t t.c && ./t
cat > t.cc <<'C'
#include <thread>
#include <vector>
#include <atomic>
#include <iostream>
#include <future>
#include <regex>
int main() { std::atomic<long> n{0}; std::vector<std::thread> v; for (int i = 0; i < 8; i++) v.emplace_back([&]{ for (int k = 0; k < 100000; k++) n++; });
    for (auto &t : v) t.join(); auto f = std::async(std::launch::async, []{ return 42; });
    std::cout << "c++ n=" << n << " async=" << f.get() << " regex=" << std::regex_match("abc123", std::regex("[a-z]+\\d+")) << std::endl;
    try { throw std::runtime_error("x"); } catch (std::exception &e) { std::cout << "exception ok" << std::endl; } }
C
g++ -O2 -o tcc t.cc && ./tcc
echo "== gdb"; gdb -q -batch -ex 'break f' -ex run -ex bt -ex 'info threads' ./t 2>&1 | grep -E '^#|Breakpoint 1,|Thread' | head -6
echo "== strace"; strace -f -c -o /tmp/st.txt ./t >/dev/null; head -8 /tmp/st.txt
echo "== locale"; sed -i 's/^# *en_US.UTF-8/en_US.UTF-8/' /etc/locale.gen; locale-gen 2>&1 | tail -1; LANG=en_US.UTF-8 date +%A; locale -a | head -3
echo "== nss"; getent passwd root; getent hosts localhost; getent ahosts deb.debian.org | head -1
echo "== iconv"; printf 'h\303\251llo\n' | iconv -f UTF-8 -t ISO-8859-1 | od -c | head -1
echo "== file"; file /bin/bash
