/*
 * Host aarch64 instruction encoder, shared by Tier 1 (dbt.c) and Tier 0's native instruction stubs
 * (native.c). Each function returns the 32-bit encoding of one instruction; the caller guarantees
 * the range and alignment of immediates.
 */
#ifndef TUXPORT_ENGINE_EMIT_H
#define TUXPORT_ENGINE_EMIT_H

#include <stddef.h>
#include <stdint.h>


static inline uint32_t enc_ldr_x(unsigned rt, unsigned rn, size_t off)
{
    return 0xf9400000u | (uint32_t)(off / 8) << 10 | rn << 5 | rt;
}

/* LDUR Xt, [Xn, #off]: offset not aligned to 8 (-256..255) */
static inline uint32_t enc_ldur_x(unsigned rt, unsigned rn, int off)
{
    return 0xf8400000u | ((uint32_t)off & 0x1ff) << 12 | rn << 5 | rt;
}

static inline uint32_t enc_str_x(unsigned rt, unsigned rn, size_t off)
{
    return 0xf9000000u | (uint32_t)(off / 8) << 10 | rn << 5 | rt;
}

static inline uint32_t enc_ldr_w(unsigned rt, unsigned rn, size_t off)
{
    return 0xb9400000u | (uint32_t)(off / 4) << 10 | rn << 5 | rt;
}

static inline uint32_t enc_str_w(unsigned rt, unsigned rn, size_t off)
{
    return 0xb9000000u | (uint32_t)(off / 4) << 10 | rn << 5 | rt;
}

/* LDRB/LDRH/LDR Wt/LDR Xt, [Xn] (size = log2 of the byte count) */
static inline uint32_t enc_ldr_size(unsigned size, unsigned rt, unsigned rn)
{
    return 0x39400000u | size << 30 | rn << 5 | rt;
}

/* LDARB/LDARH/LDAR */
static inline uint32_t enc_ldar(unsigned size, unsigned rt, unsigned rn)
{
    return 0x08dffc00u | size << 30 | rn << 5 | rt;
}

/* LDAXRB/LDAXRH/LDAXR */
static inline uint32_t enc_ldaxr(unsigned size, unsigned rt, unsigned rn)
{
    return 0x085ffc00u | size << 30 | rn << 5 | rt;
}

/* STLRB/STLRH/STLR Rt, [Xn] */
static inline uint32_t enc_stlr(unsigned size, unsigned rt, unsigned rn)
{
    return 0x089ffc00u | size << 30 | rn << 5 | rt;
}

/* STLXRB/STLXRH/STLXR Ws, Rt, [Xn] */
static inline uint32_t enc_stlxr(unsigned size, unsigned rs, unsigned rt, unsigned rn)
{
    return 0x0800fc00u | size << 30 | rs << 16 | rn << 5 | rt;
}

static inline uint32_t enc_clrex(void)
{
    return 0xd503305fu;
}

static inline uint32_t enc_ldp_x(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0xa9400000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_stp_x(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0xa9000000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_stp_x_pre(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0xa9800000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_ldp_x_post(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0xa8c00000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_mov(unsigned rd, unsigned rm) /* ORR Xd, XZR, Xm */
{
    return 0xaa0003e0u | rm << 16 | rd;
}

static inline uint32_t enc_add_imm(unsigned rd, unsigned rn, uint32_t imm12, unsigned shift12)
{
    return 0x91000000u | shift12 << 22 | imm12 << 10 | rn << 5 | rd;
}

static inline uint32_t enc_sub_imm(unsigned rd, unsigned rn, uint32_t imm12, unsigned shift12)
{
    return 0xd1000000u | shift12 << 22 | imm12 << 10 | rn << 5 | rd;
}

static inline uint32_t enc_movz(unsigned rd, uint32_t imm16, unsigned hw)
{
    return 0xd2800000u | hw << 21 | imm16 << 5 | rd;
}

static inline uint32_t enc_movk(unsigned rd, uint32_t imm16, unsigned hw)
{
    return 0xf2800000u | hw << 21 | imm16 << 5 | rd;
}

/* AND Xd, Xn, #((1 << bits) - 1) */
static inline uint32_t enc_and_low(unsigned rd, unsigned rn, unsigned bits)
{
    return 0x92400000u | (bits - 1) << 10 | rn << 5 | rd;
}

/* AND Xd, Xn, #mask: `ones` consecutive ones, rotated right by `ror` bits */
static inline uint32_t enc_and_ror(unsigned rd, unsigned rn, unsigned ones, unsigned ror)
{
    return 0x92400000u | ror << 16 | (ones - 1) << 10 | rn << 5 | rd;
}

static inline uint32_t enc_ubfx(unsigned rd, unsigned rn, unsigned lsb, unsigned width)
{
    return 0xd3400000u | lsb << 16 | (lsb + width - 1) << 10 | rn << 5 | rd;
}

static inline uint32_t enc_sbfx(unsigned rd, unsigned rn, unsigned lsb, unsigned width)
{
    return 0x93400000u | lsb << 16 | (lsb + width - 1) << 10 | rn << 5 | rd;
}

static inline uint32_t enc_add_lsl(unsigned rd, unsigned rn, unsigned rm, unsigned amount)
{
    return 0x8b000000u | rm << 16 | amount << 10 | rn << 5 | rd;
}

/* EOR Xd, Xn, Xm, LSR #amount */
static inline uint32_t enc_eor_lsr(unsigned rd, unsigned rn, unsigned rm, unsigned amount)
{
    return 0xca000000u | (amount ? 1u << 22 : 0) | rm << 16 | amount << 10 | rn << 5 | rd;
}

/* EON Xd, Xn, Xm, LSR #amount (Xn ^ ~(Xm >> amount)) */
static inline uint32_t enc_eon_lsr(unsigned rd, unsigned rn, unsigned rm, unsigned amount)
{
    return 0xca200000u | (amount ? 1u << 22 : 0) | rm << 16 | amount << 10 | rn << 5 | rd;
}

/* BIC Xd, Xn, Xm (Xn & ~Xm) */
static inline uint32_t enc_bic_reg(unsigned rd, unsigned rn, unsigned rm)
{
    return 0x8a200000u | rm << 16 | rn << 5 | rd;
}

static inline uint32_t enc_add_ext(unsigned rd, unsigned rn, unsigned rm, unsigned option, unsigned shift)
{
    return 0x8b200000u | rm << 16 | option << 13 | shift << 10 | rn << 5 | rd;
}

static inline uint32_t enc_ldst_regoff(unsigned size, unsigned opc, unsigned rt, unsigned rn, unsigned rm)
{
    return size << 30 | 0x38206800u | opc << 22 | rm << 16 | rn << 5 | rt;
}

static inline uint32_t enc_b(intptr_t off)
{
    return 0x14000000u | ((uint32_t)(off >> 2) & 0x3ffffff);
}

static inline uint32_t enc_br(unsigned rn)
{
    return 0xd61f0000u | rn << 5;
}

static inline uint32_t enc_bl(intptr_t off)
{
    return 0x94000000u | ((uint32_t)(off >> 2) & 0x3ffffff);
}

/* RET Xn: the host predicts the target with its return address stack */
static inline uint32_t enc_ret_reg(unsigned rn)
{
    return 0xd65f0000u | rn << 5;
}

/* BFXIL Xd, Xn, #0, #width: put the low width bits of Xn into Xd, other bits unchanged */
static inline uint32_t enc_bfxil_low(unsigned rd, unsigned rn, unsigned width)
{
    return 0xb3400000u | (width - 1) << 10 | rn << 5 | rd;
}

static inline uint32_t enc_cbz_x(unsigned rt)
{
    return 0xb4000000u | rt;
}

static inline uint32_t enc_cbnz_x(unsigned rt)
{
    return 0xb5000000u | rt;
}

static inline uint32_t enc_tbz(unsigned rt, unsigned bit)
{
    return 0x36000000u | (bit >> 5) << 31 | (bit & 31) << 19 | rt;
}

static inline uint32_t enc_tbnz(unsigned rt, unsigned bit)
{
    return 0x37000000u | (bit >> 5) << 31 | (bit & 31) << 19 | rt;
}

static inline uint32_t enc_adr(unsigned rd, intptr_t off)
{
    return 0x10000000u | ((uint32_t)off & 3) << 29 | ((uint32_t)(off >> 2) & 0x7ffff) << 5 | rd;
}

static inline uint32_t enc_adr_self(unsigned rd)
{
    return enc_adr(rd, 0);
}

/* LDR Xt, [Xn, Xm, LSL #3] */
static inline uint32_t enc_ldr_x_index(unsigned rt, unsigned rn, unsigned rm)
{
    return 0xf8607800u | rm << 16 | rn << 5 | rt;
}

/* AND Xd, Xn, #~((1 << bits) - 1): clear the low bits bits, 1 ≤ bits ≤ 63 */
static inline uint32_t enc_and_clear_low(unsigned rd, unsigned rn, unsigned bits)
{
    return 0x92400000u | (64 - bits) << 16 | (63 - bits) << 10 | rn << 5 | rd;
}

static inline uint32_t enc_sub_reg(unsigned rd, unsigned rn, unsigned rm)
{
    return 0xcb000000u | rm << 16 | rn << 5 | rd;
}

static inline uint32_t enc_orr_reg(unsigned rd, unsigned rn, unsigned rm)
{
    return 0xaa000000u | rm << 16 | rn << 5 | rd;
}

/* ORR Xd, Xn, Xm, LSL #amount */
static inline uint32_t enc_orr_lsl(unsigned rd, unsigned rn, unsigned rm, unsigned amount)
{
    return 0xaa000000u | rm << 16 | amount << 10 | rn << 5 | rd;
}

/* LSR Xd, Xn, Xm (LSRV) */
static inline uint32_t enc_lsrv(unsigned rd, unsigned rn, unsigned rm)
{
    return 0x9ac02400u | rm << 16 | rn << 5 | rd;
}

/* MUL Xd, Xn, Xm (MADD Xd, Xn, Xm, XZR) */
static inline uint32_t enc_mul(unsigned rd, unsigned rn, unsigned rm)
{
    return 0x9b007c00u | rm << 16 | rn << 5 | rd;
}

/* AND Xd, Xn, Xm */
static inline uint32_t enc_and_reg(unsigned rd, unsigned rn, unsigned rm)
{
    return 0x8a000000u | rm << 16 | rn << 5 | rd;
}

/* LSL Xd, Xn, Xm (LSLV) */
static inline uint32_t enc_lslv(unsigned rd, unsigned rn, unsigned rm)
{
    return 0x9ac02000u | rm << 16 | rn << 5 | rd;
}

/* ---- SIMD&FP and system registers ---- */

static inline uint32_t enc_ldr_q(unsigned rt, unsigned rn, size_t off)
{
    return 0x3dc00000u | (uint32_t)(off / 16) << 10 | rn << 5 | rt;
}

static inline uint32_t enc_str_q(unsigned rt, unsigned rn, size_t off)
{
    return 0x3d800000u | (uint32_t)(off / 16) << 10 | rn << 5 | rt;
}

/* LDP/STP Qt1, Qt2, [Xn, #off] (off a multiple of 16, range ±1008) */
static inline uint32_t enc_ldp_q(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0xad400000u | ((uint32_t)(off / 16) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_stp_q(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0xad000000u | ((uint32_t)(off / 16) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

/* LDP/STP Dt1, Dt2: save and restore the host's callee-saved registers d8–d15 */
static inline uint32_t enc_stp_d_pre(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0x6d800000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_stp_d(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0x6d000000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_ldp_d(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0x6d400000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_ldp_d_post(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0x6cc00000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_mrs_nzcv(unsigned rt)
{
    return 0xd53b4200u | rt;
}

static inline uint32_t enc_msr_nzcv(unsigned rt)
{
    return 0xd51b4200u | rt;
}

static inline uint32_t enc_mrs_fpcr(unsigned rt)
{
    return 0xd53b4400u | rt;
}

static inline uint32_t enc_msr_fpcr(unsigned rt)
{
    return 0xd51b4400u | rt;
}

static inline uint32_t enc_mrs_fpsr(unsigned rt)
{
    return 0xd53b4420u | rt;
}

static inline uint32_t enc_msr_fpsr(unsigned rt)
{
    return 0xd51b4420u | rt;
}

static inline uint32_t enc_ret(void)
{
    return 0xd65f03c0u;
}

#endif /* TUXPORT_ENGINE_EMIT_H */
