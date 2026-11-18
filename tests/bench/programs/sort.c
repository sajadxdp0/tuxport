/* Quicksort of 256K 32-bit integers: memory accesses and hard-to-predict branches.
 * Usage: sort [rounds] */
#include "rt.h"

#define COUNT (1 << 18)

static uint32_t values[COUNT];

static void insertion_sort(uint32_t *v, long lo, long hi)
{
    for (long i = lo + 1; i <= hi; i++) {
        uint32_t x = v[i];
        long j = i - 1;
        while (j >= lo && v[j] > x) {
            v[j + 1] = v[j];
            j--;
        }
        v[j + 1] = x;
    }
}

static void quicksort(uint32_t *v, long lo, long hi)
{
    while (hi - lo > 16) {
        long mid = lo + (hi - lo) / 2;
        uint32_t a = v[lo], b = v[mid], c = v[hi];
        uint32_t pivot = a < b ? (b < c ? b : (a < c ? c : a)) : (a < c ? a : (b < c ? c : b));
        long i = lo, j = hi;
        while (i <= j) {
            while (v[i] < pivot)
                i++;
            while (v[j] > pivot)
                j--;
            if (i <= j) {
                uint32_t t = v[i];
                v[i] = v[j];
                v[j] = t;
                i++;
                j--;
            }
        }
        /* Recurse into the shorter side first, keeping the stack depth O(log n) */
        if (j - lo < hi - i) {
            quicksort(v, lo, j);
            lo = i;
        } else {
            quicksort(v, i, hi);
            hi = j;
        }
    }
    insertion_sort(v, lo, hi);
}

int main(int argc, char **argv)
{
    uint64_t rounds = rt_arg_u64(argc, argv, 1, 4);
    uint64_t checksum = 0;
    for (uint64_t r = 0; r < rounds; r++) {
        uint32_t s = 2463534242u + (uint32_t)r;
        for (long i = 0; i < COUNT; i++) {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            values[i] = s;
        }
        quicksort(values, 0, COUNT - 1);
        for (long i = 1; i < COUNT; i++) {
            if (values[i - 1] > values[i])
                rt_fail("sort order");
        }
        for (long i = 0; i < COUNT; i += 997)
            checksum = checksum * 31 + values[i];
    }
    rt_print("sort ");
    rt_print_u64(checksum);
    rt_print("\n");
    return 0;
}
