// Bit-exact log2 and exp2 for f32, for the texture level of detail.
//
// The CPU computes a fragment's level of detail as `log2(rho * scale / 16)`
// and the detail and sharpen weight as `exp2(lod)` with the host's libm. A
// GPU's own log2 and exp2 are accurate to a few ulp, not rounded, so a
// level of detail could land on the other side of a level boundary or a
// weight step. These compute in double precision on integer halves
// (`f64.wgsl`: add, subtract and multiply rounded to nearest even as the
// host rounds) and round once to f32, so the result is the correctly rounded
// one on every adapter, whatever its f32 unit does. The host's libm is
// correctly rounded except in the rare case it is not, so a CPU/GPU
// difference can only be that libm case.
//
// Inputs and results are bit patterns of f32. Zero, infinities and NaN
// follow IEEE (log2 of a negative is NaN, log2 of zero is -inf); a denormal
// input to log2 is taken at its value; exp2 returns denormals.

#include "f64.wgsl"

const LM_ONE: I64 = I64(0u, 0x3ff00000u);
const LM_HALF: I64 = I64(0u, 0x3fe00000u);
const LM_TWO: I64 = I64(0u, 0x40000000u);
// 2 / ln 2 and ln 2 as doubles.
const LM_TWO_OVER_LN2: I64 = I64(0x652b82feu, 0x40071547u);
const LM_LN2: I64 = I64(0xfefa39efu, 0x3fe62e42u);

// An f32 that is normal or zero, exactly, as a double.
fn lm_f32_to_f64(x: u32) -> I64 {
    let sign = x & 0x80000000u;
    let e = (x >> 23u) & 0xffu;
    if e == 0u {
        return I64(0u, sign);
    }
    let m = x & 0x7fffffu;
    return I64(m << 29u, sign | ((e + 896u) << 20u) | (m >> 3u));
}

// A double rounded to nearest even as an f32, with denormal results and
// overflow to infinity. Returns the bits.
fn lm_f64_to_f32(a: I64) -> u32 {
    let sign = f64_sign(a);
    if f64_is_nan(a) {
        return 0x7fc00000u;
    }
    if f64_is_inf(a) {
        return sign | 0x7f800000u;
    }
    if f64_is_zero(a) {
        return sign;
    }
    let e = i32(f64_exp(a)) - 1023;
    if e > 127 {
        return sign | 0x7f800000u;
    }
    let sig = f64_sig(a);
    // A normal keeps 24 of the 53 bits; a denormal result keeps fewer.
    var drop = 29u;
    var be = e;
    if e < -126 {
        drop = 29u + u32(-126 - e);
        be = -126;
    }
    if drop > 54u {
        return sign;
    }
    // The kept bits above one round bit, and whether anything lies below it.
    let r = u64_shr(sig, drop - 1u);
    let round_bit = (r.lo & 1u) != 0u;
    var m = r.lo >> 1u;
    let sticky = !i64_eq(u64_shl(r, drop - 1u), sig);
    if round_bit && (sticky || (m & 1u) != 0u) {
        m += 1u;
    }
    var bits: u32;
    if e < -126 {
        bits = m;
    } else {
        bits = (u32(be + 127) << 23u) + m - 0x800000u;
    }
    if bits >= 0x7f800000u {
        bits = 0x7f800000u;
    }
    return sign | bits;
}

// log2 of the f32 with bits `x`, correctly rounded.
fn lod_log2(x: u32) -> u32 {
    if (x >> 31u) != 0u {
        if (x << 1u) == 0u {
            return 0xff800000u;
        }
        return 0x7fc00000u;
    }
    let ex = (x >> 23u) & 0xffu;
    var mant = x & 0x7fffffu;
    if ex == 255u {
        return select(0x7fc00000u, 0x7f800000u, mant == 0u);
    }
    var e: i32;
    if ex == 0u {
        if mant == 0u {
            return 0xff800000u;
        }
        // Normalise a denormal: its value is mant * 2^-149.
        let shift = countLeadingZeros(mant) - 8u;
        mant = (mant << shift) & 0x7fffffu;
        e = -126 - i32(shift);
    } else {
        e = i32(ex) - 127;
    }
    // m in [sqrt(1/2), sqrt(2)): above sqrt 2 take half and one more exponent.
    var biased = 1023u;
    if mant > 0x3504f3u {
        biased = 1022u;
        e += 1;
    }
    let m = I64(mant << 29u, (biased << 20u) | (mant >> 3u));
    // z = (m - 1) / (m + 1), the reciprocal of m + 1 by Newton's method from 1/2:
    // the relative error squares each step, 0.21 to below 1e-21 in five.
    let den = f64_add(m, LM_ONE);
    var rc = LM_HALF;
    for (var i = 0; i < 5; i++) {
        rc = f64_mul(rc, f64_sub(LM_TWO, f64_mul(den, rc)));
    }
    let z = f64_mul(f64_sub(m, LM_ONE), rc);
    // log2 m = (2 / ln 2) atanh z = (2 / ln 2) z (1 + y/3 + y^2/5 + ...), y = z^2 <= 0.0295.
    let y = f64_mul(z, z);
    var q = I64(0xbca1af28u, 0x3faaf286u);
    q = f64_add(I64(0x1e1e1e1eu, 0x3fae1e1eu), f64_mul(y, q));
    q = f64_add(I64(0x11111111u, 0x3fb11111u), f64_mul(y, q));
    q = f64_add(I64(0x13b13b14u, 0x3fb3b13bu), f64_mul(y, q));
    q = f64_add(I64(0x745d1746u, 0x3fb745d1u), f64_mul(y, q));
    q = f64_add(I64(0x1c71c71cu, 0x3fbc71c7u), f64_mul(y, q));
    q = f64_add(I64(0x92492492u, 0x3fc24924u), f64_mul(y, q));
    q = f64_add(I64(0x9999999au, 0x3fc99999u), f64_mul(y, q));
    q = f64_add(I64(0x55555555u, 0x3fd55555u), f64_mul(y, q));
    q = f64_add(LM_ONE, f64_mul(y, q));
    let frac = f64_mul(f64_mul(z, q), LM_TWO_OVER_LN2);
    return lm_f64_to_f32(f64_add(f64_from_i32(e), frac));
}

// 2^x for the f32 with bits `x`, correctly rounded.
fn lod_exp2(x: u32) -> u32 {
    let ex = (x >> 23u) & 0xffu;
    if ex == 255u {
        if (x & 0x7fffffu) != 0u {
            return 0x7fc00000u;
        }
        return select(0x7f800000u, 0u, (x >> 31u) != 0u);
    }
    let neg = (x >> 31u) != 0u;
    // Past the ends: 2^128 and up overflows; below 2^-150.5 rounds to zero (2^-150 is the
    // tie under the least denormal).
    if !neg && ex >= 134u {
        return 0x7f800000u;
    }
    if neg && ex >= 135u {
        return 0u;
    }
    if (x << 1u) == 0u || ex == 0u {
        return 0x3f800000u;
    }
    let xd = lm_f32_to_f64(x);
    // x = n + r, r in [-1/2, 1/2], both exact in a double.
    let n = f64_floor_i32(f64_add(xd, LM_HALF));
    if n <= -151 {
        return 0u;
    }
    let r = f64_sub(xd, f64_from_i32(n));
    // e^w, w = r ln 2, |w| <= 0.347: the Taylor series to w^14 / 14!.
    let w = f64_mul(r, LM_LN2);
    var q = I64(0xa8c07c9du, 0x3da93974u);
    q = f64_add(I64(0x13a86d09u, 0x3de61246u), f64_mul(w, q));
    q = f64_add(I64(0xeff8d898u, 0x3e21eed8u), f64_mul(w, q));
    q = f64_add(I64(0x67f544e4u, 0x3e5ae645u), f64_mul(w, q));
    q = f64_add(I64(0xb7789f5cu, 0x3e927e4fu), f64_mul(w, q));
    q = f64_add(I64(0xa556c734u, 0x3ec71de3u), f64_mul(w, q));
    q = f64_add(I64(0x1a01a01au, 0x3efa01a0u), f64_mul(w, q));
    q = f64_add(I64(0x1a01a01au, 0x3f2a01a0u), f64_mul(w, q));
    q = f64_add(I64(0x16c16c17u, 0x3f56c16cu), f64_mul(w, q));
    q = f64_add(I64(0x11111111u, 0x3f811111u), f64_mul(w, q));
    q = f64_add(I64(0x55555555u, 0x3fa55555u), f64_mul(w, q));
    q = f64_add(I64(0x55555555u, 0x3fc55555u), f64_mul(w, q));
    q = f64_add(I64(0x00000000u, 0x3fe00000u), f64_mul(w, q));
    q = f64_add(I64(0x00000000u, 0x3ff00000u), f64_mul(w, q));
    q = f64_add(I64(0x00000000u, 0x3ff00000u), f64_mul(w, q));
    // Scale by 2^n through the exponent field (q is within [0.70, 1.42]).
    let scaled = I64(q.lo, u32(bitcast<i32>(q.hi) + (n << 20u)));
    return lm_f64_to_f32(scaled);
}
