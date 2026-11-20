//! `src/disp_timing.rs`, the part of iris's `disp` the scan-out uses.

#[path = "../../../src/disp_timing.rs"]
mod timing;

pub use timing::{decode_vc2_timings, FB_MAX_H, FB_STRIDE};
