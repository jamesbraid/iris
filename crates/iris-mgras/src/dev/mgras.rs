//! The files of `src/dev/mgras` the engine is made of, in place: the HQ3
//! command parser, the GE11 stand-in, the raster engine and the texture
//! engine.

#[path = "../../../../src/dev/mgras/fixed.rs"]
pub mod fixed;
#[path = "../../../../src/dev/mgras/ge11.rs"]
pub mod ge11;
#[path = "../../../../src/dev/mgras/gl.rs"]
pub mod gl;
#[path = "../../../../src/dev/mgras/hostmem.rs"]
pub mod hostmem;
#[path = "../../../../src/dev/mgras/hq3.rs"]
pub mod hq3;
#[path = "../../../../src/dev/mgras/microcode.rs"]
pub mod microcode;
#[path = "../../../../src/dev/mgras/pixmem.rs"]
pub mod pixmem;
#[path = "../../../../src/dev/mgras/plain.rs"]
pub mod plain;
#[path = "../../../../src/dev/mgras/pixpipe.rs"]
pub mod pixpipe;
#[path = "../../../../src/dev/mgras/quirks.rs"]
pub mod quirks;
#[path = "../../../../src/dev/mgras/rss.rs"]
pub mod rss;
#[cfg(feature = "gr4-jit")]
#[path = "../../../../src/dev/mgras/rss_jit/mod.rs"]
pub mod rss_jit;
#[path = "../../../../src/dev/mgras/te1.rs"]
pub mod te1;
#[path = "../../../../src/dev/mgras/wgpu/mod.rs"]
pub mod wgpu;

pub use gl::Gl;

pub mod back;
pub mod engine;
