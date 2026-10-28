//! The SGI Octane (IP30): its processor-side physical bus. HEART, the
//! Crossbow and the widgets behind it, low memory, and SDRAM where HEART's
//! bank descriptors put it. `ip30_machine` builds and drives it.
//!
//! The map, from OpenBSD's `ip30.h` and Linux's `ip30-xtalk.c` (also
//! `docs/Octane/ip30-address-map.md`):
//!
//! | Range | What |
//! |---|---|
//! | `0x0000_0000`, 16 KiB | low memory, the first 16 KiB of SDRAM (`heart_mem::LowAlias`) |
//! | `0x0ff0_0000` | HEART's processor-interface registers |
//! | `0x1000_0000 + (w << 24)` | widget `w`'s 16 MiB small window |
//! | `0x2000_0000` up | SDRAM through HEART's bank descriptors (`heart_mem::OctaneMemory`); its first 16 KiB bus-error here (`heart_mem::SdramGuard`) |
//! | `w << 36` | widget `w`'s 64 GiB large window (8 to 15) |
//!
//! Widget 0 is the Crossbow, 8 HEART, 15 the onboard Bridge (whose window
//! holds the boot flash at 0x1fc0_0000, the reset vector), 9 to 12 the
//! graphics ports.
//!
//! The decode is the machine-independent [`Physical`], as on the Indy and
//! the O2: a `SparseDeviceMap` of 64 KiB slots over the R10000's 40-bit
//! physical space, each pointing at the device that answers it, and the
//! ppmem window over SDRAM. `OctaneMemory` makes it and lays SDRAM out in it;
//! this module maps the chips.
//!
//! Set IRIS_IP30_TRACE=1 to log accesses nothing answers as they happen.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::dev::heart_mem::{OctaneMemory, GUARD};
use crate::physical::Physical;
use crate::ppmem::devmap::{PHYS_MASK, SLOT_SHIFT};
use crate::traits::{BusDevice, BusRead16, BusRead32, BusRead64, BusRead8, BUS_ERR, BUS_OK};

/// Widget numbers on the Octane's Crossbow.
pub const WIDGET_XBOW: u8 = 0x0;
pub const WIDGET_HEART: u8 = 0x8;
pub const WIDGET_BRIDGE: u8 = 0xf;
/// The four ports graphics and expansion boards take.
pub const WIDGET_GFX_FIRST: u8 = 0x9;

/// Widget `w`'s small window: 16 MiB at `0x1000_0000 + (w << 24)`.
pub const fn widget_small_base(w: u8) -> u64 {
    0x1000_0000 | (w as u64) << 24
}
pub const WIDGET_SMALL_SIZE: u64 = 0x0100_0000;
/// Widget `w`'s large window (widgets 8 to 15): 64 GiB at `w << 36`.
pub const fn widget_large_base(w: u8) -> u64 {
    (w as u64) << 36
}
pub const WIDGET_LARGE_SIZE: u64 = 1 << 36;
/// Where SDRAM space ends: the first large window. Descriptors reach no
/// further than 18.5 GiB of it.
pub const SDRAM_SPACE_END: u64 = widget_large_base(8);
/// The boot flash window, both chip selects, as the processor addresses it.
pub const BOOT_FLASH: std::ops::Range<u64> = 0x1fc0_0000..0x2000_0000;

/// Where the CPU was at the end of its last batch, for the unmapped log.
/// The machine's batch hook stores it.
pub static LAST_PC: AtomicU64 = AtomicU64::new(0);

/// What answers where nothing on the Octane's bus does: a bus error for
/// every access, the default device of its `Physical`. The first 64
/// distinct addresses are kept, as `(addr, is_write, pc)`, for `ip30
/// unmapped`; once full, a miss costs one relaxed load, so a guest polling
/// an absent device does not lock and scan the list each time. Low memory's
/// tail, the guard over SDRAM's first 16 KiB and a widget's windows while
/// its link is down note here too. A device's own refusal (a register it
/// does not have, the flash refusing a write) is not a miss.
#[derive(Default)]
pub struct Misses {
    list: Mutex<Vec<(u64, bool, u64)>>,
    full: AtomicBool,
}

impl Misses {
    #[inline]
    pub fn note(&self, addr: u64, write: bool) {
        if !self.full.load(Ordering::Relaxed) {
            self.record(addr, write);
        }
    }

    #[cold]
    fn record(&self, addr: u64, write: bool) {
        let pc = LAST_PC.load(Ordering::Relaxed);
        let mut u = self.list.lock();
        if !u.iter().any(|&(a, w, _)| a == addr && w == write) {
            if std::env::var_os("IRIS_IP30_TRACE").is_some() {
                eprintln!("ip30: unmapped {} at {addr:#x} (pc {pc:#x})", if write { "write" } else { "read" });
            }
            u.push((addr, write, pc));
            if u.len() == 64 {
                self.full.store(true, Ordering::Relaxed);
            }
        }
    }

    /// The misses kept, in the order they came.
    pub fn list(&self) -> Vec<(u64, bool, u64)> {
        self.list.lock().clone()
    }
}

impl BusDevice for Misses {
    fn read8(&self, a: u64) -> BusRead8 { self.note(a, false); BusRead8::err() }
    fn read16(&self, a: u64) -> BusRead16 { self.note(a, false); BusRead16::err() }
    fn read32(&self, a: u64) -> BusRead32 { self.note(a, false); BusRead32::err() }
    fn read64(&self, a: u64) -> BusRead64 { self.note(a, false); BusRead64::err() }
    fn write8(&self, a: u64, _: u8) -> u32 { self.note(a, true); BUS_ERR }
    fn write16(&self, a: u64, _: u16) -> u32 { self.note(a, true); BUS_ERR }
    fn write32(&self, a: u64, _: u32) -> u32 { self.note(a, true); BUS_ERR }
    fn write64(&self, a: u64, _: u64) -> u32 { self.note(a, true); BUS_ERR }
}

/// The Octane's processor-side physical bus: HEART's [`Physical`] with the
/// chips mapped on it, and the widgets the Crossbow switches.
///
/// Devices see full physical addresses (masked to 40 bits), not offsets,
/// and mask to their own window. A device that fills only part of a slot
/// answers the rest of it itself.
///
/// Built in three steps: `new` over the memory (which has already put low
/// memory and the guard on the bus), then the machine registers devices
/// (`map`, `attach_widget`), then `init` points every switched window at
/// its link as it stands, and from then on the CPU may run. Table changes
/// after that (`set_widget_link`, a descriptor write) replace whole slots;
/// readers on device threads see the old slot or the new one, never a torn
/// one (`docs/devmap-design.md`).
///
/// This is what the CPU is given, uncached and as its cache's memory, and
/// what the JIT reads generations through. Width accesses go straight to
/// `Physical`. The direct paths (`mem_ptr`, `read_block`, `write_block`,
/// `gen_ptr`) do too, except over the guard, where `Physical` would take the
/// window: there they go to `SdramGuard`, so the bus path faults the guard
/// whatever the window maps.
pub struct Ip30Bus {
    phys: Arc<Physical>,
    ram: Arc<OctaneMemory>,
    /// The widgets on the Crossbow's ports 9 to 15, and whether each also
    /// takes its large window.
    widgets: Mutex<[Option<(Arc<dyn BusDevice>, bool)>; 16]>,
}

impl Ip30Bus {
    /// The bus `ram` is on, with nothing but memory mapped yet.
    pub fn new(ram: Arc<OctaneMemory>) -> std::io::Result<Self> {
        Ok(Self { phys: ram.phys().clone(), ram, widgets: Mutex::new(Default::default()) })
    }

    /// The machine-independent bus, for the monitor's `phys`.
    pub fn phys(&self) -> &Arc<Physical> {
        &self.phys
    }

    /// Put `dev` at physical `start..end` (`end` exclusive, rounded up to a
    /// 64 KiB slot): HEART's PIU registers, the Crossbow's window (widget 0)
    /// and HEART's widget registers (widget 8), none of which a link gates.
    /// Takes effect at once; a later `map` over the same slots wins. The bus
    /// keeps `dev`.
    pub fn map(&self, start: u64, end: u64, dev: Arc<dyn BusDevice>) {
        self.phys.map(start, end, dev, &Self::name(start));
    }

    /// What `phys map` calls the device at `start`. The bus keeps the first
    /// name a device is mapped under, so HEART's widget registers show as
    /// HEART.
    fn name(start: u64) -> String {
        match start {
            crate::dev::heart::PIU_BASE => "heart".into(),
            s if (0x1000_0000..0x2000_0000).contains(&s) => format!("widget {}", (s >> 24) & 0xf),
            s => format!("device at {s:#x}"),
        }
    }

    /// Put `dev` on the Crossbow's port `w` (9 to 15): its small window and,
    /// with `large`, its large window. Routed only while the link is alive;
    /// until `set_widget_link(w, true)` both windows bus-error.
    pub fn attach_widget(&self, w: u8, dev: Arc<dyn BusDevice>, large: bool) {
        assert!((9..16).contains(&w), "ip30: widget {w} is not a port the Crossbow switches");
        // Mapping it is how the bus comes to keep it; the link puts the
        // windows where they belong.
        let small = widget_small_base(w);
        self.phys.map(small, small + WIDGET_SMALL_SIZE, dev.clone(), &format!("widget {w}"));
        self.widgets.lock()[w as usize] = Some((dev, large));
        self.set_widget_link(w, false);
    }

    /// Point every attached widget's windows as its link stands (dead).
    /// Call once, after the devices are registered and before the CPU runs.
    pub fn init(&self) {
        for w in 9..16 {
            self.set_widget_link(w, false);
        }
    }

    /// HEART's bank descriptors changed: memory decodes against them from
    /// now on. HEART calls this (through the machine) from the descriptor
    /// write, on the CPU thread.
    pub fn remap_banks(&self, memcfg: [u32; 4]) {
        self.ram.set_memcfg(memcfg);
    }

    /// The Crossbow brought widget `w`'s link up or took it down: point its
    /// windows at the widget or at the default device, a bus error. The
    /// Crossbow's link-change callback lands here, outside the Crossbow's
    /// own lock, on whichever thread touched it. Ports other than 9 to 15
    /// have no windows to switch.
    pub fn set_widget_link(&self, w: u8, alive: bool) {
        if !(9..16).contains(&w) {
            return;
        }
        let none = self.phys.default_entry();
        let (dev, large) = match &self.widgets.lock()[w as usize] {
            Some((d, large)) if alive => (Arc::as_ptr(d), *large),
            Some((_, large)) => (none, *large),
            None => (none, false),
        };
        self.phys.fill_slots(widget_small_base(w) >> SLOT_SHIFT, WIDGET_SMALL_SIZE >> SLOT_SHIFT, dev);
        if large {
            self.phys.fill_slots(widget_large_base(w) >> SLOT_SHIFT, WIDGET_LARGE_SIZE >> SLOT_SHIFT, dev);
        }
    }

    /// Memory as the bus masters (the Bridge and its `PciDma`s, IMPACT's
    /// HQ4) reach it: `OctaneMemory::dma_view`.
    pub fn memory(&self) -> Arc<dyn BusDevice> {
        self.ram.dma_view()
    }

    /// Accesses nothing answered, first 64 distinct, as `(addr, is_write,
    /// pc)`.
    pub fn unmapped(&self) -> Vec<(u64, bool, u64)> {
        self.ram.misses().list()
    }

    /// Whether `a` falls in the guard, whose direct paths `Physical` would
    /// take through the window.
    #[inline(always)]
    fn guarded(a: u64) -> bool {
        GUARD.contains(&(a & PHYS_MASK))
    }
}

/// Every access forwarded to `Physical`, the direct paths over the guard
/// excepted (see [`Ip30Bus`]).
impl BusDevice for Ip30Bus {
    #[inline] fn read8(&self, a: u64) -> BusRead8 { self.phys.read8(a) }
    #[inline] fn read16(&self, a: u64) -> BusRead16 { self.phys.read16(a) }
    #[inline] fn read32(&self, a: u64) -> BusRead32 { self.phys.read32(a) }
    #[inline] fn read64(&self, a: u64) -> BusRead64 { self.phys.read64(a) }
    #[inline] fn write8(&self, a: u64, v: u8) -> u32 { self.phys.write8(a, v) }
    #[inline] fn write16(&self, a: u64, v: u16) -> u32 { self.phys.write16(a, v) }
    #[inline] fn write32(&self, a: u64, v: u32) -> u32 { self.phys.write32(a, v) }
    #[inline] fn write64(&self, a: u64, v: u64) -> u32 { self.phys.write64(a, v) }
    #[inline] fn write64_masked(&self, a: u64, v: u64, m: u64) -> u32 { self.phys.write64_masked(a, v, m) }
    #[inline] fn dma_read64(&self, a: u64) -> BusRead64 { self.phys.dma_read64(a) }
    #[inline] fn dma_write64(&self, a: u64, v: u64) -> u32 { self.phys.dma_write64(a, v) }
    #[inline] fn dma_read64_bulk(&self, a: u64, out: &mut [u64]) -> u32 { self.phys.dma_read64_bulk(a, out) }
    #[inline] fn dma_write64_bulk(&self, a: u64, vals: &[u64]) -> u32 { self.phys.dma_write64_bulk(a, vals) }
    #[inline]
    fn mem_ptr(&self, a: u64) -> Option<*const u64> {
        if Self::guarded(a) {
            return self.ram.guard().mem_ptr(a & PHYS_MASK);
        }
        self.phys.mem_ptr(a)
    }
    #[inline]
    fn read_block(&self, a: u64, buf: &mut [u64]) -> u32 {
        if Self::guarded(a) {
            return self.ram.guard().read_block(a & PHYS_MASK, buf);
        }
        self.phys.read_block(a, buf)
    }
    #[inline]
    fn write_block(&self, a: u64, buf: &[u64]) -> u32 {
        if Self::guarded(a) {
            return self.ram.guard().write_block(a & PHYS_MASK, buf);
        }
        self.phys.write_block(a, buf)
    }
    #[cfg(feature = "jitv2")]
    #[inline]
    fn gen_ptr(&self, a: u64) -> *const AtomicU64 {
        if Self::guarded(a) {
            return self.ram.guard().gen_ptr(a & PHYS_MASK);
        }
        self.phys.gen_ptr(a)
    }
}

/// An R10000 secondary cache line.
const CACHE_LINE: u64 = 128;

/// The CPU's cached view of the bus: what the cache model reaches, while
/// uncached accesses go to the bus directly. Bus masters never see it.
///
/// The PROM runs cached from the boot flash and keeps its initialised data
/// there, storing to its own variables at their flash addresses. On an
/// R10000 a cached store allocates the line in the write-back cache, dirty
/// exclusive, and later cached loads hit it; the PROM never has the line
/// written back, the same pattern as firmware that keeps its working data
/// in the cache before memory exists. Nothing reaches the Bridge, whose
/// flash port refuses anything but a 16-bit write.
///
/// Approximation: iris's R10000 cache model passes every cached access to
/// memory, so the dirty lines live here instead. The first cached store to a
/// flash line copies the line in; from then on cached loads and stores of it
/// use the copy, which is never written back or evicted. Uncached accesses
/// always reach the flash. Reset empties it. What the hardware does if such
/// a line is ever written back is not modelled. Outside the flash window
/// every access, the fast paths included, goes straight to the bus.
pub struct CachedFlashLines {
    bus: Arc<dyn BusDevice>,
    lines: Mutex<std::collections::HashMap<u64, [u8; CACHE_LINE as usize]>>,
}

impl CachedFlashLines {
    pub fn new(bus: Arc<dyn BusDevice>) -> Self {
        Self { bus, lines: Mutex::new(std::collections::HashMap::new()) }
    }

    /// Forget every line, as a reset loses the cache's contents.
    pub fn clear(&self) {
        self.lines.lock().clear();
    }

    /// Bytes `addr..addr + n` of a held line, big-endian, or `None` if the
    /// line is not held.
    fn load(&self, addr: u64, n: usize) -> Option<u64> {
        if !BOOT_FLASH.contains(&addr) {
            return None;
        }
        let lines = self.lines.lock();
        let line = lines.get(&(addr & !(CACHE_LINE - 1)))?;
        let at = (addr & (CACHE_LINE - 1)) as usize;
        Some(line[at..at + n].iter().fold(0, |v, &b| v << 8 | b as u64))
    }

    /// Store into the line, filling it from the flash first. `None`: not a
    /// flash address.
    fn store(&self, addr: u64, n: usize, val: u64) -> Option<u32> {
        if !BOOT_FLASH.contains(&addr) {
            return None;
        }
        let base = addr & !(CACHE_LINE - 1);
        let mut lines = self.lines.lock();
        let line = match lines.entry(base) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let mut words = [0u64; (CACHE_LINE / 8) as usize];
                let r = self.bus.read_block(base, &mut words);
                if r != BUS_OK {
                    return Some(r);
                }
                let mut line = [0u8; CACHE_LINE as usize];
                for (chunk, w) in line.chunks_mut(8).zip(words) {
                    chunk.copy_from_slice(&w.to_be_bytes());
                }
                e.insert(line)
            }
        };
        let at = (addr & (CACHE_LINE - 1)) as usize;
        line[at..at + n].copy_from_slice(&val.to_be_bytes()[8 - n..]);
        Some(BUS_OK)
    }

    /// Whether `n` doublewords at `addr` touch the flash window.
    fn in_flash(addr: u64, n: usize) -> bool {
        addr < BOOT_FLASH.end && addr.saturating_add(8 * n as u64) > BOOT_FLASH.start
    }
}

macro_rules! cached_access {
    ($read:ident, $write:ident, $r:ident, $t:ty, $n:expr) => {
        fn $read(&self, addr: u64) -> $r {
            match self.load(addr, $n) {
                Some(v) => $r::ok(v as $t),
                None => self.bus.$read(addr),
            }
        }
        fn $write(&self, addr: u64, val: $t) -> u32 {
            self.store(addr, $n, val as u64).unwrap_or_else(|| self.bus.$write(addr, val))
        }
    };
}

impl BusDevice for CachedFlashLines {
    cached_access!(read8, write8, BusRead8, u8, 1);
    cached_access!(read16, write16, BusRead16, u16, 2);
    cached_access!(read32, write32, BusRead32, u32, 4);
    cached_access!(read64, write64, BusRead64, u64, 8);

    fn mem_ptr(&self, addr: u64) -> Option<*const u64> {
        if BOOT_FLASH.contains(&addr) {
            return None;
        }
        self.bus.mem_ptr(addr)
    }

    /// A line fill from the flash goes word by word, so held lines answer.
    fn read_block(&self, addr: u64, buf: &mut [u64]) -> u32 {
        if !Self::in_flash(addr, buf.len()) {
            return self.bus.read_block(addr, buf);
        }
        for (i, slot) in buf.iter_mut().enumerate() {
            let r = self.read64(addr + 8 * i as u64);
            if r.status != BUS_OK {
                return r.status;
            }
            *slot = r.data;
        }
        BUS_OK
    }

    fn write_block(&self, addr: u64, buf: &[u64]) -> u32 {
        if !Self::in_flash(addr, buf.len()) {
            return self.bus.write_block(addr, buf);
        }
        for (i, &v) in buf.iter().enumerate() {
            let s = self.write64(addr + 8 * i as u64, v);
            if s != BUS_OK {
                return s;
            }
        }
        BUS_OK
    }

    #[cfg(feature = "jitv2")]
    fn gen_ptr(&self, addr: u64) -> *const AtomicU64 {
        self.bus.gen_ptr(addr)
    }
}

/// Cross-device tests: the chips wired as the machine wires them, no CPU.
#[cfg(test)]
mod tests;
