use super::*;

use std::alloc::Layout;

/// Memory to hand the heap, aligned generously.
fn arena(bytes: usize) -> Vec<u128> {
    vec![0u128; bytes / 16]
}

#[test]
fn everything_freed_is_one_block_again() {
    let mut memory = arena(64 * 1024);
    let mut heap = Heap::empty();
    // SAFETY: the arena is ours, writable, and outlives the heap's use.
    unsafe { heap.init(memory.as_mut_ptr() as usize, 64 * 1024) };
    let total = heap.total();
    assert_eq!(heap.block_count(), 1);

    let layout = Layout::from_size_align(24, 8).unwrap();
    let blocks: Vec<*mut u8> = (0..100).map(|_| unsafe { heap.alloc(layout) }).collect();
    assert!(blocks.iter().all(|block| !block.is_null()));
    // Freed in an order that leaves holes until the very end.
    for block in blocks
        .iter()
        .step_by(2)
        .chain(blocks.iter().skip(1).step_by(2))
    {
        unsafe { heap.dealloc(*block, layout) };
    }
    assert_eq!(heap.block_count(), 1);
    assert_eq!(heap.free(), total);
}

#[test]
fn a_request_too_big_is_null_and_harmless() {
    let mut memory = arena(4096);
    let mut heap = Heap::empty();
    unsafe { heap.init(memory.as_mut_ptr() as usize, 4096) };
    let huge = Layout::from_size_align(1 << 20, 16).unwrap();
    assert!(unsafe { heap.alloc(huge) }.is_null());
    // As large as a layout may be: the sums inside must not wrap.
    let wrap = Layout::from_size_align(isize::MAX as usize - 8192, 4096).unwrap();
    assert!(unsafe { heap.alloc(wrap) }.is_null());
    assert_eq!(heap.free(), heap.total());
}

#[test]
fn regions_added_later_are_used_and_kept_apart() {
    let (mut first, mut second) = (arena(1024), arena(1024));
    let mut heap = Heap::empty();
    unsafe {
        heap.init(first.as_mut_ptr() as usize, 1024);
        heap.add_region(second.as_mut_ptr() as usize, 1024);
    }
    assert_eq!(heap.total(), 2048);
    // More than either region alone holds, in pieces that fit in each.
    let layout = Layout::from_size_align(512, 16).unwrap();
    let blocks: Vec<*mut u8> = (0..4).map(|_| unsafe { heap.alloc(layout) }).collect();
    assert!(blocks.iter().all(|block| !block.is_null()));
    assert!(unsafe { heap.alloc(layout) }.is_null());
    for block in blocks {
        unsafe { heap.dealloc(block, layout) };
    }
    // Two regions that do not touch stay two blocks.
    assert_eq!(heap.block_count(), 2);
}

/// Random allocations and frees against a model of which bytes are taken:
/// nothing handed out overlaps anything else live, everything is aligned as
/// asked and inside the arena, and freeing it all coalesces to one block.
#[test]
fn random_use_never_overlaps_and_always_coalesces() {
    const SIZE: usize = 256 * 1024;
    let mut memory = arena(SIZE);
    let base = memory.as_mut_ptr() as usize;
    let mut heap = Heap::empty();
    unsafe { heap.init(base, SIZE) };
    let total = heap.total();

    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut live: Vec<(usize, Layout)> = Vec::new();
    let mut taken = vec![false; SIZE];

    for _ in 0..20_000 {
        if live.is_empty() || next() % 3 != 0 {
            let size = 1 + (next() % 700) as usize;
            let align = 1 << (next() % 8);
            let layout = Layout::from_size_align(size, align).unwrap();
            let block = unsafe { heap.alloc(layout) } as usize;
            if block == 0 {
                continue;
            }
            assert_eq!(block % align, 0);
            assert!(block >= base && block + size <= base + SIZE);
            for byte in &mut taken[block - base..block - base + size] {
                assert!(!*byte, "handed out a byte already in use");
                *byte = true;
            }
            live.push((block, layout));
        } else {
            let (block, layout) = live.swap_remove((next() % live.len() as u64) as usize);
            for byte in &mut taken[block - base..block - base + layout.size()] {
                *byte = false;
            }
            unsafe { heap.dealloc(block as *mut u8, layout) };
        }
    }
    for (block, layout) in live.drain(..) {
        unsafe { heap.dealloc(block as *mut u8, layout) };
    }
    assert_eq!(heap.block_count(), 1);
    assert_eq!(heap.free(), total);
}
