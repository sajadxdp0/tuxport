/*
 * soft-MMU page table (the kernel/mmu map): a 4-level radix tree with 512 entries per level,
 * covering a 48-bit guest address space. Intermediate entries point to the next level's table;
 * leaf entries are a host page address | permission bits. The tree is walked only on a TLB miss.
 *
 * Entries are built on demand: the kernel layer builds them in advance only for small mappings,
 * and large ones get a whole leaf table filled at the guest's first access (tuxport_mmu_fill), so
 * the page table grows with the memory touched, not with the mapping size. A walk over a range
 * skips a missing intermediate table by the whole span of the missing level (one entry covers
 * 2 MB, 1 GB or 512 GB).
 *
 * Concurrency: only the kernel layer modifies the table, while holding the BKL, but other threads
 * sharing the address space may walk it at the same time (TLB misses). So entries are always read
 * and written atomically: new intermediate tables are linked in with release semantics and read
 * with acquire semantics while walking. Intermediate tables are not freed before the mmu is
 * destroyed.
 */
#include <stdbool.h>
#include <stdlib.h>

#include "internal.h"

#define LEVELS MMU_LEVELS
#define LEVEL_BITS MMU_LEVEL_BITS
#define LEVEL_ENTRIES (1u << LEVEL_BITS)
/* Bytes covered by one leaf table */
#define LEAF_SPAN ((uint64_t)TUXPORT_PAGE_SIZE << LEVEL_BITS)

static unsigned level_shift(int level)
{
    return TUXPORT_PAGE_SHIFT + LEVEL_BITS * (LEVELS - 1 - level);
}

static unsigned level_index(uint64_t gva, int level)
{
    return (unsigned)(gva >> level_shift(level)) & (LEVEL_ENTRIES - 1);
}

static uintptr_t *new_table(void)
{
    return calloc(LEVEL_ENTRIES, sizeof(uintptr_t));
}

static uintptr_t load_entry(const uintptr_t *slot)
{
    return __atomic_load_n(slot, __ATOMIC_ACQUIRE);
}

static void store_entry(uintptr_t *slot, uintptr_t v)
{
    __atomic_store_n(slot, v, __ATOMIC_RELEASE);
}

static void free_table(uintptr_t *table, int level)
{
    if (level < LEVELS - 1) {
        for (unsigned i = 0; i < LEVEL_ENTRIES; i++) {
            if (table[i])
                free_table((uintptr_t *)table[i], level + 1);
        }
    }
    free(table);
}

/*
 * The leaf table containing gva. If an intermediate table is missing: create it when alloc is
 * true; otherwise return NULL and set *hole_end to the end of the range from gva that has no
 * entries at all (aligned to the missing level, at most 2^48). With alloc, a failed allocation
 * also returns NULL
 */
static uintptr_t *leaf_table(const struct tuxport_mmu *mmu, uint64_t gva, bool alloc,
                             uint64_t *hole_end)
{
    uintptr_t *table = mmu->root;
    for (int level = 0; level < LEVELS - 1; level++) {
        uintptr_t *slot = &table[level_index(gva, level)];
        uintptr_t next = load_entry(slot);
        if (!next) {
            if (!alloc) {
                unsigned shift = level_shift(level);
                *hole_end = ((gva >> shift) + 1) << shift;
                return NULL;
            }
            uintptr_t *fresh = new_table();
            if (!fresh)
                return NULL;
            next = (uintptr_t)fresh;
            store_entry(slot, next);
        }
        table = (uintptr_t *)next;
    }
    return table;
}

/* End of the range covered by gva's leaf table, at most end */
static uint64_t leaf_end(uint64_t gva, uint64_t end)
{
    uint64_t e = (gva | (LEAF_SPAN - 1)) + 1;
    return e < end ? e : end;
}

static bool range_ok(uint64_t gva, uint64_t len)
{
    uint64_t limit = UINT64_C(1) << TUXPORT_VA_BITS;
    return (gva & PAGE_MASK) == 0 && (len & PAGE_MASK) == 0 && gva <= limit && len <= limit - gva;
}

struct tuxport_mmu *tuxport_mmu_new(void)
{
    struct tuxport_mmu *mmu = calloc(1, sizeof(*mmu));
    if (!mmu)
        return NULL;
    mmu->root = new_table();
    if (!mmu->root) {
        free(mmu);
        return NULL;
    }
    return mmu;
}

void tuxport_mmu_free(struct tuxport_mmu *mmu)
{
    if (!mmu)
        return;
    free_table(mmu->root, 0);
    free(mmu);
}

/* map and fill: with only_empty, existing entries (including ones with permission 0) stay as they
 * are */
static int store_range(struct tuxport_mmu *mmu, uint64_t gva, void *host, uint64_t len,
                       uint32_t prot, bool only_empty)
{
    uintptr_t h = (uintptr_t)host;
    if (!range_ok(gva, len) || (h & PAGE_MASK) || !h || (prot & ~PTE_PROT_MASK))
        return TUXPORT_EINVAL;
    uint64_t end = gva + len;
    for (uint64_t va = gva; va < end;) {
        uintptr_t *table = leaf_table(mmu, va, true, NULL);
        if (!table)
            return TUXPORT_ENOMEM;
        for (uint64_t stop = leaf_end(va, end); va < stop; va += TUXPORT_PAGE_SIZE) {
            uintptr_t *slot = &table[level_index(va, LEVELS - 1)];
            if (!only_empty || !load_entry(slot))
                store_entry(slot, (h + (va - gva)) | prot);
        }
    }
    return TUXPORT_OK;
}

int tuxport_mmu_map(struct tuxport_mmu *mmu, uint64_t gva, void *host, uint64_t len, uint32_t prot)
{
    return store_range(mmu, gva, host, len, prot, false);
}

int tuxport_mmu_fill(struct tuxport_mmu *mmu, uint64_t gva, void *host, uint64_t len, uint32_t prot)
{
    return store_range(mmu, gva, host, len, prot, true);
}

int tuxport_mmu_unmap(struct tuxport_mmu *mmu, uint64_t gva, uint64_t len)
{
    if (!range_ok(gva, len))
        return TUXPORT_EINVAL;
    uint64_t end = gva + len;
    for (uint64_t va = gva; va < end;) {
        uintptr_t *table = leaf_table(mmu, va, false, &va);
        if (!table)
            continue;
        for (uint64_t stop = leaf_end(va, end); va < stop; va += TUXPORT_PAGE_SIZE)
            store_entry(&table[level_index(va, LEVELS - 1)], 0);
    }
    return TUXPORT_OK;
}

int tuxport_mmu_protect(struct tuxport_mmu *mmu, uint64_t gva, uint64_t len, uint32_t prot)
{
    if (!range_ok(gva, len) || (prot & ~PTE_PROT_MASK))
        return TUXPORT_EINVAL;
    uint64_t end = gva + len;
    for (uint64_t va = gva; va < end;) {
        uintptr_t *table = leaf_table(mmu, va, false, &va);
        if (!table)
            continue;
        for (uint64_t stop = leaf_end(va, end); va < stop; va += TUXPORT_PAGE_SIZE) {
            uintptr_t *slot = &table[level_index(va, LEVELS - 1)];
            uintptr_t old = load_entry(slot);
            if (old)
                store_entry(slot, (old & ~(uintptr_t)PTE_PROT_MASK) | prot);
        }
    }
    return TUXPORT_OK;
}

int tuxport_mmu_move(struct tuxport_mmu *mmu, uint64_t from, uint64_t to, uint64_t len)
{
    if (!range_ok(from, len) || !range_ok(to, len) || (from < to + len && to < from + len))
        return TUXPORT_EINVAL;
    uint64_t end = from + len;
    for (uint64_t va = from; va < end;) {
        uintptr_t *table = leaf_table(mmu, va, false, &va);
        if (!table)
            continue;
        for (uint64_t stop = leaf_end(va, end); va < stop; va += TUXPORT_PAGE_SIZE) {
            uintptr_t *slot = &table[level_index(va, LEVELS - 1)];
            uintptr_t v = load_entry(slot);
            if (!v)
                continue;
            uint64_t dst = to + (va - from), unused;
            uintptr_t *target = leaf_table(mmu, dst, true, &unused);
            if (!target)
                return TUXPORT_ENOMEM;
            store_entry(&target[level_index(dst, LEVELS - 1)], v);
            store_entry(slot, 0);
        }
    }
    return TUXPORT_OK;
}

uint64_t tuxport_mmu_next_present(const struct tuxport_mmu *mmu, uint64_t gva, uint64_t end)
{
    uint64_t limit = UINT64_C(1) << TUXPORT_VA_BITS;
    if (end > limit)
        end = limit;
    for (uint64_t va = gva & ~(uint64_t)PAGE_MASK; va < end;) {
        uintptr_t *table = leaf_table(mmu, va, false, &va);
        if (!table)
            continue;
        for (uint64_t stop = leaf_end(va, end); va < stop; va += TUXPORT_PAGE_SIZE) {
            if (load_entry(&table[level_index(va, LEVELS - 1)]))
                return va;
        }
    }
    return end;
}

uintptr_t tuxport_mmu_lookup_pte(const struct tuxport_mmu *mmu, uint64_t gva)
{
    if (!mmu || (gva >> TUXPORT_VA_BITS))
        return 0;
    const uintptr_t *table = mmu->root;
    for (int level = 0; level < LEVELS - 1; level++) {
        uintptr_t next = load_entry(&table[level_index(gva, level)]);
        if (!next)
            return 0;
        table = (const uintptr_t *)next;
    }
    return load_entry(&table[level_index(gva, LEVELS - 1)]);
}

void *tuxport_mmu_translate(const struct tuxport_mmu *mmu, uint64_t gva, uint32_t required_prot)
{
    uintptr_t pte = tuxport_mmu_lookup_pte(mmu, gva);
    /* An entry with permission 0 (a PROT_NONE page keeping its host page) cannot be accessed, and
     * does not count as mapped even when 0 is passed */
    if (!(pte & PTE_PROT_MASK) || (pte & required_prot) != required_prot)
        return NULL;
    return (void *)((pte & ~(uintptr_t)PAGE_MASK) + (gva & PAGE_MASK));
}
