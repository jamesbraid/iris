// KONA's pixel store as hooks on the shared batch kernels.
//
// A record is one thing the store's writers do to the planes; its kind (word 37) says which:
//   0 triangle   back/raster/pixel.rs `run_tri` with no texture, fog, smoothing or multisampling
//   1 X fill     back/x/fill.rs `rows`: one clipped box of a request
//   2 box        the fill rule's box (`Store::apply(FillBox)`), a point being one pixel
//   3 bitmap     `Store::apply(Bitmap)`
//   4 clear      back/raster/pixel.rs `run` on a `ClearJob`
// The CPU path is the reference: every integer operation and every f32 product that feeds a
// sum is written so the result is the same word. Signed `%` and `/` never see a negative operand
// (the GL backend leaves them undefined there).
//
// Defines: WIDTH, HEIGHT (the managed area), PLANE (= WIDTH * HEIGHT).
//
// Planes live in up to four storage buffers (chunks), because one binding can be smaller than
// all of them (llvmpipe: 128 MiB) and indexing a runtime array of buffers is not portable to the
// GL backend. A plane is named by a reference: chunk in bits 31:28, word offset in the chunk
// below.

#include "wide.wgsl"
#include "kona_texture.wgsl"
#include "kona_fragment.wgsl"

const REC_W: u32 = 84u;

struct Rec {
    slot: u32,
    // 1.0 as bits: the multiply that keeps a product apart from a sum.
    one: u32,
    w: array<u32, REC_W>,
}

struct Slot {
    w: array<u32, 128>,
}

@group(1) @binding(0) var<storage, read> recs: array<Rec>;
@group(1) @binding(1) var<storage, read> slots: array<Slot>;
@group(2) @binding(0) var<storage, read_write> pl0: array<u32>;
@group(2) @binding(1) var<storage, read_write> pl1: array<u32>;
@group(2) @binding(2) var<storage, read_write> pl2: array<u32>;
@group(2) @binding(3) var<storage, read_write> pl3: array<u32>;
// Host data records carry (group 3 also holds the texture memory).
@group(3) @binding(3) var<storage, read> payload: array<u32>;

fn ld(r: u32, i: u32) -> u32 {
    let o = (r & 0x0fffffffu) + i;
    switch r >> 28u {
        case 0u: { return pl0[o]; }
        case 1u: { return pl1[o]; }
        case 2u: { return pl2[o]; }
        default: { return pl3[o]; }
    }
}

fn st(r: u32, i: u32, v: u32) {
    let o = (r & 0x0fffffffu) + i;
    switch r >> 28u {
        case 0u: { pl0[o] = v; }
        case 1u: { pl1[o] = v; }
        case 2u: { pl2[o] = v; }
        default: { pl3[o] = v; }
    }
}

const K_TRI: u32 = 0u;
const K_XFILL: u32 = 1u;
const K_BOX: u32 = 2u;
const K_BITMAP: u32 = 3u;
const K_CLEAR: u32 = 4u;
const K_ACCUM: u32 = 5u;
const K_CLEARACC: u32 = 6u;
const K_STENCILW: u32 = 7u;
const K_IMAGE: u32 = 8u;
const R_KIND: u32 = 37u;

// Triangle record words.
const T_A: u32 = 0u;     // 3 x i32 (edge x steps, multiples of 8)
const T_B: u32 = 3u;     // 3 x i32
const T_C: u32 = 6u;     // 3 x I64
const T_TIE: u32 = 12u;  // bit k: edge k owns its pixels
const T_ORG: u32 = 13u;  // job origin x, y
const T_AT: u32 = 15u;   // 4 x f32
const T_DX: u32 = 19u;   // 4 x f32
const T_DY: u32 = 23u;   // 4 x f32
const T_Z: u32 = 27u;    // at, dx, dy: 3 x I64 (32.16)
const T_RECT: u32 = 33u; // x0, y0, x1, y1 (far corner exclusive); the box of every kind but X fill
const T_TEX: u32 = 38u;  // 4 x (at, dx, dy) f32: s/w, t/w, r/w, q/w
const T_FOG: u32 = 50u;  // 2 x (at, dx, dy) f32: fog coordinate / w, 1 / w
const T_LD: u32 = 56u;   // at, dx, dy f32: distance along a line
const T_COV: u32 = 59u;  // smoothing: kind | inner << 2 | n << 8 | line << 16, then 8 x (nx, ny, d)
                         // for edges or x, y, r for a disc

// Draw-state slot words (triangles and clears).
const S_FLAGS: u32 = 0u;
const F_DEPTH_PLANE: u32 = 1u;
const F_DEPTH_TEST: u32 = 2u;
const F_DEPTH_MASK: u32 = 4u;
const F_ALPHA: u32 = 8u;
const F_STENCIL: u32 = 16u;
const F_BLEND: u32 = 32u;
const F_LOGIC: u32 = 64u;
const F_OWN: u32 = 128u;
const F_STIPPLE: u32 = 256u;
const F_CMASK: u32 = 512u;
const F_TEX: u32 = 1024u;
const F_LSTIP: u32 = 2048u;
const S_NCOL: u32 = 1u;
const S_COL0: u32 = 2u;   // plane references
const S_COL1: u32 = 3u;
const S_DEPTH: u32 = 4u;
const S_STENCIL: u32 = 5u;
const S_DFUNC: u32 = 6u;
const S_AFUNC: u32 = 7u;
const S_AREF: u32 = 8u;
const S_SFUNC: u32 = 9u;
const S_SREF: u32 = 10u;
const S_SRMASK: u32 = 11u;
const S_SWMASK: u32 = 12u;
const S_SFAIL: u32 = 13u;
const S_ZFAIL: u32 = 14u;
const S_ZPASS: u32 = 15u;
const S_BSRC: u32 = 16u;
const S_BDST: u32 = 17u;
const S_BEQ: u32 = 18u;
const S_BK: u32 = 19u;    // 4 words
const S_LOGICOP: u32 = 23u;
const S_KEEP: u32 = 24u;
const S_OWN_REF: u32 = 25u;
const S_OWN_MASK: u32 = 26u;
const S_DID: u32 = 27u;
const S_WORG: u32 = 28u;  // window origin x, y (stipple)
const S_STIP: u32 = 32u;  // 32 rows
const S_TEXD: u32 = 64u;  // the texture descriptor's word offset in tex_descs
const S_FOGC: u32 = 66u;  // fog curve (0 none, 1 linear, 2 exp, 3 exp2), a, b, then colour r, g, b
const S_LSTIP: u32 = 72u; // line stipple pattern, repeat
// Multisampling (0 words when the draw has no samples): count, the mask's coverage word,
// alpha_to_mask | alpha_to_one << 1, the words of the colour, depth and stencil runs, then 8 sample
// offsets (x | y << 8, eighths of a pixel), 6 colour, 6 depth and 2 stencil plane references.
// Accumulation (kinds 5 and 6): the accumulation planes' references (up to 8, 4 for an operation),
// the colour source's (0xffffffff: none), and for a clear the 8 values.
const S_ACC: u32 = 112u;
const S_MS: u32 = 80u;
const S_MS_AT: u32 = 86u;
const S_MS_COL: u32 = 94u;
const S_MS_DEPTH: u32 = 100u;
const S_MS_STEN: u32 = 106u;

const FULL: u32 = 8191u;
const DEPTH_MAX: u32 = 4294967040u;

var<private> nc: f32;

fn p(x: f32) -> f32 {
    return x * nc;
}

fn rw(pi: u32, i: u32) -> u32 {
    return recs[pi].w[i];
}

fn r64(pi: u32, i: u32) -> I64 {
    return I64(recs[pi].w[i], recs[pi].w[i + 1u]);
}

fn cmp(f: u32, n: u32, o: u32) -> bool {
    switch f {
        case 0u: { return false; }
        case 1u: { return n < o; }
        case 2u: { return n == o; }
        case 3u: { return n <= o; }
        case 4u: { return n > o; }
        case 5u: { return n != o; }
        case 6u: { return n >= o; }
        default: { return true; }
    }
}

fn stencil_apply(op: u32, s: u32, r: u32) -> u32 {
    switch op {
        case 1u: { return 0u; }
        case 2u: { return r & 0xffu; }
        case 3u: { return min(s + 1u, 255u); }
        case 4u: { return select(s - 1u, 0u, s == 0u); }
        case 5u: { return (~s) & 0xffu; }
        default: { return s; }
    }
}

fn unpack(w: u32) -> vec4<u32> {
    return vec4<u32>(
        (((w >> 16u) & 0xffu) * FULL + 127u) / 255u,
        (((w >> 8u) & 0xffu) * FULL + 127u) / 255u,
        ((w & 0xffu) * FULL + 127u) / 255u,
        (((w >> 24u) & 0xffu) * FULL + 127u) / 255u,
    );
}

fn to8(c: u32) -> u32 {
    return (c * 255u + 4095u) / FULL;
}

fn pack(c: vec4<u32>) -> u32 {
    return (to8(c.w) << 24u) | (to8(c.x) << 16u) | (to8(c.y) << 8u) | to8(c.z);
}

fn weights(f: u32, s: vec4<u32>, d: vec4<u32>, k: vec4<u32>) -> vec4<u32> {
    let full = vec4<u32>(FULL);
    switch f {
        case 0u: { return vec4<u32>(0u); }
        case 1u: { return full; }
        case 2u: { return s; }
        case 3u: { return full - s; }
        case 4u: { return vec4<u32>(s.w); }
        case 5u: { return vec4<u32>(FULL - s.w); }
        case 6u: { return vec4<u32>(d.w); }
        case 7u: { return vec4<u32>(FULL - d.w); }
        case 8u: { return d; }
        case 9u: { return full - d; }
        case 10u: {
            let m = min(s.w, FULL - d.w);
            return vec4<u32>(m, m, m, FULL);
        }
        case 11u: { return k; }
        case 12u: { return full - k; }
        case 13u: { return vec4<u32>(k.w); }
        default: { return vec4<u32>(FULL - k.w); }
    }
}

fn rounded(v: i32) -> u32 {
    return min((u32(max(v, 0)) + FULL / 2u) / FULL, FULL);
}

fn blend(src: vec4<u32>, dst: vec4<u32>, sf: u32, df: u32, eq: u32, k: vec4<u32>) -> vec4<u32> {
    let sw = weights(sf, src, dst, k);
    let dw = weights(df, src, dst, k);
    switch eq {
        case 1u: {
            let a = vec4<i32>(src * sw);
            let b = vec4<i32>(dst * dw);
            let v = a - b;
            return vec4<u32>(rounded(v.x), rounded(v.y), rounded(v.z), rounded(v.w));
        }
        case 2u: {
            let a = vec4<i32>(src * sw);
            let b = vec4<i32>(dst * dw);
            let v = b - a;
            return vec4<u32>(rounded(v.x), rounded(v.y), rounded(v.z), rounded(v.w));
        }
        case 3u: { return min(src, dst); }
        case 4u: { return max(src, dst); }
        default: {
            return min((src * sw + dst * dw + vec4<u32>(FULL / 2u)) / FULL, vec4<u32>(FULL));
        }
    }
}

fn logic(op: u32, s: u32, d: u32) -> u32 {
    switch op {
        case 0u: { return 0u; }
        case 1u: { return s & d; }
        case 2u: { return s & ~d; }
        case 3u: { return s; }
        case 4u: { return ~s & d; }
        case 5u: { return d; }
        case 6u: { return s ^ d; }
        case 7u: { return s | d; }
        case 8u: { return ~(s | d); }
        case 9u: { return ~(s ^ d); }
        case 10u: { return ~d; }
        case 11u: { return s | ~d; }
        case 12u: { return ~s; }
        case 13u: { return ~s | d; }
        case 14u: { return ~(s & d); }
        default: { return 0xffffffffu; }
    }
}

fn colour_out(si: u32, src: vec4<u32>, old: u32) -> u32 {
    let fl = slots[si].w[S_FLAGS];
    var c = src;
    if (fl & F_BLEND) != 0u {
        c = blend(src, unpack(old), slots[si].w[S_BSRC], slots[si].w[S_BDST], slots[si].w[S_BEQ],
            vec4<u32>(slots[si].w[S_BK], slots[si].w[S_BK + 1u], slots[si].w[S_BK + 2u], slots[si].w[S_BK + 3u]));
    }
    var word = pack(c);
    if (fl & F_LOGIC) != 0u {
        word = logic(slots[si].w[S_LOGICOP], word, old);
    }
    if (fl & F_CMASK) != 0u {
        let keep = slots[si].w[S_KEEP];
        word = (word & ~keep) | (old & keep);
    }
    return word;
}

// The depth plane's integer part: (at + dx * i + dy * j) >> 16, wrapping,
// clamped to 0..=DEPTH_MAX.
fn depth_at(pi: u32, i: i32, j: i32) -> u32 {
    var v = r64(pi, T_Z);
    v = i64_madd(v, r64(pi, T_Z + 2u), i);
    v = i64_madd(v, r64(pi, T_Z + 4u), j);
    return depth_clamp(v);
}

// The depth at sample offset (ox, oy) eighths from pixel (i, j): the plane's step to the sample is
// divided by 8 truncating toward zero, as the CPU's signed division does.
fn depth_at_sample(pi: u32, i: i32, j: i32, ox: i32, oy: i32) -> u32 {
    var v = r64(pi, T_Z);
    v = i64_madd(v, r64(pi, T_Z + 2u), i);
    v = i64_madd(v, r64(pi, T_Z + 4u), j);
    var q = i64_madd(i64_madd(I64(0u, 0u), r64(pi, T_Z + 2u), ox), r64(pi, T_Z + 4u), oy);
    if i64_is_neg(q) {
        q = i64_neg(u64_shr(i64_neg(q), 3u));
    } else {
        q = u64_shr(q, 3u);
    }
    return depth_clamp(i64_add(v, q));
}

fn depth_clamp(v: I64) -> u32 {
    let s = i64_shr(v, 16u);
    if i64_is_neg(s) {
        return 0u;
    }
    if s.hi != 0u || s.lo > DEPTH_MAX {
        return DEPTH_MAX;
    }
    return s.lo;
}

fn attr(pi: u32, k: u32, i: f32, j: f32) -> u32 {
    let at = bitcast<f32>(rw(pi, T_AT + k));
    let dx = bitcast<f32>(rw(pi, T_DX + k));
    let dy = bitcast<f32>(rw(pi, T_DY + k));
    let v = (at + p(dx * i)) + p(dy * j);
    return u32(clamp(v, 0.0, 8191.0) + 0.5);
}

// The samples of pixel (px, py) inside the triangle, as bits: each at its offset in eighths from the
// pixel's corner. The edges' steps are multiples of 8, so the step to a sample divides exactly.
fn sample_mask(pi: u32, si: u32, px: i32, py: i32) -> u32 {
    let count = slots[si].w[S_MS];
    let tie = rw(pi, T_TIE);
    var mask = 0u;
    for (var k = 0u; k < count; k++) {
        let at = slots[si].w[S_MS_AT + k];
        let ox = i32(at & 0xffu);
        let oy = i32(at >> 8u);
        var inside = true;
        for (var e = 0u; e < 3u; e++) {
            let a = bitcast<i32>(rw(pi, T_A + e));
            let b = bitcast<i32>(rw(pi, T_B + e));
            var v = r64(pi, T_C + 2u * e);
            v = i64_add(v, i64_mul_i32(a, px));
            v = i64_add(v, i64_mul_i32(b, py));
            v = i64_add(v, i64_shr(i64_add(i64_mul_i32(a, ox), i64_mul_i32(b, oy)), 3u));
            let pos = !i64_is_neg(v) && !i64_is_zero(v);
            let own = i64_is_zero(v) && ((tie >> e) & 1u) != 0u;
            if !(pos || own) {
                inside = false;
            }
        }
        mask |= select(0u, 1u << k, inside);
    }
    return mask;
}

// Whether the sample of pixel (px, py) is inside edge k, a tie going to a right or top edge.
fn inside_edge(pi: u32, k: u32, px: i32, py: i32) -> bool {
    let tie = rw(pi, T_TIE);
    var e = r64(pi, T_C + 2u * k);
    e = i64_add(e, i64_mul_i32(bitcast<i32>(rw(pi, T_A + k)), px));
    e = i64_add(e, i64_mul_i32(bitcast<i32>(rw(pi, T_B + k)), py));
    let pos = !i64_is_neg(e) && !i64_is_zero(e);
    let own = i64_is_zero(e) && ((tie >> k) & 1u) != 0u;
    return pos || own;
}

fn covered(pi: u32, px: i32, py: i32) -> bool {
    for (var k = 0u; k < 3u; k++) {
        if !inside_edge(pi, k, px, py) {
            return false;
        }
    }
    return true;
}

// The share of the pixel a smoothed triangle covers (raster/smooth.rs `coverage`), 0 when the
// pixel is not drawn. A line's or point's share is quantised to the 8-bit alpha the ramps give.
fn smooth_cover(pi: u32, cv: u32, px: i32, py: i32) -> f32 {
    if (cv & 3u) == 2u {
        if !covered(pi, px, py) {
            return 0.0;
        }
        return smooth_quantise(smooth_disc(bitcast<f32>(rw(pi, T_COV + 1u)), bitcast<f32>(rw(pi, T_COV + 2u)), bitcast<f32>(rw(pi, T_COV + 3u)), px, py));
    }
    for (var k = 0u; k < 3u; k++) {
        if ((cv >> (2u + k)) & 1u) != 0u && !inside_edge(pi, k, px, py) {
            return 0.0;
        }
    }
    let i = f32(px - bitcast<i32>(rw(pi, T_ORG)));
    let j = f32(py - bitcast<i32>(rw(pi, T_ORG + 1u)));
    let n = (cv >> 8u) & 15u;
    var planes: array<vec4<f32>, 8>;
    for (var k = 0u; k < n; k++) {
        let b = T_COV + 1u + 3u * k;
        planes[k] = vec4<f32>(bitcast<f32>(rw(pi, b)), bitcast<f32>(rw(pi, b + 1u)), bitcast<f32>(rw(pi, b + 2u)), 0.0);
    }
    let c = smooth_edges(n, &planes, i, j);
    if ((cv >> 16u) & 1u) != 0u {
        return smooth_quantise(c);
    }
    return c;
}

// A fog plane (coordinate / w, then 1 / w) at pixel offset (i, j).
fn fog_at(pi: u32, k: u32, i: f32, j: f32) -> f32 {
    let b = T_FOG + 3u * k;
    return (bitcast<f32>(rw(pi, b)) + fp(bitcast<f32>(rw(pi, b + 1u)) * i)) + fp(bitcast<f32>(rw(pi, b + 2u)) * j);
}

// The fragment colour after fog: the coordinate is the plane of coordinate / w over the plane of 1 / w.
fn fogged(pi: u32, si: u32, i: f32, j: f32, src: vec4<u32>) -> vec4<u32> {
    let n = fog_at(pi, 0u, i, j);
    let q = fog_at(pi, 1u, i, j);
    var c = n;
    if !is_nan(q) && abs(q) > 1e-30 {
        c = div_exact(n, q);
    }
    let f = fog_factor(slots[si].w[S_FOGC], bitcast<f32>(slots[si].w[S_FOGC + 1u]), bitcast<f32>(slots[si].w[S_FOGC + 2u]), c);
    return fog_apply(src, vec3<f32>(bitcast<f32>(slots[si].w[S_FOGC + 3u]), bitcast<f32>(slots[si].w[S_FOGC + 4u]), bitcast<f32>(slots[si].w[S_FOGC + 5u])), f);
}

// ---- texture (back/raster/pixel.rs `run_tri`, `texel.rs`) --------------------------------------
// A plane of the texture words at pixel offset (i, j): at + dx * i + dy * j with each product
// rounded before it joins its sum (the CPU never fuses).
fn tex_at(pi: u32, k: u32, i: f32, j: f32) -> f32 {
    let b = T_TEX + 3u * k;
    return (bitcast<f32>(rw(pi, b)) + tex_p(bitcast<f32>(rw(pi, b + 1u)) * i)) + tex_p(bitcast<f32>(rw(pi, b + 2u)) * j);
}

// The coordinates s, t, r at pixel (x, y): the planes over q, or the planes themselves when q is
// nearly zero. r is taken only for a volume.
fn tex_coord(pi: u32, x: i32, y: i32, volume: bool) -> vec3<f32> {
    let i = f32(x - bitcast<i32>(rw(pi, T_ORG)));
    let j = f32(y - bitcast<i32>(rw(pi, T_ORG + 1u)));
    let q = tex_at(pi, 3u, i, j);
    let s = tex_at(pi, 0u, i, j);
    let t = tex_at(pi, 1u, i, j);
    var r = 0.0;
    if volume {
        r = tex_at(pi, 2u, i, j);
    }
    let nan = (bitcast<u32>(q) & 0x7fffffffu) > 0x7f800000u;
    if !nan && abs(q) > 1e-30 {
        var rq = 0.0;
        if volume {
            rq = div_exact(r, q);
        }
        return vec3<f32>(div_exact(s, q), div_exact(t, q), rq);
    }
    return vec3<f32>(s, t, r);
}

// The level of detail of the 2 x 2 quad (aligned to even coordinates) that holds pixel (px, py):
// log2 of the largest step of s and t in texels of level 0, times the scale over 16.
fn tex_lod(pi: u32, d: u32, px: i32, py: i32, volume: bool) -> f32 {
    let qx = px & -2;
    let qy = py & -2;
    let c0 = tex_coord(pi, qx, qy, volume);
    let cx = tex_coord(pi, qx + 1, qy, volume);
    let cy = tex_coord(pi, qx, qy + 1, volume);
    let l0 = tex_level_at(d, 0u);
    let fw = f32(tex_lw(l0));
    let fh = f32(tex_lh(l0));
    var rho = max(max(abs((cx.x - c0.x) * fw), abs((cy.x - c0.x) * fw)), max(abs((cx.y - c0.y) * fh), abs((cy.y - c0.y) * fh)));
    let ls = tex_descs[d + TD_LODSCALE];
    var scale = f32(max(ls & 0xffu, 1u));
    if volume {
        let fd = f32(tex_ld(l0));
        let rr = max(abs((cx.z - c0.z) * fd), abs((cy.z - c0.z) * fd));
        let rs = f32(max((ls >> 16u) & 0xffu, 1u));
        if rr * rs > rho * scale {
            rho = rr;
            scale = rs;
        }
    }
    let v = max(rho * scale / 16.0, 1e-6);
    return bitcast<f32>(lod_log2(bitcast<u32>(v)));
}

// The fragment colour `src` after the bound texture and its environment.
fn textured(pi: u32, si: u32, px: i32, py: i32, src: vec4<u32>) -> vec4<u32> {
    tex_nz = recs[pi].one ^ 0x3f800000u;
    let d = slots[si].w[S_TEXD];
    let volume = ((tex_descs[d + TD_FLAGS] >> 16u) & 1u) != 0u;
    let lod = tex_lod(pi, d, px, py, volume);
    let c = tex_coord(pi, px, py, volume);
    var texel: vec4<u32>;
    if volume {
        texel = tex_sample3(d, c.x, c.y, c.z, lod);
    } else if tex_descs[d + TD_COMBINE] != 0u {
        texel = tex_sample_enhanced(d, c.x, c.y, lod);
    } else {
        texel = tex_sample(d, c.x, c.y, lod);
    }
    return tex_environment(d, src, texel);
}

fn shade(si: u32, index: u32, src: vec4<u32>, z: u32) {
    let fl = slots[si].w[S_FLAGS];
    let has_depth = (fl & F_DEPTH_PLANE) != 0u;
    let test = (fl & F_DEPTH_TEST) != 0u && has_depth;
    let reads_dst = (fl & (F_BLEND | F_LOGIC | F_CMASK)) != 0u;
    if (fl & F_ALPHA) != 0u && !cmp(slots[si].w[S_AFUNC], src.w, slots[si].w[S_AREF]) {
        return;
    }
    let has_stencil = (fl & F_STENCIL) != 0u;
    var stored = 0u;
    let sref = slots[si].w[S_SREF];
    let swm = slots[si].w[S_SWMASK];
    if has_stencil {
        stored = ld(slots[si].w[S_STENCIL], index) & 0xffu;
        let rm = slots[si].w[S_SRMASK];
        if !cmp(slots[si].w[S_SFUNC], sref & rm, stored & rm) {
            let v = stencil_apply(slots[si].w[S_SFAIL], stored, sref);
            st(slots[si].w[S_STENCIL], index, (stored & ~swm) | (v & swm));
            return;
        }
    }
    let zpass = !test || cmp(slots[si].w[S_DFUNC], z, ld(slots[si].w[S_DEPTH], index));
    if has_stencil {
        let v = stencil_apply(select(slots[si].w[S_ZFAIL], slots[si].w[S_ZPASS], zpass), stored, sref);
        st(slots[si].w[S_STENCIL], index, (stored & ~swm) | (v & swm));
    }
    if !zpass {
        return;
    }
    if has_depth && (fl & F_DEPTH_TEST) != 0u && (fl & F_DEPTH_MASK) != 0u {
        st(slots[si].w[S_DEPTH], index, z);
    }
    let ncol = slots[si].w[S_NCOL];
    for (var b = 0u; b < ncol; b++) {
        let cref = slots[si].w[S_COL0 + b];
        var old = 0u;
        if reads_dst {
            old = ld(cref, index);
        }
        st(cref, index, colour_out(si, src, old));
    }
}

// ---- multisampled fragments (back/raster/pixel.rs `shade_samples_with`, `clear_samples`) ----------
// A pixel's samples live in runs of words (colour: 24 bits a sample; depth; stencil: 8 bits), a
// sample at its index times the stride from the run's first bit. The run is read into registers
// once, the samples' tests and writes work on the copy and the words go back together.
fn ms_load(si: u32, base: u32, n: u32, index: u32) -> array<u32, 6> {
    var run = array<u32, 6>(0u, 0u, 0u, 0u, 0u, 0u);
    for (var w = 0u; w < n; w++) {
        run[w] = ld(slots[si].w[base + w], index);
    }
    return run;
}

fn ms_store(si: u32, base: u32, n: u32, index: u32, run: ptr<function, array<u32, 6>>) {
    for (var w = 0u; w < n; w++) {
        st(slots[si].w[base + w], index, (*run)[w]);
    }
}

// One fragment of a multisampled window, for each sample of `covered`: the stencil and depth tests
// and the colour operations run against that sample's own values. The pixel's colour word is the mean
// of its samples, with the fragment's alpha. `oi`, `oj` are the pixel's offsets from the job's origin.
fn shade_samples(pi: u32, si: u32, index: u32, src: vec4<u32>, oi: i32, oj: i32, covered: u32) {
    let fl = slots[si].w[S_FLAGS];
    if (fl & F_ALPHA) != 0u && !cmp(slots[si].w[S_AFUNC], src.w, slots[si].w[S_AREF]) {
        return;
    }
    let count = slots[si].w[S_MS];
    let cn = slots[si].w[S_MS + 3u];
    let dn = slots[si].w[S_MS + 4u];
    let sn = slots[si].w[S_MS + 5u];
    let cstride = cn * 32u / count;
    let dwidth = dn * 32u / count;
    let stencil_on = (fl & F_STENCIL) != 0u && sn != 0u;
    let test = (fl & F_DEPTH_TEST) != 0u && dn != 0u;
    var crun = ms_load(si, S_MS_COL, cn, index);
    var drun = ms_load(si, S_MS_DEPTH, dn, index);
    var srun = ms_load(si, S_MS_STEN, sn, index);
    let sref = slots[si].w[S_SREF];
    let rm = slots[si].w[S_SRMASK];
    let swm = slots[si].w[S_SWMASK];
    var live = 0u;
    for (var k = 0u; k < count; k++) {
        if ((covered >> k) & 1u) == 0u {
            continue;
        }
        var stored = 0u;
        if stencil_on {
            stored = ms_sample_get(&srun, k, 8u, 8u);
            if !cmp(slots[si].w[S_SFUNC], sref & rm, stored & rm) {
                let v = stencil_apply(slots[si].w[S_SFAIL], stored, sref);
                ms_sample_set(&srun, k, 8u, 8u, (stored & ~swm) | (v & swm));
                continue;
            }
        }
        let at = slots[si].w[S_MS_AT + k];
        let z = depth_at_sample(pi, oi, oj, i32(at & 0xffu), i32(at >> 8u)) >> (32u - clamp(dwidth, 1u, 32u));
        var zpass = true;
        if test {
            zpass = cmp(slots[si].w[S_DFUNC], z, ms_sample_get(&drun, k, dwidth, dwidth));
        }
        if stencil_on {
            let v = stencil_apply(select(slots[si].w[S_ZFAIL], slots[si].w[S_ZPASS], zpass), stored, sref);
            ms_sample_set(&srun, k, 8u, 8u, (stored & ~swm) | (v & swm));
        }
        if !zpass {
            continue;
        }
        if test && (fl & F_DEPTH_MASK) != 0u {
            ms_sample_set(&drun, k, dwidth, dwidth, z);
        }
        live |= 1u << k;
    }
    ms_store(si, S_MS_STEN, sn, index, &srun);
    ms_store(si, S_MS_DEPTH, dn, index, &drun);
    if live == 0u {
        return;
    }
    // Samples keep no alpha: blending reads the main word's.
    let ncol = slots[si].w[S_NCOL];
    var alpha = 0u;
    if ncol != 0u {
        alpha = (ld(slots[si].w[S_COL0], index) >> 24u) & 0xffu;
    }
    var sum = vec3<u32>(0u);
    for (var k = 0u; k < count; k++) {
        var rgb = ms_sample_get(&crun, k, cstride, 24u);
        if ((live >> k) & 1u) != 0u {
            rgb = colour_out(si, src, (alpha << 24u) | rgb) & 0xffffffu;
            ms_sample_set(&crun, k, cstride, 24u, rgb);
        }
        sum += vec3<u32>((rgb >> 16u) & 0xffu, (rgb >> 8u) & 0xffu, rgb & 0xffu);
    }
    ms_store(si, S_MS_COL, cn, index, &crun);
    let m = (sum + vec3<u32>(count / 2u)) / vec3<u32>(count);
    let word = (to8(src.w) << 24u) | (m.x << 16u) | (m.y << 8u) | m.z;
    for (var b = 0u; b < ncol; b++) {
        st(slots[si].w[S_COL0 + b], index, word);
    }
}

// A clear of the samples of a pixel: colour under the colour mask, depth under the depth mask, stencil
// under its mask (rec: 0 colour word, 1 depth, 2 stencil value, 3 stencil mask, 4 colour bits kept).
fn clear_samples(pi: u32, si: u32, index: u32, what: u32) {
    let count = slots[si].w[S_MS];
    let cn = slots[si].w[S_MS + 3u];
    let dn = slots[si].w[S_MS + 4u];
    let sn = slots[si].w[S_MS + 5u];
    let cstride = cn * 32u / count;
    let dwidth = dn * 32u / count;
    var crun = ms_load(si, S_MS_COL, cn, index);
    var drun = ms_load(si, S_MS_DEPTH, dn, index);
    var srun = ms_load(si, S_MS_STEN, sn, index);
    let keep = rw(pi, 4u);
    for (var k = 0u; k < count; k++) {
        if (what & 16u) != 0u {
            let old = ms_sample_get(&crun, k, cstride, 24u);
            ms_sample_set(&crun, k, cstride, 24u, ((rw(pi, 0u) & ~keep) | (old & keep)) & 0xffffffu);
        }
        if (what & 32u) != 0u {
            ms_sample_set(&drun, k, dwidth, dwidth, rw(pi, 1u) >> (32u - clamp(dwidth, 1u, 32u)));
        }
        if (what & 64u) != 0u {
            let old = ms_sample_get(&srun, k, 8u, 8u);
            let m = rw(pi, 3u);
            ms_sample_set(&srun, k, 8u, 8u, (old & ~m) | (rw(pi, 2u) & m));
        }
    }
    ms_store(si, S_MS_COL, cn, index, &crun);
    ms_store(si, S_MS_DEPTH, dn, index, &drun);
    ms_store(si, S_MS_STEN, sn, index, &srun);
}

fn arch_tile_begin(tile: u32, px: i32, py: i32, lane: u32) {
}

// Whether the window owns the pixel: the display-ID plane's bits 7:2 are the main ID, and a group
// of 31 in bits 12:8 is an overlay or popup (`ownership_plane`).
fn owned(si: u32, index: u32) -> bool {
    let painted = ld(slots[si].w[S_DID], index);
    let plane = ((painted >> 2u) & 0xfffu) | select(0u, 0x800u, ((painted >> 8u) & 0x1fu) == 0x1fu);
    return ((plane ^ slots[si].w[S_OWN_REF]) & slots[si].w[S_OWN_MASK]) == 0u;
}

// ---- X fill record (kind 1) -------------------------------------------------
// Mirrors back/x/fill.rs `rows`: a clipped box in the X server's coordinates (y down; the store
// row is HEIGHT - 1 - y), the clip list's union, the 32-row stipple, and the logic operation
// under the plane mask and width mask. The slot holds the graphics context the loop reads from
// its plan.
//   rec:  0 x1, 1 y1, 2 x2, 3 y2 (i32)
//   slot: 0 plane reference, 1 fg, 2 bg, 3 mask, 4 width mask, 5 gx,
//         6 flags (1 reads destination, 2 opaque, 4 stipple),
//         7 stipple width, 8 height, 9 origin x, 10 origin y (i32),
//         11 clip count (<= 4), 12.. clips (x1, y1, x2, y2), 28.. stipple rows
// Signed % is truncated in WGSL, but the GL backend's GLSL leaves it undefined for a negative
// operand. Bias into the positive range, which is a multiple of m, and take the unsigned
// remainder.
fn rem_euclid(a: i32, m: i32) -> u32 {
    return u32(a + m * 1048576) % u32(m);
}

fn xfill(pi: u32, px: i32, py: i32) {
    let si = recs[pi].slot;
    let x1 = bitcast<i32>(rw(pi, 0u));
    let y1 = bitcast<i32>(rw(pi, 1u));
    let x2 = bitcast<i32>(rw(pi, 2u));
    let y2 = bitcast<i32>(rw(pi, 3u));
    let y = i32(HEIGHT) - 1 - py;
    if px < x1 || px >= x2 || y < y1 || y >= y2 {
        return;
    }
    let nclips = slots[si].w[11];
    if nclips > 0u {
        var inside = false;
        for (var k = 0u; k < nclips; k++) {
            let c = 12u + 4u * k;
            if px >= bitcast<i32>(slots[si].w[c]) && px < bitcast<i32>(slots[si].w[c + 2u])
                && y >= bitcast<i32>(slots[si].w[c + 1u]) && y < bitcast<i32>(slots[si].w[c + 3u]) {
                inside = true;
            }
        }
        if !inside {
            return;
        }
    }
    let fl = slots[si].w[6];
    var lit = true;
    if (fl & 4u) != 0u {
        let sw = i32(slots[si].w[7]);
        let sh = i32(slots[si].w[8]);
        let row = slots[si].w[28u + rem_euclid(y - bitcast<i32>(slots[si].w[10]), sh)];
        let col = rem_euclid(px - bitcast<i32>(slots[si].w[9]), sw);
        lit = ((row >> (31u - col)) & 1u) != 0u;
    }
    if !lit && (fl & 2u) == 0u {
        return;
    }
    let src = select(slots[si].w[2], slots[si].w[1], lit);
    let r = slots[si].w[0];
    let idx = u32(py) * WIDTH + u32(px);
    if (fl & 1u) != 0u {
        let old = ld(r, idx);
        let m = slots[si].w[3];
        st(r, idx, ((logic(slots[si].w[5], src, old) & m) | (old & ~m)) & slots[si].w[4]);
    } else {
        st(r, idx, src);
    }
}

// ---- Bitmap record (kind 3) -------------------------------------------------
//   rec:  0 value (unused: the slot holds it), 33..36 the box
//   slot: 0 plane reference, 1 x0 (i32), 2 y0 (i32), 3 width, 4 value, 5.. bits (MSB first)
fn bitmap(pi: u32, px: i32, py: i32) {
    let si = recs[pi].slot;
    let col = u32(px - bitcast<i32>(slots[si].w[1]));
    let row = u32(py - bitcast<i32>(slots[si].w[2]));
    let bit = row * slots[si].w[3] + col;
    if ((slots[si].w[5u + (bit >> 5u)] >> (31u - (bit & 31u))) & 1u) != 0u {
        st(slots[si].w[0], u32(py) * WIDTH + u32(px), slots[si].w[4]);
    }
}

// ---- Clear record (kind 4) --------------------------------------------------
//   rec:  0 colour word, 1 depth, 2 stencil value, 3 stencil mask, 4 colour bits kept,
//         5 what to write (1 colour, 2 depth, 4 stencil), 33..36 the box
//   slot: a draw state's (colour, depth and stencil references, ownership)
fn clear_px(pi: u32, px: i32, py: i32) {
    let si = recs[pi].slot;
    let index = u32(py) * WIDTH + u32(px);
    if (slots[si].w[S_FLAGS] & F_OWN) != 0u && !owned(si, index) {
        return;
    }
    let what = rw(pi, 5u);
    if (what & 1u) != 0u {
        let word = rw(pi, 0u);
        let keep = rw(pi, 4u);
        let ncol = slots[si].w[S_NCOL];
        for (var b = 0u; b < ncol; b++) {
            let cref = slots[si].w[S_COL0 + b];
            st(cref, index, (word & ~keep) | (ld(cref, index) & keep));
        }
    }
    if (what & 2u) != 0u {
        st(slots[si].w[S_DEPTH], index, rw(pi, 1u));
    }
    if (what & 4u) != 0u {
        let m = rw(pi, 3u);
        let sref = slots[si].w[S_STENCIL];
        st(sref, index, (ld(sref, index) & ~m) | (rw(pi, 2u) & m));
    }
    if (what & 8u) != 0u {
        clear_samples(pi, si, index, what);
    }
}

// ---- Accumulation (kind 5), its clear (kind 6) and a stencil pixel (kind 7) ------------------------
//   rec:  accumulate: 0 pass count, 1 op, 2 value, 3 op, 4 value, 5 accumulation words, 6 colour
//         mask (bit 0 red .. 3 alpha: set writes); clear: 0 words; stencil: 0 value, 1 mask
//   slot: a draw state's (colour references, ownership), then S_ACC.. as above; a stencil pixel's
//         slot word 0 is the plane
fn accum_px(pi: u32, px: i32, py: i32) {
    let si = recs[pi].slot;
    let index = u32(py) * WIDTH + u32(px);
    if (slots[si].w[S_FLAGS] & F_OWN) != 0u && !owned(si, index) {
        return;
    }
    fnz = recs[pi].one ^ 0x3f800000u;
    let nwords = rw(pi, 5u);
    var acc = vec4<f32>(0.0);
    for (var k = 0u; k < nwords; k++) {
        acc[k] = bitcast<f32>(ld(slots[si].w[S_ACC + k], index));
    }
    var src = vec4<f32>(0.0);
    let sref = slots[si].w[S_ACC + 4u];
    if sref != 0xffffffffu {
        src = acc_components8(ld(sref, index));
    }
    for (var p = 0u; p < rw(pi, 0u); p++) {
        let op = rw(pi, 1u + 2u * p);
        let v = bitcast<f32>(rw(pi, 2u + 2u * p));
        if op == ACCUM_RETURN {
            for (var b = 0u; b < slots[si].w[S_NCOL]; b++) {
                let cref = slots[si].w[S_COL0 + b];
                st(cref, index, acc_return(acc, nwords, v, ld(cref, index), rw(pi, 6u)));
            }
        } else {
            acc_pass(&acc, nwords, op, v, src);
        }
    }
    for (var k = 0u; k < nwords; k++) {
        st(slots[si].w[S_ACC + k], index, bitcast<u32>(acc[k]));
    }
}

fn clear_acc_px(pi: u32, px: i32, py: i32) {
    let si = recs[pi].slot;
    let index = u32(py) * WIDTH + u32(px);
    if (slots[si].w[S_FLAGS] & F_OWN) != 0u && !owned(si, index) {
        return;
    }
    for (var k = 0u; k < rw(pi, 0u); k++) {
        st(slots[si].w[S_ACC + k], index, slots[si].w[S_ACC + 8u + k]);
    }
}

// ---- An X image (kind 8) ---------------------------------------------------------------------
// The CPU walk's pixels, dense over the record's box in the payload ring: two words a pixel, a flag
// (1: the walk lands there) and the word the pixel converts to. The write is x/fill.rs `apply`.
//   slot: 0 plane reference, 1 logic operation, 2 mask, 3 width mask, 4 reads destination,
//         5 payload offset, 6 box width, 7 box x0, 8 box y0 (i32)
fn image_px(pi: u32, px: i32, py: i32) {
    let si = recs[pi].slot;
    let k = u32(py - bitcast<i32>(slots[si].w[8])) * slots[si].w[6] + u32(px - bitcast<i32>(slots[si].w[7]));
    let base = slots[si].w[5] + 2u * k;
    if payload[base] == 0u {
        return;
    }
    let r = slots[si].w[0];
    let idx = u32(py) * WIDTH + u32(px);
    var old = 0u;
    if slots[si].w[4] != 0u {
        old = ld(r, idx);
    }
    let m = slots[si].w[2];
    st(r, idx, ((logic(slots[si].w[1], payload[base + 1u], old) & m) | (old & ~m)) & slots[si].w[3]);
}

fn arch_prim(pi: u32, px: i32, py: i32, lane: u32) {
    let kind = rw(pi, R_KIND);
    if kind == K_XFILL {
        if px >= 0 && py >= 0 && u32(px) < WIDTH && u32(py) < HEIGHT {
            xfill(pi, px, py);
        }
        return;
    }
    nc = bitcast<f32>(recs[pi].one);
    let rx0 = bitcast<i32>(rw(pi, T_RECT));
    let ry0 = bitcast<i32>(rw(pi, T_RECT + 1u));
    let rx1 = bitcast<i32>(rw(pi, T_RECT + 2u));
    let ry1 = bitcast<i32>(rw(pi, T_RECT + 3u));
    if px < rx0 || px >= rx1 || py < ry0 || py >= ry1 {
        return;
    }
    if px < 0 || py < 0 || u32(px) >= WIDTH || u32(py) >= HEIGHT {
        return;
    }
    if kind == K_BOX {
        st(slots[recs[pi].slot].w[0], u32(py) * WIDTH + u32(px), rw(pi, 0u));
        return;
    }
    if kind == K_BITMAP {
        bitmap(pi, px, py);
        return;
    }
    if kind == K_CLEAR {
        clear_px(pi, px, py);
        return;
    }
    if kind == K_IMAGE {
        image_px(pi, px, py);
        return;
    }
    if kind == K_ACCUM {
        accum_px(pi, px, py);
        return;
    }
    if kind == K_CLEARACC {
        clear_acc_px(pi, px, py);
        return;
    }
    if kind == K_STENCILW {
        let r = slots[recs[pi].slot].w[0];
        let index = u32(py) * WIDTH + u32(px);
        let m = rw(pi, 1u);
        st(r, index, ((ld(r, index) & 0xffu & ~m) | (rw(pi, 0u) & m)) & 0xffu);
        return;
    }
    fnz = recs[pi].one ^ 0x3f800000u;
    let cv = rw(pi, T_COV);
    let si = recs[pi].slot;
    let count = slots[si].w[S_MS];
    var cov = 1.0;
    var cmask = 1u;
    if (cv & 3u) != 0u {
        // A smoothed pixel with any coverage writes every sample in a multisampled window.
        cov = smooth_cover(pi, cv, px, py);
        if !(cov > 0.0) {
            return;
        }
        cmask = select(1u, (1u << count) - 1u, count != 0u);
    } else if count != 0u {
        cmask = sample_mask(pi, si, px, py);
        if cmask == 0u {
            return;
        }
    } else if !covered(pi, px, py) {
        return;
    }
    let fl = slots[si].w[S_FLAGS];
    let index = u32(py) * WIDTH + u32(px);
    if (fl & F_OWN) != 0u && !owned(si, index) {
        return;
    }
    if (fl & F_STIPPLE) != 0u {
        let col = u32(px - bitcast<i32>(slots[si].w[S_WORG])) & 31u;
        let row = u32(py - bitcast<i32>(slots[si].w[S_WORG + 1u])) & 31u;
        if ((slots[si].w[S_STIP + row] >> (31u - col)) & 1u) == 0u {
            return;
        }
    }
    if (fl & F_LSTIP) != 0u {
        let d0 = bitcast<f32>(rw(pi, T_LD));
        let d1 = bitcast<f32>(rw(pi, T_LD + 1u));
        let d2 = bitcast<f32>(rw(pi, T_LD + 2u));
        let ox = f32(px - bitcast<i32>(rw(pi, T_ORG)));
        let oy = f32(py - bitcast<i32>(rw(pi, T_ORG + 1u)));
        if !stipple_line(slots[si].w[S_LSTIP], slots[si].w[S_LSTIP + 1u], d0, d1, d2, ox, oy) {
            return;
        }
    }
    let oi = px - bitcast<i32>(rw(pi, T_ORG));
    let oj = py - bitcast<i32>(rw(pi, T_ORG + 1u));
    let i = f32(oi);
    let j = f32(oj);
    var src = vec4<u32>(attr(pi, 0u, i, j), attr(pi, 1u, i, j), attr(pi, 2u, i, j), attr(pi, 3u, i, j));
    if (fl & F_TEX) != 0u {
        src = textured(pi, si, px, py, src);
    }
    // Fog follows the texture environment and the share covered scales alpha after it, before the alpha test.
    if slots[si].w[S_FOGC] != 0u {
        src = fogged(pi, si, i, j, src);
    }
    if (cv & 3u) != 0u {
        src.w = sat_u32(fp(fp(f32(src.w) * cov) + 0.5));
    }
    if count != 0u {
        var m = cmask & ms_allowed(slots[si].w[S_MS + 1u], count, px);
        let opts = slots[si].w[S_MS + 2u];
        if (opts & 1u) != 0u {
            m &= ms_alpha_samples(src.w, count);
        }
        if (opts & 2u) != 0u {
            src.w = FULL;
        }
        if m != 0u {
            shade_samples(pi, si, index, src, oi, oj, m);
        }
        return;
    }
    shade(si, index, src, depth_at(pi, oi, oj));
}

fn arch_tile_end(tile: u32, px: i32, py: i32, lane: u32) {
}
