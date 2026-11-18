/*
 * Guest-visible CPU features (the engine map): host support ∩ implemented by Tier 0 ∩ the
 * whitelist. HWCAP, the ID registers and /proc/cpuinfo are all generated from this one set, and
 * the decoders also use it to decide whether optional instructions such as
 * AES/SHA/CRC32/LSE/DotProd/I8MM/LRCPC/LRCPC2/FP16/JSCVT/RDM/FHM/FCMA/BF16/
 * FRINTTS/FlagM/FlagM2/SB/DPB/DPB2/DIT are valid.
 */
#include <pthread.h>

#include "internal.h"

#if defined(__aarch64__) && defined(__linux__)
#include <sys/auxv.h>
#define HOST_IS_AARCH64_LINUX 1
#endif

/* Features Tier 0 implements. FP, ASIMD and these optional extensions all depend on native
 * instruction stubs */
#define IMPLEMENTED                                                                                \
    (TUXPORT_HWCAP_FP | TUXPORT_HWCAP_ASIMD | TUXPORT_HWCAP_AES | TUXPORT_HWCAP_PMULL |                \
     TUXPORT_HWCAP_SHA1 | TUXPORT_HWCAP_SHA2 | TUXPORT_HWCAP_CRC32 | TUXPORT_HWCAP_ATOMICS |          \
     TUXPORT_HWCAP_SHA3 | TUXPORT_HWCAP_ASIMDDP | TUXPORT_HWCAP_SHA512 | TUXPORT_HWCAP_LRCPC |         \
     TUXPORT_HWCAP_ILRCPC | TUXPORT_HWCAP_FPHP | TUXPORT_HWCAP_ASIMDHP | TUXPORT_HWCAP_JSCVT |       \
     TUXPORT_HWCAP_ASIMDRDM | TUXPORT_HWCAP_FCMA | TUXPORT_HWCAP_ASIMDFHM)
#define IMPLEMENTED2 (TUXPORT_HWCAP2_I8MM | TUXPORT_HWCAP2_BF16 | TUXPORT_HWCAP2_FRINT)

/* The ones not using native instruction stubs: for FlagM and FlagM2 the interpreter computes the
 * flags, SB and DC CVAP, DC CVADP need no work in the interpreter, and DIT is kept in the cpu;
 * Tier 1 copies CFINV, AXFLAG, XAFLAG, RMIF, SETF and SB verbatim, and the host thread's DIT
 * follows the guest's (tuxport_sync_dit), so the host must have them too. ECV's self-synchronized
 * counter (CNTVCTSS_EL0) is read on the host by both tiers: the host's vDSO, which guests run
 * (the kernel/vdso map), reads it instead of CNTVCT_EL0 on hosts that have it */
#define INTEGER (TUXPORT_HWCAP_FLAGM | TUXPORT_HWCAP_SB | TUXPORT_HWCAP_DCPOP | TUXPORT_HWCAP_DIT)
#define INTEGER2 (TUXPORT_HWCAP2_FLAGM2 | TUXPORT_HWCAP2_DCPODP | TUXPORT_HWCAP2_ECV)

/* SHA3 and SHA512 are advertised only together, and also need SHA2: LLVM's sha3 feature covers
 * both groups, so the whitelist check cannot tell them apart, and real CPUs always implement them
 * together as well */
#define SHA3_SET (TUXPORT_HWCAP_SHA2 | TUXPORT_HWCAP_SHA3 | TUXPORT_HWCAP_SHA512)

/* Scalar and vector half precision are advertised only together: the whitelist decides by one
 * switch, and LLVM's fullfp16 is also one feature */
#define FP16_SET (TUXPORT_HWCAP_FPHP | TUXPORT_HWCAP_ASIMDHP)

static pthread_once_t once = PTHREAD_ONCE_INIT;
static uint64_t guest_hwcap, guest_hwcap2;
static uint64_t host_midr;

static void detect(void)
{
    /* We emulate the ID registers ourselves (the engine map), independently of the host */
    guest_hwcap = TUXPORT_HWCAP_CPUID;
#ifdef HOST_IS_AARCH64_LINUX
    uint64_t host = getauxval(AT_HWCAP);
    /* A host without FP and ASIMD supports no SIMD&FP instructions; the same holds when native
     * stubs are unavailable */
    if ((host & TUXPORT_HWCAP_FP) && (host & TUXPORT_HWCAP_ASIMD) && tuxport_native_available()) {
        guest_hwcap |= host & IMPLEMENTED;
        if ((guest_hwcap & SHA3_SET) != SHA3_SET)
            guest_hwcap &= ~(TUXPORT_HWCAP_SHA3 | TUXPORT_HWCAP_SHA512);
        if ((guest_hwcap & FP16_SET) != FP16_SET)
            guest_hwcap &= ~FP16_SET;
        /* FHM's source operands are half precision, so the architecture requires FP16 (LLVM's
         * fp16fml also implies fullfp16) */
        if (!(guest_hwcap & TUXPORT_HWCAP_FPHP))
            guest_hwcap &= ~TUXPORT_HWCAP_ASIMDFHM;
        /* LRCPC2 includes LRCPC (they are the same field in the ID register) */
        if (!(guest_hwcap & TUXPORT_HWCAP_LRCPC))
            guest_hwcap &= ~TUXPORT_HWCAP_ILRCPC;
        guest_hwcap2 = getauxval(AT_HWCAP2) & IMPLEMENTED2;
    }
    guest_hwcap |= host & INTEGER;
    guest_hwcap2 |= getauxval(AT_HWCAP2) & INTEGER2;
    /* FlagM2 and DPB2 are both added on top of the previous level (value 2 of the same field in
     * the ID register) */
    if (!(guest_hwcap & TUXPORT_HWCAP_FLAGM))
        guest_hwcap2 &= ~TUXPORT_HWCAP2_FLAGM2;
    if (!(guest_hwcap & TUXPORT_HWCAP_DCPOP))
        guest_hwcap2 &= ~TUXPORT_HWCAP2_DCPODP;
    if (host & TUXPORT_HWCAP_CPUID)
        __asm__ volatile("mrs %0, midr_el1" : "=r"(host_midr));
#endif
}

uint64_t tuxport_guest_hwcap(void)
{
    pthread_once(&once, detect);
    return guest_hwcap;
}

uint64_t tuxport_guest_hwcap2(void)
{
    pthread_once(&once, detect);
    return guest_hwcap2;
}

uint64_t tuxport_guest_midr(void)
{
    pthread_once(&once, detect);
    return host_midr;
}

bool tuxport_has(uint64_t hwcap)
{
    return (tuxport_guest_hwcap() & hwcap) == hwcap;
}

bool tuxport_has2(uint64_t hwcap2)
{
    return (tuxport_guest_hwcap2() & hwcap2) == hwcap2;
}

/*
 * The values returned when EL0 reads the ID register space (op0=3, op1=0, CRn=0), consistent
 * with what Linux emulates for user mode: MIDR is returned as is, MPIDR has only bit 31, and the
 * rest expose only the fields corresponding to HWCAP. key is bits 20:5 of the MRS encoding.
 * Unlike Linux, which raises SIGILL for CRm=1 and for CRm=0 registers other than MIDR, MPIDR and
 * REVIDR, those read 0 here.
 */
bool tuxport_id_register(unsigned key, uint64_t *value)
{
    unsigned op0 = key >> 14, op1 = (key >> 11) & 7, crn = (key >> 7) & 15;
    unsigned crm = (key >> 3) & 15, op2 = key & 7;
    if (op0 != 3 || op1 != 0 || crn != 0 || crm > 7)
        return false;
    uint64_t h = tuxport_guest_hwcap(), h2 = tuxport_guest_hwcap2();
    uint64_t v = 0;
    if (crm == 0) {
        if (op2 == 0)
            v = host_midr;
        else if (op2 == 5)
            v = UINT64_C(1) << 31; /* MPIDR_EL1 */
    } else if (crm == 4 && op2 == 0) { /* ID_AA64PFR0_EL1 */
        v = 1 | 1 << 4;                /* EL0 and EL1 support AArch64 only */
        /* FP, AdvSIMD: 0 is the base set, 1 includes half precision, f is absent */
        if (!(h & TUXPORT_HWCAP_FP))
            v |= UINT64_C(0xf) << 16;
        else if (h & TUXPORT_HWCAP_FPHP)
            v |= UINT64_C(1) << 16;
        if (!(h & TUXPORT_HWCAP_ASIMD))
            v |= UINT64_C(0xf) << 20;
        else if (h & TUXPORT_HWCAP_ASIMDHP)
            v |= UINT64_C(1) << 20;
        if (h & TUXPORT_HWCAP_DIT)
            v |= UINT64_C(1) << 48;
    } else if (crm == 6 && op2 == 0) { /* ID_AA64ISAR0_EL1 */
        if (h & TUXPORT_HWCAP_PMULL)
            v |= UINT64_C(2) << 4;
        else if (h & TUXPORT_HWCAP_AES)
            v |= UINT64_C(1) << 4;
        if (h & TUXPORT_HWCAP_SHA1)
            v |= UINT64_C(1) << 8;
        if (h & TUXPORT_HWCAP_SHA512)
            v |= UINT64_C(2) << 12;
        else if (h & TUXPORT_HWCAP_SHA2)
            v |= UINT64_C(1) << 12;
        if (h & TUXPORT_HWCAP_CRC32)
            v |= UINT64_C(1) << 16;
        if (h & TUXPORT_HWCAP_ATOMICS)
            v |= UINT64_C(2) << 20;
        if (h & TUXPORT_HWCAP_ASIMDRDM)
            v |= UINT64_C(1) << 28;
        if (h & TUXPORT_HWCAP_SHA3)
            v |= UINT64_C(1) << 32;
        if (h & TUXPORT_HWCAP_ASIMDDP)
            v |= UINT64_C(1) << 44;
        if (h & TUXPORT_HWCAP_ASIMDFHM)
            v |= UINT64_C(1) << 48;
        if (h2 & TUXPORT_HWCAP2_FLAGM2) /* TS */
            v |= UINT64_C(2) << 52;
        else if (h & TUXPORT_HWCAP_FLAGM)
            v |= UINT64_C(1) << 52;
    } else if (crm == 6 && op2 == 1) { /* ID_AA64ISAR1_EL1 */
        if (h2 & TUXPORT_HWCAP2_DCPODP) /* DPB */
            v |= 2;
        else if (h & TUXPORT_HWCAP_DCPOP)
            v |= 1;
        if (h & TUXPORT_HWCAP_JSCVT)
            v |= UINT64_C(1) << 12;
        if (h & TUXPORT_HWCAP_FCMA)
            v |= UINT64_C(1) << 16;
        if (h & TUXPORT_HWCAP_ILRCPC)
            v |= UINT64_C(2) << 20;
        else if (h & TUXPORT_HWCAP_LRCPC)
            v |= UINT64_C(1) << 20;
        if (h2 & TUXPORT_HWCAP2_FRINT) /* FRINTTS */
            v |= UINT64_C(1) << 32;
        if (h & TUXPORT_HWCAP_SB)
            v |= UINT64_C(1) << 36;
        if (h2 & TUXPORT_HWCAP2_BF16)
            v |= UINT64_C(1) << 44;
        if (h2 & TUXPORT_HWCAP2_I8MM)
            v |= UINT64_C(1) << 52;
    }
    *value = v;
    return true;
}
