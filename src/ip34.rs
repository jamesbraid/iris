//! Fuel IP34 processor-side composition over the shared physical bus.
//!
//! Ported from hw/mips/sgi/sn1-node.c in jamesbraid/qemu-sgi-origami.
//! SPDX-License-Identifier: BSD-3-Clause

use crate::physical::Physical;
use crate::traits::{BusDevice, BusRead16, BusRead32, BusRead64, BusRead8, BUS_ERR};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

/// Board-local request encoding for the interpreter. The CPU already passes
/// XKPHYS bits 57:58 through its configurable mask; Physical never sees them.
pub(crate) const CPU_PHYS_MASK: u64 = ((1u64 << 40) - 1) | (3u64 << 57);
const PA_MASK: u64 = (1u64 << 40) - 1;

struct Region {
    raw_uc: u8,
    base: u64,
    end: u64,
    device: Arc<dyn BusDevice>,
}

/// Memory remains owned by Physical; the board resolves the UC-space tag
/// before dispatching an I/O request. Unclaimed accesses produce a bus error.
pub struct Ip34Bus {
    pub phys: Arc<Physical>,
    regions: Vec<Region>,
    fallback: [Option<(u64, Arc<dyn BusDevice>)>; 4],
    fault: Mutex<Option<String>>,
    recent: Mutex<VecDeque<String>>,
}

impl Ip34Bus {
    pub fn new(phys: Arc<Physical>) -> Self {
        Self {
            phys,
            regions: Vec::new(),
            fallback: std::array::from_fn(|_| None),
            fault: Mutex::new(None),
            recent: Mutex::new(VecDeque::new()),
        }
    }

    /// `raw_uc` is XKPHYS bits 58:57: 0 Hub, 1 I/O, 2 special, 3 RAM.
    /// QEMU normalizes those as `(raw_uc + 1) & 3`; the raw tag remains here.
    pub fn map(&mut self, raw_uc: u8, base: u64, size: u64, device: Arc<dyn BusDevice>) {
        assert!(
            raw_uc < 4 && size > 0 && base.checked_add(size).is_some_and(|end| end <= PA_MASK + 1)
        );
        let end = base + size;
        assert!(!self
            .regions
            .iter()
            .any(|r| r.raw_uc == raw_uc && base < r.end && r.base < end));
        self.regions.push(Region {
            raw_uc,
            base,
            end,
            device,
        });
    }

    pub fn set_fallback(&mut self, raw_uc: u8, end: u64, device: Arc<dyn BusDevice>) {
        assert!(raw_uc < 4 && end <= PA_MASK + 1);
        self.fallback[raw_uc as usize] = Some((end, device));
    }

    fn target(&self, addr: u64, width: u64) -> Option<(&dyn BusDevice, u64)> {
        if addr & !CPU_PHYS_MASK != 0 {
            return None;
        }
        let raw_uc = (addr >> 57) as u8;
        let pa = addr & PA_MASK;
        let end = pa.checked_add(width)?;
        if let Some(r) = self
            .regions
            .iter()
            .find(|r| r.raw_uc == raw_uc && pa >= r.base && pa < r.end)
        {
            return (end <= r.end).then_some((r.device.as_ref(), pa - r.base));
        }
        None.or_else(|| {
            self.fallback[raw_uc as usize]
                .as_ref()
                .filter(|(limit, _)| end <= *limit)
                .map(|(_, dev)| (dev.as_ref(), pa))
        })
    }

    fn miss(&self, addr: u64, width: u64, write: bool, value: u64) {
        let mut fault = self.fault.lock();
        if fault.is_none() {
            *fault = Some(format!(
                "{} bus={addr:#018x} width={width} value={value:#018x}",
                if write { "write" } else { "read" }
            ));
        }
    }

    fn record(&self, addr: u64, width: u64, write: bool, value: u64, status: u32) {
        let mut recent = self.recent.lock();
        if recent.len() == 128 {
            recent.pop_front();
        }
        recent.push_back(format!(
            "{} bus={addr:#018x} raw_uc={} width={width} value={value:#018x} status={status:#x}",
            if write { "w" } else { "r" },
            addr >> 57
        ));
        if status == BUS_ERR {
            self.miss(addr, width, write, value);
        }
    }

    pub fn first_fault(&self) -> Option<String> {
        self.fault.lock().clone()
    }
    pub fn recent_accesses(&self) -> Vec<String> {
        self.recent.lock().iter().cloned().collect()
    }
}

macro_rules! access {
    ($read:ident, $write:ident, $result:ident, $ty:ty, $width:expr) => {
        fn $read(&self, addr: u64) -> $result {
            let result = self
                .target(addr, $width)
                .map(|(dev, local)| dev.$read(local))
                .unwrap_or_else($result::err);
            self.record(addr, $width, false, result.data as u64, result.status);
            result
        }
        fn $write(&self, addr: u64, value: $ty) -> u32 {
            let status = self
                .target(addr, $width)
                .map(|(dev, local)| dev.$write(local, value))
                .unwrap_or(BUS_ERR);
            self.record(addr, $width, true, value as u64, status);
            status
        }
    };
}
impl BusDevice for Ip34Bus {
    access!(read8, write8, BusRead8, u8, 1);
    access!(read16, write16, BusRead16, u16, 2);
    access!(read32, write32, BusRead32, u32, 4);
    access!(read64, write64, BusRead64, u64, 8);
}

#[cfg(test)]
#[path = "ip34/tests.rs"]
mod tests;
