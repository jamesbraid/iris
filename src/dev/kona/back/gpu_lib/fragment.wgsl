// Per-fragment features of the KONA raster path, on the GPU: fog, smoothed
// coverage, multisample sample bits, the accumulation buffer and the line
// and polygon stipples. Each function is the same arithmetic as its CPU
// counterpart (raster/fog.rs, smooth.rs, multisample.rs and pixel.rs,
// gl/pixels.rs), bit for bit, and the parity test beside this file holds
// it to that.
//
// The CPU is single-precision multiplies and adds with every operation
// rounded on its own, and divides that are correctly rounded. A GPU
// compiler may fuse a multiply into an add, reassociate (Metal's fast
// math) and compute a quotient a few units in the last place off, so:
//
// - a product or a sum that joins another operation goes through fp(),
//   an xor with a value the compiler cannot see (fnz is 0, set at run
//   time from a buffer by the entry point);
// - a quotient is div_exact (float.wgsl);
// - a float is made an integer through sat_u32, which clamps and sends a
//   NaN to 0 as Rust's `as u32` does, and a NaN is told by its bits,
//   because fast math may assume there are none. A NaN input is only
//   promised the CPU's answer where a function says so.
//
// There is no signed `/` or `%` here (an llvmpipe bug on negative values);
// the one signed operand, a pixel column, goes through `&`.
//
// A caller sets `fnz` to 0 from a run-time value before using anything
// here. Nothing in this file binds a resource.

#include "float.wgsl"

var<private> fnz: u32;

fn fp(x: f32) -> f32 {
    return bitcast<f32>(bitcast<u32>(x) ^ fnz);
}

fn is_nan(x: f32) -> bool {
    return (bitcast<u32>(x) & 0x7fffffffu) > 0x7f800000u;
}

// Rust's `x as u32`: truncation, saturating, NaN to 0.
fn sat_u32(x: f32) -> u32 {
    if is_nan(x) || !(x > 0.0) {
        return 0u;
    }
    if x >= 4294967296.0 {
        return 0xffffffffu;
    }
    return u32(x);
}

// Rust's `x.clamp(0.0, 1.0)` for the places that spend it on an integer
// conversion next: a NaN comes out as 0, which converts as the CPU's NaN does.
fn unit_or_zero(x: f32) -> f32 {
    if is_nan(x) || !(x > 0.0) {
        return 0.0;
    }
    return min(x, 1.0);
}

// ---- fog (raster/fog.rs)

const FOG_LINEAR: u32 = 1u;
const FOG_EXP: u32 = 2u;
const FOG_EXP2: u32 = 3u;

// exp(-x) for x >= 0 from multiplies and adds alone. A NaN is 0, a
// negative argument is 1, as on the CPU.
fn exp_neg(x_in: f32) -> f32 {
    if is_nan(x_in) || !(x_in < 12.0) {
        return 0.0;
    }
    let x = select(x_in, 0.0, x_in < 0.0);
    // The CPU's f32 constants, by bits: a decimal literal here would be
    // rounded by the compiler, not by Rust.
    let ln2 = bitcast<f32>(0x3f317218u);
    let log2e = bitcast<f32>(0x3fb8aa3bu);
    let k = sat_u32(fp(x * log2e));
    let r = fp(x - fp(f32(k) * ln2));
    var c = array<u32, 9>(0x37d00d01u, 0xb9500d01u, 0x3ab60b61u, 0xbc088889u, 0x3d2aaaabu, 0xbe2aaaabu, 0x3f000000u, 0xbf800000u, 0x3f800000u);
    var q = bitcast<f32>(0xb638ef1du);
    for (var i = 0u; i < 9u; i++) {
        q = fp(fp(q * r) + bitcast<f32>(c[i]));
    }
    return q * bitcast<f32>((127u - k) << 23u);
}

// The 12-bit table entry the factor is rounded to: all ones is 1.0.
const FOG_ENTRY_ONE: f32 = 4095.0;

// Fog::factor: 1 (no fog) to 0 (all fog) for fog coordinate c. `a` and `b`
// are the draw's (end and 1 / (end - start) for linear; density and 0 for
// the exponentials).
fn fog_factor(curve: u32, a: f32, b: f32, c: f32) -> f32 {
    var f: f32;
    if curve == FOG_LINEAR {
        f = fp(fp(a - c) * b);
    } else if curve == FOG_EXP {
        f = exp_neg(fp(a * c));
    } else {
        let u = fp(a * c);
        f = exp_neg(fp(u * u));
    }
    let g = unit_or_zero(f);
    let n = sat_u32(fp(fp(g * FOG_ENTRY_ONE) + 0.5));
    return div_exact(f32(n), FOG_ENTRY_ONE);
}

// Fog::apply: 13-bit red, green and blue toward the fog colour (in 13-bit
// units) by factor f, alpha untouched.
fn fog_apply(src: vec4<u32>, colour: vec3<f32>, f: f32) -> vec4<u32> {
    let g = 1.0 - f;
    var out = src;
    for (var k = 0u; k < 3u; k++) {
        let t = fp(fp(f * f32(src[k])) + fp(g * colour[k]));
        out[k] = sat_u32(fp(t + 0.5));
    }
    return out;
}

// ---- smoothed coverage (raster/smooth.rs)

// quantise: a line's or point's coverage as the 8-bit alpha the ramps deliver.
fn smooth_quantise(cov: f32) -> f32 {
    return div_exact(f32(sat_u32(fp(fp(cov * 255.0) + 0.5))), 255.0);
}

// half_plane: the share of the unit square about the origin on the inside
// of a line whose unit normal (nx, ny) points inside, the origin s pixels
// inside it.
fn half_plane(nx: f32, ny: f32, s: f32) -> f32 {
    let ax = abs(nx);
    let ay = abs(ny);
    var a = ax;
    var b = ay;
    if !(ax >= ay) {
        a = ay;
        b = ax;
    }
    let w = fp(a + b) * 0.5;
    let h = fp(a - b) * 0.5;
    let t = -s;
    var below: f32;
    if t <= -w {
        below = 0.0;
    } else if t >= w {
        below = 1.0;
    } else if t < -h {
        let u = fp(t + w);
        below = div_exact(fp(u * u), fp(fp(2.0 * a) * b));
    } else if t <= h {
        below = fp(div_exact(b, fp(2.0 * a)) + div_exact(fp(t + h), a));
    } else {
        let u = fp(w - t);
        below = 1.0 - div_exact(fp(u * u), fp(fp(2.0 * a) * b));
    }
    return 1.0 - below;
}

// The share of pixel (x, y) a disc of radius r about (cx, cy) covers
// (smooth.rs `disc`); the triangle's own centre test is the caller's.
fn smooth_disc(cx: f32, cy: f32, r: f32, x: i32, y: i32) -> f32 {
    let reach = 0.7072;
    let dx = f32(x) - cx;
    let dy = f32(y) - cy;
    let d2 = fp(fp(dx * dx) + fp(dy * dy));
    if r > reach && d2 <= fp(fp(r - reach) * fp(r - reach)) {
        return 1.0;
    }
    if d2 >= fp(fp(r + reach) * fp(r + reach)) {
        return 0.0;
    }
    let x0 = fp(fp(dx - 0.5) + 0.0625);
    let y0 = fp(fp(dy - 0.5) + 0.0625);
    let rr = fp(r * r);
    var n = 0u;
    for (var j = 0u; j < 8u; j++) {
        let py = fp(y0 + 0.125 * f32(j));
        for (var i = 0u; i < 8u; i++) {
            let px = fp(x0 + 0.125 * f32(i));
            if fp(fp(px * px) + fp(py * py)) <= rr {
                n++;
            }
        }
    }
    return f32(n) * 0.015625;
}

const SMOOTH_MAX_EDGES: u32 = 8u;

// The Edges arm of `coverage`, after the inner-edge centre tests (which are
// integer edge-function tests and stay with the caller): the product of the
// shares of the polygon's n edges at pixel offset (i, j) from the job's
// first pixel. Each plane is (nx, ny, d, unused).
fn smooth_edges(n: u32, planes: ptr<function, array<vec4<f32>, 8>>, i: f32, j: f32) -> f32 {
    var c = 1.0;
    for (var k = 0u; k < n; k++) {
        let e = (*planes)[k];
        let s = fp(fp(fp(e.x * i) + fp(e.y * j)) + e.z);
        if s >= 0.7072 {
            continue;
        }
        c = fp(c * half_plane(e.x, e.y, s));
        if c <= 0.0 {
            return 0.0;
        }
    }
    return c;
}

// ---- multisample (raster/multisample.rs, the packing in raster/pixel.rs)

// A pixel's samples live in a run of up to six words (eight samples of 24
// bits). Sample k is `width` bits from bit k * stride of the run, across
// word boundaries. The run is the caller's copy of the pixel's words.
const MS_RUN: u32 = 6u;

fn ms_field_mask(width: u32) -> u32 {
    return select((1u << width) - 1u, 0xffffffffu, width >= 32u);
}

fn ms_sample_get(run: ptr<function, array<u32, 6>>, k: u32, stride: u32, width: u32) -> u32 {
    let bit = k * stride;
    let w = bit >> 5u;
    let sh = bit & 31u;
    var v = (*run)[w] >> sh;
    if sh + width > 32u {
        v |= (*run)[w + 1u] << (32u - sh);
    }
    return v & ms_field_mask(width);
}

fn ms_sample_set(run: ptr<function, array<u32, 6>>, k: u32, stride: u32, width: u32, v: u32) {
    let bit = k * stride;
    let w = bit >> 5u;
    let sh = bit & 31u;
    let m = ms_field_mask(width);
    let lo = m << sh;
    (*run)[w] = ((*run)[w] & ~lo) | ((v << sh) & lo);
    if sh + width > 32u {
        let hi = m >> (32u - sh);
        (*run)[w + 1u] = ((*run)[w + 1u] & ~hi) | ((v >> (32u - sh)) & hi);
    }
}

// Samples::allowed: the nibble (4 samples) or byte (8) of the mask's
// coverage word that pixel column x picks. x may be negative.
fn ms_allowed(cover: u32, count: u32, x: i32) -> u32 {
    let c = u32(x & 7);
    if count == 8u {
        return (cover >> (8u * (c & 3u))) & 0xffu;
    }
    return (cover >> (4u * c)) & 0xfu;
}

// Samples::alpha_samples: the first round(alpha * count) samples, alpha 13 bits.
fn ms_alpha_samples(alpha: u32, count: u32) -> u32 {
    let k = (min(alpha, 8191u) * count + 4095u) / 8191u;
    return (1u << k) - 1u;
}

// A 13-bit colour component as 8 bits, rounded (ops.rs `to8`).
fn to8(c13: u32) -> u32 {
    return (c13 * 255u + 4095u) / 8191u;
}

// The colour half of shade_samples_with, for a draw with blending, a logic
// operation and the alpha test off and a colour mask of all four: each live
// sample takes the fragment's colour, and the pixel's word is the rounded
// mean of its samples with the fragment's alpha. The caller has already run
// the stencil and depth tests that make `live` (a non-empty subset of the
// covered samples). `run` is the colour run, `stride` its bits a sample
// (24 or more). Returns the pixel's word; the run is updated.
fn ms_write_resolve(run: ptr<function, array<u32, 6>>, count: u32, stride: u32, live: u32, src: vec4<u32>) -> u32 {
    let fresh = (to8(src.x) << 16u) | (to8(src.y) << 8u) | to8(src.z);
    var sum = vec3<u32>(0u);
    for (var k = 0u; k < count; k++) {
        var rgb = ms_sample_get(run, k, stride, 24u);
        if ((live >> k) & 1u) != 0u {
            rgb = fresh;
            ms_sample_set(run, k, stride, 24u, rgb);
        }
        sum += vec3<u32>((rgb >> 16u) & 0xffu, (rgb >> 8u) & 0xffu, rgb & 0xffu);
    }
    let rnd = count / 2u;
    let m = (sum + vec3<u32>(rnd)) / vec3<u32>(count);
    return (to8(src.w) << 24u) | (m.x << 16u) | (m.y << 8u) | m.z;
}

// ---- accumulation buffer (gl/pixels.rs accumulate_cpu)

const ACCUM: u32 = 0x100u;
const ACCUM_LOAD: u32 = 0x101u;
const ACCUM_RETURN: u32 = 0x102u;
const ACCUM_MULT: u32 = 0x103u;
const ACCUM_ADD: u32 = 0x104u;

// A colour word 0xAARRGGBB as red, green, blue, alpha in 0 to 1.
fn acc_components8(w: u32) -> vec4<f32> {
    return vec4<f32>(
        div_exact(f32((w >> 16u) & 0xffu), 255.0),
        div_exact(f32((w >> 8u) & 0xffu), 255.0),
        div_exact(f32(w & 0xffu), 255.0),
        div_exact(f32((w >> 24u) & 0xffu), 255.0),
    );
}

// A component the pixel has no accumulation word for reads 1.0 (and a
// write to it is dropped).
fn acc_get(acc: vec4<f32>, nwords: u32, k: u32) -> f32 {
    return select(1.0, acc[k], k < nwords);
}

// One accumulate, load, multiply or add pass on a pixel's accumulation
// (red, green, blue, alpha in acc; the first nwords of them are stored).
// src is the colour buffer's pixel as acc_components8 gives it. A pass of
// ACCUM_RETURN is acc_return, not this.
fn acc_pass(acc: ptr<function, vec4<f32>>, nwords: u32, op: u32, v: f32, src: vec4<f32>) {
    if op == ACCUM || op == ACCUM_LOAD {
        for (var k = 0u; k < min(nwords, 4u); k++) {
            let t = fp(v * src[k]);
            if op == ACCUM {
                (*acc)[k] = fp((*acc)[k] + t);
            } else {
                // 0.0 + t: a negative zero comes out as a positive one.
                (*acc)[k] = select(t, 0.0, bitcast<u32>(t) == 0x80000000u);
            }
        }
    } else if op == ACCUM_MULT {
        for (var k = 0u; k < min(nwords, 4u); k++) {
            (*acc)[k] = fp((*acc)[k] * v);
        }
    } else if op == ACCUM_ADD {
        for (var k = 0u; k < min(nwords, 4u); k++) {
            (*acc)[k] = fp((*acc)[k] + v);
        }
    }
}

// The return pass: the accumulated colour times v, clamped, as the plane
// word it writes under `mask` (bit 0 red, 1 green, 2 blue, 3 alpha: set
// writes) over the word `old` that was there.
fn acc_return(acc: vec4<f32>, nwords: u32, v: f32, old: u32, mask: u32) -> u32 {
    var q = vec4<u32>(0u);
    for (var k = 0u; k < 4u; k++) {
        let c = unit_or_zero(fp(acc_get(acc, nwords, k) * v));
        q[k] = sat_u32(fp(fp(c * 255.0) + 0.5));
    }
    let keep = (select(0xffu, 0u, (mask & 1u) != 0u) << 16u)
        | (select(0xffu, 0u, (mask & 2u) != 0u) << 8u)
        | select(0xffu, 0u, (mask & 4u) != 0u)
        | (select(0xffu, 0u, (mask & 8u) != 0u) << 24u);
    let fresh = (q.w << 24u) | (q.x << 16u) | (q.y << 8u) | q.z;
    return (fresh & ~keep) | (old & keep);
}

// ---- stipples (raster/pixel.rs run_tri)

// Whether the line stipple lets the pixel at (i, j) from the job's origin
// through: the 16-bit pattern bit picked by the distance along the line
// (the triangle's d plane) over the repeat count.
fn stipple_line(pattern: u32, repeat: u32, d0: f32, d1: f32, d2: f32, i: f32, j: f32) -> bool {
    let d = fp(fp(d0 + fp(d1 * i)) + fp(d2 * j));
    let idx = (sat_u32(d) / max(repeat, 1u)) & 15u;
    return ((pattern >> idx) & 1u) != 0u;
}

// Whether the polygon stipple lets pixel column x of a window with origin
// x ox through: `row` is the stipple row for the pixel's row, (y - oy) & 31,
// the most significant bit leftmost.
fn stipple_poly(row: u32, x: i32, ox: i32) -> bool {
    let col = u32((x - ox) & 31);
    return ((row >> (31u - col)) & 1u) != 0u;
}
