//! DP83843BVJE copper/nibble-MII management profile.
//!
//! Ported from hw/net/dp83840.c in jamesbraid/qemu-sgi-origami.
//! Link settling and the synthetic partner are emulator approximations.
//! Hardware BMSR latches link low; this profile reports the live level.
// SPDX-License-Identifier: BSD-3-Clause

use std::time::{Duration, Instant};
const SETTLE: Duration = Duration::from_secs(2);
const BREAK: Duration = Duration::from_millis(1500);

/// Management state for one selected PHY. No board GPIO reset is connected.
pub struct Dp83843 {
    configured_addr: u8,
    regs: [u16; 32],
    backend_up: bool,
    done: bool,
    forced: bool,
    resolved: u8,
    armed: bool,
    deadline: Option<Instant>,
    complete: Option<Instant>,
    pending: bool,
    restarting: bool,
    pub unimplemented_accesses: u32,
}
impl Dp83843 {
    pub fn new(addr: u8) -> Self {
        let mut p = Self {
            configured_addr: addr & 31,
            regs: [0; 32],
            backend_up: true,
            done: false,
            forced: false,
            resolved: 0,
            armed: false,
            deadline: None,
            complete: None,
            pending: false,
            restarting: false,
            unimplemented_accesses: 0,
        };
        p.reset();
        p
    }
    pub fn reset(&mut self) {
        self.regs = [0; 32];
        self.regs[0] = 0x3100 | if self.configured_addr == 0 { 0x400 } else { 0 };
        self.regs[4] = 0x1e1;
        self.regs[7] = 0x2001;
        self.regs[0x16] = 0x20;
        self.regs[0x19] = u16::from(self.configured_addr);
        self.done = false;
        self.forced = false;
        self.resolved = 0;
        self.armed = false;
        self.deadline = None;
        self.complete = None;
        self.pending = false;
        self.restarting = false;
    }
    pub fn address(&self) -> u8 {
        (self.regs[0x19] & 31) as u8
    }
    pub fn backend_up(&self) -> bool {
        self.backend_up
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
    fn autoneg(&self) -> bool {
        self.regs[0] & 0x1000 != 0
    }
    fn supported(&self) -> bool {
        self.regs[0x19] & 0x220 == 0
            && self.regs[0x18] & 0x1a20 == 0
            && self.regs[0x17] & 0x3f80 == 0
    }
    fn link(&self) -> bool {
        self.backend_up
            && self.supported()
            && self.regs[0] & 0x800 == 0
            && if self.autoneg() {
                self.done && self.resolved != 0
            } else {
                self.forced
            }
    }
    pub fn data_path(&self) -> bool {
        self.supported() && self.regs[0] & 0xc00 == 0
    }
    pub fn wire_enabled(&self) -> bool {
        self.data_path() && self.link()
    }
    fn loopback_selected(&self) -> bool {
        self.regs[0] & 0x4000 != 0 || self.regs[0x17] & 0x30 != 0
    }
    pub fn in_loopback(&self) -> bool {
        self.data_path() && self.loopback_selected()
    }
    fn status(&self) -> u16 {
        let mut v = if self.autoneg() { 0x200 } else { 0 };
        if self.loopback_selected() {
            v |= 8;
        }
        if !self.link() {
            return v;
        }
        v |= 1;
        let (speed, full) = if self.autoneg() {
            v |= 0x20;
            (self.resolved <= 2, self.resolved == 1 || self.resolved == 3)
        } else {
            (self.regs[0] & 0x2000 != 0, self.regs[0] & 0x100 != 0)
        };
        if !speed {
            v |= 2;
        }
        if full {
            v |= 4;
        }
        if self.regs[0] & 0x400 == 0 {
            v |= 0x800;
        }
        v
    }
    fn event(&mut self, before: u16) {
        let after = self.status();
        if self.regs[0x11] & 2 != 0 && ((before ^ after) & 1 != 0 || !before & after & 0x20 != 0) {
            self.pending = true;
        }
    }
    fn resolve(&mut self) {
        self.resolved = 0;
        if self.autoneg() && self.supported() {
            for (i, bit) in [0x100, 0x80, 0x40, 0x20].iter().enumerate() {
                if self.regs[4] & bit != 0 {
                    self.resolved = i as u8 + 1;
                    break;
                }
            }
        }
        if self.resolved == 0 {
            log::warn!("dp83843: no common supported advertised mode");
        }
    }
    pub fn sync(&mut self, now: Instant) {
        if self.deadline.is_some_and(|d| now >= d) {
            let before = self.status();
            if let Some(end) = self.complete.take() {
                self.regs[0] &= !0x200;
                self.deadline = Some(end);
                if now < end {
                    return;
                }
            }
            self.deadline = None;
            self.restarting = false;
            self.done = true;
            self.resolve();
            self.event(before);
        }
    }
    pub fn set_backend_link(&mut self, up: bool, now: Instant) {
        self.sync(now);
        let before = self.status();
        self.backend_up = up;
        self.event(before);
    }
    fn unimplemented(&mut self, r: u8) {
        self.unimplemented_accesses += 1;
        log::warn!("dp83843: unsupported register or control at {r:#x}");
    }
    pub fn read(&mut self, r: u8, now: Instant) -> u16 {
        self.sync(now);
        let v = match r {
            0 => self.regs[0],
            1 => {
                if !self.armed && !self.done && self.autoneg() {
                    self.armed = true;
                    self.deadline = Some(now + SETTLE);
                }
                let status = self.status();
                0x7849 | if status & 1 != 0 { 4 } else { 0 } | (status & 0x20)
            }
            2 => 0x2000,
            3 => 0x5c10,
            4 | 7 => self.regs[r as usize],
            5 => {
                if self.done {
                    0x1e1
                } else {
                    0
                }
            }
            6 => 4 | u16::from(self.done),
            0x10 => {
                let v = self.status() | if self.pending { 0x100 } else { 0 };
                self.pending = false;
                v
            }
            0x11 => self.regs[0x11],
            0x12 => {
                let v = if self.pending { 0x8000 } else { 0 };
                self.pending = false;
                v
            }
            0x13..=0x15 => {
                let v = self.regs[r as usize];
                self.regs[r as usize] = 0;
                v
            }
            0x16 => {
                let v = self.regs[0x16];
                if self.autoneg() {
                    v & !0x4000
                } else if self.regs[0x19] & 0x20 != 0 {
                    v | 0x4000
                } else {
                    v
                }
            }
            0x17 => {
                self.regs[0x17]
                    | if self.regs[0x19] & 0x20 != 0 {
                        0x1000
                    } else {
                        0
                    }
            }
            0x18 => {
                self.regs[0x18]
                    | if self.status() & 4 != 0 || self.regs[0x19] & 0x200 != 0 {
                        0x80
                    } else {
                        0
                    }
            }
            0x19 => self.regs[0x19],
            _ => {
                self.unimplemented(r);
                0
            }
        };
        if self.regs[0x11] & 1 != 0 {
            self.pending = true;
        }
        v
    }
    pub fn write(&mut self, r: u8, val: u16, now: Instant) {
        self.sync(now);
        let before = self.status();
        match r {
            0 => {
                let val = val & 0xff80;
                if val & 0x8000 != 0 {
                    self.reset();
                    return;
                }
                if val & 0x80 != 0 {
                    self.unimplemented(r);
                }
                self.regs[0x17] &= !0x380;
                if val & 0x4000 != 0 {
                    self.regs[0x17] |= 0x30;
                }
                if val & 0x400 == 0 && val & 0x1200 == 0x1200 {
                    self.done = false;
                    self.resolved = 0;
                    self.forced = false;
                    self.armed = true;
                    self.restarting = true;
                    self.deadline = Some(now + BREAK);
                    self.complete = Some(now + BREAK + SETTLE);
                    self.regs[0] = val;
                } else if val & 0x400 == 0 && (self.restarting || self.forced && !self.autoneg()) {
                    if val & 0x1000 == 0 {
                        self.restarting = false;
                        self.complete = None;
                        self.deadline = None;
                        self.forced = val & 0x800 == 0;
                    }
                    self.regs[0] = val & !0x200;
                } else {
                    self.restarting = false;
                    self.complete = None;
                    self.deadline = None;
                    self.forced = false;
                    self.done = val & 0x400 == 0;
                    self.armed = self.done;
                    self.regs[0] = val & !0x8200;
                }
                if !self.autoneg() {
                    self.resolved = 0;
                    self.done = false;
                    self.forced = self.supported() && self.regs[0] & 0xc00 == 0;
                } else if self.done && self.resolved == 0 {
                    self.resolve();
                }
            }
            1..=3 | 5 | 6 | 0x10 | 0x12 => {}
            4 => self.regs[4] = val & 0xa5ff,
            7 => self.regs[7] = (val & 0xb7ff) | (self.regs[7] & 0x800),
            0x11 => self.regs[0x11] = val & 3,
            0x13..=0x15 => self.regs[r as usize] = val,
            0x16 => {
                if val & !0x20 != 0 {
                    self.unimplemented(r);
                }
                let mask = if self.autoneg() || self.regs[0x19] & 0x20 != 0 {
                    0xbfef
                } else {
                    0xffef
                };
                self.regs[0x16] = (self.regs[0x16] & !mask) | (val & mask);
            }
            0x17 => {
                if val & 0x4000 != 0 {
                    self.unimplemented(r);
                }
                self.regs[0x17] = val & 0x7fb0;
            }
            0x18 => {
                if val & 0x55c != 0 {
                    self.unimplemented(r);
                }
                self.regs[0x18] = val & 0x1ffc;
            }
            0x19 => {
                if val & !31 != 0 {
                    self.unimplemented(r);
                }
                self.regs[0x19] = val & 0x3bff;
            }
            _ => self.unimplemented(r),
        }
        if !self.supported() {
            self.resolved = 0;
            self.forced = false;
            self.unimplemented(r);
        }
        self.event(before);
        if self.regs[0x11] & 1 != 0 {
            self.pending = true;
        }
    }
}
#[cfg(test)]
mod tests;
