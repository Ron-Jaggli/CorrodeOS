//! The PCI bus: what is plugged in, and who may touch it.
//!
//! Finding devices is the kernel's job, because the bus is one shared thing
//! and somebody has to read all of it. Driving a device is not. So this file
//! walks the bus once at boot, writes down every function it finds, and from
//! then on is a table that capabilities point into:
//!
//! * a **device** capability is one function's configuration space. READ
//!   reads its registers, WRITE writes the ones that are the driver's to
//!   write. See [`corrode_pci::WritePolicy`] for which those are and why.
//! * a **device memory** capability is one of its BARs: a run of physical
//!   addresses where the device's registers are, and how many of them.
//!
//! There is no call that takes a bus address. A task cannot ask for "device
//! 07:00.0"; it holds a capability to a device or it does not, and a task
//! that holds none cannot find out what is on the bus at all.
//!
//! # Two ways to read the same registers
//!
//! Configuration space is reached through ECAM where ACPI's MCFG table says
//! there is one -- every function's registers mapped at an address that is
//! arithmetic on its bus, device and function -- and through the original
//! pair of I/O ports otherwise. On a machine with both, the scan reads each
//! function's identity through each and compares, for the same reason the
//! boot checks count two timers: one path agreeing with itself proves
//! nothing.
//!
//! All decoding is in the `corrode-pci` crate, where it is tested on the
//! build host against a device made of an array.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use corrode_pci::mcfg::Allocation;
use corrode_pci::{Address, Bar, ConfigSpace, Header, Msi, MsiX, Width, WritePolicy};

use crate::arch::x86_64::{port, without_interrupts};
use crate::cap::{self, CapError, CapPtr, Rights};
use crate::mm::paging;
use crate::sync::SpinLock;

/// Where configuration space is mapped: 256 MiB of addresses, one page per
/// function, in the same PML4 slot as the APICs so every address space sees
/// it. Pages are mapped as the scan reaches them and never removed.
const ECAM_VIRT: u64 = 0xffff_b000_1000_0000;

const CONFIG_ADDRESS: u16 = 0xcf8;
const CONFIG_DATA: u16 = 0xcfc;
/// Bit 31 of the address port: the data port means configuration space.
const CONFIG_ENABLE: u32 = 1 << 31;

/// The port pair is an address register and a window onto whatever it names,
/// so two users interleaving would each read the other's register. Only ever
/// taken with interrupts off: a holder preempted mid-pair would wedge the
/// next task to want it on the same CPU.
static PORTS: SpinLock<()> = SpinLock::new(());

/// How one function's configuration space is reached.
#[derive(Clone, Copy)]
enum Function {
    /// Mapped: the virtual address of its 4 KiB.
    Ecam(u64),
    /// Through the port pair: the address word, without a register offset.
    Ports(u32),
}

impl Function {
    /// How many bytes of configuration space this path can reach.
    fn size(&self) -> u16 {
        match self {
            Function::Ecam(_) => corrode_pci::EXTENDED_SIZE,
            Function::Ports(_) => corrode_pci::LEGACY_SIZE,
        }
    }
}

impl ConfigSpace for Function {
    fn read(&self, offset: u16, width: Width) -> u32 {
        if !corrode_pci::in_bounds(offset as u64, width, self.size()) {
            return 0xffff_ffff;
        }
        match *self {
            Function::Ecam(base) => {
                let at = (base + offset as u64) as *const u8;
                // SAFETY: `base` is a page `function_at` mapped uncached for
                // this function, `offset` was just checked to be inside it
                // and aligned for the width, and the mapping is never removed.
                unsafe {
                    match width {
                        Width::Byte => at.read_volatile() as u32,
                        Width::Word => at.cast::<u16>().read_volatile() as u32,
                        Width::Dword => at.cast::<u32>().read_volatile(),
                    }
                }
            }
            Function::Ports(address) => without_interrupts(|| {
                let _pair = PORTS.lock();
                let data = CONFIG_DATA + (offset & 3);
                // SAFETY: 0xCF8 and 0xCFC are the PCI configuration ports on
                // every PC with a PCI bus, which `init` confirmed by reading
                // the address back. The lock makes the pair one operation.
                unsafe {
                    port::outl(CONFIG_ADDRESS, address | (offset & 0xfc) as u32);
                    match width {
                        Width::Byte => port::inb(data) as u32,
                        Width::Word => port::inw(data) as u32,
                        Width::Dword => port::inl(data),
                    }
                }
            }),
        }
    }

    fn write(&self, offset: u16, width: Width, value: u32) {
        if !corrode_pci::in_bounds(offset as u64, width, self.size()) {
            return;
        }
        match *self {
            Function::Ecam(base) => {
                let at = (base + offset as u64) as *mut u8;
                // SAFETY: as `read`. What the write means to the device is
                // the caller's business; the kernel's own callers are the
                // scan, and drivers reach this only through `config_write`.
                unsafe {
                    match width {
                        Width::Byte => at.write_volatile(value as u8),
                        Width::Word => at.cast::<u16>().write_volatile(value as u16),
                        Width::Dword => at.cast::<u32>().write_volatile(value),
                    }
                }
            }
            Function::Ports(address) => without_interrupts(|| {
                let _pair = PORTS.lock();
                let data = CONFIG_DATA + (offset & 3);
                // SAFETY: as `read`.
                unsafe {
                    port::outl(CONFIG_ADDRESS, address | (offset & 0xfc) as u32);
                    match width {
                        Width::Byte => port::outb(data, value as u8),
                        Width::Word => port::outw(data, value as u16),
                        Width::Dword => port::outl(data, value),
                    }
                }
            }),
        }
    }
}

/// The address word that selects a function through the port pair.
fn port_function(bus: u8, device: u8, function: u8) -> Function {
    Function::Ports(
        CONFIG_ENABLE
            | (bus as u32) << 16
            | (device as u32 & 0x1f) << 11
            | (function as u32 & 7) << 8,
    )
}

/// One function, as the scan found it.
#[derive(Clone, Copy)]
pub struct Device {
    pub address: Address,
    pub header: Header,
    pub bars: [Bar; 6],
    /// The expansion ROM's address and size, if it has one.
    pub rom: Option<(u64, u64)>,
    pub msi: Option<Msi>,
    pub msix: Option<MsiX>,
    function: Function,
    policy: WritePolicy,
}

impl Device {
    /// Its configuration space, for the kernel's own use: no capability is
    /// consulted, so nothing reachable from a syscall may hand this out.
    pub fn config(&self) -> impl ConfigSpace {
        self.function
    }
}

/// A run of device memory: one BAR.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    /// Which device, by its place in the table, and which of its BARs.
    pub device: usize,
    pub bar: u8,
    pub physical: u64,
    pub len: u64,
}

struct Bus {
    devices: Vec<Device>,
    regions: Vec<Region>,
}

/// Everything the scan found. Written once, at boot, and only read after.
///
/// Locked with interrupts off: it is consulted from system calls, and must
/// not be found held by a task this CPU preempted.
static BUS: SpinLock<Bus> = SpinLock::new(Bus {
    devices: Vec::new(),
    regions: Vec::new(),
});

/// Pages of configuration space mapped so far.
static ECAM_PAGES: AtomicU64 = AtomicU64::new(0);

/// How configuration space is being reached.
#[derive(Clone, Copy)]
pub enum Mechanism {
    Ecam(Allocation),
    Ports,
}

/// What a scan turned up.
pub struct Summary {
    pub mechanism: Mechanism,
    pub functions: usize,
    pub buses: usize,
    /// Functions whose identity was read through both mechanisms, and how
    /// many of those the two agreed on.
    pub cross_checked: usize,
    pub agreed: usize,
}

/// Whether the port pair is there: the address register holds what is
/// written to it, which a floating bus does not.
fn ports_present() -> bool {
    without_interrupts(|| {
        let _pair = PORTS.lock();
        // SAFETY: reading and writing 0xCF8 as a doubleword selects a
        // register and has no other effect; the old value is put back.
        unsafe {
            let saved = port::inl(CONFIG_ADDRESS);
            port::outl(CONFIG_ADDRESS, CONFIG_ENABLE);
            let held = port::inl(CONFIG_ADDRESS) == CONFIG_ENABLE;
            port::outl(CONFIG_ADDRESS, saved);
            held
        }
    })
}

/// Map one function's page of ECAM and say where.
fn ecam_function(allocation: &Allocation, bus: u8, device: u8, function: u8) -> Option<Function> {
    let physical = allocation.address(bus, device, function)?;
    let va = ECAM_VIRT + (physical - allocation.base);
    // Mapping the same page twice writes the same entry, so there is nothing
    // to remember about which have been done.
    let mapped = paging::with_kernel_mapper(|mapper| mapper.map_mmio(va, physical));
    if mapped != Some(true) {
        return None;
    }
    ECAM_PAGES.fetch_add(1, Ordering::Relaxed);
    Some(Function::Ecam(va))
}

struct Scan {
    mechanism: Mechanism,
    ports: bool,
    devices: Vec<Device>,
    /// One bit per bus already walked, so a bridge that names a bus twice --
    /// or names its own -- cannot send the scan round in a circle.
    seen: [u64; 4],
    buses: usize,
    cross_checked: usize,
    agreed: usize,
}

impl Scan {
    fn function(&self, bus: u8, device: u8, function: u8) -> Option<Function> {
        match &self.mechanism {
            Mechanism::Ecam(allocation) => ecam_function(allocation, bus, device, function),
            Mechanism::Ports => Some(port_function(bus, device, function)),
        }
    }

    fn bus(&mut self, bus: u8) {
        let (word, bit) = (bus as usize / 64, bus as u64 % 64);
        if self.seen[word] & (1 << bit) != 0 {
            return;
        }
        self.seen[word] |= 1 << bit;
        self.buses += 1;

        for device in 0..32 {
            let Some(header) = self.probe(bus, device, 0) else {
                continue;
            };
            if header.multifunction {
                for function in 1..8 {
                    self.probe(bus, device, function);
                }
            }
        }
    }

    /// Look at one function, record it if it is there, and follow it down if
    /// it is a bridge.
    fn probe(&mut self, bus: u8, device: u8, function: u8) -> Option<Header> {
        let config = self.function(bus, device, function)?;
        let header = Header::read(&config)?;

        // The same question through the other mechanism, where there is one.
        if self.ports && matches!(config, Function::Ecam(_)) {
            let other = Header::read(&port_function(bus, device, function));
            self.cross_checked += 1;
            if other == Some(header) {
                self.agreed += 1;
            }
        }

        let msi = Msi::read(&config);
        let msix = MsiX::read(&config);
        self.devices.push(Device {
            address: Address::new(0, bus, device, function),
            header,
            bars: corrode_pci::read_bars(&config, &header),
            rom: corrode_pci::read_rom(&config, &header),
            msi,
            msix,
            function: config,
            policy: WritePolicy::new(&header, msi, msix),
        });

        if header.is_bridge() {
            let secondary = config.read8(corrode_pci::SECONDARY_BUS);
            // Zero is a bridge the firmware never set up. Assigning bus
            // numbers is a job of its own, and not one for a first scan.
            if secondary != 0 {
                self.bus(secondary);
            }
        }
        Some(header)
    }
}

/// Walk the bus and record what is on it. Once, at boot.
///
/// `mcfg` is the ACPI table that says where ECAM is, if the firmware has
/// one. Without it, or if it does not cover bus 0 of segment 0, the port pair
/// is used; with neither there is no PCI bus this kernel can see, and the
/// answer is `None`.
pub fn init(mcfg: Option<&[u8]>) -> Option<Summary> {
    let ecam = mcfg.and_then(|table| {
        corrode_pci::mcfg::allocations(table)
            .find(|allocation| allocation.segment == 0 && allocation.first_bus == 0)
    });
    let ports = ports_present();
    let mechanism = match ecam {
        Some(allocation) => Mechanism::Ecam(allocation),
        None if ports => Mechanism::Ports,
        None => return None,
    };

    let mut scan = Scan {
        mechanism,
        ports,
        devices: Vec::new(),
        seen: [0; 4],
        buses: 0,
        cross_checked: 0,
        agreed: 0,
    };
    scan.bus(0);

    // Every memory BAR becomes a region a capability can name.
    let mut regions = Vec::new();
    for (index, device) in scan.devices.iter().enumerate() {
        for (bar, decoded) in device.bars.iter().enumerate() {
            let Some((physical, len)) = decoded.memory() else {
                continue;
            };
            regions.push(Region {
                device: index,
                bar: bar as u8,
                physical,
                len,
            });
        }
    }

    let summary = Summary {
        mechanism,
        functions: scan.devices.len(),
        buses: scan.buses,
        cross_checked: scan.cross_checked,
        agreed: scan.agreed,
    };
    without_interrupts(|| {
        let mut bus = BUS.lock();
        bus.devices = scan.devices;
        bus.regions = regions;
    });
    Some(summary)
}

/// Pages of ECAM the scan mapped.
pub fn ecam_pages() -> u64 {
    ECAM_PAGES.load(Ordering::Relaxed)
}

/// One function, by its place in the table.
///
/// For the kernel's boot code, which decides what to hand to whom. It is a
/// description, not authority: a task gets at a device through a capability
/// or not at all.
pub fn device(index: usize) -> Option<Device> {
    without_interrupts(|| BUS.lock().devices.get(index).copied())
}

/// One region, by its place in the table.
pub fn region(index: usize) -> Option<Region> {
    without_interrupts(|| BUS.lock().regions.get(index).copied())
}

/// What the device in `slot` says it is. Requires READ.
pub fn describe(slot: CapPtr) -> Result<Device, CapError> {
    let index = cap::pci_device_of(slot, Rights::READ)?;
    device(index).ok_or(CapError::Stale)
}

/// Read a register of the device in `slot`. Requires READ.
///
/// `offset` and `width` are whatever the caller said, and are checked here:
/// a width that is not 1, 2 or 4, or an access that is misaligned or runs
/// off the end of configuration space, is [`CapError::Invalid`].
pub fn config_read(slot: CapPtr, offset: u64, width: u64) -> Result<u32, CapError> {
    let device = describe(slot)?;
    let width = Width::from_bytes(width).ok_or(CapError::Invalid)?;
    if !corrode_pci::in_bounds(offset, width, device.function.size()) {
        return Err(CapError::Invalid);
    }
    Ok(device.function.read(offset as u16, width))
}

/// Write a register of the device in `slot`. Requires WRITE.
///
/// Holding the device is not enough for every register. The BARs, the
/// interrupt capabilities and anything in a bridge stay with the kernel, and
/// a write to one of those is [`CapError::Invalid`] -- the capability is
/// good, the request is not.
pub fn config_write(slot: CapPtr, offset: u64, width: u64, value: u64) -> Result<(), CapError> {
    let index = cap::pci_device_of(slot, Rights::WRITE)?;
    let device = device(index).ok_or(CapError::Stale)?;
    let width = Width::from_bytes(width).ok_or(CapError::Invalid)?;
    if !corrode_pci::in_bounds(offset, width, device.function.size()) {
        return Err(CapError::Invalid);
    }
    if !device.policy.allows(offset as u16, width) {
        return Err(CapError::Invalid);
    }
    device
        .function
        .write(offset as u16, width, corrode_pci::truncate(value, width));
    Ok(())
}
