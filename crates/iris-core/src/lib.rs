#![allow(dead_code, unused_variables, unused_imports)]

pub mod config;
#[macro_use]
pub mod devlog;
pub mod traits;
pub mod ppmem;
pub mod platform;
pub mod physical;
pub mod locks;
pub mod irq;
pub mod gfifo;
pub mod trace_writer;
pub mod heartbeat;
pub mod screenshot;
pub mod exp;
pub mod snapshot;
pub mod sgi_vh;
pub mod elf;
pub mod chunk_store;
pub mod thread_affinity;
pub mod hptimer;
pub mod hptimer_tests;
pub mod ip22_map;
pub mod ip30_map;
pub mod cpu;
pub mod dev;
#[macro_use]
mod perfetto_macros;
pub use perfetto_macros::{PfBits, PfLevel, PfValue};
#[cfg(feature = "perfetto")]
pub mod perfetto;
#[cfg(any(test, feature = "test-support"))]
pub mod test_board;
