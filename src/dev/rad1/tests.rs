use super::*;

const BAR: u64 = 0x1300_0000;

/// A card with BAR0 assigned and memory decoding on, as firmware leaves it.
fn card() -> Rad1 {
    let r = Rad1::new();
    r.config().write(4, BAR as u32);
    r.config().write(1, PCI_COMMAND_MEMORY_ON);
    r.fake_ns.store(0, Ordering::Relaxed);
    r
}
const PCI_COMMAND_MEMORY_ON: u32 = 2;

fn wr(r: &Rad1, off: u32, v: u32) {
    r.bar_write(0, off, v, 4);
}
fn rd(r: &Rad1, off: u32) -> u32 {
    r.bar_read(0, off, 4)
}

const STARTUP: [(u32, u32); 28] = [
    (0x44, 0xa0000008), (0x48, 0x10), (0x4c, 0x03000000), (0x50, 0x60000000),
    (0x54, 0x4000000c), (0x70, 0x1440), (0x74, 0x08000010), (0x78, 0x00fac688),
    (0x7c, 0xd6d6d6d6), (0x80, 0xffe20000), (0x88, 3), (0x8c, 2), (0x90, 2),
    (0xa0, 0x02000001), (0xa4, 0x12345678), (0xa8, 1), (0xac, 2), (0xb0, 3),
    (0xb4, 4), (0xb8, 0x9fffffff), (0xbc, 0x9fffffff), (0x4c0, 0xa000c000),
    (0x4c8, 0xa000c800), (0x4d0, 0xa000d000), (0x4d8, 0xa000d800),
    (0x4e0, 0xa000e000), (0x4e8, 0xa000e800), (0x4f0, 0xa000f000),
];

#[test]
fn pci_identity() {
    let r = Rad1::new();
    let c = r.config();
    assert_eq!(c.read(0), 0x0005_10a9);
    assert_eq!(c.read(2) & 0xff, 0xc0);
    assert_eq!(c.read(2), 0xc0);
    // Identity, class and revision do not take writes.
    c.write(2, 0xffff_ffff);
    assert_eq!(c.read(2), 0xc0);
    c.write(2, 0x0401_0070);
    assert_eq!(c.read(2), 0xc0);
    assert_eq!(c.interrupt_pin(), 0);
    assert_eq!(c.read(4), 0);
}

#[test]
fn bar_sizing_and_assignment() {
    let r = Rad1::new();
    let c = r.config();
    c.write(4, 0xffff_ffff);
    assert_eq!(c.read(4), 0xffff_e000);
    c.write(4, BAR as u32);
    c.write(1, 2);
    assert_eq!(c.read(4), BAR as u32);
    assert_eq!(c.read(1) & 0xffff, 2);
    assert_eq!(c.decode(crate::pci::Space::Mem, BAR as u32 + 0x30), Some((0, 0x30)));
    assert_eq!(c.decode(crate::pci::Space::Mem, BAR as u32 + 0x2000), None);
}

#[test]
fn startup_words_store_and_reset() {
    let r = card();
    for (off, _) in STARTUP {
        assert_eq!(rd(&r, off), 0, "{off:#x} resets to zero");
    }
    // The bring-up and teardown sequence reads each write back.
    for (off, v) in [
        (0x80, 0xffffffff), (0x80, 0xffe3cffe), (0x80, 0xffe3c0fe), (0x80, 0xffe24070),
        (0x80, 0xffe20000), (0x70, 0x1500), (0x70, 0x1540), (0x70, 0x1440), (0x70, 0x1540),
        (0x88, 3), (0x88, 2), (0x88, 0), (0x7c, 0xc0c0c0c0), (0x70, 0x480), (0x80, 0xfffffffe),
        (0x80, 0xffffcbfc), (0x80, 0xffff8bfc), (0xa8, 0x13579bdf), (0xac, 0x2468ace0),
        (0xb0, 0x89abcdef), (0xb4, 0xfedcba98),
    ] {
        wr(&r, off, v);
        assert_eq!(rd(&r, off), v, "{off:#x}");
    }
    r.reset();
    wr(&r, 0x80, 0xffe20000);
    for (off, v) in STARTUP {
        // 0x4d0 and 0x4e0 are descriptor registers, covered separately.
        if ![0x4d0, 0x4e0].contains(&off) {
            wr(&r, off, v);
            assert_eq!(rd(&r, off), v, "{off:#x}");
        }
    }
    r.reset();
    assert_eq!(rd(&r, 0x80), 0);
    assert_eq!(rd(&r, 0x4c0), 0);
}

#[test]
fn startup_refuses_unsupported_enables() {
    let r = card();
    wr(&r, 0x44, 1);
    wr(&r, 0x44, 4);
    assert_eq!(rd(&r, 0x44), 0);
    wr(&r, 0x44, 0xa000_0008);
    assert_eq!(rd(&r, 0x44), 0xa000_0008);
    for off in [0x48, 0x50, 0x54] {
        wr(&r, off, 1);
        assert_eq!(rd(&r, off), 0, "{off:#x}");
    }
    // Analog input enable with no ring programmed.
    wr(&r, 0x4c, 1);
    assert_eq!(rd(&r, 0x4c), 0);
    // AES3 subcode enable likewise.
    wr(&r, 0x48, 4);
    assert_eq!(rd(&r, 0x48), 0);
}

#[test]
fn subcode_window() {
    let r = card();
    for off in (0x1000..=0x1218).step_by(4) {
        let masked = matches!(off & !7, 0x1078 | 0x10f8);
        if masked {
            assert_eq!(rd(&r, off), 0);
            wr(&r, off, 0xdead_beef);
            assert_eq!(rd(&r, off), 0, "{off:#x} is not part of the window");
        } else {
            wr(&r, off, off ^ 0x5a5a_a5a5);
            assert_eq!(rd(&r, off), off ^ 0x5a5a_a5a5, "{off:#x}");
        }
    }
    // The two halves of a doubleword are independent words.
    assert_eq!(rd(&r, 0x1004), 0x1004 ^ 0x5a5a_a5a5);
    r.reset();
    assert_eq!(rd(&r, 0x1000), 0);
    // Not a word access.
    wr(&r, 0x1002, 1);
    assert_eq!(r.bar_read(0, 0x1000, 2), 0);
}

#[test]
fn descriptor_bank_storage() {
    let r = card();
    for (off, v) in [(0x500, 0x1000), (0x528, 0), (0x504, 0xffff_fffc), (0x4b4, 0x1000)] {
        wr(&r, off, v);
        assert_eq!(rd(&r, off), v);
    }
    // Words in the bank that belong to no supported ring are refused.
    wr(&r, 0x420, 7);
    assert_eq!(rd(&r, 0x420), 0);
    r.reset();
    assert_eq!(rd(&r, 0x500), 0);
}

#[test]
fn ust_counter_and_control() {
    let r = card();
    let at = |ns: i64| r.fake_ns.store(ns, Ordering::Relaxed);
    assert_eq!(rd(&r, 0x30), 0);
    at(100_000);
    assert_eq!(rd(&r, 0x30), 0, "stopped at reset");
    // Control must be armed (0x0e) before it is started (0x0a).
    wr(&r, 0x40, 0x0a);
    at(200_000);
    assert_eq!(rd(&r, 0x30), 0);
    wr(&r, 0x40, 0x0e);
    wr(&r, 0x40, 0x0a);
    at(200_000 + 960 * 10 + 5);
    assert_eq!(rd(&r, 0x30), 10);
    at(200_000 + 960 * 20);
    assert_eq!(rd(&r, 0x30), 20, "phase carries");
    wr(&r, 0x40, 0x02);
    at(200_000 + 960 * 50);
    assert_eq!(rd(&r, 0x30), 20, "stopped");
    // Other control values are refused.
    wr(&r, 0x40, 0x0f);
    wr(&r, 0x40, 0x0a);
    at(200_000 + 960 * 60);
    assert_eq!(rd(&r, 0x30), 20);
    // The counter is read-only.
    wr(&r, 0x30, 0x1234);
    assert_eq!(rd(&r, 0x30), 20);
}

#[test]
fn ust_wraps_to_32_bits() {
    let r = card();
    r.state.lock().unwrap().ust_count = u32::MAX - 1;
    wr(&r, 0x40, 0x0e);
    wr(&r, 0x40, 0x0a);
    r.fake_ns.store(960 * 3, Ordering::Relaxed);
    assert_eq!(rd(&r, 0x30), 1);
}

#[test]
fn position_registers_read_zero() {
    let r = card();
    for off in (0x04..=0x2c).step_by(4) {
        assert_eq!(rd(&r, off), 0);
        wr(&r, off, 0xffff_ffff);
        assert_eq!(rd(&r, off), 0, "{off:#x} is read-only");
    }
}

/// Program a ring (set A and B alike) the way the driver's channel start does.
fn program(r: &Rad1, ring: &RingRegs, base: u32, ctrl: u32) {
    for set in [0, 3] {
        wr(r, ring[set], base);
        wr(r, ring[set + 1], 0);
        wr(r, ring[set + 2], ctrl);
    }
}

#[test]
fn analog_output_enable_needs_a_matching_ring() {
    let r = card();
    // Disable is always accepted.
    wr(&r, 0x58, 0x2000_0000);
    assert_eq!(rd(&r, 0x58), 0x2000_0000);
    wr(&r, 0x58, 0x2000_0001);
    assert_eq!(rd(&r, 0x58), 0x2000_0000, "no ring programmed");
    // 4 KiB ring: 32 units of 128 bytes; length is stored negated at bit 7,
    // and the model's mask for this ring is 0xfffff008 plus the tag.
    program(&r, &RING_OUTPUT, 0x10_0000, 0xffff_f008);
    wr(&r, 0x58, 0x2000_0001);
    assert_eq!(rd(&r, 0x58), 0x2000_0001);
    assert!(r.state.lock().unwrap().chan[CH_OUT].running);
    assert_eq!(r.state.lock().unwrap().chan[CH_OUT].len, 0x1000);
    // The ring cannot be rewritten while it runs.
    wr(&r, 0x4f8, 0x20_0000);
    assert_eq!(rd(&r, 0x4f8), 0x10_0000);
    wr(&r, 0x58, 0x2000_0000);
    assert!(!r.state.lock().unwrap().chan[CH_OUT].running);
    wr(&r, 0x4f8, 0x20_0000);
    assert_eq!(rd(&r, 0x4f8), 0x20_0000);
    // Values other than the two enables are refused.
    wr(&r, 0x58, 0x2000_0002);
    assert_eq!(rd(&r, 0x58), 0x2000_0000);
}

#[test]
fn analog_input_and_subcode_enable() {
    let r = card();
    program(&r, &RING_INPUT, 0x11_0000, 0xffff_f000);
    wr(&r, 0x4c, 0x0300_0001);
    assert_eq!(rd(&r, 0x4c), 0x0300_0001);
    assert!(r.state.lock().unwrap().chan[CH_IN].running);
    wr(&r, 0x4c, 0x0300_0000);
    assert!(!r.state.lock().unwrap().chan[CH_IN].running);

    program(&r, &RING_AES_SUB, 0x12_0000, 0xffff_f030);
    wr(&r, 0x48, 4);
    assert_eq!(rd(&r, 0x48), 4);
    assert!(r.state.lock().unwrap().chan[CH_AES_SUB].running);
}

#[test]
fn status_dma_control() {
    let r = card();
    // Stop is always accepted.
    wr(&r, 0x5c, 0x1fffc);
    assert_eq!(rd(&r, 0x5c), 0x1fffc);
    // Start with no ring is refused.
    wr(&r, 0x5c, 0x40001);
    assert_eq!(rd(&r, 0x5c), 0x1fffc);
    wr(&r, 0x500, 0x30_0000);
    wr(&r, 0x504, 0xffff_fffc);
    wr(&r, 0x5c, 0x40001);
    assert_eq!(rd(&r, 0x5c), 0x40001, "one-shot");
    // Ring start: interval in bits 2..16, limit 0x78 minus slots * 0x80.
    wr(&r, 0x504, 0xffff_c078);
    wr(&r, 0x4b4, 0x30_0000);
    wr(&r, 0x4bc, 0xffff_c078);
    wr(&r, 0x5c, 0x40003 | (330 << 2));
    assert_eq!(rd(&r, 0x5c), 0x40003 | (330 << 2));
    assert!(r.state.lock().unwrap().status_running);
    // Rewriting the limit while the ring runs is refused.
    wr(&r, 0x504, 0);
    assert_eq!(rd(&r, 0x504), 0xffff_c078);
    wr(&r, 0x5c, 0x1fffc);
    assert!(!r.state.lock().unwrap().status_running);
    // A zero interval names no period.
    wr(&r, 0x5c, 0x40003);
    assert_ne!(rd(&r, 0x5c), 0x40003);
}

#[test]
fn unrecognised_accesses_read_zero_and_store_nothing() {
    let r = card();
    wr(&r, 0x34, 5);
    assert_eq!(rd(&r, 0x34), 0);
    wr(&r, 0x1ffc, 5);
    assert_eq!(rd(&r, 0x1ffc), 0);
    // Byte access to a start-up word is not recognised.
    r.bar_write(0, 0x80, 0xff, 1);
    assert_eq!(rd(&r, 0x80), 0);
}
