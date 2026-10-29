//! The V12 graphics board's Odyssey widget: register access, command FIFO,
//! integer drawing and scanout from the board's stored planes.
//!
//! Scalar GL command execution, pixel DMA, object storage and context
//! save/restore are absent. Unknown operations are reported without
//! synthesizing completion. The optional memory connection is reserved
//! for DMA, which is not implemented here.
//!
//! Ported from hw/misc/sgi-odyssey.c in jamesbraid/qemu-sgi-origami.

mod display;
mod engine;
mod render;
pub mod scalar;
use crate::dev::w1;
use crate::traits::Device;
use crate::traits::{BusDevice, BusRead16, BusRead32, BusRead64, BusRead8, Resettable, BUS_OK};
use engine::{Engine, Submission};
use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

pub const APERTURE: u64 = 0x410000;
const LOW_WATER: u32 = 0x200;

/// Fuel's V12 profile. Memory configuration and board revision follow the
/// donor's measured V12 selection; its serial number is synthetic.
#[derive(Clone, Copy)]
pub struct Config {
    pub widget: u8,
    pub nic_serial: u64,
}
impl Config {
    pub fn v12(widget: u8) -> Self {
        assert!(widget < 16);
        Self {
            widget,
            nic_serial: 0x495249534700,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet {
    pub widget: u8,
    pub address: u64,
    pub vector: u8,
    pub set: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Unsupported {
    pub address: u32,
    pub width: u8,
    pub write: bool,
}

struct State {
    regs: BTreeMap<u32, u32>,
    flags: u32,
    enabled: u32,
    asserted: bool,
    selftest_progress: usize,
    selftest_ready: bool,
    backend_index: u32,
    backend_count: u32,
    backend: Vec<u32>,
    nic: w1::Bus,
    unsupported: BTreeSet<Unsupported>,
    cursor_reports: BTreeMap<u32, Unsupported>,
}
impl State {
    fn new(c: Config) -> Self {
        let mut regs = BTreeMap::new();
        for a in [
            0x20, 0x28, 0x30, 0x38, 0x40, 0x1044, 0x104c, 0x1054, 0x1070, 0x107c, 0x1078, 0x1088,
            0x1098, 0x10a8, 0x10b8, 0x10c0, 0x10b4, 0x10bc, 0x10c4, 0x10cc, 0x10d4, 0x10d8, 0x4034,
            0x403c, 0x4004, 0x400c,
        ] {
            regs.insert(a, 0);
        }
        regs.insert(0x20, c.widget as u32);
        regs.insert(0x28, 0xfffff);
        regs.insert(0x1070, 0x1c000);
        let mut image = [0xff; 160];
        let record = b"\x01    ;Name:ODY128VERSIONB;";
        image[..record.len()].copy_from_slice(record);
        image[record.len()..32].fill(b' ');
        let mut nic = w1::Bus::new();
        nic.add(Box::new(w1::Ds2505::new(0x0b, c.nic_serial, &image)));
        Self {
            regs,
            flags: LOW_WATER,
            enabled: 0,
            asserted: false,
            selftest_progress: 0,
            selftest_ready: false,
            backend_index: 0,
            backend_count: 0,
            backend: vec![0; 0x4000],
            nic,
            unsupported: BTreeSet::new(),
            cursor_reports: BTreeMap::new(),
        }
    }
    fn report(&mut self, a: u32, width: u8, write: bool) {
        if self.unsupported.len() < 64
            && self.unsupported.insert(Unsupported {
                address: a,
                width,
                write,
            })
        {
            eprintln!(
                "odyssey: unsupported {} at {a:#x} ({width} bytes)",
                if write { "write" } else { "read" }
            );
        }
    }
    fn cursor_report(&mut self, reason: u32, index: u32) {
        if self.cursor_reports.contains_key(&reason) {
            return;
        }
        let event = Unsupported {
            address: 0x220000 + index * 8,
            width: 4,
            write: true,
        };
        let meaning = match reason {
            1 => "cursor glyph 15 drawn transparent",
            2 => "cursor planes 1 and 2 drawn transparent",
            4 => "crosshair approximated with full-raster lines",
            8 => "second display channel is not modeled",
            16 => "unknown cursor control bits",
            32 => "unknown cursor entry retained",
            64 => "unknown overlay mode bits",
            _ => "colormap group 25 uses an unverified index table",
        };
        eprintln!("odyssey: {meaning} at backend {index:#x}");
        self.cursor_reports.insert(reason, event);
    }
    fn packet(&self, shift: u32, set: bool) -> Option<Packet> {
        let upper = self.regs[&0x1044];
        let lower = self.regs[&0x104c];
        if upper == 0 && lower == 0 {
            return None;
        }
        Some(Packet {
            widget: ((upper >> 25) & 15) as u8,
            address: ((upper as u64 & 0xffff) << 32) | lower as u64,
            vector: (self.regs[&0x1054] >> shift) as u8,
            set,
        })
    }
    fn irq(&mut self) -> Option<Packet> {
        let level = self.flags & self.enabled != 0;
        if level == self.asserted {
            return None;
        }
        self.asserted = level;
        self.packet(0, level)
    }
    fn backend_word(&mut self, word: u32) {
        if self.backend_count == 0 {
            if word >> 28 & 3 != 3 {
                self.report(0x400000, 4, true);
                return;
            }
            self.backend_index = word >> 14 & 0x3fff;
            self.backend_count = word & 0x3fff;
        } else {
            let i = self.backend_index;
            if [0x2802, 0x2803].contains(&i) && word == 0 {
                self.backend_index = (i + 1) & 0x3fff;
                self.backend_count -= 1;
                return;
            }
            if (i == 0x2802 && word > 1280) || (i == 0x2803 && word > 1024) {
                self.report(0x220000 + i * 8, 4, true);
            }
            if (0x2500..0x2580).contains(&i)
                && (0..32).step_by(4).any(|shift| word >> shift & 15 == 15)
            {
                self.cursor_report(1, i);
            }
            if (0x25c0..0x2600).contains(&i)
                && (0..32)
                    .step_by(2)
                    .any(|shift| [1, 2].contains(&(word >> shift & 3)))
            {
                self.cursor_report(2, i);
            }
            if i == 0x2581 {
                if word & 5 == 5 {
                    self.cursor_report(4, i);
                }
                if word & 0x20 != 0 {
                    self.cursor_report(8, i);
                }
                if word & !0xc000003f != 0 {
                    self.cursor_report(16, i);
                }
            }
            if (0x2582..0x2590).contains(&i) || (0x259f..0x25c0).contains(&i) {
                self.cursor_report(32, i);
            }
            if (0x2920..0x2928).contains(&i) && word & !0xf9 != 0 {
                self.cursor_report(64, i);
            }
            if (0x2900..0x2920).contains(&i) && word >> 4 & 31 == 25 {
                self.cursor_report(128, i);
            }
            if i < 0x2000
                || (0x2500..0x2600).contains(&i)
                || (0x2900..0x2928).contains(&i)
                || [0x2780, 0x2740, 0x2802, 0x2803].contains(&i)
            {
                self.backend[i as usize] = if i < 0x2000 { word & 0x3fffffff } else { word };
            } else {
                self.report(0x220000 + i * 8, 4, true);
            }
            self.backend_index = (i + 1) & 0x3fff;
            self.backend_count -= 1;
        }
    }
}

/// One XIO widget. The fabric callback is invoked after releasing register
/// state; it may inspect this device. No C runtime or QEMU framework is used.
pub struct Odyssey {
    config: Config,
    state: Arc<Mutex<State>>,
    packet: Arc<OnceLock<Box<dyn Fn(Packet) + Send + Sync>>>,
    _memory: Option<Arc<dyn BusDevice>>,
    credit: Arc<OnceLock<Box<dyn Fn(u8, u32) + Send + Sync>>>,
    irq_order: Arc<Mutex<()>>,
    engine: Engine,
    display: Arc<display::Display>,
    clock: AtomicU64,
}
impl Odyssey {
    pub fn new(config: Config, memory: Option<Arc<dyn BusDevice>>) -> Self {
        assert!(config.widget < 16 && config.nic_serial < (1u64 << 48));
        let state = Arc::new(Mutex::new(State::new(config)));
        let packet: Arc<OnceLock<Box<dyn Fn(Packet) + Send + Sync>>> = Arc::new(OnceLock::new());
        let irq_order = Arc::new(Mutex::new(()));
        let credit: Arc<OnceLock<Box<dyn Fn(u8, u32) + Send + Sync>>> = Arc::new(OnceLock::new());
        let drain_credit = credit.clone();
        let (st, pk, order) = (state.clone(), packet.clone(), irq_order.clone());
        let engine = Engine::new(Box::new(move |submission| {
            let _order = order.lock();
            let mut state = st.lock();
            if submission.port == 0x110000 {
                state.selftest(submission);
            }
            state.flags |= LOW_WATER;
            let p = state.irq();
            drop(state);
            if let (Some(cb), Some(p)) = (pk.get(), p) {
                cb(p);
            }
            if submission.counted {
                if let Some(cb) = drain_credit.get() {
                    cb(config.widget, 1);
                } else {
                    eprintln!("odyssey: counted store has no credit receiver");
                }
            }
        }));
        let display = Arc::new(display::Display::new());
        let (frame_display, finish_display, frame_state) =
            (display.clone(), display.clone(), state.clone());
        engine.presentation(
            Box::new(move |r, pending| {
                let (backend, displayed) = {
                    let st = frame_state.lock();
                    (st.backend.clone(), st.regs[&0x10d4])
                };
                frame_display.frame(r, &backend, displayed, pending);
            }),
            Box::new(move || finish_display.stop()),
        );
        Self {
            config,
            state,
            packet,
            irq_order,
            engine,
            display,
            clock: AtomicU64::new(0),
            _memory: memory,
            credit,
        }
    }

    /// Return one processor-interface credit for each accepted counted
    /// store, after execution. A doubleword is one store, not two words.
    pub fn on_credit(&self, f: Box<dyn Fn(u8, u32) + Send + Sync>) {
        assert!(self.credit.set(f).is_ok());
    }
    /// Forward a store already counted by the processor interface. BUS_BUSY
    /// consumes nothing and returns no credit; the caller retains the mark
    /// across its retry. Ordinary BusDevice writes are never counted here.
    pub fn write_counted(&self, address: u64, width: u8, value: u64) -> u32 {
        if ![1, 2, 4, 8].contains(&width) {
            return crate::traits::BUS_ERR;
        }
        self.write_impl(address, width, value, true)
    }
    pub fn on_packet(&self, f: Box<dyn Fn(Packet) + Send + Sync>) {
        assert!(
            self.packet.set(f).is_ok(),
            "Odyssey packet callback already installed"
        );
    }
    pub fn unsupported(&self) -> Vec<Unsupported> {
        let state = self.state.lock();
        state
            .unsupported
            .iter()
            .copied()
            .chain(state.cursor_reports.values().copied())
            .collect()
    }
    fn send(&self, p: Option<Packet>) {
        if let (Some(cb), Some(p)) = (self.packet.get(), p) {
            cb(p);
        }
    }
    fn read(&self, address: u64, width: u8) -> u64 {
        let a = (address & 0xffffff) as u32;
        let mut s = self.state.lock();
        let reg = decode(a, width);
        match reg {
            Some(0) => 0xac013047,
            Some(0x08 | 0x10 | 0x18) => 0,
            Some(0x1014) => s.flags as u64,
            Some(0x1034) => s.enabled as u64,
            Some(0x1064) => 0x10030000,
            Some(0x106c) => 0xa1000000,
            Some(0x201594) => {
                if s.selftest_ready {
                    0x003d0046
                } else {
                    0
                }
            }
            Some(0x23380c) => 1,
            Some(0x233c00) => s.backend[0x2780] as u64,
            Some(0x233a00) => s.backend[0x2740] as u64,
            Some(0x233c08) => ((1 << 22) | if s.backend[0x2780] != 0 { 1 << 9 } else { 0 }) as u64,
            Some(0x233a08) => ((1 << 22) | if s.backend[0x2740] != 0 { 1 << 9 } else { 0 }) as u64,
            Some(0x10dc) => s.regs[&0x10d8] as u64,
            Some(r) if s.regs.contains_key(&r) && !write_only(r) => s.regs[&r] as u64,
            _ if width == 4 && a >= 0x232800 && a < 0x233000 && a & 7 == 0 => {
                s.backend[((a - 0x220000) / 8) as usize] as u64
            }
            _ => {
                s.report(a, width, false);
                0
            }
        }
    }
    fn write(&self, address: u64, width: u8, value: u64) -> u32 {
        self.write_impl(address, width, value, false)
    }
    fn write_impl(&self, address: u64, width: u8, value: u64, counted: bool) -> u32 {
        let a = (address & 0xffffff) as u32;
        if (a == 0x100000 || a == 0x110000) && (width == 4 || width == 8) {
            return self.engine.push(Submission {
                counted,
                port: a,
                width,
                value,
            });
        }
        let _order = self.irq_order.lock();
        let mut s = self.state.lock();
        let v = value as u32;
        if a == 0x100000 || a == 0x110000 {
            s.flags |= LOW_WATER;
        }
        if a == 0x400000 && (width == 4 || width == 8) {
            if width == 8 {
                s.backend_word((value >> 32) as u32);
            }
            s.backend_word(v);
        } else {
            match decode(a, width) {
                Some(0x1014) => s.flags = v,
                Some(0x101c) => s.flags &= !v,
                Some(0x1034) => s.enabled |= v,
                Some(0x103c) => s.enabled &= !v,
                Some(0x10b4) => *s.regs.get_mut(&0x10b4).unwrap() |= v,
                Some(0x10c4) => {
                    s.regs.insert(0x10c4, v);
                    *s.regs.get_mut(&0x10d4).unwrap() |= v;
                }
                Some(0x10cc) => {
                    s.regs.insert(0x10cc, v);
                    *s.regs.get_mut(&0x10d4).unwrap() &= !v;
                }
                Some(0x10d8 | 0x10dc) => {
                    let (pulse, sample) = w1::slot::fields(value);
                    let level = w1::slot::slot(&mut s.nic, pulse, sample);
                    s.regs.insert(0x10d8, (v & !3) | 2 | level as u32);
                }
                Some(0x40) => {
                    s.regs.insert(0x40, 0);
                }
                Some(r) if s.regs.contains_key(&r) => {
                    s.regs.insert(r, v);
                }
                _ => s.report(a, width, true),
            }
        }
        let p = s.irq();
        drop(s);
        self.send(p);
        if counted {
            if let Some(cb) = self.credit.get() {
                cb(self.config.widget, 1);
            } else {
                eprintln!("odyssey: counted store has no credit receiver");
            }
        }
        BUS_OK
    }
    /// Called at a modeled retrace: flip first, then notify the host.
    pub fn retrace(&self) {
        let _order = self.irq_order.lock();
        let mut s = self.state.lock();
        let arm = s.regs[&0x10b4];
        *s.regs.get_mut(&0x10d4).unwrap() ^= arm;
        s.regs.insert(0x10b4, 0);
        let p = s.packet(16, true);
        drop(s);
        self.send(p);
    }
}
fn write_only(a: u32) -> bool {
    [0x1044, 0x104c, 0x1054, 0x4034, 0x403c, 0x4004, 0x400c].contains(&a)
}
fn decode(a: u32, width: u8) -> Option<u32> {
    if a < 0x60 {
        return if (width == 8 && a & 7 == 0) || (width == 4 && a & 3 == 0) {
            Some(a & !7)
        } else {
            None
        };
    }
    if width != 4 {
        return None;
    }
    if [
        0x1004, 0x1014, 0x101c, 0x1034, 0x103c, 0x1044, 0x104c, 0x1054, 0x1064, 0x106c, 0x1070,
        0x107c, 0x10d8, 0x10dc, 0x1078, 0x1088, 0x1098, 0x10a8, 0x10b8, 0x10c0, 0x10b4, 0x10bc,
        0x10c4, 0x10cc, 0x10d4, 0x233c00, 0x233c08, 0x233a00, 0x233a08, 0x23380c, 0x4034, 0x403c,
        0x4004, 0x400c, 0x201594,
    ]
    .contains(&a)
    {
        Some(a)
    } else {
        None
    }
}
impl Resettable for Odyssey {
    fn power_on(&self) {
        let restart = self.engine.running();
        self.engine.reset();
        {
            let _order = self.irq_order.lock();
            *self.state.lock() = State::new(self.config);
        }
        self.clock.store(0, Ordering::Release);
        if restart {
            self.engine.start();
        }
    }
}
impl BusDevice for Odyssey {
    fn read8(&self, a: u64) -> BusRead8 {
        BusRead8::ok(self.read(a, 1) as u8)
    }
    fn read16(&self, a: u64) -> BusRead16 {
        BusRead16::ok(self.read(a, 2) as u16)
    }
    fn write8(&self, a: u64, v: u8) -> u32 {
        self.write(a, 1, v as u64)
    }
    fn write16(&self, a: u64, v: u16) -> u32 {
        self.write(a, 2, v as u64)
    }

    fn read32(&self, a: u64) -> BusRead32 {
        if self.engine.busy() && [0x1014, 0x1064, 0x106c].contains(&(a & 0xffffff)) {
            return BusRead32::busy();
        }
        if self.engine.busy() && (a & 0xffffff) == 0x201594 {
            return BusRead32::busy();
        }
        BusRead32::ok(self.read(a, 4) as u32)
    }
    fn read64(&self, a: u64) -> BusRead64 {
        BusRead64::ok(self.read(a, 8))
    }
    fn write32(&self, a: u64, v: u32) -> u32 {
        self.write(a, 4, v as u64)
    }
    fn write64(&self, a: u64, v: u64) -> u32 {
        self.write(a, 8, v)
    }
}

impl State {
    fn selftest(&mut self, s: Submission) {
        const PREAMBLE: [(u8, u64); 9] = [
            (4, 0x00010854),
            (4, 0),
            (4, 0x0fffffff),
            (8, 0x000006b000000000),
            (8, 0x000006b100000100),
            (8, 0x000006b300005e50),
            (8, 0x000006b200007dc0),
            (8, 0x000007b500000001),
            (4, 0x00010046),
        ];
        if PREAMBLE[self.selftest_progress] == (s.width, s.value) {
            self.selftest_progress += 1;
            if self.selftest_progress == PREAMBLE.len() {
                self.selftest_ready = true;
                self.selftest_progress = 0;
            }
        } else {
            self.selftest_progress = usize::from(PREAMBLE[0] == (s.width, s.value));
        }
    }
}
impl Odyssey {
    /// Host readback of authoritative model storage; busy while submitted
    /// commands remain outstanding. The displayed frame never replaces it.
    pub fn read_pixel(&self, buffer: usize, x: usize, y: usize) -> Option<u32> {
        if buffer > 1 || x >= render::WIDTH || y >= render::HEIGHT {
            return None;
        }
        self.engine
            .with_idle(|r| r.colour[buffer][y * render::WIDTH + x])
    }
    pub fn unsupported_commands(&self) -> Option<Vec<u32>> {
        self.engine
            .with_idle(|r| r.unsupported.iter().copied().collect())
    }
}
impl Odyssey {
    pub fn set_cpu_cycles(&self, cycles: crate::cpu::mips_core::CyclesPtr) {
        *self.display.cycles.lock() = cycles;
    }
}
impl crate::gfx_display::GfxDisplay for Odyssey {
    fn renderer_slot(&self) -> &Mutex<Option<Box<dyn crate::dev::ng1::rex3::Renderer>>> {
        &self.display.renderer
    }
    fn screen(&self) -> &Mutex<crate::disp::Rex3Screen> {
        &self.display.screen
    }
    fn request_screenshot(&self) {
        self.display.screenshot.store(true, Ordering::Release);
    }
    fn screenshot_pending(&self) -> bool {
        self.display.screenshot.load(Ordering::Acquire)
    }
    fn cycles(&self) -> crate::cpu::mips_core::CyclesPtr {
        *self.display.cycles.lock()
    }
}
impl Device for Odyssey {
    fn start(&self) {
        self.engine.start();
    }
    fn stop(&self) {
        self.engine.stop();
    }
    fn is_running(&self) -> bool {
        self.engine.running()
    }
    fn step(&self, cycles: u64) {
        self.clock.fetch_add(cycles, Ordering::Relaxed);
    }
    fn get_clock(&self) -> u64 {
        self.clock.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{BusDevice, Resettable, BUS_OK};
    #[test]
    fn v12_identity_memory_and_reset() {
        let b = Odyssey::new(Config::v12(13), None);
        assert_eq!(b.read32(4).data, 0xac013047);
        assert_eq!(b.read64(0).data, 0xac013047);
        assert_eq!(b.read32(0x24).data, 13);
        assert_eq!(b.read32(0x1014).data, 0x200);
        assert_eq!(b.read32(0x106c).data, 0xa1000000);
        assert_eq!(b.write32(0x107c, 0x12345678), BUS_OK);
        assert_eq!(b.read32(0x107c).data, 0x12345678);
        b.power_on();
        assert_eq!(b.read32(0x107c).data, 0);
    }

    #[test]
    fn flags_are_routed_outside_state_lock() {
        use std::sync::Arc;
        let b = Arc::new(Odyssey::new(Config::v12(13), None));
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let weak = Arc::downgrade(&b);
        let packets = seen.clone();
        b.on_packet(Box::new(move |p| {
            assert!(weak.upgrade().unwrap().read32(0x1014).is_ok());
            packets.lock().push(p);
        }));
        b.write32(0x1044, 8 << 25);
        b.write32(0x104c, 0x1234);
        b.write32(0x1054, 0x2a);
        b.write32(0x1034, 0x200);
        b.write32(0x1034, 0x200);
        b.write32(0x101c, 0x200);
        let seen = seen.lock();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0],
            Packet {
                widget: 8,
                address: 0x1234,
                vector: 0x2a,
                set: true
            }
        );
        assert!(!seen[1].set);
    }

    #[test]
    fn counted_doubleword_returns_one_credit_after_execution() {
        let b = Arc::new(Odyssey::new(Config::v12(13), None));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let out = seen.clone();
        let weak = Arc::downgrade(&b);
        b.on_credit(Box::new(move |widget, count| {
            assert_eq!(weak.upgrade().unwrap().read32(4).data, 0xac013047);
            out.lock().push((widget, count));
        }));
        b.write_counted(0x110000, 8, (0xc580c406u64 << 32) | 5);
        assert!(seen.lock().is_empty());
        b.start();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while b.engine.busy() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        b.stop();
        assert_eq!(*seen.lock(), [(13, 1)]);
        b.write32(0x107c, 1);
        assert_eq!(seen.lock().len(), 1);
        assert_eq!(b.write_counted(0x110000, 2, 0), BUS_OK);
        assert_eq!(*seen.lock(), [(13, 1), (13, 1)]);
    }
}
