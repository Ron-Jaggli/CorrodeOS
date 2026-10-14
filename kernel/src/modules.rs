//! Files the bootloader left in RAM.
//!
//! There is no filesystem and there is not going to be one on the boot
//! path: storage is for loading, not for running. Everything the kernel
//! starts -- every driver, and the firmware a driver needs -- is a Limine
//! module, read off the boot medium by the bootloader and sitting in memory
//! before `kmain` runs. This file is the list of them.
//!
//! A module is bytes and a name. What the bytes are is for whoever asks:
//! [`crate::task::user`] reads one as a program.

use crate::limine;

/// One file, as the kernel sees it.
#[derive(Clone, Copy)]
pub struct Module {
    /// The path it was loaded from.
    pub path: &'static str,
    pub bytes: &'static [u8],
    /// Where its first byte is in physical memory. Page aligned.
    pub physical: u64,
}

impl Module {
    /// The last component of the path: what the file is called.
    pub fn name(&self) -> &'static str {
        self.path.rsplit('/').next().unwrap_or(self.path)
    }
}

/// Every module the bootloader loaded, in the order the configuration
/// listed them.
pub fn all() -> impl Iterator<Item = Module> {
    let hhdm = limine::HHDM.response.get().map_or(0, |hhdm| hhdm.offset);
    limine::MODULES
        .response
        .get()
        .into_iter()
        .flat_map(|response| response.files())
        .filter_map(move |file| {
            // Physical or through the direct map, depending on the base
            // revision; normalise rather than assume, as for the RSDP.
            let raw = file.address();
            let physical = if raw >= hhdm { raw - hhdm } else { raw };
            if raw == 0 || hhdm == 0 {
                return None;
            }
            // SAFETY: the bootloader loaded `size` bytes of this file at
            // `address`, in memory the map marks as kernel-and-modules: never
            // handed to the frame allocator, covered by the direct map since
            // `init_paging` maps every region, and written by nobody. The
            // kernel only ever reads it.
            let bytes = unsafe {
                core::slice::from_raw_parts((physical + hhdm) as *const u8, file.size as usize)
            };
            Some(Module {
                path: file.path(),
                bytes,
                physical,
            })
        })
}

/// The module called `name`, whatever directory it was in.
pub fn find(name: &str) -> Option<Module> {
    all().find(|module| module.name() == name)
}
