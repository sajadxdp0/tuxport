# Ubuntu 24.04 (glibc 2.39): apt installs python and gcc, then compiles and runs a multithreaded
# program
export DEBIAN_FRONTEND=noninteractive
. /etc/os-release; echo "$PRETTY_NAME, $(ldd --version | head -1)"
apt-get update -q 2>&1 | tail -1
apt-get install -y -q --no-install-recommends python3 gcc libc6-dev procps ca-certificates curl >/tmp/apt.log 2>&1; echo "apt-get install: $?"; tail -2 /tmp/apt.log
printf '#include <pthread.h>\n#include <stdio.h>\nstatic void *f(void *a){return a;}\nint main(void){pthread_t t;void *r;pthread_create(&t,0,f,(void*)42);pthread_join(t,&r);printf("pthread %%ld\\n",(long)r);}\n' > /tmp/t.c
gcc -pthread -o /tmp/t /tmp/t.c && /tmp/t
python3 -c 'import concurrent.futures as c, ssl, json; print("python", __import__("sys").version.split()[0], sum(c.ThreadPoolExecutor(4).map(lambda x: x*x, range(100))), ssl.OPENSSL_VERSION.split()[1])'
ps -eo pid,comm | head -3; free -m | head -2; curl -sI https://ubuntu.com | head -1
