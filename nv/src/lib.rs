//! NVIDIA GPUs, as data.
//!
//! Everything in this crate is decoding and encoding: what a register value
//! means, where the pieces of a VBIOS are, how the firmware files NVIDIA
//! ships in linux-firmware are laid out, and what to patch into them before
//! a GPU will run them. None of it touches a card. That is deliberate and it
//! is the whole testing strategy for NVIDIA work in this project: the only
//! NVIDIA GPU here is driving the developer's desktop, QEMU does not emulate
//! one, so every part that can be checked without the hardware is checked on
//! the build host, against real firmware files where those exist and against
//! hand-built images where they do not.
//!
//! # Sources, and licences
//!
//! No code here is copied from anywhere. The layouts were read from:
//!
//! * Nova (`drivers/gpu/nova-core` in Linux, GPL-2.0), as of v6.18 for the
//!   `570.144` firmware file formats and VBIOS walk, which is the version of
//!   the files linux-firmware ships;
//! * NVIDIA's open-gpu-kernel-modules (MIT/GPL-2.0), which Nova's register
//!   names and structure names come from;
//! * envytools' documentation, for the PMC registers.
//!
//! Each module says which of those its layouts follow. Structure names are
//! kept close to OpenRM's so the next reader can find them there.

#![cfg_attr(not(test), no_std)]

pub mod chip;
pub mod firmware;
pub mod fwsec;
pub mod regs;
pub mod vbios;

/// Read a little-endian `u16` at `at`, or `None` past the end.
pub(crate) fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

/// Read a little-endian `u32` at `at`, or `None` past the end.
pub(crate) fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// Read a little-endian `u64` at `at`, or `None` past the end.
pub(crate) fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// `len` bytes at `at`, or `None` if they are not all there.
pub(crate) fn slice_at(bytes: &[u8], at: usize, len: usize) -> Option<&[u8]> {
    bytes.get(at..at.checked_add(len)?)
}

#[cfg(test)]
mod tests;
