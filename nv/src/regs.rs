//! BAR0 register offsets a driver reads while it is only looking.
//!
//! From Nova's `regs.rs`, which takes them from OpenRM's manuals. Only
//! registers that are safe to *read* on a card someone else's driver set up
//! are listed here; this crate has nothing yet that writes one.

/// Architecture, implementation and revision, in the original layout.
pub const PMC_BOOT_0: usize = 0x0000_0000;
/// The same, in the layout every chip since Fermi agrees on.
pub const PMC_BOOT_42: usize = 0x0000_0a00;

/// Where the VBIOS can be read through BAR0: the PROM window.
pub const PROM: usize = 0x0030_0000;
/// How much of it a scan will read at most.
pub const PROM_SCAN_MAX: usize = 0x0010_0000;

/// Whether the host may read [`GFW_BOOT`]: bit 0 is set once FWSEC has
/// lowered its protection.
pub const GFW_BOOT_PRIV_LEVEL_MASK: usize = 0x0011_8128;
/// GPU firmware boot progress; 0xff in the low byte means done.
pub const GFW_BOOT: usize = 0x0011_8234;
/// Framebuffer the boot firmware found usable, in MiB.
pub const USABLE_FB_SIZE_IN_MB: usize = 0x0011_83a4;

/// The value of a read the bus could not complete. A card that is powered
/// down or behind a broken link answers this to everything.
pub const DEAD: u32 = 0xffff_ffff;
