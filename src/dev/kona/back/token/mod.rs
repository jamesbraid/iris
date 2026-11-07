//! The commands that say what to draw.
//!
//! `textport`: the PROM's text console. `gc`, `x` and `xfill`: the X server's
//! graphics context, drawing state and fills.

pub mod gc;
pub mod textport;
pub mod x;
pub mod xfill;
