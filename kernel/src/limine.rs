//! Hand-rolled Limine boot protocol bindings.
//!
//! We own these rather than pulling the `limine` crate: it's a small, stable,
//! well-specified ABI, and owning it means the `unsafe` here is auditable in one
//! sitting instead of spread through a dependency we don't control.
//!
//! Protocol reference: limine-bootloader/limine PROTOCOL.md (base revision 2).
//!
//! The bootloader walks our loaded image looking for the 16-byte request IDs
//! below, fills in the `response` pointer, and only then jumps to our entry
//! point. So every response read in this module happens strictly after the
//! bootloader is done writing, single-threaded, with no other CPU running.

use core::cell::UnsafeCell;
use core::ffi::c_char;

const MAGIC0: u64 = 0xc7b1_dd30_df4c_8b88;
const MAGIC1: u64 = 0x0a82_e883_a194_f07b;

/// Wrapper making a request's bootloader-written response slot placeable in a
/// `static`.
#[repr(transparent)]
pub struct ResponsePtr<T>(UnsafeCell<*mut T>);

// SAFETY: The only writer is the bootloader, which finishes all writes before
// transferring control to `kmain`. From the kernel's perspective the cell is
// immutable for its entire lifetime, and no Corrode thread exists yet at the
// time it is written, so there is no data race to synchronise against.
unsafe impl<T> Sync for ResponsePtr<T> {}

impl<T> ResponsePtr<T> {
    const fn null() -> Self {
        Self(UnsafeCell::new(core::ptr::null_mut()))
    }

    /// Returns the bootloader's response, or `None` if it declined the request.
    pub fn get(&self) -> Option<&'static T> {
        // SAFETY: `self.0` is only written by the bootloader before `kmain`
        // runs (see the `Sync` justification above), so this read cannot race.
        // The pointer, if non-null, was produced by the bootloader and points at
        // a live, correctly-typed response in bootloader-reclaimable memory,
        // which we never reclaim. Hence promoting it to `'static` is sound.
        let ptr = unsafe { *self.0.get() };
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { &*ptr })
        }
    }
}

/// Declares the base revision of the protocol we speak. The bootloader zeroes
/// the third word if it can honour it.
#[used]
#[link_section = ".requests"]
static BASE_REVISION: [u64; 3] = [0xf956_2b2d_5c95_a6c8, 0x6a7b_3849_4453_6bdc, 2];

/// True if the bootloader acknowledged our requested base revision.
pub fn base_revision_supported() -> bool {
    // SAFETY: `BASE_REVISION` is a plain static the bootloader may have patched
    // before we ran. A volatile read prevents the compiler from constant-folding
    // the initialiser value it can see at compile time.
    unsafe { core::ptr::read_volatile(&BASE_REVISION[2]) == 0 }
}

// ---------------------------------------------------------------------------
// Framebuffer
// ---------------------------------------------------------------------------

pub const MEMORY_MODEL_RGB: u8 = 1;

#[repr(C)]
pub struct Framebuffer {
    pub address: *mut u8,
    pub width: u64,
    pub height: u64,
    pub pitch: u64,
    pub bpp: u16,
    pub memory_model: u8,
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
    _unused: [u8; 7],
    pub edid_size: u64,
    pub edid: *mut u8,
}

#[repr(C)]
pub struct FramebufferResponse {
    pub revision: u64,
    pub framebuffer_count: u64,
    pub framebuffers: *mut *mut Framebuffer,
}

impl FramebufferResponse {
    /// The first framebuffer the firmware gave us, if any.
    pub fn first(&self) -> Option<&'static Framebuffer> {
        if self.framebuffer_count == 0 || self.framebuffers.is_null() {
            return None;
        }
        // SAFETY: `framebuffer_count` is non-zero and `framebuffers` is a
        // bootloader-provided array of that many valid pointers, so index 0 is
        // in bounds and points at a live `Framebuffer`.
        Some(unsafe { &**self.framebuffers })
    }
}

#[repr(C)]
pub struct FramebufferRequest {
    id: [u64; 4],
    revision: u64,
    pub response: ResponsePtr<FramebufferResponse>,
}

#[used]
#[link_section = ".requests"]
pub static FRAMEBUFFER: FramebufferRequest = FramebufferRequest {
    id: [MAGIC0, MAGIC1, 0x9d58_27dc_d881_dd75, 0xa314_8604_f6fa_b11b],
    revision: 0,
    response: ResponsePtr::null(),
};

// ---------------------------------------------------------------------------
// Memory map
// ---------------------------------------------------------------------------

pub const MEMMAP_USABLE: u64 = 0;
pub const MEMMAP_BOOTLOADER_RECLAIMABLE: u64 = 5;

#[repr(C)]
pub struct MemmapEntry {
    pub base: u64,
    pub length: u64,
    pub kind: u64,
}

#[repr(C)]
pub struct MemmapResponse {
    pub revision: u64,
    pub entry_count: u64,
    pub entries: *mut *mut MemmapEntry,
}

impl MemmapResponse {
    pub fn entries(&self) -> impl Iterator<Item = &'static MemmapEntry> + '_ {
        (0..self.entry_count).filter_map(move |i| {
            if self.entries.is_null() {
                return None;
            }
            // SAFETY: `i < entry_count`, and the bootloader guarantees
            // `entries` points at that many valid `MemmapEntry` pointers.
            Some(unsafe { &**self.entries.add(i as usize) })
        })
    }
}

#[repr(C)]
pub struct MemmapRequest {
    id: [u64; 4],
    revision: u64,
    pub response: ResponsePtr<MemmapResponse>,
}

#[used]
#[link_section = ".requests"]
pub static MEMMAP: MemmapRequest = MemmapRequest {
    id: [MAGIC0, MAGIC1, 0x67cf_3d9d_378a_806f, 0xe304_acdf_c50c_3c62],
    revision: 0,
    response: ResponsePtr::null(),
};

// ---------------------------------------------------------------------------
// Higher-half direct map
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct HhdmResponse {
    pub revision: u64,
    /// Add this to a physical address to get a kernel-visible virtual address.
    pub offset: u64,
}

#[repr(C)]
pub struct HhdmRequest {
    id: [u64; 4],
    revision: u64,
    pub response: ResponsePtr<HhdmResponse>,
}

#[used]
#[link_section = ".requests"]
pub static HHDM: HhdmRequest = HhdmRequest {
    id: [MAGIC0, MAGIC1, 0x48dc_f1cb_8ad2_b852, 0x6398_4e95_9a98_244b],
    revision: 0,
    response: ResponsePtr::null(),
};

// ---------------------------------------------------------------------------
// Bootloader identification
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct BootloaderInfoResponse {
    pub revision: u64,
    name: *const c_char,
    version: *const c_char,
}

impl BootloaderInfoResponse {
    pub fn name(&self) -> &'static str {
        // SAFETY: bootloader-provided NUL-terminated string in memory we never
        // reclaim.
        unsafe { cstr(self.name) }
    }

    pub fn version(&self) -> &'static str {
        // SAFETY: as above.
        unsafe { cstr(self.version) }
    }
}

/// # Safety
/// `ptr` must be null or point at a NUL-terminated byte string that stays live
/// for the rest of the kernel's execution.
unsafe fn cstr(ptr: *const c_char) -> &'static str {
    if ptr.is_null() {
        return "<null>";
    }
    let mut len = 0usize;
    // SAFETY: caller guarantees a NUL terminator, so this walk stops in bounds.
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` bytes starting at `ptr` are readable per the caller's
    // guarantee. Non-UTF-8 bootloader strings degrade to a placeholder rather
    // than UB.
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
    core::str::from_utf8(bytes).unwrap_or("<non-utf8>")
}

#[repr(C)]
pub struct BootloaderInfoRequest {
    id: [u64; 4],
    revision: u64,
    pub response: ResponsePtr<BootloaderInfoResponse>,
}

#[used]
#[link_section = ".requests"]
pub static BOOTLOADER_INFO: BootloaderInfoRequest = BootloaderInfoRequest {
    id: [MAGIC0, MAGIC1, 0xf550_38d8_e2a1_202f, 0x2794_26fc_f5f5_9740],
    revision: 0,
    response: ResponsePtr::null(),
};

// ---------------------------------------------------------------------------
// Kernel load address
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct KernelAddressResponse {
    pub revision: u64,
    /// Physical address the kernel image was loaded at.
    pub physical_base: u64,
    /// Virtual address it was linked and mapped at.
    pub virtual_base: u64,
}

#[repr(C)]
pub struct KernelAddressRequest {
    id: [u64; 4],
    revision: u64,
    pub response: ResponsePtr<KernelAddressResponse>,
}

/// Needed to build our own page tables: the linker symbols give virtual
/// addresses, and mapping them requires knowing where the image physically
/// landed.
#[used]
#[link_section = ".requests"]
pub static KERNEL_ADDRESS: KernelAddressRequest = KernelAddressRequest {
    id: [MAGIC0, MAGIC1, 0x71ba_7686_3cc5_5f63, 0xb264_4a48_c516_a487],
    revision: 0,
    response: ResponsePtr::null(),
};

// ---------------------------------------------------------------------------
// ACPI root pointer
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct RsdpResponse {
    pub revision: u64,
    /// Pointer to the RSDP.
    ///
    /// Whether this is physical or HHDM-virtual changed between Limine base
    /// revisions, so callers should normalise it rather than assume.
    pub address: u64,
}

#[repr(C)]
pub struct RsdpRequest {
    id: [u64; 4],
    revision: u64,
    pub response: ResponsePtr<RsdpResponse>,
}

#[used]
#[link_section = ".requests"]
pub static RSDP: RsdpRequest = RsdpRequest {
    id: [MAGIC0, MAGIC1, 0xc5e7_7b6b_397e_7b43, 0x2763_7845_accd_cf3c],
    revision: 0,
    response: ResponsePtr::null(),
};
