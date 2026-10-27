//! SGI's MicroLAN register cell: one register write is one bit slot. The
//! IOC3, the Bridge and HEART share its layout: bits 19:10 the time the
//! master drives the wire low, bits 9:2 when it samples.
//!
//! The 400 and 40 thresholds are modelling choices, not hardware values:
//! real pulse widths in register units are unknown, and every responder
//! the guests have been run against uses these.

use super::Bus;

/// A low drive at least this long is a reset.
pub const RESET_PULSE: u32 = 400;
/// A low drive shorter than this releases the wire in time: a write-one,
/// which is also a read slot. Anything between holds it for a write-zero.
pub const ONE_PULSE: u32 = 40;

/// The low-drive time and sample offset fields of a MicroLAN register
/// value: `(pulse, sample)`.
pub fn fields(val: u64) -> (u32, u32) {
    let _ = val;
    todo!()
}

/// One slot of `pulse` on `bus`: a reset (the wire reads low if a part
/// answers presence) or a bit slot. Returns the level the master samples.
/// `sample` is accepted so every master can pass its raw fields; this
/// synchronous decode has no use for it.
pub fn slot(bus: &mut Bus, pulse: u32, sample: u32) -> bool {
    let _ = (bus, pulse, sample);
    todo!()
}

/// A slot on a master whose wire also reaches a second net: both buses
/// see it, and the wire reads low if a part on either pulls it low.
pub fn slot_wired(bus: &mut Bus, shared: Option<&mut Bus>, pulse: u32, sample: u32) -> bool {
    let _ = (bus, shared, pulse, sample);
    todo!()
}
