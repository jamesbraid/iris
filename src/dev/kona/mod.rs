//! The InfiniteReality (KONA) graphics pipe's front end: the host interface.
//!
//! Ported from hw/misc/sgi-kona.c, sgi-kona-ports.c, sgi-kona-dma.c and
//! sgi-command-port.c in jamesbraid/qemu-sgi-origami.

mod attach;
mod consts;
mod dma;
mod flash;
mod frame;
mod front;
mod ident;
mod job;
mod pipe;
mod tables;
#[cfg(test)]
mod tests;

pub use attach::{FifoLow, HostDma, InterruptPacket, KonaAttach, PacketSender};
pub use flash::FLASH_SIZE;
pub use ident::KonaConfig;
pub use job::{Backend, BackPort, ContextEvent, Grammar, Job, NullBackend, Reply};

use front::Host;
use pipe::Core;
