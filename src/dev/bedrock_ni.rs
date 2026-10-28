// SPDX-License-Identifier: BSD-3-Clause
//! Bedrock's network registers for a node without a network peer.
//!
//! Unknown power-on fields use deterministic zero state, with sideband retained
//! across reset. Routing entries retain fields without simulating a fabric.
//!
//! Ported from hw/misc/sgi-bedrock-ni.c and hw/misc/sgi-bedrock-ni-regs.c in jamesbraid/qemu-sgi-origami.
/// Length of the block-relative register aperture.
pub const SIZE: u64 = 0x2008;

use crate::traits::{BusDevice, BusRead64, Resettable, BUS_ERR, BUS_OK};
use parking_lot::{Mutex, ReentrantMutex};
use std::sync::Arc;
type Level = Arc<dyn Fn(bool) + Send + Sync>;
#[derive(Clone)]
struct Regs {
    control: [u64; 8],
    routes: [u64; 128],
    global: u64,
    errors: u64,
    sideband: u64,
}
impl Default for Regs {
    fn default() -> Self {
        Self {
            control: [0; 8],
            routes: [0; 128],
            global: 0,
            errors: 0,
            sideband: 0,
        }
    }
}
pub struct BedrockNi {
    regs: Arc<Mutex<Regs>>,
    error: Mutex<Option<Level>>,
    delivery: ReentrantMutex<()>,
    driven: Mutex<Option<bool>>,
    reset: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}
impl Default for BedrockNi {
    fn default() -> Self {
        Self::new()
    }
}
impl BedrockNi {
    pub fn new() -> Self {
        let regs = Arc::new(Mutex::new(Regs::default()));
        crate::locks::register_mutex("bedrock-ni", &regs);
        Self {
            regs,
            error: Mutex::new(None),
            delivery: ReentrantMutex::new(()),
            driven: Mutex::new(None),
            reset: Mutex::new(None),
        }
    }
    pub fn connect_error(&self, cb: Level) {
        let _delivery = self.delivery.lock();
        *self.error.lock() = Some(cb);
        *self.driven.lock() = None;
        self.update_irq();
    }
    pub fn connect_local_reset(&self, cb: Arc<dyn Fn() + Send + Sync>) {
        *self.reset.lock() = Some(cb);
    }
    pub fn error_level(&self) -> bool {
        self.regs.lock().errors & 0x1fffffff000000 != 0
    }
    fn update_irq(&self) {
        // Serialize the sample and callback, while leaving registers available
        // to the callback. Suppress unchanged levels so callbacks can reenter
        // register reads that recompute the output.
        let _delivery = self.delivery.lock();
        let cb = self.error.lock().clone();
        if let Some(cb) = cb {
            let level = self.error_level();
            let changed = {
                let mut driven = self.driven.lock();
                let changed = *driven != Some(level);
                *driven = Some(level);
                changed
            };
            if changed {
                cb(level);
            }
        }
    }
    fn mask(a: u64) -> u64 {
        match a {
            0x10 => 1,
            0x18 => 7,
            0x20 => 0x1ff,
            0x30 => 0x7efffffff,
            0x38 => 0xffff,
            _ => 0,
        }
    }
}
impl Resettable for BedrockNi {
    fn power_on(&self) {
        let mut r = self.regs.lock();
        let sideband = r.sideband;
        *r = Regs::default();
        r.sideband = sideband;
        drop(r);
        self.update_irq();
    }
}
impl BusDevice for BedrockNi {
    fn read64(&self, a: u64) -> BusRead64 {
        if a & 7 != 0 {
            return BusRead64::err();
        }
        let mut r = self.regs.lock();
        let v = if Self::mask(a) != 0 {
            r.control[a as usize / 8]
        } else if (0x1000..0x1400).contains(&a) {
            r.routes[(a as usize - 0x1000) / 8]
        } else {
            match a {
                0 => 0,
                0x100 => r.errors,
                0x108 | 0x110 => 0,
                0x118 => r.sideband,
                0x120 => {
                    let v = r.errors;
                    r.errors = 0;
                    v
                }
                0x2000 => r.global,
                _ => {
                    log::warn!("Bedrock NI unsupported read {a:#x}");
                    0
                }
            }
        };
        drop(r);
        self.update_irq();
        BusRead64::ok(v)
    }
    fn write64(&self, a: u64, v: u64) -> u32 {
        if a & 7 != 0 {
            return BUS_ERR;
        }
        let mut r = self.regs.lock();
        if Self::mask(a) != 0 {
            r.control[a as usize / 8] = v & Self::mask(a);
        } else if (0x1000..0x1400).contains(&a) {
            r.routes[(a as usize - 0x1000) / 8] = v & 31;
        } else {
            match a {
                0x08 => {}
                0x100 => r.errors = v & ((1 << 53) - 1),
                0x118 => r.sideband = v & 0xfffff,
                0x2000 => r.global = v & 31,
                _ => log::warn!("Bedrock NI unsupported or read-only write {a:#x} = {v:#x}"),
            }
        }
        drop(r);
        self.update_irq();
        if a == 8 && v & 4 != 0 {
            let cb = self.reset.lock().clone();
            if let Some(cb) = cb {
                cb();
            }
        }
        BUS_OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn masks_errors_reset() {
        let d = BedrockNi::new();
        assert_eq!(d.read64(0).data, 0);
        d.write64(0x30, !0);
        assert_eq!(d.read64(0x30).data, 0x7efffffff);
        d.write64(0x100, !0);
        assert!(d.error_level());
        assert_eq!(d.read64(0x120).data, (1 << 53) - 1);
        assert!(!d.error_level());
        d.write64(0x118, !0);
        d.power_on();
        assert_eq!(d.read64(0x118).data, 0xfffff);
        assert_eq!(d.read64(0x30).data, 0);
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    #[test]
    fn masks_routes_reset_and_callbacks() {
        let d = Arc::new(BedrockNi::new());
        for (a, mask) in [
            (0x10, 1),
            (0x18, 7),
            (0x20, 0x1ff),
            (0x30, 0x7efffffff),
            (0x38, 0xffff),
        ] {
            d.write64(a, !0);
            assert_eq!(d.read64(a).data, mask);
        }
        for a in (0x1000..0x1400).step_by(8) {
            d.write64(a, !0);
            assert_eq!(d.read64(a).data, 31);
        }
        d.write64(0x2000, !0);
        assert_eq!(d.read64(0x2000).data, 31);
        let weak = Arc::downgrade(&d);
        d.connect_error(Arc::new(move |_| {
            let d = weak.upgrade().unwrap();
            let _ = d.read64(0x118);
        }));
        let resets = Arc::new(AtomicU64::new(0));
        let count = resets.clone();
        d.connect_local_reset(Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        d.write64(0x10, 0);
        d.write64(8, 4);
        assert_eq!(resets.load(Ordering::SeqCst), 1);
        d.write64(0x100, 0xffffff);
        assert!(!d.error_level());
        d.write64(0x100, 1 << 24);
        assert!(d.error_level());
        d.read64(0x120);
        assert!(!d.error_level());
        assert_eq!(d.read32(0).status, BUS_ERR);
    }
}
