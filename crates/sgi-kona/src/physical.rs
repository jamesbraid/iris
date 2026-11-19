//! The two functions of iris's `physical` the mounted files call.

use crate::traits::BusDevice;

pub fn read_bytes(bus: &dyn BusDevice, addr: u64, buf: &mut [u8]) -> u32 {
    bus.read_bytes(addr, buf)
}

pub fn write_bytes(bus: &dyn BusDevice, addr: u64, data: &[u8]) -> u32 {
    bus.write_bytes(addr, data)
}
