// KONA's texture sampling and texture environment, for the GPU raster
// kernel: the same arithmetic as the CPU's `sample`, `sample3`,
// `sample_enhanced` and `environment`, word for word.
//
// A bound texture is a run of TD_WORDS words in `tex_descs`, packed by
// `gpu_lib::pack_desc` from the CPU's `TexDesc`; every function takes the
// word index `d` of its descriptor, so nothing the size of a descriptor is
// copied. Texel memory is `tex_word` (texture_mem.wgsl).
//
// The including module must set the private `tex_nz` to a zero the compiler
// cannot see (a word of run-time data with the value 0, or a one xor'd with
// its own bit pattern) before it calls anything here: it keeps each
// product apart from the sum it feeds, as the CPU, which never fuses, adds
// them (`p()` in the other kernels).
//
// Descriptor words:
//   0 count   1 min filter code   2 mag filter code (0 nearest, 1 linear,
//   2 | within << 2 | between << 3 mip)   3 clamp bits (s, t, r)
//   4 lod scales (s | t << 8 | r << 16)   5 class | env mode << 8 | volume << 16
//   6..9 environment colour   10 combine kind (0 none, 1 detail, 2 sharpen)
//   11 modulate | scale << 8 | channels << 16   12..17 detail image level
//   18..81 curve weights (f32 bits)   82.. 14 levels of 6 words.
// A level: plane, off, pitch | ox << 16, oy | w << 16, h | d << 16, slice.

#include "kona_texture_mem.wgsl"
#include "kona_lodmath.wgsl"
#include "float.wgsl"

const TD_COUNT: u32 = 0u;
const TD_MIN: u32 = 1u;
const TD_MAG: u32 = 2u;
const TD_WRAP: u32 = 3u;
const TD_LODSCALE: u32 = 4u;
const TD_FLAGS: u32 = 5u;
const TD_ENVCOL: u32 = 6u;
const TD_COMBINE: u32 = 10u;
const TD_DETAIL: u32 = 11u;
const TD_IMAGE: u32 = 12u;
const TD_WEIGHTS: u32 = 18u;
const TD_LEVELS: u32 = 82u;
const TD_LEVEL_WORDS: u32 = 6u;
const TD_MAX_LEVELS: u32 = 14u;
const TD_CURVE_NODES: u32 = 64u;
const TD_WORDS: u32 = 166u;

const TEX_FULL: u32 = 8191u;

var<private> tex_nz: u32;

fn tex_p(x: f32) -> f32 {
    return bitcast<f32>(bitcast<u32>(x) ^ tex_nz);
}

// Rust's `as i32` on a float: truncation, saturating, NaN to 0.
fn tex_sat_i32(x: f32) -> i32 {
    let c = clamp(x, -2147483648.0, 2147483520.0);
    return select(i32(c), 0, x != x);
}

// Rust's `as u32` on a float: truncation, saturating, NaN and negatives to 0.
fn tex_sat_u32(x: f32) -> u32 {
    let c = clamp(x, 0.0, 4294967040.0);
    return select(u32(c), 0u, x != x);
}

// A coordinate wrapped into 0..n (n >= 1): Rust's `rem_euclid` for repeat,
// without a signed remainder, or clamped to the edge texel.
fn tex_wrap(i: i32, n: i32, clamp_edge: bool) -> i32 {
    if clamp_edge {
        return clamp(i, 0, n - 1);
    }
    if i >= 0 {
        return i32(u32(i) % u32(n));
    }
    let below = u32(-(i + 1)) % u32(n);
    return n - 1 - i32(below);
}

fn tex_bytes(w: u32) -> vec4<u32> {
    return vec4<u32>((w >> 16u) & 0xffu, (w >> 8u) & 0xffu, w & 0xffu, w >> 24u);
}

fn tex_texel(lv: u32, x: i32, y: i32) -> u32 {
    let pitch = tex_descs[lv + 2u] & 0xffffu;
    let ox = tex_descs[lv + 2u] >> 16u;
    let oy = tex_descs[lv + 3u] & 0xffffu;
    return tex_word(tex_descs[lv], tex_descs[lv + 1u] + (oy + u32(y)) * pitch + ox + u32(x));
}

fn tex_texel3(lv: u32, x: i32, y: i32, z: i32) -> u32 {
    let pitch = tex_descs[lv + 2u] & 0xffffu;
    let ox = tex_descs[lv + 2u] >> 16u;
    let oy = tex_descs[lv + 3u] & 0xffffu;
    return tex_word(tex_descs[lv], tex_descs[lv + 1u] + u32(z) * tex_descs[lv + 5u] + (oy + u32(y)) * pitch + ox + u32(x));
}

fn tex_lw(lv: u32) -> i32 {
    return i32(max(tex_descs[lv + 3u] >> 16u, 1u));
}

fn tex_lh(lv: u32) -> i32 {
    return i32(max(tex_descs[lv + 4u] & 0xffffu, 1u));
}

fn tex_ld(lv: u32) -> i32 {
    return i32(max(tex_descs[lv + 4u] >> 16u, 1u));
}

// A level sampled at s, t: nearest, or bilinear with 8-bit weights. `wb` has
// the clamp bits (bit 0 s, bit 1 t, bit 2 r), a clear bit repeating.
fn tex_level(lv: u32, s: f32, t: f32, linear: bool, wb: u32) -> vec4<u32> {
    let w = tex_lw(lv);
    let h = tex_lh(lv);
    let cs = (wb & 1u) != 0u;
    let ct = (wb & 2u) != 0u;
    var u = tex_p(s * f32(w));
    var v = tex_p(t * f32(h));
    if !linear {
        return tex_bytes(tex_texel(lv, tex_wrap(tex_sat_i32(floor(u)), w, cs), tex_wrap(tex_sat_i32(floor(v)), h, ct)));
    }
    u = u - 0.5;
    v = v - 0.5;
    let i0f = floor(u);
    let j0f = floor(v);
    let fu = tex_sat_u32((u - i0f) * 256.0);
    let fv = tex_sat_u32((v - j0f) * 256.0);
    let i0 = tex_sat_i32(i0f);
    let j0 = tex_sat_i32(j0f);
    let xa = tex_wrap(i0, w, cs);
    let xb = tex_wrap(i0 + 1, w, cs);
    let ya = tex_wrap(j0, h, ct);
    let yb = tex_wrap(j0 + 1, h, ct);
    let t00 = tex_bytes(tex_texel(lv, xa, ya));
    let t10 = tex_bytes(tex_texel(lv, xb, ya));
    let t01 = tex_bytes(tex_texel(lv, xa, yb));
    let t11 = tex_bytes(tex_texel(lv, xb, yb));
    let top = t00 * (256u - fu) + t10 * fu;
    let bot = t01 * (256u - fu) + t11 * fu;
    return (top * (256u - fv) + bot * fv + 32768u) >> vec4<u32>(16u);
}

// The level `l` of the descriptor at `d`, as a level word index.
fn tex_level_at(d: u32, l: u32) -> u32 {
    return d + TD_LEVELS + min(l, TD_MAX_LEVELS - 1u) * TD_LEVEL_WORDS;
}

fn tex_filter_level(d: u32, f: u32, lod: f32, s: f32, t: f32, r: f32, volume: bool) -> vec4<u32> {
    let wb = tex_descs[d + TD_WRAP];
    let count = tex_descs[d + TD_COUNT];
    let top = select(count - 1u, 0u, count == 0u);
    let kind = f & 3u;
    if kind < 2u {
        if volume {
            return tex_level3(tex_level_at(d, 0u), s, t, r, kind == 1u, wb);
        }
        return tex_level(tex_level_at(d, 0u), s, t, kind == 1u, wb);
    }
    let within = ((f >> 2u) & 1u) != 0u;
    let between = ((f >> 3u) & 1u) != 0u;
    let lodc = clamp(lod, 0.0, f32(top));
    let l0 = tex_sat_u32(floor(lodc));
    if !between || l0 >= top {
        let l = min(select(tex_sat_u32(lodc + 0.5), l0, between), top);
        if volume {
            return tex_level3(tex_level_at(d, l), s, t, r, within, wb);
        }
        return tex_level(tex_level_at(d, l), s, t, within, wb);
    }
    var a: vec4<u32>;
    var b: vec4<u32>;
    if volume {
        a = tex_level3(tex_level_at(d, l0), s, t, r, within, wb);
        b = tex_level3(tex_level_at(d, l0 + 1u), s, t, r, within, wb);
    } else {
        a = tex_level(tex_level_at(d, l0), s, t, within, wb);
        b = tex_level(tex_level_at(d, l0 + 1u), s, t, within, wb);
    }
    let f8 = tex_sat_u32((lodc - f32(l0)) * 256.0);
    return (a * (256u - f8) + b * f8 + 128u) >> vec4<u32>(8u);
}

// The texel colour (r, g, b, a in 8 bits) at s, t with level of detail lod.
fn tex_sample(d: u32, s: f32, t: f32, lod: f32) -> vec4<u32> {
    let f = select(tex_descs[d + TD_MAG], tex_descs[d + TD_MIN], lod > 0.0);
    return tex_filter_level(d, f, lod, s, t, 0.0, false);
}

// A volume level sampled at s, t, r: nearest, or trilinear with 8-bit weights.
fn tex_level3(lv: u32, s: f32, t: f32, r: f32, linear: bool, wb: u32) -> vec4<u32> {
    let w = tex_lw(lv);
    let h = tex_lh(lv);
    let dp = tex_ld(lv);
    let cs = (wb & 1u) != 0u;
    let ct = (wb & 2u) != 0u;
    let cr = (wb & 4u) != 0u;
    var u = tex_p(s * f32(w));
    var v = tex_p(t * f32(h));
    var q = tex_p(r * f32(dp));
    if !linear {
        return tex_bytes(tex_texel3(lv, tex_wrap(tex_sat_i32(floor(u)), w, cs), tex_wrap(tex_sat_i32(floor(v)), h, ct), tex_wrap(tex_sat_i32(floor(q)), dp, cr)));
    }
    u = u - 0.5;
    v = v - 0.5;
    q = q - 0.5;
    let i0f = floor(u);
    let j0f = floor(v);
    let k0f = floor(q);
    let fu = tex_sat_u32((u - i0f) * 256.0);
    let fv = tex_sat_u32((v - j0f) * 256.0);
    let fz = tex_sat_u32((q - k0f) * 256.0);
    let i0 = tex_sat_i32(i0f);
    let j0 = tex_sat_i32(j0f);
    let k0 = tex_sat_i32(k0f);
    let xa = tex_wrap(i0, w, cs);
    let xb = tex_wrap(i0 + 1, w, cs);
    let ya = tex_wrap(j0, h, ct);
    let yb = tex_wrap(j0 + 1, h, ct);
    let za = tex_wrap(k0, dp, cr);
    let zb = tex_wrap(k0 + 1, dp, cr);
    // Each slice's bilinear blend, 16 fraction bits.
    let a = tex_slice(lv, xa, xb, ya, yb, za, fu, fv);
    let b = tex_slice(lv, xa, xb, ya, yb, zb, fu, fv);
    return (a * (256u - fz) + b * fz + 8388608u) >> vec4<u32>(24u);
}

fn tex_slice(lv: u32, xa: i32, xb: i32, ya: i32, yb: i32, z: i32, fu: u32, fv: u32) -> vec4<u32> {
    let t00 = tex_bytes(tex_texel3(lv, xa, ya, z));
    let t10 = tex_bytes(tex_texel3(lv, xb, ya, z));
    let t01 = tex_bytes(tex_texel3(lv, xa, yb, z));
    let t11 = tex_bytes(tex_texel3(lv, xb, yb, z));
    let top = t00 * (256u - fu) + t10 * fu;
    let bot = t01 * (256u - fu) + t11 * fu;
    return top * (256u - fv) + bot * fv;
}

// The texel colour at s, t, r with level of detail lod, for a volume texture.
fn tex_sample3(d: u32, s: f32, t: f32, r: f32, lod: f32) -> vec4<u32> {
    let f = select(tex_descs[d + TD_MAG], tex_descs[d + TD_MIN], lod > 0.0);
    return tex_filter_level(d, f, lod, s, t, r, true);
}

// The curve's weight at a level of detail at or below zero: the node at
// 64 * 2^lod, blended linearly with the next.
fn tex_curve_weight(d: u32, lod: f32) -> f32 {
    let at = min(bitcast<f32>(lod_exp2(bitcast<u32>(lod))) * 64.0, 63.0);
    let n = tex_sat_u32(at);
    let f = at - f32(n);
    let w0 = bitcast<f32>(tex_descs[d + TD_WEIGHTS + n]);
    let w1 = bitcast<f32>(tex_descs[d + TD_WEIGHTS + min(n + 1u, TD_CURVE_NODES - 1u)]);
    return tex_p(w0 * (1.0 - f)) + tex_p(w1 * f);
}

fn tex_channel_on(channels: u32, k: u32) -> bool {
    if channels == 1u {
        return k < 3u;
    }
    if channels == 2u {
        return k == 3u;
    }
    return true;
}

fn tex_unit(v: u32) -> f32 {
    return div_exact(f32(v), 255.0);
}

fn tex_out(v: f32) -> u32 {
    return tex_sat_u32(tex_p(clamp(v, 0.0, 1.0) * 255.0) + 0.5);
}

fn tex_detail_channel(b: u32, o: u32, w: f32, modulate: bool) -> u32 {
    let bu = tex_unit(b);
    let signed = tex_p(w * (tex_p(2.0 * tex_unit(o)) - 1.0));
    if modulate {
        return tex_out(bu * (1.0 + signed));
    }
    return tex_out(bu + signed);
}

fn tex_sharpen_channel(b: u32, c: u32, w: f32) -> u32 {
    let bu = tex_unit(b);
    return tex_out(bu + tex_p(w * (bu - tex_unit(c))));
}

// The texel colour at s, t for a 2D texture with a detail or sharpen
// combine: a minified fragment is sampled as `tex_sample`; a magnified one is
// the base bilinear and then combined.
fn tex_sample_enhanced(d: u32, s: f32, t: f32, lod: f32) -> vec4<u32> {
    if lod > 0.0 {
        return tex_sample(d, s, t, lod);
    }
    let wb = tex_descs[d + TD_WRAP];
    let base = tex_level(tex_level_at(d, 0u), s, t, true, wb);
    let kind = tex_descs[d + TD_COMBINE];
    let cw = tex_descs[d + TD_DETAIL];
    let channels = cw >> 16u;
    if kind == 1u {
        let b0 = tex_level_at(d, 0u);
        let img = d + TD_IMAGE;
        let k = f32(1u << min((cw >> 8u) & 0xffu, 15u));
        let ds = s * div_exact(f32(tex_lw(b0)), f32(tex_lw(img))) * k;
        let dt = t * div_exact(f32(tex_lh(b0)), f32(tex_lh(img))) * k;
        let other = tex_level(img, ds, dt, true, 0u);
        let w = tex_curve_weight(d, lod);
        let modulate = (cw & 1u) != 0u;
        var out = base;
        if tex_channel_on(channels, 0u) { out.x = tex_detail_channel(base.x, other.x, w, modulate); }
        if tex_channel_on(channels, 1u) { out.y = tex_detail_channel(base.y, other.y, w, modulate); }
        if tex_channel_on(channels, 2u) { out.z = tex_detail_channel(base.z, other.z, w, modulate); }
        if tex_channel_on(channels, 3u) { out.w = tex_detail_channel(base.w, other.w, w, modulate); }
        return out;
    }
    if kind == 2u {
        if tex_descs[d + TD_COUNT] < 2u {
            return base;
        }
        let coarse = tex_level(tex_level_at(d, 1u), s, t, true, wb);
        let w = tex_curve_weight(d, lod);
        var out = base;
        if tex_channel_on(channels, 0u) { out.x = tex_sharpen_channel(base.x, coarse.x, w); }
        if tex_channel_on(channels, 1u) { out.y = tex_sharpen_channel(base.y, coarse.y, w); }
        if tex_channel_on(channels, 2u) { out.z = tex_sharpen_channel(base.z, coarse.z, w); }
        if tex_channel_on(channels, 3u) { out.w = tex_sharpen_channel(base.w, coarse.w, w); }
        return out;
    }
    return base;
}

// a * b of two 13-bit values.
fn tex_mul13(a: u32, b: u32) -> u32 {
    return (a * b + TEX_FULL / 2u) / TEX_FULL;
}

fn tex_unpack13(w: u32) -> vec4<u32> {
    let c = vec4<u32>((w >> 16u) & 0xffu, (w >> 8u) & 0xffu, w & 0xffu, w >> 24u);
    return (c * TEX_FULL + 127u) / 255u;
}

// The fragment colour f (13-bit r, g, b, a) after the texture environment,
// with the texel t (8-bit r, g, b, a from the sampler).
fn tex_environment(d: u32, f: vec4<u32>, t: vec4<u32>) -> vec4<u32> {
    let flags = tex_descs[d + TD_FLAGS];
    let cls = flags & 0xffu;
    var mode = (flags >> 8u) & 3u;
    let tc = tex_unpack13(t.w << 24u | t.x << 16u | t.y << 8u | t.z);
    let ct = tc.xyz;
    var at = TEX_FULL;
    if cls == 1u || cls == 3u {
        at = tc.w;
    } else if cls == 4u {
        at = tc.x;
    }
    let k = vec4<u32>(tex_descs[d + TD_ENVCOL], tex_descs[d + TD_ENVCOL + 1u], tex_descs[d + TD_ENVCOL + 2u], tex_descs[d + TD_ENVCOL + 3u]);
    // Decal is defined for classes 2 and 3 only.
    if mode == 1u && cls != 2u && cls != 3u {
        mode = 0u;
    }
    var out: vec4<u32>;
    if mode == 0u {
        if cls == 1u {
            out = vec4<u32>(f.xyz, tex_mul13(f.w, at));
        } else {
            out = vec4<u32>(tex_mul13(f.x, ct.x), tex_mul13(f.y, ct.y), tex_mul13(f.z, ct.z), tex_mul13(f.w, at));
        }
    } else if mode == 1u {
        if cls == 2u {
            out = vec4<u32>(ct, f.w);
        } else {
            out = vec4<u32>(
                tex_mul13(f.x, TEX_FULL - at) + tex_mul13(ct.x, at),
                tex_mul13(f.y, TEX_FULL - at) + tex_mul13(ct.y, at),
                tex_mul13(f.z, TEX_FULL - at) + tex_mul13(ct.z, at),
                f.w);
        }
    } else if mode == 2u {
        if cls == 1u {
            out = vec4<u32>(f.xyz, tex_mul13(f.w, at));
        } else {
            var a = tex_mul13(f.w, at);
            if cls == 4u {
                a = tex_mul13(f.w, TEX_FULL - at) + tex_mul13(k.w, at);
            } else if cls == 2u {
                a = f.w;
            }
            out = vec4<u32>(
                tex_mul13(f.x, TEX_FULL - ct.x) + tex_mul13(k.x, ct.x),
                tex_mul13(f.y, TEX_FULL - ct.y) + tex_mul13(k.y, ct.y),
                tex_mul13(f.z, TEX_FULL - ct.z) + tex_mul13(k.z, ct.z),
                a);
        }
    } else {
        if cls == 1u {
            out = vec4<u32>(f.xyz, at);
        } else if cls == 2u {
            out = vec4<u32>(ct, f.w);
        } else {
            out = vec4<u32>(ct, at);
        }
    }
    return min(out, vec4<u32>(TEX_FULL));
}
