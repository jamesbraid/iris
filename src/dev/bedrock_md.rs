//! Bedrock memory/directory registers and local memory operations.
//!
//! Ported from hw/misc/sgi-bedrock-md.c in jamesbraid/qemu-sgi-origami.
//! SPDX-License-Identifier: BSD-3-Clause
//!
//! CPU coherence requests and hardware protection enforcement are not implemented.
//! Protection carriers retain opaque words, as in the donor. Unsupported operations
//! return a bus error and are counted, rather than reporting diagnostic success.

use super::bedrock_mem::{BedrockMemory, BANK_SPAN, FITTED_BANK};
use crate::traits::*;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

const CONFIG_MASK: u64 = 0xfbfbfbfb | (3 << 32) | (15 << 44) | (63 << 48) | (7 << 56) | (3 << 60);
const ECC_COLUMNS: [u8; 72] = [
    0x13, 0x23, 0x43, 0x83, 0x2f, 0xf1, 0x0d, 0x07, 0xd0, 0x70, 0x4f, 0xf8, 0x61, 0x62, 0x64, 0x68,
    0x1c, 0x2c, 0x4c, 0x8c, 0x15, 0x25, 0x45, 0x85, 0x19, 0x29, 0x49, 0x89, 0x1a, 0x2a, 0x4a, 0x8a,
    0x51, 0x52, 0x54, 0x58, 0x91, 0x92, 0x94, 0x98, 0xa1, 0xa2, 0xa4, 0xa8, 0x31, 0x32, 0x34, 0x38,
    0x16, 0x26, 0x46, 0x86, 0x1f, 0xf2, 0x0b, 0x0e, 0xb0, 0xe0, 0x8f, 0xf4, 0xc1, 0xc2, 0xc4, 0xc8,
    1, 2, 4, 8, 16, 32, 64, 128,
];
pub fn ecc_compute(mut value: u64) -> u8 {
    let mut code = 0;
    while value != 0 {
        code ^= ECC_COLUMNS[value.trailing_zeros() as usize];
        value &= value - 1;
    }
    code
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryError {
    Correctable,
    Uncorrectable,
}
type ErrorCallback = Arc<dyn Fn(MemoryError) + Send + Sync>;

struct State {
    cell: u64,
    config: u64,
    refresh: u64,
    directory_config: u64,
    storage: HashMap<u64, u64>,
    directory: HashMap<u64, u64>,
    counters: HashMap<u64, u64>,
    protection: HashMap<u64, u64>,
    compat: HashMap<u64, u64>,
    ecc: HashMap<u64, u8>,
    admitted: [u16; 2],
    memory_error: u64,
    directory_error: u64,
    marked_directory: HashMap<u64, ()>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            cell: u64::MAX,
            config: 0,
            refresh: 0,
            directory_config: 0,
            storage: HashMap::new(),
            directory: HashMap::new(),
            counters: HashMap::new(),
            protection: HashMap::new(),
            compat: HashMap::new(),
            ecc: HashMap::new(),
            admitted: [0; 2],
            memory_error: 0,
            directory_error: 0,
            marked_directory: HashMap::new(),
        }
    }
}

pub struct BedrockMd {
    memory: BedrockMemory,
    state: Mutex<State>,
    callback: Mutex<Option<ErrorCallback>>,
    unsupported: AtomicU64,
}
#[derive(Clone, Copy)]
enum View {
    Registers,
    Memory,
    Dma,
    Directory,
    Fetchop,
    Ecc,
}
struct MdBus {
    md: Weak<BedrockMd>,
    view: View,
}

impl BedrockMd {
    pub fn new() -> Arc<Self> {
        let md = Arc::new(Self {
            memory: BedrockMemory::new(),
            state: Mutex::new(State::default()),
            callback: Mutex::new(None),
            unsupported: AtomicU64::new(0),
        });
        md.physical().map(
            0,
            8 * BANK_SPAN,
            md.view(View::Memory),
            "Bedrock RAM decoder",
        );
        md
    }
    pub fn physical(&self) -> &Arc<crate::physical::Physical> {
        self.memory.physical()
    }
    fn view(self: &Arc<Self>, view: View) -> Arc<dyn BusDevice> {
        Arc::new(MdBus {
            md: Arc::downgrade(self),
            view,
        })
    }
    pub fn md_bus(self: &Arc<Self>) -> Arc<dyn BusDevice> {
        self.view(View::Registers)
    }
    pub fn directory_bus(self: &Arc<Self>) -> Arc<dyn BusDevice> {
        self.view(View::Directory)
    }
    pub fn fetchop_bus(self: &Arc<Self>) -> Arc<dyn BusDevice> {
        self.view(View::Fetchop)
    }
    pub fn ecc_bus(self: &Arc<Self>) -> Arc<dyn BusDevice> {
        self.view(View::Ecc)
    }
    /// DMA shares decode and ECC. Hardware protection enforcement remains unsupported.
    pub fn dma_view(self: &Arc<Self>) -> Arc<dyn BusDevice> {
        self.view(View::Dma)
    }
    pub fn on_error(&self, callback: ErrorCallback) {
        *self.callback.lock() = Some(callback);
    }
    pub fn current_cell(&self) -> u64 {
        self.state.lock().cell
    }
    pub fn unsupported(&self) -> u64 {
        self.unsupported.load(Ordering::Relaxed)
    }
    fn refuse<T>(&self) -> Option<T> {
        self.unsupported.fetch_add(1, Ordering::Relaxed);
        None
    }
    pub fn soft_reset(&self) {
        let mut s = self.state.lock();
        s.cell = u64::MAX;
        s.storage.clear();
        s.memory_error = 0;
        s.directory_error = 0;
        s.ecc.clear();
        s.marked_directory.clear();
        s.refresh &= (1 << 63) | 0xfff;
    }
    fn access(&self, view: View, addr: u64, bytes: u64, write: Option<u64>) -> Option<u64> {
        if ![1, 2, 4, 8].contains(&bytes) || addr % bytes != 0 {
            return self.refuse();
        }
        let mut event = None;
        let result = {
            let mut s = self.state.lock();
            match view {
                View::Registers => self.register(&mut s, addr, bytes, write),
                View::Directory => self.directory(&mut s, addr, bytes, write),
                View::Ecc => self.ecc(&mut s, addr, bytes, write),
                View::Fetchop => self.fetchop(&mut s, addr, bytes, write, &mut event),
                View::Memory | View::Dma => self.ram(&mut s, addr, bytes, write, &mut event),
            }
        };
        if let Some(event) = event {
            let callback = self.callback.lock().clone();
            if let Some(callback) = callback {
                callback(event);
            }
        }
        result
    }
    fn register(&self, s: &mut State, at: u64, bytes: u64, write: Option<u64>) -> Option<u64> {
        if bytes != 8 {
            return self.refuse();
        }
        let old = match at {
            0 => s.cell,
            8 => s.config,
            0x60 => 0x001818180c0c0c00,
            0x20000 => s.refresh,
            0x40000 => s.directory_config,
            0x40050 => 0x04040400,
            0x60010 | 0x60018 => s.memory_error,
            0x40010 | 0x40018 => s.directory_error,
            _ if storage_mask(at).is_some() => *s.storage.get(&at).unwrap_or(&0),
            _ if idle(at) => 0,
            _ => return self.refuse(),
        };
        if let Some(value) = write {
            match at {
                0 => s.cell = value,
                8 => {
                    s.config = value & CONFIG_MASK;
                    self.memory.changed_decode();
                }
                0x20000 => s.refresh = value & ((1 << 63) | 0xfff),
                0x40000 => s.directory_config = value & 3,
                _ if storage_mask(at).is_some() => {
                    s.storage.insert(at, value & storage_mask(at).unwrap());
                }
                _ => return self.refuse(),
            }
        } else if at == 0x60018 {
            s.memory_error = 0;
        } else if at == 0x40018 {
            s.directory_error = 0;
        }
        Some(old)
    }
    fn directory(&self, s: &mut State, at: u64, bytes: u64, write: Option<u64>) -> Option<u64> {
        if bytes != 8 || at >= 1 << 30 {
            return self.refuse();
        }
        let software = at / (BANK_SPAN / 8);
        let bank = software ^ ((s.config >> 32) & 3);
        if at & 0x7c8 == 0x408 {
            if bank >= 2 {
                return Some(0);
            }
            let within =
                ((((at % (BANK_SPAN / 8)) >> 11) << 14) | (((at >> 4) & 3) << 7)) % FITTED_BANK;
            let key = ((bank * FITTED_BANK + within) >> 14) * 4 + ((within >> 7) & 3);
            let old = *s.protection.get(&key).unwrap_or(&0);
            if let Some(v) = write {
                s.protection.insert(key, v);
            }
            return Some(old);
        }
        let within = ((at % (BANK_SPAN / 8)) & !15) * 8;
        let field = s.config >> (bank / 2 * 8);
        let code = (field >> 4) & 15;
        if field & (1 << (bank & 1)) == 0
            || !(1..=5).contains(&code)
            || within >= (32 << 20) << code
        {
            return self.refuse();
        }
        let mask = if at & 8 != 0 || s.directory_config & 1 != 0 {
            u64::MAX
        } else {
            u32::MAX as u64
        };
        let table;
        let key;
        if bank >= 2 || within >= FITTED_BANK {
            if !s.compat.contains_key(&at) && s.compat.len() >= 74 {
                return self.refuse();
            }
            table = &mut s.compat;
            key = at;
        } else {
            key = (bank * FITTED_BANK + within) / 128;
            table = if at & 8 != 0 {
                &mut s.counters
            } else {
                &mut s.directory
            };
        }
        let old = *table.get(&key).unwrap_or(&0) & mask;
        if let Some(value) = write {
            let value = if at & 8 == 0 && s.directory_config & 2 == 0 {
                directory_encode(value, s.directory_config & 1 != 0)
            } else {
                value
            };
            table.insert(key, value & mask);
            if bank < 2 && within < FITTED_BANK && at & 8 == 0 {
                if directory_syndrome(value, s.directory_config & 1 != 0) != 0 {
                    s.marked_directory.insert(key, ());
                } else {
                    s.marked_directory.remove(&key);
                }
            }
        }
        Some(old)
    }
    fn checked_word(
        &self,
        s: &mut State,
        bank: usize,
        at: u64,
        address: u64,
        event: &mut Option<MemoryError>,
    ) -> u64 {
        let mut data = self.memory.raw_read(bank, at);
        let key = (bank as u64 * FITTED_BANK + at) / 8;
        if s.storage.get(&0x60000).copied().unwrap_or(0) & 1 == 0 {
            if let Some(&check) = s.ecc.get(&key) {
                let syndrome = check ^ ecc_compute(data);
                if syndrome != 0 {
                    let bit = ECC_COLUMNS.iter().position(|&v| v == syndrome);
                    let valid = if bit.is_some() { 1 << 52 } else { 1 << 60 };
                    if s.memory_error & (valid | (1 << 60)) != 0 {
                        s.memory_error |= valid << 1;
                    } else {
                        s.memory_error = (s.memory_error
                            & !((0xff << 40) | (((1u64 << 30) - 1) << 3)))
                            | ((syndrome as u64) << 40)
                            | (address & (((1u64 << 30) - 1) << 3));
                    }
                    s.memory_error |= valid;
                    if let Some(bit) = bit {
                        if bit < 64 {
                            data ^= 1 << bit;
                            self.memory.raw_write(bank, at, data);
                        }
                        s.ecc.remove(&key);
                        *event = Some(MemoryError::Correctable);
                    } else {
                        *event = Some(MemoryError::Uncorrectable);
                    }
                }
            }
        }
        data
    }
    fn ram(
        &self,
        s: &mut State,
        address: u64,
        bytes: u64,
        write: Option<u64>,
        event: &mut Option<MemoryError>,
    ) -> Option<u64> {
        let Some((bank, offset)) = BedrockMemory::decode(s.config, address, bytes) else {
            return self.refuse();
        };
        let line = (bank as u64 * FITTED_BANK + offset) / 128;
        if s.marked_directory.contains_key(&line) {
            let premium = s.directory_config & 1 != 0;
            let word = *s.directory.get(&line).unwrap_or(&0);
            if write.is_some() {
                s.directory.insert(line, directory_encode(word, premium));
                s.marked_directory.remove(&line);
            } else if s.directory_config & 2 == 0 {
                let syndrome = directory_syndrome(word, premium);
                if syndrome != 0 {
                    let bit = directory_columns(premium)
                        .iter()
                        .position(|&v| v == syndrome);
                    let valid = if bit.is_some() { 1 << 61 } else { 1 << 63 };
                    if s.directory_error & ((1 << 63) | valid) != 0 {
                        s.directory_error |= if bit.is_some() { 1 << 57 } else { 1 << 59 };
                    } else {
                        let carrier = 0x1c0000000
                            + (address / BANK_SPAN) * (BANK_SPAN / 8)
                            + ((offset & !127) / 8);
                        s.directory_error = (s.directory_error
                            & !((0x7f << 40) | (((1u64 << 30) - 1) << 3)))
                            | ((syndrome as u64) << 40)
                            | (carrier & (((1u64 << 30) - 1) << 3));
                    }
                    s.directory_error |= valid;
                    if let Some(bit) = bit {
                        s.directory.insert(line, word ^ (1 << bit));
                        s.marked_directory.remove(&line);
                        *event = Some(MemoryError::Correctable);
                    } else {
                        *event = Some(MemoryError::Uncorrectable);
                        return None;
                    }
                }
            }
        }
        let at = offset & !7;
        let shift = (8 - (offset & 7) - bytes) * 8;
        let mask = u64::MAX >> ((8 - bytes) * 8);
        if let Some(value) = write {
            let old = self.memory.raw_read(bank, at);
            self.memory.raw_write(
                bank,
                at,
                (old & !(mask << shift)) | ((value & mask) << shift),
            );
            s.ecc.remove(&((bank as u64 * FITTED_BANK + at) / 8));
            Some(0)
        } else {
            Some((self.checked_word(s, bank, at, address, event) >> shift) & mask)
        }
    }
    fn fetchop(
        &self,
        s: &mut State,
        at: u64,
        bytes: u64,
        write: Option<u64>,
        event: &mut Option<MemoryError>,
    ) -> Option<u64> {
        if bytes != 4 && bytes != 8 {
            return self.refuse();
        }
        let op = at & 0x38;
        if op > if write.is_some() { 32 } else { 24 } {
            return self.refuse();
        }
        let address = (at & !63) | (at & 4);
        let old = self.ram(s, address, bytes, None, event)?;
        let next = match (write, op) {
            (None, 0) => return Some(old),
            (Some(v), 0) => v,
            (_, 8) => old.wrapping_add(1),
            (_, 16) => old.wrapping_sub(1),
            (None, 24) => 0,
            (Some(v), 24) => old & v,
            (Some(v), 32) => old | v,
            _ => return self.refuse(),
        };
        self.ram(s, address, bytes, Some(next), event)?;
        Some(old)
    }
    fn ecc(&self, s: &mut State, at: u64, bytes: u64, write: Option<u64>) -> Option<u64> {
        if bytes != 1 && bytes != 2 {
            return self.refuse();
        }
        let software = at / (BANK_SPAN / 8);
        let within = (at % (BANK_SPAN / 8)) ^ if bytes == 1 { 7 } else { 6 };
        let address = software * BANK_SPAN + within * 8;
        let Some((bank, offset)) = BedrockMemory::decode(s.config, address, bytes * 8) else {
            if bytes == 2 && [0x04000006, 0x08000006].contains(&at) {
                let i = usize::from(at == 0x08000006);
                let old = s.admitted[i];
                if let Some(value) = write {
                    s.admitted[i] = value as u16;
                }
                return Some(old as u64);
            }
            return self.refuse();
        };
        let mut old = 0;
        for i in 0..bytes {
            let raw = offset + i * 8;
            let key = (bank as u64 * FITTED_BANK + raw) / 8;
            let computed = ecc_compute(self.memory.raw_read(bank, raw));
            old |= (s.ecc.get(&key).copied().unwrap_or(computed) as u64) << (i * 8);
            if let Some(value) = write {
                let code = (value >> (i * 8)) as u8;
                if code == computed {
                    s.ecc.remove(&key);
                } else {
                    s.ecc.insert(key, code);
                }
            }
        }
        Some(old)
    }
}
impl Resettable for BedrockMd {
    fn power_on(&self) {
        *self.state.lock() = State::default();
        self.memory.changed_decode();
    }
}
impl BusDevice for MdBus {
    fn read8(&self, a: u64) -> BusRead8 {
        self.md
            .upgrade()
            .and_then(|m| m.access(self.view, a, 1, None))
            .map_or(BusRead8::err(), |v| BusRead8::ok(v as u8))
    }
    fn read16(&self, a: u64) -> BusRead16 {
        self.md
            .upgrade()
            .and_then(|m| m.access(self.view, a, 2, None))
            .map_or(BusRead16::err(), |v| BusRead16::ok(v as u16))
    }
    fn read32(&self, a: u64) -> BusRead32 {
        self.md
            .upgrade()
            .and_then(|m| m.access(self.view, a, 4, None))
            .map_or(BusRead32::err(), |v| BusRead32::ok(v as u32))
    }
    fn read64(&self, a: u64) -> BusRead64 {
        self.md
            .upgrade()
            .and_then(|m| m.access(self.view, a, 8, None))
            .map_or(BusRead64::err(), BusRead64::ok)
    }
    fn write8(&self, a: u64, v: u8) -> u32 {
        self.md
            .upgrade()
            .and_then(|m| m.access(self.view, a, 1, Some(v as u64)))
            .map_or(BUS_ERR, |_| BUS_OK)
    }
    fn write16(&self, a: u64, v: u16) -> u32 {
        self.md
            .upgrade()
            .and_then(|m| m.access(self.view, a, 2, Some(v as u64)))
            .map_or(BUS_ERR, |_| BUS_OK)
    }
    fn write32(&self, a: u64, v: u32) -> u32 {
        self.md
            .upgrade()
            .and_then(|m| m.access(self.view, a, 4, Some(v as u64)))
            .map_or(BUS_ERR, |_| BUS_OK)
    }
    fn write64(&self, a: u64, v: u64) -> u32 {
        self.md
            .upgrade()
            .and_then(|m| m.access(self.view, a, 8, Some(v)))
            .map_or(BUS_ERR, |_| BUS_OK)
    }
}
fn storage_mask(at: u64) -> Option<u64> {
    Some(match at {
        0x10 => 0xff,
        0x18 => 0x3ff | (0xff << 16) | (1 << 32),
        0x10000 | 0x10008 => {
            3 | (15 << 4) | (127 << 8) | (15 << 16) | (0xffffffffff << 20) | (1 << 63)
        }
        0x10010 | 0x10018 => (1 << 41) - 1,
        0x20008 => 0xffffffffffff,
        0x20038 => 0xff,
        0x20060 => 3,
        0x40040 => 0x7fff | (7 << 32) | (7 << 35) | (1 << 63),
        0x40048 => 0x7fff | (7 << 32) | (7 << 35) | (3 << 62),
        0x60000 => 1,
        0x40060 => 3,
        0x60040 => 1,
        _ => return None,
    })
}
fn idle(at: u64) -> bool {
    matches!(
        at,
        0x20 | 0x28
            | 0x40
            | 0x48
            | 0x58
            | 0x20010
            | 0x20018
            | 0x20020
            | 0x20028
            | 0x20030
            | 0x20040
            | 0x20048
            | 0x40020
            | 0x40028
            | 0x40030
            | 0x40038
            | 0x40058
            | 0x60020
            | 0x60028
            | 0x60030
            | 0x60038
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn configured() -> Arc<BedrockMd> {
        let md = BedrockMd::new();
        assert_eq!(md.md_bus().write64(8, 0x33), BUS_OK);
        md
    }
    #[test]
    fn bedrock_banks_remap_without_losing_data_or_exposing_holes() {
        let md = configured();
        let bus = md.physical();
        assert_eq!(bus.write64(0, 0x1122334455667788), BUS_OK);
        assert_eq!(bus.write64(BANK_SPAN, 0xabcdef), BUS_OK);
        assert_eq!(bus.read8(0).data, 0x11);
        assert_eq!(bus.read32(4).data, 0x55667788);
        assert_eq!(bus.read64(FITTED_BANK).status, BUS_ERR);
        assert_eq!(bus.read64(2 * BANK_SPAN).status, BUS_ERR);
        assert_eq!(md.md_bus().write64(8, 0x33 | 1 << 32), BUS_OK);
        assert_eq!(bus.read64(0).data, 0xabcdef);
        assert_eq!(bus.read64(BANK_SPAN).data, 0x1122334455667788);
        assert!(bus.mem_ptr(0).is_none());
        md.soft_reset();
        assert_eq!(bus.read64(0).data, 0xabcdef);
        md.power_on();
        assert_eq!(bus.read64(0).status, BUS_ERR);
    }
    #[test]
    fn bedrock_register_masks_and_widths_do_not_accept_unknown_operations() {
        let md = BedrockMd::new();
        let r = md.md_bus();
        assert_eq!(r.read64(0).data, u64::MAX);
        assert_eq!(r.read32(0).status, BUS_ERR);
        assert_eq!(r.write64(8, u64::MAX), BUS_OK);
        assert_eq!(r.read64(8).data, CONFIG_MASK);
        assert_eq!(r.write64(0x20000, u64::MAX), BUS_OK);
        assert_eq!(r.read64(0x20000).data, 1 << 63 | 0xfff);
        assert_eq!(r.read64(0x60).data, 0x001818180c0c0c00);
        assert_eq!(r.write64(0x20030, 1), BUS_ERR);
        assert_eq!(r.write64(0x70000, 0), BUS_ERR);
        assert!(md.unsupported() >= 3);
    }
    #[test]
    fn bedrock_fetchop_uses_big_endian_word_lanes_and_preserves_other_word() {
        let md = configured();
        let ram = md.physical();
        let f = md.fetchop_bus();
        ram.write64(0, 0xffffffff00000009);
        assert_eq!(f.read32(8).data, u32::MAX);
        assert_eq!(ram.read64(0).data, 9);
        assert_eq!(f.read32(12).data, 9);
        assert_eq!(ram.read64(0).data, 10);
        assert_eq!(f.write32(28, 3), BUS_OK);
        assert_eq!(ram.read64(0).data, 2);
        assert_eq!(f.write64(32, 8), BUS_OK);
        assert_eq!(ram.read64(0).data, 10);
        assert_eq!(f.read64(24).data, 10);
        assert_eq!(ram.read64(0).data, 0);
        assert_eq!(f.read64(32).status, BUS_ERR);
        assert_eq!(f.write64(40, 1), BUS_ERR);
        assert_eq!(f.read16(0).status, BUS_ERR);
    }
    #[test]
    fn bedrock_ecc_corrects_and_callback_can_reenter_controller() {
        let md = configured();
        let e = md.ecc_bus();
        md.physical().write64(0, 0);
        let weak = Arc::downgrade(&md);
        let count = Arc::new(AtomicU64::new(0));
        let seen = count.clone();
        md.on_error(Arc::new(move |kind| {
            assert_eq!(kind, MemoryError::Correctable);
            assert_eq!(weak.upgrade().unwrap().current_cell(), u64::MAX);
            seen.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(e.write8(7, 1), BUS_OK);
        assert_eq!(md.physical().read64(0).data, 0);
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert_eq!(e.read8(7).data, 0);
        assert_ne!(md.md_bus().read64(0x60018).data & (1 << 52), 0);
        assert_eq!(md.md_bus().read64(0x60010).data, 0);
        assert_eq!(e.write16(6, 0x2313), BUS_OK);
        assert_eq!(e.read16(6).data, 0x2313);
        assert_eq!(e.read32(4).status, BUS_ERR);
        assert_eq!(e.write16(0x04000006, 0xface), BUS_OK);
        assert_eq!(e.read16(0x04000006).data, 0xface);
    }
    #[test]
    fn bedrock_directory_protection_is_independent_and_wraps_at_fitted_density() {
        let md = configured();
        let d = md.directory_bus();
        assert_eq!(d.write64(0x408, 0x1234), BUS_OK);
        assert_eq!(d.read64(0x2000408).data, 0x1234);
        assert_eq!(d.write64(0x418, 0x5678), BUS_OK);
        assert_eq!(d.read64(0x408).data, 0x1234);
        assert_eq!(d.read64(0x418).data, 0x5678);
        assert_eq!(d.write64(8, u64::MAX), BUS_OK);
        assert_eq!(d.read64(8).data, u64::MAX);
        assert_eq!(d.read32(0).status, BUS_ERR);
        assert_eq!(d.read64(1 << 30).status, BUS_ERR);
    }
    #[test]
    fn bedrock_directory_writes_generate_check_bits_and_correct_on_ram_read() {
        let md = configured();
        let r = md.md_bus();
        let d = md.directory_bus();
        assert_eq!(d.write64(0, 1), BUS_OK);
        assert_eq!(d.read64(0).data, 0x001f0001);
        r.write64(0x40000, 2);
        d.write64(0, 0x001e0001);
        r.write64(0x40000, 0);
        assert_eq!(md.physical().read64(0).status, BUS_OK);
        assert_eq!(d.read64(0).data, 0x001f0001);
        assert_ne!(r.read64(0x40018).data & (1 << 61), 0);
        assert_eq!(r.read64(0x40010).data, 0);
    }
}

const DIRECTORY_PREMIUM: [u8; 64] = [
    0x1f, 0x2f, 0x37, 0x3b, 0x3d, 0x3e, 0x07, 0x0b, 0x0d, 0x0e, 0x13, 0x15, 0x16, 0x19, 0x1a, 0x1c,
    0x7f, 0x4f, 0x57, 0x5b, 0x5d, 0x5e, 0x67, 0x6b, 0x6d, 0x6e, 0x73, 0x75, 0x76, 0x79, 0x7a, 0x7c,
    1, 2, 4, 8, 16, 32, 0x23, 0x25, 0x26, 0x29, 0x2a, 0x2c, 0x31, 0x32, 0x34, 0x38, 0x40, 0x43,
    0x45, 0x46, 0x49, 0x4a, 0x4c, 0x51, 0x52, 0x54, 0x58, 0x61, 0x62, 0x64, 0x68, 0x70,
];
const DIRECTORY_STANDARD: [u8; 32] = [
    0x1f, 0x2f, 0x37, 0x3b, 0x3d, 0x3e, 0x07, 0x0b, 0x0d, 0x0e, 0x13, 0x15, 0x16, 0x19, 0x1a, 0x1c,
    1, 2, 4, 8, 16, 32, 0x23, 0x25, 0x26, 0x29, 0x2a, 0x2c, 0x31, 0x32, 0x34, 0x38,
];
fn directory_columns(premium: bool) -> &'static [u8] {
    if premium {
        &DIRECTORY_PREMIUM
    } else {
        &DIRECTORY_STANDARD
    }
}
fn directory_syndrome(mut word: u64, premium: bool) -> u8 {
    if !premium {
        word &= u32::MAX as u64;
    }
    let mut check = 0;
    while word != 0 {
        check ^= directory_columns(premium)[word.trailing_zeros() as usize];
        word &= word - 1;
    }
    check
}
pub fn directory_encode(word: u64, premium: bool) -> u64 {
    let word = word
        & !if premium {
            (63 << 32) | (1 << 48)
        } else {
            63 << 16
        };
    let check = directory_syndrome(word, premium) as u64;
    word | if premium {
        (check & 63) << 32 | (check >> 6) << 48
    } else {
        check << 16
    }
}
