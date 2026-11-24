//! The part of iris's `traits` the board names. iris's `BusDevice` is its CPU core's bus interface; here the
//! board's register accesses come from the engine's `read` and `write`, and the tests drive them through this.

pub const BUS_OK: u32 = 0;
pub const BUS_BUSY: u32 = 0x100;
pub const BUS_ERR: u32 = 0x0800_001C;

macro_rules! bus_read {
    ($t:ident, $d:ty) => {
        #[derive(Clone, Copy, Debug)]
        pub struct $t {
            pub status: u32,
            pub data: $d,
        }
        impl $t {
            pub fn ok(data: $d) -> Self {
                Self { status: BUS_OK, data }
            }
            pub fn busy() -> Self {
                Self { status: BUS_BUSY, data: 0 }
            }
            pub fn err() -> Self {
                Self { status: BUS_ERR, data: 0 }
            }
        }
    };
}
bus_read!(BusRead8, u8);
bus_read!(BusRead16, u16);
bus_read!(BusRead32, u32);
bus_read!(BusRead64, u64);

/// The accesses the GIO slot answers, by width.
pub trait BusDevice: Send + Sync {
    fn read8(&self, addr: u64) -> BusRead8;
    fn write8(&self, addr: u64, val: u8) -> u32;
    fn read16(&self, addr: u64) -> BusRead16;
    fn write16(&self, addr: u64, val: u16) -> u32;
    fn read32(&self, addr: u64) -> BusRead32;
    fn write32(&self, addr: u64, val: u32) -> u32;
    fn read64(&self, addr: u64) -> BusRead64;
    fn write64(&self, addr: u64, val: u64) -> u32;
}

/// What a test board needs to run and stop its threads.
pub trait Device: Send + Sync {
    fn start(&self);
    fn stop(&self);
    fn is_running(&self) -> bool;
}
