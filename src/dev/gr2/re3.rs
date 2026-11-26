//! RE3 raster engine: register state, VRAM and generic (unoptimised) drawing.
//!
//! Register writes arrive through the RE3 FIFO, from the CPU (0x6c200..) or
//! from the HQ2 thread. The RE3 thread applies them in order, and a `DRAW_TRIGGER`
//! write executes. Semantics follow MAME's RE2 model (`sgi_re2.cpp`), the RE3's
//! predecessor.
//!
//! VRAM is one u32 per pixel: `cid[31:28] | aux[27:24] | rgb/ci[23:0]`.
//! Rows are stored bottom-up (y = 0 is the bottom scanline).
//! X registers hold the linear pixel column (not bank-coded).

pub const FB_W: usize = 1280;
pub const FB_H: usize = 1024;

// Buffered registers.
pub const RE3_COLOR_WRITE_ON: usize = 0x04;
pub const RE3_LOGIC_OP: usize = 0x06;
pub const RE3_SKIP_POPUP: usize = 0x08;
/// Subpixel start fraction (4 bits; see load_iterators).
pub const RE3_SUBPIXEL: usize = 0x09;
pub const RE3_COLOR_PACKED: usize = 0x0a;
pub const RE3_XY_PACKED: usize = 0x0b;
pub const RE3_POPUP_VALUE: usize = 0x0c;
pub const RE3_PATTERN_LO: usize = 0x0d;
pub const RE3_PATTERN_HI: usize = 0x0e;
pub const RE3_Z_STEP_INT: usize = 0x0f;
pub const RE3_Z_STEP_FRAC: usize = 0x10;
pub const RE3_RED_STEP: usize = 0x11;
pub const RE3_GREEN_STEP: usize = 0x12;
pub const RE3_BLUE_STEP: usize = 0x13;
pub const RE3_Z_START: usize = 0x14;
pub const RE3_RED_START: usize = 0x15;
pub const RE3_GREEN_START: usize = 0x16;
pub const RE3_BLUE_START: usize = 0x17;
pub const RE3_STIPPLE_BITS: usize = 0x18;
pub const RE3_STIPPLE_RUN: usize = 0x19;
pub const RE3_X_STEP: usize = 0x1a;
pub const RE3_Y_STEP: usize = 0x1b;
pub const RE3_SPAN_LEN: usize = 0x1c;
pub const RE3_CUR_X: usize = 0x1d;
pub const RE3_CUR_Y: usize = 0x1e;
pub const RE3_DRAW_TRIGGER: usize = 0x1f;
// Unbuffered control registers.
pub const RE3_HOST_DATA: usize = 0x20;
pub const RE3_COLOR_PLANE_MASK: usize = 0x21;
pub const RE3_AUX_PLANE_MASK: usize = 0x22;
pub const RE3_WID_VALUE: usize = 0x23;
pub const RE3_AUX_VALUE: usize = 0x24;
pub const RE3_HOST_PLANE: usize = 0x25;
pub const RE3_PATTERN_ANCHOR: usize = 0x29;
pub const RE3_PATTERN_ON: usize = 0x2a;
pub const RE3_DITHER_ON: usize = 0x2c;
pub const RE3_WID_TEST_ON: usize = 0x2d;
pub const RE3_WINDOW_ID: usize = 0x2e;
pub const RE3_Z_COMPARE: usize = 0x2f;
pub const RE3_LINE_WID_TEST_ON: usize = 0x31;
pub const RE3_FB_OPTIONS: usize = 0x32;
pub const RE3_TOP_SCAN: usize = 0x33;
pub const RE3_UNPACK_MODE: usize = 0x38;
pub const RE3_CLIP_Y_LO: usize = 0x39;
pub const RE3_CLIP_Y_HI: usize = 0x3a;
pub const RE3_CLIP_X_LO: usize = 0x3b;
pub const RE3_CLIP_X_HI: usize = 0x3c;

pub const DRAW_SMOOTH: u32 = 1;
pub const DRAW_FLAT_5: u32 = 2;
pub const DRAW_FLAT_20: u32 = 3;
pub const DRAW_LINE_UPPER: u32 = 4;
pub const DRAW_LINE_LOWER: u32 = 5;
pub const DRAW_HOST_READ: u32 = 6;
pub const DRAW_HOST_WRITE: u32 = 7;

pub const HPLANE_COLOR: u32 = 0;
pub const HPLANE_POPUP: u32 = 1;
pub const HPLANE_AUX: u32 = 2;
pub const HPLANE_Z: u32 = 3;
pub const HPLANE_WID: u32 = 4;
pub const HPLANE_COLOR_RAW: u32 = 6;
pub const HPLANE_Z_RAW: u32 = 7;

pub const ROP_COPY: u32 = 3;

/// Register write masks (MAME `sgi_re2.cpp` regmask[]).
pub const REGMASK: [u32; 64] = [
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000001, 0x00000001, 0x0000000f, 0x00000003,
    0x00000001, 0x0000000f, 0x07ffffff, 0x003fffff, 0x00000003, 0x0000ffff, 0x0000ffff, 0x00ffffff,
    0x00003fff, 0x00ffffff, 0x000fffff, 0x000fffff, 0x00ffffff, 0x007fffff, 0x0007ffff, 0x0007ffff,
    0x0000ffff, 0x000000ff, 0x0000ffff, 0x0000ffff, 0x000007ff, 0x0000ffff, 0x000007ff, 0x00000007,
    0xffffffff, 0x00ffffff, 0x000001ff, 0x0000000f, 0x0000000f, 0x00000007, 0x00000001, 0x00000003,
    0x0000003f, 0x00000001, 0x00000001, 0x00000001, 0x00000001, 0x00000001, 0x0000000f, 0x0000000f,
    0x000000ff, 0x00000001, 0x00000003, 0x0003ffff, 0x00000001, 0x00003fff, 0x00000001, 0x000000ff,
    0x00000003, 0x000007ff, 0x000007ff, 0x00000fff, 0x00000fff, 0x00000001, 0x00000001, 0x00000000,
];

/// Register names for the monitor and traces: this crate's own, and MAME's `sgi_re2.cpp` regname[] where unnamed.
pub const REG_NAMES: [&str; 64] = [
    "R00", "R01", "R02", "R03", "COLOR_WRITE_ON", "BIGENDIAN", "LOGIC_OP", "HADDR",
    "SKIP_POPUP", "SUBPIXEL", "COLOR_PACKED", "XY_PACKED", "POPUP_VALUE", "PATTERN_LO", "PATTERN_HI", "Z_STEP_INT",
    "Z_STEP_FRAC", "RED_STEP", "GREEN_STEP", "BLUE_STEP", "Z_START", "RED_START", "GREEN_START", "BLUE_START",
    "STIPPLE_BITS", "STIPPLE_RUN", "X_STEP", "Y_STEP", "SPAN_LEN", "CUR_X", "CUR_Y", "DRAW_TRIGGER",
    "HOST_DATA", "COLOR_PLANE_MASK", "AUX_PLANE_MASK", "WID_VALUE", "AUX_VALUE", "HOST_PLANE", "READBUF", "PIXTYPE",
    "ASELECT", "PATTERN_ANCHOR", "PATTERN_ON", "ENABSTIP", "DITHER_ON", "WID_TEST_ON", "WINDOW_ID", "Z_COMPARE",
    "REPSTIP", "LINE_WID_TEST_ON", "FB_OPTIONS", "TOP_SCAN", "TESTMODE", "TESTDATA", "ZBOPTION", "XZOOM",
    "UNPACK_MODE", "CLIP_Y_LO", "CLIP_Y_HI", "CLIP_X_LO", "CLIP_X_HI", "COLORCMP", "MEGOPTION", "R3F",
];

pub fn draw_op_name(draw_op: u32) -> &'static str {
    match draw_op {
        DRAW_SMOOTH => "SMOOTH",
        DRAW_FLAT_5 => "FLAT_5",
        DRAW_FLAT_20 => "FLAT_20",
        DRAW_LINE_UPPER => "LINE_UPPER",
        DRAW_LINE_LOWER => "LINE_LOWER",
        DRAW_HOST_READ => "HOST_READ",
        DRAW_HOST_WRITE => "HOST_WRITE",
        _ => "DRAW?",
    }
}

pub fn rwmode_name(m: u32) -> &'static str {
    match m {
        HPLANE_COLOR => "COLOR",
        HPLANE_POPUP => "POPUP",
        HPLANE_AUX => "AUX",
        HPLANE_Z => "Z",
        HPLANE_WID => "WID",
        HPLANE_COLOR_RAW => "COLOR_RAW",
        HPLANE_Z_RAW => "Z_RAW",
        _ => "?",
    }
}

/// One-line decode of the primitive a `DRAW_TRIGGER` write is about to execute, from
/// the register file as it stands (before `execute` loads the iterators).
pub fn describe_draw_op(reg: &[u32; 64], draw_op: u32) -> String {
    let x = reg[RE3_CUR_X] as i32;
    let rgb = |r: u32| r >> 11;
    let mut s = format!("{} x={} y={} n={} rgb=({},{},{}) func={} rwmode={}",
        draw_op_name(draw_op), x, reg[RE3_CUR_Y], reg[RE3_SPAN_LEN],
        rgb(reg[RE3_RED_START]), rgb(reg[RE3_GREEN_START]), rgb(reg[RE3_BLUE_START]), reg[RE3_LOGIC_OP], rwmode_name(reg[RE3_HOST_PLANE]));
    if reg[RE3_PATTERN_ON] != 0 {
        s += &format!(" pat={:#010x}{}", (reg[RE3_PATTERN_HI] << 16) | reg[RE3_PATTERN_LO],
            if reg[RE3_PATTERN_ANCHOR] != 0 { "/aligned" } else { "" });
    }
    if draw_op == DRAW_SMOOTH {
        s += &format!(" dx={:#x} dy={:#x} drgb=({:#x},{:#x},{:#x}) z={:#x} dz={:#x}.{:#x}",
            reg[RE3_X_STEP], reg[RE3_Y_STEP], reg[RE3_RED_STEP], reg[RE3_GREEN_STEP], reg[RE3_BLUE_STEP], reg[RE3_Z_START], reg[RE3_Z_STEP_INT], reg[RE3_Z_STEP_FRAC]);
    }
    if reg[RE3_WID_TEST_ON] != 0 {
        s += &format!(" wid={}", reg[RE3_WINDOW_ID]);
    }
    s
}

// X registers (X, XMIN, XMAX, the X half of YX) hold the linear pixel column.
// Guest software never writes RE3 registers directly (only the HQ2/GE
// microcode, which is HLE'd), so this is internal. The bank-coded form
// (x / 5) * 8 + x % 5 from MAME's RE2 model is not used.

// ── Private FIFO operations (emulator-only; what the real HQ2/GE7 microcode
//    did with longer register sequences) ────────────────────────────────────
/// Screen-to-screen copy, first half: val = sx | sy<<16 | w<<32 | h<<48.
pub const RE3_OP_COPY_A: u32 = 0xFFFF_1001;
/// Second half (executes): val = dx | dy<<16. Coordinates are linear, y bottom-up.
pub const RE3_OP_COPY_B: u32 = 0xFFFF_1002;
/// RE3_OP_COPY_B bits 63:32 for a GL copy (glCopyPixels; the microcode's
/// copy, emulated): zoom x (7:0) and y (15:8), integer >= 1; the pixel
/// depth of the visual's buffers / 4 (17:16: 0 = 24-bit, no banks) and the
/// read buffer's bank (18); bit 19 set (nonzero = GL copy). The source
/// pixel is taken from the read bank and replicated into every bank, so
/// COLOR_PLANE_MASK picks the draw buffer, as for drawing.
pub fn copy_mode(zx: u32, zy: u32, bits: u32, bank: u32) -> u32 {
    (zx & 0xff) | ((zy & 0xff) << 8) | (((bits / 4) & 3) << 16) | ((bank & 1) << 18) | 1 << 19
}
/// CPU consumed HOST_DATA during a HOST_READ stream; fetch the next pixel.
pub const RE3_OP_READ_ADVANCE: u32 = 0xFFFF_1003;
/// Emulator-private stand-in for RE3's (unidentified) packing-mode register:
/// the GE sends neutral 8-bit-per-channel colour and RE3 packs each pixel
/// into the window's frame-buffer format. 0 = 24-bit / colour index as the iterators
/// produce it; PIXFMT_RGB12 = 12-bit 4:4:4 RGB (R in bits 3:0) duplicated
/// into both 12-bit buffers, the plane mask selecting the buffer.
pub const RE3_OP_PIXFMT: u32 = 0xFFFF_1004;
pub const PIXFMT_RGB12: u32 = 1;
/// 12-bit colour index: the R iterator (12.11) carries the index, written
/// into both 12-bit banks like PIXFMT_RGB12 (the plane mask picks the buffer).
pub const PIXFMT_CI12: u32 = 2;
/// 8-bit colour index: the R iterator carries the index (0..255), written
/// as i | i << 8 so either 8-bit buffer mask works (libglcore builds the
/// double-buffer masks by shifting by the depth: 0x00FF / 0xFF00 for 8
/// bits; the bank-1 position is unverified).
pub const PIXFMT_CI8: u32 = 3;
/// 8-bit 3:3:2 TrueColor (Xsgi visual masks: R 7:5, B 4:3, G 2:0), packed
/// from 8:8:8 by truncation and written as v | v << 8 like PIXFMT_CI8.
pub const PIXFMT_RGB8: u32 = 4;
/// 4-bit 1:2:1 TrueColor (Xsgi visual 0x24 masks: R 3, G 2:1, B 0), packed
/// from 8:8:8 by truncation and written as v | v << 4 (bank 1 = bits 7:4,
/// libglcore's write masks shifted by the depth).
pub const PIXFMT_RGB4: u32 = 5;
/// Emulator-private depth control (the enables and write mask have no
/// documented RE3 register): val = test enable (bit 0) | func (bits 3:1,
/// GL order NEVER..ALWAYS) | Z write mask << 8 (24 bits). Depth is tested
/// and written by SMOOTH spans (the only spans that iterate Z per pixel).
pub const RE3_OP_ZCTL: u32 = 0xFFFF_1005;
/// Emulator-private stencil control: val = enable (bit 0) | func << 1 |
/// ref << 4 (8 bits) | mask << 12 (8) | write mask << 20 (8) | fail << 28 |
/// zfail << 32 | zpass << 36 (ops 0 KEEP, 1 INVERT, 2 ZERO, 3 REPLACE,
/// 4 INCR, 5 DECR; libglcore's codes).
pub const RE3_OP_STENCIL: u32 = 0xFFFF_1006;
/// Emulator-private Z/stencil rectangle fill: RE3_OP_ZFILL_A = x0 | y0 << 16
/// | x1 << 32 | y1 << 48 (exclusive), RE3_OP_ZFILL_B = value (bits 31:0) |
/// mask << 32: zbuf = (zbuf & !mask) | (value & mask). Z is bits 23:0,
/// stencil bits 31:24 of the emulator's zbuf word (layout unverified).
pub const RE3_OP_ZFILL_A: u32 = 0xFFFF_1007;
pub const RE3_OP_ZFILL_B: u32 = 0xFFFF_1008;
pub const ZBUF_STENCIL_SHIFT: u32 = 24;
/// EMULATOR EXTENSION, not RE3 hardware. Real RE3 has no blend unit and no
/// alpha iterator: on GR2 the GE7 microcode does blending (reading the
/// destination back through RE3 and writing the result). Doing it here, per
/// pixel inside RE3, gives the same pixels without a span read-modify-write
/// round trip (verified against a real Indy
/// XZ with glprim --scene blendsmooth). There is deliberately no alpha test:
/// libglcore sends only the fragments that pass.
///
/// Emulator-private blend control: val = enable (bit 0) | src factor << 1 |
/// dst factor << 4 (libglcore's codes: 0 ZERO, 1 ONE,
/// 2 DST_COLOR (src) / SRC_COLOR (dst), 3 ONE_MINUS of that, 4 SRC_ALPHA,
/// 5 ONE_MINUS_SRC_ALPHA). Applied by SMOOTH spans; the destination is the
/// frame-buffer colour expanded to 8 bits per channel (no alpha planes).
pub const RE3_OP_SET_BLENDER: u32 = 0xFFFF_1009;
/// Emulator-private source alpha iterator: val = start (8.11, bits 31:0) |
/// per-pixel step (signed 8.11) << 32.
pub const RE3_OP_SET_ALPHA: u32 = 0xFFFF_100A;

/// Stop the RE3 thread.
pub const RE3_OP_EXIT: u32 = 0xFFFF_1FFF;
/// Set on register entries pushed by the HQ2 thread (vs. CPU writes to the
/// RE3 window), so traces can show where each write came from.
pub const RE3_SRC_HQ: u32 = 0x100;

/// Streaming state of HOST_READ/HOST_WRITE.
pub const STREAM_IDLE: u32 = 0;
pub const STREAM_READ: u32 = 1;
pub const STREAM_WRITE: u32 = 2;

/// RE3 register file plus the working state of the current primitive.
/// C-style and `Copy` so a future JIT can address it by offset.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Re3Context {
    pub reg: [u32; 64],
    /// Iterators for the executing primitive (14-bit fraction, like MAME).
    pub x: i64,
    pub y: i64,
    pub z: i64,
    pub r: i64,
    pub g: i64,
    pub b: i64,
    pub numpix: u32,
    pub stream: u32,
    /// RE3_OP_PIXFMT value.
    pub pixfmt: u32,
    /// RE3_OP_ZCTL / RE3_OP_STENCIL values.
    pub zctl: u32,
    pub stencil: u64,
    /// RE3_OP_ZFILL_A, until RE3_OP_ZFILL_B arrives.
    pub zfill_rect: u64,
    /// RE3_OP_SET_BLENDER value; alpha iterator (8.11) and its per-pixel step.
    pub blend: u32,
    pub a: i64,
    pub da: i64,
}

/// RE3 state owned by the RE3 thread. Plain data, zero-valid.
#[repr(C)]
pub struct Re3 {
    pub ctx: Re3Context,
    pub vram: [u32; FB_W * FB_H],
    pub zbuf: [u32; FB_W * FB_H],
    /// The span JIT's state (`re3_jit`): mode, memoised shaders, counts.
    #[cfg(feature = "gr2-jit")]
    pub jit: super::re3_jit::JitState,
}

impl Re3 {
    /// Apply one register write (masked), with MAME's fan-out side effects.
    /// Returns true when the write executed something that touches VRAM.
    pub fn write_reg(&mut self, reg: usize, val: u32) -> bool {
        let reg = reg & 63;
        let v = val & REGMASK[reg];
        #[cfg(feature = "gr2-jit")]
        if super::re3_jit::shapes_key(reg) && self.ctx.reg[reg] != v {
            self.jit.invalidate();
        }
        self.ctx.reg[reg] = v;
        match reg {
            RE3_COLOR_PACKED => {
                // 27-bit packed 9:9:9. Each 9-bit component lands in
                // the integer part of the 8.11 R/G/B iterators.
                self.ctx.reg[RE3_RED_START] = ((v >> 18) & 0x1ff) << 11;
                self.ctx.reg[RE3_GREEN_START] = (((v >> 9) & 0x1ff) << 11) & REGMASK[RE3_GREEN_START];
                self.ctx.reg[RE3_BLUE_START] = ((v & 0x1ff) << 11) & REGMASK[RE3_BLUE_START];
                false
            }
            RE3_XY_PACKED => {
                self.ctx.reg[RE3_CUR_X] = v & 0xfff;
                self.ctx.reg[RE3_CUR_Y] = (v >> 12) & 0x7ff;
                false
            }
            RE3_DRAW_TRIGGER => {
                self.execute();
                true
            }
            RE3_HOST_DATA => {
                if self.ctx.stream == STREAM_WRITE {
                    self.write_buffer();
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// The private operations that shape span pipelines (RE3_OP_PIXFMT,
    /// RE3_OP_ZCTL, RE3_OP_STENCIL, RE3_OP_SET_BLENDER).
    pub fn set_pixfmt(&mut self, v: u32) {
        #[cfg(feature = "gr2-jit")]
        if self.ctx.pixfmt != v {
            self.jit.invalidate();
        }
        self.ctx.pixfmt = v;
    }

    pub fn set_zctl(&mut self, v: u32) {
        #[cfg(feature = "gr2-jit")]
        if self.ctx.zctl & 0xf != v & 0xf {
            self.jit.invalidate();
        }
        self.ctx.zctl = v;
    }

    pub fn set_stencil(&mut self, v: u64) {
        #[cfg(feature = "gr2-jit")]
        {
            // Enable, function and the three ops; the rest is span data.
            let shape = |s: u64| s & (0xf | 0xfff << 28);
            if shape(self.ctx.stencil) != shape(v) {
                self.jit.invalidate();
            }
        }
        self.ctx.stencil = v;
    }

    pub fn set_blend(&mut self, v: u32) {
        #[cfg(feature = "gr2-jit")]
        if self.ctx.blend != v {
            self.jit.invalidate();
        }
        self.ctx.blend = v;
    }

    fn load_iterators(&mut self) {
        let c = &mut self.ctx;
        c.x = (c.reg[RE3_CUR_X] as i64) << 14;
        c.y = (c.reg[RE3_CUR_Y] as i64) << 14;
        // SUBPIXEL (inferred): the minor axis's subpixel start in 1/16 pixel,
        // the GE's vertex precision. The major axis steps whole pixels from
        // a pixel start (its step is +-1.0), so one 4-bit fraction suffices;
        // it becomes the top 4 bits of the minor iterator's 14-bit fraction.
        let frac = ((c.reg[RE3_SUBPIXEL] & 0xf) as i64) << 10;
        let dx = (c.reg[RE3_X_STEP] as u16 as i16) as i32;
        if dx.abs() == 0x4000 {
            c.y |= frac;
        } else {
            c.x |= frac;
        }
        c.z = ((c.reg[RE3_Z_START] as i64) << 40) >> 26;
        c.r = c.reg[RE3_RED_START] as i64;
        c.g = c.reg[RE3_GREEN_START] as i64;
        c.b = c.reg[RE3_BLUE_START] as i64;
        c.numpix = c.reg[RE3_SPAN_LEN];
    }

    fn step_iterators(&mut self) {
        let c = &mut self.ctx;
        c.x += (c.reg[RE3_X_STEP] as u16 as i16) as i64;
        c.y += (c.reg[RE3_Y_STEP] as u16 as i16) as i64;
        c.z += (((c.reg[RE3_Z_STEP_INT] as i64) << 40) >> 26) | c.reg[RE3_Z_STEP_FRAC] as i64;
        c.r += sext(c.reg[RE3_RED_STEP], 24);
        c.g += sext(c.reg[RE3_GREEN_STEP], 20);
        c.b += sext(c.reg[RE3_BLUE_STEP], 20);
        c.a += c.da;
    }

    fn execute(&mut self) {
        self.load_iterators();
        #[cfg(feature = "gr2-jit")]
        if super::re3_jit::run(self, self.ctx.reg[RE3_DRAW_TRIGGER]) {
            return;
        }
        match self.ctx.reg[RE3_DRAW_TRIGGER] {
            DRAW_SMOOTH => self.draw_shaded_span(),
            DRAW_FLAT_5 => self.draw_flat_span(5),
            DRAW_FLAT_20 => self.draw_flat_span(20),
            DRAW_HOST_READ => {
                self.ctx.stream = STREAM_READ;
                self.read_buffer();
            }
            DRAW_HOST_WRITE => {
                self.ctx.stream = if self.ctx.numpix > 0 { STREAM_WRITE } else { STREAM_IDLE };
            }
            // LINE_UPPER / LINE_LOWER (antialiased lines): not needed for the textport yet.
            _ => {}
        }
    }

    /// Current colour from the iterators: 8.11 R/G/B -> pixel bits at
    /// (x, y). In colour-index mode G/B are zero and R carries the (up to
    /// 12-bit) index. 12-bit RGB packs 4:4:4 (R low, same convention as
    /// REX3 DRAWDEPTH 12) into both buffers, dithered 8 -> 4 bits with the
    /// REX3 4x4 Bayer matrix when DITHER_ON is set.
    #[inline]
    fn color(&self, x: i32, y: i32) -> u32 {
        let c = &self.ctx;
        if c.pixfmt == PIXFMT_CI12 {
            let i = (c.r >> 11).clamp(0, 0xfff) as u32;
            return i | (i << 12);
        }
        if c.pixfmt == PIXFMT_CI8 {
            let i = (c.r >> 11).clamp(0, 0xff) as u32;
            return i | (i << 8);
        }
        let ch = |v: i64| (v >> 11).clamp(0, 255) as u32;
        self.pack(ch(c.r) | (ch(c.g) << 8) | (ch(c.b) << 16), x, y)
    }

    /// Colour for a SMOOTH span pixel at `off`, blended with the frame
    /// buffer when RE3_OP_SET_BLENDER is on.
    #[inline]
    fn color_blended(&self, x: i32, y: i32, off: usize) -> u32 {
        let c = &self.ctx;
        // No blending in colour-index mode (GL).
        if c.blend & 1 == 0 || c.pixfmt == PIXFMT_CI12 || c.pixfmt == PIXFMT_CI8 {
            return self.color(x, y);
        }
        let ch = |v: i64| (v >> 11).clamp(0, 255) as u32;
        let s = [ch(c.r), ch(c.g), ch(c.b)];
        let a = ch(c.a);
        let d = self.dst_rgb8(off);
        let factor = |code: u32, k: usize, other: [u32; 3]| -> u32 {
            match code {
                0 => 0,
                1 => 255,
                2 => other[k],
                3 => 255 - other[k],
                4 => a,
                5 => 255 - a,
                _ => 255,
            }
        };
        let (sf, df) = ((c.blend >> 1) & 7, (c.blend >> 4) & 7);
        let mut out = 0;
        for k in 0..3 {
            let v = (s[k] * factor(sf, k, d) + d[k] * factor(df, k, s) + 127) / 255;
            out |= v.min(255) << (8 * k);
        }
        self.pack(out, x, y)
    }

    /// Frame-buffer colour at `off` as 8-bit R, G, B for blending. For 12-bit
    /// RGB, the buffer being written (COLOR_PLANE_MASK) is read and nibbles expanded.
    fn dst_rgb8(&self, off: usize) -> [u32; 3] {
        let p = self.vram[off];
        if self.ctx.pixfmt == PIXFMT_RGB12 {
            let buf = if self.ctx.reg[RE3_COLOR_PLANE_MASK] & 0xfff == 0 { (p >> 12) & 0xfff } else { p & 0xfff };
            return [(buf & 0xf) * 17, ((buf >> 4) & 0xf) * 17, ((buf >> 8) & 0xf) * 17];
        }
        if self.ctx.pixfmt == PIXFMT_RGB8 {
            let v = if self.ctx.reg[RE3_COLOR_PLANE_MASK] & 0xff == 0 { (p >> 8) & 0xff } else { p & 0xff };
            return rgb332_to_888(v);
        }
        if self.ctx.pixfmt == PIXFMT_RGB4 {
            let v = if self.ctx.reg[RE3_COLOR_PLANE_MASK] & 0xf == 0 { (p >> 4) & 0xf } else { p & 0xf };
            return rgb121_to_888(v);
        }
        [p & 0xff, (p >> 8) & 0xff, (p >> 16) & 0xff]
    }

    /// Pack an 8-bit-per-channel colour (R in bits 7:0) into the frame-buffer
    /// format selected by RE3_OP_PIXFMT.
    #[inline]
    fn pack(&self, rgb: u32, x: i32, y: i32) -> u32 {
        let c = &self.ctx;
        if c.pixfmt == PIXFMT_RGB12 {
            let v = if c.reg[RE3_DITHER_ON] != 0 {
                crate::dev::bayer::rgb24_to_rgb12_dither(crate::dev::bayer::bayer_pack(rgb, x, y))
            } else {
                crate::dev::bayer::rgb24_to_rgb12(rgb)
            };
            return v | (v << 12);
        }
        if c.pixfmt == PIXFMT_RGB8 {
            let v = (rgb & 0xe0) | ((rgb >> 19) & 0x18) | ((rgb >> 13) & 0x07);
            return v | (v << 8);
        }
        if c.pixfmt == PIXFMT_RGB4 {
            let v = ((rgb >> 4) & 0x8) | ((rgb >> 13) & 0x6) | ((rgb >> 23) & 0x1);
            return v | (v << 4);
        }
        rgb & 0x00ff_ffff
    }

    /// aux/cid bits written with a span, and the combined write mask.
    #[inline]
    fn aux_and_mask(&self) -> (u32, u32) {
        let r = &self.ctx.reg;
        let aux = if r[RE3_SKIP_POPUP] != 0 {
            (r[RE3_WID_VALUE] << 28) | (r[RE3_AUX_VALUE] << 24)
        } else {
            (r[RE3_WID_VALUE] << 28) | ((r[RE3_AUX_VALUE] & 3) << 26) | (r[RE3_POPUP_VALUE] << 24)
        };
        let mask = ((r[RE3_AUX_PLANE_MASK] & 0xff) << 24) | r[RE3_COLOR_PLANE_MASK];
        (aux, mask)
    }

    #[inline]
    fn clip(&self, x: i32, y: i32) -> bool {
        let r = &self.ctx.reg;
        x >= 0 && y >= 0 && (x as usize) < FB_W && (y as usize) < FB_H
            && x >= (r[RE3_CLIP_X_LO] as i32) && x <= (r[RE3_CLIP_X_HI] as i32)
            && y >= r[RE3_CLIP_Y_LO] as i32 && y <= r[RE3_CLIP_Y_HI] as i32
    }

    #[inline]
    fn pattern(&self, x: i32, n: u32) -> bool {
        let r = &self.ctx.reg;
        if r[RE3_PATTERN_ON] == 0 {
            return true;
        }
        let idx = if r[RE3_PATTERN_ANCHOR] != 0 { x as u32 } else { n } & 31;
        let pat = (r[RE3_PATTERN_HI] << 16) | r[RE3_PATTERN_LO];
        (pat >> (31 - idx)) & 1 != 0
    }

    #[inline]
    fn wid_ok(&self, draw_op: u32, offset: usize) -> bool {
        let r = &self.ctx.reg;
        if r[RE3_WID_TEST_ON] == 0 {
            return true;
        }
        if (draw_op == DRAW_LINE_UPPER || draw_op == DRAW_LINE_LOWER) && r[RE3_LINE_WID_TEST_ON] == 0 {
            return true;
        }
        let wid = self.vram[offset] >> 28;
        if r[RE3_FB_OPTIONS] & 1 != 0 {
            if r[RE3_Z_COMPARE] & 8 != 0 {
                (wid & 0xe) == (r[RE3_WINDOW_ID] & 0xe)
            } else {
                (wid & 0xf) == (r[RE3_WINDOW_ID] & 0xf)
            }
        } else {
            (wid & 3) == (r[RE3_WINDOW_ID] & 3)
        }
    }

    /// Raster op on the colour/aux bits, then a masked store.
    #[inline]
    fn store(&mut self, offset: usize, src: u32, mask: u32) {
        let dst = self.vram[offset];
        let v = rop(self.ctx.reg[RE3_LOGIC_OP], src, dst);
        self.vram[offset] = (dst & !mask) | (v & mask);
    }

    /// Stencil and depth tests for one fragment at `off`, applying the
    /// stencil ops and the masked Z write. True if the colour may be written.
    #[inline]
    fn zs_test(&mut self, off: usize) -> bool {
        let (zctl, st) = (self.ctx.zctl, self.ctx.stencil);
        if zctl & 1 == 0 && st & 1 == 0 {
            return true;
        }
        let cmp = |func: u32, a: u32, b: u32| match func & 7 {
            0 => false,
            1 => a < b,
            2 => a == b,
            3 => a <= b,
            4 => a > b,
            5 => a != b,
            6 => a >= b,
            _ => true,
        };
        let word = self.zbuf[off];
        let mut new = word;
        let mut pass = true;
        let mut sop = 0u64;
        if st & 1 != 0 {
            let (func, sref, smask) = ((st >> 1) & 7, (st >> 4) & 0xff, (st >> 12) & 0xff);
            let sv = (word >> ZBUF_STENCIL_SHIFT) as u64 & 0xff;
            if !cmp(func as u32, (sref & smask) as u32, (sv & smask) as u32) {
                pass = false;
                sop = (st >> 28) & 0xf;
            }
        }
        if pass && zctl & 1 != 0 {
            // Z is a signed 24-bit value: IRIS GL uses the full range
            // (zclear -0x800000, lsetdepth ZMIN..ZMAX, ZF_GEQUAL), OpenGL
            // only 0..0x7FFFFF (23 bits). Bias both sides so the unsigned
            // compare orders them as signed.
            let z = ((self.ctx.z >> 14).clamp(-0x80_0000, 0x7f_ffff) as u32) & 0x00ff_ffff;
            if !cmp((zctl >> 1) & 7, z ^ 0x80_0000, (word & 0x00ff_ffff) ^ 0x80_0000) {
                pass = false;
                sop = (st >> 32) & 0xf;
            } else {
                let wm = zctl >> 8 & 0x00ff_ffff;
                new = (new & !wm) | (z & wm);
            }
        }
        if pass {
            sop = (st >> 36) & 0xf;
        }
        if st & 1 != 0 {
            let (sref, wmask) = ((st >> 4) & 0xff, ((st >> 20) & 0xff) as u32);
            let sv = (new >> ZBUF_STENCIL_SHIFT) & 0xff;
            let nv = match sop {
                1 => !sv,
                2 => 0,
                3 => sref as u32,
                4 => (sv + 1).min(0xff),
                5 => sv.saturating_sub(1),
                _ => sv,
            } & 0xff;
            let sv2 = (sv & !wmask) | (nv & wmask);
            new = (new & 0x00ff_ffff) | (sv2 << ZBUF_STENCIL_SHIFT);
        }
        self.zbuf[off] = new;
        pass
    }

    /// RE3_OP_ZFILL: fill a rectangle of the Z/stencil buffer.
    pub fn zfill(&mut self, rect: u64, val: u64) {
        let f = |s: u32| ((rect >> s) & 0xffff) as usize;
        let (x0, y0, x1, y1) = (f(0), f(16), f(32).min(FB_W), f(48).min(FB_H));
        let (value, mask) = (val as u32, (val >> 32) as u32);
        for y in y0..y1 {
            for z in &mut self.zbuf[y * FB_W + x0..y * FB_W + x1.max(x0)] {
                *z = (*z & !mask) | (value & mask);
            }
        }
    }

    fn draw_shaded_span(&mut self) {
        let (aux, mask) = self.aux_and_mask();
        let mut n = 0u32;
        while self.ctx.numpix > 0 {
            self.ctx.numpix -= 1;
            let x = (self.ctx.x >> 14) as i32;
            let y = (self.ctx.y >> 14) as i32;
            if self.clip(x, y) && self.pattern(x, n) {
                let off = y as usize * FB_W + x as usize;
                if self.wid_ok(DRAW_SMOOTH, off) && self.zs_test(off) {
                    let c = self.color_blended(x, y, off);
                    self.store(off, aux | c, mask);
                }
            }
            self.step_iterators();
            n += 1;
        }
    }

    /// Flat span: `numpix` consecutive pixels from (x, y). Colour steps once
    /// per `n` pixels.
    fn draw_flat_span(&mut self, n: u32) {
        let (aux, mask) = self.aux_and_mask();
        let x0 = (self.ctx.x >> 14) as i32;
        let y = (self.ctx.y >> 14) as i32;
        let count = self.ctx.numpix;
        // Dithering makes the colour depend on the pixel position.
        let per_pixel = self.ctx.pixfmt == PIXFMT_RGB12 && self.ctx.reg[RE3_DITHER_ON] != 0;
        let mut c = self.color(x0, y);
        for i in 0..count {
            let x = x0 + i as i32;
            if self.clip(x, y) && self.pattern(x, i) {
                let off = y as usize * FB_W + x as usize;
                if self.wid_ok(DRAW_FLAT_5, off) {
                    let c = if per_pixel { self.color(x, y) } else { c };
                    self.store(off, aux | c, mask);
                }
            }
            if (i + 1) % n == 0 {
                self.step_iterators();
                c = self.color(x + 1, y);
            }
        }
        self.ctx.numpix = 0;
    }

    /// Plane value at `offset` for the current HOST_PLANE.
    fn plane_read(&self, offset: usize) -> u32 {
        let p = self.vram[offset];
        match self.ctx.reg[RE3_HOST_PLANE] {
            HPLANE_POPUP => (p >> 24) & 0x3,
            HPLANE_AUX => (p >> 24) & 0xf,
            HPLANE_WID => p >> 28,
            HPLANE_Z | HPLANE_Z_RAW => self.zbuf[offset],
            HPLANE_COLOR_RAW => p,
            _ => p & 0x00ff_ffff,
        }
    }

    fn plane_write(&mut self, offset: usize, data: u32) {
        let r = &self.ctx.reg;
        let aux_mask = r[RE3_AUX_PLANE_MASK];
        match r[RE3_HOST_PLANE] {
            HPLANE_POPUP => {
                let m = (aux_mask & 0x3) << 24;
                self.store(offset, (data & 0x3) << 24, m);
            }
            HPLANE_AUX => {
                let m = (aux_mask & if r[RE3_SKIP_POPUP] != 0 { 0xf } else { 0xc }) << 24;
                self.store(offset, (data & 0xf) << 24, m);
            }
            HPLANE_WID => {
                let m = ((aux_mask >> 4) & 0xf) << 28;
                self.store(offset, (data & 0xf) << 28, m);
            }
            HPLANE_Z | HPLANE_Z_RAW => self.zbuf[offset] = data & 0x00ff_ffff,
            HPLANE_COLOR_RAW => self.vram[offset] = data,
            _ => {
                let m = r[RE3_COLOR_PLANE_MASK];
                self.store(offset, data & 0x00ff_ffff, m);
            }
        }
    }

    /// HOST_READ: latch the next pixel into HOST_DATA, or finish the stream.
    pub fn read_buffer(&mut self) {
        if self.ctx.numpix == 0 {
            self.ctx.stream = STREAM_IDLE;
            return;
        }
        let x = (self.ctx.x >> 14) as i32;
        let y = (self.ctx.y >> 14) as i32;
        self.ctx.reg[RE3_HOST_DATA] = if x >= 0 && y >= 0 && (x as usize) < FB_W && (y as usize) < FB_H {
            self.plane_read(y as usize * FB_W + x as usize)
        } else {
            0
        };
        self.step_iterators();
        self.ctx.numpix -= 1;
    }

    /// HOST_WRITE: consume HOST_DATA (unpacked per UNPACK_MODE) into VRAM.
    fn write_buffer(&mut self) {
        let mode = self.ctx.reg[RE3_UNPACK_MODE];
        let data = self.ctx.reg[RE3_HOST_DATA];
        for i in 0..=mode {
            if self.ctx.numpix == 0 {
                break;
            }
            let x = (self.ctx.x >> 14) as i32;
            let y = (self.ctx.y >> 14) as i32;
            if self.clip(x, y) {
                let off = y as usize * FB_W + x as usize;
                if self.wid_ok(DRAW_HOST_WRITE, off) {
                    self.plane_write(off, unpack(data, i, mode));
                }
            }
            self.step_iterators();
            self.ctx.numpix -= 1;
        }
        if self.ctx.numpix == 0 {
            self.ctx.stream = STREAM_IDLE;
        }
    }

    /// GL copy (see copy_mode): source rectangle at (sx, sy) to (dx, dy),
    /// each pixel repeated zx x zy times, through the logic op, write masks,
    /// scissor and WID test. The source is read first, so overlap is safe.
    pub fn copy_rect_gl(&mut self, sx: i32, sy: i32, w: i32, h: i32, dx: i32, dy: i32, mode: u32) {
        if w <= 0 || h <= 0 {
            return;
        }
        let (zx, zy) = ((mode & 0xff).max(1) as i32, ((mode >> 8) & 0xff).max(1) as i32);
        let bits = ((mode >> 16) & 3) * 4;
        let bank = (mode >> 18) & 1;
        let pix = |v: u32| -> u32 {
            if bits == 0 {
                return v;
            }
            let m = (1u32 << bits) - 1;
            let c = (v >> (bits * bank)) & m;
            (v & 0xff00_0000) | c | (c << bits)
        };
        // Only the destination pixels on the screen are written, so only the source pixels they repeat
        // are read: a rectangle far larger than the screen costs what the screen does.
        let (i0, i1) = (dx.saturating_neg().max(0), w.saturating_mul(zx).min((FB_W as i32).saturating_sub(dx)));
        let (j0, j1) = (dy.saturating_neg().max(0), h.saturating_mul(zy).min((FB_H as i32).saturating_sub(dy)));
        if i0 >= i1 || j0 >= j1 {
            return;
        }
        let (ci0, ci1, cj0, cj1) = (i0 / zx, (i1 - 1) / zx + 1, j0 / zy, (j1 - 1) / zy + 1);
        let sw = (ci1 - ci0) as usize;
        let mut src = vec![0u32; sw * (cj1 - cj0) as usize];
        for j in cj0..cj1 {
            for i in ci0..ci1 {
                let (x, y) = (sx.wrapping_add(i), sy.wrapping_add(j));
                if x >= 0 && y >= 0 && (x as usize) < FB_W && (y as usize) < FB_H {
                    src[(j - cj0) as usize * sw + (i - ci0) as usize] = pix(self.vram[y as usize * FB_W + x as usize]);
                }
            }
        }
        let mask = ((self.ctx.reg[RE3_AUX_PLANE_MASK] & 0xff) << 24) | self.ctx.reg[RE3_COLOR_PLANE_MASK];
        for j in j0..j1 {
            for i in i0..i1 {
                let (x, y) = (dx + i, dy + j);
                if !self.clip(x, y) {
                    continue;
                }
                let off = y as usize * FB_W + x as usize;
                if self.wid_ok(DRAW_HOST_WRITE, off) {
                    self.store(off, src[(j / zy - cj0) as usize * sw + (i / zx - ci0) as usize], mask);
                }
            }
        }
    }

    /// Emulator-private screen-to-screen copy of the planes selected by
    /// COLOR_PLANE_MASK/AUX_PLANE_MASK. Linear coordinates, y bottom-up; overlap-safe.
    pub fn copy_rect(&mut self, sx: i32, sy: i32, w: i32, h: i32, dx: i32, dy: i32) {
        if w <= 0 || h <= 0 {
            return;
        }
        let mask = ((self.ctx.reg[RE3_AUX_PLANE_MASK] & 0xff) << 24) | self.ctx.reg[RE3_COLOR_PLANE_MASK];
        // Walk away from the overlap: bottom-up rows when moving down in
        // memory, right-to-left columns when moving right. Only rows and columns whose source and
        // destination are both on the screen can move anything, so only those are walked.
        let (j0, j1) = ((-sy).max(-dy).max(0), h.min(FB_H as i32 - sy.max(dy)));
        let (i0, i1) = ((-sx).max(-dx).max(0), w.min(FB_W as i32 - sx.max(dx)));
        for jj in 0..(j1 - j0).max(0) {
            let j = if dy > sy { j1 - 1 - jj } else { j0 + jj };
            let (ys, yd) = (sy + j, dy + j);
            for ii in 0..(i1 - i0).max(0) {
                let i = if dx > sx { i1 - 1 - ii } else { i0 + ii };
                let (xs, xd) = (sx + i, dx + i);
                if !self.clip(xd, yd) {
                    continue;
                }
                let src = self.vram[ys as usize * FB_W + xs as usize];
                let off = yd as usize * FB_W + xd as usize;
                let dst = self.vram[off];
                self.vram[off] = (dst & !mask) | (src & mask);
            }
        }
    }
}

#[inline]
fn sext(v: u32, bits: u32) -> i64 {
    let s = 64 - bits;
    ((v as i64) << s) >> s
}

/// 16 raster ops, X11/GL numbering.
#[inline]
pub fn rop(func: u32, s: u32, d: u32) -> u32 {
    match func & 0xf {
        0 => 0,
        1 => s & d,
        2 => s & !d,
        3 => s,
        4 => !s & d,
        5 => d,
        6 => s ^ d,
        7 => s | d,
        8 => !(s | d),
        9 => !(s ^ d),
        10 => !d,
        11 => s | !d,
        12 => !s,
        13 => !s | d,
        14 => !(s & d),
        _ => !0,
    }
}

/// Extract pixel `n` of a packed HOST_DATA word (UNPACK_MODE 0 = 32, 1 = 2x16, 3 = 4x8).
#[inline]
fn unpack(data: u32, n: u32, mode: u32) -> u32 {
    match mode {
        1 => (data >> (16 * (1 - n))) & 0xffff,
        3 => (data >> (8 * (3 - n))) & 0xff,
        _ => data,
    }
}

/// 4-bit 1:2:1 (R 3, G 2:1, B 0) to 8:8:8 values [R, G, B].
pub fn rgb121_to_888(v: u32) -> [u32; 3] {
    [((v >> 3) & 1) * 255, ((v >> 1) & 3) * 85, (v & 1) * 255]
}

/// 3:3:2 (R 7:5, B 4:3, G 2:0) to 8 bits a component by bit replication,
/// which is also GL's rounded fixed-point conversion for every 3- and
/// 2-bit value (4 -> 0x92, not 0x91).
pub fn rgb332_to_888(v: u32) -> [u32; 3] {
    let c3 = |c: u32| (c << 5) | (c << 2) | (c >> 1);
    [c3((v >> 5) & 7), c3(v & 7), ((v >> 3) & 3) * 0x55]
}
