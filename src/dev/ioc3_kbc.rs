//! SGI IOC3 PS/2 keyboard and mouse controller.
//!
//! A block of the IOC3 that the IOC4 carries too, at another place in its
//! BAR. It is a bit-level PS/2 line engine, not an 8042: five 32-bit
//! registers, a control and status register, a receive register per port
//! and a transmit register per port. Each port holds up to three received
//! characters; the clamp bits in the control register stop a port's device
//! once it has sent one or three, and with no clamp armed a fourth
//! character is lost and the receive register reports the overflow.
//!
//! The host's keyboard and mouse model is a [`Ps2Controller`], a byte queue
//! shared by both ports. This block drains it into the per-port holdings.
//! The chip around it owns the interrupt: every call that can move a
//! character in says so, and the chip latches its own source bit.
//!
//! # What is approximated here
//!
//! * The data-available line is asserted for every character that reaches a
//!   holding, not once per drain. The silicon may pulse per character or hold
//!   a level over the holding, and no public driver tells them apart.
//! * A device stopped by a clamp resumes [`RESUME_DELAY`] after the read that
//!   empties the holding, so its characters arrive after the guest's
//!   acknowledge (IRIX reads until empty, then acknowledges). The delay is one
//!   PS/2 character time, not a measured value: 11 bits at the 10 to 16.7 kHz
//!   clock of Chapweske's public PS/2 protocol note, taken as 1 ms.
//! * The control register's timeout bits (16, 17) and the hardware status
//!   bits read clear: no stalled line is simulated, and a transmit completes
//!   inside the write.
//! * Reset values and the register's endianness are the emulator's choice.
//!
//! Ported from hw/input/sgi-ioc3-kbc.c in jamesbraid/qemu-sgi-origami. QEMU's
//! two `ps2` devices are the shared [`Ps2Controller`] here, and its
//! re-entrancy guard around a fill is the owner's poke flag.

use crate::dev::ioc3::{due, until, usize_field};
use crate::snapshot::{field, field_bool, field_u32, field_u64, field_u8s, StateTable};
use std::sync::Weak;
use std::time::{Duration, Instant};

use crate::dev::ps2::Ps2Controller;
use crate::irq::InterruptController;

/// Register offsets within the controller's window.
pub mod reg {
    /// Control and status.
    pub const CONTROL: u32 = 0x00;
    /// Keyboard receive (read-only).
    pub const KBD_RECEIVE: u32 = 0x04;
    /// Mouse receive (read-only).
    pub const AUX_RECEIVE: u32 = 0x08;
    /// Keyboard transmit (write-only).
    pub const KBD_TRANSMIT: u32 = 0x0c;
    /// Mouse transmit (write-only).
    pub const AUX_TRANSMIT: u32 = 0x10;
    pub const MMIO_SIZE: u32 = AUX_TRANSMIT + 4;
}

/// How long a stopped device takes to send again once a read has emptied the
/// holding that clamped it.
pub const RESUME_DELAY: Duration = Duration::from_millis(1);

/// Characters a port holds.
pub const RX_SLOTS: usize = 3;
pub const PORT_KBD: usize = 0;
pub const PORT_AUX: usize = 1;

/// Control bits, from the public Linux header.
pub mod ctrl {
    pub const EMULATION: u32 = 1 << 12;
    pub const FAST_CLOCK: u32 = 1 << 13;
    pub const K_TO_EN: u32 = 1 << 18;
    pub const M_TO_EN: u32 = 1 << 19;
    pub const K_CLAMP_1: u32 = 1 << 20;
    pub const M_CLAMP_1: u32 = 1 << 21;
    pub const K_CLAMP_3: u32 = 1 << 22;
    pub const M_CLAMP_3: u32 = 1 << 23;
    pub const CLAMP_MASK: u32 = 0x00f0_0000;
    /// Writable bits: the low half, the timeout enables and the clamps.
    pub const MASK: u32 = 0x0000_ffff | K_TO_EN | M_TO_EN | CLAMP_MASK;
}

/// Receive register: bit 27 says the characters came from the mouse, bit 28
/// that one arrived with the slots full and was lost.
pub const RX_SOURCE_AUX: u32 = 1 << 27;
pub const RX_OVERFLOW: u32 = 1 << 28;

/// The valid flag of receive slot `slot` (2 is the oldest character).
pub const fn rx_valid(slot: usize) -> u32 {
    1 << (29 + slot)
}

/// What one port holds: the characters it has taken, oldest first, and
/// whether one was lost for want of room.
#[derive(Default, Clone, Copy)]
struct Holding {
    data: [u8; RX_SLOTS],
    count: usize,
    overflow: bool,
}

/// A chip that owns a [`Ioc3Kbc`] and wants to know when the host's PS/2
/// model has a character.
pub trait KbcOwner: Send + Sync {
    /// A device raised its line. If an access holds the chip (possibly this
    /// very thread, from inside a transmit), that access picks the character
    /// up when it finishes.
    fn kbc_poke(&self);
}

/// The line a PS/2 model drives when it has something to send.
pub struct KbcLine<O: KbcOwner>(pub Weak<O>);

impl<O: KbcOwner> InterruptController for KbcLine<O> {
    fn set_line(&self, _line: u32, _level: bool) {
        if let Some(owner) = self.0.upgrade() {
            owner.kbc_poke();
        }
    }
}

pub struct Ioc3Kbc {
    /// Board wiring: whether the connectors are fitted.
    ports_present: bool,
    control: u32,
    port: [Holding; 2],
    /// When a read that released a clamp lets the stopped devices send again.
    resume: Option<Instant>,
}

impl Ioc3Kbc {
    pub fn new(ports_present: bool) -> Ioc3Kbc {
        Ioc3Kbc { ports_present, control: 0, port: [Holding::default(); 2], resume: None }
    }

    /// Reset: the control register and both holdings clear, and a pending
    /// release goes with them.
    pub fn reset(&mut self) {
        self.control = 0;
        self.port = [Holding::default(); 2];
        self.resume = None;
    }

    /// When the stopped devices are next let go, for the owner's timers.
    pub fn deadline(&self) -> Option<Instant> {
        self.resume
    }

    /// Let the stopped devices send if their character time has passed.
    /// Returns whether a character arrived.
    pub fn catch_up(&mut self, now: Instant, ps2: &Ps2Controller) -> bool {
        if self.resume.is_some_and(|d| d <= now) {
            self.resume = None;
            return self.fill_all(ps2);
        }
        false
    }

    /// How many characters this port may take before its clock line is held
    /// low. Zero means no clamp is armed, and then the controller has no way
    /// to stop the device. Arming both clamps is not a combination any driver
    /// writes, so the tighter one is taken.
    fn clamp(&self, port: usize) -> usize {
        let (one, three) = if port == PORT_AUX {
            (ctrl::M_CLAMP_1, ctrl::M_CLAMP_3)
        } else {
            (ctrl::K_CLAMP_1, ctrl::K_CLAMP_3)
        };
        if self.control & one != 0 {
            1
        } else if self.control & three != 0 {
            RX_SLOTS
        } else {
            0
        }
    }

    /// Move what the device has sent into this port's holding, up to what
    /// the clamp allows. A clamped port stops with its clock held low and the
    /// rest still in the device; an unclamped one loses the surplus and says
    /// so through the overflow flag. Returns whether a character arrived,
    /// which is when the chip latches its source.
    pub fn fill(&mut self, port: usize, ps2: &Ps2Controller) -> bool {
        if !self.ports_present {
            return false;
        }
        let clamp = self.clamp(port);
        let mut arrived = false;
        loop {
            if clamp != 0 && self.port[port].count >= clamp {
                break;
            }
            let Some(byte) = ps2.pop_from(port == PORT_AUX) else { break };
            let p = &mut self.port[port];
            if p.count >= RX_SLOTS {
                p.overflow = true;
                continue;
            }
            p.data[p.count] = byte;
            p.count += 1;
            arrived = true;
        }
        arrived
    }

    /// Fill both ports.
    pub fn fill_all(&mut self, ps2: &Ps2Controller) -> bool {
        let kbd = self.fill(PORT_KBD, ps2);
        let aux = self.fill(PORT_AUX, ps2);
        kbd || aux
    }

    /// Report what the port holds, oldest character in the highest slot, and
    /// empty the holding: the register packs the characters since the last
    /// read, so the read clears them, their flags and the overflow flag
    /// together. The clamp releases with it, but the stopped device sends
    /// again only [`RESUME_DELAY`] after `now`: see [`Self::catch_up`].
    fn receive(&mut self, port: usize, ps2: &Ps2Controller, now: Instant) -> u32 {
        let p = self.port[port];
        let mut val = 0u32;
        for (i, &b) in p.data.iter().enumerate().take(p.count) {
            let slot = (RX_SLOTS - 1) - i;
            val |= u32::from(b) << (8 * slot);
            val |= rx_valid(slot);
            // The per-character error flag (bit 24 + slot) stays clear: the
            // PS/2 layer delivers clean bytes.
        }
        if p.count != 0 && port == PORT_AUX {
            val |= RX_SOURCE_AUX;
        }
        if p.overflow {
            val |= RX_OVERFLOW;
        }
        self.port[port] = Holding::default();
        if p.count != 0 && self.resume.is_none() && ps2.queued() {
            self.resume = Some(now + RESUME_DELAY);
        }
        val
    }

    /// A 32-bit read at `off` at time `now`. A read leaves nothing arriving:
    /// what it releases comes at [`Self::deadline`].
    pub fn read(&mut self, off: u32, ps2: &Ps2Controller, now: Instant) -> u32 {
        match off {
            // Write-pending and the other status bits read clear: a
            // synchronous transmit completes within the register write.
            reg::CONTROL => self.control,
            reg::KBD_RECEIVE => self.receive(PORT_KBD, ps2, now),
            reg::AUX_RECEIVE => self.receive(PORT_AUX, ps2, now),
            _ => {
                crate::dlog!(crate::devlog::LogModule::Ioc, "ioc3-kbc: read of write-only register {off:#x}");
                0
            }
        }
    }

    /// A 32-bit write at `off`. Returns whether a character arrived.
    pub fn write(&mut self, off: u32, val: u32, ps2: &Ps2Controller) -> bool {
        match off {
            reg::CONTROL => {
                self.control = val & ctrl::MASK;
                if val & (ctrl::EMULATION | ctrl::FAST_CLOCK) != 0 {
                    crate::dlog!(crate::devlog::LogModule::Ioc, "ioc3-kbc: emulation/fast-clock effects are not modeled");
                }
                // Releasing a clamp lets a stopped device send again.
                self.fill_all(ps2)
            }
            reg::KBD_TRANSMIT | reg::AUX_TRANSMIT => {
                if !self.ports_present {
                    crate::dlog!(crate::devlog::LogModule::Ioc, "ioc3-kbc: transmit on an unwired port");
                    return false;
                }
                ps2.write_to(val as u8, off == reg::AUX_TRANSMIT);
                self.fill_all(ps2)
            }
            _ => {
                crate::dlog!(crate::devlog::LogModule::Ioc, "ioc3-kbc: write of {val:#x} to read-only register {off:#x}");
                false
            }
        }
    }
}

// The controller's state for a snapshot of the IOC3 that carries it: the
// control register and each port's held characters. Whether the connectors
// are fitted is the board's.

impl Holding {
    fn save_state(&self) -> toml::Value {
        StateTable::new()
            .u8s("data", &self.data)
            .u64("count", self.count as u64)
            .bool("overflow", self.overflow)
            .build()
    }
    fn load_state(v: &toml::Value) -> Result<Self, String> {
        Ok(Self {
            data: field_u8s(v, "data")?,
            count: usize_field(v, "count", RX_SLOTS)?,
            overflow: field_bool(v, "overflow")?,
        })
    }
}

impl Ioc3Kbc {
    /// A pending release is saved as the time it has left, and the key is
    /// absent when none is pending, so a state without it loads as before.
    pub fn save_state(&self, now: Instant) -> toml::Value {
        let mut t = StateTable::new();
        t.u32("control", self.control).value("port0", self.port[0].save_state()).value("port1", self.port[1].save_state());
        if self.resume.is_some() {
            t.u64("resume", until(self.resume, now));
        }
        t.build()
    }

    /// The controller `save_state` described, on this board's connectors.
    pub fn loaded(&self, v: &toml::Value, now: Instant) -> Result<Ioc3Kbc, String> {
        let left = if v.get("resume").is_some() { field_u64(v, "resume")? } else { 0 };
        Ok(Ioc3Kbc {
            ports_present: self.ports_present,
            control: field_u32(v, "control")?,
            port: [Holding::load_state(field(v, "port0")?)?, Holding::load_state(field(v, "port1")?)?],
            resume: due(left, now),
        })
    }
}

#[cfg(test)]
mod tests {
    // Cases follow the sgi-ioc3-kbc qtests in jamesbraid/qemu-sgi-origami:
    // clamp-after-one-splits-a-reply, the unclamped overflow, the resume
    // timer and the migration of a pending resume.
    use super::*;

    fn kbc() -> (Ioc3Kbc, Ps2Controller) {
        (Ioc3Kbc::new(true), Ps2Controller::new(None))
    }

    /// Clamping after one character splits a reset reply: the acknowledge
    /// first and the self-test byte next, each alone in the oldest slot. The
    /// read that empties the holding does not send the next at once: the
    /// stopped keyboard needs a character time.
    #[test]
    fn clamp_after_one_splits_a_reset_reply() {
        let (mut k, ps2) = kbc();
        let t0 = Instant::now();
        assert!(!k.write(reg::CONTROL, ctrl::K_CLAMP_1, &ps2));
        assert!(k.write(reg::KBD_TRANSMIT, 0xff, &ps2), "the acknowledge arrives");
        assert_eq!(k.read(reg::KBD_RECEIVE, &ps2, t0), 0x80fa_0000);
        assert_eq!(k.read(reg::KBD_RECEIVE, &ps2, t0), 0, "the self-test byte is still in the keyboard");
        assert!(!k.catch_up(t0 + RESUME_DELAY - Duration::from_nanos(1), &ps2), "not before its time");
        assert!(k.catch_up(t0 + RESUME_DELAY, &ps2), "the self-test byte follows");
        assert_eq!(k.read(reg::KBD_RECEIVE, &ps2, t0 + RESUME_DELAY), 0x80aa_0000);
        assert_eq!(k.read(reg::KBD_RECEIVE, &ps2, t0 + RESUME_DELAY), 0);
        assert_eq!(k.deadline(), None, "nothing left behind the clamp, so nothing armed");
    }

    /// The read that empties a holding while the device has more arms one
    /// release; a read with nothing behind the clamp arms none, and a read of
    /// an empty holding arms none.
    #[test]
    fn only_a_read_that_frees_a_stopped_device_arms_the_release() {
        let (mut k, ps2) = kbc();
        let t0 = Instant::now();
        k.write(reg::CONTROL, ctrl::K_CLAMP_1, &ps2);
        assert_eq!(k.read(reg::KBD_RECEIVE, &ps2, t0), 0);
        assert_eq!(k.deadline(), None, "an empty holding frees nothing");
        k.write(reg::KBD_TRANSMIT, 0xff, &ps2);
        k.read(reg::KBD_RECEIVE, &ps2, t0);
        assert_eq!(k.deadline(), Some(t0 + RESUME_DELAY));
        // A second read before the release does not push it out.
        k.read(reg::KBD_RECEIVE, &ps2, t0 + RESUME_DELAY / 2);
        assert_eq!(k.deadline(), Some(t0 + RESUME_DELAY));
    }

    /// With no clamp a port holds three characters, oldest in the highest
    /// slot; a fourth is lost and the read reports the overflow, which the
    /// read clears.
    #[test]
    fn an_unclamped_port_holds_three_and_reports_a_lost_fourth() {
        let (mut k, ps2) = kbc();
        let t0 = Instant::now();
        k.write(reg::KBD_TRANSMIT, 0xff, &ps2);
        k.write(reg::KBD_TRANSMIT, 0xff, &ps2);
        let all = rx_valid(0) | rx_valid(1) | rx_valid(2);
        assert_eq!(k.read(reg::KBD_RECEIVE, &ps2, t0), all | RX_OVERFLOW | 0x00fa_aafa);
        assert_eq!(k.read(reg::KBD_RECEIVE, &ps2, t0), 0, "the read cleared the holding and the flag");
        assert_eq!(k.deadline(), None, "no clamp, so nothing was stopped");
    }

    /// A board without the connectors takes no transmit and holds nothing.
    #[test]
    fn unwired_ports_hold_nothing() {
        let (mut k, ps2) = (Ioc3Kbc::new(false), Ps2Controller::new(None));
        assert!(!k.write(reg::KBD_TRANSMIT, 0xff, &ps2));
        assert_eq!(k.read(reg::KBD_RECEIVE, &ps2, Instant::now()), 0);
    }

    /// A reset forgets a pending release.
    #[test]
    fn reset_cancels_a_pending_release() {
        let (mut k, ps2) = kbc();
        k.write(reg::CONTROL, ctrl::K_CLAMP_1, &ps2);
        k.write(reg::KBD_TRANSMIT, 0xff, &ps2);
        k.read(reg::KBD_RECEIVE, &ps2, Instant::now());
        assert!(k.deadline().is_some());
        k.reset();
        assert_eq!(k.deadline(), None);
    }

    /// A release pending at save is pending after the load, with the time it
    /// had left, and a state saved with none loads with none.
    #[test]
    fn a_pending_release_survives_a_snapshot() {
        let (mut k, ps2) = kbc();
        let t0 = Instant::now();
        k.write(reg::CONTROL, ctrl::K_CLAMP_1, &ps2);
        k.write(reg::KBD_TRANSMIT, 0xff, &ps2);
        k.read(reg::KBD_RECEIVE, &ps2, t0);
        let saved = k.save_state(t0 + RESUME_DELAY / 4);
        let t1 = t0 + Duration::from_secs(5);
        let l = k.loaded(&saved, t1).unwrap();
        assert_eq!(l.deadline(), Some(t1 + RESUME_DELAY * 3 / 4), "the time left carries over");
        assert_eq!(l.control, ctrl::K_CLAMP_1);

        let idle = Ioc3Kbc::new(true);
        assert!(idle.save_state(t0).get("resume").is_none());
        assert_eq!(idle.loaded(&idle.save_state(t0), t1).unwrap().deadline(), None);
    }
}
