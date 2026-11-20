//! The display side the engine keeps beside the raster side: the display
//! control bus (colour maps, DAC, XMAP, VC3) and the frame composed from it
//! and the pixel planes.
//!
//! It has a lock of its own so the shell's bus accesses never wait for the
//! raster worker; the worker never touches it.

use super::dcb::{Dcb, Txn, DEV_DAC, DEV_VC3, VC3_CONTROL, VC3_CONTROL_DISPLAY};
use super::frame::{self, Frame};
use super::plain::boxed_zeroed;
use super::rss::Rss;
use super::engine::Board;

/// The bus's slot window, and the control registers after it.
pub const DCB_FIRST: u32 = 0x60000;
pub const DCB_LAST: u32 = 0x6803F;
const DCBCTRL: u32 = 0x68000;
const DCB_BUS_END: u32 = 0x67FFF;

/// The two board-version bytes of a board that does not say: RB revision,
/// RB and RA TRAM counts, then the product and GE count.
pub fn default_bdvers(board: Board) -> [u32; 2] {
    match board {
        Board::Solid => [0x70, 0x21],
        Board::High => [0x72, 0x01],
        Board::Max => [0x0A, 0x02],
    }
}

/// A device on the display control bus that the shell models, such as an option board: the bus asks it
/// first about every transaction on a slot the engine has no device for (the I2C slot and any other
/// the colour maps, DAC, XMAP, VC3 and the board-version slot do not take), and keeps its own scratch
/// register for a slot the slave leaves unclaimed. Called under the display lock, with the engine's
/// call rules: it must not call back into the engine.
pub trait BusSlave: Send {
    /// A load of register block `crs` of slot `dev`, `width` bytes: `Some(data)` when the slave answers.
    fn read(&mut self, dev: u32, crs: u32, width: u32) -> Option<u32>;
    /// A store of `data`: whether the slave took it.
    fn write(&mut self, dev: u32, crs: u32, width: u32, data: u32) -> bool;
}

/// The video timing the guest's tables and the DAC's PLL give: a frame of `frame_clocks` of `clock_hz`, in `lines`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub frame_clocks: u64,
    pub clock_hz: u32,
    pub lines: u32,
}

/// The PLL's reference oscillator. The other reference `0x072408` bit 24 selects has no known frequency.
const PLL_REFERENCE_HZ: u128 = 33_000_000;
/// A frame shorter than this has no retrace worth raising (1 ms).
const MIN_FRAME_NS: u128 = 1_000_000;

pub struct Display {
    dcb: Box<Dcb>,
    slave: Option<Box<dyn BusSlave>>,
    /// The decoded frame; allocated by the first composition on the CPU.
    frame: Option<Box<Frame>>,
    /// Derived when the guest enables the display; None until then or when the tables give no frame.
    timing: Option<Timing>,
    /// Moves whenever `timing` does, across resets.
    timing_generation: u64,
}

impl Display {
    pub fn new(bdvers: [u32; 2], cmap_rev: [u32; 2], pp1_revision: u32) -> Display {
        let mut dcb = boxed_zeroed::<Dcb>();
        dcb.init(bdvers, cmap_rev);
        dcb.xmap.set_pp1_revision(pp1_revision);
        Display { dcb, slave: None, frame: None, timing: None, timing_generation: 0 }
    }

    /// Back to power-on state: the shell's bus device stays, and the timing generation moves on.
    pub fn reset(&mut self, bdvers: [u32; 2], cmap_rev: [u32; 2], pp1_revision: u32) {
        let mut fresh = Display::new(bdvers, cmap_rev, pp1_revision);
        fresh.slave = self.slave.take();
        fresh.timing_generation = self.timing_generation + self.timing.is_some() as u64;
        *self = fresh;
    }

    /// The video timing from the VC3's frame table and the DAC's PLL, read when the display was last enabled;
    /// None until then, or when the tables give no usable frame (the shell keeps its default).
    pub fn timing(&self) -> Option<(Timing, u64)> {
        self.timing.map(|t| (t, self.timing_generation))
    }

    /// Take the timing the registers give now. A new period moves the generation; an unchanged one does not.
    fn derive_timing(&mut self) {
        let next = self.timing_from_tables();
        if next != self.timing {
            self.timing = next;
            self.timing_generation += 1;
        }
    }

    /// One frame is the VC3's table walk, in VC3 clocks at half the pixel clock; the PLL relation and the clock
    /// ratio are inferred (kept from the QEMU model they replace).
    fn timing_from_tables(&self) -> Option<Timing> {
        let (frame, lines) = self.dcb.vc3.frame_table()?;
        let (num, den) = self.dcb.dac.pll_ratio();
        // frame VC3 clocks at half the pixel clock: 2 * frame / f_pix seconds, in lowest terms.
        let (mut clocks, mut hz) = (2 * frame as u128 * den as u128, PLL_REFERENCE_HZ * num as u128);
        let (mut a, mut b) = (clocks, hz);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        (clocks, hz) = (clocks / a, hz / a);
        while clocks > u32::MAX as u128 || hz > u32::MAX as u128 {
            clocks = (clocks + 1) >> 1;
            hz = (hz + 1) >> 1;
        }
        (hz != 0 && clocks * 1_000_000_000 / hz >= MIN_FRAME_NS).then_some(Timing { frame_clocks: clocks as u64, clock_hz: hz as u32, lines })
    }

    pub fn dcb(&self) -> &Dcb {
        &self.dcb
    }

    /// The shell's device on the bus, or none; it survives a reset and a load.
    pub fn set_slave(&mut self, slave: Option<Box<dyn BusSlave>>) {
        self.slave = slave;
    }

    pub fn take_slave(&mut self) -> Option<Box<dyn BusSlave>> {
        self.slave.take()
    }

    /// A load from the bus window at `off` (`DCB_FIRST..=DCB_LAST`).
    pub fn read(&mut self, off: u32, bits: u32) -> u64 {
        if off <= DCB_BUS_END {
            let t = Txn::decode(off);
            if let (false, Some(slave)) = (Dcb::models(t.dev), &mut self.slave) {
                if let Some(d) = slave.read(t.dev, t.crs, t.width) {
                    return t.load_value(bits, d);
                }
            }
            // The address register reads back as the halfword the CPU wrote, register number in bits 15:8.
            let d = if t.dev == DEV_DAC && t.crs == 0 && t.width >= 2 { self.dcb.dac.address_halfword() } else { self.dcb.read(t) };
            return t.load_value(bits, d);
        }
        let dev = ((off - DCBCTRL) >> 2) as usize;
        if bits == 64 {
            (self.dcb.read_dcbctrl(dev) as u64) << 32 | self.dcb.read_dcbctrl(dev + 1) as u64
        } else {
            self.dcb.read_dcbctrl(dev) as u64
        }
    }

    /// A store to the bus window at `off`. True when what is displayed
    /// changed: colour maps, modes and the cursor change the picture without
    /// touching the planes, while an index write alone changes nothing.
    pub fn write(&mut self, off: u32, bits: u32, val: u64) -> bool {
        if off <= DCB_BUS_END {
            let t = Txn::decode(off);
            if let (false, Some(slave)) = (Dcb::models(t.dev), &mut self.slave) {
                if slave.write(t.dev, t.crs, t.width, t.store_data(bits, val)) {
                    return false;
                }
            }
            // A byte store at +1 of the DAC's address halfword sets the register number, its bits 15:8.
            if t.dev == DEV_DAC && t.crs == 0 && t.width >= 2 && bits == 8 {
                self.dcb.dac.set_register_number(val as u32);
                return false;
            }
            self.dcb.write(t, t.store_data(bits, val));
            if t.dev == DEV_VC3 && (t.crs == 1 || (t.crs == 0 && t.width >= 2)) {
                let enabled = self.dcb.vc3.index() == VC3_CONTROL && self.dcb.vc3.regs[VC3_CONTROL as usize] & VC3_CONTROL_DISPLAY != 0;
                if enabled {
                    self.derive_timing();
                }
            }
            return t.crs >= 2 || t.dev == DEV_VC3;
        }
        let dev = ((off - DCBCTRL) >> 2) as usize;
        if bits == 64 {
            self.dcb.write_dcbctrl(dev, (val >> 32) as u32);
            self.dcb.write_dcbctrl(dev + 1, val as u32);
        } else {
            self.dcb.write_dcbctrl(dev, val as u32);
        }
        false
    }

    /// The displayed size, from the VC3's tables.
    pub fn size(&self) -> (u32, u32) {
        let (w, h) = frame::display_size(&self.dcb);
        (w as u32, h as u32)
    }

    /// Compose the frame from the planes of `rss` on the CPU into `out`,
    /// `0x00RRGGBB`, rows from the top; the frame's size.
    pub fn compose(&mut self, rss: &Rss, out: &mut Vec<u32>) -> (u32, u32) {
        let f = self.frame.get_or_insert_with(boxed_zeroed::<Frame>);
        f.snapshot(rss, &self.dcb);
        let (w, h) = (f.width, f.height);
        out.clear();
        out.reserve(w * h);
        for y in 0..h {
            out.extend((0..w).map(|x| rgb(f.pixel(x, y))));
        }
        (w as u32, h as u32)
    }

    /// The state as bytes, for a saved engine.
    pub fn snapshot_bytes(&self) -> Vec<u8> {
        // SAFETY: `Dcb` is plain numeric state allocated zeroed (as `Hq3Regs`
        // is), so its padding is initialised.
        unsafe { std::slice::from_raw_parts((&*self.dcb as *const Dcb).cast::<u8>(), std::mem::size_of::<Dcb>()).to_vec() }
    }

    /// Replace the state with `bytes`, which `snapshot_bytes` wrote.
    pub fn apply_snapshot(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() != std::mem::size_of::<Dcb>() {
            return Err(format!("display bus state is {} bytes, expected {}", bytes.len(), std::mem::size_of::<Dcb>()));
        }
        // SAFETY: the length was checked and `Dcb` is plain numeric state.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), (&mut *self.dcb as *mut Dcb).cast::<u8>(), bytes.len()) };
        // The loaded tables are the guest's; its period starts afresh.
        self.timing = self.timing_from_tables();
        self.timing_generation += 1;
        Ok(())
    }
}

/// `0xFFBBGGRR`, what `frame.rs` composes, as `0x00RRGGBB`.
pub fn rgb(p: u32) -> u32 {
    (p & 0xFF) << 16 | p & 0xFF00 | (p >> 16) & 0xFF
}
