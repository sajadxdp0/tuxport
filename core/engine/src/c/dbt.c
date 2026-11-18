/*
 * Tier 1: a same-ISA pass-through DBT (the engine map: `.agents/maps/engine.md`).
 *
 * Register convention while translated code runs (this is the default set; Go programs use
 * another set, see struct regs):
 *   - guest X0–X15, X19–X25, X29 and X30 live in the same-numbered host registers, and guest SP
 *     lives in host X26;
 *   - host X28 points to struct tuxport_cpu; host X16, X17 and X27 are temporaries;
 *   - host X18 is the platform register (on Android, the shadow call stack pointer bionic sets up
 *     for each thread), and translated code does not touch it: the host's signal handlers may use
 *     it at any time;
 *   - guest X16, X17, X18, X26, X27 and X28 are kept in the cpu struct ("stolen registers"), and
 *     instructions that reference them are rewritten before they run. SP appears in 17% of
 *     instructions (function prologues and epilogues), and X26 is the least used callee-saved
 *     register in compiler-generated code (less than a tenth of SP in gcc's cc1), so host X26 is
 *     given to SP. The host registers are a convention only among pieces of translated code and
 *     have nothing to do with the host calling convention: translated code calls no host
 *     functions, and the enter routine saves all callee-saved registers;
 *   - guest NZCV is host NZCV, so inserted code never changes the condition flags;
 *   - guest V0–V31, FPCR and FPSR are the host's V0–V31, FPCR and FPSR, and SIMD&FP
 *     data-processing instructions run verbatim.
 *
 * Memory-access instructions are rewritten into the inline TLB fast path; a TLB miss or a
 * cross-page access enters the refill routine, and a failed refill (unmapped, no permission and so
 * on) or a misaligned SP exits to the dispatcher, where Tier 0 executes this one instruction (at
 * that point the instruction has had no side effect at all). Direct branches jump directly through
 * block links; indirect branches first look up the jump cache. Function calls push the shadow
 * return stack, and RET looks at the top of the stack first (emit_call). Instructions Tier 0 does
 * not recognize are likewise handed to Tier 0, which keeps the two tiers' whitelists exactly the
 * same (the engine map).
 *
 * Translations never invalidate themselves; see tuxport_dbt_invalidate(). The code cache is
 * double-mapped W^X.
 */
#define _GNU_SOURCE /* needed for madvise, mremap and their flags under -std=c17 */
#include <stddef.h>
#include <stdlib.h>

#include "emit.h"
#include "internal.h"

#if defined(__aarch64__) && defined(__linux__)
#define DBT_SUPPORTED 1
#endif

#ifdef DBT_SUPPORTED

#include <pthread.h>
#include <stdio.h>
#include <sys/mman.h>
#include <unistd.h>

#define BIT(x, n) ((unsigned)(((x) >> (n)) & 1u))
#define FIELD(x, hi, lo) ((unsigned)(((x) >> (lo)) & ((1u << ((hi) - (lo) + 1)) - 1)))

/* Shared by all threads of an address space; JIT workloads need room for more than a hundred
 * thousand live blocks. A block's jumps to the routines at the start of the cache and the links
 * between blocks are each one b, whose range is ±128 MB, so it cannot be larger */
#define CODE_CACHE_BYTES (UINT64_C(128) << 20)
#define MAX_BLOCK_INSNS 64
_Static_assert(CODE_CACHE_BYTES <= (UINT64_C(128) << 20), "the offset range of b");
/* One guest instruction expands to at most about 40 host instructions, plus exit stubs */
#define MAX_BLOCK_WORDS (MAX_BLOCK_INSNS * 64)
#define MAX_STUBS (MAX_BLOCK_INSNS * 2 + 4)
#define MAX_FIXUPS (MAX_BLOCK_INSNS * 4 + 4)
/* The translation table starts at 2^12 entries, doubles when three quarters full, and has at most
 * 2^19 entries: the table is spread by hash, so if a short-lived process's few hundred blocks were
 * put in the largest table, every block would touch a new page of memory. All threads of an
 * address space share one table, and JIT workloads (the JVM) have more than a hundred thousand
 * live blocks */
#define TABLE_MIN_BITS 12
#define TABLE_BITS 19
#define TABLE_SIZE (1u << TABLE_BITS)
#define TABLE_LIMIT (TABLE_SIZE / 4 * 3)
#define JC_BITS 12
#define JC_SIZE (1u << JC_BITS)
/* Key of a tombstone in the translation table: misaligned, and lookups exclude misaligned pcs
 * first (table_find_for, emit_lookup), so it is never found. Empty jump-cache entries use jc_empty
 * instead */
#define JC_EMPTY UINT64_C(1)
/* Stack frame of the enter routine: x19–x30, host FPCR, d8–d15 */
#define FRAME_BYTES 176

/*
 * Host registers: CTX points to struct tuxport_cpu, T0, T1 and T2 are temporaries, and SP_HOST
 * holds the guest SP. Which set an instance uses is chosen at creation (tuxport_dbt_new), and
 * routines, translations and templates are all generated for it, so these names can only be used
 * where there is an `x` (struct xlate *). The same-numbered guest registers and X18 are the stolen
 * registers (is_stolen).
 *
 * The Go compiler uses X28 as the current goroutine (g), X27 as the assembler's temporary and X26
 * as the closure context, and the stack check in function prologues uses X16: in a native
 * `go build`, compile spends 15.5% of its execution time on instructions that use the default
 * set's stolen registers (5.2% for gcc's cc1), while X19–X25 together account for less than 1.5%
 * (register allocation starts from the low numbers). So Go programs steal X21–X25 instead (the
 * engine map).
 */
struct regs {
    unsigned ctx, t0, t1, t2, sp;
};
static const struct regs reg_sets[] = {
    [TUXPORT_DBT_REGS_DEFAULT] = {.ctx = 28, .t0 = 16, .t1 = 17, .t2 = 27, .sp = 26},
    [TUXPORT_DBT_REGS_GO] = {.ctx = 25, .t0 = 21, .t1 = 22, .t2 = 23, .sp = 24},
};
#define CTX (x->d->r->ctx)
#define T0 (x->d->r->t0)
#define T1 (x->d->r->t1)
#define T2 (x->d->r->t2)
#define SP_HOST (x->d->r->sp)
#define REG_SP 32 /* pseudo register number: guest SP */

/* Exit reasons (returning from translated code to the dispatcher). R_CHAIN_FWD is a forward
 * direct branch, which when linked skips the kick check at the target block's entry: block start
 * addresses strictly increase along forward edges, so every loop has at least one edge that passes
 * a check. R_UNGROUP is a group of accesses sharing one check that failed it (see plan_group);
 * as with R_INTERP, Tier 0 executes this one instruction */
enum { R_CODE, R_CHAIN, R_CHAIN_FWD, R_LOOKUP, R_INTERP, R_SVC, R_KICK, R_UNGROUP };

/* The other kind of stub besides exit stubs: on a TLB miss it enters the refill routine and exits
 * only if the refill fails (R_INTERP or R_UNGROUP) */
#define STUB_REFILL (R_UNGROUP + 1)

/* The most words one stub takes: a link stub takes 5 words (6 when the pc does not fit in 48
 * bits), an exit stub 4 (5), a refill stub 5 */
#define MAX_STUB_WORDS 6

/* In an exit stub's data word the pc takes the low 48 bits, and the high 16 bits are the exit
 * reason (emit_link_stub) */
#define PACKED_PC_BITS 48

/* The info word in a refill stub (emit_refill_stub): the low 12 bits are the byte count minus one
 * (a group of accesses sharing a check covers at most GROUP_SPAN plus one more access), the next
 * 3 bits are the exit reason if the refill fails, and the high 17 bits are the distance in words
 * of the access itself from the info word (signed) */
#define REFILL_REASON_SHIFT 12
#define REFILL_RESUME_SHIFT 15
_Static_assert(MAX_BLOCK_WORDS < 1u << (31 - REFILL_RESUME_SHIFT), "the access's distance in words from the info word must fit in it");

/* Words taken by the kick check at a block's entry */
#define ENTRY_CHECK_WORDS 2

/* The low 3 bits of an exit reason are R_*; a link stub carries in the high bits the distance in
 * words from the conditional branch that jumps to it to the stub (0 means none), and link stubs of
 * shared blocks (TAG_SHIFT) also carry LINK_SHARED: they can only link to shared blocks */
#define REASON_BITS 3
#define REASON_MASK ((1u << REASON_BITS) - 1)
#define BRANCH_MASK (MAX_BLOCK_WORDS - 1)
#define LINK_SHARED (1u << 15)
_Static_assert(R_UNGROUP <= REASON_MASK, "the exit reason goes in the low 3 bits");
_Static_assert((MAX_BLOCK_WORDS & BRANCH_MASK) == 0 && MAX_BLOCK_WORDS << REASON_BITS <= LINK_SHARED,
               "the conditional branch's distance to the stub and LINK_SHARED must fit in a movz immediate");

#define SYSREG_NZCV 0xda10
#define SYSREG_FPCR 0xda20
#define SYSREG_FPSR 0xda21
#define SYSREG_TPIDR_EL0 0xde82
#define SYSREG_DCZID_EL0 0xd807
#define SYSREG_CNTFRQ_EL0 0xdf00
#define SYSREG_CNTVCT_EL0 0xdf02
#define SYSREG_CNTVCTSS_EL0 0xdf06

/* Block size of DC ZVA, matching the DCZID_EL0 that Tier 0 reports */
#define DCZ_BLOCK 64
#define DCZID_VALUE 4

#define TLB_BITS 12
#define TLB_OFF offsetof(struct tuxport_cpu, tlb)
_Static_assert(TUXPORT_TLB_ENTRIES == 1u << TLB_BITS, "the TLB index is fixed at 12 bits in generated code");
_Static_assert(sizeof(struct tuxport_tlb_entry) == 32, "generated code addresses TLB entries as 32 bytes each");
_Static_assert(TLB_OFF % 16 == 0 && TLB_OFF + 16 <= 504, "the TLB must be addressable by ldp's 7-bit scaled immediate");
_Static_assert(offsetof(struct tuxport_cpu, dbt_rs) < TLB_OFF, "dbt_rs is also read and written directly with ldr/str");
_Static_assert(offsetof(struct tuxport_cpu, dbt_jump_cache) < TLB_OFF,
               "fields that generated code reads and writes directly with ldr/str must precede the TLB");

struct slot {
    uint64_t pc;
    uint32_t *code;
};

/*
 * Translation table: guest pc → translation, open addressing. The dispatcher looks it up without
 * the lock (table_lookup), so: insertion only uses empty slots (pc written first, then code), and
 * invalidation only changes the pc into a tombstone — a thread that read the old code enters the
 * invalidated block's entry, which jumps to the forwarding stub and exits with the original pc;
 * tombstones are not reused and are removed when the table grows or is rebuilt at the same size.
 * After the table is replaced, the old one is freed only once the threads looking it up have
 * finished (table_publish).
 */
struct table {
    unsigned bits;
    unsigned shift; /* 64 - bits: the index is the pc's hash shifted right by this many bits */
    uint64_t mask;  /* 2^bits - 1 */
    struct slot slots[];
};
#define GOLDEN UINT64_C(0x9e3779b97f4a7c15)

/*
 * Shadow return stack: each entry is (guest return address, host landing pad); the 64-entry ring
 * buffer is aligned to its own size, so push and pop change only the low RS_SHIFT bits of the top
 * pointer (bfxil, which does not touch the condition flags).
 */
#define RS_SHIFT 10
#define RS_ENTRIES ((1u << RS_SHIFT) / 16)

/*
 * When several address spaces of the same program share one instance (the kernel layer's
 * CodeGroup), blocks in the trusted file mappings registered with the group (tuxport_dbt_share) are
 * valid for every member and go into the translation table by pc; blocks elsewhere (JIT-generated
 * or written code) are valid only for the member that translated them, and their key in the table
 * is pc | the member's tag << TAG_SHIFT. A guest pc has only 48 bits and there are no blocks above
 * that (excluded before the lookup), so the two kinds of key never coincide. Both kinds are hashed
 * by pc, and one linear probe finds both the shared blocks and this member's private ones: with
 * two separate probes, JIT code would take one more cache miss on every jump-cache miss. Shared
 * blocks do not link to private blocks (LINK_SHARED). Tags are allocated by the instance
 * (tuxport_dbt_new_tag); when the instance returns to the pool its private blocks are removed
 * (drop_private) and tags are allocated from the start again. Tag 0 means not in a group, and all
 * blocks are keyed by pc.
 */
#define TAG_SHIFT TUXPORT_VA_BITS
#define TAG_MAX 0xffffu
#define PC_MASK ((UINT64_C(1) << TAG_SHIFT) - 1)

/* When invalidating translations, find the blocks that start in a guest page by that page
 * (tuxport_dbt_invalidate) */
#define PAGE_BUCKET_BITS 14
#define INVALIDATE_MAX_PAGES 256 /* beyond this many pages, look at every block in turn */

struct block {
    uint64_t pc;
    uint32_t *code;  /* NULL once invalidated */
    uint32_t next;   /* previous block in the same bucket (index plus 1), 0 if none */
    uint32_t words;  /* where the guest words at translation time sit in words (for recheck) */
    uint8_t nguest;  /* number of guest instructions */
    /* Parked (revive): the entry jumps to the forwarding stub like an invalidated block's, and the
     * code is still there */
    uint8_t parked;
    uint16_t tag;    /* tag of the member owning a private block; 0 for shared ones (TAG_SHIFT) */
    uint32_t first;  /* the instruction after the entry check, which is rewritten when parked */
};
_Static_assert(MAX_BLOCK_INSNS <= UINT8_MAX, "nguest of struct block");

/* A block's key in the translation table */
static uint64_t tagged(uint64_t pc, unsigned tag)
{
    return pc | (uint64_t)tag << TAG_SHIFT;
}

static uint64_t block_key(const struct block *b)
{
    return tagged(b->pc, b->tag);
}

/* After a group of accesses fails the shared check a second time, the leader's pc is recorded
 * here, and on retranslation neither it nor the accesses after it in the same block lead a group
 * any more; when full, the oldest entries are overwritten first. A first failure is only recorded
 * in strikes (direct-mapped by pc) */
#define NOGROUP_MAX 64
#define STRIKES 64

/* The most words the guest instruction words of all blocks can take (from the block count limit
 * and the per-block instruction limit) */
#define WORDS_LIMIT ((size_t)TABLE_LIMIT * MAX_BLOCK_INSNS)
/* The most blocks kept when returning to the pool: checking is much cheaper than copying templates
 * and linking again (one node startup has about 50,000 blocks); beyond that they are mostly
 * JIT-generated code that will most likely not match next time */
#define KEEP_MAX_BLOCKS 131072

/*
 * The translations of one address space (code cache, translation table, various records), shared
 * by all threads in that address space (tuxport_dbt_attach): multi-threaded programs (the Go
 * compiler, the JVM) used to translate the same code once per thread, doing translation, template
 * copying, linking and instruction-cache syncing several times over, each with its own code cache.
 *
 * Apart from the dispatcher's table lookups, all reads and writes hold lock. A full flush must wait
 * for all threads to leave translated code: the initiating thread kicks the other threads and waits
 * until each has stopped in the dispatcher (parked) or left dbt_run; see flush_world.
 */
struct tuxport_dbt {
    /* Host register assignment (one set in reg_sets); routines and translations are all generated
     * for it */
    const struct regs *r;
    uint32_t *cache;    /* address for execution; every code pointer points here */
    intptr_t rw_delta;  /* writable alias minus execution address; 0 when mapped directly as RWX */
    struct written *written; /* pages written in the writable alias (note_written); NULL for RWX */
    /* The list pool.all linking all instances (live and pooled), walked by
     * tuxport_dbt_unmap_written. Read and written while holding pool.lock; prev_instance is NULL
     * when not in the list */
    struct tuxport_dbt *next_instance, **prev_instance;
    size_t cache_words;
    size_t used_words;
    size_t routine_words;
    uint32_t *enter;
    uint32_t *exit;
    uint32_t *exit_stub; /* entry exit stubs jump to (emit_link_stub), followed directly by exit */
    uint32_t *exit_full; /* same, for pcs that do not fit in 48 bits */
    uint32_t *refill[2]; /* TLB refill routines: read, write */
    uint32_t *lookup;    /* routine that looks up the table on a jump-cache miss (emit_lookup) */
    uint32_t *rs_miss;   /* landing pad of empty shadow return stack entries: looks up guest x30 */
    struct table *table; /* read with __atomic_load_n when not holding the lock */
    uint32_t *table_used; /* slots used, in insertion order, at most TABLE_LIMIT of them */
    size_t table_count;
    size_t table_occupied; /* non-empty slots in the table: live blocks and tombstones */
    size_t table_dead;     /* the tombstones among them */
    struct block *blocks; /* one-to-one with table_used, recorded in translation order */
    size_t blocks_dead;   /* those already invalidated */
    size_t nparked;       /* those parked (revive) */
    uint32_t *words;      /* blocks' guest instruction words at translation, mapped on demand */
    size_t words_used;
    /* Address space changed (taken from the pool after exec or fork); check before the next run */
    bool stale;
    /* The mappings the translations are in, recorded before returning to the pool
     * (tuxport_dbt_set_sources) */
    struct tuxport_dbt_source sources[TUXPORT_DBT_SOURCES];
    unsigned nsources;
    /* Ranges [start, end) the next check need not look at (tuxport_dbt_trust) */
    uint64_t trusted[TUXPORT_DBT_SOURCES][2];
    unsigned ntrusted;
    /* The previous user's file mappings [start, end): blocks in them that are not mapped yet are
     * parked during the check (recheck) */
    uint64_t parkable[TUXPORT_DBT_SOURCES][2];
    unsigned nparkable;
    /* A host fault occurred while running (TUXPORT_EXIT_HOST_FAULT): the tables may be only half
     * updated, so they are no longer used */
    bool poisoned;
    uint32_t *page_head;  /* hash of a guest page → list head in blocks (index plus 1) */
    uint32_t *page_parked; /* same hash: parked count, read lock-free if a lookup fails (revive) */
    /* The group's registered mappings (tuxport_dbt_share), sorted by start and non-overlapping;
     * read without the lock. A replaced old array hangs off the new one's older and is freed when
     * the instance returns to the pool */
    struct shared_ranges *shared;
    unsigned next_tag; /* next tag that can be allocated (tuxport_dbt_new_tag) */
    uint64_t nogroup[NOGROUP_MAX];
    unsigned nogroup_count;
    uint64_t strikes[STRIKES];
    /* Incremented on every flush: invalidates links not yet patched, and threads use it to clear
     * their own jump cache and shadow return stack */
    uint64_t generation;
    size_t high_words; /* the most code-cache words ever used */

    pthread_mutex_t lock;
    pthread_cond_t flushed; /* a full flush has finished (or the instance was poisoned) */
    struct tuxport_dbt_thread *threads; /* attached threads */
    unsigned nthreads;                 /* their number (read atomically) */
    bool flushing;     /* a thread waits for the others to stop for a full flush (atomic read) */
    /* A full flush is wanted: the caller asked for one, or a forwarding stub did not fit when
     * invalidating (read atomically) */
    bool flush_wanted;
    /* Statistics of the lock holder: translations and invalidations are charged to it */
    struct tuxport_dbt_stats *st;
};

/* Registered mappings: [r[i][0], r[i][1]), sorted by start and non-overlapping */
struct shared_ranges {
    struct shared_ranges *older;
    unsigned n;
    uint64_t r[][2];
};

/* Whether pc is in a registered mapping (without the lock) */
static bool in_shared(const struct tuxport_dbt *d, uint64_t pc)
{
    const struct shared_ranges *s = __atomic_load_n(&d->shared, __ATOMIC_ACQUIRE);
    if (!s)
        return false;
    unsigned lo = 0, hi = s->n;
    while (lo < hi) {
        unsigned mid = (lo + hi) / 2;
        if (pc < s->r[mid][0])
            hi = mid;
        else if (pc >= s->r[mid][1])
            lo = mid + 1;
        else
            return true;
    }
    return false;
}

/* The part a thread uses to run in an address space: jump cache, shadow return stack and
 * statistics */
struct tuxport_dbt_thread {
    struct tuxport_dbt *d;
    unsigned tag; /* the group tag of the thread's address space, 0 if not in a group (TAG_SHIFT) */
    struct tuxport_dbt_thread *next; /* d->threads list, read and written holding d->lock */
    struct tuxport_cpu *cpu; /* cpu running in dbt_run or NULL (atomic): a full flush kicks it */
    bool parked;            /* stopped in park for a full flush (accessed holding d->lock) */
    uint64_t generation;    /* the d->generation that jc and rs correspond to */
    struct slot *jc;        /* jump cache for indirect branches, direct-mapped */
    bool jc_dirty;
    uint64_t (*rs)[2]; /* shadow return stack */
    struct tuxport_dbt_stats stats;
    /* Workspace for translating without the lock (prepare): the translation in progress, the
     * generated code and the result */
    struct xlate *xlate;
    uint32_t *scratch;
    struct tuxport_cpu *probe;
    uint64_t *defined; /* results of tier0_defined, per instruction word (DEFINED_CACHE entries) */
    struct prep *prep;
};

/* ---- Emitter ---- */

enum fixup_kind { FIX_IMM19, FIX_IMM14 };

struct stub {
    unsigned reason; /* R_* or STUB_REFILL; for R_LOOKUP the target pc is already in T0 */
    uint64_t pc;
    uint32_t *addr;
    /* Refill stubs: where execution continues after a successful refill (the access after the TLB
     * check), the byte count and direction of the access, and the exit reason on failure */
    uint32_t *resume;
    unsigned bytes;
    bool write;
    unsigned fail;
    /* The number of branches jumping to this stub and the position of one of them (filled in when
     * emitting stubs) */
    unsigned sites;
    const uint32_t *branch;
};

struct fixup {
    uint32_t *site;
    enum fixup_kind kind;
    unsigned stub;
};

/*
 * One direct-branch exit of a block (a link stub): the stub's offset in words from the block
 * start, the offset in words from the block start of the only conditional branch that jumps to it
 * (NO_BRANCH if none), and the target pc. If the target is already translated when the block is
 * built, it is linked right away (link_block), saving the exit to the dispatcher the first time
 * execution gets here and a separate instruction-cache sync (the dsb after ic ivau waits for the
 * invalidation broadcast to complete, about 0.1 microseconds each time)
 */
struct link {
    uint32_t stub; /* LINK_FWD: forward, skips the kick check at the target block's entry */
    uint32_t branch;
    uint64_t pc;
};
#define LINK_FWD (UINT32_C(1) << 31)
#define NO_BRANCH UINT32_MAX

/* A b to a routine: its offset must be recomputed for the new position when a translation
 * template is copied */
enum { TO_EXIT, TO_REFILL_READ, TO_REFILL_WRITE, TO_LOOKUP, TO_EXIT_FULL };

/* The most bytes and the most accesses one group of accesses sharing a check may cover, and the
 * most groups at once (plan_group) */
#define GROUP_SPAN 512
#define GROUP_MAX 16
#define GROUP_SLOTS 4
_Static_assert(sizeof(((struct tuxport_cpu *)0)->dbt_groups) == GROUP_SLOTS * 8, "one slot per group");
#define MAX_RELOCS (MAX_STUBS * 2 + 4)

struct xlate {
    struct tuxport_dbt *d;
    uint32_t *buf;
    /* Write address minus buf: 0 when translating into the thread's scratch area, rw_delta when
     * writing the code cache directly */
    intptr_t wdelta;
    struct tuxport_dbt_stats *st; /* whom the translation statistics are charged to */
    struct tuxport_cpu *probe;    /* used to ask Tier 0 whether it recognizes an instruction */
    uint64_t *defined;           /* its answers, cached per instruction word */
    size_t n;
    struct stub stubs[MAX_STUBS];
    unsigned nstubs;
    struct fixup fixups[MAX_FIXUPS];
    unsigned nfixups;
    uint32_t relocs[MAX_RELOCS]; /* index << 3 | TO_* */
    unsigned nrelocs;
    /* The block being translated: start pc, guest instruction words, index of the current
     * instruction, and the most instructions the block may have */
    uint64_t pc;
    const uint32_t *guest;
    unsigned idx, limit;
    /* This block goes into the translation table by pc (TAG_SHIFT), and its link stubs carry
     * LINK_SHARED */
    bool shared;
    /* SP's alignment was already checked in this block, and SP has since changed only by
     * multiples of 16 */
    bool sp_aligned;
    /* General registers in this block whose values are known at translation time (set by
     * ADR/ADRP, plus immediates added to them); bit r stands for Xr, and the values are in kval
     * (konst_update) */
    uint32_t konst;
    uint64_t kval[31];
    /* Groups of accesses sharing a check (plan_group): the members not translated yet. A group's
     * host base is in T2, or spilled to the cpu's dbt_groups[group index] */
    struct group {
        unsigned n, next;
        unsigned idx[GROUP_MAX];
        int32_t off[GROUP_MAX]; /* relative to the base before the leader executes */
        bool reload[GROUP_MAX]; /* T2 used earlier: get the host base back from the slot first */
        int32_t lo;             /* range the whole group covers, also relative to that base */
        unsigned bytes;
        bool write;
        bool spill; /* some member reloads: the leader stores the host base in the slot */
    } groups[GROUP_SLOTS];
    int t2_group; /* which group's host base T2 holds now, -1 for none */
};

/* Write address of code (the writable alias of the W^X double mapping) */
static uint32_t *rw(const struct tuxport_dbt *d, const uint32_t *p)
{
    return (uint32_t *)((uintptr_t)p + (uintptr_t)d->rw_delta);
}

/* Host page size (4K, 16K): madvise ranges must be aligned to it */
static size_t host_page_size(void)
{
    static size_t cached;
    size_t page = __atomic_load_n(&cached, __ATOMIC_RELAXED);
    if (!page) {
        page = (size_t)sysconf(_SC_PAGESIZE);
        __atomic_store_n(&cached, page, __ATOMIC_RELAXED);
    }
    return page;
}

/*
 * The host's RSS counts pages per mapping: a code-cache page touched through both the write and
 * the execute view counts twice, so the code cache is counted double in RssShmem, in the RSS the
 * LMK looks at and in the usage the memory limit measures. So the pages written in the writable
 * view are recorded, and the kernel layer's timer thread drops their page-table entries once a
 * second (tuxport_dbt_unmap_written): MADV_DONTNEED on shared memory only removes the mapping and
 * keeps the content, and the next write faults once more. Dropping is by time, not by amount: JIT
 * workloads patch links and invalidate blocks in thousands of different pages per second, and
 * when dropping by amount (once every 256 KB accumulated) those pages faulted again and again and
 * Maven was 3% slower. Threads patching links without the lock also write, so the bitmap and the
 * count are atomic; recording one page too many or too few only affects when dropping happens.
 */
struct written {
    unsigned pages; /* number of pages set in bits (atomic) */
    bool dropping;  /* a thread is dropping them (atomic) */
    /* Host pages written in the writable view and not dropped yet */
    uint64_t bits[CODE_CACHE_BYTES / 4096 / 64];
};

/* Drop the pages recorded in the writable view */
static void drop_written(const struct tuxport_dbt *d)
{
    struct written *w = d->written;
    if (!w || !__atomic_load_n(&w->pages, __ATOMIC_RELAXED) ||
        __atomic_exchange_n(&w->dropping, true, __ATOMIC_ACQUIRE))
        return;
    size_t lo = SIZE_MAX, hi = 0;
    unsigned n = 0;
    for (size_t i = 0; i < sizeof(w->bits) / sizeof(w->bits[0]); i++) {
        uint64_t bits = __atomic_exchange_n(&w->bits[i], 0, __ATOMIC_RELAXED);
        if (!bits)
            continue;
        n += (unsigned)__builtin_popcountll(bits);
        if (lo == SIZE_MAX)
            lo = i * 64 + (size_t)__builtin_ctzll(bits);
        hi = i * 64 + 63 - (size_t)__builtin_clzll(bits);
    }
    if (n) {
        __atomic_sub_fetch(&w->pages, n, __ATOMIC_RELAXED);
        size_t page = host_page_size();
        madvise((char *)rw(d, d->cache) + lo * page, (hi - lo + 1) * page, MADV_DONTNEED);
    }
    __atomic_store_n(&w->dropping, false, __ATOMIC_RELEASE);
}

/* [start, end) (execution addresses) was written through the writable view */
static void note_written(const struct tuxport_dbt *d, const void *start, const void *end)
{
    struct written *w = d->written;
    if (!w || start == end)
        return;
    size_t page = host_page_size();
    size_t first = ((uintptr_t)start - (uintptr_t)d->cache) / page;
    size_t last = ((uintptr_t)end - 1 - (uintptr_t)d->cache) / page;
    for (size_t p = first; p <= last; p++) {
        uint64_t bit = UINT64_C(1) << (p & 63);
        if (!(__atomic_load_n(&w->bits[p / 64], __ATOMIC_RELAXED) & bit) &&
            !(__atomic_fetch_or(&w->bits[p / 64], bit, __ATOMIC_RELAXED) & bit))
            __atomic_add_fetch(&w->pages, 1, __ATOMIC_RELAXED);
    }
}

/* Make instruction fetch see several pieces of code after they are written */
static void sync_written(const struct tuxport_dbt *d, void *const *start, void *const *end, unsigned n)
{
    sync_code_ranges(start, end, n, d->rw_delta);
    for (unsigned i = 0; i < n; i++)
        note_written(d, start[i], end[i]);
}

/* Make instruction fetch see the new code after [start, end) is written */
static void sync_range(const struct tuxport_dbt *d, const uint32_t *start, const uint32_t *end)
{
    void *s = (void *)start, *e = (void *)end;
    sync_written(d, &s, &e, 1);
}

/*
 * Do not wait for a rewritten link point to take effect: the rewriting thread goes on directly
 * into the target block and does not need the new instruction; other threads (and this one later)
 * execute the old instruction until they see the new one, jump to the exit routine, and the
 * dispatcher finds the target block just the same, with the same result. Cache maintenance is
 * still done and the new instruction is seen sooner or later, only without waiting for the dsb or
 * doing an isb: at java startup each block is linked once or twice, and a full sync each time
 * waits for the invalidation broadcast to complete, which took a fifth of the startup time.
 */
static void publish_link(const struct tuxport_dbt *d, uint32_t *site, uint32_t *branch)
{
    publish_code_lazily(site, site + 1, d->rw_delta);
    if (branch)
        publish_code_lazily(branch, branch + 1, d->rw_delta);
    note_written(d, site, site + 1);
    if (branch)
        note_written(d, branch, branch + 1);
}

/* Pieces of code collected to be synced together (sync_code_ranges) */
struct syncs {
    void *start[3], *end[3];
    unsigned n;
};

static void sync_add(struct syncs *s, const uint32_t *start, const uint32_t *end)
{
    s->start[s->n] = (void *)start;
    s->end[s->n] = (void *)end;
    s->n++;
}

static void sync_all(const struct tuxport_dbt *d, struct syncs *s)
{
    if (s->n)
        sync_written(d, s->start, s->end, s->n);
    s->n = 0;
}

static void emit(struct xlate *x, uint32_t insn)
{
    if (x->n < MAX_BLOCK_WORDS)
        *(uint32_t *)((uintptr_t)&x->buf[x->n] + (uintptr_t)x->wdelta) = insn;
    x->n++;
}

static uint32_t *here(struct xlate *x)
{
    return &x->buf[x->n];
}

static uint32_t *routine(const struct tuxport_dbt *d, unsigned to)
{
    switch (to) {
    case TO_EXIT:
        return d->exit_stub;
    case TO_EXIT_FULL:
        return d->exit_full;
    case TO_LOOKUP:
        return d->lookup;
    default:
        return d->refill[to - TO_REFILL_READ];
    }
}

/* A b to a routine: TO_EXIT is the exit entry used by stubs (exit_stub, see emit_link_stub) */
static void emit_to_routine(struct xlate *x, unsigned to)
{
    if (x->nrelocs < MAX_RELOCS)
        x->relocs[x->nrelocs] = (uint32_t)x->n << 3 | to;
    x->nrelocs++; /* overflow is checked by prepare() */
    emit(x, enc_b((intptr_t)routine(x->d, to) - (intptr_t)here(x)));
}

static void emit_mov_const(struct xlate *x, unsigned rd, uint64_t v)
{
    if (v == 0) {
        emit(x, enc_movz(rd, 0, 0));
        return;
    }
    bool first = true;
    for (unsigned hw = 0; hw < 4; hw++) {
        uint32_t part = (uint32_t)(v >> (16 * hw)) & 0xffff;
        if (!part)
            continue;
        emit(x, first ? enc_movz(rd, part, hw) : enc_movk(rd, part, hw));
        first = false;
    }
}

/* rd = rn + imm, without changing the condition flags; requires |imm| < 2^24; rn cannot be 31 */
static void emit_add_const(struct xlate *x, unsigned rd, unsigned rn, int64_t imm)
{
    if (imm == 0) {
        if (rd != rn)
            emit(x, enc_mov(rd, rn));
        return;
    }
    bool sub = imm < 0;
    uint64_t a = sub ? (uint64_t)-imm : (uint64_t)imm;
    uint32_t hi = (uint32_t)(a >> 12), lo = (uint32_t)(a & 0xfff);
    unsigned src = rn;
    if (hi) {
        emit(x, sub ? enc_sub_imm(rd, src, hi, 1) : enc_add_imm(rd, src, hi, 1));
        src = rd;
    }
    if (lo)
        emit(x, sub ? enc_sub_imm(rd, src, lo, 0) : enc_add_imm(rd, src, lo, 0));
}

/* Whether guest Xr is a stolen register */
static bool stolen_in(const struct regs *regs, unsigned r)
{
    return r == 18 || r == regs->ctx || r == regs->t0 || r == regs->t1 || r == regs->t2 || r == regs->sp;
}
#define is_stolen(g) stolen_in(x->d->r, g)

static size_t guest_off(unsigned r)
{
    return r == REG_SP ? offsetof(struct tuxport_cpu, sp) : offsetof(struct tuxport_cpu, x) + 8 * r;
}

static void emit_load_guest(struct xlate *x, unsigned host, unsigned guest)
{
    emit(x, enc_ldr_x(host, CTX, guest_off(guest)));
}

static void emit_store_guest(struct xlate *x, unsigned host, unsigned guest)
{
    emit(x, enc_str_x(host, CTX, guest_off(guest)));
}

/* ---- Exit stubs ---- */

/*
 * Exit stubs hand the exit routine T0 = guest pc, T1 = patch site, T2 = exit reason
 * (emit_routines). The guest pc is a 48-bit address that would take three or four movs to build
 * as an immediate, so it is kept together with the exit reason in the two data words at the end
 * of the stub (the reason in the high 16 bits), and read by the routine's stub entry (exit_stub):
 *
 *     [b 4]   adr T1, 0   b exit_stub   .quad reason << 48 | pc
 *
 * The first instruction of a link stub is a b to the next instruction, and linking rewrites it
 * into a b straight to the target block: other threads may be executing it, and changing a b into
 * a b is a concurrent modification the architecture allows (the CMODX rules of the ARM ARM). The
 * routine hands T1 minus 4 to the dispatcher: for a link stub that is its patch site; for other
 * stubs the dispatcher does not look at it. branch is the only conditional branch that jumps to
 * this stub, and when linking, if the target is within its offset range it is made to jump there
 * directly (only when there are no other threads, see dbt_run). When the computed branch target is
 * beyond 48 bits (the guest jumping there can only fault), the reason is passed separately with a
 * movz to exit_full.
 */
static void emit_link_stub(struct xlate *x, unsigned reason, uint64_t pc, const uint32_t *branch)
{
    unsigned delta = branch ? (unsigned)(here(x) - branch) : 0;
    if (reason == R_LOOKUP) { /* target pc already in T0: look it up in translated code first */
        emit_to_routine(x, TO_LOOKUP);
        return;
    }
    if (reason == R_CHAIN || reason == R_CHAIN_FWD)
        emit(x, enc_b(4));
    emit(x, enc_adr_self(T1));
    uint64_t code = reason | delta << REASON_BITS;
    if ((reason == R_CHAIN || reason == R_CHAIN_FWD) && x->shared)
        code |= LINK_SHARED;
    if (pc >> PACKED_PC_BITS == 0) {
        emit_to_routine(x, TO_EXIT);
        pc |= code << PACKED_PC_BITS;
    } else {
        emit(x, enc_movz(T2, (unsigned)code, 0));
        emit_to_routine(x, TO_EXIT_FULL);
    }
    emit(x, (uint32_t)pc);
    emit(x, (uint32_t)(pc >> 32));
}

static void emit_exit_stub(struct xlate *x, unsigned reason, uint64_t pc)
{
    emit_link_stub(x, reason, pc, NULL);
}

/* Emit a conditional branch (offset patched later) whose target is an exit stub */
static void emit_to_stub(struct xlate *x, uint32_t insn, enum fixup_kind kind, unsigned reason, uint64_t pc)
{
    unsigned i;
    for (i = 0; i < x->nstubs; i++) {
        if (x->stubs[i].reason == reason && x->stubs[i].pc == pc)
            break;
    }
    if (i == x->nstubs) {
        if (x->nstubs < MAX_STUBS)
            x->stubs[i] = (struct stub){.reason = reason, .pc = pc};
        x->nstubs++; /* overflow is checked by prepare() */
    }
    if (x->nfixups < MAX_FIXUPS)
        x->fixups[x->nfixups++] = (struct fixup){here(x), kind, i};
    else
        x->nfixups++; /* record the overflow; prepare() checks it */
    emit(x, insn);
}

/* The instruction at pc jumps directly to target */
static unsigned chain_reason(uint64_t pc, uint64_t target)
{
    return target > pc ? R_CHAIN_FWD : R_CHAIN;
}

/* Branch to the refill stub on a TLB miss: one per memory-access instruction, not merged with
 * other stubs. After a successful refill, execution continues at the next instruction (the access
 * itself) */
static void emit_to_refill(struct xlate *x, uint64_t pc, unsigned bytes, bool write, unsigned fail)
{
    if (x->nstubs < MAX_STUBS)
        x->stubs[x->nstubs] = (struct stub){.reason = STUB_REFILL,
                                            .pc = pc,
                                            .resume = here(x) + 1,
                                            .bytes = bytes,
                                            .write = write,
                                            .fail = fail};
    if (x->nfixups < MAX_FIXUPS)
        x->fixups[x->nfixups] = (struct fixup){here(x), FIX_IMM19, x->nstubs};
    x->nstubs++;
    x->nfixups++; /* overflow is checked by prepare() */
    emit(x, enc_cbnz_x(T1));
}

/*
 * Refill stub: T1 points to the info word and the refill routine is entered; the info word is
 * followed by this instruction's guest pc. On success the routine sets T2 = addend and jumps back
 * to the access itself; on failure it exits with the pc and the reason in the info word, and
 * Tier 0 executes this instruction.
 *
 *     adr T1, 8   b <refill routine>   .word info   .quad pc
 */
static void emit_refill_stub(struct xlate *x, const struct stub *s)
{
    if (s->bytes > 1u << REFILL_REASON_SHIFT || s->fail >> (REFILL_RESUME_SHIFT - REFILL_REASON_SHIFT))
        tuxport_guard_fail(); /* the info word cannot hold it: an engine bug */
    emit(x, enc_adr(T1, 8));
    emit_to_routine(x, s->write ? TO_REFILL_WRITE : TO_REFILL_READ);
    intptr_t resume = s->resume - here(x);
    emit(x, (uint32_t)resume << REFILL_RESUME_SHIFT | s->fail << REFILL_REASON_SHIFT | (s->bytes - 1));
    emit(x, (uint32_t)s->pc);
    emit(x, (uint32_t)(s->pc >> 32));
}

static bool fallback(struct xlate *x, uint64_t pc)
{
    x->st->insns_fallback++;
    emit_exit_stub(x, R_INTERP, pc);
    return false;
}

/* Patch the offset of a b/cbz/cbnz/tbz/tbnz */
static void patch_branch(intptr_t wdelta, uint32_t *site, const uint32_t *target)
{
    uint32_t off = (uint32_t)(target - site), *w = (uint32_t *)((uintptr_t)site + (uintptr_t)wdelta);
    if ((*w & 0x7c000000) == 0x14000000)
        *w |= off & 0x3ffffff;
    else if ((*w & 0x7e000000) == 0x36000000)
        *w |= (off & 0x3fff) << 5;
    else
        *w |= (off & 0x7ffff) << 5;
}

/* A local branch within the block; an overflowed block is never used (prepare() checks), it just
 * must not write out of bounds */
static void patch_local(struct xlate *x, uint32_t *site, const uint32_t *target)
{
    if (site < x->buf + MAX_BLOCK_WORDS)
        patch_branch(x->wdelta, site, target);
}

/* ---- Data processing: copy verbatim or rewrite stolen registers ---- */

struct operands {
    unsigned n;
    unsigned pos[4]; /* start bit of the field */
    bool sp[4];      /* whether 31 means SP */
    bool writes_rd;  /* the Rd field at pos[0] == 0 is written */
};

static void add_operand(struct operands *o, unsigned pos, bool sp)
{
    o->pos[o->n] = pos;
    o->sp[o->n] = sp;
    o->n++;
}

/* List the register fields of a data-processing instruction; Tier 0 has already confirmed that the
 * encoding is valid */
static bool dp_operands(uint32_t insn, struct operands *o)
{
    o->n = 0;
    o->writes_rd = true;
    if (FIELD(insn, 28, 26) == 4) { /* immediate */
        switch (FIELD(insn, 25, 23)) {
        case 2: /* ADD/SUB (immediate) */
            add_operand(o, 0, !BIT(insn, 29));
            add_operand(o, 5, true);
            return true;
        case 4: /* logical (immediate) */
            add_operand(o, 0, FIELD(insn, 30, 29) != 3);
            add_operand(o, 5, false);
            return true;
        case 5: /* MOVN/MOVZ/MOVK */
            add_operand(o, 0, false);
            return true;
        case 6: /* bitfield */
            add_operand(o, 0, false);
            add_operand(o, 5, false);
            return true;
        case 7: /* EXTR */
            add_operand(o, 0, false);
            add_operand(o, 5, false);
            add_operand(o, 16, false);
            return true;
        default:
            return false;
        }
    }
    unsigned op2 = FIELD(insn, 24, 21);
    if (!BIT(insn, 28)) {
        bool extended = (op2 & 9) == 9;
        add_operand(o, 0, extended && !BIT(insn, 29));
        add_operand(o, 5, extended);
        add_operand(o, 16, false);
        return true;
    }
    switch (op2) {
    case 0: /* ADC/SBC */
        if (FIELD(insn, 15, 10)) { /* RMIF, SETF8, SETF16 (FlagM): only read Rn, only write flags */
            o->writes_rd = false;
            add_operand(o, 5, false);
            return true;
        }
        /* fallthrough */
    case 4: /* the CSEL family */
        add_operand(o, 0, false);
        add_operand(o, 5, false);
        add_operand(o, 16, false);
        return true;
    case 2: /* CCMP/CCMN: no Rd, bits 4:0 are nzcv */
        o->writes_rd = false;
        add_operand(o, 5, false);
        if (!BIT(insn, 11))
            add_operand(o, 16, false);
        return true;
    case 6:
        add_operand(o, 0, false);
        add_operand(o, 5, false);
        if (!BIT(insn, 30))
            add_operand(o, 16, false);
        return true;
    default:
        if (!(op2 & 8))
            return false;
        add_operand(o, 0, false);
        add_operand(o, 5, false);
        add_operand(o, 16, false);
        add_operand(o, 10, false);
        return true;
    }
}

/* Replace stolen guest registers with temporaries: load them from the cpu struct first, and write
 * Rd back after execution */
static bool emit_operands(struct xlate *x, uint32_t insn, const struct operands *o)
{
    const unsigned pool[3] = {T0, T1, T2};
    unsigned guest[3], nmap = 0, rd_host = 0, rd_guest = 0;
    bool rd_stolen = false;
    uint32_t out = insn;
    bool renamed = false;
    for (unsigned i = 0; i < o->n; i++) {
        unsigned r = (insn >> o->pos[i]) & 31;
        if (r == 31 && o->sp[i]) { /* SP is already in a host register; just change the number */
            out = (out & ~(31u << o->pos[i])) | SP_HOST << o->pos[i];
            renamed = true;
            continue;
        }
        unsigned g = r;
        if (!is_stolen(g))
            continue;
        unsigned k;
        for (k = 0; k < nmap && guest[k] != g; k++)
            ;
        if (k == nmap) {
            if (nmap == 3)
                return false;
            guest[nmap++] = g;
        }
        out = (out & ~(31u << o->pos[i])) | pool[k] << o->pos[i];
        if (o->pos[i] == 0 && o->writes_rd) {
            rd_stolen = true;
            rd_host = pool[k];
            rd_guest = g;
        }
    }
    if (nmap == 0 && !renamed) {
        emit(x, insn);
        x->st->insns_copied++;
        return true;
    }
    if (nmap == 3)
        x->t2_group = -1; /* later members get the host base back from the slot (plan_group) */
    for (unsigned k = 0; k < nmap; k++)
        emit_load_guest(x, pool[k], guest[k]);
    emit(x, out);
    if (rd_stolen)
        emit_store_guest(x, rd_host, rd_guest);
    x->st->insns_remapped++;
    return true;
}

/* ADR/ADRP: the result is a compile-time constant */
static void emit_pcrel(struct xlate *x, uint32_t insn, uint64_t pc)
{
    unsigned rd = insn & 31;
    uint64_t imm = ((uint64_t)FIELD(insn, 23, 5) << 2) | FIELD(insn, 30, 29);
    int64_t off = (int64_t)(imm << 43) >> 43;
    uint64_t v = BIT(insn, 31) ? (pc & ~UINT64_C(0xfff)) + ((uint64_t)off << 12) : pc + (uint64_t)off;
    x->st->insns_remapped++;
    if (rd == 31)
        return;
    if (is_stolen(rd)) {
        emit_mov_const(x, T0, v);
        emit_store_guest(x, T0, rd);
    } else {
        emit_mov_const(x, rd, v);
    }
}

/* ---- Memory access: the inline TLB fast path ---- */

enum mem_mode { M_OFFSET, M_PRE, M_POST, M_REG, M_LITERAL };

struct memop {
    enum mem_mode mode;
    bool load, pair, prefetch;
    bool simd;      /* Rt/Rt2 are SIMD&FP registers */
    bool structure; /* LD1–LD4/ST1–ST4: final lacks only Rn */
    bool wb_reg;    /* writeback adds Xm instead of imm */
    unsigned size;  /* log2 of one element's byte count */
    unsigned bytes; /* total bytes accessed, for structure */
    unsigned rt, rt2, rn, rm, option, shift;
    int64_t imm;    /* the absolute address for M_LITERAL */
    uint32_t final; /* template of the final access instruction, with register fields 0 */
};

static bool decode_memop(uint32_t insn, uint64_t pc, struct memop *m)
{
    unsigned size = FIELD(insn, 31, 30), opc = FIELD(insn, 23, 22);
    *m = (struct memop){.rt = insn & 31, .rn = FIELD(insn, 9, 5), .size = size};
    if (BIT(insn, 26))
        return false;
    if ((insn & 0x3b000000) == 0x18000000) { /* LDR (literal), bits 31:30 are opc */
        static const unsigned sizes[3] = {2, 3, 2}, opcs[3] = {1, 1, 2};
        m->mode = M_LITERAL;
        m->imm = (int64_t)(pc + (uint64_t)(((int64_t)((uint64_t)FIELD(insn, 23, 5) << 45) >> 45) * 4));
        if (size == 3) {
            m->prefetch = true;
            return true;
        }
        m->load = true;
        m->size = sizes[size];
        m->final = enc_ldst_regoff(sizes[size], opcs[size], 0, 0, 0);
        return true;
    }
    if ((insn & 0x3a000000) == 0x28000000) { /* LDP/STP/LDPSW */
        unsigned idx = FIELD(insn, 24, 23);
        m->pair = true;
        m->load = BIT(insn, 22);
        m->rt2 = FIELD(insn, 14, 10);
        m->size = size == 2 ? 3 : 2;
        m->imm = ((int64_t)((uint64_t)FIELD(insn, 21, 15) << 57) >> 57) * (1 << m->size);
        m->mode = idx == 1 ? M_POST : idx == 3 ? M_PRE : M_OFFSET;
        m->final = (insn & 0xc0400000) | 0x29000000; /* signed-offset form, offset 0 */
        return true;
    }
    if (BIT(insn, 29) != 1 || FIELD(insn, 28, 27) != 3)
        return false;
    m->load = opc != 0;
    m->prefetch = size == 3 && opc == 2;
    m->final = enc_ldst_regoff(size, opc, 0, 0, 0);
    if ((insn & 0x3b000000) == 0x39000000) {
        m->mode = M_OFFSET;
        m->imm = (int64_t)FIELD(insn, 21, 10) << size;
        return true;
    }
    if ((insn & 0x3b200000) == 0x38000000) {
        unsigned op = FIELD(insn, 11, 10);
        m->imm = (int64_t)((uint64_t)FIELD(insn, 20, 12) << 55) >> 55;
        m->mode = op == 1 ? M_POST : op == 3 ? M_PRE : M_OFFSET;
        return true;
    }
    if ((insn & 0x3b200c00) == 0x38200800) {
        m->mode = M_REG;
        m->rm = FIELD(insn, 20, 16);
        m->option = FIELD(insn, 15, 13);
        m->shift = BIT(insn, 12) ? size : 0;
        return true;
    }
    return false;
}

/* Total bytes accessed by LD1–LD4/ST1–ST4; Tier 0 has already confirmed that the encoding is
 * valid */
static unsigned structure_bytes(uint32_t insn)
{
    unsigned q = BIT(insn, 30), opcode = FIELD(insn, 15, 12), size = FIELD(insn, 11, 10);
    if (!BIT(insn, 24)) { /* multiple structures: opcode maps to rpt × selem registers */
        static const uint8_t regs[16] = {[0x0] = 4, [0x2] = 4, [0x4] = 3, [0x6] = 3,
                                         [0x7] = 1, [0x8] = 2, [0xa] = 2};
        return regs[opcode] * (q ? 16u : 8u);
    }
    unsigned selem = ((opcode >> 1 & 1) << 1 | BIT(insn, 21)) + 1, scale = opcode >> 2;
    if (scale == 3) /* LD1R–LD4R */
        scale = size;
    else if (scale == 2 && (size & 1))
        scale = 3;
    return selem << scale;
}

/* SIMD&FP memory access (bit 26 set): decoded the same way as Tier 0's tuxport_simd_ldst */
static bool decode_simd_memop(uint32_t insn, uint64_t pc, struct memop *m)
{
    unsigned size = FIELD(insn, 31, 30), opc = FIELD(insn, 23, 22);
    *m = (struct memop){.simd = true, .rt = insn & 31, .rn = FIELD(insn, 9, 5)};
    if ((insn & 0xbe000000) == 0x0c000000) { /* LD1–LD4/ST1–ST4 */
        m->structure = true;
        m->load = BIT(insn, 22);
        m->bytes = structure_bytes(insn);
        m->rm = FIELD(insn, 20, 16);
        m->mode = BIT(insn, 23) ? M_POST : M_OFFSET;
        if (m->mode == M_POST) {
            m->wb_reg = m->rm != 31;
            m->imm = m->bytes;
        }
        /* Change to the form without writeback, with Rn replaced by the host address */
        m->final = insn & ~(1u << 23 | 31u << 16 | 31u << 5);
        return true;
    }
    if ((insn & 0x3f000000) == 0x1c000000) { /* LDR (literal): bits 31:30 are opc */
        m->mode = M_LITERAL;
        m->imm = (int64_t)(pc + (uint64_t)(((int64_t)((uint64_t)FIELD(insn, 23, 5) << 45) >> 45) * 4));
        m->load = true;
        m->size = 2 + size;
        m->final = size == 2 ? enc_ldst_regoff(0, 3, 0, 0, 0) : enc_ldst_regoff(2 + size, 1, 0, 0, 0);
        m->final |= 1u << 26;
        return true;
    }
    if ((insn & 0x3e000000) == 0x2c000000) { /* LDP/STP/LDNP/STNP */
        unsigned idx = FIELD(insn, 24, 23);
        m->pair = true;
        m->load = BIT(insn, 22);
        m->rt2 = FIELD(insn, 14, 10);
        m->size = 2 + size;
        m->imm = ((int64_t)((uint64_t)FIELD(insn, 21, 15) << 57) >> 57) * (1 << m->size);
        m->mode = idx == 1 ? M_POST : idx == 3 ? M_PRE : M_OFFSET;
        m->final = (insn & 0xc0400000) | 0x2d000000; /* signed-offset form, offset 0 */
        return true;
    }
    /* Single register: size = 00 with opc = 1x is the 128-bit Q register */
    m->load = opc & 1;
    m->size = (size == 0 && (opc & 2)) ? 4 : size;
    m->final = enc_ldst_regoff(size, opc, 0, 0, 0) | 1u << 26;
    if ((insn & 0x3f000000) == 0x3d000000) {
        m->mode = M_OFFSET;
        m->imm = (int64_t)FIELD(insn, 21, 10) << m->size;
        return true;
    }
    if ((insn & 0x3f200000) == 0x3c000000) {
        unsigned op = FIELD(insn, 11, 10);
        m->imm = (int64_t)((uint64_t)FIELD(insn, 20, 12) << 55) >> 55;
        m->mode = op == 1 ? M_POST : op == 3 ? M_PRE : M_OFFSET;
        return true;
    }
    if ((insn & 0x3f200c00) == 0x3c200800) {
        m->mode = M_REG;
        m->rm = FIELD(insn, 20, 16);
        m->option = FIELD(insn, 15, 13);
        m->shift = BIT(insn, 12) ? m->size : 0;
        return true;
    }
    return false;
}

/*
 * Look up the TLB with T0 = guest address; on a hit T2 = addend. Take the entry of the page of
 * the last byte and compare it with the page of the first byte: the TLB is direct-mapped, so for
 * a cross-page access the first page's page number cannot appear in the next page's entry, and
 * this one comparison also rules out cross-page accesses. A miss or a cross-page access enters the
 * refill routine, and a failed refill exits with `fail`. `in_granule` means the access lies within
 * one aligned 16 bytes and cannot cross a page, so the first byte's page can be used directly.
 * `last_ready` means the caller has already computed the last byte's address into T1
 * ([`emit_address_imm`]).
 */
static void emit_tlb_check(struct xlate *x, uint64_t pc, unsigned bytes, bool write, bool in_granule,
                           bool last_ready, unsigned fail)
{
    x->t2_group = -1;
    unsigned last = T0;
    if (last_ready) {
        last = T1;
    } else if (bytes > 1 && !in_granule) {
        emit(x, enc_add_imm(T1, T0, bytes - 1, 0));
        last = T1;
    }
    emit(x, enc_ubfx(T1, last, TUXPORT_PAGE_SHIFT, TLB_BITS));
    emit(x, enc_add_lsl(T1, CTX, T1, 5));
    emit(x, enc_ldp_x(T1, T2, T1, (int)TLB_OFF + (write ? 16 : 0)));
    emit(x, enc_eon_lsr(T1, T1, T0, TUXPORT_PAGE_SHIFT)); /* tag is the inverted page number */
    emit_to_refill(x, pc, bytes, write, fail);
}

/*
 * The access's guest address is known at translation time (`addr`, also in T0, and the access
 * does not cross a page): the entry's position is a constant, so loading the entry need not wait
 * for T0 to be computed (global variables: for an access after ADRP, T0 waits for the few movks
 * ADRP was turned into).
 */
static void emit_tlb_check_known(struct xlate *x, uint64_t pc, uint64_t addr, unsigned bytes, bool write,
                                 unsigned fail)
{
    x->t2_group = -1;
    uint64_t off = TLB_OFF + ((addr >> TUXPORT_PAGE_SHIFT) & (TUXPORT_TLB_ENTRIES - 1)) * 32 + (write ? 16 : 0);
    int64_t lo = (int64_t)(off & 0xfff);
    uint64_t hi = off - (uint64_t)lo;
    if (lo > 504 && lo >= 4096 - 512) { /* ldp offsets are -512..504 */
        hi += 4096;
        lo -= 4096;
    }
    unsigned b = CTX;
    if (hi) {
        emit(x, enc_add_imm(T1, CTX, (uint32_t)(hi >> 12), 1));
        b = T1;
    }
    if (lo > 504) {
        emit(x, enc_add_imm(T1, b, (uint32_t)lo, 0));
        b = T1;
        lo = 0;
    }
    emit(x, enc_ldp_x(T1, T2, b, (int)lo));
    emit(x, enc_eon_lsr(T1, T1, T0, TUXPORT_PAGE_SHIFT)); /* tag is the inverted page number */
    emit_to_refill(x, pc, bytes, write, fail);
}

/* The guest address of a `bytes`-byte access at offset `imm` from register `rn`, whose value is
 * known; returns false if the register's value is not known or the access crosses a page */
static bool known_address(const struct xlate *x, unsigned rn, int64_t imm, unsigned bytes, uint64_t *addr)
{
    if (rn > 30 || !(x->konst >> rn & 1))
        return false;
    *addr = x->kval[rn] + (uint64_t)imm;
    return ((*addr ^ (*addr + bytes - 1)) >> TUXPORT_PAGE_SHIFT) == 0;
}

/*
 * T0 = base + imm. When the last byte's page is needed, T1 = base + imm + bytes - 1 is computed
 * straight from the base, not through T0: the TLB index need not wait for T0, which takes one
 * step off the access's dependency chain (pointer-chasing loads go through the whole chain every
 * time), with the same instruction count. Returns whether T1 has been computed.
 */
static bool emit_address_imm(struct xlate *x, unsigned base, int64_t imm, unsigned bytes, bool in_granule)
{
    int64_t last = imm + (int64_t)bytes - 1;
    bool fold = bytes > 1 && !in_granule && last > -4096 && last < 4096;
    if (fold)
        emit(x, last >= 0 ? enc_add_imm(T1, base, (uint32_t)last, 0) : enc_sub_imm(T1, base, (uint32_t)-last, 0));
    emit_add_const(x, T0, base, imm);
    return fold;
}

/* Byte count of an access */
static unsigned memop_bytes(const struct memop *m)
{
    return m->structure ? m->bytes : (m->pair ? 2u : 1u) << m->size;
}

/* Check SP's alignment before an SP-based access; not needed if it was already checked in this
 * block and SP has since changed only by multiples of 16 */
static void emit_sp_check(struct xlate *x, uint64_t pc)
{
    if (x->sp_aligned)
        return;
    emit(x, enc_and_low(T1, SP_HOST, 4));
    emit_to_stub(x, enc_cbnz_x(T1), FIX_IMM19, R_INTERP, pc);
    x->sp_aligned = true;
}

/* Whether SP is still aligned after it changes by `delta` */
static void sp_moved(struct xlate *x, int64_t delta)
{
    if (delta & 15)
        x->sp_aligned = false;
}

static int plan_group(struct xlate *x, const struct memop *lead);
static void emit_group(struct xlate *x, const struct memop *lead, uint64_t pc, int slot);

static void emit_memop(struct xlate *x, const struct memop *m, uint64_t pc)
{
    unsigned rn = m->rn == 31 ? REG_SP : m->rn;
    int slot = plan_group(x, m);
    if (slot >= 0) {
        emit_group(x, m, pc, slot);
        return;
    }

    /* 1. guest address → T0 */
    bool in_granule = false, last_ready = false, known = false;
    uint64_t addr = 0;
    if (m->mode == M_LITERAL) {
        emit_mov_const(x, T0, (uint64_t)m->imm);
        addr = (uint64_t)m->imm;
        known = ((addr ^ (addr + memop_bytes(m) - 1)) >> TUXPORT_PAGE_SHIFT) == 0;
    } else {
        unsigned base = rn == REG_SP ? SP_HOST : rn;
        if (rn == REG_SP) { /* SP must be 16-byte aligned */
            emit_sp_check(x, pc);
            int64_t off = m->mode == M_POST ? 0 : m->imm;
            in_granule = m->mode != M_REG && (off & 15) + memop_bytes(m) <= 16;
        } else if (is_stolen(rn)) {
            emit_load_guest(x, T0, rn);
            base = T0;
        }
        switch (m->mode) {
        case M_REG: {
            unsigned idx = m->rm;
            if (is_stolen(idx)) {
                emit_load_guest(x, T1, idx);
                idx = T1;
            }
            emit(x, enc_add_ext(T0, base, idx, m->option, m->shift));
            break;
        }
        case M_POST:
            last_ready = emit_address_imm(x, base, 0, memop_bytes(m), in_granule);
            break;
        default:
            known = m->mode == M_OFFSET && known_address(x, m->rn, m->imm, memop_bytes(m), &addr);
            if (known)
                emit_add_const(x, T0, base, m->imm);
            else
                last_ready = emit_address_imm(x, base, m->imm, memop_bytes(m), in_granule);
            break;
        }
    }

    /* 2. TLB */
    if (known)
        emit_tlb_check_known(x, pc, addr, memop_bytes(m), !m->load, R_INTERP);
    else
        emit_tlb_check(x, pc, memop_bytes(m), !m->load, in_granule, last_ready, R_INTERP);

    /* 3. Access host memory */
    if (m->structure) {
        emit(x, enc_add_lsl(T2, T2, T0, 0));
        emit(x, m->final | T2 << 5);
    } else if (!m->pair) {
        if (m->simd || !is_stolen(m->rt)) {
            emit(x, m->final | T0 << 16 | T2 << 5 | m->rt);
        } else if (m->load) {
            emit(x, m->final | T0 << 16 | T2 << 5 | T1);
            emit_store_guest(x, T1, m->rt);
        } else {
            emit_load_guest(x, T1, m->rt);
            emit(x, m->final | T0 << 16 | T2 << 5 | T1);
        }
    } else {
        emit(x, enc_add_lsl(T2, T2, T0, 0));
        bool s1 = !m->simd && is_stolen(m->rt), s2 = !m->simd && is_stolen(m->rt2);
        unsigned h1 = s1 ? T0 : m->rt, h2 = s2 ? T1 : m->rt2;
        if (!m->load) {
            if (s1)
                emit_load_guest(x, T0, m->rt);
            if (s2)
                emit_load_guest(x, T1, m->rt2);
        }
        emit(x, m->final | h2 << 10 | T2 << 5 | h1);
        if (m->load) {
            if (s1)
                emit_store_guest(x, T0, m->rt);
            if (s2)
                emit_store_guest(x, T1, m->rt2);
        }
    }

    /* 4. Base writeback */
    if (m->mode == M_PRE || m->mode == M_POST) {
        unsigned inc = m->rm, dst = rn == REG_SP ? SP_HOST : is_stolen(rn) ? T0 : rn;
        if (m->wb_reg && is_stolen(inc)) {
            emit_load_guest(x, T1, inc);
            inc = T1;
        }
        if (dst == T0)
            emit_load_guest(x, T0, rn);
        if (m->wb_reg)
            emit(x, enc_add_lsl(dst, dst, inc, 0));
        else
            emit_add_const(x, dst, dst, m->imm);
        if (dst == T0)
            emit_store_guest(x, T0, rn);
        if (rn == REG_SP)
            sp_moved(x, m->wb_reg ? 1 : m->imm);
    }
    x->st->insns_memory++;
}

/* ---- A group of accesses sharing one TLB check ---- */

/*
 * Several accesses in the same block based on the same register (function prologues and
 * epilogues, several fields of a struct) share one check: the leader checks the range the whole
 * group covers, after which T2 = the host address for the base's value before the leader
 * executes, and members access [T2, #offset] directly. Between members there may only be
 * instructions that do not write the base (except adding or subtracting a constant, whose
 * accumulated offset is recorded) and conditional branches, and SP may only change by multiples
 * of 16 (the leader's alignment check then also holds for the members).
 *
 * Other accesses (of other groups, or ungrouped) and instructions that use T2 may sit between
 * members: the leader stores the host base in the cpu's dbt_groups[slot], and the members after
 * them reload it first. The slot's address is a constant and can be loaded early, so members'
 * accesses need not wait for a TLB lookup: in python's interpreter loop, accesses on the stack
 * frame and code object bases interleave, and previously not one of a dozen or so checks in a
 * block could be saved. Mappings do not change while a block executes (the TLB is only refilled),
 * so the host base stays valid until the base changes. At most GROUP_SLOTS groups at once.
 *
 * Across pages the refill routine handles both pages, and the check fails if the host addresses
 * are not contiguous. Members after a conditional branch do not necessarily execute, and the
 * covered range may also include pages they cannot access, so a failed check exits with
 * R_UNGROUP and Tier 0 executes the leader; the second failure of the same leader invalidates this
 * block, and on retranslation it is no longer grouped (ungroup).
 */

static bool is_atomic(uint32_t insn);

/* A member's access instruction: host address = Rn + off, with the Rn and Rt fields 0. Returns
 * false if the immediate does not fit */
static bool member_insn(const struct memop *m, int64_t off, uint32_t *out)
{
    unsigned sz = m->size;
    if (m->structure) { /* LD1–LD4/ST1–ST4 have no offset */
        *out = m->final;
        return off == 0;
    }
    if (m->pair) {
        if (off & ((1 << sz) - 1) || off < -64 * (1 << sz) || off > 63 * (1 << sz))
            return false;
        *out = m->final | ((uint32_t)(off >> sz) & 0x7f) << 15;
        return true;
    }
    uint32_t common = m->final & 0xc4c00000; /* size, V, opc */
    if (off >= 0 && !(off & ((1 << sz) - 1)) && off >> sz < 4096) { /* unsigned offset */
        *out = common | 0x39000000 | (uint32_t)(off >> sz) << 10;
        return true;
    }
    if (off >= -256 && off < 256) { /* LDUR/STUR */
        *out = common | 0x38000000 | ((uint32_t)off & 0x1ff) << 12;
        return true;
    }
    return false;
}

/* The register borrowed for the host address when the immediate does not fit: a stolen Rt uses T1
 * (T0 and T1 for a pair) */
static unsigned member_scratch(const struct xlate *x, const struct memop *m)
{
    return m->pair && !m->simd && is_stolen(m->rt) ? T1 : T0;
}

static bool member_fits(const struct xlate *x, const struct memop *m, int64_t off)
{
    uint32_t insn = 0;
    if (member_insn(m, off, &insn))
        return true;
    return !(m->pair && !m->simd && is_stolen(m->rt) && is_stolen(m->rt2));
}

/* Whether the access at pc may lead a group: a recorded leader and anything within one block's
 * length after it may not, since its former members would most likely fail the check for the same
 * reason */
static bool nogroup(const struct tuxport_dbt *d, uint64_t pc)
{
    unsigned n = d->nogroup_count < NOGROUP_MAX ? d->nogroup_count : NOGROUP_MAX;
    for (unsigned i = 0; i < n; i++) {
        if (pc - d->nogroup[i] < 4 * MAX_BLOCK_INSNS)
            return true;
    }
    return false;
}

/*
 * Instructions that do not write the base and do not end the block may sit between members:
 * returns 1, or 2 if it uses T2, otherwise 0. An ADD/SUB (immediate) that adds a constant to or
 * subtracts one from the base records the change in `delta`.
 */
static int keeps_group(const struct xlate *x, uint32_t insn, unsigned rn, int64_t *delta)
{
    unsigned top = FIELD(insn, 28, 25), stolen[3], nstolen = 0;
    struct operands o = {.n = 0};
    if ((top & 7) == 7) { /* SIMD&FP data processing: only general-register operands matter */
        struct native_desc nd;
        if (!tuxport_simd_dp_decode(insn, &nd))
            return false;
        for (unsigned i = 0; i < nd.n; i++) {
            add_operand(&o, nd.gpr[i].pos, false);
            if (nd.gpr[i].store && ((insn >> nd.gpr[i].pos) & 31) == rn && rn != 31)
                return false;
        }
    } else if (top == 0x8 || top == 0x9 || top == 0x5 || top == 0xd) {
        if (FIELD(insn, 28, 24) == 0x10) /* ADR/ADRP: a stolen Rd only uses T0 */
            return (insn & 31) != rn || rn == 31;
        if (!dp_operands(insn, &o))
            return false;
        uint32_t op = insn & 0xff800000;
        if ((op == 0x91000000 || op == 0xd1000000) && (insn & 31) == rn && FIELD(insn, 9, 5) == rn) {
            int64_t imm = (int64_t)FIELD(insn, 21, 10) << (BIT(insn, 22) ? 12 : 0);
            *delta += op == 0xd1000000 ? -imm : imm;
            return true;
        }
        unsigned rd = insn & 31;
        if (o.writes_rd && (rn == 31 ? rd == 31 && o.sp[0] : rd == rn))
            return false;
    } else {
        return (insn & 0xff000010) == 0x54000000 || /* B.cond */
               (insn & 0x7e000000) == 0x34000000 || /* CBZ/CBNZ */
               (insn & 0x7e000000) == 0x36000000 || /* TBZ/TBNZ */
               (insn & 0xfffff01f) == 0xd503201f || /* HINT */
               (insn & 0xfffff01f) == 0xd503301f;   /* barriers, CLREX */
    }
    /* Stolen registers are replaced by T0, T1, T2 in turn (emit_operands): three of them use T2 */
    for (unsigned i = 0; i < o.n; i++) {
        unsigned r = (insn >> o.pos[i]) & 31, k;
        if ((r == 31 && o.sp[i]) || !is_stolen(r))
            continue;
        for (k = 0; k < nstolen && stolen[k] != r; k++)
            ;
        if (k == nstolen && nstolen++ == 2)
            return 2;
        stolen[k] = r;
    }
    return 1;
}

/* When access `m` executes, the base is `delta` more than before the leader executed: the start
 * of the access range (relative to the base before the leader) */
static int64_t member_start(const struct memop *m, int64_t delta)
{
    return delta + (m->mode == M_POST ? 0 : m->imm);
}

/* Starting from access `lead` (instruction x->idx of the block), look ahead for members with the
 * same base; if there is at least one member, fill in this group in a free slot and return the
 * slot's index, otherwise return -1 */
static int plan_group(struct xlate *x, const struct memop *lead)
{
    int slot = -1;
    for (int i = 0; i < GROUP_SLOTS && slot < 0; i++) {
        if (x->groups[i].next >= x->groups[i].n)
            slot = i;
    }
    if (slot < 0)
        return -1;
    struct group *g = &x->groups[slot];
    unsigned rn = lead->rn;
    uint64_t pc0 = x->pc + 4ull * x->idx;
    g->n = g->next = 0;
    g->spill = false;
    if (lead->mode == M_LITERAL || lead->mode == M_REG || lead->wb_reg || (rn != 31 && is_stolen(rn)) ||
        nogroup(x->d, pc0))
        return -1;
    int64_t delta = 0, lo = member_start(lead, 0), hi = lo + memop_bytes(lead);
    bool write = !lead->load;
    if (!member_fits(x, lead, lo))
        return -1;
    g->idx[0] = x->idx;
    g->off[0] = (int32_t)lo;
    g->reload[0] = false;
    g->n = 1;
    if (lead->mode != M_OFFSET)
        delta = lead->imm;
    bool overwritten = lead->load && !lead->simd && (lead->rt == rn || (lead->pair && lead->rt2 == rn));
    bool clobbered = false; /* an instruction used T2 after the previous member */
    for (unsigned j = x->idx + 1; !overwritten && j < x->limit && g->n < GROUP_MAX; j++) {
        uint32_t insn = x->guest[j];
        unsigned top = FIELD(insn, 28, 25);
        if (top != 0x4 && top != 0x6 && top != 0xc && top != 0xe) {
            int keep = keeps_group(x, insn, rn, &delta);
            if (!keep)
                break;
            clobbered |= keep == 2;
            continue;
        }
        struct memop m;
        uint64_t pc = x->pc + 4ull * j;
        if (is_atomic(insn) || !(BIT(insn, 26) ? decode_simd_memop(insn, pc, &m) : decode_memop(insn, pc, &m)))
            break;
        if (m.prefetch) /* generates no code */
            continue;
        if (m.rn != rn) {
            /* An access on another base uses T2; stop here if the loaded value overwrites the
             * base */
            if (m.load && !m.simd && (m.rt == rn || (m.pair && m.rt2 == rn)))
                break;
            clobbered = true;
            continue;
        }
        if (m.mode == M_LITERAL || m.mode == M_REG || (rn == 31 && (delta & 15)))
            break;
        int64_t start = member_start(&m, delta), end = start + memop_bytes(&m);
        int64_t nlo = start < lo ? start : lo, nhi = end > hi ? end : hi;
        if (nhi - nlo > GROUP_SPAN || !member_fits(x, &m, start))
            break;
        lo = nlo;
        hi = nhi;
        write |= !m.load;
        g->idx[g->n] = j;
        g->off[g->n] = (int32_t)start;
        g->reload[g->n] = clobbered;
        g->spill |= clobbered;
        clobbered = false;
        g->n++;
        if (m.mode != M_OFFSET) {
            if (m.wb_reg)
                break;
            delta += m.imm;
        }
        overwritten = m.load && !m.simd && (m.rt == rn || (m.pair && m.rt2 == rn));
    }
    if (g->n < 2) {
        g->n = 0;
        return -1;
    }
    g->next = g->n; /* starts only after the leader's check is emitted */
    g->lo = (int32_t)lo;
    g->bytes = (unsigned)(hi - lo);
    g->write = write;
    return slot;
}

/* A member's (including the leader's) access: host address = T2 + off, then base writeback */
static void emit_member(struct xlate *x, const struct memop *m, int64_t off)
{
    unsigned rn = m->rn == 31 ? REG_SP : m->rn, base = rn == REG_SP ? SP_HOST : rn, addr = T2;
    bool s1 = !m->simd && is_stolen(m->rt), s2 = m->pair && !m->simd && is_stolen(m->rt2);
    uint32_t insn = 0;
    if (!member_insn(m, off, &insn)) {
        addr = member_scratch(x, m);
        emit_add_const(x, addr, T2, off);
        member_insn(m, 0, &insn);
    }
    if (m->structure) {
        emit(x, insn | addr << 5);
    } else if (!m->pair) {
        unsigned rt = s1 ? T1 : m->rt;
        if (s1 && !m->load)
            emit_load_guest(x, T1, m->rt);
        emit(x, insn | addr << 5 | rt);
        if (s1 && m->load)
            emit_store_guest(x, T1, m->rt);
    } else {
        unsigned h1 = s1 ? T0 : m->rt, h2 = s2 ? T1 : m->rt2;
        if (!m->load) {
            if (s1)
                emit_load_guest(x, T0, m->rt);
            if (s2)
                emit_load_guest(x, T1, m->rt2);
        }
        emit(x, insn | h2 << 10 | addr << 5 | h1);
        if (m->load) {
            if (s1)
                emit_store_guest(x, T0, m->rt);
            if (s2)
                emit_store_guest(x, T1, m->rt2);
        }
    }
    if (m->mode == M_PRE || m->mode == M_POST) {
        if (m->wb_reg) {
            unsigned inc = m->rm;
            if (is_stolen(inc)) {
                emit_load_guest(x, T1, inc);
                inc = T1;
            }
            emit(x, enc_add_lsl(base, base, inc, 0));
        } else {
            emit_add_const(x, base, base, m->imm);
        }
        if (rn == REG_SP)
            sp_moved(x, m->wb_reg ? 1 : m->imm);
    }
    x->st->insns_memory++;
}

/* The leader: check the range the whole group covers, T2 = the base's host address (stored in
 * the slot when some member reloads it), then do the leader's own access */
static void emit_group(struct xlate *x, const struct memop *lead, uint64_t pc, int slot)
{
    struct group *g = &x->groups[slot];
    unsigned rn = lead->rn == 31 ? REG_SP : lead->rn, base = rn == REG_SP ? SP_HOST : rn;
    if (rn == REG_SP)
        emit_sp_check(x, pc);
    bool in_granule = rn == REG_SP && (g->lo & 15) + g->bytes <= 16;
    uint64_t addr;
    if (known_address(x, rn, g->lo, g->bytes, &addr)) {
        emit_add_const(x, T0, base, g->lo);
        emit_tlb_check_known(x, pc, addr, g->bytes, g->write, R_UNGROUP);
    } else {
        bool last_ready = emit_address_imm(x, base, g->lo, g->bytes, in_granule);
        emit_tlb_check(x, pc, g->bytes, g->write, in_granule, last_ready, R_UNGROUP);
    }
    emit(x, enc_add_lsl(T2, T2, base, 0));
    if (g->spill)
        emit(x, enc_str_x(T2, CTX, offsetof(struct tuxport_cpu, dbt_groups) + 8 * (size_t)slot));
    x->t2_group = slot;
    g->next = 1;
    emit_member(x, lead, g->off[0]);
}

/* ---- Exclusive accesses, acquire/release, LSE atomics (the engine map) ---- */

/*
 * Address of an atomic access, the same as Tier 0's atomic_target: natural alignment is required
 * (so it never crosses a page), and with SP as the base SP must be 16-byte aligned; otherwise
 * Tier 0 is left to report the alignment fault. Afterwards T0 = guest address and T2 = host
 * address.
 */
static void emit_atomic_address_off(struct xlate *x, unsigned rn, int64_t off, unsigned bytes,
                                    bool write, uint64_t pc)
{
    unsigned base = rn == 31 ? SP_HOST : rn;
    if (rn == 31)
        emit_sp_check(x, pc);
    else if (is_stolen(rn)) {
        emit_load_guest(x, T0, rn);
        base = T0;
    }
    if (off) {
        emit_add_const(x, T0, base, off);
        base = T0;
    }
    /* Match Tier 0's exclusive monitor even if LDXR and STXR use different tags. */
    emit(x, enc_and_low(T0, base, DATA_TAG_SHIFT));
    if (bytes > 1) {
        emit(x, enc_and_low(T1, T0, (unsigned)__builtin_ctz(bytes)));
        emit_to_stub(x, enc_cbnz_x(T1), FIX_IMM19, R_INTERP, pc);
    }
    emit_tlb_check(x, pc, bytes, write, true, false, R_INTERP);
    emit(x, enc_add_lsl(T2, T2, T0, 0));
    x->st->insns_memory++;
}

static void emit_atomic_address(struct xlate *x, unsigned rn, unsigned bytes, bool write, uint64_t pc)
{
    emit_atomic_address_off(x, rn, 0, bytes, write, pc);
}

struct atomic_operand {
    unsigned pos;
    bool in, out;
};

/* Execute verbatim on host address T2: stolen register operands are replaced by T0 and T1 */
static void emit_on_host_address(struct xlate *x, uint32_t insn, const struct atomic_operand *ops,
                                 unsigned n)
{
    const unsigned pool[2] = {T0, T1};
    uint32_t out = (insn & ~(31u << 5)) | T2 << 5;
    for (unsigned i = 0; i < n; i++) {
        unsigned r = (insn >> ops[i].pos) & 31;
        if (!is_stolen(r))
            continue;
        out = (out & ~(31u << ops[i].pos)) | pool[i] << ops[i].pos;
        if (ops[i].in)
            emit_load_guest(x, pool[i], r);
    }
    emit(x, out);
    for (unsigned i = 0; i < n; i++) {
        unsigned r = (insn >> ops[i].pos) & 31;
        if (is_stolen(r) && ops[i].out)
            emit_store_guest(x, pool[i], r);
    }
}

/* Write STXR's status: 0 for success, 1 for failure */
static void emit_status(struct xlate *x, unsigned rs, unsigned v)
{
    if (rs == 31)
        return;
    unsigned h = is_stolen(rs) ? T0 : rs;
    emit(x, enc_movz(h, v, 0));
    if (h == T0)
        emit_store_guest(x, T0, rs);
}

/*
 * LDXR/STXR are rewritten into a compare-and-swap (the same as Tier 0): LDXR records the address,
 * the size and the value it read; STXR first confirms that the exclusive marker is for the same
 * address and the same size, then does a CAS on the host address with ldaxr/stlxr. Between the two
 * instructions there is only register arithmetic, so the host's exclusive monitor is not cleared
 * by our own memory accesses.
 */
static bool translate_exclusive(struct xlate *x, uint32_t insn, uint64_t pc)
{
    unsigned size = FIELD(insn, 31, 30), bytes = 1u << size;
    unsigned rs = FIELD(insn, 20, 16), rn = FIELD(insn, 9, 5), rt = insn & 31;
    size_t addr = offsetof(struct tuxport_cpu, excl_addr), value = offsetof(struct tuxport_cpu, excl_value),
           len = offsetof(struct tuxport_cpu, excl_size);

    if (BIT(insn, 22)) { /* LDXR/LDAXR */
        unsigned h = (rt == 31 || is_stolen(rt)) ? T1 : rt;
        emit_atomic_address(x, rn, bytes, false, pc);
        emit(x, BIT(insn, 15) ? enc_ldar(size, h, T2) : enc_ldr_size(size, h, T2));
        emit(x, enc_str_x(T0, CTX, addr));
        emit(x, enc_str_x(h, CTX, value));
        emit(x, enc_movz(T0, bytes, 0));
        emit(x, enc_str_w(T0, CTX, len));
        if (h == T1 && rt != 31)
            emit_store_guest(x, T1, rt);
        return true;
    }

    emit_atomic_address(x, rn, bytes, true, pc);
    /*
     * The value to store is in a stolen register (libgcc's outline atomics all write it this way:
     * on Alpine they do not recognize LSE and always take the LL/SC loop): T0, T1 and T2 are all
     * in use in the CAS loop, so a guest register this instruction does not use is borrowed to
     * hold it. The borrowed register is first saved into its own slot in the cpu struct (unused
     * while translated code runs) and restored afterwards; nothing exits in between, and a host
     * fault does not restore guest state from the ucontext either (guard.c), so nobody can see
     * that it was borrowed. Previously the whole instruction went to Tier 0, exiting more than ten
     * thousand times on every node startup.
     */
    unsigned v = rt, borrow = 0;
    if (is_stolen(rt)) {
        while (borrow == rn || borrow == rs)
            borrow++;
        emit_store_guest(x, borrow, borrow);
        emit_load_guest(x, borrow, rt);
        v = borrow;
    }
    uint32_t *mismatch[2], *differ, *done;
    emit(x, enc_ldr_x(T1, CTX, addr));
    emit(x, enc_eor_lsr(T1, T1, T0, 0));
    mismatch[0] = here(x);
    emit(x, enc_cbnz_x(T1));
    emit(x, enc_ldr_w(T1, CTX, len));
    emit(x, enc_sub_imm(T1, T1, bytes, 0));
    mismatch[1] = here(x);
    emit(x, enc_cbnz_x(T1));
    uint32_t *loop = here(x);
    emit(x, enc_ldr_x(T1, CTX, value));
    emit(x, enc_ldaxr(size, T0, T2));
    emit(x, enc_eor_lsr(T0, T0, T1, 0));
    differ = here(x);
    emit(x, enc_cbnz_x(T0));
    emit(x, enc_stlxr(size, T1, v, T2));
    uint32_t *again = here(x);
    emit(x, enc_cbnz_x(T1));
    patch_local(x, again, loop);
    emit_status(x, rs, 0);
    done = here(x);
    emit(x, enc_b(0));
    patch_local(x, differ, here(x));
    emit(x, enc_clrex());
    patch_local(x, mismatch[0], here(x));
    patch_local(x, mismatch[1], here(x));
    emit_status(x, rs, 1);
    patch_local(x, done, here(x));
    if (v != rt)
        emit_load_guest(x, borrow, borrow);
    emit(x, enc_str_w(31, CTX, len));
    return true;
}

static bool is_atomic(uint32_t insn)
{
    return (insn & 0x3f000000) == 0x08000000 || ((insn & 0x3f200c00) == 0x38200000 && !BIT(insn, 26)) ||
           (insn & 0x3f200c00) == 0x19000000;
}

/* The encoding has been confirmed valid by tier0_defined; when false is returned, no instruction
 * has been emitted yet */
static bool translate_atomic(struct xlate *x, uint32_t insn, uint64_t pc)
{
    unsigned size = FIELD(insn, 31, 30), rn = FIELD(insn, 9, 5);
    /* LRCPC2's LDAPUR* and STLUR*: check base plus offset, and execute verbatim on the host
     * address with the offset removed */
    if ((insn & 0x3f200c00) == 0x19000000) {
        bool load = FIELD(insn, 23, 22) != 0;
        int64_t off = (int64_t)((uint64_t)FIELD(insn, 20, 12) << 55) >> 55;
        struct atomic_operand rt = {0, !load, load};
        emit_atomic_address_off(x, rn, off, 1u << size, !load, pc);
        emit_on_host_address(x, insn & ~(0x1ffu << 12), &rt, 1);
        return true;
    }
    if ((insn & 0x3f000000) == 0x08000000) {
        unsigned o2 = BIT(insn, 23), o1 = BIT(insn, 21);
        if (!o2 && !o1)
            return translate_exclusive(x, insn, pc);
        if (!o2) { /* LDXP/STXP (size ≥ 2) go to Tier 0; neither CASP register pair may be stolen */
            unsigned rs = FIELD(insn, 20, 16), rt = insn & 31;
            if (size >= 2 || is_stolen(rs) || is_stolen(rs + 1) || is_stolen(rt) || is_stolen(rt + 1))
                return false;
            emit_atomic_address(x, rn, 8u << size, true, pc);
            emit(x, (insn & ~(31u << 5)) | T2 << 5);
            return true;
        }
        if (!o1) { /* LDAR/STLR */
            bool load = BIT(insn, 22);
            struct atomic_operand rt = {0, !load, load};
            emit_atomic_address(x, rn, 1u << size, !load, pc);
            emit_on_host_address(x, insn, &rt, 1);
            return true;
        }
        /* CAS: Rs is the compare value and also receives the old value */
        struct atomic_operand ops[2] = {{16, true, true}, {0, true, false}};
        emit_atomic_address(x, rn, 1u << size, true, pc);
        emit_on_host_address(x, insn, ops, 2);
        return true;
    }
    /* LDAPR (LRCPC; Tier 0 has confirmed the host has it): executed verbatim on the host address
     * like LDAR; Rs is not an operand */
    if ((insn & 0x3ffffc00) == 0x38bfc000) {
        struct atomic_operand rt = {0, false, true};
        emit_atomic_address(x, rn, 1u << size, false, pc);
        emit_on_host_address(x, insn, &rt, 1);
        return true;
    }
    /* LD<op>, SWP */
    struct atomic_operand ops[2] = {{16, true, false}, {0, false, true}};
    emit_atomic_address(x, rn, 1u << size, true, pc);
    emit_on_host_address(x, insn, ops, 2);
    return true;
}

/* ---- Branches and system instructions ---- */
/* The five FlagM and FlagM2 instructions: RMIF, SETF8/SETF16, CFINV, XAFLAG, AXFLAG. They all
 * change only NZCV, and in translated code the guest's NZCV is the host's (advertising them means
 * the host has them), so they run verbatim */
static bool is_flagm(uint32_t insn)
{
    return (insn & 0xffe07c10) == 0xba000400 || (insn & 0xffffbc1f) == 0x3a00080d ||
           insn == 0xd500401f || insn == 0xd500403f || insn == 0xd500405f;
}


static int64_t sext(uint64_t v, unsigned bits)
{
    return (int64_t)(v << (64 - bits)) >> (64 - bits);
}

/*
 * Function calls (BL, BLR): a host bl skips the "landing pad" right after it, so the host's
 * return address stack records the landing pad; the landing pad is a link stub linked to the
 * block at the guest return address. The bl's target pushes (guest return address, landing pad)
 * onto the shadow return stack and sets guest x30 to the return address. The caller then emits
 * the code that jumps to the callee (T0 unchanged).
 *
 * When RET sees that the guest address at the top of the stack equals x30, it pops and jumps
 * back to the landing pad with a host ret, predicted by the host's return address stack; when
 * they differ (longjmp, signal handlers, coroutine switches) it looks up the jump cache as usual.
 * Every entry in the stack jumps to code that continues at the guest address it records (empty
 * entries jump to rs_miss), so whatever the guest sets x30 to, it never jumps to the wrong
 * place.
 */
static void emit_call(struct xlate *x, uint64_t pc)
{
    size_t rs = offsetof(struct tuxport_cpu, dbt_rs);
    uint32_t *site = here(x);
    emit(x, enc_bl(0));
    emit_exit_stub(x, R_CHAIN, pc + 4); /* landing pad */
    patch_local(x, site, here(x));
    emit(x, enc_ldr_x(T1, CTX, rs));
    emit(x, enc_add_imm(T2, T1, 16, 0));
    emit(x, enc_bfxil_low(T1, T2, RS_SHIFT));
    emit(x, enc_str_x(T1, CTX, rs));
    /* Host x30 is the landing pad, and the guest return address is in the data words at its end
     * (emit_link_stub) */
    if ((pc + 4) >> PACKED_PC_BITS == 0) {
        emit(x, enc_ldur_x(T2, 30, 12));
        emit(x, enc_and_low(T2, T2, PACKED_PC_BITS));
    } else {
        emit(x, enc_ldur_x(T2, 30, 16));
    }
    emit(x, enc_stp_x(T2, 30, T1, 0));
    emit(x, enc_mov(30, T2));
}

/* RET: when the top of the stack matches, pop and jump back to the landing pad; otherwise fall
 * through to the jump-cache lookup after it */
static void emit_predicted_return(struct xlate *x)
{
    size_t rs = offsetof(struct tuxport_cpu, dbt_rs);
    emit(x, enc_ldr_x(T1, CTX, rs));
    emit(x, enc_ldp_x(T0, T2, T1, 0));
    emit(x, enc_eor_lsr(T0, T0, 30, 0));
    uint32_t *miss = here(x);
    emit(x, enc_cbnz_x(T0));
    emit(x, enc_sub_imm(T0, T1, 16, 0));
    emit(x, enc_bfxil_low(T1, T0, RS_SHIFT));
    emit(x, enc_str_x(T1, CTX, rs));
    emit(x, enc_ret_reg(T2));
    patch_local(x, miss, here(x));
}

/* Replace a conditional branch's register operand with a directly readable host register */
static uint32_t branch_operand(struct xlate *x, uint32_t insn)
{
    unsigned rt = insn & 31;
    if (!is_stolen(rt))
        return insn;
    emit_load_guest(x, T0, rt);
    return (insn & ~31u) | T0;
}

/* Copy an X register operand to T0. Register 31 denotes XZR here. */
static void emit_xreg_to_t0(struct xlate *x, unsigned reg)
{
    if (reg == 31)
        emit(x, enc_movz(T0, 0, 0));
    else if (is_stolen(reg))
        emit_load_guest(x, T0, reg);
    else
        emit(x, enc_mov(T0, reg));
}

/* A changed FPCR needs Tier 0 to set the guest mode for the next entry. */
static void emit_fpcr_write_check(struct xlate *x, unsigned src, uint64_t pc)
{
    emit_mov_const(x, T1, fpcr_write_mask());
    emit(x, enc_and_reg(T1, src, T1));
    emit(x, enc_ldr_x(T0, CTX, offsetof(struct tuxport_cpu, fpcr)));
    emit(x, enc_eor_lsr(T1, T1, T0, 0));
    emit_to_stub(x, enc_cbnz_x(T1), FIX_IMM19, R_INTERP, pc);
}

static bool translate_branch_sys(struct xlate *x, uint32_t insn, uint64_t pc)
{
    if ((insn & 0x7c000000) == 0x14000000) { /* B / BL */
        uint64_t target = pc + (uint64_t)(sext(FIELD(insn, 25, 0), 26) * 4);
        if (BIT(insn, 31))
            emit_call(x, pc);
        emit_exit_stub(x, chain_reason(pc, target), target);
        return false;
    }
    if ((insn & 0xff000010) == 0x54000000 || (insn & 0x7e000000) == 0x34000000) { /* B.cond/CBZ/CBNZ */
        uint64_t target = pc + (uint64_t)(sext(FIELD(insn, 23, 5), 19) * 4);
        /* Bits 4:0 of B.cond are the condition code, kept as is; CBZ/CBNZ's are a register */
        uint32_t base = BIT(insn, 30) ? insn : branch_operand(x, insn);
        emit_to_stub(x, base & ~(0x7ffffu << 5), FIX_IMM19, chain_reason(pc, target), target);
        return true; /* the not-taken side is translated next; the taken side is a side exit */
    }
    if ((insn & 0x7e000000) == 0x36000000) { /* TBZ/TBNZ */
        uint64_t target = pc + (uint64_t)(sext(FIELD(insn, 18, 5), 14) * 4);
        uint32_t base = branch_operand(x, insn) & ~(0x3fffu << 5);
        emit_to_stub(x, base, FIX_IMM14, chain_reason(pc, target), target);
        return true;
    }
    if (insn == 0xd4000001) { /* SVC #0 */
        emit_exit_stub(x, R_SVC, pc + 4);
        return false;
    }
    if ((insn & 0xfffff01f) == 0xd503201f) /* HINT: a NOP, as in Tier 0 */
        return true;
    if (is_flagm(insn)) { /* CFINV, XAFLAG, AXFLAG: whitelist already confirmed by Tier 0 */
        emit(x, insn);
        x->st->insns_copied++;
        return true;
    }
    /* Barriers (including SB): the whitelist has already been confirmed by Tier 0 */
    if ((insn & 0xfffff01f) == 0xd503301f) {
        unsigned op2 = FIELD(insn, 7, 5);
        if (op2 == 2) { /* CLREX: clear the guest's exclusive marker */
            emit(x, enc_str_w(31, CTX, offsetof(struct tuxport_cpu, excl_size)));
            x->st->insns_remapped++;
            return true;
        }
        /* Complete the barrier before resuming at pc + 4 with any changed blocks retired. */
        emit(x, insn);
        if (op2 == 4 || op2 == 6) {
            emit(x, enc_ldr_x(T0, CTX, offsetof(struct tuxport_cpu, ivau_page)));
            emit_to_stub(x, enc_cbnz_x(T0), FIX_IMM19, R_CODE, pc + 4);
        }
        x->st->insns_copied++;
        return true;
    }
    if ((insn & 0xffd00000) == 0xd5100000) { /* MRS/MSR */
        bool read = BIT(insn, 21);
        unsigned rt = insn & 31;
        unsigned sysreg = FIELD(insn, 20, 5);
        switch (sysreg) {
        case SYSREG_FPSR:
            /* Read the flags raised by translated FP instructions. Tier 0 masks writes. */
            if (!read)
                return fallback(x, pc);
            /* fallthrough */
        case SYSREG_NZCV:
        /* tier0_defined rejects writes to the timers. */
        case SYSREG_CNTFRQ_EL0:
        case SYSREG_CNTVCT_EL0:
        case SYSREG_CNTVCTSS_EL0: {
            struct operands o = {.n = 1, .pos = {0}, .sp = {false}, .writes_rd = read};
            return emit_operands(x, insn, &o) || fallback(x, pc);
        }
        case SYSREG_FPCR:
        case SYSREG_TPIDR_EL0: {
            bool fpcr = sysreg == SYSREG_FPCR;
            size_t off = fpcr ? offsetof(struct tuxport_cpu, fpcr) : offsetof(struct tuxport_cpu, tpidr_el0);
            unsigned h = is_stolen(rt) ? T0 : rt;
            if (!read) {
                if (h == T0)
                    emit_load_guest(x, T0, rt);
                if (fpcr)
                    emit_fpcr_write_check(x, h, pc);
                else
                    emit(x, enc_str_x(h, CTX, off));
            } else if (rt != 31) {
                emit(x, enc_ldr_x(h, CTX, off));
                if (h == T0)
                    emit_store_guest(x, T0, rt);
            }
            x->st->insns_remapped++;
            return true;
        }
        case SYSREG_DCZID_EL0: /* read-only, writes already rejected by tier0_defined */
            if (rt != 31) {
                unsigned h = is_stolen(rt) ? T0 : rt;
                emit(x, enc_movz(h, DCZID_VALUE, 0));
                if (h == T0)
                    emit_store_guest(x, T0, rt);
            }
            x->st->insns_remapped++;
            return true;
        default:
            return fallback(x, pc);
        }
    }
    /* DC ZVA: zero the whole block containing the address; a block is always within one page */
    if ((insn & 0xffffffe0) == 0xd50b7420) {
        emit_xreg_to_t0(x, insn & 31);
        emit(x, enc_and_clear_low(T0, T0, 6));
        _Static_assert(DCZ_BLOCK == 64, "the block size of DC ZVA");
        emit_tlb_check(x, pc, DCZ_BLOCK, true, false, false, R_INTERP);
        emit(x, enc_add_lsl(T2, T2, T0, 0));
        for (int off = 0; off < DCZ_BLOCK; off += 16)
            emit(x, enc_stp_x(31, 31, T2, off));
        x->st->insns_memory++;
        return true;
    }
    /*
     * DC CVAC, DC CVAU, DC CIVAC, DC CVAP, DC CVADP, IC IVAU: as in Tier 0, the address is only
     * checked for read access (the DC CVAP and CVADP features have been confirmed by Tier 0).
     * ic ivau does not exit on every instruction: it records its page (a JIT flushing its cache
     * executes dozens of them per page), and the next dsb or isb exits so that dbt_run
     * invalidates the page's translations in place, in one go; when another page is already
     * recorded, Tier 0 executes this one, and dbt_run invalidates both pages right after it.
     */
    uint32_t cmo = insn & 0xffffffe0;
    if (cmo == 0xd50b7a20 || cmo == 0xd50b7b20 || cmo == 0xd50b7e20 || cmo == 0xd50b7520 ||
        cmo == 0xd50b7c20 || cmo == 0xd50b7d20) {
        emit_xreg_to_t0(x, insn & 31);
        emit_tlb_check(x, pc, 1, false, true, false, R_INTERP);
        if (cmo == 0xd50b7520) {
            emit(x, enc_and_ror(T0, T0, DATA_TAG_SHIFT - TUXPORT_PAGE_SHIFT, 64 - TUXPORT_PAGE_SHIFT));
            /* Zero means no pending page. Tier 0 returns this address explicitly. */
            emit_to_stub(x, enc_cbz_x(T0), FIX_IMM19, R_INTERP, pc);
            emit(x, enc_ldr_x(T1, CTX, offsetof(struct tuxport_cpu, ivau_page)));
            emit(x, enc_eor_lsr(T2, T1, T0, 0));
            uint32_t *same_page = here(x);
            emit(x, enc_cbz_x(T2));
            emit_to_stub(x, enc_cbnz_x(T1), FIX_IMM19, R_INTERP, pc);
            emit(x, enc_str_x(T0, CTX, offsetof(struct tuxport_cpu, ivau_page)));
            patch_local(x, same_page, here(x));
        }
        x->st->insns_memory++;
        return true;
    }
    uint32_t reg_branch = insn & 0xfffffc1f;
    if (reg_branch == 0xd61f0000 || reg_branch == 0xd63f0000 || reg_branch == 0xd65f0000) {
        /* BR/BLR/RET: put the target in T0 first, then look up the jump cache */
        unsigned rn = FIELD(insn, 9, 5);
        if (reg_branch == 0xd65f0000 && rn == 30)
            emit_predicted_return(x);
        emit_xreg_to_t0(x, rn);
        if (reg_branch == 0xd63f0000)
            emit_call(x, pc);
        emit(x, enc_ldr_x(T1, CTX, offsetof(struct tuxport_cpu, dbt_jump_cache)));
        emit(x, enc_ubfx(T2, T0, 2, JC_BITS));
        emit(x, enc_add_lsl(T1, T1, T2, 4));
        emit(x, enc_ldp_x(T2, T1, T1, 0));
        emit(x, enc_eor_lsr(T2, T2, T0, 0));
        emit_to_stub(x, enc_cbnz_x(T2), FIX_IMM19, R_LOOKUP, 0);
        emit(x, enc_br(T1));
        return false;
    }
    return fallback(x, pc);
}

/*
 * Ask Tier 0 whether it recognizes this instruction (the two tiers' whitelists must agree, see
 * the engine map). The answer depends only on the instruction word and is cached by it in `cache`
 * (direct-mapped, each entry is 1 << 40 | instruction word << 1 | answer): Tier 0 has to execute
 * the instruction to find out, and asking once for every instruction translated at java startup
 * took two percentage points.
 */
#define DEFINED_CACHE 4096
static bool tier0_defined(struct tuxport_cpu *probe, uint64_t *cache, uint32_t insn, uint64_t pc)
{
    uint64_t *e = &cache[(insn * UINT32_C(0x9e3779b1)) >> 20];
    if ((*e >> 1) == ((uint64_t)1 << 39 | insn))
        return *e & 1;
    probe->pc = pc;
    probe->sp = 0;
    bool ok = tuxport_interp_exec(probe, insn) != TUXPORT_EXIT_UNDEF;
    *e = (uint64_t)1 << 40 | (uint64_t)insn << 1 | ok;
    return ok;
}

/*
 * SIMD&FP data processing: the whitelist decision shares tuxport_simd_dp_decode with Tier 0. V
 * registers need no rewriting; only general-register operands may be stolen registers. It does
 * not go through tier0_defined, to avoid generating a native stub for every instruction.
 */
static bool translate_simd_dp(struct xlate *x, uint32_t insn)
{
    struct native_desc nd;
    if (!tuxport_has(TUXPORT_HWCAP_FP | TUXPORT_HWCAP_ASIMD) || !tuxport_native_available() ||
        !tuxport_simd_dp_decode(insn, &nd))
        return false;
    struct operands o = {.n = 0, .writes_rd = false};
    for (unsigned i = 0; i < nd.n; i++) {
        add_operand(&o, nd.gpr[i].pos, false);
        if (nd.gpr[i].pos == 0 && nd.gpr[i].store)
            o.writes_rd = true;
    }
    return emit_operands(x, insn, &o);
}

/* Translate one instruction; returning false means the block ends here */
static bool translate_insn_body(struct xlate *x, uint32_t insn, uint64_t pc);


/*
 * Which registers have known values after this instruction (xlate.konst): ADR/ADRP set constants,
 * adding an immediate to a known value keeps it known, and other instructions that may write
 * general registers invalidate them. Anything uncertain is treated as writing all of them.
 */
static void konst_update(struct xlate *x, uint32_t insn, uint64_t pc)
{
    unsigned rd = insn & 31, rn = FIELD(insn, 9, 5);
    uint32_t kill = 1u << rd;
    if (is_flagm(insn)) /* writes only flags */
        return;
    if (FIELD(insn, 28, 24) == 0x10) { /* ADR/ADRP */
        uint64_t imm = ((uint64_t)FIELD(insn, 23, 5) << 2) | FIELD(insn, 30, 29);
        int64_t off = (int64_t)(imm << 43) >> 43;
        if (rd != 31) {
            x->konst |= 1u << rd;
            x->kval[rd] = BIT(insn, 31) ? (pc & ~UINT64_C(0xfff)) + ((uint64_t)off << 12) : pc + (uint64_t)off;
        }
        return;
    }
    if ((insn & 0xff800000) == 0x91000000 && rd != 31 && rn != 31 && (x->konst >> rn & 1)) {
        /* ADD Xd, Xn, #imm{, lsl #12} (does not set flags) */
        x->kval[rd] = x->kval[rn] + ((uint64_t)FIELD(insn, 21, 10) << (BIT(insn, 22) ? 12 : 0));
        x->konst |= 1u << rd;
        return;
    }
    switch (FIELD(insn, 28, 25)) {
    case 0x8:
    case 0x9:
    case 0x5:
    case 0xd:
    case 0x7:
    case 0xf: /* data processing (SIMD ones may also write general registers, e.g. UMOV) */
        break;
    case 0x4:
    case 0x6:
    case 0xc:
    case 0xe: {
        struct memop m;
        if (is_atomic(insn)) { /* Rt, Rs, plus one more for pairs */
            unsigned rs = FIELD(insn, 20, 16);
            kill |= 3u << rd | 3u << rs;
            break;
        }
        if (!(BIT(insn, 26) ? decode_simd_memop(insn, pc, &m) : decode_memop(insn, pc, &m))) {
            kill = ~0u;
            break;
        }
        kill = 0;
        if (m.load && !m.simd && !m.prefetch) {
            kill |= 1u << m.rt;
            if (m.pair)
                kill |= 1u << m.rt2;
        }
        if (m.mode == M_PRE || m.mode == M_POST)
            kill |= 1u << m.rn;
        break;
    }
    default:
        /* Conditional branches, CBZ, TBZ, HINT and barriers write no registers; the rest (MRS,
         * BL and so on) are uncertain */
        if ((insn & 0xff000010) == 0x54000000 || (insn & 0x7e000000) == 0x34000000 ||
            (insn & 0x7e000000) == 0x36000000 || (insn & 0xfffff01f) == 0xd503201f ||
            (insn & 0xfffff01f) == 0xd503301f)
            kill = 0;
        else
            kill = ~0u;
        break;
    }
    x->konst &= ~kill;
}

static bool translate_insn(struct xlate *x, uint32_t insn, uint64_t pc)
{
    bool r = translate_insn_body(x, insn, pc);
    konst_update(x, insn, pc);
    return r;
}

static bool translate_insn_body(struct xlate *x, uint32_t insn, uint64_t pc)
{
    if ((FIELD(insn, 28, 25) & 7) == 7)
        return translate_simd_dp(x, insn) || fallback(x, pc);
    if (!tier0_defined(x->probe, x->defined, insn, pc))
        return fallback(x, pc);
    switch (FIELD(insn, 28, 25)) {
    case 0x8:
    case 0x9:
        if (FIELD(insn, 28, 24) == 0x10) {
            emit_pcrel(x, insn, pc);
            return true;
        }
        /* fallthrough */
    case 0x5:
    case 0xd: {
        struct operands o;
        if (!dp_operands(insn, &o) || !emit_operands(x, insn, &o))
            return fallback(x, pc);
        if (o.writes_rd && o.sp[0] && (insn & 31) == 31) { /* wrote SP */
            uint32_t op = insn & 0xff800000;
            if ((op == 0x91000000 || op == 0xd1000000) && FIELD(insn, 9, 5) == 31)
                sp_moved(x, (int64_t)FIELD(insn, 21, 10) << (BIT(insn, 22) ? 12 : 0));
            else
                x->sp_aligned = false;
        }
        return true;
    }
    case 0x4:
    case 0x6:
    case 0xc:
    case 0xe: {
        if (is_atomic(insn))
            return translate_atomic(x, insn, pc) || fallback(x, pc);
        struct memop m;
        if (!(BIT(insn, 26) ? decode_simd_memop(insn, pc, &m) : decode_memop(insn, pc, &m)))
            return fallback(x, pc);
        if (m.prefetch)
            return true;
        for (int i = 0; i < GROUP_SLOTS; i++) {
            struct group *g = &x->groups[i];
            if (g->next >= g->n || x->idx != g->idx[g->next])
                continue;
            if (g->reload[g->next]) {
                size_t off = offsetof(struct tuxport_cpu, dbt_groups) + 8 * (size_t)i;
                emit(x, enc_ldr_x(T2, CTX, off));
                x->t2_group = i;
            } else if (x->t2_group != i) {
                tuxport_guard_fail(); /* plan_group ensures nothing used T2 before a member not
                                      * marked reload */
            }
            emit_member(x, &m, g->off[g->next++]);
            x->st->insns_grouped++;
            return true;
        }
        emit_memop(x, &m, pc);
        return true;
    }
    case 0xa:
    case 0xb:
        return translate_branch_sys(x, insn, pc);
    default:
        return fallback(x, pc);
    }
}

/* ---- Translation table and code cache ---- */

/* Point a conditional branch directly at the target block; if that is out of its offset range,
 * leave it unchanged (it still goes through the stub) and return false.
 * Does not sync the instruction cache */
static bool relink_word(const struct tuxport_dbt *d, uint32_t *site, const uint32_t *target)
{
    /* Read through the execution address: a read fault on the writable alias makes the host also
     * map a dozen or so nearby pages (fault-around), which are not recorded in struct written */
    uint32_t insn = *site;
    bool test = (insn & 0x7e000000) == 0x36000000; /* TBZ/TBNZ: 14-bit offset, others 19 bits */
    intptr_t off = target - site, limit = (intptr_t)1 << (test ? 13 : 18);
    if (off < -limit || off >= limit)
        return false;
    uint32_t mask = (test ? 0x3fffu : 0x7ffffu) << 5;
    *rw(d, site) = (insn & ~mask) | (((uint32_t)off << 5) & mask);
    return true;
}

/*
 * The instance and its tables are allocated with mmap, which guarantees zeroed pages committed on
 * demand: one instance per address space, one jump cache per thread, and the tables are allocated
 * at their largest size, with most pages never touched. calloc does not necessarily do this (glibc
 * raises the mmap threshold after a block of the same size has been freed, and then allocates from
 * the heap and zeroes byte by byte).
 */
static void *zalloc(size_t bytes)
{
    void *p = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    return p == MAP_FAILED ? NULL : p;
}

/* Return the first `bytes` bytes of memory from zalloc to the host; later reads see 0 */
static void release_pages(void *p, size_t bytes)
{
    size_t page = (size_t)sysconf(_SC_PAGESIZE);
    if (bytes)
        madvise(p, (bytes + page - 1) & ~(page - 1), MADV_DONTNEED);
}

static void zfree(void *p, size_t bytes)
{
    if (p)
        munmap(p, bytes);
}

#define TABLE_BYTES(bits) (sizeof(struct table) + (sizeof(struct slot) << (bits)))

/* Index of a key: a tagged key is hashed by its pc, in the same probe sequence as the shared block
 * of the same pc (TAG_SHIFT) */
static size_t table_index(const struct table *t, uint64_t key)
{
    return (size_t)((((key & PC_MASK) >> 2) * GOLDEN) >> t->shift);
}

static size_t table_mask(const struct table *t)
{
    return (size_t)t->mask;
}

/* Allocate an empty table of 2^bits entries */
static struct table *table_new(unsigned bits)
{
    struct table *t = zalloc(TABLE_BYTES(bits));
    if (t) {
        t->bits = bits;
        t->shift = 64 - bits;
        t->mask = (UINT64_C(1) << bits) - 1;
    }
    return t;
}

/*
 * Find in t the block executed at pc by the member with tag `tag` (0 when not in a group): a
 * shared one or one private to it (TAG_SHIFT), with `*found` set to the found block's tag; when
 * both exist (private blocks translated before the registered range grew), either is correct.
 * Called directly while holding the lock; without the lock, through table_lookup. Misaligned pcs
 * and pcs beyond 48 bits have no blocks (Tier 0 reports the fault), and must not be compared with
 * the tombstones' JC_EMPTY or other members' keys either
 */
static uint32_t *table_find_for(const struct table *t, uint64_t pc, unsigned tag, unsigned *found)
{
    *found = 0;
    if (pc & (3 | ~PC_MASK))
        return NULL;
    uint64_t mine = tagged(pc, tag);
    for (size_t i = table_index(t, pc);; i = (i + 1) & table_mask(t)) {
        /* Insertion writes the key before code: once code reads non-NULL, the key is the matching
         * one */
        uint32_t *code = __atomic_load_n(&t->slots[i].code, __ATOMIC_ACQUIRE);
        if (!code)
            return NULL;
        uint64_t key = __atomic_load_n(&t->slots[i].pc, __ATOMIC_RELAXED);
        if (key == pc || key == mine) {
            *found = key == pc ? 0 : tag;
            return code;
        }
    }
}

/*
 * The dispatcher and the lookup routine (emit_lookup) look up the table without the lock: while
 * the cpu's dbt_lookup_seq is odd, a replaced old table is not freed (table_publish)
 */
static uint32_t *table_lookup(struct tuxport_dbt *d, struct tuxport_cpu *c, uint64_t pc, unsigned *found)
{
    __atomic_store_n(&c->dbt_lookup_seq, c->dbt_lookup_seq + 1, __ATOMIC_SEQ_CST);
    uint32_t *code = table_find_for(__atomic_load_n(&d->table, __ATOMIC_SEQ_CST), pc, c->dbt_tag, found);
    __atomic_store_n(&c->dbt_lookup_seq, c->dbt_lookup_seq + 1, __ATOMIC_RELEASE);
    return code;
}

/* Install the new table, and free the old one once the threads looking it up have finished. Lock
 * held */
static void table_publish(struct tuxport_dbt *d, struct table *table)
{
    struct table *old = d->table;
    __atomic_store_n(&d->table, table, __ATOMIC_SEQ_CST);
    /* Threads not in dbt_run do not look up the table; those entering later register their cpu
     * before reading table, so they read the new table */
    for (struct tuxport_dbt_thread *t = d->threads; t; t = t->next) {
        struct tuxport_cpu *c = __atomic_load_n(&t->cpu, __ATOMIC_SEQ_CST);
        if (!c)
            continue;
        unsigned seq = __atomic_load_n(&c->dbt_lookup_seq, __ATOMIC_SEQ_CST);
        while ((seq & 1) && __atomic_load_n(&c->dbt_lookup_seq, __ATOMIC_ACQUIRE) == seq)
            __asm__ volatile("yield");
    }
    zfree(old, TABLE_BYTES(old->bits));
}

/*
 * A block just built whose instruction cache is not synced yet (start pc `pc`; a private block
 * belongs to member `tag`): exits whose target is already translated are linked directly, the
 * same as the linking the dispatcher does the first time an exit is taken (R_CHAIN in dbt_run).
 * Shared blocks only link to shared blocks (TAG_SHIFT). The changes are synced together with the
 * whole block. Lock held
 */
static void link_block(const struct tuxport_dbt *d, uint32_t *code, uint64_t pc, unsigned tag,
                       const struct link *links, unsigned n)
{
    for (unsigned i = 0; i < n; i++) {
        const struct link *l = &links[i];
        unsigned found;
        uint32_t *target = l->pc == pc ? code : table_find_for(d->table, l->pc, tag, &found);
        if (!target)
            continue;
        if (l->stub & LINK_FWD)
            target += ENTRY_CHECK_WORDS;
        uint32_t *site = code + (l->stub & ~LINK_FWD);
        *rw(d, site) = enc_b((intptr_t)target - (intptr_t)site);
        if (l->branch != NO_BRANCH)
            relink_word(d, code + l->branch, target);
    }
}

static unsigned page_bucket(uint64_t page)
{
    return (unsigned)((page * UINT64_C(0x9e3779b97f4a7c15)) >> (64 - PAGE_BUCKET_BITS));
}

/* Special value in table_used: this block is invalidated and was not put in when the table was
 * rebuilt */
#define SLOT_NONE UINT32_MAX

/* Put the live blocks into a new table of 2^bits entries in their original insertion order,
 * leaving out invalidated blocks (tombstones), with table_used and blocks keeping matching
 * indices; returns false if allocation fails, leaving the original table unchanged. Lock held */
static void table_rebuild(struct tuxport_dbt *d, struct table *table);

static bool table_resize(struct tuxport_dbt *d, unsigned bits)
{
    struct table *table = table_new(bits);
    if (!table)
        return false;
    table_rebuild(d, table);
    return true;
}

/* Fill the newly allocated table from blocks and install it (table_resize) */
static void table_rebuild(struct tuxport_dbt *d, struct table *table)
{
    size_t occupied = 0;
    for (size_t k = 0; k < d->table_count; k++) {
        const struct block *b = &d->blocks[k];
        if (!b->code || b->parked) {
            d->table_used[k] = SLOT_NONE;
            continue;
        }
        size_t i = table_index(table, block_key(b));
        while (table->slots[i].code)
            i = (i + 1) & table_mask(table);
        table->slots[i] = (struct slot){block_key(b), b->code};
        d->table_used[k] = (uint32_t)i;
        occupied++;
    }
    d->table_occupied = occupied;
    d->table_dead = 0;
    table_publish(d, table);
}

/* Insert only when pc is not in the table (a private block belongs to member `tag`), using only
 * empty slots (see struct table). Lock held */
static void table_insert(struct tuxport_dbt *d, uint64_t pc, unsigned tag, uint32_t *code,
                         const uint32_t *guest, uint32_t nguest)
{
    struct table *t = d->table;
    size_t i = table_index(t, tagged(pc, tag));
    while (t->slots[i].code)
        i = (i + 1) & table_mask(t);
    d->table_occupied++;
    __atomic_store_n(&t->slots[i].pc, tagged(pc, tag), __ATOMIC_RELAXED);
    __atomic_store_n(&t->slots[i].code, code, __ATOMIC_RELEASE);
    unsigned h = page_bucket(pc >> TUXPORT_PAGE_SHIFT);
    memcpy(d->words + d->words_used, guest, nguest * 4);
    d->blocks[d->table_count] =
        (struct block){pc, code, d->page_head[h], (uint32_t)d->words_used, (uint8_t)nguest, 0, (uint16_t)tag, 0};
    d->words_used += nguest;
    d->table_used[d->table_count++] = (uint32_t)i;
    d->page_head[h] = (uint32_t)d->table_count;
}

/*
 * Remove the block with key `key` from the translation table, leaving a tombstone that never
 * hits. Old entries in the threads' jump caches need not be removed: they point to the
 * invalidated block's entry, which jumps to the forwarding stub. Lock held
 */
static void table_remove(struct tuxport_dbt *d, uint64_t key)
{
    struct table *t = d->table;
    for (size_t i = table_index(t, key); t->slots[i].code; i = (i + 1) & table_mask(t)) {
        if (t->slots[i].pc == key) {
            __atomic_store_n(&t->slots[i].pc, JC_EMPTY, __ATOMIC_RELEASE);
            d->table_dead++;
            break;
        }
    }
}

/*
 * The pc of jump-cache entry i when it is empty: misaligned, and the index computed from it is
 * not i, so wherever the guest jumps it never hits an empty entry (translated code jumps straight
 * to code on a hit, and an empty entry's code is NULL). If they all used the same misaligned
 * value, a jump to that address would hit the entry at its index
 */
static uint64_t jc_empty(unsigned i)
{
    return (uint64_t)((i + 1) & (JC_SIZE - 1)) << 2 | 1;
}

static void clear_jump_cache(struct tuxport_dbt_thread *t)
{
    for (unsigned i = 0; i < JC_SIZE; i++)
        t->jc[i] = (struct slot){jc_empty(i), NULL};
    t->jc_dirty = false;
}

/* Replace the whole shadow return stack with empty entries: the code holding the landing pads is
 * about to be overwritten */
static void rs_reset(struct tuxport_dbt_thread *t)
{
    for (unsigned i = 0; i < RS_ENTRIES; i++) {
        t->rs[i][0] = 1; /* misaligned, never a RET target; if matched, rs_miss still looks up */
        t->rs[i][1] = (uint64_t)(uintptr_t)t->d->rs_miss;
    }
}

/* The code cache has been fully flushed (generation changed): the thread's own jump cache and
 * shadow return stack point to old code */
static void thread_catch_up(struct tuxport_dbt_thread *t)
{
    uint64_t g = __atomic_load_n(&t->d->generation, __ATOMIC_ACQUIRE);
    if (t->generation != g) {
        t->generation = g;
        rs_reset(t);
        if (t->jc_dirty)
            clear_jump_cache(t);
    }
}

/*
 * blocks is full, but mostly with invalidated blocks (a JIT repeatedly rewriting and
 * retranslating the same code): remove their records, then rebuild the translation table at the
 * same size to remove the tombstones, with no full flush needed. The invalidated blocks' code stays
 * in the code cache, and their entries still jump to the forwarding stubs. Lock held; returns
 * false if allocating the new table fails
 */
static bool compact_blocks(struct tuxport_dbt *d)
{
    struct table *table = table_new(d->table->bits);
    if (!table)
        return false;
    size_t n = 0, words = 0;
    memset(d->page_head, 0, sizeof(*d->page_head) << PAGE_BUCKET_BITS);
    for (size_t k = 0; k < d->table_count; k++) {
        struct block b = d->blocks[k];
        if (!b.code)
            continue;
        memmove(d->words + words, d->words + b.words, b.nguest * 4);
        b.words = (uint32_t)words;
        words += b.nguest;
        unsigned h = page_bucket(b.pc >> TUXPORT_PAGE_SHIFT);
        b.next = d->page_head[h];
        d->blocks[n++] = b;
        d->page_head[h] = (uint32_t)n;
    }
    d->table_count = n;
    d->words_used = words;
    d->blocks_dead = 0;
    table_rebuild(d, table);
    return true;
}

/*
 * Discard all translations. Lock held, and no other thread is in translated code or looking up
 * the table without the lock: they have all stopped in the dispatcher, or are not in dbt_run
 * (flush_world).
 */
static void flush_locked(struct tuxport_dbt *d)
{
    if (d->used_words > d->high_words)
        d->high_words = d->used_words;
    struct table *t = d->table;
    for (size_t i = 0; i < d->table_count; i++) {
        if (d->table_used[i] != SLOT_NONE)
            t->slots[d->table_used[i]] = (struct slot){0, NULL};
        d->page_head[page_bucket(d->blocks[i].pc >> TUXPORT_PAGE_SHIFT)] = 0;
        d->page_parked[page_bucket(d->blocks[i].pc >> TUXPORT_PAGE_SHIFT)] = 0;
    }
    d->table_count = d->table_occupied = d->table_dead = d->blocks_dead = d->nparked = 0;
    d->words_used = 0;
    d->stale = false;
    d->nsources = d->ntrusted = d->nparkable = 0;
    d->flush_wanted = false;
    d->used_words = d->routine_words;
    __atomic_store_n(&d->generation, d->generation + 1, __ATOMIC_RELEASE);
    d->st->flushes++;
}

/*
 * Keep invalidated blocks from being executed: the entry (two places, before and after the kick
 * check) is changed to jump to a forwarding stub. The forwarding stub exits with R_CHAIN and the
 * old entry as the patch site, so after retranslation the old entry jumps directly to the new
 * block, and branches of other blocks linked to the old entry need not be found and rewritten.
 * Another thread may be executing this block: it still runs to the end of the block, just as on
 * hardware another core may still execute old instructions before a context synchronization.
 *
 * The forwarding stubs must be synced before the entries can jump to them, and every sync waits
 * for the instruction cache's invalidation broadcast to complete, so blocks are done in batches:
 * retire_add writes the forwarding stub, and retire_flush syncs all forwarding stubs, changes all
 * entries, then syncs once more.
 * Lock held
 */
#define RETIRE_BATCH 32

struct retire_batch {
    unsigned n;
    struct block *blocks[RETIRE_BATCH];
    bool park[RETIRE_BATCH];
    uint32_t *fwd_start;
};

static void retire_flush(struct tuxport_dbt *d, struct retire_batch *r)
{
    if (!r->n)
        return;
    void *start[RETIRE_BATCH], *end[RETIRE_BATCH];
    start[0] = r->fwd_start;
    end[0] = d->cache + d->used_words;
    sync_written(d, start, end, 1);
    uint32_t *fwd = r->fwd_start;
    for (unsigned i = 0; i < r->n; i++) {
        uint32_t *code = r->blocks[i]->code;
        if (r->park[i])
            r->blocks[i]->first = code[ENTRY_CHECK_WORDS];
        *rw(d, code) = enc_b((intptr_t)fwd - (intptr_t)code);
        *rw(d, code + ENTRY_CHECK_WORDS) = enc_b(-ENTRY_CHECK_WORDS * 4);
        start[i] = code;
        end[i] = code + ENTRY_CHECK_WORDS + 1;
        fwd += 16;
    }
    sync_written(d, start, end, r->n);
    for (unsigned i = 0; i < r->n; i++) {
        struct block *b = r->blocks[i];
        table_remove(d, block_key(b));
        if (r->park[i]) {
            b->parked = 1;
            d->nparked++;
            d->page_parked[page_bucket(b->pc >> TUXPORT_PAGE_SHIFT)]++;
            continue;
        }
        b->code = NULL;
        d->blocks_dead++;
        d->st->invalidated++;
    }
    r->n = 0;
}

/* Invalidate a parked block: its entry already jumps to the forwarding stub */
static void kill_parked(struct tuxport_dbt *d, struct block *b)
{
    b->parked = 0;
    b->code = NULL;
    d->nparked--;
    d->page_parked[page_bucket(b->pc >> TUXPORT_PAGE_SHIFT)]--;
    d->blocks_dead++;
    d->st->invalidated++;
}

/* Invalidate b: write its forwarding stub first (16 words each), and leave the entry for
 * retire_flush to change together with the others. With `park` it is only parked (revive).
 * Returns false if the code cache has no room for the forwarding stub */
static bool retire_add(struct tuxport_dbt *d, struct retire_batch *r, struct block *b, bool park)
{
    if (r->n == RETIRE_BATCH)
        retire_flush(d, r);
    if (d->used_words + 16 > d->cache_words)
        return false;
    struct xlate fwd = {.d = d, .buf = d->cache + d->used_words, .wdelta = d->rw_delta, .st = d->st},
                 *x = &fwd;
    emit_mov_const(x, T1, (uint64_t)(uintptr_t)b->code);
    emit_mov_const(x, T0, b->pc);
    emit(x, enc_movz(T2, R_CHAIN | (b->tag ? 0 : LINK_SHARED), 0));
    emit(x, enc_b((intptr_t)d->exit - (intptr_t)here(x)));
    while (x->n < 16)
        emit(x, enc_b(4)); /* padding: forwarding stubs are laid out every 16 words */
    d->used_words += x->n;
    if (!r->n)
        r->fwd_start = x->buf;
    r->park[r->n] = park;
    r->blocks[r->n++] = b;
    return true;
}

static bool block_matches(const struct tuxport_dbt *d, const struct tuxport_mmu *mmu,
                          const struct block *b, uint64_t *page, const uint32_t **host);

/*
 * Invalidate the blocks that start in the pages covered by [start, start + len); if a forwarding
 * stub does not fit, request a full flush instead, done by the next thread to enter the
 * dispatcher. With `mmu`, only blocks whose guest instruction words differ from the current ones
 * are invalidated (a translation depends only on the pc and the instruction words; literals are
 * also read only at run time): a JIT rewriting one piece of code issues ic ivau for only a line or
 * two, while ic ivau is recorded per page, so blocks of other methods in the same page remain
 * usable. HotSpot keeps rewriting inline caches and method entries, and invalidating the whole
 * page each time would retranslate 310,000 blocks per Maven build. After an unmap or the removal
 * of exec permission the page has no executable mapping, and all its blocks are invalidated.
 * Parked blocks are left alone: they are checked anyway when revived. Without `mmu` every block
 * in the range is invalidated, parked ones included. Lock held
 */
static void invalidate_locked(struct tuxport_dbt *d, const struct tuxport_mmu *mmu, uint64_t start,
                              uint64_t len)
{
    if (!len)
        return;
    uint64_t end = start + len - 1, first = start >> TUXPORT_PAGE_SHIFT, last = end >> TUXPORT_PAGE_SHIFT;
    if (end < start)
        last = UINT64_MAX >> TUXPORT_PAGE_SHIFT;
    /* A very large range (unmapping a library): look at every block in turn, which is cheaper
     * than a full flush, and other threads need not retranslate either. Blocks do not cross pages,
     * so compare page by page */
    struct retire_batch r = {.n = 0};
    uint64_t seen = 1; /* misaligned, never equal to any page */
    const uint32_t *host = NULL;
    if (end < start || last - first >= INVALIDATE_MAX_PAGES) {
        for (size_t i = 0; i < d->table_count; i++) {
            struct block *b = &d->blocks[i];
            uint64_t page = b->pc >> TUXPORT_PAGE_SHIFT;
            if (!b->code || page < first || page > last)
                continue;
            if (b->parked) {
                if (!mmu)
                    kill_parked(d, b);
            } else if (mmu && block_matches(d, mmu, b, &seen, &host)) {
                continue;
            } else if (!retire_add(d, &r, b, false)) {
                goto full;
            }
        }
        retire_flush(d, &r);
        return;
    }
    for (uint64_t page = first; page <= last; page++) {
        uint32_t *link = &d->page_head[page_bucket(page)];
        while (*link) {
            struct block *b = &d->blocks[*link - 1];
            /* A JIT may rewrite one block thousands of times between compactions. */
            if (!b->code) {
                *link = b->next;
                continue;
            }
            link = &b->next;
            if (b->pc >> TUXPORT_PAGE_SHIFT != page)
                continue;
            if (b->parked) {
                if (!mmu)
                    kill_parked(d, b);
            } else if (mmu && block_matches(d, mmu, b, &seen, &host)) {
                continue;
            } else if (!retire_add(d, &r, b, false)) {
                goto full;
            }
        }
    }
    retire_flush(d, &r);
    return;
full:
    retire_flush(d, &r);
    __atomic_store_n(&d->flush_wanted, true, __ATOMIC_SEQ_CST);
}

/*
 * Leader `pc` failed the shared check, and the dispatcher has Tier 0 execute the leader. The first
 * time it is only recorded: cases such as the stack not having grown that far yet are resolved
 * once the members' own accesses have run. The second time it goes into nogroup and the blocks
 * containing it are invalidated (blocks do not cross pages, so they are all in its page's
 * bucket), and on retranslation it is no longer grouped. Lock held
 */
static void ungroup(struct tuxport_dbt *d, uint64_t pc)
{
    uint64_t *strike = &d->strikes[(pc >> 2) % STRIKES];
    if (*strike != pc) {
        *strike = pc;
        return;
    }
    *strike = 0;
    d->nogroup[d->nogroup_count++ % NOGROUP_MAX] = pc;
    d->st->ungrouped++;
    uint64_t page = pc >> TUXPORT_PAGE_SHIFT;
    struct retire_batch r = {.n = 0};
    for (uint32_t i = d->page_head[page_bucket(page)]; i; i = d->blocks[i - 1].next) {
        struct block *b = &d->blocks[i - 1];
        if (!b->code || b->pc > pc || pc >= b->pc + 4ull * b->nguest)
            continue;
        if (b->parked) {
            kill_parked(d, b);
        } else if (!retire_add(d, &r, b, false)) {
            __atomic_store_n(&d->flush_wanted, true, __ATOMIC_SEQ_CST);
            break;
        }
    }
    retire_flush(d, &r);
}

/* ---- Translation templates: shared by all instances (the engine map) ---- */

/*
 * A block's translation depends only on its guest pc and the instruction words it translated
 * (plus the global feature set and the instance's register assignment), not on the process or
 * the mappings. Every translated block is stored as a template; when another instance (that of
 * another address space: the threads of one address space, and the address spaces of one code
 * group, share one instance) translates the same pc, it first compares the instruction words
 * recorded in the template with those in its own memory, and if they match it copies the template
 * into its own code cache, recomputing only the few b instructions that jump to routines. Links
 * between blocks are patched in each copy, so different processes mapping different code at the
 * same address do not affect each other. When the template arena is full it is discarded
 * entirely, since instances only copy templates and never reference them.
 *
 * All threads of all processes use the templates, and with a single lock, workloads such as go
 * build that start compiler processes concurrently spent a tenth of their time on the lock, so
 * locking is sharded by bucket; allocating from the template arena uses another lock held only
 * briefly, and discarding everything holds all the locks (shards first, then allocation, the same
 * order as when storing a template).
 */
#define TEMPLATE_BYTES (UINT64_C(32) << 20)
#define TEMPLATE_BUCKET_BITS 16
#define TEMPLATE_SHARDS 64
/* The most versions kept for one pc: when a JIT repeatedly rewrites the same code, only the newest
 * few are kept */
#define TEMPLATES_PER_PC 4

struct template {
    struct template *next;
    uint64_t pc;
    uint64_t key; /* grouping records used during translation (nogroup_key) */
    const struct regs *r;
    bool shared; /* a shared block (its link stubs carry LINK_SHARED) */
    uint32_t nguest, nhost, nrelocs, nlinks;
    uint32_t copied, remapped, memory, fallback, grouped; /* statistics, added up when copied */
    /* Guest instruction words, host instruction words, relocations, links (4 words each, see
     * struct link) */
    uint32_t words[];
};

static struct {
    struct {
        pthread_mutex_t lock;
        char pad[64 - sizeof(pthread_mutex_t) % 64]; /* one cache line each */
    } shards[TEMPLATE_SHARDS]; /* shard i covers buckets whose index mod TEMPLATE_SHARDS is i */
    pthread_mutex_t arena_lock;
    uint8_t *arena; /* mapped on first use, cleared when full */
    size_t used;
    bool full; /* full: cleared after the threads storing templates release their shard locks */
    struct template *buckets[1u << TEMPLATE_BUCKET_BITS];
} templates = {
    .shards = {[0 ... TEMPLATE_SHARDS - 1] = {.lock = PTHREAD_MUTEX_INITIALIZER}},
    .arena_lock = PTHREAD_MUTEX_INITIALIZER,
};

static unsigned template_index(uint64_t pc)
{
    return (unsigned)(((pc >> 2) * UINT64_C(0x9e3779b97f4a7c15)) >> (64 - TEMPLATE_BUCKET_BITS));
}

static struct template **template_bucket(uint64_t pc)
{
    return &templates.buckets[template_index(pc)];
}

/* The lock of pc's bucket, also registered with the guard */
static pthread_mutex_t *template_lock(uint64_t pc)
{
    pthread_mutex_t *lock = &templates.shards[template_index(pc) % TEMPLATE_SHARDS].lock;
    pthread_mutex_lock(lock);
    tuxport_guard_hold(lock, false);
    return lock;
}

static void template_unlock(pthread_mutex_t *lock)
{
    tuxport_guard_drop();
    pthread_mutex_unlock(lock);
}

/*
 * The grouping records that translating the block starting at pc uses (those recorded by ungroup
 * that affect leaders after pc in this page): if the instance's records differ, the translated
 * block may differ too, so translation templates are versioned by it. 0 if there are none.
 */
static uint64_t nogroup_key(const struct tuxport_dbt *d, uint64_t pc)
{
    uint64_t key = 0, end = (pc | PAGE_MASK) + 1;
    unsigned n = d->nogroup_count < NOGROUP_MAX ? d->nogroup_count : NOGROUP_MAX;
    for (unsigned i = 0; i < n; i++) {
        uint64_t q = d->nogroup[i];
        if (q + 4 * MAX_BLOCK_INSNS > pc && q < end)
            key += (q * UINT64_C(0x9e3779b97f4a7c15)) | 1;
    }
    return key;
}

/* The caller holds the lock of pc's shard */
static struct template *template_find(uint64_t pc, const uint32_t *guest, uint64_t key, const struct regs *r,
                                      bool shared)
{
    for (struct template *t = *template_bucket(pc); t; t = t->next) {
        if (t->pc == pc && t->key == key && t->r == r && t->shared == shared &&
            memcmp(t->words, guest, t->nguest * 4) == 0)
            return t;
    }
    return NULL;
}

/*
 * A block that is prepared but not yet installed in the code cache (prepare translates without
 * the lock, install puts it in with the lock held): the generated host instructions are in the
 * thread's scratch area, the positions of the b instructions to routines are in relocs, and the
 * exits are in links.
 */
struct prep {
    uint64_t pc;
    unsigned tag; /* private block: tag of the member it belongs to (TAG_SHIFT) */
    const uint32_t *guest; /* host location of the guest words: compared again before installing */
    uint32_t nguest;
    uint32_t words[MAX_BLOCK_INSNS]; /* guest instruction words read at translation */
    const uint32_t *host;
    uint32_t nhost;
    uint32_t relocs[MAX_RELOCS];
    unsigned nrelocs;
    struct link links[MAX_STUBS];
    unsigned nlinks;
};

/* If there is a template matching `guest` (the instruction words read at translation), copy it
 * into the thread's scratch area and record it in p */
static bool template_fetch(uint64_t pc, const uint32_t *guest, uint64_t key, const struct regs *r,
                           bool shared, struct prep *p, uint32_t *scratch, struct tuxport_dbt_stats *st)
{
    pthread_mutex_t *lock = template_lock(pc);
    struct template *t = template_find(pc, guest, key, r, shared);
    if (t) {
        const uint32_t *host = t->words + t->nguest, *relocs = host + t->nhost;
        memcpy(scratch, host, t->nhost * 4);
        memcpy(p->relocs, relocs, t->nrelocs * 4);
        p->host = scratch;
        p->nhost = t->nhost;
        p->nrelocs = t->nrelocs;
        p->nguest = t->nguest;
        const uint32_t *l = relocs + t->nrelocs;
        for (uint32_t i = 0; i < t->nlinks; i++, l += 4)
            p->links[i] = (struct link){l[0], l[1], (uint64_t)l[3] << 32 | l[2]};
        p->nlinks = t->nlinks;
        st->insns += t->nguest;
        st->insns_copied += t->copied;
        st->insns_remapped += t->remapped;
        st->insns_memory += t->memory;
        st->insns_fallback += t->fallback;
        st->insns_grouped += t->grouped;
        st->blocks_shared++;
    }
    template_unlock(lock);
    return t != NULL;
}

/* Allocate `bytes` bytes from the template arena; when full, return NULL and mark it to be
 * cleared. The caller holds one of the shard locks */
static void *template_alloc(size_t bytes)
{
    void *p = NULL;
    pthread_mutex_lock(&templates.arena_lock);
    tuxport_guard_hold(&templates.arena_lock, false);
    if (!templates.arena) {
        void *m = mmap(NULL, TEMPLATE_BYTES, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        templates.arena = m == MAP_FAILED ? NULL : m;
    }
    if (templates.arena && templates.used + bytes <= TEMPLATE_BYTES) {
        p = templates.arena + templates.used;
        templates.used += bytes;
    } else if (templates.arena) {
        templates.full = true;
    }
    tuxport_guard_drop();
    pthread_mutex_unlock(&templates.arena_lock);
    return p;
}

/* Release every template lock, as template_clear takes them (also after a host fault) */
static void template_unlock_all(void *unused)
{
    (void)unused;
    pthread_mutex_unlock(&templates.arena_lock);
    for (unsigned i = TEMPLATE_SHARDS; i-- > 0;)
        pthread_mutex_unlock(&templates.shards[i].lock);
}

/* Discard all translation templates: the template arena is full (`if_full`; not cleared again if
 * another thread already cleared it), or a host fault occurred (the templates themselves may be
 * what is broken) */
static void template_clear(bool if_full)
{
    for (unsigned i = 0; i < TEMPLATE_SHARDS; i++)
        pthread_mutex_lock(&templates.shards[i].lock);
    pthread_mutex_lock(&templates.arena_lock);
    tuxport_guard_hold_fn(NULL, template_unlock_all);
    if (templates.full || !if_full) {
        memset(templates.buckets, 0, sizeof(templates.buckets));
        templates.used = 0;
        templates.full = false;
    }
    tuxport_guard_drop();
    template_unlock_all(NULL);
}

/* Store a just-translated block whose links have not been patched yet as a template */
static void template_store(uint64_t pc, const uint32_t *guest, uint32_t nguest, uint64_t key,
                           const struct xlate *x, const struct tuxport_dbt_stats *before,
                           const struct link *links, unsigned nlinks)
{
    const struct tuxport_dbt_stats *after = x->st;
    size_t bytes = sizeof(struct template) + 4 * (nguest + x->n + x->nrelocs + 4 * nlinks);
    bytes = (bytes + 7) & ~(size_t)7;
    pthread_mutex_t *lock = template_lock(pc);
    struct template *t = template_find(pc, guest, key, x->d->r, x->shared) ? NULL : template_alloc(bytes);
    if (t) {
        *t = (struct template){
            .pc = pc,
            .key = key,
            .r = x->d->r,
            .shared = x->shared,
            .nguest = nguest,
            .nhost = (uint32_t)x->n,
            .nrelocs = x->nrelocs,
            .nlinks = nlinks,
            .copied = (uint32_t)(after->insns_copied - before->insns_copied),
            .remapped = (uint32_t)(after->insns_remapped - before->insns_remapped),
            .memory = (uint32_t)(after->insns_memory - before->insns_memory),
            .fallback = (uint32_t)(after->insns_fallback - before->insns_fallback),
            .grouped = (uint32_t)(after->insns_grouped - before->insns_grouped),
        };
        memcpy(t->words, guest, 4 * nguest);
        memcpy(t->words + nguest, x->buf, 4 * x->n);
        memcpy(t->words + nguest + x->n, x->relocs, 4 * x->nrelocs);
        uint32_t *l = t->words + nguest + x->n + x->nrelocs;
        for (unsigned i = 0; i < nlinks; i++, l += 4) {
            l[0] = links[i].stub;
            l[1] = links[i].branch;
            l[2] = (uint32_t)links[i].pc;
            l[3] = (uint32_t)(links[i].pc >> 32);
        }
        struct template **b = template_bucket(pc);
        t->next = *b;
        *b = t;
        /* Newest first: old versions of the same pc beyond the limit (regardless of register
         * assignment) are unlinked from the chain (their space is reclaimed when the template
         * arena is cleared) */
        unsigned versions = 0;
        for (struct template **p = &t->next; *p;) {
            if ((*p)->pc == pc && ++versions >= TEMPLATES_PER_PC)
                *p = (*p)->next;
            else
                p = &(*p)->next;
        }
    }
    template_unlock(lock);
    if (__atomic_load_n(&templates.full, __ATOMIC_RELAXED))
        template_clear(true);
}

/*
 * For profiling: when the environment variable TUXPORT_PERF_MAP is set, write each block's
 * location in the code cache to /tmp/perf-<pid>.map, which perf report uses to attribute samples
 * to guest pcs (g_<pc>). Without the variable it costs only one extra check.
 */
static void perf_map(const void *code, size_t bytes, uint64_t pc)
{
    static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
    static FILE *file;
    static int enabled = -1;
    if (__atomic_load_n(&enabled, __ATOMIC_RELAXED) == 0)
        return;
    pthread_mutex_lock(&lock);
    tuxport_guard_hold(&lock, false);
    if (__atomic_load_n(&enabled, __ATOMIC_RELAXED) < 0) {
        int on = getenv("TUXPORT_PERF_MAP") != NULL;
        if (on) {
            char path[64];
            snprintf(path, sizeof path, "/tmp/perf-%d.map", (int)getpid());
            file = fopen(path, "w");
        }
        __atomic_store_n(&enabled, on, __ATOMIC_RELAXED);
    }
    if (file) {
        fprintf(file, "%lx %zx g_%lx\n", (unsigned long)(uintptr_t)code, bytes,
                (unsigned long)pc);
        fflush(file);
    }
    tuxport_guard_drop();
    pthread_mutex_unlock(&lock);
}

/* Return values of install: the code cache or the translation table is full and a full flush
 * (flush_world) is needed first; the guest code was rewritten after translation and must be
 * retranslated */
#define NEED_FLUSH ((uint32_t *)(uintptr_t)1)
#define STALE ((uint32_t *)(uintptr_t)2)

/* A block built by install but not yet put in the translation table: it can only be put in after
 * the instruction cache is synced, since other threads look up the table without the lock */
struct fresh {
    uint64_t pc;
    unsigned tag;
    uint32_t *code;
    const uint32_t *guest;
    uint32_t nguest;
};

/*
 * Without the lock: translate the block starting at pc into the thread's scratch area (or copy a
 * template), recording the result in t->prep. Translation takes the longest, and when translating
 * under the lock the JVM's main thread often had to wait for the compiler threads. The guest
 * instruction words are copied first and then translated, and the copy is compared with memory
 * before installing (install). Returns false if the instructions cannot be fetched.
 */
static bool prepare(struct tuxport_dbt_thread *t, struct tuxport_cpu *c, uint64_t pc)
{
    struct tuxport_dbt *d = t->d;
    struct prep *p = t->prep;
    /* A block does not cross a page (it ends at a page boundary), and the whole page has the same
     * permissions */
    const uint32_t *page = NULL;
    if (!(pc & 3))
        page = tuxport_mmu_translate(c->mmu, pc & ~PAGE_MASK, TUXPORT_PROT_EXEC);
    if (!page)
        return false;
    unsigned limit = (unsigned)((TUXPORT_PAGE_SIZE - (pc & PAGE_MASK)) / 4);
    if (limit > MAX_BLOCK_INSNS)
        limit = MAX_BLOCK_INSNS;
    p->pc = pc;
    /* Blocks a group member translates outside the registered mappings are valid only for that
     * member (TAG_SHIFT) */
    p->tag = t->tag && !in_shared(d, pc) ? t->tag : 0;
    p->guest = page + (pc & PAGE_MASK) / 4;
    memcpy(p->words, p->guest, limit * 4);
    p->nlinks = 0;
    const uint32_t *guest = p->words;
    /* Read the grouping records without the lock: reading stale ones only affects grouping, which
     * is undone again when a check fails (ungroup) */
    uint64_t key = d->nogroup_count ? nogroup_key(d, pc) : 0;
    t->stats.blocks++;
    if (template_fetch(pc, guest, key, d->r, !p->tag, p, t->scratch, &t->stats))
        return true;

    struct tuxport_dbt_stats before = t->stats;
    struct xlate *x = t->xlate;
    x->d = d;
    x->buf = t->scratch;
    x->wdelta = 0;
    x->st = &t->stats;
    x->probe = t->probe;
    x->defined = t->defined;
    x->n = 0;
    x->nstubs = 0;
    x->nfixups = 0;
    x->nrelocs = 0;
    x->pc = pc;
    x->guest = guest;
    x->limit = limit;
    x->shared = !p->tag;
    x->sp_aligned = false;
    x->konst = 0;
    for (unsigned i = 0; i < GROUP_SLOTS; i++)
        x->groups[i].n = x->groups[i].next = 0;
    x->t2_group = -1;

    /* Check for a kick at the block entry, so that linked loops can be kicked too */
    emit(x, enc_ldr_x(T0, CTX, offsetof(struct tuxport_cpu, pending)));
    emit_to_stub(x, enc_cbnz_x(T0), FIX_IMM19, R_KICK, pc);
    _Static_assert(ENTRY_CHECK_WORDS == 2, "the length of the block entry check");

    uint32_t nguest = 0;
    for (;; nguest++) {
        uint64_t va = pc + 4ull * nguest;
        if (nguest == MAX_BLOCK_INSNS || (nguest > 0 && (va & PAGE_MASK) == 0)) {
            emit_exit_stub(x, R_CHAIN_FWD, va);
            break;
        }
        x->st->insns++;
        x->idx = nguest;
        if (!translate_insn(x, guest[nguest], va)) {
            nguest++;
            break;
        }
    }

    /* A wrong upper-bound estimate is an engine bug: stop before writing out of bounds */
    if (x->nstubs > MAX_STUBS || x->nfixups > MAX_FIXUPS ||
        x->n + (size_t)x->nstubs * MAX_STUB_WORDS > MAX_BLOCK_WORDS)
        tuxport_guard_fail();
    for (unsigned i = 0; i < x->nstubs; i++)
        x->stubs[i].sites = 0;
    for (unsigned k = 0; k < x->nfixups; k++) {
        struct stub *s = &x->stubs[x->fixups[k].stub];
        s->branch = x->fixups[k].site;
        s->sites++;
    }
    for (unsigned i = 0; i < x->nstubs; i++) {
        struct stub *s = &x->stubs[i];
        s->addr = here(x);
        if (s->reason == STUB_REFILL) {
            emit_refill_stub(x, s);
            continue;
        }
        bool link = s->reason == R_CHAIN || s->reason == R_CHAIN_FWD;
        emit_link_stub(x, s->reason, s->pc, link && s->sites == 1 ? s->branch : NULL);
        if (link)
            p->links[p->nlinks++] = (struct link){
                (uint32_t)(s->addr - x->buf) | (s->reason == R_CHAIN_FWD ? LINK_FWD : 0),
                s->sites == 1 ? (uint32_t)(s->branch - x->buf) : NO_BRANCH, s->pc};
    }
    for (unsigned i = 0; i < x->nfixups; i++) {
        struct fixup *f = &x->fixups[i];
        intptr_t off = (intptr_t)x->stubs[f->stub].addr - (intptr_t)f->site;
        if (f->kind == FIX_IMM19)
            *f->site |= ((uint32_t)(off >> 2) & 0x7ffff) << 5;
        else
            *f->site |= ((uint32_t)(off >> 2) & 0x3fff) << 5;
    }

    if (x->nrelocs > MAX_RELOCS)
        tuxport_guard_fail();
    template_store(pc, guest, nguest, key, x, &before, p->links, p->nlinks);
    p->nguest = nguest;
    p->host = x->buf;
    p->nhost = (uint32_t)x->n;
    memcpy(p->relocs, x->relocs, x->nrelocs * 4);
    p->nrelocs = x->nrelocs;
    return true;
}

/*
 * With the lock held: install a prepared block (prepare) in the code cache and link the exits
 * that are already translated. The new code's range is recorded in `sync` and the block in `f`,
 * and the caller puts it in the translation table (table_insert) after syncing the instruction
 * cache.
 */
static uint32_t *install(struct tuxport_dbt *d, const struct prep *p, struct syncs *sync, struct fresh *f)
{
    /* Another thread rewrote this code during translation (this block was not in the table yet
     * when it invalidated): retranslate */
    if (memcmp(p->guest, p->words, p->nguest * 4) != 0)
        return STALE;
    /* When three quarters of the table are non-empty, rebuild it at the same size if tombstones
     * are at least half, otherwise double it; when blocks is full, remove the invalidated blocks'
     * records, or do a full flush if few are invalidated */
    if (d->table_count == TABLE_LIMIT &&
        (d->blocks_dead * 8 < d->table_count || !compact_blocks(d)))
        return NEED_FLUSH;
    unsigned bits = d->table->bits;
    if (d->table_occupied >= ((size_t)3 << bits) / 4) {
        bool purge = d->table_dead * 2 >= d->table_occupied || bits == TABLE_BITS;
        if (!table_resize(d, purge ? bits : bits + 1) ||
            d->table_occupied >= ((size_t)3 << d->table->bits) / 4)
            return NEED_FLUSH;
    }
    if (d->used_words + p->nhost > d->cache_words)
        return NEED_FLUSH;
    uint32_t *code = d->cache + d->used_words;
    memcpy(rw(d, code), p->host, p->nhost * 4);
    for (unsigned i = 0; i < p->nrelocs; i++) {
        uint32_t *site = code + (p->relocs[i] >> 3);
        *rw(d, site) = enc_b((intptr_t)routine(d, p->relocs[i] & 7) - (intptr_t)site);
    }
    d->used_words += p->nhost;
    link_block(d, code, p->pc, p->tag, p->links, p->nlinks);
    perf_map(code, p->nhost * 4, p->pc);
    sync_add(sync, code, code + p->nhost);
    *f = (struct fresh){p->pc, p->tag, code, p->guest, p->nguest};
    return code;
}

/* Each of three page walks can fail at any level or on the extra permission check for writes. */
struct refill_fails {
    uint32_t *site[3 * (MMU_LEVELS + 1) + 3];
    unsigned n;
};

static void emit_to_fail(struct xlate *x, struct refill_fails *f, uint32_t insn)
{
    f->site[f->n++] = here(x);
    emit(x, insn);
}

/*
 * Walk the page table with mmu.c's structure and fill the page containing address `a` (T0 or x3)
 * into the TLB: the tags of the read and write halves are filled separately according to the leaf
 * entry's permissions, and tlb_used is recorded at the same time, as in Tier 0's tuxport_tlb_fill.
 * Jumps to failure if unmapped or lacking the permission this access needs. Afterwards
 * x2 = addend; uses up x0, x1, x4 and T2, and preserves x3, T0 and T1.
 */
static void emit_fill(struct xlate *x, unsigned a, bool write, struct refill_fails *f)
{
    size_t used = offsetof(struct tuxport_cpu, tlb_used);
    emit(x, enc_ldr_x(0, CTX, offsetof(struct tuxport_cpu, mmu)));
    emit(x, enc_ldr_x(0, 0, offsetof(struct tuxport_mmu, root)));
    /* An intermediate entry is the address of the next level's table; loads through the address
     * dependency are ordered just like mmu.c's acquire loads */
    for (unsigned level = 0; level < MMU_LEVELS; level++) {
        emit(x, enc_ubfx(1, a, TUXPORT_PAGE_SHIFT + MMU_LEVEL_BITS * (MMU_LEVELS - 1 - level),
                         MMU_LEVEL_BITS));
        emit(x, enc_ldr_x_index(0, 0, 1));
        if (level < MMU_LEVELS - 1)
            emit_to_fail(x, f, enc_cbz_x(0));
    }
    /* Leaf entry = host page address | permission bits; an unmapped one is 0, which the permission
     * check rules out as well */
    emit_to_fail(x, f, enc_tbz(0, 0));
    if (write) /* the write half also serves atomic and grouped reads */
        emit_to_fail(x, f, enc_tbz(0, 1));
    _Static_assert(TUXPORT_PROT_READ == 1 && TUXPORT_PROT_WRITE == 2, "the positions of the permission bits");
    emit(x, enc_and_clear_low(1, a, TUXPORT_PAGE_SHIFT));
    emit(x, enc_and_clear_low(2, 0, TUXPORT_PAGE_SHIFT));
    emit(x, enc_sub_reg(2, 2, 1)); /* addend */
    emit(x, enc_ubfx(4, a, TUXPORT_PAGE_SHIFT, 64 - TUXPORT_PAGE_SHIFT)); /* page number */
    emit(x, enc_ubfx(1, a, TUXPORT_PAGE_SHIFT, TLB_BITS));
    emit(x, enc_add_lsl(1, CTX, 1, 5));
    for (unsigned half = 0; half < 2; half++) {
        /* tag = (0 - 1) & ~page number with the permission, else 0 & ~page number = TLB_INVALID */
        emit(x, enc_ubfx(T2, 0, half, 1));
        emit(x, enc_sub_reg(T2, 31, T2));
        emit(x, enc_bic_reg(T2, T2, 4));
        emit(x, enc_stp_x(T2, 2, 1, (int)(TLB_OFF + 16 * half)));
    }
    /* tlb_used[index / 64] |= 1 << (index % 64) */
    emit(x, enc_ubfx(1, a, TUXPORT_PAGE_SHIFT + 6, TLB_BITS - 6));
    emit(x, enc_add_lsl(1, CTX, 1, 3));
    emit_add_const(x, 1, 1, (int64_t)(used & ~(size_t)0xfff));
    emit(x, enc_ldr_x(T2, 1, used & 0xfff));
    emit(x, enc_ubfx(4, a, TUXPORT_PAGE_SHIFT, 6));
    emit(x, enc_movz(0, 1, 0));
    emit(x, enc_lslv(0, 0, 4));
    emit(x, enc_orr_reg(T2, T2, 0));
    emit(x, enc_str_x(T2, 1, used & 0xfff));
}

/* Look up the page of address `a` in the TLB: on a hit the `addend` register = addend, otherwise
 * jump to `miss` (offset patched later); uses up x0 */
static uint32_t *emit_probe(struct xlate *x, unsigned a, unsigned addend, bool write)
{
    emit(x, enc_ubfx(0, a, TUXPORT_PAGE_SHIFT, TLB_BITS));
    emit(x, enc_add_lsl(0, CTX, 0, 5));
    emit(x, enc_ldp_x(0, addend, 0, (int)TLB_OFF + (write ? 16 : 0)));
    emit(x, enc_eon_lsr(0, 0, a, TUXPORT_PAGE_SHIFT));
    uint32_t *miss = here(x);
    emit(x, enc_cbnz_x(0));
    return miss;
}

/*
 * TLB refill routine: a TLB miss or a cross-page access is handled in translated code, without
 * exiting to the dispatcher. Entry: T0 = guest address, T1 points to the refill stub's info word
 * (emit_refill_stub). On success T2 = addend and it returns to the access itself; on failure it
 * exits with the reason in the info word.
 *
 * Without a page crossing, the entry is filled from the page table. Across pages both pages must
 * have the permission this access needs and the host addresses must be contiguous (both pages
 * have the same addend; all pages in one host mapping do), and then the whole access is still
 * done on the host in one go: when both pages are in the TLB only the two entries are compared,
 * and a page that is not is filled first. A function whose stack frame crosses a page accesses it
 * this way on every call, so this cannot fall back to Tier 0.
 *
 * The page-table indices ignore the top byte. Keep it in the TLB tag and subtract the full guest
 * page base in the addend. A hit for this tag then yields an untagged host address. A different top
 * byte at the same slot misses. Bits 55:48 must still be zero at both ends of the access.
 *
 * Borrows the x0–x4 slots in the cpu struct to save host x0–x4: while translated code runs these
 * slots do not hold the guest's values, and the exit routine writes them again.
 */
static uint32_t *emit_refill(struct xlate *x, bool write)
{
    size_t x0 = offsetof(struct tuxport_cpu, x);
    uint32_t *entry = here(x);
    struct refill_fails f = {.n = 0};

    emit(x, enc_stp_x(0, 1, CTX, (int)x0));
    emit(x, enc_stp_x(2, 3, CTX, (int)x0 + 16));
    emit(x, enc_str_x(4, CTX, x0 + 32));
    /* With watchpoints (set by ptrace), do not fill from the page table: watched pages must not
     * enter the TLB. Tier 0 executes this instruction instead; it checks the watchpoints and fills
     * the TLB by tuxport_tlb_fill's rules, so unwatched pages hit as usual afterwards */
    size_t nwatch = offsetof(struct tuxport_cpu, nwatch);
    emit_add_const(x, 0, CTX, (int64_t)(nwatch & ~(size_t)0xfff));
    emit(x, enc_ldr_w(0, 0, nwatch & 0xfff));
    emit_to_fail(x, &f, enc_cbnz_x(0));
    emit(x, enc_ldr_w(3, T1, 0));
    emit(x, enc_ubfx(3, 3, 0, REFILL_REASON_SHIFT));
    emit(x, enc_add_lsl(3, T0, 3, 0)); /* last byte */
    /* Check both ends: adding the byte count can carry into or out of the address space. */
    emit(x, enc_orr_reg(0, T0, 3));
    emit(x, enc_ubfx(0, 0, TUXPORT_VA_BITS, DATA_TAG_SHIFT - TUXPORT_VA_BITS));
    emit_to_fail(x, &f, enc_cbnz_x(0));
    emit(x, enc_eor_lsr(0, T0, 3, 0));
    emit(x, enc_ubfx(0, 0, TUXPORT_PAGE_SHIFT, 64 - TUXPORT_PAGE_SHIFT));
    uint32_t *cross = here(x);
    emit(x, enc_cbnz_x(0));

    emit_fill(x, T0, write, &f);
    emit(x, enc_mov(T2, 2));
    uint32_t *done = here(x);
    emit(x, enc_ldr_w(0, T1, 0));
    emit(x, enc_sbfx(0, 0, REFILL_RESUME_SHIFT, 32 - REFILL_RESUME_SHIFT));
    emit(x, enc_add_lsl(T1, T1, 0, 2));
    emit(x, enc_ldp_x(0, 1, CTX, (int)x0));
    emit(x, enc_ldp_x(2, 3, CTX, (int)x0 + 16));
    emit(x, enc_ldr_x(4, CTX, x0 + 32));
    emit(x, enc_br(T1));

    /* Cross-page: x1, x2 = the two pages' addends */
    uint32_t *probe = here(x);
    patch_branch(x->wdelta, cross, probe);
    uint32_t *miss[2];
    miss[0] = emit_probe(x, T0, 1, write);
    miss[1] = emit_probe(x, 3, 2, write);
    emit(x, enc_sub_reg(0, 2, 1));
    emit_to_fail(x, &f, enc_cbnz_x(0));
    emit(x, enc_mov(T2, 1));
    emit(x, enc_b((intptr_t)done - (intptr_t)here(x)));
    for (unsigned i = 0; i < 2; i++) {
        patch_branch(x->wdelta, miss[i], here(x));
        emit_fill(x, i ? 3 : T0, write, &f);
        emit(x, enc_b((intptr_t)probe - (intptr_t)here(x)));
    }

    for (unsigned i = 0; i < f.n; i++)
        patch_branch(x->wdelta, f.site[i], here(x));
    emit(x, enc_ldr_w(0, T1, 0));
    emit(x, enc_ubfx(T2, 0, REFILL_REASON_SHIFT, REFILL_RESUME_SHIFT - REFILL_REASON_SHIFT));
    emit(x, enc_ldur_x(T0, T1, 4));
    emit(x, enc_ldp_x(0, 1, CTX, (int)x0));
    emit(x, enc_ldp_x(2, 3, CTX, (int)x0 + 16));
    emit(x, enc_ldr_x(4, CTX, x0 + 32));
    emit(x, enc_b((intptr_t)x->d->exit - (intptr_t)here(x)));
    return entry;
}

/* Increment the cpu's dbt_lookup_seq (uses up x1 and x2); see table_lookup */
static void emit_seq_step(struct xlate *x)
{
    size_t seq = offsetof(struct tuxport_cpu, dbt_lookup_seq);
    emit(x, enc_ldr_w(2, CTX, seq));
    emit(x, enc_add_imm(2, 2, 1, 0));
    emit_add_const(x, 1, CTX, (int64_t)seq);
    emit(x, enc_stlr(2, 2, 1));
}

/*
 * Lookup routine for jump-cache misses: T0 = guest pc. Like table_lookup it looks up this address
 * space's shared translation table without the lock; if found, it fills the thread's jump cache
 * and jumps there, and only if not found does it exit to the dispatcher to translate (R_LOOKUP).
 * Previously every miss exited and saved and restored all registers, more than six million times
 * in one Maven build for JIT workloads. Borrows the x0–x3 slots in the cpu struct to save host
 * x0–x3; see emit_refill.
 */
static uint32_t *emit_lookup(struct xlate *x, struct tuxport_dbt *d)
{
    size_t x0 = offsetof(struct tuxport_cpu, x);
    uint32_t *entry = here(x);
    emit(x, enc_stp_x(0, 1, CTX, (int)x0));
    emit(x, enc_stp_x(2, 3, CTX, (int)x0 + 16));
    /* Read the table only after the sequence number has become odd: an stlr and a later ldar are
     * ordered */
    emit_seq_step(x);
    /* Misaligned pcs and pcs beyond 48 bits go to the dispatcher; see table_find_for. So does pc
     * 0: the ldp below reads key and code in no particular order, and racing an insertion it can
     * pair the new code with the empty slot's key 0, which only pc 0 would match */
    emit(x, enc_and_ror(2, T0, 2 + 64 - TAG_SHIFT, 64 - TAG_SHIFT));
    uint32_t *invalid = here(x);
    emit(x, enc_cbnz_x(2));
    uint32_t *zero = here(x);
    emit(x, enc_cbz_x(T0));
    /* x1 = this member's tag << TAG_SHIFT: an entry whose key XOR pc is 0 is a shared block, and
     * one where it equals x1 is this member's private block */
    emit(x, enc_ldr_w(1, CTX, offsetof(struct tuxport_cpu, dbt_tag)));
    emit(x, enc_orr_lsl(1, 31, 1, TAG_SHIFT));
    emit_mov_const(x, 0, (uint64_t)(uintptr_t)&d->table);
    emit(x, enc_ldar(3, 0, 0));
    emit(x, enc_ldr_w(2, 0, offsetof(struct table, shift)));
    emit(x, enc_ldr_x(3, 0, offsetof(struct table, mask)));
    emit(x, enc_add_imm(0, 0, offsetof(struct table, slots), 0));
    emit(x, enc_ubfx(T1, T0, 2, TAG_SHIFT - 2));
    emit_mov_const(x, T2, GOLDEN);
    emit(x, enc_mul(T1, T1, T2));
    emit(x, enc_lsrv(T1, T1, 2));
    /* Linear probing: x2 = the entry's key, T2 = code */
    uint32_t *loop = here(x);
    emit(x, enc_add_lsl(T2, 0, T1, 4));
    emit(x, enc_ldp_x(2, T2, T2, 0));
    uint32_t *miss = here(x);
    emit(x, enc_cbz_x(T2));
    emit(x, enc_eor_lsr(2, 2, T0, 0));
    uint32_t *found[2] = {here(x)};
    emit(x, enc_cbz_x(2));
    emit(x, enc_eor_lsr(2, 2, 1, 0));
    found[1] = here(x);
    emit(x, enc_cbz_x(2));
    emit(x, enc_add_imm(T1, T1, 1, 0));
    emit(x, enc_and_reg(T1, T1, 3));
    emit(x, enc_b((intptr_t)loop - (intptr_t)here(x)));

    patch_branch(x->wdelta, found[0], here(x));
    patch_branch(x->wdelta, found[1], here(x));
    emit_seq_step(x);
    emit(x, enc_ldr_x(0, CTX, offsetof(struct tuxport_cpu, dbt_jump_cache)));
    emit(x, enc_ubfx(1, T0, 2, JC_BITS));
    emit(x, enc_add_lsl(0, 0, 1, 4));
    emit(x, enc_stp_x(T0, T2, 0, 0));
    emit(x, enc_mov(T1, T2));
    emit(x, enc_ldp_x(0, 1, CTX, (int)x0));
    emit(x, enc_ldp_x(2, 3, CTX, (int)x0 + 16));
    emit(x, enc_br(T1));

    patch_branch(x->wdelta, miss, here(x));
    patch_branch(x->wdelta, invalid, here(x));
    patch_branch(x->wdelta, zero, here(x));
    emit_seq_step(x);
    emit(x, enc_ldp_x(0, 1, CTX, (int)x0));
    emit(x, enc_ldp_x(2, 3, CTX, (int)x0 + 16));
    emit(x, enc_movz(T1, 0, 0));
    emit(x, enc_movz(T2, R_LOOKUP, 0));
    emit(x, enc_b((intptr_t)d->exit - (intptr_t)here(x)));
    return entry;
}

/* Move the guest registers kept in same-numbered host registers to or from the cpu struct (`base`
 * points to x[0]): two adjacent ones use one ldp/stp */
static void move_guest_regs(struct xlate *x, unsigned base, bool store)
{
    for (unsigned r = 0; r < 31; r++) {
        if (is_stolen(r))
            continue;
        if (r < 30 && !is_stolen(r + 1)) {
            emit(x, store ? enc_stp_x(r, r + 1, base, (int)(8 * r)) : enc_ldp_x(r, r + 1, base, (int)(8 * r)));
            r++;
        } else {
            emit(x, store ? enc_str_x(r, base, 8 * r) : enc_ldr_x(r, base, 8 * r));
        }
    }
}

/* The enter and exit routines: placed at the start of the code cache, so that every exit stub can
 * reach them with one b */
static void emit_routines(struct tuxport_dbt *d)
{
    struct xlate routines = {.d = d, .buf = d->cache, .wdelta = d->rw_delta, .st = d->st}, *x = &routines;
    size_t x0 = offsetof(struct tuxport_cpu, x), v0 = offsetof(struct tuxport_cpu, v);

    /* uint64_t enter(struct tuxport_cpu *cpu, uint32_t *code): save the callee-saved registers
     * (including d8–d15) and the host FPCR. Frame layout: x29/x30, the five pairs of x27–x19, 8
     * unused bytes, host FPCR (+104), d8–d15 (+112) */
    d->enter = here(x);
    emit(x, enc_stp_x_pre(29, 30, 31, -FRAME_BYTES));
    emit(x, enc_stp_x(27, 28, 31, 16));
    emit(x, enc_stp_x(25, 26, 31, 32));
    emit(x, enc_stp_x(23, 24, 31, 48));
    emit(x, enc_stp_x(21, 22, 31, 64));
    emit(x, enc_stp_x(19, 20, 31, 80));
    for (unsigned r = 8; r < 16; r += 2)
        emit(x, enc_stp_d(r, r + 1, 31, (int)(112 + 8 * (r - 8))));
    emit(x, enc_mrs_fpcr(T1));
    emit(x, enc_str_x(T1, 31, 104));
    emit(x, enc_mov(CTX, 0));
    emit(x, enc_mov(T0, 1));
    /* The guest's FP state stays in host registers throughout translated code (the engine map) */
    emit(x, enc_ldr_x(T1, CTX, offsetof(struct tuxport_cpu, fpcr)));
    emit(x, enc_msr_fpcr(T1));
    emit(x, enc_ldr_x(T1, CTX, offsetof(struct tuxport_cpu, fpsr)));
    emit(x, enc_msr_fpsr(T1));
    emit(x, enc_ldr_x(T1, CTX, offsetof(struct tuxport_cpu, nzcv)));
    emit(x, enc_msr_nzcv(T1));
    emit_add_const(x, T1, CTX, (int64_t)v0);
    for (unsigned q = 0; q < 32; q += 2)
        emit(x, enc_ldp_q(q, q + 1, T1, (int)(16 * q)));
    emit_add_const(x, T1, CTX, (int64_t)x0);
    move_guest_regs(x, T1, false);
    emit(x, enc_ldr_x(SP_HOST, CTX, offsetof(struct tuxport_cpu, sp)));
    emit(x, enc_br(T0));

    /* The two entries used by stubs: T1 points to the adr in the stub, and the pc is in the data
     * words at the end of the stub (emit_link_stub) */
    d->exit_full = here(x);
    emit(x, enc_ldur_x(T0, T1, 12));
    uint32_t *join = here(x);
    emit(x, enc_b(0));
    d->exit_stub = here(x);
    emit(x, enc_ldur_x(T0, T1, 8));
    emit(x, enc_ubfx(T2, T0, PACKED_PC_BITS, 64 - PACKED_PC_BITS));
    emit(x, enc_and_low(T0, T0, PACKED_PC_BITS));
    patch_branch(x->wdelta, join, here(x));
    emit(x, enc_sub_imm(T1, T1, 4, 0));
    /* Exit: T0 = guest pc, T1 = patch site (meaningful only for link stubs), T2 = reason */
    d->exit = here(x);
    emit(x, enc_str_x(T0, CTX, offsetof(struct tuxport_cpu, pc)));
    emit(x, enc_str_x(T1, CTX, offsetof(struct tuxport_cpu, dbt_site)));
    emit(x, enc_str_x(T2, CTX, offsetof(struct tuxport_cpu, dbt_reason)));
    emit(x, enc_mrs_nzcv(T0));
    emit(x, enc_str_x(T0, CTX, offsetof(struct tuxport_cpu, nzcv)));
    emit_add_const(x, T1, CTX, (int64_t)x0);
    move_guest_regs(x, T1, true);
    emit(x, enc_str_x(SP_HOST, CTX, offsetof(struct tuxport_cpu, sp)));
    emit_add_const(x, T1, CTX, (int64_t)v0);
    for (unsigned q = 0; q < 32; q += 2)
        emit(x, enc_stp_q(q, q + 1, T1, (int)(16 * q)));
    emit(x, enc_mrs_fpsr(T0));
    emit(x, enc_str_x(T0, CTX, offsetof(struct tuxport_cpu, fpsr)));
    emit(x, enc_ldr_x(T0, 31, 104));
    emit(x, enc_msr_fpcr(T0));
    emit(x, enc_mov(0, T2));
    for (unsigned r = 8; r < 16; r += 2)
        emit(x, enc_ldp_d(r, r + 1, 31, (int)(112 + 8 * (r - 8))));
    emit(x, enc_ldp_x(19, 20, 31, 80));
    emit(x, enc_ldp_x(21, 22, 31, 64));
    emit(x, enc_ldp_x(23, 24, 31, 48));
    emit(x, enc_ldp_x(25, 26, 31, 32));
    emit(x, enc_ldp_x(27, 28, 31, 16));
    emit(x, enc_ldp_x_post(29, 30, 31, FRAME_BYTES));
    emit(x, enc_ret());

    d->refill[0] = emit_refill(x, false);
    d->refill[1] = emit_refill(x, true);
    d->lookup = emit_lookup(x, d);

    /* Landing pad of empty shadow return stack entries: look up by guest x30, as for an indirect
     * branch that misses the jump cache */
    d->rs_miss = here(x);
    emit(x, enc_mov(T0, 30));
    emit(x, enc_b((intptr_t)d->lookup - (intptr_t)here(x)));

    d->routine_words = d->used_words = x->n;
    sync_range(d, d->cache, d->cache + x->n);
}

/*
 * Instances no longer in use (their address space is gone) go into a small pool for new address
 * spaces to reuse: short-lived processes then need not map the code cache and the tables anew
 * each time and then fault on the first write to every page. Instances with few translations are
 * kept, distinguished by program (a key the caller gives; the kernel layer uses the executable's
 * inode): when the same program runs again (busybox exec'd over and over by a shell script, go
 * build's compiles one after another, repeated `node -e`), it takes back the translations it
 * left and keeps using them after checking them (the register assignment must also be the same);
 * without one of the same key it takes an empty one or creates one, rather than clearing what
 * other programs left. When several processes run the same program at once each takes an
 * instance, so the pool must have room for them. The total code kept in the pool is capped, and
 * when the cap is exceeded the least recently used go first. An emptied instance marks the used
 * part of its code cache MADV_FREE, so the host can reclaim it directly under memory pressure (a
 * later write just faults again). The thread parts (jump cache, shadow return stack) have a pool
 * of their own.
 */
#define POOL_SIZE 16
#define POOL_KEEP_WORDS ((size_t)(64u << 20) / 4) /* at most 64 MB of code kept in the pool */
#define THREAD_POOL_SIZE 8

static struct {
    pthread_mutex_t lock;
    struct pooled {
        struct tuxport_dbt *d;
        uint64_t key;  /* which program's translations are kept; 0 if emptied */
        uint64_t when; /* order of arrival, to remove the least recently used */
    } free[POOL_SIZE];
    unsigned n;
    uint64_t clock;
    size_t kept_words; /* code kept in the pool */
    struct tuxport_dbt_thread *threads[THREAD_POOL_SIZE];
    unsigned nthreads;
    struct tuxport_dbt *all; /* all instances */
} pool = {.lock = PTHREAD_MUTEX_INITIALIZER};

/* Take entry i out of the pool. Pool lock held */
static struct tuxport_dbt *pool_take(unsigned i)
{
    struct tuxport_dbt *d = pool.free[i].d;
    if (pool.free[i].key)
        pool.kept_words -= d->used_words - d->routine_words;
    pool.free[i] = pool.free[--pool.n];
    return d;
}

/* Free the registered ranges (tuxport_dbt_share), the current list and every replaced one, and
 * clear d->shared; no thread is using this instance */
static void drop_shared(struct tuxport_dbt *d)
{
    for (struct shared_ranges *s = d->shared, *older; s; s = older) {
        older = s->older;
        free(s);
    }
    d->shared = NULL;
}

static void release(struct tuxport_dbt *d)
{
    drop_shared(d);
    pthread_mutex_lock(&pool.lock);
    if (d->prev_instance) {
        *d->prev_instance = d->next_instance;
        if (d->next_instance)
            d->next_instance->prev_instance = d->prev_instance;
    }
    pthread_mutex_unlock(&pool.lock);
    if (d->cache)
        tuxport_unmap_code(d->cache, CODE_CACHE_BYTES, d->rw_delta);
    zfree(d->written, sizeof(*d->written));
    if (d->table)
        zfree(d->table, TABLE_BYTES(d->table->bits));
    zfree(d->table_used, TABLE_LIMIT * sizeof(*d->table_used));
    zfree(d->blocks, TABLE_LIMIT * sizeof(*d->blocks));
    zfree(d->words, WORDS_LIMIT * sizeof(*d->words));
    zfree(d->page_head, sizeof(*d->page_head) << PAGE_BUCKET_BITS);
    zfree(d->page_parked, sizeof(*d->page_parked) << PAGE_BUCKET_BITS);
    pthread_mutex_destroy(&d->lock);
    pthread_cond_destroy(&d->flushed);
    zfree(d, sizeof(*d));
}

/* Statistics of an instance itself while no threads are attached (flushing and checking when it
 * is taken out of or put back into the pool) */
static struct tuxport_dbt_stats spare_stats;

struct tuxport_dbt *tuxport_dbt_new(uint64_t key, enum tuxport_dbt_regs regs)
{
    const struct regs *r = &reg_sets[regs];
    pthread_mutex_lock(&pool.lock);
    int best = -1;
    for (unsigned i = 0; i < pool.n; i++) {
        bool same = key && pool.free[i].key == key && pool.free[i].d->r == r;
        bool empty = !pool.free[i].key;
        if ((same && (best < 0 || pool.free[best].key != key || pool.free[i].when > pool.free[best].when)) ||
            (empty && best < 0))
            best = (int)i;
    }
    struct tuxport_dbt *d = best >= 0 ? pool_take((unsigned)best) : NULL;
    pthread_mutex_unlock(&pool.lock);
    if (d && d->r != r) { /* an empty instance: regenerate the routines for this register set */
        d->r = r;
        emit_routines(d);
    }
    if (d)
        return d;

    d = zalloc(sizeof(*d));
    if (!d)
        return NULL;
    pthread_mutex_init(&d->lock, NULL);
    pthread_cond_init(&d->flushed, NULL);
    d->st = &spare_stats;
    d->cache = tuxport_map_code(CODE_CACHE_BYTES, &d->rw_delta);
    if (d->cache && d->rw_delta)
        d->written = zalloc(sizeof(*d->written));
    d->table = table_new(TABLE_MIN_BITS);
    d->table_used = zalloc(TABLE_LIMIT * sizeof(*d->table_used));
    d->blocks = zalloc(TABLE_LIMIT * sizeof(*d->blocks));
    d->words = zalloc(WORDS_LIMIT * sizeof(*d->words));
    d->page_head = zalloc(sizeof(*d->page_head) << PAGE_BUCKET_BITS);
    d->page_parked = zalloc(sizeof(*d->page_parked) << PAGE_BUCKET_BITS);
    if (!d->cache || (d->rw_delta && !d->written) || !d->table || !d->table_used || !d->blocks ||
        !d->words || !d->page_head || !d->page_parked) {
        release(d);
        return NULL;
    }
    d->cache_words = CODE_CACHE_BYTES / 4;
    d->r = r;
    d->next_tag = 1;
    emit_routines(d);
    pthread_mutex_lock(&pool.lock);
    d->next_instance = pool.all;
    if (pool.all)
        pool.all->prev_instance = &d->next_instance;
    d->prev_instance = &pool.all;
    pool.all = d;
    pthread_mutex_unlock(&pool.lock);
    return d;
}

/*
 * When returning to the pool with translations kept, remove the members' private blocks, so tags
 * can be allocated from the start again. Only private blocks of the same member link to them
 * (shared blocks do not link to private blocks), and no threads are attached, so no forwarding
 * stubs are needed
 */
static void drop_private(struct tuxport_dbt *d)
{
    for (size_t i = 0; i < d->table_count; i++) {
        struct block *b = &d->blocks[i];
        if (!b->tag || !b->code)
            continue;
        if (b->parked) {
            kill_parked(d, b);
            continue;
        }
        table_remove(d, block_key(b));
        b->code = NULL;
        d->blocks_dead++;
    }
}

/* Put back into the pool: no threads are attached, so it can be flushed directly */
void tuxport_dbt_free(struct tuxport_dbt *d, uint64_t key)
{
    if (!d)
        return;
    if (d->poisoned) {
        release(d);
        return;
    }
    d->st = &spare_stats;
    size_t words = d->used_words - d->routine_words;
    /* Keep the translations when there are not many blocks: checking them once before the next
     * user running the same program runs (recheck) is much cheaper than copying templates block by
     * block, relinking and syncing the instruction cache */
    bool keep = key && d->table_count <= KEEP_MAX_BLOCKS && words <= POOL_KEEP_WORDS && !d->flush_wanted;
    /* A large instance using more than half of the pool (one JVM run takes 40–50 MB) is kept only
     * if it does not push others out: otherwise one java run would push out all the translations
     * left by frequently used programs such as node and python */
    if (keep && words > POOL_KEEP_WORDS / 2) {
        pthread_mutex_lock(&pool.lock);
        keep = pool.kept_words + words <= POOL_KEEP_WORDS;
        pthread_mutex_unlock(&pool.lock);
    }
    drop_shared(d);
    if (keep) {
        drop_private(d);
        d->stale = d->table_count > 0;
    } else {
        key = 0;
        words = 0;
        size_t nblocks = d->table_count, nguest = d->words_used;
        flush_locked(d);
        if (d->table->bits > TABLE_MIN_BITS) /* next user is most likely a short-lived process */
            table_resize(d, TABLE_MIN_BITS);
        /* Block records and instruction words are mapped in advance at their limits, and the used
         * parts keep occupying memory: return them too */
        release_pages(d->blocks, nblocks * sizeof(*d->blocks));
        release_pages(d->table_used, nblocks * sizeof(*d->table_used));
        release_pages(d->words, nguest * sizeof(*d->words));
        /*
         * Keep the pages holding the enter/exit routines, and hand the rest of the used part to
         * the host to reclaim: private mappings use MADV_FREE, reclaimed only under memory
         * pressure; double-mapped shared memory does not support it and can only be freed at once
         * with MADV_REMOVE
         */
        uintptr_t page = host_page_size();
        uintptr_t start = ((uintptr_t)rw(d, d->cache + d->routine_words) + page - 1) & ~(page - 1);
        uintptr_t end = ((uintptr_t)rw(d, d->cache + d->high_words) + page - 1) & ~(page - 1);
        if (end > start)
            madvise((void *)start, end - start, d->rw_delta ? MADV_REMOVE : MADV_FREE);
        d->high_words = 0;
        drop_written(d); /* the routines' pages, and the recorded page count */
    }
    d->next_tag = 1; /* all tagged blocks have been removed */
    /* Make room: the pool is full, or the kept code exceeds the limit. Remove empty ones first
     * (rebuilding them is cheap), then the least recently used */
    struct tuxport_dbt *evicted[POOL_SIZE];
    unsigned nevicted = 0;
    pthread_mutex_lock(&pool.lock);
    while (pool.n && (pool.n == POOL_SIZE || pool.kept_words + words > POOL_KEEP_WORDS)) {
        /* When making room for code, removing empty ones does not help */
        bool need_code = pool.kept_words + words > POOL_KEEP_WORDS;
        int victim = -1;
        for (unsigned i = 0; i < pool.n; i++) {
            bool empty = !pool.free[i].key;
            if (need_code && empty)
                continue;
            if (victim < 0) {
                victim = (int)i;
                continue;
            }
            bool victim_empty = !pool.free[victim].key;
            if (empty != victim_empty ? empty : pool.free[i].when < pool.free[victim].when)
                victim = (int)i;
        }
        if (victim < 0)
            break;
        evicted[nevicted++] = pool_take((unsigned)victim);
    }
    pool.free[pool.n++] = (struct pooled){d, key, ++pool.clock};
    pool.kept_words += words;
    pthread_mutex_unlock(&pool.lock);
    for (unsigned i = 0; i < nevicted; i++)
        release(evicted[i]);
}

/* Drop the pages written in the writable view of every instance's code cache (see the comment
 * before struct written) */
void tuxport_dbt_unmap_written(void)
{
    pthread_mutex_lock(&pool.lock);
    for (struct tuxport_dbt *d = pool.all; d; d = d->next_instance)
        drop_written(d);
    pthread_mutex_unlock(&pool.lock);
}

/* Total bytes of the pages written in the writable view and not dropped yet: they count twice in
 * the host's RSS */
uint64_t tuxport_dbt_written_bytes(void)
{
    uint64_t pages = 0;
    pthread_mutex_lock(&pool.lock);
    for (struct tuxport_dbt *d = pool.all; d; d = d->next_instance)
        if (d->written)
            pages += __atomic_load_n(&d->written->pages, __ATOMIC_RELAXED);
    pthread_mutex_unlock(&pool.lock);
    return pages * host_page_size();
}

/* Release every instance in the pool (when the app is short of memory; kept translations can be
 * rebuilt) */
void tuxport_dbt_trim(void)
{
    struct tuxport_dbt *evicted[POOL_SIZE];
    pthread_mutex_lock(&pool.lock);
    unsigned n = 0;
    while (pool.n)
        evicted[n++] = pool_take(pool.n - 1);
    pthread_mutex_unlock(&pool.lock);
    for (unsigned i = 0; i < n; i++)
        release(evicted[i]);
}

static void release_thread(struct tuxport_dbt_thread *t)
{
    zfree(t->jc, JC_SIZE * sizeof(*t->jc));
    zfree(t->rs, TUXPORT_PAGE_SIZE);
    free(t->xlate);
    zfree(t->scratch, MAX_BLOCK_WORDS * sizeof(*t->scratch));
    zfree(t->probe, sizeof(*t->probe));
    zfree(t->defined, DEFINED_CACHE * sizeof(*t->defined));
    free(t->prep);
    zfree(t, sizeof(*t));
}

/* Take d's lock, recording whom this round's translations and invalidations are charged to. The
 * lock is always held only briefly (installing a block, invalidating a batch of blocks), so spin
 * for a while first rather than sleep and be woken again (tens of microseconds each time in a
 * virtual machine) */
static void lock_dbt(struct tuxport_dbt *d, struct tuxport_dbt_stats *st)
{
    unsigned spin = 0;
    while (pthread_mutex_trylock(&d->lock) != 0) {
        if (++spin == 200) {
            pthread_mutex_lock(&d->lock);
            break;
        }
        __asm__ volatile("yield");
    }
    tuxport_guard_hold(&d->lock, false);
    d->st = st;
}

static void unlock_dbt(struct tuxport_dbt *d)
{
    tuxport_guard_drop();
    pthread_mutex_unlock(&d->lock);
}

unsigned tuxport_dbt_new_tag(struct tuxport_dbt *d)
{
    pthread_mutex_lock(&d->lock);
    unsigned tag = d->next_tag <= TAG_MAX ? d->next_tag++ : 0;
    pthread_mutex_unlock(&d->lock);
    return tag;
}

static int range_cmp(const void *a, const void *b)
{
    uint64_t x = ((const uint64_t *)a)[0], y = ((const uint64_t *)b)[0];
    return x < y ? -1 : x > y;
}

void tuxport_dbt_share(struct tuxport_dbt *d, const struct tuxport_dbt_source *s, size_t n)
{
    struct shared_ranges *r = malloc(sizeof(*r) + n * sizeof(r->r[0]));
    if (!r) /* keep the old ranges: blocks outside them count as private, only less is shared */
        return;
    for (size_t i = 0; i < n; i++) {
        r->r[i][0] = s[i].start;
        r->r[i][1] = s[i].start + s[i].len;
    }
    qsort(r->r, n, sizeof(r->r[0]), range_cmp);
    unsigned m = 0; /* merge overlapping and adjacent ones into one range */
    for (size_t i = 0; i < n; i++) {
        if (m && r->r[i][0] <= r->r[m - 1][1]) {
            if (r->r[i][1] > r->r[m - 1][1])
                r->r[m - 1][1] = r->r[i][1];
        } else {
            r->r[m][0] = r->r[i][0];
            r->r[m][1] = r->r[i][1];
            m++;
        }
    }
    r->n = m;
    pthread_mutex_lock(&d->lock);
    r->older = d->shared;
    __atomic_store_n(&d->shared, r, __ATOMIC_RELEASE);
    pthread_mutex_unlock(&d->lock);
}

struct tuxport_dbt_thread *tuxport_dbt_attach(struct tuxport_dbt *d, unsigned tag)
{
    pthread_mutex_lock(&pool.lock);
    struct tuxport_dbt_thread *t = pool.nthreads ? pool.threads[--pool.nthreads] : NULL;
    pthread_mutex_unlock(&pool.lock);
    if (!t) {
        t = zalloc(sizeof(*t));
        if (!t)
            return NULL;
        t->jc = zalloc(JC_SIZE * sizeof(*t->jc));
        t->rs = zalloc(TUXPORT_PAGE_SIZE); /* page-aligned, and so also 1 KB-aligned */
        t->xlate = malloc(sizeof(*t->xlate));
        t->scratch = zalloc(MAX_BLOCK_WORDS * sizeof(*t->scratch));
        t->probe = zalloc(sizeof(*t->probe)); /* TLB is over 100 KB; unused pages take no memory */
        t->defined = zalloc(DEFINED_CACHE * sizeof(*t->defined));
        t->prep = malloc(sizeof(*t->prep));
        if (!t->jc || !t->rs || !t->xlate || !t->scratch || !t->probe || !t->defined || !t->prep) {
            release_thread(t);
            return NULL;
        }
        tuxport_cpu_init(t->probe, NULL);
        t->jc_dirty = true;
    }
    t->d = d;
    t->tag = tag;
    t->cpu = NULL;
    t->parked = false;
    t->stats = (struct tuxport_dbt_stats){0};
    if (t->jc_dirty)
        clear_jump_cache(t);
    lock_dbt(d, &t->stats);
    t->generation = d->generation;
    rs_reset(t);
    t->next = d->threads;
    d->threads = t;
    __atomic_store_n(&d->nthreads, d->nthreads + 1, __ATOMIC_RELAXED);
    unlock_dbt(d);
    return t;
}

void tuxport_dbt_detach(struct tuxport_dbt_thread *t)
{
    if (!t)
        return;
    struct tuxport_dbt *d = t->d;
    lock_dbt(d, &spare_stats);
    for (struct tuxport_dbt_thread **p = &d->threads; *p; p = &(*p)->next) {
        if (*p == t) {
            *p = t->next;
            __atomic_store_n(&d->nthreads, d->nthreads - 1, __ATOMIC_RELAXED);
            break;
        }
    }
    unlock_dbt(d);
    pthread_mutex_lock(&pool.lock);
    bool keep = pool.nthreads < THREAD_POOL_SIZE;
    if (keep)
        pool.threads[pool.nthreads++] = t;
    pthread_mutex_unlock(&pool.lock);
    if (!keep)
        release_thread(t);
}

void tuxport_dbt_flush(struct tuxport_dbt_thread *t)
{
    __atomic_store_n(&t->d->flush_wanted, true, __ATOMIC_SEQ_CST);
}

void tuxport_dbt_recheck(struct tuxport_dbt_thread *t)
{
    lock_dbt(t->d, &t->stats);
    t->d->stale = t->d->table_count > 0;
    unlock_dbt(t->d);
}

void tuxport_dbt_invalidate(struct tuxport_dbt_thread *t, const struct tuxport_cpu *cpu, uint64_t start,
                           uint64_t len)
{
    lock_dbt(t->d, &t->stats);
    invalidate_locked(t->d, cpu ? cpu->mmu : NULL, start, len);
    unlock_dbt(t->d);
}

/* Whether a block's guest instruction words at translation equal those at the same address in
 * `mmu` now; `page`/`host` remember the page looked up last time */
static bool block_matches(const struct tuxport_dbt *d, const struct tuxport_mmu *mmu,
                          const struct block *b, uint64_t *page, const uint32_t **host)
{
    uint64_t p = b->pc & ~PAGE_MASK;
    if (p != *page) {
        *page = p;
        *host = tuxport_mmu_translate(mmu, p, TUXPORT_PROT_EXEC);
    }
    return *host && memcmp(*host + (b->pc & PAGE_MASK) / 4, d->words + b->words, b->nguest * 4) == 0;
}

/* The block lies entirely within one of the previous user's file mappings (recorded by
 * tuxport_dbt_trust): while its page is not mapped yet it is worth parking, since the same content
 * will most likely be mapped back later; blocks in anonymous memory (JIT-generated code) will
 * most likely not match next time */
static bool block_parkable(const struct tuxport_dbt *d, const struct block *b)
{
    for (unsigned i = 0; i < d->nparkable; i++) {
        if (b->pc >= d->parkable[i][0] && b->pc + (uint64_t)b->nguest * 4 <= d->parkable[i][1])
            return true;
    }
    return false;
}

/* The block lies entirely within a range given by tuxport_dbt_trust */
static bool block_trusted(const struct tuxport_dbt *d, const struct block *b)
{
    for (unsigned i = 0; i < d->ntrusted; i++) {
        if (b->pc >= d->trusted[i][0] && b->pc + (uint64_t)b->nguest * 4 <= d->trusted[i][1])
            return true;
    }
    return false;
}

/*
 * After an instance changes address space and before its first run: check the guest instruction
 * words block by block and invalidate those that do not match. It is the same check as when
 * copying a translation template, only done all at once, saving the per-block copying, relinking
 * and instruction-cache syncing. When most do not match (another program was exec'd), request a
 * full flush. Lock held
 */
static void recheck(struct tuxport_dbt *d, const struct tuxport_cpu *c)
{
    d->stale = false;
    size_t live = 0, bad = 0, unmapped = 0;
    uint64_t page = 1; /* misaligned, never equal to any page */
    const uint32_t *host = NULL;
    for (size_t i = 0; i < d->table_count; i++) {
        const struct block *b = &d->blocks[i];
        if (b->code && !b->parked) {
            live++;
            if (!block_trusted(d, b) && !block_matches(d, c->mmu, b, &page, &host)) {
                bool park = !host && block_parkable(d, b);
                bad += !park;
                unmapped += park;
            }
        }
    }
    if (bad * 2 > live) {
        d->ntrusted = d->nparkable = 0;
        d->flush_wanted = true;
        return;
    }
    /* Blocks whose page is not mapped yet (shared libraries wait for the dynamic linker to load
     * them; at this point there are only the program and the dynamic linker) are parked and
     * checked the first time they are executed (revive): previously they were all invalidated,
     * and every node startup copied six or seven thousand blocks from templates again */
    page = 1;
    struct retire_batch r = {.n = 0};
    for (size_t i = 0; (bad || unmapped) && i < d->table_count; i++) {
        struct block *b = &d->blocks[i];
        if (b->code && !b->parked && !block_trusted(d, b) &&
            !block_matches(d, c->mmu, b, &page, &host)) {
            bool park = !host && block_parkable(d, b);
            if (!retire_add(d, &r, b, park)) {
                d->flush_wanted = true;
                break;
            }
            if (park)
                unmapped--;
            else
                bad--;
        }
    }
    retire_flush(d, &r);
    d->ntrusted = d->nparkable = 0;
}

/*
 * Put a parked block back into the translation table (revive). If the tombstone left when it was
 * parked is still there (the table has not been rebuilt since), restore it in place: the code has
 * not changed, so a thread looking up the table without the lock is right whichever of the before
 * and after states it reads. If it were inserted into another empty slot, no record would point
 * to that tombstone any more, and after a full flush (flush_locked only clears the slots recorded
 * in table_used) it would still occupy its slot without being counted; with repeated parking,
 * reviving and flushing, the table would fill up, and neither insertion nor lookup would find an
 * empty slot. Lock held
 */
static void table_relink(struct tuxport_dbt *d, size_t k)
{
    struct table *t = d->table;
    const struct block *b = &d->blocks[k];
    size_t i = d->table_used[k];
    if (i != SLOT_NONE && t->slots[i].pc == JC_EMPTY && t->slots[i].code == b->code) {
        d->table_dead--;
        __atomic_store_n(&t->slots[i].pc, block_key(b), __ATOMIC_RELEASE);
        return;
    }
    i = table_index(t, block_key(b));
    while (t->slots[i].code)
        i = (i + 1) & table_mask(t);
    d->table_occupied++;
    __atomic_store_n(&t->slots[i].pc, block_key(b), __ATOMIC_RELAXED);
    __atomic_store_n(&t->slots[i].code, b->code, __ATOMIC_RELEASE);
    d->table_used[k] = (uint32_t)i;
}

/*
 * The dispatcher did not find pc, and its page has parked blocks (recheck): check the parked
 * blocks of the same page together; those whose guest instruction words match the current ones
 * get their entries restored and go back into the table, with no retranslation or template copy
 * needed; those that do not match are invalidated. A page at a time: each parked block exits here
 * through the forwarding stub the first time it is executed, and when reviving them one by one,
 * those exits alone would use up the time saved. Links to a parked block always jump to its
 * entry, which while parked exits here through the forwarding stub, so code that has not been
 * checked is never executed. When the table is too full nothing is revived, and the normal
 * translation path grows the table. Lock held
 */
/* Sync the revived batch first, then put it into the table; only then can other threads jump
 * there */
static void revive_flush(struct tuxport_dbt *d, const uint32_t *idx, unsigned n)
{
    if (!n)
        return;
    void *start[RETIRE_BATCH], *end[RETIRE_BATCH];
    for (unsigned k = 0; k < n; k++) {
        start[k] = d->blocks[idx[k]].code;
        end[k] = d->blocks[idx[k]].code + ENTRY_CHECK_WORDS + 1;
    }
    sync_written(d, start, end, n);
    for (unsigned k = 0; k < n; k++)
        table_relink(d, idx[k]);
}

static void revive(struct tuxport_dbt *d, const struct tuxport_cpu *c, uint64_t pc)
{
    uint64_t page = pc >> TUXPORT_PAGE_SHIFT, p = 1;
    const uint32_t *host = NULL;
    uint32_t idx[RETIRE_BATCH];
    unsigned n = 0;
    /* The first instruction of the entry check is the same for every block (prepare), so it need
     * not be recorded separately */
    uint32_t check = enc_ldr_x(d->r->t0, d->r->ctx, offsetof(struct tuxport_cpu, pending));
    for (uint32_t i = d->page_head[page_bucket(page)]; i; i = d->blocks[i - 1].next) {
        struct block *b = &d->blocks[i - 1];
        if (!b->parked || b->pc >> TUXPORT_PAGE_SHIFT != page)
            continue;
        if (!block_matches(d, c->mmu, b, &p, &host)) {
            /* The page has no entry yet (large mappings fill entries on demand): keep it; when
             * the dispatcher cannot fetch the instruction, Tier 0 reports the page fault, and it
             * comes back after the kernel layer fills in the entry */
            if (!host)
                break;
            kill_parked(d, b);
            continue;
        }
        if (d->table_occupied + n >= ((size_t)3 << d->table->bits) / 4)
            break;
        if (n == RETIRE_BATCH) {
            revive_flush(d, idx, n);
            n = 0;
        }
        *rw(d, b->code) = check;
        *rw(d, b->code + ENTRY_CHECK_WORDS) = b->first;
        b->parked = 0;
        d->nparked--;
        d->page_parked[page_bucket(page)]--;
        idx[n++] = i - 1;
    }
    revive_flush(d, idx, n);
}

void tuxport_dbt_set_sources(struct tuxport_dbt *d, const struct tuxport_dbt_source *s, size_t n)
{
    pthread_mutex_lock(&d->lock);
    /* What does not fit is not recorded: blocks in unrecorded mappings are checked as usual */
    d->nsources = n < TUXPORT_DBT_SOURCES ? (unsigned)n : TUXPORT_DBT_SOURCES;
    memcpy(d->sources, s, d->nsources * sizeof(*s));
    pthread_mutex_unlock(&d->lock);
}

void tuxport_dbt_trust(struct tuxport_dbt *d, const struct tuxport_dbt_source *s, size_t n)
{
    pthread_mutex_lock(&d->lock);
    d->ntrusted = 0;
    for (size_t i = 0; d->stale && i < n; i++) {
        for (unsigned j = 0; j < d->nsources; j++) {
            if (!memcmp(&s[i], &d->sources[j], sizeof(*s)) && d->ntrusted < TUXPORT_DBT_SOURCES) {
                d->trusted[d->ntrusted][0] = s[i].start;
                d->trusted[d->ntrusted][1] = s[i].start + s[i].len;
                d->ntrusted++;
                break;
            }
        }
    }
    /* What was recorded is used only once: it is recorded again when this address space stops
     * being used. All of it is recorded as parkable ranges (recheck) */
    for (unsigned j = 0; j < d->nsources; j++) {
        d->parkable[j][0] = d->sources[j].start;
        d->parkable[j][1] = d->sources[j].start + d->sources[j].len;
    }
    d->nparkable = d->stale ? d->nsources : 0;
    d->nsources = 0;
    pthread_mutex_unlock(&d->lock);
}

void tuxport_dbt_retire_outside(struct tuxport_dbt *d, const struct tuxport_dbt_source *s, size_t n)
{
    pthread_mutex_lock(&d->lock);
    d->st = &spare_stats; /* the previous thread holding the lock may have detached already */
    struct retire_batch r = {.n = 0};
    for (size_t i = 0; i < d->table_count; i++) {
        struct block *b = &d->blocks[i];
        if (!b->code || b->tag) /* private blocks are found only by their member */
            continue;
        bool inside = false;
        for (size_t j = 0; j < n && !inside; j++)
            inside = b->pc - s[j].start < s[j].len;
        if (inside)
            continue;
        if (b->parked) {
            kill_parked(d, b);
        } else if (!retire_add(d, &r, b, false)) {
            __atomic_store_n(&d->flush_wanted, true, __ATOMIC_SEQ_CST);
            break;
        }
    }
    retire_flush(d, &r);
    pthread_mutex_unlock(&d->lock);
}

/* Stop and wait for another thread to finish a full flush. Lock held, released while waiting */
static void park(struct tuxport_dbt *d, struct tuxport_dbt_thread *self)
{
    self->parked = true;
    while (__atomic_load_n(&d->flushing, __ATOMIC_ACQUIRE) && !d->poisoned)
        pthread_cond_wait(&d->flushed, &d->lock);
    self->parked = false;
}

/* Whether a thread other than self is in dbt_run and not stopped in park. Lock held */
static bool others_running(const struct tuxport_dbt *d, const struct tuxport_dbt_thread *self)
{
    for (const struct tuxport_dbt_thread *t = d->threads; t; t = t->next)
        if (t != self && !t->parked && __atomic_load_n(&t->cpu, __ATOMIC_SEQ_CST))
            return true;
    return false;
}

/*
 * Full flush: kick the other threads in dbt_run, wait until they have all stopped (park) or left
 * dbt_run, then flush. Entered and returned with the lock held; the lock is released while
 * waiting so that other threads can reach park. If a thread is already doing it, stop along with
 * the others and wait for it.
 *
 * flushing is set before looking at each thread's cpu, and a thread registers its cpu before
 * looking at flushing: either this side kicks it, or it sees flushing and stops by itself. While
 * waiting, each thread's cpu and parked are checked with the lock held: parked only changes under
 * the lock, and a stopped thread does not leave before the flush ends.
 *
 * Previously two shared counters (threads in dbt_run, stopped threads) were changed atomically on
 * every entry into and exit from dbt_run: when several threads of one address space entered and
 * left often (every system call is one), that cache line bounced between cores, and with 8
 * threads reading the clock at the same time each read went from 94 ns to 844 ns. Now entry and
 * exit only write the thread's own cpu.
 */
static void flush_world(struct tuxport_dbt *d, struct tuxport_dbt_thread *self)
{
    if (__atomic_load_n(&d->flushing, __ATOMIC_RELAXED)) {
        park(d, self);
        return;
    }
    __atomic_store_n(&d->flushing, true, __ATOMIC_SEQ_CST);
    for (struct tuxport_dbt_thread *t = d->threads; t; t = t->next) {
        struct tuxport_cpu *c = __atomic_load_n(&t->cpu, __ATOMIC_SEQ_CST);
        if (t != self && c)
            tuxport_cpu_kick(c);
    }
    while (others_running(d, self)) {
        unlock_dbt(d);
        sched_yield();
        lock_dbt(d, &self->stats);
    }
    flush_locked(d);
    __atomic_store_n(&d->flushing, false, __ATOMIC_SEQ_CST);
    pthread_cond_broadcast(&d->flushed);
}

/* The guest changed code in the page containing `addr` (ic ivau): invalidate the blocks in it
 * whose instruction words changed */
static void code_changed(struct tuxport_dbt_thread *t, const struct tuxport_cpu *c, uint64_t addr)
{
    lock_dbt(t->d, &t->stats);
    invalidate_locked(t->d, c->mmu, addr & ~(uint64_t)PAGE_MASK, TUXPORT_PAGE_SIZE);
    unlock_dbt(t->d);
}

static enum tuxport_exit dbt_run(void *ctx, struct tuxport_cpu *c)
{
    struct tuxport_dbt_thread *t = ctx;
    struct tuxport_dbt *d = t->d;
    uint64_t (*enter)(struct tuxport_cpu *, uint32_t *) =
        (uint64_t(*)(struct tuxport_cpu *, uint32_t *))(uintptr_t)d->enter;
    uint32_t *patch_site = NULL, *patch_branch_site = NULL;
    uint64_t patch_generation = 0;
    bool patch_forward = false, patch_shared = false;
    bool fill_jump_cache = false;

    if (UNLIKELY(d->stale)) {
        lock_dbt(d, &t->stats);
        if (d->stale)
            recheck(d, c);
        unlock_dbt(d);
    }
    c->dbt_jump_cache = t->jc;
    c->dbt_tag = t->tag;
    t->jc_dirty = true; /* the lookup routine fills it */
    if (((uintptr_t)c->dbt_rs & ~(uintptr_t)((1u << RS_SHIFT) - 1)) != (uintptr_t)t->rs)
        c->dbt_rs = t->rs;
    for (;;) {
        /* Someone wants a full flush: stop along with the others, or do it ourselves */
        if (UNLIKELY(__atomic_load_n(&d->flushing, __ATOMIC_SEQ_CST) ||
                     __atomic_load_n(&d->flush_wanted, __ATOMIC_SEQ_CST))) {
            lock_dbt(d, &t->stats);
            if (d->flushing)
                park(d, t);
            else if (d->flush_wanted)
                flush_world(d, t);
            unlock_dbt(d);
            patch_site = NULL;
        }
        thread_catch_up(t);
        if (UNLIKELY(__atomic_load_n(&c->pending, __ATOMIC_RELAXED)) &&
            __atomic_exchange_n(&c->pending, 0, __ATOMIC_ACQUIRE))
            return TUXPORT_EXIT_KICK;

        /* Tag of the found block: shared blocks do not link to private blocks (LINK_SHARED) */
        unsigned found;
        uint32_t *code = table_lookup(d, c, c->pc, &found);
        if (!code && __atomic_load_n(&d->page_parked[page_bucket(c->pc >> TUXPORT_PAGE_SHIFT)],
                                     __ATOMIC_RELAXED)) {
            lock_dbt(d, &t->stats);
            if (!__atomic_load_n(&d->flushing, __ATOMIC_RELAXED) && !d->flush_wanted) {
                code = table_find_for(d->table, c->pc, t->tag, &found);
                if (!code) {
                    revive(d, c, c->pc);
                    code = table_find_for(d->table, c->pc, t->tag, &found);
                }
            }
            unlock_dbt(d);
        }
        if (!code) {
            /* Translate without the lock, install with it */
            bool fetched = prepare(t, c, c->pc);
            lock_dbt(d, &t->stats);
            if (__atomic_load_n(&d->flushing, __ATOMIC_RELAXED) || d->flush_wanted) {
                unlock_dbt(d);
                continue;
            }
            struct syncs sync = {.n = 0};
            struct fresh f = {.code = NULL};
            /* When this is the only thread, sync the new block and the link point together once;
             * otherwise sync the new block first and put it in the table, only after which other
             * threads can jump there */
            bool alone = d->threads == t && !t->next;
            /* Another thread may have just installed the same block */
            code = table_find_for(d->table, c->pc, t->tag, &found);
            if (!code && fetched) {
                code = install(d, t->prep, &sync, &f);
                found = t->prep->tag;
                if (code == NEED_FLUSH) {
                    flush_world(d, t);
                    unlock_dbt(d);
                    patch_site = NULL;
                    continue;
                }
                if (code == STALE) {
                    unlock_dbt(d);
                    continue;
                }
                if (!alone) {
                    sync_all(d, &sync);
                    table_insert(d, f.pc, f.tag, f.code, f.guest, f.nguest);
                }
            }
            /* While the lock is held the table has only live blocks: the patch target is never an
             * invalidated block (whose entry jumps to itself) */
            uint32_t *target = code && patch_forward ? code + ENTRY_CHECK_WORDS : code;
            if (code && patch_site && patch_generation == d->generation && target != patch_site &&
                !(patch_shared && found)) {
                *rw(d, patch_site) = enc_b((intptr_t)target - (intptr_t)patch_site);
                /* Conditional branches are not among the instructions CMODX allows to be modified
                 * concurrently: do not change it when there are other threads */
                bool branch = patch_branch_site && alone && relink_word(d, patch_branch_site, target);
                if (sync.n) { /* the new block is synced anyway, so the link point goes with it */
                    sync_add(&sync, patch_site, patch_site + 1);
                    if (branch)
                        sync_add(&sync, patch_branch_site, patch_branch_site + 1);
                } else {
                    publish_link(d, patch_site, branch ? patch_branch_site : NULL);
                }
            }
            sync_all(d, &sync);
            if (f.code && alone)
                table_insert(d, f.pc, f.tag, f.code, f.guest, f.nguest);
            unlock_dbt(d);
            patch_site = NULL;
        } else if (patch_site) {
            /*
             * The target is already translated: patch the link without the lock. Changing a b
             * into a b may be done concurrently; a full flush waits for this thread to stop, so
             * it cannot happen at the same time. The lock-free lookup may have found a block that
             * was just invalidated. Linking to it is harmless, since its entry jumps to its
             * forwarding stub, which still exits to here; the case to avoid is the block whose
             * old entry is the patch site itself (its forwarding stub's exit brought us here),
             * where the patch would make the entry branch to itself (hence target != patch_site)
             */
            uint32_t *target = patch_forward ? code + ENTRY_CHECK_WORDS : code;
            if (patch_generation == __atomic_load_n(&d->generation, __ATOMIC_ACQUIRE) &&
                target != patch_site && !(patch_shared && found)) {
                uint32_t b = enc_b((intptr_t)target - (intptr_t)patch_site);
                /* Read through the execution address; see relink_word */
                bool again = __atomic_load_n(patch_site, __ATOMIC_RELAXED) == b;
                __atomic_store_n(rw(d, patch_site), b, __ATOMIC_RELAXED);
                bool branch = patch_branch_site && __atomic_load_n(&d->nthreads, __ATOMIC_RELAXED) == 1 &&
                              relink_word(d, patch_branch_site, target);
                if (again) {
                    /* Already changed, yet this thread still executed the old instruction: do
                     * one full sync */
                    struct syncs sync = {.n = 0};
                    sync_add(&sync, patch_site, patch_site + 1);
                    if (branch)
                        sync_add(&sync, patch_branch_site, patch_branch_site + 1);
                    sync_all(d, &sync);
                } else {
                    publish_link(d, patch_site, branch ? patch_branch_site : NULL);
                }
            }
            patch_site = NULL;
        }
        if (!code) { /* cannot fetch: let Tier 0 report the precise fault */
            enum tuxport_exit r = tuxport_interp_step(c);
            if (r != TUXPORT_EXIT_NONE)
                return r;
            continue;
        }
        if (fill_jump_cache) {
            t->jc[(c->pc >> 2) & (JC_SIZE - 1)] = (struct slot){c->pc, code};
            t->jc_dirty = true;
            fill_jump_cache = false;
        }

        uint64_t reason = enter(c, code);
        switch (reason & REASON_MASK) {
        case R_CHAIN:
        case R_CHAIN_FWD:
            t->stats.exits_chain++;
            patch_site = (uint32_t *)(uintptr_t)c->dbt_site;
            patch_branch_site = reason >> REASON_BITS & BRANCH_MASK
                                    ? patch_site - (reason >> REASON_BITS & BRANCH_MASK)
                                    : NULL;
            patch_shared = reason & LINK_SHARED;
            patch_generation = __atomic_load_n(&d->generation, __ATOMIC_RELAXED);
            patch_forward = (reason & REASON_MASK) == R_CHAIN_FWD;
            break;
        case R_LOOKUP:
            t->stats.exits_lookup++;
            fill_jump_cache = true;
            break;
        case R_UNGROUP:
            /* With watchpoints the refill always fails (emit_refill), which does not mean this
             * group of accesses cannot share a check */
            if (!c->nwatch) {
                lock_dbt(d, &t->stats);
                ungroup(d, c->pc);
                unlock_dbt(d);
            }
            /* fallthrough */
        case R_INTERP: {
            t->stats.exits_interp++;
            enum tuxport_exit r = tuxport_interp_step(c);
            /*
             * When Tier 0 executes ic ivau on a second page, invalidate both pages. The
             * translations are shared, so invalidating once in place by content is enough, without
             * exiting to the caller. The invalidated blocks' entries jump to the forwarding
             * stubs, and other threads see that the next time they enter them (on hardware they
             * may likewise still execute old instructions before a context synchronization).
             * HotSpot does an ic ivau and a dsb for every code change, 80,000 times per Maven
             * build, and exiting for each one would take the BKL and log the change before coming
             * back to invalidate
             */
            if (r == TUXPORT_EXIT_CODE)
                code_changed(t, c, c->fault_addr);
            else if (r != TUXPORT_EXIT_NONE)
                return r;
            if (LIKELY(!c->ivau_page))
                break;
        }
        /* fallthrough */
        case R_CODE:
            code_changed(t, c, c->ivau_page);
            c->ivau_page = 0;
            break;
        case R_SVC:
            t->stats.exits_svc++;
            c->exit_info = 0;
            c->excl_size = 0; /* as in Tier 0: an exception return clears the exclusive marker */
            return TUXPORT_EXIT_SVC;
        default:
            t->stats.exits_kick++;
            break;
        }
    }
}

enum tuxport_exit tuxport_dbt_run(struct tuxport_dbt_thread *t, struct tuxport_cpu *c)
{
    struct tuxport_dbt *d = t->d;
    /* A host fault poisoned this instance: the remaining threads run in Tier 0 until the process
     * is killed */
    if (UNLIKELY(d->poisoned))
        return tuxport_cpu_run(c);
    tuxport_sync_dit(c);
    /* Register the cpu first, then look at flushing (see flush_world) */
    __atomic_store_n(&t->cpu, c, __ATOMIC_SEQ_CST);
    enum tuxport_exit r = tuxport_guard_run(dbt_run, t, c);
    if (UNLIKELY(r == TUXPORT_EXIT_HOST_FAULT)) {
        /* The guard has already released the locks held. Threads stopped waiting for a flush
         * must not wait forever */
        pthread_mutex_lock(&d->lock);
        d->poisoned = true;
        __atomic_store_n(&d->flushing, false, __ATOMIC_SEQ_CST);
        pthread_cond_broadcast(&d->flushed);
        pthread_mutex_unlock(&d->lock);
        template_clear(false);
    }
    __atomic_store_n(&t->cpu, NULL, __ATOMIC_RELEASE);
    return r;
}

void tuxport_dbt_get_stats(const struct tuxport_dbt_thread *t, struct tuxport_dbt_stats *out)
{
    *out = t->stats;
}

#else /* !DBT_SUPPORTED */

struct tuxport_dbt *tuxport_dbt_new(uint64_t key, enum tuxport_dbt_regs regs)
{
    (void)key;
    (void)regs;
    return NULL;
}

void tuxport_dbt_free(struct tuxport_dbt *d, uint64_t key)
{
    (void)d;
    (void)key;
}

void tuxport_dbt_trim(void)
{
}

void tuxport_dbt_unmap_written(void)
{
}

uint64_t tuxport_dbt_written_bytes(void)
{
    return 0;
}

void tuxport_dbt_set_sources(struct tuxport_dbt *d, const struct tuxport_dbt_source *s, size_t n)
{
    (void)d;
    (void)s;
    (void)n;
}

void tuxport_dbt_trust(struct tuxport_dbt *d, const struct tuxport_dbt_source *s, size_t n)
{
    (void)d;
    (void)s;
    (void)n;
}

void tuxport_dbt_retire_outside(struct tuxport_dbt *d, const struct tuxport_dbt_source *s, size_t n)
{
    (void)d;
    (void)s;
    (void)n;
}

unsigned tuxport_dbt_new_tag(struct tuxport_dbt *d)
{
    (void)d;
    return 0;
}

void tuxport_dbt_share(struct tuxport_dbt *d, const struct tuxport_dbt_source *s, size_t n)
{
    (void)d;
    (void)s;
    (void)n;
}

struct tuxport_dbt_thread *tuxport_dbt_attach(struct tuxport_dbt *d, unsigned tag)
{
    (void)d;
    (void)tag;
    return NULL;
}

void tuxport_dbt_detach(struct tuxport_dbt_thread *t)
{
    (void)t;
}

void tuxport_dbt_flush(struct tuxport_dbt_thread *t)
{
    (void)t;
}

void tuxport_dbt_recheck(struct tuxport_dbt_thread *t)
{
    (void)t;
}

void tuxport_dbt_invalidate(struct tuxport_dbt_thread *t, const struct tuxport_cpu *cpu, uint64_t start,
                           uint64_t len)
{
    (void)t;
    (void)cpu;
    (void)start;
    (void)len;
}

enum tuxport_exit tuxport_dbt_run(struct tuxport_dbt_thread *t, struct tuxport_cpu *c)
{
    (void)t;
    return tuxport_cpu_run(c);
}

void tuxport_dbt_get_stats(const struct tuxport_dbt_thread *t, struct tuxport_dbt_stats *out)
{
    (void)t;
    memset(out, 0, sizeof(*out));
}

#endif
