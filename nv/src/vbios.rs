//! The VBIOS: the card's own ROM, and the one piece of firmware not in
//! linux-firmware.
//!
//! A GSP-era driver reads it for one thing above all: **FWSEC**, a signed
//! falcon program that carves out the protected region the rest of the boot
//! needs and that NVIDIA ships only inside each card's ROM. Finding it is a
//! walk through several layers of container, each with its own header:
//!
//! ```text
//!   ROM ── image ── image ── image ...         PCI expansion ROM images,
//!          │                 │                 each 0x55aa then a PCIR
//!          │ PCI-AT          │ FWSEC (0xe0)
//!          │   BIT table     │   PMU lookup table
//!          │     token 0x70 ─┼─> │   entry 0x85 ──> falcon ucode
//!          │                 │                      descriptor, signatures,
//!                                                   code and data
//! ```
//!
//! Layouts follow Nova's `vbios.rs` (Linux v6.18), which follows OpenRM.
//! One oddity is kept exactly as it is there: the pointers in the PCI-AT
//! image are offsets into the PCI-AT image and the FWSEC images laid end to
//! end, *skipping* any image between them -- usually an EFI one. Nova
//! subtracts the PCI-AT image's length and indexes the FWSEC data; so does
//! this.
//!
//! Everything here works on a byte slice: [`read_rom`] copies the ROM out
//! of whatever can read it, and [`Vbios::parse`] takes it apart. The input
//! is data from a device, so every offset is checked and every failure is
//! an [`Error`], never a panic.

use crate::{slice_at, u16_at, u32_at};

/// What went wrong reading a VBIOS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// An image does not start with 0x55aa (or NVIDIA's 0x564e).
    NoRomSignature,
    /// The PCI data structure is not where the header says, or not "PCIR".
    NoPcir,
    /// An image's declared length is zero, or runs past the scan limit.
    BadImageLength,
    /// The IFR header, which some ROMs put first, names a version this
    /// does not know.
    UnknownIfr,
    /// No PCI-AT image, or it has no BIT table.
    NoBit,
    /// The BIT table has no falcon data token.
    NoFalconData,
    /// No FWSEC image.
    NoFwsec,
    /// The PMU lookup table has no production FWSEC entry.
    NoFwsecEntry,
    /// The falcon ucode descriptor is a version not handled, or does not fit.
    BadDescriptor,
    /// A pointer that leads outside the data it points into.
    OutOfBounds,
}

/// Kinds of image, by the PCIR code type.
pub const IMAGE_PCI_AT: u8 = 0x00;
pub const IMAGE_EFI: u8 = 0x03;
pub const IMAGE_NBSI: u8 = 0x70;
pub const IMAGE_FWSEC: u8 = 0xe0;

/// One image of the ROM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Image {
    /// Where it starts in the ROM.
    pub offset: usize,
    pub len: usize,
    pub code_type: u8,
    pub vendor: u16,
    pub device: u16,
    pub last: bool,
}

const PCIR_LEN: usize = 24;
const NPDE_LEN: usize = 11;

/// Read the image header at `offset`.
pub fn image_at(rom: &[u8], offset: usize) -> Result<Image, Error> {
    let data = rom.get(offset..).ok_or(Error::OutOfBounds)?;
    match u16_at(data, 0) {
        Some(0xaa55) | Some(0x4e56) => {}
        _ => return Err(Error::NoRomSignature),
    }
    let pcir_at = u16_at(data, 0x18).ok_or(Error::NoPcir)? as usize;
    let pcir = slice_at(data, pcir_at, PCIR_LEN).ok_or(Error::NoPcir)?;
    if &pcir[0..4] != b"PCIR" && &pcir[0..4] != b"NPDS" {
        return Err(Error::NoPcir);
    }
    let pcir_struct_len = u16_at(pcir, 0x0a).unwrap_or(0) as usize;
    let image_blocks = u16_at(pcir, 0x10).unwrap_or(0) as usize;
    let code_type = pcir[0x14];
    let mut last = pcir[0x15] & 0x80 != 0;
    let mut len = image_blocks * 512;

    // NVIDIA's extension, if present, sits on the next 16-byte boundary
    // after the PCIR and overrides the length and the last-image bit.
    let npde_at = (pcir_at + pcir_struct_len + 0x0f) & !0x0f;
    if let Some(npde) = slice_at(data, npde_at, NPDE_LEN) {
        let sub_blocks = u16_at(npde, 8).unwrap_or(0) as usize;
        if &npde[0..4] == b"NPDE" && sub_blocks != 0 {
            len = sub_blocks * 512;
            last = npde[10] & 0x80 != 0;
        }
    }
    if len == 0 {
        return Err(Error::BadImageLength);
    }
    // An NBSI image is the last one, whatever its bits say.
    if code_type == IMAGE_NBSI {
        last = true;
    }
    Ok(Image {
        offset,
        len,
        code_type,
        vendor: u16_at(pcir, 4).unwrap_or(0),
        device: u16_at(pcir, 6).unwrap_or(0),
        last,
    })
}

/// Where in the PROM window the PCI images start.
///
/// Usually zero. Some ROMs put an IFR ("NVGI") header first, and it says,
/// in one of three ways depending on its version, how far to skip. Takes
/// a function that reads a 32-bit word of the PROM by offset.
pub fn rom_offset(read: &mut impl FnMut(usize) -> u32) -> Result<usize, Error> {
    const IFR_SIGNATURE: u32 = u32::from_le_bytes(*b"NVGI");
    const DIRECTORY_SIGNATURE: u32 = u32::from_le_bytes(*b"RFRD");
    if read(0) != IFR_SIGNATURE {
        return Ok(0);
    }
    let fixed1 = read(4);
    match (fixed1 >> 8) & 0xff {
        1 | 2 => {
            let fixed_size = ((fixed1 >> 16) & 0x7fff) as usize;
            Ok(read(fixed_size + 4) as usize)
        }
        3 => {
            let total = (read(8) & 0xf_ffff) as usize;
            let flash_status = read(total) as usize;
            let directory = flash_status + 4096;
            if read(directory) != DIRECTORY_SIGNATURE {
                return Err(Error::UnknownIfr);
            }
            Ok(read(directory + 8) as usize)
        }
        _ => Err(Error::UnknownIfr),
    }
}

/// Copy the ROM out, image by image, into `out`, reading 32-bit words with
/// `read`. Stops after the last image, or when `out` is full. Returns how
/// many bytes are valid.
///
/// Reads only as far as the images say they go: past the end of a ROM the
/// window returns whatever the flash part feels like.
pub fn read_rom(read: &mut impl FnMut(usize) -> u32, out: &mut [u8]) -> Result<usize, Error> {
    let base = rom_offset(read)?;
    // Copy words up to `end`, or as far as `out` goes. Returns how far the
    // copy now reaches.
    fn fill_to(
        end: usize,
        filled: &mut usize,
        out: &mut [u8],
        base: usize,
        read: &mut impl FnMut(usize) -> u32,
    ) -> usize {
        let end = end.min(out.len()) & !3;
        while *filled < end {
            let word = read(base + *filled).to_le_bytes();
            out[*filled..*filled + 4].copy_from_slice(&word);
            *filled += 4;
        }
        *filled
    }

    let mut filled = 0usize;
    let mut offset = 0usize;
    loop {
        // Enough for the headers first, then the whole image.
        if fill_to(offset + 1024, &mut filled, out, base, read) < offset + 64 {
            return Err(Error::BadImageLength);
        }
        let image = image_at(&out[..filled], offset)?;
        let end = offset + image.len;
        if end > out.len() {
            return Err(Error::BadImageLength);
        }
        fill_to(end, &mut filled, out, base, read);
        if image.last {
            return Ok(end);
        }
        offset = (end + 511) & !511;
    }
}

/// The FWSEC falcon program, located.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fwsec {
    /// Offset into the FWSEC data (the FWSEC images, end to end) of the
    /// ucode descriptor.
    pub descriptor: usize,
    pub desc: DescriptorV3,
}

/// `FALCON_UCODE_DESC_V3`: how to load a heavy-secure falcon program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescriptorV3 {
    /// Bytes of descriptor, signatures included: the code follows.
    pub header_len: usize,
    pub stored_size: u32,
    /// Where in DMEM the signature goes.
    pub pkc_data_offset: u32,
    /// Where, after the code, the application interface table is.
    pub interface_offset: u32,
    pub imem_phys_base: u32,
    pub imem_load_size: u32,
    pub imem_virt_base: u32,
    pub dmem_phys_base: u32,
    pub dmem_load_size: u32,
    pub engine_id_mask: u16,
    pub ucode_id: u8,
    pub signature_count: u8,
    pub signature_versions: u16,
}

/// Bytes of the V3 descriptor itself. Signatures follow it.
pub const DESCRIPTOR_V3_LEN: usize = 44;
/// One RSA-3K signature.
pub const SIGNATURE_LEN: usize = 384;

impl DescriptorV3 {
    pub fn parse(bytes: &[u8]) -> Result<DescriptorV3, Error> {
        let field = |at| u32_at(bytes, at).ok_or(Error::BadDescriptor);
        let hdr = field(0)?;
        // Byte 1 of the header is the version.
        if (hdr >> 8) & 0xff != 3 {
            return Err(Error::BadDescriptor);
        }
        let tail = slice_at(bytes, 36, 8).ok_or(Error::BadDescriptor)?;
        Ok(DescriptorV3 {
            header_len: (hdr >> 16) as usize,
            stored_size: field(4)?,
            pkc_data_offset: field(8)?,
            interface_offset: field(12)?,
            imem_phys_base: field(16)?,
            imem_load_size: field(20)?,
            imem_virt_base: field(24)?,
            dmem_phys_base: field(28)?,
            dmem_load_size: field(32)?,
            engine_id_mask: u16::from_le_bytes([tail[0], tail[1]]),
            ucode_id: tail[2],
            signature_count: tail[3],
            signature_versions: u16::from_le_bytes([tail[4], tail[5]]),
        })
    }

    /// Bytes of code and data together, which follow the header.
    pub fn ucode_len(&self) -> usize {
        self.imem_load_size as usize + self.dmem_load_size as usize
    }
}

/// A VBIOS taken apart as far as a driver needs.
#[derive(Clone, Copy, Debug)]
pub struct Vbios<'a> {
    rom: &'a [u8],
    images: [Option<Image>; 8],
    pci_at: Image,
    /// The BIT table: where it is in the PCI-AT image, and its shape.
    bit_offset: usize,
    bit_header_len: usize,
    bit_token_len: usize,
    bit_tokens: usize,
}

/// A token from the BIT table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitToken {
    pub id: u8,
    pub version: u8,
    pub size: u16,
    pub offset: u16,
}

/// The token pointing at falcon data, which leads to FWSEC.
pub const BIT_FALCON_DATA: u8 = 0x70;
/// PMU lookup table application ids.
pub const APPID_FWSEC_PROD: u8 = 0x85;

impl<'a> Vbios<'a> {
    /// Walk the images of a ROM read by [`read_rom`] and find the BIT table.
    pub fn parse(rom: &'a [u8]) -> Result<Vbios<'a>, Error> {
        let mut images = [None; 8];
        let mut offset = 0;
        for slot in images.iter_mut() {
            let image = image_at(rom, offset)?;
            if image.offset + image.len > rom.len() {
                return Err(Error::BadImageLength);
            }
            *slot = Some(image);
            if image.last {
                break;
            }
            offset = (image.offset + image.len + 511) & !511;
        }

        let pci_at = images
            .iter()
            .flatten()
            .find(|image| image.code_type == IMAGE_PCI_AT)
            .copied()
            .ok_or(Error::NoBit)?;
        let data = &rom[pci_at.offset..pci_at.offset + pci_at.len];
        const PATTERN: [u8; 6] = [0xff, 0xb8, b'B', b'I', b'T', 0];
        let bit_offset = data
            .windows(PATTERN.len())
            .position(|window| window == PATTERN)
            .ok_or(Error::NoBit)?;
        let header = slice_at(data, bit_offset, 12).ok_or(Error::NoBit)?;
        Ok(Vbios {
            rom,
            images,
            pci_at,
            bit_offset,
            bit_header_len: header[8] as usize,
            bit_token_len: header[9] as usize,
            bit_tokens: header[10] as usize,
        })
    }

    pub fn images(&self) -> impl Iterator<Item = Image> + '_ {
        self.images.iter().flatten().copied()
    }

    /// The PCI-AT image's bytes.
    pub fn pci_at(&self) -> &'a [u8] {
        &self.rom[self.pci_at.offset..self.pci_at.offset + self.pci_at.len]
    }

    /// The BIT version, as BCD: 0x0100 is 1.0.
    pub fn bit_version(&self) -> u16 {
        u16_at(self.pci_at(), self.bit_offset + 6).unwrap_or(0)
    }

    /// Every BIT token.
    pub fn tokens(&self) -> impl Iterator<Item = BitToken> + '_ {
        let start = self.bit_offset + self.bit_header_len;
        (0..self.bit_tokens).filter_map(move |i| {
            let entry = slice_at(self.pci_at(), start + i * self.bit_token_len, 6)?;
            Some(BitToken {
                id: entry[0],
                version: entry[1],
                size: u16::from_le_bytes([entry[2], entry[3]]),
                offset: u16::from_le_bytes([entry[4], entry[5]]),
            })
        })
    }

    /// The FWSEC images, which may be more than one, and the length of
    /// the PCI-AT image that pointers into them are offset by.
    fn fwsec_parts(&self) -> impl Iterator<Item = &'a [u8]> + '_ {
        // FWSEC is the first image of its type and everything after it.
        let first = self
            .images()
            .position(|image| image.code_type == IMAGE_FWSEC)
            .unwrap_or(usize::MAX);
        self.images()
            .skip(first)
            .map(|image| &self.rom[image.offset..image.offset + image.len])
    }

    /// Bytes of the FWSEC data, as if its images were one: `len` bytes
    /// from offset `at`, copied into `out`. Images need not be adjacent in
    /// the ROM, so this is a copy rather than a slice.
    pub fn fwsec_bytes(&self, at: usize, out: &mut [u8]) -> Result<(), Error> {
        let mut skip = at;
        let mut written = 0;
        for part in self.fwsec_parts() {
            if skip >= part.len() {
                skip -= part.len();
                continue;
            }
            let take = (part.len() - skip).min(out.len() - written);
            out[written..written + take].copy_from_slice(&part[skip..skip + take]);
            written += take;
            skip = 0;
            if written == out.len() {
                return Ok(());
            }
        }
        if out.is_empty() {
            return Ok(());
        }
        Err(Error::OutOfBounds)
    }

    /// Follow BIT token 0x70 to the PMU lookup table, and that to the
    /// production FWSEC descriptor.
    pub fn fwsec(&self) -> Result<Fwsec, Error> {
        if self.fwsec_parts().next().is_none() {
            return Err(Error::NoFwsec);
        }
        let token = self
            .tokens()
            .find(|token| token.id == BIT_FALCON_DATA)
            .ok_or(Error::NoFalconData)?;
        let pointer = u32_at(self.pci_at(), token.offset as usize).ok_or(Error::OutOfBounds)?;
        let table = (pointer as usize)
            .checked_sub(self.pci_at.len)
            .ok_or(Error::OutOfBounds)?;

        let mut header = [0u8; 4];
        self.fwsec_bytes(table, &mut header)?;
        let (header_len, entry_len, entries) =
            (header[1] as usize, header[2] as usize, header[3] as usize);
        if entry_len < 6 {
            return Err(Error::OutOfBounds);
        }
        for i in 0..entries {
            let mut entry = [0u8; 6];
            self.fwsec_bytes(table + header_len + i * entry_len, &mut entry)?;
            if entry[0] != APPID_FWSEC_PROD {
                continue;
            }
            let data = u32::from_le_bytes([entry[2], entry[3], entry[4], entry[5]]) as usize;
            let descriptor = data
                .checked_sub(self.pci_at.len)
                .ok_or(Error::OutOfBounds)?;
            let mut bytes = [0u8; DESCRIPTOR_V3_LEN];
            self.fwsec_bytes(descriptor, &mut bytes)?;
            let desc = DescriptorV3::parse(&bytes)?;
            if desc.header_len < DESCRIPTOR_V3_LEN + desc.signature_count as usize * SIGNATURE_LEN {
                return Err(Error::BadDescriptor);
            }
            return Ok(Fwsec { descriptor, desc });
        }
        Err(Error::NoFwsecEntry)
    }
}
