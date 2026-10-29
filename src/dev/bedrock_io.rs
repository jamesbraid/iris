//! Bedrock's I/O interface: widget PIO windows, inbound access protection
//! and the block-transfer engines that move data through node memory.
//!
//! Block transfers finish synchronously, as in the QEMU model. Unpublished
//! fields retain that model's stored values and deterministic reset state.
//! Transfers and completion callbacks run outside the register lock.
//!
//! Ported from hw/misc/sgi-bedrock-io.c in jamesbraid/qemu-sgi-origami.

/// Length of the block-relative register aperture.
pub const SIZE: u64 = 0x30010;

use crate::traits::{BusDevice, BusRead64, Resettable, BUS_ERR, BUS_OK};
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::thread::ThreadId;
const ID: u64 = (2 << 28) | (0xc110 << 12) | (0x36 << 1);
const ADDR_MASK: u64 = 0xffffffff80;
const BUSY: u64 = 1 << 20;
const ERROR: u64 = 1 << 16;
#[derive(Clone, Copy, Default)]
struct Bte {
    status: u64,
    source: u64,
    destination: u64,
    control: u64,
    notification: u64,
    interrupt: u64,
}
struct Regs {
    words: BTreeMap<u64, u64>,
    bte: [Bte; 2],
    node: u8,
    peer: bool,
}
impl Default for Regs {
    fn default() -> Self {
        let mut words = BTreeMap::new();
        for (a, v) in [
            (0x20, 0xa),
            (0x100, !0),
            (0x110, !0),
            (0x118, !0),
            (0x120, !0),
            (0x128, 0x3fc0fff6403 & !(1 << 13)),
            (0x1e8, 0x111),
        ] {
            words.insert(a, v);
        }
        Self {
            words,
            bte: [Bte::default(); 2],
            node: 0,
            peer: false,
        }
    }
}
// Only a synchronous callback on the same thread is recursive. DMA workers
// on other threads may call the thread-safe downstream bus independently.
struct InboundAccess<'a> {
    active: &'a Mutex<HashSet<ThreadId>>,
    thread: ThreadId,
}
impl Drop for InboundAccess<'_> {
    fn drop(&mut self) {
        self.active.lock().remove(&self.thread);
    }
}

pub struct BedrockIo {
    inbound_threads: Mutex<HashSet<ThreadId>>,
    regs: Arc<Mutex<Regs>>,
    widgets: Mutex<[Option<Arc<dyn BusDevice>>; 16]>,
    memory: Mutex<Option<Arc<dyn BusDevice>>>,
    hub: Mutex<Option<Arc<dyn BusDevice>>>,
}
impl Default for BedrockIo {
    fn default() -> Self {
        Self::new()
    }
}
impl BedrockIo {
    pub fn new() -> Self {
        let regs = Arc::new(Mutex::new(Regs::default()));
        crate::locks::register_mutex("bedrock-io", &regs);
        Self {
            regs,
            inbound_threads: Mutex::new(HashSet::new()),
            widgets: Mutex::new(std::array::from_fn(|_| None)),
            memory: Mutex::new(None),
            hub: Mutex::new(None),
        }
    }
    pub fn connect_widget(&self, widget: u8, dev: Arc<dyn BusDevice>) {
        assert!(widget < 16);
        self.widgets.lock()[widget as usize] = Some(dev);
        self.set_xio_peer(true);
    }
    pub fn set_xio_peer(&self, peer: bool) {
        let mut r = self.regs.lock();
        r.peer = peer;
        if peer {
            *r.words.entry(0x128).or_default() |= 1 << 13;
        } else {
            *r.words.entry(0x128).or_default() &= !(1 << 13);
        }
    }
    pub fn connect_memory(&self, dev: Arc<dyn BusDevice>) {
        *self.memory.lock() = Some(dev);
    }
    pub fn connect_hub(&self, dev: Arc<dyn BusDevice>) {
        *self.hub.lock() = Some(dev);
    }
    pub fn set_node_number(&self, node: u8) {
        self.regs.lock().node = node;
    }
    pub fn gfx_map(&self, entry: usize) -> u64 {
        assert!(entry < 2);
        self.word(0x140 + entry as u64 * 8)
    }
    fn word(&self, a: u64) -> u64 {
        self.regs.lock().words.get(&a).copied().unwrap_or(0)
    }
    pub fn disable_device(&self, widget: u8, device: u8) {
        if (8..16).contains(&widget) && device < 8 {
            *self.regs.lock().words.entry(0x120).or_default() &=
                !(1 << ((widget - 8) * 8 + device));
        }
    }
    fn permitted(r: &Regs, widget: u8, device: u8) -> bool {
        widget < 16
            && device < 8
            && r.words.get(&0x118).copied().unwrap_or(0) & (1 << widget) != 0
            && (widget < 8
                || r.words.get(&0x120).copied().unwrap_or(0) & (1 << ((widget - 8) * 8 + device))
                    != 0)
    }
    fn retained(a: u64) -> bool {
        matches!(
            a,
            0x208
                | 0x210
                | 0x218
                | 0x220
                | 0x228
                | 0x300
                | 0x140
                | 0x148
                | 0x388
                | 0x390
                | 0x398
                | 0x3a0
                | 0x600
                | 0x608
                | 0x610
                | 0x30000
                | 0x30008
        ) || (0x308..0x348).contains(&a)
            || (0x400..=0x5d8).contains(&a)
    }
    fn mask(a: u64) -> Option<u64> {
        Some(match a {
            0x20 => 0x7fe01f,
            0x100 | 0x110 | 0x118 | 0x120 | 0x150 | 0x158 => !0,
            0x128 => 0x3ff0fff7d3f,
            0x138 => 0x101ff7f,
            0x160..=0x190 => 0x1f1f,
            0x198..=0x1d8 => !0,
            0x1e0 => 0x3ffffff,
            0x1e8 => 0x1111,
            0x1f0 => 0x0fffffff03ffffff,
            0x200 => 0x10f,
            0x3a8 => 0x13ffffffff30f3,
            0x3b0 => 0x1ff0f,
            0x3b8 => 0xff,
            0x3c0 => 0xffffff,
            _ if Self::retained(a) => !0,
            _ => return None,
        })
    }
    fn bte_at(a: u64) -> Option<(usize, u64)> {
        if (0x10000..0x10030).contains(&a) {
            Some((0, a - 0x10000))
        } else if (0x20000..0x20030).contains(&a) {
            Some((1, a - 0x20000))
        } else {
            None
        }
    }
    fn target(&self, offset: u64) -> Option<(u8, u64)> {
        if offset < 0x10000000 {
            Some(((offset >> 24) as u8, offset & 0xffffff))
        } else if offset >= 0x20000000 {
            let entry = (offset >> 29) - 1;
            if entry >= 7 {
                return None;
            }
            let itte = self.word(0x160 + entry * 8);
            Some((
                ((itte >> 8) & 15) as u8,
                ((itte & 31) << 29)
                    | (offset & 0x1fffffff)
                    | if itte & (1 << 12) != 0 { 1 << 47 } else { 0 },
            ))
        } else {
            None
        }
    }
    pub fn pio_read(&self, offset: u64, size: u8) -> BusRead64 {
        self.pio(offset, size, None)
    }
    pub fn pio_write(&self, offset: u64, size: u8, value: u64) -> u32 {
        self.pio(offset, size, Some(value)).status
    }
    fn pio(&self, offset: u64, size: u8, value: Option<u64>) -> BusRead64 {
        let Some((widget, target)) = self.target(offset) else {
            return BusRead64::err();
        };
        {
            let r = self.regs.lock();
            if r.words.get(&0x100).copied().unwrap_or(0) & 1 == 0
                || r.words.get(&0x110).copied().unwrap_or(0) & (1 << widget) == 0
            {
                return BusRead64::err();
            }
        }
        if !matches!(size, 1 | 2 | 4 | 8) {
            return BusRead64::err();
        }
        if widget == 1 && target == 0 {
            return BusRead64::ok(ID);
        }
        let dev = self.widgets.lock()[widget as usize].clone();
        Self::access(dev, target, size, value)
    }
    /// Device None means every possible DMA master must be permitted.
    pub fn inbound_read(
        &self,
        widget: u8,
        device: Option<u8>,
        address: u64,
        size: u8,
    ) -> BusRead64 {
        self.inbound(widget, device, address, size, None)
    }
    pub fn inbound_write(
        &self,
        widget: u8,
        device: Option<u8>,
        address: u64,
        size: u8,
        value: u64,
    ) -> u32 {
        self.inbound(widget, device, address, size, Some(value))
            .status
    }
    fn inbound(
        &self,
        widget: u8,
        device: Option<u8>,
        address: u64,
        size: u8,
        value: Option<u64>,
    ) -> BusRead64 {
        if address >= (1 << 48) || size as u64 > (1 << 48) - address {
            return BusRead64::err();
        }
        {
            let r = self.regs.lock();
            let allowed = if let Some(device) = device {
                Self::permitted(&r, widget, device)
            } else {
                (0..8).all(|d| Self::permitted(&r, widget, d))
            };
            if !allowed {
                log::warn!(
                    "Bedrock IO inbound access refused from widget {widget} device {device:?}"
                );
                return BusRead64::err();
            }
        }
        let thread = std::thread::current().id();
        if !self.inbound_threads.lock().insert(thread) {
            log::warn!("Bedrock IO reentrant inbound packet refused");
            return BusRead64::err();
        }
        let _access = InboundAccess {
            active: &self.inbound_threads,
            thread,
        };
        if address & (1 << 47) != 0 {
            let node = ((address >> 39) & 255) as u8;
            let window = address & ((1 << 39) - 1);
            if node != self.regs.lock().node || window & !0x7fffff != 0x1800000 {
                return BusRead64::err();
            }
            let dev = self.hub.lock().clone();
            Self::access(dev, window & 0x7fffff, size, value)
        } else {
            let dev = self.memory.lock().clone();
            Self::access(dev, address, size, value)
        }
    }
    fn access(dev: Option<Arc<dyn BusDevice>>, a: u64, size: u8, value: Option<u64>) -> BusRead64 {
        let Some(dev) = dev else {
            return BusRead64::err();
        };
        if let Some(v) = value {
            let status = match size {
                1 => dev.write8(a, v as u8),
                2 => dev.write16(a, v as u16),
                4 => dev.write32(a, v as u32),
                8 => dev.write64(a, v),
                _ => BUS_ERR,
            };
            BusRead64 { status, data: 0 }
        } else {
            match size {
                1 => {
                    let r = dev.read8(a);
                    BusRead64 {
                        status: r.status,
                        data: r.data as u64,
                    }
                }
                2 => {
                    let r = dev.read16(a);
                    BusRead64 {
                        status: r.status,
                        data: r.data as u64,
                    }
                }
                4 => {
                    let r = dev.read32(a);
                    BusRead64 {
                        status: r.status,
                        data: r.data as u64,
                    }
                }
                8 => dev.read64(a),
                _ => BusRead64::err(),
            }
        }
    }
    fn run_bte(&self, n: usize, mut b: Bte) {
        let memory = self.memory.lock().clone();
        let count = b.status & 0xffff;
        let bytes = count * 128;
        let overflow = (b.control & 1 == 0 && b.source > (1 << 40) - bytes)
            || b.destination > (1 << 40) - bytes;
        let mut remaining = count;
        if overflow {
            b.status = remaining | ERROR;
        } else {
            while remaining != 0 {
                let mut line = [0u64; 16];
                let mut status = BUS_OK;
                if b.control & 1 == 0 {
                    for (i, v) in line.iter_mut().enumerate() {
                        let r = Self::access(memory.clone(), b.source + i as u64 * 8, 8, None);
                        status = r.status;
                        if status != BUS_OK {
                            break;
                        }
                        *v = r.data;
                    }
                }
                if status == BUS_OK {
                    for (i, v) in line.into_iter().enumerate() {
                        status =
                            Self::access(memory.clone(), b.destination + i as u64 * 8, 8, Some(v))
                                .status;
                        if status != BUS_OK {
                            break;
                        }
                    }
                }
                if status != BUS_OK {
                    break;
                }
                remaining -= 1;
                b.source = (b.source + 128) & ADDR_MASK;
                b.destination = (b.destination + 128) & ADDR_MASK;
            }
            b.status = if remaining == 0 { 0 } else { remaining | ERROR };
        }
        self.regs.lock().bte[n] = b;
        if b.control & 0x10 != 0 {
            if Self::access(memory, b.notification, 8, Some(b.status)).status != BUS_OK {
                log::warn!("Bedrock IO BTE notification write failed");
            }
        } else {
            let node = ((b.interrupt >> 1) & 255) as u8;
            if node != self.regs.lock().node {
                log::warn!("Bedrock IO BTE interrupt reaches no node");
                return;
            }
            let hub = self.hub.lock().clone();
            let offset = if b.interrupt & 1 != 0 { 0x200090 } else { 0x90 };
            if Self::access(hub, offset, 8, Some(0x100 | ((b.interrupt >> 16) & 127))).status
                != BUS_OK
            {
                log::warn!("Bedrock IO BTE completion interrupt failed");
            }
        }
    }
}
impl Resettable for BedrockIo {
    fn power_on(&self) {
        let mut r = self.regs.lock();
        let peer = r.peer;
        let node = r.node;
        *r = Regs::default();
        r.node = node;
        r.peer = peer;
        if peer {
            *r.words.entry(0x128).or_default() |= 1 << 13;
        }
    }
}
impl BusDevice for BedrockIo {
    fn read64(&self, a: u64) -> BusRead64 {
        if a & 7 != 0 {
            return BusRead64::err();
        }
        let mut r = self.regs.lock();
        if let Some((n, a)) = Self::bte_at(a) {
            let b = &mut r.bte[n];
            return BusRead64::ok(match a {
                0 => b.status,
                8 => b.source,
                0x10 => b.destination,
                0x18 => {
                    b.status &= !BUSY;
                    b.control
                }
                0x20 => b.notification,
                0x28 => b.interrupt,
                _ => 0,
            });
        }
        let v = match a {
            0 => ID,
            8 | 0x1f8 => 0,
            0x138 => r.words.get(&a).copied().unwrap_or(0) & !(1 << 28),
            0x130 => r.words.get(&a).copied().unwrap_or(0),
            _ if Self::mask(a).is_some() => r.words.get(&a).copied().unwrap_or(0),
            _ => {
                log::warn!("Bedrock IO unsupported read {a:#x}");
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
        if let Some((n, offset)) = Self::bte_at(a) {
            let b = &mut r.bte[n];
            let mut run = false;
            match offset {
                0 => b.status = v & (0xffff | BUSY),
                8 => b.source = v & ADDR_MASK,
                0x10 => b.destination = v & ADDR_MASK,
                0x18 => {
                    b.control = v & 0x111;
                    if b.control & 0x100 != 0 {
                        log::warn!("Bedrock IO BTE poison unsupported");
                    } else if b.status & BUSY != 0 {
                        if b.status & 0xffff == 0 {
                            b.status = 0;
                            log::warn!("Bedrock IO zero-count BTE start");
                        } else {
                            run = true;
                        }
                    }
                }
                0x20 => b.notification = v & ADDR_MASK,
                0x28 => b.interrupt = v & 0x7f01ff,
                _ => {}
            }
            let b = *b;
            drop(r);
            if run {
                self.run_bte(n, b);
            }
            return BUS_OK;
        }
        match a {
            0x108 => {
                if v == 0x53474972756c6573 {
                    *r.words.entry(0x100).or_default() |= 1;
                }
            }
            0x120 | 0x1e8 => {
                *r.words.entry(a).or_default() |= v & Self::mask(a).unwrap();
            }
            0x128 => {
                let v = v & Self::mask(a).unwrap() & if r.peer { !0 } else { !(1 << 13) };
                r.words.insert(a, v);
            }
            0x130 => {
                if v == 0 {
                    r.words.insert(a, 0);
                } else {
                    log::warn!("Bedrock IO nonzero link-log write ignored");
                }
            }
            0x1f8 | 0 | 8 => {
                log::warn!("Bedrock IO read-only or unsupported clear write {a:#x}");
            }
            _ => {
                if let Some(mask) = Self::mask(a) {
                    r.words.insert(a, v & mask);
                } else {
                    log::warn!("Bedrock IO unsupported write {a:#x} = {v:#x}");
                }
            }
        }
        BUS_OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Memory {
        words: Mutex<BTreeMap<u64, u64>>,
        fail_read: Mutex<Option<u64>>,
        fail_write: Mutex<Option<u64>>,
    }
    impl BusDevice for Memory {
        fn read64(&self, a: u64) -> BusRead64 {
            if *self.fail_read.lock() == Some(a) {
                return BusRead64::err();
            }
            BusRead64::ok(self.words.lock().get(&a).copied().unwrap_or(0))
        }
        fn write64(&self, a: u64, v: u64) -> u32 {
            if *self.fail_write.lock() == Some(a) {
                return BUS_ERR;
            }
            self.words.lock().insert(a, v);
            BUS_OK
        }
    }
    fn launch(d: &BedrockIo, base: u64, count: u64, src: u64, dst: u64, notify: u64, control: u64) {
        d.write64(base, count | BUSY);
        d.write64(base + 8, src);
        d.write64(base + 0x10, dst);
        d.write64(base + 0x20, notify);
        d.write64(base + 0x18, control);
    }
    struct Reentrant(std::sync::Weak<BedrockIo>);
    impl BusDevice for Reentrant {
        fn read64(&self, a: u64) -> BusRead64 {
            self.0.upgrade().unwrap().inbound_read(15, Some(0), a, 8)
        }
    }
    struct Busy;
    impl BusDevice for Busy {
        fn read64(&self, _: u64) -> BusRead64 {
            BusRead64::busy()
        }
        fn write64(&self, _: u64, _: u64) -> u32 {
            crate::traits::BUS_BUSY
        }
    }
    struct BlockingMemory {
        entered: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        words: Mutex<BTreeMap<u64, u64>>,
    }
    impl BusDevice for BlockingMemory {
        fn write64(&self, address: u64, value: u64) -> u32 {
            if address == 0x1000 {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }
            self.words.lock().insert(address, value);
            BUS_OK
        }
    }

    #[test]
    fn pio_and_inbound_source_permissions() {
        {
            let d = BedrockIo::new();
            let m = Arc::new(Memory::default());
            d.connect_widget(8, m.clone());
            d.write64(0x160, 0x802);
            assert_eq!(d.pio_write(0x20000080, 8, 0xabcd), BUS_OK);
            assert_eq!(m.read64(0x40000080).data, 0xabcd);
            assert_eq!(d.pio_read(0x20000080, 8).data, 0xabcd);
            d.write64(0x110, 0);
            assert_eq!(d.pio_read(0x20000080, 8).status, BUS_ERR);
            d.write64(0x110, !0);
            d.write64(0x100, 0);
            assert_eq!(d.pio_read(0x20000080, 8).status, BUS_ERR);
            d.write64(0x108, 0x53474972756c6573);
            assert_eq!(d.pio_read(0x20000080, 8).status, BUS_OK);
            assert_eq!(d.pio_read(0x10000000, 8).status, BUS_ERR);
            d.power_on();
            assert_eq!(d.read64(0x100).data, !0);
            assert_eq!(d.read64(0x1e8).data, 0x111);

        }
        {
            let d = BedrockIo::new();
            let m = Arc::new(Memory::default());
            let h = Arc::new(Memory::default());
            d.connect_memory(m.clone());
            d.connect_hub(h.clone());
            assert_eq!(d.inbound_write(14, Some(2), 0x80, 8, 42), BUS_OK);
            assert_eq!(m.read64(0x80).data, 42);
            d.disable_device(14, 2);
            assert_eq!(d.inbound_read(14, Some(2), 0x80, 8).status, BUS_ERR);
            assert_eq!(d.inbound_read(14, None, 0x80, 8).status, BUS_ERR);
            assert_eq!(d.inbound_read(14, Some(1), 0x80, 8).status, BUS_OK);
            let a = (1 << 47) | 0x1800090;
            assert_eq!(d.inbound_write(15, Some(0), a, 8, 0x123), BUS_OK);
            assert_eq!(h.read64(0x90).data, 0x123);
            assert_eq!(
                d.inbound_read(15, Some(0), a | (1 << 39), 8).status,
                BUS_ERR
            );
            assert_eq!(d.inbound_read(15, Some(0), 1 << 48, 8).status, BUS_ERR);
            assert_eq!(d.inbound_read(15, Some(0), a + 0x800000, 8).status, BUS_ERR);
        }
    }
    #[test]
    fn bte_copy_zero_and_completion() {
        let d = BedrockIo::new();
        let m = Arc::new(Memory::default());
        let h = Arc::new(Memory::default());
        d.connect_memory(m.clone());
        d.connect_hub(h.clone());
        for i in 0..32 {
            m.write64(0x1000 + i * 8, 0xfeed0000 | i);
        }
        launch(&d, 0x10000, 2, 0x1000, 0x2000, 0x3000, 0x10);
        for i in 0..32 {
            assert_eq!(m.read64(0x2000 + i * 8).data, 0xfeed0000 | i);
        }
        assert_eq!(d.read64(0x10000).data, 0);
        assert_eq!(d.read64(0x10008).data, 0x1100);
        assert_eq!(m.read64(0x3000).data, 0);
        d.write64(0x20028, 53 << 16);
        launch(&d, 0x20000, 1, 0, 0x2000, 0, 1);
        for i in 0..16 {
            assert_eq!(m.read64(0x2000 + i * 8).data, 0);
        }
        assert_eq!(h.read64(0x90).data, 0x135);
    }
    #[test]
    fn bte_errors_and_termination() {
        {
            for (read, fail) in [(true, 0x1080), (false, 0x2080)] {
                let d = BedrockIo::new();
                let m = Arc::new(Memory::default());
                d.connect_memory(m.clone());
                if read {
                    *m.fail_read.lock() = Some(fail);
                } else {
                    *m.fail_write.lock() = Some(fail);
                }
                launch(&d, 0x10000, 3, 0x1000, 0x2000, 0x3000, 0x10);
                assert_eq!(d.read64(0x10000).data, 2 | ERROR);
                assert_eq!(d.read64(0x10008).data, 0x1080);
                assert_eq!(d.read64(0x10010).data, 0x2080);
                assert_eq!(m.read64(0x3000).data, 2 | ERROR);
            }
            let d = BedrockIo::new();
            let m = Arc::new(Memory::default());
            d.connect_memory(m.clone());
            *m.fail_write.lock() = Some(0x3000);
            launch(&d, 0x10000, 1, 0x1000, 0x2000, 0x3000, 0x10);
            assert_eq!(d.read64(0x10000).data, 0);
            launch(&d, 0x10000, 2, 0xffffffff80, 0x2000, 0x3000, 0x10);
            assert_eq!(d.read64(0x10000).data, 2 | ERROR);
        }
        {
            let d = BedrockIo::new();
            launch(&d, 0x10000, 1, 0, 0, 0, 0x100);
            assert_eq!(d.read64(0x10000).data, 1 | BUSY);
            d.read64(0x10018);
            assert_eq!(d.read64(0x10000).data, 1);
            launch(&d, 0x10000, 0, 0, 0, 0, 0x10);
            assert_eq!(d.read64(0x10000).data, 0);
        }
    }
    #[test]
    fn forwarding_reentrancy_and_retry_status() {
        let d = Arc::new(BedrockIo::new());
        d.connect_memory(Arc::new(Reentrant(Arc::downgrade(&d))));
        assert_eq!(d.inbound_read(15, Some(0), 0, 8).status, BUS_ERR);
        d.connect_memory(Arc::new(Busy));
        assert_eq!(
            d.inbound_read(15, Some(0), 0, 8).status,
            crate::traits::BUS_BUSY
        );
        d.connect_widget(8, Arc::new(Busy));
        assert_eq!(d.pio_read(0x8000000, 8).status, crate::traits::BUS_BUSY);
        assert_eq!(d.pio_write(0x8000000, 8, 42), crate::traits::BUS_BUSY);

    }
    #[test]
    fn independent_dma_workers_both_reach_memory() {
        let d = Arc::new(BedrockIo::new());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let memory = Arc::new(BlockingMemory {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            words: Mutex::new(BTreeMap::new()),
        });
        d.connect_memory(memory.clone());
        let first_io = d.clone();
        let first =
            std::thread::spawn(move || first_io.inbound_write(14, Some(0), 0x1000, 8, 0x1234));
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let second_io = d.clone();
        let second =
            std::thread::spawn(move || second_io.inbound_write(15, Some(1), 0x2000, 8, 0x5678));
        let second_status = second.join().unwrap();
        release_tx.send(()).unwrap();
        let first_status = first.join().unwrap();
        assert_eq!(second_status, BUS_OK);
        assert_eq!(first_status, BUS_OK);
        assert_eq!(
            *memory.words.lock(),
            BTreeMap::from([(0x1000, 0x1234), (0x2000, 0x5678)])
        );
    }
}
