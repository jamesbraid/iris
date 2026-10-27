//! HEART, the Octane's processor and memory controller: SDRAM in four
//! banks behind bank descriptors, the 64 interrupt bits that drive the
//! R10000's interrupt pins, a 52-bit counter, a one-wire master for the
//! processor module's ID part, and the XIO widget (widget 8) that the
//! Crossbow's other widgets reach memory and interrupts through.
//!
//! This is a port of the Origami QEMU model, which is a functional profile
//! rather than a register-exact HEART: what it does not model it leaves
//! observable (see [`Heart::unsupported`]). The approximations it keeps are
//! said where they live.
//!
//! The processor sees HEART's registers at [`PIU_BASE`], its widget registers
//! at 0x1800_0000 (the machine decodes that) and memory from [`RAM_BASE`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::irq::IrqLine;
use crate::ip30::xio::{XioInterruptSink, XioMemory};
use crate::ppmem::{MappedMemory, PpMemSpace, PpMemory};

/// HEART's processor-interface registers.
pub const PIU_BASE: u32 = 0x0ff0_0000;
pub const PIU_SIZE: u32 = 0x0006_0008;
/// HEART's own widget registers, the window the machine maps at the start
/// of widget 8's small window.
pub const WIDGET_REG_SIZE: u32 = 0x1000;
/// Where SDRAM starts. Bank descriptors count from here in [`UNIT`]s.
pub const RAM_BASE: u32 = 0x2000_0000;
/// The descriptors' unit: base and size are counted in 32 MiB.
pub const UNIT: u64 = 32 << 20;
pub const BANKS: usize = 4;

const COUNT_MASK: u64 = (1 << 52) - 1;
/// The counter ticks at 12.5 MHz.
const COUNT_NS: u64 = 80;

const LEVEL_MASKS: [u64; 5] = [
    0x0000_0000_0000_ffff,
    0x0000_0000_ffff_0000,
    0x0003_ffff_0000_0000,
    0x0004_0000_0000_0000,
    0xfff8_0000_0000_0000,
];

/// Mode register bits the model looks at.
const MODE_INTERRUPT_ENABLE: u64 = 1 << 13;
const MODE_ECC_GENERATE: u64 = 1 << 19;
const MODE_ECC_DOUBLE: u64 = 1 << 20;
const MODE_ECC_FORCE_WRITE: u64 = 1 << 21;
/// The memory-error interrupt bit, which only reaches the CPU while the
/// mode register enables it.
const ECC_PENDING: u64 = 1 << 63;

const DESC_VALID: u32 = 0x8000_0000;

const ONEWIRE_DONE: u64 = 2;
const ONEWIRE_LEVEL: u64 = 1;

/// Where a byte offset into a fitted bank lands for an address `offset` into
/// a descriptor's span.
///
/// The stock PROM sizes a bank by writing through a temporary 2 GiB density-6
/// descriptor (`0x81bf0040`) and checking which address bits alias. Real
/// HEART's address wiring for that probe is not established; this
/// projection, the Origami model's functional approximation, makes each
/// fitted size give the PROM the answer it needs to pick the right
/// descriptor. Every other descriptor wraps at the bank's size. All of them
/// keep the low 25 bits, so a 32 MiB unit maps to one 32 MiB run of the bank.
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
    RAM_BASE as u64 + (d & 0x1ff) as u64 * UNIT
}

fn desc_span(d: u32) -> u64 {
    (((d >> 16) & 0x3f) as u64 + 1) * UNIT
}

/// Where a physical address in SDRAM space lands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decode {
    /// A byte offset into the fitted memory.
    Ram(usize),
    /// A valid descriptor for a bank with nothing fitted: reads are zero
    /// and writes go nowhere, from the processor.
    Absent,
    /// No descriptor claims it: a bus error.
    None,
}

/// The fitted banks, packed in bank order in one ppmem object, and the
/// window the CPU and the JIT reach them through, mapped as the
/// descriptors stand. Byte `o` lives at `host + (o ^ 3)`, the layout every
/// iris RAM keeps.
pub struct HeartMemory {
    store: PpMemory,
    host: *mut u8,
    total: usize,
    space: PpMemSpace,
    size: [u64; BANKS],
    start: [u64; BANKS],
    desc: [u32; BANKS],
}

// `host` points into `store`, which lives as long as this does. Writers
// through it hold the memory's lock; the CPU and DMA masters race through
// the window only the way the hardware's masters do.
unsafe impl Send for HeartMemory {}
unsafe impl Sync for HeartMemory {}

const PAGE: usize = 4096;

impl HeartMemory {
    /// Banks of `sizes` bytes: each 0 or a power of two from 64 MiB to 2 GiB.
    pub fn new(sizes: [u64; BANKS]) -> Result<Self, String> {
        let mut start = [0u64; BANKS];
        let mut total = 0u64;
        for (i, &s) in sizes.iter().enumerate() {
            if s != 0 && (s < 64 << 20 || s > 2 << 30 || !s.is_power_of_two()) {
                return Err(format!("HEART bank {i} capacity must be 0, 64M, 128M, 256M, 512M, 1G or 2G"));
            }
            start[i] = total;
            total += s;
        }
        if total > 2 << 30 {
            return Err(format!("HEART banks hold {} MiB; an Octane takes at most 2048", total >> 20));
        }
        let store = PpMemory::try_new(((total >> 20) as usize).max(1)).map_err(|e| format!("ppmem: {e}"))?;
        let space = PpMemSpace::over(std::slice::from_ref(&store)).map_err(|e| format!("ppmem: {e}"))?;
        let host = store.host_ptr();
        let mut m = Self { store, host, total: total as usize, space, size: sizes, start, desc: [0; BANKS] };
        m.remap();
        Ok(m)
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn bank_sizes(&self) -> [u64; BANKS] {
        self.size
    }

    /// The window the CPU executes out of: physical `p` at `window_base() + p`
    /// wherever the published bitmap says.
    pub fn space(&self) -> &PpMemSpace {
        &self.space
    }

    /// The highest-numbered bank whose descriptor claims `addr` answers, as
    /// in the model, where later banks overlay earlier ones.
    pub fn decode(&self, addr: u64) -> Decode {
        for bank in (0..BANKS).rev() {
            let d = self.desc[bank];
            let base = desc_base(d);
            if d & DESC_VALID != 0 && addr >= base && addr - base < desc_span(d) {
                if self.size[bank] == 0 {
                    return Decode::Absent;
                }
                let off = bank_offset(d, self.size[bank], addr - base);
                return Decode::Ram((self.start[bank] + off) as usize);
            }
        }
        Decode::None
    }

    /// A bank descriptor changed: lay the window out again.
    fn set_descriptors(&mut self, desc: [u32; BANKS]) {
        if self.desc != desc {
            self.desc = desc;
            self.remap();
        }
    }

    /// Every 32 MiB unit a valid descriptor gives a fitted bank, mapped from
    /// its run of the bank, lower banks first so higher ones overlay them as
    /// they do in the decode. Units above 4 GiB are out of the CPU's reach
    /// here and stay unmapped.
    fn remap(&mut self) {
        self.space.clear_mappings();
        for bank in 0..BANKS {
            let d = self.desc[bank];
            if d & DESC_VALID == 0 || self.size[bank] == 0 {
                continue;
            }
            let base = desc_base(d);
            for unit in 0..desc_span(d) / UNIT {
                let at = base + unit * UNIT;
                if at + UNIT > 1 << 32 {
                    break;
                }
                let src = self.start[bank] + bank_offset(d, self.size[bank], unit * UNIT);
                if let Err(e) = self.space.map_range(0, src, at, UNIT) {
                    eprintln!("ip30: HEART bank {bank} unit {unit}: window map failed: {e}");
                }
            }
        }
    }

    #[inline]
    fn byte(&self, off: usize) -> u8 {
        unsafe { *self.host.add(off ^ 3) }
    }

    #[inline]
    fn set_byte(&mut self, off: usize, b: u8) {
        unsafe { *self.host.add(off ^ 3) = b }
    }

    #[inline]
    fn touch(&self, off: usize) {
        #[cfg(feature = "jitv2")]
        self.store.bump_gen_off(off & !(PAGE - 1));
        #[cfg(not(feature = "jitv2"))]
        let _ = off;
    }

    /// The generation counter of the page holding byte `off`.
    #[cfg(feature = "jitv2")]
    pub fn gen_ptr(&self, off: usize) -> *const AtomicU64 {
        self.store.gen_ptr(off as u64)
    }

    /// `n` bytes (1 to 8) at physical `addr`, big-endian, as the processor
    /// reads them. `None` is a bus error.
    pub fn cpu_read(&self, addr: u64, n: usize) -> Option<u64> {
        let mut v = 0u64;
        for k in 0..n {
            let b = match self.decode(addr + k as u64) {
                Decode::Ram(o) => self.byte(o),
                Decode::Absent => 0,
                Decode::None => return None,
            };
            v = (v << 8) | b as u64;
        }
        Some(v)
    }

    /// The low `n` bytes of `val` at `addr`, big-endian. False is a bus error,
    /// and then nothing is written.
    pub fn cpu_write(&mut self, addr: u64, n: usize, val: u64) -> bool {
        let mut at = [Decode::None; 8];
        for k in 0..n {
            at[k] = self.decode(addr + k as u64);
            if at[k] == Decode::None {
                return false;
            }
        }
        for k in 0..n {
            if let Decode::Ram(o) = at[k] {
                self.set_byte(o, (val >> (8 * (n - 1 - k))) as u8);
                self.touch(o);
            }
        }
        true
    }

    /// DMA: every byte must be fitted memory, or nothing moves.
    pub fn dma_read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let Some(offs) = self.dma_offsets(addr, buf.len()) else {
            buf.fill(0);
            return false;
        };
        for (b, o) in buf.iter_mut().zip(offs) {
            *b = self.byte(o);
        }
        true
    }

    pub fn dma_write(&mut self, addr: u64, data: &[u8]) -> bool {
        let Some(offs) = self.dma_offsets(addr, data.len()) else { return false };
        let mut last_page = usize::MAX;
        for (&b, o) in data.iter().zip(offs) {
            self.set_byte(o, b);
            if o / PAGE != last_page {
                last_page = o / PAGE;
                self.touch(o);
            }
        }
        true
    }

    /// Byte offsets for `len` bytes from `addr`, decoded a run at a time:
    /// within one 32 MiB unit every byte follows the first.
    fn dma_offsets(&self, addr: u64, len: usize) -> Option<impl Iterator<Item = usize>> {
        if addr < RAM_BASE as u64 {
            return None;
        }
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut a = addr;
        let end = addr.checked_add(len as u64)?;
        while a < end {
            let Decode::Ram(o) = self.decode(a) else { return None };
            let unit_end = (a - RAM_BASE as u64) / UNIT * UNIT + UNIT + RAM_BASE as u64;
            let n = (unit_end.min(end) - a) as usize;
            runs.push((o, n));
            a += n as u64;
        }
        Some(runs.into_iter().flat_map(|(o, n)| o..o + n))
    }

    /// Every fitted byte as host words, for a snapshot.
    pub fn words(&self) -> &[u32] {
        unsafe { std::slice::from_raw_parts(self.host as *const u32, self.total / 4) }
    }
}

/// A part on HEART's one-wire bus, at the bit-slot level.
pub trait OneWireResponder: Send {
    /// A reset pulse: true if the part answers with presence.
    fn reset(&mut self) -> bool;
    /// One slot: the master writes `bit` (a read is a written one); the part
    /// may pull the line low. Returns what the part leaves on the line.
    fn touch_bit(&mut self, bit: bool) -> bool;
}

/// HEART's registers. Everything is behind one lock: no register access
/// calls out except to drive the CPU's pins, which call nothing back.
struct Regs {
    mode: u64,
    sdram_mode: u64,
    mem_refresh: u64,
    mem_req_arb: u64,
    flow_cell: [u64; 2],
    clock_divider: u64,
    compare: u32,
    descriptor: [u32; 8],
    mask: [u64; 4],
    pending: u64,
    ecc_cause: u64,
    ecc_address: u64,
    ecc_data: u64,
    /// The counter is `saved_count` plus the ticks since `epoch_ns`.
    saved_count: u64,
    epoch_ns: u64,
    widget_control: u32,
    widget_request_timeout: u32,
    widget_error_mask: u32,
    onewire_control: u64,
    onewire_deadline_ns: u64,
    onewire_level: bool,
    onewire_busy: bool,
    /// The levels last driven onto IP2..IP6.
    out: u8,
}

impl Regs {
    fn new(now: u64) -> Self {
        Self {
            mode: 0,
            sdram_mode: 0,
            mem_refresh: 0,
            mem_req_arb: 0,
            flow_cell: [0; 2],
            clock_divider: 0,
            compare: 0,
            descriptor: [0; 8],
            mask: [0; 4],
            pending: 0,
            ecc_cause: 0,
            ecc_address: 0,
            ecc_data: 0,
            saved_count: 0,
            epoch_ns: now,
            widget_control: 0,
            widget_request_timeout: 0,
            widget_error_mask: 0,
            onewire_control: 0,
            onewire_deadline_ns: 0,
            onewire_level: false,
            onewire_busy: false,
            out: 0,
        }
    }

    fn counter(&self, now: u64) -> u64 {
        (self.saved_count + now.saturating_sub(self.epoch_ns) / COUNT_NS) & COUNT_MASK
    }

    /// What IP2..IP6 should be: CPU 0's mask over the pending bits, one
    /// output per level.
    fn levels(&self) -> u8 {
        let mut enabled = self.pending & self.mask[0];
        let mut out = 0u8;
        for (i, m) in LEVEL_MASKS.iter().enumerate().take(4) {
            if enabled & m != 0 {
                out |= 1 << i;
            }
        }
        if self.mode & MODE_INTERRUPT_ENABLE == 0 {
            enabled &= !ECC_PENDING;
        }
        if enabled & LEVEL_MASKS[4] != 0 {
            out |= 1 << 4;
        }
        out
    }

    fn banks(&self) -> [u32; BANKS] {
        [self.descriptor[0], self.descriptor[1], self.descriptor[2], self.descriptor[3]]
    }
}

/// What a machine fixes about its HEART.
#[derive(Clone, Debug)]
pub struct HeartConfig {
    /// Each bank's capacity in bytes.
    pub banks: [u64; BANKS],
    /// The widget identification word's revision field.
    pub widget_revision: u8,
    /// The whole identification word, when the machine pins it; 0 builds it
    /// from the revision.
    pub widget_id_64: u64,
}

impl HeartConfig {
    /// The Octane: HEART revision D (4) and its identification word.
    pub fn octane(banks: [u64; BANKS]) -> Self {
        Self { banks, widget_revision: 4, widget_id_64: 0x4c00_106d }
    }
}

/// Nanoseconds from some fixed point, for the counter and the one-wire slot
/// timing. The machine uses the host clock; tests step their own.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

pub fn host_clock() -> Clock {
    let start = std::time::Instant::now();
    Arc::new(move || start.elapsed().as_nanos() as u64)
}

pub struct Heart {
    regs: Mutex<Regs>,
    pub mem: Mutex<HeartMemory>,
    cfg: HeartConfig,
    clock: Clock,
    /// IP2..IP6, once the machine has a CPU.
    pins: Mutex<Option<[IrqLine; 5]>>,
    onewire: Mutex<Vec<Box<dyn OneWireResponder>>>,
    /// Accesses the model does not support, first 64 distinct, for the
    /// monitor: `(offset, size, write)`. The guest sees what the model
    /// answers; this is how a person sees it was answered for want of more.
    unsupported: Mutex<Vec<(u32, u8, bool)>>,
    /// Packets that reached HEART's interrupt register, for the monitor.
    pub packets: AtomicU64,
}

impl Heart {
    pub fn new(cfg: HeartConfig, clock: Clock) -> Result<Self, String> {
        let mem = HeartMemory::new(cfg.banks)?;
        let now = clock();
        Ok(Self {
            regs: Mutex::new(Regs::new(now)),
            mem: Mutex::new(mem),
            cfg,
            clock,
            pins: Mutex::new(None),
            onewire: Mutex::new(Vec::new()),
            unsupported: Mutex::new(Vec::new()),
            packets: AtomicU64::new(0),
        })
    }

    pub fn config(&self) -> &HeartConfig {
        &self.cfg
    }

    /// HEART's five outputs, levels 0 to 4, which the Octane wires to the
    /// CPU's IP2 to IP6.
    pub fn connect_cpu(&self, pins: [IrqLine; 5]) {
        let r = self.regs.lock().unwrap();
        for (i, p) in pins.iter().enumerate() {
            p.set(r.out & (1 << i) != 0);
        }
        *self.pins.lock().unwrap() = Some(pins);
    }

    pub fn attach_onewire(&self, part: Box<dyn OneWireResponder>) {
        self.onewire.lock().unwrap().push(part);
    }

    pub fn unsupported(&self) -> Vec<(u32, u8, bool)> {
        self.unsupported.lock().unwrap().clone()
    }

    fn note(&self, off: u32, size: usize, write: bool) {
        let mut u = self.unsupported.lock().unwrap();
        if u.len() < 64 && !u.iter().any(|&(o, s, w)| o == off && s as usize == size && w == write) {
            if trace() {
                eprintln!("ip30: HEART: unsupported {size}-byte {} at {off:#x}", if write { "write" } else { "read" });
            }
            u.push((off, size as u8, write));
        }
    }

    /// Drive the CPU's pins to what the registers now say. Called with the
    /// register lock held: the CPU's pins are atomics and call nothing back.
    fn update_irq(&self, r: &mut Regs) {
        let new = r.levels();
        if new == r.out {
            return;
        }
        let changed = new ^ r.out;
        r.out = new;
        if let Some(pins) = self.pins.lock().unwrap().as_ref() {
            for (i, p) in pins.iter().enumerate() {
                if changed & (1 << i) != 0 {
                    p.set(new & (1 << i) != 0);
                }
            }
        }
    }

    /// The level IPn (n = 2..6) is at, for tests and the monitor.
    pub fn pin(&self, n: u32) -> bool {
        self.regs.lock().unwrap().out & (1 << (n - 2)) != 0
    }

    pub fn pending(&self) -> u64 {
        self.regs.lock().unwrap().pending
    }

    // ── one-wire ───────────────────────────────────────

    /// One bit slot, started by a write of the command register: the low
    /// drive time selects reset (at least 400), write-one/read (under 40) or
    /// write-zero. The thresholds are the model's, not measured. The parts
    /// answer at once; the done bit follows after pulse + sample, counted as
    /// microseconds (the silicon's units are not known).
    fn onewire_start(&self, r: &mut Regs, command: u64) {
        let pulse = ((command >> 10) & 0x3ff) as u32;
        let sample = ((command >> 2) & 0xff) as u32;
        r.onewire_control = command & !(ONEWIRE_DONE | ONEWIRE_LEVEL);
        let mut parts = self.onewire.lock().unwrap();
        let level = if pulse >= 400 {
            // The wire is low if any part answers presence.
            let mut present = false;
            for p in parts.iter_mut() {
                present |= p.reset();
            }
            !present
        } else {
            let bit = pulse < 40;
            let mut line = bit;
            for p in parts.iter_mut() {
                line &= p.touch_bit(bit);
            }
            line
        };
        r.onewire_level = level;
        r.onewire_busy = true;
        r.onewire_deadline_ns = (self.clock)() + (pulse + sample) as u64 * 1000;
    }

    fn onewire_refresh(&self, r: &mut Regs) {
        if r.onewire_busy && (self.clock)() >= r.onewire_deadline_ns {
            r.onewire_busy = false;
            r.onewire_control |= ONEWIRE_DONE;
            if r.onewire_level {
                r.onewire_control |= ONEWIRE_LEVEL;
            }
        }
    }

    // ── processor-interface registers ───────────────────

    /// A PIU register read of `size` bytes (4 or 8; HEART refuses narrower
    /// accesses, `None`). Unsupported offsets read as zero, as in the model.
    pub fn piu_read(&self, off: u32, size: usize) -> Option<u64> {
        if size != 4 && size != 8 {
            return None;
        }
        let mut r = self.regs.lock().unwrap();
        if (0x20..0x40).contains(&off) {
            let n = ((off - 0x20) / 4) as usize;
            if size == 4 {
                return Some(r.descriptor[n] as u64);
            }
            if n & 1 == 0 {
                return Some(((r.descriptor[n] as u64) << 32) | r.descriptor[n + 1] as u64);
            }
        }
        if (off == 0xb8 && size == 8) || (off == 0xbc && size == 4) {
            self.onewire_refresh(&mut r);
            return Some(r.onewire_control);
        }
        if size == 8 {
            let v = match off {
                0x00 => Some(r.mode),
                0x08 => Some(r.sdram_mode),
                0x10 => Some(r.mem_refresh),
                0x18 => Some(r.mem_req_arb),
                0x50 => Some(r.flow_cell[0]),
                0x58 => Some(r.flow_cell[1]),
                // Processor status: CPU 0 present, and this is CPU 0.
                0x80 => Some(0x18),
                0x98 => Some(r.ecc_address),
                0xa0 => Some(r.ecc_data),
                0xb0 => Some(r.clock_divider),
                0x10000 | 0x10008 | 0x10010 | 0x10018 => Some(r.mask[((off - 0x10000) / 8) as usize]),
                0x10030 => Some(r.pending),
                0x10038 => Some(r.pending & r.mask[0]),
                0x10040 => Some(r.ecc_cause),
                0x20000 => Some(r.counter((self.clock)())),
                0x30000 => Some(r.compare as u64),
                0x50000 => Some(0),
                _ => None,
            };
            if let Some(v) = v {
                return Some(v);
            }
        }
        drop(r);
        self.note(off, size, false);
        Some(0)
    }

    /// A PIU register write; false is a refused width. Unsupported offsets
    /// are recorded and otherwise ignored, as in the model.
    pub fn piu_write(&self, off: u32, size: usize, val: u64) -> bool {
        if size != 4 && size != 8 {
            return false;
        }
        let mut r = self.regs.lock().unwrap();
        if (0x20..0x40).contains(&off) {
            let n = ((off - 0x20) / 4) as usize;
            if size == 4 || n & 1 == 0 {
                if size == 4 {
                    r.descriptor[n] = val as u32;
                } else {
                    r.descriptor[n] = (val >> 32) as u32;
                    r.descriptor[n + 1] = val as u32;
                }
                let banks = r.banks();
                drop(r);
                // On the CPU thread: the window may only be remapped there.
                self.mem.lock().unwrap().set_descriptors(banks);
                return true;
            }
        }
        if (off == 0xb8 && size == 8) || (off == 0xbc && size == 4) {
            self.onewire_start(&mut r, val);
            return true;
        }
        if size == 8 {
            match off {
                0x00 => {
                    // ECC generation and forced writes plant errors in
                    // memory, which this port does not model.
                    if val & (MODE_ECC_GENERATE | MODE_ECC_DOUBLE | MODE_ECC_FORCE_WRITE) != 0 {
                        drop(r);
                        self.note(off, size, true);
                        r = self.regs.lock().unwrap();
                    }
                    r.mode = val;
                    self.update_irq(&mut r);
                    return true;
                }
                0x08 => r.sdram_mode = val,
                0x10 => r.mem_refresh = val,
                0x18 => r.mem_req_arb = val,
                0x50 => r.flow_cell[0] = val,
                0x58 => r.flow_cell[1] = val,
                0xb0 => r.clock_divider = val,
                0x10040 => r.ecc_cause &= !val,
                0x30000 => r.compare = (val & 0xff_ffff) as u32,
                0x10000 | 0x10008 | 0x10010 | 0x10018 => {
                    r.mask[((off - 0x10000) / 8) as usize] = val;
                    self.update_irq(&mut r);
                }
                0x10020 => {
                    r.pending |= val;
                    self.update_irq(&mut r);
                }
                0x10028 => {
                    r.pending &= !val;
                    self.update_irq(&mut r);
                }
                _ => {
                    drop(r);
                    self.note(off, size, true);
                }
            }
            return true;
        }
        drop(r);
        self.note(off, size, true);
        true
    }

    // ── widget registers ───────────────────────────────

    /// HEART's widget registers as the processor and other widgets see
    /// them, `None` for what the model refuses.
    pub fn widget_read(&self, off: u32, size: usize) -> Option<u64> {
        let r = self.regs.lock().unwrap();
        match (off, size) {
            (0x00, 8) | (0x04, 4) => Some(if self.cfg.widget_id_64 != 0 {
                if size == 4 { self.cfg.widget_id_64 & 0xffff_ffff } else { self.cfg.widget_id_64 }
            } else {
                ((self.cfg.widget_revision as u64) << 28) | (0xc001 << 12) | (0x0036 << 1)
            }),
            (0x24, 4) => Some(r.widget_control as u64),
            (0x28, 8) | (0x2c, 4) => Some(r.widget_request_timeout as u64),
            (0x14 | 0x1c | 0x44, 4) => Some(0),
            (0x60 | 0x70 | 0x78 | 0x90, 8) => Some(0),
            (0x68, 8) => Some(r.widget_error_mask as u64),
            _ => {
                drop(r);
                self.note(0x1000_0000 | off, size, false);
                None
            }
        }
    }

    pub fn widget_write(&self, off: u32, size: usize, val: u64) -> bool {
        let mut r = self.regs.lock().unwrap();
        match (off, size) {
            (0x24, 4) => r.widget_control = val as u32,
            (0x28, 8) | (0x2c, 4) => r.widget_request_timeout = val as u32,
            (0x44, 4) if val == 0 => {}
            // No widget error producer exists in the model.
            (0x60, 8) => {}
            (0x68, 8) => r.widget_error_mask = val as u32,
            _ => {
                drop(r);
                self.note(0x1000_0000 | off, size, true);
                return false;
            }
        }
        true
    }

    /// An interrupt packet: a doubleword write of `value` to HEART's widget
    /// offset 0x80, bits 7:0 the vector and bit 8 set or clear. Anything
    /// else there is refused, as in the model.
    pub fn packet_write(&self, off: u64, size: usize, value: u64) -> bool {
        if off != 0x80 || size != 8 || value & !0x1ff != 0 || value & 0xff >= 64 {
            self.note(0x2000_0000 | off as u32, size, true);
            return false;
        }
        self.set_vector((value & 0xff) as u8, value & 0x100 != 0);
        true
    }

    fn set_vector(&self, vector: u8, set: bool) {
        self.packets.fetch_add(1, Ordering::Relaxed);
        let mut r = self.regs.lock().unwrap();
        if set {
            r.pending |= 1 << vector;
        } else {
            r.pending &= !(1 << vector);
        }
        self.update_irq(&mut r);
    }

    // ── memory, as the processor sees it ─────────────────

    pub fn ram_read(&self, addr: u64, n: usize) -> Option<u64> {
        self.mem.lock().unwrap().cpu_read(addr, n)
    }

    pub fn ram_write(&self, addr: u64, n: usize, val: u64) -> bool {
        self.mem.lock().unwrap().cpu_write(addr, n, val)
    }

    /// The counter, for the monitor.
    pub fn counter(&self) -> u64 {
        self.regs.lock().unwrap().counter((self.clock)())
    }
}

/// The widgets' interrupts. A vector outside 0..63 cannot be carried by the
/// packet and is dropped.
impl XioInterruptSink for Heart {
    fn xio_interrupt(&self, vector: u8, set: bool) {
        if vector < 64 {
            self.set_vector(vector, set);
        } else {
            self.note(0x2000_0080, 8, true);
        }
    }
}

/// Bus masters reach fitted memory through HEART's descriptors. A byte
/// that is not fitted memory (an absent bank included) fails the whole
/// transfer, as the model's packet face does.
impl XioMemory for Heart {
    fn read(&self, phys: u64, buf: &mut [u8]) -> bool {
        self.mem.lock().unwrap().dma_read(phys, buf)
    }

    fn write(&self, phys: u64, data: &[u8]) -> bool {
        self.mem.lock().unwrap().dma_write(phys, data)
    }
}

/// Reset: every register to zero, the counter from zero, the one-wire
/// master idle. Memory keeps its contents; with no descriptor valid none of
/// it is reachable until the PROM (or the machine) sets them again.
impl crate::traits::Resettable for Heart {
    fn power_on(&self) {
        let now = (self.clock)();
        let mut r = self.regs.lock().unwrap();
        let out = r.out;
        *r = Regs::new(now);
        r.out = out;
        self.update_irq(&mut r);
        let banks = r.banks();
        drop(r);
        self.mem.lock().unwrap().set_descriptors(banks);
    }
}

fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("IRIS_IP30_TRACE").is_some())
}

impl crate::traits::Device for Heart {
    fn step(&self, _cycles: u64) {}
    fn stop(&self) {}
    fn start(&self) {}
    fn is_running(&self) -> bool {
        true
    }
    fn get_clock(&self) -> u64 {
        0
    }

    fn register_commands(&self) -> Vec<(String, String)> {
        vec![("heart".into(), "HEART: heart [status|unsupported]".into())]
    }

    fn execute_command(&self, cmd: &str, args: &[&str], mut w: Box<dyn std::io::Write + Send>) -> Result<(), String> {
        if cmd != "heart" {
            return Err("Command not found".into());
        }
        let out = match args.first().copied().unwrap_or("status") {
            "status" => {
                let r = self.regs.lock().unwrap();
                let mut s = format!(
                    "mode {:#x} pending {:#018x} mask0 {:#018x} out IP2-6 {:05b} counter {:#x} packets {}\n",
                    r.mode,
                    r.pending,
                    r.mask[0],
                    r.out,
                    r.counter((self.clock)()),
                    self.packets.load(Ordering::Relaxed)
                );
                for (i, d) in r.descriptor[..BANKS].iter().enumerate() {
                    s.push_str(&format!("bank {i}: descriptor {d:#010x}, {} MiB fitted\n", self.cfg.banks[i] >> 20));
                }
                s
            }
            "unsupported" => self
                .unsupported()
                .iter()
                .map(|(o, s, w)| format!("{} {s} bytes at {o:#x}\n", if *w { "write" } else { "read" }))
                .collect(),
            other => return Err(format!("heart: unknown subcommand {other}")),
        };
        w.write_all(out.as_bytes()).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const PROBE_DESC: u32 = 0x81bf_0040;
    const MODULO_DESC: u32 = 0x817f_0040;
    const PROBE_RAM: u32 = RAM_BASE + 0x40 * UNIT as u32;

    struct Fixture {
        heart: Heart,
        now: Arc<AtomicU64>,
    }

    impl Fixture {
        fn step_us(&self, us: u64) {
            self.now.fetch_add(us * 1000, Ordering::SeqCst);
        }
    }

    fn heart(mib: u64) -> Fixture {
        let now = Arc::new(AtomicU64::new(0));
        let n = now.clone();
        let clock: Clock = Arc::new(move || n.load(Ordering::SeqCst));
        let heart = Heart::new(HeartConfig::octane([mib * MIB, 0, 0, 0]), clock).unwrap();
        Fixture { heart, now }
    }

    fn wq(h: &Heart, addr: u32, v: u64) {
        if (PIU_BASE..PIU_BASE + PIU_SIZE).contains(&addr) {
            assert!(h.piu_write(addr - PIU_BASE, 8, v));
        } else {
            assert!(h.ram_write(addr as u64, 8, v), "write {addr:#x}");
        }
    }

    fn rq(h: &Heart, addr: u32) -> u64 {
        if (PIU_BASE..PIU_BASE + PIU_SIZE).contains(&addr) {
            h.piu_read(addr - PIU_BASE, 8).unwrap()
        } else {
            h.ram_read(addr as u64, 8).unwrap()
        }
    }

    fn wl(h: &Heart, off: u32, v: u32) {
        assert!(h.piu_write(off, 4, v as u64));
    }

    #[test]
    fn descriptors_find_the_fitted_size() {
        for mib in [64u64, 128, 256] {
            let f = heart(mib);
            let h = &f.heart;
            let span = mib * MIB;
            assert_eq!(h.piu_read(0x20, 8), Some(0));
            wl(h, 0x20, MODULO_DESC);
            assert_eq!(h.piu_read(0x20, 4), Some(MODULO_DESC as u64));
            wq(h, PROBE_RAM + 0x100, 0x1234_5678_9abc_def0);
            let mut units = 0;
            for unit in 1..32u32 {
                if rq(h, PROBE_RAM + 0x100 + unit * UNIT as u32) == 0x1234_5678_9abc_def0 {
                    units = unit;
                    break;
                }
            }
            assert_eq!(units as u64, mib / 32, "{mib} MiB");
            assert_eq!(rq(h, PROBE_RAM + 0x100 + span as u32), 0x1234_5678_9abc_def0);
            // An absent descriptor still claims probe space but keeps nothing.
            wl(h, 0x20, 0);
            wl(h, 0x24, PROBE_DESC);
            assert!(h.ram_write(PROBE_RAM as u64 + 0x100, 8, u64::MAX));
            assert_eq!(h.ram_read(PROBE_RAM as u64 + 0x100, 8), Some(0));
            wq(h, PIU_BASE + 0x20, 0);
            assert_eq!(h.piu_read(0x20, 8), Some(0));
            assert_eq!(h.ram_read(PROBE_RAM as u64, 8), None);
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
            let f = heart(mib);
            let h = &f.heart;
            let final_desc = DESC_VALID | (((mib * MIB / UNIT) as u32 - 1) << 16);
            for off in [0u64, 8, 0x100, UNIT, 2 * UNIT, 4 * UNIT, 8 * UNIT] {
                wl(h, 0x20, PROBE_DESC);
                wq(h, PROBE_RAM + off as u32, 0x98a9_bacb_dced_fe0f);
                // DMA reaches the same bytes through the same descriptor.
                let mut b = [0u8; 8];
                assert!(XioMemory::read(h, PROBE_RAM as u64 + off, &mut b));
                assert_eq!(u64::from_be_bytes(b), 0x98a9_bacb_dced_fe0f);
                assert!(XioMemory::write(h, PROBE_RAM as u64 + off, &0x1021_3243_5465_7687u64.to_be_bytes()));
                wl(h, 0x20, final_desc);
                assert_eq!(rq(h, RAM_BASE + projected(mib, off) as u32), 0x1021_3243_5465_7687, "{mib} {off:#x}");
            }
        }
    }

    /// The stock PROM's bank sizing, as the model's qtest replays it: every
    /// size reaches the descriptor the PROM's table names for it.
    #[test]
    fn the_prom_probe_reaches_the_right_descriptor() {
        const TABLE: [u32; 16] = [
            0x0000_0040, 0x0001_0040, 0x0043_0040, 0x0047_0040, 0x008f_0040, 0x009f_0040, 0x0000_0040, 0x0000_0040,
            0x0101_0040, 0x0103_0040, 0x0147_0040, 0x014f_0040, 0x019f_0040, 0x01bf_0040, 0x0000_0040, 0x0000_0040,
        ];
        fn distinct(h: &Heart, base: u32, step: u64, pattern: u64) -> bool {
            wq(h, base, pattern);
            wq(h, base + step as u32, !pattern);
            rq(h, base) == pattern
        }
        fn capacity(h: &Heart, base: u32) -> u32 {
            let pattern = 0x1111_1111_1111_1111u64;
            let mut bits = 0;
            wq(h, base + 0x100, pattern);
            wq(h, base, !pattern);
            if rq(h, base + 0x100) == pattern {
                bits |= 0x20;
            }
            if distinct(h, base, 4 * UNIT, pattern << 1) {
                bits |= 0x10;
            } else if distinct(h, base, UNIT, pattern << 2) {
                bits |= 0x08;
            }
            if distinct(h, base, 32 * UNIT, pattern << 3) {
                bits |= 0x04;
            }
            TABLE[bits / 4]
        }
        for (mib, want) in [(64u64, 0x8101_0040u32), (128, 0x8103_0040), (256, 0x8147_0040), (512, 0x814f_0040)] {
            let f = heart(mib);
            let h = &f.heart;
            wl(h, 0x20, PROBE_DESC);
            let word = capacity(h, PROBE_RAM) | DESC_VALID;
            wl(h, 0x20, word);
            if (word >> 22) & 1 != 0 {
                wq(h, PROBE_RAM + 0x80, 0);
                wq(h, PROBE_RAM + UNIT as u32 + 0x80, !0);
                assert_ne!(rq(h, PROBE_RAM + 0x80), rq(h, PROBE_RAM + UNIT as u32 + 0x80));
            }
            assert_eq!(word, want, "{mib} MiB");
            for unit in 0..mib / 32 {
                wq(h, PROBE_RAM + (unit * UNIT) as u32 + 0x100, 0x1021_3243_5465_7687 + unit);
            }
            for unit in 0..mib / 32 {
                assert_eq!(rq(h, PROBE_RAM + (unit * UNIT) as u32 + 0x100), 0x1021_3243_5465_7687 + unit);
            }
        }
    }

    #[test]
    fn the_window_follows_the_descriptors() {
        let f = heart(128);
        let h = &f.heart;
        wl(h, 0x20, DESC_VALID | (3 << 16));
        wq(h, RAM_BASE + 0x1000, 0x0123_4567_89ab_cdef);
        let m = h.mem.lock().unwrap();
        let bits = m.space().mapped_bitmap();
        assert_eq!(bits, 0b11 << (RAM_BASE >> 26), "128 MiB from 0x20000000 is two 64 MiB regions");
        let p = unsafe { m.space().window_base().add(RAM_BASE as usize + 0x1000) } as *const u32;
        assert_eq!(unsafe { p.read_unaligned() }, 0x0123_4567, "the window holds host words");
    }

    #[test]
    fn widget_identification() {
        let f = heart(128);
        let h = &f.heart;
        assert_eq!(h.widget_read(4, 4), Some(0x4c00_106d));
        assert_eq!(h.widget_read(0, 8), Some(0x4c00_106d));
        assert_eq!(h.widget_read(0, 1), None);
        assert_eq!(h.widget_read(0, 4), None);
        assert_eq!(h.widget_read(8, 8), None);
    }

    #[test]
    fn packets_set_and_clear_vectors_by_level() {
        let f = heart(128);
        let h = &f.heart;
        let (l0, l1, l2) = (1u64 << 3, 1u64 << 16, 1u64 << 42);
        assert!(!h.packet_write(0x80, 4, 0x103));
        assert!(!h.packet_write(0x80, 8, 0x140));
        assert!(!h.packet_write(0x80, 8, 0x303));
        assert!(!h.packet_write(0x88, 8, 0x103));
        assert_eq!(h.pending(), 0);
        wq(h, PIU_BASE + 0x10000, l0 | l1 | l2);
        assert!(h.packet_write(0x80, 8, 0x103));
        assert_eq!(h.pending(), l0);
        assert!(h.pin(2) && !h.pin(3) && !h.pin(4));
        assert!(h.packet_write(0x80, 8, 0x110));
        assert!(h.pin(3));
        h.xio_interrupt(42, true);
        assert_eq!(rq(h, PIU_BASE + 0x10038), l0 | l1 | l2);
        assert!(h.pin(4));
        // Masking drops the pin; the pending bit stays.
        wq(h, PIU_BASE + 0x10000, l0 | l2);
        assert_eq!(h.pending(), l0 | l1 | l2);
        assert!(!h.pin(3));
        wq(h, PIU_BASE + 0x10000, l0 | l1 | l2);
        assert!(h.pin(3));
        h.xio_interrupt(16, false);
        assert_eq!(h.pending(), l0 | l2);
        assert!(h.pin(2) && !h.pin(3) && h.pin(4));
        h.xio_interrupt(3, false);
        assert!(!h.pin(2));
        h.xio_interrupt(42, false);
        assert!(!h.pin(4));
    }

    #[test]
    fn set_and_clear_registers() {
        let f = heart(128);
        let h = &f.heart;
        wq(h, PIU_BASE + 0x10008, 2);
        wq(h, PIU_BASE + 0x10020, 3);
        assert_eq!(rq(h, PIU_BASE + 0x10030), 3);
        assert_eq!(rq(h, PIU_BASE + 0x10038), 0);
        wq(h, PIU_BASE + 0x10000, 1);
        assert_eq!(rq(h, PIU_BASE + 0x10038), 1);
        wq(h, PIU_BASE + 0x10028, 1);
        assert_eq!(rq(h, PIU_BASE + 0x10030), 2);
        assert_eq!(rq(h, PIU_BASE + 0x10008), 2);
    }

    #[test]
    fn timer_and_error_levels() {
        let f = heart(128);
        let h = &f.heart;
        let (timer, error) = (1u64 << 50, 1u64 << 51);
        wq(h, PIU_BASE + 0x10020, timer | error);
        assert!(!h.pin(5) && !h.pin(6));
        wq(h, PIU_BASE + 0x10000, timer | error);
        assert!(h.pin(5) && h.pin(6));
        wq(h, PIU_BASE + 0x10000, error);
        assert!(!h.pin(5) && h.pin(6));
        wq(h, PIU_BASE + 0x10028, error);
        assert!(!h.pin(6));
        // The memory-error bit reaches IP6 only with interrupts enabled.
        wq(h, PIU_BASE + 0x10020, ECC_PENDING);
        wq(h, PIU_BASE + 0x10000, ECC_PENDING);
        assert!(!h.pin(6));
        wq(h, PIU_BASE, MODE_INTERRUPT_ENABLE);
        assert!(h.pin(6));
        crate::traits::Resettable::power_on(h);
        assert!(!h.pin(6));
    }

    #[test]
    fn the_counter_ticks_at_12_5_mhz_and_resets() {
        let f = heart(128);
        let h = &f.heart;
        assert_eq!(rq(h, PIU_BASE + 0x80), 0x18);
        f.now.fetch_add(800, Ordering::SeqCst);
        assert_eq!(rq(h, PIU_BASE + 0x20000), 10);
        wl(h, 0x20, PROBE_DESC);
        wq(h, PROBE_RAM, 0xfeed_face);
        wq(h, PIU_BASE + 0x10000, 1 << 16);
        wq(h, PIU_BASE + 0x10020, 1 << 16);
        assert!(h.pin(3));
        crate::traits::Resettable::power_on(h);
        assert_eq!(rq(h, PIU_BASE + 0x20000), 0);
        assert_eq!(rq(h, PIU_BASE + 0x10030), 0);
        assert!(!h.pin(3));
        assert_eq!(h.piu_read(0x20, 4), Some(0));
        assert_eq!(h.ram_read(PROBE_RAM as u64, 8), None);
        // Memory survives; a descriptor brings it back.
        wl(h, 0x20, PROBE_DESC);
        assert_eq!(rq(h, PROBE_RAM), 0xfeed_face);
    }

    #[test]
    fn narrow_piu_accesses_are_refused() {
        let f = heart(128);
        assert_eq!(f.heart.piu_read(0, 1), None);
        assert!(!f.heart.piu_write(0, 2, 0));
    }

    #[test]
    fn dma_needs_every_byte_fitted() {
        let f = heart(64);
        let h = &f.heart;
        wl(h, 0x20, DESC_VALID | (1 << 16));
        let end = RAM_BASE as u64 + 64 * MIB;
        assert!(XioMemory::write(h, end - 8, &[1; 8]));
        assert!(!XioMemory::write(h, end - 4, &[2; 8]));
        let mut b = [9u8; 8];
        assert!(!XioMemory::read(h, end - 4, &mut b));
        assert_eq!(b, [0; 8]);
        assert!(XioMemory::read(h, end - 8, &mut b));
        assert_eq!(b, [1; 8]);
        // Across a unit boundary, DMA and the CPU agree.
        assert!(XioMemory::write(h, RAM_BASE as u64 + UNIT - 4, &[1, 2, 3, 4, 5, 6, 7, 8]));
        assert_eq!(h.ram_read(RAM_BASE as u64 + UNIT - 4, 8), Some(0x0102_0304_0506_0708));
    }

    #[test]
    fn invalid_bank_sizes_are_refused() {
        assert!(HeartMemory::new([96 * MIB, 0, 0, 0]).is_err());
        assert!(HeartMemory::new([32 * MIB, 0, 0, 0]).is_err());
    }

    struct Present;
    impl OneWireResponder for Present {
        fn reset(&mut self) -> bool {
            true
        }
        fn touch_bit(&mut self, bit: bool) -> bool {
            bit
        }
    }

    #[test]
    fn one_wire_slots_complete_after_their_time() {
        let f = heart(64);
        let h = &f.heart;
        let cmd = |pulse: u64, sample: u64| (pulse << 10) | (sample << 2);
        let reset = cmd(500, 65);
        assert_eq!(h.piu_read(0xb8, 8), Some(0));
        assert!(h.piu_write(0xb8, 8, reset));
        assert_eq!(h.piu_read(0xb8, 8).unwrap() & 3, 0);
        f.step_us(564);
        assert_eq!(h.piu_read(0xbc, 4).unwrap() & 3, 0);
        f.step_us(1);
        // No part: the wire stays high through the presence sample.
        assert_eq!(h.piu_read(0xb8, 8), Some(reset | 3));
        let read = cmd(6, 13);
        assert!(h.piu_write(0xbc, 4, read));
        f.step_us(19);
        assert_eq!(h.piu_read(0xbc, 4), Some(read | 3));
        assert!(h.piu_write(0xbc, 4, cmd(80, 30)));
        f.step_us(110);
        assert_eq!(h.piu_read(0xbc, 4).unwrap() & 3, 2);

        h.attach_onewire(Box::new(Present));
        assert!(h.piu_write(0xbc, 4, reset));
        f.step_us(565);
        assert_eq!(h.piu_read(0xbc, 4).unwrap() & 3, 2, "presence pulls the wire low");
    }
}
