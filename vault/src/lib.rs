//! The vault: the root directory, encrypted on the disk.
//!
//! The boot medium carries the kernel and the programs, each checked by
//! Limine against its hash before it is loaded (`scripts/seal-iso.sh`).
//! The person's files -- the root directory, a tar -- are not on it. They
//! are on the disk, sealed under a key made from the passcode, and nothing
//! reads them until the lock screen is given it.
//!
//! On the disk, after the theme's sector (PROGRESS 54), a header at byte
//! [`OFFSET`]:
//!
//! | bytes  | what                          |
//! |--------|-------------------------------|
//! | 0..8   | [`MAGIC`]                     |
//! | 8..12  | [`VERSION`]                   |
//! | 12..16 | PBKDF2 iterations             |
//! | 16..32 | salt                          |
//! | 32..44 | CCM nonce                     |
//! | 44..48 | bytes of the tar              |
//! | 48..64 | CCM tag                       |
//! | 64..72 | the sector the tar starts at  |
//!
//! and the tar, encrypted, from that sector: [`DATA_OFFSET`]'s, or one
//! slot past it. Two slots, so a vault sealed again (a new passcode) is
//! written beside the old copy, and only the header -- one sector, written
//! last -- says which is current: a crash between leaves the old passcode
//! opening the old copy. A header from before the field has zeros there,
//! read as the first slot.
//!
//! The key is the first 16 bytes of PBKDF2-HMAC-SHA-256 of the passcode
//! (`corrode_theme::passcode::pbkdf2`), an AES-128 key, and the tar is
//! sealed with AES-CCM (`corrode_seal`). The header is not sealed, but each
//! field in it goes into the key (salt, iterations) or the tag (nonce,
//! length): changing one makes the right passcode fail, and never makes a
//! wrong one work. A wrong passcode and a damaged vault are told apart by
//! nothing: the tag does not match, and what was decrypted is zeroed.
//!
//! Like `corrode_seal`, nothing here runs AES: the caller passes the block
//! cipher, AES-NI under the given key, so this is what the build host can
//! test and the kernel's part is one `asm!` block.

#![no_std]

use corrode_seal::{NONCE, TAG};
use corrode_theme::passcode::pbkdf2;

/// Where the header is on the disk, in bytes: the theme has sector 0.
pub const OFFSET: u64 = 4096;
/// Where the first slot of the sealed tar starts, in bytes: the sector
/// after the header's.
pub const DATA_OFFSET: u64 = OFFSET + SECTOR as u64;
pub const SECTOR: usize = 512;
pub const HEADER_LEN: usize = 72;
/// The first slot's first sector.
pub const DATA_SECTOR: u64 = DATA_OFFSET / SECTOR as u64;

pub const MAGIC: [u8; 8] = *b"CRDVAULT";
pub const VERSION: u32 = 1;

/// What `./corrode vault-disk` uses: about half a second a try in the
/// kernel under QEMU, once a boot.
pub const ITERATIONS: u32 = 200_000;
/// A header asking for fewer is refused, so a disk cannot be made to take
/// a cheap key; and for more, so it cannot hold the boot up for minutes.
pub const MIN_ITERATIONS: u32 = 50_000;
pub const MAX_ITERATIONS: u32 = 2_000_000;

/// The largest root directory, well under CCM's limit with a 12-byte nonce.
pub const MAX_LEN: usize = 4 << 20;

/// A block cipher: encrypt the block in place under the key.
pub trait Cipher: FnMut(&[u8; 16], &mut [u8; 16]) {}
impl<F: FnMut(&[u8; 16], &mut [u8; 16])> Cipher for F {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub iterations: u32,
    pub salt: [u8; 16],
    pub nonce: [u8; NONCE],
    /// Bytes of the tar, and of what is sealed.
    pub len: u32,
    pub tag: [u8; TAG],
    /// Where the sealed tar starts: [`DATA_SECTOR`], or [`Header::other`].
    pub data_sector: u64,
}

impl Header {
    /// The header at the start of `bytes`, if it is one this reads.
    pub fn parse(bytes: &[u8]) -> Option<Header> {
        let bytes: &[u8; HEADER_LEN] = bytes.get(..HEADER_LEN)?.try_into().ok()?;
        let word = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        let data_sector = match u64::from_le_bytes(bytes[64..72].try_into().ok()?) {
            0 => DATA_SECTOR,
            sector => sector,
        };
        let mut header = Header {
            iterations: word(12),
            salt: bytes[16..32].try_into().ok()?,
            nonce: bytes[32..44].try_into().ok()?,
            len: word(44),
            tag: bytes[48..64].try_into().ok()?,
            data_sector: DATA_SECTOR,
        };
        let sound = bytes[..8] == MAGIC
            && word(8) == VERSION
            && (MIN_ITERATIONS..=MAX_ITERATIONS).contains(&header.iterations)
            && (1..=MAX_LEN).contains(&(header.len as usize));
        // Only the two slots: the header cannot send the reads elsewhere.
        let slot = (data_sector == DATA_SECTOR || data_sector == header.other()).then_some(data_sector);
        header.data_sector = slot?;
        sound.then_some(header)
    }

    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut bytes = [0u8; HEADER_LEN];
        bytes[..8].copy_from_slice(&MAGIC);
        bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.iterations.to_le_bytes());
        bytes[16..32].copy_from_slice(&self.salt);
        bytes[32..44].copy_from_slice(&self.nonce);
        bytes[44..48].copy_from_slice(&self.len.to_le_bytes());
        bytes[48..64].copy_from_slice(&self.tag);
        bytes[64..72].copy_from_slice(&self.data_sector.to_le_bytes());
        bytes
    }

    /// Sectors one slot takes: the tar's, rounded up to a page.
    pub fn slot_sectors(&self) -> u64 {
        (self.len as u64).div_ceil(4096) * (4096 / SECTOR as u64)
    }

    /// The slot this vault is not in: where sealing it again goes.
    pub fn other(&self) -> u64 {
        if self.data_sector == DATA_SECTOR {
            DATA_SECTOR + self.slot_sectors()
        } else {
            DATA_SECTOR
        }
    }

    /// Bytes of disk the vault needs, both slots.
    pub fn disk_len(&self) -> u64 {
        (DATA_SECTOR + 2 * self.slot_sectors()) * SECTOR as u64
    }

    /// Sectors the sealed tar takes, from [`DATA_OFFSET`].
    pub fn sectors(&self) -> u64 {
        (self.len as u64).div_ceil(SECTOR as u64)
    }
}

/// The key made from a passcode, zeroed when dropped.
pub struct Key([u8; 16]);

impl Key {
    pub fn new(passcode: &[u8], salt: &[u8; 16], iterations: u32) -> Key {
        let mut derived = pbkdf2(passcode, salt, iterations);
        let mut key = [0u8; 16];
        key.copy_from_slice(&derived[..16]);
        wipe(&mut derived);
        Key(key)
    }

    pub fn bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

/// Zero `bytes` with writes the compiler may not drop.
pub fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: a valid, aligned, exclusive reference.
        unsafe { core::ptr::write_volatile(byte, 0) };
    }
}

/// Seal `data` in place under `passcode`, and give the header that opens
/// it. `salt` and `nonce` must be fresh and random.
///
/// # Panics
/// If `data` is empty or longer than [`MAX_LEN`], or `iterations` is
/// outside what [`Header::parse`] takes.
pub fn seal(
    cipher: &mut impl Cipher,
    passcode: &[u8],
    salt: [u8; 16],
    nonce: [u8; NONCE],
    iterations: u32,
    data: &mut [u8],
) -> Header {
    assert!((1..=MAX_LEN).contains(&data.len()));
    assert!((MIN_ITERATIONS..=MAX_ITERATIONS).contains(&iterations));
    let key = Key::new(passcode, &salt, iterations);
    let tag = corrode_seal::seal(&mut |block: &mut [u8; 16]| cipher(key.bytes(), block), &nonce, data);
    Header { iterations, salt, nonce, len: data.len() as u32, tag, data_sector: DATA_SECTOR }
}

/// Open what `header` describes, `data` being its `len` sealed bytes,
/// in place. False, with `data` zeroed, if the passcode is wrong or
/// anything was changed.
pub fn open(cipher: &mut impl Cipher, passcode: &[u8], header: &Header, data: &mut [u8]) -> bool {
    if data.len() != header.len as usize {
        wipe(data);
        return false;
    }
    let key = Key::new(passcode, &header.salt, header.iterations);
    corrode_seal::open(
        &mut |block: &mut [u8; 16]| cipher(key.bytes(), block),
        &header.nonce,
        data,
        &header.tag,
    )
}

#[cfg(test)]
mod tests;
