//! Files that lie, and one that does not.

use super::*;

const ALLOWED: Range<u64> = 0x40_0000..0x1000_0000;
const BASE: u64 = 0x40_0000;

struct Section {
    rva: u32,
    virtual_len: u32,
    raw: Vec<u8>,
    flags: u32,
}

const TEXT: u32 = 0x6000_0020;
const RDATA: u32 = 0x4000_0040;
const DATA: u32 = 0xc000_0040;

/// Lay out a PE32+ executable: DOS header, PE header, optional header with
/// sixteen empty directories, section table, then each section's bytes at
/// the next 512-byte boundary. The same layout `scripts/hello-exe.sh` writes.
fn build(entry: u32, image_len: u32, sections: &[Section]) -> Vec<u8> {
    let mut file = vec![0u8; 0x200];
    file[..2].copy_from_slice(b"MZ");
    file[LFANEW..LFANEW + 4].copy_from_slice(&0x40u32.to_le_bytes());
    file[0x40..0x44].copy_from_slice(b"PE\0\0");
    let coff = 0x44;
    file[coff..coff + 2].copy_from_slice(&MACHINE_AMD64.to_le_bytes());
    file[coff + 2..coff + 4].copy_from_slice(&(sections.len() as u16).to_le_bytes());
    file[coff + 16..coff + 18].copy_from_slice(&240u16.to_le_bytes());
    // An executable, large-address aware, its relocations stripped.
    file[coff + 18..coff + 20].copy_from_slice(&0x0023u16.to_le_bytes());
    let optional = coff + COFF_LEN;
    let put32 = |file: &mut Vec<u8>, at: usize, value: u32| {
        file[optional + at..optional + at + 4].copy_from_slice(&value.to_le_bytes())
    };
    file[optional..optional + 2].copy_from_slice(&PE32_PLUS.to_le_bytes());
    put32(&mut file, ENTRY, entry);
    file[optional + IMAGE_BASE..optional + IMAGE_BASE + 8].copy_from_slice(&BASE.to_le_bytes());
    put32(&mut file, SECTION_ALIGNMENT, 0x1000);
    put32(&mut file, FILE_ALIGNMENT, 0x200);
    put32(&mut file, SIZE_OF_IMAGE, image_len);
    put32(&mut file, SIZE_OF_HEADERS, 0x200);
    // The console subsystem.
    file[optional + 68..optional + 70].copy_from_slice(&3u16.to_le_bytes());
    put32(&mut file, DIRECTORY_COUNT, 16);

    let table = optional + 240;
    for (index, section) in sections.iter().enumerate() {
        let raw_at = file.len();
        let mut raw = section.raw.clone();
        raw.resize(raw.len().div_ceil(0x200) * 0x200, 0);
        let at = table + index * SECTION_LEN;
        file[at..at + 8].copy_from_slice(b".sect\0\0\0");
        file[at + 8..at + 12].copy_from_slice(&section.virtual_len.to_le_bytes());
        file[at + 12..at + 16].copy_from_slice(&section.rva.to_le_bytes());
        file[at + 16..at + 20].copy_from_slice(&(raw.len() as u32).to_le_bytes());
        file[at + 20..at + 24].copy_from_slice(&(raw_at as u32).to_le_bytes());
        file[at + 36..at + 40].copy_from_slice(&section.flags.to_le_bytes());
        file.extend_from_slice(&raw);
    }
    file
}

/// Code in one page, read-only data in the next: the shape of hello.exe.
fn hello() -> Vec<u8> {
    build(
        0x1000,
        0x3000,
        &[
            Section { rva: 0x1000, virtual_len: 62, raw: vec![0xcc; 62], flags: TEXT },
            Section { rva: 0x2000, virtual_len: 32, raw: b"hello".to_vec(), flags: RDATA },
        ],
    )
}

/// Where a field of the optional header is in [`hello`]'s file.
const OPTIONAL: usize = 0x44 + COFF_LEN;
const TABLE: usize = OPTIONAL + 240;

fn put32(file: &mut [u8], at: usize, value: u32) {
    file[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn refused(file: &[u8]) -> Error {
    Pe::parse(file, ALLOWED).expect_err("should be refused")
}

#[test]
fn a_good_program_loads() {
    let file = hello();
    let pe = Pe::parse(&file, ALLOWED).expect("a valid program");
    assert_eq!(pe.entry(), BASE + 0x1000);
    let segments: Vec<_> = pe.segments().collect();
    assert_eq!(segments.len(), 3);
    // The headers, read-only at the base.
    assert_eq!((segments[0].address, segments[0].len), (BASE, 0x200));
    assert!(!segments[0].writable && !segments[0].executable);
    assert_eq!(&segments[0].data[..2], b"MZ");
    // Code: its virtual size, not the file's padding.
    assert_eq!((segments[1].address, segments[1].len), (BASE + 0x1000, 62));
    assert_eq!(segments[1].data, &[0xcc; 62][..]);
    assert!(segments[1].executable && !segments[1].writable);
    // Data: 32 bytes, of which the file supplies them all (padding to 512).
    assert_eq!((segments[2].address, segments[2].len), (BASE + 0x2000, 32));
    assert_eq!(&segments[2].data[..5], b"hello");
    assert!(!segments[2].executable && !segments[2].writable);
}

#[test]
fn bss_is_memory_the_file_does_not_supply() {
    let file = build(
        0x1000,
        0x4000,
        &[
            Section { rva: 0x1000, virtual_len: 16, raw: vec![0x90; 16], flags: TEXT },
            // 8 KiB of zeroed, writable memory with no bytes in the file.
            Section { rva: 0x2000, virtual_len: 0x2000, raw: vec![], flags: DATA },
        ],
    );
    let pe = Pe::parse(&file, ALLOWED).expect("a valid program");
    let data = pe.segments().nth(2).unwrap();
    assert_eq!((data.len, data.data.len()), (0x2000, 0));
    assert!(data.writable);
}

#[test]
fn the_wrong_kind_of_file_is_refused() {
    let good = hello();
    let mut file = good.clone();
    file[0] = b'Z';
    assert_eq!(refused(&file), Error::NotPe);
    let mut file = good.clone();
    file[0x41] = b'X';
    assert_eq!(refused(&file), Error::NotPe);
    // e_lfanew far past the end.
    let mut file = good.clone();
    put32(&mut file, LFANEW, 0xffff_fff0);
    assert!(matches!(refused(&file), Error::NotPe | Error::Truncated));
    // An i386 file, and a PE32 optional header.
    let mut file = good.clone();
    file[0x44..0x46].copy_from_slice(&0x014cu16.to_le_bytes());
    assert_eq!(refused(&file), Error::WrongMachine);
    let mut file = good.clone();
    file[OPTIONAL..OPTIONAL + 2].copy_from_slice(&0x10bu16.to_le_bytes());
    assert_eq!(refused(&file), Error::WrongMachine);
    // A DLL, and an object file (not marked executable).
    let mut file = good.clone();
    file[0x44 + 18..0x44 + 20].copy_from_slice(&0x2023u16.to_le_bytes());
    assert_eq!(refused(&file), Error::NotExecutable);
    let mut file = good.clone();
    file[0x44 + 18..0x44 + 20].copy_from_slice(&0x0021u16.to_le_bytes());
    assert_eq!(refused(&file), Error::NotExecutable);
}

#[test]
fn what_it_would_need_is_refused_until_it_is_done() {
    let good = hello();
    let mut file = good.clone();
    put32(&mut file, OPTIONAL + DIRECTORIES + IMPORTS * 8, 0x2000);
    put32(&mut file, OPTIONAL + DIRECTORIES + IMPORTS * 8 + 4, 40);
    assert_eq!(refused(&file), Error::Imports);
    let mut file = good.clone();
    put32(&mut file, OPTIONAL + DIRECTORIES + TLS * 8 + 4, 40);
    assert_eq!(refused(&file), Error::ThreadLocalStorage);
    // The same directories past the count the file declares are not read.
    let mut file = good.clone();
    put32(&mut file, OPTIONAL + DIRECTORY_COUNT, 1);
    put32(&mut file, OPTIONAL + DIRECTORIES + IMPORTS * 8 + 4, 40);
    assert!(Pe::parse(&file, ALLOWED).is_ok());
    // Seventeen directories, or more than the optional header holds.
    let mut file = good.clone();
    put32(&mut file, OPTIONAL + DIRECTORY_COUNT, 17);
    assert_eq!(refused(&file), Error::Malformed);
    let mut file = good;
    file[0x44 + 16..0x44 + 18].copy_from_slice(&200u16.to_le_bytes());
    assert_eq!(refused(&file), Error::Malformed);
}

#[test]
fn the_image_must_fit_where_it_asks_to_go() {
    let good = hello();
    // Where a linker puts a 64-bit executable by default: outside the range,
    // and nothing here can move it yet.
    let mut file = good.clone();
    file[OPTIONAL + IMAGE_BASE..OPTIONAL + IMAGE_BASE + 8].copy_from_slice(&0x1_4000_0000u64.to_le_bytes());
    assert_eq!(refused(&file), Error::OutsideRange);
    let mut file = good.clone();
    file[OPTIONAL + IMAGE_BASE..OPTIONAL + IMAGE_BASE + 8].copy_from_slice(&0x1000u64.to_le_bytes());
    assert_eq!(refused(&file), Error::Malformed);
    let mut file = good.clone();
    file[OPTIONAL + IMAGE_BASE..OPTIONAL + IMAGE_BASE + 8].copy_from_slice(&0xffff_ffff_ffff_0000u64.to_le_bytes());
    assert_eq!(refused(&file), Error::OutsideRange);
    // An image that runs past the end of the range.
    let mut file = good.clone();
    file[OPTIONAL + IMAGE_BASE..OPTIONAL + IMAGE_BASE + 8].copy_from_slice(&0xfff_0000u64.to_le_bytes());
    put32(&mut file, OPTIONAL + SIZE_OF_IMAGE, 0x2_0000);
    assert_eq!(refused(&file), Error::OutsideRange);
    let mut file = good;
    put32(&mut file, OPTIONAL + SIZE_OF_IMAGE, 0x2800);
    assert_eq!(refused(&file), Error::Malformed);
}

#[test]
fn alignment_other_than_a_page_is_refused() {
    let good = hello();
    for (field, value) in [
        (SECTION_ALIGNMENT, 0x200),
        (SECTION_ALIGNMENT, 0x2000),
        (FILE_ALIGNMENT, 0x100),
        (FILE_ALIGNMENT, 0x300),
        (FILE_ALIGNMENT, 0x2000),
    ] {
        let mut file = good.clone();
        put32(&mut file, OPTIONAL + field, value);
        assert_eq!(refused(&file), Error::Alignment, "{field} = {value:#x}");
    }
}

#[test]
fn sections_that_lie_are_refused() {
    let good = hello();
    let section = |index: usize, field: usize| TABLE + index * SECTION_LEN + field;
    // None, and more than there is room for.
    let mut file = good.clone();
    file[0x44 + 2..0x44 + 4].copy_from_slice(&0u16.to_le_bytes());
    assert_eq!(refused(&file), Error::SectionCount);
    // Eight: with headers long enough to hold their table.
    let mut file = good.clone();
    file[0x44 + 2..0x44 + 4].copy_from_slice(&8u16.to_le_bytes());
    put32(&mut file, OPTIONAL + SIZE_OF_HEADERS, 0x400);
    assert_eq!(refused(&file), Error::SectionCount);
    // So many the table runs past the headers.
    let mut file = good.clone();
    file[0x44 + 2..0x44 + 4].copy_from_slice(&0xffffu16.to_le_bytes());
    assert_eq!(refused(&file), Error::Malformed);
    // Writable and executable.
    let mut file = good.clone();
    put32(&mut file, section(0, 36), TEXT | 0x8000_0000);
    assert_eq!(refused(&file), Error::WritableAndExecutable);
    // Over the headers, unaligned, overlapping, out of order, past the end.
    for (index, field, value) in [
        (0, 12, 0x0),
        (0, 12, 0x1100),
        (1, 12, 0x1000),
        (1, 12, 0x0),
        (1, 8, 0x1001),
        (0, 8, 0x2000),
    ] {
        let mut file = good.clone();
        put32(&mut file, section(index, field), value);
        assert_eq!(refused(&file), Error::Overlap, "section {index} field {field} = {value:#x}");
    }
    // A section's bytes past the end of the file.
    let mut file = good.clone();
    put32(&mut file, section(1, 20), 0x5000);
    assert_eq!(refused(&file), Error::OutsideFile);
    let mut file = good.clone();
    put32(&mut file, section(1, 20), 0xffff_ff00);
    assert_eq!(refused(&file), Error::OutsideFile);
    // Nothing at all: no virtual size and no bytes.
    let mut file = good.clone();
    put32(&mut file, section(1, 8), 0);
    put32(&mut file, section(1, 16), 0);
    assert_eq!(refused(&file), Error::Malformed);
    // Headers that end before the section table does.
    let mut file = good;
    put32(&mut file, OPTIONAL + SIZE_OF_HEADERS, 0x100);
    assert_eq!(refused(&file), Error::Malformed);
}

#[test]
fn the_entry_must_be_code() {
    for entry in [0x2000, 0x0, 0x2ff0, 0xffff_ffff] {
        let mut file = hello();
        put32(&mut file, OPTIONAL + ENTRY, entry);
        assert_eq!(refused(&file), Error::EntryNotExecutable, "entry {entry:#x}");
    }
}

#[test]
fn every_truncation_is_refused() {
    let file = hello();
    for len in 0..file.len() {
        // The last section's padding is not needed, but its bytes are.
        if len >= 0x400 + 32 {
            break;
        }
        assert!(Pe::parse(&file[..len], ALLOWED).is_err(), "{len} bytes");
    }
}

/// Whatever comes out of `parse` keeps the promises the kernel relies on.
fn check_promises(pe: &Pe) {
    let mut last_end = 0;
    for segment in pe.segments() {
        assert!(segment.address >= ALLOWED.start && segment.address + segment.len <= ALLOWED.end);
        assert!(!(segment.writable && segment.executable));
        assert!(segment.data.len() as u64 <= segment.len);
        assert!(segment.pages().start >= last_end, "segments share a page");
        last_end = segment.pages().end;
    }
    assert!(pe
        .segments()
        .any(|s| s.executable && (s.address..s.address + s.len).contains(&pe.entry())));
}

#[test]
fn random_damage_never_panics_and_never_breaks_a_promise() {
    let good = hello();
    let mut state = 0x853c_49e6_748f_ea9bu64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut accepted = 0;
    for _ in 0..20_000 {
        let mut file = good.clone();
        // Damage is aimed at the headers, where the parser reads.
        for _ in 0..1 + next() % 4 {
            let at = (next() % 0x200) as usize;
            file[at] = next() as u8;
        }
        if let Ok(pe) = Pe::parse(&file, ALLOWED) {
            check_promises(&pe);
            accepted += 1;
        }
    }
    // Most single-byte changes land on fields nobody reads.
    assert!(accepted > 0);
}

/// The program the boot test runs, as `scripts/hello-exe.sh` writes it.
#[test]
fn the_scripts_hello_exe_loads() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/hello-exe.sh");
    let out = std::env::temp_dir().join(format!("corrode-hello-{}.exe", std::process::id()));
    let status = std::process::Command::new("bash")
        .arg(script)
        .arg(&out)
        .status()
        .expect("bash runs");
    assert!(status.success());
    let file = std::fs::read(&out).expect("the script wrote it");
    let _ = std::fs::remove_file(&out);
    let pe = Pe::parse(&file, ALLOWED).expect("hello.exe is a valid program");
    check_promises(&pe);
    assert_eq!(pe.entry(), BASE + 0x1000);
    let segments: Vec<_> = pe.segments().collect();
    assert_eq!(segments.len(), 3);
    assert!(segments[1].executable);
    assert_eq!(segments[2].data, b"hello from a PE, over int 0x80\0\0");
}
