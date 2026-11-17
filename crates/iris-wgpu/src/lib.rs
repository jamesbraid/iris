//! The wgpu layer of iris without the rest of iris.
//!
//! The modules are the files under `src/wgpu`, mounted in place so iris and
//! this crate build the same code. `present` (window presentation) is left
//! out; `mod.rs` is mirrored below without it.

pub mod wgpu;
