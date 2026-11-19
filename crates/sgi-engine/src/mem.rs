//! Memory across the interface: guest RAM the engine reaches, and board
//! memory the shell lends to it.

use std::sync::atomic::{AtomicU64, Ordering};

/// A DMA access that reached no RAM. What an engine does about it is the
/// board's: MGRAS reports `Event::BusFault` and goes on; KONA withholds the
/// DMA's completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemFault;

/// Byte order of an access. Memory is the guest's: byte `i` of an access is
/// the byte at `addr + i`, and an engine reads words from it big-endian, as
/// the guest bus presents them. `Swap` reverses the bytes within each unit
/// of that width on the way, for a bridge that swaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Swap {
    None = 0,
    U16 = 1,
    U32 = 2,
    U64 = 3,
}

/// Guest memory as the shell lends it to the engine. `addr` is a bus
/// address: the shell's callback translates it, and what it reads to do so
/// must be safe without the BQL (RCU-published, or an immutable copy made at
/// each bridge-register write).
///
/// `ram_*` is the path engine threads use. It must be lock-free and reach RAM
/// only: take no emulator lock, above all not the BQL, since the thread that
/// holds it may be waiting for the engine. A target in MMIO or unmapped
/// space is a `MemFault`. In QEMU that is `address_space_translate` under
/// RCU, `memory_region_is_ram`, a copy and the dirty marking; never
/// `address_space_read` or `_write`, which take the BQL for an MMIO target.
///
/// `bus_*` may reach MMIO and take locks, so the engine calls it only on
/// the caller's thread inside `write`, `read`, `run`, `advance` or
/// `wait_idle`. The default sends it down the RAM path. An engine that pulls
/// its stream (R300's ring and indirect-buffer fetch) does that fetch on the
/// caller's thread and hands decoded work to its worker.
pub trait HostMem: Send + Sync + 'static {
    fn ram_read(&self, addr: u64, out: &mut [u8], swap: Swap) -> Result<(), MemFault>;
    fn ram_write(&self, addr: u64, data: &[u8], swap: Swap) -> Result<(), MemFault>;
    fn bus_read(&self, addr: u64, out: &mut [u8], swap: Swap) -> Result<(), MemFault> {
        self.ram_read(addr, out, swap)
    }
    fn bus_write(&self, addr: u64, data: &[u8], swap: Swap) -> Result<(), MemFault> {
        self.ram_write(addr, data, swap)
    }
}

/// The block a dirty report is rounded to.
pub const DIRTY_BLOCK: u64 = 16 * 1024;

/// A byte range of lent memory, from its start. `drain_dirty` runs are
/// multiples of `DIRTY_BLOCK` but for the one at the end of the memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyRun {
    pub offset: u64,
    pub len: u64,
}

/// Board memory the shell owns and lends to an engine that does not
/// allocate its own (VPro's 128 MB, R300's 128 or 256 MB). Aligned to 8
/// bytes, a whole number of host-endian 64-bit words, valid until the engine
/// is dropped.
///
/// Who writes what: the engine writes it from any of its threads; the CPU
/// writes it through the shell's mapping, then calls `cpu_wrote`. The engine's
/// stores do not mark the shell's dirty bits: it reports them through
/// `drain_dirty` (`DirtyBlocks` does the bookkeeping).
///
/// Access rule. The memory is ordinary shared RAM: the guest's stores through
/// the shell's mapping are plain host stores of 1 to 8 bytes, and a board's own
/// units race on it as the hardware's do. `load` and `store` here are relaxed
/// atomic word accesses, which an engine that uses them gets for free. An engine
/// that takes `as_ptr` instead takes over the rule: it uses aligned accesses of
/// at most 8 bytes through raw pointers, and a narrow store is a read-modify-write
/// of its word that can lose a CPU store to another byte of the same word made at
/// the same moment. A guest that races the CPU against the board on one word gets
/// whatever it asked for.
#[derive(Clone, Copy, Debug)]
pub struct Lent {
    ptr: *mut u64,
    words: usize,
}

// SAFETY: accesses follow the rule on `Lent`, and the lender promises the
// memory outlives the engine.
unsafe impl Send for Lent {}
unsafe impl Sync for Lent {}

impl Lent {
    /// # Safety
    /// `ptr` is 8-aligned and valid for `len` bytes for as long as the engine
    /// lives, and nothing else accesses it but as the rule on `Lent` says.
    pub unsafe fn new(ptr: *mut u8, len: usize) -> Option<Lent> {
        (!ptr.is_null() && ptr as usize % 8 == 0 && len % 8 == 0).then_some(Lent { ptr: ptr.cast(), words: len / 8 })
    }

    pub fn len(&self) -> u64 {
        self.words as u64 * 8
    }

    pub fn is_empty(&self) -> bool {
        self.words == 0
    }

    /// The start of the region, for an engine whose raster code addresses
    /// memory by pointer. Access it only as `Lent` says: whole words.
    pub fn as_ptr(&self) -> *mut u64 {
        self.ptr
    }

    fn word(&self, i: usize) -> &AtomicU64 {
        assert!(i < self.words, "lent word {i} past {}", self.words);
        // SAFETY: in range by the assert; `new`'s contract.
        unsafe { &*self.ptr.add(i).cast::<AtomicU64>() }
    }

    pub fn load(&self, i: usize) -> u64 {
        self.word(i).load(Ordering::Relaxed)
    }

    pub fn store(&self, i: usize, v: u64) {
        self.word(i).store(v, Ordering::Relaxed)
    }
}

/// Which blocks of lent memory the engine wrote since the last drain.
pub struct DirtyBlocks {
    bits: Vec<AtomicU64>,
    len: u64,
}

impl DirtyBlocks {
    pub fn new(len: u64) -> DirtyBlocks {
        let blocks = len.div_ceil(DIRTY_BLOCK) as usize;
        DirtyBlocks { bits: (0..blocks.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(), len }
    }

    /// Note that `len` bytes at `off` were written.
    pub fn mark(&self, off: u64, len: u64) {
        if len == 0 || off >= self.len {
            return;
        }
        let last = (off + len - 1).min(self.len - 1) / DIRTY_BLOCK;
        for b in off / DIRTY_BLOCK..=last {
            self.bits[b as usize / 64].fetch_or(1 << (b % 64), Ordering::Release);
        }
    }

    /// Append the runs written since the last drain, and forget them.
    pub fn drain(&self, out: &mut Vec<DirtyRun>) {
        let mut run: Option<(u64, u64)> = None;
        let flush = |run: &mut Option<(u64, u64)>, out: &mut Vec<DirtyRun>| {
            if let Some((first, n)) = run.take() {
                let offset = first * DIRTY_BLOCK;
                out.push(DirtyRun { offset, len: (n * DIRTY_BLOCK).min(self.len - offset) });
            }
        };
        for (w, bits) in self.bits.iter().enumerate() {
            let v = bits.swap(0, Ordering::Acquire);
            for k in 0..64u64 {
                let b = w as u64 * 64 + k;
                if v >> k & 1 != 0 {
                    match &mut run {
                        Some((first, n)) if *first + *n == b => *n += 1,
                        _ => {
                            flush(&mut run, out);
                            run = Some((b, 1));
                        }
                    }
                } else {
                    flush(&mut run, out);
                }
            }
        }
        flush(&mut run, out);
    }
}
