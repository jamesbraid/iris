//! RE3 span shader emitters: one Cranelift function per `SpanKey`.
//!
//! Each emitter mirrors an interpreter routine in `re3.rs` and is named
//! after it (`shaded` for `draw_shaded_span`, `flat` for `draw_flat_span`,
//! and their helpers `clip`, `pattern`, `wid_ok`, `zs_test`, `color`,
//! `color_blended`, `dst_rgb8`, `pack`, `store`). Everything is integer
//! code on both sides, so a shader's output is bit-exact with the
//! interpreter's.
//!
//! What the key fixes is decided here, at compile time. What the registers
//! hold is loaded once, in the entry block, and pinned (`E::pin`) so the
//! arithmetic that derives it stays out of the pixel loop. The shader's one
//! argument is the `Re3`: registers, iterators, VRAM and the Z buffer are
//! all at fixed offsets from it.

use std::mem::offset_of;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64, I8};
use cranelift_codegen::ir::{self, AbiParam, Block, InstBuilder, MemFlagsData, Type, Value};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};

use super::{Fmt, Pat, Prim, ShaderFn, SpanKey, Wid};
use crate::dev::gr2::re3::{self, Re3, FB_H, FB_W, ZBUF_STENCIL_SHIFT};
use crate::dev::ng1::rex3_generic::BAYER_PACKED;

macro_rules! off {
    ($($f:tt)+) => {
        offset_of!(Re3, $($f)+) as i32
    };
}

/// Byte offset of RE3 register `r`.
fn reg_off(r: usize) -> i32 {
    off!(ctx.reg) + 4 * r as i32
}

pub struct Compiler {
    module: JITModule,
    ctx: Context,
    fctx: FunctionBuilderContext,
    n: u32,
}

impl Compiler {
    pub fn new() -> Self {
        let mut flags = settings::builder();
        flags.set("opt_level", "speed").unwrap();
        flags.set("is_pic", "false").unwrap();
        let isa = cranelift_native::builder()
            .expect("host ISA not supported")
            .finish(settings::Flags::new(flags))
            .unwrap();
        let module = JITModule::new(JITBuilder::with_isa(isa, cranelift_module::default_libcall_names()));
        Compiler { ctx: module.make_context(), module, fctx: FunctionBuilderContext::new(), n: 0 }
    }

    /// Compile the shader for a packed key: its entry and code size.
    pub fn compile(&mut self, packed: u64) -> Option<(ShaderFn, u32)> {
        let key = SpanKey::unpack(packed);
        let ptr = self.module.target_config().pointer_type();
        let name = format!("gr2_{packed:016x}_{}", self.n);
        self.n += 1;
        self.ctx.func.signature.params.clear();
        self.ctx.func.signature.returns.clear();
        self.ctx.func.signature.params.push(AbiParam::new(ptr));
        let id = match self.module.declare_function(&name, Linkage::Local, &self.ctx.func.signature) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("GR2 JIT: declare_function: {e}");
                return None;
            }
        };
        {
            let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            b.seal_block(entry);
            let base = b.block_params(entry)[0];
            let mut e = E::new(b, key, base, ptr);
            match key.prim {
                Prim::Shaded => e.shaded(),
                Prim::Flat | Prim::Flat4 => e.flat(key.prim.flat_run()),
            }
            e.b.ins().return_(&[]);
            e.b.seal_all_blocks();
            let fc = self.module.target_config();
            e.b.finalize(fc);
        }
        let disasm = std::env::var_os("GR2_JIT_DISASM").is_some();
        self.ctx.set_disasm(disasm);
        if let Err(err) = self.module.define_function(id, &mut self.ctx) {
            eprintln!("GR2 JIT: define_function failed for {key}: {err}");
            eprintln!("--- Cranelift IR ---\n{}", self.ctx.func.display());
            self.module.clear_context(&mut self.ctx);
            return None;
        }
        let bytes = self.ctx.compiled_code().map(|c| c.code_buffer().len() as u32).unwrap_or(0);
        if disasm {
            if let Some(v) = self.ctx.compiled_code().and_then(|c| c.vcode.as_ref()) {
                eprintln!("GR2 JIT: {key} ({bytes} bytes)\n{v}");
            }
        }
        self.module.clear_context(&mut self.ctx);
        if let Err(err) = self.module.finalize_definitions() {
            eprintln!("GR2 JIT: finalize_definitions: {err}");
            return None;
        }
        let code = self.module.get_finalized_function(id);
        // SAFETY: the function was built with the ShaderFn signature.
        Some((unsafe { std::mem::transmute::<*const u8, ShaderFn>(code) }, bytes))
    }
}

/// A span's iterators (`Re3Context` x, y, z, r, g, b, a).
const ITERS: [usize; 7] = [
    offset_of!(re3::Re3Context, x),
    offset_of!(re3::Re3Context, y),
    offset_of!(re3::Re3Context, z),
    offset_of!(re3::Re3Context, r),
    offset_of!(re3::Re3Context, g),
    offset_of!(re3::Re3Context, b),
    offset_of!(re3::Re3Context, a),
];
const IX: usize = 0;
const IY: usize = 1;
const IZ: usize = 2;
const IR: usize = 3;
const IG: usize = 4;
const IB: usize = 5;
const IA: usize = 6;

/// What the registers hold for the span, loaded and pinned in the entry
/// block (zero where the key does not use it).
#[derive(Clone, Copy)]
struct Span {
    /// Per-pixel steps of the iterators (`step_iterators`).
    step: [Value; 7],
    /// Scissor ∩ screen, inclusive (`clip`).
    xlo: Value,
    xhi: Value,
    ylo: Value,
    yhi: Value,
    /// aux/cid bits and the combined write mask (`aux_and_mask`).
    aux: Value,
    mask: Value,
    /// (PATH << 16) | PATL.
    pat: Value,
    /// CURWID's compared bits.
    wid: Value,
    /// Z write mask; stencil reference & mask, mask, write mask.
    zmask: Value,
    sref: Value,
    sref_m: Value,
    smask: Value,
    swmask: Value,
    /// The 12-, 8- or 4-bit bank the blend reads back (`dst_rgb8`).
    bank: Value,
}

/// One shader under construction.
struct E<'a> {
    b: FunctionBuilder<'a>,
    k: SpanKey,
    base: Value,
    m: MemFlagsData,
}

impl<'a> E<'a> {
    fn new(b: FunctionBuilder<'a>, k: SpanKey, base: Value, _ptr: Type) -> Self {
        E { b, k, base, m: MemFlagsData::trusted() }
    }

    // ── small helpers ────────────────────────────────────────────────────

    fn ld(&mut self, ty: Type, off: i32) -> Value {
        self.b.ins().load(ty, self.m, self.base, off)
    }

    fn st(&mut self, v: Value, off: i32) {
        self.b.ins().store(self.m, v, self.base, off);
    }

    fn reg(&mut self, r: usize) -> Value {
        self.ld(I32, reg_off(r))
    }

    fn i32c(&mut self, v: i64) -> Value {
        self.b.ins().iconst(I32, v as i32 as u32 as i64)
    }

    fn i64c(&mut self, v: i64) -> Value {
        self.b.ins().iconst(I64, v)
    }

    fn var(&mut self, ty: Type, init: Value) -> Variable {
        let v = self.b.declare_var(ty);
        self.b.def_var(v, init);
        v
    }

    fn get(&mut self, v: Variable) -> Value {
        self.b.use_var(v)
    }

    fn set(&mut self, v: Variable, x: Value) {
        self.b.def_var(v, x);
    }

    /// The values as parameters of a fresh block, so what computed them
    /// stays here instead of being rematerialised in the pixel loop (as in
    /// `gr4-jit`): the block is entered from a never-false branch on the
    /// RE3 pointer with the values, and from an unreachable path with zeros.
    fn pin(&mut self, vals: &[Value]) -> Vec<Value> {
        let blk = self.b.create_block();
        let never = self.b.create_block();
        let params: Vec<Value> = vals
            .iter()
            .map(|&v| {
                let ty = self.b.func.dfg.value_type(v);
                self.b.append_block_param(blk, ty)
            })
            .collect();
        let args: Vec<ir::BlockArg> = vals.iter().map(|&v| ir::BlockArg::Value(v)).collect();
        let live = self.b.ins().icmp_imm_s(IntCC::NotEqual, self.base, 0);
        self.b.ins().brif(live, blk, &args, never, &[]);
        self.b.switch_to_block(never);
        self.b.seal_block(never);
        let zeros: Vec<ir::BlockArg> = vals
            .iter()
            .map(|&v| {
                let ty = self.b.func.dfg.value_type(v);
                ir::BlockArg::Value(self.b.ins().iconst(ty, 0))
            })
            .collect();
        self.b.ins().jump(blk, &zeros);
        self.b.switch_to_block(blk);
        self.b.seal_block(blk);
        params
    }

    /// Continue in a fresh block when `cond` holds, else go to `other`.
    fn need(&mut self, cond: Value, other: Block) {
        let next = self.b.create_block();
        self.b.ins().brif(cond, next, &[], other, &[]);
        self.b.switch_to_block(next);
        self.b.seal_block(next);
    }

    /// `for n in start..end` (i32, signed): `body` gets n and the block
    /// that ends the pixel (it may branch there); `tail` runs at the end of
    /// every pixel.
    fn range(&mut self, start: Value, end: Value, body: impl FnOnce(&mut Self, Value, Block), tail: impl FnOnce(&mut Self, Value)) {
        let iv = self.var(I32, start);
        let head = self.b.create_block();
        let bodyb = self.b.create_block();
        let cont = self.b.create_block();
        let exit = self.b.create_block();
        self.b.ins().jump(head, &[]);
        self.b.switch_to_block(head);
        let n = self.get(iv);
        let c = self.b.ins().icmp(IntCC::SignedLessThan, n, end);
        self.b.ins().brif(c, bodyb, &[], exit, &[]);
        self.b.switch_to_block(bodyb);
        self.b.seal_block(bodyb);
        body(self, n, cont);
        self.b.ins().jump(cont, &[]);
        self.b.switch_to_block(cont);
        self.b.seal_block(cont);
        let n = self.get(iv);
        tail(self, n);
        let n = self.get(iv);
        let n1 = self.b.ins().iadd_imm_s(n, 1);
        self.set(iv, n1);
        self.b.ins().jump(head, &[]);
        self.b.seal_block(head);
        self.b.switch_to_block(exit);
        self.b.seal_block(exit);
    }

    /// `(v as i64) << (64 - bits) >> (64 - bits)` of a register.
    fn sext_reg(&mut self, r: usize, bits: i64) -> Value {
        let v = self.reg(r);
        let v = self.b.ins().uextend(I64, v);
        let v = self.b.ins().ishl_imm_s(v, 64 - bits);
        self.b.ins().sshr_imm_s(v, 64 - bits)
    }

    /// `(v >> 14) as i32` of a position iterator.
    fn pos(&mut self, v: Value) -> Value {
        let s = self.b.ins().sshr_imm_s(v, 14);
        self.b.ins().ireduce(I32, s)
    }

    /// `(v >> 11).clamp(0, max) as u32` of a colour iterator.
    fn chan(&mut self, v: Value, max: i64) -> Value {
        let s = self.b.ins().sshr_imm_s(v, 11);
        let zero = self.i64c(0);
        let s = self.b.ins().smax(s, zero);
        let hi = self.i64c(max);
        let s = self.b.ins().smin(s, hi);
        self.b.ins().ireduce(I32, s)
    }

    /// `rop`: 16 raster ops, X11/GL numbering.
    fn rop(&mut self, op: u8, s: Value, d: Value) -> Value {
        match op & 0xF {
            0x0 => self.i32c(0),
            0x1 => self.b.ins().band(s, d),
            0x2 => self.b.ins().band_not(s, d),
            0x3 => s,
            0x4 => self.b.ins().band_not(d, s),
            0x5 => d,
            0x6 => self.b.ins().bxor(s, d),
            0x7 => self.b.ins().bor(s, d),
            0x8 => {
                let v = self.b.ins().bor(s, d);
                self.b.ins().bnot(v)
            }
            0x9 => self.b.ins().bxor_not(s, d),
            0xA => self.b.ins().bnot(d),
            0xB => self.b.ins().bor_not(s, d),
            0xC => self.b.ins().bnot(s),
            0xD => self.b.ins().bor_not(d, s),
            0xE => {
                let v = self.b.ins().band(s, d);
                self.b.ins().bnot(v)
            }
            _ => self.i32c(-1),
        }
    }

    /// The compare of `zs_test` (GL order, "a OP b", unsigned).
    fn compare(&mut self, func: u8, a: Value, b: Value) -> Value {
        let cc = match func & 7 {
            0 => return self.b.ins().iconst(I8, 0),
            1 => IntCC::UnsignedLessThan,
            2 => IntCC::Equal,
            3 => IntCC::UnsignedLessThanOrEqual,
            4 => IntCC::UnsignedGreaterThan,
            5 => IntCC::NotEqual,
            6 => IntCC::UnsignedGreaterThanOrEqual,
            _ => return self.b.ins().iconst(I8, 1),
        };
        self.b.ins().icmp(cc, a, b)
    }

    // ── span setup ───────────────────────────────────────────────────────

    /// Load and pin what the registers hold for this span.
    fn setup(&mut self) -> Span {
        let k = self.k;
        let zero = self.i32c(0);
        // step_iterators.
        let dx = self.sext_reg(re3::REG_DX, 16);
        let dy = self.sext_reg(re3::REG_DY, 16);
        let dzi = self.sext_reg(re3::REG_DZI, 24);
        let dzi = self.b.ins().ishl_imm_s(dzi, 14);
        let dzf = self.reg(re3::REG_DZF);
        let dzf = self.b.ins().uextend(I64, dzf);
        let dz = self.b.ins().bor(dzi, dzf);
        let dr = self.sext_reg(re3::REG_DR, 24);
        let dg = self.sext_reg(re3::REG_DG, 20);
        let db = self.sext_reg(re3::REG_DB, 20);
        let da = self.ld(I64, off!(ctx.da));
        // clip: the scissor registers are 12-bit, so never negative.
        let xmin = self.reg(re3::REG_XMIN);
        let xmax = self.reg(re3::REG_XMAX);
        let ymin = self.reg(re3::REG_YMIN);
        let ymax = self.reg(re3::REG_YMAX);
        let wmax = self.i32c(FB_W as i64 - 1);
        let hmax = self.i32c(FB_H as i64 - 1);
        let xhi = self.b.ins().smin(xmax, wmax);
        let yhi = self.b.ins().smin(ymax, hmax);
        // aux_and_mask.
        let widd = self.reg(re3::REG_WIDDATA);
        let widd = self.b.ins().ishl_imm_s(widd, 28);
        let uaux = self.reg(re3::REG_UAUXDATA);
        let np = self.b.ins().ishl_imm_s(uaux, 24);
        let np = self.b.ins().bor(widd, np);
        let u3 = self.b.ins().band_imm_s(uaux, 3);
        let u3 = self.b.ins().ishl_imm_s(u3, 26);
        let pup = self.reg(re3::REG_PUPDATA);
        let pup = self.b.ins().ishl_imm_s(pup, 24);
        let p = self.b.ins().bor(widd, u3);
        let p = self.b.ins().bor(p, pup);
        let nopup = self.reg(re3::REG_NOPUP);
        let aux = self.b.ins().select(nopup, np, p);
        let am = self.reg(re3::REG_AUXMASK);
        let am = self.b.ins().band_imm_s(am, 0xff);
        let am = self.b.ins().ishl_imm_s(am, 24);
        let pm = self.reg(re3::REG_PIXMASK);
        let mask = self.b.ins().bor(am, pm);
        let pat = if k.pat != Pat::Off {
            let h = self.reg(re3::REG_PATH);
            let h = self.b.ins().ishl_imm_s(h, 16);
            let l = self.reg(re3::REG_PATL);
            self.b.ins().bor(h, l)
        } else {
            zero
        };
        let wid = if k.wid != Wid::Off {
            let c = self.reg(re3::REG_CURWID);
            self.b.ins().band_imm_s(c, k.wid.mask() as i64)
        } else {
            zero
        };
        let zmask = if k.z.is_some() {
            let z = self.ld(I32, off!(ctx.zctl));
            let z = self.b.ins().ushr_imm_s(z, 8);
            self.b.ins().band_imm_s(z, 0x00ff_ffff)
        } else {
            zero
        };
        let (sref, sref_m, smask, swmask) = if k.stencil.is_some() {
            let st = self.ld(I64, off!(ctx.stencil));
            let field = |e: &mut Self, sh: i64| {
                let v = e.b.ins().ushr_imm_s(st, sh);
                let v = e.b.ins().ireduce(I32, v);
                e.b.ins().band_imm_s(v, 0xff)
            };
            let sref = field(self, 4);
            let smask = field(self, 12);
            let swmask = field(self, 20);
            let sref_m = self.b.ins().band(sref, smask);
            (sref, sref_m, smask, swmask)
        } else {
            (zero, zero, zero, zero)
        };
        let bank = if k.blend.is_some() {
            let (m, sh) = match k.fmt {
                Fmt::Rgb12 => (0xfff, 12),
                Fmt::Rgb8 => (0xff, 8),
                Fmt::Rgb4 => (0xf, 4),
                _ => (0, 0),
            };
            if sh == 0 {
                zero
            } else {
                let pm = self.reg(re3::REG_PIXMASK);
                let lo = self.b.ins().band_imm_s(pm, m);
                let none = self.b.ins().icmp_imm_s(IntCC::Equal, lo, 0);
                let shv = self.i32c(sh);
                self.b.ins().select(none, shv, zero)
            }
        } else {
            zero
        };
        let v = self.pin(&[dx, dy, dz, dr, dg, db, da, xmin, xhi, ymin, yhi, aux, mask, pat, wid, zmask, sref, sref_m, smask, swmask, bank]);
        Span {
            step: [v[0], v[1], v[2], v[3], v[4], v[5], v[6]],
            xlo: v[7],
            xhi: v[8],
            ylo: v[9],
            yhi: v[10],
            aux: v[11],
            mask: v[12],
            pat: v[13],
            wid: v[14],
            zmask: v[15],
            sref: v[16],
            sref_m: v[17],
            smask: v[18],
            swmask: v[19],
            bank: v[20],
        }
    }

    /// The span's iterators as `load_iterators` left them.
    fn load_iters(&mut self) -> [Value; 7] {
        let mut out = [self.base; 7];
        for (i, o) in ITERS.iter().enumerate() {
            out[i] = self.ld(I64, off!(ctx) + *o as i32);
        }
        out
    }

    /// The iterators a pixel of this key reads: the position (`pos`), the
    /// colour (`color`: R, and G and B unless the R iterator carries an
    /// index), alpha when it blends, Z when it tests depth.
    fn live(&self, pos: bool) -> Vec<usize> {
        let k = self.k;
        let mut v = Vec::new();
        if pos {
            v.extend([IX, IY]);
        }
        v.push(IR);
        if !k.fmt.ci() {
            v.extend([IG, IB]);
        }
        if k.blend.is_some() {
            v.push(IA);
        }
        if k.z.is_some() {
            v.push(IZ);
        }
        v
    }

    /// Variables for the `live` iterators, `k` steps on from `it0` (the
    /// others are defined but never read).
    fn iter_vars(&mut self, it0: &[Value; 7], live: &[usize], s: &Span, k: Option<Value>) -> [Variable; 7] {
        let vars: Vec<Variable> = (0..7)
            .map(|i| {
                let v = match k {
                    Some(k) if live.contains(&i) => {
                        let d = self.b.ins().imul(k, s.step[i]);
                        self.b.ins().iadd(it0[i], d)
                    }
                    _ => it0[i],
                };
                self.var(I64, v)
            })
            .collect();
        [vars[0], vars[1], vars[2], vars[3], vars[4], vars[5], vars[6]]
    }

    /// `step_iterators`, for the live iterators.
    fn step(&mut self, it: &[Variable; 7], live: &[usize], s: &Span) {
        for &i in live {
            let v = self.get(it[i]);
            let v = self.b.ins().iadd(v, s.step[i]);
            self.set(it[i], v);
        }
    }

    /// The iterators as `steps` (i64) steps from `it0` leave them, back
    /// into the context, and `numpix` = 0. Stepping only adds, so this is
    /// the interpreter's state exactly (both wrap).
    fn finish(&mut self, it0: &[Value; 7], steps: Value, s: &Span) {
        for (i, o) in ITERS.iter().enumerate() {
            let d = self.b.ins().imul(steps, s.step[i]);
            let v = self.b.ins().iadd(it0[i], d);
            self.st(v, off!(ctx) + *o as i32);
        }
        let zero = self.i32c(0);
        self.st(zero, off!(ctx.numpix));
    }

    /// The pixels of a row span at (x0 .. x0 + count, y) inside the
    /// scissor: [lo, hi) as offsets from x0, or to `done` when none are.
    fn row_range(&mut self, s: &Span, x0: Value, y: Value, count: Value, done: Block) -> (Value, Value) {
        let a = self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, y, s.ylo);
        let b = self.b.ins().icmp(IntCC::SignedLessThanOrEqual, y, s.yhi);
        let ok = self.b.ins().band(a, b);
        self.need(ok, done);
        let zero = self.i32c(0);
        let lo = self.b.ins().isub(s.xlo, x0);
        let lo = self.b.ins().smax(lo, zero);
        let hi = self.b.ins().isub(s.xhi, x0);
        let hi = self.b.ins().iadd_imm_s(hi, 1);
        let hi = self.b.ins().smin(hi, count);
        let some = self.b.ins().icmp(IntCC::SignedLessThan, lo, hi);
        self.need(some, done);
        (lo, hi)
    }

    /// The address of pixel (x0 + i, y) (x0 + i, y on screen), as `locate`.
    fn row_addr(&mut self, x0: Value, y: Value, i: Value) -> Value {
        let x = self.b.ins().iadd(x0, i);
        self.locate(x, y).1
    }

    // ── per pixel ────────────────────────────────────────────────────────

    /// `clip`: on to the pixel, or to `skip`.
    fn clip(&mut self, s: &Span, x: Value, y: Value, skip: Block) {
        let a = self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, x, s.xlo);
        let b = self.b.ins().icmp(IntCC::SignedLessThanOrEqual, x, s.xhi);
        let c = self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, y, s.ylo);
        let d = self.b.ins().icmp(IntCC::SignedLessThanOrEqual, y, s.yhi);
        let ab = self.b.ins().band(a, b);
        let cd = self.b.ins().band(c, d);
        let ok = self.b.ins().band(ab, cd);
        // The screen's 0 edges: the scissor minimums are never negative.
        self.need(ok, skip);
    }

    /// `pattern`: bit `31 - (x or n) % 32` set, or to `skip`.
    fn pattern(&mut self, s: &Span, x: Value, n: Value, skip: Block) {
        let idx = if self.k.pat == Pat::Aligned { x } else { n };
        let idx = self.b.ins().band_imm_s(idx, 31);
        // Bit 31 - idx of the pattern, moved to the sign bit.
        let v = self.b.ins().ishl(s.pat, idx);
        let on = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, v, 0);
        self.need(on, skip);
    }

    /// The pixel's offset and VRAM word address.
    fn locate(&mut self, x: Value, y: Value) -> (Value, Value) {
        let x = self.b.ins().uextend(I64, x);
        let y = self.b.ins().uextend(I64, y);
        let row = self.b.ins().imul_imm_s(y, FB_W as i64);
        let off = self.b.ins().iadd(row, x);
        let byte = self.b.ins().ishl_imm_s(off, 2);
        let addr = self.b.ins().iadd(self.base, byte);
        (off, addr)
    }

    /// `wid_ok` on the old word: on, or to `skip`.
    fn wid_ok(&mut self, s: &Span, dst: Value, skip: Block) {
        if self.k.wid == Wid::Off {
            return;
        }
        let w = self.b.ins().ushr_imm_s(dst, 28);
        let w = self.b.ins().band_imm_s(w, self.k.wid.mask() as i64);
        let ok = self.b.ins().icmp(IntCC::Equal, w, s.wid);
        self.need(ok, skip);
    }

    /// `zs_test`: the stencil and depth tests and their writes; on to the
    /// colour when they pass, else to `skip`.
    fn zs_test(&mut self, s: &Span, addr: Value, z: Value, skip: Block) {
        let k = self.k;
        if k.z.is_none() && k.stencil.is_none() {
            return;
        }
        let zoff = off!(zbuf);
        let word = self.b.ins().load(I32, self.m, addr, zoff);
        let mut new = word;
        // 0: stencil failed, 1: depth failed, 2: passed.
        let mut stage = self.i32c(2);
        let mut pass = self.b.ins().iconst(I8, 1);
        let sv = if k.stencil.is_some() {
            let v = self.b.ins().ushr_imm_s(word, ZBUF_STENCIL_SHIFT as i64);
            self.b.ins().band_imm_s(v, 0xff)
        } else {
            word
        };
        if let Some(st) = k.stencil {
            let svm = self.b.ins().band(sv, s.smask);
            let ok = self.compare(st.func, s.sref_m, svm);
            let zero = self.i32c(0);
            stage = self.b.ins().select(ok, stage, zero);
            pass = ok;
        }
        if let Some(zf) = k.z {
            let zi = self.b.ins().sshr_imm_s(z, 14);
            let lo = self.i64c(-0x80_0000);
            let hi = self.i64c(0x7f_ffff);
            let zi = self.b.ins().smax(zi, lo);
            let zi = self.b.ins().smin(zi, hi);
            let zi = self.b.ins().ireduce(I32, zi);
            let zv = self.b.ins().band_imm_s(zi, 0x00ff_ffff);
            let a = self.b.ins().bxor_imm_s(zv, 0x80_0000);
            let old = self.b.ins().band_imm_s(word, 0x00ff_ffff);
            let bb = self.b.ins().bxor_imm_s(old, 0x80_0000);
            let zok = self.compare(zf, a, bb);
            // Only tested when the stencil passed.
            let tested_ok = self.b.ins().band(pass, zok);
            let fail_here = self.b.ins().band_not(pass, zok);
            let one = self.i32c(1);
            stage = self.b.ins().select(fail_here, one, stage);
            let keep = self.b.ins().band_not(new, s.zmask);
            let put = self.b.ins().band(zv, s.zmask);
            let written = self.b.ins().bor(keep, put);
            new = self.b.ins().select(tested_ok, written, new);
            pass = tested_ok;
        }
        if let Some(st) = k.stencil {
            let ops = [st.fail, st.zfail, st.zpass];
            let mut nv = [sv; 3];
            for (i, &op) in ops.iter().enumerate() {
                nv[i] = match op {
                    1 => {
                        let v = self.b.ins().bnot(sv);
                        self.b.ins().band_imm_s(v, 0xff)
                    }
                    2 => self.i32c(0),
                    3 => s.sref,
                    4 => {
                        let v = self.b.ins().iadd_imm_s(sv, 1);
                        let m = self.i32c(0xff);
                        self.b.ins().umin(v, m)
                    }
                    5 => {
                        let v = self.b.ins().iadd_imm_s(sv, -1);
                        let z = self.b.ins().icmp_imm_s(IntCC::Equal, sv, 0);
                        let zero = self.i32c(0);
                        self.b.ins().select(z, zero, v)
                    }
                    _ => sv,
                };
            }
            let is0 = self.b.ins().icmp_imm_s(IntCC::Equal, stage, 0);
            let is1 = self.b.ins().icmp_imm_s(IntCC::Equal, stage, 1);
            let v12 = self.b.ins().select(is1, nv[1], nv[2]);
            let v = self.b.ins().select(is0, nv[0], v12);
            let keep = self.b.ins().band_not(sv, s.swmask);
            let put = self.b.ins().band(v, s.swmask);
            let sv2 = self.b.ins().bor(keep, put);
            let sv2 = self.b.ins().ishl_imm_s(sv2, ZBUF_STENCIL_SHIFT as i64);
            let low = self.b.ins().band_imm_s(new, 0x00ff_ffff);
            new = self.b.ins().bor(low, sv2);
        }
        self.b.ins().store(self.m, new, addr, zoff);
        self.need(pass, skip);
    }

    /// `color`: the iterators as pixel bits at (x, y).
    fn color(&mut self, it: &[Variable; 7], x: Value, y: Value) -> Value {
        let r = self.get(it[IR]);
        match self.k.fmt {
            Fmt::Ci12 => {
                let i = self.chan(r, 0xfff);
                let h = self.b.ins().ishl_imm_s(i, 12);
                self.b.ins().bor(i, h)
            }
            Fmt::Ci8 => {
                let i = self.chan(r, 0xff);
                let h = self.b.ins().ishl_imm_s(i, 8);
                self.b.ins().bor(i, h)
            }
            _ => {
                let rgb = self.rgb8(it);
                let c = [rgb[0], rgb[1], rgb[2]];
                self.pack(c, x, y)
            }
        }
    }

    /// The three colour iterators as 8-bit channels.
    fn rgb8(&mut self, it: &[Variable; 7]) -> [Value; 3] {
        let mut out = [self.i32c(0); 3];
        for (k, i) in [IR, IG, IB].iter().enumerate() {
            let v = self.get(it[*i]);
            out[k] = self.chan(v, 255);
        }
        out
    }

    /// `color_blended`: `color`, blended with the frame-buffer word `dst`
    /// when the key blends.
    fn color_blended(&mut self, it: &[Variable; 7], s: &Span, dst: Value, x: Value, y: Value) -> Value {
        let Some(bl) = self.k.blend else {
            return self.color(it, x, y);
        };
        let src = self.rgb8(it);
        let av = self.get(it[IA]);
        let a = self.chan(av, 255);
        let d = self.dst_rgb8(s, dst);
        let c255 = self.i32c(255);
        let factor = |e: &mut Self, code: u8, k: usize, other: &[Value; 3]| -> Value {
            match code {
                0 => e.i32c(0),
                2 => other[k],
                3 => e.b.ins().isub(c255, other[k]),
                4 => a,
                5 => e.b.ins().isub(c255, a),
                _ => c255,
            }
        };
        let mut out = [c255; 3];
        for k in 0..3 {
            let fs = factor(self, bl.src, k, &d);
            let fd = factor(self, bl.dst, k, &src);
            let p = self.b.ins().imul(src[k], fs);
            let q = self.b.ins().imul(d[k], fd);
            let v = self.b.ins().iadd(p, q);
            let v = self.b.ins().iadd_imm_s(v, 127);
            let v = self.b.ins().udiv_imm_s(v, 255);
            out[k] = self.b.ins().umin(v, c255);
        }
        self.pack(out, x, y)
    }

    /// `dst_rgb8`: the frame-buffer colour as 8-bit channels.
    fn dst_rgb8(&mut self, s: &Span, p: Value) -> [Value; 3] {
        match self.k.fmt {
            Fmt::Rgb12 => {
                let v = self.b.ins().ushr(p, s.bank);
                let mut out = [v; 3];
                for (k, o) in out.iter_mut().enumerate() {
                    let c = self.b.ins().ushr_imm_s(v, 4 * k as i64);
                    let c = self.b.ins().band_imm_s(c, 0xf);
                    *o = self.b.ins().imul_imm_s(c, 17);
                }
                out
            }
            Fmt::Rgb8 => {
                let v = self.b.ins().ushr(p, s.bank);
                let c3 = |e: &mut Self, sh: i64| {
                    let c = e.b.ins().ushr_imm_s(v, sh);
                    let c = e.b.ins().band_imm_s(c, 7);
                    let a = e.b.ins().ishl_imm_s(c, 5);
                    let b = e.b.ins().ishl_imm_s(c, 2);
                    let d = e.b.ins().ushr_imm_s(c, 1);
                    let ab = e.b.ins().bor(a, b);
                    e.b.ins().bor(ab, d)
                };
                let r = c3(self, 5);
                let g = c3(self, 0);
                let b = self.b.ins().ushr_imm_s(v, 3);
                let b = self.b.ins().band_imm_s(b, 3);
                let b = self.b.ins().imul_imm_s(b, 0x55);
                [r, g, b]
            }
            Fmt::Rgb4 => {
                let v = self.b.ins().ushr(p, s.bank);
                let r = self.b.ins().ushr_imm_s(v, 3);
                let r = self.b.ins().band_imm_s(r, 1);
                let r = self.b.ins().imul_imm_s(r, 255);
                let g = self.b.ins().ushr_imm_s(v, 1);
                let g = self.b.ins().band_imm_s(g, 3);
                let g = self.b.ins().imul_imm_s(g, 85);
                let b = self.b.ins().band_imm_s(v, 1);
                let b = self.b.ins().imul_imm_s(b, 255);
                [r, g, b]
            }
            _ => {
                let r = self.b.ins().band_imm_s(p, 0xff);
                let g = self.b.ins().ushr_imm_s(p, 8);
                let g = self.b.ins().band_imm_s(g, 0xff);
                let b = self.b.ins().ushr_imm_s(p, 16);
                let b = self.b.ins().band_imm_s(b, 0xff);
                [r, g, b]
            }
        }
    }

    /// `pack`: 8-bit channels into the frame-buffer format.
    fn pack(&mut self, c: [Value; 3], x: Value, y: Value) -> Value {
        let [r, g, b] = c;
        match self.k.fmt {
            Fmt::Rgb12 => {
                let v = if self.k.dither {
                    // rgb24_to_rgb12_dither(bayer_pack(rgb, x, y)).
                    let yi = self.b.ins().band_imm_s(y, 3);
                    let yi = self.b.ins().ishl_imm_s(yi, 2);
                    let xi = self.b.ins().band_imm_s(x, 3);
                    let idx = self.b.ins().bor(yi, xi);
                    let sh = self.b.ins().ishl_imm_s(idx, 2);
                    let sh = self.b.ins().uextend(I64, sh);
                    let tbl = self.i64c(BAYER_PACKED as i64);
                    let t = self.b.ins().ushr(tbl, sh);
                    let t = self.b.ins().ireduce(I32, t);
                    let bayer = self.b.ins().band_imm_s(t, 0xf);
                    let mut d = [r; 3];
                    for (k, &ch) in [r, g, b].iter().enumerate() {
                        let hi = self.b.ins().ushr_imm_s(ch, 4);
                        let sc = self.b.ins().isub(ch, hi);
                        let dc = self.b.ins().ushr_imm_s(sc, 4);
                        let dc = self.b.ins().band_imm_s(dc, 15);
                        let frac = self.b.ins().band_imm_s(sc, 0xf);
                        let up = self.b.ins().icmp(IntCC::UnsignedGreaterThan, frac, bayer);
                        let inc = self.b.ins().iadd_imm_s(dc, 1);
                        let m15 = self.i32c(15);
                        let inc = self.b.ins().umin(inc, m15);
                        d[k] = self.b.ins().select(up, inc, dc);
                    }
                    let g4 = self.b.ins().ishl_imm_s(d[1], 4);
                    let b4 = self.b.ins().ishl_imm_s(d[2], 8);
                    let v = self.b.ins().bor(d[0], g4);
                    self.b.ins().bor(v, b4)
                } else {
                    let r4 = self.b.ins().ushr_imm_s(r, 4);
                    let g4 = self.b.ins().band_imm_s(g, 0xf0);
                    let b4 = self.b.ins().ushr_imm_s(b, 4);
                    let b4 = self.b.ins().ishl_imm_s(b4, 8);
                    let v = self.b.ins().bor(r4, g4);
                    self.b.ins().bor(v, b4)
                };
                let h = self.b.ins().ishl_imm_s(v, 12);
                self.b.ins().bor(v, h)
            }
            Fmt::Rgb8 => {
                // (rgb & 0xe0) | ((rgb >> 19) & 0x18) | ((rgb >> 13) & 0x07)
                let rv = self.b.ins().band_imm_s(r, 0xe0);
                let bv = self.b.ins().ushr_imm_s(b, 3);
                let bv = self.b.ins().band_imm_s(bv, 0x18);
                let gv = self.b.ins().ushr_imm_s(g, 5);
                let v = self.b.ins().bor(rv, bv);
                let v = self.b.ins().bor(v, gv);
                let h = self.b.ins().ishl_imm_s(v, 8);
                self.b.ins().bor(v, h)
            }
            Fmt::Rgb4 => {
                // ((rgb >> 4) & 0x8) | ((rgb >> 13) & 0x6) | ((rgb >> 23) & 0x1)
                let rv = self.b.ins().ushr_imm_s(r, 4);
                let rv = self.b.ins().band_imm_s(rv, 0x8);
                let gv = self.b.ins().ushr_imm_s(g, 5);
                let gv = self.b.ins().band_imm_s(gv, 0x6);
                let bv = self.b.ins().ushr_imm_s(b, 7);
                let v = self.b.ins().bor(rv, gv);
                let v = self.b.ins().bor(v, bv);
                let h = self.b.ins().ishl_imm_s(v, 4);
                self.b.ins().bor(v, h)
            }
            _ => {
                let g8 = self.b.ins().ishl_imm_s(g, 8);
                let b8 = self.b.ins().ishl_imm_s(b, 16);
                let v = self.b.ins().bor(r, g8);
                self.b.ins().bor(v, b8)
            }
        }
    }

    /// `store`: the raster op on the colour/aux bits, then a masked write
    /// over the old word `dst`.
    fn store(&mut self, s: &Span, addr: Value, dst: Value, c: Value) {
        let src = self.b.ins().bor(s.aux, c);
        let v = self.rop(self.k.rop, src, dst);
        let keep = self.b.ins().band_not(dst, s.mask);
        let put = self.b.ins().band(v, s.mask);
        let new = self.b.ins().bor(keep, put);
        self.b.ins().store(self.m, new, addr, off!(vram));
    }

    // ── spans ────────────────────────────────────────────────────────────

    /// One SHADED pixel at (x, y), the n-th of the span, its word at `addr`.
    fn shaded_pixel(&mut self, s: &Span, it: &[Variable; 7], x: Value, y: Value, n: Value, addr: Value, skip: Block) {
        if self.k.pat != Pat::Off {
            self.pattern(s, x, n, skip);
        }
        let dst = self.b.ins().load(I32, self.m, addr, off!(vram));
        self.wid_ok(s, dst, skip);
        let z = self.get(it[IZ]);
        self.zs_test(s, addr, z, skip);
        let c = self.color_blended(it, s, dst, x, y);
        self.store(s, addr, dst, c);
    }

    /// `draw_shaded_span`. A row span (DX = 1.0, DY = 0, as GL fills
    /// send) is clipped to the scissor once and walks a pointer; any other
    /// steps the position per pixel and clips each pixel.
    fn shaded(&mut self) {
        let it0 = self.load_iters();
        let count = self.ld(I32, off!(ctx.numpix));
        let s = self.setup();
        let row = self.b.create_block();
        let other = self.b.create_block();
        let done = self.b.create_block();
        let dx1 = self.b.ins().icmp_imm_s(IntCC::Equal, s.step[IX], 1 << 14);
        let dy0 = self.b.ins().icmp_imm_s(IntCC::Equal, s.step[IY], 0);
        let horiz = self.b.ins().band(dx1, dy0);
        self.b.ins().brif(horiz, row, &[], other, &[]);

        // A row: x = X + n exactly (the start fraction went to y).
        self.b.switch_to_block(row);
        self.b.seal_block(row);
        let x0 = self.pos(it0[IX]);
        let y = self.pos(it0[IY]);
        let (lo, hi) = self.row_range(&s, x0, y, count, done);
        let live = self.live(false);
        let lo64 = self.b.ins().sextend(I64, lo);
        let it = self.iter_vars(&it0, &live, &s, Some(lo64));
        let a0 = self.row_addr(x0, y, lo);
        let addr = self.var(I64, a0);
        self.range(
            lo,
            hi,
            |e, n, skip| {
                let x = e.b.ins().iadd(x0, n);
                let a = e.get(addr);
                e.shaded_pixel(&s, &it, x, y, n, a, skip);
            },
            |e, _| {
                let a = e.get(addr);
                let a = e.b.ins().iadd_imm_s(a, 4);
                e.set(addr, a);
                e.step(&it, &live, &s);
            },
        );
        self.b.ins().jump(done, &[]);

        // Anything else: the interpreter's walk.
        self.b.switch_to_block(other);
        self.b.seal_block(other);
        let live = self.live(true);
        let it = self.iter_vars(&it0, &live, &s, None);
        let zero = self.i32c(0);
        self.range(
            zero,
            count,
            |e, n, skip| {
                let xv = e.get(it[IX]);
                let yv = e.get(it[IY]);
                let x = e.pos(xv);
                let y = e.pos(yv);
                e.clip(&s, x, y, skip);
                let (_, addr) = e.locate(x, y);
                e.shaded_pixel(&s, &it, x, y, n, addr, skip);
            },
            |e, _| e.step(&it, &live, &s),
        );
        self.b.ins().jump(done, &[]);

        self.b.switch_to_block(done);
        self.b.seal_block(done);
        let steps = self.b.ins().uextend(I64, count);
        self.finish(&it0, steps, &s);
    }

    /// `draw_flat_span`: `numpix` consecutive pixels from the start, the
    /// colour stepping once every `run` pixels (and per pixel when 12-bit
    /// RGB dithers). Only the pixels inside the scissor are visited; the
    /// colour starts where the interpreter's would be at the first of them.
    fn flat(&mut self, run: u32) {
        let it0 = self.load_iters();
        let count = self.ld(I32, off!(ctx.numpix));
        let s = self.setup();
        let done = self.b.create_block();
        let x0 = self.pos(it0[IX]);
        let y = self.pos(it0[IY]);
        let per_pixel = self.k.fmt == Fmt::Rgb12 && self.k.dither;
        let (lo, hi) = self.row_range(&s, x0, y, count, done);
        let live = self.live(false);
        let live: Vec<usize> = live.into_iter().filter(|i| matches!(*i, IR | IG | IB)).collect();
        // Colour steps before pixel lo, and pixels until the next one.
        let k0 = self.b.ins().udiv_imm_s(lo, run as i64);
        let done_px = self.b.ins().imul_imm_s(k0, run as i64);
        let rem = self.b.ins().isub(lo, done_px);
        let k0 = self.b.ins().uextend(I64, k0);
        let it = self.iter_vars(&it0, &live, &s, Some(k0));
        let runc = self.i32c(run as i64);
        let l0 = self.b.ins().isub(runc, rem);
        let left = self.var(I32, l0);
        let xs = self.b.ins().iadd(x0, lo);
        let c0 = self.color(&it, xs, y);
        let cv = self.var(I32, c0);
        let a0 = self.row_addr(x0, y, lo);
        let addr = self.var(I64, a0);
        self.range(
            lo,
            hi,
            |e, n, skip| {
                let x = e.b.ins().iadd(x0, n);
                if e.k.pat != Pat::Off {
                    e.pattern(&s, x, n, skip);
                }
                let a = e.get(addr);
                let dst = e.b.ins().load(I32, e.m, a, off!(vram));
                e.wid_ok(&s, dst, skip);
                let c = if per_pixel { e.color(&it, x, y) } else { e.get(cv) };
                e.store(&s, a, dst, c);
            },
            |e, n| {
                let a = e.get(addr);
                let a = e.b.ins().iadd_imm_s(a, 4);
                e.set(addr, a);
                let l = e.get(left);
                let l = e.b.ins().iadd_imm_s(l, -1);
                e.set(left, l);
                let due = e.b.ins().icmp_imm_s(IntCC::Equal, l, 0);
                let then = e.b.create_block();
                let join = e.b.create_block();
                e.b.ins().brif(due, then, &[], join, &[]);
                e.b.switch_to_block(then);
                e.b.seal_block(then);
                let r = e.i32c(run as i64);
                e.set(left, r);
                e.step(&it, &live, &s);
                if !per_pixel {
                    let x = e.b.ins().iadd(x0, n);
                    let x1 = e.b.ins().iadd_imm_s(x, 1);
                    let c = e.color(&it, x1, y);
                    e.set(cv, c);
                }
                e.b.ins().jump(join, &[]);
                e.b.switch_to_block(join);
                e.b.seal_block(join);
            },
        );
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        self.b.seal_block(done);
        let steps = self.b.ins().udiv_imm_s(count, run as i64);
        let steps = self.b.ins().uextend(I64, steps);
        self.finish(&it0, steps, &s);
    }
}
