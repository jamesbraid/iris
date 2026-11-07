//! The commands that say what to draw.
//!
//! `textport`: the PROM's text console. `gc`, `x`, `xfill` and `stipple`: the X
//! server's graphics context, drawing state, fills and stipple cache.

pub mod gc;
pub mod stipple;
pub mod textport;
pub mod x;
pub mod xfill;
