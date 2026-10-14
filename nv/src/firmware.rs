//! The firmware files linux-firmware ships for a GSP-era GPU.
//!
//! For each chip, four files under `nvidia/<chip>/gsp/`, version 570.144
//! here:
//!
//! * `booter_load-*.bin`, `booter_unload-*.bin` -- heavy-secure falcon
//!   programs SEC2 runs to put the GSP firmware in place and take it out;
//! * `bootloader-*.bin` -- the RISC-V program GSP starts with, which checks
//!   and loads the real thing;
//! * `gsp-*.bin` -- GSP-RM itself, an ELF file whose sections carry the
//!   image and its signatures.
//!
//! The first three start with the same `nvfw_bin_hdr`. Layouts follow Nova
//! (Linux v6.18, `firmware.rs`, `firmware/booter.rs`, `firmware/riscv.rs`,
//! `firmware/gsp.rs`), which follows OpenRM's `nvfw` headers.
//!
//! Every function takes the whole file as bytes and checks every offset in
//! it; a file is input, and a bad one is an [`Error`].

use crate::{slice_at, u16_at, u32_at, u64_at};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not an `nvfw` file: the magic is not 0x10de.
    NotNvfw,
    /// A header or a range it names is outside the file.
    OutOfBounds,
    /// Not an ELF file, or one without the section asked for.
    NoSection,
    /// Signatures that do not divide evenly, or none where some are needed.
    BadSignatures,
}

/// `nvfw_bin_hdr`: the header every `.bin` but GSP-RM starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BinHeader {
    pub version: u32,
    /// Where the format-specific header is.
    pub header_offset: u32,
    /// Where the payload is, and how long.
    pub data_offset: u32,
    pub data_size: u32,
}

pub const BIN_MAGIC: u32 = 0x10de;

impl BinHeader {
    pub fn parse(file: &[u8]) -> Result<BinHeader, Error> {
        let field = |at| u32_at(file, at).ok_or(Error::OutOfBounds);
        if field(0)? != BIN_MAGIC {
            return Err(Error::NotNvfw);
        }
        let header = BinHeader {
            version: field(4)?,
            header_offset: field(12)?,
            data_offset: field(16)?,
            data_size: field(20)?,
        };
        header.data(file)?;
        Ok(header)
    }

    /// The payload.
    pub fn data<'a>(&self, file: &'a [u8]) -> Result<&'a [u8], Error> {
        slice_at(file, self.data_offset as usize, self.data_size as usize).ok_or(Error::OutOfBounds)
    }
}

/// A heavy-secure falcon program: Booter, here.
///
/// `nvfw_hs_header_v2` points at everything else by offset into the file:
/// the signatures, where in the code to patch one, which one, the
/// signature parameters, and the load header that says where the code and
/// data are in the payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HsFirmware<'a> {
    file: &'a [u8],
    pub bin: BinHeader,
    pub signatures_offset: u32,
    pub signatures_size: u32,
    /// Where in the payload a signature is patched in.
    pub patch_location: u32,
    /// Index of the signature to start counting from.
    pub patch_signature: u32,
    pub signature_count: u32,
    /// From `nvfw_hs_signature_params`.
    pub fuse_version: u32,
    pub engine_id_mask: u32,
    pub ucode_id: u32,
    /// From `nvfw_hs_load_header_v2`.
    pub os_code_offset: u32,
    pub os_code_size: u32,
    pub os_data_offset: u32,
    pub os_data_size: u32,
    pub app_count: u32,
    load_header_offset: u32,
}

impl<'a> HsFirmware<'a> {
    pub fn parse(file: &'a [u8]) -> Result<HsFirmware<'a>, Error> {
        let bin = BinHeader::parse(file)?;
        let field = |at: u32| u32_at(file, at as usize).ok_or(Error::OutOfBounds);
        let hs = bin.header_offset;
        let sig_prod_offset = field(hs)?;
        let sig_prod_size = field(hs + 4)?;
        let patch_loc_offset = field(hs + 8)?;
        let patch_sig_offset = field(hs + 12)?;
        let meta_data_offset = field(hs + 16)?;
        let meta_data_size = field(hs + 20)?;
        let num_sig_offset = field(hs + 24)?;
        let header_offset = field(hs + 28)?;
        if meta_data_size < 12 {
            return Err(Error::OutOfBounds);
        }
        let firmware = HsFirmware {
            file,
            bin,
            signatures_offset: sig_prod_offset,
            signatures_size: sig_prod_size,
            patch_location: field(patch_loc_offset)?,
            patch_signature: field(patch_sig_offset)?,
            signature_count: field(num_sig_offset)?,
            fuse_version: field(meta_data_offset)?,
            engine_id_mask: field(meta_data_offset + 4)?,
            ucode_id: field(meta_data_offset + 8)?,
            os_code_offset: field(header_offset)?,
            os_code_size: field(header_offset + 4)?,
            os_data_offset: field(header_offset + 8)?,
            os_data_size: field(header_offset + 12)?,
            app_count: field(header_offset + 16)?,
            load_header_offset: header_offset,
        };
        // Everything it names has to be there.
        let payload = bin.data(file)?;
        let fits = |offset: u32, len: u32| (offset as u64 + len as u64) <= payload.len() as u64;
        if !fits(firmware.os_data_offset, firmware.os_data_size)
            || !fits(
                firmware.patch_location,
                firmware.signature_len().unwrap_or(0) as u32,
            )
        {
            return Err(Error::OutOfBounds);
        }
        firmware.signatures()?;
        for app in 0..firmware.app_count {
            let (offset, len) = firmware.app(app)?;
            if !fits(offset, len) {
                return Err(Error::OutOfBounds);
            }
        }
        Ok(firmware)
    }

    /// The payload: code and data, with a hole for the signature.
    pub fn payload(&self) -> &'a [u8] {
        // Checked in `parse`.
        self.bin.data(self.file).unwrap_or(&[])
    }

    /// Where app `index`'s code is in the payload, and how long.
    pub fn app(&self, index: u32) -> Result<(u32, u32), Error> {
        if index >= self.app_count {
            return Err(Error::OutOfBounds);
        }
        let at = self.load_header_offset as usize + 20 + index as usize * 8;
        Ok((
            u32_at(self.file, at).ok_or(Error::OutOfBounds)?,
            u32_at(self.file, at + 4).ok_or(Error::OutOfBounds)?,
        ))
    }

    /// Bytes per signature.
    pub fn signature_len(&self) -> Option<usize> {
        (self.signatures_size as usize).checked_div(self.signature_count as usize)
    }

    /// The signatures, all of them, in order.
    pub fn signatures(&self) -> Result<&'a [u8], Error> {
        if self.signature_count == 0 {
            return Ok(&[]);
        }
        if !self.signatures_size.is_multiple_of(self.signature_count) {
            return Err(Error::BadSignatures);
        }
        let start = self.signatures_offset as usize + self.patch_signature as usize;
        slice_at(self.file, start, self.signatures_size as usize).ok_or(Error::OutOfBounds)
    }

    /// The signature a GPU whose fuses read `fuse` will accept.
    ///
    /// Zero means "the last one". Otherwise the file's own fuse version
    /// minus the card's is the index -- Nova's rule, from OpenRM.
    pub fn signature_for_fuse(&self, fuse: u32) -> Result<&'a [u8], Error> {
        let all = self.signatures()?;
        let len = self.signature_len().ok_or(Error::BadSignatures)?;
        let count = self.signature_count as usize;
        let index = match fuse {
            0 => count.checked_sub(1),
            fuse => self
                .fuse_version
                .checked_sub(fuse)
                .map(|index| index as usize)
                .filter(|index| *index < count),
        }
        .ok_or(Error::BadSignatures)?;
        Ok(&all[index * len..(index + 1) * len])
    }

    /// The `pkc_data_offset` the boot ROM is told: where in DMEM the
    /// signature is, relative to the start of the data.
    pub fn pkc_data_offset(&self) -> Option<u32> {
        self.patch_location.checked_sub(self.os_data_offset)
    }
}

/// `RM_RISCV_UCODE_DESC`: the GSP bootloader's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RiscvDescriptor {
    pub version: u32,
    pub manifest_offset: u32,
    pub manifest_size: u32,
    pub app_version: u32,
    pub monitor_data_offset: u32,
    pub monitor_data_size: u32,
    pub monitor_code_offset: u32,
    pub monitor_code_size: u32,
}

impl RiscvDescriptor {
    pub fn parse(file: &[u8]) -> Result<(BinHeader, RiscvDescriptor), Error> {
        let bin = BinHeader::parse(file)?;
        let at = bin.header_offset as usize;
        let field = |n: usize| u32_at(file, at + 4 * n).ok_or(Error::OutOfBounds);
        let desc = RiscvDescriptor {
            version: field(0)?,
            app_version: field(7)?,
            manifest_offset: field(8)?,
            manifest_size: field(9)?,
            monitor_data_offset: field(10)?,
            monitor_data_size: field(11)?,
            monitor_code_offset: field(12)?,
            monitor_code_size: field(13)?,
        };
        let payload = bin.data(file)?.len() as u64;
        for (offset, len) in [
            (desc.manifest_offset, desc.manifest_size),
            (desc.monitor_data_offset, desc.monitor_data_size),
            (desc.monitor_code_offset, desc.monitor_code_size),
        ] {
            if offset as u64 + len as u64 > payload {
                return Err(Error::OutOfBounds);
            }
        }
        Ok((bin, desc))
    }
}

/// The bytes of section `name` of an ELF64 file.
///
/// GSP-RM is packaged as ELF with sections used as named blobs:
/// `.fwimage` is the image, `.fwsignature_<family>` its signatures, and
/// so on. Nothing else about ELF matters here.
pub fn elf_section<'a>(file: &'a [u8], name: &[u8]) -> Result<&'a [u8], Error> {
    if slice_at(file, 0, 4) != Some(b"\x7fELF") || file.get(4) != Some(&2) {
        return Err(Error::NoSection);
    }
    let table = u64_at(file, 0x28).ok_or(Error::NoSection)? as usize;
    let entry_len = u16_at(file, 0x3a).ok_or(Error::NoSection)? as usize;
    let count = u16_at(file, 0x3c).ok_or(Error::NoSection)? as usize;
    let names_index = u16_at(file, 0x3e).ok_or(Error::NoSection)? as usize;
    if entry_len < 64 {
        return Err(Error::NoSection);
    }
    let header = |index: usize| -> Option<(u32, u64, u64)> {
        let at = table.checked_add(index.checked_mul(entry_len)?)?;
        Some((
            u32_at(file, at)?,
            u64_at(file, at.checked_add(0x18)?)?,
            u64_at(file, at.checked_add(0x20)?)?,
        ))
    };
    let (_, names, _) = header(names_index).ok_or(Error::NoSection)?;

    for index in 0..count {
        let Some((name_at, offset, size)) = header(index) else {
            continue;
        };
        let Some(start) = (names as usize).checked_add(name_at as usize) else {
            continue;
        };
        let Some(found) = file.get(start..) else {
            continue;
        };
        let end = found
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(found.len());
        if &found[..end] == name {
            return slice_at(file, offset as usize, size as usize).ok_or(Error::OutOfBounds);
        }
    }
    Err(Error::NoSection)
}
