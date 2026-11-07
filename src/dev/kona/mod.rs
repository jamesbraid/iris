//! The InfiniteReality (KONA) graphics pipe's front end: the host interface.
//!
//! Ported from hw/misc/sgi-kona.c in jamesbraid/qemu-sgi-origami.

mod consts;
mod frame;
mod ident;
mod tables;

pub use ident::KonaConfig;
