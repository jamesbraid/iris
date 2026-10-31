//! Graphics flow control in the Bedrock processor interface: the two
//! per-slot credit counters a processor pays from with each uncached I/O
//! store into its graphics page, and the credits a graphics head returns.
//!
//! Each processor slot has a page register (an enable bit and the 128 KiB
//! page address), a 12-bit credit counter, a bias, and an interrupt count
//! and compare. A store the slot's processor makes into its enabled page is
//! counted: the counter goes down by one, and falling below the bias raises
//! the slot's graphics interrupt once until the counter climbs above the
//! bias again. The store is marked as counted while it is forwarded, so the
//! head it reaches returns one credit for it after executing it. The node
//! I/O block's two graphics map entries say which node, interface and slot
//! a widget's credits go to.
//!
//! # What is approximated here
//!
//! - Whether the counter wraps, saturates or goes negative is unknown; it
//!   wraps, and the wrap is logged.
//! - The interrupt is raised as the store is counted, below the bias, with
//!   no write stall; the interrupt count and compare are stored but have no
//!   effect.
//!
//! Ported from hw/mips/sgi/pi-gfx.c in jamesbraid/qemu-sgi-origami.

use crate::devlog::LogModule;
use std::cell::Cell;

/// Processor slots of one interface.
pub const SLOTS: usize = 2;
/// Registers of the block, five a slot from 0x300 in steps of 8.
pub const REGS: usize = 10;
const SLOT_REGS: usize = 5;
const PAGE: usize = 0;
const CREDIT: usize = 1;
const BIAS: usize = 2;
const INT_COUNT: usize = 3;

pub const PAGE_ENABLE: u64 = 1 << 40;
pub const PAGE_ADDRESS: u64 = 0xff_fffe_0000;
const PAGE_MASK: u64 = PAGE_ENABLE | PAGE_ADDRESS;
pub const CREDIT_MASK: u64 = 0xfff;
const INT_MASK: u64 = 0x3ff_ffff;

/// One interface's graphics registers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PiGfx {
    page: [u64; SLOTS],
    credit: [u64; SLOTS],
    bias: [u64; SLOTS],
    int_count: [u64; SLOTS],
    int_compare: [u64; SLOTS],
    starved: [bool; SLOTS],
    stall_logged: [bool; SLOTS],
}

impl PiGfx {
    /// Register `n` (0..10) of the block.
    pub fn read(&self, n: usize) -> u64 {
        assert!(n < REGS);
        let slot = n / SLOT_REGS;
        match n % SLOT_REGS {
            PAGE => self.page[slot],
            CREDIT => self.credit[slot],
            BIAS => self.bias[slot],
            INT_COUNT => self.int_count[slot],
            _ => self.int_compare[slot],
        }
    }

    pub fn write(&mut self, n: usize, value: u64) {
        assert!(n < REGS);
        let slot = n / SLOT_REGS;
        match n % SLOT_REGS {
            PAGE => self.page[slot] = value & PAGE_MASK,
            CREDIT => {
                self.credit[slot] = value & CREDIT_MASK;
                self.rearm(slot);
            }
            BIAS => self.bias[slot] = value & CREDIT_MASK,
            INT_COUNT => self.int_count[slot] = value & INT_MASK,
            _ => self.int_compare[slot] = value & INT_MASK,
        }
    }

    /// Count a store `slot`'s processor makes at I/O offset `address`:
    /// whether it fell in the slot's enabled graphics page.
    pub fn count_store(&mut self, slot: usize, address: u64) -> bool {
        let page = self.page[slot];
        if page & PAGE_ENABLE == 0 || address & PAGE_ADDRESS != page & PAGE_ADDRESS {
            return false;
        }
        if self.credit[slot] == 0 {
            crate::dlog!(LogModule::Bedrock, "PI graphics credit counter {} wrapped below zero", slot_name(slot));
        }
        self.credit[slot] = self.credit[slot].wrapping_sub(1) & CREDIT_MASK;
        true
    }

    /// Whether a counted store left the counter below the bias for the first
    /// time since it was last above it: the slot's graphics interrupt.
    pub fn take_starved(&mut self, slot: usize) -> bool {
        if self.starved[slot] || self.credit[slot] >= self.bias[slot] {
            return false;
        }
        self.starved[slot] = true;
        if !self.stall_logged[slot] {
            self.stall_logged[slot] = true;
            crate::dlog!(
                LogModule::Bedrock,
                "PI graphics credit counter {} fell below its bias; interrupt raised at once, no write stall",
                slot_name(slot)
            );
        }
        true
    }

    /// Credits a head returned for `slot`.
    pub fn add_credits(&mut self, slot: usize, credits: u32) {
        let sum = self.credit[slot] + u64::from(credits);
        if sum > CREDIT_MASK {
            crate::dlog!(LogModule::Bedrock, "PI graphics credit counter {} wrapped above {CREDIT_MASK:#x}", slot_name(slot));
        }
        self.credit[slot] = sum & CREDIT_MASK;
        self.rearm(slot);
    }

    /// The counter above the bias re-arms the slot's interrupt.
    fn rearm(&mut self, slot: usize) {
        if self.credit[slot] > self.bias[slot] {
            self.starved[slot] = false;
        }
    }
}

fn slot_name(slot: usize) -> char {
    (b'A' + slot as u8) as char
}

/// Where a widget's credits go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub node: u8,
    pub interface: usize,
    pub slot: usize,
}

/// The Bedrock node I/O block's graphics map: widget in bits 3:0, interface
/// in bit 4, node in bits 12:5 and slot in bit 16 (Linux sn1/hubio_next.h,
/// IIO_IGFX_INIT). There is no valid bit; the first entry naming the widget
/// wins.
pub fn route(map: [u64; 2], widget: u8) -> Option<Route> {
    map.into_iter().find(|entry| entry & 0xf == u64::from(widget)).map(|entry| Route {
        node: ((entry >> 5) & 0xff) as u8,
        interface: ((entry >> 4) & 1) as usize,
        slot: ((entry >> 16) & 1) as usize,
    })
}

thread_local! {
    /// Whether the store the processor is forwarding on this thread was
    /// counted, for the head that receives it.
    static COUNTED: Cell<bool> = const { Cell::new(false) };
}

/// Forward a store marked counted or not; the head takes the mark with
/// [`take_mark`]. The previous mark is restored afterwards.
pub fn with_mark<R>(counted: bool, f: impl FnOnce() -> R) -> R {
    let previous = COUNTED.with(|c| c.replace(counted));
    let result = f();
    COUNTED.with(|c| c.set(previous));
    result
}

/// Whether the store being received was counted, consuming the mark so a
/// second write in the same forward is not.
pub fn take_mark() -> bool {
    COUNTED.with(|c| c.replace(false))
}

#[cfg(test)]
mod tests {
    // Cases follow tests/qtest/sn1-pi-gfx-test.c in
    // jamesbraid/qemu-sgi-origami: uncovered-stores-not-counted,
    // map-matching, wrap-mode-and-interrupt and
    // head-returns-only-counted-stores.
    use super::*;

    const PAGE_BASE: u64 = 0x0140_0000;

    fn enabled(credits: u64, bias: u64) -> PiGfx {
        let mut g = PiGfx::default();
        g.write(PAGE, PAGE_ENABLE | PAGE_BASE);
        g.write(CREDIT, credits);
        g.write(BIAS, bias);
        g
    }

    #[test]
    fn only_stores_into_an_enabled_page_are_counted() {
        let mut g = PiGfx::default();
        g.write(CREDIT, 5);
        assert!(!g.count_store(0, PAGE_BASE + 0x158), "page disabled");
        g.write(PAGE, PAGE_ENABLE | PAGE_BASE);
        assert!(!g.count_store(0, 0x0142_0158), "another page");
        assert!(!g.count_store(1, PAGE_BASE + 0x158), "the other slot's page");
        assert!(g.count_store(0, PAGE_BASE + 0x1_fff8));
        assert_eq!(g.read(CREDIT), 4);
        assert_eq!(g.read(PAGE), PAGE_ENABLE | PAGE_BASE);
    }

    #[test]
    fn the_counter_wraps_and_the_interrupt_fires_once_below_the_bias() {
        let mut g = enabled(2, 2);
        assert!(g.count_store(0, PAGE_BASE));
        assert!(g.take_starved(0), "one below the bias");
        assert!(g.count_store(0, PAGE_BASE));
        assert!(!g.take_starved(0), "once until re-armed");
        assert!(g.count_store(0, PAGE_BASE));
        assert_eq!(g.read(CREDIT), CREDIT_MASK, "wraps below zero");
        g.write(CREDIT, 1);
        g.add_credits(0, 2);
        assert_eq!(g.read(CREDIT), 3);
        assert!(g.count_store(0, PAGE_BASE));
        assert!(g.count_store(0, PAGE_BASE));
        assert!(g.take_starved(0), "re-armed above the bias");
        g.add_credits(0, 0x1000);
        assert_eq!(g.read(CREDIT), 1, "wraps above the mask");
    }

    #[test]
    fn registers_keep_their_fields_and_each_slot_its_own() {
        let mut g = PiGfx::default();
        g.write(SLOT_REGS + PAGE, u64::MAX);
        g.write(SLOT_REGS + CREDIT, u64::MAX);
        g.write(SLOT_REGS + INT_COUNT, u64::MAX);
        assert_eq!(g.read(SLOT_REGS + PAGE), PAGE_MASK);
        assert_eq!(g.read(SLOT_REGS + CREDIT), CREDIT_MASK);
        assert_eq!(g.read(SLOT_REGS + INT_COUNT), INT_MASK);
        assert_eq!(g.read(PAGE), 0);
    }

    #[test]
    fn the_map_names_node_interface_and_slot_and_the_first_match_wins() {
        let entry = |node: u64, pi: u64, slot: u64, widget: u64| (slot << 16) | (node << 5) | (pi << 4) | widget;
        let map = [entry(3, 1, 1, 0xc), entry(0, 0, 0, 0xc)];
        assert_eq!(route(map, 0xc), Some(Route { node: 3, interface: 1, slot: 1 }));
        assert_eq!(route(map, 0xb), None);
        assert_eq!(route([0, 0], 0), Some(Route { node: 0, interface: 0, slot: 0 }), "no valid bit");
    }

    #[test]
    fn a_head_sees_the_mark_only_for_the_store_it_was_set_for() {
        assert!(!take_mark());
        assert!(with_mark(true, take_mark));
        assert!(with_mark(true, || {
            let first = take_mark();
            first && !take_mark()
        }));
        assert!(!with_mark(false, take_mark));
        assert!(!take_mark(), "restored after the forward");
    }
}
