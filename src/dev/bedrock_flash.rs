// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) 2026 James Braid
//! Ported from hw/misc/sgi-bedrock-flash.c and hw/mips/sgi/initial-flash.c
//! in jamesbraid/qemu-sgi-origami.
//!
//! Two independent 8 MiB positions, selected 32 MiB apart. Both boot-space
//! aliases and executable views share these bytes. This implementation exposes
//! read-array mode only. Command/program stores fail observably. AMD commands,
//! CFI query responses and erase timing await a complete reviewed contract.

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn label_and_configuration() {
        let seed = vec![0x21; 0xb0];
        let f = Flash::new(&seed).unwrap();
        let bytes = f.contents();
        assert_eq!(sum(&bytes[0x60..0xb0]), sum(&seed[0x60..0xb0]));
        assert_eq!(&bytes[0x64..0x68], &0x549aa985u32.to_be_bytes());
        assert_eq!(&bytes[0x74..0x78], &500_000_000u32.to_be_bytes());
        assert_eq!(&bytes[0x90..0x94], &2u32.to_be_bytes());
        let label = PART_SIZE + 0x1e0000;
        assert_eq!(&bytes[label..label + 16], &[0xff; 16]);
        assert_eq!(
            &bytes[label + 16..label + 28],
            &[0x50, 0x4c, 0x4f, 0x49, 0, 0, 0, 1, 0, 0, 0, 1]
        );
        assert!(bytes[label + 28..].iter().all(|b| *b == 0xff));
        assert!(Flash::new(&vec![0; PART_SIZE + 1]).is_err());
    }
    #[test]
    fn views_and_bounds() {
        let f = Arc::new(Flash::new(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap());
        let a = f.boot_view();
        let b = f.boot_view();
        assert_eq!(a.read64(0).data, 0x0102030405060708);
        assert_eq!(a.read8(1).data, 2);
        assert_eq!(a.read16(2).data, 0x0304);
        assert_eq!(a.read32(4).data, 0x05060708);
        assert_eq!(a.read64(SECOND_POSITION).data, u64::MAX);
        assert_eq!(b.read64(0).data, a.read64(0).data);
        assert!(!a.read8(PART_SIZE as u64).is_ok());
        assert!(!a.read64(PART_SIZE as u64 - 4).is_ok());
        assert!(!a.read8(SECOND_POSITION + PART_SIZE as u64).is_ok());
        let x = f.array_view(0, PART_SIZE).unwrap();
        assert_eq!(x.read64(0).data, a.read64(0).data);
        assert!(!x.read8(PART_SIZE as u64).is_ok());
        assert_eq!(a.write64(8 * 0x555, 0xaa), BUS_ERR);
        assert_eq!(a.write16(0, 0), BUS_ERR);
        assert_eq!(f.generation(), 0);
    }
    #[test]
    fn backing_size_and_seed_precedence() {
        let path = std::env::temp_dir().join(format!("iris-bedrock-store-{}", std::process::id()));
        let mut bytes = vec![0x5a; FLASH_SIZE];
        bytes[PART_SIZE] = 0xc7;
        std::fs::write(&path, &bytes).unwrap();
        let f = Flash::with_file(&[0x11], &path).unwrap();
        assert_eq!(f.contents()[0], 0x5a);
        assert_eq!(f.contents()[PART_SIZE], 0xc7);
        assert_eq!(std::fs::read(&path).unwrap(), f.contents());
        drop(f);
        std::fs::write(&path, [1, 2, 3]).unwrap();
        assert!(Flash::with_file(&[], &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), [1, 2, 3]);
        std::fs::write(&path, []).unwrap();
        let g = Flash::with_file(&[0x34], &path).unwrap();
        assert_eq!(g.contents()[0], 0x34);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), FLASH_SIZE as u64);
        drop(g);
        std::fs::remove_file(path).unwrap();
    }
}

use crate::traits::{BusDevice, BusRead16, BusRead32, BusRead64, BusRead8, Resettable, BUS_ERR};
use parking_lot::Mutex;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

pub const PART_SIZE: usize = 8 << 20;
pub const FLASH_SIZE: usize = 2 * PART_SIZE;
pub const SECOND_POSITION: u64 = 32 << 20;

fn sum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |s, b| s.wrapping_add(*b))
}

/// Apply Fuel's firmware record fields while retaining the configuration sum.
/// This policy also applies when reopening an existing persistent store.
fn configure(data: &mut [u8]) {
    let config = &mut data[0x60..0xb0];
    let old = sum(config);
    config[0x34..0x38].fill(0);
    config[4..8].copy_from_slice(&0x549aa985u32.to_be_bytes());
    config[0x14..0x18].copy_from_slice(&500_000_000u32.to_be_bytes());
    config[0x30..0x34].copy_from_slice(&2u32.to_be_bytes());
    config[0x38..0x3c].copy_from_slice(&1u32.to_be_bytes());
    let adjustment = old.wrapping_sub(sum(config));
    config[0x34..0x38].copy_from_slice(&(adjustment as u32).to_be_bytes());
}

pub struct Flash {
    data: Mutex<Vec<u8>>,
    gen: AtomicU64,
}

impl Flash {
    /// Format a fresh two-position store from a decoded payload.
    pub fn new(payload: &[u8]) -> io::Result<Self> {
        if payload.len() > PART_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fuel PROM payload exceeds 8 MiB",
            ));
        }
        let mut data = vec![0xff; FLASH_SIZE];
        data[..payload.len()].copy_from_slice(payload);
        let label = PART_SIZE + 0x1e0000;
        data[label + 0x10..label + 0x14].copy_from_slice(&0x504c4f49u32.to_be_bytes());
        data[label + 0x14..label + 0x18].copy_from_slice(&1u32.to_be_bytes());
        data[label + 0x18..label + 0x1c].copy_from_slice(&1u32.to_be_bytes());
        configure(&mut data);
        Ok(Self {
            data: Mutex::new(data),
            gen: AtomicU64::new(0),
        })
    }

    /// Load an exact 16 MiB store, or seed a missing/empty one. Wrong sizes are
    /// refused without writes. Reopening reapplies Fuel's configuration policy.
    /// Guest programming is unsupported; only initialization writes this file.
    pub fn with_file(payload: &[u8], path: impl AsRef<Path>) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let len = file.metadata()?.len();
        if len != 0 && len != FLASH_SIZE as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fuel flash store must be exactly 16 MiB (or empty)",
            ));
        }
        let flash = if len == 0 {
            Self::new(payload)?
        } else {
            let mut data = vec![0; FLASH_SIZE];
            file.read_exact(&mut data)?;
            configure(&mut data);
            Self {
                data: Mutex::new(data),
                gen: AtomicU64::new(0),
            }
        };
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&flash.data.lock())?;
        Ok(flash)
    }

    pub fn contents(&self) -> Vec<u8> {
        self.data.lock().clone()
    }
    pub fn generation(&self) -> u64 {
        self.gen.load(Ordering::Acquire)
    }
    pub fn reset(&self) { /* Read-array mode has no volatile command state yet. */
    }
    pub fn register_locks(self: &Arc<Self>) {
        let me = self.clone();
        crate::locks::register_lock_fn("bedrock_flash::data", move || me.data.is_locked());
    }
    /// Use the same shared flash for local and remote boot spaces.
    pub fn boot_view(self: &Arc<Self>) -> FlashView {
        FlashView {
            flash: self.clone(),
            range: None,
        }
    }
    /// A contiguous executable alias. Offsets refer to the 16 MiB backing.
    pub fn array_view(self: &Arc<Self>, offset: usize, len: usize) -> io::Result<FlashView> {
        if offset.checked_add(len).is_none_or(|end| end > FLASH_SIZE) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fuel flash alias exceeds backing",
            ));
        }
        Ok(FlashView {
            flash: self.clone(),
            range: Some((offset, len)),
        })
    }
}
impl Resettable for Flash {
    fn power_on(&self) {
        self.reset();
    }
}

pub struct FlashView {
    flash: Arc<Flash>,
    range: Option<(usize, usize)>,
}
impl FlashView {
    fn offset(&self, addr: u64, width: usize) -> Option<usize> {
        if let Some((base, len)) = self.range {
            let off = usize::try_from(addr).ok()?;
            return off
                .checked_add(width)
                .filter(|end| *end <= len)
                .map(|_| base + off);
        }
        let (base, within) = if addr < PART_SIZE as u64 {
            (0, addr)
        } else if addr >= SECOND_POSITION {
            (PART_SIZE, addr - SECOND_POSITION)
        } else {
            return None;
        };
        let within = usize::try_from(within).ok()?;
        within
            .checked_add(width)
            .filter(|end| *end <= PART_SIZE)
            .map(|_| base + within)
    }
    fn read(&self, addr: u64, width: usize) -> Option<u64> {
        let off = self.offset(addr, width)?;
        let bytes = self.flash.data.lock();
        Some(
            bytes[off..off + width]
                .iter()
                .fold(0, |v, b| (v << 8) | *b as u64),
        )
    }
    fn refuse(&self, addr: u64, width: usize, value: u64) -> u32 {
        log::warn!("bedrock-flash: unsupported {width}-byte store {value:#x} at {addr:#x}; command engine absent");
        BUS_ERR
    }
}
impl BusDevice for FlashView {
    fn read8(&self, a: u64) -> BusRead8 {
        self.read(a, 1)
            .map_or_else(BusRead8::err, |v| BusRead8::ok(v as u8))
    }
    fn read16(&self, a: u64) -> BusRead16 {
        self.read(a, 2)
            .map_or_else(BusRead16::err, |v| BusRead16::ok(v as u16))
    }
    fn read32(&self, a: u64) -> BusRead32 {
        self.read(a, 4)
            .map_or_else(BusRead32::err, |v| BusRead32::ok(v as u32))
    }
    fn read64(&self, a: u64) -> BusRead64 {
        self.read(a, 8).map_or_else(BusRead64::err, BusRead64::ok)
    }
    fn write8(&self, a: u64, v: u8) -> u32 {
        self.refuse(a, 1, v as u64)
    }
    fn write16(&self, a: u64, v: u16) -> u32 {
        self.refuse(a, 2, v as u64)
    }
    fn write32(&self, a: u64, v: u32) -> u32 {
        self.refuse(a, 4, v as u64)
    }
    fn write64(&self, a: u64, v: u64) -> u32 {
        self.refuse(a, 8, v)
    }
    #[cfg(feature = "jitv2")]
    fn gen_ptr(&self, _addr: u64) -> *const AtomicU64 {
        &self.flash.gen
    }
}
