//! Which logical CPUs share a physical core.
//!
//! Two hardware threads on one core (Intel's Hyper-Threading, AMD's SMT) share
//! that core's L1 and L2 caches, TLBs, branch predictors and execution ports.
//! For scheduling that is a detail. For isolation it is the whole question:
//! code on one thread can measure the other through every one of those shared
//! structures, which is what L1TF, MDS and PortSmash are. A CPU set aside from
//! the rest of the kernel is only set aside if its sibling is too.
//!
//! # How it is worked out
//!
//! An APIC ID is not an arbitrary number. Firmware builds it from fields --
//! thread within core, core within package, package -- and CPUID says how wide
//! the lowest field is. Shift that many bits off an APIC ID and what is left
//! names the physical core; two CPUs with the same remainder are siblings.
//!
//! The width comes from the first source that answers, newest first:
//!
//! * leaf 0x1F, Intel's V2 extended topology;
//! * leaf 0x0B, extended topology, on Intel since Nehalem and AMD since Zen;
//! * leaf 0x8000_001E, AMD's own, for parts with topology extensions but no 0x0B;
//! * leaf 1's logical-processor count over leaf 4's core count, the legacy
//!   Intel calculation, which predates all of the above.
//!
//! The width is a property of the package, not of one CPU, so asking on the
//! bootstrap processor answers for everyone. It is also a maximum: firmware
//! reserves the field even with SMT switched off. Which CPUs actually exist is
//! the firmware's CPU list, never `1 << width`.

/// How the SMT width was found. Reported so a wrong answer on real hardware
/// can be traced to the leaf that gave it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Leaf1F,
    Leaf0B,
    AmdExtended,
    Legacy,
    /// Nothing advertised more than one thread per core.
    None,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Leaf1F => "cpuid 0x1f",
            Source::Leaf0B => "cpuid 0x0b",
            Source::AmdExtended => "cpuid 0x8000001e",
            Source::Legacy => "cpuid 1 + 4",
            Source::None => "no smt advertised",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Topology {
    /// Bits of an APIC ID that number the thread within its core.
    smt_shift: u32,
    pub source: Source,
}

impl Topology {
    /// The physical core an APIC ID belongs to. Comparable only with other
    /// results from the same `Topology`.
    pub fn core_of(&self, apic_id: u32) -> u32 {
        apic_id >> self.smt_shift
    }

    /// The most threads one core can have. An upper bound; see the module docs.
    pub fn max_threads_per_core(&self) -> u32 {
        1 << self.smt_shift
    }
}

/// Read the topology on this CPU.
pub fn detect() -> Topology {
    let max_leaf = cpuid(0, 0).eax;

    for (leaf, source) in [(0x1f, Source::Leaf1F), (0x0b, Source::Leaf0B)] {
        if max_leaf >= leaf {
            if let Some(shift) = extended_smt_shift(leaf) {
                return Topology {
                    smt_shift: shift,
                    source,
                };
            }
        }
    }

    if let Some(threads) = amd_threads_per_core() {
        return Topology {
            smt_shift: bits_for(threads),
            source: Source::AmdExtended,
        };
    }

    if let Some(threads) = legacy_threads_per_core(max_leaf) {
        return Topology {
            smt_shift: bits_for(threads),
            source: Source::Legacy,
        };
    }

    Topology {
        smt_shift: 0,
        source: Source::None,
    }
}

/// Leaf 0x1F or 0x0B, sub-leaf 0. Both report one topology level per sub-leaf,
/// lowest first, and when SMT exists at all it is the lowest.
fn extended_smt_shift(leaf: u32) -> Option<u32> {
    const LEVEL_SMT: u32 = 1;
    let level = cpuid(leaf, 0);
    // EBX is the number of CPUs at this level; zero means the leaf is not
    // implemented even though the maximum leaf said it might be.
    if level.ebx & 0xffff == 0 {
        return None;
    }
    let level_type = (level.ecx >> 8) & 0xff;
    // A first level that is not SMT means there is no SMT level: every core
    // runs one thread and its APIC ID needs no bits for one.
    Some(if level_type == LEVEL_SMT {
        level.eax & 0x1f
    } else {
        0
    })
}

/// AMD leaf 0x8000_001E, EBX bits 15:8: threads per compute unit, minus one.
fn amd_threads_per_core() -> Option<u32> {
    const EXTENDED_FEATURES: u32 = 0x8000_0001;
    const TOPOLOGY: u32 = 0x8000_001e;
    const TOPOLOGY_EXTENSIONS: u32 = 1 << 22;

    let max_extended = cpuid(0x8000_0000, 0).eax;
    if max_extended < TOPOLOGY || cpuid(EXTENDED_FEATURES, 0).ecx & TOPOLOGY_EXTENSIONS == 0 {
        return None;
    }
    Some(((cpuid(TOPOLOGY, 0).ebx >> 8) & 0xff) + 1)
}

/// Logical CPUs per package (leaf 1) over cores per package (leaf 4). Leaf 4
/// is Intel-only; without it every logical CPU is assumed to be its own core,
/// which over-isolates rather than under-isolates.
fn legacy_threads_per_core(max_leaf: u32) -> Option<u32> {
    const HTT: u32 = 1 << 28;
    let features = cpuid(1, 0);
    if features.edx & HTT == 0 || max_leaf < 4 {
        return None;
    }
    let logical = (features.ebx >> 16) & 0xff;
    let cores = ((cpuid(4, 0).eax >> 26) & 0x3f) + 1;
    (logical > cores).then(|| logical / cores)
}

/// Bits needed to number `count` things.
fn bits_for(count: u32) -> u32 {
    if count <= 1 {
        0
    } else {
        32 - (count - 1).leading_zeros()
    }
}

struct Regs {
    eax: u32,
    ebx: u32,
    ecx: u32,
    edx: u32,
}

fn cpuid(leaf: u32, subleaf: u32) -> Regs {
    let (eax, ebx, ecx, edx): (u32, u32, u32, u32);
    // SAFETY: `cpuid` exists on every x86_64 CPU and only writes the four
    // registers named here. It clobbers rbx, which the compiler reserves, so
    // the result is moved out through a scratch register and rbx put back.
    unsafe {
        core::arch::asm!(
            "mov {saved:r}, rbx",
            "cpuid",
            "xchg {saved:r}, rbx",
            saved = out(reg) ebx,
            inout("eax") leaf => eax,
            inout("ecx") subleaf => ecx,
            out("edx") edx,
            options(nomem, nostack, preserves_flags),
        );
    }
    Regs { eax, ebx, ecx, edx }
}
