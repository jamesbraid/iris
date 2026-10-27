//! Octane main memory: the DIMMs that are fitted, and where HEART's MEMCFG
//! puts them.
//!
//! HEART has four banks (two DIMM slots each) and a 32-bit MEMCFG word per
//! bank: bit 31 valid, bits 21:16 the size less one in 32 MB units, bits
//! 8:0 the base in 32 MB units from 0x20000000 (`dev::heart`). The PROM
//! sizes a bank by programming it large and finding where its contents
//! repeat, so a bank told it is bigger than its DIMMs ignores the address
//! lines it does not have and repeats through its window. When two banks
//! claim an address the lower-numbered one answers. The first 16 KB of
//! memory also answers at physical 0 (the exception vectors live there).
//!
//! The fitted banks live packed, in bank order, in one ppmem object (byte
//! `o` at `host + (o ^ 3)`, the layout every iris RAM keeps), and the
//! object is mapped into a ppmem window as MEMCFG stands, 32 MB at a time,
//! which is what the CPU's cache and the JIT read and write directly. The
//! window is 4 GB: a bank the PROM places beyond it is reachable only
//! through the bus, and then only below 4 GB, until the bus takes 40-bit
//! addresses.

use crate::dev::heart::{MEMCFG_ADDR_MASK, MEMCFG_SIZE_MASK, MEMCFG_SIZE_SHIFT, MEMCFG_UNIT_SHIFT, MEMCFG_VALID};
use crate::ppmem::{MappedMemory, PpMemSpace, PpMemory, WINDOW_SIZE};
use crate::traits::{BusDevice, BusRead16, BusRead32, BusRead64, BusRead8, BUS_OK};
use std::sync::Arc;

/// Where memory starts.
pub const RAM_BASE: u64 = 0x2000_0000;
/// How much of it also answers at physical 0.
pub const LOW_ALIAS_SIZE: u64 = 0x4000;
/// MEMCFG's unit, and the mapping granule.
const UNIT: u64 = 32 << 20;
/// The page jitv2 tracks code by.
const PAGE: usize = 4096;

/// A bank as MEMCFG places it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Bank {
    /// Physical base and the window MEMCFG gives it.
    base: u64,
    span: u64,
    /// Its bytes in the store, and its real size less one.
    start: usize,
    mask: u64,
    /// Valid in MEMCFG but no DIMMs fitted: the PROM probing for memory.
    empty: bool,
}

/// Where a physical address lands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decode {
    /// A byte offset into the store.
    Ram(usize),
    /// A valid bank with nothing in it: writes go nowhere, reads see what
    /// the memory bus last carried (zero, as the O2's CRIME is modelled).
    Empty,
    /// No bank answers.
    None,
}

pub struct HeartMemory {
    store: PpMemory,
    host: *mut u8,
    total: usize,
    space: PpMemSpace,
    /// Each bank's real size in bytes (0 for nothing fitted) and where it
    /// starts in the store.
    size: [u64; 4],
    start: [usize; 4],
    memcfg: [u32; 4],
    banks: [Option<Bank>; 4],
}

// The raw pointer is into `store`, which lives as long as this does; writers
// through it hold the memory's lock, and the CPU reaches the same bytes
// through the window the way the hardware's own masters do.
unsafe impl Send for HeartMemory {}
unsafe impl Sync for HeartMemory {}

impl HeartMemory {
    /// `banks_mb` is the DIMM pair in each bank, in MB (a multiple of 32,
    /// 0 for an empty bank). `memcfg` is HEART's view of them at reset.
    pub fn new(banks_mb: [u32; 4], memcfg: [u32; 4]) -> Self {
        let mut size = [0u64; 4];
        let mut start = [0usize; 4];
        let mut total = 0usize;
        for i in 0..4 {
            size[i] = (banks_mb[i] as u64) << 20;
            start[i] = total;
            total += size[i] as usize;
        }
        let store = PpMemory::new(total.div_ceil(1 << 20).max(1));
        let space = PpMemSpace::over(std::slice::from_ref(&store)).expect("ppmem: Octane memory window");
        let host = store.host_ptr();
        let mut m = Self { store, host, total, space, size, start, memcfg: [0; 4], banks: [None; 4] };
        m.set_memcfg(memcfg);
        m
    }

    /// HEART's MEMCFG words were written: place the banks again.
    pub fn set_memcfg(&mut self, words: [u32; 4]) {
        self.memcfg = words;
        for i in 0..4 {
            let w = words[i];
            self.banks[i] = if w & MEMCFG_VALID == 0 {
                None
            } else {
                let span = ((((w & MEMCFG_SIZE_MASK) >> MEMCFG_SIZE_SHIFT) as u64) + 1) << MEMCFG_UNIT_SHIFT;
                let base = RAM_BASE + (((w & MEMCFG_ADDR_MASK) as u64) << MEMCFG_UNIT_SHIFT);
                let empty = self.size[i] == 0;
                Some(Bank { base, span, start: self.start[i], mask: self.size[i].saturating_sub(1), empty })
            };
        }
        self.remap();
    }

    /// Lay the window out as the banks now stand: every 32 MB of a bank's
    /// span that falls inside the window mapped from the bank's bytes,
    /// repeating through its real size. Later banks never shadow earlier
    /// ones.
    pub fn remap(&mut self) {
        self.space.clear_mappings();
        let mut taken: Vec<(u64, u64)> = Vec::new();
        for bank in self.banks.iter().flatten() {
            if bank.empty || bank.mask < UNIT - 1 {
                continue; // nothing to map, or smaller than a granule: bus only
            }
            let mut at = bank.base;
            while at < bank.base + bank.span && at + UNIT <= WINDOW_SIZE {
                if !taken.iter().any(|&(s, e)| at >= s && at < e) {
                    let src = bank.start as u64 + ((at - bank.base) & bank.mask);
                    let _ = self.space.map_range(0, src, at, UNIT);
                    taken.push((at, at + UNIT));
                }
                at += UNIT;
            }
        }
    }

    /// Where `addr` lands, the low alias included.
    pub fn decode(&self, addr: u64) -> Decode {
        let addr = if addr < LOW_ALIAS_SIZE { RAM_BASE + addr } else { addr };
        for bank in self.banks.iter().flatten() {
            if addr >= bank.base && addr < bank.base + bank.span {
                if bank.empty {
                    return Decode::Empty;
                }
                return Decode::Ram(bank.start + ((addr - bank.base) & bank.mask) as usize);
            }
        }
        Decode::None
    }

    /// The big-endian value of the `n`-byte item (1, 2, 4 or 8) at `off`,
    /// aligned to `n`, in one or two host accesses: each word's bytes sit
    /// reversed, so an aligned word is a host u32 as it stands.
    #[inline]
    pub fn get_be(&self, off: usize, n: u32) -> u64 {
        debug_assert!(off + n as usize <= self.total && off % n as usize == 0);
        unsafe {
            match n {
                1 => *self.host.add(off ^ 3) as u64,
                2 => (self.host.add(off ^ 2) as *const u16).read_unaligned() as u64,
                4 => (self.host.add(off) as *const u32).read_unaligned() as u64,
                _ => {
                    let hi = (self.host.add(off) as *const u32).read_unaligned() as u64;
                    let lo = (self.host.add(off + 4) as *const u32).read_unaligned() as u64;
                    hi << 32 | lo
                }
            }
        }
    }

    /// Store the low `n` bytes of `v` at `off`, aligned to `n`, big-endian.
    #[inline]
    pub fn put_be(&mut self, off: usize, n: u32, v: u64) {
        debug_assert!(off + n as usize <= self.total && off % n as usize == 0);
        unsafe {
            match n {
                1 => *self.host.add(off ^ 3) = v as u8,
                2 => (self.host.add(off ^ 2) as *mut u16).write_unaligned(v as u16),
                4 => (self.host.add(off) as *mut u32).write_unaligned(v as u32),
                _ => {
                    (self.host.add(off) as *mut u32).write_unaligned((v >> 32) as u32);
                    (self.host.add(off + 4) as *mut u32).write_unaligned(v as u32);
                }
            }
        }
        self.touch(off / PAGE);
    }

    /// Page `page` was written: bump its generation.
    #[inline]
    fn touch(&mut self, page: usize) {
        #[cfg(feature = "jitv2")]
        self.store.bump_gen_off(page * PAGE);
        #[cfg(not(feature = "jitv2"))]
        let _ = page;
    }

    /// The generation counter for the page holding byte offset `off`.
    #[cfg(feature = "jitv2")]
    pub fn gen_ptr(&self, off: usize) -> *const std::sync::atomic::AtomicU64 {
        self.store.gen_ptr(off as u64)
    }

    /// Bytes fitted.
    pub fn len(&self) -> usize {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// The CPU's window: guest physical `p` at `window_base() + p` wherever
    /// the mapped bitmap says RAM is.
    pub fn space(&self) -> &PpMemSpace {
        &self.space
    }

    /// The MEMCFG words last placed.
    pub fn memcfg(&self) -> [u32; 4] {
        self.memcfg
    }

    /// The banks as placed: (physical base, span, real size) for each;
    /// a real size of 0 is a valid bank with no DIMMs.
    pub fn layout(&self) -> Vec<(u64, u64, u64)> {
        self.banks.iter().flatten().map(|b| (b.base, b.span, if b.empty { 0 } else { b.mask + 1 })).collect()
    }
}

// ---------------------------------------------------------------------------
// Memory as the processor's map and the bus masters reach it, with no lock.
// These replace `HeartMemory` above and the lock `Ip30Bus` holds it under.
// ---------------------------------------------------------------------------

/// The fitted DIMMs where HEART's MEMCFG puts them: the device
/// `Ip30Physical` maps over SDRAM space, and the processor's view of it.
///
/// No lock on an access. Every master reaches the same host bytes, as on
/// the Indy: aligned 32- and 64-bit host accesses are atomic, and the bank
/// placement an access decodes against is published whole by `set_memcfg`.
/// Addresses are full physical addresses (up to 40 bits), not offsets.
/// A bank MEMCFG marks valid but with no DIMMs reads zero and drops writes;
/// an address no bank claims is a bus error.
///
/// Fast paths, as `Physical`'s banks give them: `mem_ptr`, `read_block` and
/// `write_block` go straight to the host bytes; `gen_ptr` returns the 4 KiB
/// page's generation counter, bumped by every write path, DMA included.
/// The ppmem window (`space`) covers the first 4 GB; memory MEMCFG places
/// above that is reached through this device only.
pub struct HeartRam {
    _todo: (),
}

impl HeartRam {
    /// `banks_mb` is the DIMM pair in each of the four banks, in MB (0 for
    /// an empty bank), placed back to back from `RAM_BASE`.
    pub fn new(banks_mb: [u32; 4]) -> Self {
        let _ = banks_mb;
        todo!()
    }

    /// HEART's MEMCFG words changed: decode against them from now on and
    /// lay the ppmem window out again. Called on the CPU thread, through
    /// `Ip30Physical::remap_banks`.
    pub fn set_memcfg(&self, words: [u32; 4]) {
        let _ = words;
        todo!()
    }

    /// The MEMCFG words in force, for the monitor.
    pub fn memcfg(&self) -> [u32; 4] {
        todo!()
    }

    /// The ppmem window the CPU and the JIT reach memory through.
    pub fn space(&self) -> &PpMemSpace {
        todo!()
    }

    /// Memory as an XIO bus master reaches it through HEART: what the
    /// Bridge (and its per-slot `PciDma`) and IMPACT's HQ4 are given.
    /// Fitted memory only, at 64-bit physical addresses: a bank with no
    /// DIMMs, low memory, HEART's registers and the widget windows are all
    /// bus errors here. `read_block` and `write_block` move nothing unless
    /// every doubleword is fitted memory; use `dma_read_bytes` and
    /// `dma_write_bytes` for byte transfers.
    pub fn dma_view(self: &Arc<Self>) -> Arc<dyn BusDevice> {
        todo!()
    }
}

impl BusDevice for HeartRam {
    fn read8(&self, _addr: u64) -> BusRead8 { todo!() }
    fn read16(&self, _addr: u64) -> BusRead16 { todo!() }
    fn read32(&self, _addr: u64) -> BusRead32 { todo!() }
    fn read64(&self, _addr: u64) -> BusRead64 { todo!() }
    fn write8(&self, _addr: u64, _val: u8) -> u32 { todo!() }
    fn write16(&self, _addr: u64, _val: u16) -> u32 { todo!() }
    fn write32(&self, _addr: u64, _val: u32) -> u32 { todo!() }
    fn write64(&self, _addr: u64, _val: u64) -> u32 { todo!() }
    fn mem_ptr(&self, _addr: u64) -> Option<*const u64> { todo!() }
    fn read_block(&self, _addr: u64, _buf: &mut [u64]) -> u32 { todo!() }
    fn write_block(&self, _addr: u64, _buf: &[u64]) -> u32 { todo!() }
    #[cfg(feature = "jitv2")]
    fn gen_ptr(&self, _addr: u64) -> *const std::sync::atomic::AtomicU64 { todo!() }
}

/// The first 16 KiB of the processor's physical space: storage of its own,
/// not an alias of SDRAM (the IDE diagnostics tell the two apart), with its
/// own jitv2 generation counters, since the kernel writes its exception
/// vectors here. It fills the 64 KiB decode slot at 0, so it answers
/// 0x4000..0x10000 with a bus error itself. Zero at power-on.
pub struct LowMemory {
    _todo: (),
}

impl LowMemory {
    pub fn new() -> Self {
        todo!()
    }
}

impl Default for LowMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl BusDevice for LowMemory {
    fn read8(&self, _addr: u64) -> BusRead8 { todo!() }
    fn read16(&self, _addr: u64) -> BusRead16 { todo!() }
    fn read32(&self, _addr: u64) -> BusRead32 { todo!() }
    fn read64(&self, _addr: u64) -> BusRead64 { todo!() }
    fn write8(&self, _addr: u64, _val: u8) -> u32 { todo!() }
    fn write16(&self, _addr: u64, _val: u16) -> u32 { todo!() }
    fn write32(&self, _addr: u64, _val: u32) -> u32 { todo!() }
    fn write64(&self, _addr: u64, _val: u64) -> u32 { todo!() }
    #[cfg(feature = "jitv2")]
    fn gen_ptr(&self, _addr: u64) -> *const std::sync::atomic::AtomicU64 { todo!() }
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
    use crate::dev::heart::encode_memcfg;

    #[test]
    fn banks_follow_memcfg_and_the_alias_is_the_first_16k() {
        let cfg = [encode_memcfg(0, 256), encode_memcfg(256 << 20, 256), 0, 0];
        let mut m = HeartMemory::new([256, 256, 0, 0], cfg);
        assert_eq!(m.decode(RAM_BASE), Decode::Ram(0));
        assert_eq!(m.decode(RAM_BASE + (256 << 20)), Decode::Ram(256 << 20));
        assert_eq!(m.decode(0x1000), Decode::Ram(0x1000));
        assert_eq!(m.decode(RAM_BASE + (512 << 20)), Decode::None);
        m.put_be(0x1000, 4, 0x1234_5678);
        assert_eq!(m.get_be(0x1000, 4), 0x1234_5678);
        assert_eq!(m.get_be(0x1000, 1), 0x12);
        assert_eq!(m.get_be(0x1002, 2), 0x5678);
        // The PROM's sizing probe: 1 GB at 2 GB on a 256 MB bank repeats
        // every 256 MB.
        m.set_memcfg([0x819f_0040, 0, 0, 0]);
        assert_eq!(m.decode(RAM_BASE + (2 << 30)), Decode::Ram(0));
        assert_eq!(m.decode(RAM_BASE + (2 << 30) + (256 << 20) + 8), Decode::Ram(8));
        assert_eq!(m.decode(RAM_BASE), Decode::None);
        // The PROM probing bank 2, which has no DIMMs: a valid bank that
        // takes writes and reads back nothing.
        m.set_memcfg([0x019f_01ff, 0, 0x81bf_0040, 0]);
        assert_eq!(m.decode(RAM_BASE + (2 << 30)), Decode::Empty);
        assert_eq!(m.layout(), vec![(RAM_BASE + (2 << 30), 2 << 30, 0)]);
    }
}
