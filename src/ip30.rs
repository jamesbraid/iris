//! The SGI Octane (IP30): HEART, the Crossbow, the onboard Bridge and the
//! machine that composes them with IOC3, the ISP1040s and IMPACT graphics.
//!
//! The device models are ports of James Braid's Origami project models
//! (https://github.com/jamesbraid/origami), from the QEMU sgi-origami branch
//! at https://github.com/jamesbraid/qemu-sgi-origami, BSD-3-Clause, with
//! the same behaviour and approximations.

pub mod xio;
pub mod bridge;
pub mod flash;
