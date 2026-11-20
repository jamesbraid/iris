//! The files of `src/dev/mgras` the engine is made of, in place: the HQ3
//! command parser, the GE11 stand-in, the raster engine, the texture
//! engine and the display side: the display control bus and the scan-out.

#[path = "../../../../src/dev/mgras/dcb.rs"]
pub mod dcb;
#[path = "../../../../src/dev/mgras/fixed.rs"]
pub mod fixed;
#[path = "../../../../src/dev/mgras/frame.rs"]
pub mod frame;
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
#[cfg(test)]
#[path = "../../../../src/dev/mgras/test_gen.rs"]
pub mod test_gen;
#[path = "../../../../src/dev/mgras/wgpu/mod.rs"]
pub mod wgpu;

pub use gl::Gl;

pub mod back;
pub mod display;
pub mod engine;

#[cfg(test)]
mod test_board;
#[cfg(test)]
pub use test_board::{record, Mgras};
#[cfg(test)]
#[path = "../../../../src/dev/mgras/mgras_tests.rs"]
mod mgras_tests;
