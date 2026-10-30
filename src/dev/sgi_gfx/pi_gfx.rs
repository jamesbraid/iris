//! Ported from include/hw/mips/sgi/pi-gfx.h, hw/mips/sgi/pi-gfx.c and
//! hw/mips/sgi/pi-gfx-state.c in jamesbraid/qemu-sgi-origami.
//!
//! Graphics flow control in the Hub and Bedrock processor interfaces. The
//! Octane has no processor-interface link (its board's `pi` stays `None`),
//! so on that machine the credit calls are no-ops, as they are in QEMU
//! when the board's `pi` property is unset. `SGIPIGfx` is the register
//! block a node's processor interface embeds; its functions are pure state.
//! The QOM interface (`TYPE_SGI_PI_GFX`) becomes the `SGIPIGfxLink` trait.
//! Skipped: the vmstate description, and the per-thread forwarding-source
//! mark (sgi_pi_gfx_mark_source/store_source), which names a QOM node.

use super::*;
use std::cell::Cell;

pub const SGI_PI_GFX_SLOTS: usize = 2;
/// The block's first register and its extent in the interface's window.
pub const SGI_PI_GFX_BASE: u32 = 0x300;
pub const SGI_PI_GFX_SLOT_STRIDE: u32 = 0x28;
pub const SGI_PI_GFX_REGS: u32 = 10;

/// The five registers of one slot, in address order.
pub const SGI_PI_GFX_PAGE: u32 = 0;
pub const SGI_PI_GFX_CREDIT: u32 = 1;
pub const SGI_PI_GFX_BIAS: u32 = 2;
pub const SGI_PI_GFX_INT_COUNT: u32 = 3;
pub const SGI_PI_GFX_INT_COMPARE: u32 = 4;
pub const SGI_PI_GFX_SLOT_REGS: u32 = 5;

/// Page: bit 40 enables it, bits 39:17 name it; the rest is reserved.
pub const SGI_PI_GFX_PAGE_ENABLE: u64 = 1 << 40;
pub const SGI_PI_GFX_PAGE_ADDRESS: u64 = 0xfffffe0000;
pub const SGI_PI_GFX_PAGE_MASK: u64 = SGI_PI_GFX_PAGE_ENABLE | SGI_PI_GFX_PAGE_ADDRESS;
pub const SGI_PI_GFX_CREDIT_MASK: u32 = 0xfff;
pub const SGI_PI_GFX_BIAS_MASK: u32 = 0xfff;
pub const SGI_PI_GFX_INT_MASK: u32 = 0x3ffffff;

#[derive(Clone, Copy, Default)]
pub struct SGIPIGfx {
    pub page: [u64; SGI_PI_GFX_SLOTS],
    pub credit: [u32; SGI_PI_GFX_SLOTS],
    pub bias: [u32; SGI_PI_GFX_SLOTS],
    pub int_count: [u32; SGI_PI_GFX_SLOTS],
    pub int_compare: [u32; SGI_PI_GFX_SLOTS],
    /// The slot's graphics interrupt has been raised for the counter falling
    /// below the bias, and not yet re-armed by the counter rising above it.
    pub starved: [bool; SGI_PI_GFX_SLOTS],
    /// Host-only: the zero-length stall has been logged for this slot.
    pub stall_logged: [bool; SGI_PI_GFX_SLOTS],
    /// Bedrock: a counted store left the counter below the bias, and the
    /// slot's next counted store waits until the processor has taken the
    /// graphics interrupt or the counter has risen above the bias.
    pub stopped: [bool; SGI_PI_GFX_SLOTS],
    /// Bedrock: the interrupt counter runs (the credit counter is at zero).
    pub int_running: [bool; SGI_PI_GFX_SLOTS],
    /// The clock tick at which int_count held its value.
    pub int_epoch: [u64; SGI_PI_GFX_SLOTS],
}

/// The interface a node device implements for the graphics heads behind it
/// (the C's TYPE_SGI_PI_GFX class: return_credits, owe_credits,
/// repay_credits). `widget` is the head's crosstalk widget number, the
/// value the driver writes into the node-widget map.
pub trait SGIPIGfxLink: Send + Sync {
    /// A graphics head took `n` counted stores off its port.
    fn return_credits(&self, widget: u32, n: u32);
    /// Credits a head owes for counted stores it has not yet taken off its
    /// port; a negative count cancels owed credits, as a board reset that
    /// abandons the words does. A node without a ledger ignores the debt.
    fn owe_credits(&self, widget: u32, n: i32) {
        let _ = (widget, n);
    }
    /// Owed credits returned as the head takes the words. A node without a
    /// ledger routes a repayment as an ordinary return.
    fn repay_credits(&self, widget: u32, n: u32) {
        self.return_credits(widget, n);
    }
}

/// sgi_pi_gfx_return_credits().
pub fn sgi_pi_gfx_return_credits(node: &dyn SGIPIGfxLink, widget: u32, credits: u32) {
    node.return_credits(widget, credits);
}

/// sgi_pi_gfx_owe_credits().
pub fn sgi_pi_gfx_owe_credits(node: &dyn SGIPIGfxLink, widget: u32, credits: i32) {
    node.owe_credits(widget, credits);
}

/// sgi_pi_gfx_repay_credits().
pub fn sgi_pi_gfx_repay_credits(node: &dyn SGIPIGfxLink, widget: u32, credits: u32) {
    node.repay_credits(widget, credits);
}

/// The counter above the bias re-arms the slot's graphics interrupt.
fn sgi_pi_gfx_rearm(g: &mut SGIPIGfx, slot: usize) {
    if g.credit[slot] > g.bias[slot] {
        g.starved[slot] = false;
    }
}

/// Every register zero, every page disabled (the model's choice).
pub fn sgi_pi_gfx_reset(g: &mut SGIPIGfx) {
    *g = SGIPIGfx::default();
}

/// Register n of the block, 0 to SGI_PI_GFX_REGS - 1, counted in
/// doublewords from SGI_PI_GFX_BASE.
pub fn sgi_pi_gfx_read(g: &SGIPIGfx, n: u32) -> u64 {
    let slot = (n / SGI_PI_GFX_SLOT_REGS) as usize;

    assert!(n < SGI_PI_GFX_REGS);
    match n % SGI_PI_GFX_SLOT_REGS {
        SGI_PI_GFX_PAGE => g.page[slot],
        SGI_PI_GFX_CREDIT => g.credit[slot] as u64,
        SGI_PI_GFX_BIAS => g.bias[slot] as u64,
        SGI_PI_GFX_INT_COUNT => g.int_count[slot] as u64,
        _ => g.int_compare[slot] as u64,
    }
}

/// A write keeps the field's bits; a write to the credit counter replaces
/// the count.
pub fn sgi_pi_gfx_write(g: &mut SGIPIGfx, n: u32, value: u64) {
    let slot = (n / SGI_PI_GFX_SLOT_REGS) as usize;

    assert!(n < SGI_PI_GFX_REGS);
    match n % SGI_PI_GFX_SLOT_REGS {
        SGI_PI_GFX_PAGE => g.page[slot] = value & SGI_PI_GFX_PAGE_MASK,
        SGI_PI_GFX_CREDIT => {
            g.credit[slot] = (value as u32) & SGI_PI_GFX_CREDIT_MASK;
            sgi_pi_gfx_rearm(g, slot);
        }
        SGI_PI_GFX_BIAS => g.bias[slot] = (value as u32) & SGI_PI_GFX_BIAS_MASK,
        SGI_PI_GFX_INT_COUNT => g.int_count[slot] = (value as u32) & SGI_PI_GFX_INT_MASK,
        _ => g.int_compare[slot] = (value as u32) & SGI_PI_GFX_INT_MASK,
    }
}

/// Whether the slot's page is enabled and covers physical address `addr`.
pub fn sgi_pi_gfx_covers(g: &SGIPIGfx, slot: usize, addr: u64) -> bool {
    assert!(slot < SGI_PI_GFX_SLOTS);
    let page = g.page[slot];
    (page & SGI_PI_GFX_PAGE_ENABLE) != 0
        && (addr & SGI_PI_GFX_PAGE_ADDRESS) == (page & SGI_PI_GFX_PAGE_ADDRESS)
}

/// An uncached I/O store by the processor in `slot` reached physical
/// address bits 39:0 `addr`. Charges one credit when the slot's page is
/// enabled and covers it, and returns whether it did. `who` names the
/// device in log messages.
pub fn sgi_pi_gfx_count_store(g: &mut SGIPIGfx, slot: usize, addr: u64, who: &str) -> bool {
    if !sgi_pi_gfx_covers(g, slot, addr) {
        return false;
    }
    // Whether the twelve-bit counter wraps, saturates or goes negative is
    // unknown. It wraps, and says so.
    if g.credit[slot] == 0 {
        crate::qlog!("{}: graphics credit counter {} wrapped below zero\n", who, (b'A' + slot as u8) as char);
    }
    g.credit[slot] = g.credit[slot].wrapping_sub(1) & SGI_PI_GFX_CREDIT_MASK;
    true
}

/// After a counted store: whether the slot's graphics interrupt must be
/// raised now. True once when the counter has fallen below the bias; the
/// counter rising above the bias again re-arms it. The stall that
/// accompanies it on the chip is modelled as zero length.
pub fn sgi_pi_gfx_take_starved(g: &mut SGIPIGfx, slot: usize, who: &str) -> bool {
    assert!(slot < SGI_PI_GFX_SLOTS);
    if g.starved[slot] || g.credit[slot] >= g.bias[slot] {
        return false;
    }
    g.starved[slot] = true;
    if !g.stall_logged[slot] {
        g.stall_logged[slot] = true;
        crate::qlog!(
            "{}: graphics credit counter {} fell below its bias; interrupt raised at once, the write stall and the interrupt counter are not modelled\n",
            who,
            (b'A' + slot as u8) as char
        );
    }
    true
}

/// Ticks from the count to the compare: a whole turn when they are equal.
fn sgi_pi_gfx_int_left(g: &SGIPIGfx, slot: usize) -> u64 {
    let left = g.int_compare[slot].wrapping_sub(g.int_count[slot]) & SGI_PI_GFX_INT_MASK;

    if left != 0 { left as u64 } else { SGI_PI_GFX_INT_MASK as u64 + 1 }
}

/// Brings int_count up to `now`, then starts or stops the count for the
/// credit counter as it now stands. True when the counter reached the
/// compare: the caller sends the interrupt.
pub fn sgi_pi_gfx_int_advance(g: &mut SGIPIGfx, slot: usize, now: u64) -> bool {
    let mut fired = false;

    assert!(slot < SGI_PI_GFX_SLOTS);
    if g.int_running[slot] && now > g.int_epoch[slot] {
        let elapsed = now - g.int_epoch[slot];

        if elapsed >= sgi_pi_gfx_int_left(g, slot) {
            g.int_count[slot] = g.int_compare[slot];
            fired = true;
        } else {
            g.int_count[slot] = (g.int_count[slot] as u64).wrapping_add(elapsed) as u32 & SGI_PI_GFX_INT_MASK;
        }
    }
    g.int_epoch[slot] = now;
    // At or below zero, and (the model's choice) below a nonzero bias.
    g.int_running[slot] = g.credit[slot] == 0 && g.bias[slot] > 0;
    fired
}

/// The tick at which the interrupt counter reaches the compare, or
/// u64::MAX while it does not run.
pub fn sgi_pi_gfx_int_due(g: &SGIPIGfx, slot: usize) -> u64 {
    assert!(slot < SGI_PI_GFX_SLOTS);
    if !g.int_running[slot] {
        return u64::MAX;
    }
    g.int_epoch[slot].wrapping_add(sgi_pi_gfx_int_left(g, slot))
}

thread_local! {
    /// The mark on the store a processor path is forwarding, per host
    /// thread: the forward and the head's handler run on the issuing
    /// vCPU's thread.
    static SGI_PI_GFX_FORWARDING_COUNTED: Cell<bool> = const { Cell::new(false) };
}

/// A processor interface marks each store it forwards down its I/O path;
/// returns the previous mark, to be restored with sgi_pi_gfx_unmark_store().
pub fn sgi_pi_gfx_mark_store(counted: bool) -> bool {
    SGI_PI_GFX_FORWARDING_COUNTED.with(|m| m.replace(counted))
}

pub fn sgi_pi_gfx_unmark_store(previous: bool) {
    SGI_PI_GFX_FORWARDING_COUNTED.with(|m| m.set(previous));
}

/// The one-shot read: the call clears the mark.
pub fn sgi_pi_gfx_take_mark() -> bool {
    SGI_PI_GFX_FORWARDING_COUNTED.with(|m| m.replace(false))
}

/// `credits` credits returned to `slot`.
pub fn sgi_pi_gfx_add_credits(g: &mut SGIPIGfx, slot: usize, credits: u32, who: &str) {
    assert!(slot < SGI_PI_GFX_SLOTS);
    let sum = g.credit[slot] as u64 + credits as u64;
    if sum > SGI_PI_GFX_CREDIT_MASK as u64 {
        crate::qlog!("{}: graphics credit counter {} wrapped above {:#x}\n", who, (b'A' + slot as u8) as char, SGI_PI_GFX_CREDIT_MASK);
    }
    g.credit[slot] = (sum & SGI_PI_GFX_CREDIT_MASK as u64) as u32;
    sgi_pi_gfx_rearm(g, slot);
}

/// Where a graphics node-widget map sends a widget's credits.
#[derive(Clone, Copy, Default)]
pub struct SGIPIGfxRoute {
    pub node: u32,
    pub pi: u32,
    pub slot: u32,
}

/// The Hub's map: widget 3:0, node 12:4, processor 16, valid 20. The first
/// matching entry wins.
pub fn sgi_pi_gfx_route_hub(map: &[u64; 2], widget: u32, route: &mut SGIPIGfxRoute) -> bool {
    for entry in map.iter().copied() {
        if entry & (1 << 20) == 0 || (entry & 0xf) as u32 != widget {
            continue;
        }
        route.node = ((entry >> 4) & 0x1ff) as u32;
        route.pi = 0;
        route.slot = ((entry >> 16) & 1) as u32;
        return true;
    }
    false
}

/// The Bedrock's map: widget 3:0, PI 4, node 12:5, processor 16. There is
/// no valid bit, so an entry of zero names widget 0 on node 0.
pub fn sgi_pi_gfx_route_bedrock(map: &[u64; 2], widget: u32, route: &mut SGIPIGfxRoute) -> bool {
    for entry in map.iter().copied() {
        if (entry & 0xf) as u32 != widget {
            continue;
        }
        route.node = ((entry >> 5) & 0xff) as u32;
        route.pi = ((entry >> 4) & 1) as u32;
        route.slot = ((entry >> 16) & 1) as u32;
        return true;
    }
    false
}
