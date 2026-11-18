/*
 * The Tier 0 interpreter (the engine map).
 *
 * Coverage: integer data processing (immediate, register, including FlagM's RMIF and SETF),
 * CRC32, branches, svc/brk, hints and barriers (including SB), CFINV/XAFLAG/AXFLAG, MSR DIT,
 * NZCV/TPIDR_EL0/TPIDRRO_EL0/FPCR/FPSR/DIT and the ID registers, integer load/store (immediate,
 * register offset, literal, pair). SIMD&FP is in simd.c, exclusive accesses and atomics in
 * atomic.c. Every other encoding is reported as an undefined instruction, following the
 * whitelist principle in the engine map: what is not implemented is not executed.
 *
 * Convention: an execution function returns TUXPORT_EXIT_NONE to continue; on error, pc stays
 * unchanged and no register is written back.
 */
#include "internal.h"

#define BIT(x, n) ((unsigned)(((x) >> (n)) & 1u))
#define FIELD(x, hi, lo) ((unsigned)(((x) >> (lo)) & ((1u << ((hi) - (lo) + 1)) - 1)))

#define NZCV(n, z, c, v)                                                                           \
    (((uint64_t)(n) << 31) | ((uint64_t)(z) << 30) | ((uint64_t)(c) << 29) | ((uint64_t)(v) << 28))

/* System register encoding: bits 20:5 = op0<1>:op0<0>:op1:CRn:CRm:op2 */
#define SYSREG(op0, op1, crn, crm, op2)                                                            \
    (((op0) << 14) | ((op1) << 11) | ((crn) << 7) | ((crm) << 3) | (op2))
#define SYSREG_NZCV SYSREG(3, 3, 4, 2, 0)
#define SYSREG_FPCR SYSREG(3, 3, 4, 4, 0)
#define SYSREG_FPSR SYSREG(3, 3, 4, 4, 1)
#define SYSREG_TPIDR_EL0 SYSREG(3, 3, 13, 0, 2)
#define SYSREG_TPIDRRO_EL0 SYSREG(3, 3, 13, 0, 3)
#define SYSREG_DCZID_EL0 SYSREG(3, 3, 0, 0, 7)
#define SYSREG_CTR_EL0 SYSREG(3, 3, 0, 0, 1)
#define SYSREG_CNTFRQ_EL0 SYSREG(3, 3, 14, 0, 0)
#define SYSREG_CNTVCT_EL0 SYSREG(3, 3, 14, 0, 2)
#define SYSREG_CNTVCTSS_EL0 SYSREG(3, 3, 14, 0, 6)
#define SYSREG_DIT SYSREG(3, 3, 4, 2, 5)

/*
 * The CTR_EL0 the guest reads: 64-byte cache lines, PIPT instruction cache. DIC = IDC = 0, so
 * after modifying code dc cvau and ic ivau must be executed: Tier 1 relies on ic ivau to know
 * which translations to invalidate.
 */
#define CTR_VALUE UINT64_C(0x8444c004)

/* Block size zeroed by DC ZVA: DCZID_EL0.BS = 4, i.e. 2^4 words, as on common arm64 cores */
#define DCZ_BLOCK 64

/* Cumulative exception bits in FPSR: QC, IDC, IXC, UFC, OFC, DZC, IOC */
#define FPSR_MASK UINT64_C(0x0800009f)

typedef struct tuxport_cpu cpu_t;

/* ---- General helpers ---- */

static inline int64_t sext(uint64_t v, unsigned bits)
{
    return (int64_t)(v << (64 - bits)) >> (64 - bits);
}

static inline uint64_t ones(unsigned n)
{
    return n >= 64 ? ~UINT64_C(0) : (UINT64_C(1) << n) - 1;
}

/* Truncates to the data width: results of 32-bit operations are zero-extended when written back */
static inline uint64_t narrow(uint64_t v, bool sf)
{
    return sf ? v : (uint32_t)v;
}

/* Register 31 is XZR in most encodings and SP in a few */
static inline uint64_t rx(const cpu_t *c, unsigned r)
{
    return r == 31 ? 0 : c->x[r];
}

static inline uint64_t rx_sp(const cpu_t *c, unsigned r)
{
    return r == 31 ? c->sp : c->x[r];
}

static inline void wx(cpu_t *c, unsigned r, uint64_t v)
{
    if (r != 31)
        c->x[r] = v;
}

static inline void wx_sp(cpu_t *c, unsigned r, uint64_t v)
{
    if (r == 31)
        c->sp = v;
    else
        c->x[r] = v;
}

static inline enum tuxport_exit next(cpu_t *c)
{
    c->pc += 4;
    return TUXPORT_EXIT_NONE;
}

static inline enum tuxport_exit undef(cpu_t *c, uint32_t insn)
{
    c->exit_info = insn;
    return TUXPORT_EXIT_UNDEF;
}

/* SP-based accesses require SP to be 16-byte aligned (Linux enables SCTLR_EL1.SA0) */
static inline bool sp_aligned(cpu_t *c, unsigned rn, uint32_t access)
{
    if (rn == 31 && (c->sp & 15))
        return raise_fault(c, c->sp, access, TUXPORT_FAULT_SP_ALIGN);
    return true;
}

static uint64_t add_with_carry(uint64_t x, uint64_t y, unsigned carry, bool sf, uint64_t *nzcv)
{
    if (sf) {
        unsigned __int128 u = (unsigned __int128)x + y + carry;
        uint64_t r = (uint64_t)u;
        __int128 s = (__int128)(int64_t)x + (__int128)(int64_t)y + carry;
        *nzcv = NZCV(r >> 63, r == 0, (unsigned)(u >> 64), s != (__int128)(int64_t)r);
        return r;
    }
    uint32_t a = (uint32_t)x, b = (uint32_t)y;
    uint64_t u = (uint64_t)a + b + carry;
    uint32_t r = (uint32_t)u;
    int64_t s = (int64_t)(int32_t)a + (int32_t)b + carry;
    *nzcv = NZCV(r >> 31, r == 0, (unsigned)(u >> 32), s != (int32_t)r);
    return r;
}

static inline uint64_t logic_flags(uint64_t r, bool sf)
{
    return NZCV(sf ? r >> 63 : (r >> 31) & 1, r == 0, 0, 0);
}

static bool cond_holds(uint64_t nzcv, unsigned cond)
{
    bool n = BIT(nzcv, 31), z = BIT(nzcv, 30), c = BIT(nzcv, 29), v = BIT(nzcv, 28);
    bool r;
    switch (cond >> 1) {
    case 0: r = z; break;
    case 1: r = c; break;
    case 2: r = n; break;
    case 3: r = v; break;
    case 4: r = c && !z; break;
    case 5: r = n == v; break;
    case 6: r = n == v && !z; break;
    default: r = true; break;
    }
    if ((cond & 1) && cond != 15)
        r = !r;
    return r;
}

/* Rotates right within size bits */
static inline uint64_t ror(uint64_t v, unsigned amount, unsigned size)
{
    if (amount == 0)
        return v;
    return ((v >> amount) | (v << (size - amount))) & ones(size);
}

/* shift_type: 0 LSL, 1 LSR, 2 ASR, 3 ROR; requires amount < the data width */
static uint64_t shift_reg(uint64_t v, unsigned type, unsigned amount, bool sf)
{
    unsigned size = sf ? 64 : 32;
    v = narrow(v, sf);
    if (amount == 0)
        return v;
    switch (type) {
    case 0: return narrow(v << amount, sf);
    case 1: return v >> amount;
    case 2:
        return sf ? (uint64_t)((int64_t)v >> amount) : (uint32_t)((int32_t)(uint32_t)v >> amount);
    default: return ror(v, amount, size);
    }
}

static uint64_t extend_reg(uint64_t v, unsigned option, unsigned shift, bool sf)
{
    uint64_t r;
    switch (option) {
    case 0: r = (uint8_t)v; break;
    case 1: r = (uint16_t)v; break;
    case 2: r = (uint32_t)v; break;
    case 4: r = (uint64_t)(int64_t)(int8_t)v; break;
    case 5: r = (uint64_t)(int64_t)(int16_t)v; break;
    case 6: r = (uint64_t)(int64_t)(int32_t)v; break;
    default: r = v; break;
    }
    return narrow(r << shift, sf);
}

/* The ARM ARM's DecodeBitMasks(); returns false for reserved encodings */
static bool decode_bit_masks(unsigned n, unsigned imms, unsigned immr, bool immediate,
                             unsigned size, uint64_t *wmask, uint64_t *tmask)
{
    unsigned combined = (n << 6) | (~imms & 0x3f);
    if (combined == 0)
        return false;
    unsigned len = 31 - (unsigned)__builtin_clz(combined);
    if (len < 1 || (1u << len) > size)
        return false;
    unsigned levels = (1u << len) - 1;
    if (immediate && (imms & levels) == levels)
        return false;
    unsigned s = imms & levels, r = immr & levels;
    unsigned esize = 1u << len;
    unsigned d = (s - r) & levels;
    uint64_t welem = ror(ones(s + 1), r, esize);
    uint64_t telem = ones(d + 1);
    uint64_t w = 0, t = 0;
    for (unsigned i = 0; i < size; i += esize) {
        w |= welem << i;
        t |= telem << i;
    }
    *wmask = narrow(w, size == 64);
    *tmask = narrow(t, size == 64);
    return true;
}

static uint64_t rbit64(uint64_t v)
{
    v = ((v >> 1) & UINT64_C(0x5555555555555555)) | ((v & UINT64_C(0x5555555555555555)) << 1);
    v = ((v >> 2) & UINT64_C(0x3333333333333333)) | ((v & UINT64_C(0x3333333333333333)) << 2);
    v = ((v >> 4) & UINT64_C(0x0f0f0f0f0f0f0f0f)) | ((v & UINT64_C(0x0f0f0f0f0f0f0f0f)) << 4);
    return __builtin_bswap64(v);
}

/* ---- Data processing: immediate ---- */

static enum tuxport_exit dp_imm(cpu_t *c, uint32_t insn)
{
    bool sf = BIT(insn, 31);
    unsigned rd = insn & 31, rn = FIELD(insn, 9, 5);

    switch (FIELD(insn, 25, 23)) {
    case 0:
    case 1: { /* ADR / ADRP */
        uint64_t imm = ((uint64_t)FIELD(insn, 23, 5) << 2) | FIELD(insn, 30, 29);
        int64_t off = sext(imm, 21);
        if (BIT(insn, 31))
            wx(c, rd, (c->pc & ~UINT64_C(0xfff)) + ((uint64_t)off << 12));
        else
            wx(c, rd, c->pc + (uint64_t)off);
        return next(c);
    }
    case 2: { /* ADD/ADDS/SUB/SUBS (immediate) */
        uint64_t imm = FIELD(insn, 21, 10);
        if (BIT(insn, 22))
            imm <<= 12;
        uint64_t n = rx_sp(c, rn), nzcv, r;
        if (!BIT(insn, 29)) {
            r = narrow(BIT(insn, 30) ? n - imm : n + imm, sf);
            wx_sp(c, rd, r);
        } else {
            r = BIT(insn, 30) ? add_with_carry(n, ~imm, 1, sf, &nzcv)
                              : add_with_carry(n, imm, 0, sf, &nzcv);
            c->nzcv = nzcv;
            wx(c, rd, r);
        }
        return next(c);
    }
    case 4: { /* AND/ORR/EOR/ANDS (immediate) */
        unsigned opc = FIELD(insn, 30, 29), n_bit = BIT(insn, 22);
        uint64_t wmask, tmask;
        if (!sf && n_bit)
            return undef(c, insn);
        if (!decode_bit_masks(n_bit, FIELD(insn, 15, 10), FIELD(insn, 21, 16), true, sf ? 64 : 32,
                              &wmask, &tmask))
            return undef(c, insn);
        uint64_t n = narrow(rx(c, rn), sf), r;
        switch (opc) {
        case 1: r = n | wmask; break;
        case 2: r = n ^ wmask; break;
        default: r = n & wmask; break;
        }
        if (opc == 3) {
            c->nzcv = logic_flags(r, sf);
            wx(c, rd, r);
        } else {
            wx_sp(c, rd, r);
        }
        return next(c);
    }
    case 5: { /* MOVN/MOVZ/MOVK */
        unsigned opc = FIELD(insn, 30, 29), hw = FIELD(insn, 22, 21);
        if (opc == 1 || (!sf && hw >= 2))
            return undef(c, insn);
        unsigned pos = hw * 16;
        uint64_t imm = (uint64_t)FIELD(insn, 20, 5) << pos, r;
        if (opc == 0)
            r = narrow(~imm, sf);
        else if (opc == 2)
            r = imm;
        else
            r = narrow((rx(c, rd) & ~(UINT64_C(0xffff) << pos)) | imm, sf);
        wx(c, rd, r);
        return next(c);
    }
    case 6: { /* SBFM/BFM/UBFM */
        unsigned opc = FIELD(insn, 30, 29), n_bit = BIT(insn, 22);
        unsigned immr = FIELD(insn, 21, 16), imms = FIELD(insn, 15, 10);
        unsigned size = sf ? 64 : 32;
        uint64_t wmask, tmask;
        if (opc == 3 || n_bit != (unsigned)sf || (!sf && ((immr | imms) & 0x20)))
            return undef(c, insn);
        if (!decode_bit_masks(n_bit, imms, immr, false, size, &wmask, &tmask))
            return undef(c, insn);
        uint64_t src = narrow(rx(c, rn), sf);
        uint64_t dst = opc == 1 ? narrow(rx(c, rd), sf) : 0;
        uint64_t bot = (dst & ~wmask) | (ror(src, immr, size) & wmask);
        uint64_t top = opc == 0 ? ((src >> imms) & 1 ? ones(size) : 0) : dst;
        wx(c, rd, narrow((top & ~tmask) | (bot & tmask), sf));
        return next(c);
    }
    case 7: { /* EXTR */
        unsigned lsb = FIELD(insn, 15, 10);
        if (FIELD(insn, 30, 29) || BIT(insn, 21) || BIT(insn, 22) != (unsigned)sf ||
            (!sf && lsb >= 32))
            return undef(c, insn);
        unsigned size = sf ? 64 : 32;
        uint64_t n = narrow(rx(c, rn), sf), m = narrow(rx(c, FIELD(insn, 20, 16)), sf);
        uint64_t r = lsb == 0 ? m : narrow((m >> lsb) | (n << (size - lsb)), sf);
        wx(c, rd, r);
        return next(c);
    }
    default: /* add/subtract with tag (MTE), min/max (CSSC): not in the whitelist */
        return undef(c, insn);
    }
}

/* ---- Data processing: register ---- */

/* The CRC32 family runs as native stubs */
static enum tuxport_exit crc32(cpu_t *c, uint32_t insn)
{
    struct native_desc d = {.addr_pos = NATIVE_NO_ADDR, .n = 3};
    d.gpr[0] = (struct native_operand){.pos = 0, .store = 1};
    d.gpr[1] = (struct native_operand){.pos = 5, .load = 1};
    d.gpr[2] = (struct native_operand){.pos = 16, .load = 1};
    if (!tuxport_native_exec(insn, &d, c, NULL))
        return undef(c, insn);
    return next(c);
}

/*
 * The two FlagM instructions that set flags from a register (encodings in ADC's group with bits
 * 15:10 nonzero):
 *   RMIF Xn, #shift, #mask: rotates Xn right by shift bits and writes the bits of the low 4 that
 *   mask selects into N, Z, C, V respectively;
 *   SETF8/SETF16 Wn: N is bit 7 (15), Z is whether the low 8 (16) bits are all 0, V is bit 8 (16)
 *   XOR bit 7 (15), and C is unchanged (turning the result of a signed 8- or 16-bit operation
 *   into flags).
 */
static enum tuxport_exit flags_from_reg(cpu_t *c, uint32_t insn)
{
    if (!tuxport_has(TUXPORT_HWCAP_FLAGM))
        return undef(c, insn);
    uint64_t n = rx(c, FIELD(insn, 9, 5));
    if ((insn & 0xffe07c10) == 0xba000400) { /* RMIF */
        unsigned shift = FIELD(insn, 20, 15), mask = insn & 15;
        uint64_t r = shift ? (n >> shift) | (n << (64 - shift)) : n;
        uint64_t sel = (uint64_t)mask << 28;
        c->nzcv = (c->nzcv & ~sel) | ((r & 15) << 28 & sel);
        return next(c);
    }
    if ((insn & 0xffffbc1f) == 0x3a00080d) { /* SETF8, SETF16 */
        unsigned top = BIT(insn, 14) ? 15 : 7;
        uint64_t low = n & ((UINT64_C(2) << top) - 1);
        unsigned neg = (n >> top) & 1, ovf = ((n >> (top + 1)) ^ (n >> top)) & 1;
        c->nzcv = (c->nzcv & NZCV(0, 0, 1, 0)) | NZCV(neg, low == 0, 0, ovf);
        return next(c);
    }
    return undef(c, insn);
}

static enum tuxport_exit dp_reg(cpu_t *c, uint32_t insn)
{
    bool sf = BIT(insn, 31);
    unsigned rd = insn & 31, rn = FIELD(insn, 9, 5), rm = FIELD(insn, 20, 16);
    unsigned op2 = FIELD(insn, 24, 21);
    uint64_t nzcv;

    if (!BIT(insn, 28)) {
        unsigned imm6 = FIELD(insn, 15, 10), shift = FIELD(insn, 23, 22);
        if (!(op2 & 8)) { /* logical (shifted register) */
            unsigned opc = FIELD(insn, 30, 29);
            if (!sf && imm6 >= 32)
                return undef(c, insn);
            uint64_t m = shift_reg(rx(c, rm), shift, imm6, sf);
            if (BIT(insn, 21))
                m = narrow(~m, sf);
            uint64_t n = narrow(rx(c, rn), sf), r;
            switch (opc) {
            case 1: r = n | m; break;
            case 2: r = n ^ m; break;
            default: r = n & m; break;
            }
            if (opc == 3)
                c->nzcv = logic_flags(r, sf);
            wx(c, rd, r);
            return next(c);
        }
        uint64_t m, n, r;
        if (!(op2 & 1)) { /* add/subtract (shifted register) */
            if (shift == 3 || (!sf && imm6 >= 32))
                return undef(c, insn);
            m = shift_reg(rx(c, rm), shift, imm6, sf);
            n = rx(c, rn);
        } else { /* add/subtract (extended register) */
            unsigned imm3 = FIELD(insn, 12, 10);
            if (shift != 0 || imm3 > 4)
                return undef(c, insn);
            m = extend_reg(rx(c, rm), FIELD(insn, 15, 13), imm3, sf);
            n = rx_sp(c, rn);
        }
        if (!BIT(insn, 29)) {
            r = narrow(BIT(insn, 30) ? n - m : n + m, sf);
            if (op2 & 1)
                wx_sp(c, rd, r);
            else
                wx(c, rd, r);
        } else {
            r = BIT(insn, 30) ? add_with_carry(n, ~m, 1, sf, &nzcv)
                              : add_with_carry(n, m, 0, sf, &nzcv);
            c->nzcv = nzcv;
            wx(c, rd, r);
        }
        return next(c);
    }

    switch (op2) {
    case 0: { /* ADC/ADCS/SBC/SBCS */
        if (FIELD(insn, 15, 10))
            return flags_from_reg(c, insn);
        unsigned carry = BIT(c->nzcv, 29);
        uint64_t n = rx(c, rn), m = rx(c, rm);
        uint64_t r = add_with_carry(n, BIT(insn, 30) ? ~m : m, carry, sf, &nzcv);
        if (BIT(insn, 29))
            c->nzcv = nzcv;
        wx(c, rd, r);
        return next(c);
    }
    case 2: { /* CCMN/CCMP (register, immediate) */
        if (!BIT(insn, 29) || BIT(insn, 10) || BIT(insn, 4))
            return undef(c, insn);
        if (cond_holds(c->nzcv, FIELD(insn, 15, 12))) {
            uint64_t n = rx(c, rn);
            uint64_t m = BIT(insn, 11) ? rm : rx(c, rm);
            if (BIT(insn, 30))
                add_with_carry(n, ~m, 1, sf, &nzcv);
            else
                add_with_carry(n, m, 0, sf, &nzcv);
            c->nzcv = nzcv;
        } else {
            c->nzcv = (uint64_t)FIELD(insn, 3, 0) << 28;
        }
        return next(c);
    }
    case 4: { /* CSEL/CSINC/CSINV/CSNEG */
        if (BIT(insn, 29) || BIT(insn, 11))
            return undef(c, insn);
        uint64_t r;
        if (cond_holds(c->nzcv, FIELD(insn, 15, 12))) {
            r = rx(c, rn);
        } else {
            r = rx(c, rm);
            switch ((BIT(insn, 30) << 1) | BIT(insn, 10)) {
            case 1: r = r + 1; break;
            case 2: r = ~r; break;
            case 3: r = -r; break;
            default: break;
            }
        }
        wx(c, rd, narrow(r, sf));
        return next(c);
    }
    case 6: {
        if (BIT(insn, 29))
            return undef(c, insn);
        unsigned opcode = FIELD(insn, 15, 10);
        unsigned size = sf ? 64 : 32;
        uint64_t n = narrow(rx(c, rn), sf), r;
        if (!BIT(insn, 30)) { /* 2 source operands */
            uint64_t m = narrow(rx(c, rm), sf);
            switch (opcode) {
            case 2: /* UDIV */
                r = m ? n / m : 0;
                break;
            case 3: /* SDIV: division by zero gives 0, minimum / -1 wraps around */
                if (m == 0)
                    r = 0;
                else if (sf)
                    r = (n == UINT64_C(1) << 63 && m == ~UINT64_C(0))
                            ? n
                            : (uint64_t)((int64_t)n / (int64_t)m);
                else
                    r = (n == UINT64_C(1) << 31 && m == UINT32_MAX)
                            ? n
                            : (uint32_t)((int32_t)(uint32_t)n / (int32_t)(uint32_t)m);
                break;
            case 8:
            case 9:
            case 10:
            case 11: /* LSLV/LSRV/ASRV/RORV */
                r = shift_reg(n, opcode - 8, (unsigned)(m & (size - 1)), sf);
                break;
            case 0x10:
            case 0x11:
            case 0x12:
            case 0x13:
            case 0x14:
            case 0x15:
            case 0x16:
            case 0x17: /* CRC32B/H/W/X, CRC32CB/H/W/X: the X forms require sf = 1 */
                if (!tuxport_has(TUXPORT_HWCAP_CRC32) || sf != ((opcode & 3) == 3))
                    return undef(c, insn);
                return crc32(c, insn);
            default: /* PACGA, MTE, CSSC: not in the whitelist */
                return undef(c, insn);
            }
        } else { /* 1 source operand */
            if (rm != 0)
                return undef(c, insn); /* the PAC family and others */
            switch (opcode) {
            case 0: /* RBIT */
                r = sf ? rbit64(n) : rbit64(n) >> 32;
                break;
            case 1: /* REV16 */
                r = narrow(((n & UINT64_C(0x00ff00ff00ff00ff)) << 8) |
                              ((n >> 8) & UINT64_C(0x00ff00ff00ff00ff)),
                          sf);
                break;
            case 2: /* sf=1: REV32; sf=0: REV */
                r = sf ? ((uint64_t)__builtin_bswap32((uint32_t)(n >> 32)) << 32) |
                             __builtin_bswap32((uint32_t)n)
                       : __builtin_bswap32((uint32_t)n);
                break;
            case 3: /* REV (64-bit) */
                if (!sf)
                    return undef(c, insn);
                r = __builtin_bswap64(n);
                break;
            case 4: /* CLZ */
                r = n == 0 ? size : (sf ? (unsigned)__builtin_clzll(n) : (unsigned)__builtin_clz((uint32_t)n));
                break;
            case 5: { /* CLS: number of bits after the top bit that equal it */
                uint64_t y = ((n >> 1) ^ n) & ones(size - 1);
                r = y == 0 ? size - 1 : (unsigned)__builtin_clzll(y) - (64 - size) - 1;
                break;
            }
            default:
                return undef(c, insn);
            }
        }
        wx(c, rd, r);
        return next(c);
    }
    default:
        break;
    }

    if (op2 & 8) { /* 3 source operands */
        unsigned op31 = FIELD(insn, 23, 21), o0 = BIT(insn, 15), ra = FIELD(insn, 14, 10);
        uint64_t n = rx(c, rn), m = rx(c, rm), a = rx(c, ra), r;
        if (FIELD(insn, 30, 29))
            return undef(c, insn);
        switch (op31) {
        case 0: /* MADD/MSUB */
            r = narrow(o0 ? a - n * m : a + n * m, sf);
            break;
        case 1: { /* SMADDL/SMSUBL */
            if (!sf)
                return undef(c, insn);
            uint64_t p = (uint64_t)((int64_t)(int32_t)n * (int64_t)(int32_t)m);
            r = o0 ? a - p : a + p;
            break;
        }
        case 2: /* SMULH */
            if (!sf || o0)
                return undef(c, insn);
            r = (uint64_t)(((__int128)(int64_t)n * (__int128)(int64_t)m) >> 64);
            break;
        case 5: { /* UMADDL/UMSUBL */
            if (!sf)
                return undef(c, insn);
            uint64_t p = (uint64_t)(uint32_t)n * (uint32_t)m;
            r = o0 ? a - p : a + p;
            break;
        }
        case 6: /* UMULH */
            if (!sf || o0)
                return undef(c, insn);
            r = (uint64_t)(((unsigned __int128)n * m) >> 64);
            break;
        default:
            return undef(c, insn);
        }
        wx(c, rd, r);
        return next(c);
    }
    return undef(c, insn);
}

/* ---- Branches, exception generation and system instructions ---- */

static enum tuxport_exit sysreg_move(cpu_t *c, uint32_t insn)
{
    bool read = BIT(insn, 21);
    unsigned rt = insn & 31;
    uint64_t v = rx(c, rt);
    switch (FIELD(insn, 20, 5)) {
    case SYSREG_NZCV:
        if (read)
            wx(c, rt, c->nzcv & UINT64_C(0xf0000000));
        else
            c->nzcv = v & UINT64_C(0xf0000000);
        return next(c);
    case SYSREG_FPSR:
        if (read)
            wx(c, rt, c->fpsr);
        else
            c->fpsr = v & FPSR_MASK;
        return next(c);
    case SYSREG_TPIDR_EL0:
        if (read)
            wx(c, rt, c->tpidr_el0);
        else
            c->tpidr_el0 = v;
        return next(c);
    case SYSREG_DIT: /* only bit 24 */
        if (!tuxport_has(TUXPORT_HWCAP_DIT))
            return undef(c, insn);
        if (read) {
            wx(c, rt, (uint64_t)c->dit << 24);
        } else {
            c->dit = (v >> 24) & 1;
            tuxport_sync_dit(c);
        }
        return next(c);
    case SYSREG_TPIDRRO_EL0: /* Linux user mode always reads 0, and EL0 cannot write it */
        if (!read)
            return undef(c, insn);
        wx(c, rt, 0);
        return next(c);
    case SYSREG_DCZID_EL0: /* DZP = 0: DC ZVA allowed */
        if (!read)
            return undef(c, insn);
        wx(c, rt, 4);
        return next(c);
    case SYSREG_CTR_EL0:
        if (!read)
            return undef(c, insn);
        wx(c, rt, CTR_VALUE);
        return next(c);
#ifdef __aarch64__
    /* Generic timer: Linux lets EL0 read the virtual counter and frequency (Go and V8 use them);
     * read the host's directly, the same counter the guest's clock_gettime uses */
    case SYSREG_CNTFRQ_EL0:
        if (!read)
            return undef(c, insn);
        __asm__ volatile("mrs %0, cntfrq_el0" : "=r"(v));
        wx(c, rt, v);
        return next(c);
    case SYSREG_CNTVCT_EL0:
        if (!read)
            return undef(c, insn);
        __asm__ volatile("isb\n\tmrs %0, cntvct_el0" : "=r"(v)::"memory");
        wx(c, rt, v);
        return next(c);
    case SYSREG_CNTVCTSS_EL0: /* ECV: the same counter, self-synchronized (no isb needed) */
        if (!read || !tuxport_has2(TUXPORT_HWCAP2_ECV))
            return undef(c, insn);
        __asm__ volatile("mrs %0, s3_3_c14_c0_6" : "=r"(v)::"memory");
        wx(c, rt, v);
        return next(c);
#endif
    default: {
        uint64_t id; /* the ID register space, consistent with HWCAP (the engine map) */
        if (read && tuxport_has(TUXPORT_HWCAP_CPUID) && tuxport_id_register(FIELD(insn, 20, 5), &id)) {
            wx(c, rt, id);
            return next(c);
        }
        return undef(c, insn);
    }
    }
}

/*
 * The three MSR (immediate) instructions that only change NZCV (op1 = 0, CRm = 0, op2 selects
 * which):
 *   CFINV (FlagM): inverts C;
 *   XAFLAG (FlagM2): converts the "external format" of a floating-point compare into Arm's NZCV;
 *   AXFLAG (FlagM2): the reverse: merges unordered (V) into Z, clears C, and zeroes N and V.
 */
static enum tuxport_exit flags_convert(cpu_t *c, uint32_t insn)
{
    unsigned z = BIT(c->nzcv, 30), cf = BIT(c->nzcv, 29), v = BIT(c->nzcv, 28);
    switch (FIELD(insn, 7, 5)) {
    case 0:
        if (!tuxport_has(TUXPORT_HWCAP_FLAGM))
            return undef(c, insn);
        c->nzcv ^= NZCV(0, 0, 1, 0);
        return next(c);
    case 1:
        if (!tuxport_has2(TUXPORT_HWCAP2_FLAGM2))
            return undef(c, insn);
        c->nzcv = NZCV(!cf && !z, z && cf, cf || z, !cf && z);
        return next(c);
    case 2:
        if (!tuxport_has2(TUXPORT_HWCAP2_FLAGM2))
            return undef(c, insn);
        c->nzcv = NZCV(0, z || v, cf && !v, 0);
        return next(c);
    default:
        return undef(c, insn);
    }
}

static enum tuxport_exit branch_sys(cpu_t *c, uint32_t insn)
{
    if ((insn & 0x7c000000) == 0x14000000) { /* B / BL */
        if (BIT(insn, 31))
            c->x[30] = c->pc + 4;
        c->pc += (uint64_t)(sext(FIELD(insn, 25, 0), 26) * 4);
        return TUXPORT_EXIT_NONE;
    }
    if ((insn & 0x7e000000) == 0x34000000) { /* CBZ / CBNZ */
        uint64_t t = narrow(rx(c, insn & 31), BIT(insn, 31));
        bool take = BIT(insn, 24) ? t != 0 : t == 0;
        c->pc += take ? (uint64_t)(sext(FIELD(insn, 23, 5), 19) * 4) : 4;
        return TUXPORT_EXIT_NONE;
    }
    if ((insn & 0x7e000000) == 0x36000000) { /* TBZ / TBNZ */
        unsigned bit = (BIT(insn, 31) << 5) | FIELD(insn, 23, 19);
        bool set = (rx(c, insn & 31) >> bit) & 1;
        bool take = BIT(insn, 24) ? set : !set;
        c->pc += take ? (uint64_t)(sext(FIELD(insn, 18, 5), 14) * 4) : 4;
        return TUXPORT_EXIT_NONE;
    }
    if ((insn & 0xff000010) == 0x54000000) { /* B.cond */
        bool take = cond_holds(c->nzcv, insn & 15);
        c->pc += take ? (uint64_t)(sext(FIELD(insn, 23, 5), 19) * 4) : 4;
        return TUXPORT_EXIT_NONE;
    }
    /* Decimal parsers read FPCR for each conversion. */
    if ((insn & 0xffdfffe0) == (0xd5000000u | SYSREG_FPCR << 5)) {
        unsigned rt = insn & 31;
        if (BIT(insn, 21))
            wx(c, rt, c->fpcr);
        else
            c->fpcr = rx(c, rt) & fpcr_write_mask();
        return next(c);
    }
    if ((insn & 0xffe0001f) == 0xd4000001) { /* SVC: exception return clears the exclusive mark */
        c->exit_info = FIELD(insn, 20, 5);
        c->excl_size = 0;
        c->pc += 4;
        return TUXPORT_EXIT_SVC;
    }
    if ((insn & 0xffe0001f) == 0xd4200000) { /* BRK */
        c->exit_info = FIELD(insn, 20, 5);
        return TUXPORT_EXIT_BRK;
    }
    /* HINT: NOP, YIELD and the PAC/BTI family are all treated as NOP (the engine map) */
    if ((insn & 0xfffff01f) == 0xd503201f)
        return next(c);
    if ((insn & 0xfffff01f) == 0xd503301f) { /* barriers */
        switch (FIELD(insn, 7, 5)) {
        case 2: /* CLREX */
            c->excl_size = 0;
            return next(c);
        case 6: /* ISB */
            return next(c);
        case 4: /* DSB */
        case 5: /* DMB */
            __atomic_thread_fence(__ATOMIC_SEQ_CST);
            return next(c);
        case 7: /* SB: a speculation barrier; under interpretation there is nothing to hold back */
            if (FIELD(insn, 11, 8) || !tuxport_has(TUXPORT_HWCAP_SB))
                return undef(c, insn);
            return next(c);
        default: /* DSB nXS and others are not in the whitelist */
            return undef(c, insn);
        }
    }
    /* MSR (immediate) to PSTATE.NZCV: CFINV, XAFLAG, AXFLAG */
    if ((insn & 0xffffff1f) == 0xd500401f)
        return flags_convert(c, insn);
    /* MSR DIT, #imm: takes the lowest bit of CRm, as the hardware does */
    if ((insn & 0xfffff0ff) == 0xd503405f) {
        if (!tuxport_has(TUXPORT_HWCAP_DIT))
            return undef(c, insn);
        c->dit = BIT(insn, 8);
        tuxport_sync_dit(c);
        return next(c);
    }
    if ((insn & 0xffd00000) == 0xd5100000) /* MRS / MSR (register) */
        return sysreg_move(c, insn);
    /* DC ZVA: zeroes the whole block containing the address; a block is always within one page */
    if ((insn & 0xffffffe0) == 0xd50b7420) {
        uint64_t va = untag(rx(c, insn & 31)) & ~(uint64_t)(DCZ_BLOCK - 1);
        void *p = tlb_lookup(c, va, TUXPORT_ACCESS_WRITE);
        if (!p) {
            raise_fault(c, va, TUXPORT_ACCESS_WRITE, TUXPORT_FAULT_PAGE);
            return TUXPORT_EXIT_FAULT;
        }
        if (UNLIKELY(watch_hit(c, va, DCZ_BLOCK, TUXPORT_WATCH_STORE))) {
            raise_fault(c, va, TUXPORT_ACCESS_WRITE, TUXPORT_FAULT_WATCH);
            return TUXPORT_EXIT_FAULT;
        }
        memset(p, 0, DCZ_BLOCK);
        return next(c);
    }
    /*
     * DC CVAC, DC CVAU, DC CIVAC, IC IVAU, and DC CVAP (DPB), DC CVADP (DPB2): guest memory is
     * host memory, so no cache maintenance is needed; the address is only checked for read access
     * (host memory is not persistent memory either). ic ivau means the guest modified the code
     * here, and is handed to the caller to invalidate translations.
     */
    uint32_t cmo = insn & 0xffffffe0;
    if ((cmo == 0xd50b7c20 && !tuxport_has(TUXPORT_HWCAP_DCPOP)) ||
        (cmo == 0xd50b7d20 && !tuxport_has2(TUXPORT_HWCAP2_DCPODP)))
        return undef(c, insn);
    if (cmo == 0xd50b7a20 || cmo == 0xd50b7b20 || cmo == 0xd50b7e20 || cmo == 0xd50b7520 ||
        cmo == 0xd50b7c20 || cmo == 0xd50b7d20) {
        uint64_t va = untag(rx(c, insn & 31));
        if (!tlb_lookup(c, va, TUXPORT_ACCESS_READ)) {
            raise_fault(c, va, TUXPORT_ACCESS_READ, TUXPORT_FAULT_PAGE);
            return TUXPORT_EXIT_FAULT;
        }
        if (cmo != 0xd50b7520)
            return next(c);
        c->fault_addr = va;
        c->pc += 4;
        return TUXPORT_EXIT_CODE;
    }

    uint32_t reg_branch = insn & 0xfffffc1f;
    if (reg_branch == 0xd61f0000 || reg_branch == 0xd63f0000 || reg_branch == 0xd65f0000) {
        uint64_t target = rx(c, FIELD(insn, 9, 5)); /* BR / BLR / RET */
        if (reg_branch == 0xd63f0000)
            c->x[30] = c->pc + 4;
        c->pc = target;
        return TUXPORT_EXIT_NONE;
    }
    return undef(c, insn);
}

/* ---- Integer load/store ---- */

enum ldst_kind { K_STORE, K_LOAD, K_LOAD_S64, K_LOAD_S32, K_PRFM, K_UNDEF };

static enum ldst_kind ldst_kind(unsigned size, unsigned opc)
{
    switch (opc) {
    case 0: return K_STORE;
    case 1: return K_LOAD;
    case 2: return size == 3 ? K_PRFM : K_LOAD_S64;
    default: return size >= 2 ? K_UNDEF : K_LOAD_S32;
    }
}

/* Constant sizes compile to unaligned scalar loads, with zero extension from the source width. */
static inline uint64_t load_scalar(const void *src, unsigned size)
{
    uint8_t b;
    uint16_t h;
    uint32_t w;
    uint64_t x;
    switch (size) {
    case 0: memcpy(&b, src, sizeof(b)); return b;
    case 1: memcpy(&h, src, sizeof(h)); return h;
    case 2: memcpy(&w, src, sizeof(w)); return w;
    default: memcpy(&x, src, sizeof(x)); return x;
    }
}

static inline void store_scalar(void *dst, unsigned size, uint64_t value)
{
    switch (size) {
    case 0: memcpy(dst, &value, 1); break;
    case 1: memcpy(dst, &value, 2); break;
    case 2: memcpy(dst, &value, 4); break;
    default: memcpy(dst, &value, 8); break;
    }
}

static inline uint64_t extend_load(enum ldst_kind kind, uint64_t value, unsigned size)
{
    if (kind == K_LOAD_S64)
        return (uint64_t)sext(value, 8u << size);
    if (kind == K_LOAD_S32)
        return (uint32_t)sext(value, 8u << size);
    return value;
}

/* Keeping this out of line lets scalar TLB hits run without a stack frame. */
static __attribute__((noinline)) bool ldst_slow(cpu_t *c, enum ldst_kind kind, unsigned size,
                                               uint64_t va, unsigned rt)
{
    unsigned n = 1u << size;
    if (kind == K_STORE) {
        uint64_t value = rx(c, rt);
        return mem_write(c, va, &value, n);
    }
    uint64_t value = 0;
    if (!mem_read(c, va, &value, n))
        return false;
    wx(c, rt, extend_load(kind, value, size));
    return true;
}

/* Performs one single-register access; on failure Rt is not modified */
static bool ldst_access(cpu_t *c, enum ldst_kind kind, unsigned size, uint64_t addr, unsigned rt)
{
    uint64_t va = untag(addr);
    unsigned access = kind == K_STORE ? TUXPORT_ACCESS_WRITE : TUXPORT_ACCESS_READ;
    void *host;
    if (UNLIKELY(c->nwatch || (va & PAGE_MASK) + (1u << size) > TUXPORT_PAGE_SIZE ||
                 !tlb_probe(c, va, access, &host)))
        return ldst_slow(c, kind, size, va, rt);

    if (kind == K_STORE)
        store_scalar(host, size, rx(c, rt));
    else
        wx(c, rt, extend_load(kind, load_scalar(host, size), size));
    return true;
}

static enum tuxport_exit ldst(cpu_t *c, uint32_t insn)
{
    unsigned size = FIELD(insn, 31, 30), opc = FIELD(insn, 23, 22);
    unsigned rt = insn & 31, rn = FIELD(insn, 9, 5);

    if (BIT(insn, 26))
        return tuxport_simd_ldst(c, insn);

    if ((insn & 0x3b000000) == 0x39000000) { /* unsigned offset */
        enum ldst_kind kind = ldst_kind(size, opc);
        if (kind == K_UNDEF)
            return undef(c, insn);
        if (kind == K_PRFM)
            return next(c); /* prefetch is only a hint: no memory access, no alignment check */
        if (!sp_aligned(c, rn, kind == K_STORE ? TUXPORT_ACCESS_WRITE : TUXPORT_ACCESS_READ))
            return TUXPORT_EXIT_FAULT;
        uint64_t addr = rx_sp(c, rn) + ((uint64_t)FIELD(insn, 21, 10) << size);
        if (!ldst_access(c, kind, size, addr, rt))
            return TUXPORT_EXIT_FAULT;
        return next(c);
    }
    /* 9-bit immediate: unscaled, post-index, unprivileged, pre-index */
    if ((insn & 0x3b200000) == 0x38000000) {
        enum ldst_kind kind = ldst_kind(size, opc);
        unsigned op = FIELD(insn, 11, 10);
        if (kind == K_UNDEF || (kind == K_PRFM && op != 0))
            return undef(c, insn);
        if (kind == K_PRFM)
            return next(c); /* prefetch is only a hint: no memory access, no alignment check */
        if (!sp_aligned(c, rn, kind == K_STORE ? TUXPORT_ACCESS_WRITE : TUXPORT_ACCESS_READ))
            return TUXPORT_EXIT_FAULT;
        uint64_t base = rx_sp(c, rn);
        uint64_t off = (uint64_t)sext(FIELD(insn, 20, 12), 9);
        uint64_t addr = op == 1 ? base : base + off;
        if (!ldst_access(c, kind, size, addr, rt))
            return TUXPORT_EXIT_FAULT;
        if (op == 1 || op == 3)
            wx_sp(c, rn, base + off);
        return next(c);
    }
    if ((insn & 0x3b200c00) == 0x38200800) { /* register offset */
        enum ldst_kind kind = ldst_kind(size, opc);
        unsigned option = FIELD(insn, 15, 13);
        if (kind == K_UNDEF || !(option & 2))
            return undef(c, insn);
        if (kind == K_PRFM)
            return next(c); /* prefetch is only a hint: no memory access, no alignment check */
        if (!sp_aligned(c, rn, kind == K_STORE ? TUXPORT_ACCESS_WRITE : TUXPORT_ACCESS_READ))
            return TUXPORT_EXIT_FAULT;
        uint64_t off = extend_reg(rx(c, FIELD(insn, 20, 16)), option, BIT(insn, 12) ? size : 0, true);
        if (!ldst_access(c, kind, size, rx_sp(c, rn) + off, rt))
            return TUXPORT_EXIT_FAULT;
        return next(c);
    }
    if ((insn & 0x3b000000) == 0x18000000) { /* LDR (literal) */
        uint64_t addr = c->pc + (uint64_t)(sext(FIELD(insn, 23, 5), 19) * 4);
        bool ok;
        switch (size) { /* here bits 31:30 are opc */
        case 0: ok = ldst_access(c, K_LOAD, 2, addr, rt); break;
        case 1: ok = ldst_access(c, K_LOAD, 3, addr, rt); break;
        case 2: ok = ldst_access(c, K_LOAD_S64, 2, addr, rt); break;
        default: ok = true; break; /* PRFM (literal) */
        }
        if (!ok)
            return TUXPORT_EXIT_FAULT;
        return next(c);
    }
    if ((insn & 0x3a000000) == 0x28000000) { /* LDP/STP/LDPSW, including LDNP/STNP */
        unsigned pair_opc = size, idx = FIELD(insn, 24, 23), load = BIT(insn, 22);
        unsigned rt2 = FIELD(insn, 14, 10);
        /* opc = 01 is only LDPSW: STGP (MTE) is not in the whitelist, and there is no
         * no-allocate form */
        if (pair_opc == 3 || (pair_opc == 1 && (!load || idx == 0)))
            return undef(c, insn);
        unsigned scale = pair_opc == 2 ? 3 : 2, n = 1u << scale;
        if (!sp_aligned(c, rn, load ? TUXPORT_ACCESS_READ : TUXPORT_ACCESS_WRITE))
            return TUXPORT_EXIT_FAULT;
        uint64_t base = rx_sp(c, rn);
        uint64_t off = (uint64_t)sext(FIELD(insn, 21, 15), 7) * n;
        uint64_t addr = idx == 1 ? base : base + off;
        uint8_t buf[16];
        if (load) {
            if (!mem_read(c, addr, buf, 2 * n))
                return TUXPORT_EXIT_FAULT;
            uint64_t v1 = 0, v2 = 0;
            memcpy(&v1, buf, n);
            memcpy(&v2, buf + n, n);
            if (pair_opc == 1) {
                v1 = (uint64_t)sext(v1, 32);
                v2 = (uint64_t)sext(v2, 32);
            }
            wx(c, rt, v1);
            wx(c, rt2, v2);
        } else {
            uint64_t v1 = rx(c, rt), v2 = rx(c, rt2);
            memcpy(buf, &v1, n);
            memcpy(buf + n, &v2, n);
            if (!mem_write(c, addr, buf, 2 * n))
                return TUXPORT_EXIT_FAULT;
        }
        if (idx == 1 || idx == 3)
            wx_sp(c, rn, base + off);
        return next(c);
    }
    return tuxport_atomic_exec(c, insn); /* exclusives, acquire/release, LSE atomics */
}

/* ---- SIMD&FP data processing: handed to native stubs after the whitelist decision ---- */

static enum tuxport_exit simd_dp(cpu_t *c, uint32_t insn)
{
    struct native_desc d;
    if (!tuxport_has(TUXPORT_HWCAP_FP | TUXPORT_HWCAP_ASIMD) || !tuxport_simd_dp_decode(insn, &d))
        return undef(c, insn);
    if (!tuxport_native_exec(insn, &d, c, NULL))
        return undef(c, insn);
    return next(c);
}

/* ---- Top-level decode ---- */

enum tuxport_exit tuxport_interp_exec(cpu_t *c, uint32_t insn)
{
    switch (FIELD(insn, 28, 25)) {
    case 0x8:
    case 0x9: return dp_imm(c, insn);
    case 0xa:
    case 0xb: return branch_sys(c, insn);
    case 0x5:
    case 0xd: return dp_reg(c, insn);
    case 0x4:
    case 0x6:
    case 0xc:
    case 0xe: return ldst(c, insn);
    case 0x7:
    case 0xf: return simd_dp(c, insn);
    default: /* reserved space (including UDF), SVE, unallocated */
        return undef(c, insn);
    }
}
