//! Bedrock's local block: local registers, atomic scratch aliases and the
//! MicroLAN master.
//!
//! The MicroLAN wire has no identity device attached. A slot completes after
//! one microsecond on the machine clock. Permission registers retain their
//! values without enforcing requester access, as in the QEMU model.
//!
//! Ported from hw/misc/sgi-bedrock-lb.c in jamesbraid/qemu-sgi-origami.

/// Length of the block-relative register aperture.
pub const SIZE: u64 = 0xc040;

use crate::traits::{BusDevice, BusRead64, Resettable, BUS_ERR, BUS_OK};
use parking_lot::{Mutex, ReentrantMutex};
use std::collections::BTreeMap;
use std::sync::Arc;
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;
type Level = Arc<dyn Fn(bool) + Send + Sync>;
const ID: u64 = (0x36 << 1) | (0x3012 << 12) | (2 << 28);
const VECTOR_MASK: u64 = 3 | (0x1fffff << 3) | (0xff << 32) | (0x7ff << 40);
struct Regs {
    words: BTreeMap<u64, u64>,
    scratch: [u64; 5],
    node: u8,
    region: u8,
    deadline: Option<u64>,
    sample: bool,
}
impl Default for Regs {
    fn default() -> Self {
        let mut words = BTreeMap::new();
        words.insert(0x4000, !0);
        words.insert(0x4010, !0);
        Self {
            words,
            scratch: [0; 5],
            node: 0,
            region: 0,
            deadline: None,
            sample: true,
        }
    }
}
pub struct BedrockLb {
    regs: Arc<Mutex<Regs>>,
    clock: Clock,
    error: Mutex<Option<Level>>,
    delivery: ReentrantMutex<()>,
    driven: Mutex<Option<bool>>,
    reset: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    node_changed: Mutex<Option<Arc<dyn Fn(u8) + Send + Sync>>>,
}
impl BedrockLb {
    pub fn new(clock: Clock) -> Self {
        let regs = Arc::new(Mutex::new(Regs::default()));
        crate::locks::register_mutex("bedrock-lb", &regs);
        Self {
            regs,
            clock,
            error: Mutex::new(None),
            delivery: ReentrantMutex::new(()),
            driven: Mutex::new(None),
            reset: Mutex::new(None),
            node_changed: Mutex::new(None),
        }
    }
    pub fn connect_error(&self, cb: Level) {
        let _delivery = self.delivery.lock();
        *self.error.lock() = Some(cb);
        *self.driven.lock() = None;
        self.update_irq();
    }
    pub fn connect_soft_reset(&self, cb: Arc<dyn Fn() + Send + Sync>) {
        *self.reset.lock() = Some(cb);
    }
    pub fn connect_node_changed(&self, cb: Arc<dyn Fn(u8) + Send + Sync>) {
        let _delivery = self.delivery.lock();
        *self.node_changed.lock() = Some(cb);
    }
    pub fn node_number(&self) -> u8 {
        self.regs.lock().node
    }
    pub fn error_level(&self) -> bool {
        self.regs.lock().words.get(&0x4040).copied().unwrap_or(0) != 0
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
    fn notify_node(&self) {
        let _delivery = self.delivery.lock();
        let cb = self.node_changed.lock().clone();
        if let Some(cb) = cb {
            cb(self.node_number());
        }
    }
    fn complete(r: &mut Regs, now: u64) {
        if r.deadline.is_some_and(|d| now >= d) {
            *r.words.entry(0x4030).or_default() |= 2 | u64::from(r.sample);
            r.deadline = None;
        }
    }
    fn mask(a: u64) -> Option<u64> {
        Some(match a {
            0x4000 | 0x4010 | 0x4020 | 0x4060 | 0xc008 | 0xc010 | 0xc028 | 0xc030 => !0,
            0x4028 => 0x80ff80ff80ff80ff,
            0x4040 => 0x7ff,
            0x4050 => 0x7f | (0x7ff << 8) | (0x7ff << 20) | (0xf << 32) | (1 << 40) | (1 << 44),
            0x4058 => (1 << 38) - 1,
            0x4100 => 0x3f,
            0x4200 => 1 | (0x3ff << 4) | (0x3ff << 16) | (1 << 28) | (1 << 32),
            0x4208 => {
                0x1f | (0xfff << 8) | (1 << 20) | (0x3ff << 24) | (0x3ff << 36) | (0x3ff << 48)
            }
            0xc000 => VECTOR_MASK,
            _ => return None,
        })
    }
}
impl Resettable for BedrockLb {
    fn power_on(&self) {
        let mut r = self.regs.lock();
        let survivors = [
            r.scratch[1],
            r.scratch[2],
            r.words.get(&0x4040).copied().unwrap_or(0),
        ];
        *r = Regs::default();
        r.scratch[1] = survivors[0];
        r.scratch[2] = survivors[1];
        r.words.insert(0x4040, survivors[2]);
        drop(r);
        self.update_irq();
        self.notify_node();
    }
}
impl BusDevice for BedrockLb {
    fn read64(&self, a: u64) -> BusRead64 {
        if a & 7 != 0 {
            return BusRead64::err();
        }
        let now = (self.clock)();
        let mut r = self.regs.lock();
        Self::complete(&mut r, now);
        let v = match a {
            0 => ID | ((r.node as u64) << 32) | ((r.region as u64) << 46),
            0x8000..=0x8020 => r.scratch[(a as usize - 0x8000) / 8],
            0x8058 | 0x8060 => {
                let n = 3 + (a as usize - 0x8058) / 8;
                let v = r.scratch[n];
                r.scratch[n] = 1;
                v
            }
            0xc038 => r.words.remove(&0xc020).unwrap_or(0),
            0xc020 | 0x4030 => r.words.get(&a).copied().unwrap_or(0),
            _ if Self::mask(a).is_some() => r.words.get(&a).copied().unwrap_or(0),
            _ => {
                log::warn!("Bedrock LB unsupported read {a:#x}");
                0
            }
        };
        BusRead64::ok(v)
    }
    fn write64(&self, a: u64, v: u64) -> u32 {
        if a & 7 != 0 {
            return BUS_ERR;
        }
        let now = (self.clock)();
        let mut r = self.regs.lock();
        Self::complete(&mut r, now);
        match a {
            0 => {
                let region = ((v >> 46) & 3) as u8;
                if region == 3 {
                    log::warn!("Bedrock LB invalid region size");
                } else {
                    r.node = (v >> 32) as u8;
                    r.region = region;
                }
            }
            0x8000..=0x8020 => {
                let n = (a as usize - 0x8000) / 8;
                r.scratch[n] = if n < 3 { v } else { v & 1 };
            }
            0x8040..=0x8050 => {
                let n = (a as usize - 0x8040) / 8;
                if r.scratch[n] == 0 {
                    r.scratch[n] = v;
                }
            }
            0x4008 => {
                if v == 0x53474972756c6573 {
                    *r.words.entry(0x4000).or_default() |= 1;
                }
            }
            0x4018 => {}
            0x4030 => {
                if r.deadline.is_some() {
                    log::warn!("Bedrock LB MicroLAN write while slot pending discarded");
                } else {
                    r.words.insert(a, v & ((1 << 34) - 4));
                    let pulse = (v >> 10) & 0x3ff;
                    r.sample = pulse >= 400 || pulse < 40;
                    r.deadline = Some(now.saturating_add(1000));
                }
            }
            0x4040 => {
                *r.words.entry(a).or_default() |= v & 0x7ff;
            }
            0x4048 => {
                *r.words.entry(0x4040).or_default() &= !(v & 0x7ff);
            }
            0xc000 => {
                r.words.insert(a, v & VECTOR_MASK);
                if r.words.get(&0xc020).copied().unwrap_or(0) & (1 << 63) != 0 {
                    *r.words.entry(0xc020).or_default() |= 1 << 62;
                } else {
                    log::warn!("Bedrock LB vector launch reaches no peer; no reply latched");
                }
            }
            0xc020 | 0xc038 => {
                if v == 0 {
                    r.words.insert(0xc020, 0);
                } else {
                    log::warn!("Bedrock LB nonzero vector status write ignored");
                }
            }
            _ => {
                if let Some(mask) = Self::mask(a) {
                    r.words.insert(a, v & mask);
                    if (a == 0x4000 || a == 0x4010) && v != !0 {
                        log::warn!("Bedrock LB permission enforcement requires requester context");
                    }
                } else {
                    log::warn!("Bedrock LB unsupported write {a:#x} = {v:#x}");
                }
            }
        }
        drop(r);
        self.update_irq();
        if a == 0 {
            self.notify_node();
        }
        if a == 0x4018 && v & 1 != 0 {
            self.power_on();
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
    fn identity_scratch_and_survivors() {
        let d = BedrockLb::new(Arc::new(|| 0));
        assert_eq!(d.read64(0).data, (0x36 << 1) | (0x3012 << 12) | (2 << 28));
        d.write64(0x8008, 42);
        d.write64(0x8010, 43);
        d.write64(0x8018, !0);
        assert_eq!(d.read64(0x8058).data, 1);
        d.write64(0x8018, 0);
        assert_eq!(d.read64(0x8058).data, 0);
        assert_eq!(d.read64(0x8058).data, 1);
        d.power_on();
        assert_eq!(d.read64(0x8008).data, 42);
        assert_eq!(d.read64(0x8010).data, 43);
        assert_eq!(d.read64(0x8018).data, 0);
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    #[test]
    fn microlan_timing_pending_and_reset() {
        let time = Arc::new(AtomicU64::new(0));
        let t = time.clone();
        let d = BedrockLb::new(Arc::new(move || t.load(Ordering::SeqCst)));
        let word = (0x66 << 27) | (60 << 10) | (0x41 << 2) | 3;
        d.write64(0x4030, word);
        assert_eq!(d.read64(0x4030).data, word & !3);
        d.write64(0x4030, 8 << 10);
        assert_eq!(d.read64(0x4030).data, word & !3);
        time.store(1000, Ordering::SeqCst);
        assert_eq!(d.read64(0x4030).data, (word & !3) | 2);
        d.write64(0x4030, 8 << 10);
        time.store(2000, Ordering::SeqCst);
        assert_eq!(d.read64(0x4030).data, (8 << 10) | 3);
        d.write64(0x4030, 60 << 10);
        d.power_on();
        time.store(3000, Ordering::SeqCst);
        assert_eq!(d.read64(0x4030).data, 0);
    }
    #[test]
    fn revision_permissions_errors() {
        let d = BedrockLb::new(Arc::new(|| 0));
        d.write64(0, (0x5a << 32) | (2 << 46));
        assert_eq!(d.node_number(), 0x5a);
        d.write64(0, 3 << 46);
        assert_eq!(d.node_number(), 0x5a);
        d.write64(0x4000, 0);
        d.write64(0x4008, 0);
        assert_eq!(d.read64(0x4000).data, 0);
        d.write64(0x4008, 0x53474972756c6573);
        assert_eq!(d.read64(0x4000).data, 1);
        d.write64(0x4040, !0);
        assert_eq!(d.read64(0x4040).data, 0x7ff);
        d.power_on();
        assert!(d.error_level());
        d.write64(0x4048, 0x7ff);
        assert!(!d.error_level());
        assert_eq!(d.node_number(), 0);
    }
    #[test]
    fn write_zero_and_vector_no_reply() {
        let d = BedrockLb::new(Arc::new(|| 0));
        d.write64(0x8040, 42);
        d.write64(0x8040, 43);
        assert_eq!(d.read64(0x8000).data, 42);
        d.write64(0xc030, 0xdead);
        d.write64(0xc000, !0);
        assert_eq!(d.read64(0xc020).data, 0);
        assert_eq!(d.read64(0xc030).data, 0xdead);
        assert_eq!(d.read64(0xc000).data, VECTOR_MASK);
        assert_eq!(d.read32(0x8000).status, BUS_ERR);
        assert_eq!(d.read64(1).status, BUS_ERR);
    }
}

#[cfg(test)]
mod notification_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    /// Hold an IRQ packet's assertion callback while a CPU clears the register.
    pub(super) fn check_order(
        device: Arc<dyn BusDevice>,
        install: impl FnOnce(Level),
        raise: u64,
        value: u64,
        clear: impl FnOnce() + Send + 'static,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let delivered = Arc::new(AtomicBool::new(false));
        let sink = delivered.clone();
        install(Arc::new(move |level| {
            if level {
                entered_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }
            sink.store(level, Ordering::SeqCst);
        }));
        let io = crate::dev::bedrock_io::BedrockIo::new();
        io.connect_hub(device);
        let first = std::thread::spawn(move || {
            io.inbound_write(14, Some(0), (1 << 47) | 0x1800000 | raise, 8, value)
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            clear();
            done_tx.send(()).unwrap();
        });
        // An ordered callback waits for the blocked assertion. A stale publisher
        // instead completes the clear early, then overwrites it after release.
        let _ = done_rx.recv_timeout(std::time::Duration::from_millis(100));
        release_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap(), BUS_OK);
        second.join().unwrap();
        assert!(!delivered.load(Ordering::SeqCst));
    }
    #[test]
    fn irq_notifications_follow_concurrent_ingress() {
        let ni = Arc::new(crate::dev::bedrock_ni::BedrockNi::new());
        let a = ni.clone();
        let b = ni.clone();
        check_order(
            ni,
            move |cb| a.connect_error(cb),
            0x100,
            1 << 24,
            move || {
                b.read64(0x120);
            },
        );
    }
}

#[cfg(test)]
mod further_notification_tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};
    #[test]
    fn node_notifications_follow_concurrent_ingress() {
        let d = Arc::new(BedrockLb::new(Arc::new(|| 0)));
        let delivered = Arc::new(AtomicU8::new(0));
        let sink = delivered.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        d.connect_node_changed(Arc::new(move |node| {
            if node == 1 {
                entered_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }
            sink.store(node, Ordering::SeqCst);
        }));
        let io = crate::dev::bedrock_io::BedrockIo::new();
        io.connect_hub(d.clone());
        let first = std::thread::spawn(move || {
            io.inbound_write(14, Some(0), (1 << 47) | 0x1800000, 8, 1 << 32)
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let other = d.clone();
        let second = std::thread::spawn(move || {
            other.write64(0, 2 << 32);
            done_tx.send(()).unwrap();
        });
        let _ = done_rx.recv_timeout(std::time::Duration::from_millis(100));
        release_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap(), BUS_OK);
        second.join().unwrap();
        assert_eq!(d.node_number(), 2);
        assert_eq!(delivered.load(Ordering::SeqCst), 2);
    }
}

#[cfg(test)]
mod more_irq_tests {
    use super::*;
    #[test]
    fn local_irq_order() {
        let d = Arc::new(BedrockLb::new(Arc::new(|| 0)));
        let a = d.clone();
        let b = d.clone();
        super::notification_tests::check_order(
            d,
            move |cb| a.connect_error(cb),
            0x4040,
            1,
            move || {
                b.write64(0x4048, 1);
            },
        );
    }
    #[test]
    fn crossbar_irq_order() {
        let d = Arc::new(crate::dev::bedrock_xb::BedrockXb::new());
        let a = d.clone();
        let b = d.clone();
        super::notification_tests::check_order(
            d,
            move |cb| a.connect_error(cb),
            0x20,
            1,
            move || {
                b.write64(0x120, 0);
                b.write64(0x190, 0);
            },
        );
    }
}
