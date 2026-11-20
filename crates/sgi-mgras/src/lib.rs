//! The IMPACT raster engine of iris without the board.
//!
//! The modules are the files under `src/dev/mgras`, mounted in place and
//! under the paths iris gives them, so the files build unchanged here and in
//! iris.
#![allow(dead_code, unused_imports, private_interfaces)]

pub use sgi_gpu_core as wgpu;

#[path = "../../../src/perfetto_macros.rs"]
mod perfetto_macros;

pub mod disp;
pub mod display;
pub mod jit;
pub mod ffi;
pub mod locks;
pub mod dev;

/// Stand-ins for the parts of iris the mounted whole-board tests name.
#[cfg(test)]
pub mod config {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum GraphicsBoard {
        SolidImpact,
        HighImpact,
        MaxImpact,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum RasterUnit {
        Cpu,
        Wgpu,
    }
}

#[cfg(test)]
pub mod traits {
    pub const BUS_OK: u32 = 0;

    pub struct BusRead8 {
        pub data: u8,
    }

    impl BusRead8 {
        pub fn ok(data: u8) -> Self {
            BusRead8 { data }
        }
    }

    pub struct BusRead32 {
        pub data: u32,
    }

    impl BusRead32 {
        pub fn ok(data: u32) -> Self {
            BusRead32 { data }
        }
    }

    /// What the tests' host memory implements; the engine's shell hands the engine RAM another way.
    pub trait BusDevice: Send + Sync {
        fn read8(&self, addr: u64) -> BusRead8;
        fn write8(&self, addr: u64, val: u8) -> u32;
        fn read32(&self, addr: u64) -> BusRead32;
    }
}
