//! Crossbow, the Octane's XIO crossbar: widget 0's registers, and which of
//! the eight links (widgets 8 to 15) are present and carrying traffic.
//!
//! Ported from hw/misc/sgi-xbow.c in
//! https://github.com/jamesbraid/qemu-sgi-origami (sgi-origami branch),
//! BSD-3-Clause.
//!
//! Cut to what the Octane uses; registers follow the Crossbow ASIC
//! specification, revision 12.0 (`docs/Octane/`). The switch
//! translates no address: the machine routes a processor access by widget
//! number and asks [`Xbow::carries`] whether the link is up. Packets from
//! widget 0 itself are its interrupts, which go to the target its interrupt
//! destination registers name; on the Octane that is HEART.
//!
//! Two approximations: a link reset on a fitted link brings the link back
//! alive as it was before the reset (link training is not modelled, and the
//! stock PROM resets each link it finds and expects it back), and register
//! accesses are 32-bit at the data half of each eight-byte slot only.

use std::sync::{Arc, Mutex};

use crate::ip30::xio::{XioInterruptSink, WIDGET_HEART};

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
    alive: u8,
    came_up: u8,
    initializing: u8,
    eight_bit: u8,
    interrupt_asserted: bool,
}

impl Regs {
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
            alive: startup,
            came_up: startup,
            initializing: 0,
            eight_bit: 0,
            interrupt_asserted: false,
        }
    }
}

/// Told of a link reset with the widget at the far end.
type LinkResetHook = Box<dyn Fn(u8) + Send + Sync>;

pub struct Xbow {
    regs: Mutex<Regs>,
    revision: u32,
    /// Links with something attached, bit n for widget 8 + n.
    present: Mutex<u8>,
    /// Where widget 0's interrupt packets go when they target HEART.
    heart: Mutex<Option<Arc<dyn XioInterruptSink>>>,
    /// Told of a link reset, with the widget, once the switch's own state
    /// has settled: the widget at the far end resets too.
    on_link_reset: Mutex<Option<LinkResetHook>>,
    unsupported: Mutex<Vec<(u32, u8, bool)>>,
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
            present: Mutex::new(present),
            heart: Mutex::new(None),
            on_link_reset: Mutex::new(None),
            unsupported: Mutex::new(Vec::new()),
        }
    }

    pub fn connect_heart(&self, sink: Arc<dyn XioInterruptSink>) {
        *self.heart.lock().unwrap() = Some(sink);
    }

    /// What to do when a link reset reaches the widget on the far end.
    pub fn on_link_reset(&self, f: Box<dyn Fn(u8) + Send + Sync>) {
        *self.on_link_reset.lock().unwrap() = Some(f);
    }

    /// Fit a board on `widget` after construction (graphics), alive.
    pub fn attach(&self, widget: u8) {
        if let Some(bit) = link_bit(widget) {
            *self.present.lock().unwrap() |= bit;
            let mut r = self.regs.lock().unwrap();
            r.alive |= bit;
            r.came_up |= bit;
        }
    }

    /// Whether the switch passes traffic to and from `widget`.
    pub fn carries(&self, widget: u8) -> bool {
        link_bit(widget).is_some_and(|b| self.regs.lock().unwrap().alive & b != 0)
    }

    pub fn unsupported(&self) -> Vec<(u32, u8, bool)> {
        self.unsupported.lock().unwrap().clone()
    }

    fn identification(&self) -> u32 {
        // Part number and manufacturer are zero on Crossbow.
        (self.revision << 28) | 1
    }

    fn link_state(r: &Regs, link: usize) -> u32 {
        let bit = 1u8 << link;
        if r.alive & bit != 0 {
            LINK_ALIVE
        } else if r.initializing & bit != 0 {
            0
        } else {
            LINK_MAX_RETRY
        }
    }

    fn link_status(r: &Regs, link: usize) -> u32 {
        r.link_status[link] | Self::link_state(r, link)
    }

    fn link_aux(&self, r: &Regs, link: usize) -> u32 {
        let bit = 1u8 << link;
        let mut v = r.link_aux_status[link] & AUX_ERRORS;
        if *self.present.lock().unwrap() & bit != 0 {
            v |= AUX_PRESENT;
        }
        if r.alive & bit != 0 {
            if r.eight_bit & bit != 0 {
                v |= AUX_EIGHT_BIT;
            }
        } else if r.initializing & bit == 0 && r.came_up & bit == 0 {
            v |= AUX_FAILURE_MODE;
        }
        v
    }

    fn link_requesting(r: &Regs, link: usize) -> bool {
        let (enables, events) = (r.link_control[link], r.link_event[link]);
        events & enables & (LINK_ALIVE_ENABLE | LINK_DIRECT_ENABLES) != 0
            || (enables & LINK_BANDWIDTH_ENABLE != 0 && events & LINK_BANDWIDTH_SOURCE != 0)
    }

    /// Recompute the request bits and, when the summary changes, send widget
    /// 0's interrupt packet: one assertion outstanding at a time, the
    /// deassertion once every request is acknowledged. A packet to anything
    /// but HEART goes nowhere on this machine.
    fn update_interrupt(&self, r: &mut Regs) {
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
        let target = ((r.int_upper >> 16) & 0xf) as u8;
        let vector = (r.int_upper >> 24) as u8;
        if target != WIDGET_HEART || r.alive & link_bit(target).unwrap() == 0 {
            return;
        }
        if let Some(h) = self.heart.lock().unwrap().as_ref() {
            h.xio_interrupt(vector, pending);
            r.interrupt_asserted = pending;
        }
    }

    fn record_access_error(&self, r: &mut Regs) {
        if r.status & STATUS_ACCESS_ERROR != 0 {
            r.status |= STATUS_MULTIPLE_ERROR;
        }
        r.status |= STATUS_ACCESS_ERROR;
        self.update_interrupt(r);
    }

    fn note(&self, off: u32, size: usize, write: bool) {
        let mut u = self.unsupported.lock().unwrap();
        if u.len() < 64 && !u.iter().any(|&(o, s, w)| o == off && s as usize == size && w == write) {
            u.push((off, size as u8, write));
        }
    }

    fn accessible(slot: u32, write: bool) -> bool {
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
    fn decode(&self, off: u32, size: usize, write: bool) -> Option<u32> {
        if size == 8 {
            self.note(off, size, write);
            return None;
        }
        let slot = off.wrapping_sub(4);
        if size != 4 || off % 8 != 4 || !Self::accessible(slot, write) {
            self.note(off, size, write);
            let mut r = self.regs.lock().unwrap();
            self.record_access_error(&mut r);
            return None;
        }
        Some(slot)
    }

    /// A processor read of widget 0's window at `off`; `None` is a bus error.
    pub fn read(&self, off: u32, size: usize) -> Option<u64> {
        let slot = self.decode(off, size, false)?;
        let mut r = self.regs.lock().unwrap();
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
                self.update_interrupt(&mut r);
                v
            }
            ARB_RELOAD => r.arb_reload,
            PERF_A => r.perf_a,
            PERF_B => r.perf_b,
            NIC => r.nic,
            s => {
                let link = ((s - LINK_BASE) / LINK_STRIDE) as usize;
                match (s - LINK_BASE) % LINK_STRIDE {
                    // Nothing is queued, so a flush is over before it is read.
                    LINK_FLUSH => 0,
                    LINK_CONTROL => r.link_control[link],
                    LINK_STATUS => Self::link_status(&r, link),
                    LINK_ARB_UPPER => r.link_arb_upper[link],
                    LINK_ARB_LOWER => r.link_arb_lower[link],
                    LINK_STATUS_CLEAR => {
                        let v = Self::link_status(&r, link);
                        r.link_status[link] &= !LINK_STATUS_ERRORS;
                        r.link_aux_status[link] &= !AUX_ERRORS;
                        r.link_event[link] = 0;
                        self.update_interrupt(&mut r);
                        v
                    }
                    LINK_AUX_STATUS => self.link_aux(&r, link),
                    _ => unreachable!(),
                }
            }
        };
        Some(v as u64)
    }

    /// A processor write; false is a bus error.
    pub fn write(&self, off: u32, size: usize, val: u64) -> bool {
        let Some(slot) = self.decode(off, size, true) else { return false };
        let val = val as u32;
        let mut r = self.regs.lock().unwrap();
        match slot {
            CONTROL => {
                r.control = val & CONTROL_MASK;
                self.update_interrupt(&mut r);
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
            // The selector is kept; nothing counts.
            PERF_A => r.perf_a = val & PERF_SELECT_MASK,
            PERF_B => r.perf_b = val & PERF_SELECT_MASK,
            NIC => r.nic = val & NIC_MASK,
            s => {
                let link = ((s - LINK_BASE) / LINK_STRIDE) as usize;
                match (s - LINK_BASE) % LINK_STRIDE {
                    LINK_CONTROL => {
                        r.link_control[link] = val & LINK_CONTROL_MASK & !LINK_FORCE_BAD;
                        self.update_interrupt(&mut r);
                    }
                    // Stored; nothing arbitrates on them.
                    LINK_ARB_UPPER => r.link_arb_upper[link] = val,
                    LINK_ARB_LOWER => r.link_arb_lower[link] = val,
                    LINK_RESET => {
                        self.link_reset(&mut r, link);
                        // Outside the lock: the widget's reset may ask
                        // the switch about its links.
                        drop(r);
                        if let Some(f) = self.on_link_reset.lock().unwrap().as_ref() {
                            f(FIRST_WIDGET + link as u8);
                        }
                        return true;
                    }
                    _ => unreachable!(),
                }
            }
        }
        true
    }

    /// A link reset. The pulse is two core cycles, over before the
    /// processor can look again, so it is not kept. A fitted link that was
    /// alive comes back alive with an alive event, as the Octane's
    /// restore-alive policy has it; one that was down stays down, still
    /// initializing; an empty port fails initialization.
    fn link_reset(&self, r: &mut Regs, link: usize) {
        let bit = 1u8 << link;
        let was_alive = r.alive & bit != 0;
        r.eight_bit &= !bit;
        if *self.present.lock().unwrap() & bit != 0 {
            if was_alive {
                r.initializing &= !bit;
                r.came_up |= bit;
                r.link_event[link] |= LINK_ALIVE;
                if r.link_control[link] & LINK_EIGHT_BIT_FORCE != 0 {
                    r.eight_bit |= bit;
                }
            } else {
                r.initializing |= bit;
            }
        } else {
            r.alive &= !bit;
            r.initializing &= !bit;
            r.came_up &= !bit;
        }
        self.update_interrupt(r);
    }
}

impl crate::traits::Resettable for Xbow {
    fn power_on(&self) {
        let present = *self.present.lock().unwrap();
        *self.regs.lock().unwrap() = Regs::reset(present);
    }
}

/// The switch's registers and each link's state. Which ports have a board
/// is the machine's build, not state, so a load keeps this Crossbow's. The
/// interrupt line to HEART is not driven: HEART's own state carries the
/// bit, and a load restores HEART after the Crossbow.
impl crate::traits::Saveable for Xbow {
    fn save_state(&self) -> toml::Value {
        use crate::snapshot::{hex_u32, hex_u8, u32_slice_to_toml};
        let r = self.regs.lock().unwrap();
        let mut t = toml::map::Map::new();
        for (k, v) in [
            ("status", r.status),
            ("err_upper", r.err_upper),
            ("err_lower", r.err_lower),
            ("control", r.control),
            ("packet_timeout", r.packet_timeout),
            ("int_upper", r.int_upper),
            ("int_lower", r.int_lower),
            ("err_cmd", r.err_cmd),
            ("llp_control", r.llp_control),
            ("arb_reload", r.arb_reload),
            ("perf_a", r.perf_a),
            ("perf_b", r.perf_b),
            ("nic", r.nic),
        ] {
            t.insert(k.into(), hex_u32(v));
        }
        for (k, v) in [
            ("link_control", &r.link_control),
            ("link_status", &r.link_status),
            ("link_arb_upper", &r.link_arb_upper),
            ("link_arb_lower", &r.link_arb_lower),
            ("link_aux_status", &r.link_aux_status),
            ("link_event", &r.link_event),
        ] {
            t.insert(k.into(), u32_slice_to_toml(v));
        }
        for (k, v) in [("alive", r.alive), ("came_up", r.came_up), ("initializing", r.initializing), ("eight_bit", r.eight_bit)] {
            t.insert(k.into(), hex_u8(v));
        }
        t.insert("interrupt_asserted".into(), toml::Value::Boolean(r.interrupt_asserted));
        toml::Value::Table(t)
    }

    fn load_state(&self, v: &toml::Value) -> Result<(), String> {
        use crate::snapshot::{get_field, load_u32_slice, toml_bool, toml_u32, toml_u8};
        let mut r = self.regs.lock().unwrap();
        *r = Regs::reset(*self.present.lock().unwrap());
        let d = |k: &str, def: u32| get_field(v, k).and_then(toml_u32).unwrap_or(def);
        r.status = d("status", r.status);
        r.err_upper = d("err_upper", r.err_upper);
        r.err_lower = d("err_lower", r.err_lower);
        r.control = d("control", r.control);
        r.packet_timeout = d("packet_timeout", r.packet_timeout);
        r.int_upper = d("int_upper", r.int_upper);
        r.int_lower = d("int_lower", r.int_lower);
        r.err_cmd = d("err_cmd", r.err_cmd);
        r.llp_control = d("llp_control", r.llp_control);
        r.arb_reload = d("arb_reload", r.arb_reload);
        r.perf_a = d("perf_a", r.perf_a);
        r.perf_b = d("perf_b", r.perf_b);
        r.nic = d("nic", r.nic);
        let regs = &mut *r;
        for (k, dst) in [
            ("link_control", &mut regs.link_control),
            ("link_status", &mut regs.link_status),
            ("link_arb_upper", &mut regs.link_arb_upper),
            ("link_arb_lower", &mut regs.link_arb_lower),
            ("link_aux_status", &mut regs.link_aux_status),
            ("link_event", &mut regs.link_event),
        ] {
            if let Some(a) = get_field(v, k) {
                load_u32_slice(a, dst);
            }
        }
        let b = |k: &str, def: u8| get_field(v, k).and_then(toml_u8).unwrap_or(def);
        r.alive = b("alive", r.alive);
        r.came_up = b("came_up", r.came_up);
        r.initializing = b("initializing", 0);
        r.eight_bit = b("eight_bit", 0);
        r.interrupt_asserted = get_field(v, "interrupt_asserted").and_then(toml_bool).unwrap_or(false);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct Sink(AtomicU64);
    impl XioInterruptSink for Sink {
        fn xio_interrupt(&self, vector: u8, set: bool) {
            let bit = 1u64 << vector;
            if set {
                self.0.fetch_or(bit, Ordering::SeqCst);
            } else {
                self.0.fetch_and(!bit, Ordering::SeqCst);
            }
        }
    }

    fn octane() -> Xbow {
        Xbow::new(4, 0x8100)
    }

    #[test]
    fn identification_and_link_status() {
        let x = octane();
        assert_eq!(x.read(0x04, 4), Some(0x4000_0001));
        // HEART (8) and Bridge (15) alive; 11 empty.
        assert_eq!(x.read(LINK_BASE + 4 * 0 + LINK_STATUS + 4, 4).unwrap() as u32 & LINK_ALIVE, LINK_ALIVE);
        assert_eq!(x.read(LINK_BASE + LINK_STRIDE * 7 + LINK_STATUS + 4, 4).unwrap() as u32 & LINK_ALIVE, LINK_ALIVE);
        assert_eq!(x.read(LINK_BASE + LINK_STRIDE * 3 + LINK_STATUS + 4, 4).unwrap() as u32, LINK_MAX_RETRY);
        let aux = x.read(LINK_BASE + LINK_STRIDE * 3 + LINK_AUX_STATUS + 4, 4).unwrap() as u32;
        assert_eq!(aux & (AUX_PRESENT | AUX_FAILURE_MODE), AUX_FAILURE_MODE);
        assert!(x.carries(8) && x.carries(15) && !x.carries(11) && !x.carries(3));
    }

    #[test]
    fn malformed_accesses_are_refused() {
        let x = octane();
        assert_eq!(x.read(0x00, 4), None);
        assert_eq!(x.read(0x00, 8), None);
        assert_eq!(x.read(STATUS + 4, 4).unwrap() as u32 & STATUS_ACCESS_ERROR, STATUS_ACCESS_ERROR);
        assert!(!x.write(IDENTIFICATION + 4, 4, 0));
        assert_eq!(x.read(STATUS + 4, 4).unwrap() as u32 & STATUS_MULTIPLE_ERROR, STATUS_MULTIPLE_ERROR);
        let s = x.read(STATUS_CLEAR + 4, 4).unwrap() as u32;
        assert_ne!(s & STATUS_ACCESS_ERROR, 0);
        assert_eq!(x.read(STATUS + 4, 4).unwrap() as u32 & STATUS_WIDGET0_ERRORS, 0);
    }

    #[test]
    fn a_link_reset_restores_a_live_link() {
        let x = octane();
        let sink = Arc::new(Sink::default());
        x.connect_heart(sink.clone());
        // Interrupt to HEART, vector 0x2a, alive events enabled on link 15.
        assert!(x.write(INT_UPPER + 4, 4, (0x2a << 24) | (8 << 16)));
        let ctl = LINK_BASE + LINK_STRIDE * 7 + LINK_CONTROL + 4;
        assert!(x.write(ctl, 4, LINK_ALIVE_ENABLE as u64));
        assert!(x.write(LINK_BASE + LINK_STRIDE * 7 + LINK_RESET + 4, 4, 0));
        assert!(x.carries(15));
        assert_eq!(sink.0.load(Ordering::SeqCst), 1 << 0x2a);
        assert_ne!(x.read(STATUS + 4, 4).unwrap() as u32 & (1 << 31), 0);
        x.read(LINK_BASE + LINK_STRIDE * 7 + LINK_STATUS_CLEAR + 4, 4).unwrap();
        assert_eq!(sink.0.load(Ordering::SeqCst), 0);
        // An empty port stays down.
        assert!(x.write(LINK_BASE + LINK_STRIDE * 3 + LINK_RESET + 4, 4, 0));
        assert!(!x.carries(11));
    }

    #[test]
    fn save_load_round_trip() {
        use crate::traits::Saveable;
        let x = octane();
        assert!(x.write(INT_UPPER + 4, 4, (0x2a << 24) | (8 << 16)));
        assert!(x.write(INT_LOWER + 4, 4, 0x1234));
        assert!(x.write(LINK_BASE + LINK_STRIDE * 7 + LINK_CONTROL + 4, 4, LINK_ALIVE_ENABLE as u64));
        assert!(x.write(LINK_BASE + LINK_STRIDE * 7 + LINK_RESET + 4, 4, 0));
        assert_eq!(x.read(0x00, 4), None, "an access error latched in status");
        let v1 = x.save_state();
        let y = octane();
        y.load_state(&v1).unwrap();
        assert_eq!(v1, y.save_state(), "Xbow save_state mismatch after load_state round-trip");
        assert_eq!(y.read(INT_LOWER + 4, 4), Some(0x1234));
        assert!(y.carries(15) && !y.carries(11));
        assert_eq!(x.read(STATUS + 4, 4), y.read(STATUS + 4, 4));
    }

    #[test]
    fn reset_restores_startup_links() {
        let x = octane();
        x.attach(11);
        assert!(x.carries(11));
        assert!(x.write(INT_LOWER + 4, 4, 0x1234));
        crate::traits::Resettable::power_on(&x);
        assert!(x.carries(11));
        assert_eq!(x.read(INT_LOWER + 4, 4), Some(0));
        assert_eq!(x.read(INT_UPPER + 4, 4), Some(INT_UPPER_TARGET_HIGH as u64));
        assert_eq!(x.read(LLP_CONTROL + 4, 4), Some(LLP_CONTROL_RESET as u64));
    }
}
