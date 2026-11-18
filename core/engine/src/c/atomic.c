/*
 * Exclusive accesses, acquire/release and LSE atomics (the engine map).
 *
 * LDXR/STXR cannot be handed to the host as they are: our own code accesses memory between the
 * two instructions, and the host's exclusive monitor may be cleared at any time, so STXR could
 * fail forever. Instead they are emulated with compare-and-swap: LDXR records the address, size
 * and loaded value, and STXR does a CAS on the host address, succeeding only if the memory value
 * still equals the recorded one (the same approach as QEMU, ABA allowed).
 *
 * LSE atomic instructions (LDADD, SWP, CAS, CASP and so on) are single instructions themselves,
 * and run as native stubs on the translated host address, with semantics and memory ordering
 * exactly as on the hardware.
 */
#include "internal.h"

#define BIT(x, n) ((unsigned)(((x) >> (n)) & 1u))
#define FIELD(x, hi, lo) ((unsigned)(((x) >> (lo)) & ((1u << ((hi) - (lo) + 1)) - 1)))

static enum tuxport_exit undef(struct tuxport_cpu *c, uint32_t insn)
{
    c->exit_info = insn;
    return TUXPORT_EXIT_UNDEF;
}

static enum tuxport_exit next(struct tuxport_cpu *c)
{
    c->pc += 4;
    return TUXPORT_EXIT_NONE;
}

static void set_x(struct tuxport_cpu *c, unsigned r, uint64_t v)
{
    if (r != 31)
        c->x[r] = v;
}

static uint64_t get_x(const struct tuxport_cpu *c, unsigned r)
{
    return r == 31 ? 0 : c->x[r];
}

/*
 * Gets the host address of an aligned access. These instructions require natural alignment,
 * otherwise it is an alignment fault; an aligned access never crosses a page. When access is
 * write, read permission is also required. watch is which kind of hardware watchpoint this
 * access counts as (TUXPORT_WATCH_*; a read-modify-write counts as both).
 */
static void *atomic_target_off(struct tuxport_cpu *c, unsigned rn, int64_t off, unsigned bytes,
                               uint32_t access, uint32_t watch)
{
    uint64_t va = rn == 31 ? c->sp : c->x[rn];
    if (rn == 31 && (c->sp & 15)) {
        raise_fault(c, c->sp, access, TUXPORT_FAULT_SP_ALIGN);
        return NULL;
    }
    va = untag(va + (uint64_t)off);
    if (va & (bytes - 1)) {
        raise_fault(c, va, access, TUXPORT_FAULT_ALIGN);
        return NULL;
    }
    if (access == TUXPORT_ACCESS_WRITE && !tlb_lookup(c, va, TUXPORT_ACCESS_READ)) {
        raise_fault(c, va, TUXPORT_ACCESS_READ, TUXPORT_FAULT_PAGE);
        return NULL;
    }
    void *p = tlb_lookup(c, va, access);
    if (!p)
        raise_fault(c, va, access, TUXPORT_FAULT_PAGE);
    else if (UNLIKELY(watch_hit(c, va, bytes, watch)))
        return raise_fault(c, va, access, TUXPORT_FAULT_WATCH), NULL;
    return p;
}

static void *atomic_target(struct tuxport_cpu *c, unsigned rn, unsigned bytes, uint32_t access,
                           uint32_t watch)
{
    return atomic_target_off(c, rn, 0, bytes, access, watch);
}

static void load_atomic(const void *p, unsigned bytes, bool acquire, uint64_t out[2])
{
    int order = acquire ? __ATOMIC_ACQUIRE : __ATOMIC_RELAXED;
    out[1] = 0;
    switch (bytes) {
    case 1: out[0] = __atomic_load_n((const uint8_t *)p, order); break;
    case 2: out[0] = __atomic_load_n((const uint16_t *)p, order); break;
    case 4: out[0] = __atomic_load_n((const uint32_t *)p, order); break;
    case 8: out[0] = __atomic_load_n((const uint64_t *)p, order); break;
    default: {
#ifdef __aarch64__
        /* 16 bytes: a pair of exclusive loads guarantees single-copy atomicity, and the host
         * monitor is cleared right after the read */
        uint64_t lo, hi;
        __asm__ volatile("ldaxp %0, %1, [%2]\n\tclrex" : "=&r"(lo), "=&r"(hi) : "r"(p) : "memory");
        out[0] = lo;
        out[1] = hi;
#else
        memcpy(out, p, 16);
#endif
        break;
    }
    }
}

static bool cas(void *p, unsigned bytes, const uint64_t expected[2], const uint64_t desired[2])
{
    switch (bytes) {
    case 1: {
        uint8_t e = (uint8_t)expected[0];
        return __atomic_compare_exchange_n((uint8_t *)p, &e, (uint8_t)desired[0], false,
                                           __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
    }
    case 2: {
        uint16_t e = (uint16_t)expected[0];
        return __atomic_compare_exchange_n((uint16_t *)p, &e, (uint16_t)desired[0], false,
                                           __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
    }
    case 4: {
        uint32_t e = (uint32_t)expected[0];
        return __atomic_compare_exchange_n((uint32_t *)p, &e, (uint32_t)desired[0], false,
                                           __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
    }
    case 8: {
        uint64_t e = expected[0];
        return __atomic_compare_exchange_n((uint64_t *)p, &e, desired[0], false, __ATOMIC_SEQ_CST,
                                           __ATOMIC_SEQ_CST);
    }
    default: {
#ifdef __aarch64__
        unsigned ok;
        __asm__ volatile("1: ldaxp x16, x17, [%1]\n\t"
                         "cmp x16, %2\n\t"
                         "ccmp x17, %3, #0, eq\n\t"
                         "b.ne 2f\n\t"
                         "stlxp %w0, %4, %5, [%1]\n\t"
                         "cbnz %w0, 1b\n\t"
                         "mov %w0, #1\n\t"
                         "b 3f\n"
                         "2: clrex\n\t"
                         "mov %w0, #0\n"
                         "3:"
                         : "=&r"(ok)
                         : "r"(p), "r"(expected[0]), "r"(expected[1]), "r"(desired[0]), "r"(desired[1])
                         : "x16", "x17", "cc", "memory");
        return ok;
#else
        if (memcmp(p, expected, 16) != 0)
            return false;
        memcpy(p, desired, 16);
        return true;
#endif
    }
    }
}

/* LDXR/LDAXR/LDXP/LDAXP, STXR/STLXR/STXP/STLXP */
static enum tuxport_exit exclusive(struct tuxport_cpu *c, uint32_t insn, bool pair)
{
    unsigned size = FIELD(insn, 31, 30), load = BIT(insn, 22), ordered = BIT(insn, 15);
    unsigned rs = FIELD(insn, 20, 16), rt2 = FIELD(insn, 14, 10), rn = FIELD(insn, 9, 5);
    unsigned rt = insn & 31;
    unsigned esize = 1u << size, bytes = pair ? 2 * esize : esize;
    void *p = atomic_target(c, rn, bytes, load ? TUXPORT_ACCESS_READ : TUXPORT_ACCESS_WRITE,
                            load ? TUXPORT_WATCH_LOAD : TUXPORT_WATCH_STORE);
    if (!p)
        return TUXPORT_EXIT_FAULT;
    uint64_t va = untag(rn == 31 ? c->sp : c->x[rn]);

    if (load) {
        uint64_t v[2];
        load_atomic(p, bytes, ordered, v);
        c->excl_addr = va;
        c->excl_size = bytes;
        c->excl_value[0] = v[0];
        c->excl_value[1] = v[1];
        if (!pair) {
            set_x(c, rt, v[0]);
        } else if (esize == 4) {
            set_x(c, rt, (uint32_t)v[0]);
            set_x(c, rt2, v[0] >> 32);
        } else {
            set_x(c, rt, v[0]);
            set_x(c, rt2, v[1]);
        }
        return next(c);
    }

    uint64_t desired[2] = {get_x(c, rt), 0};
    if (pair && esize == 4)
        desired[0] = (uint32_t)desired[0] | get_x(c, rt2) << 32;
    else if (pair)
        desired[1] = get_x(c, rt2);
    if (esize < 8 && !pair)
        desired[0] &= (UINT64_C(1) << (8 * esize)) - 1;
    bool ok = c->excl_size == bytes && c->excl_addr == va && cas(p, bytes, c->excl_value, desired);
    c->excl_size = 0;
    set_x(c, rs, ok ? 0 : 1);
    return next(c);
}

static void store_release(void *p, unsigned bytes, uint64_t v)
{
    switch (bytes) {
    case 1: __atomic_store_n((uint8_t *)p, (uint8_t)v, __ATOMIC_RELEASE); break;
    case 2: __atomic_store_n((uint16_t *)p, (uint16_t)v, __ATOMIC_RELEASE); break;
    case 4: __atomic_store_n((uint32_t *)p, (uint32_t)v, __ATOMIC_RELEASE); break;
    default: __atomic_store_n((uint64_t *)p, v, __ATOMIC_RELEASE); break;
    }
}

/* LDAR/STLR */
static enum tuxport_exit acquire_release(struct tuxport_cpu *c, uint32_t insn)
{
    unsigned size = FIELD(insn, 31, 30), load = BIT(insn, 22), rn = FIELD(insn, 9, 5);
    unsigned rt = insn & 31, bytes = 1u << size;
    void *p = atomic_target(c, rn, bytes, load ? TUXPORT_ACCESS_READ : TUXPORT_ACCESS_WRITE,
                            load ? TUXPORT_WATCH_LOAD : TUXPORT_WATCH_STORE);
    if (!p)
        return TUXPORT_EXIT_FAULT;
    if (load) {
        uint64_t v[2];
        load_atomic(p, bytes, true, v);
        set_x(c, rt, v[0]);
        return next(c);
    }
    store_release(p, bytes, get_x(c, rt));
    return next(c);
}

/* LDAPR, LDAPRB, LDAPRH (LRCPC): read as LDAR does; acquire is stronger than RCpc, which the
 * architecture allows */
static enum tuxport_exit load_acquire_rcpc(struct tuxport_cpu *c, uint32_t insn)
{
    unsigned bytes = 1u << FIELD(insn, 31, 30), rt = insn & 31;
    void *p = atomic_target(c, FIELD(insn, 9, 5), bytes, TUXPORT_ACCESS_READ, TUXPORT_WATCH_LOAD);
    if (!p)
        return TUXPORT_EXIT_FAULT;
    uint64_t v[2];
    load_atomic(p, bytes, true, v);
    set_x(c, rt, v[0]);
    return next(c);
}

/*
 * LRCPC2's STLUR*, LDAPUR*, LDAPURS*: base plus a 9-bit signed offset, written and read as STLR
 * and LDAR do (alignment required). opc is 00 store, 01 load with zero extension, 10 load with
 * sign extension to 64 bits, 11 extension to 32 bits
 */
static enum tuxport_exit rcpc_unscaled(struct tuxport_cpu *c, uint32_t insn)
{
    unsigned bytes = 1u << FIELD(insn, 31, 30), opc = FIELD(insn, 23, 22), rt = insn & 31;
    int64_t off = (int64_t)((uint64_t)FIELD(insn, 20, 12) << 55) >> 55;
    bool load = opc != 0;
    void *p = atomic_target_off(c, FIELD(insn, 9, 5), off, bytes,
                                load ? TUXPORT_ACCESS_READ : TUXPORT_ACCESS_WRITE,
                                load ? TUXPORT_WATCH_LOAD : TUXPORT_WATCH_STORE);
    if (!p)
        return TUXPORT_EXIT_FAULT;
    if (!load) {
        store_release(p, bytes, get_x(c, rt));
        return next(c);
    }
    uint64_t v[2];
    load_atomic(p, bytes, true, v);
    unsigned shift = 64 - 8 * bytes;
    if (opc >= 2)
        v[0] = (uint64_t)((int64_t)(v[0] << shift) >> shift);
    if (opc == 3)
        v[0] = (uint32_t)v[0];
    set_x(c, rt, v[0]);
    return next(c);
}

/* Executes one LSE instruction as is on the translated host address */
static enum tuxport_exit native_atomic(struct tuxport_cpu *c, uint32_t insn, unsigned bytes,
                                      const struct native_desc *d)
{
    void *p = atomic_target(c, FIELD(insn, 9, 5), bytes, TUXPORT_ACCESS_WRITE,
                            TUXPORT_WATCH_LOAD | TUXPORT_WATCH_STORE);
    if (!p)
        return TUXPORT_EXIT_FAULT;
    if (!tuxport_native_exec(insn, d, c, p))
        return undef(c, insn);
    return next(c);
}

enum tuxport_exit tuxport_atomic_exec(struct tuxport_cpu *c, uint32_t insn)
{
    bool lse = tuxport_has(TUXPORT_HWCAP_ATOMICS);
    unsigned size = FIELD(insn, 31, 30);

    if ((insn & 0x3f000000) == 0x08000000) { /* exclusives, acquire/release, CAS, CASP */
        unsigned o2 = BIT(insn, 23), o1 = BIT(insn, 21), o0 = BIT(insn, 15);
        /*
         * Fields the architecture says "should be one" (SBO): exclusive accesses and LDAR/STLR
         * ignore them; CAS/CASP need Rt2 all ones, otherwise the encoding is unallocated. This
         * matches LLVM's decoding.
         */
        bool rt2_ones = FIELD(insn, 14, 10) == 31;
        if (!o2 && !o1)
            return exclusive(c, insn, false);
        if (!o2 && o1) {
            if (size >= 2)
                return exclusive(c, insn, true);
            /* CASP: Rs and Rt must both be even-numbered register pairs */
            unsigned rs = FIELD(insn, 20, 16), rt = insn & 31;
            if (!lse || (rs & 1) || (rt & 1) || !rt2_ones)
                return undef(c, insn);
            struct native_desc d = {.addr_pos = 5, .n = 2};
            d.gpr[0] = (struct native_operand){.pos = 16, .pair = 1, .load = 1, .store = 1};
            d.gpr[1] = (struct native_operand){.pos = 0, .pair = 1, .load = 1};
            return native_atomic(c, insn, 8u << size, &d);
        }
        if (o2 && !o1) /* LDAR/STLR; LDLAR/STLLR with o0 = 0 belong to the LOR extension */
            return o0 ? acquire_release(c, insn) : undef(c, insn);
        if (!lse || !rt2_ones)
            return undef(c, insn);
        /* CAS: Rs is both the compare value and where the old value is returned */
        struct native_desc d = {.addr_pos = 5, .n = 2};
        d.gpr[0] = (struct native_operand){.pos = 16, .load = 1, .store = 1};
        d.gpr[1] = (struct native_operand){.pos = 0, .load = 1};
        return native_atomic(c, insn, 1u << size, &d);
    }

    /* size 11 has no sign-extending load, size 10 none extending to 32 bits */
    if ((insn & 0x3f200c00) == 0x19000000) {
        unsigned opc = FIELD(insn, 23, 22);
        if (!tuxport_has(TUXPORT_HWCAP_ILRCPC) || (size == 3 && opc >= 2) || (size == 2 && opc == 3))
            return undef(c, insn);
        return rcpc_unscaled(c, insn);
    }
    /* LDAPR is in the LSE group: A = 1, R = 0, Rs all ones (the only form LLVM accepts), o3 = 1,
     * opc = 100 */
    if ((insn & 0x3ffffc00) == 0x38bfc000)
        return tuxport_has(TUXPORT_HWCAP_LRCPC) ? load_acquire_rcpc(c, insn) : undef(c, insn);
    if ((insn & 0x3f200c00) == 0x38200000 && !BIT(insn, 26)) { /* LD<op>, SWP */
        unsigned o3 = BIT(insn, 15), opc = FIELD(insn, 14, 12);
        if (!lse || (o3 && opc != 0)) /* the other forms with o3 = 1 and opc ≠ 0 are undefined */
            return undef(c, insn);
        struct native_desc d = {.addr_pos = 5, .n = 2};
        d.gpr[0] = (struct native_operand){.pos = 16, .load = 1};
        d.gpr[1] = (struct native_operand){.pos = 0, .store = 1};
        return native_atomic(c, insn, 1u << size, &d);
    }
    return undef(c, insn);
}
