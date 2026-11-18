/*
 * Tier 0's native instruction stubs (the engine map): SIMD&FP data processing, CRC32 and LSE
 * atomics are handed to the host CPU, which executes the same instruction, so results match the
 * hardware bit for bit. Tier 0 is only responsible for the whitelist decision (the engine map),
 * address translation and moving registers.
 *
 * Each instruction encoding has one stub, cached in a block of executable memory:
 *
 *   void stub(struct tuxport_cpu *cpu, void *host_addr);
 *
 * General register fields are replaced with host x2–x7 (a register pair takes two), and the
 * address field with x1; x9 and x10 are the stub's own temporaries. Integer vector add/subtract
 * and logical operations use caller-saved v0–v2. Full FP stubs also save host
 * d8–d15 and FPCR, load the guest's FPCR/FPSR/NZCV and V0–V31, and write them back after
 * executing.
 *
 * The cache is shared by all threads and flushed entirely when full. To avoid overwriting a stub
 * another thread is executing, lookup and execution both happen under the read lock, while
 * generating and flushing need the write lock. The cache is double-mapped for W^X
 * (tuxport_map_code), and stubs are generated in the writable alias.
 */
#define _GNU_SOURCE /* needed for pthread_rwlock_t and mremap under -std=c17 */

#include <pthread.h>
#include <stddef.h>
#include <stdlib.h>

#include "emit.h"
#include "internal.h"

#if defined(__aarch64__) && defined(__linux__)
#define NATIVE_SUPPORTED 1
#endif

#ifdef NATIVE_SUPPORTED

#include <sys/mman.h>

#define CACHE_BYTES (UINT64_C(2) << 20)
#define MAX_STUB_WORDS 96
#define TABLE_BITS 12
#define TABLE_SIZE (1u << TABLE_BITS)

struct slot {
    uint32_t insn;
    native_fn fn;
};

static pthread_once_t once = PTHREAD_ONCE_INIT;
static pthread_rwlock_t lock = PTHREAD_RWLOCK_INITIALIZER;
static uint32_t *cache;   /* the address used for executing */
static intptr_t rw_delta; /* the alias used for writing minus the address used for executing */
static size_t used_words;
static size_t table_count;
static struct slot table[TABLE_SIZE];

uint32_t *tuxport_map_code(size_t bytes, intptr_t *delta)
{
    /* Duplicate the shared mapping with mremap, without depending on memfd */
    void *w = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (w != MAP_FAILED) {
        void *x = mremap(w, 0, bytes, MREMAP_MAYMOVE);
        if (x != MAP_FAILED && mprotect(x, bytes, PROT_READ | PROT_EXEC) == 0) {
            *delta = (intptr_t)w - (intptr_t)x;
            return x;
        }
        if (x != MAP_FAILED)
            munmap(x, bytes);
        munmap(w, bytes);
    }
    void *c = mmap(NULL, bytes, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    *delta = 0;
    return c == MAP_FAILED ? NULL : c;
}

void tuxport_unmap_code(uint32_t *code, size_t bytes, intptr_t delta)
{
    munmap(code, bytes);
    if (delta)
        munmap((char *)code + delta, bytes);
}

static void init(void)
{
    cache = tuxport_map_code(CACHE_BYTES, &rw_delta);
}

bool tuxport_native_available(void)
{
    pthread_once(&once, init);
    return cache != NULL;
}

struct buf {
    uint32_t *p;
    size_t n;
};

static void put(struct buf *b, uint32_t insn)
{
    if (b->n < MAX_STUB_WORDS)
        b->p[b->n] = insn;
    b->n++;
}

/* x9 = cpu + off; off is below 2^24 */
static void put_add(struct buf *b, unsigned rd, unsigned rn, size_t off)
{
    uint32_t hi = (uint32_t)(off >> 12), lo = (uint32_t)(off & 0xfff);
    unsigned src = rn;
    if (hi) {
        put(b, enc_add_imm(rd, src, hi, 1));
        src = rd;
    }
    if (lo || !hi)
        put(b, enc_add_imm(rd, src, lo, 0));
}

static size_t x_off(unsigned r)
{
    return offsetof(struct tuxport_cpu, x) + 8 * r;
}

static void generate(struct buf *b, uint32_t insn, const struct native_desc *d)
{
    enum { CPU = 0, ADDR = 1, VBASE = 9, TMP = 10 };
    size_t v_off = offsetof(struct tuxport_cpu, v);
    uint32_t out = insn;
    unsigned host[3];

    if (d->vectors == NATIVE_VN_VM || d->vectors == NATIVE_VD_VN_VM) {
        unsigned rd = insn & 31, rn = (insn >> 5) & 31, rm = (insn >> 16) & 31;
        put_add(b, VBASE, CPU, v_off);
        if (d->vectors == NATIVE_VD_VN_VM)
            put(b, enc_ldr_q(0, VBASE, 16 * rd));
        put(b, enc_ldr_q(1, VBASE, 16 * rn));
        put(b, enc_ldr_q(2, VBASE, 16 * rm));
        /* Execute Vd, Vn, Vm as v0, v1, v2, even when guest operands alias. */
        const uint32_t operand_mask = 31u | (31u << 5) | (31u << 16);
        put(b, (insn & ~operand_mask) | (1u << 5) | (2u << 16));
        put(b, enc_str_q(0, VBASE, 16 * rd));
        put(b, enc_ret());
        return;
    }

    if (d->vectors == NATIVE_FULL_FP) {
        put(b, enc_stp_d_pre(8, 9, 31, -80));
        put(b, enc_stp_d(10, 11, 31, 16));
        put(b, enc_stp_d(12, 13, 31, 32));
        put(b, enc_stp_d(14, 15, 31, 48));
        put(b, enc_mrs_fpcr(TMP));
        put(b, enc_str_x(TMP, 31, 64));
        put(b, enc_ldr_x(TMP, CPU, offsetof(struct tuxport_cpu, fpcr)));
        put(b, enc_msr_fpcr(TMP));
        put(b, enc_ldr_x(TMP, CPU, offsetof(struct tuxport_cpu, fpsr)));
        put(b, enc_msr_fpsr(TMP));
        put(b, enc_ldr_x(TMP, CPU, offsetof(struct tuxport_cpu, nzcv)));
        put(b, enc_msr_nzcv(TMP));
        put_add(b, VBASE, CPU, v_off);
        for (unsigned q = 0; q < 32; q += 2)
            put(b, enc_ldp_q(q, q + 1, VBASE, (int)(16 * q)));
    }

    /* General register operands: 31 means ZR and is kept as is */
    for (unsigned i = 0; i < d->n; i++) {
        const struct native_operand *op = &d->gpr[i];
        unsigned r = (insn >> op->pos) & 31;
        host[i] = 2 + 2 * i;
        if (r == 31)
            continue;
        out = (out & ~(31u << op->pos)) | host[i] << op->pos;
        if (!op->load)
            continue;
        put(b, enc_ldr_x(host[i], CPU, x_off(r)));
        if (op->pair) {
            if (r + 1 < 31)
                put(b, enc_ldr_x(host[i] + 1, CPU, x_off(r + 1)));
            else
                put(b, enc_movz(host[i] + 1, 0, 0));
        }
    }
    if (d->addr_pos != NATIVE_NO_ADDR)
        out = (out & ~(31u << d->addr_pos)) | (uint32_t)ADDR << d->addr_pos;

    put(b, out);

    for (unsigned i = 0; i < d->n; i++) {
        const struct native_operand *op = &d->gpr[i];
        unsigned r = (insn >> op->pos) & 31;
        if (r == 31 || !op->store)
            continue;
        put(b, enc_str_x(host[i], CPU, x_off(r)));
        if (op->pair && r + 1 < 31)
            put(b, enc_str_x(host[i] + 1, CPU, x_off(r + 1)));
    }

    if (d->vectors == NATIVE_FULL_FP) {
        for (unsigned q = 0; q < 32; q += 2)
            put(b, enc_stp_q(q, q + 1, VBASE, (int)(16 * q)));
        put(b, enc_mrs_fpsr(TMP));
        put(b, enc_str_x(TMP, CPU, offsetof(struct tuxport_cpu, fpsr)));
        put(b, enc_mrs_nzcv(TMP));
        put(b, enc_str_x(TMP, CPU, offsetof(struct tuxport_cpu, nzcv)));
        put(b, enc_ldr_x(TMP, 31, 64));
        put(b, enc_msr_fpcr(TMP));
        put(b, enc_ldp_d(14, 15, 31, 48));
        put(b, enc_ldp_d(12, 13, 31, 32));
        put(b, enc_ldp_d(10, 11, 31, 16));
        put(b, enc_ldp_d_post(8, 9, 31, 80));
    }
    put(b, enc_ret());
}

static size_t slot_index(uint32_t insn)
{
    return (size_t)((insn * UINT32_C(0x9e3779b1)) >> (32 - TABLE_BITS));
}

/* Called with the lock held: looks up a generated stub */
static native_fn lookup(uint32_t insn)
{
    size_t i = slot_index(insn);
    while (table[i].fn && table[i].insn != insn)
        i = (i + 1) & (TABLE_SIZE - 1);
    return table[i].fn;
}

/* Called with the write lock held: generates a stub, flushing everything first when the cache is
 * full */
static void insert(uint32_t insn, const struct native_desc *desc)
{
    if (table_count >= TABLE_SIZE / 2 || used_words + MAX_STUB_WORDS > CACHE_BYTES / 4) {
        memset(table, 0, sizeof(table));
        table_count = 0;
        used_words = 0;
    }
    size_t i = slot_index(insn);
    while (table[i].fn)
        i = (i + 1) & (TABLE_SIZE - 1);
    uint32_t *code = cache + used_words;
    struct buf b = {(uint32_t *)((char *)code + rw_delta), 0}; /* stubs have no relative branches */
    generate(&b, insn, desc);
    if (b.n > MAX_STUB_WORDS)
        tuxport_guard_fail(); /* the upper-bound estimate was wrong: an engine bug */
    sync_code((void *)code, (void *)(code + b.n), rw_delta);
    used_words += b.n;
    table[i] = (struct slot){insn, (native_fn)(uintptr_t)code};
    table_count++;
}

bool tuxport_native_exec(uint32_t insn, const struct native_desc *desc, struct tuxport_cpu *cpu,
                        void *host_addr)
{
    if (!tuxport_native_available())
        return false;
    /* Stubs access memory at host addresses: if an address is wrong (an engine bug), the host
     * fault guard must release this lock */
    for (;;) {
        pthread_rwlock_rdlock(&lock);
        tuxport_guard_hold(&lock, true);
        native_fn fn = lookup(insn);
        if (fn) {
            fn(cpu, host_addr);
            tuxport_guard_drop();
            pthread_rwlock_unlock(&lock);
            return true;
        }
        tuxport_guard_drop();
        pthread_rwlock_unlock(&lock);
        /* Look up again after switching to the write lock: another thread may have generated the
         * same stub */
        pthread_rwlock_wrlock(&lock);
        tuxport_guard_hold(&lock, true);
        if (!lookup(insn))
            insert(insn, desc);
        tuxport_guard_drop();
        pthread_rwlock_unlock(&lock);
    }
}

#else /* !NATIVE_SUPPORTED */

bool tuxport_native_available(void)
{
    return false;
}

bool tuxport_native_exec(uint32_t insn, const struct native_desc *desc, struct tuxport_cpu *cpu,
                        void *host_addr)
{
    (void)insn;
    (void)desc;
    (void)cpu;
    (void)host_addr;
    return false;
}

#endif
