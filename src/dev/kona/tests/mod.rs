//! Register-contract tests of the pipe's front end.
//!
//! The cases are ported from the QEMU model's qtests (bring-up, config,
//! EEPROM and DMA); a doc comment names the case a test comes from.

use super::*;

mod host;
pub(super) use host::{attach, TestHost};
