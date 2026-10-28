// SPDX-License-Identifier: BSD-3-Clause
//! Bedrock internal crossbar register storage and software error injection.
//!
//! Arbitration timing and debug outputs are unsupported. Unpublished queue
//! layouts remain observable and cannot fabricate error fields.
//!
//! Ported from hw/misc/sgi-bedrock-xb.c in jamesbraid/qemu-sgi-origami.
/// Length of the block-relative register aperture.
pub const SIZE: u64 = 0x198;

use crate::traits::{BusDevice, BusRead64, Resettable, BUS_ERR, BUS_OK};
use parking_lot::{Mutex, ReentrantMutex};
use std::sync::Arc;
type Level = Arc<dyn Fn(bool) + Send + Sync>;
const MASKS: [u64; 14] = [
    0x3333, 0x33, 0, 0, 0x3330, 0, 0, 0xf0, 0xfff0, 0, 0, 0, 0x3303, 0x11,
];
struct Regs {
    control: [u64; 4],
    queues: [u64; 14],
    first: u64,
    debug: [u64; 2],
    unserviced: u16,
}
impl Default for Regs {
    fn default() -> Self {
        Self {
            control: [0x0008000080000021, 0, 0, 0],
            queues: [0; 14],
            first: 0,
            debug: [0; 2],
            unserviced: 0,
        }
    }
}
pub struct BedrockXb {
    regs: Arc<Mutex<Regs>>,
    error: Mutex<Option<Level>>,
    delivery: ReentrantMutex<()>,
    driven: Mutex<Option<bool>>,
}
impl Default for BedrockXb {
    fn default() -> Self {
        Self::new()
    }
}
impl BedrockXb {
    pub fn new() -> Self {
        let regs = Arc::new(Mutex::new(Regs::default()));
        crate::locks::register_mutex("bedrock-xb", &regs);
        Self {
            regs,
            error: Mutex::new(None),
            delivery: ReentrantMutex::new(()),
            driven: Mutex::new(None),
        }
    }
    pub fn connect_error(&self, cb: Level) {
        let _delivery = self.delivery.lock();
        *self.error.lock() = Some(cb);
        *self.driven.lock() = None;
        self.update_irq();
    }
    pub fn error_level(&self) -> bool {
        self.regs.lock().first & 0x1000 != 0
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
}
impl Resettable for BedrockXb {
    fn power_on(&self) {
        *self.regs.lock() = Regs::default();
        self.update_irq();
    }
}
impl BusDevice for BedrockXb {
    fn read64(&self, a: u64) -> BusRead64 {
        if a & 7 != 0 {
            return BusRead64::err();
        }
        let r = self.regs.lock();
        let v = match a {
            0..=0x18 => r.control[a as usize / 8],
            0x20..=0x88 => {
                let n = (a as usize - 0x20) / 8;
                if MASKS[n] == 0 {
                    log::warn!("Bedrock XB unsupported queue {n}");
                }
                r.queues[n]
            }
            0x90 => r.first,
            0x98 | 0xa0 => r.debug[(a as usize - 0x98) / 8],
            _ => {
                log::warn!("Bedrock XB unsupported read {a:#x}");
                0
            }
        };
        BusRead64::ok(v)
    }
    fn write64(&self, a: u64, v: u64) -> u32 {
        if a & 7 != 0 {
            return BUS_ERR;
        }
        let mut r = self.regs.lock();
        match a {
            0..=0x18 => {
                let n = a as usize / 8;
                r.control[n] = v & [0x000ffffffffffff1, 0xffffffff, 0x7ff, 0xf][n];
                if a == 0x10 && r.control[n] != 0 {
                    log::warn!("Bedrock XB traffic observation unsupported");
                }
            }
            0x20..=0x88 => {
                let n = (a as usize - 0x20) / 8;
                let v = v & MASKS[n];
                if MASKS[n] == 0 {
                    log::warn!("Bedrock XB unsupported queue {n}");
                }
                if v != 0 {
                    r.queues[n] |= v;
                    r.unserviced |= 1 << n;
                    if r.first & 0x1000 == 0 {
                        r.first = 0x1000 | ((15 - n) as u64) << 8;
                    }
                }
            }
            0x120..=0x188 => {
                let n = (a as usize - 0x120) / 8;
                r.queues[n] = 0;
                r.unserviced &= !(1 << n);
            }
            0x190 => {
                if r.first & 0x1000 != 0 {
                    let n = 15 - ((r.first >> 8) & 15) as usize;
                    if r.unserviced & (1 << n) != 0 {
                        log::warn!("Bedrock XB first error cleared before queue");
                    } else {
                        r.first = 0;
                        for n in 0..14 {
                            if r.queues[n] != 0 {
                                r.first = 0x1000 | ((15 - n) as u64) << 8;
                                break;
                            }
                        }
                    }
                }
            }
            0x98 | 0xa0 => {
                let n = (a as usize - 0x98) / 8;
                r.debug[n] = v & [0xffff, 7][n];
                if r.debug[n] != 0 {
                    log::warn!("Bedrock XB debug output unsupported");
                }
            }
            _ => log::warn!("Bedrock XB unsupported or read-only write {a:#x} = {v:#x}"),
        }
        drop(r);
        self.update_irq();
        BUS_OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_order() {
        let d = BedrockXb::new();
        assert_eq!(d.read64(0).data, 0x0008000080000021);
        d.write64(0x20, 1);
        assert!(d.error_level());
        assert_eq!(d.read64(0x90).data, 0x1f00);
        d.write64(0x190, !0);
        assert_eq!(d.read64(0x90).data, 0x1f00);
        d.write64(0x188, !0);
        d.write64(0x120, 0);
        d.write64(0x190, 0);
        assert!(!d.error_level());
        d.write64(0x88, !0);
        assert_eq!(d.read64(0x88).data, 0x11);
        d.power_on();
        assert!(!d.error_level());
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    #[test]
    fn all_masks_and_unpublished_queues() {
        let d = BedrockXb::new();
        for (a, mask) in [
            (0, 0x000ffffffffffff1),
            (8, 0xffffffff),
            (0x10, 0x7ff),
            (0x18, 15),
            (0x98, 0xffff),
            (0xa0, 7),
        ] {
            d.write64(a, !0);
            assert_eq!(d.read64(a).data, mask);
        }
        for (n, mask) in MASKS.into_iter().enumerate() {
            d.write64(0x20 + n as u64 * 8, !0);
            assert_eq!(d.read64(0x20 + n as u64 * 8).data, mask);
        }
        d.power_on();
        assert_eq!(d.read64(0).data, 0x0008000080000021);
        assert!(!d.error_level());
        assert_eq!(d.read32(0).status, BUS_ERR);
    }
    #[test]
    fn next_error_and_reinjection() {
        let d = BedrockXb::new();
        d.write64(0x88, 0x10);
        d.write64(0x20, 1);
        assert_eq!(d.read64(0x90).data, 0x1200);
        d.write64(0x188, 0);
        d.write64(0x190, 0);
        assert_eq!(d.read64(0x90).data, 0x1f00);
        d.write64(0x120, 0);
        d.write64(0x20, 0x10);
        d.write64(0x190, 0);
        assert_eq!(d.read64(0x90).data, 0x1f00);
        d.write64(0x120, 0);
        d.write64(0x190, 0);
        assert!(!d.error_level());
    }
}
