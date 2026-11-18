/*
 * Minimal runtime of the benchmark programs: no libc, only the two syscalls write and exit_group,
 * so that M0's kernel layer can run them (design doc §14 M2).
 */
#ifndef BENCH_RT_H
#define BENCH_RT_H

#include <stddef.h>
#include <stdint.h>

int main(int argc, char **argv);

long rt_write(int fd, const void *buf, size_t len);
_Noreturn void rt_exit(int code);

void rt_print(const char *s);
void rt_print_u64(uint64_t v);
void rt_print_hex(const void *p, size_t n);
_Noreturn void rt_fail(const char *what);

/* The decimal value of argv[index]; fallback when there is no such argument */
uint64_t rt_arg_u64(int argc, char **argv, int index, uint64_t fallback);

/* The compiler may generate calls to these functions */
void *memcpy(void *dst, const void *src, size_t n);
void *memmove(void *dst, const void *src, size_t n);
void *memset(void *dst, int c, size_t n);
int memcmp(const void *a, const void *b, size_t n);

#endif /* BENCH_RT_H */
