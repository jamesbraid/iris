//! The commands that say what to draw.
//!
//! `textport`: the PROM's text console. `gc` and `x`: the X server's graphics
//! context and drawing state.

pub mod gc;
pub mod textport;
pub mod x;
