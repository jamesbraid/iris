//! The display option board: what the display generator's firmware does on the display bus for it, so a
//! shell that models the board as a device of its own sees the accesses the firmware makes.
//!
//! The firmware is not run; the display requests it answers are. For an option mask of 0x10 (a GVO, the
//! board whose type 6 is in the display identity word, bits 21:19) the requests reach the shell's device
//! through [`DisplayBusSlave`] as the bus accesses the firmware makes (kona 9d9f07de,
//! `devices/dg/gvo-option.md`, established from `arm1.u` and `irsaudit` unless said):
//!
//! - Requests 17 (rebuild the default display state) and 83 (window-system video initialise) probe the video
//!   output chips: each of the VOCs 0xc6 and 0xc7 is present when id + 0x3e8 written to its register 0x31
//!   (bus 0xc831 and 0xc931) reads back the same. Request 83 also pulses 0xc000 (soft reset), 1 then 0.
//!   The channel exists only when both are present.
//! - Request 82 loads the board's logic, whether or not the probe has run (the X server sends it first): type 0, count -1 writes 1 to 0x8900 (clear) and otherwise up to 16
//!   bytes, packed four to a word in words 2 to 5 (first byte in the top byte: inferred), each written to
//!   0x8a02 and acknowledged by bit 7 of that word within 999 reads. The reply (27) is 1 on success, 0 on
//!   a refusal or a timeout.
//! - Request 88 (data format: VOC id, format, flag) writes the option mode word 0x8400 (bits 12:11 = 1 for a
//!   GVO), the control register 0xc006 (0x484, the firmware's default after loading a GVO format: frame-buffer
//!   mode, external clock, true 10-bit output) and the converter's control registers 0xc214 to 0xc216 for
//!   the format. The converter matrix (0xc200 to 0xc213, its values from two tables the firmware holds) and the
//!   genlock chip's image are not written: their values are not in the sources read.
//! - Blanking the board's channel (request 31) writes 1024 zero words to its colour look-up table at 0xf000, so the
//!   board outputs black (established). The blank field the firmware also sets in 0xc104 is not modelled: its value
//!   is not in the sources read.
//! - Requests 1 and 2 write and read a display-bus word, so the diagnostic's register tests reach the device.
//!
//! The board's logic family and what the download implements are unknown, so the model keeps no more of it
//! than what the firmware polls.

use super::channels::GVO_ID;

/// The shell's device on the display bus for the option board: a read of display-bus word `addr` answers
/// `Some(value)` when the board has the register; a write returns whether the board took it. Called from the
/// engine's calls, on the thread making them, and must not call the engine.
pub trait DisplayBusSlave: Send {
    fn read(&mut self, addr: u32) -> Option<u32>;
    fn write(&mut self, addr: u32, data: u32) -> bool;
}

/// A 10-bit output component with its two low bits formed as `0xc006` bits 10-9 (`mode`) say (established, kona
/// `devices/dg/gvo-option.md`): 0 zero, 2 the gamma output's own bits, 3 a duplicate of the top bits (bits 9:8). Mode 1 is
/// unnamed in the sources: shown as 2.
pub fn low_bits(c: u32, mode: u32) -> u32 {
    match mode {
        0 => c & !3,
        3 => c & !3 | c >> 8 & 3,
        _ => c,
    }
}

/// The option mask a GVO reports (request 18).
pub const MASK_GVO: u32 = 0x10;

/// The option board's logic load control and data, and its mode word.
const LOAD_CONTROL: u32 = 0x8900;
const LOAD_DATA: u32 = 0x8a02;
const MODE_WORD: u32 = 0x8400;
const MODE_GVO: u32 = 1 << 11;
const SOFT_RESET: u32 = 0xc000;
const CONTROL: u32 = 0xc006;
const CONTROL_DEFAULT: u32 = 0x484;
const CONVERTER_CONTROL: u32 = 0xc214;
const LUT: u32 = 0xf000;
const LUT_WORDS: u32 = 1024;

/// Reads of the load data's bit 7 before the firmware gives a byte up.
const LOAD_POLLS: u32 = 999;
/// The most bytes one request carries.
const LOAD_BYTES: u32 = 16;

/// The VOC ids the probe walks, and the bus address of one's register 0x31.
const VOCS: [u32; 2] = [GVO_ID, GVO_ID + 1];
fn probe_address(id: u32) -> u32 {
    0x200 + 0x100 * id + 0x31
}

pub struct OptionBoard {
    mask: u32,
    slave: Option<Box<dyn DisplayBusSlave>>,
    /// The last probe found the board's VOCs.
    present: bool,
}

impl OptionBoard {
    /// A board that reports option mask `mask` (0 for none).
    pub fn new(mask: u32) -> OptionBoard {
        OptionBoard { mask, slave: None, present: false }
    }

    pub fn mask(&self) -> u32 {
        self.mask
    }

    /// Whether the channel exists: the firmware found the VOCs.
    pub fn present(&self) -> bool {
        self.present
    }

    /// The shell's device, or none; it stays through a reset and a load.
    pub fn set_slave(&mut self, slave: Option<Box<dyn DisplayBusSlave>>) {
        self.slave = slave;
    }

    /// Power-on: the firmware has not probed yet.
    pub fn reset(&mut self) {
        self.present = false;
    }

    /// Set from a saved state; the device's own state travels with the shell's.
    pub fn restore(&mut self, present: bool) {
        self.present = present;
    }

    fn probe(&mut self) {
        let Some(slave) = self.slave.as_mut().filter(|_| self.mask == MASK_GVO) else {
            self.present = false;
            return;
        };
        self.present = VOCS.iter().all(|&id| {
            let (at, value) = (probe_address(id), id + 0x3e8);
            slave.write(at, value) && slave.read(at) == Some(value)
        });
    }

    /// The display request `command` with its data words: the bus accesses the firmware makes, and the
    /// first data word of its reply when it has one that comes from the board (the word read, the logic
    /// load's status).
    pub fn request(&mut self, command: u32, data: &[u32]) -> Option<u32> {
        let d = |i: usize| data.get(i).copied().unwrap_or(0);
        match command {
            0x01 => {
                if let Some(s) = self.slave.as_mut() {
                    s.write(d(0), d(1));
                }
                None
            }
            0x02 => Some(self.slave.as_mut().and_then(|s| s.read(d(0))).unwrap_or(0)),
            0x11 => {
                self.probe();
                None
            }
            0x53 => {
                self.probe();
                if self.present {
                    if let Some(s) = self.slave.as_mut() {
                        s.write(SOFT_RESET, 1);
                        s.write(SOFT_RESET, 0);
                    }
                }
                None
            }
            0x52 => Some(self.load_logic(data) as u32),
            0x58 => {
                self.data_format(d(0), d(1));
                None
            }
            _ => None,
        }
    }

    /// The control register's value as the board holds it (0xc006: bits 10-9 say how the two low bits of the
    /// 10-bit output are formed), read from the shell's device; None without one or when it has no such register.
    pub fn control(&mut self) -> Option<u32> {
        self.slave.as_mut().filter(|_| self.present).and_then(|s| s.read(CONTROL))
    }

    /// The board's channel is blanked: the firmware zeroes the colour look-up table.
    pub fn blank_channel(&mut self) {
        if let (Some(slave), true) = (self.slave.as_mut(), self.present) {
            (0..LUT_WORDS).for_each(|i| {
                slave.write(LUT + i, 0);
            });
        }
    }

    /// Request 82: type, count (-1 clears), the bytes. True when the board took it.
    fn load_logic(&mut self, data: &[u32]) -> bool {
        let d = |i: usize| data.get(i).copied().unwrap_or(0);
        // The X server loads the logic before it probes the board (the screen set-up loads it first, the firmware's
        // load routine checks nothing of the probe), so the board only has to be there to answer.
        let (Some(slave), true) = (self.slave.as_mut(), d(0) == 0) else { return false };
        let count = d(1);
        if count == u32::MAX {
            return slave.write(LOAD_CONTROL, 1);
        }
        if count > LOAD_BYTES {
            return false;
        }
        for i in 0..count as usize {
            let byte = d(2 + i / 4) >> (24 - 8 * (i % 4)) & 0xff;
            if !slave.write(LOAD_DATA, byte) || !(0..LOAD_POLLS).any(|_| slave.read(LOAD_DATA).is_some_and(|v| v & 0x80 != 0)) {
                return false;
            }
        }
        true
    }

    /// Request 88: for the GVO's VOC, its mode word, control register and converter control.
    fn data_format(&mut self, voc: u32, format: u32) {
        let Some(slave) = self.slave.as_mut().filter(|_| self.present && voc == GVO_ID) else { return };
        let format = if format > 9 { 3 } else { format };
        slave.write(MODE_WORD, MODE_GVO);
        slave.write(CONTROL, CONTROL_DEFAULT);
        slave.write(CONVERTER_CONTROL, if matches!(format, 2 | 3) { 0x31 } else { 0x10 });
        slave.write(CONVERTER_CONTROL + 1, if format == 3 { 0xa7 } else { 0xa3 });
        slave.write(CONVERTER_CONTROL + 2, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    /// A board that stores every register and answers bit 7 of the load data at once.
    #[derive(Clone, Default)]
    pub(crate) struct Fake {
        pub regs: Arc<Mutex<BTreeMap<u32, u32>>>,
        pub log: Arc<Mutex<Vec<(u32, u32)>>>,
        pub refuse: bool,
    }

    impl DisplayBusSlave for Fake {
        fn read(&mut self, addr: u32) -> Option<u32> {
            if self.refuse {
                return None;
            }
            let v = self.regs.lock().unwrap().get(&addr).copied().unwrap_or(0);
            Some(if addr == LOAD_DATA { v | 0x80 } else { v })
        }

        fn write(&mut self, addr: u32, data: u32) -> bool {
            self.log.lock().unwrap().push((addr, data));
            self.regs.lock().unwrap().insert(addr, data);
            !self.refuse
        }
    }

    fn board(fake: &Fake) -> OptionBoard {
        let mut b = OptionBoard::new(MASK_GVO);
        b.set_slave(Some(Box::new(fake.clone())));
        b
    }

    #[test]
    fn the_two_low_bits_of_the_output_follow_control_register_bits_10_9() {
        let c = 0x2a5;
        assert_eq!([0, 1, 2, 3].map(|m| low_bits(c, m)), [0x2a4, 0x2a5, 0x2a5, 0x2a6]);
        assert_eq!(low_bits(0x3ff, 0), 0x3fc);
        assert_eq!(low_bits(0x001, 3), 0x000, "the top bits of 1 are zero");
    }

    #[test]
    fn the_probe_writes_each_vocs_id_plus_0x3e8_to_its_register_0x31_and_reads_it_back() {
        let fake = Fake::default();
        let mut b = board(&fake);
        assert!(!b.present());
        b.request(0x11, &[0]);
        assert!(b.present());
        assert_eq!(*fake.log.lock().unwrap(), vec![(0xc831, 0xc6 + 0x3e8), (0xc931, 0xc7 + 0x3e8)]);
        b.request(0x53, &[0]);
        assert_eq!(fake.log.lock().unwrap()[2..], [(0xc831, 0x4ae), (0xc931, 0x4af), (0xc000, 1), (0xc000, 0)], "request 83 probes, then pulses the soft reset");
        // A board that stores nothing, or none, is absent.
        let mut gone = board(&Fake { refuse: true, ..Fake::default() });
        gone.request(0x11, &[0]);
        assert!(!gone.present());
        let mut none = OptionBoard::new(MASK_GVO);
        none.request(0x11, &[0]);
        assert!(!none.present());
        let mut plain = OptionBoard::new(0);
        plain.set_slave(Some(Box::new(fake.clone())));
        plain.request(0x11, &[0]);
        assert!(!plain.present(), "a board without the option never probes");
    }

    #[test]
    fn the_logic_load_clears_then_sends_each_byte_to_0x8a02_and_polls_its_bit_7() {
        let fake = Fake::default();
        let mut b = board(&fake);
        assert_eq!(b.request(0x52, &[0, u32::MAX, 0, 0, 0, 0]), Some(1), "before the probe: the X server loads the logic first");
        assert!(!b.present());
        b.request(0x11, &[0]);
        fake.log.lock().unwrap().clear();
        assert_eq!(b.request(0x52, &[0, u32::MAX, 0, 0, 0, 0]), Some(1));
        assert_eq!(*fake.log.lock().unwrap(), vec![(0x8900, 1)]);
        fake.log.lock().unwrap().clear();
        assert_eq!(b.request(0x52, &[0, 5, 0x0102_0304, 0x0500_0000, 0, 0]), Some(1));
        assert_eq!(*fake.log.lock().unwrap(), (1..=5).map(|v| (0x8a02, v)).collect::<Vec<_>>());
        assert_eq!(b.request(0x52, &[0, 17, 0, 0, 0, 0]), Some(0), "more than 16 bytes is refused");
        assert_eq!(b.request(0x52, &[1, 1, 0, 0, 0, 0]), Some(0), "only type 0");
        let mut absent = OptionBoard::new(MASK_GVO);
        assert_eq!(absent.request(0x52, &[0, u32::MAX, 0, 0, 0, 0]), Some(0), "no board, no load");
    }

    #[test]
    fn the_data_format_sets_the_mode_word_the_control_register_and_the_converter_control() {
        let fake = Fake::default();
        let mut b = board(&fake);
        b.request(0x11, &[0]);
        for (format, c214, c215) in [(0, 0x10, 0xa3), (2, 0x31, 0xa3), (3, 0x31, 0xa7), (99, 0x31, 0xa7)] {
            b.request(0x58, &[0xc6, format, 0]);
            let regs = fake.regs.lock().unwrap();
            assert_eq!((regs[&0x8400], regs[&0xc006], regs[&0xc214], regs[&0xc215], regs[&0xc216]), (0x800, 0x484, c214, c215, 0), "format {format}");
        }
        fake.log.lock().unwrap().clear();
        b.request(0x58, &[0, 3, 0]);
        assert!(fake.log.lock().unwrap().is_empty(), "another VOC's format is not the option board's");
    }

    #[test]
    fn requests_1_and_2_reach_the_device_and_a_missing_device_reads_zero() {
        let fake = Fake::default();
        let mut b = board(&fake);
        b.request(0x01, &[0xc104, 0x5a]);
        assert_eq!(b.request(0x02, &[0xc104]), Some(0x5a));
        assert_eq!(b.request(0x02, &[0xc105]), Some(0));
        assert_eq!(OptionBoard::new(0).request(0x02, &[0xc104]), Some(0));
    }
}
