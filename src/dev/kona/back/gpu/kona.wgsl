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

const REC_W: u32 = 50u;

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
    if !covered(pi, px, py) {
        return;
    }
    let si = recs[pi].slot;
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
    let oi = px - bitcast<i32>(rw(pi, T_ORG));
    let oj = py - bitcast<i32>(rw(pi, T_ORG + 1u));
    let i = f32(oi);
    let j = f32(oj);
    var src = vec4<u32>(attr(pi, 0u, i, j), attr(pi, 1u, i, j), attr(pi, 2u, i, j), attr(pi, 3u, i, j));
    if (fl & F_TEX) != 0u {
        src = textured(pi, si, px, py, src);
    }
    shade(si, index, src, depth_at(pi, oi, oj));
}

fn arch_tile_end(tile: u32, px: i32, py: i32, lane: u32) {
}
