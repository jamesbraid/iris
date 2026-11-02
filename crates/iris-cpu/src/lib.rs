#![allow(dead_code, unused_variables, unused_imports)]

// The layers below this one, re-exported at the crate root so `crate::config`,
// `crate::traits` and the rest resolve in code moved out of the iris crate.
#[macro_use]
extern crate iris_core;
pub use iris_core::config;
pub use iris_core::traits;
pub use iris_core::devlog;
pub use iris_core::ppmem;
pub use iris_core::memory_map;
pub use iris_core::ip30_map;
pub use iris_core::platform;
pub use iris_core::physical;
pub use iris_core::ip22_map;
pub use iris_core::locks;
pub use iris_core::irq;
pub use iris_core::gfifo;
pub use iris_core::trace_writer;
pub use iris_core::heartbeat;
pub use iris_core::screenshot;
pub use iris_core::exp;
pub use iris_core::snapshot;
pub use iris_core::sgi_vh;
pub use iris_core::elf;
pub use iris_core::chunk_store;
pub use iris_core::thread_affinity;
pub use iris_core::hptimer;
pub use iris_core::hptimer_tests;
#[cfg(feature = "perfetto")]
pub use iris_core::perfetto;
#[cfg(test)]
pub use iris_core::test_board;
pub use iris_core::{PfBits, PfLevel, PfValue};
pub use iris_core::{dlog, dlog_unconditional, dlog_dev, pf_instant, pf_begin, pf_end, pf_counter, pf_scope, pf_level, pf_bits, pf_value};
pub use iris_core::dev;

pub mod cpu;
pub mod gdb_stub;
