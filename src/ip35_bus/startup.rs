// SPDX-License-Identifier: BSD-3-Clause
//! Private early data for Fuel's sole processor.
//!
//! Ported from hw/mips/sgi/sn-promdata.c and hw/mips/sgi/sn-promdata.h in jamesbraid/qemu-sgi-origami.
//! The PC/phase-bit lifetime is an existing emulator approximation, not a
//! physical cache model. Only raw-0 cached window accesses select these bytes.

use crate::traits::*;
use parking_lot::Mutex;
use std::{cell::Cell, marker::PhantomData, rc::Rc, sync::Arc};

pub(super) const WINDOW_BASE: u64 = 0x100000;
pub(super) const WINDOW_SIZE: usize = 0x8000;
thread_local! { static ACTIVE: Cell<usize> = const { Cell::new(0) }; }

pub(crate) struct StartupWindow {
    state: Mutex<State>,
    flash_size: u64,
}
struct State {
    bytes: Box<[u8; WINDOW_SIZE]>,
    private: bool,
    left: bool,
    phase_bit: bool,
    pc: Option<u64>,
    fetch_pc: Option<u64>,
    reads: u64,
    writes: u64,
}
impl Default for State {
    fn default() -> Self {
        Self {
            bytes: Box::new([0; WINDOW_SIZE]),
            private: false,
            left: false,
            phase_bit: false,
            pc: None,
            fetch_pc: None,
            reads: 0,
            writes: 0,
        }
    }
}
/// Restores the previous CPU scope even when instruction execution unwinds.
/// Rc makes this guard non-Send: another thread cannot inherit CPU access.
pub(crate) struct CpuAccess<'a> {
    previous: usize,
    _window: &'a StartupWindow,
    _thread: PhantomData<Rc<()>>,
}
impl Drop for CpuAccess<'_> {
    fn drop(&mut self) {
        ACTIVE.with(|a| a.set(self.previous));
    }
}
impl StartupWindow {
    pub(crate) fn new(flash_size: u64) -> Self {
        Self {
            state: Mutex::new(State::default()),
            flash_size,
        }
    }
    pub(crate) fn reset(&self) {
        *self.state.lock() = State::default();
    }
    pub(crate) fn begin_access(&self) -> CpuAccess<'_> {
        let previous = ACTIVE.with(|a| a.replace(self as *const Self as usize));
        CpuAccess {
            previous,
            _window: self,
            _thread: PhantomData,
        }
    }
    fn cpu_access(&self) -> bool {
        ACTIVE.with(|a| a.get() == self as *const Self as usize)
    }
    fn home(&self, pc: Option<u64>) -> bool {
        pc.is_some_and(|p| {
            (0x1fc00000..0x1fc00000 + self.flash_size).contains(&p)
                || (0x01a00000..0x01c00000).contains(&p)
        })
    }
    /// Publish only on the CPU thread before executing an instruction.
    /// The observer can release an existing latch but never arms one.
    pub(crate) fn observe(&self, fpr0: u64, translated_pc: Option<u64>, live_node: u8) {
        let node_base = (live_node as u64) << 33;
        let pc = translated_pc.map(|p| {
            let p = p & ((1u64 << 40) - 1);
            // Retain low local carriers. Normalize only our programmed node,
            // never every named node. Fuel's node extent is 8 GiB.
            if p.wrapping_sub(node_base) < 1 << 33 {
                p - node_base
            } else {
                p
            }
        });
        let mut s = self.state.lock();
        s.fetch_pc = translated_pc;
        s.phase_bit = fpr0 & 2 != 0;
        s.pc = pc;
        if s.private && !s.phase_bit && !self.home(s.pc) {
            s.private = false;
            s.left = true;
        }
    }
    fn private(&self, s: &mut State, read: bool) -> bool {
        if !self.cpu_access() {
            return false;
        }
        // The donor read handler ends the phase if execution enters the
        // window, including while the phase bit is still set.
        if read
            && s.fetch_pc
                .is_some_and(|p| (WINDOW_BASE..WINDOW_BASE + WINDOW_SIZE as u64).contains(&p))
        {
            s.private = false;
            s.left = true;
        }
        if s.left {
            return false;
        }
        if !s.private && s.phase_bit && self.home(s.pc) {
            s.private = true;
        }
        if s.private {
            if read {
                s.reads += 1;
            } else {
                s.writes += 1;
            }
        }
        s.private
    }
}
impl Resettable for StartupWindow {
    fn power_on(&self) {
        self.reset();
    }
}

/// Cached processor view. SysAd, DMA and debugger accesses use normal RAM.
/// No direct pointers are published, so translated/decode caches must re-read.
pub(crate) struct CachedView {
    backing: Arc<dyn BusDevice>,
    window: Arc<StartupWindow>,
}
impl CachedView {
    pub(crate) fn new(backing: Arc<dyn BusDevice>, window: Arc<StartupWindow>) -> Self {
        Self { backing, window }
    }
    fn overlaps(a: u64, n: u64) -> bool {
        a < WINDOW_BASE + WINDOW_SIZE as u64 && a.saturating_add(n) > WINDOW_BASE
    }
    fn read_private(&self, a: u64, n: usize) -> Option<BusRead64> {
        if !Self::overlaps(a, n as u64) {
            return None;
        }
        let mut s = self.window.state.lock();
        if !self.window.private(&mut s, true) {
            return None;
        }
        let mut value = 0;
        for i in 0..n {
            let p = a + i as u64;
            let byte = if (WINDOW_BASE..WINDOW_BASE + WINDOW_SIZE as u64).contains(&p) {
                s.bytes[(p - WINDOW_BASE) as usize]
            } else {
                // Never invoke a downstream device while holding state.
                drop(s);
                let r = self.backing.read8(p);
                if r.status != BUS_OK {
                    return Some(BusRead64 {
                        data: 0,
                        status: r.status,
                    });
                }
                s = self.window.state.lock();
                r.data
            };
            value = (value << 8) | byte as u64;
        }
        Some(BusRead64::ok(value))
    }
    fn write_private(&self, a: u64, n: usize, v: u64) -> Option<u32> {
        if !Self::overlaps(a, n as u64) {
            return None;
        }
        let mut s = self.window.state.lock();
        if !self.window.private(&mut s, false) {
            return None;
        }
        for i in 0..n {
            let p = a + i as u64;
            let byte = (v >> (8 * (n - 1 - i))) as u8;
            if (WINDOW_BASE..WINDOW_BASE + WINDOW_SIZE as u64).contains(&p) {
                s.bytes[(p - WINDOW_BASE) as usize] = byte;
            } else {
                drop(s);
                let status = self.backing.write8(p, byte);
                if status != BUS_OK {
                    return Some(status);
                }
                s = self.window.state.lock();
            }
        }
        Some(BUS_OK)
    }
}
macro_rules! access {
    ($r:ident,$w:ident,$t:ty,$out:ident,$n:expr) => {
        fn $r(&self, a: u64) -> $out {
            self.read_private(a, $n)
                .map(|r| $out {
                    data: r.data as $t,
                    status: r.status,
                })
                .unwrap_or_else(|| self.backing.$r(a))
        }
        fn $w(&self, a: u64, v: $t) -> u32 {
            self.write_private(a, $n, v as u64)
                .unwrap_or_else(|| self.backing.$w(a, v))
        }
    };
}
impl BusDevice for CachedView {
    access!(read8, write8, u8, BusRead8, 1);
    access!(read16, write16, u16, BusRead16, 2);
    access!(read32, write32, u32, BusRead32, 4);
    access!(read64, write64, u64, BusRead64, 8);
    fn dma_read64(&self, a: u64) -> BusRead64 {
        self.backing.dma_read64(a)
    }
    fn dma_write64(&self, a: u64, v: u64) -> u32 {
        self.backing.dma_write64(a, v)
    }
}
#[cfg(test)]
#[path = "startup_tests.rs"]
mod tests;
