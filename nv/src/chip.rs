//! Which GPU is this: `NV_PMC_BOOT_0` and `NV_PMC_BOOT_42`.
//!
//! Field positions from Nova's `regs.rs`; the chipset numbers from its
//! `gpu.rs`, which match OpenRM's. A chipset is the architecture shifted up
//! four bits with the implementation in the bottom four: GA104 is Ampere
//! (0x17), implementation 4, so 0x174.

/// The generations this crate knows by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Architecture {
    Turing,
    Ampere,
    Hopper,
    Ada,
    BlackwellGb10x,
    BlackwellGb20x,
}

impl Architecture {
    pub const fn from_code(code: u32) -> Option<Architecture> {
        Some(match code {
            0x16 => Architecture::Turing,
            0x17 => Architecture::Ampere,
            0x18 => Architecture::Hopper,
            0x19 => Architecture::Ada,
            0x1a => Architecture::BlackwellGb10x,
            0x1b => Architecture::BlackwellGb20x,
            _ => return None,
        })
    }

    /// Whether this generation boots through GSP firmware loaded by SEC2
    /// running Booter, after FWSEC has carved out WPR2: the path this
    /// project follows. Turing and Ampere do; Hopper and later boot by other
    /// means.
    pub const fn boots_by_booter(self) -> bool {
        matches!(
            self,
            Architecture::Turing | Architecture::Ampere | Architecture::Ada
        )
    }
}

/// A chip, by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chipset {
    Tu102,
    Tu104,
    Tu106,
    Tu116,
    Tu117,
    Ga100,
    Ga102,
    Ga103,
    Ga104,
    Ga106,
    Ga107,
    Ad102,
    Ad103,
    Ad104,
    Ad106,
    Ad107,
}

impl Chipset {
    pub const fn from_code(code: u32) -> Option<Chipset> {
        Some(match code {
            0x162 => Chipset::Tu102,
            0x164 => Chipset::Tu104,
            0x166 => Chipset::Tu106,
            0x168 => Chipset::Tu116,
            0x167 => Chipset::Tu117,
            0x170 => Chipset::Ga100,
            0x172 => Chipset::Ga102,
            0x173 => Chipset::Ga103,
            0x174 => Chipset::Ga104,
            0x176 => Chipset::Ga106,
            0x177 => Chipset::Ga107,
            0x192 => Chipset::Ad102,
            0x193 => Chipset::Ad103,
            0x194 => Chipset::Ad104,
            0x196 => Chipset::Ad106,
            0x197 => Chipset::Ad107,
            _ => return None,
        })
    }

    /// The directory linux-firmware keeps this chip's files in.
    pub const fn name(self) -> &'static str {
        match self {
            Chipset::Tu102 => "tu102",
            Chipset::Tu104 => "tu104",
            Chipset::Tu106 => "tu106",
            Chipset::Tu116 => "tu116",
            Chipset::Tu117 => "tu117",
            Chipset::Ga100 => "ga100",
            Chipset::Ga102 => "ga102",
            Chipset::Ga103 => "ga103",
            Chipset::Ga104 => "ga104",
            Chipset::Ga106 => "ga106",
            Chipset::Ga107 => "ga107",
            Chipset::Ad102 => "ad102",
            Chipset::Ad103 => "ad103",
            Chipset::Ad104 => "ad104",
            Chipset::Ad106 => "ad106",
            Chipset::Ad107 => "ad107",
        }
    }

    pub const fn architecture(self) -> Architecture {
        match self {
            Chipset::Tu102 | Chipset::Tu104 | Chipset::Tu106 | Chipset::Tu116 | Chipset::Tu117 => {
                Architecture::Turing
            }
            Chipset::Ga100
            | Chipset::Ga102
            | Chipset::Ga103
            | Chipset::Ga104
            | Chipset::Ga106
            | Chipset::Ga107 => Architecture::Ampere,
            _ => Architecture::Ada,
        }
    }
}

/// `NV_PMC_BOOT_0`, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boot0 {
    pub raw: u32,
    /// Bits 28:24 with bit 8 above them.
    pub architecture: u32,
    pub implementation: u32,
    pub major: u32,
    pub minor: u32,
}

impl Boot0 {
    pub const fn decode(raw: u32) -> Boot0 {
        Boot0 {
            raw,
            architecture: ((raw >> 8) & 1) << 5 | (raw >> 24) & 0x1f,
            implementation: (raw >> 20) & 0xf,
            major: (raw >> 4) & 0xf,
            minor: raw & 0xf,
        }
    }

    /// Before Fermi the second architecture bit was not there and the
    /// numbers were smaller than GF100's 0xc. Nothing that old has GSP.
    pub const fn older_than_fermi(&self) -> bool {
        (self.raw >> 8) & 1 == 0 && (self.raw >> 24) & 0x1f < 0xc
    }
}

/// `NV_PMC_BOOT_42`, decoded: the register to identify a chip by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boot42 {
    pub raw: u32,
    pub architecture: u32,
    pub implementation: u32,
    pub major: u32,
    pub minor: u32,
}

impl Boot42 {
    pub const fn decode(raw: u32) -> Boot42 {
        Boot42 {
            raw,
            architecture: (raw >> 24) & 0x3f,
            implementation: (raw >> 20) & 0xf,
            major: (raw >> 16) & 0xf,
            minor: (raw >> 12) & 0xf,
        }
    }

    pub const fn code(&self) -> u32 {
        self.architecture << 4 | self.implementation
    }

    pub const fn chipset(&self) -> Option<Chipset> {
        Chipset::from_code(self.code())
    }
}
