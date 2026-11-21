//! The display side the engine keeps beside the raster side: the display
//! control bus (colour maps, DAC, XMAP, VC3) and the frame composed from it
//! and the pixel planes.
//!
//! It has a lock of its own so the shell's bus accesses never wait for the
//! raster worker; the worker never touches it.

use super::dcb::{Dcb, Txn, DEV_CMAP0, DEV_CMAP1, DEV_CMAP_ALL, DEV_DAC, DEV_VC3, DEV_XMAP, VC3_CONTROL, VC3_CONTROL_DISPLAY, VC3_CURRENT_LINE, VC3_CURSOR_X, VC3_CURSOR_X_LATCH, VC3_SRAM_POINTER};
use super::frame::{self, Frame};
use super::plain::boxed_zeroed;
use super::rss::Rss;
use super::engine::Board;
use crate::display::Rect;

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

/// The DAC's signature analyzer (ADV7160/7162 data sheet, Appendix 5 and command register 4): a 33-cell chain, S0 to S32,
/// clocked by the 30-bit pixel after the palette and gamma. Cell i takes `input[i] ^ (feedback & S[i-1])`, with cell 0's
/// previous cell S32 and its input S19 (taps at stages 33 and 20); the input is B0..B9 into S1..S10, G0..G9 into S11..S20,
/// R0..R9 into S21..S30 and 0 into S31, S32. With the feedback off the pixel is latched straight in (the data sheet's
/// words), which is what the gate in this form does; the figure does not show whether the gate after the NAND is an XOR or
/// its complement, so the form is *inferred*. Reset is taken as all zeros (inferred: "a known starting point").
#[derive(Default)]
struct Signature {
    state: u64,
    /// CR4 bit 5 and bit 6 are both high: clocking.
    clocking: bool,
    /// The frame (since the beam's origin) clocking began in.
    start_frame: u64,
    feedback: bool,
}

const SIGNATURE_CELLS: u32 = 33;
const SIGNATURE_MASK: u64 = (1 << SIGNATURE_CELLS) - 1;
/// CR4: bit 5 the signature clock, bit 6 the reset (low resets), bit 7 the feedback.
const CR4: u32 = 8;
const CR4_CLOCK: u32 = 0x20;
const CR4_RUN: u32 = 0x40;
const CR4_FEEDBACK: u32 = 0x80;
/// The most frames one clocking is run for, whatever the shell's clock says passed (a paused host would
/// otherwise run a million).
const SIGNATURE_MAX_FRAMES: u64 = 16;

impl Signature {
    /// One clock with the 30-bit `pixel` (`x2b10g10r10`).
    fn clock(&mut self, pixel: u32) {
        let (r, g, b) = ((pixel & 0x3ff) as u64, (pixel >> 10 & 0x3ff) as u64, (pixel >> 20 & 0x3ff) as u64);
        let input = (self.state >> 19 & 1) | b << 1 | g << 11 | r << 21;
        let previous = (self.state << 1 | self.state >> (SIGNATURE_CELLS - 1)) & SIGNATURE_MASK;
        self.state = (input ^ if self.feedback { previous } else { 0 }) & SIGNATURE_MASK;
    }

    /// Register 0x10 to 0x13 of the part: red S21..S30, green S11..S20, blue S1..S10, then S32, S31, S0 in bits 2:0.
    fn register(&self, n: u32) -> u32 {
        let field = |from: u32| (self.state >> from & 0x3ff) as u32;
        match n {
            0x10 => field(21),
            0x11 => field(11),
            0x12 => field(1),
            _ => ((self.state >> 32 & 1) << 2 | (self.state >> 31 & 1) << 1 | (self.state & 1)) as u32,
        }
    }
}

/// What a read of the line counter adds to the beam's clock between the shell's time updates. A guest that polls
/// the counter in a loop (the stock diagnostic's frame waits) reaches the next line, and so the next frame,
/// without the shell calling the pump. Approximation: a display-bus read's own duration is unknown.
const BEAM_READ_NS: u128 = 1_000;

/// What a store to the display bus changed of the picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Nothing the picture is made of: a select or address, a register the picture does not read, or a value
    /// stored over itself.
    None,
    /// The cursor alone: the box its glyph covered or covers, in display pixels, corners included.
    Cursor(Rect),
    /// A colour map entry, the DAC's gamma or pixel mask, an XMAP mode or pointer, or a VC3 table.
    All,
}

/// VC3 registers that only the cursor reads: the glyph's address, its position and its control.
const VC3_CURSOR_REGS: [usize; 4] = [1, 2, 3, 0x1D];
/// VC3 registers no picture reads: the SRAM pointer and the line counter.
const VC3_INERT_REGS: [usize; 2] = [VC3_SRAM_POINTER, VC3_CURRENT_LINE as usize];

/// The state a bus store can land on and the picture reads, as it was before the store.
enum Landing {
    /// Colour map entries of the maps a store reaches: map, entry, value.
    Cmap([Option<(usize, usize, u32)>; 2]),
    Gamma(usize, [u16; 3]),
    PixMask(u32),
    CursorBase(usize),
    BufSelect(u32),
    Dib(u32),
    MainModes([u32; 32]),
    OverlayModes([u32; 32]),
    /// The VC3's registers, the SRAM words at its pointer and the cursor's box.
    Vc3([u16; 32], usize, [u16; 2], Option<Rect>),
}

fn cursor_box(dcb: &Dcb) -> Option<Rect> {
    dcb.vc3.cursor().map(|(x, y, size, _)| Rect::new(x, y, x + size as i32 - 1, y + size as i32 - 1))
}

impl Landing {
    /// Where store `t` lands, if on state the picture reads.
    fn of(dcb: &Dcb, t: Txn) -> Option<Landing> {
        let modes = |f: &dyn Fn(u32) -> u32| std::array::from_fn(|did| f(did as u32));
        Some(match (t.dev, t.crs) {
            (DEV_CMAP_ALL | DEV_CMAP0 | DEV_CMAP1, 2) => {
                let at = |map: usize| Some((map, dcb.cmap[map].address(), dcb.cmap[map].pal[dcb.cmap[map].address()]));
                Landing::Cmap(match t.dev {
                    DEV_CMAP_ALL => [at(0), at(1)],
                    DEV_CMAP0 => [at(0), None],
                    _ => [at(1), None],
                })
            }
            (DEV_DAC, 1) => Landing::Gamma(dcb.dac.selected() as usize, dcb.dac.gamma[dcb.dac.selected() as usize]),
            (DEV_DAC, 2) => Landing::PixMask(dcb.dac.pixmask()),
            (DEV_XMAP, 2) => Landing::CursorBase(dcb.xmap.cursor_cmap_base()),
            (DEV_XMAP, 3) => Landing::BufSelect(dcb.xmap.buf_select()),
            (DEV_XMAP, 4) => Landing::MainModes(modes(&|did| dcb.xmap.main_mode(did))),
            (DEV_XMAP, 5) => Landing::OverlayModes(modes(&|did| dcb.xmap.overlay_mode(did))),
            (DEV_XMAP, 6) => Landing::Dib(dcb.xmap.dib_pointers()),
            (DEV_VC3, 0 | 1 | 3) => {
                let at = dcb.vc3.regs[VC3_SRAM_POINTER] as usize & 0x7FFF;
                Landing::Vc3(dcb.vc3.regs, at, [dcb.vc3.sram[at], dcb.vc3.sram[(at + 1) & 0x7FFF]], cursor_box(dcb))
            }
            _ => return None,
        })
    }

    /// What the store changed, with the state as it is now.
    fn change(self, dcb: &Dcb) -> Change {
        let same = match self {
            Landing::Cmap(e) => e.iter().flatten().all(|&(map, at, v)| dcb.cmap[map].pal[at] == v),
            Landing::Gamma(i, row) => dcb.dac.gamma[i] == row,
            Landing::PixMask(v) => dcb.dac.pixmask() == v,
            Landing::CursorBase(v) => dcb.xmap.cursor_cmap_base() == v,
            Landing::BufSelect(v) => dcb.xmap.buf_select() == v,
            Landing::Dib(v) => dcb.xmap.dib_pointers() == v,
            Landing::MainModes(m) => m.iter().enumerate().all(|(did, &v)| dcb.xmap.main_mode(did as u32) == v),
            Landing::OverlayModes(m) => m.iter().enumerate().all(|(did, &v)| dcb.xmap.overlay_mode(did as u32) == v),
            Landing::Vc3(regs, at, words, cursor) => {
                let moved = |i: &usize| dcb.vc3.regs[*i] != regs[*i];
                let elsewhere = (0..32).filter(|i| !VC3_CURSOR_REGS.contains(i) && !VC3_INERT_REGS.contains(i)).any(|i| moved(&i));
                if elsewhere || dcb.vc3.sram[at] != words[0] || dcb.vc3.sram[(at + 1) & 0x7FFF] != words[1] {
                    return Change::All;
                }
                return match (cursor, cursor_box(dcb)) {
                    _ if !VC3_CURSOR_REGS.iter().any(moved) => Change::None,
                    (Some(a), Some(b)) => Change::Cursor(a.union(&b)),
                    (Some(a), None) | (None, Some(a)) => Change::Cursor(a),
                    (None, None) => Change::None,
                };
            }
        };
        if same { Change::None } else { Change::All }
    }
}

pub struct Display {
    dcb: Box<Dcb>,
    beam: Beam,
    signature: Signature,
    /// A signature clocking ended and its frames are waiting for a pixel stream (`take_signature_due`).
    signature_due: Option<u64>,
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
        Display { dcb, beam: Beam::default(), signature: Signature::default(), signature_due: None, slave: None, frame: None, timing: None, timing_generation: 0 }
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

    /// Command register 4 was written: the signature analyzer's reset, clock and feedback.
    fn signature_control(&mut self, cr4: u32) {
        let (clock, run) = (cr4 & CR4_CLOCK != 0, cr4 & CR4_RUN != 0);
        self.signature.feedback = cr4 & CR4_FEEDBACK != 0;
        if !run {
            self.signature.state = 0;
        }
        let clocking = clock && run;
        let frame = self.beam(0).map(|b| b.1);
        if clocking && !self.signature.clocking {
            self.signature.start_frame = frame.unwrap_or(0);
        } else if !clocking && self.signature.clocking {
            // Every frame boundary since clocking began has run one whole frame's pixels through (the stream's place
            // within a frame is not modelled); with no timing loaded, one frame.
            let frames = frame.map_or(1, |f| f.saturating_sub(self.signature.start_frame)).min(SIGNATURE_MAX_FRAMES);
            self.signature_due = Some(frames).filter(|&f| f > 0);
        }
        self.signature.clocking = clocking;
    }

    /// Frames of pixels the analyzer is owed since its clock was switched off, once; the engine composes the frame
    /// and hands it back (`accumulate_signature`).
    pub fn take_signature_due(&mut self) -> Option<u64> {
        self.signature_due.take()
    }

    /// Run `frames` whole frames of `pixels` (`x2b10g10r10`, the active pixels) through the analyzer.
    pub fn accumulate_signature(&mut self, pixels: &[u32], frames: u64) {
        for _ in 0..frames {
            pixels.iter().for_each(|&p| self.signature.clock(p));
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
            let mut d = if t.dev == DEV_DAC && t.crs == 0 && t.width >= 2 { self.dcb.dac.address_halfword() } else { self.dcb.read(t) };
            if t.dev == DEV_DAC && t.crs == 2 && (0x10..=0x13).contains(&self.dcb.dac.selected()) {
                // The ten-bit read-only signature registers, in the palette's two-byte form: bits 9:2 in the first
                // byte and 1:0 in the second (the stock diagnostic's halfword read takes the top two bits from bits
                // 15:14). What a single byte read returns is not known; it gets bits 9:2.
                let v = self.signature.register(self.dcb.dac.selected());
                d = if t.width >= 2 { (v >> 2) << 8 | (v & 3) } else { v >> 2 };
            }
            return t.load_value(bits, d);
        }
        let dev = ((off - DCBCTRL) >> 2) as usize;
        if bits == 64 {
            (self.dcb.read_dcbctrl(dev) as u64) << 32 | self.dcb.read_dcbctrl(dev + 1) as u64
        } else {
            self.dcb.read_dcbctrl(dev) as u64
        }
    }

    /// A store to the bus window at `off`, and what it changed of the picture: colour maps, modes and the cursor
    /// change it without touching the planes, while an index write, or a value written over itself, changes nothing.
    pub fn write(&mut self, off: u32, bits: u32, val: u64) -> Change {
        if off <= DCB_BUS_END {
            let t = Txn::decode(off);
            if let (false, Some(slave)) = (Dcb::models(t.dev), &mut self.slave) {
                if slave.write(t.dev, t.crs, t.width, t.store_data(bits, val)) {
                    return Change::None;
                }
            }
            // A byte store at +1 of the DAC's address halfword sets the register number, its bits 15:8.
            if t.dev == DEV_DAC && t.crs == 0 && t.width >= 2 && bits == 8 {
                self.dcb.dac.set_register_number(val as u32);
                return Change::None;
            }
            let landing = Landing::of(&self.dcb, t);
            self.dcb.write(t, t.store_data(bits, val));
            if t.dev == DEV_DAC && t.crs == 2 && self.dcb.dac.selected() == CR4 {
                self.signature_control(self.dcb.dac.register(CR4));
            }
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
            return landing.map_or(Change::None, |l| l.change(&self.dcb));
        }
        let dev = ((off - DCBCTRL) >> 2) as usize;
        if bits == 64 {
            self.dcb.write_dcbctrl(dev, (val >> 32) as u32);
            self.dcb.write_dcbctrl(dev + 1, val as u32);
        } else {
            self.dcb.write_dcbctrl(dev, val as u32);
        }
        Change::None
    }

    /// A copy of the display bus in `to`, for a reader that composes without holding the display lock.
    pub fn copy_dcb(&self, to: &mut Dcb) {
        // SAFETY: `Dcb` is plain numeric state (`snapshot_bytes`), so copying its bytes copies it.
        unsafe { std::ptr::copy_nonoverlapping(&*self.dcb as *const Dcb, to as *mut Dcb, 1) };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The chain by its definition, cell by cell, apart from `Signature::clock`'s shifts.
    fn cells(prev: &[bool; 33], pixel: u32, feedback: bool) -> [bool; 33] {
        let bit = |v: u32, k: u32| v >> k & 1 == 1;
        let mut input = [false; 33];
        input[0] = prev[19];
        for k in 0..10 {
            input[1 + k as usize] = bit(pixel >> 20, k);
            input[11 + k as usize] = bit(pixel >> 10, k);
            input[21 + k as usize] = bit(pixel, k);
        }
        let mut next = [false; 33];
        for i in 0..33 {
            next[i] = input[i] ^ (feedback && prev[(i + 32) % 33]);
        }
        next
    }

    fn word(cells: &[bool; 33]) -> u64 {
        cells.iter().enumerate().fold(0, |w, (i, &c)| w | (c as u64) << i)
    }

    /// With the feedback off the pixel is latched straight in; with it on the previous state rotates up a cell and
    /// feeds back. The vector is worked by hand: pixel R 1, G 2, B 3 gives S1, S2 (blue 3), S12 (green 2) and S21 (red 1);
    /// a zero pixel after it with feedback on moves each up one cell to S2, S3, S13 and S22, and S0 takes S19, which is clear.
    #[test]
    fn the_chain_latches_the_pixel_with_feedback_off_and_rotates_the_state_up_with_it_on() {
        let mut s = Signature::default();
        s.clock(1 | 2 << 10 | 3 << 20);
        assert_eq!(s.state, 1 << 1 | 1 << 2 | 1 << 12 | 1 << 21);
        assert_eq!((s.register(0x10), s.register(0x11), s.register(0x12), s.register(0x13)), (1, 2, 3, 0));
        s.feedback = true;
        s.clock(0);
        assert_eq!(s.state, 1 << 2 | 1 << 3 | 1 << 13 | 1 << 22);
        assert_eq!((s.register(0x10), s.register(0x11), s.register(0x12)), (2, 4, 6), "S22; S13; S2 and S3");
        // The ends: S32 feeds S0, S19 feeds S0, and S31, S32 take only the feedback.
        s.state = 1 << 32;
        s.clock(0);
        assert_eq!(s.state, 1, "S32 into S0");
        s.state = 1 << 19;
        s.clock(0);
        assert_eq!(s.state, 1 << 20 | 1, "S19 into S0 and, rotating, into S20");
        s.state = 1 << 30;
        s.clock(0);
        assert_eq!((s.register(0x13), s.state >> 31 & 1), (0b010, 1), "S30 into S31, shown in the misc register");
    }

    #[test]
    fn the_shift_form_is_the_cell_by_cell_definition_over_a_pixel_stream() {
        let mut s = Signature { feedback: true, ..Signature::default() };
        let mut model = [false; 33];
        let mut x = 0x2545_f491u32;
        for _ in 0..5000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            s.clock(x & 0x3fff_ffff);
            model = cells(&model, x & 0x3fff_ffff, true);
            assert_eq!(s.state, word(&model));
        }
    }
}
