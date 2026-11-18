//! The VPro back end of iris without the board.
//!
//! The modules are the files under `src/dev/odyssey`, mounted in place and
//! under the paths iris gives them, so the files build unchanged here and in
//! iris.

// The back end is a part of a board: much of what the mounted files define
// is reached only through the board.
#![allow(dead_code, unused_imports, private_interfaces)]

pub use sgi_gpu_core as wgpu;

pub mod devlog;
pub mod display;

#[path = "../../../src/tile_pool.rs"]
pub mod tile_pool;

pub mod dev;
