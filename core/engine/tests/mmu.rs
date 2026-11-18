//! Unit tests for the soft-MMU page table (mmu.c): the operations on-demand filling uses.

use std::ptr::NonNull;

use tuxport_engine::{Mmu, PAGE_SIZE, prot};

#[repr(C, align(4096))]
struct Page([u8; 4096]);

const RW: u32 = prot::READ | prot::WRITE;
/// The range one leaf table covers.
const LEAF: u64 = PAGE_SIZE << 9;

#[test]
fn fill_only_adds_missing_entries() {
    let mut mem = pages(4);
    let mmu = Mmu::new();
    let a = 0x10_0000;
    // Page 1 is first mapped elsewhere, and page 2 keeps an entry with permission 0 (as left by
    // copy-on-write or PROT_NONE)
    unsafe {
        mmu.map_shared(a + PAGE_SIZE, host(&mut mem, 3), PAGE_SIZE, prot::READ)
            .unwrap();
        mmu.map_shared(a + 2 * PAGE_SIZE, host(&mut mem, 2), PAGE_SIZE, 0)
            .unwrap();
        mmu.fill_shared(a, host(&mut mem, 0), 3 * PAGE_SIZE, RW)
            .unwrap();
    }
    assert_eq!(mmu.translate(a, prot::WRITE), Some(host(&mut mem, 0)));
    assert_eq!(mmu.translate(a + PAGE_SIZE, 0), Some(host(&mut mem, 3)));
    assert!(mmu.translate(a + PAGE_SIZE, prot::WRITE).is_none());
    // An entry with permission 0 cannot be accessed and does not count as mapped even when 0 is
    // passed; restoring a permission makes it usable again
    assert!(mmu.translate(a + 2 * PAGE_SIZE, 0).is_none());
    assert_eq!(
        mmu.next_present(a + 2 * PAGE_SIZE, a + LEAF),
        a + 2 * PAGE_SIZE
    );
    mmu.protect_shared(a + 2 * PAGE_SIZE, PAGE_SIZE, RW)
        .unwrap();
    assert_eq!(
        mmu.translate(a + 2 * PAGE_SIZE, RW),
        Some(host(&mut mem, 2))
    );
}

#[test]
fn ranges_skip_missing_tables_at_every_level() {
    let mut mem = pages(2);
    let mut mmu = Mmu::new();
    let top = 1u64 << 48;
    let (lo, hi) = (0x7000_0000, top - 3 * LEAF);
    unsafe {
        mmu.map(lo, host(&mut mem, 0), PAGE_SIZE, RW).unwrap();
        mmu.map(hi, host(&mut mem, 1), PAGE_SIZE, prot::READ)
            .unwrap();
    }
    // Only two pages in the whole address space: lookup, unmap and protect all skip the places
    // without tables
    assert_eq!(mmu.next_present(0, top), lo);
    assert_eq!(mmu.next_present(lo + PAGE_SIZE, top), hi);
    assert_eq!(mmu.next_present(hi + PAGE_SIZE, top), top);
    mmu.protect(0, top, prot::READ).unwrap();
    assert!(mmu.translate(lo, prot::WRITE).is_none());
    assert!(mmu.translate(lo, prot::READ).is_some());
    mmu.unmap(PAGE_SIZE, top - PAGE_SIZE).unwrap();
    assert_eq!(mmu.next_present(0, top), top);
}

#[test]
fn move_carries_entries_and_their_permissions() {
    let mut mem = pages(3);
    let mut mmu = Mmu::new();
    let (from, to) = (LEAF - PAGE_SIZE, 0x40_0000_0000);
    unsafe {
        mmu.map(from, host(&mut mem, 0), PAGE_SIZE, RW).unwrap();
        // Crossing leaf tables; the middle page has no entry, and the third page has permission 0
        mmu.map(from + 2 * PAGE_SIZE, host(&mut mem, 1), PAGE_SIZE, 0)
            .unwrap();
    }
    mmu.move_entries(from, to, 3 * PAGE_SIZE).unwrap();
    assert_eq!(
        mmu.next_present(from, from + 3 * PAGE_SIZE),
        from + 3 * PAGE_SIZE
    );
    assert_eq!(mmu.translate(to, prot::WRITE), Some(host(&mut mem, 0)));
    assert_eq!(
        mmu.next_present(to + PAGE_SIZE, to + 3 * PAGE_SIZE),
        to + 2 * PAGE_SIZE
    );
    assert!(mmu.translate(to + 2 * PAGE_SIZE, 0).is_none());
    // Two overlapping ranges are not allowed
    assert!(mmu.move_entries(to, to + PAGE_SIZE, 2 * PAGE_SIZE).is_err());
}

fn pages(n: usize) -> Box<[Page]> {
    (0..n).map(|_| Page([0; 4096])).collect()
}

fn host(p: &mut [Page], i: usize) -> NonNull<u8> {
    NonNull::new(p[i..].as_mut_ptr().cast::<u8>()).unwrap()
}
