//! Virtio, as a driver needs it and no more.
//!
//! Virtio is the interface QEMU's paravirtual devices speak: a few
//! configuration structures found through PCI vendor capabilities, and
//! queues of descriptors in ordinary memory that the device reads and
//! writes on its own. It is the one GPU interface that can be driven
//! completely inside an emulator, which is why it is the first one Corrode
//! drives -- everything a real GPU driver needs from the operating system
//! (its registers, its interrupt, memory it can DMA into, physical
//! addresses for that memory) is needed here too.
//!
//! Like `corrode-pci`, this crate decodes and encodes and does not touch a
//! machine. The queue works on memory it is pointed at, and the tests point
//! it at a vector and play the device themselves.
//!
//! Layouts are from the Virtual I/O Device specification, version 1.2,
//! sections 2.7 (split virtqueues), 4.1 (the PCI transport) and 5.7 (the
//! GPU device).

#![cfg_attr(not(test), no_std)]

pub mod gpu;
pub mod pci;
pub mod queue;

/// Device status bits, written by the driver as it goes (2.1).
pub mod status {
    /// "I have seen you."
    pub const ACKNOWLEDGE: u8 = 1;
    /// "I know how to drive you."
    pub const DRIVER: u8 = 2;
    /// "I am ready; go."
    pub const DRIVER_OK: u8 = 4;
    /// "I have chosen features and will not change my mind."
    pub const FEATURES_OK: u8 = 8;
    /// The device has got into a state it cannot get out of.
    pub const NEEDS_RESET: u8 = 64;
    /// The driver has given up.
    pub const FAILED: u8 = 128;
}

/// The one feature bit this crate requires: the device speaks virtio 1.x,
/// not the legacy interface.
pub const F_VERSION_1: u64 = 1 << 32;
