/*
 * The Tuxport engine's public interface (design doc §13.1, the engine and kernel/mmu maps).
 *
 * The engine only executes guest instructions and the soft-MMU fast path. On svc, a memory access
 * failure, an undefined instruction or brk, tuxport_cpu_run() returns an exit reason; the kernel
 * layer (Rust) handles it and calls tuxport_cpu_run() again to continue.
 *
 * Threading model: a struct tuxport_cpu can be run by only one thread; apart from
 * tuxport_cpu_kick(), no function is thread-safe. Several cpus can share one mmu and run at the
 * same time: the page table may be modified while it is being walked, but the caller must
 * serialize the modifications themselves. After unmapping or lowering permissions, the caller
 * must make every cpu using that mmu call tuxport_cpu_tlb_flush(), and must not free the unmapped
 * host memory before they all have.
 */
#ifndef TUXPORT_ENGINE_H
#define TUXPORT_ENGINE_H

#include <stddef.h>
#include <stdint.h>

#define TUXPORT_PAGE_SHIFT 12
#define TUXPORT_PAGE_SIZE (UINT64_C(1) << TUXPORT_PAGE_SHIFT)
#define TUXPORT_VA_BITS 48
#define TUXPORT_TLB_ENTRIES 4096 /* data TLB, direct-mapped */
#define TUXPORT_ITLB_ENTRIES 512 /* instruction-fetch TLB, used only by Tier 0 */

/* Page permissions, with the same values as Linux's PROT_* */
enum {
    TUXPORT_PROT_READ = 1u << 0,
    TUXPORT_PROT_WRITE = 1u << 1,
    TUXPORT_PROT_EXEC = 1u << 2,
};

/* Memory access type, for fault reporting */
enum tuxport_access {
    TUXPORT_ACCESS_READ = 1,
    TUXPORT_ACCESS_WRITE = 2,
    TUXPORT_ACCESS_EXEC = 3,
};

enum tuxport_fault_kind {
    TUXPORT_FAULT_PAGE = 1,     /* unmapped or wrong permissions; corresponds to SIGSEGV */
    /* the PC, or a data access that requires alignment, is misaligned; corresponds to SIGBUS */
    TUXPORT_FAULT_ALIGN = 2,
    /* SP misaligned when used as the base (fault_addr = SP); corresponds to SIGBUS */
    TUXPORT_FAULT_SP_ALIGN = 3,
    /* the access hit a hardware watchpoint (fault_addr = start of the access); corresponds to
     * SIGTRAP */
    TUXPORT_FAULT_WATCH = 4,
    /* the pc hit a hardware breakpoint (fault_addr = pc); corresponds to SIGTRAP */
    TUXPORT_FAULT_BREAK = 5,
};

/* Exit reasons of tuxport_cpu_run() */
enum tuxport_exit {
    /* used only by tuxport_cpu_exec_insn() and tuxport_cpu_step(): the instruction completed
     * normally */
    TUXPORT_EXIT_NONE = 0,
    TUXPORT_EXIT_SVC = 1,   /* pc already points at the next instruction, exit_info = imm16 */
    TUXPORT_EXIT_UNDEF = 2, /* pc points at the instruction, exit_info = instruction encoding */
    TUXPORT_EXIT_BRK = 3,   /* pc points at the instruction, exit_info = imm16 */
    /* pc points at the faulting instruction, the fault_* fields are valid, and the instruction
     * had no side effects */
    TUXPORT_EXIT_FAULT = 4,
    TUXPORT_EXIT_KICK = 5,  /* interrupted by tuxport_cpu_kick(); pending already cleared */
    /* ic ivau: pc already points at the next instruction, fault_addr = its address (tag
     * removed) */
    TUXPORT_EXIT_CODE = 6,
    /* A bug in the engine itself made the host receive SIGSEGV, SIGBUS or SIGILL (only after
     * tuxport_fault_guard_install()): guest state cannot be trusted and the caller should end this
     * guest process. Details from tuxport_host_fault_last(); the Tier 1 instance is already
     * poisoned, and tuxport_dbt_free() releases it directly */
    TUXPORT_EXIT_HOST_FAULT = 7,
};

/*
 * Data TLB entry: host address = guest address + addend. tag and addend are stored next to each
 * other, and Tier 1's generated code loads both with one ldp. tag holds the inverted guest page
 * number, and 0 means invalid: zeroed memory is an empty TLB that need not be written entry by
 * entry (so untouched pages take no memory either).
 */
struct tuxport_tlb_entry {
    uint64_t tag_read; /* ~guest page number; 0 means invalid */
    uint64_t addend_read;
    uint64_t tag_write;
    uint64_t addend_write;
};

/*
 * Hardware watchpoint (ptrace's NT_ARM_HW_WATCH): hits when any byte in [lo, hi) is accessed with
 * a kind in type (TUXPORT_WATCH_*); the instruction is not executed.
 */
#define TUXPORT_WATCH_LOAD 1u
#define TUXPORT_WATCH_STORE 2u
#define TUXPORT_HW_SLOTS 16
struct tuxport_watch {
    uint64_t lo, hi;
    uint32_t type;
    uint32_t reserved;
};

/* Instruction-fetch TLB entry, used only by Tier 0's instruction fetch (Tier 1's translation
 * reads guest code through the page table) */
struct tuxport_itlb_entry {
    uint64_t tag;
    uint64_t addend;
};

struct tuxport_mmu;

/*
 * Guest CPU state. The Rust side mirrors the layout field by field (core/engine/src/lib.rs);
 * changes must be made in both, and are checked by tuxport_cpu_layout_check().
 */
struct tuxport_cpu {
    /* Tier 1's generated code reads and writes the fields before tlb directly with ldr/str
     * immediate offsets */
    uint64_t x[31];
    uint64_t sp;
    uint64_t pc;
    uint64_t nzcv; /* only bits 31..28 are used */
    uint64_t fpcr;
    uint64_t fpsr;
    uint64_t tpidr_el0;
    /* x0 restored when a syscall restarts (the kernel/signal map), maintained by the kernel
     * layer */
    uint64_t orig_x0;
    uint64_t fault_addr;
    uint32_t fault_access; /* enum tuxport_access */
    uint32_t fault_kind;   /* enum tuxport_fault_kind */
    uint32_t exit_info;
    /* Tier 1: odd means a lock-free lookup of the translation table is in progress (dbt.c's
     * table_lookup) */
    uint32_t dbt_lookup_seq;
    /* accessed only atomically; 64 bits so that Tier 1 can read it with one ldr */
    uint64_t pending;
    struct tuxport_mmu *mmu;
    /* Exclusive monitor (LDXR/STXR): the recorded address, size and loaded value */
    uint64_t excl_addr;
    uint64_t excl_value[2];
    uint32_t excl_size; /* 0 means no exclusive mark */
    /* Tier 1: the tag of the thread's address space in the group sharing translations (dbt.c's
     * TAG_SHIFT) */
    uint32_t dbt_tag;
    /* The following fields are used internally by Tier 1 */
    uint64_t dbt_reason;
    uint64_t dbt_site;
    void *dbt_jump_cache;
    void *dbt_rs; /* top of the shadow return stack (a 1 KB-aligned ring in the Tier 1 instance) */
    /* The page recorded by Tier 1's ic ivau whose translations are not invalidated yet (0 means
     * none): tuxport_dbt_run() invalidates it in place at the next dsb or isb, and the caller
     * collects it at other exits; also aligns tlb to 16 bytes */
    uint64_t ivau_page;
    /* Host base addresses of several access groups, kept by Tier 1 while a block runs (dbt.c's
     * plan_group) */
    uint64_t dbt_groups[4];
    /* Tier 1 addresses entries as "ldp [cpu + index * 32 + offset]", so the offset of tlb must
     * not exceed the range of ldp's immediate */
    struct tuxport_tlb_entry tlb[TUXPORT_TLB_ENTRIES];
    struct tuxport_itlb_entry itlb[TUXPORT_ITLB_ENTRIES];
    uint64_t v[32][2]; /* V0–V31, low 64 bits first */
    /* tlb entries that were filled; flushing only handles these */
    uint64_t tlb_used[TUXPORT_TLB_ENTRIES / 64];
    /* Hardware breakpoints and watchpoints (ptrace's NT_ARM_HW_BREAK, NT_ARM_HW_WATCH): the first
     * nbrk, nwatch entries of brk and watch are valid. Only Tier 0 checks them: a thread with
     * breakpoints runs only in Tier 0; with watchpoints, watched pages stay out of tlb
     * (tuxport_tlb_fill), and Tier 1 falls back to Tier 0 when accessing them. tlb must be flushed
     * after the watchpoints change */
    uint32_t nbrk, nwatch;
    /* PSTATE.DIT (0 or 1): read and written by MSR and MRS DIT; bit 24 of pstate in signal frames
     * and ptrace. When entering guest code the host thread's DIT follows it (tuxport_sync_dit) */
    uint32_t dit;
    uint32_t reserved;
    uint64_t brk[TUXPORT_HW_SLOTS];
    struct tuxport_watch watch[TUXPORT_HW_SLOTS];
};

/* ---- Guest-visible CPU features (the engine map), with the same values as Linux's HWCAP_* ---- */

#define TUXPORT_HWCAP_FP (UINT64_C(1) << 0)
#define TUXPORT_HWCAP_ASIMD (UINT64_C(1) << 1)
#define TUXPORT_HWCAP_AES (UINT64_C(1) << 3)
#define TUXPORT_HWCAP_PMULL (UINT64_C(1) << 4)
#define TUXPORT_HWCAP_SHA1 (UINT64_C(1) << 5)
#define TUXPORT_HWCAP_SHA2 (UINT64_C(1) << 6)
#define TUXPORT_HWCAP_CRC32 (UINT64_C(1) << 7)
#define TUXPORT_HWCAP_ATOMICS (UINT64_C(1) << 8)
#define TUXPORT_HWCAP_FPHP (UINT64_C(1) << 9)
#define TUXPORT_HWCAP_ASIMDHP (UINT64_C(1) << 10)
#define TUXPORT_HWCAP_CPUID (UINT64_C(1) << 11)
#define TUXPORT_HWCAP_ASIMDRDM (UINT64_C(1) << 12)
#define TUXPORT_HWCAP_JSCVT (UINT64_C(1) << 13)
#define TUXPORT_HWCAP_FCMA (UINT64_C(1) << 14)
#define TUXPORT_HWCAP_LRCPC (UINT64_C(1) << 15)
#define TUXPORT_HWCAP_DCPOP (UINT64_C(1) << 16)
#define TUXPORT_HWCAP_SHA3 (UINT64_C(1) << 17)
#define TUXPORT_HWCAP_ASIMDDP (UINT64_C(1) << 20)
#define TUXPORT_HWCAP_SHA512 (UINT64_C(1) << 21)
#define TUXPORT_HWCAP_ASIMDFHM (UINT64_C(1) << 23)
#define TUXPORT_HWCAP_DIT (UINT64_C(1) << 24)
#define TUXPORT_HWCAP_ILRCPC (UINT64_C(1) << 26)
#define TUXPORT_HWCAP_FLAGM (UINT64_C(1) << 27)
#define TUXPORT_HWCAP_SB (UINT64_C(1) << 29)

/* AT_HWCAP2 bits, with the same values as Linux's HWCAP2_* */
#define TUXPORT_HWCAP2_DCPODP (UINT64_C(1) << 0)
#define TUXPORT_HWCAP2_FLAGM2 (UINT64_C(1) << 7)
#define TUXPORT_HWCAP2_FRINT (UINT64_C(1) << 8)
#define TUXPORT_HWCAP2_I8MM (UINT64_C(1) << 13)
#define TUXPORT_HWCAP2_BF16 (UINT64_C(1) << 14)
#define TUXPORT_HWCAP2_ECV (UINT64_C(1) << 19)

/* The AT_HWCAP advertised to the guest: host support ∩ implemented by Tier 0 ∩ the whitelist */
uint64_t tuxport_guest_hwcap(void);

/* The AT_HWCAP2 advertised to the guest, by the same rule */
uint64_t tuxport_guest_hwcap2(void);

/* The MIDR_EL1 the guest reads (0 when the host does not allow reading ID registers) */
uint64_t tuxport_guest_midr(void);

/* ---- soft-MMU page table (the kernel/mmu map) ---- */

enum {
    TUXPORT_OK = 0,
    /* argument not 4K-aligned, beyond the 48-bit address space, or invalid permission bits */
    TUXPORT_EINVAL = -1,
    TUXPORT_ENOMEM = -2, /* page table allocation failed; pages already done keep the new mapping */
};

struct tuxport_mmu *tuxport_mmu_new(void);
void tuxport_mmu_free(struct tuxport_mmu *mmu);

/*
 * Maps guest [gva, gva + len) to host [host, host + len), replacing existing mappings. gva, host
 * and len must all be 4K-aligned. The caller owns the host memory, which must stay valid while the
 * mapping exists.
 */
int tuxport_mmu_map(struct tuxport_mmu *mmu, uint64_t gva, void *host, uint64_t len, uint32_t prot);
/* Same as tuxport_mmu_map, but pages with existing entries (including ones with permission 0) stay
 * unchanged: on-demand filling */
int tuxport_mmu_fill(struct tuxport_mmu *mmu, uint64_t gva, void *host, uint64_t len, uint32_t prot);
int tuxport_mmu_unmap(struct tuxport_mmu *mmu, uint64_t gva, uint64_t len);
/* Changes only the permissions of existing entries (possibly to 0: the host page is kept but
 * cannot be accessed); pages without entries stay unchanged */
int tuxport_mmu_protect(struct tuxport_mmu *mmu, uint64_t gva, uint64_t len, uint32_t prot);
/* Moves the existing entries in [from, from + len) unchanged to the same positions starting at to,
 * clearing the originals; the two ranges must not overlap */
int tuxport_mmu_move(struct tuxport_mmu *mmu, uint64_t from, uint64_t to, uint64_t len);
/* The first page in [gva, end) with an entry (including ones with permission 0); end if none */
uint64_t tuxport_mmu_next_present(const struct tuxport_mmu *mmu, uint64_t gva, uint64_t end);

/*
 * Looks up the host pointer for a guest address (including the offset in the page). Every
 * permission in required_prot must be present, otherwise NULL is returned; 0 means only an
 * accessible entry is required (ones with permission 0 do not count).
 */
void *tuxport_mmu_translate(const struct tuxport_mmu *mmu, uint64_t gva, uint32_t required_prot);

/* ---- CPU ---- */

/*
 * Allocates and initializes a cpu (NULL on failure); release it with tuxport_cpu_free(). The TLB in
 * the struct is over a hundred KB, so it is allocated with mmap and unused pages take no memory.
 */
struct tuxport_cpu *tuxport_cpu_new(struct tuxport_mmu *mmu);
void tuxport_cpu_free(struct tuxport_cpu *cpu);

/* Clears all state and binds mmu (may be NULL, in which case every memory access fails) */
void tuxport_cpu_init(struct tuxport_cpu *cpu, struct tuxport_mmu *mmu);
void tuxport_cpu_tlb_flush(struct tuxport_cpu *cpu);

/* Executes from cpu->pc until the kernel layer is needed */
enum tuxport_exit tuxport_cpu_run(struct tuxport_cpu *cpu);

/* Executes the given instruction at cpu->pc (without fetching), for the lockstep tests */
enum tuxport_exit tuxport_cpu_exec_insn(struct tuxport_cpu *cpu, uint32_t insn);

/* Executes the instruction at cpu->pc under the interpreter (ptrace single-step), ignoring
 * pending */
enum tuxport_exit tuxport_cpu_step(struct tuxport_cpu *cpu);

/* May be called from any thread: makes tuxport_cpu_run() return TUXPORT_EXIT_KICK at the next
 * instruction boundary */
void tuxport_cpu_kick(struct tuxport_cpu *cpu);

/* ---- Host-side fault guard (the engine map, design doc §12 item 5) ---- */

/* A fault signal received by the host (TUXPORT_EXIT_HOST_FAULT) */
struct tuxport_host_fault {
    int32_t signo;
    int32_t code;  /* si_code */
    uint64_t addr; /* si_addr: a host address */
    uint64_t pc;   /* the faulting host instruction */
};

/*
 * Installs the host's SIGSEGV, SIGBUS and SIGILL handlers; may be called repeatedly. Afterwards
 * these faults during tuxport_cpu_run() or tuxport_dbt_run() become TUXPORT_EXIT_HOST_FAULT; other
 * faults first write a one-line report to stderr and are then passed to the original handling (on
 * Android, debuggerd produces a tombstone from it).
 */
void tuxport_fault_guard_install(void);

/* Details of the current thread's latest TUXPORT_EXIT_HOST_FAULT */
void tuxport_host_fault_last(struct tuxport_host_fault *out);

/* Checks the struct layout mirrored on the Rust side; returns 1 when it matches */
int tuxport_cpu_layout_check(size_t size, size_t off_x, size_t off_pc, size_t off_tpidr,
                            size_t off_pending, size_t off_mmu, size_t off_jump_cache,
                            size_t off_tlb, size_t off_v);

/* ---- Tier 1: a same-ISA pass-through DBT (the engine map) ---- */

struct tuxport_dbt;

/* Translation and run statistics */
struct tuxport_dbt_stats {
    uint64_t blocks;         /* blocks translated */
    uint64_t insns;          /* guest instructions translated */
    uint64_t insns_copied;   /* copied verbatim */
    uint64_t insns_remapped; /* involve a stolen register or SP, rewritten before execution */
    uint64_t insns_memory;   /* memory access instructions rewritten into the TLB fast path */
    uint64_t insns_fallback; /* instructions handed to Tier 0 (static count) */
    uint64_t exits_chain;    /* exits to the dispatcher at run time, by reason */
    uint64_t exits_lookup;
    uint64_t exits_interp;
    uint64_t exits_svc;
    uint64_t exits_kick;
    uint64_t flushes;
    uint64_t invalidated;   /* blocks invalidated by tuxport_dbt_invalidate() */
    uint64_t blocks_shared; /* blocks among blocks obtained by copying a translation template */
    uint64_t insns_grouped; /* of insns_memory, those sharing one TLB check with an earlier one */
    uint64_t ungrouped;     /* times a shared check failed and it was retranslated ungrouped */
};

/*
 * The Tier 1 translations of one address space (code cache, translation table): shared by all
 * threads in that address space; each thread attaches with tuxport_dbt_attach() to get its own part
 * (jump cache, shadow return stack, statistics) and runs with it. Returns NULL when the host is not
 * aarch64 Linux. tuxport_dbt_free() is called after every thread has detached; the instance goes
 * back to the pool, and when there are not many translations they are kept and checked block by
 * block before the next user runs. `key` distinguishes programs (0 means no distinction):
 * tuxport_dbt_new() first takes back translations left under the same key. `regs` selects which
 * host registers the translated code takes for itself (enum tuxport_dbt_regs); it only affects
 * speed.
 *
 * tuxport_dbt_run() handles ic ivau in place: it invalidates the page recorded in ivau_page at the
 * next dsb or isb, and the page of an ic ivau it hands to Tier 0 as soon as Tier 0 runs it.
 * Otherwise translations never invalidate themselves: after TUXPORT_EXIT_CODE, after an exit that
 * leaves ivau_page set, or after an executable mapping is torn down or changes permissions, the
 * caller must call tuxport_dbt_invalidate() or tuxport_dbt_flush() (once per address space, through
 * any one attached thread).
 */
enum tuxport_dbt_regs {
    TUXPORT_DBT_REGS_DEFAULT, /* x16, x17, x26–x28: rarely used in compiler-generated code */
    /* x21–x25: Go programs use x16 and x26–x28 heavily and these hardly at all */
    TUXPORT_DBT_REGS_GO,
};
struct tuxport_dbt *tuxport_dbt_new(uint64_t key, enum tuxport_dbt_regs regs);
void tuxport_dbt_free(struct tuxport_dbt *dbt, uint64_t key);
/* Releases the translations kept in the pool (when host memory is tight) */
void tuxport_dbt_trim(void);
/*
 * Unmaps the pages of every instance's code cache written through the writable view (the one used
 * for writing in the W^X double mapping), keeping the content: the host counts RSS per mapping,
 * and pages touched through both views count twice. The caller calls it periodically (the kernel
 * layer's timer thread, once a second); any thread may call it
 */
void tuxport_dbt_unmap_written(void);
/* How many bytes in total were written through the writable view and not yet unmapped (they count
 * twice in the host's RSS) */
uint64_t tuxport_dbt_written_bytes(void);

/*
 * A mapping that translations come from: a guest address range, and an identity given by the
 * caller (the file and its version) from which identical content can be concluded. Two mappings
 * with the same identity always have the same content; the caller guarantees it.
 */
struct tuxport_dbt_source {
    uint64_t start, len;
    uint64_t id[5];
};

/* Before the address space goes out of use (the translations go back to the pool): records the
 * mappings the translations come from, at most TUXPORT_DBT_SOURCES ranges */
#define TUXPORT_DBT_SOURCES 64
void tuxport_dbt_set_sources(struct tuxport_dbt *dbt, const struct tuxport_dbt_source *s, size_t n);
/*
 * Before an instance taken from the pool first runs in a new address space: blocks in mappings of
 * the new address space that are exactly the same as the recorded ones need not be checked block
 * by block (on execve, the code segments of the program and the dynamic linker; checking them
 * would read every page)
 */
void tuxport_dbt_trust(struct tuxport_dbt *dbt, const struct tuxport_dbt_source *s, size_t n);
/*
 * Retires blocks whose guest pc is not in these mappings (parked ones included): before the
 * instance is shared by several address spaces, drops the translations valid only for the
 * original address space (for example, ones left by the previous user from the pool). Other
 * threads may be running
 */
void tuxport_dbt_retire_outside(struct tuxport_dbt *dbt, const struct tuxport_dbt_source *s, size_t n);
/*
 * When several address spaces share one instance: the trusted file mappings they all map
 * identically (the same version of the same file at the same addresses), all of them given every
 * time, only growing. Blocks in these mappings are shared by all members, and blocks elsewhere
 * (JIT-generated code) are only for the member that translated them, told apart by the tag given
 * when attaching (tuxport_dbt_attach)
 */
void tuxport_dbt_share(struct tuxport_dbt *dbt, const struct tuxport_dbt_source *s, size_t n);
/* Allocates a tag for one address space sharing this instance (1–65535, not repeated until the
 * instance goes back to the pool); returns 0 when they run out */
unsigned tuxport_dbt_new_tag(struct tuxport_dbt *dbt);

struct tuxport_dbt_thread;
/* `tag` is the tag of the thread's address space (tuxport_dbt_new_tag), 0 when not shared */
struct tuxport_dbt_thread *tuxport_dbt_attach(struct tuxport_dbt *dbt, unsigned tag);
void tuxport_dbt_detach(struct tuxport_dbt_thread *t);

/* Discards all translations: this has to wait until every thread has left translated code, so it
 * is only a request, completed by the next thread to enter tuxport_dbt_run() */
void tuxport_dbt_flush(struct tuxport_dbt_thread *t);

/* About to run in a different address space (after exec): keeps the translations and, before the
 * next run, checks the guest instruction words block by block, invalidating the ones that do not
 * match. Instances taken from the pool are handled the same way. */
void tuxport_dbt_recheck(struct tuxport_dbt_thread *t);

/* Invalidates the blocks starting in the pages covered by [start, start + len); when `cpu` is
 * given, only those whose guest instruction words differ from the current ones in its address
 * space. When there is no room it is the same as tuxport_dbt_flush() */
void tuxport_dbt_invalidate(struct tuxport_dbt_thread *t, const struct tuxport_cpu *cpu, uint64_t start,
                           uint64_t len);

/* Same semantics as tuxport_cpu_run(), but executes in translated code */
enum tuxport_exit tuxport_dbt_run(struct tuxport_dbt_thread *t, struct tuxport_cpu *cpu);
/* This thread's statistics: how many times it exited, and the translations and invalidations it
 * did */
void tuxport_dbt_get_stats(const struct tuxport_dbt_thread *t, struct tuxport_dbt_stats *out);

#endif /* TUXPORT_ENGINE_H */
