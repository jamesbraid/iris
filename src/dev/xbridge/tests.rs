use super::*;
use crate::traits::BUS_ERR;
struct Memory(Mutex<Vec<u8>>);
impl BusDevice for Memory {
    fn read32(&self, a: u64) -> BusRead32 {
        let mem = self.0.lock();
        if a as usize + 4 > mem.len() {
            return BusRead32::err();
        }
        BusRead32::ok(
            mem[a as usize..a as usize + 4]
                .iter()
                .fold(0, |v, &b| (v << 8) | b as u32),
        )
    }
    fn write32(&self, a: u64, v: u32) -> u32 {
        let mut mem = self.0.lock();
        if a as usize + 4 > mem.len() {
            return BUS_ERR;
        }
        for i in 0..4 {
            mem[a as usize + i] = (v >> (24 - i * 8)) as u8;
        }
        BUS_OK
    }

    fn read64(&self, a: u64) -> BusRead64 {
        let mem = self.0.lock();
        if a as usize + 8 > mem.len() {
            return BusRead64::err();
        }
        BusRead64::ok(
            mem[a as usize..a as usize + 8]
                .iter()
                .fold(0, |v, &b| (v << 8) | b as u64),
        )
    }
    fn write64(&self, a: u64, v: u64) -> u32 {
        let mut mem = self.0.lock();
        if a as usize + 8 > mem.len() {
            return BUS_ERR;
        }
        for i in 0..8 {
            mem[a as usize + i] = (v >> (56 - i * 8)) as u8;
        }
        BUS_OK
    }
    fn write64_masked(&self, a: u64, v: u64, mask: u64) -> u32 {
        let old = self.read64(a);
        if old.status != BUS_OK {
            return BUS_ERR;
        }
        self.write64(a, (old.data & !mask) | (v & mask))
    }

    fn read8(&self, a: u64) -> BusRead8 {
        match self.0.lock().get(a as usize) {
            Some(&value) => BusRead8 {
                data: value,
                status: BUS_OK,
            },
            None => BusRead8 {
                data: 0,
                status: BUS_ERR,
            },
        }
    }
    fn write8(&self, a: u64, v: u8) -> u32 {
        match self.0.lock().get_mut(a as usize) {
            Some(b) => {
                *b = v;
                BUS_OK
            }
            None => BUS_ERR,
        }
    }
}
fn chip() -> (Arc<XBridge>, Arc<Memory>) {
    let mem = Arc::new(Memory(Mutex::new(vec![0; 0x8000])));
    (XBridge::new(mem.clone()), mem)
}
#[test]
fn two_heads_and_crossbar_are_distinct_shared_chip_views() {
    let (c, _) = chip();
    let e = c.head(14);
    let f = c.head(15);
    assert_eq!(e.read32(4).data, 0x2d002049);
    assert_eq!(f.read32(4).data, 0x2d002049);
    e.write32(0x84, 0xa00000);
    assert_eq!(f.read32(0x84).data, 0x20000);
    assert_eq!(c.crossbar().read32(4).data, 0x2d000001);
    assert_eq!(c.crossbar().read32(0xc).data >> 6 & 15, 10);
    assert_eq!(c.crossbar().read32(0x194).data, 1 << 31);
}
#[test]
fn page_dma_keeps_valid_prefix_and_refuses_invalid_suffix() {
    let (c, mem) = chip();
    let f = c.head(15);
    f.write64(0x10000, 0x2000 | (10 << 8) | 0x21);
    let dma = c.dma(15, 2);
    let bytes = [1, 2, 3, 4, 5, 6, 7, 8];
    assert_eq!(dma.dma_write(0x40000ffc, &bytes), PciDmaStatus::MasterAbort);
    assert_eq!(&mem.0.lock()[0x2ffc..0x3000], &[1, 2, 3, 4]);
    assert_eq!(&mem.0.lock()[0x3000..0x3004], &[0; 4]);
    let mut read = [0xff; 8];
    assert_eq!(
        dma.dma_read(0x40000ffc, &mut read),
        PciDmaStatus::MasterAbort
    );
    assert_eq!(read, [1, 2, 3, 4, 0, 0, 0, 0]);
    assert_ne!(f.read32(0x104).data & (1 << 30), 0);
    f.write32(0x114, 2);
    assert_eq!(f.read32(0x104).data & (1 << 30), 0);
}
#[test]
fn interrupt_keeps_destination_and_enabling_a_live_pin_asserts() {
    let (c, _) = chip();
    let f = c.head(15);
    let packets = Arc::new(Mutex::new(Vec::new()));
    let capture = packets.clone();
    c.set_interrupt_sender(Arc::new(move |p| {
        capture.lock().push(p);
        true
    }));
    f.write32(0x34, 0xa0001);
    f.write32(0x3c, 0x23450080);
    f.write32(0x134, (0x155 << 8) | 0x42);
    c.pin_line(15, 0).set(true);
    assert!(packets.lock().is_empty());
    f.write32(0x10c, 1);
    f.write32(0x11c, 1);
    c.pin_line(15, 0).set(false);
    let p = packets.lock();
    assert_eq!(p.len(), 2);
    assert_eq!(p[0].source_widget, 15);
    assert_eq!(p[0].widget, 10);
    assert_eq!(p[0].vector, 0x42);
    assert_eq!(p[0].address, (0x123450080 & !(0x3ff << 38)) | (0x155 << 38));
    assert!(p[0].level);
    assert!(!p[1].level);
}

#[test]
fn revision_two_alias_and_requester_width_place_bytes() {
    let (c, mem) = chip();
    let f = c.head(15);
    let d = c.dma(15, 0);
    f.write32(0x84, 10 << 20);
    assert_eq!(
        d.dma_write(0x80002000, &[0, 1, 2, 3, 4, 5, 6, 7]),
        PciDmaStatus::Ok
    );
    assert_eq!(&mem.0.lock()[0x2000..0x2008], &[3, 2, 1, 0, 7, 6, 5, 4]);
    c.set_dma_64bit(15, 0, true);
    assert_eq!(
        d.dma_write(0x80002000, &[0, 1, 2, 3, 4, 5, 6, 7]),
        PciDmaStatus::Ok
    );
    assert_eq!(&mem.0.lock()[0x2000..0x2008], &[7, 6, 5, 4, 3, 2, 1, 0]);
    f.write64(0x10000, 0x3000 | (10 << 8) | 1);
    assert_eq!(
        d.dma_write(0x60000000, &[0, 1, 2, 3, 4, 5, 6, 7]),
        PciDmaStatus::Ok
    );
    assert_eq!(&mem.0.lock()[0x3000..0x3008], &[0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(
        d.dma_write((10 << 60) | (1 << 55) | 0x2000, &[11, 22]),
        PciDmaStatus::Ok
    );
    assert_eq!(&mem.0.lock()[0x2000..0x2002], &[11, 22]);
}
#[test]
fn source_packet_retains_programmed_widget_slot_and_half_requests() {
    let (c, _) = chip();
    let f = c.head(15);
    let records = Arc::new(Mutex::new(Vec::new()));
    let out = records.clone();
    c.set_dma_sender(Arc::new(move |p, b| {
        out.lock().push((p, b.to_vec()));
        true
    }));
    c.set_wide_slot_mask(15, 1 << 3);
    c.set_dma_64bit(15, 3, true);
    f.write32(0x24, 8);
    f.write32(0x84, 10 << 20);
    f.write32(0x21c, 1 << 19);
    assert_eq!(
        c.dma(15, 3)
            .dma_write(0x80002000, &[0, 1, 2, 3, 4, 5, 6, 7]),
        PciDmaStatus::Ok
    );
    let r = records.lock();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0.source_widget, 8);
    assert_eq!(r[0].0.slot, 3);
    assert_eq!(r[0].0.widget, 10);
    assert_eq!(r[0].0.address, 0x2000);
    assert!(r[0].0.half_write_requested);
    assert_eq!(r[0].1, [0, 1, 2, 3, 4, 5, 6, 7]);
}
#[test]
fn source_packet_refusal_keeps_successful_prefix() {
    let (c, _) = chip();
    let f = c.head(15);
    f.write32(0x84, 10 << 20);
    c.set_dma_sender(Arc::new(|p, b| {
        if p.address >= 0x2008 {
            return false;
        }
        b.fill(0x42);
        true
    }));
    let mut bytes = [0xff; 16];
    assert_eq!(
        c.dma(15, 0).dma_read(0x80002000, &mut bytes),
        PciDmaStatus::MasterAbort
    );
    assert_eq!(
        bytes,
        [0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0, 0, 0, 0, 0, 0, 0, 0]
    );
}
#[test]
fn ate_readback_geometry_and_invalid_mapper_entry_are_distinct() {
    let (c, _) = chip();
    let f = c.head(15);
    f.write64(0x10000 + 800 * 8, 0xffff123456789abc);
    assert_eq!(f.read32(0x10004 + 800 * 8).data, 0x1234);
    assert_eq!(f.read32(0x12004 + 800 * 8).data, 0x56789abc);
    f.write64(0x10000 + 512 * 8, 0x2a21);
    assert_eq!(
        c.dma(15, 0).dma_write(0x40200000, &[1]),
        PciDmaStatus::MasterAbort
    );
}
#[test]
fn warm_reset_uses_old_fences_and_the_quiet_window() {
    let (c, _) = chip();
    let x = c.crossbar();
    let e = c.head(14);
    let f = c.head(15);
    e.write32(0x84, 0xa00000);
    f.write32(0x84, 0xa00000);
    x.write32(0xb4, 0xff & !(1 << 2)); // Port E rejects port A.
    c.warm_reset(10, 0);
    assert_eq!(e.read32(0x84).data, 0xa00000);
    assert_eq!(f.read32(0x84).data, 0x20000);
    f.write32(0x84, 0xa00000);
    c.warm_reset(10, 10239);
    assert_eq!(f.read32(0x84).data, 0xa00000);
    c.warm_reset(10, 10240);
    assert_eq!(f.read32(0x84).data, 0x20000);
    c.reset();
    f.write32(0x84, 0xa00000);
    c.warm_reset(10, 10240);
    assert_eq!(f.read32(0x84).data, 0x20000);
}
#[test]
fn link_reset_changes_one_head_and_preserves_crossbar_programming() {
    let (c, _) = chip();
    let ports = Arc::new(Mutex::new(Vec::new()));
    let output = ports.clone();
    c.set_reset_output(Arc::new(move |port| output.lock().push(port)));
    let x = c.crossbar();
    let e = c.head(14);
    let f = c.head(15);
    x.write32(0x24, 0xfe);
    e.write32(0x84, 0xa00000);
    f.write32(0x84, 0xa00000);
    x.write32(0x2b4, 1);
    assert_eq!(*ports.lock(), vec![14]);
    assert_eq!(e.read32(0x84).data, 0x20000);
    assert_eq!(f.read32(0x84).data, 0xa00000);
    assert_eq!(x.read32(0x24).data, 0xfe);
}
#[test]
fn crossbar_errors_deliver_once_then_clear_the_original_destination() {
    let (c, _) = chip();
    let x = c.crossbar();
    let packets = Arc::new(Mutex::new(Vec::new()));
    let out = packets.clone();
    c.set_interrupt_sender(Arc::new(move |p| {
        out.lock().push(p);
        true
    }));
    x.write32(0x34, 0x550a0000);
    x.write32(0x3c, 0x80);
    x.write32(0x24, 0x20);
    x.read64(0);
    x.write32(4, 1);
    assert_eq!(packets.lock().len(), 1);
    assert_eq!(x.read32(0x54).data & ((1 << 23) | 32), (1 << 23) | 32);
    let p = packets.lock();
    assert_eq!(p.len(), 2);
    assert_eq!(p[0].vector, 0x55);
    assert_eq!(p[0].widget, 10);
    assert_eq!(p[0].address, 0x80);
    assert!(p[0].level);
    assert!(!p[1].level);
}

struct Card {
    config: crate::pci::PciConfig,
    bytes: Mutex<[u8; 32]>,
    irq: Mutex<Option<IrqLine>>,
}
impl Card {
    fn new() -> Arc<Self> {
        use crate::pci::{Bar, PciConfig};
        Arc::new(Self {
            config: PciConfig::new(
                0x1234,
                0x5678,
                0,
                0,
                [
                    Bar::Mem(32),
                    Bar::Unused,
                    Bar::Unused,
                    Bar::Unused,
                    Bar::Unused,
                    Bar::Unused,
                ],
                1,
            ),
            bytes: Mutex::new(std::array::from_fn(|i| i as u8)),
            irq: Mutex::new(None),
        })
    }
}
impl PciBusDevice for Card {
    fn config(&self) -> &crate::pci::PciConfig {
        &self.config
    }
    fn bar_read(&self, _: usize, off: u32, width: usize) -> u32 {
        let b = self.bytes.lock();
        (0..width).fold(0, |v, i| v | ((b[off as usize + i] as u32) << (i * 8)))
    }
    fn bar_write(&self, _: usize, off: u32, value: u32, width: usize) {
        let mut b = self.bytes.lock();
        for i in 0..width {
            b[off as usize + i] = (value >> (i * 8)) as u8;
        }
    }
    fn connect_interrupt(&self, line: IrqLine) {
        *self.irq.lock() = Some(line);
    }
}
#[test]
fn configuration_bar_decode_and_programmed_pio_placement() {
    let (c, _) = chip();
    let f = c.head(15);
    let card = Card::new();
    c.attach(15, 0, card.clone());
    assert_eq!(f.read32(0x20000).data, 0x56781234);
    assert_eq!(f.read8(0x20003).data, 0x34);
    assert_eq!(f.read32(0x21000).data, u32::MAX);
    f.write32(0x20010, 0x02000000);
    f.write32(0x20004, 2);
    f.write32(0x204, 0x1020);
    assert_eq!(f.read8(0x200000).data, 3);
    assert_eq!(f.read32(0x200000).data, 0x03020100);
    assert_eq!(f.read64(0x200000).data, 0x0302010007060504);
    f.write32(0x204, 0x3020);
    assert_eq!(f.read8(0x200000).data, 0);
    assert_eq!(f.read32(0x200000).data, 0x00010203);
    f.write32(0x20004, 0);
    assert_eq!(f.read32(0x200000).data, u32::MAX);
}
#[test]
fn long_alias_obeys_control_swap_and_target_width() {
    let (c, _) = chip();
    let f = c.head(15);
    let card = Card::new();
    c.attach(15, 0, card);
    f.write32(0x20010, 0x02000000);
    f.write32(0x20004, 2);
    assert_eq!(f.read8(0x82000000).data, 3);
    c.set_dma_64bit(15, 0, true);
    assert_eq!(f.read8(0x82000000).data, 7);
    f.write32(0x24, 1 << 22);
    assert_eq!(f.read8(0x82000000).data, 0);
    assert_eq!(f.read32(0x42000000).data, 0x00010203);
}
#[test]
fn pci_raw_output_is_independent_of_head_pin() {
    let (c, _) = chip();
    let card = Card::new();
    c.attach(15, 2, card.clone());
    let (probe, line) = crate::irq::LevelProbe::line();
    c.set_pci_irq(15, 2, 0, line);
    card.irq.lock().as_ref().unwrap().set(true);
    assert!(probe.level());
    assert_eq!(c.head(15).read32(0x104).data & 255, 0);
    c.reset();
    assert!(!probe.level());
}

#[test]
fn pio_narrow_and_doubleword_accesses_follow_all_four_placement_rows() {
    let (c, _) = chip();
    let f = c.head(15);
    c.attach(15, 0, Card::new());
    f.write32(0x20010, 0x02000000);
    f.write32(0x20004, 2);
    for wide in [false, true] {
        for stream in [false, true] {
            f.write32(
                0x204,
                0x1020 | if wide { 1 << 22 } else { 0 } | if stream { 1 << 13 } else { 0 },
            );
            let mask = if stream {
                0
            } else if wide {
                7
            } else {
                3
            };
            for size in [1, 2, 4, 8] {
                for off in (0..8).step_by(size) {
                    let expected = (0..size).fold(0u64, |v, i| v << 8 | ((off + i) ^ mask) as u64);
                    let a = 0x200000 + off as u64;
                    let got = match size {
                        1 => f.read8(a).data as u64,
                        2 => f.read16(a).data as u64,
                        4 => f.read32(a).data as u64,
                        _ => f.read64(a).data,
                    };
                    assert_eq!(
                        got, expected,
                        "wide={wide} stream={stream} size={size} off={off}"
                    );
                }
            }
        }
    }
}
#[test]
fn crossbar_reader_views_links_and_lock_share_state() {
    let (c, _) = chip();
    let x = c.crossbar();
    let view = c.crossbar_for_port(14);
    assert_eq!(view.read32(0xc).data >> 6 & 15, 14);
    x.write32(0xc4, 0x11);
    view.write32(0xc4, 0x22);
    assert_eq!(x.read32(0xc4).data, 0x11);
    view.write32(0xcc, 0);
    assert_eq!(x.read32(0xc4).data, 0);
    c.set_link_present(9, true);
    c.set_link_8bit(9, true);
    assert_eq!(x.read32(0x17c).data, 0x30);
    assert_eq!(x.read32(0x154).data, 1 << 31);
    assert_eq!(x.read32(0x13c).data, 0x40);
}
#[test]
fn invalid_register_captures_first_address_and_reports_error_destination() {
    let (c, _) = chip();
    let f = c.head(15);
    let packets = Arc::new(Mutex::new(Vec::new()));
    let out = packets.clone();
    c.set_interrupt_sender(Arc::new(move |p| {
        out.lock().push(p);
        true
    }));
    f.write32(0x34, 0xa0000);
    f.write32(0x3c, 0x80);
    f.write32(0x12c, 73);
    f.write32(0x10c, 1 << 24);
    f.read32(0x1000004);
    f.read32(0x2000004);
    assert_eq!(f.read32(0x1c).data, 0x1000004);
    assert_eq!(packets.lock().len(), 1);
    assert_eq!(packets.lock()[0].vector, 73);
    f.write32(0x114, 8);
    assert_eq!(f.read32(0x104).data & (1 << 24), 0);
}
#[test]
fn type_one_configuration_and_absent_dma_targets_are_explicit() {
    let (c, _) = chip();
    let f = c.head(15);
    c.attach(15, 2, Card::new());
    f.write32(0xcc, 2 << 11);
    assert_eq!(f.read32(0x28000).data, 0x56781234);
    f.write32(0xcc, (1 << 16) | (2 << 11));
    assert_eq!(f.read32(0x28000).data, u32::MAX);
    f.write32(0x84, 9 << 20);
    let mut bytes = [0xff; 4];
    assert_eq!(
        c.dma(15, 0).dma_read(0x80002000, &mut bytes),
        PciDmaStatus::MasterAbort
    );
    assert_eq!(bytes, [0; 4]);
    assert_eq!(
        c.dma(15, 0).dma_read(0x3fff8000, &mut bytes),
        PciDmaStatus::Ok
    );
    assert_eq!(
        c.dma(15, 0).dma_read(0x40800000, &mut bytes),
        PciDmaStatus::MasterAbort
    );
}
