//! Files that lie, and one that does not.

use super::*;

const ALLOWED: Range<u64> = 0x40_0000..0x1000_0000;

struct Load {
    flags: u32,
    address: u64,
    data: Vec<u8>,
    len: u64,
}

fn load(flags: u32, address: u64, data: &[u8], len: u64) -> Load {
    Load {
        flags,
        address,
        data: data.to_vec(),
        len,
    }
}

/// Lay out a file: header, program headers, then each segment's bytes.
fn build(entry: u64, loads: &[Load]) -> Vec<u8> {
    let mut file = vec![0u8; HEADER_LEN];
    file[..4].copy_from_slice(b"\x7fELF");
    file[4] = 2;
    file[5] = 1;
    file[6] = 1;
    file[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
    file[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
    file[24..32].copy_from_slice(&entry.to_le_bytes());
    file[32..40].copy_from_slice(&(HEADER_LEN as u64).to_le_bytes());
    file[54..56].copy_from_slice(&(PROGRAM_HEADER_LEN as u16).to_le_bytes());
    file[56..58].copy_from_slice(&(loads.len() as u16).to_le_bytes());

    let mut offset = (HEADER_LEN + loads.len() * PROGRAM_HEADER_LEN) as u64;
    for segment in loads {
        let mut header = [0u8; PROGRAM_HEADER_LEN];
        header[0..4].copy_from_slice(&PT_LOAD.to_le_bytes());
        header[4..8].copy_from_slice(&segment.flags.to_le_bytes());
        header[8..16].copy_from_slice(&offset.to_le_bytes());
        header[16..24].copy_from_slice(&segment.address.to_le_bytes());
        header[32..40].copy_from_slice(&(segment.data.len() as u64).to_le_bytes());
        header[40..48].copy_from_slice(&segment.len.to_le_bytes());
        file.extend_from_slice(&header);
        offset += segment.data.len() as u64;
    }
    for segment in loads {
        file.extend_from_slice(&segment.data);
    }
    file
}

/// Text, read-only data, and data with some `.bss` after it.
fn program() -> Vec<u8> {
    build(
        0x40_0010,
        &[
            load(PF_X | 4, 0x40_0000, &[0x90; 100], 100),
            load(4, 0x40_1000, b"hello", 5),
            load(PF_W | 4, 0x40_2000, &[1, 2, 3], 0x3000),
        ],
    )
}

/// Overwrite a field of program header `index`.
fn patch(file: &mut [u8], index: usize, field: usize, value: u64, width: usize) {
    let at = HEADER_LEN + index * PROGRAM_HEADER_LEN + field;
    file[at..at + width].copy_from_slice(&value.to_le_bytes()[..width]);
}

#[test]
fn a_real_program_is_described() {
    let file = program();
    let elf = Elf::parse(&file, ALLOWED).unwrap();
    assert_eq!(elf.entry(), 0x40_0010);

    let segments: Vec<Segment> = elf.segments().collect();
    assert_eq!(segments.len(), 3);
    assert!(segments[0].executable && !segments[0].writable);
    assert_eq!(segments[0].data, &[0x90; 100]);
    assert!(!segments[1].executable && !segments[1].writable);
    assert_eq!(segments[1].data, b"hello");

    // The file supplies three bytes of a segment three pages long: the rest
    // is the program's zeroed memory.
    assert!(segments[2].writable && !segments[2].executable);
    assert_eq!(segments[2].data, &[1, 2, 3]);
    assert_eq!(segments[2].len, 0x3000);
    assert_eq!(segments[2].pages(), 0x40_2000..0x40_5000);
}

#[test]
fn pages_round_outward() {
    let segment = Segment {
        address: 0x40_0ff0,
        len: 0x20,
        data: &[],
        writable: false,
        executable: false,
    };
    assert_eq!(segment.pages(), 0x40_0000..0x40_2000);
}

#[test]
fn things_that_are_not_programs_are_refused() {
    assert_eq!(Elf::parse(&[], ALLOWED).err(), Some(Error::Truncated));
    assert_eq!(Elf::parse(&[0u8; 64], ALLOWED).err(), Some(Error::NotElf));

    let mut file = program();
    file[4] = 1;
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::WrongMachine));

    let mut file = program();
    file[5] = 2;
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::WrongMachine));

    let mut file = program();
    file[18] = 183;
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::WrongMachine));
}

#[test]
fn anything_wanting_a_dynamic_linker_is_refused() {
    // A shared object, or a position-independent executable.
    let mut file = program();
    file[16] = 3;
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::NotStatic));

    for kind in [PT_DYNAMIC, PT_INTERP] {
        let mut file = program();
        patch(&mut file, 1, 0, kind as u64, 4);
        assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::NotStatic));
    }
}

#[test]
fn writable_and_executable_is_refused() {
    let file = build(
        0x40_0000,
        &[load(PF_X | PF_W | 4, 0x40_0000, &[0x90; 8], 8)],
    );
    assert_eq!(
        Elf::parse(&file, ALLOWED).err(),
        Some(Error::WritableAndExecutable)
    );
}

#[test]
fn a_segment_must_be_inside_the_file() {
    let mut file = program();
    // Claims more bytes than the file has.
    patch(&mut file, 0, 32, 1 << 20, 8);
    patch(&mut file, 0, 40, 1 << 20, 8);
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::OutsideFile));

    // An offset chosen so that offset + length wraps to something small.
    let mut file = program();
    patch(&mut file, 0, 8, u64::MAX - 10, 8);
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::OutsideFile));
}

#[test]
fn a_segment_must_be_inside_the_allowed_range() {
    // Below it: the null page.
    let file = build(0, &[load(PF_X | 4, 0, &[0x90; 8], 8)]);
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::OutsideRange));

    // In the kernel half.
    let high = 0xffff_ffff_8000_0000;
    let file = build(high, &[load(PF_X | 4, high, &[0x90; 8], 8)]);
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::OutsideRange));

    // Starting inside and running out of the top by its zeroed tail.
    let file = build(
        0x40_0000,
        &[
            load(PF_X | 4, 0x40_0000, &[0x90; 8], 8),
            load(PF_W | 4, 0x0fff_f000, &[0], 0x2000),
        ],
    );
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::OutsideRange));

    // An address and length that wrap past the top of the address space.
    let mut file = program();
    patch(&mut file, 2, 16, u64::MAX - 0x100, 8);
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::OutsideRange));
}

#[test]
fn memory_cannot_be_shorter_than_the_bytes_that_fill_it() {
    let mut file = program();
    patch(&mut file, 1, 40, 2, 8);
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::Malformed));
}

#[test]
fn two_segments_cannot_share_a_page() {
    // Text ends and data starts in the same 4 KiB.
    let file = build(
        0x40_0000,
        &[
            load(PF_X | 4, 0x40_0000, &[0x90; 0x800], 0x800),
            load(PF_W | 4, 0x40_0800, &[0; 16], 16),
        ],
    );
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::SharedPage));

    // Adjacent pages are fine.
    let file = build(
        0x40_0000,
        &[
            load(PF_X | 4, 0x40_0000, &[0x90; 0x800], 0x1000),
            load(PF_W | 4, 0x40_1000, &[0; 16], 16),
        ],
    );
    assert!(Elf::parse(&file, ALLOWED).is_ok());
}

#[test]
fn the_entry_point_must_be_code() {
    // In the data.
    let mut file = program();
    file[24..32].copy_from_slice(&0x40_2000u64.to_le_bytes());
    assert_eq!(
        Elf::parse(&file, ALLOWED).err(),
        Some(Error::EntryNotExecutable)
    );
    // One past the end of the text.
    let mut file = program();
    file[24..32].copy_from_slice(&(0x40_0000u64 + 100).to_le_bytes());
    assert_eq!(
        Elf::parse(&file, ALLOWED).err(),
        Some(Error::EntryNotExecutable)
    );
}

#[test]
fn a_program_needs_something_to_load_and_not_too_much() {
    let file = build(0x40_0000, &[]);
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::SegmentCount));

    let many: Vec<Load> = (0..MAX_SEGMENTS as u64 + 1)
        .map(|i| load(PF_X | 4, 0x40_0000 + i * 0x1000, &[0x90], 1))
        .collect();
    let file = build(0x40_0000, &many);
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::SegmentCount));

    let file = build(0x40_0000, &many[..MAX_SEGMENTS]);
    assert!(Elf::parse(&file, ALLOWED).is_ok());
}

#[test]
fn headers_past_the_end_of_the_file_are_a_short_file() {
    let mut file = program();
    file.truncate(HEADER_LEN + 10);
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::Truncated));

    // A table offset that wraps.
    let mut file = program();
    file[32..40].copy_from_slice(&(u64::MAX - 8).to_le_bytes());
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::Truncated));

    let mut file = program();
    file[54] = 32;
    assert_eq!(Elf::parse(&file, ALLOWED).err(), Some(Error::Malformed));
}

#[test]
fn no_prefix_of_a_good_file_panics() {
    // Every truncation is an error or a smaller program, never a crash.
    let file = program();
    for len in 0..file.len() {
        let _ = Elf::parse(&file[..len], ALLOWED);
    }
}

#[test]
fn no_single_corrupted_byte_panics() {
    let file = program();
    let headers = HEADER_LEN + 3 * PROGRAM_HEADER_LEN;
    for at in 0..headers {
        for value in [0x00, 0x01, 0x7f, 0x80, 0xff] {
            let mut bad = file.clone();
            bad[at] = value;
            if let Ok(elf) = Elf::parse(&bad, ALLOWED) {
                // Whatever survived still obeys every rule.
                for segment in elf.segments() {
                    assert!(segment.address >= ALLOWED.start);
                    assert!(segment.address + segment.len <= ALLOWED.end);
                    assert!(!(segment.writable && segment.executable));
                    assert!(segment.data.len() as u64 <= segment.len);
                }
            }
        }
    }
}
