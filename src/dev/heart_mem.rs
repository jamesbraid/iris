//! Octane main memory: the DIMMs that are fitted, where HEART's bank
//! descriptors (MEMCFG) put them, and the alias of their start at physical 0.
//!
//! HEART has four banks (a DIMM pair each) and a 32-bit descriptor per
//! bank: bit 31 valid, bits 21:16 the span less one and bits 8:0 the base,
//! both in 32 MiB units from 0x2000_0000. When two descriptors claim an
//! address the higher-numbered bank answers.
//!
//! [`OctaneMemory`] owns the Octane's [`Physical`], whose RAM banks are the
//! fitted HEART banks, one store each (byte `o` at `host + (o ^ 3)`, the
//! layout every iris RAM keeps). A descriptor write lays the bus out again
//! as the O2's memory does: each 64 KiB slot of a 32 MiB unit points at the
//! bank that answers there (`Physical::bank_view`), at memory that reads
//! zero for a bank with nothing fitted, or at the bus's default, and the
//! 4 GiB ppmem window the CPU's cache and the JIT read maps the same units.
//! Memory a descriptor puts above 4 GiB is in the table only, so it is
//! reached through the bus.
//!
//! The first 16 KiB of SDRAM are the processor's physical 0..0x4000, and it
//! reaches them only there: its access to 0x2000_0000..+16 KiB is a bus
//! error, and SDRAM is reachable at its own address from 0x2000_4000. The
//! stock IDE diagnostics check exactly this. Two slots hold less than 64 KiB
//! of anything, so each has a device of its own that decodes the
//! descriptors itself: [`LowAlias`] at 0, the same bytes and the same jitv2
//! page generations, and [`SdramGuard`] at 0x2000_0000. Whether a bus master
//! reaches 0x2000_0000..+16 KiB is not known; the `dma_view` does, as the
//! same bytes. Public software uses physical 0x400 (the GDA) and 0x600
//! (MPCONF).
//!
//! Ported from hw/misc/sgi-heart.c in jamesbraid/qemu-sgi-origami.

use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::ip30::Misses;
use crate::physical::{Physical, RamBank};
use crate::ppmem::devmap::SLOT_SHIFT;
use crate::ppmem::{MappedMemory, PpMemSpace, WINDOW_SIZE};
use crate::traits::{BusDevice, BusRead16, BusRead32, BusRead64, BusRead8, BUS_ERR, BUS_OK};

/// Where SDRAM space starts. Descriptors count from here in [`UNIT`]s.
pub const RAM_BASE: u64 = 0x2000_0000;
/// The descriptors' unit, and the window's mapping granule.
pub const UNIT: u64 = 32 << 20;
pub const BANKS: usize = 4;
/// Low memory at physical 0: the first `LOW_SIZE` bytes of SDRAM, which the
/// processor reaches only there.
pub const LOW_SIZE: u64 = 0x4000;
/// The processor's bus error over the first 16 KiB of SDRAM space.
pub const GUARD: std::ops::Range<u64> = RAM_BASE..RAM_BASE + LOW_SIZE;
/// The page jitv2 tracks code by.
const PAGE: usize = 4096;
/// Units a descriptor can reach: a base of up to 511 and a span of up to 64.
const UNITS: usize = 0x200 + 0x40;
/// Decode-table slots per unit.
const SLOTS_PER_UNIT: u64 = UNIT >> SLOT_SHIFT;

const DESC_VALID: u32 = 0x8000_0000;

/// Where a byte offset into a fitted bank lands for an address `offset` into
/// a descriptor's span.
///
/// The stock PROM sizes a bank by writing through a temporary 2 GiB density-6
/// descriptor (`0x81bf0040`) and checking which address bits alias. Real
/// HEART's address wiring for that probe is not documented; this projection
/// is an approximation that makes each fitted size give the PROM the answer
/// it needs to pick the right descriptor. Every other descriptor wraps at
/// the bank's size. All of them keep the low 25 bits, so a 32 MiB unit maps
/// to one 32 MiB run of the bank.
pub fn bank_offset(descriptor: u32, capacity: u64, offset: u64) -> u64 {
    const MIB: u64 = 1 << 20;
    if descriptor & 0x803f_0000 == 0x803f_0000 && (descriptor >> 22) & 7 == 6 {
        match capacity {
            c if c == 64 * MIB => return (offset & (32 * MIB - 1)) | ((offset & (64 * MIB)) >> 1),
            c if c == 128 * MIB => {
                return (offset & (32 * MIB - 1)) | ((offset & (64 * MIB)) >> 1) | ((offset & (1024 * MIB)) >> 4)
            }
            c if c == 256 * MIB => return (offset & (128 * MIB - 1)) | ((offset & (256 * MIB)) >> 1),
            c if c == 512 * MIB => {
                return (offset & (128 * MIB - 1)) | ((offset & (256 * MIB)) >> 1) | ((offset & (1024 * MIB)) >> 2)
            }
            _ => {}
        }
    }
    offset % capacity
}

fn desc_base(d: u32) -> u64 {
    RAM_BASE + (d & 0x1ff) as u64 * UNIT
}

fn desc_span(d: u32) -> u64 {
    (((d >> 16) & 0x3f) as u64 + 1) * UNIT
}

/// Where a physical address in SDRAM space lands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decode {
    /// A byte offset into HEART bank `bank`'s DIMMs.
    Ram { bank: usize, off: usize },
    /// A valid descriptor for a bank with nothing fitted: from the
    /// processor, reads are zero and writes go nowhere.
    Absent,
    /// No descriptor claims it: a bus error.
    None,
}

/// What answers in one 32 MiB unit of SDRAM space, as the table holds it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Unit {
    None,
    Absent,
    /// HEART bank `bank`, from byte `src` of its store.
    Ram { bank: usize, src: u64 },
}

/// A byte of fitted memory: the HEART bank and the offset into it.
type Loc = (usize, usize);

// Host-word layout shared by the views: what `PpMemory` keeps, so an
// aligned word is a host u32 and a doubleword is two of them swapped
// (`rotate_left(32)`). Offsets are aligned to the access.
mod layout {
    #[inline(always)]
    pub unsafe fn ld8(h: *mut u8, o: usize) -> u8 {
        *h.add(o ^ 3)
    }
    #[inline(always)]
    pub unsafe fn ld16(h: *mut u8, o: usize) -> u16 {
        *(h as *const u16).add((o >> 1) ^ 1)
    }
    #[inline(always)]
    pub unsafe fn ld32(h: *mut u8, o: usize) -> u32 {
        *(h as *const u32).add(o >> 2)
    }
    #[inline(always)]
    pub unsafe fn ld64(h: *mut u8, o: usize) -> u64 {
        (*(h as *const u64).add(o >> 3)).rotate_left(32)
    }
    #[inline(always)]
    pub unsafe fn st8(h: *mut u8, o: usize, v: u8) {
        *h.add(o ^ 3) = v
    }
    #[inline(always)]
    pub unsafe fn st16(h: *mut u8, o: usize, v: u16) {
        *(h as *mut u16).add((o >> 1) ^ 1) = v
    }
    #[inline(always)]
    pub unsafe fn st32(h: *mut u8, o: usize, v: u32) {
        *(h as *mut u32).add(o >> 2) = v
    }
    #[inline(always)]
    pub unsafe fn st64(h: *mut u8, o: usize, v: u64) {
        *(h as *mut u64).add(o >> 3) = v.rotate_left(32)
    }
    #[inline(always)]
    pub unsafe fn st64_masked(h: *mut u8, o: usize, v: u64, m: u64) {
        let p = (h as *mut u64).add(o >> 3);
        let (v, m) = (v.rotate_left(32), m.rotate_left(32));
        *p = (*p & !m) | (v & m);
    }
}
use layout::*;

/// The four descriptors, published whole: a seqlock, so an access on any
/// thread decodes against one MEMCFG or the next, never half of each.
/// Writers are serialised by `OctaneMemory::placed`.
#[derive(Default)]
struct Placement {
    seq: AtomicU64,
    words: [AtomicU32; BANKS],
}

impl Placement {
    #[inline]
    fn load(&self) -> [u32; BANKS] {
        loop {
            let s = self.seq.load(Ordering::Acquire);
            if s & 1 == 0 {
                let w = std::array::from_fn(|i| self.words[i].load(Ordering::Relaxed));
                fence(Ordering::Acquire);
                if self.seq.load(Ordering::Relaxed) == s {
                    return w;
                }
            }
            std::hint::spin_loop();
        }
    }

    fn store(&self, w: [u32; BANKS]) {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        for (a, v) in self.words.iter().zip(w) {
            a.store(v, Ordering::Relaxed);
        }
        self.seq.store(s + 2, Ordering::Release);
    }
}

/// The descriptors and the fitted banks, for the devices that decode an
/// address themselves rather than through the table: low memory, the
/// guarded slot and the bus masters' view.
///
/// No lock on an access. Every master reaches the same host bytes, as on
/// the Indy: aligned 32- and 64-bit host accesses are atomic, and the
/// descriptors an access decodes against are published whole.
struct Sdram {
    desc: Placement,
    /// Each HEART bank's capacity in bytes, 0 if nothing is fitted.
    size: [u64; BANKS],
    /// Each fitted bank's host mapping, and its store among the bus's
    /// banks (null where nothing is fitted). Both point into the
    /// `Physical`, which outlives every device the table holds.
    host: [*mut u8; BANKS],
    store: [*const RamBank; BANKS],
    /// Where a bus error from low memory or the guard is noted.
    misses: Arc<Misses>,
}

// `host` and `store` point into the bus's banks, which live as long as the
// bus, and every holder of an `Sdram` is a device on that bus or holds the
// bus itself. Every master reads and writes the bytes with no lock, as the
// hardware's own masters race.
unsafe impl Send for Sdram {}
unsafe impl Sync for Sdram {}

impl Sdram {
    /// The highest-numbered bank whose descriptor claims `addr` answers.
    #[inline]
    fn decode(&self, addr: u64) -> Decode {
        Self::decode_in(&self.desc.load(), &self.size, addr)
    }

    #[inline]
    fn decode_in(d: &[u32; BANKS], size: &[u64; BANKS], addr: u64) -> Decode {
        for bank in (0..BANKS).rev() {
            let w = d[bank];
            if w & DESC_VALID == 0 {
                continue;
            }
            let base = desc_base(w);
            if addr >= base && addr - base < desc_span(w) {
                if size[bank] == 0 {
                    return Decode::Absent;
                }
                return Decode::Ram { bank, off: bank_offset(w, size[bank], addr - base) as usize };
            }
        }
        Decode::None
    }

    /// Where the processor's access at `addr` lands: `Some(Some(o))` memory,
    /// `Some(None)` an empty bank, `None` a bus error.
    #[inline]
    fn cpu_at(&self, addr: u64) -> Option<Option<Loc>> {
        if GUARD.contains(&addr) {
            return None;
        }
        self.unguarded_at(addr)
    }

    /// `cpu_at` without the guard: how low memory reaches those bytes.
    #[inline]
    fn unguarded_at(&self, addr: u64) -> Option<Option<Loc>> {
        match self.decode(addr) {
            Decode::Ram { bank, off } => Some(Some((bank, off))),
            Decode::Absent => Some(None),
            Decode::None => None,
        }
    }

    #[inline]
    fn dma_at(&self, addr: u64) -> Option<Loc> {
        match self.decode(addr) {
            Decode::Ram { bank, off } => Some((bank, off)),
            _ => None,
        }
    }

    #[inline(always)]
    fn bank(&self, bank: usize) -> &RamBank {
        debug_assert!(!self.store[bank].is_null(), "a fitted bank");
        unsafe { &*self.store[bank] }
    }

    /// Byte `off` of bank `bank` was written: bump its page's generation.
    #[inline(always)]
    fn touch(&self, (bank, off): Loc) {
        #[cfg(feature = "jitv2")]
        self.bank(bank).bump_gen_off(off);
        #[cfg(not(feature = "jitv2"))]
        let _ = (bank, off);
    }

    /// The generation counter of the page holding that byte.
    #[cfg(feature = "jitv2")]
    #[inline(always)]
    fn gen_at(&self, (bank, off): Loc) -> *const AtomicU64 {
        self.bank(bank).gen_ptr(off as u64)
    }

    /// The host word holding that byte's doubleword.
    #[inline(always)]
    fn word_at(&self, (bank, off): Loc) -> *const u64 {
        unsafe { self.host[bank].add(off & !7) as *const u64 }
    }

    /// The doublewords `addr..addr + 8 * n`, decoded a 32 MiB unit at a time
    /// (within a unit every byte follows the first): calls `f(where, first,
    /// count)` for each run, `where` None for an empty bank. False, before
    /// any call, if any of it is not reachable (`dma`: not fitted memory).
    fn runs(&self, addr: u64, n: usize, dma: bool, mut f: impl FnMut(Option<Loc>, usize, usize)) -> bool {
        let d = self.desc.load();
        let end = addr.wrapping_add(8 * n as u64);
        let walk = |f: &mut dyn FnMut(Option<Loc>, usize, usize)| -> bool {
            let (mut a, mut i) = (addr, 0usize);
            while i < n {
                // The guard ends inside a unit; a run never straddles it.
                let unit_end = (a / UNIT + 1) * UNIT;
                let stop = if !dma && a < GUARD.end && GUARD.end < unit_end { GUARD.end } else { unit_end };
                let words = (((stop.min(end).wrapping_sub(a)) / 8) as usize).clamp(1, n - i);
                if !dma && GUARD.contains(&a) {
                    return false;
                }
                match Self::decode_in(&d, &self.size, a) {
                    Decode::Ram { bank, off } => f(Some((bank, off)), i, words),
                    Decode::Absent if !dma => f(None, i, words),
                    _ => return false,
                }
                i += words;
                a += 8 * words as u64;
            }
            true
        };
        // All or nothing: check every run before moving anything.
        walk(&mut |_, _, _| {}) && walk(&mut f)
    }

    fn block_read(&self, addr: u64, buf: &mut [u64], dma: bool) -> u32 {
        let ok = self.runs(addr, buf.len(), dma, |at, first, count| {
            let out = &mut buf[first..first + count];
            match at {
                // Same layout as `PpMemory::read_block`.
                Some(at) => unsafe {
                    let p = self.word_at(at);
                    for (j, slot) in out.iter_mut().enumerate() {
                        *slot = (*p.add(j)).rotate_left(32);
                    }
                },
                None => out.fill(0),
            }
        });
        if ok {
            BUS_OK
        } else {
            BUS_ERR
        }
    }

    fn block_write(&self, addr: u64, buf: &[u64], dma: bool) -> u32 {
        let ok = self.runs(addr, buf.len(), dma, |at, first, count| {
            let Some((bank, o)) = at else { return };
            unsafe {
                let p = self.word_at((bank, o)) as *mut u64;
                for (j, &v) in buf[first..first + count].iter().enumerate() {
                    *p.add(j) = v.rotate_left(32);
                }
            }
            // One bump per page touched.
            let mut page = usize::MAX;
            for j in 0..count {
                let b = o + 8 * j;
                if b / PAGE != page {
                    page = b / PAGE;
                    self.touch((bank, b));
                }
            }
        });
        if ok {
            BUS_OK
        } else {
            BUS_ERR
        }
    }
}

/// The fitted DIMMs and the bus they are on: HEART's banks, the
/// [`Physical`] the processor and the bus masters reach them through, and
/// the layout of that bus's SDRAM space as the descriptors stand.
///
/// Built before anything else is mapped, because the bus's default device
/// (a bus error that `ip30 unmapped` lists), low memory and the guarded slot
/// go in first; `Ip30Bus` maps the chips over the rest.
pub struct OctaneMemory {
    phys: Arc<Physical>,
    sdram: Arc<Sdram>,
    /// Where each HEART bank is among the bus's banks.
    index: [Option<usize>; BANKS],
    /// The slot at `RAM_BASE`, for the bus's direct paths.
    guard: Arc<SdramGuard>,
    /// Each unit of SDRAM space as the table holds it. Held across a
    /// descriptor change: the bus is laid out by one caller at a time.
    placed: Mutex<Vec<Unit>>,
}

impl OctaneMemory {
    /// `banks_mb` is the DIMM pair in each of the four banks, in MB (0 for
    /// an empty bank). No descriptor is valid until HEART writes one. Panics on a size `try_new` refuses.
    pub fn new(banks_mb: [u32; 4]) -> Self {
        Self::try_new(banks_mb).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Each bank 0 or a power of two from 64 MiB to 2 GiB, 2 GiB in all.
    pub fn try_new(banks_mb: [u32; 4]) -> Result<Self, String> {
        let mut size = [0u64; BANKS];
        let mut total = 0u64;
        for (i, &mb) in banks_mb.iter().enumerate() {
            let s = (mb as u64) << 20;
            if s != 0 && (!(64 << 20..=2 << 30).contains(&s) || !s.is_power_of_two()) {
                return Err(format!("HEART bank {i} capacity must be 0, 64M, 128M, 256M, 512M, 1G or 2G"));
            }
            size[i] = s;
            total += s;
        }
        if total > 2 << 30 {
            return Err(format!("HEART banks hold {} MiB; an Octane takes at most 2048", total >> 20));
        }
        let mut banks = Vec::new();
        let mut index = [None; BANKS];
        for (i, &s) in size.iter().enumerate().filter(|(_, &s)| s != 0) {
            let bank = RamBank::try_new((s >> 20) as usize).map_err(|e| format!("ppmem: {e}"))?;
            index[i] = Some(banks.len());
            banks.push(bank);
        }
        let phys = Physical::new(banks);
        if phys.ppmem_space().is_none() {
            return Err("ppmem: could not reserve the Octane's memory window".into());
        }
        let misses = Arc::new(Misses::default());
        phys.set_default(misses.clone(), "nothing (ip30 unmapped)");
        let mut host = [std::ptr::null_mut(); BANKS];
        let mut store = [std::ptr::null(); BANKS];
        for (bank, i) in index.iter().enumerate() {
            if let Some(i) = *i {
                host[bank] = phys.banks()[i].host_ptr();
                store[bank] = &phys.banks()[i];
            }
        }
        let sdram = Arc::new(Sdram { desc: Placement::default(), size, host, store, misses });
        let guard = Arc::new(SdramGuard(sdram.clone()));
        phys.map(0, 1 << SLOT_SHIFT, Arc::new(LowAlias(sdram.clone())), "low memory (sdram's first 16k)");
        phys.map(RAM_BASE, RAM_BASE + (1 << SLOT_SHIFT), guard.clone(), "sdram (its first 16k a bus error)");
        Ok(Self { phys, sdram, index, guard, placed: Mutex::new(vec![Unit::None; UNITS]) })
    }

    /// The bus the banks are on.
    pub fn phys(&self) -> &Arc<Physical> {
        &self.phys
    }

    /// The device in the slot at `RAM_BASE`, which faults the first 16 KiB.
    pub fn guard(&self) -> &Arc<SdramGuard> {
        &self.guard
    }

    /// Accesses nothing on the bus answered, first 64 distinct.
    pub fn misses(&self) -> &Misses {
        &self.sdram.misses
    }

    /// HEART's descriptors changed: decode against them from now on and lay
    /// the bus and its ppmem window out again. Called on the CPU thread (a
    /// MEMCFG write) or with the CPU stopped (reset), as the window requires.
    ///
    /// A write that leaves the decode as it was does nothing to the bus or
    /// the page generations: the PROM rewrites the same descriptors, and a
    /// remap empties the window under a running CPU.
    ///
    /// The table moves straight to each unit's new entry, never through the
    /// default, so a bus master on another thread sees the old memory or the
    /// new. The slot at `RAM_BASE` keeps [`SdramGuard`], which decodes the
    /// descriptors itself; the window maps that unit whole, guard included
    /// (see `SdramGuard`).
    pub fn set_memcfg(&self, words: [u32; 4]) {
        let mut placed = self.placed.lock();
        let same = Self::decoded(&self.sdram.desc.load()) == Self::decoded(&words);
        self.sdram.desc.store(words);
        if same {
            return;
        }
        let phys = &*self.phys;
        let sp = self.space();
        sp.clear_mappings();
        // A bank now answering where another did must look changed to the
        // JIT even if the two pages' counters happen to be equal.
        #[cfg(feature = "jitv2")]
        phys.banks().iter().for_each(RamBank::bump_gen_all);
        // Lower banks first, so higher ones overlay them as in the decode.
        let mut units = vec![Unit::None; UNITS];
        for (bank, &d) in words.iter().enumerate() {
            if d & DESC_VALID == 0 {
                continue;
            }
            let first = (d & 0x1ff) as usize;
            for u in 0..(desc_span(d) / UNIT) as usize {
                units[first + u] = match self.sdram.size[bank] {
                    0 => Unit::Absent,
                    size => Unit::Ram { bank, src: bank_offset(d, size, u as u64 * UNIT) },
                };
            }
        }
        for (u, (&now, was)) in units.iter().zip(placed.iter_mut()).enumerate() {
            let at = RAM_BASE + u as u64 * UNIT;
            if now != *was {
                let entry = match now {
                    Unit::None => phys.default_entry(),
                    Unit::Absent => phys.unmapped_entry(),
                    Unit::Ram { bank, src } => phys.bank_view(self.index[bank].expect("a fitted bank"), at.wrapping_sub(src)),
                };
                let (first, count) = match u {
                    0 => ((at >> SLOT_SHIFT) + 1, SLOTS_PER_UNIT - 1),
                    _ => (at >> SLOT_SHIFT, SLOTS_PER_UNIT),
                };
                phys.fill_slots(first, count, entry);
                *was = now;
            }
            let Unit::Ram { bank, src } = now else { continue };
            if at + UNIT > WINDOW_SIZE {
                continue;
            }
            if let Err(e) = sp.map_range(self.index[bank].unwrap(), src, at, UNIT) {
                eprintln!("ip30: HEART bank {bank} at {at:#x}: window map failed: {e}");
            }
        }
    }

    /// The bits of each descriptor the decode reads: all of a valid one
    /// (valid, density, span, base), nothing of an invalid one.
    fn decoded(words: &[u32; BANKS]) -> [u32; BANKS] {
        words.map(|d| if d & DESC_VALID == 0 { 0 } else { d & (DESC_VALID | 0x7 << 22 | 0x3f << 16 | 0x1ff) })
    }

    /// The descriptors in force, for the monitor.
    pub fn memcfg(&self) -> [u32; 4] {
        self.sdram.desc.load()
    }

    /// Each bank's capacity in bytes.
    pub fn bank_sizes(&self) -> [u64; BANKS] {
        self.sdram.size
    }

    /// Bytes fitted.
    pub fn total(&self) -> usize {
        self.sdram.size.iter().sum::<u64>() as usize
    }

    /// The valid descriptors as placed: (physical base, span, capacity), a
    /// capacity of 0 for a bank with nothing fitted.
    pub fn layout(&self) -> Vec<(u64, u64, u64)> {
        let d = self.sdram.desc.load();
        (0..BANKS)
            .filter(|&b| d[b] & DESC_VALID != 0)
            .map(|b| (desc_base(d[b]), desc_span(d[b]), self.sdram.size[b]))
            .collect()
    }

    /// The ppmem window the CPU and the JIT reach memory through.
    pub fn space(&self) -> &PpMemSpace {
        self.phys.ppmem_space().expect("ppmem: the Octane's window, reserved by try_new")
    }

    /// Memory as an XIO bus master reaches it through HEART: what the
    /// Bridge (and its per-slot `PciDma`) and IMPACT's HQ4 are given.
    /// Fitted memory only, at 64-bit physical addresses: a bank with no
    /// DIMMs, low memory, HEART's registers and the widget windows are all
    /// bus errors here, and the guard is not (it is the processor's). Its
    /// `read_block` and `write_block` move nothing unless every doubleword
    /// is fitted memory; use `dma_read_bytes` and `dma_write_bytes` for byte
    /// transfers.
    pub fn dma_view(self: &Arc<Self>) -> Arc<dyn BusDevice> {
        Arc::new(DmaView(self.clone()))
    }

    /// The highest-numbered bank whose descriptor claims `addr` answers.
    pub fn decode(&self, addr: u64) -> Decode {
        self.sdram.decode(addr)
    }}

/// Width accesses for one view: `$at` decodes an address to
/// `Option<Option<Loc>>` (bus error, empty bank, memory), and a bus error
/// goes to the view's `fault`.
macro_rules! ram_widths {
    ($at:expr) => {
        fn read8(&self, addr: u64) -> BusRead8 {
            match $at(self, addr) {
                Some(Some((b, o))) => BusRead8::ok(unsafe { ld8(self.sd().host[b], o) }),
                Some(None) => BusRead8::ok(0),
                None => {
                    self.fault(addr, false);
                    BusRead8::err()
                },
            }
        }
        fn read16(&self, addr: u64) -> BusRead16 {
            match $at(self, addr) {
                Some(Some((b, o))) => BusRead16::ok(unsafe { ld16(self.sd().host[b], o) }),
                Some(None) => BusRead16::ok(0),
                None => {
                    self.fault(addr, false);
                    BusRead16::err()
                },
            }
        }
        fn read32(&self, addr: u64) -> BusRead32 {
            match $at(self, addr) {
                Some(Some((b, o))) => BusRead32::ok(unsafe { ld32(self.sd().host[b], o) }),
                Some(None) => BusRead32::ok(0),
                None => {
                    self.fault(addr, false);
                    BusRead32::err()
                },
            }
        }
        fn read64(&self, addr: u64) -> BusRead64 {
            match $at(self, addr) {
                Some(Some((b, o))) => BusRead64::ok(unsafe { ld64(self.sd().host[b], o) }),
                Some(None) => BusRead64::ok(0),
                None => {
                    self.fault(addr, false);
                    BusRead64::err()
                },
            }
        }
        fn write8(&self, addr: u64, val: u8) -> u32 {
            match $at(self, addr) {
                Some(Some(loc)) => {
                    let (b, o) = loc;
                    unsafe { st8(self.sd().host[b], o, val) };
                    self.sd().touch(loc);
                    BUS_OK
                }
                Some(None) => BUS_OK,
                None => {
                    self.fault(addr, true);
                    BUS_ERR
                },
            }
        }
        fn write16(&self, addr: u64, val: u16) -> u32 {
            match $at(self, addr) {
                Some(Some(loc)) => {
                    let (b, o) = loc;
                    unsafe { st16(self.sd().host[b], o, val) };
                    self.sd().touch(loc);
                    BUS_OK
                }
                Some(None) => BUS_OK,
                None => {
                    self.fault(addr, true);
                    BUS_ERR
                },
            }
        }
        fn write32(&self, addr: u64, val: u32) -> u32 {
            match $at(self, addr) {
                Some(Some(loc)) => {
                    let (b, o) = loc;
                    unsafe { st32(self.sd().host[b], o, val) };
                    self.sd().touch(loc);
                    BUS_OK
                }
                Some(None) => BUS_OK,
                None => {
                    self.fault(addr, true);
                    BUS_ERR
                },
            }
        }
        fn write64(&self, addr: u64, val: u64) -> u32 {
            match $at(self, addr) {
                Some(Some(loc)) => {
                    let (b, o) = loc;
                    unsafe { st64(self.sd().host[b], o, val) };
                    self.sd().touch(loc);
                    BUS_OK
                }
                Some(None) => BUS_OK,
                None => {
                    self.fault(addr, true);
                    BUS_ERR
                },
            }
        }
        fn write64_masked(&self, addr: u64, val: u64, mask: u64) -> u32 {
            match $at(self, addr) {
                Some(Some(loc)) => {
                    let (b, o) = loc;
                    unsafe { st64_masked(self.sd().host[b], o, val, mask) };
                    self.sd().touch(loc);
                    BUS_OK
                }
                Some(None) => BUS_OK,
                None => {
                    self.fault(addr, true);
                    BUS_ERR
                },
            }
        }
    };
}

/// `OctaneMemory::dma_view`: fitted memory only. Holds the memory, so the bus's
/// banks outlive a bus master that keeps it.
struct DmaView(Arc<OctaneMemory>);

impl DmaView {
    #[inline(always)]
    fn sd(&self) -> &Sdram {
        &self.0.sdram
    }

    #[inline(always)]
    fn at(&self, addr: u64) -> Option<Option<Loc>> {
        self.sd().dma_at(addr).map(Some)
    }

    /// A bus master's error is its own to report, not the processor's.
    #[inline(always)]
    fn fault(&self, _addr: u64, _write: bool) {}
}

impl BusDevice for DmaView {
    ram_widths!(DmaView::at);

    fn mem_ptr(&self, addr: u64) -> Option<*const u64> {
        self.sd().dma_at(addr).map(|at| self.sd().word_at(at))
    }

    fn read_block(&self, addr: u64, buf: &mut [u64]) -> u32 {
        self.sd().block_read(addr, buf, true)
    }

    fn write_block(&self, addr: u64, buf: &[u64]) -> u32 {
        self.sd().block_write(addr, buf, true)
    }

    #[cfg(feature = "jitv2")]
    fn gen_ptr(&self, addr: u64) -> *const AtomicU64 {
        self.sd().dma_at(addr).map_or(std::ptr::null(), |at| self.sd().gen_at(at))
    }
}

/// The first 16 KiB of the processor's physical space: the same bytes as
/// SDRAM at `RAM_BASE`, with the same jitv2 page generations, so code the
/// kernel rewrites at its exception vectors is recompiled. This is the only
/// way the processor reaches them, so it takes the unguarded decode. It
/// fills the 64 KiB decode slot at 0, so it answers 0x4000..0x10000 with a
/// bus error itself, noted as a miss, and where no bank descriptor covers
/// `RAM_BASE` it answers as SDRAM does there.
///
/// The ppmem window does not map it; the CPU's cache and the JIT reach it
/// through the bus.
pub struct LowAlias(Arc<Sdram>);

impl LowAlias {
    #[inline(always)]
    fn sd(&self) -> &Sdram {
        &self.0
    }

    /// Where `addr` lands, as `cpu_at`: SDRAM's first 16 KiB, past the guard.
    #[inline(always)]
    fn at(&self, addr: u64) -> Option<Option<Loc>> {
        let o = addr & 0xffff;
        if o >= LOW_SIZE {
            return None;
        }
        self.0.unguarded_at(RAM_BASE + o)
    }

    #[cold]
    fn fault(&self, addr: u64, write: bool) {
        self.0.misses.note(addr, write);
    }
}

impl BusDevice for LowAlias {
    ram_widths!(LowAlias::at);

    fn mem_ptr(&self, addr: u64) -> Option<*const u64> {
        match self.at(addr) {
            Some(Some(at)) => Some(self.0.word_at(at)),
            _ => None,
        }
    }

    /// The same counter SDRAM's page has: without it a page reached through
    /// the alias reads a fixed generation, and the JIT keeps running code
    /// compiled before the kernel rewrote it.
    #[cfg(feature = "jitv2")]
    fn gen_ptr(&self, addr: u64) -> *const AtomicU64 {
        match self.at(addr) {
            Some(Some(at)) => self.0.gen_at(at),
            _ => std::ptr::null(),
        }
    }
}

/// The slot at `RAM_BASE`: a bus error, noted as a miss, below
/// `RAM_BASE + LOW_SIZE`, and SDRAM as the descriptors place it above. A
/// remap leaves this slot alone, since the device decodes the descriptors
/// itself.
///
/// Approximation: this is the bus path. The ppmem window maps the whole
/// unit at `RAM_BASE`, guard included, so the CPU's cached accesses there
/// reach the bytes where real HEART faults them. Leaving 16 KiB out would
/// clear the window's fast-path bit for the first 64 MiB of SDRAM and send
/// every cached access to the kernel through the bus. No supported guest
/// makes such an access; `ip30::tests` pins the behaviour.
pub struct SdramGuard(Arc<Sdram>);

impl SdramGuard {
    #[inline(always)]
    fn sd(&self) -> &Sdram {
        &self.0
    }

    #[inline(always)]
    fn at(&self, addr: u64) -> Option<Option<Loc>> {
        self.0.cpu_at(addr)
    }

    #[cold]
    fn fault(&self, addr: u64, write: bool) {
        self.0.misses.note(addr, write);
    }
}

impl BusDevice for SdramGuard {
    ram_widths!(SdramGuard::at);

    fn mem_ptr(&self, addr: u64) -> Option<*const u64> {
        match self.at(addr) {
            Some(Some(at)) => Some(self.0.word_at(at)),
            _ => None,
        }
    }

    fn read_block(&self, addr: u64, buf: &mut [u64]) -> u32 {
        self.0.block_read(addr, buf, false)
    }

    fn write_block(&self, addr: u64, buf: &[u64]) -> u32 {
        self.0.block_write(addr, buf, false)
    }

    #[cfg(feature = "jitv2")]
    fn gen_ptr(&self, addr: u64) -> *const AtomicU64 {
        match self.at(addr) {
            Some(Some(at)) => self.0.gen_at(at),
            _ => std::ptr::null(),
        }
    }
}

/// `buf.len()` bytes from physical `addr` of `mem` (a `dma_view`), for a
/// master that moves bytes. All or nothing: false if any byte is not
/// memory, and then `buf` is zero.
pub fn dma_read_bytes(mem: &dyn BusDevice, addr: u64, buf: &mut [u8]) -> bool {
    if buf.is_empty() {
        return true;
    }
    let Some((first, words)) = covering(addr, buf.len()) else {
        buf.fill(0);
        return false;
    };
    let mut w = vec![0u64; words];
    if mem.read_block(first, &mut w) != BUS_OK {
        buf.fill(0);
        return false;
    }
    let skip = (addr - first) as usize;
    for (i, b) in buf.iter_mut().enumerate() {
        let k = skip + i;
        *b = w[k / 8].to_be_bytes()[k % 8];
    }
    true
}

/// `data` to physical `addr` of `mem` (a `dma_view`). All or nothing:
/// false if any byte is not memory, and then nothing is written. Partial
/// doublewords at the ends go as masked writes, so the bytes around them
/// are left alone.
pub fn dma_write_bytes(mem: &dyn BusDevice, addr: u64, data: &[u8]) -> bool {
    if data.is_empty() {
        return true;
    }
    let Some((first, words)) = covering(addr, data.len()) else { return false };
    // The check: a read of the whole span fails exactly when a write would.
    let mut probe = vec![0u64; words];
    if mem.read_block(first, &mut probe) != BUS_OK {
        return false;
    }
    let mut vals = vec![0u64; words];
    let mut masks = vec![0u64; words];
    let skip = (addr - first) as usize;
    for (i, &b) in data.iter().enumerate() {
        let k = skip + i;
        let sh = 56 - 8 * (k % 8);
        vals[k / 8] |= (b as u64) << sh;
        masks[k / 8] |= 0xff << sh;
    }
    let mut i = 0;
    while i < words {
        let at = first + 8 * i as u64;
        if masks[i] != u64::MAX {
            if mem.write64_masked(at, vals[i], masks[i]) != BUS_OK {
                return false;
            }
            i += 1;
            continue;
        }
        let j = (i..words).find(|&j| masks[j] != u64::MAX).unwrap_or(words);
        if mem.write_block(at, &vals[i..j]) != BUS_OK {
            return false;
        }
        i = j;
    }
    true
}

/// The aligned doubleword span covering `len` bytes at `addr`: its first
/// address and how many doublewords. `None` if it would wrap.
fn covering(addr: u64, len: usize) -> Option<(u64, usize)> {
    let end = addr.checked_add(len as u64)?;
    let first = addr & !7;
    let last = end.checked_add(7)? & !7;
    Some((first, ((last - first) / 8) as usize))
}

#[cfg(test)]
mod dma_tests {
    use super::*;
    use crate::traits::BUS_ERR;

    /// 64 bytes of memory at 0x1000, big-endian, 8- and 32-bit only.
    struct Bytes(parking_lot::Mutex<[u8; 64]>);

    impl Bytes {
        fn at(&self, a: u64) -> Option<usize> {
            (0x1000..0x1040).contains(&a).then(|| (a - 0x1000) as usize)
        }
    }

    impl BusDevice for Bytes {
        fn read8(&self, a: u64) -> BusRead8 {
            self.at(a).map_or(BusRead8::err(), |o| BusRead8::ok(self.0.lock()[o]))
        }
        fn write8(&self, a: u64, v: u8) -> u32 {
            self.at(a).map_or(BUS_ERR, |o| { self.0.lock()[o] = v; BUS_OK })
        }
        fn read32(&self, a: u64) -> BusRead32 {
            match self.at(a) {
                Some(o) if o + 4 <= 64 => {
                    let m = self.0.lock();
                    BusRead32::ok(u32::from_be_bytes(m[o..o + 4].try_into().unwrap()))
                }
                _ => BusRead32::err(),
            }
        }
        fn write32(&self, a: u64, v: u32) -> u32 {
            match self.at(a) {
                Some(o) if o + 4 <= 64 => {
                    self.0.lock()[o..o + 4].copy_from_slice(&v.to_be_bytes());
                    BUS_OK
                }
                _ => BUS_ERR,
            }
        }
    }

    fn mem() -> Bytes {
        Bytes(parking_lot::Mutex::new(std::array::from_fn(|i| i as u8)))
    }

    #[test]
    fn unaligned_bytes_read_and_write_in_place() {
        let m = mem();
        let mut buf = [0u8; 11];
        assert!(dma_read_bytes(&m, 0x1003, &mut buf));
        assert_eq!(buf, [3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
        assert!(dma_write_bytes(&m, 0x1005, &[0xaa; 20]));
        let got = *m.0.lock();
        assert_eq!(&got[..5], &[0, 1, 2, 3, 4]);
        assert_eq!(&got[5..25], &[0xaa; 20]);
        assert_eq!(&got[25..27], &[25, 26], "the bytes after are left alone");
    }

    #[test]
    fn a_transfer_off_the_end_of_memory_moves_nothing() {
        let m = mem();
        let mut buf = [0x55u8; 8];
        assert!(!dma_read_bytes(&m, 0x103c, &mut buf));
        assert_eq!(buf, [0; 8]);
        assert!(!dma_write_bytes(&m, 0x103c, &[0xee; 8]));
        assert_eq!(m.0.lock()[0x3c..], [0x3c, 0x3d, 0x3e, 0x3f]);
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const PROBE_DESC: u32 = 0x81bf_0040;
    const MODULO_DESC: u32 = 0x817f_0040;
    const PROBE_RAM: u64 = RAM_BASE + 0x40 * UNIT;

    fn ram(mib: u64) -> OctaneMemory {
        OctaneMemory::new([mib as u32, 0, 0, 0])
    }

    fn bank0(m: &OctaneMemory, d: u32) {
        m.set_memcfg([d, 0, 0, 0]);
    }

    fn w(m: &OctaneMemory, addr: u64, v: u64) {
        assert_eq!(m.phys().write64(addr, v), BUS_OK, "write {addr:#x}");
    }

    fn r(m: &OctaneMemory, addr: u64) -> u64 {
        let v = m.phys().read64(addr);
        assert_eq!(v.status, BUS_OK, "read {addr:#x}");
        v.data
    }

    #[test]
    fn descriptors_find_the_fitted_size() {
        for mib in [64u64, 128, 256] {
            let m = ram(mib);
            assert_eq!(m.memcfg(), [0; 4], "nothing valid at power-on");
            bank0(&m, MODULO_DESC);
            w(&m, PROBE_RAM + 0x100, 0x1234_5678_9abc_def0);
            let units = (1..32u64).find(|u| r(&m, PROBE_RAM + 0x100 + u * UNIT) == 0x1234_5678_9abc_def0);
            assert_eq!(units, Some(mib / 32), "{mib} MiB");
            // An absent bank's descriptor still claims probe space but keeps
            // nothing.
            m.set_memcfg([0, PROBE_DESC, 0, 0]);
            assert_eq!(m.phys().write64(PROBE_RAM + 0x100, u64::MAX), BUS_OK);
            assert_eq!(r(&m, PROBE_RAM + 0x100), 0);
            m.set_memcfg([0; 4]);
            assert_eq!(m.phys().read64(PROBE_RAM).status, BUS_ERR);
        }
    }

    fn projected(mib: u64, offset: u64) -> u64 {
        match mib {
            64 => (offset & (UNIT - 1)) | ((offset & (2 * UNIT)) >> 1),
            128 => (offset & (UNIT - 1)) | ((offset & (2 * UNIT)) >> 1) | ((offset & (1 << 30)) >> 4),
            256 => (offset & (4 * UNIT - 1)) | ((offset & (8 * UNIT)) >> 1),
            _ => unreachable!(),
        }
    }

    #[test]
    fn the_probe_descriptor_projects_onto_the_bank() {
        for mib in [64u64, 128, 256] {
            let m = Arc::new(ram(mib));
            let dma = m.dma_view();
            let final_desc = DESC_VALID | (((mib * MIB / UNIT) as u32 - 1) << 16);
            for off in [0u64, 8, 0x100, UNIT, 2 * UNIT, 4 * UNIT, 8 * UNIT] {
                bank0(&m, PROBE_DESC);
                w(&m, PROBE_RAM + off, 0x98a9_bacb_dced_fe0f);
                // DMA reaches the same bytes through the same descriptor.
                let mut b = [0u8; 8];
                assert!(dma_read_bytes(&*dma, PROBE_RAM + off, &mut b));
                assert_eq!(u64::from_be_bytes(b), 0x98a9_bacb_dced_fe0f);
                assert!(dma_write_bytes(&*dma, PROBE_RAM + off, &0x1021_3243_5465_7687u64.to_be_bytes()));
                bank0(&m, final_desc);
                // Read back through DMA: the first projected offsets fall in
                // the 16 KiB the processor cannot reach.
                let mut b = [0u8; 8];
                assert!(dma_read_bytes(&*dma, RAM_BASE + projected(mib, off), &mut b));
                assert_eq!(u64::from_be_bytes(b), 0x1021_3243_5465_7687, "{mib} {off:#x}");
            }
        }
    }

    /// The stock PROM's bank sizing, replayed: every size reaches the
    /// descriptor the PROM's table names for it.
    #[test]
    fn the_prom_probe_reaches_the_right_descriptor() {
        const TABLE: [u32; 16] = [
            0x0000_0040, 0x0001_0040, 0x0043_0040, 0x0047_0040, 0x008f_0040, 0x009f_0040, 0x0000_0040, 0x0000_0040,
            0x0101_0040, 0x0103_0040, 0x0147_0040, 0x014f_0040, 0x019f_0040, 0x01bf_0040, 0x0000_0040, 0x0000_0040,
        ];
        fn distinct(m: &OctaneMemory, base: u64, step: u64, pattern: u64) -> bool {
            w(m, base, pattern);
            w(m, base + step, !pattern);
            r(m, base) == pattern
        }
        fn capacity(m: &OctaneMemory, base: u64) -> u32 {
            let pattern = 0x1111_1111_1111_1111u64;
            let mut bits = 0;
            w(m, base + 0x100, pattern);
            w(m, base, !pattern);
            if r(m, base + 0x100) == pattern {
                bits |= 0x20;
            }
            if distinct(m, base, 4 * UNIT, pattern << 1) {
                bits |= 0x10;
            } else if distinct(m, base, UNIT, pattern << 2) {
                bits |= 0x08;
            }
            if distinct(m, base, 32 * UNIT, pattern << 3) {
                bits |= 0x04;
            }
            TABLE[bits / 4]
        }
        // Memory addresses here run past 4 GiB: the probe descriptor's base
        // is 0x40 units up.
        let probe = PROBE_RAM;
        for (mib, want) in [
            (64u64, 0x8101_0040u32),
            (128, 0x8103_0040),
            (256, 0x8147_0040),
            (512, 0x814f_0040),
            (1024, 0x819f_0040),
            (2048, 0x81bf_0040),
        ] {
            let m = ram(mib);
            bank0(&m, PROBE_DESC);
            let word = capacity(&m, probe) | DESC_VALID;
            bank0(&m, word);
            if (word >> 22) & 1 != 0 {
                w(&m, probe + 0x80, 0);
                w(&m, probe + UNIT + 0x80, !0);
                assert_ne!(r(&m, probe + 0x80), r(&m, probe + UNIT + 0x80));
            }
            assert_eq!(word, want, "{mib} MiB");
            for unit in 0..mib / 32 {
                w(&m, probe + unit * UNIT + 0x100, 0x1021_3243_5465_7687 + unit);
            }
            for unit in 0..mib / 32 {
                assert_eq!(r(&m, probe + unit * UNIT + 0x100), 0x1021_3243_5465_7687 + unit);
            }
        }
    }

    #[test]
    fn the_window_follows_the_descriptors() {
        let m = ram(128);
        bank0(&m, DESC_VALID | (3 << 16));
        w(&m, RAM_BASE + 0x8000, 0x0123_4567_89ab_cdef);
        let bits = m.space().mapped_bitmap();
        assert_eq!(bits, 0b11 << (RAM_BASE >> 26), "128 MiB from 0x20000000 is two 64 MiB regions");
        let p = unsafe { m.space().window_base().add(RAM_BASE as usize + 0x8000) } as *const u32;
        assert_eq!(unsafe { p.read_unaligned() }, 0x0123_4567, "the window holds host words");
        m.set_memcfg([0; 4]);
        assert_eq!(m.space().mapped_bitmap(), 0);
    }

    #[test]
    fn a_write_that_leaves_the_decode_alone_does_not_touch_the_window() {
        let m = ram(128);
        let d = DESC_VALID | (3 << 16);
        bank0(&m, d);
        let mapped = m.space().mapped_bitmap();
        assert_ne!(mapped, 0);
        // Drop the mappings behind its back: a remap would put them back.
        m.space().clear_mappings();
        bank0(&m, d);
        assert_eq!(m.space().mapped_bitmap(), 0, "the same descriptors: no remap");
        // Bits the decode ignores, and the rest of an invalid descriptor.
        m.set_memcfg([d | 1 << 29, 0x7fff_ffff, 0, 0]);
        assert_eq!(m.space().mapped_bitmap(), 0, "ignored bits: no remap");
        assert_eq!(m.memcfg()[1], 0x7fff_ffff, "but the monitor sees what was written");
        bank0(&m, DESC_VALID | (1 << 16));
        assert_eq!(m.space().mapped_bitmap(), 1 << (RAM_BASE >> 26), "a change lays it out");
        assert_ne!(mapped, m.space().mapped_bitmap());
    }

    /// A remap bumps every page's generation, so compiled code from a page
    /// that now holds other bytes is dropped; a repeated write does not.
    #[cfg(feature = "jitv2")]
    #[test]
    fn a_changed_descriptor_bumps_the_page_generations() {
        let gen = |p: *const AtomicU64| unsafe { (*p).load(Ordering::Acquire) };
        let m = Arc::new(ram(128));
        let d = DESC_VALID | (3 << 16);
        bank0(&m, d);
        // The bank's own counter: the window's moves with each remap.
        let g = m.dma_view().gen_ptr(RAM_BASE + 0x8000);
        let v = gen(g);
        bank0(&m, d);
        assert_eq!(gen(g), v, "the same descriptors leave it");
        bank0(&m, DESC_VALID | (1 << 16));
        assert_ne!(gen(g), v, "a change advances it");
        let v = gen(g);
        m.set_memcfg([0; 4]);
        assert_ne!(gen(g), v, "so does taking the memory away");
    }

    /// Banks of 64 and 128 MiB are 192 MiB in all, which is not a power of
    /// two. Each page must still have a counter of its own, the one the CPU's
    /// window shows.
    #[cfg(feature = "jitv2")]
    #[test]
    fn page_generations_are_distinct_when_the_banks_total_is_not_a_power_of_two() {
        let gen = |p: *const AtomicU64| unsafe { (*p).load(Ordering::Acquire) };
        let m = OctaneMemory::new([64, 128, 0, 0]);
        m.set_memcfg([DESC_VALID | (1 << 16), DESC_VALID | (3 << 16) | 2, 0, 0]);
        let in_bank0 = RAM_BASE + 0x5000;
        let in_bank1 = RAM_BASE + 64 * MIB + 0x5000;
        assert_ne!(m.phys().gen_ptr(in_bank0), m.phys().gen_ptr(in_bank1), "two pages, two counters");
        let win = |a: u64| unsafe { m.space().gen_window_base().add((a >> 12) as usize) } as *const AtomicU64;
        let (v0, v1) = (gen(m.phys().gen_ptr(in_bank0)), gen(m.phys().gen_ptr(in_bank1)));
        w(&m, in_bank1, 1);
        assert_eq!(gen(m.phys().gen_ptr(in_bank0)), v0, "a store in bank 1 leaves bank 0's page alone");
        assert_ne!(gen(m.phys().gen_ptr(in_bank1)), v1);
        assert_eq!(gen(win(in_bank1)), gen(m.phys().gen_ptr(in_bank1)), "and the window shows that counter");
        assert_eq!(gen(win(in_bank0)), v0);
    }

    #[test]
    fn the_higher_bank_answers_where_two_overlap() {
        let m = OctaneMemory::new([64, 64, 0, 0]);
        let both = DESC_VALID | (1 << 16);
        m.set_memcfg([both, 0, 0, 0]);
        w(&m, RAM_BASE + 0x8000, 1);
        m.set_memcfg([both, both, 0, 0]);
        w(&m, RAM_BASE + 0x8000, 2);
        assert_eq!(r(&m, RAM_BASE + 0x8000), 2);
        m.set_memcfg([both, 0, 0, 0]);
        assert_eq!(r(&m, RAM_BASE + 0x8000), 1);
    }

    #[test]
    fn the_fast_paths_see_what_the_width_accesses_see() {
        let m = ram(64);
        bank0(&m, DESC_VALID | (1 << 16));
        let a = RAM_BASE + UNIT - 16;
        w(&m, a, 0x0011_2233_4455_6677);
        w(&m, a + 8, 0x8899_aabb_ccdd_eeff);
        let p = m.phys().mem_ptr(a).expect("memory has a pointer");
        assert_eq!(unsafe { *p }.rotate_left(32), 0x0011_2233_4455_6677, "read64's layout");
        assert_eq!(m.phys().read32(a + 4).data, 0x4455_6677);
        assert_eq!(m.phys().read16(a + 2).data, 0x2233);
        assert_eq!(m.phys().read8(a + 1).data, 0x11);
        // A block across a unit boundary: the second unit is the bank's
        // second 32 MiB.
        assert_eq!(m.phys().write_block(a + 16, &[1, 2]), BUS_OK);
        let mut buf = [0u64; 4];
        assert_eq!(m.phys().read_block(a, &mut buf), BUS_OK);
        assert_eq!(buf, [0x0011_2233_4455_6677, 0x8899_aabb_ccdd_eeff, 1, 2]);
        assert_eq!(m.phys().write64_masked(a, 0xffff_0000_0000_ffff, 0xff00_0000_0000_00ff), BUS_OK);
        assert_eq!(r(&m, a), 0xff11_2233_4455_66ff);
        // Past the end: the slot after the bank is a bus error.
        let end = RAM_BASE + (64 << 20);
        assert_eq!(m.phys().write_block(end, &[9]), BUS_ERR);
        assert_eq!(m.phys().read64(end).status, BUS_ERR);
    }

    #[test]
    fn an_empty_bank_reads_zero_from_the_processor_but_faults_dma() {
        let m = Arc::new(OctaneMemory::new([64, 0, 0, 0]));
        m.set_memcfg([0, PROBE_DESC, 0, 0]);
        let mut buf = [5u64; 2];
        assert_eq!(m.phys().read_block(PROBE_RAM, &mut buf), BUS_OK);
        assert_eq!(buf, [0, 0]);
        assert_eq!(m.phys().write_block(PROBE_RAM, &[1]), BUS_OK);
        let dma = m.dma_view();
        assert_eq!(dma.read64(PROBE_RAM).status, BUS_ERR);
        assert_eq!(dma.read_block(PROBE_RAM, &mut buf), BUS_ERR);
        assert!(!dma_write_bytes(&*dma, PROBE_RAM, &[1; 3]));
    }

    #[test]
    fn dma_needs_every_byte_fitted() {
        let m = Arc::new(ram(64));
        bank0(&m, DESC_VALID | (1 << 16));
        let dma = m.dma_view();
        let end = RAM_BASE + 64 * MIB;
        assert!(dma_write_bytes(&*dma, end - 8, &[1; 8]));
        assert!(!dma_write_bytes(&*dma, end - 4, &[2; 8]));
        let mut b = [9u8; 8];
        assert!(!dma_read_bytes(&*dma, end - 4, &mut b));
        assert_eq!(b, [0; 8]);
        assert!(dma_read_bytes(&*dma, end - 8, &mut b));
        assert_eq!(b, [1; 8]);
        // Across a unit boundary, DMA and the CPU agree.
        assert!(dma_write_bytes(&*dma, RAM_BASE + UNIT - 4, &[1, 2, 3, 4, 5, 6, 7, 8]));
        assert_eq!(r(&m, RAM_BASE + UNIT - 8) & 0xffff_ffff, 0x0102_0304);
        assert_eq!(r(&m, RAM_BASE + UNIT) >> 32, 0x0506_0708);
        assert_eq!(dma.read8(RAM_BASE - 1).status, BUS_ERR, "below SDRAM space");
    }

    #[test]
    fn invalid_bank_sizes_are_refused() {
        assert!(OctaneMemory::try_new([96, 0, 0, 0]).is_err());
        assert!(OctaneMemory::try_new([32, 0, 0, 0]).is_err());
        assert!(OctaneMemory::try_new([2048, 64, 0, 0]).is_err());
    }

    #[test]
    fn low_memory_aliases_the_start_of_sdram() {
        let m = Arc::new(ram(128));
        bank0(&m, DESC_VALID | (3 << 16));
        // Low memory is the bus at 0. The processor reaches the bytes only
        // there; a bus master's view shows what it did.
        let l = m.phys();
        let dma = m.dma_view();
        assert_eq!(l.read64(0).data, 0);
        assert_eq!(l.write32(0x100, 0x1234_5678), BUS_OK);
        assert_eq!(dma.read32(RAM_BASE + 0x100).data, 0x1234_5678, "a low store is in SDRAM");
        assert_eq!(m.phys().read32(RAM_BASE + 0x100).status, BUS_ERR, "and not visible to the processor there");
        assert_eq!(l.read16(0x102).data, 0x5678);
        assert_eq!(l.read8(0x100).data, 0x12);
        assert_eq!(dma.write64(RAM_BASE + 0x2000, 0xa5a5_a5a5_5a5a_5a5a), BUS_OK);
        assert_eq!(l.read64(0x2000).data, 0xa5a5_a5a5_5a5a_5a5a, "and the reverse");
        assert_eq!(l.write8(0x3fff, 7), BUS_OK);
        assert_eq!(dma.read8(RAM_BASE + 0x3fff).data, 7);
        assert_eq!(l.write64_masked(0x2000, 0, 0xffff_ffff_0000_0000), BUS_OK);
        assert_eq!(dma.read64(RAM_BASE + 0x2000).data, 0x5a5a_5a5a);
        assert_eq!(l.read8(0x4000).status, BUS_ERR);
        assert_eq!(l.write32(0xfffc, 0), BUS_ERR, "the rest of the slot");
        let mut line = [0u64; 2];
        assert_eq!(l.read_block(0x2000, &mut line), BUS_OK);
        assert_eq!(line[0], 0x5a5a_5a5a);
        assert_eq!(l.mem_ptr(0x100), dma.mem_ptr(RAM_BASE + 0x100), "one word");
        assert!(l.mem_ptr(0x4000).is_none());
    }

    #[test]
    fn sdram_starts_past_the_guard() {
        let m = Arc::new(ram(128));
        bank0(&m, DESC_VALID | (3 << 16));
        assert_eq!(m.phys().read64(RAM_BASE).status, BUS_ERR);
        assert_eq!(m.phys().read8(RAM_BASE + 0x3fff).status, BUS_ERR);
        assert_eq!(m.phys().write32(RAM_BASE + 0x10, 1), BUS_ERR);
        // The direct paths, which `Ip30Bus` sends to the guard rather
        // than to the window.
        let g = m.guard();
        assert!(g.mem_ptr(RAM_BASE).is_none());
        let mut line = [0u64; 16];
        assert_eq!(g.read_block(RAM_BASE + 0x3f80, &mut line), BUS_ERR);
        assert_eq!(g.write_block(RAM_BASE + 0x3f80, &line), BUS_ERR);
        assert_eq!(g.read_block(RAM_BASE + 0x4000, &mut line), BUS_OK);
        assert!(g.mem_ptr(RAM_BASE + 0x4000).is_some());
        w(&m, RAM_BASE + 0x4000, 0x0102_0304_0506_0708);
        assert_eq!(m.phys().read32(RAM_BASE + 0x4004).data, 0x0506_0708);
        assert_eq!(m.phys().read64(RAM_BASE + (128 << 20)).status, BUS_ERR, "past the bank");
        // A bus master reaches the guard's bytes, and low memory is the
        // processor's way to them.
        assert_eq!(m.dma_view().write64(RAM_BASE, 7), BUS_OK);
        assert_eq!(m.dma_view().read64(RAM_BASE).data, 7);
        assert_eq!(m.phys().read64(0).data, 7);
    }

    /// Every write path advances the page's generation, so code the JIT
    /// compiled from it is dropped; reads do not.
    #[cfg(feature = "jitv2")]
    #[test]
    fn stores_advance_the_page_generation() {
        let gen = |p: *const AtomicU64| unsafe { (*p).load(Ordering::Acquire) };
        let m = Arc::new(ram(64));
        bank0(&m, DESC_VALID | (1 << 16));
        let a = RAM_BASE + 0x10_0000;
        let (g0, g1) = (m.phys().gen_ptr(a), m.phys().gen_ptr(a + 0x1000));
        assert!(!g0.is_null() && g0 != g1);
        assert_eq!(m.phys().gen_ptr(a + 0xff8), g0);
        let v = gen(g0);
        let _ = m.phys().read64(a);
        assert_eq!(gen(g0), v, "a read leaves it");
        w(&m, a, 1);
        assert_ne!(gen(g0), v, "a store advances it");
        let (v0, v1) = (gen(g0), gen(g1));
        assert_eq!(m.phys().write_block(a + 0xff8, &[1, 2]), BUS_OK);
        assert!(gen(g0) != v0 && gen(g1) != v1, "a block across pages advances both");
        let v = gen(g1);
        assert!(dma_write_bytes(&*m.dma_view(), a + 0x1003, &[1]));
        assert_ne!(gen(g1), v, "DMA too");
        // The window maps the banks' counters: the CPU's fast path sees
        // the bus's stores.
        let win = unsafe { m.space().gen_window_base().add((a >> 12) as usize) } as *const AtomicU64;
        assert_eq!(gen(win), gen(g0));
        w(&m, a + 8, 2);
        assert_eq!(gen(win), gen(g0));
        // Low memory shares SDRAM's counters.
        let l = m.phys();
        assert!(m.guard().gen_ptr(RAM_BASE).is_null(), "the guard has none");
        let dma = m.dma_view();
        assert_eq!(l.gen_ptr(0x180), dma.gen_ptr(RAM_BASE + 0x180));
        assert_ne!(l.gen_ptr(0x180), l.gen_ptr(0x1000));
        let (l0, a0) = (l.gen_ptr(0x180), gen(l.gen_ptr(0x180)));
        assert_eq!(l.write64(0x180, 0xdf5a_c000_0340_0008), BUS_OK);
        assert_ne!(gen(l0), a0, "a low-memory store advances the page");
        let v = gen(l0);
        assert_eq!(dma.write64(RAM_BASE + 0x180, 1), BUS_OK);
        assert_ne!(gen(l0), v, "and so does a bus master's at its SDRAM address");
        assert!(l.gen_ptr(0x4000).is_null());
    }
}
