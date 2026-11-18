#include "rt.h"

__asm__(".text\n"
        ".globl _start\n"
        "_start:\n"
        "    mov x29, #0\n"
        "    mov x30, #0\n"
        "    mov x0, sp\n"
        "    b rt_start\n");

_Noreturn void rt_start(uint64_t *sp);

void rt_start(uint64_t *sp)
{
    rt_exit(main((int)sp[0], (char **)(sp + 1)));
}

static long syscall3(long nr, long a, long b, long c)
{
    register long x8 __asm__("x8") = nr;
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
    return x0;
}

long rt_write(int fd, const void *buf, size_t len)
{
    return syscall3(64, fd, (long)buf, (long)len);
}

void rt_exit(int code)
{
    syscall3(94, code, 0, 0);
    for (;;) {
    }
}

void rt_print(const char *s)
{
    size_t n = 0;
    while (s[n])
        n++;
    rt_write(1, s, n);
}

void rt_print_u64(uint64_t v)
{
    char buf[21];
    int i = sizeof(buf);
    do {
        buf[--i] = (char)('0' + v % 10);
        v /= 10;
    } while (v);
    rt_write(1, buf + i, sizeof(buf) - (size_t)i);
}

void rt_print_hex(const void *p, size_t n)
{
    static const char digits[] = "0123456789abcdef";
    const uint8_t *b = p;
    for (size_t i = 0; i < n; i++) {
        char pair[2] = {digits[b[i] >> 4], digits[b[i] & 15]};
        rt_write(1, pair, 2);
    }
}

void rt_fail(const char *what)
{
    rt_print("FAILED: ");
    rt_print(what);
    rt_print("\n");
    rt_exit(1);
}

uint64_t rt_arg_u64(int argc, char **argv, int index, uint64_t fallback)
{
    if (index >= argc)
        return fallback;
    uint64_t v = 0;
    for (const char *s = argv[index]; *s >= '0' && *s <= '9'; s++)
        v = v * 10 + (uint64_t)(*s - '0');
    return v;
}

void *memcpy(void *dst, const void *src, size_t n)
{
    uint8_t *d = dst;
    const uint8_t *s = src;
    while (n--)
        *d++ = *s++;
    return dst;
}

void *memmove(void *dst, const void *src, size_t n)
{
    uint8_t *d = dst;
    const uint8_t *s = src;
    if (d < s) {
        while (n--)
            *d++ = *s++;
    } else {
        while (n--)
            d[n] = s[n];
    }
    return dst;
}

void *memset(void *dst, int c, size_t n)
{
    uint8_t *d = dst;
    while (n--)
        *d++ = (uint8_t)c;
    return dst;
}

int memcmp(const void *a, const void *b, size_t n)
{
    const uint8_t *x = a, *y = b;
    for (size_t i = 0; i < n; i++) {
        if (x[i] != y[i])
            return x[i] < y[i] ? -1 : 1;
    }
    return 0;
}
