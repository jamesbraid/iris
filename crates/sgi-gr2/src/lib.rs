//! The GR2 (Express) engine of iris without the board's machine side.
//!
//! The modules are the files under `src/dev/gr2`, mounted in place and under
//! the paths iris gives them, so the files build unchanged here and in iris.
//! What they reach in the rest of iris is a small stand-in here: the bus
//! result types, the display types and the log macros. The board's retrace
//! and display thread, its monitor commands and the TOML snapshot stay with
//! iris; the shell paces the retrace and composes through `BoardEngine`.
#![allow(dead_code, unused_imports, private_interfaces, unexpected_cfgs)]

#[path = "../../../src/perfetto_macros.rs"]
mod perfetto_macros;
pub use perfetto_macros::{PfBits, PfLevel, PfValue};

pub mod devlog;
pub mod disp;
pub mod gfifo;
pub mod gfx_display;
pub mod jit;
pub mod traits;

#[cfg(test)]
#[path = "../../../src/test_board.rs"]
mod test_board;
#[cfg(test)]
#[path = "../../../src/test_frame_pin.rs"]
mod test_frame_pin;

pub mod dev;
pub mod ffi;

/// The GPU scan-out handle iris's board takes. This crate composes on the CPU, so none exists.
pub enum SharedGpu {}
