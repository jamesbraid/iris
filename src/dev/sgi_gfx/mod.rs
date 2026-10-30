//! The shared SGI graphics foundation the VPro (Odyssey) board is built on:
//! board memory and planes, the scanout, pixel DMA, the GL context and the
//! scalar rasteriser.
//!
//! Ported from hw/display/sgi/ and include/hw/display/sgi/ in
//! jamesbraid/qemu-sgi-origami. Each file is a port of the C file of the
//! same name and keeps its names, so the two can be read side by side.
//! Everything is re-exported flat, as the C headers were one namespace.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, clippy::all)]

pub mod board_memory;
pub mod cutil;
pub mod gl_context;
pub mod head_ops;
pub mod host;
pub mod pi_gfx;
pub mod pixel_dma;
pub mod rasterizer;
pub mod rasterizer2;
pub mod rasterizer_fixed;
pub mod renderer_types;
pub mod scanout;
pub mod x_gc;

pub use board_memory::*;
pub use cutil::*;
pub use gl_context::*;
pub use head_ops::*;
pub use host::*;
pub use pi_gfx::*;
pub use pixel_dma::*;
pub use rasterizer::*;
pub use rasterizer2::*;
pub use rasterizer_fixed::*;
pub use renderer_types::*;
pub use scanout::*;
pub use x_gc::*;
