//! PCI: the configuration header every function has, and the trait a
//! function implements to sit behind a host bridge.
//!
//! The bridge (the root complex) owns everything machine-specific: how
//! configuration cycles are generated, where the CPU sees PCI memory, byte
//! lanes, and which interrupt input each slot's pins are wired to. A
//! function only sees configuration registers, accesses to its BARs, and an
//! [`IrqLine`] for its pin.

pub mod bus;
pub mod decode;
pub mod intx;
pub mod pci_to_pci_bridge;
#[cfg(test)]
pub(crate) mod testing;

pub use bus::PciBus;
pub use decode::{BarRun, BarRunSink, CpuWindow};
pub use intx::{PciUpstream, PciWiring, Wire};

use std::sync::{Arc, Mutex, OnceLock};

use crate::irq::IrqLine;
use crate::snapshot::{get_field, load_u32_slice, u32_slice_to_toml};

/// Command register bits.
pub const PCI_COMMAND_IO: u32 = 0x1;
pub const PCI_COMMAND_MEMORY: u32 = 0x2;

/// Interrupt pins, as register 0x3d holds them.
pub const PCI_INTA: u8 = 1;

/// What one base address register decodes, and how much.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bar {
    Unused,
    /// I/O space, this many bytes (a power of two).
    Io(u32),
    /// 32-bit memory space, this many bytes (a power of two).
    Mem(u32),
    /// 64-bit memory space, this many bytes (a power of two, under 4 GiB).
    /// The BAR takes two header dwords: the next entry of the array must be
    /// [`Bar::Unused`], and the header makes it the upper half. Both host
    /// bridges generate 32-bit PCI addresses, so a base programmed above 4
    /// GiB (a non-zero upper half) is not reachable and decodes nothing.
    Mem64(u32),
    /// The upper dword of the [`Bar::Mem64`] before it; set by
    /// [`PciConfig::new`], never given.
    MemUpper,
}

impl Bar {
    fn size(self) -> u32 {
        match self {
            Bar::Unused | Bar::MemUpper => 0,
            Bar::Io(n) | Bar::Mem(n) | Bar::Mem64(n) => n,
        }
    }
}

/// Which address space an access is in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Space {
    Io,
    Mem,
}

/// One function's type 0 configuration header.
///
/// Enough of it to be discovered and configured: identity, class, command
/// and status, the interrupt pin, and base address registers that answer a
/// size probe. Writing all ones to a BAR and reading it back is how firmware
/// learns how much space a device wants, so a BAR that stores whatever it is
/// given reports a size of four gigabytes and the bus layout falls apart.
pub struct PciConfig {
    regs: Mutex<[u32; 64]>,
    bars: [Bar; 6],
    /// The registers as the function powers up, for [`PciConfig::reset`].
    power_on: [u32; 64],
    /// Dwords `16..caps_end` hold a capability list, which firmware cannot
    /// overwrite. 0 when the function has none.
    caps_end: usize,
    /// A type 1 header (PCI-to-PCI bridge): no BARs, and dwords 6 to 12 are
    /// bus numbers and forwarding windows.
    type1: bool,
}

impl PciConfig {
    /// `class` is the 24-bit class/subclass/interface, as it sits in the top
    /// three bytes of register 0x08. `pin` is 0 for none, or [`PCI_INTA`]
    /// and on; the line register is left for firmware to assign.
    pub fn new(vendor: u16, device: u16, class: u32, revision: u8, bars: [Bar; 6], pin: u8) -> Self {
        let mut bars = bars;
        for i in 0..5 {
            if let Bar::Mem64(_) = bars[i] {
                assert!(bars[i + 1] == Bar::Unused, "a 64-bit BAR takes the next one");
                bars[i + 1] = Bar::MemUpper;
            }
        }
        assert!(!matches!(bars[5], Bar::Mem64(_)), "a 64-bit BAR needs a BAR after it");
        let mut r = [0u32; 64];
        r[0] = ((device as u32) << 16) | vendor as u32;
        r[2] = (class << 8) | revision as u32;
        r[15] = (pin as u32) << 8;
        for (i, b) in bars.iter().enumerate() {
            match b {
                Bar::Io(_) => r[4 + i] = 1,
                // Memory type 10b: decodes anywhere in 64-bit space.
                Bar::Mem64(_) => r[4 + i] = 0x4,
                _ => {}
            }
        }
        Self { regs: Mutex::new(r), bars, power_on: r, caps_end: 0, type1: false }
    }

    /// A PCI-to-PCI bridge's type 1 header: class 0604, header type 1, no
    /// BARs and no interrupt pin. Dwords 6 (primary, secondary and
    /// subordinate bus numbers), 7 (16-bit I/O base and limit, bits 15:12 of
    /// each), 8 (memory base and limit, bits 31:20) and 9 (32-bit
    /// prefetchable memory) store what firmware writes, to the granularity
    /// the register has; dwords 10 to 12, the 64-bit and 32-bit-I/O upper
    /// halves, and dword 14, the expansion ROM base, read zero. The bridge
    /// control register is the top half of dword 15. Everything resets to
    /// zero, and the windows forward nothing until the command register
    /// enables them.
    pub fn bridge(vendor: u16, device: u16, revision: u8) -> Self {
        let mut c = Self::new(vendor, device, 0x06_0400, revision, [Bar::Unused; 6], 0);
        c.type1 = true;
        let r = c.regs.get_mut().unwrap();
        r[3] = 1 << 16;
        c.power_on = *r;
        c
    }

    /// Whether this type 1 header forwards the range `base..base + size` of
    /// `space` to its secondary bus: the range lies wholly inside one
    /// enabled window (I/O, memory or prefetchable memory). A range that
    /// straddles a window's edge is not forwarded; what a bridge does with
    /// one is not defined.
    pub fn forwards(&self, space: Space, base: u32, size: u32) -> bool {
        let r = self.regs.lock().unwrap();
        let (lo, hi) = (base as u64, base as u64 + size as u64 - 1);
        let inside = |wlo: u64, whi: u64| wlo <= whi && wlo <= lo && hi <= whi;
        match space {
            Space::Io => {
                r[1] & PCI_COMMAND_IO != 0 && inside(((r[7] & 0xf0) as u64) << 8, ((r[7] >> 8 & 0xf0) as u64) << 8 | 0xfff)
            }
            Space::Mem => {
                r[1] & PCI_COMMAND_MEMORY != 0 && {
                    let window = |dword: u32| (((dword & 0xfff0) as u64) << 16, ((dword >> 16 & 0xfff0) as u64) << 16 | 0xf_ffff);
                    let (mem, pf) = (window(r[8]), window(r[9]));
                    inside(mem.0, mem.1) || inside(pf.0, pf.1)
                }
            }
        }
    }

    /// Give the function a capability list: `caps` are the raw dwords of the
    /// list, which starts at dword `first` (at least 16, the end of the
    /// header). They are read-only and survive a reset. The header's
    /// capability pointer and status bit are set to match.
    pub fn with_capabilities(mut self, first: usize, caps: &[u32]) -> Self {
        assert!(first >= 16 && first + caps.len() <= 64, "capabilities must fit in the 256-byte header");
        let r = self.regs.get_mut().unwrap();
        for (i, &d) in caps.iter().enumerate() {
            r[first + i] = d;
        }
        // Capabilities pointer (byte offset) in the low byte of dword 13.
        r[13] = (first * 4) as u32;
        self.power_on = *r;
        self.caps_end = first + caps.len();
        self
    }

    /// RST#: back to the power-up header. The command register clears, so
    /// the function stops decoding, and the BARs and line forget what
    /// firmware assigned.
    pub fn reset(&self) {
        let mut r = self.regs.lock().unwrap();
        // The subsystem identity is the board's, set once after
        // construction (IOC3's selects what a guest instantiates).
        let subsystem = r[11];
        *r = self.power_on;
        r[11] = subsystem;
    }

    /// Register `reg` (a dword index, 0..64).
    pub fn read(&self, reg: usize) -> u32 {
        let r = self.regs.lock().unwrap();
        match reg {
            // Status: a medium-speed device with no errors to report.
            // A function with a capability list also sets status bit 4.
            1 => (r[1] & 0xffff) | 0x0200_0000 | if r[13] != 0 { 0x0010_0000 } else { 0 },
            _ => r[reg],
        }
    }

    pub fn write(&self, reg: usize, val: u32) {
        let mut r = self.regs.lock().unwrap();
        match reg {
            // Identity and class are read-only.
            0 | 2 => {}
            // Cache line size and latency timer are stored; the header type
            // and BIST bytes above them are not.
            3 => r[3] = (r[3] & 0xffff_0000) | (val & 0xffff),
            // Command; status bits are write-one-to-clear and none are set.
            1 => r[1] = val & 0xffff,
            // A bridge's bus numbers and forwarding windows. The low nibbles
            // of the base and limit registers are read-only zero (16-bit I/O
            // and 32-bit memory), which also makes the upper halves of the
            // prefetchable and I/O windows (dwords 10 to 12) read-only zero;
            // the I/O secondary status is not kept.
            6 if self.type1 => r[reg] = val,
            10..=12 if self.type1 => {}
            7 if self.type1 => r[reg] = val & 0x0000_f0f0,
            8 | 9 if self.type1 => r[reg] = val & 0xfff0_fff0,
            // A type 1 header's expansion ROM base is dword 14, and the
            // bridge has no ROM either.
            14 if self.type1 => {}
            4..=9 => {
                let bar = self.bars[reg - 4];
                r[reg] = match bar {
                    Bar::Unused => 0,
                    // The upper half of a 64-bit base takes every bit.
                    Bar::MemUpper => val,
                    // Address bits above the size take the write; those
                    // below read back as zero, which tells firmware the
                    // size. The type bits stay what the BAR is.
                    Bar::Io(n) => (val & !(n - 1)) | 1,
                    Bar::Mem(n) => val & !(n - 1),
                    Bar::Mem64(n) => (val & !(n - 1)) | 0x4,
                };
            }
            // Expansion ROM base: no function has a ROM, so the register is
            // unimplemented and reads zero (PCI 2.2, 6.1 and 6.2.5.2). A size
            // probe that read back its ones would have firmware map a ROM
            // that is not there.
            12 => {}
            // The capability list is read-only.
            16.. if reg < self.caps_end => {}
            // Interrupt pin is read-only; the line is firmware's scratch.
            15 => r[15] = (r[15] & 0xff00) | (val & !0xff00),
            _ => r[reg] = val,
        }
    }

    /// The base address firmware programmed into BAR `i`, type bits masked.
    pub fn bar_base(&self, i: usize) -> u32 {
        let r = self.regs.lock().unwrap();
        let v = r[4 + i];
        match self.bars[i] {
            Bar::Io(_) => v & !3,
            // Above 4 GiB: out of reach of a 32-bit PCI address.
            Bar::Mem64(_) if r[5 + i] != 0 => 0,
            _ => v & !15,
        }
    }

    pub fn command(&self) -> u32 {
        self.regs.lock().unwrap()[1] & 0xffff
    }

    pub fn interrupt_pin(&self) -> u8 {
        (self.regs.lock().unwrap()[15] >> 8) as u8
    }

    /// The windows this function decodes: (space, BAR, base, size) for
    /// each BAR that has been assigned an address in a space the command
    /// register enables, in BAR order. A bridge caches these and rebuilds
    /// them after a configuration write.
    pub fn windows(&self) -> Vec<(Space, usize, u32, u32)> {
        let cmd = self.command();
        (0..6)
            .filter_map(|i| {
                let (space, size) = match self.bars[i] {
                    Bar::Io(n) if cmd & PCI_COMMAND_IO != 0 => (Space::Io, n),
                    Bar::Mem(n) | Bar::Mem64(n) if cmd & PCI_COMMAND_MEMORY != 0 => (Space::Mem, n),
                    _ => return None,
                };
                let base = self.bar_base(i);
                (base != 0).then_some((space, i, base, size))
            })
            .collect()
    }

    /// The BAR decoding `addr` in `space`, and the offset within it: only
    /// once the command register enables that space, and only for a BAR
    /// that has been assigned an address.
    pub fn decode(&self, space: Space, addr: u32) -> Option<(usize, u32)> {
        self.decode_with(space, addr, false)
    }

    /// [`Self::decode`], but a BAR left at zero decodes at zero: what a
    /// function does on a bus whose firmware enables it without moving it
    /// (the Octane's PROM reaches the RAD that way).
    pub fn decode_unmoved(&self, space: Space, addr: u32) -> Option<(usize, u32)> {
        self.decode_with(space, addr, true)
    }

    fn decode_with(&self, space: Space, addr: u32, zero_ok: bool) -> Option<(usize, u32)> {
        let enable = match space {
            Space::Io => PCI_COMMAND_IO,
            Space::Mem => PCI_COMMAND_MEMORY,
        };
        if self.command() & enable == 0 {
            return None;
        }
        (0..6).find_map(|i| {
            let bar = self.bars[i];
            let in_space = matches!((bar, space), (Bar::Io(_), Space::Io) | (Bar::Mem(_) | Bar::Mem64(_), Space::Mem));
            let base = self.bar_base(i);
            let size = bar.size();
            (in_space && (base != 0 || zero_ok) && addr.wrapping_sub(base) < size).then(|| (i, addr - base))
        })
    }
}

/// The header's registers. Identity, class and the BARs' sizes come from
/// the function, so a header only loads into the same kind of function.
impl crate::traits::Saveable for PciConfig {
    fn save_state(&self) -> toml::Value {
        let mut t = toml::map::Map::new();
        t.insert("regs".into(), u32_slice_to_toml(&*self.regs.lock().unwrap()));
        toml::Value::Table(t)
    }

    fn load_state(&self, v: &toml::Value) -> Result<(), String> {
        let Some(r) = get_field(v, "regs") else { return Ok(()) };
        let mut saved = [0u32; 64];
        load_u32_slice(r, &mut saved);
        let mut regs = self.regs.lock().unwrap();
        if saved[0] != regs[0] {
            return Err(format!("pci: saved header is for {:#010x}, this function is {:#010x}", saved[0], regs[0]));
        }
        *regs = saved;
        Ok(())
    }
}

/// What the bus hands a function when it is attached: its interrupt line
/// and its way to memory, each set once. A function that raises no
/// interrupt (RAD1) never has a line.
#[derive(Default)]
pub struct PciPort {
    irq: OnceLock<IrqLine>,
    dma: OnceLock<Arc<dyn PciDma>>,
}

impl PciPort {
    pub fn irq(&self) -> Option<&IrqLine> {
        self.irq.get()
    }

    pub fn dma(&self) -> Option<&Arc<dyn PciDma>> {
        self.dma.get()
    }

    /// For a function whose own `connect_*` does more than keep the value.
    pub fn set_irq(&self, line: IrqLine) {
        let _ = self.irq.set(line);
    }

    pub fn set_dma(&self, dma: Arc<dyn PciDma>) {
        let _ = self.dma.set(dma);
    }
}

/// A PCI function, as a host bridge sees it.
///
/// BAR accesses arrive with the offset and value the device sees: whatever
/// the bridge does to byte lanes it has already done.
pub trait PciFunction: Send + Sync {
    /// The function's configuration header.
    fn config(&self) -> &PciConfig;

    /// What a configuration read of dword `index` returns to the guest. The
    /// header as stored, unless the chip decodes fewer address lines than
    /// the bus gives it (IOC3 repeats its first 32 bytes).
    fn config_dword(&self, index: usize) -> u32 {
        self.config().read(index)
    }

    /// A read of `width` bytes (1, 2 or 4) at `off` within BAR `bar`.
    fn bar_read(&self, bar: usize, off: u32, width: usize) -> u32;

    fn bar_write(&self, bar: usize, off: u32, val: u32, width: usize);

    /// Where the bus leaves the function's interrupt line and its way to
    /// memory.
    fn port(&self) -> &PciPort;

    /// The bridge wires the function's interrupt pin to `line`.
    fn connect_interrupt(&self, line: IrqLine) {
        self.port().set_irq(line);
    }

    /// The bridge gives a function that masters the bus its way to memory.
    fn connect_dma(&self, dma: Arc<dyn PciDma>) {
        self.port().set_dma(dma);
    }

    /// The bus wrote configuration dword `reg` of this function's header and
    /// it changed from `old` to `new`. A PCI-to-PCI bridge acts on bridge
    /// control here; every other function has nothing to do.
    fn config_written(&self, _reg: usize, _old: u32, _new: u32) {}

    /// The bus behind this function, if it is a PCI-to-PCI bridge.
    fn secondary(&self) -> Option<&PciBus> {
        None
    }

    /// RST# reaches the function after the bus has reset its header: drop
    /// the interrupt line and return to power-on state. A function never
    /// resets its own header, so the bus's decode table cannot go stale.
    fn reset(&self) {}
}

/// Where a function sits on a bus: its device number and its function number.
/// Orders by device, then function, the order RST# visits them in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PciAddr {
    pub device: u8,
    pub function: u8,
}

/// How a bus master's access ended.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PciDmaStatus {
    Ok,
    /// Some of the bytes had no target: reads return zero for them and
    /// writes drop them. A bridge may also log it and raise its error
    /// interrupt.
    MasterAbort,
}

impl PciDmaStatus {
    pub fn is_ok(self) -> bool {
        self == PciDmaStatus::Ok
    }
}

/// Memory as a bus master reaches it. The bridge decides what a PCI address
/// is and in which order its bytes arrive; the function sees bytes at bus
/// addresses, as it would on the bus.
///
/// A function calls only the object `connect_dma` gave it and never learns
/// whether it is shared. The O2's MACE hands every function one object (its
/// translation is fixed); a bridge whose translation depends on the
/// mastering device, as the Octane's does, hands each its own.
pub trait PciDma: Send + Sync {
    /// `buf.len()` bytes from bus address `addr`, 64 bits wide for masters
    /// that use dual address cycles.
    fn dma_read(&self, addr: u64, buf: &mut [u8]) -> PciDmaStatus;

    /// `data` to bus address `addr`.
    fn dma_write(&self, addr: u64, data: &[u8]) -> PciDmaStatus;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scsi_like() -> PciConfig {
        PciConfig::new(0x9004, 0x8078, 0x0100_00, 1,
            [Bar::Io(0x100), Bar::Mem(0x1000), Bar::Unused, Bar::Unused, Bar::Unused, Bar::Unused], PCI_INTA)
    }

    /// Firmware learns how much space a device wants by writing all ones to
    /// a BAR and reading back which address bits stuck.
    #[test]
    fn save_load_round_trip() {
        use crate::traits::Saveable;
        let src = scsi_like();
        src.write(1, PCI_COMMAND_MEMORY | PCI_COMMAND_IO);
        src.write(4, 0x0000_1000);
        src.write(5, 0x8000_1000);
        src.write(15, 0x0000_0108);
        let v1 = src.save_state();
        let dst = scsi_like();
        dst.load_state(&v1).expect("load_state");
        assert_eq!(v1, dst.save_state(), "PciConfig save_state mismatch after load_state round-trip");
        assert_eq!(dst.bar_base(1), 0x8000_1000);
        let other = PciConfig::new(0x1234, 0x5678, 0, 0, [Bar::Unused; 6], 0);
        assert!(other.load_state(&v1).is_err(), "a header loads only into the same kind of function");
    }

    fn nvme_like() -> PciConfig {
        PciConfig::new(0x1b36, 0x0010, 0x01_0802, 1,
            [Bar::Mem64(0x2000), Bar::Unused, Bar::Unused, Bar::Unused, Bar::Unused, Bar::Unused], PCI_INTA)
    }

    /// A 64-bit BAR reads as memory type 10b, sizes through its lower dword,
    /// and keeps whatever its upper dword is given.
    #[test]
    fn a_64_bit_bar_probes_and_stores_both_halves() {
        let c = nvme_like();
        assert_eq!(c.read(4), 0x4, "64-bit memory, not prefetchable");
        c.write(4, 0xffff_ffff);
        c.write(5, 0xffff_ffff);
        assert_eq!(c.read(4), 0xffff_e004, "8 KiB, type bits kept");
        assert_eq!(c.read(5), 0xffff_ffff, "the upper half is plain storage");
        c.write(5, 0);
        c.write(4, 0x8000_2000);
        assert_eq!(c.bar_base(0), 0x8000_2000);
        assert_eq!(c.read(4), 0x8000_2004);
        c.write(6, 0xffff_ffff);
        assert_eq!(c.read(6), 0, "the BAR after the pair is its own");
    }

    #[test]
    fn a_64_bit_bar_decodes_below_4_gib_only() {
        let c = nvme_like();
        c.write(4, 0x8000_2000);
        c.write(1, PCI_COMMAND_MEMORY);
        assert_eq!(c.decode(Space::Mem, 0x8000_2010), Some((0, 0x10)));
        assert_eq!(c.windows(), vec![(Space::Mem, 0, 0x8000_2000, 0x2000)]);
        c.write(5, 1);
        assert_eq!(c.decode(Space::Mem, 0x8000_2010), None, "a base above 4 GiB cannot be reached");
        assert!(c.windows().is_empty());
        c.write(1, 0);
        c.reset();
        assert_eq!((c.read(4), c.read(5)), (4, 0), "reset forgets both halves");
    }

    #[test]
    fn a_64_bit_bar_saves_with_its_upper_half() {
        use crate::traits::Saveable;
        let src = nvme_like();
        src.write(4, 0x8000_2000);
        src.write(5, 3);
        let dst = nvme_like();
        dst.load_state(&src.save_state()).unwrap();
        assert_eq!((dst.read(4), dst.read(5)), (0x8000_2004, 3));
    }

    #[test]
    fn a_bar_reports_its_size_to_a_probe() {
        let c = scsi_like();
        assert_eq!(c.read(0), 0x8078_9004, "device and vendor id");
        c.write(4, 0xffff_ffff);
        assert_eq!(c.read(4), 0xffff_ff01, "256 bytes of I/O space");
        c.write(5, 0xffff_ffff);
        assert_eq!(c.read(5), 0xffff_f000, "4 KiB of memory space");
        // An unimplemented BAR reads back as zero however hard it is probed.
        c.write(6, 0xffff_ffff);
        assert_eq!(c.read(6), 0);
        c.write(5, 0x8000_1000);
        assert_eq!(c.bar_base(1), 0x8000_1000, "and then holds the address assigned to it");
    }

    /// No function has an expansion ROM, so the ROM base register is
    /// unimplemented: a size probe reads back zero, which tells firmware
    /// there is no ROM to map (PCI 2.2, 6.1 and 6.2.5.2).
    #[test]
    fn the_expansion_rom_register_reads_zero_when_probed() {
        let c = scsi_like();
        c.write(12, 0xffff_f800);
        assert_eq!(c.read(12), 0);
        c.write(12, 0x00b0_2001);
        assert_eq!(c.read(12), 0, "an assigned address does not stick either");
    }

    /// A function decodes a space only once it has been told it may, and
    /// only within the BAR it was assigned.
    #[test]
    fn a_function_claims_only_its_own_window_and_only_when_enabled() {
        let c = scsi_like();
        c.write(5, 0x8000_1000);
        assert_eq!(c.decode(Space::Mem, 0x8000_1000), None, "memory decoding is off until enabled");
        c.write(1, PCI_COMMAND_MEMORY);
        assert_eq!(c.decode(Space::Mem, 0x8000_1084), Some((1, 0x84)));
        assert_eq!(c.decode(Space::Mem, 0x8000_2000), None, "past the end of the BAR");
        assert_eq!(c.decode(Space::Io, 0x8000_1084), None, "and not in I/O space");
    }

    #[test]
    fn reset_returns_the_header_to_power_up() {
        let c = scsi_like();
        let fresh: Vec<u32> = (0..64).map(|i| c.read(i)).collect();
        c.write(1, PCI_COMMAND_MEMORY | PCI_COMMAND_IO);
        c.write(4, 0x0000_1000);
        c.write(5, 0x8000_1000);
        c.write(15, 0x0000_0009);
        c.reset();
        assert_eq!((0..64).map(|i| c.read(i)).collect::<Vec<_>>(), fresh);
        assert_eq!(c.decode(Space::Mem, 0x8000_1000), None);
        assert_eq!(c.interrupt_pin(), PCI_INTA, "the pin is wiring, not state");
    }

    /// A subsystem identity set after construction, as IOC3 sets its own,
    /// is the board's and survives a reset.
    #[test]
    fn reset_keeps_the_subsystem_identity() {
        let c = scsi_like();
        c.write(11, 0xc304_10a9);
        c.write(4, 0x0000_1000);
        c.reset();
        assert_eq!(c.read(11), 0xc304_10a9);
        assert_eq!(c.read(4), 1);
    }

    /// Firmware programs the latency timer and cache line size, and reads
    /// them back; the header type byte above them stays what it is.
    #[test]
    fn the_latency_timer_and_cache_line_size_are_stored() {
        let c = scsi_like();
        c.write(3, 0x00ff_2810);
        assert_eq!(c.read(3), 0x0000_2810);
        c.reset();
        assert_eq!(c.read(3), 0);
    }

    #[test]
    fn the_interrupt_pin_is_read_only() {
        let c = scsi_like();
        c.write(15, 0x0000_0009);
        assert_eq!(c.read(15) & 0xffff, 0x0109, "line 9 assigned, pin A kept");
        c.write(15, 0);
        assert_eq!(c.interrupt_pin(), PCI_INTA);
    }

    struct Func(PciConfig, PciPort);
    impl PciFunction for Func {
        fn config(&self) -> &PciConfig { &self.0 }
        fn port(&self) -> &PciPort { &self.1 }
        fn bar_read(&self, _: usize, off: u32, _: usize) -> u32 { off }
        fn bar_write(&self, _: usize, _: u32, _: u32, _: usize) {}
    }

    #[test]
    fn a_port_takes_its_line_and_dma_once() {
        struct Dma;
        impl PciDma for Dma {
            fn dma_read(&self, _: u64, _: &mut [u8]) -> PciDmaStatus { PciDmaStatus::Ok }
            fn dma_write(&self, _: u64, _: &[u8]) -> PciDmaStatus { PciDmaStatus::Ok }
        }
        let (a, b) = (crate::irq::LevelProbe::line(), crate::irq::LevelProbe::line());
        let f = Func(scsi_like(), PciPort::default());
        assert!(f.port().irq().is_none() && f.port().dma().is_none());
        f.connect_interrupt(a.1);
        f.connect_interrupt(b.1);
        f.connect_dma(Arc::new(Dma));
        f.port().irq().unwrap().set(true);
        assert!(a.0.level() && !b.0.level(), "the second line is ignored");
        assert!(f.port().dma().is_some());
        let rad = Func(scsi_like(), PciPort::default());
        assert!(rad.port().irq().is_none(), "a function with no interrupt never has a line");
    }

    #[test]
    fn pci_addr_orders_by_device_then_function() {
        let a = |device, function| PciAddr { device, function };
        let mut v = [a(3, 0), a(1, 1), a(1, 0)];
        v.sort();
        assert_eq!(v, [a(1, 0), a(1, 1), a(3, 0)]);
    }
}
