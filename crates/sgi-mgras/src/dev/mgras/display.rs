//! The display side the engine keeps beside the raster side: the display
//! control bus (colour maps, DAC, XMAP, VC3) and the frame composed from it
//! and the pixel planes.
//!
//! It has a lock of its own so the shell's bus accesses never wait for the
//! raster worker; the worker never touches it.

use super::dcb::{Dcb, Txn, DEV_DAC, DEV_VC3, VC3_CONTROL, VC3_CONTROL_DISPLAY, VC3_CURRENT_LINE, VC3_CURSOR_X, VC3_CURSOR_X_LATCH};
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

/// The video timing the guest's tables and the DAC's PLL give: a frame of `frame_clocks` of `clock_hz`, in `lines`
/// and `fields`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub frame_clocks: u64,
    pub clock_hz: u32,
    pub lines: u32,
    pub fields: u32,
}

/// The VC3's current line (index 0x0b) and current cursor X (index 0x0e) as the beam makes them: driven by the
/// shell's time and the timing the guest loaded (`Display::beam_line`). Not part of the saved display bus.
#[derive(Default)]
struct Beam {
    /// The shell's time at its last pump, and the beam's origin: the time the display was enabled with the
    /// timing now loaded, which is the start of line 0.
    now_ns: u64,
    origin_ns: u64,
    /// Line-counter reads since `now_ns` moved: each stands for `BEAM_READ_NS` of beam time.
    reads: u64,
    /// The cursor X register was written and has not yet latched, and the frame it was written in.
    x_pending: bool,
    x_frame: u64,
    x_latched: u16,
}

/// What a read of the line counter adds to the beam's clock between the shell's time updates. A guest that polls
/// the counter in a loop (the stock diagnostic's frame waits) reaches the next line, and so the next frame,
/// without the shell calling the pump. Approximation: a display-bus read's own duration is unknown.
const BEAM_READ_NS: u128 = 1_000;

pub struct Display {
    dcb: Box<Dcb>,
    beam: Beam,
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
        dcb.dac.set_identity(0x79, 1);
        Display { dcb, beam: Beam::default(), slave: None, frame: None, timing: None, timing_generation: 0 }
    }

    /// Back to power-on state: the shell's bus device stays, and the timing generation moves on.
    pub fn reset(&mut self, bdvers: [u32; 2], cmap_rev: [u32; 2], pp1_revision: u32) {
        let mut fresh = Display::new(bdvers, cmap_rev, pp1_revision);
        fresh.slave = self.slave.take();
        fresh.beam.now_ns = self.beam.now_ns;
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
            self.beam.origin_ns = self.beam.now_ns;
            self.beam.x_pending = false;
        }
    }

    /// The shell's time (`advance`, `wait_idle`): the beam moves with it.
    pub fn set_now(&mut self, now_ns: u64) {
        if now_ns != self.beam.now_ns {
            self.beam.now_ns = now_ns;
            self.beam.reads = 0;
        }
    }

    /// The beam's frame length in ns and its position in it: (frame ns, frames since the origin, ns into the
    /// frame); None while no timing is loaded. `extra_reads` counts the reads of the line counter at this call.
    fn beam(&self, extra_reads: u64) -> Option<(u128, u64, u128)> {
        let (t, _) = self.timing()?;
        let frame_ns = t.frame_clocks as u128 * 1_000_000_000 / t.clock_hz as u128;
        if frame_ns == 0 {
            return None;
        }
        let since = self.beam.now_ns.saturating_sub(self.beam.origin_ns) as u128 + (self.beam.reads + extra_reads) as u128 * BEAM_READ_NS;
        Some((frame_ns, (since / frame_ns) as u64, since % frame_ns))
    }

    /// A read of VC3 index 0x0b: the line the beam is on, 0 at the start of each frame to the frame's last line,
    /// wrapping every frame (inferred from the stock diagnostic's waits: it waits for 0, then for at least 200,
    /// then for 0 again, and the Digital Video driver's window around line 27 or 34). None while no timing is
    /// loaded: the counter then steps per read (`Vc3`).
    fn beam_line(&mut self) -> Option<u32> {
        let (frame_ns, _, into) = self.beam(1)?;
        self.beam.reads += 1;
        let lines = self.timing?.lines as u128;
        Some((into * lines / frame_ns) as u32)
    }

    /// A read of VC3 index 0x0e: the cursor X the beam has latched. The X written takes effect at the next frame
    /// boundary (the stock diagnostic writes X, waits two frames, and reads it back); with no timing loaded it
    /// latches at once.
    fn cursor_x_latched(&mut self) -> u16 {
        let frame = self.beam(0).map(|b| b.1);
        if self.beam.x_pending && frame.is_none_or(|f| f > self.beam.x_frame) {
            self.beam.x_latched = self.dcb.vc3.regs[2];
            self.beam.x_pending = false;
        }
        self.beam.x_latched
    }

    /// The tables' and the PLL's timing (`Dcb::video_timing`), in pixel clocks.
    fn timing_from_tables(&self) -> Option<Timing> {
        let t = self.dcb.video_timing()?;
        Some(Timing { frame_clocks: t.h_total as u64 * t.v_total as u64, clock_hz: u32::try_from(t.pixel_clock_hz).ok()?, lines: t.v_total, fields: t.fields })
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
            if t.dev == DEV_VC3 && t.crs == 1 && true {
                let line = (self.dcb.vc3.index() == VC3_CURRENT_LINE && !self.dcb.vc3.line_held()).then(|| self.beam_line()).flatten();
                if let Some(l) = line {
                    return t.load_value(bits, l);
                }
                if self.dcb.vc3.index() == VC3_CURSOR_X_LATCH {
                    let x = self.cursor_x_latched();
                    return t.load_value(bits, x as u32);
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
                if self.dcb.vc3.index() == VC3_CURSOR_X {
                    self.beam.x_pending = true;
                    self.beam.x_frame = self.beam(0).map_or(0, |b| b.1);
                }
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

    /// As `compose` with the DAC gamma's ten bits a channel, `x2b10g10r10` (red low): the top 8 bits of each are
    /// `compose`'s.
    pub fn compose10(&mut self, rss: &Rss, out: &mut Vec<u32>) -> (u32, u32) {
        let f = self.frame.get_or_insert_with(boxed_zeroed::<Frame>);
        f.snapshot(rss, &self.dcb);
        let (w, h) = (f.width, f.height);
        out.clear();
        out.reserve(w * h);
        for y in 0..h {
            out.extend((0..w).map(|x| f.pixel10(x, y)));
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
