//! Ported from the QEMU platform calls hw/display/sgi/ and
//! hw/misc/sgi-odyssey.c make (system/dma.h, system/address-spaces.h,
//! qemu/timer.h), in jamesbraid/qemu-sgi-origami.
//!
//! The model reaches guest memory the way the QEMU device did: through one
//! system address space, here the machine's bus-master view of memory
//! (HEART's RAM on the Octane) installed by the board when it is built.
//! Clocks are the host's monotonic clock; the board has no guest-time
//! source of its own.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

use parking_lot::RwLock;

use crate::traits::BusDevice;

/// `AddressSpace`: a view of the bus a DMA master uses.
#[derive(Clone, Default)]
pub struct AddressSpace {
    pub mem: Option<Arc<dyn BusDevice>>,
}

/// `MemTxResult`.
pub type MemTxResult = u32;
pub const MEMTX_OK: MemTxResult = 0;
pub const MEMTX_ERROR: MemTxResult = 1;
pub const MEMTX_DECODE_ERROR: MemTxResult = 2;

/// `DMADirection`.
pub type DMADirection = u32;
pub const DMA_DIRECTION_TO_DEVICE: DMADirection = 0;
pub const DMA_DIRECTION_FROM_DEVICE: DMADirection = 1;

static SYSTEM_MEMORY: RwLock<Option<Arc<dyn BusDevice>>> = RwLock::new(None);

/// Install the bus-master view of memory `address_space_memory()` reaches.
pub fn set_address_space_memory(mem: Arc<dyn BusDevice>) {
    *SYSTEM_MEMORY.write() = Some(mem);
}

/// `&address_space_memory`.
pub fn address_space_memory() -> AddressSpace {
    AddressSpace { mem: SYSTEM_MEMORY.read().clone() }
}

impl AddressSpace {
    /// Read `buf.len()` bytes at `addr`, eight at a time where aligned.
    pub fn read(&self, addr: u64, buf: &mut [u8]) -> MemTxResult {
        let Some(m) = &self.mem else { return MEMTX_DECODE_ERROR };
        let mut i = 0usize;
        while i < buf.len() {
            let a = addr + i as u64;
            if a & 7 == 0 && buf.len() - i >= 8 {
                let r = m.read64(a);
                if r.status != crate::traits::BUS_OK {
                    return MEMTX_ERROR;
                }
                buf[i..i + 8].copy_from_slice(&r.data.to_be_bytes());
                i += 8;
            } else {
                let r = m.read8(a);
                if r.status != crate::traits::BUS_OK {
                    return MEMTX_ERROR;
                }
                buf[i] = r.data;
                i += 1;
            }
        }
        MEMTX_OK
    }

    /// Write `buf` at `addr`, eight bytes at a time where aligned.
    pub fn write(&self, addr: u64, buf: &[u8]) -> MemTxResult {
        let Some(m) = &self.mem else { return MEMTX_DECODE_ERROR };
        let mut i = 0usize;
        while i < buf.len() {
            let a = addr + i as u64;
            let st = if a & 7 == 0 && buf.len() - i >= 8 {
                let v = u64::from_be_bytes(buf[i..i + 8].try_into().unwrap());
                i += 8;
                m.write64(a, v)
            } else {
                let v = buf[i];
                i += 1;
                m.write8(a, v)
            };
            if st != crate::traits::BUS_OK {
                return MEMTX_ERROR;
            }
        }
        MEMTX_OK
    }
}

/// `dma_memory_read(as, addr, buf, len, attrs)`.
pub fn dma_memory_read(as_: &AddressSpace, addr: u64, buf: &mut [u8]) -> MemTxResult {
    as_.read(addr, buf)
}

/// `dma_memory_write(as, addr, buf, len, attrs)`.
pub fn dma_memory_write(as_: &AddressSpace, addr: u64, buf: &[u8]) -> MemTxResult {
    as_.write(addr, buf)
}

pub fn address_space_read(as_: &AddressSpace, addr: u64, buf: &mut [u8]) -> MemTxResult {
    as_.read(addr, buf)
}

pub fn address_space_write(as_: &AddressSpace, addr: u64, buf: &[u8]) -> MemTxResult {
    as_.write(addr, buf)
}

pub fn address_space_stl_be(as_: &AddressSpace, addr: u64, v: u32) -> MemTxResult {
    as_.write(addr, &v.to_be_bytes())
}

pub fn address_space_stw_be(as_: &AddressSpace, addr: u64, v: u32) -> MemTxResult {
    as_.write(addr, &(v as u16).to_be_bytes())
}

pub fn address_space_stq_be(as_: &AddressSpace, addr: u64, v: u64) -> MemTxResult {
    as_.write(addr, &v.to_be_bytes())
}

pub fn address_space_ldl_be(as_: &AddressSpace, addr: u64) -> (u32, MemTxResult) {
    let mut b = [0u8; 4];
    let r = as_.read(addr, &mut b);
    (u32::from_be_bytes(b), r)
}

pub fn address_space_ldq_be(as_: &AddressSpace, addr: u64) -> (u64, MemTxResult) {
    let mut b = [0u8; 8];
    let r = as_.read(addr, &mut b);
    (u64::from_be_bytes(b), r)
}

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// `qemu_clock_get_ns(QEMU_CLOCK_VIRTUAL)`: the host's monotonic clock.
pub fn qemu_clock_virtual_ns() -> i64 {
    epoch().elapsed().as_nanos() as i64
}

/// `qemu_clock_get_ns(QEMU_CLOCK_REALTIME)`.
pub fn qemu_clock_realtime_ns() -> i64 {
    epoch().elapsed().as_nanos() as i64
}

pub const SCALE_US: i64 = 1_000;
pub const SCALE_MS: i64 = 1_000_000;
pub const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;
