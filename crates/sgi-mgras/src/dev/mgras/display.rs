//! The display side the engine keeps beside the raster side: the display
//! control bus (colour maps, DAC, XMAP, VC3) and the frame composed from it
//! and the pixel planes.
//!
//! It has a lock of its own so the shell's bus accesses never wait for the
//! raster worker; the worker never touches it.

use super::dcb::{Dcb, Txn, DEV_VC3};
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

pub struct Display {
    dcb: Box<Dcb>,
    slave: Option<Box<dyn BusSlave>>,
    /// The decoded frame; allocated by the first composition on the CPU.
    frame: Option<Box<Frame>>,
}

impl Display {
    pub fn new(bdvers: [u32; 2], cmap_rev: [u32; 2]) -> Display {
        let mut dcb = boxed_zeroed::<Dcb>();
        dcb.init(bdvers, cmap_rev);
        Display { dcb, slave: None, frame: None }
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
            let d = self.dcb.read(t);
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
            self.dcb.write(t, t.store_data(bits, val));
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
        Ok(())
    }
}

/// `0xFFBBGGRR`, what `frame.rs` composes, as `0x00RRGGBB`.
pub fn rgb(p: u32) -> u32 {
    (p & 0xFF) << 16 | p & 0xFF00 | (p >> 16) & 0xFF
}
