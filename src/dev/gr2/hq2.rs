//! HQ2 command engine: CPU-visible registers, microcode RAM, and the HLE
//! command interpreter that runs on the HQ2 thread.
//!
//! The HQ2 and GE7 microcode is stored and read back but never executed.
//! Each FIFO word index is the HQ2 microcode entry address, so the
//! interpreter dispatches on it directly and turns each command into RE3
//! register writes. Only the textport commands are implemented so far.

use std::sync::atomic::{AtomicU32, Ordering};

use super::ge7::{Ge7, LOADUCODE_MASK};
use super::re3;

#[path = "gl.rs"]
mod gl;

// Register byte offsets from the HQ2 base (0x6a000).
pub const HQ_ATTRJMP: u32 = 0x00; // 16 words
pub const HQ_VERSION: u32 = 0x40;
pub const HQ_NUMGE: u32 = 0x44;
pub const HQ_FIN1: u32 = 0x48;
pub const HQ_FIN2: u32 = 0x4c;
pub const HQ_DMASYNC: u32 = 0x50;
pub const HQ_FIFO_FULL_TIMEOUT: u32 = 0x54;
pub const HQ_FIFO_EMPTY_TIMEOUT: u32 = 0x58;
pub const HQ_FIFO_FULL: u32 = 0x5c;
pub const HQ_FIFO_EMPTY: u32 = 0x60;
pub const HQ_GE7LOADUCODE: u32 = 0x64;
pub const HQ_GEDMA: u32 = 0x68;
pub const HQ_HQ_GEPC: u32 = 0x6c;
pub const HQ_GEPC: u32 = 0x70;
pub const HQ_INTR: u32 = 0x74;
pub const HQ_UNSTALL: u32 = 0x78;
pub const HQ_MYSTERY: u32 = 0x7c;
pub const HQ_REFRESH: u32 = 0x80;
pub const HQ_FIN3: u32 = 0x100;

pub const HQ2_MYSTERY: u32 = 0xdead_beef;
/// The HQ2 revision iris's boards report in version\[31:23\] and hq_gepc\[31:16\].
pub const HQ2_REV: u32 = 2;
pub const HQ_UCODE_WORDS: usize = 8192;

/// FIFO low-watermark flag in `version`.
const VERSION_LOWATER: u32 = 1 << 13;

/// Finish flags in `version`: FIN1 = bit 2, FIN2 = bit 1, FIN3 = bit 0.
pub const FIN1: usize = 0;
pub const FIN2: usize = 1;
pub const FIN3: usize = 2;

// Textport command indices (FIFO word index = HQ2 microcode entry point).
pub const PUC_INIT: u32 = 401;
pub const PUC_COLOR: u32 = 402;
pub const PUC_FINISH: u32 = 403;
pub const PUC_PNT2I: u32 = 404;
pub const PUC_RECTI2D: u32 = 405;
pub const PUC_CMOV2I: u32 = 406;
pub const PUC_LINE2I: u32 = 407;
pub const PUC_DRAWCHAR: u32 = 408;
pub const PUC_RECTCOPY: u32 = 409;
pub const PUC_DATA: u32 = 479;

/// Pseudo FIFO index for words the CPU or VDMA writes to `HQ2_GEDMA`
/// (0x6a068). Real FIFO indices are 0..0x7fff; this marks the DMA data stream.
pub const HQ_TOKEN_GEDMA: u32 = 0x1_0000;
/// Pseudo FIFO index queued when the CPU writes `unstall`: the microcode
/// restarts, and its reset path takes one DATA word (the start argument).
pub const HQ_TOKEN_UNSTALL: u32 = 0x1_0001;
/// Context switch (GE_CTX_SWITCH, 0x1f0): context id, then 2 DATA words.
pub const GE_CTX_SWITCH: u32 = 496;
/// Context save / restore through HQ2_GEDMA (the kernel's context save and restore).
pub const GE_DUMP_CTX: u32 = 0x1e1;
pub const GE_LOAD_CTX: u32 = 0x1e2;
pub const GE_DUMP_CTX_EXTRA: u32 = 0x1e3;
pub const GE_LOAD_CTX_EXTRA: u32 = 0x1e4;
/// shram words the microcode fills in for Gr2PcxSwap: main context size in
/// words (0 = nothing to save), the owner (GE_CTX_SWITCH id) of that state, and
/// the extended context size.
pub const SHRAM_CTX_WORDS: usize = 0x302;
pub const SHRAM_CTX_HOLDER: usize = 0x303;
pub const SHRAM_CTX_EXTRA_WORDS: usize = 0x304;
/// Words of one saved GL context (the image streamed through HQ2_GEDMA).
pub const CX_WORDS: usize = gl::CX_WORDS;
/// Undocumented kernel-to-microcode requests used by Gr2PcxSwap (one word
/// each, fifo offsets 0x780 / 0x798); both finish with FIN2.
pub const HQ_PCX_1E0: u32 = 0x1e0;
pub const HQ_PCX_1E6: u32 = 0x1e6;
/// GL microcode request that completes by raising FIN3: Xsgi sends it and
/// spins on version bit 0, then clears FIN3 via 0x6b000.
pub const HQ_GL_FIN3: u32 = 0x155;
// Known word counts (token included, from shipped binaries), no modelled effect. The GL state tokens
// with the same property are modelled in `gl`.
/// Finish flag 2, diagnostic only.
pub const CMD_DIAG_FIN2: u32 = 1;
/// Token and 5 data words, sent by the X server at start-up.
pub const CMD_X_INIT_BLOCK: u32 = 354;
/// OpenGL tokens (libglcore's FIFO block: block offset X is FIFO
/// index (X - 0x2000) / 4).
/// glFinish / SwapBuffers: the readback trigger (0x228C); the client then spins
/// on version bit 0 (FIN3) and acks at the FIN3 port.
pub const CMD_READBACK_KICK: u32 = 0xa3;
/// glFlush / SwapBuffers: the swap sync (block offset 0x2018).
pub const CMD_SWAP_WAIT: u32 = 0x006;
/// SwapBuffers: block offset 0x279C = swap interval, then ioctl 1005.
pub const CMD_SWAP_INTERVAL: u32 = 0x1e7;

// ── 2D (Xsgi DDX) commands, GL microcode ───────────────────────────────────
pub const X2D_SESSION_START: u32 = 0x12c; // 0x404b0, sent first by Xsgi's init (1 word)
pub const X2D_FILL_BOXES: u32 = 304; // stream: 4 DATA words per box
pub const X2D_AUX_PLANE_MASK: u32 = 309; // aux-plane write mask
/// Colour for CLEAR bits of glyphs/mono images/stipples when drawn opaque
/// (its documented name is FG_COLOR; the DDX writes the GC background here).
pub const X2D_BIT_ZERO_COLOR: u32 = 311;
/// Colour for SET bits of glyphs/mono images/stipples (its documented name is
/// BG_COLOR; the DDX writes the text foreground here).
pub const X2D_BIT_ONE_COLOR: u32 = 312;
pub const X2D_PLANE_SELECT: u32 = 331; // visual/plane select (| 0x1000)
pub const X2D_FG_ALU: u32 = 332; // fg; DATA planemask, alu, flag
pub const X2D_CID_FIELD: u32 = 336; // (cid << 8) | 0xF000, 0 = off
/// Pixel format and alignment for the next image transfer: [format (2 =
/// 8-bit, 1 = 16-bit, 0 = 32-bit per pixel, as TILE_BEGIN), first pixel's
/// slot in the first word of each row] (image reads and draws).
pub const X2D_PIXEL_LAYOUT: u32 = 337;
/// Screen-to-host readback: words per row; DATA x, y (X-style, top-down),
/// width, rows. The microcode packs the rows into shram from word
/// READ_IMAGE_SHRAM (format and alignment from PIXEL_LAYOUT, MSB = first
/// pixel) and raises FIN3 (image read).
pub const X2D_IMAGE_TO_SHRAM: u32 = 344;
/// First shram word of the IMAGE_TO_SHRAM result (Xsgi reads base + 0x1B000).
pub const READ_IMAGE_SHRAM: usize = 0x6C00;
pub const X2D_PRIM_TERMINATOR: u32 = 490;
/// Tile setup: size; DATA width, height, format (2), 0xD022 (tile fill).
pub const X2D_TILE_BEGIN: u32 = 314;
/// Tile data chunks (64/32/16/8-word loaders): words append to the tile.
pub const X2D_TILE_CHUNK_FIRST: u32 = 315;
pub const X2D_TILE_CHUNK_LAST: u32 = 318;
/// Tiled fill: DATA origin x, origin y, then boxes (x1, y1, x2, y2).
pub const X2D_TILE_FILL: u32 = 321;
/// Same, used when the tile width is not a whole number of words (tile fill).
pub const X2D_TILE_FILL_RAGGED: u32 = 322;
/// 1-bpp images: x; DATA y, width, rows; then a fixed data block.
pub const X2D_BITMAP_BLOCK_A: u32 = 349;
pub const X2D_BITMAP_BLOCK_B: u32 = 350;
pub const X2D_BITMAP_BLOCK_C: u32 = 351;
pub const X2D_BITMAP_BLOCK_D: u32 = 352;
/// Host-to-screen image (image draw): x; DATA y, width, rows, words/row,
/// format, left skip; then rows*words/row pixel words, padded with
/// 0xDEADBEEF to the chunk size. Streamed.
pub const X2D_PUT_IMAGE: u32 = 342;
pub const X2D_PUT_IMAGE_SHORT: u32 = 343;
/// Host-to-screen pixel DMA (the kernel's pixel DMA start):
/// fifo\[0x147\] = x; then 6 words through FIFO index 0: y, width (pixels),
/// height, words per row, flag, 0; then height * words_per_row pixel words
/// through HQ2_GEDMA (VDMA). The kernel waits for FIN2 after the transfer.
pub const PIXDMA_TO_SCREEN: u32 = 327;
/// GL-microcode pixel DMA (IRIS GL lrectwrite):
/// 0x0B5 at pixel zoom 1, 0x0B8 zoomed. Same kernel protocol and header as
/// 0x147, drawn in the current GL context (window-relative, GL y up).
pub const GLDMA_TO_WINDOW: u32 = 0x0b5;
pub const GLDMA_TO_WINDOW_ZOOM: u32 = 0x0b8;
/// Pixel DMA reads: the same kernel ioctl and
/// header as the writes, but the VDMA reads height * words/row words from
/// HQ2_GEDMA; FIN2 when done. 0x152 = Xsgi's image reads (screen, X-style
/// y = top row, format from PIXEL_LAYOUT); 0x0AC = GL lrectread /
/// glReadPixels (window-relative, y = bottom row, source from 0x10A).
pub const PIXDMA_FROM_SCREEN: u32 = 0x152;
pub const GLDMA_FROM_WINDOW: u32 = 0x0ac;

/// Pixel rectangles streamed by the kernel's pixel DMA:
/// x on the token; y, width, height, words/row, flag, 0 on GE_DATA; then
/// the pixel words through HQ2_GEDMA; FIN2 when done.
fn is_pixel_dma(cmd: u32) -> bool {
    matches!(cmd, PIXDMA_TO_SCREEN | GLDMA_TO_WINDOW | GLDMA_TO_WINDOW_ZOOM)
}
/// Screen-to-screen copy: pitch; DATA chunk, srcX, srcY, w, h, dstX, dstY
/// (X-style y). Layout unverified against a trace.
pub const X2D_SCREEN_COPY: u32 = 340;
/// Glyphs (TE/CW text): x; DATA y, width, rows; then 8/16/32/64 data words.
pub const X2D_GLYPH_NARROW: u32 = 323;
pub const X2D_GLYPH_WIDE: u32 = 326;
/// Fill spans: stream of (x, y, width) triples, padded with (1280, 1024, 1).
pub const X2D_SPAN_LIST: u32 = 305;
/// Line segments: 0; then a stream of (x1, y1, x2, y2) quads until the next
/// command (PRIM_TERMINATOR); padding quads have x1 = x2 = 1280 (line and
/// segment draws; 4Dwm's move/resize frame is 16 of them).
pub const X2D_LINE_QUADS: u32 = 301;
/// Line mode word (line draws write 0xA).
pub const X2D_LINE_STYLE: u32 = 328;
/// Line clip box: x1; DATA x2, y1, y2 (inclusive).
pub const X2D_LINE_BOUNDS: u32 = 330;
/// Polyline: stream of (x, y) points (0x165 on some configurations).
pub const X2D_PATH_POINTS: u32 = 302;
pub const X2D_PATH_POINTS_ALT: u32 = 357;
/// Segments: stream of (x1, y1, x2, y2), padded with (1280, 1024) pairs.
pub const X2D_SEGMENT_LIST: u32 = 345;
/// Points (point draws, XDrawPoints): 0; stream of (x, y), padded with
/// (1280, 1024) pairs, clipped to LINE_BOUNDS.
pub const X2D_DOT_LIST: u32 = 353;
/// Stipple opaque flag (1 = zero bits get bg).
pub const X2D_STIPPLE_OPAQUE: u32 = 313;
/// Stippled box: 0; DATA origin x, x1, origin y, y1, x2, y2.
pub const X2D_STIPPLED_BOX: u32 = 348;
/// Stippled boxes from Xsgi's rectangle fill, same 7 words as STIPPLED_BOX,
/// stipple loaded through TILE_BEGIN (format 3) + TILE_CHUNK. 319 is used for
/// power-of-two stipple widths, 320 for the rest.
pub const X2D_STIPFILL_POW2: u32 = 319;
pub const X2D_STIPFILL_OTHER: u32 = 320;
/// Stippled spans (span fill): 0; stream of (x, y, w, pattern)
/// quads, y top-down. The pattern is pre-rotated so its MSB is pixel x and
/// repeats every 32 pixels. 346 for stipples narrower than 32 / multi-word,
/// 347 for 32-wide stipples. BIT_ONE_COLOR = set bits, STIPPLE_OPAQUE 1 = opaque
/// with BIT_ZERO_COLOR.
pub const X2D_STIPPLE_SPANS_NARROW: u32 = 346;
pub const X2D_STIPPLE_SPANS_32: u32 = 347;
/// Off-screen sentinel coordinates the DDX pads streams with.
const PAD_X: u32 = 1280;
const PAD_Y: u32 = 1024;
/// Max pixel words per image row kept (1280 8-bit pixels = 320 words).
const IMG_ROW_WORDS: usize = 1280;
/// Pixel words one FIFO pixel-write chunk can hold (chunks are <= 48 pixels;
/// IRIS GL pads its last 0x071 block to 16 words).
pub(super) const PIX_WORDS: usize = 64;
/// Max tile words kept (a 64x64 8-bit tile is 1024 words; larger is clipped).
/// Tile/stipple store. The tile fill sends tiles of up to 0x1300 words
/// (words per row * rows <= 0x1300; 4Dwm's 85x67 8-bit icon images need
/// 1474), padded to a multiple of 8 words.
const TILE_WORDS: usize = 0x1308;

/// 2D drawing state set by the MODE/ROP/AUX_PLANE_MASK/... commands.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct State2d {
    pub mode: u32,
    pub aux_mask: u32,
    /// ROP fg: colour for solid fills, spans, lines.
    pub fg: u32,
    /// 0x138: set bits of 1-bpp primitives.
    pub color_on: u32,
    /// 0x137: clear bits of 1-bpp primitives when opaque.
    pub color_off: u32,
    pub planemask: u32,
    pub alu: u32,
    pub flag: u32,
    pub cid_write: u32,
    pub buf_select: u32,
    pub buf_offset: u32,
}

/// How a screen-to-host read turns a VRAM word into a pixel value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadDecode {
    /// Xsgi: the plane group selected by the 2D MODE (`read_value`).
    Planes2d,
    /// GL 24-bit RGB: 0xAABBGGRR, alpha 0xFF (no alpha planes).
    Rgb24,
    /// GL 12-bit RGB in `bank` (0 = bits 11:0, 1 = bits 23:12), nibbles
    /// widened to 8 bits (x 17), as Rgb24.
    Rgb12 { bank: u32 },
    /// GL colour index in 12-bit `bank`.
    Ci12 { bank: u32 },
    /// GL 8-bit colour index in 8-bit `bank` (bits 7:0 / 15:8, see
    /// re3::PIXFMT_CI8).
    Ci8 { bank: u32 },
    /// GL 8-bit 3:3:2 RGB in 8-bit `bank`, widened as Rgb24.
    Rgb8 { bank: u32 },
    /// GL 4-bit 1:2:1 RGB in 4-bit `bank` (bits 3:0 / 7:4), widened as Rgb24.
    Rgb4 { bank: u32 },
    /// GL depth: Z right-justified (libglcore shifts it up by 32 - bits),
    /// with `sbits` stencil bits above it in the 24-bit word (a z20 s4
    /// visual: Z 19:0, stencil 23:20). Inferred: every libglcore depth read
    /// drops what is above Z (the DMA path shifts by 32 - 20 and masks
    /// 0xFFFFF), while its stencil read masks the same word with 0xF.
    Depth { sbits: u32 },
}

/// Where a screen-to-host read goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadDest {
    /// shram from this word (IMAGE_TO_SHRAM: READ_IMAGE_SHRAM; GL READ_BOX:
    /// the mailbox), then FIN3.
    Shram(usize),
    /// Pixel DMA read: the HQ2_GEDMA read port, then FIN2.
    Gedma,
}

/// A screen-to-host read (IMAGE_TO_SHRAM, 0x152, 0x0AC), as the sink needs it.
#[derive(Clone, Copy)]
pub struct ReadImage {
    /// Left x and GL row of the top line (y = 0 at the bottom); rows go
    /// down from there.
    pub x: i32,
    pub top: i32,
    pub w: u32,
    pub rows: u32,
    pub words_per_row: u32,
    /// Pixel format (buf_select: 2 = 8-bit, 1 = 16-bit, 0 = 32-bit) and
    /// first-pixel slot (buf_offset); the planes for Planes2d.
    pub s2d: State2d,
    pub decode: ReadDecode,
    /// Rows bottom first (GL mailbox reads, 0x0AD) instead of top first.
    pub bottom_up: bool,
}

impl ReadImage {
    pub fn per_word(&self) -> u32 {
        match self.s2d.buf_select { 2 => 4, 1 => 2, _ => 1 }
    }

    /// Pack the rectangle (as `read` returns VRAM words) into `dst`:
    /// rows of `words_per_row` words, MSB = first pixel, the first pixel
    /// of each row in slot `buf_offset`.
    pub fn pack(&self, read: impl Fn(i32, i32) -> u32, dst: &mut [u32]) {
        let per_word = self.per_word();
        let bits = 32 / per_word;
        let mask = if bits == 32 { u32::MAX } else { (1 << bits) - 1 };
        let off = self.s2d.buf_offset.min(per_word - 1) as i32;
        let wpr = self.words_per_row as usize;
        for r in 0..self.rows as usize {
            let gy = if self.bottom_up { self.top.wrapping_sub(self.rows as i32 - 1).wrapping_add(r as i32) } else { self.top.wrapping_sub(r as i32) };
            for k in 0..wpr {
                let Some(d) = dst.get_mut(r * wpr + k) else { return };
                let mut word = 0;
                for p in 0..per_word {
                    let i = (k as u32 * per_word + p) as i32 - off;
                    if i >= 0 && (i as u32) < self.w {
                        let v = self.value(read(self.x.wrapping_add(i), gy)) & mask;
                        word |= v << (32 - bits * (p + 1));
                    }
                }
                *d = word;
            }
        }
    }
}

impl ReadImage {
    /// The pixel value of VRAM word `vram` (for Depth: the Z buffer word).
    pub fn value(&self, vram: u32) -> u32 {
        let wide = |n: u32| (n & 0xf) * 17;
        match self.decode {
            ReadDecode::Planes2d => read_value(&self.s2d, vram),
            ReadDecode::Rgb24 => 0xff00_0000 | (vram & 0x00ff_ffff),
            ReadDecode::Rgb12 { bank } => {
                let v = (vram >> (12 * (bank & 1))) & 0xfff;
                0xff00_0000 | wide(v) | (wide(v >> 4) << 8) | (wide(v >> 8) << 16)
            }
            ReadDecode::Ci12 { bank } => (vram >> (12 * (bank & 1))) & 0xfff,
            ReadDecode::Ci8 { bank } => (vram >> (8 * (bank & 1))) & 0xff,
            ReadDecode::Rgb8 { bank } => {
                let [r, g, b] = re3::rgb332_to_888((vram >> (8 * (bank & 1))) & 0xff);
                0xff00_0000 | r | (g << 8) | (b << 16)
            }
            ReadDecode::Rgb4 { bank } => {
                let [r, g, b] = re3::rgb121_to_888((vram >> (4 * (bank & 1))) & 0xf);
                0xff00_0000 | r | (g << 8) | (b << 16)
            }
            ReadDecode::Depth { sbits } => {
                let zbits = 24 - sbits;
                let s = (vram >> re3::ZBUF_STENCIL_SHIFT) & ((1u32 << sbits) - 1);
                (vram & ((1u32 << zbits) - 1)) | (s << zbits)
            }
        }
    }
}

/// The value a VRAM word holds for the active plane group (the inverse of
/// `value2d`): 4-bit overlay (MODE 8), 2-bit overlay or popup (MODE 0xB),
/// or the colour planes (R in bits 7:0).
pub fn read_value(s: &State2d, vram: u32) -> u32 {
    let aux = (vram >> 24) & 0xf;
    match s.mode & 0xff {
        8 => aux,
        0xb => if s.aux_mask & 0xc != 0 { (aux >> 2) & 3 } else { aux & 3 },
        // 12-bit RGB (the 12-bit image read: MODE 0x1002, ROP flag = the
        // window's buffer * 8): the bank the flag selects, R 3:0, G 7:4,
        // B 11:8, as 8:8:8 with each nibble replicated. The DDX takes the
        // low nibble of each byte back ((w & 0xF) | (w & 0xF00) >> 4 |
        // (w & 0xF0000) >> 8); the 12-bit image draw sends the high nibbles.
        2 => {
            let v = (vram >> (12 * ((s.flag >> 3) & 1))) & 0xfff;
            let wide = |n: u32| (n & 0xf) * 17;
            wide(v) | (wide(v >> 4) << 8) | (wide(v >> 8) << 16)
        }
        _ => vram & 0x00ff_ffff,
    }
}

/// Name of a FIFO word index: textport (PUC_*), GL/X (GE_*) or context-switch
/// tokens. Indices are microcode entry points, so the meaning
/// depends on the loaded microcode; these are the documented ones.
pub fn token_name(index: u32) -> Option<&'static str> {
    Some(match index {
        PUC_INIT => "PUC_INIT", PUC_COLOR => "PUC_COLOR", PUC_FINISH => "PUC_FINISH",
        PUC_PNT2I => "PUC_PNT2I", PUC_RECTI2D => "PUC_RECTI2D", PUC_CMOV2I => "PUC_CMOV2I",
        PUC_LINE2I => "PUC_LINE2I", PUC_DRAWCHAR => "PUC_DRAWCHAR", PUC_RECTCOPY => "PUC_RECTCOPY",
        PUC_DATA => "DATA", HQ_TOKEN_GEDMA => "GEDMA", HQ_TOKEN_UNSTALL => "UNSTALL",
        PIXDMA_TO_SCREEN => "PIXDMA_TO_SCREEN",
        HQ_PCX_1E0 => "PCX_1E0", HQ_PCX_1E6 => "PCX_1E6", HQ_GL_FIN3 => "2D_SYNC",
        CMD_READBACK_KICK => "CMD_READBACK_KICK", CMD_SWAP_WAIT => "CMD_SWAP_WAIT", CMD_SWAP_INTERVAL => "CMD_SWAP_INTERVAL",
        X2D_SESSION_START => "2D_SESSION_START", X2D_LINE_QUADS => "2D_LINE_QUADS", 303 => "2D_POINTS",
        X2D_FILL_BOXES => "2D_FILL_BOXES", 305 => "2D_SPAN_LIST", 307 => "2D_LINE",
        X2D_AUX_PLANE_MASK => "2D_AUX_PLANE_MASK", X2D_BIT_ZERO_COLOR => "2D_BIT_ZERO_COLOR", X2D_BIT_ONE_COLOR => "2D_BIT_ONE_COLOR",
        313 => "2D_STIPPLE_OPAQUE", X2D_TILE_BEGIN => "2D_TILE_BEGIN", 315..=318 => "2D_TILE_CHUNK",
        X2D_STIPFILL_POW2 | X2D_STIPFILL_OTHER => "2D_STIPPLE_FILL", X2D_TILE_FILL | X2D_TILE_FILL_RAGGED => "2D_TILE_FILL",
        X2D_PATH_POINTS => "2D_PATH_POINTS", X2D_PATH_POINTS_ALT => "2D_PATH_POINTS", X2D_SEGMENT_LIST => "2D_SEGMENT_LIST", X2D_DOT_LIST => "2D_DOT_LIST",
        323 => "2D_GLYPH_12W", 324 => "2D_GLYPH_20W", 325 => "2D_GLYPH_36W", 326 => "2D_GLYPH_68W",
        X2D_LINE_STYLE => "2D_LINE_STYLE", X2D_LINE_BOUNDS => "2D_LINE_BOUNDS", X2D_PLANE_SELECT => "2D_PLANE_SELECT", X2D_FG_ALU => "2D_FG_ALU",
        X2D_CID_FIELD => "2D_CID_FIELD", X2D_PIXEL_LAYOUT => "2D_PIXEL_LAYOUT",
        PIXDMA_FROM_SCREEN => "PIXDMA_FROM_SCREEN", GLDMA_FROM_WINDOW => "GLDMA_FROM_WINDOW",
        340 => "2D_SCREEN_COPY", 342 => "2D_PUT_IMAGE", 343 => "2D_PUT_IMAGE_SHORT",
        X2D_IMAGE_TO_SHRAM => "2D_IMAGE_TO_SHRAM", 346 | 347 => "2D_STIPPLE_SPANS", X2D_STIPPLED_BOX => "2D_STIPPLED_BOX",
        349 => "2D_BITMAP_8W", 350 => "2D_BITMAP_12W", 351 => "2D_BITMAP_20W",
        352 => "2D_BITMAP_36W", 354 => "X_INIT_BLOCK", X2D_PRIM_TERMINATOR => "2D_PRIM_TERMINATOR",
        0 => "GE_DATA", 1 => "GE_INIT", 10 => "GE_COLOR", 11 => "GE_FINISH0", 12 => "GE_PNT2I",
        13 => "GE_SBOXI", 14 => "GE_CMOV2I", 15 => "GE_DRAWCHAR", 20 => "GE_READPIXELS",
        21 => "GE_WRITEPIXELS", 22 => "GE_LOADRE", 26 => "GE_PIXWRITEMASK", 29 => "GE_READBLOCK",
        30 => "GE_RECTREAD", 32 => "GE_READPIXDMA", 35 => "GE_RECTCOPY", 36 => "GE_RGBCOLOR",
        38 => "GE_RWMODE", 39 => "GE_SCREENCLEAR", 42 => "GE_WRITEBLOCK", 43 => "GE_RECTWRITE",
        44 => "GE_WRITEPIXDMA", 46 => "GE_ZBUFFER", 47 => "GE_ZCLEAR", 48 => "GE_ZOOMFACTOR",
        49 => "GE_READSOURCE", 50 => "GE_DRAWMODE", 51 => "GE_CZCLEAR", 61 => "GE_POLYGON",
        62 => "GE_ENDPOLYGON", 82 => "GE_LINESTYLE", 83 => "GE_LINEWIDTH", 84 => "GE_VERTEX2I",
        86 => "GE_VERTEX3I", 87 => "GE_VERTEX3F", 90 => "GE_MOVE2I", 92 => "GE_DRAW2I",
        94 => "GE_RDRAW2I", 96 => "GE_PATTERN", 101 => "GE_SBOXF", 102 => "GE_SBOXFI",
        105 => "GE_SBOX", 108 => "GE_SCRMASK", 109 => "GE_RASTEROP",
        481 => "GE_DUMP_CTX", 482 => "GE_LOAD_CTX", 483 => "GE_DUMP_CTX_EXTRA",
        484 => "GE_LOAD_CTX_EXTRA", 496 => "GE_CTX_SWITCH", 511 => "DIAG_CTXSW",
        _ => return None,
    })
}

/// OpenGL microcode tokens (FIFO index bits 8:0), named after their libglcore
/// EXPRESS emitters.
pub fn gl_token_name(tok: u32) -> Option<&'static str> {
    Some(match tok {
        0x003 => "GL_PIXWRITEMASK", 0x004 => "GL_BIND_CONTEXT", 0x006 => "CMD_SWAP_WAIT",
        0x00a => "GL_DEPTH_WRITE", 0x00e => "GL_STENCIL_BITS", 0x00f => "GL_STENCIL_ON",
        0x014 => "GL_DEPTH_TEST", 0x01a => "GL_LOGIC_OP", 0x01e => "GL_POLYGON_STIPPLE_OFF",
        0x01f => "GL_POLYGON_STIPPLE", 0x024 => "GL_DEPTH_FUNC", 0x026 => "GL_BLEND_FAST",
        0x02b => "GL_SOFT_FRAGMENT", 0x0a1 => "GL_CLEAR_STENCIL", 0x0bd => "GL_PEEK_ACK",
        0x0e9 => "GL_PEEK_COLOR", 0x0ea => "GL_PEEK_NORMAL", 0x107 => "GL_PEEK_RASTER_COLOR",
        0x105 => "GL_SET_RASTER_POS", 0x18c..=0x18e => "GL_BITMAP", 0x0fe => "GL_INDEX",
        0x010 => "GL_STENCIL_WMASK", 0x011 => "GL_DITHER", 0x013 => "GL_SHADE_MODEL",
        0x016 => "GL_LINE_STIPPLE_PATTERN", 0x017 => "GL_LINE_WIDTH", 0x018 => "GL_LINE_STIPPLE",
        0x01b => "GL_CULL_A", 0x01c => "GL_CULL_B", 0x020 => "GL_POINT_SIZE",
        0x021 => "GL_POINT_SMOOTH", 0x022 => "GL_LINE_SMOOTH", 0x025 => "GL_BLEND_FACTOR",
        0x028 => "GL_FOG", 0x037 => "GL_MODELVIEW", 0x038 => "GL_PROJECTION", 0x039 => "GL_NORMAL_MATRIX",
        0x03a => "GL_TEXTURE_MATRIX", 0x03b => "GL_VIEWPORT", 0x03c => "GL_SCISSOR",
        0x046 => "GL_BEGIN_TSTRIP_TFAN", 0x047 => "GL_VTX_TSTRIP", 0x04a => "GL_END_TSTRIP_TFAN",
        0x04c => "GL_BEGIN_QSTRIP", 0x04d => "GL_VTX_QSTRIP", 0x051 => "GL_END_QSTRIP",
        0x056 => "GL_VTX_LSTRIP", 0x057 => "GL_END_LINES", 0x058 => "GL_BEGIN_LLOOP",
        0x059 => "GL_VTX_LLOOP", 0x05a => "GL_END_LLOOP", 0x063 => "VERTEX", 0x065 => "GL_VTX_CLOSE_POLY",
        0x074 => "GL_SPEC_LUT", 0x076..=0x07d => "GL_MATERIAL", 0x07f..=0x085 => "GL_LIGHT",
        0x0a0 => "GL_CLEAR_RGBA_Z", 0x0a3 => "CMD_READBACK_KICK", 0x0d4 => "GL_LIGHTPATH", 0x0e2 => "GL_POLYGON_MODE",
        0x0e3 => "GL_NORMALIZE", 0x0e4 => "GL_POLYGON_OFFSET", 0x0e6 => "GL_CONTEXT_FLUSH",
        0x0eb => "GL_VTX_LINES", 0x0ed => "GL_VTX_TFAN",
        0x0f0 => "GL_BEGIN_TRIANGLES", 0x0f1 => "GL_END_TRIANGLES", 0x0f2 => "GL_VTX_TRIANGLES",
        0x0f3 => "GL_BEGIN_QUADS", 0x0f4 => "GL_END_QUADS", 0x0f5 => "GL_VTX_QUADS",
        0x0f6 => "GL_EDGE_FLAG", 0x0f7 => "GL_BEGIN_POLYGON", 0x0f8 => "GL_VTX_POLYGON",
        0x0fc | 0x0fd => "GL_END_POLYGON",         0x104 => "GL_CLEAR_COLOR", 0x106 => "GL_STATE_106", 0x108 => "GL_FRONT_FACE",
        0x10a => "GL_READ_BUFFER", 0x10b => "GL_COLOR_WRITEMASK", 0x10d => "GL_NORMAL",
        0x10e => "GL_LIGHTPATH", 0x11d => "GL_BEGIN_POINTS", 0x123 => "GL_VTX_POINTS",
        0x168 => "GL_END_POINTS", 0x17c => "GL_BEGIN_LINES", 0x182 => "COLOR",
        0x1e5 => "GL_WIN_BOX", 0x1e7 => "CMD_SWAP_INTERVAL",
        _ => return None,
    })
}

/// Trace label for any FIFO index: a documented name, or the GL token with
/// its HQ2 modifier bits (IDX_INT_SOURCE, conversion, IDX_VERTEX_ROUTINE, IDX_VERTEX_CONSUME) decoded.
pub fn index_label(index: u32) -> String {
    // GL names first: the IrisGL-era GE_* names for low indices
    // do not apply to the OpenGL microcode and are only used above 0x12B
    // (2D, textport, kernel tokens) or for the data ports.
    if let Some(n) = gl_token_name(index) {
        return n.to_string();
    }
    if let Some(n) = token_name(index).filter(|_| index >= 0x12c || index == 0) {
        return n.to_string();
    }
    let tok = index & 0x1ff;
    let mut mods: Vec<&str> = Vec::new();
    if index & 0x4000 != 0 { mods.push("IDX_INT_SOURCE"); }
    match (index >> 11) & 7 {
        1 => mods.push("V3"), 2 => mods.push("V2"), 3 => mods.push("C3"), 4 => mods.push("C4"),
        5 => mods.push("CP"), 6 => mods.push("C1"), 7 => mods.push("CONV7"), _ => {}
    }
    if index & 0x400 != 0 { mods.push("IDX_VERTEX_ROUTINE"); }
    if index & 0x200 != 0 { mods.push("IDX_VERTEX_CONSUME"); }
    let base = gl_token_name(tok).or_else(|| token_name(tok).filter(|_| tok >= 0x12c || tok == 0))
        .map(str::to_string).unwrap_or_else(|| format!("tok{tok:#x}"));
    if mods.is_empty() { base } else { format!("{}:{base}", mods.join("|")) }
}

/// Screen height the textport commands assume (1280x1024 CRT).
const SCREEN_H: i32 = 1024;
const MAX_ARGS: usize = 72;
/// `need` value for an open (variable-length, unknown) command.
const OPEN: u32 = u32::MAX;
/// `need` value for a streaming command: DATA words are consumed as they
/// arrive (e.g. 4 per box for FILL_BOXES) until the next command.
const STREAM: u32 = u32::MAX - 1;
const FIFO_WORDS: usize = 0x8000;
/// The longest side a tile is kept at (its words are 0x1308 at most).
const TILE_SIDE_MAX: u32 = 0x4000;
/// The widest image row a draw follows.
const IMAGE_ROW_MAX: u32 = 0x10000;
/// The longest run of either coordinate a 2D line may span.
const LINE_SPAN_MAX: i64 = 0xffff;

/// CPU-visible HQ2 registers and microcode RAM. Plain data, zero-valid.
#[repr(C)]
pub struct Hq2Regs {
    pub attrjmp: [u32; 16],
    pub numge: u32,
    /// Finish flags FIN1..FIN3. The CPU clears them; the HQ2 thread sets
    /// them when the microcode would, so they are atomics.
    pub fin: [AtomicU32; 3],
    pub dmasync: u32,
    pub fifo_full_timeout: u32,
    pub fifo_empty_timeout: u32,
    pub fifo_full: u32,
    pub fifo_empty: u32,
    /// Readback latch for `ge7loaducode` (reloaded by a `gepc` write).
    pub loaducode: u32,
    pub gepc: u32,
    pub intr: u32,
    pub refresh: u32,
    /// Set by a write to `unstall`: the engine is running microcode.
    pub running: u32,
    /// The revision `version` and `hq_gepc` report; the board's probe sets it.
    pub rev: u32,
    pub ucode: [u32; HQ_UCODE_WORDS],
}

impl Hq2Regs {
    pub fn read(&self, off: u32) -> u32 {
        match off {
            o if o < HQ_VERSION => self.attrjmp[(o >> 2) as usize & 15],
            HQ_VERSION => {
                let f = |i: usize| (self.fin[i].load(Ordering::Acquire) != 0) as u32;
                (self.rev << 23) | VERSION_LOWATER | (f(FIN1) << 2) | (f(FIN2) << 1) | f(FIN3)
            }
            HQ_NUMGE => self.numge,
            HQ_FIN1 => self.fin[FIN1].load(Ordering::Acquire),
            HQ_FIN2 => self.fin[FIN2].load(Ordering::Acquire),
            HQ_DMASYNC => self.dmasync,
            HQ_FIFO_FULL_TIMEOUT => self.fifo_full_timeout,
            HQ_FIFO_EMPTY_TIMEOUT => self.fifo_empty_timeout,
            HQ_FIFO_FULL => self.fifo_full,
            HQ_FIFO_EMPTY => self.fifo_empty,
            HQ_GE7LOADUCODE => self.loaducode & LOADUCODE_MASK,
            // The kernel reads the HQ2 revision from hq_gepc[31:16].
            HQ_HQ_GEPC => (self.rev << 16) | (self.gepc & 0xffff),
            HQ_GEPC => self.gepc,
            HQ_INTR => self.intr,
            HQ_MYSTERY => HQ2_MYSTERY,
            HQ_REFRESH => self.refresh,
            HQ_FIN3 => self.fin[FIN3].load(Ordering::Acquire),
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u32, val: u32, ge: &mut Ge7) {
        match off {
            o if o < HQ_VERSION => self.attrjmp[(o >> 2) as usize & 15] = val,
            HQ_NUMGE => self.numge = val,
            HQ_FIN1 => self.fin[FIN1].store(val, Ordering::Release),
            HQ_FIN2 => self.fin[FIN2].store(val, Ordering::Release),
            HQ_DMASYNC => self.dmasync = val,
            HQ_FIFO_FULL_TIMEOUT => self.fifo_full_timeout = val,
            HQ_FIFO_EMPTY_TIMEOUT => self.fifo_empty_timeout = val,
            HQ_FIFO_FULL => self.fifo_full = val,
            HQ_FIFO_EMPTY => self.fifo_empty = val,
            HQ_GE7LOADUCODE => {
                self.loaducode = val;
                ge.commit(self.gepc, val);
            }
            HQ_GEPC => {
                self.gepc = val & 0xffff;
                self.loaducode = ge.reload(self.gepc);
            }
            HQ_INTR => self.intr = val,
            HQ_UNSTALL => self.running = 1,
            HQ_REFRESH => self.refresh = val,
            HQ_FIN3 => self.fin[FIN3].store(val, Ordering::Release),
            _ => {}
        }
    }
}

/// Where the interpreter's output goes: RE3 register writes and private ops.
pub trait Re3Sink {
    fn reg(&mut self, reg: usize, val: u32);
    /// Screen-to-screen copy; `mode` 0 = raw (2D), else a GL copy
    /// (re3::copy_mode: zoom, read bank, logic op and WID test).
    fn copy(&mut self, sx: i32, sy: i32, w: i32, h: i32, dx: i32, dy: i32, mode: u32);
    /// Raise a finish flag (FIN1/FIN2/FIN3), as the microcode does when it
    /// completes a request the host is waiting on.
    fn finish(&mut self, flag: usize);
    /// Screen-to-host read: once all drawing is done, pack the rectangle
    /// into shram and raise FIN3 (IMAGE_TO_SHRAM), or feed it to the HQ2_GEDMA
    /// read port and raise FIN2 (pixel DMA reads).
    fn read_image(&mut self, req: &ReadImage, dest: ReadDest);
    /// Emulator-private RE3 operation (re3::RE3_OP_*).
    fn op(&mut self, op: u32, val: u64);
    /// Store a word into shared RAM, as the microcode does for readbacks.
    fn shram(&mut self, word: usize, val: u32);
    /// Start a new transfer on the HQ2_GEDMA read port and queue `words`
    /// (a context image, 0x1E1). Words a previous transfer left unread are
    /// dropped. Blocks while the port is full.
    fn gedma_out(&mut self, words: &[u32]);
}

/// HLE command interpreter state. Owned by the HQ2 thread. Plain data.
#[repr(C)]
pub struct Hq2Engine {
    cmd: u32,
    need: u32,
    nargs: u32,
    last_nargs: u32,
    /// Total words of the last command, including any beyond MAX_ARGS.
    last_total: u32,
    args: [u32; MAX_ARGS],
    /// Character raster position (GL coordinates, y = 0 at the bottom).
    cx: i32,
    cy: i32,
    pattern_on: u32,
    /// Set by UNSTALL: the next stray DATA word is the start argument.
    restarted: u32,
    /// Streaming command state: words collected for the current item, items done.
    stream_words: u32,
    stream_items: u32,
    s2d: State2d,
    /// Tile for TILE_FILL: geometry from TILE_BEGIN, words from TILE_CHUNK.
    tile_w: u32,
    tile_h: u32,
    tile_len: u32,
    /// Tile pixel format from TILE_BEGIN: 2 = 8-bit pixels, 3 = 1-bit stipple.
    tile_fmt: u32,
    tile: [u32; TILE_WORDS],
    stipple_opaque: u32,
    /// Line clip box (inclusive), set by LINE_BOUNDS; full screen after BEGIN.
    line_clip: [i32; 4],
    /// Previous polyline point, and whether one is pending.
    line_prev: [i32; 2],
    line_have_prev: u32,
    /// PUT_IMAGE: header, current row buffer, rows done.
    img_hdr: [u32; 7],
    img_row: [u32; IMG_ROW_WORDS],
    img_rows_done: u32,
    /// One bit per FIFO index already reported as unimplemented.
    warned: [u64; FIFO_WORDS / 64],
    /// OpenGL (GE7 geometry) state.
    gl: gl::GlState,
    /// GL context switch bookkeeping and restore stream (GE_CTX_SWITCH).
    gl_ctx: gl::GlCx,
    /// GE_LOAD_CTX_EXTRA words still to consume (u32::MAX: count not yet in).
    cx_ext_left: u32,
    /// FIFO pixel writes (0x0B1 start, 0x071 data, 0x0B2 place, 0x0B3
    /// end): words collected for the current chunk (up to 48 pixels).
    pix: [u32; PIX_WORDS],
    pix_n: u32,
}

impl Hq2Engine {
    pub(super) fn scrub_snapshot_image(bytes: &mut [u8]) {
        let start = std::mem::offset_of!(Hq2Engine, warned);
        let len = std::mem::size_of::<[u64; FIFO_WORDS / 64]>();
        if let Some(warned) = bytes.get_mut(start..start + len) {
            warned.fill(0);
        }
    }

    const fn zero_warned() -> [u64; FIFO_WORDS / 64] {
        [0; FIFO_WORDS / 64]
    }

    pub(super) fn clear_runtime_diagnostics(&mut self) {
        self.warned = Self::zero_warned();
    }

    pub(super) fn unimplemented_commands(&self) -> Vec<u32> {
        self.warned
            .iter()
            .enumerate()
            .flat_map(|(word, &bits)| {
                (0..64).filter_map(move |bit| {
                    (bits & (1u64 << bit) != 0).then_some((word * 64 + bit) as u32)
                })
            })
            .collect()
    }

    pub(super) fn validate_snapshot(&self) -> Result<(), String> {
        if self.need != 0 && self.need != OPEN && self.need != STREAM && self.need as usize > MAX_ARGS {
            return Err(format!("HQ2 command needs {} arguments (maximum {MAX_ARGS})", self.need));
        }
        if self.need != OPEN && self.need != STREAM && self.nargs > self.need {
            return Err(format!("HQ2 command has {} of {} arguments", self.nargs, self.need));
        }
        if self.need != OPEN && self.nargs as usize > MAX_ARGS {
            return Err(format!("HQ2 command retains {} arguments (maximum {MAX_ARGS})", self.nargs));
        }
        if self.last_nargs as usize > MAX_ARGS {
            return Err(format!("HQ2 last command retained {} arguments (maximum {MAX_ARGS})", self.last_nargs));
        }
        if self.line_have_prev > 1 {
            return Err(format!("HQ2 line continuation flag is {}", self.line_have_prev));
        }
        if self.pix_n as usize > PIX_WORDS {
            return Err(format!("HQ2 pixel chunk contains {} words (maximum {PIX_WORDS})", self.pix_n));
        }
        self.gl.validate_snapshot()?;
        self.gl_ctx.validate_snapshot()?;
        Ok(())
    }

    /// Total words (command word + PUC_DATA words) each command consumes.
    fn arg_count(cmd: u32) -> u32 {
        match cmd {
            PUC_INIT | PUC_COLOR | PUC_FINISH => 1,
            PUC_PNT2I | PUC_CMOV2I => 2,
            PUC_RECTI2D | PUC_LINE2I => 4,
            PUC_DRAWCHAR => 25,
            PUC_RECTCOPY => 8,
            GE_CTX_SWITCH => 3,
            GE_DUMP_CTX | GE_DUMP_CTX_EXTRA => 1,
            // Restore main: the token carries the word count; the image
            // follows on HQ2_GEDMA. Restore ext: token, DATA count, image.
            GE_LOAD_CTX | GE_LOAD_CTX_EXTRA => STREAM,
            HQ_GL_FIN3 | CMD_READBACK_KICK | CMD_SWAP_WAIT => 1,
            HQ_PCX_1E0 | HQ_PCX_1E6 => 1,
            CMD_DIAG_FIN2 => 1,
            CMD_X_INIT_BLOCK => 6,
            X2D_SESSION_START | X2D_AUX_PLANE_MASK | X2D_BIT_ZERO_COLOR | X2D_BIT_ONE_COLOR | X2D_PLANE_SELECT
            | X2D_CID_FIELD | X2D_PRIM_TERMINATOR => 1,
            X2D_PIXEL_LAYOUT => 2,
            X2D_IMAGE_TO_SHRAM => 5,
            // Token x, then y, width, height, words/row, flag, 0 on GE_DATA.
            PIXDMA_FROM_SCREEN | GLDMA_FROM_WINDOW => 7,
            X2D_FG_ALU => 4,
            X2D_TILE_BEGIN => 5,
            X2D_BITMAP_BLOCK_A => 8,
            X2D_BITMAP_BLOCK_B => 12,
            X2D_BITMAP_BLOCK_C => 20,
            X2D_BITMAP_BLOCK_D => 36,
            X2D_GLYPH_NARROW => 12,
            324 => 20,
            325 => 36,
            X2D_GLYPH_WIDE => 68,
            X2D_LINE_STYLE | X2D_STIPPLE_OPAQUE => 1,
            X2D_LINE_BOUNDS => 4,
            X2D_STIPPLED_BOX | X2D_STIPFILL_POW2 | X2D_STIPFILL_OTHER => 7,
            X2D_STIPPLE_SPANS_NARROW | X2D_STIPPLE_SPANS_32 => STREAM,
            X2D_SPAN_LIST | X2D_PATH_POINTS | X2D_PATH_POINTS_ALT | X2D_SEGMENT_LIST | X2D_LINE_QUADS
            | X2D_DOT_LIST => STREAM,
            X2D_FILL_BOXES => STREAM,
            X2D_TILE_FILL | X2D_TILE_FILL_RAGGED => 7,
            X2D_PUT_IMAGE | X2D_PUT_IMAGE_SHORT | PIXDMA_TO_SCREEN
            | GLDMA_TO_WINDOW | GLDMA_TO_WINDOW_ZOOM => STREAM,
            X2D_SCREEN_COPY => 8,
            X2D_TILE_CHUNK_FIRST..=X2D_TILE_CHUNK_LAST => STREAM,
            _ => Self::gl_arg_count(cmd).unwrap_or(OPEN),
        }
    }

    /// Feed one FIFO entry (word index, data). `done` is called with a
    /// decoded description of every command that completes (tracing only).
    ///
    /// Known commands have a fixed word count. Unknown (not yet implemented)
    /// commands are *open*: they collect every following DATA word until the
    /// next command index arrives, so traces show each unknown command with
    /// all its arguments.
    pub fn push(&mut self, index: u32, val: u32, out: &mut dyn Re3Sink, done: Option<&mut dyn FnMut(String)>) {
        let mut done = done;
        if index == HQ_TOKEN_UNSTALL {
            if self.need == OPEN {
                self.complete(out, &mut done);
            }
            self.end_short_raster_pos(out, &mut done);
            self.need = 0;
            self.nargs = 0;
            self.restarted = 1;
            return;
        }
        // FIFO index 0 (GE_DATA) is a data port too: the kernel's pixel-DMA
        // trigger streams its arguments there.
        // Any index whose token bits are PUC_DATA is a data port: the IDX_INT_SOURCE
        // variant (0x41DF, integer DATA) carries e.g. software fragments.
        if index == PUC_DATA || index == HQ_TOKEN_GEDMA || index == 0
            || (index < FIFO_WORDS as u32 && index & 0x1ff == PUC_DATA) {
            if self.need == 0 && self.gl_port_wants_data(val, out, &mut done) {
                return;
            }
            if self.need == 0 {
                if self.restarted != 0 && index == PUC_DATA {
                    // Microcode reset path (the kernel's start): one argument
                    // (0/1/2 by board rev), then FIN2.
                    self.restarted = 0;
                    self.cmd = HQ_TOKEN_UNSTALL;
                    self.args[0] = val;
                    self.nargs = 1;
                    self.complete(out, &mut done);
                }
                // Stray argument: the last command took fewer words than
                // the host sent (usually a stream mistaken for fixed-size).
                if let Some(f) = done.as_mut() {
                    f(format!("stray DATA {val:#x} after {}", token_name(self.cmd).map(str::to_string)
                        .unwrap_or_else(|| format!("CMD{:#x}", self.cmd))));
                }
                return;
            }
            if self.need == STREAM {
                self.stream_word(val, out, &mut done);
                return;
            }
            if (self.nargs as usize) < MAX_ARGS {
                self.args[self.nargs as usize] = val;
            }
            self.nargs += 1;
        } else {
            // Approximation: 4 operands or none, told at the next command start. Misread: operands on
            // the token's own index (colour only), bare DATA after the short form (operands).
            self.end_short_raster_pos(out, &mut done);
            // GL self-streaming ports (matrix, vertex, colour, normal): every
            // word goes to the same index; they close any open command.
            let mid_fixed = self.need != 0 && self.need != OPEN && self.need != STREAM && self.nargs < self.need;
            if !mid_fixed && self.gl_port_ready(index) {
                if self.need == STREAM {
                    self.end_stream(&mut done);
                } else if self.need == OPEN {
                    self.complete(out, &mut done);
                }
                self.need = 0;
                self.nargs = 0;
                self.gl_port(index, val, out, &mut done);
                return;
            }
            // Commands whose arguments all go to the command's own index
            // (0x02E enable, plane; 0x008 IRIS GL buffer): a repeat of the
            // index mid-command is the next argument, not a new command.
            if index == self.cmd && self.need != 0 && self.need != OPEN && self.need != STREAM
                && self.nargs < self.need
            {
                if (self.nargs as usize) < MAX_ARGS {
                    self.args[self.nargs as usize] = val;
                }
                self.nargs += 1;
                if self.nargs >= self.need {
                    self.complete(out, &mut done);
                }
                return;
            }
            if self.need == STREAM {
                self.end_stream(&mut done);
            } else if self.need == OPEN {
                self.complete(out, &mut done);
            } else if self.need != 0 {
                crate::dlog_dev!(crate::devlog::LogModule::Gr2,
                    "HQ2: command {} dropped after {}/{} words", self.cmd, self.nargs, self.need);
            }
            self.restarted = 0;
            self.cmd = index;
            self.need = Self::arg_count(index);
            self.args[0] = val;
            self.nargs = 1;
            if self.need == STREAM {
                self.stream_words = 0;
                self.stream_items = 0;
                // Tile data loaders carry a tile word in the command word too.
                if (X2D_TILE_CHUNK_FIRST..=X2D_TILE_CHUNK_LAST).contains(&index) {
                    self.tile_push(val);
                }
                if index == X2D_PATH_POINTS || index == X2D_PATH_POINTS_ALT {
                    self.line_have_prev = 0;
                }
                if index == X2D_PUT_IMAGE || index == X2D_PUT_IMAGE_SHORT || is_pixel_dma(index) {
                    self.img_hdr[0] = val;
                    self.img_rows_done = 0;
                }
                if index == GE_LOAD_CTX {
                    self.gl_cx_restore_begin(val);
                    if val == 0 {
                        self.need = 0;
                        out.finish(FIN2);
                    }
                }
                if index == GE_LOAD_CTX_EXTRA {
                    // The word count arrives as the first DATA word.
                    self.cx_ext_left = u32::MAX;
                }
                return;
            }
        }
        if self.need != OPEN && self.nargs >= self.need {
            self.complete(out, &mut done);
        }
    }

    /// A raster-position token with no operand yet is the short form: complete it (colour latch only).
    fn end_short_raster_pos(&mut self, out: &mut dyn Re3Sink, done: &mut Option<&mut dyn FnMut(String)>) {
        if self.cmd == gl::T_SET_RASTER_POS && self.need == 4 && self.nargs == 1 {
            self.complete(out, done);
        }
    }

    fn tile_push(&mut self, val: u32) {
        if (self.tile_len as usize) < TILE_WORDS {
            self.tile[self.tile_len as usize] = val;
        }
        self.tile_len += 1;
    }

    /// One DATA word for the active streaming command.
    fn stream_word(&mut self, val: u32, out: &mut dyn Re3Sink, done: &mut Option<&mut dyn FnMut(String)>) {
        if self.cmd == GE_LOAD_CTX {
            if self.gl_cx_restore_word(val) {
                self.need = 0;
                out.finish(FIN2);
                if let Some(f) = done.as_mut() {
                    let why = match self.gl_ctx.restore_reject {
                        0 => "restored".to_string(),
                        1 => format!("REJECTED: {} words, image is {}: previous context's state stays live", self.args[0], CX_WORDS),
                        2 => "REJECTED: bad magic: previous context's state stays live".to_string(),
                        _ => "REJECTED: GlState size differs: previous context's state stays live".to_string(),
                    };
                    f(format!("GE_LOAD_CTX {} words {} -> FIN2", self.args[0], why));
                }
            }
            return;
        }
        if self.cmd == GE_LOAD_CTX_EXTRA {
            // The HLE reports no extended context (shram 0x304 = 0); if the
            // kernel restores one anyway, consume its words and ack.
            if self.cx_ext_left == u32::MAX {
                self.cx_ext_left = val;
            } else if self.cx_ext_left > 0 {
                self.cx_ext_left -= 1;
            }
            if self.cx_ext_left == 0 {
                self.need = 0;
                out.finish(FIN2);
            }
            return;
        }
        if (X2D_TILE_CHUNK_FIRST..=X2D_TILE_CHUNK_LAST).contains(&self.cmd) {
            self.tile_push(val);
            return;
        }
        if self.cmd == X2D_PUT_IMAGE || self.cmd == X2D_PUT_IMAGE_SHORT || is_pixel_dma(self.cmd) {
            self.image_word(val, out, done);
            return;
        }
        let group = match self.cmd {
            X2D_SPAN_LIST => 3,
            X2D_PATH_POINTS | X2D_PATH_POINTS_ALT | X2D_DOT_LIST => 2,
            X2D_SEGMENT_LIST | X2D_LINE_QUADS | X2D_STIPPLE_SPANS_NARROW | X2D_STIPPLE_SPANS_32 => 4,
            _ => 0,
        };
        if group != 0 {
            let k = self.stream_words as usize;
            self.args[1 + k] = val;
            self.stream_words += 1;
            if self.stream_words as usize == group {
                self.stream_words = 0;
                self.stream_items += 1;
                let a = self.args;
                let stippled = matches!(self.cmd, X2D_STIPPLE_SPANS_NARROW | X2D_STIPPLE_SPANS_32);
                let fg = if stippled { self.s2d.color_on } else { self.s2d.fg };
                if self.stream_items == 1 {
                    self.setup2d(fg, out);
                }
                match self.cmd {
                    X2D_SPAN_LIST => {
                        if a[1] < PAD_X && a[2] < PAD_Y {
                            let (x, y, w) = (a[1] as i32, a[2] as i32, a[3] as i32);
                            let (x1, x2) = (x.max(0), x.saturating_add(w).min(re3::FB_W as i32));
                            if x1 < x2 {
                                self.span(x1, SCREEN_H.wrapping_sub(1).wrapping_sub(y), (x2 - x1) as u32, None, out);
                            }
                        }
                    }
                    X2D_STIPPLE_SPANS_NARROW | X2D_STIPPLE_SPANS_32 => {
                        self.stippled_span(a[1] as i32, a[2] as i32, a[3] as i32, a[4], out);
                        if let Some(f) = done.as_mut() {
                            f(format!("2D_STIPPLE_SPANS ({}, {}) w={} pattern={:#010x} opaque={} on={:#x} off={:#x}",
                                a[1] as i32, a[2] as i32, a[3] as i32, a[4], self.stipple_opaque, self.s2d.color_on, self.s2d.color_off));
                        }
                    }
                    X2D_DOT_LIST => {
                        if !(a[1] == PAD_X && a[2] == PAD_Y) {
                            let p = [a[1] as i32, a[2] as i32];
                            self.line2d(p, p, true, out);
                        }
                    }
                    X2D_SEGMENT_LIST | X2D_LINE_QUADS => {
                        // Padding: (1280, 1024) pairs (SEGMENT_LIST) or x1 = x2
                        // = 1280 with y 0 (LINE_QUADS, 4Dwm frames).
                        let pad = (a[1] == PAD_X && a[2] == PAD_Y) || (a[1] == PAD_X && a[3] == PAD_X);
                        if !pad {
                            // SEGMENT_LIST draws both endpoints; LINE_QUADS is the
                            // CapNotLast form and stops before (x2, y2)
                            // (Xsgi's segment draw picks it by cap style and swaps
                            // reversed segments with +1 to keep the pixels).
                            let last = self.cmd == X2D_SEGMENT_LIST;
                            self.line2d([a[1] as i32, a[2] as i32], [a[3] as i32, a[4] as i32], last, out);
                        }
                        if let Some(f) = done.as_mut() {
                            f(format!("{} ({}, {})-({}, {}){} fg={:#x}", token_name(self.cmd).unwrap_or("?"),
                                a[1] as i32, a[2] as i32, a[3] as i32, a[4] as i32, if pad { " pad" } else { "" }, self.s2d.fg));
                        }
                    }
                    _ => {
                        let p = [a[1] as i32, a[2] as i32];
                        if a[1] == PAD_X && a[2] == PAD_Y {
                            self.line_have_prev = 0;
                        } else if self.line_have_prev != 0 {
                            // Each segment stops before its end point, so
                            // joints are drawn once and the final point is
                            // not drawn: Xsgi's line draw sends an extra (x + 1, y)
                            // point when the cap style wants it.
                            self.line2d(self.line_prev, p, false, out);
                            self.line_prev = p;
                        } else {
                            self.line_prev = p;
                            self.line_have_prev = 1;
                        }
                    }
                }
            }
            return;
        }
        let k = self.stream_words as usize;
        self.args[1 + k] = val;
        self.stream_words += 1;
        if self.cmd == X2D_FILL_BOXES && self.stream_words == 4 {
            self.stream_words = 0;
            self.stream_items += 1;
            let b = [self.args[1], self.args[2], self.args[3], self.args[4]].map(|v| v as i32);
            self.rect2d(b[0], b[1], b[2], b[3], out);
            if let Some(f) = done.as_mut() {
                let s = self.s2d;
                f(format!("2D_FILL_BOXES box {} ({}, {})-({}, {}) mode={:#x} fg={:#x} pm={:#x} aux={:#x} alu={} cid={:#x}",
                    self.stream_items, b[0], b[1], b[2], b[3], s.mode, s.fg, s.planemask, s.aux_mask, s.alu, s.cid_write));
            }
        }
    }

    fn end_stream(&mut self, done: &mut Option<&mut dyn FnMut(String)>) {
        if self.cmd == X2D_PATH_POINTS || self.cmd == X2D_PATH_POINTS_ALT {
            self.line_have_prev = 0;
        }
        let leftover = if self.cmd == X2D_PUT_IMAGE || self.cmd == X2D_PUT_IMAGE_SHORT {
            self.img_rows_done < self.img_hdr[3]
        } else { self.stream_words != 0 };
        if leftover {
            if let Some(f) = done.as_mut() {
                f(format!("{} ended with {} stray words", token_name(self.cmd).unwrap_or("?"), self.stream_words));
            }
        }
        self.need = 0;
        self.nargs = 0;
        self.stream_words = 0;
    }

    /// 2D solid rectangle: X box (x2/y2 exclusive, y down from the top of
    /// the screen) into the planes selected by MODE / CID_FIELD.
    fn rect2d(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, out: &mut dyn Re3Sink) {
        let (x1, x2) = (x1.max(0), x2.min(re3::FB_W as i32));
        let (y1, y2) = (y1.max(0), y2.min(SCREEN_H));
        if x1 >= x2 || y1 >= y2 {
            return;
        }
        let fg = self.s2d.fg;
        self.setup2d(fg, out);
        for y in y1..y2 {
            self.span(x1, SCREEN_H.wrapping_sub(1).wrapping_sub(y), (x2 - x1) as u32, None, out);
        }
    }

    /// Set the value the next spans write, for the active plane group:
    /// CID (from CID_FIELD, fixed), 4-bit overlay (MODE 8), 2-bit overlay or
    /// popup (MODE 0xB), or the colour planes. Used once per primitive by
    /// `setup2d` and again for every colour run of tiles/images/stipples.
    fn value2d(&self, v: u32, out: &mut dyn Re3Sink) {
        let s = self.s2d;
        if s.cid_write & 0xf000 != 0 {
            return;
        }
        match s.mode & 0xff {
            8 => out.reg(re3::RE3_AUX_VALUE, v & 0xf),
            0xb => out.reg(re3::RE3_AUX_VALUE, if s.aux_mask & 0xc != 0 { (v << 2) & 0xc } else { v & 3 }),
            _ => self.color2d(v, out),
        }
    }

    /// Set the RE3 colour for a GC pixel (fg, BIT_ONE_COLOR / OFF; colour planes:
    /// R in bits 7:0). In 12-bit mode (MODE 2) the pixel is the X visual's
    /// 12-bit value (R 3:0, G 7:4, B 11:8): the DDX passes GC pixels
    /// unconverted (Xsgi's point draw), so widen it here (inferred).
    fn color2d(&self, pixel: u32, out: &mut dyn Re3Sink) {
        if self.s2d.mode & 0xff == 2 {
            let wide = |n: u32| (n & 0xf) * 17;
            return self.rgb2d(wide(pixel) | (wide(pixel >> 4) << 8) | (wide(pixel >> 8) << 16), out);
        }
        self.rgb2d(pixel, out);
    }

    /// Colour of an image / tile pixel word: in 12-bit mode the DDX has
    /// already widened it to 8:8:8 (the 12-bit image draw, tile fill: the
    /// nibbles in the high halves), which the RE3 12-bit packer takes as is.
    fn image2d(&self, pixel: u32, out: &mut dyn Re3Sink) {
        if self.s2d.mode & 0xff == 2 && self.s2d.cid_write & 0xf000 == 0 {
            return self.rgb2d(pixel, out);
        }
        self.value2d(pixel, out);
    }

    /// RE3 colour registers from an 8:8:8 value (R in bits 7:0).
    fn rgb2d(&self, pixel: u32, out: &mut dyn Re3Sink) {
        out.reg(re3::RE3_RED_START, (pixel & 0xff) << 11);
        out.reg(re3::RE3_GREEN_START, ((pixel >> 8) & 0xff) << 11);
        out.reg(re3::RE3_BLUE_START, ((pixel >> 16) & 0xff) << 11);
    }

    /// 1-bpp bitmap (X2D_BITMAP_BLOCK_* and X2D_GLYPH_*), transparent: fg where a bit
    /// is 1. Header x; y, width, rows; `words` data words follow. Rows are
    /// MSB-leftmost; width <= 16 packs two 16-bit rows per word (first row
    /// high), <= 32 one row per word, wider two words per row.
    fn mono_image(&mut self, words: usize, out: &mut dyn Re3Sink) {
        let a = self.args;
        let (x, y, w) = (a[0] as i32, a[1] as i32, a[2].min(64));
        let cap = if w <= 16 { words * 2 } else if w <= 32 { words } else { words / 2 };
        let rows = (a[3] as usize).min(cap);
        let on = self.s2d.color_on;
        self.setup2d(on, out);
        for r in 0..rows {
            let gy = SCREEN_H.wrapping_sub(1).wrapping_sub(y.wrapping_add(r as i32));
            let word = |i: usize| a.get(4 + i).copied().unwrap_or(0);
            // Up to two 32-pixel halves per row.
            let halves: [u32; 2] = if w <= 16 {
                let wd = word(r / 2);
                [if r % 2 == 0 { wd & 0xffff_0000 } else { wd << 16 }, 0]
            } else if w <= 32 {
                [word(r), 0]
            } else {
                [word(2 * r), word(2 * r + 1)]
            };
            for (h, &bits) in halves.iter().enumerate() {
                let hw = w.saturating_sub(32 * h as u32).min(32);
                if hw == 0 {
                    break;
                }
                let pat = if hw < 32 { bits & !(u32::MAX >> hw) } else { bits };
                if pat != 0 {
                    self.span(x.wrapping_add(32 * h as i32), gy, hw, Some(pat), out);
                }
            }
        }
    }

    /// One PUT_IMAGE data word: 6 header words after x, then pixel rows;
    /// anything after the last row (0xDEADBEEF padding) is ignored.
    fn image_word(&mut self, val: u32, out: &mut dyn Re3Sink, done: &mut Option<&mut dyn FnMut(String)>) {
        let k = self.stream_words as usize;
        self.stream_words += 1;
        if k < 6 {
            self.img_hdr[1 + k] = val;
            if k == 5 {
                if is_pixel_dma(self.cmd) {
                    // DMA header is y, width, height, words/row, flag, 0:
                    // derive the pixel format from pixels per word.
                    let (w, wpr) = (self.img_hdr[2], self.img_hdr[4].max(1));
                    let per_word = w.div_ceil(wpr);
                    self.img_hdr[5] = match per_word { 4 => 2, 2 => 1, _ => 0 };
                    self.img_hdr[6] = 0;
                }
                if self.cmd == PIXDMA_TO_SCREEN || !is_pixel_dma(self.cmd) {
                    let fg = self.s2d.fg;
                    self.setup2d(fg, out);
                }
            }
            return;
        }
        let [x, y, w, rows, wpr, fmt, skip] = self.img_hdr;
        let wpr = wpr.max(1) as usize;
        if self.img_rows_done >= rows {
            return; // padding
        }
        let i = (k - 6) % wpr;
        if i < IMG_ROW_WORDS {
            self.img_row[i] = val;
        }
        if i + 1 == wpr {
            let row = self.img_rows_done;
            if self.cmd == GLDMA_TO_WINDOW || self.cmd == GLDMA_TO_WINDOW_ZOOM {
                let words = self.img_row;
                let zoom = if self.cmd == GLDMA_TO_WINDOW_ZOOM { gl::PixZoom::Both } else { gl::PixZoom::None };
                self.gl_pixel_row(x as i32, y as i32, row, w, rows, fmt, &words[..wpr.min(IMG_ROW_WORDS)], zoom, out);
            } else {
                self.image_row(x as i32, (y as i32).wrapping_add(row as i32), w, fmt, skip, wpr.min(IMG_ROW_WORDS), out);
            }
            self.img_rows_done += 1;
            if self.img_rows_done == rows {
                if is_pixel_dma(self.cmd) {
                    // The kernel polls FIN2 for completion of the transfer.
                    out.finish(FIN2);
                }
                if let Some(f) = done.as_mut() {
                    f(format!("{} at ({}, {}) {}x{} words/row={} format={} skip={} fg={:#x} mode={:#x}",
                        token_name(self.cmd).unwrap_or("?"), x as i32, y as i32, w, rows, wpr, fmt, skip,
                        self.s2d.fg, self.s2d.mode));
                }
            }
        }
    }

    /// Draw one image row as runs of equal pixels. Format 2 = 8-bit pixels
    /// (4 per word, MSB first; seen from the DDX). 1 = 16-bit and 0 = 32-bit
    /// pixels are (unverified) guesses.
    fn image_row(&mut self, x: i32, y: i32, w: u32, fmt: u32, skip: u32, wpr: usize, out: &mut dyn Re3Sink) {
        if y < 0 || y >= SCREEN_H {
            return;
        }
        let row = self.img_row;
        let (per_word, bits) = match fmt { 2 => (4usize, 8u32), 1 => (2, 16), _ => (1, 32) };
        let px = |n: usize| -> u32 {
            let wi = n / per_word;
            if wi >= wpr { return 0; }
            let sh = 32 - bits * (1 + (n % per_word) as u32);
            if bits == 32 { row[wi] } else { (row[wi] >> sh) & ((1 << bits) - 1) }
        };
        let gy = SCREEN_H.wrapping_sub(1).wrapping_sub(y);
        // A row is as wide as 16-bit coordinates reach: more would only walk what the clip rejects.
        let w = w.min(IMAGE_ROW_MAX);
        let (start, end) = (skip as usize, skip as usize + w as usize);
        let mut n = start;
        while n < end {
            let c = px(n);
            let mut e = n + 1;
            while e < end && px(e) == c {
                e += 1;
            }
            let sx = x.wrapping_add((n - start) as i32);
            if sx < re3::FB_W as i32 && sx.wrapping_add((e - n) as i32) > 0 {
                self.image2d(c, out);
                self.span(sx, gy, (e - n) as u32, None, out);
            }
            n = e;
        }
    }

    /// Zero-width line with Bresenham, clipped to the line clip box and the
    /// screen, emitted as horizontal runs. The start point is always drawn;
    /// `last` also draws the end point.
    fn line2d(&mut self, p0: [i32; 2], p1: [i32; 2], last: bool, out: &mut dyn Re3Sink) {
        let c = self.line_clip;
        let (cx1, cx2) = (c[0].max(0), c[1].min(re3::FB_W as i32 - 1));
        let (cy1, cy2) = (c[2].max(0), c[3].min(SCREEN_H - 1));
        let (mut x, mut y) = (p0[0], p0[1]);
        // X coordinates are 16 bits, so no real segment spans more than 0xffff. A longer one is a
        // guest's garbage: walking it overflows `2 * err` and takes billions of steps.
        let (sdx, sdy) = ((p1[0] as i64 - x as i64).abs(), (p1[1] as i64 - y as i64).abs());
        if sdx > LINE_SPAN_MAX || sdy > LINE_SPAN_MAX {
            return;
        }
        let (dx, dy) = (sdx as i32, -(sdy as i32));
        let (sx, sy) = (if p1[0] >= x { 1 } else { -1 }, if p1[1] >= y { 1 } else { -1 });
        let mut err = dx + dy;
        let mut run: Option<(i32, i32, i32)> = None; // (y, xmin, xmax)
        loop {
            let end = x == p1[0] && y == p1[1];
            if end && !last {
                break;
            }
            if x >= cx1 && x <= cx2 && y >= cy1 && y <= cy2 {
                run = match run {
                    Some((ry, a, b)) if ry == y && (x == b + 1 || x == a - 1) => Some((ry, a.min(x), b.max(x))),
                    Some((ry, a, b)) => {
                        self.span(a, SCREEN_H.wrapping_sub(1).wrapping_sub(ry), (b - a + 1) as u32, None, out);
                        Some((y, x, x))
                    }
                    None => Some((y, x, x)),
                };
            }
            if end {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
        if let Some((ry, a, b)) = run {
            self.span(a, SCREEN_H.wrapping_sub(1).wrapping_sub(ry), (b - a + 1) as u32, None, out);
        }
    }

    /// Stippled box (X2D_STIPPLED_BOX): 1-bit tile anchored at the origin; bits
    /// set draw fg, clear bits draw bg when STIPPLE_OPAQUE is 1 (opaque).
    /// One stippled span: `w` pixels from (x, y) (y top-down); `pat` MSB is
    /// pixel x and repeats every 32 pixels. The colour currently loaded is
    /// BIT_ONE_COLOR (setup2d at the start of the stream).
    fn stippled_span(&mut self, x: i32, y: i32, w: i32, pat: u32, out: &mut dyn Re3Sink) {
        if y < 0 || y >= SCREEN_H || w <= 0 || x >= re3::FB_W as i32 {
            return;
        }
        let w = w.min(IMAGE_ROW_MAX as i32);
        let gy = SCREEN_H.wrapping_sub(1).wrapping_sub(y);
        let (on, off, opaque) = (self.s2d.color_on, self.s2d.color_off, self.stipple_opaque != 0);
        if opaque {
            // Background first, then the set bits on top.
            self.value2d(off, out);
            self.span(x, gy, w as u32, None, out);
            self.value2d(on, out);
        }
        let mut i = 0;
        while i < w {
            let n = (w - i).min(32);
            let p = if n < 32 { pat & !(u32::MAX >> n) } else { pat };
            if p != 0 {
                self.span(x.wrapping_add(i), gy, n as u32, Some(p), out);
            }
            i += 32;
        }
    }

    fn stipple_rect(&mut self, ox: i32, oy: i32, b: [i32; 4], out: &mut dyn Re3Sink) {
        let (x1, x2) = (b[0].max(0), b[2].min(re3::FB_W as i32));
        let (y1, y2) = (b[1].max(0), b[3].min(SCREEN_H));
        let (tw, th) = (self.tile_w.max(1) as i32, self.tile_h.max(1) as i32);
        if x1 >= x2 || y1 >= y2 {
            return;
        }
        let stride = ((tw + 31) / 32) as usize;
        let tile = self.tile;
        let bit = |tx: i32, ty: i32| -> bool {
            let i = ty as usize * stride + tx as usize / 32;
            let wd = if i < TILE_WORDS { tile[i] } else { 0 };
            (wd >> (31 - tx % 32)) & 1 != 0
        };
        let (fg, bg, opaque) = (self.s2d.color_on, self.s2d.color_off, self.stipple_opaque != 0);
        self.setup2d(fg, out);
        let mut cur = fg;
        for y in y1..y2 {
            let ty = y.wrapping_sub(oy).rem_euclid(th);
            let gy = SCREEN_H.wrapping_sub(1).wrapping_sub(y);
            let mut x = x1;
            while x < x2 {
                let on = bit(x.wrapping_sub(ox).rem_euclid(tw), ty);
                let mut e = x + 1;
                while e < x2 && bit(e.wrapping_sub(ox).rem_euclid(tw), ty) == on {
                    e += 1;
                }
                if on || opaque {
                    let c = if on { fg } else { bg };
                    if c != cur {
                        self.value2d(c, out);
                        cur = c;
                    }
                    self.span(x, gy, (e - x) as u32, None, out);
                }
                x = e;
            }
        }
    }

    /// Tiled fill of one X box. The tile is 8-bit pixels, 4 per word, MSB
    /// first, rows of ceil(width/4) words; the pattern is anchored at the
    /// origin. (Format inferred from the root-window trace: (unverified).)
    fn tile_rect(&mut self, ox: i32, oy: i32, b: [i32; 4], out: &mut dyn Re3Sink) {
        let (x1, x2) = (b[0].max(0), b[2].min(re3::FB_W as i32));
        let (y1, y2) = (b[1].max(0), b[3].min(SCREEN_H));
        let (tw, th) = (self.tile_w.max(1) as i32, self.tile_h.max(1) as i32);
        if x1 >= x2 || y1 >= y2 {
            return;
        }
        // Pixel format (TILE_BEGIN word 3, tile fill): 2 = 8-bit (4 per
        // word), 1 = 16-bit (2 per word), 0 = 24/32-bit (1 per word); MSB
        // first; rows are ceil(width * bpp / 32) words.
        let per_word: i32 = match self.tile_fmt { 2 => 4, 1 => 2, _ => 1 };
        let bits = 32 / per_word as u32;
        let stride = ((tw + per_word - 1) / per_word) as usize;
        let tile = self.tile;
        let tile_px = |tx: i32, ty: i32| -> u32 {
            let i = ty as usize * stride + (tx / per_word) as usize;
            let wd = if i < TILE_WORDS { tile[i] } else { 0 };
            if bits == 32 { wd } else { (wd >> (32 - bits * (1 + (tx % per_word) as u32))) & ((1 << bits) - 1) }
        };
        let fg = self.s2d.fg;
        self.setup2d(fg, out);
        for y in y1..y2 {
            let ty = y.wrapping_sub(oy).rem_euclid(th);
            let gy = SCREEN_H.wrapping_sub(1).wrapping_sub(y);
            // Emit runs of equal colour.
            let mut x = x1;
            while x < x2 {
                let c = tile_px(x.wrapping_sub(ox).rem_euclid(tw), ty);
                let mut e = x + 1;
                while e < x2 && tile_px(e.wrapping_sub(ox).rem_euclid(tw), ty) == c {
                    e += 1;
                }
                self.image2d(c, out);
                self.span(x, gy, (e - x) as u32, None, out);
                x = e;
            }
        }
    }

    /// RE3 state for a 2D primitive: raster op, colour and the plane group
    /// selected by MODE / CID_FIELD.
    fn setup2d(&mut self, fg: u32, out: &mut dyn Re3Sink) {
        let s = self.s2d;
        out.reg(re3::RE3_LOGIC_OP, s.alu & 0xf);
        self.color2d(fg, out);
        out.reg(re3::RE3_SKIP_POPUP, 1);
        if s.cid_write & 0xf000 != 0 {
            // CID planes (bits 31:28) only.
            out.reg(re3::RE3_WID_VALUE, (s.cid_write >> 8) & 0xf);
            out.reg(re3::RE3_AUX_VALUE, 0);
            out.reg(re3::RE3_AUX_PLANE_MASK, 0xf0);
            out.reg(re3::RE3_COLOR_PLANE_MASK, 0);
        } else {
            match s.mode & 0xff {
                // 4-bit overlay (MODE 8) and 2-bit overlay / popup (MODE 0xB):
                // mask AUX_PLANE_MASK, value set by value2d (popup value shifted
                // up, inferred).
                8 | 0xb => {
                    out.reg(re3::RE3_AUX_PLANE_MASK, s.aux_mask & 0xf);
                    out.reg(re3::RE3_COLOR_PLANE_MASK, 0);
                    self.value2d(fg, out);
                }
                // 12-bit RGB (MODE 2, 12-bit TrueColor windows): the RE3
                // writes the value to both 12-bit buffers and the plane mask
                // picks one. The buffers never move; ROP flag bit 3
                // (the window's dbc buffer * 8) names buffer 1. A mask the
                // DDX already moved or doubled (dbc -3 / -1, tile fill)
                // is used as it is.
                2 => {
                    let pm = s.planemask & 0x00ff_ffff;
                    let mask = if s.flag & 8 == 0 {
                        pm & 0xfff
                    } else if pm & 0xfff000 != 0 {
                        pm
                    } else {
                        pm << 12
                    };
                    out.reg(re3::RE3_AUX_VALUE, 0);
                    out.reg(re3::RE3_AUX_PLANE_MASK, 0);
                    out.reg(re3::RE3_COLOR_PLANE_MASK, mask);
                    out.op(re3::RE3_OP_PIXFMT, re3::PIXFMT_RGB12 as u64);
                    out.reg(re3::RE3_DITHER_ON, 0);
                }
                // Colour planes.
                _ => {
                    out.reg(re3::RE3_AUX_VALUE, 0);
                    out.reg(re3::RE3_AUX_PLANE_MASK, 0);
                    out.reg(re3::RE3_COLOR_PLANE_MASK, s.planemask & 0x00ff_ffff);
                    out.op(re3::RE3_OP_PIXFMT, 0);
                }
            }
        }
    }

    fn complete(&mut self, out: &mut dyn Re3Sink, done: &mut Option<&mut dyn FnMut(String)>) {
        let cmd = self.cmd;
        self.last_nargs = self.nargs.min(MAX_ARGS as u32);
        self.last_total = self.nargs;
        self.need = 0;
        self.nargs = 0;
        self.execute(cmd, out);
        if let Some(f) = done.as_mut() {
            f(self.describe(cmd));
        }
    }

    /// Decoded form of the command that just completed (for traces).
    pub fn describe(&self, cmd: u32) -> String {
        let a = &self.args[..self.last_nargs as usize];
        let name = index_label(cmd);
        let i = |k: usize| a.get(k).copied().unwrap_or(0) as i32;
        match cmd {
            PUC_INIT => format!("{name} arg={}", i(0)),
            HQ_TOKEN_UNSTALL => format!("microcode start arg={} -> FIN2", i(0)),
            GE_CTX_SWITCH => {
                let prev = self.gl_ctx.prev_owner;
                let live = if prev == u32::MAX { "none".to_string() } else { format!("{prev:#x}") };
                // State 1 = the kernel believes this context's state is still
                // in the GE: no restore follows. If the HLE's live state
                // belongs to another context, that one's state leaks.
                let leak = if i(1) == 1 && prev != u32::MAX && prev != a[0] { "  LEAK? state 1 but live state is another context's" } else { "" };
                format!("{name} ctx={:#x} state={} mode={} (live was {live}){leak} -> FIN2", i(0), i(1), i(2))
            }
            HQ_PCX_1E0 | HQ_PCX_1E6 => format!("{name} {} -> FIN2", i(0)),
            HQ_GL_FIN3 | CMD_READBACK_KICK => format!("{name} {} -> FIN3", i(0)),
            X2D_FG_ALU => format!("{name} fg={:#x} planemask={:#x} alu={} flag={:#x}", a[0], a[1], i(2), a[3]),
            X2D_TILE_BEGIN => format!("{name} size={:#x} {}x{} format={} {:#x}", a[0], i(1), i(2), i(3), a[4]),
            X2D_SCREEN_COPY => format!("{name} pitch={} chunk={} src=({}, {}) {}x{} dst=({}, {})",
                i(0), i(1), i(2), i(3), i(4), i(5), i(6), i(7)),
            X2D_BITMAP_BLOCK_A..=X2D_BITMAP_BLOCK_D | X2D_GLYPH_NARROW..=X2D_GLYPH_WIDE =>
                format!("{name} at ({}, {}) {}x{} on={:#x}", i(0), i(1), i(2), i(3), self.s2d.color_on),
            X2D_LINE_BOUNDS => format!("{name} x {}..{} y {}..{}", i(0), i(1), i(2), i(3)),
            X2D_LINE_STYLE | X2D_STIPPLE_OPAQUE => format!("{name} {:#x}", a[0]),
            X2D_TILE_FILL | X2D_TILE_FILL_RAGGED => format!("{name} ({}, {})-({}, {}) origin ({}, {}) tile {}x{} format={} ({} words)",
                i(2), i(4), i(5), i(6), i(1), i(3), self.tile_w, self.tile_h, self.tile_fmt, self.tile_len),
            X2D_STIPPLED_BOX | X2D_STIPFILL_POW2 | X2D_STIPFILL_OTHER => format!("{name} ({}, {})-({}, {}) origin ({}, {}) stipple {}x{} opaque={} fg={:#x} bg={:#x}",
                i(2), i(4), i(5), i(6), i(1), i(3), self.tile_w, self.tile_h, self.stipple_opaque, self.s2d.color_on, self.s2d.color_off),
            X2D_PLANE_SELECT | X2D_AUX_PLANE_MASK | X2D_BIT_ZERO_COLOR | X2D_BIT_ONE_COLOR | X2D_CID_FIELD
            | X2D_SESSION_START | X2D_PRIM_TERMINATOR => format!("{name} {:#x}", a[0]),
            X2D_PIXEL_LAYOUT => format!("{name} format={} offset={}", i(0), i(1)),
            PIXDMA_FROM_SCREEN => format!("{name} ({}, {}) {}x{} words/row={} flag={} format={} offset={} mode={:#x} -> GEDMA, FIN2",
                i(0), i(1), i(2), i(3), i(4), i(5), self.s2d.buf_select, self.s2d.buf_offset, self.s2d.mode),
            GLDMA_FROM_WINDOW => format!("{name} ({}, {}) {}x{} words/row={} flag={} {} -> GEDMA, FIN2",
                i(0), i(1), i(2), i(3), i(4), i(5), self.gl_read_desc()),
            X2D_IMAGE_TO_SHRAM => format!("{name} ({}, {}) {}x{} words/row={} format={} offset={} mode={:#x} -> shram[{:#x}], FIN3",
                i(1), i(2), i(3), i(4), i(0), self.s2d.buf_select, self.s2d.buf_offset, self.s2d.mode, READ_IMAGE_SHRAM),
            PUC_COLOR => format!("{name} ci={}", i(0)),
            PUC_PNT2I | PUC_CMOV2I => format!("{name} ({}, {})", i(0), i(1)),
            PUC_RECTI2D => format!("{name} ({}, {})-({}, {})", i(0), i(1), i(2), i(3)),
            PUC_DRAWCHAR => {
                let rows: Vec<String> = a.iter().skip(7).take(i(1).clamp(0, 18) as usize)
                    .map(|r| format!("{r:04x}")).collect();
                format!("{name} {}x{} mode={} orig=({}, {}) move=({}, {}) raster now ({}, {}) rows=[{}]",
                    i(0), i(1), i(2), i(3), i(4), i(5), i(6), self.cx, self.cy, rows.join(" "))
            }
            PUC_RECTCOPY => format!("{name} src=({}, {}) {}x{} dst=({}, {}) [X-style y] word_len={} max_lines={}",
                i(2), i(3), i(4), i(5), i(6), i(7), i(0), i(1)),
            _ if self.gl_describe(cmd).is_some() => self.gl_describe(cmd).unwrap_or_default(),
            _ => {
                let words: Vec<String> = a.iter().map(|w| format!("{w:#x}")).collect();
                let more = self.last_total.saturating_sub(a.len() as u32);
                let more = if more > 0 { format!(" +{more} more") } else { String::new() };
                format!("{name} [{}{more}] ({} words, not implemented)", words.join(", "), self.last_total)
            }
        }
    }

    /// Current interpreter state, for the monitor.
    pub fn summary(&self) -> String {
        format!("pending={} {}/{} words  raster=({}, {})  pattern_on={}",
            if self.need != 0 { token_name(self.cmd).unwrap_or("?").to_string() } else { "-".into() },
            self.nargs, self.need, self.cx, self.cy, self.pattern_on)
    }

    fn execute(&mut self, cmd: u32, out: &mut dyn Re3Sink) {
        let a = self.args;
        match cmd {
            PUC_INIT => self.init(out),
            PUC_COLOR => {
                // The pixel is one byte; OpenBSD repeats the index in all four (rasops).
                out.reg(re3::RE3_RED_START, (a[0] & 0xff) << 11);
                out.reg(re3::RE3_GREEN_START, 0);
                out.reg(re3::RE3_BLUE_START, 0);
            }
            PUC_FINISH => {}
            // Context switch (Gr2PcxSwap): swap the HLE's GL state; the
            // kernel waits for FIN2.
            GE_CTX_SWITCH => {
                self.gl_switch_context(a[0], a[1], out);
                out.finish(FIN2);
            }
            // Save main: hand out the image taken at GE_CTX_SWITCH; the kernel
            // reads it from HQ2_GEDMA after FIN2. Save ext: nothing to save.
            GE_DUMP_CTX => {
                self.gl_cx_save(out);
                out.finish(FIN2);
            }
            GE_DUMP_CTX_EXTRA => {
                out.gedma_out(&[]);
                out.finish(FIN2);
            }
            HQ_PCX_1E0 | HQ_PCX_1E6 => out.finish(FIN2),
            HQ_GL_FIN3 | CMD_READBACK_KICK => out.finish(FIN3),
            // Nothing is buffered beyond the FIFO.
            CMD_SWAP_WAIT => {}
            // First word of the DDX's hardware init: make sure the raster
            // engine is in a known full-screen state for 2D drawing.
            X2D_SESSION_START => {
                for (reg, val) in [
                    (re3::RE3_CLIP_X_LO, (0)),
                    (re3::RE3_CLIP_X_HI, (re3::FB_W as u32 - 1)),
                    (re3::RE3_CLIP_Y_LO, 0),
                    (re3::RE3_CLIP_Y_HI, re3::FB_H as u32 - 1),
                    (re3::RE3_PATTERN_ON, 0),
                    (re3::RE3_WID_TEST_ON, 0),
                    (re3::RE3_HOST_PLANE, re3::HPLANE_COLOR),
                ] {
                    out.reg(reg, val);
                }
                self.pattern_on = 0;
                self.line_clip = [0, re3::FB_W as i32 - 1, 0, SCREEN_H - 1];
            }
            X2D_PRIM_TERMINATOR => {}
            X2D_PLANE_SELECT => self.s2d.mode = a[0],
            X2D_AUX_PLANE_MASK => self.s2d.aux_mask = a[0],
            X2D_BIT_ZERO_COLOR => self.s2d.color_off = a[0],
            X2D_BIT_ONE_COLOR => self.s2d.color_on = a[0],
            X2D_CID_FIELD => self.s2d.cid_write = a[0],
            X2D_PIXEL_LAYOUT => {
                self.s2d.buf_select = a[0];
                self.s2d.buf_offset = a[1];
            }
            X2D_IMAGE_TO_SHRAM => {
                let req = ReadImage {
                    x: a[1] as i32,
                    top: SCREEN_H.wrapping_sub(1).wrapping_sub(a[2] as i32),
                    w: a[3],
                    rows: a[4],
                    words_per_row: a[0],
                    s2d: self.s2d,
                    decode: ReadDecode::Planes2d,
                    bottom_up: false,
                };
                out.read_image(&req, ReadDest::Shram(READ_IMAGE_SHRAM));
            }
            PIXDMA_FROM_SCREEN => {
                let req = ReadImage {
                    x: a[0] as i32,
                    top: SCREEN_H.wrapping_sub(1).wrapping_sub(a[1] as i32),
                    w: a[2],
                    rows: a[3],
                    words_per_row: a[4],
                    s2d: self.s2d,
                    decode: ReadDecode::Planes2d,
                    bottom_up: false,
                };
                out.read_image(&req, ReadDest::Gedma);
            }
            GLDMA_FROM_WINDOW => self.gl_dma_read(&a, out),
            X2D_TILE_BEGIN => {
                // A tile that is not in the tile store reads zeros; its sides are bounded so index sums are.
                self.tile_w = a[1].min(TILE_SIDE_MAX);
                self.tile_h = a[2].min(TILE_SIDE_MAX);
                self.tile_fmt = a[3];
                self.tile_len = 0;
            }
            X2D_BITMAP_BLOCK_A..=X2D_BITMAP_BLOCK_D => {
                self.mono_image(4 << (cmd - X2D_BITMAP_BLOCK_A), out)
            }
            X2D_GLYPH_NARROW..=X2D_GLYPH_WIDE => self.mono_image(8 << (cmd - X2D_GLYPH_NARROW), out),
            X2D_LINE_STYLE => {}
            X2D_LINE_BOUNDS => self.line_clip = [a[0] as i32, a[1] as i32, a[2] as i32, a[3] as i32],
            X2D_STIPPLE_OPAQUE => self.stipple_opaque = a[0] & 1,
            // One box per command: 0; DATA origin x, x1, origin y, y1, x2, y2
            // (same interleaved order as STIPPLED_BOX; tile fill).
            X2D_TILE_FILL | X2D_TILE_FILL_RAGGED => {
                let (ox, oy) = (a[1] as i32, a[3] as i32);
                if self.tile_fmt == 3 {
                    self.stipple_rect(ox, oy, [a[2] as i32, a[4] as i32, a[5] as i32, a[6] as i32], out);
                } else {
                    self.tile_rect(ox, oy, [a[2] as i32, a[4] as i32, a[5] as i32, a[6] as i32], out);
                }
            }
            X2D_STIPPLED_BOX | X2D_STIPFILL_POW2 | X2D_STIPFILL_OTHER => {
                let (ox, oy) = (a[1] as i32, a[3] as i32);
                self.stipple_rect(ox, oy, [a[2] as i32, a[4] as i32, a[5] as i32, a[6] as i32], out);
            }
            X2D_SCREEN_COPY => {
                // X-style y (top-down); RE3 copy takes GL bottom-row y.
                let (sx, sy, w, h, dx, dy) = (a[2] as i32, a[3] as i32, a[4] as i32, a[5] as i32, a[6] as i32, a[7] as i32);
                let fg = self.s2d.fg;
                self.setup2d(fg, out);
                out.copy(sx, SCREEN_H.wrapping_sub(sy).wrapping_sub(h), w, h, dx, SCREEN_H.wrapping_sub(dy).wrapping_sub(h), 0);
            }
            X2D_FG_ALU => {
                self.s2d.fg = a[0];
                self.s2d.planemask = a[1];
                self.s2d.alu = a[2];
                self.s2d.flag = a[3];
            }
            // Microcode reset path after unstall (the kernel's start): the start
            // argument (0/1/2 by board rev) resets drawing state, then FIN2.
            HQ_TOKEN_UNSTALL => {
                self.init(out);
                out.finish(FIN2);
            }
            PUC_PNT2I => self.span(a[0] as i32, a[1] as i32, 1, None, out),
            PUC_RECTI2D => {
                let (x1, y1, x2, y2) = (a[0] as i32, a[1] as i32, a[2] as i32, a[3] as i32);
                let (xl, xr) = (x1.min(x2), x1.max(x2));
                // Rows off the screen draw nothing, so walk only the ones on it.
                let width = (xr as i64 - xl as i64 + 1).min(i32::MAX as i64) as u32;
                for y in y1.min(y2).max(0)..=y1.max(y2).min(SCREEN_H - 1) {
                    self.span(xl, y, width, None, out);
                }
            }
            PUC_CMOV2I => {
                self.cx = a[0] as i32;
                self.cy = a[1] as i32;
            }
            PUC_DRAWCHAR => self.drawchar(out),
            PUC_RECTCOPY => {
                let (sx, sy, w, h, dx, dy) =
                    (a[2] as i32, a[3] as i32, a[4] as i32, a[5] as i32, a[6] as i32, a[7] as i32);
                // RECTCOPY takes X-style y (top edge, top-down); VRAM is bottom-up.
                out.copy(sx, SCREEN_H.wrapping_sub(sy).wrapping_sub(h), w, h, dx, SCREEN_H.wrapping_sub(dy).wrapping_sub(h), 0);
            }
            _ if self.gl_execute(cmd, out) => {}
            _ => {
                let i = cmd as usize & (FIFO_WORDS - 1);
                if self.warned[i / 64] & (1 << (i % 64)) == 0 {
                    self.warned[i / 64] |= 1 << (i % 64);
                    crate::dlog_dev!(crate::devlog::LogModule::Gr2,
                        "HQ2: unimplemented command {} ({:#x}) data {:#010x}", cmd, cmd, a[0]);
                }
            }
        }
    }

    /// PUC_INIT: put the RE3 into the textport's drawing state.
    fn init(&mut self, out: &mut dyn Re3Sink) {
        self.cx = 0;
        self.cy = 0;
        self.pattern_on = 0;
        for (reg, val) in [
            (re3::RE3_LOGIC_OP, re3::ROP_COPY),
            (re3::RE3_COLOR_WRITE_ON, 0),
            (re3::RE3_SKIP_POPUP, 0),
            (re3::RE3_POPUP_VALUE, 0),
            (re3::RE3_COLOR_PLANE_MASK, 0x00ff_ffff),
            (re3::RE3_AUX_PLANE_MASK, 0),
            (re3::RE3_WID_VALUE, 0),
            (re3::RE3_AUX_VALUE, 0),
            (re3::RE3_HOST_PLANE, re3::HPLANE_COLOR),
            (re3::RE3_PATTERN_ON, 0),
            (re3::RE3_PATTERN_ANCHOR, 0),
            (re3::RE3_WID_TEST_ON, 0),
            (re3::RE3_CLIP_X_LO, (0)),
            (re3::RE3_CLIP_X_HI, (re3::FB_W as u32 - 1)),
            (re3::RE3_CLIP_Y_LO, 0),
            (re3::RE3_CLIP_Y_HI, re3::FB_H as u32 - 1),
            (re3::RE3_X_STEP, 0),
            (re3::RE3_Y_STEP, 0),
            (re3::RE3_RED_STEP, 0),
            (re3::RE3_GREEN_STEP, 0),
            (re3::RE3_BLUE_STEP, 0),
            (re3::RE3_RED_START, 0),
            (re3::RE3_GREEN_START, 0),
            (re3::RE3_BLUE_START, 0),
        ] {
            out.reg(reg, val);
        }
    }

    /// One horizontal flat span, optionally masked by a 32-bit pattern
    /// (bit 31 = leftmost pixel).
    fn span(&mut self, x: i32, y: i32, n: u32, pattern: Option<u32>, out: &mut dyn Re3Sink) {
        let (mut x, mut n, mut pat) = (x, n as i32, pattern.unwrap_or(0));
        if x < 0 {
            // Bank-coded X can't express negative columns: trim the span.
            n = n.saturating_add(x);
            if pattern.is_some() {
                pat = if x <= -32 { 0 } else { pat << -x };
            }
            x = 0;
        }
        if n <= 0 || y < 0 || y >= re3::FB_H as i32 || x >= re3::FB_W as i32 {
            return;
        }
        let want_pat = pattern.is_some() as u32;
        if want_pat != self.pattern_on {
            out.reg(re3::RE3_PATTERN_ON, want_pat);
            self.pattern_on = want_pat;
        }
        if pattern.is_some() {
            out.reg(re3::RE3_PATTERN_HI, pat >> 16);
            out.reg(re3::RE3_PATTERN_LO, pat & 0xffff);
        }
        out.reg(re3::RE3_CUR_X, x as u32);
        out.reg(re3::RE3_CUR_Y, y as u32);
        out.reg(re3::RE3_SPAN_LEN, n as u32);
        out.reg(re3::RE3_DRAW_TRIGGER, re3::DRAW_FLAT_5);
    }

    /// PUC_DRAWCHAR: xsize; ysize, mode, xorig, yorig, xmove, ymove,
    /// then 18 rows, bottom row first, MSB = leftmost pixel.
    fn drawchar(&mut self, out: &mut dyn Re3Sink) {
        let a = self.args;
        let xsize = a[0].min(32);
        let ysize = a[1].min(18);
        let mode = a[2];
        let (xorig, yorig) = (a[3] as i32, a[4] as i32);
        let (xmove, ymove) = (a[5] as i32, a[6] as i32);
        let left = self.cx.wrapping_sub(xorig);
        let bottom = self.cy.wrapping_sub(yorig);
        for k in 0..ysize as usize {
            let row = a[7 + k];
            // Narrow glyphs are 16-bit rows in the low halfword whatever the mode
            // (NetBSD and OpenBSD send mode 2); only wide mode 2 rows are 32-bit.
            let mut pat = if mode == 2 && xsize > 16 { row } else { (row & 0xffff) << 16 };
            if xsize < 32 {
                pat &= !(u32::MAX >> xsize); // keep only the leftmost xsize bits
            }
            if pat != 0 {
                self.span(left, bottom + k as i32, xsize, Some(pat), out);
            }
        }
        self.cx = self.cx.wrapping_add(xmove);
        self.cy = self.cy.wrapping_add(ymove);
    }
}

impl Hq2Engine {
    /// The interpreter's state as words: the command being collected and its words so far, the 2D
    /// and GL context, the images being built. The once-per-token warning bitmap is left out.
    pub fn snapshot(&self) -> Vec<u32> {
        let mut w = vec![self.cmd, self.need, self.nargs, self.last_nargs, self.last_total];
        w.extend(self.args);
        w.extend([self.cx as u32, self.cy as u32, self.pattern_on, self.restarted, self.stream_words, self.stream_items]);
        let s = &self.s2d;
        w.extend([s.mode, s.aux_mask, s.fg, s.color_on, s.color_off, s.planemask, s.alu, s.flag, s.cid_write, s.buf_select, s.buf_offset]);
        w.extend([self.tile_w, self.tile_h, self.tile_len, self.tile_fmt]);
        w.extend(self.tile);
        w.push(self.stipple_opaque);
        w.extend(self.line_clip.map(|v| v as u32));
        w.extend(self.line_prev.map(|v| v as u32));
        w.push(self.line_have_prev);
        w.extend(self.img_hdr);
        w.extend(self.img_row);
        w.extend([self.img_rows_done, self.cx_ext_left]);
        w.extend(self.pix);
        w.push(self.pix_n);
        self.gl_snapshot(&mut w);
        w
    }

    /// The inverse of `snapshot`. On an error the engine is part loaded: reset it.
    pub fn restore(&mut self, words: &[u32]) -> Result<(), String> {
        let mut it = words.iter().copied();
        let mut next = move || it.next().ok_or_else(|| "the interpreter state ends early".to_string());
        macro_rules! fill {
            ($dst:expr) => {
                for v in $dst.iter_mut() {
                    *v = next()?;
                }
            };
        }
        let mut head = [0u32; 5];
        fill!(head);
        (self.cmd, self.need, self.nargs, self.last_nargs, self.last_total) = (head[0], head[1], head[2], head[3], head[4]);
        fill!(self.args);
        let mut p = [0u32; 6];
        fill!(p);
        (self.cx, self.cy, self.pattern_on, self.restarted, self.stream_words, self.stream_items) = (p[0] as i32, p[1] as i32, p[2], p[3], p[4], p[5]);
        let mut s = [0u32; 11];
        fill!(s);
        self.s2d = State2d { mode: s[0], aux_mask: s[1], fg: s[2], color_on: s[3], color_off: s[4], planemask: s[5], alu: s[6], flag: s[7], cid_write: s[8], buf_select: s[9], buf_offset: s[10] };
        let mut t = [0u32; 4];
        fill!(t);
        (self.tile_w, self.tile_h, self.tile_len, self.tile_fmt) = (t[0], t[1], t[2], t[3]);
        fill!(self.tile);
        self.stipple_opaque = next()?;
        for v in self.line_clip.iter_mut().chain(self.line_prev.iter_mut()) {
            *v = next()? as i32;
        }
        self.line_have_prev = next()?;
        fill!(self.img_hdr);
        fill!(self.img_row);
        self.img_rows_done = next()?;
        self.cx_ext_left = next()?;
        fill!(self.pix);
        self.pix_n = next()?;
        self.gl_restore(&mut next)?;
        // Words past the end mean another build's layout.
        match next() {
            Ok(_) => Err("the interpreter state has words past its end".into()),
            Err(_) => Ok(()),
        }
    }
}
