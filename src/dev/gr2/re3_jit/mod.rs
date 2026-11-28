//! The RE3 span JIT (`gr2-jit`): GR2 span pipelines compiled to native code
//! with Cranelift, on the pattern of the IMPACT raster JIT (`mgras::rss_jit`).
//!
//! On GR2 the HQ2/GE7 side walks primitives and RE3 draws spans: SMOOTH
//! (every iterator steps per pixel, with depth, stencil and the emulator's
//! blend) and FLAT_5 / FLAT_20 (consecutive pixels, colour stepping every 5 or
//! 20). The interpreter in `re3.rs` decides, pixel by pixel, from the
//! registers, what each stage does. A shader decides it once: the state that
//! shapes the pipeline is reduced to a `SpanKey` (instruction, pixel format
//! and dither, raster op, pattern mode, WID test, depth and stencil tests,
//! blend factors), and the shader for that key is straight-line code for
//! exactly that pipeline. Everything the pipeline only reads (colours and
//! steps, pattern bits, masks, scissor, the current WID, stencil reference
//! and masks) it reads from the RE3 registers as it starts: the shader's one
//! argument is the `Re3` itself, whose context is laid out for this.
//!
//! The shaders are bit-exact with the interpreter (`re3_jit_tests.rs`
//! sweeps keys and compares whole boards).
//!
//! Shaders are compiled on one background thread and published in a
//! process-wide map; a span whose shader is not ready yet runs in the
//! interpreter. `IRIS_GR2_JIT=off|sync|async` picks the mode (`sync`
//! compiles on first use and waits, for tests and comparisons), as does the
//! monitor's `gr2 jit`. `IRIS_NO_JIT` forces it off, as it does the other
//! graphics JITs.

pub mod compiler;
pub mod key;

pub use key::{Blend, Fmt, Pat, Prim, SpanKey, Stencil, Wid};

use super::re3::{self, Re3};
use crate::jit::shader_store;
use std::sync::OnceLock;

/// The shader ABI: the RE3 state, everything reached through it.
pub type ShaderFn = unsafe extern "C" fn(*mut Re3);

/// Interpreter only (a zeroed `Re3` is off).
pub const MODE_OFF: u32 = 0;
/// Compile in the background; the interpreter draws until a shader is ready.
pub const MODE_ASYNC: u32 = 1;
/// Compile on first use and wait (tests, comparisons).
pub const MODE_SYNC: u32 = 2;

/// Whether the host forbids JIT code (`IRIS_NO_JIT`): off then, whatever
/// `IRIS_GR2_JIT` or `gr2 jit` ask for.
pub fn forbidden() -> bool {
    std::env::var_os("IRIS_NO_JIT").is_some()
}

/// The mode a new board starts in: off if `forbidden`, else `IRIS_GR2_JIT`,
/// else async (off in unit tests, so they stay deterministic unless asked).
pub fn default_mode() -> u32 {
    if forbidden() {
        return MODE_OFF;
    }
    match std::env::var("IRIS_GR2_JIT").as_deref() {
        Ok("off") | Ok("0") => MODE_OFF,
        Ok("sync") => MODE_SYNC,
        Ok("async") | Ok("on") | Ok("1") => MODE_ASYNC,
        _ if cfg!(test) => MODE_OFF,
        _ => MODE_ASYNC,
    }
}

/// An instruction's last shader: valid while `epoch` matches and `f` is set.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Memo {
    epoch: u32,
    _pad: u32,
    f: usize,
    packed: u64,
}

/// The JIT's state on one board: plain data, part of `Re3`, valid zeroed.
///
/// Each instruction memoises its last shader. A change to state that shapes
/// the key (`shapes_key`, the private pixel-format, Z, stencil and blend
/// operations) bumps `epoch`, which drops them all.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct JitState {
    pub mode: u32,
    epoch: u32,
    memo: [Memo; 3],
    last: u64,
    /// Spans drawn by a shader / by the interpreter while their shader was
    /// compiling (or failed).
    pub hits: u64,
    pub misses: u64,
}

impl JitState {
    #[inline]
    pub fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// The packed key of the last shader run.
    pub fn last_key(&self) -> u64 {
        self.last
    }
}

/// RE3 registers that shape the key. Every other register a span uses is
/// read by the shader as it starts.
#[inline]
pub fn shapes_key(r: usize) -> bool {
    matches!(
        r,
        re3::RE3_LOGIC_OP | re3::RE3_PATTERN_ANCHOR | re3::RE3_PATTERN_ON | re3::RE3_DITHER_ON | re3::RE3_WID_TEST_ON | re3::RE3_Z_COMPARE | re3::RE3_FB_OPTIONS
    )
}

/// The key of instruction `prim` in the current state.
pub fn key_of(re3: &Re3, prim: Prim) -> SpanKey {
    let c = &re3.ctx;
    let r = &c.reg;
    let wid = if r[re3::RE3_WID_TEST_ON] == 0 {
        Wid::Off
    } else if r[re3::RE3_FB_OPTIONS] & 1 != 0 {
        if r[re3::RE3_Z_COMPARE] & 8 != 0 { Wid::Three } else { Wid::Four }
    } else {
        Wid::Two
    };
    let pat = if r[re3::RE3_PATTERN_ON] == 0 {
        Pat::Off
    } else if r[re3::RE3_PATTERN_ANCHOR] != 0 {
        Pat::Aligned
    } else {
        Pat::Span
    };
    let st = c.stencil;
    SpanKey {
        prim,
        fmt: Fmt::of(c.pixfmt),
        dither: r[re3::RE3_DITHER_ON] != 0,
        rop: (r[re3::RE3_LOGIC_OP] & 0xf) as u8,
        pat,
        wid,
        z: (c.zctl & 1 != 0).then_some(((c.zctl >> 1) & 7) as u8),
        stencil: (st & 1 != 0).then_some(Stencil {
            func: ((st >> 1) & 7) as u8,
            fail: ((st >> 28) & 0xf) as u8,
            zfail: ((st >> 32) & 0xf) as u8,
            zpass: ((st >> 36) & 0xf) as u8,
        }),
        blend: c.blends().then_some(Blend { src: ((c.blend >> 1) & 7) as u8, dst: ((c.blend >> 4) & 7) as u8 }),
    }
    .normalized()
}

// ── dispatch ─────────────────────────────────────────────────────────────────

/// Draw the span `draw_op` starts (iterators loaded) with its shader. False when
/// there is none (yet): the caller draws with the interpreter.
pub fn run(re3: &mut Re3, draw_op: u32) -> bool {
    if re3.jit.mode == MODE_OFF {
        return false;
    }
    let Some(prim) = Prim::of_draw_op(draw_op) else {
        return false;
    };
    let st = &mut re3.jit;
    let m = st.memo[prim as usize];
    let f = if m.f != 0 && m.epoch == st.epoch {
        st.last = m.packed;
        // SAFETY: memoised from a ShaderFn; shaders are never freed.
        unsafe { std::mem::transmute::<usize, ShaderFn>(m.f) }
    } else {
        let packed = key_of(re3, prim).pack();
        let st = &mut re3.jit;
        match store().get(packed, st.mode == MODE_SYNC) {
            Some(f) => {
                st.memo[prim as usize] = Memo { epoch: st.epoch, _pad: 0, f: f as usize, packed };
                st.last = packed;
                f
            }
            None => {
                st.misses += 1;
                return false;
            }
        }
    };
    // SAFETY: the shader reads and writes only this RE3's context, VRAM and
    // Z buffer, held mutably here, within the bounds the clip test keeps.
    unsafe { f(re3) };
    re3.jit.hits += 1;
    true
}

// ── shader store and compiler thread ─────────────────────────────────────────

pub type Store = shader_store::Store<compiler::Compiler>;

pub fn store() -> &'static Store {
    static STORE: OnceLock<Store> = OnceLock::new();
    STORE.get_or_init(Store::new)
}

impl shader_store::Compile for compiler::Compiler {
    type Shader = ShaderFn;
    const THREAD: &'static str = "gr2-jit";
    const LABEL: &'static str = "GR2 JIT";

    fn fresh() -> Self {
        compiler::Compiler::new()
    }

    fn compile_key(&mut self, key: u64) -> Option<(ShaderFn, u32)> {
        self.compile(key)
    }

    fn key_text(key: u64) -> String {
        SpanKey::unpack(key).to_string()
    }
}
