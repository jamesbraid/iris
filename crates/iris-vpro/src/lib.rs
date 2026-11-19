//! The VPro engine of iris without the board's host side.
//!
//! The modules are the files under `src/dev/odyssey`, mounted in place and
//! under the paths iris gives them, so the files build unchanged here and in
//! iris. What they reach in the rest of iris is a small stand-in here: the
//! guest memory trait, locks, display types and the interrupt target.

// The engine is a part of a board: much of what the mounted files define
// is reached only through the board.
#![allow(dead_code, unused_imports, private_interfaces)]

pub use sgi_gpu_core as wgpu;

#[path = "../../../src/perfetto_macros.rs"]
mod perfetto_macros;
pub use perfetto_macros::{PfBits, PfLevel, PfValue};

pub mod devlog;
pub mod display;
pub mod ffi;
pub mod gfifo;
pub mod gfx_display;
pub mod ip30;
pub mod irq;
pub mod locks;
pub mod physical;
pub mod snapshot;
pub mod traits;

#[path = "../../../src/tile_pool.rs"]
pub mod tile_pool;

pub mod dev;
