//! The InfiniteReality (KONA) graphics pipe's front end: the host interface.
//!
//! Ported from hw/misc/sgi-kona.c in jamesbraid/qemu-sgi-origami.

mod attach;
mod consts;
mod flash;
mod frame;
mod front;
mod ident;
mod tables;

pub use attach::{FifoLow, HostDma, InterruptPacket, KonaAttach, PacketSender};
pub use flash::FLASH_SIZE;
pub use ident::KonaConfig;
