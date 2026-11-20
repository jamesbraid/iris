//! The display timing the board scan-outs share: the buffer geometry and the
//! visible size a VC2/VC3 timing program describes. Kept apart from `disp`
//! so a crate that has no windowing or NG1 can mount it.

use crate::dev::ng1::vc2::{VC2_REG_VIDEO_ENTRY_PTR, VT_CBLANK_XMAP_N, VT_HPOS_VC_N};

/// Row stride of every display buffer, in pixels.
pub const FB_STRIDE: usize = 2048;
/// Rows of the display buffers and output textures: the tallest frame a
/// board can hand over (Newport and GR2 use the first 1024; IMPACT's
/// 1600x1200 modes need more).
pub const FB_MAX_H: usize = 2048;


/// Decode visible display size from VC2 state (used by compositor and timing presets).
pub fn decode_vc2_timings(vc2_regs: &[u16; 32], vc2_ram: &[u16]) -> (usize, usize, i32) {
    let (w, h, adjust, _) = decode_vc2_timings_gap(vc2_regs, vc2_ram);
    (w, h, adjust)
}

/// A cursor placement the video table does not give: the distance from the
/// HPOS edge to the first visible pixel was missing (`None`) or outside what
/// the cursor adjustment covers, so the default adjustment is used.
pub type CursorGapFault = Option<usize>;

/// [`decode_vc2_timings`], also saying when the cursor adjustment is the
/// fallback and why. It matters only while the cursor is shown.
pub fn decode_vc2_timings_gap(vc2_regs: &[u16; 32], vc2_ram: &[u16]) -> (usize, usize, i32, Option<CursorGapFault>) {
    let frame_ptr = vc2_regs[VC2_REG_VIDEO_ENTRY_PTR as usize] as usize;
    let ram = vc2_ram;

    let mut max_visible_width   = 0usize;
    let mut total_visible_lines = 0usize;
    let mut hpos_to_visible: Option<usize> = None;
    let mut curr_frame_ptr = frame_ptr;
    let mut loop_safety    = 0usize;

    loop {
        if curr_frame_ptr + 1 >= ram.len() { break; }
        let line_seq_ptr    = ram[curr_frame_ptr] as usize;
        let mut line_seq_len = ram[curr_frame_ptr + 1] as usize;
        if line_seq_len == 0 { break; }
        let mut curr_line_ptr = line_seq_ptr;

        while line_seq_len > 0 {
            let mut line_visible_width = 0usize;
            let mut eol  = false;
            let mut line_loop_safety = 0usize;
            let mut state_c = 0u8;
            let mut pixel_offset = 0usize;
            let mut hpos_pixel: Option<usize> = None;
            let mut visible_pixel: Option<usize> = None;
            let mut hpos_seen_deasserted = false;

            while !eol {
                if curr_line_ptr >= ram.len() { break; }
                let w1 = ram[curr_line_ptr]; curr_line_ptr += 1;
                let duration      = ((w1 >> 8) & 0x7F) as usize;
                let state_a       = (w1 & 0x7F) as u8;
                let sb_sc_absent  = (w1 & 0x0080) != 0;
                let mut eol_bit   = (w1 & 0x8000) != 0;

                if !sb_sc_absent {
                    if curr_line_ptr >= ram.len() { break; }
                    let w2 = ram[curr_line_ptr];
                    if (w2 & 0x8000) != 0 { eol_bit = true; }
                    curr_line_ptr += 1;
                    state_c = (w2 & 0x7F) as u8;
                }

                eol = eol_bit;
                let pixels = duration * 2;

                if hpos_pixel.is_none() {
                    if (state_a & VT_HPOS_VC_N) != 0 {
                        hpos_seen_deasserted = true;
                    } else if hpos_seen_deasserted {
                        hpos_pixel = Some(pixel_offset);
                    }
                }

                let visible = (state_c & VT_CBLANK_XMAP_N) != 0;
                   // && (state_a & VT_VIS_LN_VC_N)    == 0
                   // && (state_a & VT_DSPLY_EN_RO_N)   == 0;
                   // including these allow us for more precise 1024/1280 visible pixels
                   // but we need two more to account for IRIX keeping 2 pixels on the left blank

                if visible {
                    if visible_pixel.is_none() { visible_pixel = Some(pixel_offset); }
                    line_visible_width += pixels;
                }
                pixel_offset += pixels;
                line_loop_safety += 1;
                if line_loop_safety > 1000 { break; }
            }

            if line_visible_width > 0 {
                total_visible_lines += 1;
                if line_visible_width > max_visible_width {
                    max_visible_width = line_visible_width;
                }
                if hpos_to_visible.is_none() {
                    if let (Some(h), Some(v)) = (hpos_pixel, visible_pixel) {
                        if v >= h { hpos_to_visible = Some(v - h); }
                    }
                }
            }

            line_seq_len -= 1;
            if curr_line_ptr >= ram.len() { break; }
            curr_line_ptr = ram[curr_line_ptr] as usize;
        }

        curr_frame_ptr += 2;
        loop_safety    += 1;
        if loop_safety > 1000 { break; }
    }

    if max_visible_width > 0 && total_visible_lines > 0 {
        let w = max_visible_width.min(2048);
        let h = total_visible_lines.min(1024);
        let (cursor_x_adjust, fault) = match hpos_to_visible {
            // The NG2 table (a Newport in an expansion slot) has a 30-pixel
            // gap, so the adjustment goes slightly negative.
            Some(d) if (-16..=64).contains(&(d as i32 - 31)) => (d as i32 - 31, None),
            other => (11, Some(other)),
        };
        (w, h, cursor_x_adjust - 2, fault)
    } else {
        (0, 0, 0, None)
    }
}

/// What a VC2 or VC3 timing generator's tables add up to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vc2Totals {
    /// Pixel clocks a line, blanking included.
    pub h_total: u32,
    /// Lines a frame, blanking included, over all of its fields.
    pub v_total: u32,
    /// Lines with any pixel out of blanking.
    pub visible_lines: u32,
    /// Runs of visible lines: 1, or 2 for an interlaced, a two-field or a
    /// field-sequential stereo format, each with its own vertical blank.
    pub fields: u32,
}

/// Total a VC2's video timing tables (the VC3's are laid out the same): the
/// frame table at the video entry pointer lists (line table, repeat count)
/// pairs up to a zero count; a line is runs of states, each lasting its
/// duration field in units of two pixel clocks, and ends with the address
/// of the line that follows. `visible` says whether a run's states (the
/// first word's seven A bits, and the C bits of the last second word) put
/// its line in the picture: the VC2's composite blank is a C bit, the VC3
/// marks its visible lines in an A bit. None while no table is loaded.
pub fn decode_vc2_totals(vc2_regs: &[u16; 32], ram: &[u16], visible_run: impl Fn(u8, u8) -> bool) -> Option<Vc2Totals> {
    let mut frame = vc2_regs[VC2_REG_VIDEO_ENTRY_PTR as usize] as usize;
    let (mut h_total, mut v_total, mut visible_lines, mut fields) = (0u32, 0u32, 0u32, 0u32);
    let mut in_picture = false;
    // The tables are the guest's: every walk is bounded.
    for _ in 0..1024 {
        let (mut line, mut repeat) = (*ram.get(frame)? as usize, *ram.get(frame + 1)? as usize);
        if repeat == 0 {
            break;
        }
        while repeat > 0 {
            let (mut clocks, mut visible, mut state_c) = (0u32, false, 0u8);
            for _ in 0..1024 {
                let w1 = *ram.get(line)?;
                line += 1;
                let mut eol = w1 & 0x8000 != 0;
                if w1 & 0x0080 == 0 {
                    let w2 = *ram.get(line)?;
                    line += 1;
                    eol |= w2 & 0x8000 != 0;
                    state_c = (w2 & 0x7F) as u8;
                }
                visible |= visible_run((w1 & 0x7F) as u8, state_c);
                clocks += 2 * ((w1 >> 8) & 0x7F) as u32;
                if eol {
                    break;
                }
            }
            h_total = h_total.max(clocks);
            v_total += 1;
            if visible {
                visible_lines += 1;
                fields += !in_picture as u32;
            }
            in_picture = visible;
            if v_total > 8192 {
                return None;
            }
            repeat -= 1;
            line = *ram.get(line)? as usize;
        }
        frame += 2;
    }
    (h_total != 0 && v_total != 0 && visible_lines != 0).then_some(Vc2Totals { h_total, v_total, visible_lines, fields })
}
