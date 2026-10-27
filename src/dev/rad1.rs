//! SGI Octane onboard RAD audio (PCI 10a9:0005, revision 0xc0) on Bridge
//! slot 3.
//!
//! This is the Octane variant of our RAD1 model: the register file the
//! driver's start-up programs, the free-running UST counter and its control
//! word, the DMA descriptor words, status-DMA control and the analog output
//! enable. Behaviour that no source establishes is kept as the model
//! has it, and flagged below.
//!
//! Approximations kept from the model:
//!
//! * BAR0 is 0x2000 bytes and BAR sizing is a model property, not a
//!   measurement of a physical card.
//! * The PCI class is "not defined" (0) and the card raises no interrupt:
//!   the interrupt pin is 0.
//! * The start-up words, the AES3 subcode window and the descriptor bank
//!   store what was written and return it. No reset values or masks are
//!   established beyond zero; the few writes the model refuses (see
//!   [`Rad1::write`]) are refused because no supported guest makes them.
//! * UST ticks every 960 ns, a selected board policy; the physical rate is
//!   unmeasured.
//! * A BAR0 access the model does not recognise is logged and reads zero.
//!   The model fails the bus access instead, which the PCI bus layer here
//!   cannot express.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use crate::pci::{Bar, PciBusDevice, PciConfig};

pub const PCI_VENDOR_SGI: u16 = 0x10a9;
pub const PCI_DEVICE_RAD1: u16 = 0x0005;
pub const RAD1_REVISION: u8 = 0xc0;
/// "Not defined" (base class 0), as the model has it.
const PCI_CLASS_NOT_DEFINED: u32 = 0;
pub const BAR0_SIZE: u32 = 0x2000;

const REG_UST_COUNTER: u32 = 0x30;
const REG_UST_CONTROL: u32 = 0x40;
const REG_PREDISPATCH_B: u32 = 0x48;
const REG_SHIFT_LOAD_CLOCK: u32 = 0x4c;
const REG_ANALOG_OUT_EN: u32 = 0x58;
const REG_STATUS_DMA_CTRL: u32 = 0x5c;

const POS_FIRST: u32 = 0x04;
const POS_LAST: u32 = 0x2c;
const POS_WORDS: usize = ((POS_LAST - POS_FIRST) / 4 + 1) as usize;

const DMA_BANK_FIRST: u32 = 0x400;
const DMA_BANK_LAST: u32 = 0x528;
const DMA_BANK_WORDS: usize = ((DMA_BANK_LAST - DMA_BANK_FIRST) / 4 + 1) as usize;

/// A descriptor set is three registers (address low, address high, control)
/// for set A, then the same for set B.
type RingRegs = [u32; 6];
const RING_STATUS: RingRegs = [0x500, 0x528, 0x504, 0x4b4, 0x4b8, 0x4bc];
const RING_INPUT: RingRegs = [0x4d0, 0x510, 0x4d4, 0x400, 0x404, 0x408];
const RING_OUTPUT: RingRegs = [0x4f8, 0x524, 0x4fc, 0x40c, 0x410, 0x414];
const RING_AES_SUB: RingRegs = [0x4e0, 0x518, 0x4e4, 0x448, 0x44c, 0x450];
const RINGS: [RingRegs; 4] = [RING_STATUS, RING_INPUT, RING_OUTPUT, RING_AES_SUB];

/// The start-up words: stored and read back. Unsupported interfaces stay
/// disabled (see the refused values in [`Rad1::write`]).
const STARTUP_OFFSETS: [u32; 26] = [
    0x44, 0x48, 0x4c, 0x50, 0x54, 0x70, 0x74, 0x78, 0x7c, 0x80, 0x88, 0x8c, 0x90, 0xa0, 0xa4,
    0xa8, 0xac, 0xb0, 0xb4, 0xb8, 0xbc, 0x4c0, 0x4c8, 0x4d8, 0x4e8, 0x4f0,
];

const SUBCODE_FIRST: u32 = 0x1000;
const SUBCODE_LAST: u32 = 0x1218;
const SUBCODE_WORDS: usize = ((SUBCODE_LAST - SUBCODE_FIRST) / 8 + 1) as usize;

const ENABLE_BIT: u32 = 1;
const SUBCODE_ENABLE_BIT: u32 = 4;
const CTRL_TAG_SHIFT: u32 = 3;
const CTRL_TAG_MASK: u32 = 0xf;
const CTRL_LEN_SHIFT: u32 = 7;
const CTRL_LEN_MASK: u32 = 0x1ff_ffff;
const DMA_UNIT_BYTES: u64 = 128;
/// Largest ring walked; only stops a garbage control word naming gigabytes.
const DMA_MAX_BYTES: u64 = 1 << 20;

const STATUS_START: u32 = 0x40003;
const STATUS_ONESHOT_START: u32 = 0x40001;
const STATUS_STOP: u32 = 0x1fffc;

const UST_NS_PER_TICK: i64 = 960;

/// Channel types, as indices into [`State::chan`]. Only the three the
/// Octane supports are modelled.
const CH_IN: usize = 0;
const CH_OUT: usize = 1;
const CH_AES_SUB: usize = 2;
const NUM_CH: usize = 3;

struct ChanInfo {
    name: &'static str,
    ring: RingRegs,
    tag: u32,
    /// Bytes in one frame of the ring.
    frame_bytes: u64,
    enable_bit: u32,
}

const CHAN_INFO: [ChanInfo; NUM_CH] = [
    ChanInfo { name: "analog input", ring: RING_INPUT, tag: 0, frame_bytes: 8, enable_bit: ENABLE_BIT },
    ChanInfo { name: "analog output", ring: RING_OUTPUT, tag: 1, frame_bytes: 8, enable_bit: ENABLE_BIT },
    ChanInfo { name: "AES3-input subcode", ring: RING_AES_SUB, tag: 6, frame_bytes: 256, enable_bit: SUBCODE_ENABLE_BIT },
];

#[derive(Default, Clone, Copy)]
struct Chan {
    running: bool,
    /// Ring latched from set A at the enable edge.
    base: u64,
    len: u64,
    offset: u64,
    start_ns: i64,
    frames: u64,
}

struct State {
    startup: [u32; 26],
    subcode: [u64; SUBCODE_WORDS],
    dma_bank: [u32; DMA_BANK_WORDS],
    sample_pos: [u32; POS_WORDS],
    analog_out_en: u32,
    status_dma_ctrl: u32,
    status_running: bool,
    chan: [Chan; NUM_CH],
    ust_count: u32,
    ust_phase: i64,
    ust_enabled: bool,
    ust_armed: bool,
    ust_last_ns: i64,
}

impl State {
    fn new(now: i64) -> Self {
        Self {
            startup: [0; 26],
            subcode: [0; SUBCODE_WORDS],
            dma_bank: [0; DMA_BANK_WORDS],
            sample_pos: [0; POS_WORDS],
            analog_out_en: 0,
            status_dma_ctrl: 0,
            status_running: false,
            chan: [Chan::default(); NUM_CH],
            ust_count: 0,
            ust_phase: 0,
            ust_enabled: false,
            ust_armed: false,
            ust_last_ns: now,
        }
    }

    fn bank(&self, reg: u32) -> u32 {
        self.dma_bank[((reg - DMA_BANK_FIRST) / 4) as usize]
    }

    /// Advance the UST counter to `now`. It counts only while enabled.
    fn ust_update(&mut self, now: i64) {
        if self.ust_enabled && now > self.ust_last_ns {
            let total = now - self.ust_last_ns + self.ust_phase;
            self.ust_count = self.ust_count.wrapping_add((total / UST_NS_PER_TICK) as u32);
            self.ust_phase = total % UST_NS_PER_TICK;
        }
        self.ust_last_ns = now;
    }

    /// Whether the descriptor set `ring` is programmed for a transfer with
    /// control word `limit`: an address, the control word, and set B equal
    /// to set A. The status ring's whole-memory limit needs no set B.
    fn ring_valid(&self, ring: &RingRegs, is_status: bool, limit: u32) -> bool {
        if (self.bank(ring[0]) == 0 && self.bank(ring[1]) == 0) || self.bank(ring[2]) != limit {
            return false;
        }
        if is_status && limit == 0xffff_fffc {
            return true;
        }
        self.bank(ring[0]) == self.bank(ring[3])
            && self.bank(ring[1]) == self.bank(ring[4])
            && self.bank(ring[5]) == limit
    }

    fn ring_running(&self, ring: &RingRegs) -> bool {
        if *ring == RING_INPUT {
            self.chan[CH_IN].running
        } else if *ring == RING_OUTPUT {
            self.chan[CH_OUT].running
        } else if *ring == RING_AES_SUB {
            self.chan[CH_AES_SUB].running
        } else {
            self.status_running
        }
    }

    /// The enable edge: latch set A and start. A ring that is not
    /// programmed, or not a whole number of frames, starts nothing.
    fn chan_start(&mut self, idx: usize, now: i64) {
        let info = &CHAN_INFO[idx];
        let lo = self.bank(info.ring[0]);
        let hi = self.bank(info.ring[1]);
        let ctrl = self.bank(info.ring[2]);
        let base = (u64::from(hi) << 32) | u64::from(lo);
        let neg = (ctrl >> CTRL_LEN_SHIFT) & CTRL_LEN_MASK;
        let units = 0u32.wrapping_sub(neg) & CTRL_LEN_MASK;
        let mut len = u64::from(units) * DMA_UNIT_BYTES;
        let tag = (ctrl >> CTRL_TAG_SHIFT) & CTRL_TAG_MASK;

        if base == 0 || len == 0 {
            eprintln!("rad1: {} enabled with no ring programmed (address {base:#x}, control {ctrl:#x})", info.name);
            return;
        }
        if len > DMA_MAX_BYTES {
            eprintln!("rad1: {} ring length {len:#x} exceeds the modelled cap; clamping", info.name);
            len = DMA_MAX_BYTES;
        }
        // What hardware does with a partial frame is unknown; start nothing.
        if len % info.frame_bytes != 0 {
            eprintln!("rad1: {} ring length {len:#x} is not a whole number of frames", info.name);
            return;
        }
        if tag != info.tag {
            eprintln!("rad1: {} control word {ctrl:#x} carries tag {tag}, not the channel type {}", info.name, info.tag);
        }
        self.chan[idx] = Chan { running: true, base, len, offset: 0, start_ns: now, frames: 0 };
    }

    fn chan_stop(&mut self, idx: usize) {
        self.chan[idx].running = false;
    }

    /// A write to a channel's enable register: its own bit starts and stops
    /// it; rewriting a set bit while it runs does not restart it.
    fn enable_write(&mut self, idx: usize, val: u32, now: i64) {
        if val & CHAN_INFO[idx].enable_bit != 0 {
            if !self.chan[idx].running {
                self.chan_start(idx, now);
            }
        } else {
            self.chan_stop(idx);
        }
    }
}

pub struct Rad1 {
    pci: PciConfig,
    state: Mutex<State>,
    start: Instant,
    /// When not `i64::MIN`, the clock in nanoseconds, for tests.
    fake_ns: AtomicI64,
}

impl Default for Rad1 {
    fn default() -> Self {
        Self::new()
    }
}

impl Rad1 {
    pub fn new() -> Self {
        Self {
            pci: PciConfig::new(
                PCI_VENDOR_SGI,
                PCI_DEVICE_RAD1,
                PCI_CLASS_NOT_DEFINED,
                RAD1_REVISION,
                [Bar::Mem(BAR0_SIZE), Bar::Unused, Bar::Unused, Bar::Unused, Bar::Unused, Bar::Unused],
                0,
            ),
            state: Mutex::new(State::new(0)),
            start: Instant::now(),
            fake_ns: AtomicI64::new(i64::MIN),
        }
    }

    fn now(&self) -> i64 {
        match self.fake_ns.load(Ordering::Relaxed) {
            i64::MIN => self.start.elapsed().as_nanos() as i64,
            t => t,
        }
    }

    /// Back to the power-on state.
    pub fn reset(&self) {
        let now = self.now();
        *self.state.lock().unwrap() = State::new(now);
    }

    fn startup_index(off: u32) -> Option<usize> {
        STARTUP_OFFSETS.iter().position(|&o| o == off)
    }

    fn is_descriptor(off: u32) -> bool {
        RINGS.iter().any(|r| r.contains(&off))
    }

    fn subcode_access(off: u32, width: usize) -> bool {
        let word = off & !7;
        // The model also takes 8-byte accesses; this bus layer's widest is 4.
        width == 4
            && off & 3 == 0
            && (SUBCODE_FIRST..=SUBCODE_LAST).contains(&word)
            && word != 0x1078
            && word != 0x10f8
    }

    fn read(&self, off: u32, width: usize) -> u32 {
        let now = self.now();
        let mut s = self.state.lock().unwrap();
        if width == 4 {
            if let Some(i) = Self::startup_index(off) {
                return s.startup[i];
            }
        }
        if Self::subcode_access(off, width) {
            let w = s.subcode[((off - SUBCODE_FIRST) / 8) as usize];
            return (w >> ((off & 4) * 8)) as u32;
        }
        if width == 4 && off & 3 == 0 {
            if Self::is_descriptor(off) {
                return s.bank(off);
            }
            if (POS_FIRST..=POS_LAST).contains(&off) {
                return s.sample_pos[((off - POS_FIRST) / 4) as usize];
            }
            if off == REG_ANALOG_OUT_EN {
                return s.analog_out_en;
            }
            if off == REG_STATUS_DMA_CTRL {
                return s.status_dma_ctrl;
            }
        }
        if off == REG_UST_COUNTER && width == 4 {
            s.ust_update(now);
            return s.ust_count;
        }
        eprintln!("rad1: unsupported {width}-byte BAR0 read at {off:#x}");
        0
    }

    /// A write the model does not support is logged and dropped. The model
    /// fails the bus cycle; this bus layer cannot, so a refused write leaves
    /// the register unchanged.
    fn write(&self, off: u32, val: u32, width: usize) {
        let now = self.now();
        let mut s = self.state.lock().unwrap();
        if self.write_locked(&mut s, off, val, width, now).is_none() {
            eprintln!("rad1: unsupported {width}-byte BAR0 write at {off:#x} value {val:#x}");
        }
    }

    /// `None` when the write is unsupported.
    fn write_locked(&self, s: &mut State, off: u32, val: u32, width: usize, now: i64) -> Option<()> {
        if width == 4 {
            if let Some(i) = Self::startup_index(off) {
                let refused = match off {
                    0x44 => 5,
                    0x48 | 0x50 | 0x54 => 1,
                    _ => 0,
                };
                if val & refused != 0 {
                    return None;
                }
                if off == REG_SHIFT_LOAD_CLOCK {
                    // The analog input's interface word: bit 0 runs it.
                    if val & ENABLE_BIT != 0
                        && !s.chan[CH_IN].running
                        && !s.ring_valid(&RING_INPUT, false, 0xffff_f000)
                    {
                        return None;
                    }
                    s.enable_write(CH_IN, val, now);
                } else if off == REG_PREDISPATCH_B {
                    // AES3 input control: bit 2 runs the input subcode channel.
                    if val & SUBCODE_ENABLE_BIT != 0
                        && !s.chan[CH_AES_SUB].running
                        && !s.ring_valid(&RING_AES_SUB, false, 0xffff_f030)
                    {
                        return None;
                    }
                    s.enable_write(CH_AES_SUB, val, now);
                }
                s.startup[i] = val;
                return Some(());
            }
        }
        if Self::subcode_access(off, width) {
            let word = &mut s.subcode[((off - SUBCODE_FIRST) / 8) as usize];
            let shift = (off & 4) * 8;
            *word = (*word & !(0xffff_ffffu64 << shift)) | (u64::from(val) << shift);
            return Some(());
        }
        if width == 4 && off & 3 == 0 {
            if let Some(ring) = RINGS.iter().find(|r| r.contains(&off)) {
                if !s.ring_running(ring) {
                    s.dma_bank[((off - DMA_BANK_FIRST) / 4) as usize] = val;
                    return Some(());
                }
            } else if off == REG_ANALOG_OUT_EN && (val == 0x2000_0000 || val == 0x2000_0001) {
                if val == 0x2000_0000 || s.ring_valid(&RING_OUTPUT, false, 0xffff_f008) {
                    s.enable_write(CH_OUT, val, now);
                    s.analog_out_en = val;
                    return Some(());
                }
            } else if off == REG_STATUS_DMA_CTRL {
                self.status_ctrl_write(s, val)?;
                s.status_dma_ctrl = val;
                return Some(());
            }
        }
        if off == REG_UST_CONTROL && width == 4 {
            match val {
                0x0e => {
                    s.ust_update(now);
                    s.ust_enabled = false;
                    s.ust_armed = true;
                    return Some(());
                }
                0x0a if s.ust_armed => {
                    s.ust_update(now);
                    s.ust_enabled = true;
                    s.ust_armed = false;
                    return Some(());
                }
                0x02 => {
                    s.ust_update(now);
                    s.ust_enabled = false;
                    s.ust_armed = false;
                    return Some(());
                }
                _ => {}
            }
        }
        None
    }

    /// The status-DMA control word: stop, one-shot start, or ring start.
    fn status_ctrl_write(&self, s: &mut State, val: u32) -> Option<()> {
        if val == STATUS_STOP {
            s.status_running = false;
        } else if val == STATUS_ONESHOT_START
            && !s.status_running
            && s.ring_valid(&RING_STATUS, true, 0xffff_fffc)
        {
            // The status slot is filled in by the DMA engine.
        } else if val & !0x1fffc == STATUS_START
            && val & 0x1fffc != 0
            && !s.status_running
            && s.ring_valid(&RING_STATUS, true, 0xffff_c078)
        {
            s.status_running = true;
        } else {
            return None;
        }
        Some(())
    }
}

impl PciBusDevice for Rad1 {
    fn config(&self) -> &PciConfig {
        &self.pci
    }

    fn bar_read(&self, _bar: usize, off: u32, width: usize) -> u32 {
        self.read(off, width)
    }

    fn bar_write(&self, _bar: usize, off: u32, val: u32, width: usize) {
        self.write(off, val, width)
    }
}

#[cfg(test)]
mod tests;
