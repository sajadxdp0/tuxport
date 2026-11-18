/* Helpers shared inside the engine: the soft-MMU fast path and guest memory access. */
#ifndef TUXPORT_ENGINE_INTERNAL_H
#define TUXPORT_ENGINE_INTERNAL_H

#include <stdbool.h>
#include <stdint.h>
#include <string.h>

#include "tuxport/engine.h"

#define TLB_INVALID 0 /* a TLB tag is ~page number, and a page number cannot be all ones */
#define PAGE_MASK (TUXPORT_PAGE_SIZE - 1)

/* Leaf page-table entry = host page address | permission bits; host pages are 4K-aligned, so the
 * low 12 bits are free */
#define PTE_PROT_MASK UINT64_C(0x7)

#define LIKELY(x) __builtin_expect(!!(x), 1)
#define UNLIKELY(x) __builtin_expect(!!(x), 0)

/*
 * Make instruction fetch see new code after it is written, the same way compiler-rt's
 * __clear_cache does on aarch64. __builtin___clear_cache is not used: when Rust links the Android
 * shared library it does not include compiler-rt, and that symbol is not found at load time. When
 * code is written through another address (W^X double mapping), the data cache is cleaned by the
 * written address start + write_delta and the instruction cache is invalidated by the executed
 * address start.
 */
#if defined(__aarch64__) && defined(__linux__)
/* The host's CTR_EL0. Read only once: some kernels make EL0 reads of CTR_EL0 trap into the kernel
 * to work around errata */
static inline uint64_t host_ctr(void)
{
    static uint64_t cached;
    uint64_t ctr = __atomic_load_n(&cached, __ATOMIC_RELAXED);
    if (!ctr) { /* bit 31 is always 1, so 0 means not read yet */
        __asm__ volatile("mrs %0, ctr_el0" : "=r"(ctr));
        __atomic_store_n(&cached, ctr, __ATOMIC_RELAXED);
    }
    return ctr;
}
#endif

static inline void sync_code_ranges(void *const *start, void *const *end, unsigned n,
                                    intptr_t write_delta)
{
#if defined(__aarch64__) && defined(__linux__)
    uint64_t ctr = host_ctr(), addr;
    /* CTR_EL0.IDC: the data cache need not be cleaned to the point of unification */
    if (!((ctr >> 28) & 1)) {
        uint64_t line = UINT64_C(4) << ((ctr >> 16) & 15);
        for (unsigned i = 0; i < n; i++)
            for (addr = (uint64_t)(uintptr_t)start[i] & ~(line - 1); addr < (uint64_t)(uintptr_t)end[i];
                 addr += line)
                __asm__ volatile("dc cvau, %0" ::"r"(addr + (uint64_t)write_delta) : "memory");
    }
    __asm__ volatile("dsb ish" ::: "memory");
    /* CTR_EL0.DIC: the instruction cache need not be invalidated */
    if (!((ctr >> 29) & 1)) {
        uint64_t line = UINT64_C(4) << (ctr & 15);
        for (unsigned i = 0; i < n; i++)
            for (addr = (uint64_t)(uintptr_t)start[i] & ~(line - 1); addr < (uint64_t)(uintptr_t)end[i];
                 addr += line)
                __asm__ volatile("ic ivau, %0" ::"r"(addr) : "memory");
        /* This dsb waits for the invalidation broadcast to complete, which costs much more than
         * the invalidation itself: several code ranges are synced together so it waits only once */
        __asm__ volatile("dsb ish" ::: "memory");
    }
    __asm__ volatile("isb" ::: "memory");
#else
    (void)write_delta;
    for (unsigned i = 0; i < n; i++)
        __builtin___clear_cache((char *)start[i], (char *)end[i]);
#endif
}

/*
 * Only do the cache maintenance, without waiting for it to complete and without context
 * synchronization (sync_code_ranges without the dsb and isb): for places such as rewriting link
 * points, where "the old instruction is still correct and it does not matter if the new one is
 * seen a little later". The maintenance always completes, after which instruction fetch sees the
 * new instruction sooner or later; until then the old instruction still executes.
 */
static inline void publish_code_lazily(void *start, void *end, intptr_t write_delta)
{
#if defined(__aarch64__) && defined(__linux__)
    uint64_t ctr = host_ctr(), addr;
    if (!((ctr >> 28) & 1)) {
        uint64_t line = UINT64_C(4) << ((ctr >> 16) & 15);
        for (addr = (uint64_t)(uintptr_t)start & ~(line - 1); addr < (uint64_t)(uintptr_t)end; addr += line)
            __asm__ volatile("dc cvau, %0" ::"r"(addr + (uint64_t)write_delta) : "memory");
    }
    if (!((ctr >> 29) & 1)) {
        uint64_t line = UINT64_C(4) << (ctr & 15);
        for (addr = (uint64_t)(uintptr_t)start & ~(line - 1); addr < (uint64_t)(uintptr_t)end; addr += line)
            __asm__ volatile("ic ivau, %0" ::"r"(addr) : "memory");
    }
#else
    __builtin___clear_cache((char *)start, (char *)end);
    (void)write_delta;
#endif
}

static inline void sync_code(void *start, void *end, intptr_t write_delta)
{
    sync_code_ranges(&start, &end, 1, write_delta);
}

static inline void sync_icache(void *start, void *end)
{
    sync_code(start, end, 0);
}

/*
 * Allocate memory for generated code (Tier 1's code cache, native instruction stubs), mapped W^X
 * (§12 item 8): the same shared memory is mapped twice; execution uses the returned address and
 * writing uses it plus *rw_delta. If the host does not allow double mapping, fall back to RWX with
 * *rw_delta 0. Returns NULL on failure. Available only on aarch64 Linux.
 */
uint32_t *tuxport_map_code(size_t bytes, intptr_t *rw_delta);
void tuxport_unmap_code(uint32_t *code, size_t bytes, intptr_t rw_delta);

/* soft-MMU page table (mmu.c): Tier 1's TLB refill routine also walks this structure */
#define MMU_LEVELS 4
#define MMU_LEVEL_BITS 9

struct tuxport_mmu {
    uintptr_t *root;
};

/* Return the leaf entry of gva's page, or 0 if it is not mapped */
uintptr_t tuxport_mmu_lookup_pte(const struct tuxport_mmu *mmu, uint64_t gva);

/* TLB miss: look up the page table and fill the TLB; returns NULL on failure */
void *tuxport_tlb_fill(struct tuxport_cpu *cpu, uint64_t va, uint32_t access);

/* Linux enables TBI for EL0: data accesses ignore the top 8 bits of the address */
#define DATA_TAG_SHIFT 56

static inline uint64_t untag(uint64_t va)
{
    return va & ((UINT64_C(1) << DATA_TAG_SHIFT) - 1);
}

/* Compute the candidate address alongside the tag check so the host can load tag and addend
 * together. Only a hit makes *host usable. */
static inline bool tlb_probe(const struct tuxport_cpu *cpu, uint64_t va, uint32_t access, void **host)
{
    uint64_t vpn = va >> TUXPORT_PAGE_SHIFT;
    unsigned idx = (unsigned)(vpn & (TUXPORT_TLB_ENTRIES - 1));
    uint64_t tag, addend;
    if (access == TUXPORT_ACCESS_EXEC) {
        tag = cpu->itlb[vpn & (TUXPORT_ITLB_ENTRIES - 1)].tag;
        addend = cpu->itlb[vpn & (TUXPORT_ITLB_ENTRIES - 1)].addend;
    } else if (access == TUXPORT_ACCESS_READ) {
        tag = cpu->tlb[idx].tag_read;
        addend = cpu->tlb[idx].addend_read;
    } else {
        tag = cpu->tlb[idx].tag_write;
        addend = cpu->tlb[idx].addend_write;
    }
    *host = (void *)(uintptr_t)(va + addend);
    return tag == ~vpn;
}

static inline void *tlb_lookup(struct tuxport_cpu *cpu, uint64_t va, uint32_t access)
{
    void *host;
    if (LIKELY(tlb_probe(cpu, va, access, &host)))
        return host;
    return tuxport_tlb_fill(cpu, va, access);
}

static inline bool raise_fault(struct tuxport_cpu *cpu, uint64_t va, uint32_t access, uint32_t kind)
{
    cpu->fault_addr = va;
    cpu->fault_access = access;
    cpu->fault_kind = kind;
    return false;
}

/*
 * The access [va, va + n) hits a hardware watchpoint (type is a combination of TUXPORT_WATCH_*; a
 * read-modify-write counts as both). Called after the page-table check and before any side
 * effect: as on hardware, translation faults take priority, and on a hit the instruction is not
 * executed.
 */
static inline bool watch_hit(const struct tuxport_cpu *cpu, uint64_t va, uint64_t n, uint32_t type)
{
    if (LIKELY(!cpu->nwatch))
        return false;
    for (unsigned i = 0; i < cpu->nwatch; i++) {
        const struct tuxport_watch *w = &cpu->watch[i];
        if ((w->type & type) && va < w->hi && va + n > w->lo)
            return true;
    }
    return false;
}

/*
 * Read and write guest memory (n ≤ 16). A cross-page access first confirms that both pages are
 * accessible, then copies.
 * On failure, record the fault and return false, without any side effect.
 */
static inline bool mem_read(struct tuxport_cpu *cpu, uint64_t va, void *out, unsigned n)
{
    va = untag(va);
    uint64_t off = va & PAGE_MASK;
    void *p0 = tlb_lookup(cpu, va, TUXPORT_ACCESS_READ);
    if (UNLIKELY(!p0))
        return raise_fault(cpu, va, TUXPORT_ACCESS_READ, TUXPORT_FAULT_PAGE);
    if (LIKELY(off + n <= TUXPORT_PAGE_SIZE)) {
        if (UNLIKELY(watch_hit(cpu, va, n, TUXPORT_WATCH_LOAD)))
            return raise_fault(cpu, va, TUXPORT_ACCESS_READ, TUXPORT_FAULT_WATCH);
        memcpy(out, p0, n);
        return true;
    }
    unsigned first = (unsigned)(TUXPORT_PAGE_SIZE - off);
    void *p1 = tlb_lookup(cpu, va + first, TUXPORT_ACCESS_READ);
    if (UNLIKELY(!p1))
        return raise_fault(cpu, va + first, TUXPORT_ACCESS_READ, TUXPORT_FAULT_PAGE);
    if (UNLIKELY(watch_hit(cpu, va, n, TUXPORT_WATCH_LOAD)))
        return raise_fault(cpu, va, TUXPORT_ACCESS_READ, TUXPORT_FAULT_WATCH);
    memcpy(out, p0, first);
    memcpy((uint8_t *)out + first, p1, n - first);
    return true;
}

static inline bool mem_write(struct tuxport_cpu *cpu, uint64_t va, const void *in, unsigned n)
{
    va = untag(va);
    uint64_t off = va & PAGE_MASK;
    void *p0 = tlb_lookup(cpu, va, TUXPORT_ACCESS_WRITE);
    if (UNLIKELY(!p0))
        return raise_fault(cpu, va, TUXPORT_ACCESS_WRITE, TUXPORT_FAULT_PAGE);
    if (LIKELY(off + n <= TUXPORT_PAGE_SIZE)) {
        if (UNLIKELY(watch_hit(cpu, va, n, TUXPORT_WATCH_STORE)))
            return raise_fault(cpu, va, TUXPORT_ACCESS_WRITE, TUXPORT_FAULT_WATCH);
        memcpy(p0, in, n);
        return true;
    }
    unsigned first = (unsigned)(TUXPORT_PAGE_SIZE - off);
    void *p1 = tlb_lookup(cpu, va + first, TUXPORT_ACCESS_WRITE);
    if (UNLIKELY(!p1))
        return raise_fault(cpu, va + first, TUXPORT_ACCESS_WRITE, TUXPORT_FAULT_PAGE);
    if (UNLIKELY(watch_hit(cpu, va, n, TUXPORT_WATCH_STORE)))
        return raise_fault(cpu, va, TUXPORT_ACCESS_WRITE, TUXPORT_FAULT_WATCH);
    memcpy(p0, in, first);
    memcpy(p1, (const uint8_t *)in + first, n - first);
    return true;
}

/* ---- Host fault guard (guard.c) ---- */

/*
 * Run fn(ctx, cpu) under the guard: meanwhile a host SIGSEGV, SIGBUS or SIGILL makes it return
 * TUXPORT_EXIT_HOST_FAULT (effective only after tuxport_fault_guard_install()). fn must not call
 * malloc, and every lock it takes must be registered.
 */
enum tuxport_exit tuxport_guard_run(enum tuxport_exit (*fn)(void *, struct tuxport_cpu *), void *ctx,
                                  struct tuxport_cpu *cpu);

/* Just took a lock (a pthread_rwlock_t when rw is true, otherwise a pthread_mutex_t): released by
 * tuxport_guard_run() if a fault occurs under the guard. Holds and drops nest; beyond four held at
 * once the inner ones are not released after a fault */
void tuxport_guard_hold(void *lock, bool rw);

/* The same for locks that `release(lock)` releases (several taken together) */
void tuxport_guard_hold_fn(void *lock, void (*release)(void *));

/* About to release the most recently registered lock */
void tuxport_guard_drop(void);

/* An internal check failed (an engine bug found without a host fault): under an armed guard the
 * run ends as after a host fault, recorded as SIGABRT at the caller, so only the guest process is
 * ended; otherwise abort() as before. Must not be called while holding an unregistered lock */
_Noreturn void tuxport_guard_fail(void);

/* ---- CPU features (features.c) ---- */

/* Whether the guest feature set contains all bits in hwcap */
bool tuxport_has(uint64_t hwcap);

/* Same, for AT_HWCAP2 bits */
bool tuxport_has2(uint64_t hwcap2);

/* FPCR bits writable by MSR: AHP, DN, FZ, RMode and, with FP16, FZ16. */
static inline uint64_t fpcr_write_mask(void)
{
    return UINT64_C(0x07c00000) | (tuxport_has(TUXPORT_HWCAP_FPHP) ? UINT64_C(1) << 19 : 0);
}

/* Make the host thread's PSTATE.DIT follow the guest's (cpu.c) */
void tuxport_sync_dit(struct tuxport_cpu *cpu);

/* The value EL0 reads from the ID register space; key is bits 20:5 of the MRS encoding; returns
 * false outside that space */
bool tuxport_id_register(unsigned key, uint64_t *value);

/* ---- Native instruction stubs (native.c) ---- */

/*
 * A native stub executes one guest instruction on the host CPU (the engine map): general-register
 * fields are replaced with host temporaries, loaded from the cpu struct before execution and
 * written back after it. Vector operations specify the register and flag state they need.
 */
struct native_operand {
    uint8_t pos;   /* start bit of the register field */
    uint8_t pair;  /* the field names a pair of adjacent registers (CASP) */
    uint8_t load;  /* load the guest value before execution */
    uint8_t store; /* write back after execution */
};

enum native_vectors {
    NATIVE_NO_VECTORS,
    NATIVE_FULL_FP,
    NATIVE_VN_VM,     /* Vd = op(Vn, Vm), no flag effects */
    NATIVE_VD_VN_VM,  /* Vd = op(Vd, Vn, Vm), no flag effects */
};

struct native_desc {
    uint8_t vectors;  /* enum native_vectors */
    uint8_t addr_pos; /* start bit of the address register field, NATIVE_NO_ADDR if none */
    uint8_t n;
    struct native_operand gpr[3];
};

#define NATIVE_NO_ADDR 0xff

typedef void (*native_fn)(struct tuxport_cpu *cpu, void *host_addr);

bool tuxport_native_available(void);

/*
 * Get (generating it if needed) the native stub for insn and execute it; returns false when
 * native stubs are unavailable. desc must always be the same for the same insn. May be called from
 * several threads at once.
 */
bool tuxport_native_exec(uint32_t insn, const struct native_desc *desc, struct tuxport_cpu *cpu,
                        void *host_addr);

/* ---- SIMD&FP (simd.c) and atomic memory access (atomic.c) ---- */

/* Whether a SIMD&FP data-processing instruction is in the whitelist; if so, fill in the native
 * stub's description */
bool tuxport_simd_dp_decode(uint32_t insn, struct native_desc *desc);

/* SIMD&FP memory access (load/store encodings with bit 26 set) */
enum tuxport_exit tuxport_simd_ldst(struct tuxport_cpu *cpu, uint32_t insn);

/* Exclusive accesses, acquire/release and LSE atomics; returns UNDEF when the encoding is not in
 * these classes */
enum tuxport_exit tuxport_atomic_exec(struct tuxport_cpu *cpu, uint32_t insn);

/* Execute an instruction that has already been fetched */
enum tuxport_exit tuxport_interp_exec(struct tuxport_cpu *cpu, uint32_t insn);

/* Fetch from cpu->pc and execute one instruction (Tier 0's single step) */
enum tuxport_exit tuxport_interp_step(struct tuxport_cpu *cpu);

#endif /* TUXPORT_ENGINE_INTERNAL_H */
