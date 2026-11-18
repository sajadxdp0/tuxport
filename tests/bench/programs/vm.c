/*
 * A tiny bytecode interpreter: the switch dispatch compiles to a jump table and produces many
 * indirect branches, approximating interpreter workloads such as python. All values are unsigned
 * and wrap around modulo on overflow.
 * Usage: vm [n]
 */
#include "rt.h"

enum {
    OP_PUSH, /* followed by an immediate */
    OP_LOAD, /* followed by a local variable number */
    OP_STORE,
    OP_ADD,
    OP_SUB,
    OP_MUL,
    OP_MOD,
    OP_LT,
    OP_JZ, /* followed by the target address */
    OP_JMP,
    OP_XOR,
    OP_SHR,
    OP_HALT,
};

/* locals: 0 = i, 1 = acc, 2 = n; acc += ((i * i) % 7) ^ (acc >> 3), for i from 0 to n - 1 */
static const uint64_t program[] = {
    OP_PUSH, 0, OP_STORE, 0,                            /*  0: i = 0 */
    OP_PUSH, 0, OP_STORE, 1,                            /*  4: acc = 0 */
    OP_LOAD, 0, OP_LOAD, 2, OP_LT, OP_JZ, 43,           /*  8: while (i < n) */
    OP_LOAD, 0, OP_LOAD, 0, OP_MUL, OP_PUSH, 7, OP_MOD, /* 15:   (i * i) % 7 */
    OP_LOAD, 1, OP_PUSH, 3, OP_SHR, OP_XOR,             /* 23:   ^ (acc >> 3) */
    OP_LOAD, 1, OP_ADD, OP_STORE, 1,                    /* 29:   acc += ... */
    OP_LOAD, 0, OP_PUSH, 1, OP_ADD, OP_STORE, 0,        /* 34:   i++ */
    OP_JMP, 8,                                          /* 41 */
    OP_HALT,                                            /* 43 */
};

static uint64_t run(uint64_t n)
{
    const uint64_t *code = program;
    uint64_t stack[32], locals[3] = {0, 0, n};
    int sp = 0;
    size_t pc = 0;
    for (;;) {
        switch (code[pc++]) {
        case OP_PUSH: stack[sp++] = code[pc++]; break;
        case OP_LOAD: stack[sp++] = locals[code[pc++]]; break;
        case OP_STORE: locals[code[pc++]] = stack[--sp]; break;
        case OP_ADD: sp--; stack[sp - 1] += stack[sp]; break;
        case OP_SUB: sp--; stack[sp - 1] -= stack[sp]; break;
        case OP_MUL: sp--; stack[sp - 1] *= stack[sp]; break;
        case OP_MOD: sp--; stack[sp - 1] %= stack[sp]; break;
        case OP_LT: sp--; stack[sp - 1] = stack[sp - 1] < stack[sp]; break;
        case OP_JZ: {
            size_t target = (size_t)code[pc++];
            if (!stack[--sp])
                pc = target;
            break;
        }
        case OP_JMP: pc = (size_t)code[pc]; break;
        case OP_XOR: sp--; stack[sp - 1] ^= stack[sp]; break;
        case OP_SHR: sp--; stack[sp - 1] >>= stack[sp]; break;
        case OP_HALT: return locals[1];
        default: rt_fail("vm opcode");
        }
    }
}

int main(int argc, char **argv)
{
    if (run(10) != 20)
        rt_fail("vm self-test");
    uint64_t n = rt_arg_u64(argc, argv, 1, 100000);
    rt_print("vm ");
    rt_print_u64(run(n));
    rt_print("\n");
    return 0;
}
