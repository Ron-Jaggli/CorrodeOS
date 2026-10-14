//! Finding a virtio device's structures through PCI (4.1.4).
//!
//! A modern virtio device describes itself with vendor-specific PCI
//! capabilities, each saying which BAR one of its structures is behind and
//! where in it. A driver reads those through its configuration space
//! capability, then maps the BARs it was given.

use corrode_pci::{Capabilities, ConfigSpace, CAP_VENDOR};

pub const COMMON: u8 = 1;
pub const NOTIFY: u8 = 2;
pub const ISR: u8 = 3;
pub const DEVICE: u8 = 4;

/// One structure: which BAR, how far in, how long.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub bar: u8,
    pub offset: u32,
    pub length: u32,
}

/// Where everything is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub common: Option<Region>,
    pub notify: Option<Region>,
    /// Bytes between one queue's doorbell and the next.
    pub notify_multiplier: u32,
    pub isr: Option<Region>,
    pub device: Option<Region>,
}

impl Layout {
    /// Read every virtio capability a function has. The first of each kind
    /// wins, as the specification asks: a device may offer alternatives and
    /// lists the one it prefers first.
    pub fn read(config: &impl ConfigSpace) -> Layout {
        let mut layout = Layout::default();
        for (id, at) in Capabilities::new(config) {
            if id != CAP_VENDOR {
                continue;
            }
            let at = at as u16;
            let kind = config.read8(at + 3);
            let region = Region {
                bar: config.read8(at + 4),
                offset: config.read32(at + 8),
                length: config.read32(at + 12),
            };
            // BARs past the sixth are reserved values, not BARs.
            if region.bar > 5 {
                continue;
            }
            let slot = match kind {
                COMMON => &mut layout.common,
                NOTIFY => &mut layout.notify,
                ISR => &mut layout.isr,
                DEVICE => &mut layout.device,
                _ => continue,
            };
            if slot.is_none() {
                *slot = Some(region);
                if kind == NOTIFY {
                    layout.notify_multiplier = config.read32(at + 16);
                }
            }
        }
        layout
    }
}

/// Offsets into the common configuration structure (4.1.4.3).
pub mod common {
    pub const DEVICE_FEATURE_SELECT: usize = 0x00;
    pub const DEVICE_FEATURE: usize = 0x04;
    pub const DRIVER_FEATURE_SELECT: usize = 0x08;
    pub const DRIVER_FEATURE: usize = 0x0c;
    pub const MSIX_CONFIG: usize = 0x10;
    pub const NUM_QUEUES: usize = 0x12;
    pub const DEVICE_STATUS: usize = 0x14;
    pub const CONFIG_GENERATION: usize = 0x15;
    pub const QUEUE_SELECT: usize = 0x16;
    pub const QUEUE_SIZE: usize = 0x18;
    pub const QUEUE_MSIX_VECTOR: usize = 0x1a;
    pub const QUEUE_ENABLE: usize = 0x1c;
    pub const QUEUE_NOTIFY_OFF: usize = 0x1e;
    pub const QUEUE_DESC: usize = 0x20;
    pub const QUEUE_DRIVER: usize = 0x28;
    pub const QUEUE_DEVICE: usize = 0x30;
    /// Written to a vector register to mean "no interrupt for this".
    pub const NO_VECTOR: u16 = 0xffff;
}
