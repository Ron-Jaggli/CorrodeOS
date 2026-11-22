//! Reading a Windows program: just enough PE to load one.
//!
//! Verdigris (see `verdigris/DESIGN.md`) runs Windows programs as ordinary
//! CorrodeOS tasks, and a Windows program is a PE file: the bytes the kernel
//! is about to build an address space from. Like `corrode-elf`, this is a
//! parser of untrusted input that will run in ring 0, so it is a crate of its
//! own, and every way a file can lie is a test on the build host.
//!
//! What is accepted is narrow on purpose. A 64-bit (PE32+) x86-64
//! executable, not a DLL, with page-sized section alignment, that fits at
//! its preferred base inside the range the caller allows -- or, if it
//! carries base relocations, at the lowest base in that range that it fits
//! at, every relocation then checked and applied (see [`Pe::moved_fixups`],
//! and below) -- whose sections lie
//! inside the file and the image, in order, never both writable and
//! executable, and whose entry point is in an executable section. It may
//! import functions by name -- [`Pe::bind`] fills its import address table
//! from a table of exports, such as Verdigris's kernel32 carries (see
//! [`exports`]) -- but not by ordinal, and it may not have thread-local
//! storage yet, whose callbacks would have to run: a program that needs
//! what is not done is refused rather than started half loaded.
//!
//! What comes out is what `corrode-elf` gives -- segments and an entry -- so
//! the kernel maps both kinds of program with the same code. The headers are
//! a segment of their own, read-only at the image base, as Windows maps them:
//! programs read them there.
//!
//! A linker gives a 64-bit program a base of `0x1_4000_0000` and the
//! relocations to move it, unless told `/FIXED`; Windows moves nearly every
//! program, to randomise where it is. Here a program is moved only when it
//! must be, to the lowest base allowed: there is nothing yet to randomise
//! against. Only `DIR64` relocations (and the padding entry) are taken,
//! each inside a section and not across a page, the only kinds an x86-64
//! linker writes; and the header's `ImageBase` is made the base it was
//! loaded at, as Windows does.

#![cfg_attr(not(test), no_std)]

use core::ops::Range;

pub use corrode_elf::Segment;

const PAGE: u64 = 4096;

/// Most sections a program may have: the headers take one more segment of
/// `corrode-elf`'s eight.
pub const MAX_SECTIONS: usize = 32;

const DOS_MAGIC: &[u8; 2] = b"MZ";
const PE_MAGIC: &[u8; 4] = b"PE\0\0";
/// Where the DOS header says the PE header is.
const LFANEW: usize = 0x3c;
const COFF_LEN: usize = 20;
const SECTION_LEN: usize = 40;

const MACHINE_AMD64: u16 = 0x8664;
const PE32_PLUS: u16 = 0x20b;
const RELOCS_STRIPPED: u16 = 0x0001;
const EXECUTABLE_IMAGE: u16 = 0x0002;
const DLL: u16 = 0x2000;

/// Fields of the PE32+ optional header, by offset.
const ENTRY: usize = 16;
const IMAGE_BASE: usize = 24;
const SECTION_ALIGNMENT: usize = 32;
const FILE_ALIGNMENT: usize = 36;
const SIZE_OF_IMAGE: usize = 56;
const SIZE_OF_HEADERS: usize = 60;
const SIZE_OF_STACK_RESERVE: usize = 72;
const DIRECTORY_COUNT: usize = 108;
const DIRECTORIES: usize = 112;

/// Data directories, by index.
const IMPORTS: usize = 1;
const RELOCATIONS: usize = 5;
const TLS: usize = 9;
const MAX_DIRECTORIES: usize = 16;

/// Windows wants an image base on a 64 KiB boundary.
const BASE_ALIGNMENT: u64 = 0x1_0000;

/// Most DLLs a program may import from, and functions in all.
pub const MAX_DLLS: usize = 64;
pub const MAX_IMPORTS: usize = 4096;
/// Most bytes of thread-local storage a program may ask for: its template
/// and the zeroes after it.
pub const MAX_TLS: u64 = 64 * 1024;
/// Longest DLL or function name read, without its terminating zero.
pub const MAX_NAME: usize = 63;
/// Most addresses a moved program's relocations may name. Generous (a
/// linker writes one per pointer in the image, and a program with a
/// million of them is very large), and a bound on the loader's work.
pub const MAX_RELOCATIONS: usize = 1 << 20;
const DESCRIPTOR_LEN: usize = 20;
const BY_ORDINAL: u64 = 1 << 63;

/// Base relocation types: padding, and a 64-bit address.
const RELOCATION_PADDING: u16 = 0;
const RELOCATION_DIR64: u16 = 10;

const SCN_EXECUTE: u32 = 0x2000_0000;
const SCN_WRITE: u32 = 0x8000_0000;

/// Why a file is not a Windows program this kernel will run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Shorter than its own headers say.
    Truncated,
    /// No `MZ`, or no `PE\0\0` where it points.
    NotPe,
    /// A PE file for another machine, or a 32-bit one.
    WrongMachine,
    /// A DLL, or an object file: not something that runs on its own.
    NotExecutable,
    /// A header field that contradicts the format, or the other fields.
    Malformed,
    /// Section alignment other than a page, or a file alignment that is not
    /// a power of two from 512 to a page.
    Alignment,
    /// More sections than [`MAX_SECTIONS`], or none.
    SectionCount,
    /// A section whose bytes are not all inside the file.
    OutsideFile,
    /// The image, at its preferred base, outside the addresses allowed.
    OutsideRange,
    /// Sections out of order, overlapping, over the headers, or past the
    /// image's end.
    Overlap,
    /// A section that is writable and executable at once.
    WritableAndExecutable,
    /// An entry point that is not in an executable section.
    EntryNotExecutable,
    /// An import table that does not hold together: a name or a table
    /// outside the file's sections, a name too long or never ended, an
    /// import address slot that is unaligned, outside the image, or in code.
    BadImports,
    /// More than [`MAX_DLLS`] DLLs or [`MAX_IMPORTS`] functions imported.
    TooManyImports,
    /// A function imported by number; only names are bound.
    ImportByOrdinal,
    /// A function the exports given to [`Pe::bind`] do not have.
    Unresolved,
    /// A thread-local storage directory that does not hold together: its
    /// template outside the file's sections, too big, or its index slot
    /// outside the image or not zero.
    BadTls,
    /// It must be moved, and its relocation table does not hold together:
    /// outside the file's sections, a block that runs past it or names an
    /// unaligned page, a type other than `DIR64`, or an address outside a
    /// section or across a page.
    BadRelocations,
    /// It must be moved, and its relocations name more than
    /// [`MAX_RELOCATIONS`] addresses.
    TooManyRelocations,
}

/// One function a program imports, and where its address goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Import<'a> {
    /// The DLL's name, as the file spells it.
    pub dll: &'a [u8],
    pub name: &'a [u8],
    /// The address of its 8-byte slot in the import address table.
    pub slot: u64,
}

/// A program's thread-local storage, as its TLS directory describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tls<'a> {
    /// What each thread's block starts as.
    pub template: &'a [u8],
    /// The block's whole length: the template, then zeroes.
    pub len: u64,
    /// Where, once loaded, its zero-ended array of callbacks is; 0 for none.
    /// The array's entries are addresses in the image, moved with it.
    pub callbacks: u64,
}

/// An 8-byte value to write at an address before the program runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fixup {
    pub address: u64,
    pub value: u64,
}

/// A Windows program that has passed every check.
#[derive(Clone, Copy, Debug)]
pub struct Pe<'a> {
    entry: u64,
    segments: [Option<Segment<'a>>; MAX_SECTIONS + 1],
    /// Where its import directory is, checked whole in `parse`; 0 for none.
    import_table: u64,
    tls: Option<Tls<'a>>,
    /// Where it was loaded, and how far that is from where it asked to be.
    base: u64,
    delta: u64,
    /// When it was moved, its relocation table, every entry checked; and
    /// the address of the header's `ImageBase`.
    relocations: &'a [u8],
    base_field: u64,
    /// The stack it asks for (`SizeOfStackReserve`), in bytes.
    stack_reserve: u64,
}

/// Each address the base relocation table `table` names, for an image at
/// `base`, given to `each`; padding skipped. Any other type, or a block
/// that does not hold together, is [`Error::BadRelocations`].
fn each_relocation(table: &[u8], base: u64, mut each: impl FnMut(u64) -> Result<(), Error>) -> Result<(), Error> {
    let bad = |_| Error::BadRelocations;
    let mut at = 0;
    while at < table.len() {
        let page = u32_at(table, at).map_err(bad)? as u64;
        let size = u32_at(table, at + 4).map_err(bad)? as usize;
        if size < 8 || !size.is_multiple_of(2) || !page.is_multiple_of(PAGE) {
            return Err(Error::BadRelocations);
        }
        let end = at.checked_add(size).filter(|end| *end <= table.len()).ok_or(Error::BadRelocations)?;
        for entry in table[at + 8..end].as_chunks::<2>().0 {
            let entry = u16::from_le_bytes(*entry);
            match entry >> 12 {
                RELOCATION_PADDING => {}
                RELOCATION_DIR64 => each(base + page + u64::from(entry & 0xfff))?,
                _ => return Err(Error::BadRelocations),
            }
        }
        at = end;
    }
    Ok(())
}

/// The eight bytes the file puts at `address`, zero past its bytes (a
/// section's uninitialised end), as they are before a relocation.
fn original(segments: &[Option<Segment<'_>>], address: u64) -> u64 {
    let mut bytes = [0u8; 8];
    for segment in segments.iter().flatten() {
        let Some(offset) = address.checked_sub(segment.address) else {
            continue;
        };
        if offset < segment.len {
            for (index, byte) in bytes.iter_mut().enumerate() {
                *byte = segment.data.get(offset as usize + index).copied().unwrap_or(0);
            }
            break;
        }
    }
    u64::from_le_bytes(bytes)
}

/// `len` bytes at `address`, if a section's file bytes hold them all.
fn file_at<'a>(segments: &[Option<Segment<'a>>], address: u64, len: u64) -> Result<&'a [u8], Error> {
    // Index 0 is the headers, never where an import table may be.
    for segment in segments[1..].iter().flatten() {
        let Some(offset) = address.checked_sub(segment.address) else {
            continue;
        };
        let end = offset.checked_add(len).ok_or(Error::BadImports)?;
        if end <= segment.data.len() as u64 {
            return Ok(&segment.data[offset as usize..end as usize]);
        }
    }
    Err(Error::BadImports)
}

/// The zero-terminated name at `address`, without its zero.
fn name_at<'a>(segments: &[Option<Segment<'a>>], address: u64) -> Result<&'a [u8], Error> {
    for segment in segments[1..].iter().flatten() {
        let Some(offset) = address.checked_sub(segment.address) else {
            continue;
        };
        let Some(rest) = segment.data.get(offset as usize..) else {
            continue;
        };
        let len = rest.iter().take(MAX_NAME + 1).position(|byte| *byte == 0);
        return match len {
            Some(len) if len > 0 => Ok(&rest[..len]),
            _ => Err(Error::BadImports),
        };
    }
    Err(Error::BadImports)
}

/// Read the import directory at `table`: every function, by name, and the
/// slot its address goes in.
fn read_imports<'a>(
    segments: &[Option<Segment<'a>>],
    base: u64,
    table: u64,
    mut each: impl FnMut(Import<'a>),
) -> Result<(), Error> {
    let mut count = 0;
    for index in 0.. {
        let at = base + table + (index * DESCRIPTOR_LEN) as u64;
        let descriptor = file_at(segments, at, DESCRIPTOR_LEN as u64)?;
        // A descriptor of zeros ends the table.
        if descriptor.iter().all(|byte| *byte == 0) {
            break;
        }
        if index == MAX_DLLS {
            return Err(Error::TooManyImports);
        }
        let lookup = u32_at(descriptor, 0)? as u64;
        let dll = name_at(segments, base + u32_at(descriptor, 12)? as u64)?;
        let slots = u32_at(descriptor, 16)? as u64;
        // The names are in the lookup table, or, when a linker left it out,
        // in the address table before it is bound.
        let lookup = if lookup == 0 { slots } else { lookup };
        if slots == 0 {
            return Err(Error::BadImports);
        }
        for entry in 0.. {
            let entry_at = (entry * 8) as u64;
            let value = u64_at(file_at(segments, base + lookup + entry_at, 8)?, 0)?;
            if value == 0 {
                break;
            }
            if value & BY_ORDINAL != 0 {
                return Err(Error::ImportByOrdinal);
            }
            if count == MAX_IMPORTS {
                return Err(Error::TooManyImports);
            }
            // A hint (two bytes nobody needs), then the name.
            let hint = value.checked_add(2).filter(|at| *at < 1 << 31).ok_or(Error::BadImports)?;
            let name = name_at(segments, base + hint)?;
            let slot = base + slots + entry_at;
            // The slot is written before its page is mapped, so it may be
            // in read-only data, as linkers put it; but not in code, and
            // not straddling two pages.
            let holder = segments[1..].iter().flatten().find(|segment| {
                slot >= segment.address && slot + 8 <= segment.address + segment.len
            });
            if !slot.is_multiple_of(8) || holder.is_none_or(|segment| segment.executable) {
                return Err(Error::BadImports);
            }
            each(Import { dll, name, slot });
            count += 1;
        }
    }
    Ok(())
}

/// Read the TLS directory at `directory` (an address), for an image at
/// `base` that asked to be at `preferred`. Its fields are addresses at the
/// preferred base.
fn read_tls<'a>(
    segments: &[Option<Segment<'a>>],
    base: u64,
    preferred: u64,
    directory: u64,
) -> Result<Tls<'a>, Error> {
    let bad = |_| Error::BadTls;
    let fields = file_at(segments, directory, 40).map_err(bad)?;
    let here = |va: u64| -> Result<u64, Error> {
        va.checked_sub(preferred).and_then(|rva| rva.checked_add(base)).ok_or(Error::BadTls)
    };
    let (start, end) = (u64_at(fields, 0).map_err(bad)?, u64_at(fields, 8).map_err(bad)?);
    let (index, callbacks) = (u64_at(fields, 16).map_err(bad)?, u64_at(fields, 24).map_err(bad)?);
    let zero_fill = u32_at(fields, 32).map_err(bad)? as u64;
    let template_len = end.checked_sub(start).ok_or(Error::BadTls)?;
    let len = template_len.checked_add(zero_fill).filter(|len| *len <= MAX_TLS).ok_or(Error::BadTls)?;
    let template = if template_len == 0 {
        &[][..]
    } else {
        file_at(segments, here(start)?, template_len).map_err(bad)?
    };
    // Windows writes the program's TLS index (always 0 here: one module
    // has TLS) into this slot. Linkers leave it 0, so it is only checked.
    let index = here(index)?;
    let inside = |address: u64, len: u64| {
        segments[1..].iter().flatten().any(|segment| {
            address >= segment.address && address.saturating_add(len) <= segment.address + segment.len
        })
    };
    if !inside(index, 4) || original(segments, index) as u32 != 0 {
        return Err(Error::BadTls);
    }
    let callbacks = if callbacks == 0 {
        0
    } else {
        let at = here(callbacks)?;
        if !inside(at, 8) || !at.is_multiple_of(8) {
            return Err(Error::BadTls);
        }
        at
    };
    Ok(Tls { template, len, callbacks })
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, Error> {
    let field = bytes.get(at..at.checked_add(2).ok_or(Error::Truncated)?);
    Ok(u16::from_le_bytes(field.ok_or(Error::Truncated)?.try_into().map_err(|_| Error::Truncated)?))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, Error> {
    let field = bytes.get(at..at.checked_add(4).ok_or(Error::Truncated)?);
    Ok(u32::from_le_bytes(field.ok_or(Error::Truncated)?.try_into().map_err(|_| Error::Truncated)?))
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64, Error> {
    let field = bytes.get(at..at.checked_add(8).ok_or(Error::Truncated)?);
    Ok(u64::from_le_bytes(field.ok_or(Error::Truncated)?.try_into().map_err(|_| Error::Truncated)?))
}

impl<'a> Pe<'a> {
    /// Check `bytes` is a program that may be loaded with every byte of its
    /// image inside `allowed` -- at its preferred base, or moved to the
    /// lowest base there if it carries relocations -- and describe it.
    pub fn parse(bytes: &'a [u8], allowed: Range<u64>) -> Result<Pe<'a>, Error> {
        if bytes.get(..2) != Some(DOS_MAGIC) {
            return Err(Error::NotPe);
        }
        let pe = u32_at(bytes, LFANEW)? as usize;
        if bytes.get(pe..pe.checked_add(4).ok_or(Error::Truncated)?) != Some(PE_MAGIC) {
            return Err(Error::NotPe);
        }
        let coff = pe + 4;
        if u16_at(bytes, coff)? != MACHINE_AMD64 {
            return Err(Error::WrongMachine);
        }
        let sections = u16_at(bytes, coff + 2)? as usize;
        let optional_len = u16_at(bytes, coff + 16)? as usize;
        let characteristics = u16_at(bytes, coff + 18)?;
        if characteristics & EXECUTABLE_IMAGE == 0 || characteristics & DLL != 0 {
            return Err(Error::NotExecutable);
        }

        let optional = coff + COFF_LEN;
        let header = bytes
            .get(optional..optional.checked_add(optional_len).ok_or(Error::Truncated)?)
            .ok_or(Error::Truncated)?;
        if u16_at(header, 0)? != PE32_PLUS {
            return Err(Error::WrongMachine);
        }
        let entry_rva = u32_at(header, ENTRY)? as u64;
        let base = u64_at(header, IMAGE_BASE)?;
        let section_alignment = u32_at(header, SECTION_ALIGNMENT)? as u64;
        let file_alignment = u32_at(header, FILE_ALIGNMENT)? as u64;
        let image_len = u32_at(header, SIZE_OF_IMAGE)? as u64;
        let headers_len = u32_at(header, SIZE_OF_HEADERS)? as u64;
        let stack_reserve = u64_at(header, SIZE_OF_STACK_RESERVE)?;
        let directories = u32_at(header, DIRECTORY_COUNT)? as usize;

        if section_alignment != PAGE
            || !file_alignment.is_power_of_two()
            || !(512..=PAGE).contains(&file_alignment)
        {
            return Err(Error::Alignment);
        }
        // The directories must fit in the optional header the file says it
        // has, and there are never more than sixteen.
        if directories > MAX_DIRECTORIES || DIRECTORIES + directories * 8 > optional_len {
            return Err(Error::Malformed);
        }
        let directory = |index: usize| -> Result<(u32, u32), Error> {
            if index >= directories {
                return Ok((0, 0));
            }
            let at = DIRECTORIES + index * 8;
            Ok((u32_at(header, at)?, u32_at(header, at + 4)?))
        };
        let (import_table, import_len) = directory(IMPORTS)?;
        let (relocation_table, relocation_len) = directory(RELOCATIONS)?;
        let movable = characteristics & RELOCS_STRIPPED == 0 && relocation_len != 0;
        let (tls_table, tls_len) = directory(TLS)?;

        if !base.is_multiple_of(BASE_ALIGNMENT) || image_len == 0 || !image_len.is_multiple_of(PAGE) {
            return Err(Error::Malformed);
        }
        let fits = |base: u64| base >= allowed.start && base.checked_add(image_len).is_some_and(|end| end <= allowed.end);
        let preferred = base;
        let base = if fits(preferred) {
            preferred
        } else {
            let lowest = allowed.start.checked_next_multiple_of(BASE_ALIGNMENT);
            match lowest {
                Some(lowest) if movable && fits(lowest) => lowest,
                _ => return Err(Error::OutsideRange),
            }
        };
        let table = optional + optional_len;
        let table_end = sections
            .checked_mul(SECTION_LEN)
            .and_then(|len| len.checked_add(table))
            .ok_or(Error::Truncated)?;
        // The headers are everything up to the end of the section table, and
        // must fit in the file and below the first section.
        if headers_len < table_end as u64 || headers_len > image_len {
            return Err(Error::Malformed);
        }
        let header_bytes = bytes.get(..headers_len as usize).ok_or(Error::Truncated)?;
        if sections == 0 || sections > MAX_SECTIONS {
            return Err(Error::SectionCount);
        }

        let mut segments = [None; MAX_SECTIONS + 1];
        segments[0] = Some(Segment {
            address: base,
            len: headers_len,
            data: header_bytes,
            writable: false,
            executable: false,
        });
        // Where the next section may start: past the headers' pages, then
        // past each section's.
        let mut next = headers_len.div_ceil(PAGE) * PAGE;
        let mut entry_ok = false;
        for index in 0..sections {
            let at = table + index * SECTION_LEN;
            let virtual_len = u32_at(bytes, at + 8)? as u64;
            let rva = u32_at(bytes, at + 12)? as u64;
            let raw_len = u32_at(bytes, at + 16)? as u64;
            let raw_at = u32_at(bytes, at + 20)? as u64;
            let flags = u32_at(bytes, at + 36)?;

            // A section's size in memory is its virtual size, or, when a
            // linker left that zero, its raw size. Raw bytes past the virtual
            // size are file padding, not the program's.
            let len = if virtual_len == 0 { raw_len } else { virtual_len };
            if len == 0 {
                return Err(Error::Malformed);
            }
            if !rva.is_multiple_of(PAGE) || rva < next {
                return Err(Error::Overlap);
            }
            let end = rva.checked_add(len).ok_or(Error::Overlap)?;
            if end > image_len {
                return Err(Error::Overlap);
            }
            next = end.div_ceil(PAGE) * PAGE;

            let supplied = raw_len.min(len);
            let data = if supplied == 0 {
                &bytes[..0]
            } else {
                let from = usize::try_from(raw_at).map_err(|_| Error::OutsideFile)?;
                let to = usize::try_from(raw_at + supplied).map_err(|_| Error::OutsideFile)?;
                bytes.get(from..to).ok_or(Error::OutsideFile)?
            };

            let (writable, executable) = (flags & SCN_WRITE != 0, flags & SCN_EXECUTE != 0);
            if writable && executable {
                return Err(Error::WritableAndExecutable);
            }
            if executable && (rva..end).contains(&entry_rva) {
                entry_ok = true;
            }
            segments[1 + index] = Some(Segment {
                address: base + rva,
                len,
                data,
                writable,
                executable,
            });
        }
        if !entry_ok {
            return Err(Error::EntryNotExecutable);
        }
        let import_table = if import_len == 0 {
            0
        } else {
            read_imports(&segments, base, import_table as u64, |_| {})?;
            import_table as u64
        };
        let tls = if tls_len == 0 {
            None
        } else {
            Some(read_tls(&segments, base, preferred, base + tls_table as u64)?)
        };

        // Moved: every relocation must name eight bytes inside a section,
        // in one page, before any is applied.
        let relocations = if base == preferred {
            &bytes[..0]
        } else {
            let table = file_at(&segments, base + relocation_table as u64, relocation_len as u64)
                .map_err(|_| Error::BadRelocations)?;
            let mut count = 0usize;
            each_relocation(table, base, |address| {
                count += 1;
                if count > MAX_RELOCATIONS {
                    return Err(Error::TooManyRelocations);
                }
                let inside = segments[1..].iter().flatten().any(|segment| {
                    address >= segment.address && address + 8 <= segment.address + segment.len
                });
                if inside && address % PAGE <= PAGE - 8 {
                    Ok(())
                } else {
                    Err(Error::BadRelocations)
                }
            })?;
            table
        };

        Ok(Pe {
            entry: base + entry_rva,
            segments,
            import_table,
            tls,
            base,
            delta: base.wrapping_sub(preferred),
            relocations,
            base_field: base + (optional + IMAGE_BASE) as u64,
            stack_reserve,
        })
    }

    /// Every function the program imports, given to `each`.
    pub fn each_import(&self, each: impl FnMut(Import<'a>)) {
        if self.import_table != 0 {
            // Checked whole in `parse`, so nothing here can fail.
            let _ = read_imports(&self.segments, self.base, self.import_table, each);
        }
    }

    /// How many functions it imports.
    pub fn import_count(&self) -> usize {
        let mut count = 0;
        self.each_import(|_| count += 1);
        count
    }

    /// Its thread-local storage, if it has any.
    pub fn tls(&self) -> Option<Tls<'a>> {
        self.tls
    }

    /// Fill the import address table: `resolve` gives each import's address
    /// from the DLL's name and the function's, or `None` if it has no such
    /// function, which refuses the program ([`Error::Unresolved`]). The
    /// fixups are to be written before the pages are mapped.
    ///
    /// Each fixup goes to `each`; on an error some may already have gone,
    /// so a caller keeps them until it has `Ok`.
    pub fn bind(
        &self,
        mut resolve: impl FnMut(&[u8], &[u8]) -> Option<u64>,
        mut each: impl FnMut(Fixup),
    ) -> Result<(), Error> {
        let mut result = Ok(());
        self.each_import(|import| {
            if result.is_err() {
                return;
            }
            match resolve(import.dll, import.name) {
                Some(value) => each(Fixup { address: import.slot, value }),
                None => result = Err(Error::Unresolved),
            }
        });
        result
    }

    /// Where it is loaded: its preferred base, or where it was moved to.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Whether it was moved from its preferred base.
    pub fn moved(&self) -> bool {
        self.delta != 0
    }

    /// The base it asked for.
    pub fn preferred_base(&self) -> u64 {
        self.base.wrapping_sub(self.delta)
    }

    /// How many addresses moving it changes: none if it was not moved.
    pub fn relocations(&self) -> usize {
        let mut count = 0;
        let _ = each_relocation(self.relocations, self.base, |_| {
            count += 1;
            Ok(())
        });
        count
    }

    /// Every fixup moving it makes, given to `each` in one walk of the
    /// relocation table: the header's `ImageBase` made the base it is at,
    /// then each address a relocation names, moved by as far as the image
    /// was. None if it was not moved. Each lies inside a section (the
    /// headers, for `ImageBase`) and inside one page, as `parse` checked.
    /// They go before [`Pe::bind`]'s, which win at the same address: an
    /// import's slot is the import's.
    pub fn moved_fixups(&self, mut each: impl FnMut(Fixup)) {
        if self.delta == 0 {
            return;
        }
        each(Fixup { address: self.base_field, value: self.base });
        // Checked whole in `parse`, so nothing here can fail.
        let _ = each_relocation(self.relocations, self.base, |address| {
            let value = original(&self.segments, address).wrapping_add(self.delta);
            each(Fixup { address, value });
            Ok(())
        });
    }

    /// The stack it asks for, in bytes: the header's `SizeOfStackReserve`.
    /// The loader decides how much of it to give.
    pub fn stack_reserve(&self) -> u64 {
        self.stack_reserve
    }

    /// Where execution starts.
    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// The runs of memory to load: the headers, then each section in order.
    pub fn segments(&self) -> impl Iterator<Item = Segment<'a>> + '_ {
        self.segments.iter().flatten().copied()
    }
}

pub mod exports;

#[cfg(test)]
mod tests;
