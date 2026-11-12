//! The IOC3's PS/2 keyboard and mouse controller.
//!
//! Linux (`ioc3kbd.c`) and OpenBSD (`iockbc`) reach it through the main
//! register block, at 0x9c..0xac, not through the Super I/O window. Each
//! port holds up to three received characters, which software reads in one
//! word, and sends a byte at a time. The PS/2 devices are in `dev/ps2.rs`.
//!
//! A character that arrives from the host while an access holds the
//! controller is picked up by the holder when it finishes: the flag is set
//! before the lock is tried, so either the device's thread gets the lock or
//! the holder sees the flag.
//!
//! Characters reach a holding at the speed of the wire, one per
//! [`CHAR_TIME`](crate::dev::ioc3_kbc::CHAR_TIME) on each port, and a device that a clamp stopped is let go a
//! character time after the read that emptied the holding, not inside the
//! read. IRIX reads until the register comes back empty and then
//! acknowledges; a character that arrived in the few microseconds between
//! the last read and the acknowledge would latch its interrupt just before
//! the acknowledge cleared it, and the port would sit with the character
//! held and nothing to signal it. The status bit stays a plain
//! write-one-to-clear latch.
//!
//! Ported from hw/misc/sgi-ioc3.c and hw/input/sgi-ioc3-kbc.c in
//! jamesbraid/qemu-sgi-origami.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use parking_lot::Mutex;

use super::{int, usize_field, Core};
use crate::dev::deadline::{decode, encode};
use crate::dev::ioc3_kbc::Pacer;
use crate::dev::ps2::Ps2Controller;
use crate::irq::{InterruptController, IrqLine};
use crate::snapshot::{field, field_bool, field_u32, field_u8s, StateTable};
use crate::traits::Device;

pub(super) const KBC_CONTROL: u32 = 0x9c;
pub(super) const KBC_KBD_RECEIVE: u32 = 0xa0;
pub(super) const KBC_AUX_RECEIVE: u32 = 0xa4;
pub(super) const KBC_KBD_TRANSMIT: u32 = 0xa8;
pub(super) const KBC_AUX_TRANSMIT: u32 = 0xac;
pub(super) const KBC_RX_SLOTS: usize = 3;
/// Bits 16 and 17 (timeouts) read clear: no stalled line is simulated.
pub(super) const KBC_CONTROL_MASK: u32 = 0x00fc_ffff;
pub(super) const KBC_K_CLAMP_1: u32 = 0x0010_0000;
pub(super) const KBC_M_CLAMP_1: u32 = 0x0020_0000;
pub(super) const KBC_K_CLAMP_3: u32 = 0x0040_0000;
pub(super) const KBC_M_CLAMP_3: u32 = 0x0080_0000;
pub(super) const KBC_RX_SOURCE_AUX: u32 = 1 << 27;
pub(super) const KBC_RX_OVERFLOW: u32 = 1 << 28;

#[derive(Default, Clone, Copy)]
struct KbcPort {
    data: [u8; KBC_RX_SLOTS],
    count: usize,
    overflow: bool,
    /// When the port's device may send its next character.
    pace: Pacer,
}


#[derive(Default)]
struct Kbc {
    control: u32,
    port: [KbcPort; 2],
}

/// A PS/2 device model's "data waiting" line, which tells the controller a
/// character has arrived.
struct Sink(Weak<Kbd>);

impl InterruptController for Sink {
    fn set_line(&self, _line: u32, _level: bool) {
        if let Some(k) = self.0.upgrade() {
            k.poke();
        }
    }
}

pub(super) struct Kbd {
    core: Arc<Core>,
    ps2: Arc<Ps2Controller>,
    st: Mutex<Kbc>,
    /// A character arrived while an access held the controller.
    dirty: AtomicBool,
    /// The earliest of the ports' deadlines, encoded, where the timers and a
    /// lock-free check see it. Published after every access that can move one.
    wake_due: AtomicU64,
}

impl Kbd {
    pub(super) fn new(core: Arc<Core>) -> Arc<Kbd> {
        Arc::new_cyclic(|weak: &Weak<Kbd>| {
            let line = IrqLine::new(Arc::new(Sink(weak.clone())), 0);
            let ps2 = Arc::new(Ps2Controller::new(Some(line)));
            // Host key and mouse events are accepted from the start (this
            // only sets a flag); `stop` and `start` follow the chip's.
            Device::start(&*ps2);
            Kbd { core, ps2, st: Mutex::new(Kbc::default()), dirty: AtomicBool::new(false), wake_due: AtomicU64::new(0) }
        })
    }

    pub(super) fn ps2(&self) -> &Arc<Ps2Controller> {
        &self.ps2
    }

    pub(super) fn read(&self, a: u32) -> u32 {
        let mut st = self.st.lock();
        self.land_if_due(&mut st, Instant::now());
        let v = self.read_locked(&mut st, a);
        self.publish(&st);
        v
    }

    pub(super) fn write(&self, a: u32, val: u32) {
        let mut st = self.st.lock();
        self.land_if_due(&mut st, Instant::now());
        self.write_locked(&mut st, a, val);
        self.publish(&st);
    }

    /// When a queued character is next due, for the timers.
    pub(super) fn deadline(&self) -> Option<Instant> {
        decode(self.wake_due.load(Ordering::SeqCst))
    }

    /// Let the characters whose time has come land. The check is lock-free,
    /// so an access that finds nothing pending costs a load.
    pub(super) fn catch_up(&self, now: Instant) {
        if self.deadline().is_some_and(|d| d <= now) {
            let mut st = self.st.lock();
            self.land_if_due(&mut st, now);
            self.publish(&st);
        }
    }

    fn land_if_due(&self, st: &mut Kbc, now: Instant) {
        if Self::next_due(st).is_some_and(|d| d <= now) {
            self.fill(st, 0, now);
            self.fill(st, 1, now);
        }
    }

    fn next_due(st: &Kbc) -> Option<Instant> {
        st.port.iter().filter_map(|p| p.pace.deadline()).min()
    }

    fn publish(&self, st: &Kbc) {
        self.wake_due.store(encode(Self::next_due(st)), Ordering::SeqCst);
    }

    /// A device raised its line. If an access holds the controller (possibly
    /// this very thread, from inside a transmit), that access picks the
    /// character up when it finishes.
    fn poke(&self) {
        self.dirty.store(true, Ordering::SeqCst);
        self.service();
    }

    /// Pick up any character that arrived while the controller was held.
    pub(super) fn service(&self) {
        loop {
            if self.dirty.load(Ordering::SeqCst) {
                if let Some(mut st) = self.st.try_lock() {
                    if self.dirty.swap(false, Ordering::SeqCst) {
                        let now = Instant::now();
                        self.fill(&mut st, 0, now);
                        self.fill(&mut st, 1, now);
                        self.publish(&st);
                    }
                }
            }
            if !self.dirty.load(Ordering::SeqCst) || self.st.is_locked() {
                break;
            }
        }
    }

    pub(super) fn is_locked(&self) -> bool {
        self.st.is_locked()
    }

    #[cfg(test)]
    pub(super) fn poke_for_test(&self) {
        self.poke();
    }

    pub(super) fn reset(&self) {
        let mut st = self.st.lock();
        *st = Kbc::default();
        self.publish(&st);
    }

    /// Each port's pacing is saved as the time it has left, like the chip's
    /// other host-time deadlines, in keys a snapshot from before them lacks
    /// and an idle port omits.
    pub(super) fn save_state(&self, now: Instant) -> toml::Value {
        let st = self.st.lock();
        let mut kbc = StateTable::new();
        kbc.u32("control", st.control)
            .value("port0", st.port[0].save_state(now))
            .value("port1", st.port[1].save_state(now));
        kbc.build()
    }

    pub(super) fn load_state(&self, v: &toml::Value, now: Instant) -> Result<(), String> {
        let mut st = self.st.lock();
        st.control = field_u32(v, "control")?;
        st.port = [KbcPort::load_state(field(v, "port0")?, now)?, KbcPort::load_state(field(v, "port1")?, now)?];
        self.publish(&st);
        Ok(())
    }

    fn clamp(st: &Kbc, port: usize) -> usize {
        let (one, three) = if port == 1 { (KBC_M_CLAMP_1, KBC_M_CLAMP_3) } else { (KBC_K_CLAMP_1, KBC_K_CLAMP_3) };
        if st.control & one != 0 {
            1
        } else if st.control & three != 0 {
            KBC_RX_SLOTS
        } else {
            0
        }
    }

    /// Move what the device has sent into this port's holding at the speed
    /// of the wire: one character, then the next a [`CHAR_TIME`](crate::dev::ioc3_kbc::CHAR_TIME) later. A
    /// clamped port stops with its clock held low and the rest still in the
    /// device; an unclamped one loses the surplus (each loss still takes its
    /// time) and says so through the overflow flag. Every character that
    /// arrives latches the keyboard interrupt (a model choice: per character
    /// rather than a level over the holding).
    fn fill(&self, st: &mut Kbc, port: usize, now: Instant) {
        let clamp = Self::clamp(st, port);
        let mut arrived = false;
        loop {
            let p = &mut st.port[port];
            let full = clamp != 0 && p.count >= clamp;
            if !p.pace.admit(now, self.ps2.queued_from(port == 1), full) {
                break;
            }
            let Some(byte) = self.ps2.pop_from(port == 1) else { break };
            p.pace.landed(now);
            if p.count >= KBC_RX_SLOTS {
                p.overflow = true;
                continue;
            }
            p.data[p.count] = byte;
            p.count += 1;
            arrived = true;
        }
        if arrived {
            self.core.set_bits(int::KEYBOARD);
        }
    }

    /// Report what the port holds, oldest character in the highest slot, and
    /// empty the holding; the clamp releases with it, and the stopped device
    /// sends again a [`CHAR_TIME`](crate::dev::ioc3_kbc::CHAR_TIME) later, not inside this read.
    fn receive(&self, st: &mut Kbc, port: usize) -> u32 {
        let p = st.port[port];
        let mut val = 0u32;
        for (i, &b) in p.data.iter().enumerate().take(p.count) {
            let slot = (KBC_RX_SLOTS - 1) - i;
            val |= (b as u32) << (8 * slot);
            val |= 1 << (29 + slot);
        }
        if p.count != 0 && port == 1 {
            val |= KBC_RX_SOURCE_AUX;
        }
        if p.overflow {
            val |= KBC_RX_OVERFLOW;
        }
        st.port[port] = KbcPort { pace: p.pace, ..KbcPort::default() };
        // Arms the restart of a device the clamp stopped (an approximation,
        // see CHAR_TIME), so its character follows the driver's acknowledge.
        self.fill(st, port, Instant::now());
        val
    }

    fn read_locked(&self, st: &mut Kbc, a: u32) -> u32 {
        match a {
            KBC_CONTROL => st.control,
            KBC_KBD_RECEIVE => self.receive(st, 0),
            KBC_AUX_RECEIVE => self.receive(st, 1),
            _ => 0,
        }
    }

    fn write_locked(&self, st: &mut Kbc, a: u32, val: u32) {
        match a {
            KBC_CONTROL => {
                st.control = val & KBC_CONTROL_MASK;
                // Releasing a clamp lets a stopped device start again.
                let now = Instant::now();
                self.fill(st, 0, now);
                self.fill(st, 1, now);
            }
            KBC_KBD_TRANSMIT | KBC_AUX_TRANSMIT => {
                self.ps2.write_to(val as u8, a == KBC_AUX_TRANSMIT);
                let now = Instant::now();
                self.fill(st, 0, now);
                self.fill(st, 1, now);
            }
            _ => {}
        }
    }
}

impl KbcPort {
    fn save_state(&self, now: Instant) -> toml::Value {
        let mut t = StateTable::new();
        t.u8s("data", &self.data).u64("count", self.count as u64).bool("overflow", self.overflow);
        self.pace.save_into(&mut t, now);
        t.build()
    }
    fn load_state(v: &toml::Value, now: Instant) -> Result<Self, String> {
        Ok(Self {
            data: field_u8s(v, "data")?,
            count: usize_field(v, "count", KBC_RX_SLOTS)?,
            overflow: field_bool(v, "overflow")?,
            pace: Pacer::load(v, now)?,
        })
    }
}

