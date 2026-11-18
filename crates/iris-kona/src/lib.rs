//! The KONA raster back end of iris without the board.
//!
//! The modules are the files under `src/dev/kona`, mounted in place and
//! under the paths iris gives them, so the files build unchanged here and in
//! iris.
#![allow(dead_code, unused_imports, private_interfaces)]

pub use sgi_gpu_core as wgpu;

pub mod devlog;
pub mod dev;

#[path = "../../../src/tile_pool.rs"]
pub mod tile_pool;
