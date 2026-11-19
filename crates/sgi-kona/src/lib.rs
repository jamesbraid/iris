//! The KONA engine of iris without the board: the host interface front end,
//! the token layer, raster, store and the X and GL pixel operations, with
//! the headless display transport the replay checks need.
//!
//! The modules are the files under `src/dev/kona`, mounted in place and
//! under the paths iris gives them, so the files build unchanged here and in
//! iris. What they name outside `dev/kona` is either mounted in place too
//! (`gfx_display`, `chunk_store`, `snapshot`, `dev::w1`, `tile_pool`) or a small stand-in
//! (`irq`, `traits`, `physical`, `disp`, `cpu`, `hostinput`) for a part of
//! iris that is the machine's, not the engine's.
#![allow(dead_code, unused_imports, private_interfaces)]

pub use sgi_gpu_core as wgpu;

pub mod cpu;
pub mod dev;
pub mod devlog;
pub mod ffi;
pub mod disp;
pub mod display;
pub mod status;
pub mod hostinput;
pub mod irq;
pub mod physical;
pub mod traits;

#[path = "../../../src/gfx_display.rs"]
pub mod gfx_display;
#[path = "../../../src/chunk_store.rs"]
pub mod chunk_store;
#[path = "../../../src/snapshot.rs"]
pub mod snapshot;
#[path = "../../../src/tile_pool.rs"]
pub mod tile_pool;

/// The device the store and the scan-out may draw on.
pub type SharedGpu = std::sync::Arc<wgpu::Gpu>;
