//! The SGI VPro (Odyssey) graphics board in the Octane's graphics port.
//!
//! Ported from hw/misc/sgi-odyssey.c and include/hw/misc/sgi-odyssey.h
//! in jamesbraid/qemu-sgi-origami. The C file is split here
//! by its line ranges, keeping its function names, so the two can be read
//! side by side:
//!
//! - `state`: include/hw/misc/sgi-odyssey.h, the board's state.
//! - `defs`: the register, command and field constants.
//! - `regs`: identity, registers, interrupts, the board NIC, the cursor,
//!   the back end's register file.
//! - `draw`: context save, buffers, the scanout hooks, integer drawing,
//!   off-screen objects and the image path.
//! - `gl`: the GL fragment helpers, texturing and GL draw state.
//! - `glprog`: the geometry program, lighting and the GL primitives.
//! - `dma`: pixel DMA, queries and fences.
//! - `cmd`: the command-FIFO interpreter.
//! - `front`: the host interface under its own lock: the registers that
//!   do not wait for the board's work, interrupts and the command queue.
//! - `device`: the work queue, MMIO, reset and construction.
//! - `worker`: the board's thread, which runs the queued stores.
//! - `xio`: the board as an iris widget: the bus, HEART and the display.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, clippy::all)]

pub use crate::dev::sgi_gfx::*;

use crate::devlog::LogModule;

pub mod cmd;
pub mod defs;
pub mod device;
pub mod dma;
pub mod draw;
pub mod front;
pub mod gl;
pub mod glprog;
pub mod regs;
pub mod state;
pub mod worker;
pub mod xio;

pub use cmd::*;
pub use defs::*;
pub use device::*;
pub use dma::*;
pub use draw::*;
pub use front::*;
pub use gl::*;
pub use glprog::*;
pub use regs::*;
pub use state::*;
pub use worker::*;
pub use xio::*;
