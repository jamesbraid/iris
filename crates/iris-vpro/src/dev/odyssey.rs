//! The files of `src/dev/odyssey` the engine is made of, in place: command
//! decode to the raster back end and the scan-out compose. Not here: the
//! processor-facing board (`mod.rs`), the refresh thread's window
//! (`display.rs`), the monitor and the snapshot serialiser.

#[path = "../../../../src/dev/odyssey/bits.rs"]
pub mod bits;
#[path = "../../../../src/dev/odyssey/consts.rs"]
pub mod consts;
#[path = "../../../../src/dev/odyssey/field.rs"]
pub mod field;
#[path = "../../../../src/dev/odyssey/vram.rs"]
pub mod vram;
#[path = "../../../../src/dev/odyssey/view.rs"]
pub mod view;
#[path = "../../../../src/dev/odyssey/logic.rs"]
pub mod logic;
#[path = "../../../../src/dev/odyssey/depth_word.rs"]
pub mod depth_word;
#[path = "../../../../src/dev/odyssey/clip.rs"]
pub mod clip;
#[path = "../../../../src/dev/odyssey/assembly.rs"]
pub mod assembly;
#[path = "../../../../src/dev/odyssey/gpu_unit.rs"]
pub mod gpu_unit;
#[path = "../../../../src/dev/odyssey/pipe/mod.rs"]
pub mod pipe;
#[path = "../../../../src/dev/odyssey/wgpu/mod.rs"]
pub mod wgpu;

#[path = "../../../../src/dev/odyssey/board.rs"]
pub mod board;
#[path = "../../../../src/dev/odyssey/command.rs"]
pub mod command;
#[path = "../../../../src/dev/odyssey/context.rs"]
pub mod context;
#[path = "../../../../src/dev/odyssey/copy.rs"]
pub mod copy;
#[path = "../../../../src/dev/odyssey/dma.rs"]
pub mod dma;
#[path = "../../../../src/dev/odyssey/draw.rs"]
pub mod draw;
#[path = "../../../../src/dev/odyssey/fifo.rs"]
pub mod fifo;
#[path = "../../../../src/dev/odyssey/frame.rs"]
pub mod frame;
#[path = "../../../../src/dev/odyssey/geometry.rs"]
pub mod geometry;
#[path = "../../../../src/dev/odyssey/gl.rs"]
pub mod gl;
#[path = "../../../../src/dev/odyssey/interface.rs"]
pub mod interface;
#[path = "../../../../src/dev/odyssey/mmio.rs"]
pub mod mmio;
#[path = "../../../../src/dev/odyssey/pixel_dma.rs"]
pub mod pixel_dma;
#[path = "../../../../src/dev/odyssey/pixel_write.rs"]
pub mod pixel_write;
#[path = "../../../../src/dev/odyssey/raster_unit.rs"]
pub mod raster_unit;
#[path = "../../../../src/dev/odyssey/reader.rs"]
pub mod reader;
#[path = "../../../../src/dev/odyssey/record.rs"]
pub mod record;
#[path = "../../../../src/dev/odyssey/registers.rs"]
pub mod registers;
#[path = "../../../../src/dev/odyssey/resolve.rs"]
pub mod resolve;
#[path = "../../../../src/dev/odyssey/scanout.rs"]
pub mod scanout;
#[path = "../../../../src/dev/odyssey/trace.rs"]
pub mod trace;
#[path = "../../../../src/dev/odyssey/worker.rs"]
pub mod worker;
#[path = "../../../../src/dev/odyssey/display_fifo.rs"]
pub mod display_fifo;

pub mod back;
pub mod engine;

/// Carries a board interrupt packet into a machine's crosstalk fabric: the
/// destination widget, the address within it, the vector, and whether the
/// source rose or fell. iris's `Odyssey` defines the same type.
pub type PacketSender = std::sync::Arc<dyn Fn(u8, u64, u8, bool) -> bool + Send + Sync>;
