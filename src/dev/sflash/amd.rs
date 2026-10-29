// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) Aaron Giles
// Copyright (c) 2026 James Braid
//! AMD unlock/program/erase state machine derived from MAME intelfsh.cpp,
//! revision 2ee9b1e30299b122b131cab0bd7912abc2b095ba (BSD-3-Clause).
//!
//! This engine owns protocol state, not storage or bus translation. CFI bytes
//! and model timing describe an explicit profile, not a fitted chip.
//! Buffered writes, protection, bypass and erase suspend are unsupported.

pub const SECTOR_SIZE: usize = 64 * 1024;
const SECTORS: usize = 128;
pub const PART_SIZE: usize = SECTOR_SIZE * SECTORS;
pub const ERASE_SETUP_NS: u64 = 50_000;
pub const ERASE_NS: u64 = 512_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Array,
    Unlock1,
    Unlock2,
    Program,
    Erase1,
    Erase2,
    Erase3,
    Query,
    Identify,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    Program { offset: usize, value: u16 },
}

pub struct AmdCore {
    mode: Mode,
    selected: [bool; SECTORS],
    /// Remaining time includes the sector-selection interval.
    remaining_ns: u64,
    setup_ns: u64,
    toggle: bool,
    failed: bool,
}

impl Default for AmdCore {
    fn default() -> Self {
        Self {
            mode: Mode::Array,
            selected: [false; SECTORS],
            remaining_ns: 0,
            setup_ns: 0,
            toggle: false,
            failed: false,
        }
    }
}

impl AmdCore {
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn busy(&self) -> bool {
        self.remaining_ns != 0 || self.failed
    }
    pub fn selecting(&self) -> bool {
        self.setup_ns != 0
    }
    pub fn array_reads(&self) -> bool {
        !self.busy() && self.mode != Mode::Query && self.mode != Mode::Identify
    }
    /// None selects ordinary backing bytes. Polling never advances time.
    pub fn read16(&mut self, offset: usize) -> Option<u16> {
        if self.busy() {
            let selected = self.selected[offset / SECTOR_SIZE];
            self.toggle = !self.toggle;
            return Some(
                (if self.toggle {
                    0x40 | if selected { 4 } else { 0 }
                } else {
                    0
                }) | if self.setup_ns == 0 { 8 } else { 0 }
                    | if self.failed { 0x20 } else { 0 },
            );
        }
        match self.mode {
            Mode::Query => Some(query(offset / 2) as u16),
            // IDs remain unspecified until a part/profile is selected.
            Mode::Identify => Some(0),
            _ => None,
        }
    }
    pub fn write16(&mut self, offset: usize, value: u16) -> Result<Effect, ()> {
        if offset & 1 != 0 || offset >= PART_SIZE {
            return Err(());
        }
        let cmd = value as u8;
        let word = offset / 2;
        if self.busy() {
            if (self.failed || self.setup_ns != 0) && cmd == 0xf0 {
                self.reset();
            } else if self.setup_ns != 0 && cmd == 0x30 {
                self.selected[offset / SECTOR_SIZE] = true;
                self.setup_ns = ERASE_SETUP_NS;
                self.remaining_ns = ERASE_SETUP_NS + ERASE_NS;
            } else if self.setup_ns != 0 {
                self.reset();
                log::warn!("amd-flash: sector selection cancelled by {cmd:#x}");
                return Err(());
            } else if cmd != 0xf0 {
                log::warn!("amd-flash: unsupported command {cmd:#x} while erasing");
                return Err(());
            }
            return Ok(Effect::None);
        }
        // The pending program data cycle consumes every bit, including F0.
        if self.mode == Mode::Program {
            self.mode = Mode::Array;
            return Ok(Effect::Program { offset, value });
        }
        if cmd == 0xf0 {
            self.reset();
            return Ok(Effect::None);
        }
        let next = match (self.mode, word, cmd) {
            (Mode::Array, 0x55, 0x98) => Mode::Query,
            (Mode::Array, 0x555, 0xaa) => Mode::Unlock1,
            (Mode::Unlock1, 0x2aa, 0x55) => Mode::Unlock2,
            (Mode::Unlock2, 0x555, 0xa0) => Mode::Program,
            (Mode::Unlock2, 0x555, 0x80) => Mode::Erase1,
            (Mode::Unlock2, 0x555, 0x90) => Mode::Identify,
            (Mode::Erase1, 0x555, 0xaa) => Mode::Erase2,
            (Mode::Erase2, 0x2aa, 0x55) => Mode::Erase3,
            (Mode::Erase3, _, 0x30) => {
                self.selected[offset / SECTOR_SIZE] = true;
                self.setup_ns = ERASE_SETUP_NS;
                self.remaining_ns = ERASE_SETUP_NS + ERASE_NS;
                Mode::Array
            }
            _ => {
                log::warn!(
                    "amd-flash: unsupported cycle {value:#06x} at {offset:#x} in {:?}",
                    self.mode
                );
                self.mode = Mode::Array;
                return Err(());
            }
        };
        self.mode = next;
        Ok(Effect::None)
    }
    /// The owner persists and applies selected sectors before calling finish.
    pub fn advance_ns(&mut self, ns: u64) -> bool {
        if self.remaining_ns == 0 || self.failed {
            return false;
        }
        self.setup_ns = self.setup_ns.saturating_sub(ns);
        self.remaining_ns = self.remaining_ns.saturating_sub(ns);
        self.remaining_ns == 0
    }
    pub fn selected_sectors(&self) -> impl Iterator<Item = usize> + '_ {
        self.selected
            .iter()
            .enumerate()
            .filter_map(|(i, selected)| selected.then_some(i))
    }
    pub fn finish_erase(&mut self, success: bool) {
        if success {
            self.reset();
        } else {
            self.failed = true;
        }
    }
}

// Generic model response; voltage/timing do not identify Fuel's fitted part.
// Field positions/units: AMD publication 21444 rev E+2, pp14-15.
// Region encoding also follows Renode AMDCFIFlash.cs (MIT), revision
// 5448105bc7875aa6c5305761d97a371dc640dd7f. No Renode mutation code is used.
fn query(index: usize) -> u8 {
    const PROFILE: [u8; 33] = [
        b'Q', b'R', b'Y', 2, 0, 0, 0, 0, 0, 0, 0, 0x27, 0x36, 0, 0, 3, 0, 9, 0, 4, 0, 4, 0, 23, 1,
        0, 0, 0, 1, 0x7f, 0, 0, 1,
    ];
    index
        .checked_sub(0x10)
        .and_then(|i| PROFILE.get(i))
        .copied()
        .unwrap_or(0)
}
