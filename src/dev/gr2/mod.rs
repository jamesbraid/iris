//! GR2 graphics (Indy XZ / Indigo2 XZ / Indigo2 Extreme).
//!
//! Block layout (clanker/gr2/DESIGN.md):
//!   board.rs   the register window and the HQ2 and RE3 threads (shared with crates/sgi-gr2)
//!   hq2.rs     HQ2 registers + HLE command interpreter (HQ2 thread)
//!   re3.rs     RE3 registers, VRAM, drawing (RE3 thread)
//!   ge7.rs     GE7 windows + microcode storage (inert)
//!   vc1.rs     VC1 registers + SRAM (inert)
//!   xmap5.rs   XMAP5 mode tables + CLUTs (inert)
//!   bt457.rs   Bt457 RAMDACs (inert)
//!   gr2disp.rs display thread: retrace + composition + present
//!   gr2comp.rs software compositor
//!
//! Data path: CPU FIFO writes go to `hq_fifo`, and the HQ2 thread turns them
//! into RE3 register writes on `re3_fifo`. CPU writes to the RE3 register
//! window go straight to `re3_fifo`, and the RE3 thread draws into VRAM. The
//! display thread reads VRAM and the inert blocks with no locking (tearing is
//! tolerated, as with REX3).
//!
//! Hardware facts are those of the gr2 reverse-engineering survey; each one
//! kept, with its grounding, is in crates/sgi-gr2/docs/provenance.md.

mod board;
pub use board::*;
mod variant;
pub use variant::Gr2Variant;
pub mod bt457;
mod debug;
pub mod ge7;
pub mod gr2comp;
pub mod gr2disp;
pub mod hq2;
pub mod re3;
#[cfg(feature = "gr2-jit")]
pub mod re3_jit;
mod snapshot;
pub mod vc1;
pub mod xmap5;

pub use snapshot::Gr2SnapshotParts;

#[cfg(test)]
#[path = "re3_tests.rs"]
mod re3_tests;
#[cfg(test)]
#[path = "hq2_tests.rs"]
mod hq2_tests;
#[cfg(all(test, feature = "gr2-jit"))]
#[path = "re3_jit_tests.rs"]
mod re3_jit_tests;

use std::io::Write as IoWrite;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;

use crate::cpu::mips_core::CyclesPtr;
use crate::disp::Rex3Screen;
use crate::dev::ng1::rex3::Renderer;
use crate::snapshot::{get_field, hex_u32, load_u32_slice, load_u8_slice, toml_u32, u32_slice_to_toml, u8_slice_to_toml};
use crate::traits::{BusDevice, BusRead16, BusRead32, BusRead64, BusRead8, Device, Resettable, Saveable, BUS_BUSY, BUS_OK};

impl Gr2 {
    /// A board of one of iris's two kinds, built in place on the heap.
    pub fn new(variant: Gr2Variant, status: Arc<crate::status::MachineStatus>) -> Arc<Self> {
        Self::with_gpu(variant, status, None).expect("the CPU scan-out")
    }

    /// `new` with the scan-out on the host GPU when `gpu` is given.
    pub fn with_gpu(variant: Gr2Variant, status: Arc<crate::status::MachineStatus>, gpu: Option<crate::SharedGpu>) -> Result<Arc<Self>, String> {
        Self::build(variant, variant.probe(), status, gpu)
    }
}

impl crate::gfx_display::GfxDisplay for Gr2 {
    fn renderer_slot(&self) -> &Mutex<Option<Box<dyn Renderer>>> { self.head.renderer_slot() }
    fn display_stop(&self) -> &crate::gfx_display::DisplayStop { self.head.display_stop() }
    fn screen(&self) -> &Mutex<Rex3Screen> { self.head.screen() }
    fn request_screenshot(&self) { self.head.request_screenshot(); }
    fn screenshot_pending(&self) -> bool { self.head.screenshot_pending() }
    fn request_frame(&self) { self.head.request_frame(); }
    fn frame_pending(&self) -> bool { self.head.frame_pending() }
    fn set_capture(&self, on: bool) { self.head.set_capture(on); }
    fn cycles(&self) -> CyclesPtr { self.status.cycles() }
    fn hw_cursor(&self) -> Option<crate::gfx_display::HwCursor> {
        // A racy read of the registers, as the display thread's compose does.
        let vc1 = &self.regs().vc1;
        if !vc1.cursor_visible() {
            return None;
        }
        let (x, y) = vc1.cursor_pos();
        let mask = (0..32).map(|cy| (0..32).fold(0u64, |m, cx| {
            m | ((vc1.cursor_pixel(cx, cy) != 0) as u64) << cx
        })).collect();
        Some(crate::gfx_display::HwCursor { x, y, size: 32, mask })
    }
}

impl Resettable for Gr2 {
    fn power_on(&self) {
        Gr2::power_on(self)
    }
}

impl BusDevice for Gr2 {
    fn read32(&self, addr: u64) -> BusRead32 {
        let off = slot_off(addr) & (GR2_SLOT_SIZE - 4);
        if self.fin2_wait.load(Ordering::Relaxed) != 0 || off == HQREGS + hq2::HQ_VERSION {
            self.set_time(host_clock_ns());
        }
        let r = self.reg_read(off);
        crate::pf_level!(self.pf_wait, crate::perfetto::events::GR2_WAIT, r.status == BUS_BUSY);
        r
    }
    fn write32(&self, addr: u64, val: u32) -> u32 {
        let st = self.reg_write(slot_off(addr) & (GR2_SLOT_SIZE - 4), val);
        self.pf_write(st);
        st
    }
    // Narrow accesses hit 8/16-bit peripheral chips on word addresses: the
    // access carries the register value whatever its width or lane.
    fn read8(&self, addr: u64) -> BusRead8 {
        let r = self.read32(addr & !3);
        BusRead8 { status: r.status, data: r.data as u8 }
    }
    fn write8(&self, addr: u64, val: u8) -> u32 {
        self.reg_write_narrow(slot_off(addr) & (GR2_SLOT_SIZE - 4), val as u32)
    }
    fn read16(&self, addr: u64) -> BusRead16 {
        let r = self.read32(addr & !3);
        BusRead16 { status: r.status, data: r.data as u16 }
    }
    fn write16(&self, addr: u64, val: u16) -> u32 {
        self.reg_write_narrow(slot_off(addr) & (GR2_SLOT_SIZE - 4), val as u32)
    }
    fn read64(&self, addr: u64) -> BusRead64 {
        let hi = self.read32(addr);
        if hi.status != BUS_OK {
            return BusRead64 { status: hi.status, data: 0 };
        }
        let lo = self.read32(addr + 4);
        if lo.status != BUS_OK {
            return BusRead64 { status: lo.status, data: 0 };
        }
        BusRead64::ok(((hi.data as u64) << 32) | lo.data as u64)
    }
    fn write64(&self, addr: u64, val: u64) -> u32 {
        let st = self.write64_inner(addr, val);
        self.pf_write(st);
        st
    }
}

impl Gr2 {
    /// Perfetto: a store refused with BUS_BUSY is the CPU stalled on a
    /// full FIFO; the next accepted one ends the stall.
    #[inline]
    fn pf_write(&self, st: u32) {
        crate::pf_level!(self.pf_fifo_full, crate::perfetto::events::GR2_FIFO_FULL, st == BUS_BUSY);
    }
}

impl Device for Gr2 {
    fn step(&self, _cycles: u64) {}

    fn start(&self) {
        if !self.start_engine() {
            return;
        }
        self.head.start();
    }

    fn stop(&self) {
        // First, so the stop is seen to come from this thread (`DisplayStop`).
        self.head.stop();
        self.stop_engine();
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    fn get_clock(&self) -> u64 {
        0
    }

    fn register_commands(&self) -> Vec<(String, String)> {
        vec![
            ("gr2".into(), "GR2 graphics: status|hq|ge|shram|ucode|vc1|xmap|clut|dac|pix|trace (gr2 help)".into()),
            ("re3".into(), "GR2 RE3 raster engine: regs|pix".into()),
        ]
    }

    fn execute_command(&self, cmd: &str, args: &[&str], mut w: Box<dyn IoWrite + Send>) -> Result<(), String> {
        match cmd {
            "gr2" => self.cmd_gr2(args, &mut *w),
            "re3" => self.cmd_re3(args, &mut *w),
            _ => return Err(format!("unknown gr2 command: {cmd}")),
        }
        .map_err(|e| e.to_string())
    }
}

impl Saveable for Gr2 {
    // Registers, microcode, SRAMs and colour tables. VRAM is not saved yet.
    fn save_state(&self) -> toml::Value {
        let r = self.regs();
        let mut t = toml::map::Map::new();
        t.insert("shram".into(), u32_slice_to_toml(&r.shram));
        t.insert("hq_ucode".into(), u32_slice_to_toml(&r.hq.ucode));
        t.insert("hq_attrjmp".into(), u32_slice_to_toml(&r.hq.attrjmp));
        t.insert("hq_gepc".into(), hex_u32(r.hq.gepc));
        t.insert("hq_numge".into(), hex_u32(r.hq.numge));
        t.insert("hq_running".into(), hex_u32(r.hq.running));
        t.insert("vc1_regs".into(), u8_slice_to_toml(&r.vc1.regs));
        t.insert("vc1_sram".into(), u8_slice_to_toml(&r.vc1.sram));
        t.insert("vc1_sysctl".into(), hex_u32(r.vc1.sysctl as u32));
        for (i, x) in r.xmap.iter().enumerate() {
            t.insert(format!("xmap{i}_mode"), u32_slice_to_toml(&x.mode));
            t.insert(format!("xmap{i}_clut"), u32_slice_to_toml(&x.clut));
            t.insert(format!("xmap{i}_misc"), u8_slice_to_toml(&x.misc));
        }
        for (i, d) in r.dac.iter().enumerate() {
            t.insert(format!("dac{i}_palette"), u8_slice_to_toml(&d.palette));
            t.insert(format!("dac{i}_ctrl"), u8_slice_to_toml(&[d.readmask, d.blinkmask, d.cmd, d.test]));
        }
        toml::Value::Table(t)
    }

    fn load_state(&self, v: &toml::Value) -> Result<(), String> {
        let r = self.regs();
        if let Some(x) = get_field(v, "shram") { load_u32_slice(x, &mut r.shram); }
        if let Some(x) = get_field(v, "hq_ucode") { load_u32_slice(x, &mut r.hq.ucode); }
        if let Some(x) = get_field(v, "hq_attrjmp") { load_u32_slice(x, &mut r.hq.attrjmp); }
        if let Some(x) = get_field(v, "hq_gepc") { r.hq.gepc = toml_u32(x).unwrap_or(0); }
        if let Some(x) = get_field(v, "hq_numge") { r.hq.numge = toml_u32(x).unwrap_or(0); }
        if let Some(x) = get_field(v, "hq_running") { r.hq.running = toml_u32(x).unwrap_or(0); }
        if let Some(x) = get_field(v, "vc1_regs") { load_u8_slice(x, &mut r.vc1.regs); }
        if let Some(x) = get_field(v, "vc1_sram") { load_u8_slice(x, &mut r.vc1.sram); }
        if let Some(x) = get_field(v, "vc1_sysctl") { r.vc1.sysctl = toml_u32(x).unwrap_or(0) as u8; }
        for (i, xm) in r.xmap.iter_mut().enumerate() {
            if let Some(x) = get_field(v, &format!("xmap{i}_mode")) { load_u32_slice(x, &mut xm.mode); }
            if let Some(x) = get_field(v, &format!("xmap{i}_clut")) { load_u32_slice(x, &mut xm.clut); }
            if let Some(x) = get_field(v, &format!("xmap{i}_misc")) { load_u8_slice(x, &mut xm.misc); }
        }
        for (i, d) in r.dac.iter_mut().enumerate() {
            if let Some(x) = get_field(v, &format!("dac{i}_palette")) { load_u8_slice(x, &mut d.palette); }
            if let Some(x) = get_field(v, &format!("dac{i}_ctrl")) {
                let mut c = [0u8; 4];
                load_u8_slice(x, &mut c);
                (d.readmask, d.blinkmask, d.cmd, d.test) = (c[0], c[1], c[2], c[3]);
            }
        }
        self.head.damage();
        Ok(())
    }
}

/// Monotonic host nanoseconds, the clock iris's own board gives `set_time`.
fn host_clock_ns() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
}
