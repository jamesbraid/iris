//! The IMPACT raster engine of iris without the board.
//!
//! The modules are the files under `src/dev/mgras`, mounted in place and
//! under the paths iris gives them, so the files build unchanged here and in
//! iris.
#![allow(dead_code, unused_imports, private_interfaces)]

pub use sgi_gpu_core as wgpu;

#[path = "../../../src/perfetto_macros.rs"]
mod perfetto_macros;

pub mod display;
pub mod jit;
pub mod locks;
pub mod traits;
pub mod dev;
