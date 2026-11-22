//! The root directory, kept sealed on the disk until the passcode opens it.
//!
//! See `corrode_vault` for the format. A boot medium made for a vault
//! (`./corrode vault-iso`) carries the kernel and the programs, each checked
//! by Limine against its hash, and no files. The disk carries the root
//! directory, a tar sealed under a key made from the passcode. At boot the
//! kernel reads the sealed bytes, keeps them, and puts the lock screen up;
//! the first right passcode opens them into memory the kernel holds, and
//! only then does the launcher get the files, as it gets them from the
//! boot medium otherwise.
//!
//! When the disk has a vault it is the root directory: a `root.tar` on the
//! boot medium is then not used, so swapping the medium cannot hand the
//! person someone else's files behind the lock screen.
//!
//! The key is made in kernel memory, used, and wiped (`corrode_vault::Key`
//! zeroes itself). AES runs on a general CPU in one `asm!` block a block,
//! which expands the round keys in registers and zeroes every XMM register
//! it used before it ends -- never on the secure core, whose key this has
//! nothing to do with, and never in an interrupt handler: unlocking is the
//! desktop's loop answering the lock screen.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};

use corrode_vault::{Header, OFFSET, SECTOR};

use crate::arch::x86_64::without_interrupts;
use crate::modules::Module;
use crate::sync::SpinLock;
use crate::{memory, println, store};

const NONE: u8 = 0;
const LOCKED: u8 = 1;
const OPEN: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(NONE);

/// What was read off the disk: the header, and the sealed tar.
static SEALED: SpinLock<Option<(Header, Vec<u8>)>> = SpinLock::new(None);

/// The opened tar: a memory object the kernel can see, and its length.
static OPENED: SpinLock<Option<(usize, usize)>> = SpinLock::new(None);

/// The header on the disk now, once open: `reseal` writes beside it.
static CURRENT: SpinLock<Option<Header>> = SpinLock::new(None);

/// Look for a vault on the disk, after `store::start`. Says whether there
/// is one.
pub fn probe() -> bool {
    let Some(first) = store::read_sector(OFFSET / SECTOR as u64) else {
        return false;
    };
    let Some(header) = Header::parse(&first) else {
        println!("vault      : none on the disk -- files from the boot medium");
        return false;
    };
    let mut sealed = Vec::new();
    if sealed.try_reserve_exact(header.sectors() as usize * SECTOR).is_err() {
        println!("vault      : no memory for the sealed files");
        return false;
    }
    let first_sector = header.data_sector;
    for sector in first_sector..first_sector + header.sectors() {
        let Some(bytes) = store::read_sector(sector) else {
            println!("vault      : the disk would not give sector {sector}");
            return false;
        };
        sealed.extend_from_slice(&bytes);
    }
    sealed.truncate(header.len as usize);
    println!(
        "vault      : the root directory is sealed on the disk ({} KiB); the passcode opens it",
        sealed.len().div_ceil(1024)
    );
    without_interrupts(|| *SEALED.lock() = Some((header, sealed)));
    STATE.store(LOCKED, Ordering::Release);
    true
}

/// Whether the disk has a vault that is still shut.
pub fn locked() -> bool {
    STATE.load(Ordering::Acquire) == LOCKED
}

/// Whether the disk has a vault, shut or open: then it, and nothing on the
/// boot medium, is the root directory.
pub fn present() -> bool {
    STATE.load(Ordering::Acquire) != NONE
}

/// Try `passcode`. True if it opened the vault (or it was already open).
/// Slow on purpose -- the key takes `header.iterations` rounds of PBKDF2 --
/// so called with interrupts on and no lock held.
pub fn unlock(passcode: &[u8]) -> bool {
    match STATE.load(Ordering::Acquire) {
        OPEN => return true,
        LOCKED => {}
        _ => return false,
    }
    if !aes_supported() {
        println!("vault      : this CPU has no AES-NI; the vault cannot be opened");
        return false;
    }
    let Some((header, mut data)) = without_interrupts(|| {
        SEALED.lock().as_ref().map(|(header, sealed)| {
            let mut copy = Vec::new();
            copy.try_reserve_exact(sealed.len()).ok()?;
            copy.extend_from_slice(sealed);
            Some((*header, copy))
        })
    })
    .flatten() else {
        return false;
    };
    let opened = corrode_vault::open(&mut aes_block, passcode, &header, &mut data);
    if !opened {
        // `open` zeroed what it decrypted.
        return false;
    }
    let pages = data.len().div_ceil(4096);
    let view = memory::create(pages, true).and_then(|object| Some((object, memory::in_kernel(object)?)));
    let Some((object, (address, _))) = view else {
        println!("vault      : opened, but no memory to keep the files in");
        corrode_vault::wipe(&mut data);
        return false;
    };
    // SAFETY: the object is `pages` pages, the kernel's to write, and new:
    // nothing else can see it yet.
    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), address, data.len()) };
    corrode_vault::wipe(&mut data);
    let len = data.len();
    without_interrupts(|| {
        *OPENED.lock() = Some((object, len));
        *CURRENT.lock() = Some(header);
        // The sealed copy is not needed again.
        *SEALED.lock() = None;
    });
    STATE.store(OPEN, Ordering::Release);
    println!("vault      : opened, {} KiB of files", len.div_ceil(1024));
    true
}

/// Seal the open vault again under `passcode`, with a fresh salt and nonce
/// from RDRAND, into the slot it is not in; then write the header that
/// points there, and read both back. Until that one sector is written the
/// old header, and the old passcode, still open the old copy. True if the
/// disk now holds the vault under `passcode`.
pub fn reseal(passcode: &[u8]) -> bool {
    if STATE.load(Ordering::Acquire) != OPEN || !aes_supported() {
        return false;
    }
    let Some(((object, len), old)) = without_interrupts(|| (*OPENED.lock()).zip(*CURRENT.lock())) else {
        return false;
    };
    let Some((address, _)) = memory::in_kernel(object) else {
        return false;
    };
    let (Some(salt), Some(nonce)) = (random::<16>(), random::<12>()) else {
        println!("vault      : RDRAND gave nothing; not sealed again");
        return false;
    };
    let mut data = Vec::new();
    if data.try_reserve_exact(old.slot_sectors() as usize * SECTOR).is_err() {
        return false;
    }
    // SAFETY: the opened tar, `len` bytes written in `unlock` and never
    // again.
    data.extend_from_slice(unsafe { core::slice::from_raw_parts(address as *const u8, len) });
    let mut header = corrode_vault::seal(
        &mut aes_block,
        passcode,
        salt,
        nonce,
        corrode_vault::ITERATIONS,
        &mut data,
    );
    header.data_sector = old.other();
    data.resize(header.sectors() as usize * SECTOR, 0);

    let mut written = true;
    for (index, chunk) in data.chunks(SECTOR).enumerate() {
        let mut sector = [0u8; SECTOR];
        sector.copy_from_slice(chunk);
        written &= store::write_sector(header.data_sector + index as u64, &sector);
    }
    let back = store::read_sector(header.data_sector).is_some_and(|sector| sector[..] == data[..SECTOR]);
    if !written || !back {
        println!(
            "vault      : the new copy did not reach the disk (written {written}, read back {back}); \
             the old passcode still opens it"
        );
        return false;
    }
    let mut first = [0u8; SECTOR];
    first[..corrode_vault::HEADER_LEN].copy_from_slice(&header.to_bytes());
    let switched = store::write_sector(OFFSET / SECTOR as u64, &first)
        && store::read_sector(OFFSET / SECTOR as u64).is_some_and(|sector| Header::parse(&sector) == Some(header));
    if !switched {
        println!("vault      : the header did not reach the disk; the old passcode still opens it");
        return false;
    }
    without_interrupts(|| *CURRENT.lock() = Some(header));
    println!(
        "vault      : sealed again under the new passcode, in sector {} on",
        header.data_sector
    );
    true
}

/// `N` bytes from RDRAND, or `None` if it would not give them.
fn random<const N: usize>() -> Option<[u8; N]> {
    let mut out = [0u8; N];
    for chunk in out.chunks_mut(8) {
        let mut word = 0u64;
        let mut tries = 0;
        loop {
            let ok: u8;
            // SAFETY: RDRAND writes one register and the carry flag.
            unsafe {
                core::arch::asm!(
                    "rdrand {word}",
                    "setc {ok}",
                    word = out(reg) word,
                    ok = out(reg_byte) ok,
                    options(nomem, nostack),
                );
            }
            if ok == 1 && word != u64::MAX {
                break;
            }
            tries += 1;
            if tries == 100 {
                return None;
            }
        }
        chunk.copy_from_slice(&word.to_le_bytes()[..chunk.len()]);
    }
    Some(out)
}

/// The opened root directory as a module, for `modules::all`.
pub fn module() -> Option<Module> {
    let (object, len) = without_interrupts(|| *OPENED.lock())?;
    let (address, _) = memory::in_kernel(object)?;
    // SAFETY: the object outlives the kernel's use of it (it is never
    // freed), the first `len` bytes were written in `unlock`, and nothing
    // writes them again: blobs are mapped read-only.
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, len) };
    Some(Module {
        path: "/vault/root.tar",
        bytes,
        physical: 0,
        memory: Some(object),
    })
}

/// The root directory -- its place in the module list, for a blob
/// capability, and the module -- if there is one to read: the vault's once
/// open, or the boot medium's when there is no vault.
pub fn root() -> Option<(usize, Module)> {
    if present() {
        let index = crate::modules::all().count().checked_sub(1)?;
        module().map(|module| (index, module))
    } else {
        crate::modules::position("root.tar")
    }
}

/// Whether this CPU has AES-NI.
fn aes_supported() -> bool {
    // CPUID leaf 1, ECX bit 25.
    let leaf = core::arch::x86_64::__cpuid(1);
    leaf.ecx & (1 << 25) != 0
}

/// One AES-128 block under `key`, by AES-NI: the round keys are expanded in
/// `xmm1`-`xmm3` and never stored, and every XMM register used is zeroed
/// before the block ends.
fn aes_block(key: &[u8; 16], block: &mut [u8; 16]) {
    macro_rules! round {
        ($rcon:literal, $op:literal) => {
            concat!(
                "aeskeygenassist xmm2, xmm1, ", $rcon, "\n",
                "pshufd xmm2, xmm2, 0xff\n",
                "movdqa xmm3, xmm1\n",
                "pslldq xmm3, 4\n",
                "pxor xmm1, xmm3\n",
                "pslldq xmm3, 4\n",
                "pxor xmm1, xmm3\n",
                "pslldq xmm3, 4\n",
                "pxor xmm1, xmm3\n",
                "pxor xmm1, xmm2\n",
                $op, " xmm0, xmm1\n",
            )
        };
    }
    // SAFETY: SSE is on on every general CPU before its first task, and
    // `unlock` checked AES-NI. `key` and `block` are 16 bytes each, read
    // and written unaligned. XMM registers are not named as clobbers: this
    // soft-float build keeps nothing in them, and the block zeroes the ones
    // it used. No call is made inside it.
    unsafe {
        core::arch::asm!(
            "movdqu xmm1, xmmword ptr [{key}]",
            "movdqu xmm0, xmmword ptr [{block}]",
            "pxor xmm0, xmm1",
            round!("0x01", "aesenc"),
            round!("0x02", "aesenc"),
            round!("0x04", "aesenc"),
            round!("0x08", "aesenc"),
            round!("0x10", "aesenc"),
            round!("0x20", "aesenc"),
            round!("0x40", "aesenc"),
            round!("0x80", "aesenc"),
            round!("0x1b", "aesenc"),
            round!("0x36", "aesenclast"),
            "movdqu xmmword ptr [{block}], xmm0",
            "pxor xmm0, xmm0",
            "pxor xmm1, xmm1",
            "pxor xmm2, xmm2",
            "pxor xmm3, xmm3",
            key = in(reg) key.as_ptr(),
            block = in(reg) block.as_mut_ptr(),
            options(nostack, preserves_flags),
        );
    }
}

/// Boot self-checks: the AES here against FIPS-197's example, and the vault
/// format against a vault made here, so `vault-test` is not the only thing
/// that would notice either breaking.
pub fn self_check() {
    if !aes_supported() {
        println!("vault      : no AES-NI; nothing to check");
        return;
    }
    let mut block = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
    ];
    let key: [u8; 16] = core::array::from_fn(|i| i as u8);
    aes_block(&key, &mut block);
    let expected = [
        0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5, 0x5a,
    ];
    crate::check("vault", "AES-128 by AES-NI gives FIPS-197's answer", block == expected);

    let files: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
    let mut data = files.clone();
    let header = corrode_vault::seal(
        &mut aes_block,
        b"2468",
        [3; 16],
        [5; 12],
        corrode_vault::MIN_ITERATIONS,
        &mut data,
    );
    let mut wrong = data.clone();
    let refused = !corrode_vault::open(&mut aes_block, b"1357", &header, &mut wrong)
        && wrong.iter().all(|&byte| byte == 0);
    crate::check("vault", "a wrong passcode opens nothing", refused);
    let opened = corrode_vault::open(&mut aes_block, b"2468", &header, &mut data) && data == files;
    crate::check("vault", "the right one opens it", opened);
}
