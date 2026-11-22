//! The table of functions a Verdigris DLL offers, as the kernel reads it.
//!
//! Verdigris's own DLLs are not PE files yet (see `verdigris/DESIGN.md`):
//! kernel32 is a CorrodeOS program linked at its own address, carrying this
//! table at that address. The kernel reads it to bind a Windows program's
//! imports, so like the rest of this crate it is read as untrusted bytes:
//! a count that runs off the end, or a name with no end, is no table.
//!
//! The layout: [`MAGIC`], then the number of entries and the size of one
//! ([`ENTRY`]) as two little-endian `u32`s, then the entries. Each is a DLL
//! name in 16 bytes and a function name in 40, both zero-padded, then the
//! function's address as a little-endian `u64`.

pub const MAGIC: [u8; 8] = *b"VRDGEXP1";
pub const HEADER: usize = 16;
pub const ENTRY: usize = 64;
pub const DLL_LEN: usize = 16;
pub const NAME_LEN: usize = 40;
/// Most entries a table may have.
pub const MAX_ENTRIES: usize = 256;

/// A table that has passed its checks.
#[derive(Clone, Copy, Debug)]
pub struct Exports<'a> {
    entries: &'a [u8],
}

/// A name in a fixed field: the bytes before the first zero.
fn field(bytes: &[u8]) -> &[u8] {
    let len = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    &bytes[..len]
}

impl<'a> Exports<'a> {
    /// Read a table from the start of `bytes`. `None` if it is not one.
    pub fn parse(bytes: &'a [u8]) -> Option<Exports<'a>> {
        if bytes.get(..8)? != MAGIC {
            return None;
        }
        let count = u32::from_le_bytes(bytes.get(8..12)?.try_into().ok()?) as usize;
        let size = u32::from_le_bytes(bytes.get(12..16)?.try_into().ok()?) as usize;
        if size != ENTRY || count > MAX_ENTRIES {
            return None;
        }
        let entries = bytes.get(HEADER..HEADER + count * ENTRY)?;
        Some(Exports { entries })
    }

    /// Every entry: DLL, function, address.
    pub fn iter(&self) -> impl Iterator<Item = (&'a [u8], &'a [u8], u64)> + 'a {
        self.entries.as_chunks::<ENTRY>().0.iter().map(|entry| {
            let mut address = [0; 8];
            address.copy_from_slice(&entry[DLL_LEN + NAME_LEN..]);
            (
                field(&entry[..DLL_LEN]),
                field(&entry[DLL_LEN..DLL_LEN + NAME_LEN]),
                u64::from_le_bytes(address),
            )
        })
    }

    /// The address of `dll`'s `name`. DLL names match without regard to
    /// case, as on Windows; function names exactly, as on Windows.
    pub fn find(&self, dll: &[u8], name: &[u8]) -> Option<u64> {
        self.iter()
            .find(|(their_dll, their_name, _)| their_dll.eq_ignore_ascii_case(dll) && *their_name == name)
            .map(|(_, _, address)| address)
    }
}
