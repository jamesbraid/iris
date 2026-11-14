//! The per-fragment features of the raster path as WGSL: fog, smoothed coverage, multisample
//! sample bits, the accumulation buffer and the line and polygon stipples.
//!
//! `fragment.wgsl` is a library for a raster kernel to `#include`; nothing includes it yet. Each
//! function there is the same single-precision arithmetic as its CPU counterpart in `raster` and
//! `gl::pixels`, and `tests` runs both over random inputs and compares the bits. The arithmetic
//! rules (a product kept apart from a sum, an exact divide, a saturating float to integer) are at
//! the top of the file.
//!
//! What stays for the kernel: the integer edge-function tests (the plain coverage rule, the inner
//! edges of a smoothed fan and the sample mask, which are 64-bit), the sample-position arithmetic
//! of `sample_mask`, the stencil and depth tests that decide which samples are live, and getting
//! the draw's constants to the shader.

use crate::wgpu::shader::Fragment;

/// The library fragment. A kernel adds `library::FLOAT` (the exact divide it includes) next to it
/// and sets the file's `fnz` to 0 from a run-time value before calling anything.
#[allow(dead_code)]
pub const FRAGMENT: Fragment = Fragment::new("fragment.wgsl", include_str!("fragment.wgsl"));

#[cfg(test)]
mod tests;
