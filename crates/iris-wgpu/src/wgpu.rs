//! `src/wgpu` of the iris crate, minus `present`, with `mod.rs` mirrored here.

#[path = "../../../src/wgpu/batch.rs"]
pub mod batch;
#[path = "../../../src/wgpu/device.rs"]
pub mod device;
#[path = "../../../src/wgpu/host.rs"]
pub mod host;
#[path = "../../../src/wgpu/memory.rs"]
pub mod memory;
#[path = "../../../src/wgpu/scanout.rs"]
pub mod scanout;
#[path = "../../../src/wgpu/shader.rs"]
pub mod shader;
#[path = "../../../src/wgpu/tiles.rs"]
pub mod tiles;

/// Library fragments a board's shaders may `#include`.
pub mod library {
    use super::shader::Fragment;

    pub const I64: Fragment = Fragment::new("i64.wgsl", include_str!("../../../src/wgpu/shaders/i64.wgsl"));
    pub const FLOAT: Fragment = Fragment::new("float.wgsl", include_str!("../../../src/wgpu/shaders/float.wgsl"));
    pub const WIDE: Fragment = Fragment::new("wide.wgsl", include_str!("../../../src/wgpu/shaders/wide.wgsl"));
    pub const F64: Fragment = Fragment::new("f64.wgsl", include_str!("../../../src/wgpu/shaders/f64.wgsl"));
}

pub use batch::{ArchShaders, Batch, BatchPipelines, Full};
pub use device::{set_display, Gpu, GpuOptions};
pub use memory::{GpuMemory, Staging};
pub use scanout::{Scanout, ScanoutPipeline, ScanoutShaders};
pub use shader::{Fragment, Source};
pub use tiles::{TileBox, TileGrid, TILE_H, TILE_W};
