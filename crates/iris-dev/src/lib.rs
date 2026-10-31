#![allow(dead_code, unused_variables, unused_imports)]

// The layers below this one, re-exported at the crate root so `crate::config`,
// `crate::traits` and the rest keep resolving in code moved out of the iris crate.
#[macro_use]
extern crate iris_core;
pub use iris_core::{config, traits, devlog, ppmem, platform, physical, locks, irq, gfifo, trace_writer, heartbeat, screenshot, exp, snapshot, sgi_vh, elf, chunk_store, thread_affinity, hptimer, hptimer_tests, ip22_map, ip30_map, PfBits, PfLevel, PfValue};
pub use iris_core::{dlog, dlog_unconditional, dlog_dev, pf_instant, pf_begin, pf_end, pf_counter, pf_scope, pf_level, pf_bits, pf_value};
#[cfg(feature = "perfetto")]
pub use iris_core::perfetto;
#[cfg(test)]
pub use iris_core::test_board;
pub use iris_core::cpu;

pub mod dev;
pub mod prombin;
pub mod prombini2;
pub mod prombinip28;
pub mod net;
pub mod cow_disk;
pub mod chd_disk;
pub mod block_node;
pub mod scsi;
pub mod scsi_monitor;
pub mod host_audio;
pub mod pci;
pub mod video_source;
pub mod ultra_proto;
pub mod ip32_map;
mod nv_storage;
