# Common commands on Ubuntu 26.04 (run ubuntu26-install first): the commands of coreutils written in
# Rust (uutils), findutils, tar and compression, procps, util-linux, iproute2, sudo, su, compiling
# and running C, python, git, ssh, tmux, screen. Output avoids times and pids where possible, so it
# can be compared with native with -n image
export LC_ALL=C.UTF-8 HOME=/root
cd /tmp && rm -rf u26 && mkdir u26 && cd u26
r() { "$@" >/tmp/u26.out 2>&1; rc=$?; printf '%s: rc=%s %s\n' "$*" "$rc" "$(head -c 300 /tmp/u26.out | tr '\n' '|')"; }

echo "== coreutils"
mkdir -p a/b/c && echo hello > a/b/c/f && ln -s c/f a/b/link && ln a/b/c/f a/hard
r ls -la --time-style=+ a
r ls -lR --time-style=+ a/b
r stat -c '%n %F %a %h %s' a a/b/c/f a/b/link a/hard
r cp -a a a2
r ls -l --time-style=+ a2/b
r mv a2 a3
r rm -r a3
r readlink -f a/b/link
r realpath -s a/b/link
r touch -d '2020-01-02 03:04:05' a/b/c/f
r stat -c '%y' a/b/c/f
r chmod -R go-rwx a
r stat -c '%a' a a/b a/b/c/f
r chown -R nobody:nogroup a
r stat -c '%U %G' a/b/c/f
r du -s --apparent-size a
r df -h /tmp
r sh -c 'printf "3\n1\n2\n1\n" | sort -n | uniq -c'
r sh -c 'seq 10 | head -3; seq 10 | tail -2; seq 5 | tac | paste -sd,'
r sh -c 'echo "a:b:c" | cut -d: -f2; echo abc | tr a-c A-C; echo hello | wc -c'
r sh -c 'printf "x\ty\n" | expand; echo 1048576 | numfmt --to=iec; factor 360'
r sh -c 'echo hi | base64; echo hi | sha256sum; echo hi | md5sum'
r sh -c 'dd if=/dev/zero of=z bs=1k count=8 status=none; ls -l z | cut -d" " -f5; truncate -s 100 z; stat -c %s z'
r sh -c 'split -b 3 a/b/c/f part.; ls part.*; cat part.* | od -c | head -2'
r sh -c 'mktemp -d /tmp/u26/t.XXXX | grep -c t; install -m 750 a/b/c/f inst; stat -c %a inst'
r timeout 1 sleep 5
r sh -c 'nproc >/dev/null && echo ok; uname -s; id -u; whoami; env -i A=1 env'
r sh -c 'echo x | tee t1 t2 >/dev/null; cat t1 t2'
r sh -c 'mkfifo ff; (echo via-fifo > ff &); cat ff'
r date -d @0 -u
r sh -c 'yes | head -2; printf "%05d\n" 42; expr 6 \* 7; test -d a && echo dir'
r sh -c 'nohup true >/dev/null 2>&1; echo $?; nice -n 5 nice'
r stty -a
r tty
echo "== findutils, grep, sed, awk"
r find a -name f -type f
r sh -c 'find a -type f -print0 | xargs -0 cat'
r grep -r hello a
r sh -c 'echo foo bar | sed s/foo/baz/; echo 1 2 3 | awk "{print \$2*10}"'
echo "== tar, compression"
r tar -cf t.tar a
r sh -c 'mkdir x && tar -C x -xf t.tar && find x | sort'
r sh -c 'tar -czf t.tgz a && tar -tzf t.tgz | sort'
r sh -c 'tar -cJf t.txz a && tar --zstd -cf t.tzst a && ls t.txz t.tzst'
r sh -c 'mkdir y && tar -C y -xJf t.txz && cat y/a/b/c/f'
r sh -c 'xz -k a/hard -c | xz -d; zstd -q -c a/hard | zstd -dq'
echo "== procps, util-linux"
r sh -c 'ps -o pid=,comm= -p $$ | grep -c sh'
r sh -c 'free -m | head -1'
r sh -c 'uptime | grep -c "load average"'
r sh -c 'sleep 30 & p=$!; pgrep -x sleep | grep -c .; kill $p; wait $p; echo $?'
r sh -c 'pkill -0 -x nonexistent; echo $?'
r sh -c 'lscpu | grep -c Architecture'
r findmnt -n -o TARGET /proc
r sh -c 'script -qc "tty" /dev/null | tr -d "\r" | grep -c /dev/pts'
r sh -c 'getent passwd root; getent hosts localhost'
echo "== iproute2, ping"
r sh -c 'ip -o link show lo | cut -d" " -f2'
r sh -c 'ip route show | grep -c .'
r ping -c 1 -W 2 127.0.0.1
echo "== sudo, su"
r sudo -u nobody id
r sudo -n true
r su nobody -s /bin/sh -c 'id -un'
echo "== gcc, python, git"
printf '#include <pthread.h>\n#include <stdio.h>\n#include <termios.h>\n#include <unistd.h>\nstatic void *f(void *a){return a;}\nint main(void){pthread_t t;void *r;pthread_create(&t,0,f,(void*)42);pthread_join(t,&r);struct termios ts;printf("pthread %%ld isatty %%d tcgetattr %%d\\n",(long)r,isatty(0),tcgetattr(0,&ts));}\n' > t.c
r sh -c 'gcc -O2 -pthread -o t t.c && ./t </dev/null'
r python3 -c 'import concurrent.futures as c, ssl, sqlite3, json, os; print(sum(c.ThreadPoolExecutor(4).map(lambda x: x*x, range(100))), sqlite3.sqlite_version_info >= (3,), os.isatty(0))'
r python3 -c 'import multiprocessing as m; print(m.Pool(2).map(abs, [-1, -2]))'
r sh -c 'python3 -m venv v && v/bin/python -c "import sys; print(sys.prefix.endswith(\"/v\"))"'
r sh -c 'git init -q g && cd g && git config user.email a@b && git config user.name a && echo 1 > f && git add f && git commit -qm m && git log --oneline | wc -l && git status --short | wc -l'
echo "== ssh, terminals"
r sh -c 'ssh-keygen -q -t ed25519 -N "" -f k && ls k k.pub'
r sh -c 'tmux new-session -d -s u26 "sleep 5" && tmux list-sessions -F "#S" && tmux kill-server'
# On a PTY: stty (Rust's coreutils go through glibc 2.43's tcgetattr, that is, TCGETS2), tput, bash
# with readline; GNU screen needs TIOCPKT to start a window
r sh -c 'screen -D -m sh -c "echo in-screen > /tmp/u26/scr" && cat scr'
r sh -c 'script -qec "stty -a | head -1; tput cols; stty raw; stty -a | grep -c -- -icanon; stty sane" /dev/null </dev/null | tr -d "\r"'
r sh -c 'script -qec "bash -ic \"bind -v | grep -c editing-mode\"" /dev/null </dev/null | tr -d "\r" | tail -1'
echo "== cleanup"
cd /tmp && rm -rf u26
