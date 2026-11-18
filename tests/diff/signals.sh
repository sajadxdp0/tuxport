# siginfo and ucontext of synchronous signals: on precise exceptions the registers, the fault
# address, and the EC and WnR of ESR must all match the hardware. x19–x28 are first set to known
# values; of them, x27 and x28 are kept in the cpu struct in Tier 1
set -e
cd /tmp
cat > diff2.c <<'C'
#define _GNU_SOURCE
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <ucontext.h>
#include <unistd.h>

static char *target;

static void handler(int sig, siginfo_t *si, void *ctx)
{
    mcontext_t *mc = &((ucontext_t *)ctx)->uc_mcontext;
    unsigned ec = 0, wnr = 0;
    for (struct _aarch64_ctx *h = (struct _aarch64_ctx *)mc->__reserved; h->magic;
         h = (struct _aarch64_ctx *)((char *)h + h->size)) {
        if (h->magic == ESR_MAGIC) {
            uint64_t esr = ((struct esr_context *)h)->esr;
            ec = (unsigned)(esr >> 26);
            wnr = ec == 0x24 ? (unsigned)(esr >> 6 & 1) : 0;
        }
    }
    /* Fault address: relative to target for data accesses, to pc for instructions, to sp for SP
     * alignment faults */
    long addr = (long)((char *)si->si_addr - target);
    if (sig == SIGILL || sig == SIGTRAP)
        addr = (long)((uint64_t)si->si_addr - mc->pc);
    else if (ec == 0x26)
        addr = (long)((uint64_t)si->si_addr - mc->sp);
    printf("signal %d code %d addr %+ld ec %#x wnr %u:", sig, si->si_code, addr, ec, wnr);
    for (int r = 19; r <= 28; r++)
        printf(" %llx", (unsigned long long)mc->regs[r]);
    printf("\n");
    mc->pc += 4; /* skip the faulting instruction */
}

#define FAULT(insn, addr)                                                                          \
    __asm__ volatile("mov x19, #0x19\n\tmov x20, #0x20\n\tmov x21, #0x21\n\tmov x22, #0x22\n\t"     \
                     "mov x23, #0x23\n\tmov x24, #0x24\n\tmov x25, #0x25\n\tmov x26, #0x26\n\t"     \
                     "mov x27, #0x27\n\tmov x28, #0x28\n\t" insn                                   \
                     :                                                                             \
                     : "r"(addr)                                                                   \
                     : "x0", "x1", "x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27", "x28", \
                       "memory")

int main(void)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &sa, NULL);
    sigaction(SIGBUS, &sa, NULL);
    sigaction(SIGILL, &sa, NULL);
    sigaction(SIGTRAP, &sa, NULL);
    long page = sysconf(_SC_PAGESIZE);
    target = mmap(NULL, 2 * page, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    munmap(target + page, page);
    setvbuf(stdout, NULL, _IONBF, 0);
    FAULT("ldr x0, [%0, #4096]", target);       /* read an unmapped page */
    FAULT("str xzr, [%0, #8]", target);         /* write a read-only page */
    FAULT("sub sp, sp, #8\n\tldr x0, [sp]\n\tadd sp, sp, #8", target); /* SP not aligned */
    FAULT("ldp x0, x1, [%0]", target + 4088);   /* cross into an unmapped page */
    FAULT("udf #0", target);                    /* undefined instruction */
    FAULT("brk #7", target);                    /* breakpoint */
    printf("done\n");
    return 0;
}
C
gcc -O1 -o diff2 diff2.c
./diff2
