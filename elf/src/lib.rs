//! Reading a program: just enough ELF to load one.
//!
//! A user program arrives as bytes the bootloader put in RAM, and the kernel
//! is about to build an address space out of whatever those bytes say. That
//! makes this a parser of untrusted input running in ring 0, which is the
//! reason it is a crate of its own with no kernel in it: every way a file
//! can lie is a test that runs on the build host.
//!
//! What is accepted is narrow on purpose. A 64-bit little-endian x86-64
//! executable, statically linked at fixed addresses, whose loadable segments
//! each sit entirely inside the range the caller allows, do not share a page
//! with each other, and are never both writable and executable. There is no
//! relocation, no dynamic section and no interpreter, because nothing here
//! would know what to do with one.
//!
//! [`Elf::parse`] checks all of it before returning anything. After that,
//! walking the segments cannot fail and does no arithmetic that could
//! overflow, so the loader that uses this has no error path of its own to
//! get wrong.

#![cfg_attr(not(test), no_std)]

use core::ops::Range;

const PAGE: u64 = 4096;

/// Most loadable segments a program may have. Text, read-only data and data
/// is three; this leaves room and keeps the overlap check small.
pub const MAX_SEGMENTS: usize = 8;

const HEADER_LEN: usize = 64;
const PROGRAM_HEADER_LEN: usize = 56;

const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

/// Why a file is not a program this kernel will run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Shorter than its own headers say.
    Truncated,
    /// Not an ELF file at all.
    NotElf,
    /// An ELF file for some other machine, word size or byte order.
    WrongMachine,
    /// Not a fixed-address executable: a shared object, a relocatable, or
    /// something that wants a dynamic linker.
    NotStatic,
    /// A header field that contradicts the format.
    Malformed,
    /// More loadable segments than [`MAX_SEGMENTS`], or none.
    SegmentCount,
    /// A segment whose bytes are not all inside the file.
    OutsideFile,
    /// A segment, or the entry point, outside the addresses allowed.
    OutsideRange,
    /// A segment that is writable and executable at once.
    WritableAndExecutable,
    /// Two segments that would need different permissions on one page.
    SharedPage,
    /// An entry point that is not in an executable segment.
    EntryNotExecutable,
}

/// One run of a program's memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment<'a> {
    /// Where its first byte goes.
    pub address: u64,
    /// How many bytes it occupies once loaded. Everything past `data` is
    /// zero: that is where `.bss` lives.
    pub len: u64,
    /// The bytes the file supplies, which go at `address`.
    pub data: &'a [u8],
    pub writable: bool,
    pub executable: bool,
}

impl Segment<'_> {
    /// The pages this segment touches, as addresses: first, and one past the
    /// last.
    pub fn pages(&self) -> Range<u64> {
        // Cannot overflow: `parse` checked that `address + len` is inside
        // the caller's range.
        self.address / PAGE * PAGE..(self.address + self.len).div_ceil(PAGE) * PAGE
    }
}

/// A program that has passed every check.
#[derive(Clone, Copy, Debug)]
pub struct Elf<'a> {
    entry: u64,
    segments: [Option<Segment<'a>>; MAX_SEGMENTS],
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

impl<'a> Elf<'a> {
    /// Check `bytes` is a program that may be loaded with every segment
    /// inside `allowed`, and describe it.
    pub fn parse(bytes: &'a [u8], allowed: Range<u64>) -> Result<Elf<'a>, Error> {
        if bytes.len() < HEADER_LEN {
            return Err(Error::Truncated);
        }
        if bytes[..4] != *b"\x7fELF" {
            return Err(Error::NotElf);
        }
        // Class 2 is 64-bit, data 1 is little-endian.
        if bytes[4] != 2 || bytes[5] != 1 {
            return Err(Error::WrongMachine);
        }

        let field16 = |at| u16_at(bytes, at).ok_or(Error::Truncated);
        let field64 = |at| u64_at(bytes, at).ok_or(Error::Truncated);

        if field16(18)? != EM_X86_64 {
            return Err(Error::WrongMachine);
        }
        if field16(16)? != ET_EXEC {
            return Err(Error::NotStatic);
        }
        let entry = field64(24)?;
        let table = field64(32)?;
        if field16(54)? as usize != PROGRAM_HEADER_LEN {
            return Err(Error::Malformed);
        }
        let count = field16(56)? as usize;

        let mut segments = [None; MAX_SEGMENTS];
        let mut found = 0;
        for index in 0..count {
            // The table offset is a number from the file. Everything derived
            // from it is checked arithmetic, and a header that is not wholly
            // inside the file is the file being shorter than it says.
            let at = (index as u64)
                .checked_mul(PROGRAM_HEADER_LEN as u64)
                .and_then(|offset| offset.checked_add(table))
                .and_then(|at| usize::try_from(at).ok())
                .ok_or(Error::Truncated)?;
            let header = at
                .checked_add(PROGRAM_HEADER_LEN)
                .and_then(|end| bytes.get(at..end))
                .ok_or(Error::Truncated)?;

            let kind = u32_at(header, 0).ok_or(Error::Truncated)?;
            if kind == PT_DYNAMIC || kind == PT_INTERP {
                return Err(Error::NotStatic);
            }
            if kind != PT_LOAD {
                continue;
            }

            let flags = u32_at(header, 4).ok_or(Error::Truncated)?;
            let offset = u64_at(header, 8).ok_or(Error::Truncated)?;
            let address = u64_at(header, 16).ok_or(Error::Truncated)?;
            let file_len = u64_at(header, 32).ok_or(Error::Truncated)?;
            let len = u64_at(header, 40).ok_or(Error::Truncated)?;

            // A segment of no bytes loads nothing; linkers emit them.
            if len == 0 {
                continue;
            }
            if file_len > len {
                return Err(Error::Malformed);
            }
            let data = offset
                .checked_add(file_len)
                .and_then(|end| Some(usize::try_from(offset).ok()?..usize::try_from(end).ok()?))
                .and_then(|range| bytes.get(range))
                .ok_or(Error::OutsideFile)?;

            let end = address.checked_add(len).ok_or(Error::OutsideRange)?;
            if address < allowed.start || end > allowed.end {
                return Err(Error::OutsideRange);
            }

            let (writable, executable) = (flags & PF_W != 0, flags & PF_X != 0);
            if writable && executable {
                return Err(Error::WritableAndExecutable);
            }

            if found == MAX_SEGMENTS {
                return Err(Error::SegmentCount);
            }
            segments[found] = Some(Segment {
                address,
                len,
                data,
                writable,
                executable,
            });
            found += 1;
        }

        if found == 0 {
            return Err(Error::SegmentCount);
        }

        // Permissions are per page, so two segments on one page cannot both
        // have theirs.
        let loaded = || segments.iter().flatten();
        for (index, first) in loaded().enumerate() {
            for second in loaded().skip(index + 1) {
                let (a, b) = (first.pages(), second.pages());
                if a.start < b.end && b.start < a.end {
                    return Err(Error::SharedPage);
                }
            }
        }

        let entry_ok = loaded().any(|segment| {
            segment.executable && entry >= segment.address && entry < segment.address + segment.len
        });
        if !entry_ok {
            return Err(Error::EntryNotExecutable);
        }

        Ok(Elf { entry, segments })
    }

    /// Where execution starts.
    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// The runs of memory to load, in the order the file lists them.
    pub fn segments(&self) -> impl Iterator<Item = Segment<'a>> + '_ {
        self.segments.iter().flatten().copied()
    }
}

#[cfg(test)]
mod tests;
