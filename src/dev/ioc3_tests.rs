//! IOC3 register-contract tests.
//!
//! Ported from the IOC3 qtests (tests/qtest/octane-test.c, sn0-ioc3-test.c,
//! sn0-ioc3-serial-test.c, sn0-ioc3-parallel-test.c) in
//! https://github.com/jamesbraid/qemu-sgi-origami (sgi-origami branch),
//! BSD-3-Clause.
//!
//! Accesses go through `PciBusDevice` the way a host bridge makes them: a
//! 32-bit access at its offset, a byte access at `offset ^ 3`.

use super::*;
use crate::irq::LevelProbe;

const MAC: [u8; 6] = [0x08, 0x00, 0x69, 0x12, 0x34, 0x56];

/// A sink for a serial line: what the guest sent, and what the host types.
#[derive(Default)]
struct Wire {
    out: Mutex<Vec<u8>>,
    inp: Mutex<VecDeque<u8>>,
}

impl SerialBackend for Wire {
    fn send_byte(&self, b: u8) {
        self.out.lock().unwrap().push(b);
    }
    fn recv_byte(&self) -> std::io::Result<u8> {
        self.inp
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::WouldBlock, "empty"))
    }
}

/// Bus memory, as bytes at bus addresses.
struct TestMem(Mutex<Vec<u8>>);

impl TestMem {
    fn new() -> Arc<TestMem> {
        Arc::new(TestMem(Mutex::new(vec![0; 0x10000])))
    }
}

impl PciDma for TestMem {
    fn dma_read(&self, addr: u64, buf: &mut [u8]) -> PciDmaStatus {
        let m = self.0.lock().unwrap();
        let a = addr as usize;
        if addr > u32::MAX as u64 || a + buf.len() > m.len() {
            buf.fill(0);
            return PciDmaStatus::MasterAbort;
        }
        buf.copy_from_slice(&m[a..a + buf.len()]);
        PciDmaStatus::Ok
    }
    fn dma_write(&self, addr: u64, data: &[u8]) -> PciDmaStatus {
        let mut m = self.0.lock().unwrap();
        let a = addr as usize;
        if addr > u32::MAX as u64 || a + data.len() > m.len() {
            return PciDmaStatus::MasterAbort;
        }
        m[a..a + data.len()].copy_from_slice(data);
        PciDmaStatus::Ok
    }
}

struct Rig {
    c: Ioc3,
    mem: Arc<TestMem>,
    wire: [Arc<Wire>; 2],
    superio: Arc<LevelProbe>,
}

impl Rig {
    fn new() -> Rig {
        Rig::with(Ioc3::octane(MAC, false))
    }

    fn with(c: Ioc3) -> Rig {
        let mem = TestMem::new();
        c.connect_dma(mem.clone());
        let wire = [Arc::new(Wire::default()), Arc::new(Wire::default())];
        c.set_serial_backend(0, wire[0].clone());
        c.set_serial_backend(1, wire[1].clone());
        let (superio, line) = LevelProbe::line();
        c.connect_superio_interrupt(line);
        Rig { c, mem, wire, superio }
    }

    fn r32(&self, a: u32) -> u32 {
        self.c.bar_read(0, a, 4)
    }
    fn w32(&self, a: u32, v: u32) {
        self.c.bar_write(0, a, v, 4)
    }
    /// A byte access as the bridge delivers it.
    fn r8(&self, a: u32) -> u8 {
        self.c.bar_read(0, a ^ 3, 1) as u8
    }
    fn w8(&self, a: u32, v: u8) {
        self.c.bar_write(0, a ^ 3, v as u32, 1)
    }

    /// The firmware's chip-control value: ports at 0x178/0x170, strobe 4.
    fn aim_engines(&self) {
        self.w32(reg::CHIP_CONTROL, 0x0002_2e5e);
    }

    /// DMA the way the chip sees memory: internal bytes.
    fn mem_view(&self) -> Ioc3Dma {
        Ioc3Dma::new(self.mem.clone())
    }
}

// ---------------------------------------------------------------------------
// PCI function
// ---------------------------------------------------------------------------

#[test]
fn identity_and_bar() {
    let r = Rig::new();
    let cfg = r.c.config();
    assert_eq!(cfg.read(0), 0x0003_10a9);
    assert_eq!(cfg.read(2) >> 8, 0xff_0000, "no defined class");
    assert_eq!(cfg.read(2) & 0xff, 1, "revision 1");
    assert_eq!(cfg.read(11), 0xc304_10a9, "the Octane system board");
    assert_eq!(cfg.interrupt_pin(), PCI_INTA);
    cfg.write(4, 0xffff_ffff);
    assert_eq!(cfg.read(4), 0xfff0_0000, "one megabyte of memory space");
}

// ---------------------------------------------------------------------------
// SuperIO window: the two 16550s
// ---------------------------------------------------------------------------

#[test]
fn ports_are_distinct() {
    let r = Rig::new();
    r.w8(reg::SERIAL_A + 7, 0x5a);
    r.w8(reg::SERIAL_B + 7, 0xa5);
    assert_eq!(r.r8(reg::SERIAL_A + 7), 0x5a);
    assert_eq!(r.r8(reg::SERIAL_B + 7), 0xa5);
}

#[test]
fn registers_are_a_byte_apart() {
    let r = Rig::new();
    r.w8(reg::SERIAL_A + 3, 0x03);
    r.w8(reg::SERIAL_A + 4, 0x05);
    assert_eq!(r.r8(reg::SERIAL_A + 3), 0x03);
    assert_eq!(r.r8(reg::SERIAL_A + 4), 0x05);
}

/// A four-byte access to the window is four registers, one per lane.
#[test]
fn the_port_answers_a_byte_at_a_time() {
    let r = Rig::new();
    r.w8(reg::SERIAL_A + 4, 0x05);
    r.w8(reg::SERIAL_A + 7, 0x5a);
    let lane: Vec<u8> = (0..4).map(|i| r.r8(reg::SERIAL_A + 4 + i)).collect();
    assert_eq!(lane[0], 0x05);
    assert_eq!(lane[3], 0x5a);
    // Status registers between them were read above, clearing what a read
    // clears; the wide access reads them again.
    let wide = r.r32(reg::SERIAL_A + 4);
    assert_eq!(wide, u32::from_le_bytes([lane[0], lane[1], lane[2], lane[3]]));
}

#[test]
fn console_transmit_reaches_the_backend() {
    let r = Rig::new();
    for b in b"hi\r\n" {
        assert_ne!(r.r8(reg::SERIAL_A + 5) & 0x20, 0, "THRE");
        r.w8(reg::SERIAL_A, *b);
    }
    assert_eq!(&*r.wire[0].out.lock().unwrap(), b"hi\r\n");
    assert!(r.wire[1].out.lock().unwrap().is_empty());
}

#[test]
fn console_receive_polls_lsr() {
    let r = Rig::new();
    assert_eq!(r.r8(reg::SERIAL_A + 5) & 1, 0);
    r.wire[0].inp.lock().unwrap().extend(b"ab");
    assert_eq!(r.r8(reg::SERIAL_A + 5) & 1, 1);
    assert_eq!(r.r8(reg::SERIAL_A), b'a');
}

#[test]
fn the_scratch_register_programs_the_predivider_under_dlab() {
    let r = Rig::new();
    r.w8(reg::SERIAL_A + 3, 0x80);
    r.w8(reg::SERIAL_A + 7, 6);
    assert_eq!(r.r8(reg::SERIAL_A + 7), 6, "reads back what was written");
    assert_eq!(r.c.sh.st.lock().unwrap().uart[0].predivider, 6);
    // Written 0 or 1 means nothing.
    r.w8(reg::SERIAL_A + 7, 1);
    assert_eq!(r.c.sh.st.lock().unwrap().uart[0].predivider, 6);
}

/// A serial pass-through bit follows the 16550's level and ignores an
/// acknowledgement.
#[test]
fn a_serial_bit_ignores_an_acknowledgement() {
    let r = Rig::new();
    r.w32(reg::INT_ENABLE_SET, int::SERIAL_A);
    r.w8(reg::SERIAL_A + 1, 0x01); // IER: receive data
    assert!(!r.superio.level());
    r.wire[0].inp.lock().unwrap().push_back(b'x');
    r.c.service();
    assert_ne!(r.r32(reg::INT_STATUS) & int::SERIAL_A, 0);
    assert!(r.superio.level(), "the SuperIO line is up");
    r.w32(reg::INT_STATUS, 0xffff_ffff);
    assert_ne!(r.r32(reg::INT_STATUS) & int::SERIAL_A, 0, "level bits ignore the write");
    assert_eq!(r.r8(reg::SERIAL_A), b'x');
    assert_eq!(r.r32(reg::INT_STATUS) & int::SERIAL_A, 0);
    assert!(!r.superio.level());
}

#[test]
fn loopback_returns_the_byte() {
    let r = Rig::new();
    r.w8(reg::SERIAL_A + 4, 0x13);
    r.w8(reg::SERIAL_A, 0x42);
    assert!(r.wire[0].out.lock().unwrap().is_empty(), "nothing leaves the chip");
    assert_eq!(r.r8(reg::SERIAL_A + 5) & 1, 1);
    assert_eq!(r.r8(reg::SERIAL_A), 0x42);
    // Modem outputs wire back to the inputs.
    assert_eq!(r.r8(reg::SERIAL_A + 6) & 0x30, 0x30, "RTS to CTS, DTR to DSR");
}

// ---------------------------------------------------------------------------
// Interrupt registers, chip control
// ---------------------------------------------------------------------------

#[test]
fn the_enable_mask_has_two_write_aliases() {
    let r = Rig::new();
    assert_eq!(r.r32(reg::INT_ENABLE_SET), 0);
    r.w32(reg::INT_ENABLE_SET, int::SERIAL_A | int::SERIAL_B);
    assert_eq!(r.r32(reg::INT_ENABLE_SET), int::SERIAL_A | int::SERIAL_B);
    r.w32(reg::INT_ENABLE_CLR, int::SERIAL_B);
    assert_eq!(r.r32(reg::INT_ENABLE_SET), int::SERIAL_A);
    r.w32(reg::INT_ENABLE_SET, int::KEYBOARD);
    assert_eq!(r.r32(reg::INT_ENABLE_SET), int::SERIAL_A | int::KEYBOARD);
}

#[test]
fn the_arbiter_reports_idle() {
    let r = Rig::new();
    assert_eq!(r.r32(reg::CHIP_CONTROL) & CHIP_CTRL_ARB_IDLE, CHIP_CTRL_ARB_IDLE);
    r.w32(reg::CHIP_CONTROL, 0x0007_8000);
    let cr = r.r32(reg::CHIP_CONTROL);
    assert_eq!(cr & CHIP_CTRL_ARB_IDLE, CHIP_CTRL_ARB_IDLE);
    assert_eq!(cr & 0x0007_8000, 0x0007_8000);
}

#[test]
fn the_interrupt_output_stores_its_configuration() {
    let r = Rig::new();
    r.w32(reg::INT_OUTPUT, IO_PERIODIC | 0x40);
    assert_eq!(r.r32(reg::INT_OUTPUT) & !IO_STATE, IO_PERIODIC | 0x40);
    // The pin readback bit is not stored.
    r.w32(reg::INT_OUTPUT, 0);
    r.w32(reg::INT_OUTPUT, IO_STATE);
    assert_eq!(r.r32(reg::INT_OUTPUT) & !IO_STATE, 0);
}

#[test]
fn a_fixed_level_is_visible_to_the_next_read() {
    let r = Rig::new();
    r.w32(reg::INT_OUTPUT, IO_HIGH);
    assert_eq!(r.r32(reg::INT_OUTPUT) & IO_STATE, IO_STATE);
    // A rising edge posts the output-pulse source, whatever the mode.
    assert_ne!(r.r32(reg::INT_STATUS) & int::RT_OUTPUT_PULSE, 0);
    r.w32(reg::INT_OUTPUT, IO_LOW);
    assert_eq!(r.r32(reg::INT_OUTPUT) & IO_STATE, 0);
}

#[test]
fn diagnostic_output_commands_keep_the_running_output() {
    let r = Rig::new();
    r.w32(reg::INT_OUTPUT, IO_PERIODIC | 100);
    r.w32(reg::INT_OUTPUT, IO_DIAG | IO_PERIODIC | 5);
    assert_eq!(r.c.sh.st.lock().unwrap().int_out.active, IO_PERIODIC | 100);
    // So do counts outside 2..=0xfffe.
    r.w32(reg::INT_OUTPUT, IO_SQUARE | 1);
    assert_eq!(r.c.sh.st.lock().unwrap().int_out.active, IO_PERIODIC | 100);
    r.w32(reg::INT_OUTPUT, IO_SQUARE | 0xffff);
    assert_eq!(r.c.sh.st.lock().unwrap().int_out.active, IO_PERIODIC | 100);
}

#[test]
fn a_periodic_output_catches_up_to_the_clock() {
    let r = Rig::new();
    r.w32(reg::INT_OUTPUT, IO_PERIODIC | 100);
    std::thread::sleep(Duration::from_millis(3));
    // Many periods (782 us each) have passed; the read applies them all and
    // the pulse source is posted.
    let _ = r.r32(reg::INT_OUTPUT);
    assert_ne!(r.r32(reg::INT_STATUS) & int::RT_OUTPUT_PULSE, 0);
    r.w32(reg::INT_STATUS, int::RT_OUTPUT_PULSE);
    // A fixed command cancels the periodic one for good.
    r.w32(reg::INT_OUTPUT, IO_LOW);
    std::thread::sleep(Duration::from_millis(2));
    let _ = r.r32(reg::INT_OUTPUT);
    assert_eq!(r.r32(reg::INT_STATUS) & int::RT_OUTPUT_PULSE, 0);
}

#[test]
fn reset_cancels_the_interrupt_output() {
    let r = Rig::new();
    r.w32(reg::INT_OUTPUT, IO_SQUARE | 2);
    r.c.reset();
    std::thread::sleep(Duration::from_millis(1));
    assert_eq!(r.r32(reg::INT_OUTPUT), 0);
    assert_eq!(r.r32(reg::INT_STATUS), 0);
}

#[test]
fn the_pulse_source_raises_the_superio_line_when_enabled() {
    let r = Rig::new();
    r.w32(reg::INT_ENABLE_SET, int::RT_OUTPUT_PULSE);
    r.w32(reg::INT_OUTPUT, IO_HIGH);
    assert!(r.superio.level());
    r.w32(reg::INT_STATUS, int::RT_OUTPUT_PULSE);
    assert!(!r.superio.level());
}

// ---------------------------------------------------------------------------
// GPIO and MicroLAN
// ---------------------------------------------------------------------------

#[test]
fn pin_control_has_two_write_aliases() {
    let r = Rig::new();
    r.w32(reg::PIN_CONTROL_SET, 0x00a0_0003);
    assert_eq!(r.r32(reg::PIN_CONTROL_SET), 0x00a0_0003);
    r.w32(reg::PIN_CONTROL_CLR, 0x0000_0001);
    assert_eq!(r.r32(reg::PIN_CONTROL_SET), 0x00a0_0002);
}

#[test]
fn a_pin_register_reaches_the_shared_data_word() {
    let r = Rig::new();
    r.w32(reg::PIN_DATA, 0);
    r.w32(reg::PIN_REG_FIRST + 4 * 6, 1);
    assert_eq!(r.r32(reg::PIN_DATA) & (1 << 6), 1 << 6);
    assert_eq!(r.r32(reg::PIN_REG_FIRST + 4 * 6), 1);
    r.w32(reg::PIN_REG_FIRST + 4 * 6, 0);
    assert_eq!(r.r32(reg::PIN_DATA) & (1 << 6), 0);
}

/// Pin 2 is the board's read-only product input: 1 on an Octane, 0 on an
/// Octane2. Writes and direction changes cannot replace it.
#[test]
fn the_product_input_is_read_only() {
    for (octane2, want) in [(false, 1u32), (true, 0)] {
        let r = Rig::with(Ioc3::octane(MAC, octane2));
        let pin2 = reg::PIN_REG_FIRST + 8;
        assert_eq!(r.r32(pin2), want);
        assert_eq!(r.r32(reg::PIN_DATA) >> 2 & 1, want);
        r.w32(pin2, 1 - want);
        r.w32(reg::PIN_DATA, if want == 1 { 0 } else { 0xffff_ffff });
        r.w32(reg::PIN_CONTROL_SET, 1 << 2);
        r.w32(reg::PIN_CONTROL_CLR, 1 << 2);
        assert_eq!(r.r32(pin2), want);
        assert_eq!(r.r32(reg::PIN_DATA) >> 2 & 1, want);
        r.c.reset();
        assert_eq!(r.r32(pin2), want, "a reset keeps the board's value");
        // An unrelated pin still writes.
        r.w32(reg::PIN_REG_FIRST + 4 * 3, 1);
        assert_eq!(r.r32(reg::PIN_REG_FIRST + 4 * 3), 1);
    }
}

/// The MicroLAN master, driven the way a guest drives it.
struct OneWire<'a>(&'a Rig);

impl OneWire<'_> {
    const DONE: u32 = 2;

    fn slot(&self, low_us: u32) -> bool {
        self.0.w32(reg::MICROLAN, (low_us << 10) | (13 << 2));
        let v = self.0.r32(reg::MICROLAN);
        assert_ne!(v & Self::DONE, 0, "a slot finishes inside the write");
        assert_eq!(v & !3, (low_us << 10) | (13 << 2), "the timing written reads back");
        v & 1 != 0
    }

    /// Reset; true if a part answered (the wire pulled low).
    fn reset(&self) -> bool {
        !self.slot(500)
    }

    fn bit(&self, b: bool) -> bool {
        self.slot(if b { 6 } else { 80 })
    }

    fn write_byte(&self, b: u8) {
        for i in 0..8 {
            self.bit(b >> i & 1 != 0);
        }
    }

    fn read_byte(&self) -> u8 {
        (0..8).fold(0, |a, i| a | (self.bit(true) as u8) << i)
    }

    fn enable(&self) {
        self.0.w32(reg::PIN_CONTROL_SET, 1 << 21);
    }
}

#[test]
fn the_bus_is_dead_until_enabled() {
    let r = Rig::new();
    let ow = OneWire(&r);
    assert!(!ow.reset(), "no presence with the engine unrouted");
    ow.enable();
    assert!(ow.reset());
}

/// Search ROM, enumerating every part.
fn search_all(ow: &OneWire) -> Vec<[u8; 8]> {
    fn bits(rom: &[u8; 8]) -> Vec<bool> {
        (0..64).map(|i| rom[i / 8] >> (i % 8) & 1 != 0).collect()
    }
    // Depth-first over the discrepancy tree: each pass replays the choices
    // made so far and takes the first new discrepancy it meets.
    fn walk(ow: &OneWire, path: &[bool], found: &mut Vec<[u8; 8]>) {
        assert!(ow.reset());
        ow.write_byte(0xf0);
        let mut rom = [0u8; 8];
        let mut branch_at = None;
        for i in 0..64 {
            let b = ow.bit(true);
            let c = ow.bit(true);
            let choice = match (b, c) {
                (true, true) => return,
                (true, false) => true,
                (false, true) => false,
                (false, false) if i < path.len() => path[i],
                (false, false) => {
                    branch_at = Some(i);
                    false
                }
            };
            ow.bit(choice);
            rom[i / 8] |= (choice as u8) << (i % 8);
        }
        match branch_at {
            Some(at) => {
                let mut zero = bits(&rom)[..at].to_vec();
                zero.push(false);
                walk(ow, &zero, found);
                let mut one = bits(&rom)[..at].to_vec();
                one.push(true);
                walk(ow, &one, found);
            }
            None if !found.contains(&rom) => found.push(rom),
            None => {}
        }
    }
    let mut found = vec![];
    walk(ow, &[], &mut found);
    found
}

fn match_rom(ow: &OneWire, rom: &[u8; 8]) {
    assert!(ow.reset());
    ow.write_byte(0x55);
    for b in rom {
        ow.write_byte(*b);
    }
}

fn ethernet_rom(ow: &OneWire) -> [u8; 8] {
    *search_all(ow).iter().find(|r| r[0] == 0x91).expect("the Ethernet-address part")
}

#[test]
fn a_search_finds_every_part_with_a_valid_rom() {
    let r = Rig::new();
    let ow = OneWire(&r);
    ow.enable();
    let found = search_all(&ow);
    assert_eq!(found.len(), 3, "the Ethernet part and two board records");
    for rom in &found {
        assert_eq!(rom[7], crc8_dallas(&rom[..7]), "ROM CRC-8");
    }
    let eth = found.iter().find(|r| r[0] == 0x91).expect("family 0x91");
    // UniqueWare prefix 0x5e7 in the top twelve bits of the serial.
    assert_eq!(eth[6], 0x5e);
    assert_eq!(eth[5] >> 4, 0x7);
    assert_eq!(found.iter().filter(|r| r[0] == 0x0b).count(), 2);
}

#[test]
fn skip_rom_with_several_parts_wires_their_answers_together() {
    let r = Rig::new();
    let ow = OneWire(&r);
    ow.enable();
    let found = search_all(&ow);
    assert!(ow.reset());
    ow.write_byte(0x33);
    let got = (0..8).map(|_| ow.read_byte()).collect::<Vec<_>>();
    // The wire is the AND of every part's ROM.
    let mut and = [0xffu8; 8];
    for rom in &found {
        for i in 0..8 {
            and[i] &= rom[i];
        }
    }
    assert_eq!(got, and);
}

#[test]
fn a_completed_search_leaves_the_part_selected() {
    let r = Rig::new();
    let ow = OneWire(&r);
    ow.enable();
    let eth = ethernet_rom(&ow);
    assert!(ow.reset());
    ow.write_byte(0xf0);
    for i in 0..64 {
        let want = eth[i / 8] >> (i % 8) & 1 != 0;
        let _ = ow.bit(true);
        let _ = ow.bit(true);
        ow.bit(want);
    }
    ow.write_byte(0xc3);
    ow.write_byte(0);
    ow.write_byte(0);
    assert_eq!(ow.read_byte(), crc8_dallas(&[0xc3, 0, 0]));
    assert_eq!(ow.read_byte(), 0x0a, "marker");
}

#[test]
fn a_wrong_match_rom_leaves_the_part_unselected() {
    let r = Rig::new();
    let ow = OneWire(&r);
    ow.enable();
    let mut eth = ethernet_rom(&ow);
    eth[3] ^= 1;
    match_rom(&ow, &eth);
    ow.write_byte(0xf0);
    ow.write_byte(0);
    ow.write_byte(0);
    assert_eq!(ow.read_byte(), 0xff, "nobody answers");
}

fn read_page_via(ow: &OneWire, rom: &[u8; 8], cmd: u8) -> [u8; 32] {
    match_rom(ow, rom);
    ow.write_byte(cmd);
    ow.write_byte(0);
    ow.write_byte(0);
    assert_eq!(ow.read_byte(), crc8_dallas(&[cmd, 0, 0]), "leading CRC-8 over the command");
    let mut page = [0u8; 32];
    for b in &mut page {
        *b = ow.read_byte();
    }
    page
}

fn assert_ethernet_page(page: &[u8; 32]) {
    assert_eq!(page[0], 0x0a);
    let mut mac = [0u8; 6];
    for i in 0..6 {
        mac[i] = page[10 - i];
    }
    assert_eq!(mac, MAC, "station address stored reversed");
    assert_eq!(crc16(0, &page[..13]), 0xb001, "embedded CRC-16 residue");
}

#[test]
fn reading_the_page_via_read_memory() {
    let r = Rig::new();
    let ow = OneWire(&r);
    ow.enable();
    let eth = ethernet_rom(&ow);
    assert_ethernet_page(&read_page_via(&ow, &eth, 0xf0));
}

#[test]
fn reading_the_page_via_read_memory_crc() {
    let r = Rig::new();
    let ow = OneWire(&r);
    ow.enable();
    let eth = ethernet_rom(&ow);
    let page = read_page_via(&ow, &eth, 0xc3);
    assert_ethernet_page(&page);
    assert_eq!(ow.read_byte(), crc8_dallas(&page), "CRC-8 closing the page");
}

/// Plain Read Memory has no per-page CRC: the byte after the first page is
/// the second page's first byte (erased).
#[test]
fn plain_read_memory_has_no_page_crc() {
    let r = Rig::new();
    let ow = OneWire(&r);
    ow.enable();
    let eth = ethernet_rom(&ow);
    let _ = read_page_via(&ow, &eth, 0xf0);
    assert_eq!(ow.read_byte(), 0xff);
}

#[test]
fn read_status_returns_no_redirection() {
    let r = Rig::new();
    let ow = OneWire(&r);
    ow.enable();
    let eth = ethernet_rom(&ow);
    match_rom(&ow, &eth);
    ow.write_byte(0xaa);
    ow.write_byte(1);
    ow.write_byte(0);
    assert_eq!(ow.read_byte(), crc8_dallas(&[0xaa, 1, 0]));
    let st: Vec<u8> = (0..7).map(|_| ow.read_byte()).collect();
    assert_eq!(&st[..4], &[0xff; 4], "no page redirected");
    assert_eq!(st[6], 0x00, "factory-programmed byte at 0007");
    assert_eq!(ow.read_byte(), crc8_dallas(&st));
}

#[test]
fn the_board_records_read_back_as_text() {
    let r = Rig::new();
    let ow = OneWire(&r);
    ow.enable();
    let found = search_all(&ow);
    let mut names = vec![];
    for rom in found.iter().filter(|r| r[0] == 0x0b) {
        match_rom(&ow, rom);
        ow.write_byte(0xf0);
        ow.write_byte(0);
        ow.write_byte(0);
        let rec: Vec<u8> = (0..64).map(|_| ow.read_byte()).collect();
        assert_eq!(rec[0], 0x01, "format byte");
        assert_eq!(crc16(0, &rec[..32]), 0xb001, "page 0 residue");
        assert_eq!(crc16(0, &rec[32..64]), 0xb001, "page 1 residue");
        assert_eq!(&rec[42..48], &[0xff; 6]);
        names.push(String::from_utf8(rec[48..62].to_vec()).unwrap().trim_end().to_string());
    }
    names.sort();
    assert_eq!(names, ["FP1", "PWR.SPPLY.SR"]);
}

// ---------------------------------------------------------------------------
// PS/2
// ---------------------------------------------------------------------------

const K_RX: u32 = 0xa0;
const M_RX: u32 = 0xa4;
const K_TX: u32 = 0xa8;
const M_TX: u32 = 0xac;
const CTRL: u32 = 0x9c;
const VALID: [u32; 3] = [1 << 29, 1 << 30, 1 << 31];

#[test]
fn the_ps2_ports_read_empty() {
    let r = Rig::new();
    assert_eq!(r.r32(CTRL), 0);
    assert_eq!(r.r32(K_RX), 0);
    assert_eq!(r.r32(M_RX), 0);
    assert_eq!(r.r32(reg::INT_STATUS) & int::KEYBOARD, 0);
}

#[test]
fn the_ps2_keyboard_round_trips() {
    let r = Rig::new();
    r.w32(K_TX, 0xff);
    assert_ne!(r.r32(reg::INT_STATUS) & int::KEYBOARD, 0, "a reply latches the source");
    let v = r.r32(K_RX);
    // The oldest byte is in the highest slot: the ACK.
    assert_ne!(v & VALID[2], 0);
    assert_eq!((v >> 16) & 0xff, 0xfa);
    assert_eq!(v & (1 << 27), 0, "from the keyboard");
}

#[test]
fn the_ps2_mouse_round_trips() {
    let r = Rig::new();
    r.w32(M_TX, 0xf2);
    let v = r.r32(M_RX);
    assert_ne!(v & VALID[2], 0);
    assert_ne!(v & (1 << 27), 0, "from the mouse");
    assert_eq!((v >> 16) & 0xff, 0xfa);
    assert_eq!(r.r32(K_RX), 0, "and not the keyboard");
}

#[test]
fn the_ps2_receive_drains_on_read_but_the_interrupt_latches() {
    let r = Rig::new();
    r.w32(K_TX, 0xff);
    assert_ne!(r.r32(K_RX), 0);
    assert_eq!(r.r32(K_RX), 0, "a read drains the holding");
    assert_ne!(r.r32(reg::INT_STATUS) & int::KEYBOARD, 0, "the latch stays set");
    r.w32(reg::INT_STATUS, int::KEYBOARD);
    assert_eq!(r.r32(reg::INT_STATUS) & int::KEYBOARD, 0);
}

fn type_keys(r: &Rig) {
    let ps2 = r.c.ps2();
    for k in [winit::keyboard::KeyCode::KeyA, winit::keyboard::KeyCode::KeyB, winit::keyboard::KeyCode::KeyC] {
        ps2.push_kb(k, true);
        ps2.push_kb(k, false);
    }
}

/// More than three characters behind the clamp-after-three bits: three now,
/// the rest once the guest reads, and nothing lost.
#[test]
fn the_ps2_receive_holds_three_behind_the_clamp() {
    let r = Rig::new();
    r.w32(CTRL, KBC_K_CLAMP_3 | KBC_M_CLAMP_3);
    r.w32(K_TX, 0xf4); // enable scanning
    let _ = r.r32(K_RX);
    r.w32(reg::INT_STATUS, int::KEYBOARD);
    type_keys(&r);
    assert_ne!(r.r32(reg::INT_STATUS) & int::KEYBOARD, 0);
    let first = r.r32(K_RX);
    assert_eq!(first & (VALID[0] | VALID[1] | VALID[2]), VALID[0] | VALID[1] | VALID[2]);
    assert_eq!(first & KBC_RX_OVERFLOW, 0, "nothing lost");
    // The read released the clamp; the stopped keyboard resumed.
    let second = r.r32(K_RX);
    assert_ne!(second & VALID[2], 0, "the rest were held back, not dropped");
    assert_ne!((second >> 16) & 0xff, (first >> 16) & 0xff, "and are not a replay");
}

#[test]
fn the_ps2_receive_overflows_without_a_clamp() {
    let r = Rig::new();
    r.w32(K_TX, 0xf4);
    let _ = r.r32(K_RX);
    type_keys(&r);
    let v = r.r32(K_RX);
    assert_ne!(v & KBC_RX_OVERFLOW, 0, "a fourth character was lost");
}

#[test]
fn the_ps2_control_masks_writes() {
    let r = Rig::new();
    const K_TO_EN: u32 = 0x0004_0000;
    const M_TO_EN: u32 = 0x0008_0000;
    r.w32(CTRL, K_TO_EN);
    assert_eq!(r.r32(CTRL), K_TO_EN);
    r.w32(CTRL, K_TO_EN | M_TO_EN);
    assert_eq!(r.r32(CTRL), K_TO_EN | M_TO_EN);
    r.w32(CTRL, 0x0001_0000 | 0x0002_0000);
    assert_eq!(r.r32(CTRL), 0, "timeout status bits do not store");
    r.w32(CTRL, 0xffff_ffff);
    assert_eq!(
        r.r32(CTRL),
        0x0000_ffff | K_TO_EN | M_TO_EN | KBC_K_CLAMP_1 | KBC_M_CLAMP_1 | KBC_K_CLAMP_3 | KBC_M_CLAMP_3
    );
    r.c.reset();
    assert_eq!(r.r32(CTRL), 0);
}

#[test]
fn a_host_key_latches_the_interrupt_without_a_guest_access() {
    let r = Rig::new();
    r.w32(reg::INT_ENABLE_SET, int::KEYBOARD);
    r.w32(K_TX, 0xf4);
    let _ = r.r32(K_RX);
    r.w32(reg::INT_STATUS, int::KEYBOARD);
    assert!(!r.superio.level());
    r.c.ps2().push_kb(winit::keyboard::KeyCode::KeyA, true);
    assert!(r.superio.level(), "the SuperIO line rose from the host thread");
}

// ---------------------------------------------------------------------------
// Serial DMA
// ---------------------------------------------------------------------------

const A: u32 = reg::SDMA_PORT_A;
const B: u32 = reg::SDMA_PORT_B;

#[test]
fn the_serial_dma_indices_take_a_write() {
    let r = Rig::new();
    r.w32(A + sdma_off::TX_PRODUCE, 0x40);
    assert_eq!(r.r32(A + sdma_off::TX_PRODUCE), 0x40);
    r.w32(A + sdma_off::RX_CONSUME, 0x48);
    assert_eq!(r.r32(A + sdma_off::RX_CONSUME), 0x48);
    // The two the engine advances can still be placed by software.
    r.w32(A + sdma_off::TX_CONSUME, 0x50);
    assert_eq!(r.r32(A + sdma_off::TX_CONSUME), 0x50);
    r.w32(A + sdma_off::RX_PRODUCE, 0x58);
    assert_eq!(r.r32(A + sdma_off::RX_PRODUCE), 0x58);
    // Only the receive consume index keeps bit 31; the rest keep the pointer.
    r.w32(A + sdma_off::RX_CONSUME, 0xffff_ffff);
    assert_eq!(r.r32(A + sdma_off::RX_CONSUME), 0x8000_03f8);
    for o in [sdma_off::TX_PRODUCE, sdma_off::TX_CONSUME, sdma_off::RX_PRODUCE] {
        r.w32(A + o, 0xffff_ffff);
        assert_eq!(r.r32(A + o), 0x3f8);
    }
}

#[test]
fn the_serial_dma_ports_are_distinct() {
    let r = Rig::new();
    r.w32(A + sdma_off::TX_PRODUCE, 0x18);
    r.w32(B + sdma_off::TX_PRODUCE, 0x28);
    assert_eq!(r.r32(A + sdma_off::TX_PRODUCE), 0x18);
    assert_eq!(r.r32(B + sdma_off::TX_PRODUCE), 0x28);
    r.w32(A + sdma_off::TX_CONSUME, 0x20);
    r.w32(B + sdma_off::TX_CONSUME, 0x30);
    assert_eq!(r.r32(A + sdma_off::TX_CONSUME), 0x20);
    assert_eq!(r.r32(B + sdma_off::TX_CONSUME), 0x30);
    r.w32(reg::SDMA_RING_LO, 0x3000);
    assert_eq!(r.r32(reg::SDMA_RING_LO), 0x3000, "the ring base is shared");
}

#[test]
fn the_serial_dma_shadow_is_read_only() {
    let r = Rig::new();
    for base in [A, B] {
        let s = r.r32(base + sdma_off::SHADOW);
        r.w32(base + sdma_off::SHADOW, 0xffff_ffff);
        assert_eq!(r.r32(base + sdma_off::SHADOW), s);
        assert_ne!(s & (1 << 23), 0, "the port comes up with carrier asserted");
        assert_ne!(s & (1 << 20), 0, "and clear to send");
        assert_eq!(s & 0x60, 0x60, "transmitter empty");
    }
}

#[test]
fn the_alias_pages_expose_one_port_and_refuse_the_engine_indices() {
    let r = Rig::new();
    r.w32(A + sdma_off::TX_PRODUCE, 0x18);
    assert_eq!(r.r32(reg::SDMA_ALIAS_A + sdma_off::TX_PRODUCE), 0x18);
    assert_eq!(r.r32(reg::SDMA_ALIAS_B + sdma_off::TX_PRODUCE), 0);
    r.w32(reg::SDMA_ALIAS_A + sdma_off::TX_CONSUME, 0x30);
    assert_eq!(r.r32(A + sdma_off::TX_CONSUME), 0, "read-only through the page");
    r.w32(reg::SDMA_ALIAS_A + sdma_off::TX_PRODUCE, 0x20);
    assert_eq!(r.r32(A + sdma_off::TX_PRODUCE), 0x20, "the producer index is writable");
}

#[test]
fn the_receive_timer_reload_loads_the_count() {
    let r = Rig::new();
    r.w32(A + sdma_off::RX_TIMER, 0xfff);
    assert_eq!(r.r32(A + sdma_off::RX_TIMER) & 0x0fff_0fff, 0x0fff_0fff);
}

/// Write `data` as internal bytes at internal address `a`.
fn put(r: &Rig, a: u64, data: &[u8]) {
    assert!(r.mem_view().dma_write(a, data).is_ok());
}

fn get(r: &Rig, a: u64, n: usize) -> Vec<u8> {
    let mut b = vec![0; n];
    assert!(r.mem_view().dma_read(a, &mut b).is_ok());
    b
}

/// Ring DMA in loopback: four characters out of the transmit ring come back
/// through the receive ring as one entry, with their status bytes.
#[test]
fn ring_dma_loops_back_through_port_a() {
    let r = Rig::new();
    r.aim_engines();
    r.w32(reg::SDMA_RING_LO, 0x2000 | 1); // 4 KB rings
    r.w8(reg::SERIAL_A + 4, 0x11);
    put(&r, 0x2000, &[b'a', b'b', b'c', b'd', 0x40, 0x40, 0x40, 0x40]);
    r.w32(A, sdma_ctl::ENABLE);
    r.w32(A + sdma_off::TX_PRODUCE, 8);
    assert_eq!(r.r32(A + sdma_off::TX_CONSUME), 8, "the engine took the entry");
    assert_eq!(r.r32(A + sdma_off::RX_PRODUCE) & 0xfff, 8, "and posted one back");
    let e = get(&r, 0x3000, 8);
    assert_eq!(&e[..4], b"abcd");
    for s in &e[4..] {
        assert_eq!(s & 0xc0, 0xc0, "data and modem fields valid");
        assert_eq!(s & 0x0f, 0, "no errors");
    }
    assert!(r.wire[0].out.lock().unwrap().is_empty());
}

#[test]
fn ring_dma_runs_on_port_b_too() {
    let r = Rig::new();
    r.aim_engines();
    r.w32(reg::SDMA_RING_LO, 0x2000 | 1);
    r.w8(reg::SERIAL_B + 4, 0x11);
    put(&r, 0x4000, &[b'w', b'x', b'y', b'z', 0x40, 0x40, 0x40, 0x40]);
    r.w32(B, sdma_ctl::ENABLE);
    r.w32(B + sdma_off::TX_PRODUCE, 8);
    assert_eq!(r.r32(B + sdma_off::TX_CONSUME), 8);
    assert_eq!(&get(&r, 0x5000, 4), b"wxyz");
}

/// Flow control in loopback reports clear-to-send even with RTS clear.
#[test]
fn flow_control_in_loopback_reports_cts() {
    let r = Rig::new();
    r.aim_engines();
    r.w32(reg::SDMA_RING_LO, 0x2000 | 1);
    r.w8(reg::SERIAL_A + 4, 0x11);
    put(&r, 0x2000, &[b'a', b'b', b'c', b'd', 0x40, 0x40, 0x40, 0x40]);
    r.w32(A, sdma_ctl::ENABLE | sdma_ctl::FLOW_CONTROL | 32);
    r.w32(A + sdma_off::TX_PRODUCE, 8);
    let e = get(&r, 0x3000, 8);
    assert_eq!(e[4] & 0xdf, 0xd0, "data valid, modem valid, clear to send");
}

/// An empty slot ends its entry wherever it sits; the characters behind it
/// are not sent.
#[test]
fn an_empty_slot_ends_the_entry() {
    let r = Rig::new();
    r.aim_engines();
    r.w32(reg::SDMA_RING_LO, 0x2000 | 1);
    r.w8(reg::SERIAL_A + 4, 0x11);
    put(&r, 0x2000, &[b'a', b'b', b'c', b'd', 0x40, 0x00, 0x40, 0x40]);
    put(&r, 0x2008, &[b'e', b'f', b'g', b'h', 0x40, 0x40, 0x40, 0x00]);
    r.w32(A, sdma_ctl::ENABLE);
    r.w32(A + sdma_off::TX_PRODUCE, 16);
    assert_eq!(r.r32(A + sdma_off::TX_CONSUME), 16);
    // a, then e f g: "aefg" in one receive entry.
    assert_eq!(&get(&r, 0x3000, 4), b"aefg");
}

#[test]
fn ring_dma_transmits_to_the_backend_and_raises_the_explicit_source() {
    let r = Rig::new();
    r.aim_engines();
    r.w32(reg::SDMA_RING_LO, 0x2000 | 1);
    put(&r, 0x2000, &[b'o', b'k', b'\n', 0, 0x40, 0x40, 0x60, 0x00]);
    r.w32(A, sdma_ctl::ENABLE);
    r.w32(A + sdma_off::TX_PRODUCE, 8);
    assert_eq!(&*r.wire[0].out.lock().unwrap(), b"ok\n");
    let bit = int::serial(0, int::TX_EXPLICIT);
    assert_ne!(r.r32(reg::INT_STATUS) & bit, 0);
    r.w32(reg::INT_STATUS, bit);
    assert_eq!(r.r32(reg::INT_STATUS) & bit, 0, "latched, then cleared");
}

#[test]
fn transmit_ring_empty_is_a_level() {
    let r = Rig::new();
    r.aim_engines();
    r.w32(reg::SDMA_RING_LO, 0x2000 | 1);
    let bit = int::serial(0, int::TX_RING_EMPTY);
    assert_eq!(r.r32(reg::INT_STATUS) & bit, 0, "zero while ring DMA is off");
    r.w32(A, sdma_ctl::ENABLE);
    assert_ne!(r.r32(reg::INT_STATUS) & bit, 0);
    r.w32(reg::INT_STATUS, bit);
    assert_ne!(r.r32(reg::INT_STATUS) & bit, 0, "a write cannot clear it");
}

/// Host input reaches the receive ring through the service pass, with the
/// 16550's own interrupts off, and the arm and timer post the partial entry.
#[test]
fn host_input_arrives_in_the_receive_ring_after_the_timer() {
    let r = Rig::new();
    r.aim_engines();
    r.w32(reg::SDMA_RING_LO, 0x2000 | 1);
    // 0x200 ticks of 62.5 us: 32 ms, well clear of a slow test host.
    r.w32(A + sdma_off::RX_TIMER, 0x200);
    r.w32(A + sdma_off::RX_CONSUME, ARM_RX_TIMER);
    r.w32(A, sdma_ctl::ENABLE | 4);
    r.wire[0].inp.lock().unwrap().extend(b"hi");
    r.c.service();
    assert_eq!(r.r32(A + sdma_off::RX_PRODUCE) >> HELD_SHIFT & 7, 2, "two held in the packer");
    assert_eq!(r.r32(A + sdma_off::RX_CONSUME) & ARM_RX_TIMER, ARM_RX_TIMER);
    std::thread::sleep(Duration::from_millis(60));
    r.c.service();
    assert_eq!(r.r32(A + sdma_off::RX_PRODUCE), 8, "posted when the timer ran out");
    assert_eq!(&get(&r, 0x3000, 2), b"hi");
    assert_ne!(r.r32(reg::INT_STATUS) & int::serial(0, int::RX_TIMER), 0);
    assert_eq!(r.r32(A + sdma_off::RX_CONSUME) & ARM_RX_TIMER, 0, "the arm is spent");
}

#[test]
fn an_engine_aimed_elsewhere_does_not_reach_its_port() {
    let r = Rig::new();
    r.w32(reg::CHIP_CONTROL, 0x0003_8000);
    r.w32(reg::SDMA_RING_LO, 0x2000 | 1);
    put(&r, 0x2000, &[b'a', 0, 0, 0, 0x40, 0, 0, 0]);
    r.w32(A, sdma_ctl::ENABLE);
    r.w32(A + sdma_off::TX_PRODUCE, 8);
    assert_eq!(r.r32(A + sdma_off::TX_CONSUME), 0);
    assert!(r.wire[0].out.lock().unwrap().is_empty());
}

#[test]
fn ioc3_dma_places_bytes_xor_three() {
    let r = Rig::new();
    let d = r.mem_view();
    // A big-endian word at an aligned address.
    assert!(d.dma_write(0x100, &[0x11, 0x22, 0x33, 0x44]).is_ok());
    assert_eq!(&r.mem.0.lock().unwrap()[0x100..0x104], &[0x44, 0x33, 0x22, 0x11]);
    // A single byte lands on its own lane.
    assert!(d.dma_write(0x201, &[0xab]).is_ok());
    assert_eq!(r.mem.0.lock().unwrap()[0x202], 0xab);
    // And a read of an unaligned span undoes the same placement.
    let mut b = [0u8; 3];
    assert!(d.dma_read(0x100, &mut b).is_ok());
    assert_eq!(b, [0x11, 0x22, 0x33]);
    // A failed access reports it, and the upper address bits pass through.
    let mut big = [0u8; 8];
    assert!(!d.dma_read(0xfffc, &mut big).is_ok());
    assert!(!d.dma_read(0x1_0000_0000, &mut big).is_ok());
}

// ---------------------------------------------------------------------------
// Buffer RAM, byte bus, parallel, Ethernet window
// ---------------------------------------------------------------------------

#[test]
fn the_buffer_ram_holds_seventeen_bits_and_does_not_alias() {
    let r = Rig::new();
    r.w32(reg::SSRAM, 0xffff_ffff);
    r.w32(reg::SSRAM + 0x20000, 0x1234);
    assert_eq!(r.r32(reg::SSRAM), 0x0001_ffff);
    assert_eq!(r.r32(reg::SSRAM + 0x20000), 0x1234, "a window aliasing at its midpoint fails the size probe");
    assert_eq!(r.r32(reg::SSRAM + 0x3fffc), 0);
}

#[test]
fn the_clock_is_reached_through_an_index_and_a_data_port() {
    let r = Rig::new();
    r.w8(reg::BYTEBUS_DEV1, 0x0b);
    r.w8(reg::BYTEBUS_DEV2, 0x06);
    assert_eq!(r.r8(reg::BYTEBUS_DEV2), 0x06);
    r.w8(reg::BYTEBUS_DEV1, 0x20);
    r.w8(reg::BYTEBUS_DEV2, 0xa5);
    r.w8(reg::BYTEBUS_DEV1, 0x21);
    r.w8(reg::BYTEBUS_DEV2, 0x5a);
    r.w8(reg::BYTEBUS_DEV1, 0x20);
    assert_eq!(r.r8(reg::BYTEBUS_DEV2), 0xa5);
    r.w8(reg::BYTEBUS_DEV1, 0x21);
    assert_eq!(r.r8(reg::BYTEBUS_DEV2), 0x5a);
    // The data port answers on any lane of its word.
    assert_eq!(r.c.bar_read(0, reg::BYTEBUS_DEV2 + 3, 1), 0x5a, "the lane for offset 0");
    assert_eq!(r.r8(reg::BYTEBUS_DEV2 + 3), 0x5a, "and offset 3");
    // Register D reads valid-RAM-and-time.
    r.w8(reg::BYTEBUS_DEV1, 0x0d);
    assert_eq!(r.r8(reg::BYTEBUS_DEV2) & 0x80, 0x80);
}

#[test]
fn the_clock_tells_the_time() {
    let r = Rig::new();
    r.w8(reg::BYTEBUS_DEV1, 0x09);
    let year = r.r8(reg::BYTEBUS_DEV2);
    assert!(year >= 0x26, "host time on power-up, in BCD: {year:#x}");
}

#[test]
fn nvram_survives_in_a_file() {
    let path = std::env::temp_dir().join(format!("ioc3-nvram-{}", std::process::id()));
    {
        let r = Rig::new();
        r.c.set_nvram_file(&path).unwrap();
        r.w8(reg::BYTEBUS_DEV1, 0x30);
        r.w8(reg::BYTEBUS_DEV2, 0xc3);
    }
    {
        let r = Rig::new();
        r.c.set_nvram_file(&path).unwrap();
        r.w8(reg::BYTEBUS_DEV1, 0x30);
        assert_eq!(r.r8(reg::BYTEBUS_DEV2), 0xc3);
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn software_power_off_is_reported() {
    let r = Rig::new();
    assert!(!r.c.powered_off());
    r.w8(reg::BYTEBUS_DEV1, 0x0a);
    r.w8(reg::BYTEBUS_DEV2, 0x10); // bank 1
    r.w8(reg::BYTEBUS_DEV1, 0x4a);
    r.w8(reg::BYTEBUS_DEV2, 0x08);
    assert!(r.c.powered_off());
}

const ECP: u32 = reg::PAR_ECP_BASE;

#[test]
fn the_parallel_port_presents_the_probe_values() {
    let r = Rig::new();
    assert_eq!(r.r8(ECP + 0xa), PAR_ECR_FIFO_EMPTY, "empty, not full: the driver's presence check");
    assert_eq!(r.r8(ECP + 1) & 0x08, 0x08, "no fault");
}

#[test]
fn the_parallel_test_fifo_stores_and_returns_bytes_in_order() {
    let r = Rig::new();
    r.w8(ECP + 0xa, 6 << 5);
    for i in 0..4 {
        r.w8(ECP + 8, i + 1);
    }
    assert_eq!(r.r8(ECP + 0xa) & 3, 0, "neither empty nor full");
    for i in 0..4 {
        assert_eq!(r.r8(ECP + 8), i + 1);
    }
    assert_ne!(r.r8(ECP + 0xa) & PAR_ECR_FIFO_EMPTY, 0);
    for i in 0..16 {
        r.w8(ECP + 8, i);
    }
    assert_ne!(r.r8(ECP + 0xa) & PAR_ECR_FIFO_FULL, 0, "sixteen bytes is full");
}

#[test]
fn the_parallel_configuration_is_fixed_in_config_mode() {
    let r = Rig::new();
    r.w8(ECP + 0xa, 7 << 5);
    assert_eq!(r.r8(ECP + 8), PAR_CONFIG_A);
    assert_eq!(r.r8(ECP + 9), PAR_CONFIG_B);
    r.w8(ECP + 8, 0);
    r.w8(ECP + 9, 0);
    assert_eq!(r.r8(ECP + 8), PAR_CONFIG_A, "read-only on the fitted part");
}

/// A DMA context drains to the backend and completion is the pass-through
/// service level, quieted by a write to the extended control register.
#[test]
fn parallel_dma_drains_a_context_and_raises_service() {
    let r = Rig::new();
    let out = Arc::new(Wire::default());
    r.c.set_parallel_backend(out.clone());
    put(&r, 0x3000, b"print me");
    r.w32(0x80, 0);
    r.w32(0x84, 0x3000);
    r.w8(ECP + 0xa, PAR_ECR_DMA_ENABLE | (2 << 5));
    r.w32(0x8c, PAR_DMA_ENABLE);
    r.w32(0x88, 8 | PAR_COUNT_ARM | PAR_COUNT_LAST);
    assert_eq!(&*out.out.lock().unwrap(), b"print me");
    let st = r.r32(reg::INT_STATUS);
    assert_ne!(st & (1 << int::PARALLEL_CONTEXT), 0, "context A's own source latched");
    assert_ne!(st & int::PARALLEL_SERVICE, 0);
    // The context disarmed itself.
    assert_eq!(r.r32(0x88) & (PAR_COUNT | PAR_COUNT_ARM), 0);
    r.w32(reg::INT_STATUS, 0xffff_ffff);
    let st = r.r32(reg::INT_STATUS);
    assert_eq!(st & (1 << int::PARALLEL_CONTEXT), 0, "latched bits clear on a write");
    assert_ne!(st & int::PARALLEL_SERVICE, 0, "the service level stays");
    r.w8(ECP + 0xa, PAR_ECR_DMA_ENABLE | (2 << 5));
    assert_eq!(r.r32(reg::INT_STATUS) & int::PARALLEL_SERVICE, 0, "quieted at the port");
}

/// What the Ethernet block is told and asked.
#[derive(Default)]
struct MockEth {
    regs: Mutex<std::collections::BTreeMap<u32, u32>>,
    wired: Mutex<Option<(Arc<dyn PciDma>, Box<dyn Fn(bool) + Send + Sync>)>>,
}

impl Ioc3EthPort for MockEth {
    fn read32(&self, off: u32) -> u32 {
        *self.regs.lock().unwrap().get(&off).unwrap_or(&0)
    }
    fn write32(&self, off: u32, val: u32) {
        self.regs.lock().unwrap().insert(off, val);
    }
    fn connect(&self, dma: Arc<dyn PciDma>, irq: Box<dyn Fn(bool) + Send + Sync>) {
        *self.wired.lock().unwrap() = Some((dma, irq));
    }
}

#[test]
fn the_ethernet_registers_are_forwarded_and_the_block_is_wired() {
    let r = Rig::new();
    let eth = Arc::new(MockEth::default());
    r.c.attach_eth(eth.clone());
    // Attached after the bridge gave DMA: wired at once.
    assert!(eth.wired.lock().unwrap().is_some());
    r.w32(0xf4, 0x1234);
    r.w32(0x150, 0xbeef);
    assert_eq!(eth.read32(0xf4), 0x1234);
    assert_eq!(eth.read32(0x150), 0xbeef);
    assert_eq!(r.r32(0xf4), 0x1234);
    // Just before the block and just after it are not the block's.
    r.w32(0xec, 7);
    assert_eq!(eth.read32(0xec), 0);
    r.w32(0x154, 7);
    assert_eq!(eth.read32(0x154), 0);
    // Only 32-bit accesses reach the block.
    assert_eq!(r.r8(0xf4), 0);
}

#[test]
fn the_ethernet_block_gets_the_chips_view_of_memory() {
    let r = Rig::new();
    let eth = Arc::new(MockEth::default());
    r.c.attach_eth(eth.clone());
    let dma = eth.wired.lock().unwrap().as_ref().unwrap().0.clone();
    assert!(dma.dma_write(0x500, &[0xde, 0xad, 0xbe, 0xef]).is_ok());
    assert_eq!(&r.mem.0.lock().unwrap()[0x500..0x504], &[0xef, 0xbe, 0xad, 0xde]);
    let mut b = [0u8; 4];
    assert!(dma.dma_read(0x500, &mut b).is_ok());
    assert_eq!(b, [0xde, 0xad, 0xbe, 0xef]);
}

/// Ethernet drives the PCI pin; the SuperIO line is a different one.
#[test]
fn the_ethernet_level_drives_the_pci_pin_not_the_superio_line() {
    let c = Ioc3::octane(MAC, false);
    let eth = Arc::new(MockEth::default());
    c.attach_eth(eth.clone());
    c.connect_dma(TestMem::new());
    let (pin, line) = LevelProbe::line();
    c.connect_interrupt(line);
    let (sup, sline) = LevelProbe::line();
    c.connect_superio_interrupt(sline);
    let wired = eth.wired.lock().unwrap();
    let irq = &wired.as_ref().unwrap().1;
    irq(true);
    assert!(pin.level());
    assert!(!sup.level());
    assert_eq!(c.int_status(), 0, "no SuperIO status bit for it");
    irq(false);
    assert!(!pin.level());
}

#[test]
fn buffer_ram_parity_follows_the_mac_control_bit() {
    let r = Rig::new();
    let eth = Arc::new(MockEth::default());
    r.c.attach_eth(eth.clone());
    // Checking off: stored as written, 17 bits, no error flag.
    r.w32(reg::SSRAM, 0x0001_0001);
    assert_eq!(r.r32(reg::SSRAM), 0x0001_0001);
    // Checking on: an odd number of ones in the stored word reads as a
    // parity error.
    eth.write32(reg::ENET_MAC_CONTROL, ENET_MCR_RAM_PARITY);
    r.w32(reg::SSRAM + 4, 0x0000_0001);
    assert_eq!(r.r32(reg::SSRAM + 4), 0x0000_0001 | SSRAM_PARITY_ERROR);
    // Bit 17 asks for generated parity: data 0x0001 needs the parity bit set.
    r.w32(reg::SSRAM + 8, 0x0000_0001 | SSRAM_PARITY_ERROR);
    assert_eq!(r.r32(reg::SSRAM + 8), 0x0001_0001);
}

#[test]
fn reset_clears_chip_state_but_not_the_buffer_ram() {
    let r = Rig::new();
    r.w32(reg::INT_ENABLE_SET, 0xff);
    r.w32(reg::SSRAM, 0x55);
    r.w32(reg::PIN_CONTROL_SET, 1 << 21);
    r.c.reset();
    assert_eq!(r.r32(reg::INT_ENABLE_SET), 0);
    assert_eq!(r.r32(reg::PIN_CONTROL_SET), 0);
    assert_eq!(r.r32(reg::SSRAM), 0x55);
}

// ---------------------------------------------------------------------------
// Review fixes
// ---------------------------------------------------------------------------

/// Three threads change what drives the SuperIO line; once they stop, the
/// line must agree with the status and enable registers.
#[test]
fn the_superio_line_never_ends_up_stale_across_threads() {
    for _ in 0..20 {
        let r = Arc::new(Rig::new());
        r.w32(reg::INT_ENABLE_SET, int::KEYBOARD);
        r.w32(K_TX, 0xf4);
        let _ = r.r32(K_RX);
        let go = |f: Box<dyn Fn() + Send>| std::thread::spawn(f);
        let (a, b, c) = (r.clone(), r.clone(), r.clone());
        let t = [
            go(Box::new(move || {
                for _ in 0..200 {
                    a.c.ps2().push_kb(winit::keyboard::KeyCode::KeyA, true);
                    a.c.ps2().push_kb(winit::keyboard::KeyCode::KeyA, false);
                }
            })),
            go(Box::new(move || {
                for _ in 0..200 {
                    b.w32(reg::INT_STATUS, int::KEYBOARD);
                    let _ = b.r32(K_RX);
                }
            })),
            go(Box::new(move || {
                for _ in 0..200 {
                    c.c.service();
                }
            })),
        ];
        for t in t {
            t.join().unwrap();
        }
        r.c.service();
        let want = r.r32(reg::INT_STATUS) & r.r32(reg::INT_ENABLE_SET) != 0;
        assert_eq!(r.superio.level(), want);
    }
}

#[test]
fn the_ethernet_pin_follows_the_last_level_given() {
    let c = Ioc3::octane(MAC, false);
    let eth = Arc::new(MockEth::default());
    c.attach_eth(eth.clone());
    c.connect_dma(TestMem::new());
    let (pin, line) = LevelProbe::line();
    c.connect_interrupt(line);
    let wired = eth.wired.lock().unwrap();
    let irq = &wired.as_ref().unwrap().1;
    for i in 0..1000 {
        irq(i % 2 == 0);
    }
    irq(true);
    assert!(pin.level());
}

#[test]
fn a_ring_at_the_top_of_the_address_space_is_a_dma_error_not_a_panic() {
    let r = Rig::new();
    r.aim_engines();
    r.w32(reg::SDMA_RING_HI, 0xffff_ffff);
    r.w32(reg::SDMA_RING_LO, 0xffff_f000 | 1);
    r.w32(A, sdma_ctl::ENABLE);
    r.w32(A + sdma_off::TX_PRODUCE, 8);
    assert_ne!(r.r32(reg::INT_STATUS) & int::serial(0, int::DMA_ERROR), 0);
    let d = r.mem_view();
    let mut b = [0u8; 8];
    assert!(!d.dma_read(u64::MAX - 2, &mut b).is_ok());
    assert!(!d.dma_write(u64::MAX - 2, &b).is_ok());
}

#[test]
fn an_unaligned_dma_write_keeps_its_neighbours_in_one_transfer() {
    let r = Rig::new();
    let d = r.mem_view();
    put(&r, 0x100, &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert!(d.dma_write(0x101, &[0xaa, 0xbb, 0xcc]).is_ok());
    assert_eq!(get(&r, 0x100, 8), [1, 0xaa, 0xbb, 0xcc, 5, 6, 7, 8]);
}

#[test]
fn the_ethernet_address_part_is_a_64_byte_ds2501() {
    let p = ethernet_address_part(MAC, 0x5e7_454d55_000);
    assert_eq!(p.mem.len(), 64);
    assert_eq!(p.rom[0], 0x91);
}

/// The 32-bit blocks refuse narrower accesses: they read zero and a write
/// does nothing.
#[test]
fn narrow_accesses_to_32_bit_registers_are_refused() {
    let r = Rig::new();
    r.w32(reg::INT_ENABLE_SET, 0x1234_5678);
    assert_eq!(r.r8(reg::INT_ENABLE_SET), 0);
    assert_eq!(r.c.bar_read(0, reg::INT_ENABLE_SET, 2), 0);
    r.w8(reg::INT_ENABLE_SET, 0xff);
    assert_eq!(r.r32(reg::INT_ENABLE_SET), 0x1234_5678);
    // The byte-wide windows are unaffected.
    r.w8(reg::SERIAL_A + 7, 0x77);
    assert_eq!(r.r8(reg::SERIAL_A + 7), 0x77);
}

// ---------------------------------------------------------------------------
// DS1685 extended RAM
// ---------------------------------------------------------------------------

fn rtc_w(r: &Rig, reg: u8, v: u8) {
    r.w8(reg::BYTEBUS_DEV1, reg);
    r.w8(reg::BYTEBUS_DEV2, v);
}

fn rtc_r(r: &Rig, reg: u8) -> u8 {
    r.w8(reg::BYTEBUS_DEV1, reg);
    r.r8(reg::BYTEBUS_DEV2)
}

/// The bank bit switches only 0x40..0x7f; the low registers and RAM are the
/// same in both banks.
#[test]
fn the_bank_bit_switches_only_the_upper_half() {
    let r = Rig::new();
    rtc_w(&r, 0x20, 0x11);
    rtc_w(&r, 0x60, 0x22); // bank 0 RAM
    rtc_w(&r, 0x0a, 0x10); // bank 1
    assert_eq!(rtc_r(&r, 0x20), 0x11, "low RAM is in both banks");
    assert_ne!(rtc_r(&r, 0x60), 0x22, "bank 1 has its own cell");
    rtc_w(&r, 0x60, 0x33);
    rtc_w(&r, 0x0a, 0x00);
    assert_eq!(rtc_r(&r, 0x60), 0x22, "bank 0 kept its value");
    rtc_w(&r, 0x0a, 0x10);
    assert_eq!(rtc_r(&r, 0x60), 0x33);
}

#[test]
fn the_extended_ram_is_reached_through_its_address_and_data_ports() {
    let r = Rig::new();
    rtc_w(&r, 0x0a, 0x10);
    for n in 0..128u8 {
        rtc_w(&r, 0x50, n);
        rtc_w(&r, 0x53, n ^ 0xa5);
    }
    for n in 0..128u8 {
        rtc_w(&r, 0x50, n);
        assert_eq!(rtc_r(&r, 0x53), n ^ 0xa5);
        assert_eq!(rtc_r(&r, 0x53), n ^ 0xa5, "the address does not advance");
    }
    // Seven-bit address: 0x80 wraps to cell 0.
    rtc_w(&r, 0x50, 0x80);
    assert_eq!(rtc_r(&r, 0x53), 0xa5);
    // In bank 0, register 0x53 is plain RAM, not the port.
    rtc_w(&r, 0x0a, 0x00);
    assert_eq!(rtc_r(&r, 0x53), 0);
}

#[test]
fn nvram_and_extended_ram_survive_in_a_512_byte_file() {
    let path = std::env::temp_dir().join(format!("ioc3-xram-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let r = Rig::new();
        r.c.set_nvram_file(&path).unwrap();
        rtc_w(&r, 0x0f, 0x5c);
        rtc_w(&r, 0x45, 0x66); // bank 0 RAM
        rtc_w(&r, 0x0a, 0x10);
        rtc_w(&r, 0x45, 0x77); // bank 1 register
        rtc_w(&r, 0x50, 0x09);
        rtc_w(&r, 0x53, b'1');
        rtc_w(&r, 0x50, 0x01);
        rtc_w(&r, 0x53, 0x02);
    }
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0x200);
    {
        let r = Rig::new();
        r.c.set_nvram_file(&path).unwrap();
        assert_eq!(rtc_r(&r, 0x0f), 0x5c);
        assert_eq!(rtc_r(&r, 0x45), 0x66);
        rtc_w(&r, 0x0a, 0x10);
        assert_eq!(rtc_r(&r, 0x45), 0x77);
        rtc_w(&r, 0x50, 0x09);
        assert_eq!(rtc_r(&r, 0x53), b'1');
        rtc_w(&r, 0x50, 0x01);
        assert_eq!(rtc_r(&r, 0x53), 0x02);
    }
    let _ = std::fs::remove_file(&path);
}

/// An old 114-byte dump still loads; the file becomes a 512-byte image.
#[test]
fn an_old_114_byte_file_still_loads() {
    let path = std::env::temp_dir().join(format!("ioc3-old-{}", std::process::id()));
    let mut old = vec![0u8; 114];
    old[0] = 0xab; // register 0x0e
    old[(0x45 - 0x0e) as usize] = 0xcd; // bank 0 RAM 0x45
    std::fs::write(&path, &old).unwrap();
    let r = Rig::new();
    r.c.set_nvram_file(&path).unwrap();
    assert_eq!(rtc_r(&r, 0x0e), 0xab);
    assert_eq!(rtc_r(&r, 0x45), 0xcd);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0x200);
    let _ = std::fs::remove_file(&path);
}

/// A full image in the QEMU layout loads, extended RAM included.
#[test]
fn a_qemu_layout_image_loads() {
    let path = std::env::temp_dir().join(format!("ioc3-qimg-{}", std::process::id()));
    let mut img = vec![0u8; 0x200];
    img[0x0f] = 0x42;
    img[0xc0 + 9] = b'1';
    img[0xc0 + 1] = 0x02;
    std::fs::write(&path, &img).unwrap();
    let r = Rig::new();
    r.c.set_nvram_file(&path).unwrap();
    assert_eq!(rtc_r(&r, 0x0f), 0x42);
    rtc_w(&r, 0x0a, 0x10);
    rtc_w(&r, 0x50, 9);
    assert_eq!(rtc_r(&r, 0x53), b'1');
    rtc_w(&r, 0x50, 1);
    assert_eq!(rtc_r(&r, 0x53), 0x02);
    let _ = std::fs::remove_file(&path);
}
