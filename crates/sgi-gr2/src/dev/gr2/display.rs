//! What the engine says of the picture's size and timing, from the VC1's programming and the board's
//! monitor code. Facts and their support are in docs/provenance.md.

use super::board::Gr2Probe;
use super::vc1::Vc1;

/// VC1 SRAM half-words the kernel and the PROMs fill in: the interlace flag (0, or 16 for an
/// interlaced file) and the width (a 1024-wide mode is stored as 1025).
const SRAM_INTERLACE: usize = 0x9f0;
const SRAM_WIDTH: usize = 0x9f2;
/// VC1 internal registers: the start and the end of the display-ID line tables, 2 bytes a line (so
/// the height), and the one the kernel sets to 8192 once it has loaded a timing file.
const REG_TABLE_START: usize = 0x40;
const REG_TABLE_END: usize = 0x42;
const REG_FILE_LOADED: usize = 0x14;

/// The clock `HeadTiming` is counted in: a frame is a whole number of its clocks at 60 and at 72 Hz.
/// It is not the pixel clock, which the survey does not know.
pub const TIMING_CLOCK_HZ: u32 = 3_600_000;

/// The monitor code the probe ports say (0 to 15): from port 1 bit 0 and port 2 bits 1:0 below board
/// revision 4, from port 0 bits 7:4 from it up.
pub fn monitor_code(p: &Gr2Probe) -> u32 {
    if !p.ports[0] & 0xf < 4 {
        (p.ports[1] & 1) | (p.ports[2] & 3) << 1
    } else {
        (p.ports[0] >> 4) & 0xf
    }
}

/// The picture's size and whether it is interlaced. From the VC1 once it is programmed with a
/// plausible one; else 1024 by 768 for monitor code 6 and 1280 by 1024 for the rest.
pub fn size(vc1: &Vc1, p: &Gr2Probe) -> (u32, u32) {
    programmed(vc1).map_or_else(|| if monitor_code(p) == 6 { (1024, 768) } else { (1280, 1024) }, |(w, h, _)| (w, h))
}

fn programmed(vc1: &Vc1) -> Option<(u32, u32, bool)> {
    let width = match vc1.sram16(SRAM_WIDTH) as u32 {
        1025 => 1024,
        w => w,
    };
    let lines = (vc1.reg16(REG_TABLE_END) as u32).checked_sub(vc1.reg16(REG_TABLE_START) as u32)? / 2;
    ((320..=1280).contains(&width) && (240..=1024).contains(&lines)).then(|| (width, lines, vc1.sram16(SRAM_INTERLACE) != 0))
}

/// The frame rate, when the board's own timing is loaded: the built-in 72 Hz set for monitor codes 1 and 9,
/// the 60 Hz set for the other codes but 6 and 14, which load nothing. None when no timing is programmed, a
/// timing file is loaded (its rate is in the file, which the engine does not decode) or it is interlaced.
pub fn refresh_hz(vc1: &Vc1, p: &Gr2Probe) -> Option<u32> {
    let (_, _, interlaced) = programmed(vc1)?;
    if interlaced || vc1.reg16(REG_FILE_LOADED) != 0 {
        return None;
    }
    match monitor_code(p) {
        1 | 9 => Some(72),
        6 | 14 => None,
        _ => Some(60),
    }
}
