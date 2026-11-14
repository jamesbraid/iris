// SPIKE: KONA's single-sample triangle fragment path as hooks on the shared
// batch kernels. Mirrors back/raster/pixel.rs `run_tri` (PLAIN, no texture,
// no fog, no smoothing) and `Planes::shade_with`. Throwaway.
//
// Defines: WIDTH, HEIGHT, PLANE (= WIDTH * HEIGHT, words in one word
// position's array).

#include "wide.wgsl"

const REC_W: u32 = 38u;

struct Rec {
    slot: u32,
    // 1.0 as bits: the multiply that keeps a product apart from a sum.
    one: u32,
    w: array<u32, REC_W>,
}

struct Slot {
    w: array<u32, 64>,
}

@group(1) @binding(0) var<storage, read> recs: array<Rec>;
@group(1) @binding(1) var<storage, read> slots: array<Slot>;
// One array per word position, PLANE words each, pitch WIDTH (back/store).
@group(2) @binding(0) var<storage, read_write> planes: array<u32>;

// Record words.
const T_A: u32 = 0u;     // 3 x i32 (edge x steps, multiples of 8)
const T_B: u32 = 3u;     // 3 x i32
const T_C: u32 = 6u;     // 3 x I64
const T_TIE: u32 = 12u;  // bit k: edge k owns its pixels
const T_ORG: u32 = 13u;  // job origin x, y
const T_AT: u32 = 15u;   // 4 x f32
const T_DX: u32 = 19u;   // 4 x f32
const T_DY: u32 = 23u;   // 4 x f32
const T_Z: u32 = 27u;    // at, dx, dy: 3 x I64 (32.16)
const T_RECT: u32 = 33u; // x0, y0, x1, y1 (far corner exclusive)

// Slot words.
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
const S_NCOL: u32 = 1u;
const S_COL0: u32 = 2u;   // position offsets (words), not word numbers
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

fn covered(pi: u32, px: i32, py: i32) -> bool {
    let tie = rw(pi, T_TIE);
    for (var k = 0u; k < 3u; k++) {
        var e = r64(pi, T_C + 2u * k);
        e = i64_add(e, i64_mul_i32(bitcast<i32>(rw(pi, T_A + k)), px));
        e = i64_add(e, i64_mul_i32(bitcast<i32>(rw(pi, T_B + k)), py));
        let pos = !i64_is_neg(e) && !i64_is_zero(e);
        let own = i64_is_zero(e) && ((tie >> k) & 1u) != 0u;
        if !(pos || own) {
            return false;
        }
    }
    return true;
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
    let sidx = slots[si].w[S_STENCIL] + index;
    let sref = slots[si].w[S_SREF];
    let swm = slots[si].w[S_SWMASK];
    if has_stencil {
        stored = planes[sidx] & 0xffu;
        let rm = slots[si].w[S_SRMASK];
        if !cmp(slots[si].w[S_SFUNC], sref & rm, stored & rm) {
            let v = stencil_apply(slots[si].w[S_SFAIL], stored, sref);
            planes[sidx] = (stored & ~swm) | (v & swm);
            return;
        }
    }
    let didx = slots[si].w[S_DEPTH] + index;
    let zpass = !test || cmp(slots[si].w[S_DFUNC], z, planes[didx]);
    if has_stencil {
        let v = stencil_apply(select(slots[si].w[S_ZFAIL], slots[si].w[S_ZPASS], zpass), stored, sref);
        planes[sidx] = (stored & ~swm) | (v & swm);
    }
    if !zpass {
        return;
    }
    if has_depth && (fl & F_DEPTH_TEST) != 0u && (fl & F_DEPTH_MASK) != 0u {
        planes[didx] = z;
    }
    let ncol = slots[si].w[S_NCOL];
    for (var b = 0u; b < ncol; b++) {
        let cidx = slots[si].w[S_COL0 + b] + index;
        var old = 0u;
        if reads_dst {
            old = planes[cidx];
        }
        planes[cidx] = colour_out(si, src, old);
    }
}

fn arch_tile_begin(tile: u32, px: i32, py: i32, lane: u32) {
}

fn arch_prim(pi: u32, px: i32, py: i32, lane: u32) {
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
    if !covered(pi, px, py) {
        return;
    }
    let si = recs[pi].slot;
    let fl = slots[si].w[S_FLAGS];
    let index = u32(py) * WIDTH + u32(px);
    if (fl & F_OWN) != 0u {
        let painted = planes[slots[si].w[S_DID] + index];
        let plane = ((painted >> 2u) & 0xfffu) | select(0u, 0x800u, ((painted >> 8u) & 0x1fu) == 0x1fu);
        if ((plane ^ slots[si].w[S_OWN_REF]) & slots[si].w[S_OWN_MASK]) != 0u {
            return;
        }
    }
    if (fl & F_STIPPLE) != 0u {
        let col = u32(px - bitcast<i32>(slots[si].w[S_WORG])) & 31u;
        let row = u32(py - bitcast<i32>(slots[si].w[S_WORG + 1u])) & 31u;
        if ((slots[si].w[S_STIP + row] >> (31u - col)) & 1u) == 0u {
            return;
        }
    }
    let oi = px - bitcast<i32>(rw(pi, T_ORG));
    let oj = py - bitcast<i32>(rw(pi, T_ORG + 1u));
    let i = f32(oi);
    let j = f32(oj);
    let src = vec4<u32>(attr(pi, 0u, i, j), attr(pi, 1u, i, j), attr(pi, 2u, i, j), attr(pi, 3u, i, j));
    shade(si, index, src, depth_at(pi, oi, oj));
}

fn arch_tile_end(tile: u32, px: i32, py: i32, lane: u32) {
}
