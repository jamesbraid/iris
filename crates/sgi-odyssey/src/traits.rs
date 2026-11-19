//! The part of iris's bus traits the board uses to reach host memory for
//! DMA: this crate's host-memory trait. iris's `traits` is tied to the CPU
//! core, so a front end implements this one over its own bus.

pub const BUS_OK: u32 = 0;
pub const BUS_ERR: u32 = 0x0800_001C;

#[derive(Clone, Copy, Debug)]
pub struct BusRead8 {
    pub status: u32,
    pub data: u8,
}

impl BusRead8 {
    pub fn ok(data: u8) -> Self {
        Self { status: BUS_OK, data }
    }
    pub fn err() -> Self {
        Self { status: BUS_ERR, data: 0 }
    }
    pub fn is_ok(&self) -> bool {
        self.status == BUS_OK
    }
}

/// Host memory as the board's DMA sees it, big-endian.
pub trait BusDevice: Send + Sync {
    fn read8(&self, _addr: u64) -> BusRead8 {
        BusRead8::err()
    }
    fn write8(&self, _addr: u64, _val: u8) -> u32 {
        BUS_ERR
    }
    /// A whole big-endian 16-bit or 32-bit word at an aligned `addr`, in one access.
    fn write16(&self, _addr: u64, _val: u16) -> u32 {
        BUS_ERR
    }
    fn write32(&self, _addr: u64, _val: u32) -> u32 {
        BUS_ERR
    }
    /// Whole big-endian qwords at a qword-aligned `addr`.
    fn read_block(&self, _addr: u64, _buf: &mut [u64]) -> u32 {
        BUS_ERR
    }
    fn write_block(&self, _addr: u64, _buf: &[u64]) -> u32 {
        BUS_ERR
    }
}
