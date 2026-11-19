//! The files of `src/dev/kona` the engine is made of, in place: the host
//! interface front end, the token layer and what it drives. The board
//! (`mod.rs`: the thread, the ring, the bus) and its tests stay with iris.

#[path = "../../../../src/dev/kona/defs.rs"]
pub mod defs;
#[path = "../../../../src/dev/kona/dma.rs"]
pub mod dma;
#[path = "../../../../src/dev/kona/flash.rs"]
pub mod flash;
#[path = "../../../../src/dev/kona/frame.rs"]
pub mod frame;
#[path = "../../../../src/dev/kona/front.rs"]
pub mod front;
#[path = "../../../../src/dev/kona/handle.rs"]
pub mod handle;
#[path = "../../../../src/dev/kona/ident.rs"]
pub mod ident;
#[path = "../../../../src/dev/kona/job.rs"]
pub mod job;
#[path = "../../../../src/dev/kona/pipe.rs"]
pub mod pipe;
#[path = "../../../../src/dev/kona/regs.rs"]
pub mod regs;
#[path = "../../../../src/dev/kona/stream.rs"]
pub mod stream;
#[path = "../../../../src/dev/kona/tables.rs"]
pub mod tables;
#[path = "../../../../src/dev/kona/back/mod.rs"]
pub mod back;

pub mod api;
pub mod engine;

// What iris's `snapshot` manifest names, as iris's `mod.rs` re-exports it.
pub use back::snapshot::KonaChunk;
