//! The InfiniteReality (KONA) graphics pipe's front end: the host interface.
//!
//! Ported from hw/misc/sgi-kona.c in jamesbraid/qemu-sgi-origami.

mod consts;
mod flash;
mod frame;
mod ident;
mod tables;

pub use flash::FLASH_SIZE;
pub use ident::KonaConfig;
