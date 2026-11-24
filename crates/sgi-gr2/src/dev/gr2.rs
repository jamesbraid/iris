//! The files of `src/dev/gr2` the engine is made of, in place: the board's register window and its HQ2 and
//! RE3 threads, the HQ2 command engine, the GL-level geometry, the RE3 raster and its JIT, and the display
//! chips' arithmetic. Not here: iris's machine side (`mod.rs`: bus traits, display thread, monitor commands,
//! snapshot) and its tests of those.

#[path = "../../../../src/dev/gr2/board.rs"]
mod board;
pub use board::*;
#[path = "../../../../src/dev/gr2/variant.rs"]
pub(crate) mod variant;
pub use variant::Gr2Variant;
#[path = "../../../../src/dev/gr2/bt457.rs"]
pub mod bt457;
mod debug;
#[path = "../../../../src/dev/gr2/ge7.rs"]
pub mod ge7;
#[path = "../../../../src/dev/gr2/gr2comp.rs"]
pub mod gr2comp;
#[path = "../../../../src/dev/gr2/hq2.rs"]
pub mod hq2;
#[path = "../../../../src/dev/gr2/re3.rs"]
pub mod re3;
#[cfg(feature = "gr2-jit")]
#[path = "../../../../src/dev/gr2/re3_jit/mod.rs"]
pub mod re3_jit;
#[path = "../../../../src/dev/gr2/vc1.rs"]
pub mod vc1;
#[path = "../../../../src/dev/gr2/xmap5.rs"]
pub mod xmap5;

pub mod display;
pub mod engine;
mod state;

#[cfg(test)]
#[path = "../../../../src/dev/gr2/re3_tests.rs"]
mod re3_tests;
#[cfg(test)]
#[path = "../../../../src/dev/gr2/hq2_tests.rs"]
mod hq2_tests;
#[cfg(all(test, feature = "gr2-jit"))]
#[path = "../../../../src/dev/gr2/re3_jit_tests.rs"]
mod re3_jit_tests;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;

use crate::traits::{BusDevice, BusRead16, BusRead32, BusRead64, BusRead8, Device, BUS_BUSY, BUS_OK};

impl Gr2 {
    /// A board of one of iris's two kinds, for the mounted tests.
    pub fn new(variant: Gr2Variant, status: Arc<crate::status::MachineStatus>) -> Arc<Self> {
        Self::build(variant, variant.probe(), status, None).expect("the CPU scan-out")
    }
}

/// The slot's accesses by width: every width reads the word, a narrow store carries the register's value.
impl BusDevice for Gr2 {
    fn read32(&self, addr: u64) -> BusRead32 {
        self.reg_read(slot_off(addr) & (GR2_SLOT_SIZE - 4))
    }
    fn write32(&self, addr: u64, val: u32) -> u32 {
        self.reg_write(slot_off(addr) & (GR2_SLOT_SIZE - 4), val)
    }
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
        self.write64_inner(addr, val)
    }
}

impl Device for Gr2 {
    fn start(&self) {
        self.start_engine();
    }
    fn stop(&self) {
        self.stop_engine();
    }
    fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}
