//! Register-contract tests for the Crossbow. Most are QEMU's: each names
//! the test in jamesbraid/qemu-sgi-origami it came from (sn0-xbow-test
//! drives an isolated Crossbow, octane-test and octane-mgras-test the
//! Octane's). The expected values are QEMU's raw numbers rather than this
//! module's constants, so a wrong constant cannot agree with itself. The
//! rest cover what only iris has: the monitor's list of refused accesses,
//! the bus adapter and the callbacks' locking.

use super::*;
use crate::traits::Resettable;

const HEART: u8 = 8;
const GFX: u8 = 11;
const EMPTY: u8 = 12;
const BRIDGE: u8 = 15;
/// HEART and the Bridge, the links every Octane has.
const CORE: u16 = 1 << HEART | 1 << BRIDGE;

/// HEART stand-in: the interrupt packets it took, in order, as
/// (vector, set).
#[derive(Default)]
struct Sink(Mutex<Vec<(u8, bool)>>);

impl InterruptController for Sink {
    fn set_line(&self, line: u32, level: bool) {
        self.0.lock().push((line as u8, level));
    }
}

impl Sink {
    fn take(&self) -> Vec<(u8, bool)> {
        std::mem::take(&mut *self.0.lock())
    }
}

fn octane() -> Xbow {
    Xbow::new(4, CORE)
}

fn octane2() -> Xbow {
    Xbow::new(5, CORE)
}

/// A link register's slot: `reg` in widget `w`'s block.
fn link(w: u8, reg: u32) -> u32 {
    0x100 + 0x40 * (w as u32 - 8) + reg
}

/// The 32-bit register in `slot`'s data half.
fn rd(x: &Xbow, slot: u32) -> u32 {
    x.read(slot + 4, 4).unwrap_or_else(|| panic!("read of {slot:#x} refused")) as u32
}

fn wr(x: &Xbow, slot: u32, v: u32) {
    assert!(x.write(slot + 4, 4, v as u64), "write of {slot:#x} refused");
}

/// Read and acknowledge widget 0's status.
fn clear(x: &Xbow) -> u32 {
    rd(x, 0x50)
}

/// Widget 0's interrupt goes to HEART's interrupt address with `vector`.
fn interrupt_to_heart(x: &Xbow, vector: u8) -> Arc<Sink> {
    let sink = Arc::new(Sink::default());
    x.connect_heart(sink.clone());
    wr(x, 0x30, (vector as u32) << 24 | (HEART as u32) << 16);
    wr(x, 0x38, 0x80);
    sink
}

/// The identification register is revision code, then a part number and
/// manufacturer of zero and bit 0 set, and a reset keeps it: 0x40000001 on
/// the Octane's Crossbow, 0x50000001 on the Octane2's. From QEMU's
/// sn0-xbow-test identity and configured-identity-survives-reset, and
/// octane-test widgets-and-memory and octane2/population.
#[test]
fn the_identity_is_the_revision_code_with_zero_part_and_manufacturer() {
    for (revision, id) in [(4, 0x4000_0001), (5, 0x5000_0001)] {
        let x = Xbow::new(revision, CORE);
        assert_eq!(rd(&x, 0x00), id);
        Resettable::power_on(&x);
        assert_eq!(rd(&x, 0x00), id, "a reset keeps the identity");
    }
}

/// Every register comes out of reset at the specification's value, and the
/// per-link blocks decode at 0x100 plus 0x40 a link. From QEMU's
/// sn0-xbow-test reset-values and link-register-decode.
#[test]
fn registers_come_out_of_reset_at_their_documented_values() {
    let x = octane();
    for (slot, v) in [
        (0x08, 0),
        (0x10, 0),
        (0x18, 0),
        (0x20, 0),
        (0x28, 0x000f_ffff),
        (0x30, 0x0008_0000),
        (0x38, 0),
        (0x40, 0),
        (0x48, 0x03ff_1810),
        (0x58, 0x0000_0002),
        (0x60, 0x0080_0000),
        (0x68, 0x0080_0000),
        (0x70, 0x0000_0002),
    ] {
        assert_eq!(rd(&x, slot), v, "register {slot:#x}");
    }
    for w in 8..16 {
        assert_eq!(rd(&x, link(w, 0x08)), 2 << 18, "link {w} control");
        assert_eq!(rd(&x, link(w, 0x18)), 0);
        assert_eq!(rd(&x, link(w, 0x20)), 0);
        assert_eq!(rd(&x, link(w, 0x00)), 0, "nothing is queued, so a flush is over before it is read");
    }
    assert_eq!((link(8, 0x08), link(9, 0x08), link(15, 0x08)), (0x108, 0x148, 0x2c8));
    wr(&x, link(9, 0x18), 0x1234_5678);
    assert_eq!(rd(&x, link(9, 0x18)), 0x1234_5678);
    assert_eq!(rd(&x, link(8, 0x18)), 0);
    assert_eq!(rd(&x, link(10, 0x18)), 0);
}

/// A writable register keeps its writable bits; the interrupt target's bit
/// 19 is always set, so it names widgets 8 to 15 only; the link control's
/// force-bad-packet one-shot reads back zero; and writing the error command
/// word clears it and both error addresses whatever the data. From QEMU's
/// sn0-xbow-test writable-storage, error-command-word and the control half
/// of link-width-and-control.
#[test]
fn writable_registers_keep_only_their_writable_bits() {
    let x = octane();
    for (slot, kept) in [
        (0x20, 0x0000_0026),
        (0x28, 0x000f_ffff),
        (0x48, 0x03ff_ffff),
        (0x58, 0x0000_003f),
        (0x60, 0x00f0_0000),
        (0x68, 0x00f0_0000),
        (0x70, 0x000f_ffff),
        (0x30, 0xff0f_ffff),
    ] {
        wr(&x, slot, 0xffff_ffff);
        assert_eq!(rd(&x, slot), kept, "register {slot:#x}");
    }
    wr(&x, 0x30, 0);
    assert_eq!(rd(&x, 0x30), 0x0008_0000, "the target's high bit is fixed");
    wr(&x, 0x38, 0xdead_beef);
    assert_eq!(rd(&x, 0x38), 0xdead_beef);
    wr(&x, link(10, 0x08), 0xffff_ffff);
    assert_eq!(rd(&x, link(10, 0x08)), 0xbf7f_01ff);
    assert_eq!(rd(&x, link(10, 0x38)) & 0x10, 0, "forcing eight bits is not running eight bits");
    wr(&x, 0x40, 0xffff_ffff);
    assert_eq!((rd(&x, 0x40), rd(&x, 0x10), rd(&x, 0x18)), (0, 0, 0));
}

/// A reserved address, a write to a read-only register and a write to a
/// clearing alias are register-access errors, and a second while the first
/// stands also sets multiple error. The status alias returns the status and
/// clears widget 0's errors. From QEMU's sn0-xbow-test access-errors.
#[test]
fn reserved_read_only_and_clearing_addresses_are_access_errors() {
    let x = octane();
    assert_eq!(x.read(0x78 + 4, 4), None, "reserved on revision code 4");
    assert_eq!(rd(&x, 0x08), 0x20);
    assert_eq!(x.read(0xf8 + 4, 4), None);
    assert_eq!(rd(&x, 0x08), 0x21, "the repeat");
    assert_eq!(clear(&x), 0x21);
    assert_eq!(rd(&x, 0x08), 0);

    assert!(!x.write(0x00 + 4, 4, 0), "identification is read-only");
    assert_eq!(rd(&x, 0x08), 0x20);
    assert_eq!(rd(&x, 0x00) >> 12 & 0xffff, 0);
    clear(&x);
    assert!(!x.write(0x50 + 4, 4, 0), "the clearing alias is read-only");
    assert_eq!(rd(&x, 0x08), 0x20);
    clear(&x);
    assert!(!x.write(link(HEART, 0x10) + 4, 4, 0), "link status is read-only");
    assert_eq!(rd(&x, 0x08), 0x20);
}

/// A register is a 32-bit access to the low half of its eight-byte slot.
/// The upper half and a byte access are the wrong packet and an access
/// error; an eight-byte access has no established packet form, so it is
/// refused without a status flag. From QEMU's sn0-xbow-test packet-boundary
/// and wide-access-is-refused.
#[test]
fn only_a_32_bit_access_to_the_data_half_is_a_register_access() {
    let x = octane();
    assert_eq!(x.read(0x00, 4), None, "the slot's upper half");
    assert_eq!(clear(&x), 0x20);
    assert!(!x.write(0x20, 4, 0x26));
    assert_eq!(rd(&x, 0x20), 0);
    assert_eq!(clear(&x), 0x20);
    assert!(!x.write(0x20 + 4, 1, 0xff), "a byte is not a doubleword packet");
    assert_eq!(rd(&x, 0x20), 0);
    assert_eq!(clear(&x), 0x20);

    assert_eq!(x.read(0x00, 8), None);
    assert_eq!(rd(&x, 0x00) & 1, 1, "the 32-bit form answers");
    assert!(!x.write(0x20, 8, 0x26));
    assert_eq!(rd(&x, 0x20), 0);
    assert_eq!(x.read(0x20 + 4, 8), None, "nor at the data half");
    assert_eq!(rd(&x, 0x08), 0, "refusing a wide access is not an error the chip saw");
}

/// Revision code 4, the Octane's, leaves 0x78 to 0xf8 reserved: the fences
/// and the lock are access errors there and the processor sees a bus error.
/// From QEMU's octane-test xbow-fence-lock-errors and sn0-xbow-test
/// fence-and-lock-reserved-before-revision-5.
#[test]
fn revision_code_4_leaves_the_fence_and_lock_range_reserved() {
    let x = octane();
    for off in [0x7c, 0xbc, 0xc4, 0xcc] {
        assert_eq!(x.read(off, 4), None, "read at {off:#x}");
        assert_eq!(clear(&x), 0x20);
        assert!(!x.write(off, 4, 0), "write at {off:#x}");
        assert_eq!(clear(&x), 0x20);
    }
}

/// Revision code 5, the Octane2's, has a reset fence for widget 0's core
/// and for each link, which reset to 0xff and keep bits 7:0 of a write,
/// each its own; a machine reset puts them back. From QEMU's octane-test
/// octane2/xbow-fences-and-lock and sn0-xbow-test fence-and-lock-reset-values
/// and fence-persists-across-warm-reset (its machine-reset half).
#[test]
fn revision_code_5_fences_reset_to_all_ones_and_keep_their_low_byte() {
    let x = octane2();
    assert_eq!(rd(&x, 0x78), 0xff);
    for w in 8..16 {
        assert_eq!(rd(&x, 0x80 + 8 * (w - 8)), 0xff, "link {w}'s fence");
    }
    wr(&x, 0x78, 0xaa55_ff00);
    assert_eq!(rd(&x, 0x78), 0);
    wr(&x, 0x78, 0xffff_fe7f);
    assert_eq!(rd(&x, 0x78), 0x7f);
    wr(&x, 0x88, 0x1234_ff3c);
    assert_eq!(rd(&x, 0x88), 0x3c, "widget 9's fence");
    assert_eq!(rd(&x, 0x80), 0xff, "widget 8's is its own");
    wr(&x, 0xb8, 0xf7);
    assert_eq!(rd(&x, 0xb8), 0xf7);
    assert_eq!(rd(&x, 0x08), 0, "no access error on this revision");
    Resettable::power_on(&x);
    for slot in [0x78, 0x88, 0xb8] {
        assert_eq!(rd(&x, slot), 0xff, "a machine reset restores {slot:#x}");
    }
}

/// Revision code 5's resource lock: a write at 0xc0 takes only while the
/// lock reads zero and keeps the low byte, a write at 0xc8 frees it whatever
/// the data, both read the one value, and a machine reset frees it. From
/// QEMU's sn0-xbow-test lock-test-and-set and lock-clears-on-machine-reset,
/// and octane-test octane2/xbow-fences-and-lock.
#[test]
fn revision_code_5_lock_is_a_test_and_set_a_machine_reset_frees() {
    let x = octane2();
    assert_eq!((rd(&x, 0xc0), rd(&x, 0xc8)), (0, 0));
    wr(&x, 0xc0, 0x1234);
    assert_eq!((rd(&x, 0xc0), rd(&x, 0xc8)), (0x34, 0x34));
    wr(&x, 0xc0, 0x99);
    assert_eq!(rd(&x, 0xc0), 0x34, "a held lock is not overwritten");
    wr(&x, 0xc8, 0xaa);
    assert_eq!((rd(&x, 0xc0), rd(&x, 0xc8)), (0, 0));
    wr(&x, 0xc0, 0x55);
    assert_eq!(rd(&x, 0xc0), 0x55);
    Resettable::power_on(&x);
    assert_eq!(rd(&x, 0xc0), 0);
}

/// A port reports a fitted widget present and a live link alive; an empty
/// port has failed initialization; presence comes from what is fitted, so
/// the same port reads differently on two populations. From QEMU's
/// sn0-xbow-test link-population and second-population.
#[test]
fn fitted_ports_report_present_and_empty_ones_failed() {
    let x = octane();
    assert_eq!(rd(&x, link(HEART, 0x10)) & 1 << 31, 1 << 31);
    assert_eq!(rd(&x, link(HEART, 0x38)), 0x20, "present");
    assert_eq!(rd(&x, link(BRIDGE, 0x10)) & 1 << 31, 1 << 31);
    assert_eq!(rd(&x, link(GFX, 0x10)), 0x20, "maximum retry");
    assert_eq!(rd(&x, link(GFX, 0x38)), 0x40, "failure mode");
    assert!(x.carries(HEART) && x.carries(BRIDGE) && !x.carries(GFX) && !x.carries(3));

    let with_board = Xbow::new(4, CORE | 1 << GFX);
    assert_eq!(rd(&with_board, link(GFX, 0x10)) & 1 << 31, 1 << 31);
    assert_eq!(rd(&with_board, link(GFX, 0x38)), 0x20);
    assert!(with_board.carries(GFX));
}

/// A port with nothing on it goes to initialization failure and stays
/// there through a link reset. From QEMU's sn0-xbow-test
/// link-reset-on-an-absent-port and octane-mgras-test link-reset-absent.
#[test]
fn a_link_reset_of_an_empty_port_leaves_it_failed() {
    let x = octane();
    wr(&x, link(GFX, 0x30), 0);
    assert_eq!(rd(&x, link(GFX, 0x10)), 0x20);
    assert_eq!(rd(&x, link(GFX, 0x38)), 0x40);
    assert!(!x.carries(GFX));
}

/// The PROM resets each link it finds and expects it back: a fitted live
/// link comes back alive with an alive event, which raises the link's
/// request when its arrival is enabled, and the far end is not lost. From
/// QEMU's sn0-origin2000-test xbow-baseio-link-reset and octane-mgras-test
/// link-reset-recovery and core-link-reset-recovery, less the two-cycle
/// pulse, which iris does not model.
#[test]
fn a_link_reset_restores_a_live_link() {
    let x = octane();
    let heart = interrupt_to_heart(&x, 0x2a);
    for w in [HEART, BRIDGE] {
        wr(&x, link(w, 0x08), 1 << 31);
        wr(&x, link(w, 0x30), 0);
        assert!(x.carries(w));
        assert_eq!(rd(&x, link(w, 0x10)) & 1 << 31, 1 << 31);
        assert_eq!(rd(&x, 0x08) & 1 << (24 + w - 8), 1 << (24 + w - 8), "link {w}'s request");
        assert_eq!(heart.take(), [(0x2a, true)]);
        rd(&x, link(w, 0x28));
        assert_eq!(heart.take(), [(0x2a, false)]);
    }
}

/// A link taken down after coming up reports maximum retry but not failure
/// mode, which is for a port that never came up, and its traffic stops.
/// From QEMU's sn0-xbow-test link-population and
/// unavailable-destination-is-not-illegal, and octane-mgras-test liveness.
#[test]
fn a_link_taken_down_reports_maximum_retry_and_stops_carrying() {
    let x = Xbow::new(4, CORE | 1 << GFX);
    x.set_alive(CORE).unwrap();
    assert_eq!(rd(&x, link(GFX, 0x10)), 0x20);
    assert_eq!(rd(&x, link(GFX, 0x38)), 0x20, "present, and it came up once");
    assert!(!x.carries(GFX));
    x.set_alive(CORE | 1 << GFX).unwrap();
    assert!(x.carries(GFX));
    assert!(x.set_alive(CORE | 1 << EMPTY).is_err(), "an empty port cannot come up");
    assert!(x.set_alive(CORE | 1 << 3).is_err(), "widget 3 is not a link");
}

/// Width is what the link negotiated when it last came up, not what its
/// control forces now; the force survives a link reset, and a link reset
/// does not acknowledge a standing request. From QEMU's sn0-xbow-test
/// link-width-and-control.
#[test]
fn a_link_runs_the_width_it_came_up_with() {
    let x = Xbow::new(4, CORE | 1 << GFX);
    wr(&x, link(GFX, 0x08), 0xffff_ffff);
    assert_eq!(rd(&x, link(GFX, 0x38)) & 0x10, 0);
    x.set_alive(CORE).unwrap();
    x.set_alive(CORE | 1 << GFX).unwrap();
    assert_eq!(rd(&x, link(GFX, 0x38)) & 0x10, 0x10, "eight bits, as forced");
    let request = 1 << (24 + GFX - 8);
    assert_eq!(rd(&x, 0x08) & request, request, "its arrival was enabled");
    wr(&x, link(GFX, 0x30), 0);
    assert_eq!(rd(&x, 0x08) & request, request, "a link reset is no acknowledgment");
    assert_eq!(rd(&x, link(GFX, 0x08)) & 1 << 24, 1 << 24, "the force survives");
}

/// One assertion is outstanding at a time: a second enabled condition
/// sends no packet, acknowledging one leaves the assertion standing, and
/// acknowledging the last sends the deassertion. From QEMU's sn0-xbow-test
/// interrupt-coalescing.
#[test]
fn one_interrupt_assertion_is_outstanding_at_a_time() {
    let x = octane();
    let heart = interrupt_to_heart(&x, 0x1a);
    wr(&x, 0x20, 0x20);
    let _ = x.read(0x78 + 4, 4);
    assert_eq!(rd(&x, 0x08) & 1 << 23, 1 << 23);
    assert_eq!(heart.take(), [(0x1a, true)]);

    wr(&x, link(BRIDGE, 0x08), 2 << 18 | 0x20);
    x.set_alive(1 << HEART).unwrap();
    assert_eq!(rd(&x, 0x08) & 1 << 31, 1 << 31, "the Bridge link's request");
    assert_eq!(heart.take(), [], "behind the same assertion");

    clear(&x);
    assert_eq!(rd(&x, 0x08) & 1 << 23, 0);
    assert_eq!(rd(&x, 0x08) & 1 << 31, 1 << 31, "acknowledged at its own alias only");
    assert_eq!(heart.take(), []);
    rd(&x, link(BRIDGE, 0x28));
    assert_eq!(rd(&x, 0x08) & 0xff80_0000, 0);
    assert_eq!(heart.take(), [(0x1a, false)]);
}

/// Disabling a link's notification withdraws its request without
/// acknowledging the condition, and deasserts if it was the last. A link
/// coming up asks only when its arrival is enabled, and the arrival is an
/// event: acknowledged, it stops asking though the link stays up. From
/// QEMU's sn0-xbow-test interrupt-masking and interrupt-on-link-arrival.
#[test]
fn a_link_asks_for_an_interrupt_only_while_its_condition_is_enabled() {
    let x = Xbow::new(4, CORE | 1 << GFX);
    let heart = interrupt_to_heart(&x, 0x11);
    wr(&x, link(BRIDGE, 0x08), 2 << 18 | 0x20);
    x.set_alive(1 << HEART | 1 << GFX).unwrap();
    assert_eq!(heart.take(), [(0x11, true)]);
    wr(&x, link(BRIDGE, 0x08), 2 << 18);
    assert_eq!(rd(&x, 0x08) & 1 << 31, 0);
    assert_eq!(heart.take(), [(0x11, false)]);
    assert_eq!(rd(&x, link(BRIDGE, 0x10)), 0x20, "suppressed, not acknowledged");

    x.set_alive(CORE).unwrap();
    wr(&x, link(GFX, 0x08), 2 << 18 | 1 << 31);
    x.set_alive(CORE | 1 << GFX).unwrap();
    assert_eq!(heart.take(), [(0x11, true)]);
    rd(&x, link(GFX, 0x28));
    assert_eq!(rd(&x, link(GFX, 0x10)) & 1 << 31, 1 << 31);
    assert_eq!(rd(&x, 0x08) & 0xff80_0000, 0);
    assert_eq!(heart.take(), [(0x11, false)]);
}

/// Widget 0's interrupt leaves by the target's link: with HEART's link
/// down the request stands and no packet goes, and it goes once the link
/// is back. The processor's own accesses enter by that link too, so a dead
/// one cannot reach the registers, not even to reset itself. From QEMU's
/// sn0-xbow-test interrupt-needs-its-link and octane-mgras-test
/// core-link-reset-recovery.
#[test]
fn widget_0_interrupts_and_processor_accesses_need_heart_s_link() {
    let x = octane();
    let heart = interrupt_to_heart(&x, 3);
    wr(&x, link(HEART, 0x08), 2 << 18 | 0x20);
    x.set_alive(1 << BRIDGE).unwrap();
    assert_eq!(heart.take(), [], "the packet cannot leave");
    assert_eq!(x.read(0x08 + 4, 4), None);
    assert!(!x.write(link(HEART, 0x30) + 4, 4, 0));
    assert!(!x.carries(HEART));
    x.set_alive(CORE).unwrap();
    assert_eq!(heart.take(), [(3, true)], "the standing request goes out");
}

/// A link reset of a fitted link that was down returns it to failure, not
/// back up, and a machine reset brings it up again. From QEMU's
/// sn0-origin2000-test xbow-baseio-link-reset and octane-mgras-test
/// link-reset-recovery.
#[test]
fn a_link_reset_of_a_dead_link_leaves_it_down_until_a_machine_reset() {
    let x = Xbow::new(4, CORE | 1 << GFX);
    x.set_alive(CORE).unwrap();
    wr(&x, link(GFX, 0x30), 0);
    assert_eq!(rd(&x, link(GFX, 0x10)), 0x20);
    assert!(!x.carries(GFX));
    Resettable::power_on(&x);
    assert!(x.carries(GFX));
    assert_eq!(rd(&x, link(GFX, 0x10)) & 1 << 31, 1 << 31);
}

/// The packet goes to the 48-bit address the destination pair names, in
/// the target's own space, and HEART takes it only at its interrupt address
/// with a vector it has. A packet sent anywhere else still counts as the
/// outstanding assertion. From QEMU's sn0-xbow-test
/// interrupt-destination-address, with HEART's packet rule from sgi-heart.c.
#[test]
fn heart_takes_widget_0_s_packet_only_at_its_interrupt_address() {
    let x = octane();
    let heart = interrupt_to_heart(&x, 0x7e);
    wr(&x, 0x30, 0x7e << 24 | 8 << 16 | 0x0002);
    wr(&x, 0x20, 0x20);
    let _ = x.read(0x78 + 4, 4);
    assert_eq!(heart.take(), [], "address 0x2_0000_0080 is not HEART's interrupt register");
    clear(&x);

    wr(&x, 0x30, 0x7e << 24 | 8 << 16);
    let _ = x.read(0x78 + 4, 4);
    assert_eq!(heart.take(), [], "HEART has 64 vectors");
    clear(&x);

    wr(&x, 0x30, 0x3e << 24 | 8 << 16);
    let _ = x.read(0x78 + 4, 4);
    assert_eq!(heart.take(), [(0x3e, true)]);
    clear(&x);
    assert_eq!(heart.take(), [(0x3e, false)], "the deassertion goes to the same place");

    // To the Bridge, which iris carries no packet into: sent all the same.
    wr(&x, 0x30, 0x3e << 24 | (BRIDGE as u32) << 16);
    let _ = x.read(0x78 + 4, 4);
    wr(&x, 0x30, 0x3e << 24 | 8 << 16);
    let _ = x.read(0x78 + 4, 4);
    assert_eq!(heart.take(), [], "an assertion is already outstanding");
}

/// A machine reset puts back the registers and the links fitted, and sends
/// no packet for an assertion it drops. From QEMU's sn0-xbow-test
/// reset-restores-startup-health.
#[test]
fn a_reset_restores_the_fitted_links_and_sends_nothing() {
    let x = octane();
    x.attach(GFX);
    let heart = interrupt_to_heart(&x, 0x3c);
    wr(&x, 0x20, 0x20);
    let _ = x.read(0x78 + 4, 4);
    assert_eq!(heart.take(), [(0x3c, true)]);
    wr(&x, 0x28, 0);
    Resettable::power_on(&x);
    assert_eq!(heart.take(), [], "no parting packet");
    assert_eq!(rd(&x, 0x28), 0x000f_ffff);
    assert_eq!(rd(&x, 0x20), 0);
    assert_eq!(rd(&x, 0x08), 0);
    assert_eq!(rd(&x, 0x30), 0x0008_0000);
    for w in [HEART, GFX, BRIDGE] {
        assert!(x.carries(w), "widget {w}");
        assert_eq!(rd(&x, link(w, 0x10)) & 1 << 31, 1 << 31);
    }
    assert!(!x.carries(EMPTY));
}

/// The monitor's `ip30 xbow` lists refused accesses: the first 64
/// distinct, then nothing more.
#[test]
fn the_unsupported_list_keeps_the_first_64_and_then_stops() {
    let x = octane();
    for i in 0..100u32 {
        x.note(0x1000 + 8 * i, 4, false);
    }
    let u = x.unsupported();
    assert_eq!(u.len(), 64);
    assert_eq!(u[63], (0x1000 + 8 * 63, 4, false));
    x.note(0x9000, 4, true);
    assert_eq!(x.unsupported(), u, "a full list is left alone");
}

/// The machine's bus hands the window the full physical address.
#[test]
fn the_window_decodes_its_own_16_mb_of_a_full_address() {
    let x = octane();
    assert_eq!(x.read32(0x1000_0004).data, 0x4000_0001);
    assert!(!x.read32(0x1000_0000).is_ok(), "the empty half of the slot");
    assert!(!x.read64(0x1000_0004).is_ok());
    assert_eq!(x.write32(0x1000_0000 + 0x38 + 4, 0x55), BUS_OK);
    assert_eq!(x.read32(0x1000_0000 + 0x38 + 4).data, 0x55);
    assert_eq!(x.write32(0x1000_0000 + 4, 0), BUS_ERR);
}

#[test]
fn link_changes_reach_the_callback_outside_the_lock() {
    let x = Arc::new(octane());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (s, probe) = (seen.clone(), x.clone());
    x.on_link_change(Box::new(move |w, alive| {
        // The callback asks the switch about its links and registers.
        assert_eq!(probe.carries(w), alive);
        assert!(probe.read(0x08 + 4, 4).is_some());
        s.lock().push((w, alive));
    }));
    x.attach(GFX);
    assert_eq!(*seen.lock(), [(GFX, true)]);
    // A reset of a fitted live link changes nothing; of an empty port neither.
    wr(&x, link(BRIDGE, 0x30), 0);
    wr(&x, link(13, 0x30), 0);
    assert_eq!(seen.lock().len(), 1);
    // Power-on drops nothing it fitted.
    Resettable::power_on(&*x);
    assert_eq!(seen.lock().len(), 1);
}

#[test]
fn a_link_reset_tells_the_far_end_after_the_switch_settled() {
    let x = Arc::new(octane());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (s, probe) = (seen.clone(), x.clone());
    x.on_link_reset(Box::new(move |w| {
        s.lock().push((w, probe.carries(w), probe.read(0x08 + 4, 4).is_some()));
    }));
    wr(&x, link(BRIDGE, 0x30), 0);
    assert_eq!(*seen.lock(), [(BRIDGE, true, true)]);
}
