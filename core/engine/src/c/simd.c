/*
 * SIMD&FP (the engine map).
 *
 * Data processing instructions: only the whitelist decision is made here; the computation is left
 * to native instruction stubs. The whitelist is ARMv8.0's base FP and Advanced SIMD, plus
 * whichever of AES/PMULL/SHA1/SHA2/SHA3/SHA512/DotProd/I8MM/FP16/JSCVT/RDM/FHM/FCMA/BF16/FRINTTS
 * the guest feature set has; SM3/SM4, EBF16 and other extensions are always rejected. Groups and
 * encodings follow the ARM ARM's "Data Processing -- Scalar Floating-Point and Advanced SIMD"
 * encoding table.
 *
 * Memory access instructions (the B/H/S/D/Q forms of LDR/STR/LDP/STP, LD1–LD4/ST1–ST4) are
 * implemented in C: first confirm that every page to be accessed is accessible, then read or
 * write, so a failure has no side effects at all.
 */
#include "internal.h"

#define BIT(x, n) ((unsigned)(((x) >> (n)) & 1u))
#define FIELD(x, hi, lo) ((unsigned)(((x) >> (lo)) & ((1u << ((hi) - (lo) + 1)) - 1)))

/* ---- Native stub descriptions ---- */

static bool only_v(struct native_desc *d)
{
    *d = (struct native_desc){.vectors = NATIVE_FULL_FP, .addr_pos = NATIVE_NO_ADDR};
    return true;
}

/* With one general register operand: pos = 0 means Rd (written), pos = 5 means Rn (read) */
static bool with_gpr(struct native_desc *d, unsigned pos)
{
    *d = (struct native_desc){.vectors = NATIVE_FULL_FP, .addr_pos = NATIVE_NO_ADDR, .n = 1};
    d->gpr[0] = (struct native_operand){.pos = (uint8_t)pos, .load = pos != 0, .store = pos == 0};
    return true;
}

/* Half-precision data processing (FEAT_FP16): FPHP and ASIMDHP are always advertised together
 * (features.c) */
static bool fp16(void)
{
    return tuxport_has(TUXPORT_HWCAP_FPHP | TUXPORT_HWCAP_ASIMDHP);
}

/* RDM: SQRDMLAH, SQRDMLSH */
static bool rdm(void)
{
    return tuxport_has(TUXPORT_HWCAP_ASIMDRDM);
}

/* FHM: FMLAL, FMLSL (half-precision multiply, accumulating into single precision), advertised only
 * together with FP16 (features.c) */
static bool fhm(void)
{
    return tuxport_has(TUXPORT_HWCAP_ASIMDFHM);
}

/* FCMA: FCMLA, FCADD. The half-precision forms also need FP16; double precision has only the
 * 128-bit form */
static bool fcma(unsigned q, unsigned size)
{
    if (size == 0 || (size == 3 && !q) || (size == 1 && !fp16()))
        return false;
    return tuxport_has(TUXPORT_HWCAP_FCMA);
}

/* BF16: BFDOT, BFMMLA, BFMLALB/T, BFCVT, BFCVTN */
static bool bf16(void)
{
    return tuxport_has2(TUXPORT_HWCAP2_BF16);
}

/* FRINTTS: FRINT32Z/X, FRINT64Z/X, rounding to an integral value in the 32- or 64-bit integer range
 * (JavaScript's and C's conversions) */
static bool frint(void)
{
    return tuxport_has2(TUXPORT_HWCAP2_FRINT);
}

/* Whether the precision selected by an FP instruction's type/size field can be used: 3 is half
 * precision, 2 exists only for FMOV's upper-half form */
static bool fp_type_ok(unsigned type)
{
    return type <= 1 || (type == 3 && fp16());
}

/* ---- Advanced SIMD: vector ---- */

static bool vec_three_same(uint32_t insn, struct native_desc *d)
{
    unsigned q = BIT(insn, 30), u = BIT(insn, 29), size = FIELD(insn, 23, 22);
    unsigned opcode = FIELD(insn, 15, 11);
    if (opcode >= 0x18) {
        /* FP: size<1> selects the instruction, size<0> is the precision; double precision
         * requires Q = 1 */
        static const uint8_t allowed[2][2] = {{0xdf, 0xc7}, {0xfd, 0x75}};
        if ((size & 1) && !q)
            return false;
        /* FMLAL/FMLSL (U = 0, 11101), FMLAL2/FMLSL2 (U = 1, 11001): FHM, sz must be 0 */
        if (opcode == (u ? 0x19 : 0x1d) && !(size & 1) && fhm())
            return true;
        return (allowed[u][size >> 1] >> (opcode - 0x18)) & 1;
    }
    bool q64 = size != 3 || q; /* 64-bit elements only with 128-bit vectors */
    switch (opcode) {
    case 0x00: /* SHADD/UHADD */
    case 0x02: /* SRHADD/URHADD */
    case 0x04: /* SHSUB/UHSUB */
    case 0x0c: /* SMAX/UMAX */
    case 0x0d: /* SMIN/UMIN */
    case 0x0e: /* SABD/UABD */
    case 0x0f: /* SABA/UABA */
    case 0x12: /* MLA/MLS */
    case 0x14: /* SMAXP/UMAXP */
    case 0x15: /* SMINP/UMINP */
        return size != 3;
    case 0x01: /* SQADD/UQADD */
    case 0x05: /* SQSUB/UQSUB */
    case 0x06: /* CMGT/CMHI */
    case 0x07: /* CMGE/CMHS */
    case 0x08: /* SSHL/USHL */
    case 0x09: /* SQSHL/UQSHL */
    case 0x0a: /* SRSHL/URSHL */
    case 0x0b: /* SQRSHL/UQRSHL */
    case 0x11: /* CMTST/CMEQ */
        return q64;
    case 0x10: /* ADD/SUB */
        d->vectors = NATIVE_VN_VM;
        return q64;
    case 0x03: /* AND/BIC/ORR/ORN, EOR/BSL/BIT/BIF: size selects the instruction */
        d->vectors = u && size != 0 ? NATIVE_VD_VN_VM : NATIVE_VN_VM;
        return true;
    case 0x13: /* MUL / PMUL */
        return u ? size == 0 : size != 3;
    case 0x16: /* SQDMULH/SQRDMULH */
        return size == 1 || size == 2;
    case 0x17: /* ADDP */
        return !u && q64;
    default:
        return false;
    }
}

/* Three registers of the same type (FP16): a (bit 23) together with U selects the instruction; the
 * table is the same as the single/double-precision one */
static bool vec_three_same_fp16(uint32_t insn)
{
    static const uint8_t allowed[2][2] = {{0xdf, 0xc7}, {0xfd, 0x75}};
    unsigned u = BIT(insn, 29), a = BIT(insn, 23), opcode = FIELD(insn, 13, 11);
    return fp16() && ((allowed[u][a] >> opcode) & 1);
}

/* Two-register miscellaneous (FP16): each bit is one instruction among opcodes 0x0c–0x1f */
static bool vec_two_reg_misc_fp16(uint32_t insn)
{
    static const uint32_t allowed[2][2] = {
        /* FRINTN/M, FCVTNS/MS/AS, SCVTF; FCM*#0, FABS, FRINTP/Z, FCVTPS/ZS, FRECPE */
        {0x3f000000, 0x2f00f000},
        /* FRINTA/X, FCVTNU/MU/AU, UCVTF; FCMGE/LE#0, FNEG, FRINTI, FCVTPU/ZU, FRSQRTE, FSQRT */
        {0x3f000000, 0xae00b000},
    };
    unsigned u = BIT(insn, 29), a = BIT(insn, 23), opcode = FIELD(insn, 16, 12);
    return fp16() && ((allowed[u][a] >> opcode) & 1);
}

static bool vec_three_different(uint32_t insn)
{
    unsigned u = BIT(insn, 29), size = FIELD(insn, 23, 22), opcode = FIELD(insn, 15, 12);
    switch (opcode) {
    case 0x9: /* SQDMLAL */
    case 0xb: /* SQDMLSL */
    case 0xd: /* SQDMULL */
        return !u && (size == 1 || size == 2);
    case 0xe: /* PMULL: the 1Q form belongs to the PMULL extension */
        return !u && (size == 0 || (size == 3 && tuxport_has(TUXPORT_HWCAP_PMULL)));
    case 0xf:
        return false;
    default: /* ADDL/ADDW/SUBL/SUBW/ADDHN/ABAL/SUBHN/ABDL/MLAL/MLSL/MULL */
        return size != 3;
    }
}

static bool vec_two_reg_misc(uint32_t insn)
{
    unsigned q = BIT(insn, 30), u = BIT(insn, 29), size = FIELD(insn, 23, 22);
    unsigned opcode = FIELD(insn, 16, 12), hi = size >> 1;
    bool q64 = size != 3 || q;
    bool fpq = !((size & 1) && !q); /* double-precision vectors require Q = 1 */
    if (!u) {
        switch (opcode) {
        case 0x00: return size != 3;                /* REV64 */
        case 0x01: return size == 0;                /* REV16 */
        case 0x02: return size != 3;                /* SADDLP */
        case 0x03: return q64;                      /* SUQADD */
        case 0x04: return size != 3;                /* CLS */
        case 0x05: return size == 0;                /* CNT */
        case 0x06: return size != 3;                /* SADALP */
        case 0x07: return q64;                      /* SQABS */
        case 0x08:                                  /* CMGT #0 */
        case 0x09:                                  /* CMEQ #0 */
        case 0x0a:                                  /* CMLT #0 */
        case 0x0b: return q64;                      /* ABS */
        case 0x0c:                                  /* FCMGT #0 */
        case 0x0d:                                  /* FCMEQ #0 */
        case 0x0e:                                  /* FCMLT #0 */
        case 0x0f: return hi && fpq;                /* FABS */
        case 0x12: return size != 3;                /* XTN */
        case 0x14: return size != 3;                /* SQXTN */
        case 0x16: return !hi || (size == 2 && bf16()); /* FCVTN; BFCVTN (BF16) */
        case 0x17: return !hi;                      /* FCVTL */
        case 0x18:                                  /* FRINTN / FRINTP */
        case 0x19:                                  /* FRINTM / FRINTZ */
        case 0x1a:                                  /* FCVTNS / FCVTPS */
        case 0x1b:                                  /* FCVTMS / FCVTZS */
        case 0x1d: return fpq;                      /* SCVTF / FRECPE */
        case 0x1c: return hi ? size == 2 : fpq;     /* FCVTAS / URECPE */
        case 0x1e:                                  /* FRINT32Z (FRINTTS) */
        case 0x1f: return !hi && fpq && frint();    /* FRINT64Z */
        default: return false;
        }
    }
    switch (opcode) {
    case 0x00: return size <= 1;                    /* REV32 */
    case 0x02: return size != 3;                    /* UADDLP */
    case 0x03: return q64;                          /* USQADD */
    case 0x04: return size != 3;                    /* CLZ */
    case 0x05: return size <= 1;                    /* NOT / RBIT */
    case 0x06: return size != 3;                    /* UADALP */
    case 0x07: return q64;                          /* SQNEG */
    case 0x08:                                      /* CMGE #0 */
    case 0x09:                                      /* CMLE #0 */
    case 0x0b: return q64;                          /* NEG */
    case 0x0c:                                      /* FCMGE #0 */
    case 0x0d:                                      /* FCMLE #0 */
    case 0x0f: return hi && fpq;                    /* FNEG */
    case 0x12:                                      /* SQXTUN */
    case 0x13:                                      /* SHLL */
    case 0x14: return size != 3;                    /* UQXTN */
    case 0x16: return size == 1;                    /* FCVTXN */
    case 0x18: return !hi && fpq;                   /* FRINTA */
    case 0x19:                                      /* FRINTX / FRINTI */
    case 0x1a:                                      /* FCVTNU / FCVTPU */
    case 0x1b:                                      /* FCVTMU / FCVTZU */
    case 0x1d: return fpq;                          /* UCVTF / FRSQRTE */
    case 0x1c: return hi ? size == 2 : fpq;         /* FCVTAU / URSQRTE */
    case 0x1e: return !hi && fpq && frint();        /* FRINT32X (FRINTTS) */
    case 0x1f: return fpq && (hi || frint());       /* FSQRT; FRINT64X (FRINTTS) */
    default: return false;
    }
}

static bool vec_across_lanes(uint32_t insn)
{
    unsigned q = BIT(insn, 30), u = BIT(insn, 29), size = FIELD(insn, 23, 22);
    unsigned opcode = FIELD(insn, 16, 12);
    if (opcode == 0x0c || opcode == 0x0f) { /* FMAXNMV/FMINNMV, FMAXV/FMINV */
        if (!u) /* half precision: 4H, 8H */
            return !(size & 1) && fp16();
        return q && !(size & 1); /* single precision only has 4S */
    }
    if (size == 3 || (size == 2 && !q))
        return false;
    switch (opcode) {
    case 0x03: /* SADDLV/UADDLV */
    case 0x0a: /* SMAXV/UMAXV */
    case 0x1a: /* SMINV/UMINV */
        return true;
    case 0x1b: /* ADDV */
        return !u;
    default:
        return false;
    }
}

static bool vec_copy(uint32_t insn, struct native_desc *d)
{
    unsigned q = BIT(insn, 30), op = BIT(insn, 29), imm5 = FIELD(insn, 20, 16);
    unsigned imm4 = FIELD(insn, 14, 11);
    if (!(imm5 & 0xf))
        return false;
    unsigned size = (unsigned)__builtin_ctz(imm5);
    if (op) /* INS (element) */
        return q && only_v(d);
    switch (imm4) {
    case 0x0: /* DUP (element) */
        return !(size == 3 && !q) && only_v(d);
    case 0x1: /* DUP (general) */
        return !(size == 3 && !q) && with_gpr(d, 5);
    case 0x3: /* INS (general) */
        return q && with_gpr(d, 5);
    case 0x5: /* SMOV */
        return (q ? size <= 2 : size <= 1) && with_gpr(d, 0);
    case 0x7: /* UMOV */
        return (q ? size == 3 : size <= 2) && with_gpr(d, 0);
    default:
        return false;
    }
}

static bool vec_modified_imm(uint32_t insn)
{
    unsigned q = BIT(insn, 30), op = BIT(insn, 29), cmode = FIELD(insn, 15, 12);
    if (BIT(insn, 11)) /* o2 = 1: half-precision FMOV (FP16) */
        return !op && cmode == 0xf && fp16();
    return !(op && cmode == 0xf && !q); /* double-precision FMOV only has 2D */
}

static bool vec_shift_imm(uint32_t insn)
{
    unsigned q = BIT(insn, 30), u = BIT(insn, 29), immh = FIELD(insn, 22, 19);
    unsigned opcode = FIELD(insn, 15, 11);
    bool d64 = immh & 8;
    switch (opcode) {
    case 0x00: /* SSHR/USHR */
    case 0x02: /* SSRA/USRA */
    case 0x04: /* SRSHR/URSHR */
    case 0x06: /* SRSRA/URSRA */
    case 0x0a: /* SHL/SLI */
    case 0x0e: /* SQSHL/UQSHL */
        return !(d64 && !q);
    case 0x08: /* SRI */
    case 0x0c: /* SQSHLU */
        return u && !(d64 && !q);
    case 0x10: /* SHRN/SQSHRUN */
    case 0x11: /* RSHRN/SQRSHRUN */
    case 0x12: /* SQSHRN/UQSHRN */
    case 0x13: /* SQRSHRN/UQRSHRN */
    case 0x14: /* SSHLL/USHLL */
        return !d64;
    case 0x1c: /* SCVTF/UCVTF (fixed-point) */
    case 0x1f: /* FCVTZS/FCVTZU (fixed-point): immh = 001x is half precision */
        if ((immh & 0xe) == 0x2)
            return fp16();
        return (immh & 0xc) && !(d64 && !q);
    default:
        return false;
    }
}

/* Three-register extensions: RDM, DotProd, I8MM, BF16, FCMA */
static bool vec_three_extra(uint32_t insn)
{
    unsigned q = BIT(insn, 30), u = BIT(insn, 29), size = FIELD(insn, 23, 22);
    unsigned opcode = FIELD(insn, 14, 11);
    if (opcode >= 0x8 && opcode != 0xd && opcode != 0xf) /* FCMLA (10xx), FCADD (11x0) */
        return u && fcma(q, size);
    switch (opcode) {
    case 0x0: /* SQRDMLAH (vector) */
    case 0x1: /* SQRDMLSH (vector) */
        return u && (size == 1 || size == 2) && rdm();
    case 0x2: /* SDOT/UDOT (vector) */
        return size == 2 && tuxport_has(TUXPORT_HWCAP_ASIMDDP);
    case 0x3: /* USDOT (vector) */
        return size == 2 && !u && tuxport_has2(TUXPORT_HWCAP2_I8MM);
    case 0x4: /* SMMLA/UMMLA: only the 128-bit form */
        return size == 2 && q && tuxport_has2(TUXPORT_HWCAP2_I8MM);
    case 0x5: /* USMMLA */
        return size == 2 && q && !u && tuxport_has2(TUXPORT_HWCAP2_I8MM);
    case 0xd: /* BFMMLA: only the 128-bit form */
        return u && size == 1 && q && bf16();
    case 0xf: /* BFDOT (size 01), BFMLALB/BFMLALT (size 11, Q selects B/T) */
        return u && (size == 1 || size == 3) && bf16();
    default:
        return false;
    }
}

static bool vec_indexed(uint32_t insn)
{
    unsigned q = BIT(insn, 30), u = BIT(insn, 29), size = FIELD(insn, 23, 22);
    unsigned h = BIT(insn, 11), l = BIT(insn, 21), opcode = FIELD(insn, 15, 12);
    bool hs = size == 1 || size == 2;
    if (u && (opcode & 0x9) == 0x1) {
        /* FCMLA (FCMA, opcode<2:1> is the rotation): half precision selects a pair of elements
         * by H:L, with only the first two pairs for 64 bits; single precision selects by H, and
         * has only the 128-bit form */
        if (size == 1 && h && !q)
            return false;
        return (size == 1 || (size == 2 && !l && q)) && fcma(q, size);
    }
    switch (opcode) {
    case 0x1: /* FMLA */
    case 0x5: /* FMLS */
    case 0x9: /* FMUL / FMULX */
        if (size == 0) /* half precision: H:L:M selects the element, Rm has only 4 bits */
            return fp16();
        return (size & 2) && !((size & 1) && (l || !q));
    case 0x0: /* MLA; with U = 0 it is FMLAL (FHM) */
    case 0x4: /* MLS; with U = 0 it is FMLSL (FHM) */
        return u ? hs : size == 2 && fhm();
    case 0x2: /* SMLAL/UMLAL */
    case 0x6: /* SMLSL/UMLSL */
    case 0xa: /* SMULL/UMULL */
        return hs;
    case 0x3: /* SQDMLAL */
    case 0x7: /* SQDMLSL */
    case 0xb: /* SQDMULL */
        return !u && hs;
    case 0x8: /* MUL; with U = 1 it is FMLAL2 (FHM) */
    case 0xc: /* SQDMULH; with U = 1 it is FMLSL2 (FHM) */
        return u ? size == 2 && fhm() : hs;
    case 0xd: /* SQRDMULH; with U = 1 it is SQRDMLAH (RDM) */
        return hs && (!u || rdm());
    case 0xe: /* SDOT/UDOT (by element) */
        return size == 2 && tuxport_has(TUXPORT_HWCAP_ASIMDDP);
    case 0xf:
        if (u) /* SQRDMLSH (RDM) */
            return hs && rdm();
        /* SUDOT (size 00), USDOT (size 10): I8MM; BFDOT (size 01), BFMLALB/T (size 11): BF16 */
        return (size & 1) ? bf16() : tuxport_has2(TUXPORT_HWCAP2_I8MM);
    default:
        return false;
    }
}

/* ---- Advanced SIMD: scalar ---- */

static bool scalar_three_same(uint32_t insn)
{
    unsigned u = BIT(insn, 29), size = FIELD(insn, 23, 22), opcode = FIELD(insn, 15, 11);
    if (opcode >= 0x18) {
        static const uint8_t allowed[2][2] = {{0x98, 0x80}, {0x30, 0x34}};
        return (allowed[u][size >> 1] >> (opcode - 0x18)) & 1;
    }
    switch (opcode) {
    case 0x01: /* SQADD/UQADD */
    case 0x05: /* SQSUB/UQSUB */
    case 0x09: /* SQSHL/UQSHL */
    case 0x0b: /* SQRSHL/UQRSHL */
        return true;
    case 0x06: /* CMGT/CMHI */
    case 0x07: /* CMGE/CMHS */
    case 0x08: /* SSHL/USHL */
    case 0x0a: /* SRSHL/URSHL */
    case 0x10: /* ADD/SUB */
    case 0x11: /* CMTST/CMEQ */
        return size == 3;
    case 0x16: /* SQDMULH/SQRDMULH */
        return size == 1 || size == 2;
    default:
        return false;
    }
}

/* Scalar three registers of the same type (FP16): the table is the same as the
 * single/double-precision one */
static bool scalar_three_same_fp16(uint32_t insn)
{
    static const uint8_t allowed[2][2] = {{0x98, 0x80}, {0x30, 0x34}};
    unsigned u = BIT(insn, 29), a = BIT(insn, 23), opcode = FIELD(insn, 13, 11);
    return fp16() && ((allowed[u][a] >> opcode) & 1);
}

/* Scalar two-register miscellaneous (FP16); the bits mean the same as in vec_two_reg_misc_fp16 */
static bool scalar_two_reg_misc_fp16(uint32_t insn)
{
    static const uint32_t allowed[2][2] = {
        {0x3c000000, 0xac007000}, /* FCVTNS/MS/AS, SCVTF; FCM*#0, FCVTPS/ZS, FRECPE, FRECPX */
        {0x3c000000, 0x2c003000}, /* FCVTNU/MU/AU, UCVTF; FCMGE/LE#0, FCVTPU/ZU, FRSQRTE */
    };
    unsigned u = BIT(insn, 29), a = BIT(insn, 23), opcode = FIELD(insn, 16, 12);
    return fp16() && ((allowed[u][a] >> opcode) & 1);
}

static bool scalar_two_reg_misc(uint32_t insn)
{
    unsigned u = BIT(insn, 29), size = FIELD(insn, 23, 22), opcode = FIELD(insn, 16, 12);
    unsigned hi = size >> 1;
    switch (opcode) {
    case 0x03: /* SUQADD/USQADD */
    case 0x07: /* SQABS/SQNEG */
        return true;
    case 0x08: /* CMGT/CMGE #0 */
    case 0x09: /* CMEQ/CMLE #0 */
    case 0x0b: /* ABS/NEG */
        return size == 3;
    case 0x0a: /* CMLT #0 */
        return !u && size == 3;
    case 0x0c: /* FCMGT/FCMGE #0 */
    case 0x0d: /* FCMEQ/FCMLE #0 */
        return hi;
    case 0x0e: /* FCMLT #0 */
        return !u && hi;
    case 0x12: /* SQXTUN */
        return u && size != 3;
    case 0x14: /* SQXTN/UQXTN */
        return size != 3;
    case 0x16: /* FCVTXN */
        return u && size == 1;
    case 0x1a: /* FCVTNS/FCVTNU, FCVTPS/FCVTPU */
    case 0x1b: /* FCVTMS/FCVTMU, FCVTZS/FCVTZU */
    case 0x1d: /* SCVTF/UCVTF, FRECPE/FRSQRTE */
        return true;
    case 0x1c: /* FCVTAS/FCVTAU */
        return !hi;
    case 0x1f: /* FRECPX */
        return !u && hi;
    default:
        return false;
    }
}

static bool scalar_pairwise(uint32_t insn)
{
    unsigned u = BIT(insn, 29), size = FIELD(insn, 23, 22), opcode = FIELD(insn, 16, 12);
    if (!u) { /* ADDP; the U = 0 FP form is half precision (FP16), size's low bit must be 0 */
        if (opcode == 0x1b)
            return size == 3;
        if (size & 1)
            return false;
        bool ok = opcode == 0x0c || opcode == 0x0f || (opcode == 0x0d && size == 0);
        return ok && fp16();
    }
    switch (opcode) {
    case 0x0c: /* FMAXNMP/FMINNMP */
    case 0x0f: /* FMAXP/FMINP */
        return true;
    case 0x0d: /* FADDP */
        return !(size >> 1);
    default:
        return false;
    }
}

static bool scalar_shift_imm(uint32_t insn)
{
    unsigned u = BIT(insn, 29), immh = FIELD(insn, 22, 19), opcode = FIELD(insn, 15, 11);
    bool d64 = immh & 8;
    if (!immh)
        return false;
    switch (opcode) {
    case 0x00: /* SSHR/USHR */
    case 0x02: /* SSRA/USRA */
    case 0x04: /* SRSHR/URSHR */
    case 0x06: /* SRSRA/URSRA */
    case 0x0a: /* SHL/SLI */
        return d64;
    case 0x08: /* SRI */
        return u && d64;
    case 0x0c: /* SQSHLU */
        return u;
    case 0x0e: /* SQSHL/UQSHL */
        return true;
    case 0x10: /* SQSHRUN */
    case 0x11: /* SQRSHRUN */
        return u && !d64;
    case 0x12: /* SQSHRN/UQSHRN */
    case 0x13: /* SQRSHRN/UQRSHRN */
        return !d64;
    case 0x1c: /* SCVTF/UCVTF (fixed-point) */
    case 0x1f: /* FCVTZS/FCVTZU (fixed-point): immh = 001x is half precision */
        if ((immh & 0xe) == 0x2)
            return fp16();
        return immh & 0xc;
    default:
        return false;
    }
}

static bool scalar_indexed(uint32_t insn)
{
    unsigned u = BIT(insn, 29), size = FIELD(insn, 23, 22), l = BIT(insn, 21);
    unsigned opcode = FIELD(insn, 15, 12);
    switch (opcode) {
    case 0x1: /* FMLA */
    case 0x5: /* FMLS */
    case 0x9: /* FMUL / FMULX */
        if (u && opcode != 0x9)
            return false;
        if (size == 0) /* half precision */
            return fp16();
        return (size & 2) && !((size & 1) && l);
    case 0x3: /* SQDMLAL */
    case 0x7: /* SQDMLSL */
    case 0xb: /* SQDMULL */
    case 0xc: /* SQDMULH */
        return !u && (size == 1 || size == 2);
    case 0xd: /* SQRDMULH; with U = 1 it is SQRDMLAH (RDM) */
        return (size == 1 || size == 2) && (!u || rdm());
    case 0xf: /* SQRDMLSH (RDM) */
        return u && (size == 1 || size == 2) && rdm();
    default:
        return false;
    }
}

/* Scalar three-register extensions: only RDM's SQRDMLAH, SQRDMLSH */
static bool scalar_three_extra(uint32_t insn)
{
    unsigned u = BIT(insn, 29), size = FIELD(insn, 23, 22), opcode = FIELD(insn, 14, 11);
    return u && opcode <= 1 && (size == 1 || size == 2) && rdm();
}

static bool crypto_sha3(uint32_t insn)
{
    unsigned opcode = FIELD(insn, 14, 12);
    if (opcode <= 3)
        return tuxport_has(TUXPORT_HWCAP_SHA1); /* SHA1C/SHA1P/SHA1M/SHA1SU0 */
    return opcode <= 6 && tuxport_has(TUXPORT_HWCAP_SHA2); /* SHA256H/SHA256H2/SHA256SU1 */
}

static bool crypto_sha2(uint32_t insn)
{
    unsigned opcode = FIELD(insn, 16, 12);
    if (opcode <= 1)
        return tuxport_has(TUXPORT_HWCAP_SHA1); /* SHA1H/SHA1SU1 */
    return opcode == 2 && tuxport_has(TUXPORT_HWCAP_SHA2); /* SHA256SU0 */
}

/* ---- Scalar floating point ---- */

static bool fp_int_conversion(uint32_t insn, struct native_desc *d)
{
    unsigned sf = BIT(insn, 31), type = FIELD(insn, 23, 22), rmode = FIELD(insn, 20, 19);
    unsigned opcode = FIELD(insn, 18, 16);
    if (!sf && type == 1 && rmode == 3 && opcode == 6) /* FJCVTZS (JSCVT): also writes NZCV */
        return tuxport_has(TUXPORT_HWCAP_JSCVT) && with_gpr(d, 0);
    if (opcode == 6 || opcode == 7) { /* FMOV (general register ↔ floating point) */
        bool ok = (rmode == 0 && ((!sf && type == 0) || (sf && type == 1))) ||
                  (rmode == 1 && sf && type == 2) ||
                  (rmode == 0 && type == 3 && fp16()); /* Wn/Xn ↔ Hn */
        return ok && with_gpr(d, opcode == 6 ? 0 : 5);
    }
    if (!fp_type_ok(type))
        return false;
    if (opcode <= 1) /* FCVTNS/FCVTPS/FCVTMS/FCVTZS and the unsigned forms */
        return with_gpr(d, 0);
    if (rmode != 0)
        return false;
    if (opcode <= 3) /* SCVTF/UCVTF */
        return with_gpr(d, 5);
    return with_gpr(d, 0); /* FCVTAS/FCVTAU */
}

static bool fp_one_source(uint32_t insn)
{
    unsigned type = FIELD(insn, 23, 22), opcode = FIELD(insn, 20, 15);
    if (type == 3 && !fp16()) /* without FP16, a half-precision source can only do FCVT */
        return opcode == 0x04 || opcode == 0x05;
    if (type == 2)
        return false;
    if (opcode <= 0x03) /* FMOV/FABS/FNEG/FSQRT */
        return true;
    if ((opcode & 0x3c) == 0x04) { /* FCVT: the target precision must differ from the source */
        unsigned to = opcode & 3;
        if (to == 2) /* BFCVT (BF16): only single precision to BFloat16 */
            return type == 1 && bf16();
        return to != type;
    }
    if (opcode >= 0x10 && opcode <= 0x13) /* FRINT32Z/X, FRINT64Z/X (FRINTTS): no half precision */
        return type <= 1 && frint();
    return opcode >= 0x08 && opcode <= 0x0f && opcode != 0x0d; /* the FRINT family */
}

static bool fp_scalar(uint32_t insn, struct native_desc *d)
{
    unsigned type = FIELD(insn, 23, 22);
    if (BIT(insn, 29))
        return false; /* S = 1 */
    if (BIT(insn, 24)) /* FMADD/FMSUB/FNMADD/FNMSUB */
        return !BIT(insn, 31) && fp_type_ok(type) && only_v(d);
    if (!BIT(insn, 21)) { /* floating point ↔ fixed point */
        unsigned rmode = FIELD(insn, 20, 19), opcode = FIELD(insn, 18, 16);
        if (!fp_type_ok(type) || (!BIT(insn, 31) && !BIT(insn, 15)))
            return false;
        if (rmode == 0 && (opcode == 2 || opcode == 3))
            return with_gpr(d, 5);
        if (rmode == 3 && opcode <= 1)
            return with_gpr(d, 0);
        return false;
    }
    if (FIELD(insn, 15, 10) == 0)
        return fp_int_conversion(insn, d);
    if (BIT(insn, 31))
        return false; /* M must be 0 for the following groups */
    if (BIT(insn, 14) && FIELD(insn, 13, 10) == 0) /* 1 source operand */
        return fp_one_source(insn) && only_v(d);
    if (!fp_type_ok(type))
        return false; /* the other groups: single, double, and with FP16 also half precision */
    switch (FIELD(insn, 11, 10)) {
    case 1: /* FCCMP/FCCMPE */
    case 3: /* FCSEL */
        return only_v(d);
    case 2: /* FMUL/FDIV/FADD/FSUB/FMAX/FMIN/FMAXNM/FMINNM/FNMUL */
        return FIELD(insn, 15, 12) <= 8 && only_v(d);
    default:
        break;
    }
    if (BIT(insn, 12)) /* FMOV (immediate) */
        return FIELD(insn, 9, 5) == 0 && only_v(d);
    if (BIT(insn, 13)) /* FCMP/FCMPE */
        return FIELD(insn, 15, 14) == 0 && FIELD(insn, 2, 0) == 0 && only_v(d);
    return false;
}

/* ---- Top level ---- */

/* ---- Crypto extensions starting with 1100: SHA3 (EOR3, BCAX, RAX1, XAR), SHA512; SM3, SM4
 * rejected ---- */

static bool crypto_ext(uint32_t insn)
{
    bool sha3 = tuxport_has(TUXPORT_HWCAP_SHA3), sha512 = tuxport_has(TUXPORT_HWCAP_SHA512);
    if ((insn & 0xffc08000) == 0xce000000) /* EOR3, BCAX (four registers; SM3SS1 has bit 22 set) */
        return sha3;
    if ((insn & 0xffe00000) == 0xce800000) /* XAR */
        return sha3;
    if ((insn & 0xffe0f000) == 0xce608000) /* SHA512H, SHA512H2, SHA512SU1; opcode 11 is RAX1 */
        return FIELD(insn, 11, 10) == 3 ? sha3 : sha512;
    if ((insn & 0xfffffc00) == 0xcec08000) /* SHA512SU0 */
        return sha512;
    return false;
}

bool tuxport_simd_dp_decode(uint32_t insn, struct native_desc *d)
{
    unsigned op0 = FIELD(insn, 31, 28);
    only_v(d);
    if ((op0 & 0x5) == 0x1) /* x0x1: scalar floating point */
        return fp_scalar(insn, d);
    if ((op0 & 0xd) == 0x5) { /* 01x1: Advanced SIMD scalar, including SHA */
        if ((insn & 0xffe08c00) == 0x5e000000)
            return crypto_sha3(insn);
        if ((insn & 0xfffe0c00) == 0x5e280800)
            return crypto_sha2(insn);
        if ((insn & 0xdfe08400) == 0x5e000400) /* DUP (element), scalar */
            return !BIT(insn, 29) && FIELD(insn, 14, 11) == 0 && (FIELD(insn, 19, 16) != 0);
        if ((insn & 0xdf60c400) == 0x5e400400)
            return scalar_three_same_fp16(insn);
        if ((insn & 0xdf7e0c00) == 0x5e780800)
            return scalar_two_reg_misc_fp16(insn);
        if ((insn & 0xdf200400) == 0x5e200400)
            return scalar_three_same(insn);
        if ((insn & 0xdf208400) == 0x5e008400)
            return scalar_three_extra(insn);
        if ((insn & 0xdf200c00) == 0x5e200000) /* SQDMLAL/SQDMLSL/SQDMULL */
            return !BIT(insn, 29) && (FIELD(insn, 15, 12) == 0x9 || FIELD(insn, 15, 12) == 0xb ||
                                      FIELD(insn, 15, 12) == 0xd) &&
                   (FIELD(insn, 23, 22) == 1 || FIELD(insn, 23, 22) == 2);
        if ((insn & 0xdf3e0c00) == 0x5e200800)
            return scalar_two_reg_misc(insn);
        if ((insn & 0xdf3e0c00) == 0x5e300800)
            return scalar_pairwise(insn);
        if ((insn & 0xdf800400) == 0x5f000400)
            return scalar_shift_imm(insn);
        if ((insn & 0xdf000400) == 0x5f000000)
            return scalar_indexed(insn);
        return false;
    }
    if ((op0 & 0x9) == 0x0) { /* 0xx0: Advanced SIMD vector, including AES */
        if ((insn & 0xfffe0c00) == 0x4e280800) /* AESE/AESD/AESMC/AESIMC */
            return FIELD(insn, 16, 12) >= 4 && FIELD(insn, 16, 12) <= 7 && tuxport_has(TUXPORT_HWCAP_AES);
        if ((insn & 0xbfe08c00) == 0x0e000000) /* TBL/TBX */
            return true;
        if ((insn & 0xbf208c00) == 0x0e000800) { /* UZP/TRN/ZIP */
            unsigned opcode = FIELD(insn, 14, 12);
            return (opcode & 3) != 0 && !(FIELD(insn, 23, 22) == 3 && !BIT(insn, 30));
        }
        if ((insn & 0xbfe08400) == 0x2e000000) /* EXT */
            return BIT(insn, 30) || !BIT(insn, 14);
        if ((insn & 0x9fe08400) == 0x0e000400)
            return vec_copy(insn, d);
        if ((insn & 0x9f60c400) == 0x0e400400)
            return vec_three_same_fp16(insn);
        if ((insn & 0x9f7e0c00) == 0x0e780800)
            return vec_two_reg_misc_fp16(insn);
        if ((insn & 0x9f200400) == 0x0e200400)
            return vec_three_same(insn, d);
        if ((insn & 0x9f208400) == 0x0e008400)
            return vec_three_extra(insn);
        if ((insn & 0x9f200c00) == 0x0e200000)
            return vec_three_different(insn);
        if ((insn & 0x9f3e0c00) == 0x0e200800)
            return vec_two_reg_misc(insn);
        if ((insn & 0x9f3e0c00) == 0x0e300800)
            return vec_across_lanes(insn);
        if ((insn & 0x9ff80400) == 0x0f000400)
            return vec_modified_imm(insn);
        if ((insn & 0x9f800400) == 0x0f000400)
            return vec_shift_imm(insn);
        if ((insn & 0x9f000400) == 0x0f000000)
            return vec_indexed(insn);
        return false;
    }
    if (op0 == 0xc)
        return crypto_ext(insn);
    return false;
}

/* ---- Memory access ---- */

static uint8_t *vreg(struct tuxport_cpu *c, unsigned r)
{
    return (uint8_t *)c->v[r & 31];
}

static uint64_t gpr(const struct tuxport_cpu *c, unsigned r)
{
    return r == 31 ? 0 : c->x[r];
}

static uint64_t base_reg(const struct tuxport_cpu *c, unsigned rn)
{
    return rn == 31 ? c->sp : c->x[rn];
}

static void write_base(struct tuxport_cpu *c, unsigned rn, uint64_t v)
{
    if (rn == 31)
        c->sp = v;
    else
        c->x[rn] = v;
}

static enum tuxport_exit undef(struct tuxport_cpu *c, uint32_t insn)
{
    c->exit_info = insn;
    return TUXPORT_EXIT_UNDEF;
}

/* Confirms that every page of [va, va + len) can be accessed as access, and that no hardware
 * watchpoint is hit */
static bool probe(struct tuxport_cpu *c, uint64_t va, uint64_t len, uint32_t access)
{
    va = untag(va);
    uint64_t end = va + len;
    for (uint64_t p = va; p < end; p = (p & ~PAGE_MASK) + TUXPORT_PAGE_SIZE) {
        if (!tlb_lookup(c, p, access))
            return raise_fault(c, p, access, TUXPORT_FAULT_PAGE);
    }
    uint32_t watch = access == TUXPORT_ACCESS_READ ? TUXPORT_WATCH_LOAD : TUXPORT_WATCH_STORE;
    if (UNLIKELY(watch_hit(c, va, len, watch)))
        return raise_fault(c, va, access, TUXPORT_FAULT_WATCH);
    return true;
}

/* SP-based accesses require SP to be 16-byte aligned */
static bool check_sp(struct tuxport_cpu *c, unsigned rn, uint32_t access)
{
    if (rn == 31 && (c->sp & 15))
        return raise_fault(c, c->sp, access, TUXPORT_FAULT_SP_ALIGN);
    return true;
}

/* An access already probed, which cannot fail */
static void copy_in(struct tuxport_cpu *c, uint64_t va, void *dst, unsigned n)
{
    (void)mem_read(c, va, dst, n);
}

static void copy_out(struct tuxport_cpu *c, uint64_t va, const void *src, unsigned n)
{
    (void)mem_write(c, va, src, n);
}

/* LD1–LD4/ST1–ST4 (multiple structures) */
static enum tuxport_exit ldst_multiple(struct tuxport_cpu *c, uint32_t insn)
{
    unsigned q = BIT(insn, 30), load = BIT(insn, 22), post = BIT(insn, 23);
    unsigned rm = FIELD(insn, 20, 16), opcode = FIELD(insn, 15, 12), size = FIELD(insn, 11, 10);
    unsigned rn = FIELD(insn, 9, 5), rt = insn & 31, rpt, selem;
    if (BIT(insn, 21) || (!post && rm != 0))
        return undef(c, insn);
    switch (opcode) {
    case 0x0: rpt = 1, selem = 4; break; /* LD4/ST4 */
    case 0x2: rpt = 4, selem = 1; break; /* LD1/ST1, 4 registers */
    case 0x4: rpt = 1, selem = 3; break; /* LD3/ST3 */
    case 0x6: rpt = 3, selem = 1; break; /* LD1/ST1, 3 registers */
    case 0x7: rpt = 1, selem = 1; break; /* LD1/ST1, 1 register */
    case 0x8: rpt = 1, selem = 2; break; /* LD2/ST2 */
    case 0xa: rpt = 2, selem = 1; break; /* LD1/ST1, 2 registers */
    default: return undef(c, insn);
    }
    if (size == 3 && !q && selem != 1)
        return undef(c, insn);

    uint32_t access = load ? TUXPORT_ACCESS_READ : TUXPORT_ACCESS_WRITE;
    /* LD1/ST1 lay out each whole register contiguously in memory. */
    unsigned reg_bytes = 8u << q;
    unsigned copy_bytes = selem == 1 ? reg_bytes : 1u << size;
    unsigned copies = selem == 1 ? 1 : reg_bytes >> size;
    uint64_t addr = base_reg(c, rn), total = (uint64_t)rpt * selem * reg_bytes;
    if (!check_sp(c, rn, access) || !probe(c, addr, total, access))
        return TUXPORT_EXIT_FAULT;

    uint8_t regs[4][16] = {{0}};
    uint64_t offs = 0;
    for (unsigned r = 0; r < rpt; r++) {
        for (unsigned e = 0; e < copies; e++) {
            for (unsigned s = 0; s < selem; s++) {
                uint8_t *elem = &regs[r + s][e * copy_bytes];
                if (load)
                    copy_in(c, addr + offs, elem, copy_bytes);
                else
                    copy_out(c, addr + offs, vreg(c, rt + r + s) + e * copy_bytes, copy_bytes);
                offs += copy_bytes;
            }
        }
    }
    if (load) {
        for (unsigned k = 0; k < rpt * selem; k++)
            memcpy(vreg(c, rt + k), regs[k], 16); /* with Q = 0 the high 64 bits are zeroed */
    }
    if (post)
        write_base(c, rn, addr + (rm == 31 ? total : gpr(c, rm)));
    c->pc += 4;
    return TUXPORT_EXIT_NONE;
}

/* LD1–LD4/ST1–ST4 (single structure) and LD1R–LD4R */
static enum tuxport_exit ldst_single(struct tuxport_cpu *c, uint32_t insn)
{
    unsigned q = BIT(insn, 30), load = BIT(insn, 22), post = BIT(insn, 23), r = BIT(insn, 21);
    unsigned rm = FIELD(insn, 20, 16), opcode = FIELD(insn, 15, 13), s = BIT(insn, 12);
    unsigned size = FIELD(insn, 11, 10), rn = FIELD(insn, 9, 5), rt = insn & 31;
    unsigned selem = ((opcode & 1) << 1 | r) + 1, scale = opcode >> 1, index = 0;
    bool replicate = false;
    if (!post && rm != 0)
        return undef(c, insn);
    switch (scale) {
    case 0:
        index = q << 3 | s << 2 | size;
        break;
    case 1:
        if (size & 1)
            return undef(c, insn);
        index = q << 2 | s << 1 | size >> 1;
        break;
    case 2:
        if (size & 2)
            return undef(c, insn);
        if (size & 1) {
            if (s)
                return undef(c, insn);
            index = q;
            scale = 3;
        } else {
            index = q << 1 | s;
        }
        break;
    default:
        if (!load || s)
            return undef(c, insn);
        scale = size;
        replicate = true;
        break;
    }

    uint32_t access = load ? TUXPORT_ACCESS_READ : TUXPORT_ACCESS_WRITE;
    unsigned esize = 1u << scale;
    uint64_t addr = base_reg(c, rn), total = (uint64_t)selem * esize;
    if (!check_sp(c, rn, access) || !probe(c, addr, total, access))
        return TUXPORT_EXIT_FAULT;

    for (unsigned k = 0; k < selem; k++) {
        uint8_t *v = vreg(c, rt + k);
        uint64_t va = addr + (uint64_t)k * esize;
        if (replicate) {
            uint8_t elem[8], value[16] = {0};
            copy_in(c, va, elem, esize);
            for (unsigned i = 0; i < (q ? 16u : 8u); i += esize)
                memcpy(value + i, elem, esize);
            memcpy(v, value, 16);
        } else if (load) {
            copy_in(c, va, v + index * esize, esize);
        } else {
            copy_out(c, va, v + index * esize, esize);
        }
    }
    if (post)
        write_base(c, rn, addr + (rm == 31 ? total : gpr(c, rm)));
    c->pc += 4;
    return TUXPORT_EXIT_NONE;
}

/* Reads or writes n bytes (n ≤ 16) of a whole SIMD&FP register; a read zeroes the high part */
static void load_v(struct tuxport_cpu *c, unsigned rt, uint64_t va, unsigned n)
{
    uint8_t value[16] = {0};
    copy_in(c, va, value, n);
    memcpy(vreg(c, rt), value, 16);
}

enum tuxport_exit tuxport_simd_ldst(struct tuxport_cpu *c, uint32_t insn)
{
    if (!tuxport_has(TUXPORT_HWCAP_FP | TUXPORT_HWCAP_ASIMD))
        return undef(c, insn);
    if ((insn & 0xbf000000) == 0x0c000000)
        return ldst_multiple(c, insn);
    if ((insn & 0xbf000000) == 0x0d000000)
        return ldst_single(c, insn);

    unsigned size = FIELD(insn, 31, 30), opc = FIELD(insn, 23, 22);
    unsigned rt = insn & 31, rn = FIELD(insn, 9, 5);

    if ((insn & 0x3f000000) == 0x1c000000) { /* LDR (literal): bits 31:30 are opc */
        if (size == 3)
            return undef(c, insn);
        unsigned n = 4u << size;
        uint64_t va = c->pc + (uint64_t)(((int64_t)((uint64_t)FIELD(insn, 23, 5) << 45) >> 45) * 4);
        if (!probe(c, va, n, TUXPORT_ACCESS_READ))
            return TUXPORT_EXIT_FAULT;
        load_v(c, rt, va, n);
        c->pc += 4;
        return TUXPORT_EXIT_NONE;
    }

    if ((insn & 0x3e000000) == 0x2c000000) { /* LDP/STP/LDNP/STNP */
        unsigned idx = FIELD(insn, 24, 23), load = BIT(insn, 22), rt2 = FIELD(insn, 14, 10);
        if (size == 3)
            return undef(c, insn);
        unsigned n = 4u << size;
        uint32_t access = load ? TUXPORT_ACCESS_READ : TUXPORT_ACCESS_WRITE;
        uint64_t base = base_reg(c, rn);
        int64_t off = ((int64_t)((uint64_t)FIELD(insn, 21, 15) << 57) >> 57) * (int64_t)n;
        uint64_t va = idx == 1 ? base : base + (uint64_t)off;
        if (!check_sp(c, rn, access) || !probe(c, va, 2 * n, access))
            return TUXPORT_EXIT_FAULT;
        if (load) {
            load_v(c, rt, va, n);
            load_v(c, rt2, va + n, n);
        } else {
            copy_out(c, va, vreg(c, rt), n);
            copy_out(c, va + n, vreg(c, rt2), n);
        }
        if (idx == 1 || idx == 3)
            write_base(c, rn, base + (uint64_t)off);
        c->pc += 4;
        return TUXPORT_EXIT_NONE;
    }

    /* Single register: size = 00 with opc = 1x is the 128-bit Q register */
    unsigned scale, load = opc & 1;
    if (size == 0 && (opc & 2))
        scale = 4;
    else if (opc & 2)
        return undef(c, insn);
    else
        scale = size;
    unsigned n = 1u << scale;
    uint32_t access = load ? TUXPORT_ACCESS_READ : TUXPORT_ACCESS_WRITE;
    uint64_t base = base_reg(c, rn), va, wb = 0;
    bool writeback = false;

    if ((insn & 0x3f000000) == 0x3d000000) { /* unsigned offset */
        va = base + ((uint64_t)FIELD(insn, 21, 10) << scale);
    } else if ((insn & 0x3f200000) == 0x3c000000) { /* 9-bit immediate */
        unsigned op = FIELD(insn, 11, 10);
        uint64_t off = (uint64_t)((int64_t)((uint64_t)FIELD(insn, 20, 12) << 55) >> 55);
        if (op == 2)
            return undef(c, insn); /* the unprivileged forms have no SIMD&FP version */
        va = op == 1 ? base : base + off;
        writeback = op != 0;
        wb = base + off;
    } else if ((insn & 0x3f200c00) == 0x3c200800) { /* register offset */
        unsigned option = FIELD(insn, 15, 13), shift = BIT(insn, 12) ? scale : 0;
        uint64_t m = gpr(c, FIELD(insn, 20, 16));
        if (!(option & 2))
            return undef(c, insn);
        switch (option) {
        case 2: m = (uint32_t)m; break;
        case 6: m = (uint64_t)(int64_t)(int32_t)m; break;
        default: break;
        }
        va = base + (m << shift);
    } else {
        return undef(c, insn);
    }

    if (!check_sp(c, rn, access) || !probe(c, va, n, access))
        return TUXPORT_EXIT_FAULT;
    if (load)
        load_v(c, rt, va, n);
    else
        copy_out(c, va, vreg(c, rt), n);
    if (writeback)
        write_base(c, rn, wb);
    c->pc += 4;
    return TUXPORT_EXIT_NONE;
}
