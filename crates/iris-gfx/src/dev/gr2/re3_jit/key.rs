//! The span key: everything an RE3 span shader is monomorphised on.
//!
//! A key names a span's whole pixel pipeline: which instruction walks the
//! pixels (SHADED, FLAT or FLAT4), the frame-buffer pixel format the colour
//! is packed into (RE3_OP_PIXFMT) and whether 12-bit RGB is dithered, the
//! raster op, the pattern mode, the window-ID test, and for SHADED spans the
//! depth and stencil tests and the emulator's blend (RE3_OP_ZCTL,
//! RE3_OP_STENCIL, RE3_OP_BLEND). Everything the pipeline only reads —
//! colours and their steps, the pattern bits, masks, scissor, the current
//! WID, the stencil reference and masks, the Z write mask — the shader reads
//! from the RE3 registers and context as it starts (`compiler`).
//!
//! Keys are normalised: a field that cannot affect the result in the rest
//! of the configuration is zeroed (dither outside 12-bit RGB, depth,
//! stencil and blending on FLAT spans, blending of colour indices, factor
//! codes that act alike), so two configurations that draw the same share a
//! shader. The packed form is mixed radix, as for `gr4-jit`.

use std::fmt;

/// The RE3 instruction whose pixel loop the shader runs.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Prim {
    /// IR_SHADED: every iterator steps per pixel; Z, stencil and blend.
    #[default]
    Shaded = 0,
    /// IR_FLAT: consecutive pixels, colour stepping every 5.
    Flat = 1,
    /// IR_FLAT4: the same, stepping every 20.
    Flat4 = 2,
}

impl Prim {
    pub const ALL: [Prim; 3] = [Prim::Shaded, Prim::Flat, Prim::Flat4];

    /// The RE3 instruction, if it is one a shader covers.
    pub fn of_ir(ir: u32) -> Option<Prim> {
        match ir {
            super::super::re3::IR_SHADED => Some(Prim::Shaded),
            super::super::re3::IR_FLAT => Some(Prim::Flat),
            super::super::re3::IR_FLAT4 => Some(Prim::Flat4),
            _ => None,
        }
    }

    /// Pixels per colour step of a FLAT span.
    pub fn flat_run(self) -> u32 {
        match self {
            Prim::Flat4 => 20,
            _ => 5,
        }
    }
}

/// The frame-buffer pixel format (RE3_OP_PIXFMT; unknown values pack like
/// 24-bit RGB, as `Re3::pack` does).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Fmt {
    #[default]
    Rgb24 = 0,
    Rgb12 = 1,
    Ci12 = 2,
    Ci8 = 3,
    Rgb8 = 4,
    Rgb4 = 5,
}

impl Fmt {
    pub const ALL: [Fmt; 6] = [Fmt::Rgb24, Fmt::Rgb12, Fmt::Ci12, Fmt::Ci8, Fmt::Rgb8, Fmt::Rgb4];

    pub fn of(pixfmt: u32) -> Fmt {
        Fmt::ALL.get(pixfmt as usize).copied().unwrap_or(Fmt::Rgb24)
    }

    /// The R iterator carries a colour index.
    pub fn ci(self) -> bool {
        matches!(self, Fmt::Ci12 | Fmt::Ci8)
    }
}

/// The pattern (ENABPAT, ALIGNPAT): off, bit `31 - n % 32` for the n-th
/// pixel of the span, or bit `31 - x % 32` for column x.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Pat {
    #[default]
    Off = 0,
    Span = 1,
    Aligned = 2,
}

impl Pat {
    pub const ALL: [Pat; 3] = [Pat::Off, Pat::Span, Pat::Aligned];
}

/// The window-ID test (ENABWID; FBOPTION bit 0 and DEPTHFN bit 3 pick the
/// compared bits): off, WID bits 1:0, 3:0, or 3:1.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Wid {
    #[default]
    Off = 0,
    Two = 1,
    Four = 2,
    Three = 3,
}

impl Wid {
    pub const ALL: [Wid; 4] = [Wid::Off, Wid::Two, Wid::Four, Wid::Three];

    /// The compared WID bits.
    pub fn mask(self) -> u32 {
        match self {
            Wid::Off => 0,
            Wid::Two => 0x3,
            Wid::Four => 0xf,
            Wid::Three => 0xe,
        }
    }
}

/// Stencil test and update: compare function (GL order NEVER..ALWAYS) and
/// the ops on stencil fail, depth fail and depth pass (0 KEEP, 1 INVERT,
/// 2 ZERO, 3 REPLACE, 4 INCR, 5 DECR; other codes keep).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Stencil {
    pub func: u8,
    pub fail: u8,
    pub zfail: u8,
    pub zpass: u8,
}

/// Blend factors (RE3_OP_BLEND codes 0 ZERO, 1 ONE, 2 the other side's
/// colour, 3 one minus it, 4 SRC_ALPHA, 5 ONE_MINUS_SRC_ALPHA; higher codes
/// act as ONE and fold into 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Blend {
    pub src: u8,
    pub dst: u8,
}

/// A span pipeline configuration. Build one, `normalized()` it, `pack()` it;
/// `unpack` gives the normalised key back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct SpanKey {
    pub prim: Prim,
    pub fmt: Fmt,
    /// 12-bit RGB dithered through the 4x4 Bayer matrix (ENABDITH).
    pub dither: bool,
    /// FUNC, X11 / GL logic op numbering (3 = COPY).
    pub rop: u8,
    pub pat: Pat,
    pub wid: Wid,
    /// Depth test compare function, when enabled (SHADED).
    pub z: Option<u8>,
    pub stencil: Option<Stencil>,
    pub blend: Option<Blend>,
}

#[derive(Default)]
struct Enc {
    v: u64,
    m: u64,
}

impl Enc {
    fn put(&mut self, x: u64, radix: u64) {
        debug_assert!(x < radix, "key digit {x} out of radix {radix}");
        if self.m == 0 {
            self.m = 1;
        }
        self.v += x * self.m;
        self.m *= radix;
    }
}

struct Dec(u64);

impl Dec {
    fn get(&mut self, radix: u64) -> u64 {
        let x = self.0 % radix;
        self.0 /= radix;
        x
    }
}

const STENCIL_RADIX: u64 = 1 + 8 * 6 * 6 * 6;
const BLEND_RADIX: u64 = 1 + 6 * 6;

impl SpanKey {
    /// Zero what cannot matter in this configuration.
    pub fn normalized(mut self) -> SpanKey {
        self.rop &= 0xf;
        if self.fmt != Fmt::Rgb12 {
            self.dither = false;
        }
        if self.prim != Prim::Shaded {
            self.z = None;
            self.stencil = None;
            self.blend = None;
        }
        if let Some(z) = self.z.as_mut() {
            *z &= 7;
        }
        if let Some(s) = self.stencil.as_mut() {
            let op = |o: u8| if o > 5 { 0 } else { o };
            *s = Stencil { func: s.func & 7, fail: op(s.fail), zfail: op(s.zfail), zpass: op(s.zpass) };
        }
        if self.fmt.ci() {
            self.blend = None;
        }
        if let Some(b) = self.blend.as_mut() {
            let f = |c: u8| if c > 5 { 1 } else { c };
            *b = Blend { src: f(b.src), dst: f(b.dst) };
        }
        self
    }

    /// The packed key (of the normalised configuration).
    pub fn pack(&self) -> u64 {
        let k = self.normalized();
        let mut e = Enc::default();
        e.put(k.prim as u64, Prim::ALL.len() as u64);
        e.put(k.fmt as u64, Fmt::ALL.len() as u64);
        e.put(k.dither as u64, 2);
        e.put(k.rop as u64, 16);
        e.put(k.pat as u64, Pat::ALL.len() as u64);
        e.put(k.wid as u64, Wid::ALL.len() as u64);
        e.put(k.z.map_or(0, |f| f as u64 + 1), 9);
        e.put(
            k.stencil.map_or(0, |s| 1 + s.func as u64 + 8 * (s.fail as u64 + 6 * (s.zfail as u64 + 6 * s.zpass as u64))),
            STENCIL_RADIX,
        );
        e.put(k.blend.map_or(0, |b| 1 + b.src as u64 + 6 * b.dst as u64), BLEND_RADIX);
        e.v
    }

    pub fn unpack(v: u64) -> SpanKey {
        let mut d = Dec(v);
        SpanKey {
            prim: Prim::ALL[d.get(Prim::ALL.len() as u64) as usize],
            fmt: Fmt::ALL[d.get(Fmt::ALL.len() as u64) as usize],
            dither: d.get(2) != 0,
            rop: d.get(16) as u8,
            pat: Pat::ALL[d.get(Pat::ALL.len() as u64) as usize],
            wid: Wid::ALL[d.get(Wid::ALL.len() as u64) as usize],
            z: match d.get(9) {
                0 => None,
                n => Some(n as u8 - 1),
            },
            stencil: match d.get(STENCIL_RADIX) {
                0 => None,
                n => {
                    let n = n - 1;
                    let (func, ops) = (n % 8, n / 8);
                    Some(Stencil { func: func as u8, fail: (ops % 6) as u8, zfail: (ops / 6 % 6) as u8, zpass: (ops / 36) as u8 })
                }
            },
            blend: match d.get(BLEND_RADIX) {
                0 => None,
                n => Some(Blend { src: ((n - 1) % 6) as u8, dst: ((n - 1) / 6) as u8 }),
            },
        }
    }
}

impl fmt::Display for SpanKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let k = self.normalized();
        write!(f, "{:?} {:?}{} rop={}", k.prim, k.fmt, if k.dither { " dither" } else { "" }, k.rop)?;
        if k.pat != Pat::Off {
            write!(f, " pat={:?}", k.pat)?;
        }
        if k.wid != Wid::Off {
            write!(f, " wid={:#x}", k.wid.mask())?;
        }
        if let Some(z) = k.z {
            write!(f, " z={z}")?;
        }
        if let Some(s) = k.stencil {
            write!(f, " stencil={}:{}/{}/{}", s.func, s.fail, s.zfail, s.zpass)?;
        }
        if let Some(b) = k.blend {
            write!(f, " blend={}/{}", b.src, b.dst)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_packing() {
        let k = SpanKey {
            prim: Prim::Shaded,
            fmt: Fmt::Rgb12,
            dither: true,
            rop: 6,
            pat: Pat::Aligned,
            wid: Wid::Three,
            z: Some(3),
            stencil: Some(Stencil { func: 7, fail: 5, zfail: 4, zpass: 3 }),
            blend: Some(Blend { src: 4, dst: 5 }),
        };
        assert_eq!(SpanKey::unpack(k.pack()), k.normalized());
        let f = SpanKey { prim: Prim::Flat4, fmt: Fmt::Ci8, rop: 0xa, pat: Pat::Span, ..SpanKey::default() };
        assert_eq!(SpanKey::unpack(f.pack()), f.normalized());
    }

    #[test]
    fn normalisation_folds_what_draws_alike() {
        let a = SpanKey { prim: Prim::Flat, z: Some(1), blend: Some(Blend { src: 4, dst: 5 }), dither: true, ..SpanKey::default() };
        assert_eq!(a.pack(), SpanKey { prim: Prim::Flat, ..SpanKey::default() }.pack());
        let c = SpanKey { fmt: Fmt::Ci12, blend: Some(Blend { src: 4, dst: 5 }), ..SpanKey::default() };
        assert_eq!(c.pack(), SpanKey { fmt: Fmt::Ci12, ..SpanKey::default() }.pack());
        let o = SpanKey { blend: Some(Blend { src: 7, dst: 1 }), ..SpanKey::default() };
        assert_eq!(o.pack(), SpanKey { blend: Some(Blend { src: 1, dst: 1 }), ..SpanKey::default() }.pack());
    }
}
