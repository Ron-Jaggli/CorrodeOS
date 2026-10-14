//! The split virtqueue (2.7).
//!
//! Three arrays in memory the device can reach: descriptors, each naming a
//! buffer by physical address; the *available* ring, where the driver puts
//! the first descriptor of each request; and the *used* ring, where the
//! device puts it back when it is done, with how many bytes it wrote.
//!
//! The device writes the used ring while the driver is running, so the
//! queue never holds a Rust reference to that memory. Every access is a
//! volatile read or write through a pointer, and the two places ordering
//! matters -- the descriptors are written before the ring index that
//! publishes them, and the used index is read before the entries it covers
//! -- carry fences that say so.
//!
//! All three arrays are placed in one run of memory by [`Layout`], small
//! enough at the sizes used here to sit in one page. That matters because
//! the memory a driver is given need not be physically contiguous past a
//! page.

use core::sync::atomic::{fence, Ordering};

/// The descriptor continues in the one named by `next`.
pub const DESC_NEXT: u16 = 1;
/// The device writes this buffer rather than reading it.
pub const DESC_WRITE: u16 = 2;

const DESC_LEN: usize = 16;

/// Most descriptors a queue here is built with. The device may offer more;
/// the driver may always ask for fewer.
pub const MAX_SIZE: u16 = 64;

/// Where the three arrays go, as offsets from the start of the queue's
/// memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub size: u16,
    pub desc: usize,
    pub avail: usize,
    pub used: usize,
    /// Bytes from the start of the descriptors to the end of the used ring.
    pub len: usize,
}

impl Layout {
    /// Lay out a queue of `size` descriptors, `size` a power of two no more
    /// than [`MAX_SIZE`]. Alignments are the specification's: 16 for the
    /// descriptors, 2 for the available ring, 4 for the used ring.
    pub const fn new(size: u16) -> Option<Layout> {
        if size == 0 || size > MAX_SIZE || !size.is_power_of_two() {
            return None;
        }
        let n = size as usize;
        let desc = 0;
        let avail = desc + DESC_LEN * n;
        // flags, idx, ring[n], used_event
        let avail_end = avail + 4 + 2 * n + 2;
        let used = (avail_end + 3) & !3;
        // flags, idx, ring[n] of (id, len), avail_event
        let len = used + 4 + 8 * n + 2;
        Some(Layout {
            size,
            desc,
            avail,
            used,
            len,
        })
    }
}

/// One buffer of a request: a physical address and a length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buffer {
    pub address: u64,
    pub len: u32,
    /// The device writes it (a response) rather than reading it (a request).
    pub device_writes: bool,
}

/// A request the device has finished with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Used {
    /// The token [`Queue::submit`] returned for it.
    pub token: u16,
    /// Bytes the device wrote into its writable buffers.
    pub written: u32,
}

/// The driver's side of one split virtqueue.
pub struct Queue {
    base: *mut u8,
    layout: Layout,
    /// Descriptors not in any request, as a stack.
    free: [u16; MAX_SIZE as usize],
    free_len: usize,
    /// How long each submitted chain is, by its head, so it can be freed.
    chain_len: [u8; MAX_SIZE as usize],
    /// Our next available-ring index, and the next used one to look at.
    next_avail: u16,
    next_used: u16,
}

impl Queue {
    /// A queue over `layout.len` bytes at `base`, all of them zeroed here.
    ///
    /// # Safety
    /// `base` must be valid for reads and writes of `layout.len` bytes for
    /// as long as the queue is used, 16-byte aligned, and shared with
    /// nothing but the device this queue is given to.
    pub unsafe fn new(base: *mut u8, layout: Layout) -> Queue {
        for i in 0..layout.len {
            // SAFETY: inside the range the caller vouched for.
            unsafe { base.add(i).write_volatile(0) };
        }
        let mut free = [0u16; MAX_SIZE as usize];
        for (i, slot) in free.iter_mut().enumerate().take(layout.size as usize) {
            // Popped from the end, so descriptor 0 goes first.
            *slot = layout.size - 1 - i as u16;
        }
        Queue {
            base,
            layout,
            free,
            free_len: layout.size as usize,
            chain_len: [0; MAX_SIZE as usize],
            next_avail: 0,
            next_used: 0,
        }
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Descriptors not tied up in a request.
    pub fn free(&self) -> usize {
        self.free_len
    }

    fn write16(&self, offset: usize, value: u16) {
        // SAFETY: every caller passes an offset inside `layout.len`, which
        // `new` was promised is ours; alignment follows from the layout.
        unsafe {
            self.base
                .add(offset)
                .cast::<u16>()
                .write_volatile(value.to_le())
        }
    }

    fn read16(&self, offset: usize) -> u16 {
        // SAFETY: as `write16`.
        u16::from_le(unsafe { self.base.add(offset).cast::<u16>().read_volatile() })
    }

    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: as `write16`.
        u32::from_le(unsafe { self.base.add(offset).cast::<u32>().read_volatile() })
    }

    fn write_desc(&self, index: u16, buffer: &Buffer, flags: u16, next: u16) {
        let at = self.layout.desc + DESC_LEN * index as usize;
        // SAFETY: `index` is below `size`, so the sixteen bytes are inside
        // the descriptor table.
        unsafe {
            self.base
                .add(at)
                .cast::<u64>()
                .write_volatile(buffer.address.to_le());
            self.base
                .add(at + 8)
                .cast::<u32>()
                .write_volatile(buffer.len.to_le());
        }
        self.write16(at + 12, flags);
        self.write16(at + 14, next);
    }

    /// Offer a request to the device: `chain` in order, the buffers it reads
    /// first and the ones it writes after. Returns a token naming it, or
    /// `None` if there are not enough free descriptors.
    ///
    /// The device is not told; that is the caller's doorbell to ring.
    pub fn submit(&mut self, chain: &[Buffer]) -> Option<u16> {
        if chain.is_empty() || chain.len() > self.free_len {
            return None;
        }
        // Take the descriptors, last first, so each can name the next.
        let mut next = 0u16;
        for (position, buffer) in chain.iter().enumerate().rev() {
            self.free_len -= 1;
            let index = self.free[self.free_len];
            let mut flags = if buffer.device_writes { DESC_WRITE } else { 0 };
            if position + 1 < chain.len() {
                flags |= DESC_NEXT;
            }
            self.write_desc(index, buffer, flags, next);
            next = index;
        }
        let head = next;
        self.chain_len[head as usize] = chain.len() as u8;

        let slot = self.next_avail % self.layout.size;
        self.write16(self.layout.avail + 4 + 2 * slot as usize, head);
        // The descriptors and the ring entry must be visible before the
        // index that says they are there.
        fence(Ordering::Release);
        self.next_avail = self.next_avail.wrapping_add(1);
        self.write16(self.layout.avail + 2, self.next_avail);
        // And the index before whatever doorbell the caller rings next.
        fence(Ordering::SeqCst);
        Some(head)
    }

    /// Take one request the device has finished with, if there is one, and
    /// put its descriptors back on the free list.
    pub fn take(&mut self) -> Option<Used> {
        let published = self.read16(self.layout.used + 2);
        if published == self.next_used {
            return None;
        }
        // The entries the index covers were written before it.
        fence(Ordering::Acquire);
        let slot = self.next_used % self.layout.size;
        let at = self.layout.used + 4 + 8 * slot as usize;
        let id = self.read32(at);
        let written = self.read32(at + 4);
        self.next_used = self.next_used.wrapping_add(1);

        // A device that names a descriptor it was never given is not to be
        // believed about anything; drop the entry rather than free nonsense.
        let head = id as u16;
        if id >= self.layout.size as u32 || self.chain_len[head as usize] == 0 {
            return None;
        }
        let mut index = head;
        for _ in 0..self.chain_len[head as usize] {
            self.free[self.free_len] = index;
            self.free_len += 1;
            let at = self.layout.desc + DESC_LEN * index as usize;
            index = self.read16(at + 14);
        }
        self.chain_len[head as usize] = 0;
        Some(Used {
            token: head,
            written,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plays the device: reads what the driver offered and answers it, by
    /// the same layout, from the other side.
    struct Device<'a> {
        memory: &'a mut [u8],
        layout: Layout,
        seen: u16,
        used: u16,
    }

    impl Device<'_> {
        fn u16_at(&self, at: usize) -> u16 {
            u16::from_le_bytes([self.memory[at], self.memory[at + 1]])
        }

        /// The next request, as `(head, [(address, len, flags)])`.
        fn next(&mut self) -> Option<(u16, Vec<(u64, u32, u16)>)> {
            let idx = self.u16_at(self.layout.avail + 2);
            if idx == self.seen {
                return None;
            }
            let slot = self.seen % self.layout.size;
            self.seen = self.seen.wrapping_add(1);
            let head = self.u16_at(self.layout.avail + 4 + 2 * slot as usize);
            let mut chain = Vec::new();
            let mut index = head;
            loop {
                let at = self.layout.desc + 16 * index as usize;
                let address = u64::from_le_bytes(self.memory[at..at + 8].try_into().unwrap());
                let len = u32::from_le_bytes(self.memory[at + 8..at + 12].try_into().unwrap());
                let flags = self.u16_at(at + 12);
                chain.push((address, len, flags));
                if flags & DESC_NEXT == 0 {
                    break;
                }
                index = self.u16_at(at + 14);
            }
            Some((head, chain))
        }

        fn finish(&mut self, head: u16, written: u32) {
            let slot = self.used % self.layout.size;
            let at = self.layout.used + 4 + 8 * slot as usize;
            self.memory[at..at + 4].copy_from_slice(&(head as u32).to_le_bytes());
            self.memory[at + 4..at + 8].copy_from_slice(&written.to_le_bytes());
            self.used = self.used.wrapping_add(1);
            let idx = self.layout.used + 2;
            self.memory[idx..idx + 2].copy_from_slice(&self.used.to_le_bytes());
        }
    }

    fn buffer(address: u64, len: u32, device_writes: bool) -> Buffer {
        Buffer {
            address,
            len,
            device_writes,
        }
    }

    #[test]
    fn layouts_follow_the_alignment_rules() {
        let layout = Layout::new(64).unwrap();
        assert_eq!(layout.desc, 0);
        assert_eq!(layout.avail, 1024);
        // 1024 + 4 + 128 + 2 = 1158, rounded up to 4.
        assert_eq!(layout.used, 1160);
        assert_eq!(layout.len, 1160 + 4 + 512 + 2);
        assert!(layout.len <= 4096);
        assert_eq!(Layout::new(0), None);
        assert_eq!(Layout::new(48), None);
        assert_eq!(Layout::new(128), None);
        let small = Layout::new(2).unwrap();
        assert_eq!(small.used % 4, 0);
    }

    #[test]
    fn a_request_reaches_the_device_and_comes_back() {
        let layout = Layout::new(8).unwrap();
        let mut memory = vec![0xaau8; layout.len];
        // SAFETY: the vector is exactly `layout.len` bytes and outlives the
        // queue, and nothing else touches it while the queue does.
        let mut queue = unsafe { Queue::new(memory.as_mut_ptr(), layout) };
        assert_eq!(queue.free(), 8);

        let token = queue
            .submit(&[buffer(0x1000, 24, false), buffer(0x2000, 408, true)])
            .unwrap();
        assert_eq!(queue.free(), 6);
        assert_eq!(queue.take(), None);

        let mut device = Device {
            memory: &mut memory,
            layout,
            seen: 0,
            used: 0,
        };
        let (head, chain) = device.next().unwrap();
        assert_eq!(head, token);
        assert_eq!(
            chain,
            vec![(0x1000, 24, DESC_NEXT), (0x2000, 408, DESC_WRITE)]
        );
        assert!(device.next().is_none());
        device.finish(head, 408);

        assert_eq!(
            queue.take(),
            Some(Used {
                token,
                written: 408
            })
        );
        assert_eq!(queue.free(), 8);
        assert_eq!(queue.take(), None);
    }

    #[test]
    fn indices_wrap_and_descriptors_are_reused() {
        let layout = Layout::new(4).unwrap();
        let mut memory = vec![0u8; layout.len];
        let base = memory.as_mut_ptr();
        // SAFETY: as above.
        let mut queue = unsafe { Queue::new(base, layout) };
        let mut device = Device {
            // SAFETY: the test plays both sides in turn, never at once; the
            // second view stands in for a device on the far side of a bus.
            memory: unsafe { core::slice::from_raw_parts_mut(base, layout.len) },
            layout,
            seen: 0,
            used: 0,
        };
        // Far more requests than descriptors, and than 2^16 / 1000 laps of
        // the ring, so both the ring index and the 16-bit counters wrap.
        for round in 0..70_000u32 {
            let token = queue
                .submit(&[
                    buffer(round as u64, 1, false),
                    buffer(1, 2, false),
                    buffer(2, 3, true),
                ])
                .unwrap();
            let (head, chain) = device.next().unwrap();
            assert_eq!(head, token);
            assert_eq!(chain[0].0, round as u64);
            device.finish(head, 3);
            assert_eq!(queue.take().map(|used| used.token), Some(token));
            assert_eq!(queue.free(), 4);
        }
    }

    #[test]
    fn too_long_a_chain_is_refused_whole() {
        let layout = Layout::new(2).unwrap();
        let mut memory = vec![0u8; layout.len];
        // SAFETY: as above.
        let mut queue = unsafe { Queue::new(memory.as_mut_ptr(), layout) };
        let three = [buffer(0, 1, false); 3];
        assert_eq!(queue.submit(&three), None);
        assert_eq!(queue.free(), 2);
        assert_eq!(queue.submit(&[]), None);
    }

    #[test]
    fn a_device_naming_a_descriptor_it_was_not_given_is_ignored() {
        let layout = Layout::new(4).unwrap();
        let mut memory = vec![0u8; layout.len];
        let base = memory.as_mut_ptr();
        // SAFETY: as above.
        let mut queue = unsafe { Queue::new(base, layout) };
        let token = queue.submit(&[buffer(0, 1, true)]).unwrap();
        let mut device = Device {
            // SAFETY: as above.
            memory: unsafe { core::slice::from_raw_parts_mut(base, layout.len) },
            layout,
            seen: 0,
            used: 0,
        };
        device.next().unwrap();
        // Out of range, then never submitted.
        device.finish(200, 0);
        device.finish((token + 1) % 4, 0);
        device.finish(token, 0);
        assert_eq!(queue.take(), None);
        assert_eq!(queue.take(), None);
        assert_eq!(queue.take().map(|used| used.token), Some(token));
        assert_eq!(queue.free(), 4);
    }
}
