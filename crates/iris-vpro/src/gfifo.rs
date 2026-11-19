//! iris's `src/gfifo.rs`, the lock-free ring between a processor and a
//! graphics board's thread, in place.

#[path = "../../../src/gfifo.rs"]
mod gfifo;

pub use gfifo::*;
