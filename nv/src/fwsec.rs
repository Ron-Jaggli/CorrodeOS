//! Making FWSEC do one thing: carve out WPR2 for the GSP boot.
//!
//! FWSEC can do several jobs and is told which by a command patched into
//! its DMEM before it runs: the application interface table after its code
//! names a "DMEM mapper", the mapper names a command buffer, and the
//! command written there is `FRTS` -- copy a verified VBIOS into a region
//! of video memory and fence it off -- with that region's address and size.
//! The signature that lets it run heavy-secure is patched in too, chosen by
//! the card's fuses.
//!
//! Layouts and constants from Nova's `firmware/fwsec.rs` (Linux v6.18) and
//! the fuse rule from its `falcon/hal/ga102.rs`, both following OpenRM.

use crate::u32_at;
use crate::vbios::{DescriptorV3, DESCRIPTOR_V3_LEN, SIGNATURE_LEN};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The application interface table is not version 1, or is cut off.
    BadInterfaceTable,
    /// There is no DMEM mapper to put a command in.
    NoDmemMapper,
    /// A structure the patch needs is outside the ucode.
    OutOfBounds,
    /// The card's fuse version has no signature in this FWSEC.
    NoSignature,
}

const APPIF_ID_DMEMMAPPER: u32 = 0x4;
/// Byte offset of `init_cmd` in `FALCON_APPIF_DMEMMAPPER_V3`.
const DMEMMAPPER_INIT_CMD: usize = 44;
/// Byte offset of `cmd_in_buffer_offset`.
const DMEMMAPPER_CMD_IN: usize = 8;
pub const COMMAND_FRTS: u32 = 0x15;
const FRTS_REGION_TYPE_FB: u32 = 2;
/// `FRTS_CMD`: a `READ_VBIOS` then an `FRTS_REGION`, packed.
pub const FRTS_COMMAND_LEN: usize = 24 + 20;

fn put32(out: &mut [u8], at: usize, value: u32) -> Result<(), Error> {
    out.get_mut(at..at + 4)
        .ok_or(Error::OutOfBounds)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// Write the FRTS command into `ucode` -- FWSEC's code then data, as copied
/// out of the VBIOS -- for a WPR2 region of `size` bytes at `address` in
/// video memory. Both must be 4 KiB aligned; the command counts in pages.
pub fn patch_frts(
    ucode: &mut [u8],
    desc: &DescriptorV3,
    address: u64,
    size: u64,
) -> Result<(), Error> {
    let imem = desc.imem_load_size as usize;
    let table = imem + desc.interface_offset as usize;
    let header = ucode
        .get(table..table + 4)
        .ok_or(Error::BadInterfaceTable)?;
    let (version, header_len, entry_len, entries) = (
        header[0],
        header[1] as usize,
        header[2] as usize,
        header[3] as usize,
    );
    if version != 1 || entry_len < 8 {
        return Err(Error::BadInterfaceTable);
    }

    for i in 0..entries {
        let entry = table + header_len + i * entry_len;
        let id = u32_at(ucode, entry).ok_or(Error::BadInterfaceTable)?;
        if id != APPIF_ID_DMEMMAPPER {
            continue;
        }
        let dmem_base = u32_at(ucode, entry + 4).ok_or(Error::BadInterfaceTable)? as usize;
        let mapper = imem + dmem_base;
        let command_in = u32_at(ucode, mapper + DMEMMAPPER_CMD_IN).ok_or(Error::OutOfBounds)?;
        let command = imem + command_in as usize;
        if command + FRTS_COMMAND_LEN > ucode.len() {
            return Err(Error::OutOfBounds);
        }

        // READ_VBIOS: version 1, its own size, no address, no size, flags 2.
        put32(ucode, command, 1)?;
        put32(ucode, command + 4, 24)?;
        ucode[command + 8..command + 16].fill(0);
        put32(ucode, command + 16, 0)?;
        put32(ucode, command + 20, 2)?;
        // FRTS_REGION: version 1, its own size, where and how much in pages,
        // and that it is in the framebuffer.
        let region = command + 24;
        put32(ucode, region, 1)?;
        put32(ucode, region + 4, 20)?;
        put32(ucode, region + 8, (address >> 12) as u32)?;
        put32(ucode, region + 12, (size >> 12) as u32)?;
        put32(ucode, region + 16, FRTS_REGION_TYPE_FB)?;

        put32(ucode, mapper + DMEMMAPPER_INIT_CMD, COMMAND_FRTS)?;
        return Ok(());
    }
    Err(Error::NoDmemMapper)
}

/// The version a fuse register's value means: one more than its highest
/// set bit, zero if none is set.
pub const fn fuse_version(register: u16) -> u32 {
    u16::BITS - register.leading_zeros()
}

/// Which of FWSEC's signatures a card with fuse version `fuse` accepts.
///
/// `signature_versions` has a bit set for each version a signature is
/// included for, in order, so the index is how many bits are set below
/// this one's.
pub fn signature_index(desc: &DescriptorV3, fuse: u32) -> Result<usize, Error> {
    let bit = 1u32.checked_shl(fuse).ok_or(Error::NoSignature)?;
    let versions = desc.signature_versions as u32;
    if versions & bit == 0 {
        return Err(Error::NoSignature);
    }
    let index = (versions & (bit - 1)).count_ones() as usize;
    if index >= desc.signature_count as usize {
        return Err(Error::NoSignature);
    }
    Ok(index)
}

/// Where signature `index` is, in the bytes from the descriptor on.
pub fn signature_offset(index: usize) -> usize {
    DESCRIPTOR_V3_LEN + index * SIGNATURE_LEN
}

/// Patch signature `signature` into `ucode` where the boot ROM looks for it.
pub fn patch_signature(
    ucode: &mut [u8],
    desc: &DescriptorV3,
    signature: &[u8],
) -> Result<(), Error> {
    let at = desc.imem_load_size as usize + desc.pkc_data_offset as usize;
    ucode
        .get_mut(at..at + signature.len())
        .ok_or(Error::OutOfBounds)?
        .copy_from_slice(signature);
    Ok(())
}
