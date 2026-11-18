/* CPU lifecycle, the software TLB and the main loop of the Tier 0 interpreter. */
#define _DEFAULT_SOURCE /* needed for MAP_ANONYMOUS under -std=c17 */
#include <pthread.h>
#include <stdbool.h>
#include <stddef.h>
#include <sys/mman.h>

#include "internal.h"

/*
 * Use mmap directly: the malloc implementation may allocate from the heap and zero byte by byte
 * (glibc raises the mmap threshold after a block of the same size has been freed), and then every
 * thread's TLB would occupy memory in full
 */
/*
 * Keep a few freed cpus for the next thread: a shell needs one for every process it starts, and
 * mapping a new region, faulting in the TLB's pages one by one and tearing it down at exit costs
 * much more than tuxport_cpu_init clearing only the filled entries
 */
#define SPARE_CPUS 8
static struct {
    pthread_mutex_t lock;
    struct tuxport_cpu *cpus[SPARE_CPUS];
    unsigned n;
} spare = {.lock = PTHREAD_MUTEX_INITIALIZER};

struct tuxport_cpu *tuxport_cpu_new(struct tuxport_mmu *mmu)
{
    pthread_mutex_lock(&spare.lock);
    void *p = spare.n ? spare.cpus[--spare.n] : NULL;
    pthread_mutex_unlock(&spare.lock);
    if (!p) {
        p = mmap(NULL, sizeof(struct tuxport_cpu), PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1,
                 0);
        if (p == MAP_FAILED)
            return NULL;
    }
    tuxport_cpu_init(p, mmu);
    return p;
}

void tuxport_cpu_free(struct tuxport_cpu *cpu)
{
    if (!cpu)
        return;
    pthread_mutex_lock(&spare.lock);
    bool keep = spare.n < SPARE_CPUS;
    if (keep)
        spare.cpus[spare.n++] = cpu;
    pthread_mutex_unlock(&spare.lock);
    if (!keep)
        munmap(cpu, sizeof(*cpu));
}

void tuxport_cpu_init(struct tuxport_cpu *cpu, struct tuxport_mmu *mmu)
{
    /* Clear only the filled entries of the data TLB: a newly allocated cpu is zeroed memory, and
     * untouched pages need not be written */
    memset(cpu, 0, offsetof(struct tuxport_cpu, tlb));
    memset(cpu->v, 0, sizeof(cpu->v));
    cpu->nbrk = cpu->nwatch = 0;
    cpu->dit = 0;
    cpu->mmu = mmu;
    tuxport_cpu_tlb_flush(cpu);
}

/* The data TLB has 4096 entries; clearing all of it each time is too expensive when a process
 * mmaps/munmaps often, so only the filled entries are cleared */
void tuxport_cpu_tlb_flush(struct tuxport_cpu *cpu)
{
    for (unsigned w = 0; w < TUXPORT_TLB_ENTRIES / 64; w++) {
        for (uint64_t bits = cpu->tlb_used[w]; bits; bits &= bits - 1) {
            unsigned i = w * 64 + (unsigned)__builtin_ctzll(bits);
            cpu->tlb[i] = (struct tuxport_tlb_entry){TLB_INVALID, 0, TLB_INVALID, 0};
        }
        cpu->tlb_used[w] = 0;
    }
    memset(cpu->itlb, 0, sizeof(cpu->itlb));
    _Static_assert(TLB_INVALID == 0, "a zeroed entry is an invalid one");
}

/* Kinds of the watchpoints on page vpn (TUXPORT_WATCH_*) */
static uint32_t watched_page(const struct tuxport_cpu *cpu, uint64_t vpn)
{
    uint32_t type = 0;
    for (unsigned i = 0; i < cpu->nwatch; i++) {
        const struct tuxport_watch *w = &cpu->watch[i];
        if (w->lo >> TUXPORT_PAGE_SHIFT <= vpn && (w->hi - 1) >> TUXPORT_PAGE_SHIFT >= vpn)
            type |= w->type;
    }
    return type;
}

void *tuxport_tlb_fill(struct tuxport_cpu *cpu, uint64_t va, uint32_t access)
{
    uintptr_t pte = tuxport_mmu_lookup_pte(cpu->mmu, va);
    if (!pte)
        return NULL;
    uint32_t prot = (uint32_t)(pte & PTE_PROT_MASK);
    uint32_t need = access == TUXPORT_ACCESS_READ    ? TUXPORT_PROT_READ
                    : access == TUXPORT_ACCESS_WRITE ? TUXPORT_PROT_WRITE
                                                    : TUXPORT_PROT_EXEC;
    if (!(prot & need))
        return NULL;

    uint64_t vpn = va >> TUXPORT_PAGE_SHIFT;
    uintptr_t host_page = pte & ~(uintptr_t)PAGE_MASK;
    uint64_t addend = (uint64_t)host_page - (vpn << TUXPORT_PAGE_SHIFT);
    if (access == TUXPORT_ACCESS_EXEC) {
        cpu->itlb[vpn & (TUXPORT_ITLB_ENTRIES - 1)] = (struct tuxport_itlb_entry){~vpn, addend};
    } else {
        /*
         * Pages with watchpoints are not filled in: Tier 1's access misses and falls back to
         * Tier 0, which checks watchpoints (Tier 0 checks them whether or not it hits). The read
         * half only looks at read watchpoints; the write half is not filled when the page has any
         * watchpoint, because Tier 1's atomic read-modify-writes and exclusive accesses also use
         * the write half
         */
        uint32_t watched = UNLIKELY(cpu->nwatch) ? watched_page(cpu, vpn) : 0;
        unsigned idx = (unsigned)(vpn & (TUXPORT_TLB_ENTRIES - 1));
        cpu->tlb_used[idx / 64] |= UINT64_C(1) << (idx % 64);
        uint32_t rw = TUXPORT_PROT_READ | TUXPORT_PROT_WRITE;
        cpu->tlb[idx] = (struct tuxport_tlb_entry){
            (prot & TUXPORT_PROT_READ) && !(watched & TUXPORT_WATCH_LOAD) ? ~vpn : TLB_INVALID,
            addend,
            (prot & rw) == rw && !watched ? ~vpn : TLB_INVALID,
            addend,
        };
    }
    return (void *)(host_page + (va & PAGE_MASK));
}

static inline enum tuxport_exit fetch_fault(struct tuxport_cpu *cpu, uint64_t pc, uint32_t kind)
{
    raise_fault(cpu, pc, TUXPORT_ACCESS_EXEC, kind);
    return TUXPORT_EXIT_FAULT;
}

static inline enum tuxport_exit exec_at(struct tuxport_cpu *cpu, const void *host)
{
    uint32_t insn;
    memcpy(&insn, host, sizeof(insn));
    return tuxport_interp_exec(cpu, insn);
}

static enum tuxport_exit cpu_run(void *ctx, struct tuxport_cpu *cpu)
{
    (void)ctx;
    /* A kick ends this run before the kernel can reclaim the page. Recheck execute permission
     * on the next run, even if it starts at the same PC. */
    uint64_t last_page = UINT64_MAX, addend = 0;
    for (;;) {
        /* Check quickly with a plain load first, and clear with an atomic exchange only when there
         * really is a request, so a concurrently arriving kick is not swallowed */
        if (UNLIKELY(__atomic_load_n(&cpu->pending, __ATOMIC_RELAXED)) &&
            __atomic_exchange_n(&cpu->pending, 0, __ATOMIC_ACQUIRE))
            return TUXPORT_EXIT_KICK;
        uint64_t pc = cpu->pc, page = pc >> TUXPORT_PAGE_SHIFT;
        enum tuxport_exit r;
        if (UNLIKELY((pc & 3) || cpu->nbrk)) {
            r = tuxport_interp_step(cpu);
        } else {
            if (UNLIKELY(last_page != page)) {
                void *host = tlb_lookup(cpu, pc, TUXPORT_ACCESS_EXEC);
                if (!host)
                    return fetch_fault(cpu, pc, TUXPORT_FAULT_PAGE);
                last_page = page;
                addend = (uintptr_t)host - pc;
            }
            r = exec_at(cpu, (const void *)(uintptr_t)(pc + addend));
        }
        if (r != TUXPORT_EXIT_NONE)
            return r;
    }
}

enum tuxport_exit tuxport_cpu_run(struct tuxport_cpu *cpu)
{
    tuxport_sync_dit(cpu);
    return tuxport_guard_run(cpu_run, NULL, cpu);
}

/* The host thread's current PSTATE.DIT: -1 is unknown (a new thread inherits it from the thread
 * that created it) */
static __thread int host_dit = -1;

/*
 * Make the host thread's DIT match the guest's (one host thread runs only one guest thread): this
 * way the data-processing instructions Tier 1 executes verbatim also have data-independent timing.
 * The guest's DIT also changes outside MSR (sigreturn, ptrace, execve), so it is compared on every
 * entry into guest code. When the host has no DIT it is not advertised and the guest's DIT is
 * always 0. The probe cpu that Tier 1 uses during translation to ask Tier 0 whether it recognizes
 * an instruction has no mmu, and the MSR DIT it executes does not touch the host.
 */
void tuxport_sync_dit(struct tuxport_cpu *cpu)
{
#ifdef __aarch64__
    int want = (int)cpu->dit;
    if (LIKELY(want == host_dit) || !cpu->mmu || !tuxport_has(TUXPORT_HWCAP_DIT))
        return;
    if (want)
        __asm__ volatile(".inst 0xd503415f" ::: "memory"); /* msr dit, #1 */
    else
        __asm__ volatile(".inst 0xd503405f" ::: "memory"); /* msr dit, #0 */
    host_dit = want;
#else
    (void)cpu;
#endif
}

/* pc hits a hardware breakpoint (only threads with breakpoints set by ptrace have any) */
static bool brk_hit(const struct tuxport_cpu *cpu, uint64_t pc)
{
    for (unsigned i = 0; i < cpu->nbrk; i++)
        if (cpu->brk[i] == pc)
            return true;
    return false;
}

enum tuxport_exit tuxport_interp_step(struct tuxport_cpu *cpu)
{
    uint64_t pc = cpu->pc;
    if (UNLIKELY(pc & 3))
        return fetch_fault(cpu, pc, TUXPORT_FAULT_ALIGN);
    /* As on hardware, before executing this instruction */
    if (UNLIKELY(cpu->nbrk) && brk_hit(cpu, pc))
        return fetch_fault(cpu, pc, TUXPORT_FAULT_BREAK);
    void *p = tlb_lookup(cpu, pc, TUXPORT_ACCESS_EXEC);
    if (UNLIKELY(!p))
        return fetch_fault(cpu, pc, TUXPORT_FAULT_PAGE);
    return exec_at(cpu, p);
}

enum tuxport_exit tuxport_cpu_exec_insn(struct tuxport_cpu *cpu, uint32_t insn)
{
    return tuxport_interp_exec(cpu, insn);
}

static enum tuxport_exit cpu_step(void *ctx, struct tuxport_cpu *cpu)
{
    (void)ctx;
    return tuxport_interp_step(cpu);
}

enum tuxport_exit tuxport_cpu_step(struct tuxport_cpu *cpu)
{
    tuxport_sync_dit(cpu);
    return tuxport_guard_run(cpu_step, NULL, cpu);
}

void tuxport_cpu_kick(struct tuxport_cpu *cpu)
{
    __atomic_store_n(&cpu->pending, 1, __ATOMIC_RELEASE);
}

int tuxport_cpu_layout_check(size_t size, size_t off_x, size_t off_pc, size_t off_tpidr,
                            size_t off_pending, size_t off_mmu, size_t off_jump_cache,
                            size_t off_tlb, size_t off_v)
{
    return size == sizeof(struct tuxport_cpu) && off_tlb == offsetof(struct tuxport_cpu, tlb) &&
           off_v == offsetof(struct tuxport_cpu, v) &&
           off_x == offsetof(struct tuxport_cpu, x) && off_pc == offsetof(struct tuxport_cpu, pc) &&
           off_tpidr == offsetof(struct tuxport_cpu, tpidr_el0) &&
           off_pending == offsetof(struct tuxport_cpu, pending) &&
           off_mmu == offsetof(struct tuxport_cpu, mmu) &&
           off_jump_cache == offsetof(struct tuxport_cpu, dbt_jump_cache);
}
