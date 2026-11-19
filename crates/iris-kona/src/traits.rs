//! The part of iris's `traits` the mounted files name. Iris's `BusDevice`
//! is the CPU core's bus interface; the engine's host memory is
//! `engine::PipeMemory`, which implements this.

pub const BUS_OK: u32 = 0;
pub const BUS_ERR: u32 = 2;

pub trait BusDevice: Send + Sync {
    /// Read `buf.len()` bytes at `addr`: `BUS_OK` or a failure status.
    fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> u32;
    /// Write `data` at `addr`: `BUS_OK` or a failure status.
    fn write_bytes(&self, addr: u64, data: &[u8]) -> u32;
}
