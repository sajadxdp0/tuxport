# Programs executed after being rewritten in place: the same inode gets different code (cat >,
# dd conv=notrunc, a writable shared mapping), and what is executed afterwards must be the new
# code. The previous translation is reused, without rechecking the code segment, only once the
# program's ctime is more than two seconds in the past, so wait a while both before and after
# rewriting
set -e
cd /tmp
for n in 42 43; do
    printf '#include <stdio.h>\nint main(void) { printf("%%d\\n", %s * 3 + 1); return 0; }\n' $n > v$n.c
    gcc -O2 -o v$n v$n.c
done
cat > patch.c <<'C'
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

/* Write the contents of argv[2] into argv[1] through a writable shared mapping (the two files are
 * the same length) */
int main(int argc, char **argv)
{
    int dst = open(argv[1], O_RDWR), src = open(argv[2], O_RDONLY);
    struct stat st;
    fstat(dst, &st);
    char *d = mmap(NULL, st.st_size, PROT_READ | PROT_WRITE, MAP_SHARED, dst, 0);
    char *s = mmap(NULL, st.st_size, PROT_READ, MAP_PRIVATE, src, 0);
    if (d == MAP_FAILED || s == MAP_FAILED)
        return 1;
    memcpy(d, s, st.st_size);
    munmap(d, st.st_size);
    close(dst);
    return 0;
}
C
gcc -O2 -o patch patch.c
[ "$(stat -c %s v42)" = "$(stat -c %s v43)" ] && echo same size
cp v42 prog
sleep 3
./prog; ./prog
cat v43 > prog
./prog
sleep 3
./prog; ./prog
dd if=v42 of=prog conv=notrunc 2>/dev/null
./prog
sleep 3
./prog; ./prog
./patch prog v43
./prog
sleep 3
./prog; ./prog

# Shared libraries rewritten in place: when a new process first runs, the library of a translation
# in the pool is not mapped yet, so the translation is parked first and checked when execution
# reaches it; if the contents changed it cannot be used
for n in 42 43; do
    printf 'int v(void) { return %s * 3 + 1; }\n' $n > lib$n.c
    gcc -O2 -shared -fPIC -o lib$n.so lib$n.c
done
[ "$(stat -c %s lib42.so)" = "$(stat -c %s lib43.so)" ] && echo same size
printf '#include <stdio.h>\nint v(void);\nint main(void) { printf("%%d\\n", v()); return 0; }\n' > uselib.c
cp lib42.so libv.so
gcc -O2 -o uselib uselib.c -L. -lv -Wl,-rpath,/tmp
sleep 3
./uselib; ./uselib
cat lib43.so > libv.so
./uselib; ./uselib
dd if=lib42.so of=libv.so conv=notrunc 2>/dev/null
./uselib; ./uselib
