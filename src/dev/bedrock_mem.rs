//! The Fuel's fitted memory: two 256 MiB banks owned by `Physical`, with
//! the bank decode selected by Bedrock's memory configuration register.
//!
//! This population follows the QEMU model; it is not a measured Fuel
//! topology. Processor and DMA accesses to backing memory pass through MD's
//! checks. The private cached startup window bypasses RAM during initialization;
//! there is no direct CPU window onto the backing banks.
//!
//! Ported from hw/misc/sgi-bedrock-md.c in jamesbraid/qemu-sgi-origami.

use crate::physical::{Physical, RamBank};
use crate::traits::*;
use std::sync::Arc;

pub const BANK_SPAN: u64 = 1 << 30;
pub const FITTED_BANK: u64 = 256 << 20;

pub struct BedrockMemory {
    physical: Arc<Physical>,
}
impl BedrockMemory {
    pub fn new() -> Self {
        Self {
            physical: Physical::new(vec![RamBank::new(256), RamBank::new(256)]),
        }
    }
    pub fn physical(&self) -> &Arc<Physical> {
        &self.physical
    }
    pub(crate) fn decode(config: u64, addr: u64, bytes: u64) -> Option<(usize, u64)> {
        let software = addr / BANK_SPAN;
        if software >= 8 {
            return None;
        }
        let bank = software ^ ((config >> 32) & 3);
        let field = config >> (bank / 2 * 8);
        let code = (field >> 4) & 15;
        if bank >= 2 || field & (1 << (bank & 1)) == 0 || !(1..=5).contains(&code) {
            return None;
        }
        let limit = FITTED_BANK.min((32 << 20) << code);
        let offset = addr % BANK_SPAN;
        (offset < limit && bytes <= limit - offset).then_some((bank as usize, offset))
    }
    pub(crate) fn raw_read(&self, bank: usize, at: u64) -> u64 {
        self.physical.banks()[bank].read64(at).data
    }
    pub(crate) fn raw_write(&self, bank: usize, at: u64, value: u64) -> u32 {
        self.physical.banks()[bank].write64(at, value)
    }
    pub(crate) fn zero(&self, bank: usize, size: u64) {
        assert!(size <= FITTED_BANK);
        // The bank owns this contiguous mapping; zero has the same representation
        // in host word order. MD serializes accesses and regenerates check bytes.
        unsafe {
            std::ptr::write_bytes(self.physical.banks()[bank].host_ptr(), 0, size as usize);
        }
        #[cfg(feature = "jitv2")]
        self.physical.banks()[bank].bump_gen_all();
    }
    pub(crate) fn changed_decode(&self) {
        #[cfg(feature = "jitv2")]
        for bank in self.physical.banks() {
            bank.bump_gen_all();
        }
    }
}
