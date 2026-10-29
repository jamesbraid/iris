//! IP35/Bedrock processor views over the shared Physical address decoder.
//!
//! Ported from hw/mips/sgi/sn1-node.c in jamesbraid/qemu-sgi-origami.
//! SPDX-License-Identifier: BSD-3-Clause

use crate::physical::Physical;
use crate::traits::*;
use parking_lot::Mutex;
use std::{collections::VecDeque, sync::Arc};

pub(crate) const CPU_PHYS_MASK: u64 = ((1u64 << 40) - 1) | (3u64 << 57);
const PA_MASK: u64 = (1u64 << 40) - 1;

struct Unclaimed;
impl BusDevice for Unclaimed {}

pub(crate) fn empty_view() -> Arc<Physical> {
    let view = Physical::new(Vec::new());
    view.set_default(Arc::new(Unclaimed), "IP35 unclaimed address");
    view
}

/// One chip-relative endpoint with exact bounds inside its rounded map slots.
struct OffsetEndpoint {
    base: u64,
    size: u64,
    device: Arc<dyn BusDevice>,
}
impl OffsetEndpoint {
    fn local(&self, a: u64, n: u64) -> Option<u64> {
        let local = a.checked_sub(self.base)?;
        (local.checked_add(n)? <= self.size).then_some(local)
    }
}
macro_rules! endpoint_access {
    ($r:ident,$w:ident,$t:ty,$out:ident,$n:expr) => {
        fn $r(&self, a: u64) -> $out {
            self.local(a, $n)
                .map(|at| self.device.$r(at))
                .unwrap_or_else($out::err)
        }
        fn $w(&self, a: u64, v: $t) -> u32 {
            self.local(a, $n)
                .map(|at| self.device.$w(at, v))
                .unwrap_or(BUS_ERR)
        }
    };
}
impl BusDevice for OffsetEndpoint {
    fn write_uncached_subword(&self, a: u64, width: usize, source: u64) -> u32 {
        if width != 1 && width != 2 {
            return BUS_ERR;
        }
        self.local(a, width as u64)
            .map(|at| self.device.write_uncached_subword(at, width, source))
            .unwrap_or(BUS_ERR)
    }
    endpoint_access!(read8, write8, u8, BusRead8, 1);
    endpoint_access!(read16, write16, u16, BusRead16, 2);
    endpoint_access!(read32, write32, u32, BusRead32, 4);
    endpoint_access!(read64, write64, u64, BusRead64, 8);
}
pub(crate) fn map_device(
    view: &Physical,
    base: u64,
    size: u64,
    device: Arc<dyn BusDevice>,
    name: &str,
) {
    assert!(size > 0 && base.checked_add(size).is_some_and(|end| end <= PA_MASK + 1));
    view.map(
        base,
        base + size,
        Arc::new(OffsetEndpoint { base, size, device }),
        name,
    );
}

struct Trace {
    enabled: bool,
    fault: Mutex<Option<String>>,
    recent: Mutex<VecDeque<String>>,
}
impl Default for Trace {
    fn default() -> Self {
        Self { enabled: std::env::var_os("IRIS_FUEL_TRACE").is_some(), fault: Mutex::new(None), recent: Mutex::new(VecDeque::new()) }
    }
}
impl Trace {
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
        if status == BUS_ERR { self.miss(addr, width, write, value); }
        if !self.enabled { return; }
        let mut recent = self.recent.lock();
        if recent.len() == 128 {
            recent.pop_front();
        }
        recent.push_back(format!(
            "{} bus={addr:#018x} raw_uc={} width={width} value={value:#018x} status={status:#x}",
            if write { "w" } else { "r" },
            addr >> 57
        ));
    }

    pub fn first_fault(&self) -> Option<String> {
        self.fault.lock().clone()
    }
    pub fn recent_accesses(&self) -> Vec<String> {
        self.recent.lock().iter().cloned().collect()
    }
}

/// UC-space selection only. Each view has an existing Physical decode table.
/// The cached plane owns Bedrock's banks; all SysAd planes have no banks.
pub struct Ip35Bus {
    pub phys: Arc<Physical>,
    views: [Arc<Physical>; 4],
    trace: Arc<Trace>,
}
impl Ip35Bus {
    pub fn new(phys: Arc<Physical>) -> Self {
        phys.set_default(Arc::new(Unclaimed), "IP35 unclaimed cached address");
        Self {
            phys,
            views: std::array::from_fn(|_| empty_view()),
            trace: Arc::new(Trace::default()),
        }
    }
    /// Raw XKPHYS bits 58:57 select Hub-special, I/O, fetchop, or uncached RAM.
    pub fn map(&mut self, raw_uc: u8, base: u64, size: u64, device: Arc<dyn BusDevice>) {
        assert!(raw_uc < 4);
        map_device(
            &self.views[raw_uc as usize],
            base,
            size,
            device.clone(),
            "IP35 processor aperture",
        );
    }
    pub fn set_fallback(&mut self, raw_uc: u8, end: u64, device: Arc<dyn BusDevice>) {
        self.map(raw_uc, 0, end, device);
    }
    fn target(&self, a: u64, n: u64) -> Option<(&Physical, u64)> {
        let pa = plain_address(a, n)?;
        Some((self.views[(a >> 57) as usize].as_ref(), pa))
    }
    pub fn cached_view(&self, plain_memory: Arc<dyn BusDevice>) -> Arc<dyn BusDevice> {
        Arc::new(CachedPhysical {
            phys: self.phys.clone(),
            plain_memory,
            trace: self.trace.clone(),
        })
    }
    pub fn first_fault(&self) -> Option<String> {
        self.trace.first_fault()
    }
    pub fn recent_accesses(&self) -> Vec<String> {
        self.trace.recent_accesses()
    }
}
fn plain_address(a: u64, n: u64) -> Option<u64> {
    if a & !CPU_PHYS_MASK != 0 {
        return None;
    }
    let pa = a & PA_MASK;
    (pa.checked_add(n)? <= PA_MASK + 1).then_some(pa)
}
struct CachedPhysical {
    phys: Arc<Physical>,
    plain_memory: Arc<dyn BusDevice>,
    trace: Arc<Trace>,
}
impl CachedPhysical {
    fn target(&self, a: u64, n: u64) -> Option<(&Physical, u64)> {
        // UC selectors belong to the uncached SysAd connection. Reject them
        // here rather than turning a malformed cached request into an alias.
        if a & !PA_MASK != 0 || a & (n - 1) != 0 {
            return None;
        }
        Some((self.phys.as_ref(), plain_address(a, n)?))
    }
}
macro_rules! processor_access {
    ($r:ident,$w:ident,$t:ty,$out:ident,$n:expr) => {
        fn $r(&self, a: u64) -> $out {
            let result = self
                .target(a, $n)
                .map(|(v, pa)| v.$r(pa))
                .unwrap_or_else($out::err);
            self.trace
                .record(a, $n, false, result.data as u64, result.status);
            result
        }
        fn $w(&self, a: u64, v: $t) -> u32 {
            let status = self
                .target(a, $n)
                .map(|(view, pa)| view.$w(pa, v))
                .unwrap_or(BUS_ERR);
            self.trace.record(a, $n, true, v as u64, status);
            status
        }
    };
}
impl BusDevice for Ip35Bus {
    fn write_uncached_subword(&self, a: u64, width: usize, source: u64) -> u32 {
        if width != 1 && width != 2 {
            return BUS_ERR;
        }
        let status = self.target(a, width as u64)
            .map(|(view, pa)| view.write_uncached_subword(pa, width, source))
            .unwrap_or(BUS_ERR);
        self.trace.record(a, width as u64, true, source, status);
        status
    }
    processor_access!(read8, write8, u8, BusRead8, 1);
    processor_access!(read16, write16, u16, BusRead16, 2);
    processor_access!(read32, write32, u32, BusRead32, 4);
    processor_access!(read64, write64, u64, BusRead64, 8);
}
impl BusDevice for CachedPhysical {
    processor_access!(read8, write8, u8, BusRead8, 1);
    processor_access!(read16, write16, u16, BusRead16, 2);
    processor_access!(read32, write32, u32, BusRead32, 4);
    processor_access!(read64, write64, u64, BusRead64, 8);
    fn dma_read64(&self, a: u64) -> BusRead64 {
        plain_address(a, 8)
            .map(|pa| self.plain_memory.dma_read64(pa))
            .unwrap_or_else(BusRead64::err)
    }
    fn dma_write64(&self, a: u64, v: u64) -> u32 {
        plain_address(a, 8)
            .map(|pa| self.plain_memory.dma_write64(pa, v))
            .unwrap_or(BUS_ERR)
    }
}

pub(crate) mod startup;
#[cfg(test)]
mod tests;
