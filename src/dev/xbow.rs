//! Crossbow, the Octane's XIO crossbar: widget 0's registers, and which of
//! the eight links (widgets 8 to 15) are present and carrying traffic.
//!
//! Cut to what the Octane uses; registers follow the Crossbow ASIC
//! specification, revision 12.0 (`docs/Octane/`). The switch
//! translates no address: the machine routes a processor access by widget
//! number and asks [`Xbow::carries`] whether the link is up. Packets from
//! widget 0 itself are its interrupts, which go to the target its interrupt
//! destination registers name; on the Octane that is HEART.
//!
//! Which links are present and alive are atomics, so the routing decision
//! (`carries`) never takes a lock; they change only under the register
//! lock, with the registers that describe them. Interrupt packets and the
//! callbacks (link change, link reset) go out after that lock is dropped.
//!
//! Revision code 5 (the Octane2's part) adds the reset fences and the
//! resource lock; the fences are kept but gate nothing, since no warm reset
//! travels through the modelled switch.
//!
//! Two approximations: a link reset on a fitted link brings the link back
//! alive as it was before the reset (link training is not modelled, and the
//! stock PROM resets each link it finds and expects it back), and register
//! accesses are 32-bit at the data half of each eight-byte slot only.
//!
//! Ported from hw/misc/sgi-xbow.c in jamesbraid/qemu-sgi-origami.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use crate::devlog::LogModule;
use crate::irq::InterruptController;
use crate::traits::{BusDevice, BusRead16, BusRead32, BusRead64, BusRead8, BUS_ERR, BUS_OK};

/// A widget ID register: JTAG IDCODE layout, bit 0 set.
pub const fn widget_id(rev: u32, part: u32, mfgr: u32) -> u32 {
    (rev << 28) | (part << 12) | (mfgr << 1) | 1
}

/// The widget HEART answers as, where widget 0's interrupt packets go.
const WIDGET_HEART: u8 = 8;

const FIRST_WIDGET: u8 = 8;
const LINKS: usize = 8;

/// The register block's span in widget 0's window.
pub const REG_SPAN: u32 = 0x0100_0000;

const IDENTIFICATION: u32 = 0x00;
const STATUS: u32 = 0x08;
const ERR_UPPER: u32 = 0x10;
const ERR_LOWER: u32 = 0x18;
const CONTROL: u32 = 0x20;
const PACKET_TIMEOUT: u32 = 0x28;
const INT_UPPER: u32 = 0x30;
const INT_LOWER: u32 = 0x38;
const ERR_CMD: u32 = 0x40;
const LLP_CONTROL: u32 = 0x48;
const STATUS_CLEAR: u32 = 0x50;
const ARB_RELOAD: u32 = 0x58;
const PERF_A: u32 = 0x60;
const PERF_B: u32 = 0x68;
const NIC: u32 = 0x70;
/// Revision code 5 and later: widget 0's reset fence, then links 8 to 15's
/// at a stride of 8, then the resource lock and its clear.
const FENCE_BASE: u32 = 0x78;
const FENCES: usize = 1 + LINKS;
const FENCE_END: u32 = FENCE_BASE + 8 * FENCES as u32;
const LOCK: u32 = 0xc0;
const LOCK_CLEAR: u32 = 0xc8;
const FENCE_MASK: u32 = 0xff;
/// The first revision code with fences and the lock (Crossbow 2.0 in the
/// software tables; an Octane2's code-5 part shows as 1.4 in its inventory).
const REVISION_FENCES: u32 = 5;
const LINK_BASE: u32 = 0x100;
const LINK_STRIDE: u32 = 0x40;
const LINK_END: u32 = LINK_BASE + LINK_STRIDE * LINKS as u32;
const LINK_FLUSH: u32 = 0x00;
const LINK_CONTROL: u32 = 0x08;
const LINK_STATUS: u32 = 0x10;
const LINK_ARB_UPPER: u32 = 0x18;
const LINK_ARB_LOWER: u32 = 0x20;
const LINK_STATUS_CLEAR: u32 = 0x28;
const LINK_RESET: u32 = 0x30;
const LINK_AUX_STATUS: u32 = 0x38;

const STATUS_REQUEST: u32 = 1 << 23;
const STATUS_LINK_REQUESTS: u32 = 0xff00_0000;
const STATUS_ACCESS_ERROR: u32 = 1 << 5;
const STATUS_XTALK_ERROR: u32 = 1 << 2;
const STATUS_MULTIPLE_ERROR: u32 = 1 << 0;
const STATUS_WIDGET0_ERRORS: u32 = STATUS_ACCESS_ERROR | STATUS_XTALK_ERROR | STATUS_MULTIPLE_ERROR;
const STATUS_ASSERTING: u32 = STATUS_LINK_REQUESTS | STATUS_REQUEST;
const CONTROL_MASK: u32 = 0x0000_0026;
const CONTROL_CONDITIONS: u32 = STATUS_ACCESS_ERROR | STATUS_XTALK_ERROR;
const PACKET_TIMEOUT_MASK: u32 = 0x000f_ffff;
const INT_UPPER_MASK: u32 = 0xff07_ffff;
const INT_UPPER_TARGET_HIGH: u32 = 1 << 19;
/// Address bits 47:32 of the interrupt destination; the lower register
/// holds bits 31:0.
const INT_UPPER_ADDRESS: u32 = 0xffff;
/// Where in HEART's widget space an interrupt packet sets or clears a bit.
const HEART_INT_PACKET: u64 = 0x80;
const LLP_CONTROL_MASK: u32 = 0x03ff_ffff;
const LLP_CONTROL_RESET: u32 = 0x03ff_1810;
const ARB_RELOAD_MASK: u32 = 0x3f;
const ARB_RELOAD_RESET: u32 = 0x02;
const PERF_SELECT_MASK: u32 = 0x00f0_0000;
const PERF_RESET: u32 = 0x0080_0000;
const NIC_MASK: u32 = 0x000f_ffff;
const NIC_RESET: u32 = 0x02;

const LINK_ALIVE: u32 = 1 << 31;
const LINK_MAX_RETRY: u32 = 1 << 5;
const LINK_STATUS_ERRORS: u32 = 0x0007_fffb;
const LINK_ALIVE_ENABLE: u32 = 1 << 31;
const LINK_BANDWIDTH_ENABLE: u32 = 1 << 8;
const LINK_BANDWIDTH_SOURCE: u32 = 0x0000_ff00;
const LINK_DIRECT_ENABLES: u32 = 0x0003_00fb;
const LINK_FORCE_BAD: u32 = 1 << 23;
const LINK_EIGHT_BIT_FORCE: u32 = 1 << 24;
const LINK_CONTROL_MASK: u32 = (1 << 31) | (3 << 28) | (7 << 25) | (1 << 24) | (0x1f << 18) | (1 << 17) | (1 << 16) | 0x1ff;
const LINK_CREDIT_RESET: u32 = 2 << 18;
const AUX_FAILURE_MODE: u32 = 1 << 6;
const AUX_PRESENT: u32 = 1 << 5;
const AUX_EIGHT_BIT: u32 = 1 << 4;
const AUX_ERRORS: u32 = 0xffff_ff00;

struct Regs {
    status: u32,
    err_upper: u32,
    err_lower: u32,
    control: u32,
    packet_timeout: u32,
    int_upper: u32,
    int_lower: u32,
    err_cmd: u32,
    llp_control: u32,
    arb_reload: u32,
    perf_a: u32,
    perf_b: u32,
    nic: u32,
    link_control: [u32; LINKS],
    link_status: [u32; LINKS],
    link_arb_upper: [u32; LINKS],
    link_arb_lower: [u32; LINKS],
    link_aux_status: [u32; LINKS],
    /// Conditions since the link's status was last acknowledged, in its
    /// status register's bit positions.
    link_event: [u32; LINKS],
    /// Links that came up since power-on: a failed one that never did
    /// reports failure mode.
    came_up: u8,
    eight_bit: u8,
    interrupt_asserted: bool,
    /// Revision code 5 and later: which ports' warm resets may reach the
    /// core (fence 0) and each link (fences 1 to 8). Kept; iris sends no
    /// warm resets through the switch.
    fences: [u32; FENCES],
    /// The resource lock's byte.
    lock: u32,
}

impl Regs {
    /// `startup`: the links present, which come up alive.
    fn reset(startup: u8) -> Self {
        Self {
            status: 0,
            err_upper: 0,
            err_lower: 0,
            control: 0,
            packet_timeout: PACKET_TIMEOUT_MASK,
            int_upper: INT_UPPER_TARGET_HIGH,
            int_lower: 0,
            err_cmd: 0,
            llp_control: LLP_CONTROL_RESET,
            arb_reload: ARB_RELOAD_RESET,
            perf_a: PERF_RESET,
            perf_b: PERF_RESET,
            nic: NIC_RESET,
            link_control: [LINK_CREDIT_RESET; LINKS],
            link_status: [0; LINKS],
            link_arb_upper: [0; LINKS],
            link_arb_lower: [0; LINKS],
            link_aux_status: [0; LINKS],
            link_event: [0; LINKS],
            fences: [FENCE_MASK; FENCES],
            lock: 0,
            came_up: startup,
            eight_bit: 0,
            interrupt_asserted: false,
        }
    }
}

/// What an access leaves to do once the register lock is dropped.
#[derive(Default)]
struct Out {
    /// Widget 0's interrupt packets to HEART: (vector, set).
    packets: Vec<(u8, bool)>,
    /// A link reset's far-end widget.
    link_reset: Option<u8>,
}

pub struct Xbow {
    regs: Mutex<Regs>,
    revision: u32,
    /// Links with something attached, bit n for widget 8 + n.
    present: AtomicU8,
    /// Links passing traffic, as `present`. Written under `regs`.
    alive: AtomicU8,
    /// Held across "decide a packet, send it", so two threads' packets for
    /// widget 0's one interrupt arrive in the order they were decided.
    /// Never held with `regs`, and nothing but HEART is called under it.
    irq_order: Mutex<()>,
    /// Where widget 0's interrupt packets go when they target HEART.
    heart: OnceLock<Arc<dyn InterruptController>>,
    /// Told when a link comes up or goes down, with the widget and its new
    /// state, outside the register lock.
    on_link_change: OnceLock<Box<dyn Fn(u8, bool) + Send + Sync>>,
    /// Told of a link reset, with the widget, once the switch's own state
    /// has settled: the widget at the far end resets too.
    on_link_reset: OnceLock<Box<dyn Fn(u8) + Send + Sync>>,
    unsupported: Mutex<Vec<(u32, u8, bool)>>,
    /// `unsupported` holds 64: a further access costs one relaxed load.
    unsupported_full: AtomicBool,
}

fn link_bit(widget: u8) -> Option<u8> {
    (FIRST_WIDGET..FIRST_WIDGET + LINKS as u8).contains(&widget).then(|| 1 << (widget - FIRST_WIDGET))
}

impl Xbow {
    /// A Crossbow of `revision` with the links in `widgets` (a widget mask,
    /// bit n for widget n) attached and alive at power-on.
    pub fn new(revision: u32, widgets: u16) -> Self {
        let present = (widgets >> FIRST_WIDGET) as u8;
        Self {
            regs: Mutex::new(Regs::reset(present)),
            revision,
            present: AtomicU8::new(present),
            alive: AtomicU8::new(present),
            irq_order: Mutex::new(()),
            heart: OnceLock::new(),
            on_link_change: OnceLock::new(),
            on_link_reset: OnceLock::new(),
            unsupported: Mutex::new(Vec::new()),
            unsupported_full: AtomicBool::new(false),
        }
    }

    /// Where widget 0's interrupt packets go: HEART, whose line is the
    /// vector the packet names. Once, before the machine runs.
    pub fn connect_heart(&self, heart: Arc<dyn InterruptController>) {
        let _ = self.heart.set(heart);
    }

    /// What to do when a link comes up or goes down (`widget`, `alive`):
    /// the machine points the widget's windows at it or at the bus error.
    /// Called with no Crossbow lock held.
    pub fn on_link_change(&self, f: Box<dyn Fn(u8, bool) + Send + Sync>) {
        let _ = self.on_link_change.set(f);
    }

    /// What to do when a link reset reaches the widget on the far end.
    /// Called with no Crossbow lock held.
    pub fn on_link_reset(&self, f: Box<dyn Fn(u8) + Send + Sync>) {
        let _ = self.on_link_reset.set(f);
    }

    pub fn register_locks(self: &Arc<Self>) {
        use crate::locks::register_lock_fn;
        let me = self.clone();
        register_lock_fn("xbow::regs", move || me.regs.is_locked());
        let me = self.clone();
        register_lock_fn("xbow::irq_order", move || me.irq_order.is_locked());
    }

    /// Run `f` on the registers, then do what it left outside the lock:
    /// widget 0's interrupt packets, link changes, a link reset.
    fn locked<R>(&self, f: impl FnOnce(&mut Regs, &mut Out) -> R) -> R {
        let order = self.irq_order.lock();
        let before = self.alive.load(Ordering::Acquire);
        let mut out = Out::default();
        let ret = f(&mut self.regs.lock(), &mut out);
        if let Some(h) = self.heart.get() {
            for (vector, set) in out.packets {
                h.set_line(vector as u32, set);
            }
        }
        drop(order);
        let changed = before ^ self.alive.load(Ordering::Acquire);
        if let Some(cb) = self.on_link_change.get() {
            for link in (0..LINKS as u8).filter(|l| changed >> l & 1 != 0) {
                cb(FIRST_WIDGET + link, self.alive.load(Ordering::Acquire) >> link & 1 != 0);
            }
        }
        if let (Some(w), Some(cb)) = (out.link_reset, self.on_link_reset.get()) {
            cb(w);
        }
        ret
    }

    /// Fit a board on `widget` after construction (graphics) and bring its
    /// link up, as a link comes up from the registers' point of view: alive,
    /// with an alive event and the width its control forces. A reset brings
    /// it up again, and a link reset restores it, like the links fitted at
    /// construction.
    pub fn attach(&self, widget: u8) {
        if let Some(bit) = link_bit(widget) {
            self.locked(|r, out| {
                self.present.fetch_or(bit, Ordering::AcqRel);
                let alive = self.alive.load(Ordering::Acquire) | bit;
                self.set_alive_locked(r, out, alive);
            });
        }
    }

    /// Bring links up or take them down: `widgets` is the widget mask of
    /// the links that are alive from now on, bit n for widget n, a subset
    /// of those fitted. A link that comes up reports an alive event and
    /// the width its control forces; one that goes down after coming up
    /// reports maximum retry, and its windows bus-error. No machine takes
    /// a link down yet; the tests do, as QEMU's set its `links-alive`.
    pub fn set_alive(&self, widgets: u16) -> Result<(), String> {
        if widgets & !0xff00 != 0 {
            return Err("a Crossbow link mask names widgets 8 through 15".into());
        }
        self.locked(|r, out| {
            let value = (widgets >> FIRST_WIDGET) as u8;
            if value & !self.present.load(Ordering::Acquire) != 0 {
                return Err("an operational Crossbow link must be fitted".into());
            }
            self.set_alive_locked(r, out, value);
            Ok(())
        })
    }

    fn set_alive_locked(&self, r: &mut Regs, out: &mut Out, value: u8) {
        let changed = value ^ self.alive.swap(value, Ordering::AcqRel);
        for link in (0..LINKS).filter(|l| changed >> l & 1 != 0) {
            let bit = 1u8 << link;
            if value & bit != 0 {
                r.came_up |= bit;
                r.link_event[link] |= LINK_ALIVE;
                if r.link_control[link] & LINK_EIGHT_BIT_FORCE != 0 {
                    r.eight_bit |= bit;
                } else {
                    r.eight_bit &= !bit;
                }
            } else {
                r.link_event[link] |= LINK_MAX_RETRY;
            }
        }
        self.update_interrupt(r, out);
    }

    /// Whether the switch passes traffic to and from `widget`. Lock-free.
    pub fn carries(&self, widget: u8) -> bool {
        link_bit(widget).is_some_and(|b| self.alive.load(Ordering::Acquire) & b != 0)
    }

    pub fn unsupported(&self) -> Vec<(u32, u8, bool)> {
        self.unsupported.lock().clone()
    }

    fn identification(&self) -> u32 {
        // Part number and manufacturer are zero on Crossbow.
        (self.revision << 28) | 1
    }

    /// A port is up (alive) or has failed (maximum retry), before coming
    /// up or after. The third state, initializing, shows only inside a
    /// link reset's pulse, which is not kept.
    fn link_state(&self, link: usize) -> u32 {
        if self.alive.load(Ordering::Acquire) >> link & 1 != 0 {
            LINK_ALIVE
        } else {
            LINK_MAX_RETRY
        }
    }

    fn link_status(&self, r: &Regs, link: usize) -> u32 {
        r.link_status[link] | self.link_state(link)
    }

    fn link_aux(&self, r: &Regs, link: usize) -> u32 {
        let bit = 1u8 << link;
        let mut v = r.link_aux_status[link] & AUX_ERRORS;
        if self.present.load(Ordering::Acquire) & bit != 0 {
            v |= AUX_PRESENT;
        }
        if self.alive.load(Ordering::Acquire) & bit != 0 {
            if r.eight_bit & bit != 0 {
                v |= AUX_EIGHT_BIT;
            }
        } else if r.came_up & bit == 0 {
            v |= AUX_FAILURE_MODE;
        }
        v
    }

    fn link_requesting(r: &Regs, link: usize) -> bool {
        let (enables, events) = (r.link_control[link], r.link_event[link]);
        events & enables & (LINK_ALIVE_ENABLE | LINK_DIRECT_ENABLES) != 0
            || (enables & LINK_BANDWIDTH_ENABLE != 0 && events & LINK_BANDWIDTH_SOURCE != 0)
    }

    /// Recompute the request bits and, when the summary changes, queue widget
    /// 0's interrupt packet: one assertion outstanding at a time, the
    /// deassertion once every request is acknowledged.
    fn update_interrupt(&self, r: &mut Regs, out: &mut Out) {
        for link in 0..LINKS {
            if Self::link_requesting(r, link) {
                r.status |= 1 << (24 + link);
            } else {
                r.status &= !(1 << (24 + link));
            }
        }
        if r.status & r.control & CONTROL_CONDITIONS != 0 {
            r.status |= STATUS_REQUEST;
        }
        let pending = r.status & STATUS_ASSERTING != 0;
        if pending == r.interrupt_asserted {
            return;
        }
        if self.send(r, pending, out) {
            r.interrupt_asserted = pending;
        }
    }

    /// Widget 0's interrupt packet: one doubleword write of the vector, bit
    /// 8 set to raise it, to the 48-bit address the destination pair names
    /// in the target widget's own space. Not sent when the target's link
    /// carries nothing. Once sent it counts as sent whatever the target
    /// makes of it: HEART takes it only at its interrupt address (0x80) and
    /// for vectors 0 to 63, and iris carries no packet into any other
    /// widget. The target is never below 8, since its bit 19 is fixed.
    fn send(&self, r: &Regs, raise: bool, out: &mut Out) -> bool {
        let vector = (r.int_upper >> 24) as u8;
        let target = ((r.int_upper >> 16) & 0xf) as u8;
        let address = ((r.int_upper & INT_UPPER_ADDRESS) as u64) << 32 | r.int_lower as u64;
        if !self.carries(target) {
            crate::dlog!(LogModule::Xbow, "xbow: widget {target} is not reachable, so its interrupt packet goes nowhere");
            return false;
        }
        if target != WIDGET_HEART {
            crate::dlog!(LogModule::Xbow, "xbow: interrupt packet to widget {target} at {address:#x} is not carried; iris delivers packets to HEART only");
        } else if address != HEART_INT_PACKET || vector >= 64 {
            crate::dlog!(LogModule::Xbow, "xbow: HEART refuses an interrupt packet at {address:#x} for vector {vector}");
        } else if self.heart.get().is_some() {
            out.packets.push((vector, raise));
        }
        true
    }

    /// A widget 0 access error. A processor access carries no command word
    /// to capture, so the error command and address registers stay as they
    /// are; the first such error says so.
    fn record_access_error(&self, r: &mut Regs, out: &mut Out) {
        if r.status & STATUS_ACCESS_ERROR != 0 {
            r.status |= STATUS_MULTIPLE_ERROR;
        } else {
            crate::dlog!(LogModule::Xbow, "xbow: the failing access is not captured; the first-error latch holds packets only");
        }
        r.status |= STATUS_ACCESS_ERROR;
        self.update_interrupt(r, out);
    }

    fn note(&self, off: u32, size: usize, write: bool) {
        if self.unsupported_full.load(Ordering::Relaxed) {
            return;
        }
        let mut u = self.unsupported.lock();
        if !u.iter().any(|&(o, s, w)| o == off && s as usize == size && w == write) {
            u.push((off, size as u8, write));
            if u.len() == 64 {
                self.unsupported_full.store(true, Ordering::Relaxed);
            }
        }
    }

    fn accessible(&self, slot: u32, write: bool) -> bool {
        if self.revision >= REVISION_FENCES && ((FENCE_BASE..FENCE_END).contains(&slot) || slot == LOCK || slot == LOCK_CLEAR) {
            return slot % 8 == 0;
        }
        match slot {
            IDENTIFICATION | STATUS | ERR_UPPER | ERR_LOWER | STATUS_CLEAR => !write,
            CONTROL | PACKET_TIMEOUT | INT_UPPER | INT_LOWER | ERR_CMD | LLP_CONTROL | ARB_RELOAD | PERF_A
            | PERF_B | NIC => true,
            s if (LINK_BASE..LINK_END).contains(&s) => match (s - LINK_BASE) % LINK_STRIDE {
                LINK_FLUSH | LINK_STATUS | LINK_STATUS_CLEAR | LINK_AUX_STATUS => !write,
                LINK_CONTROL | LINK_ARB_UPPER | LINK_ARB_LOWER => true,
                LINK_RESET => write,
                _ => false,
            },
            _ => false,
        }
    }

    /// Decode an access to widget 0's window. Registers are 32 bits in the
    /// low half of an eight-byte slot; an eight-byte access has no
    /// established packet form and is refused without a status flag, any
    /// other malformed access is an access error.
    fn decode(&self, r: &mut Regs, out: &mut Out, off: u32, size: usize, write: bool) -> Option<u32> {
        let rw = if write { "write" } else { "read" };
        if size == 8 {
            crate::dlog!(LogModule::Xbow, "xbow: eight-byte {rw} at {off:#x} has no established packet form; refused");
            self.note(off, size, write);
            return None;
        }
        let slot = off.wrapping_sub(4);
        if size != 4 || off % 8 != 4 || !self.accessible(slot, write) {
            crate::dlog!(LogModule::Xbow, "xbow: invalid {size}-byte {rw} at {off:#x}");
            self.note(off, size, write);
            self.record_access_error(r, out);
            return None;
        }
        Some(slot)
    }

    /// A processor read of widget 0's window at `off`; `None` is a bus error.
    /// The processor's packets enter at HEART's port, so with that link down
    /// nothing reaches the registers and nothing is recorded.
    pub fn read(&self, off: u32, size: usize) -> Option<u64> {
        if !self.carries(WIDGET_HEART) {
            return None;
        }
        self.locked(|r, out| {
            let slot = self.decode(r, out, off, size, false)?;
            let v = match slot {
                IDENTIFICATION => self.identification(),
                STATUS => r.status,
                ERR_UPPER => r.err_upper,
                ERR_LOWER => r.err_lower,
                CONTROL => r.control,
                PACKET_TIMEOUT => r.packet_timeout,
                INT_UPPER => r.int_upper,
                INT_LOWER => r.int_lower,
                ERR_CMD => r.err_cmd,
                LLP_CONTROL => r.llp_control,
                STATUS_CLEAR => {
                    // Acknowledges widget 0's own errors and request; each
                    // link's request is acknowledged at its own alias.
                    let v = r.status;
                    r.status &= !(STATUS_WIDGET0_ERRORS | STATUS_REQUEST);
                    self.update_interrupt(r, out);
                    v
                }
                ARB_RELOAD => r.arb_reload,
                PERF_A => r.perf_a,
                PERF_B => r.perf_b,
                NIC => r.nic,
                s if (FENCE_BASE..FENCE_END).contains(&s) => r.fences[((s - FENCE_BASE) / 8) as usize],
                LOCK | LOCK_CLEAR => r.lock,
                s => {
                    let link = ((s - LINK_BASE) / LINK_STRIDE) as usize;
                    match (s - LINK_BASE) % LINK_STRIDE {
                        // Nothing is queued, so a flush is over before it is read.
                        LINK_FLUSH => 0,
                        LINK_CONTROL => r.link_control[link],
                        LINK_STATUS => self.link_status(r, link),
                        LINK_ARB_UPPER => r.link_arb_upper[link],
                        LINK_ARB_LOWER => r.link_arb_lower[link],
                        LINK_STATUS_CLEAR => {
                            let v = self.link_status(r, link);
                            r.link_status[link] &= !LINK_STATUS_ERRORS;
                            r.link_aux_status[link] &= !AUX_ERRORS;
                            r.link_event[link] = 0;
                            self.update_interrupt(r, out);
                            v
                        }
                        LINK_AUX_STATUS => self.link_aux(r, link),
                        _ => unreachable!(),
                    }
                }
            };
            Some(v as u64)
        })
    }

    /// A processor write; false is a bus error.
    pub fn write(&self, off: u32, size: usize, val: u64) -> bool {
        if !self.carries(WIDGET_HEART) {
            return false;
        }
        self.locked(|r, out| {
            let Some(slot) = self.decode(r, out, off, size, true) else { return false };
            let val = val as u32;
            match slot {
                CONTROL => {
                    r.control = val & CONTROL_MASK;
                    self.update_interrupt(r, out);
                }
                PACKET_TIMEOUT => r.packet_timeout = val & PACKET_TIMEOUT_MASK,
                INT_UPPER => r.int_upper = (val & INT_UPPER_MASK) | INT_UPPER_TARGET_HIGH,
                INT_LOWER => r.int_lower = val,
                ERR_CMD => {
                    // Rearms error logging; the data is discarded.
                    r.err_cmd = 0;
                    r.err_upper = 0;
                    r.err_lower = 0;
                }
                LLP_CONTROL => r.llp_control = val & LLP_CONTROL_MASK,
                ARB_RELOAD => r.arb_reload = val & ARB_RELOAD_MASK,
                // The selector is kept; nothing counts, so the count stays zero.
                PERF_A | PERF_B => {
                    crate::dlog!(LogModule::Xbow, "xbow: performance counting is not modelled");
                    *if slot == PERF_A { &mut r.perf_a } else { &mut r.perf_b } = val & PERF_SELECT_MASK;
                }
                NIC => r.nic = val & NIC_MASK,
                s if (FENCE_BASE..FENCE_END).contains(&s) => r.fences[((s - FENCE_BASE) / 8) as usize] = val & FENCE_MASK,
                // Test-and-set: stored only while the lock is free.
                LOCK => {
                    if r.lock == 0 {
                        r.lock = val & 0xff;
                    }
                }
                LOCK_CLEAR => r.lock = 0,
                s => {
                    let link = ((s - LINK_BASE) / LINK_STRIDE) as usize;
                    match (s - LINK_BASE) % LINK_STRIDE {
                        LINK_CONTROL => {
                            if val & LINK_FORCE_BAD != 0 {
                                crate::dlog!(LogModule::Xbow, "xbow: a forced bad micropacket is not modelled; the one-shot reads back zero");
                            }
                            r.link_control[link] = val & LINK_CONTROL_MASK & !LINK_FORCE_BAD;
                            self.update_interrupt(r, out);
                        }
                        // Stored; nothing arbitrates on them.
                        reg @ (LINK_ARB_UPPER | LINK_ARB_LOWER) => {
                            crate::dlog!(LogModule::Xbow, "xbow: link arbitration is not modelled");
                            *if reg == LINK_ARB_UPPER { &mut r.link_arb_upper[link] } else { &mut r.link_arb_lower[link] } = val;
                        }
                        LINK_RESET => {
                            self.link_reset(r, link, out);
                            // After the lock: the widget's reset may ask
                            // the switch about its links.
                            out.link_reset = Some(FIRST_WIDGET + link as u8);
                        }
                        _ => unreachable!(),
                    }
                }
            }
            true
        })
    }

    /// A link reset. The pulse is two core cycles, over before the
    /// processor can look again, so it is not kept. Every fitted link is in
    /// the Octane's restore set, because the stock PROM resets each link it
    /// finds and expects it back: one that was alive comes back alive with
    /// an alive event, one that was down returns to initialization failure.
    /// An empty port fails initialization. Nothing here acknowledges a
    /// standing request; the clearing aliases do that.
    fn link_reset(&self, r: &mut Regs, link: usize, out: &mut Out) {
        let bit = 1u8 << link;
        let was_alive = self.alive.load(Ordering::Acquire) & bit != 0;
        r.eight_bit &= !bit;
        if self.present.load(Ordering::Acquire) & bit != 0 {
            if was_alive {
                r.came_up |= bit;
                r.link_event[link] |= LINK_ALIVE;
                if r.link_control[link] & LINK_EIGHT_BIT_FORCE != 0 {
                    r.eight_bit |= bit;
                }
            }
        } else {
            self.alive.fetch_and(!bit, Ordering::AcqRel);
            r.came_up &= !bit;
        }
        self.update_interrupt(r, out);
    }
}

impl crate::traits::Resettable for Xbow {
    fn power_on(&self) {
        self.locked(|r, _| {
            let present = self.present.load(Ordering::Acquire);
            *r = Regs::reset(present);
            self.alive.store(present, Ordering::Release);
        });
    }
}

/// Widget 0's window as the machine's bus sees it: the full physical
/// address, of which the window's 16 MB is decoded.
const WINDOW_MASK: u64 = REG_SPAN as u64 - 1;

impl BusDevice for Xbow {
    fn read8(&self, addr: u64) -> BusRead8 {
        self.read((addr & WINDOW_MASK) as u32, 1).map_or(BusRead8::err(), |v| BusRead8::ok(v as u8))
    }

    fn read16(&self, addr: u64) -> BusRead16 {
        self.read((addr & WINDOW_MASK) as u32, 2).map_or(BusRead16::err(), |v| BusRead16::ok(v as u16))
    }

    fn read32(&self, addr: u64) -> BusRead32 {
        self.read((addr & WINDOW_MASK) as u32, 4).map_or(BusRead32::err(), |v| BusRead32::ok(v as u32))
    }

    fn read64(&self, addr: u64) -> BusRead64 {
        self.read((addr & WINDOW_MASK) as u32, 8).map_or(BusRead64::err(), BusRead64::ok)
    }

    fn write8(&self, addr: u64, val: u8) -> u32 {
        if self.write((addr & WINDOW_MASK) as u32, 1, val as u64) { BUS_OK } else { BUS_ERR }
    }

    fn write16(&self, addr: u64, val: u16) -> u32 {
        if self.write((addr & WINDOW_MASK) as u32, 2, val as u64) { BUS_OK } else { BUS_ERR }
    }

    fn write32(&self, addr: u64, val: u32) -> u32 {
        if self.write((addr & WINDOW_MASK) as u32, 4, val as u64) { BUS_OK } else { BUS_ERR }
    }

    fn write64(&self, addr: u64, val: u64) -> u32 {
        if self.write((addr & WINDOW_MASK) as u32, 8, val) { BUS_OK } else { BUS_ERR }
    }
}

#[cfg(test)]
#[path = "xbow_tests.rs"]
mod tests;
