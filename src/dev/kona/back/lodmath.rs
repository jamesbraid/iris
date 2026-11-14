//! Correctly rounded `log2` and `exp2` on f32, from f64 arithmetic alone.
//!
//! The texture level of detail is `log2` of a ratio of texel steps and the
//! detail and sharpen weight is `exp2` of it. The host's libm gives these
//! to within an ulp, not rounded, and not the same on every host. These
//! compute in double precision with add, subtract and multiply (each
//! correctly rounded by IEEE, never fused by Rust) and round once to f32, so the
//! result is the correctly rounded one everywhere. They are the CPU twins of
//! `lod_log2` and `lod_exp2` in `gpu_lib/lodmath.wgsl`, which do the same
//! operations in the same order on integer halves, so the two agree bit for bit.

// Not called yet: the raster kernel's texture stage will use it (the GPU twin is already tested against it).
#![allow(dead_code)]

const LN2: f64 = f64::from_bits(0x3fe6_2e42_fefa_39ef);
const TWO_OVER_LN2: f64 = f64::from_bits(0x4007_1547_652b_82fe);

/// log2 of `x`, correctly rounded. Zero gives -inf, a negative NaN.
pub fn log2(x: f32) -> f32 {
    let ix = x.to_bits();
    if ix >> 31 != 0 {
        return if ix << 1 == 0 { f32::NEG_INFINITY } else { f32::NAN };
    }
    let ex = ix >> 23 & 0xff;
    let mut mant = ix & 0x7f_ffff;
    if ex == 255 {
        return if mant == 0 { f32::INFINITY } else { f32::NAN };
    }
    let mut e: i32;
    if ex == 0 {
        if mant == 0 {
            return f32::NEG_INFINITY;
        }
        // A denormal is mant * 2^-149; normalise it.
        let shift = mant.leading_zeros() - 8;
        mant = (mant << shift) & 0x7f_ffff;
        e = -126 - shift as i32;
    } else {
        e = ex as i32 - 127;
    }
    // m in [sqrt(1/2), sqrt(2)): above sqrt 2 take half and one more exponent.
    let mut biased = 1023u64;
    if mant > 0x35_04f3 {
        biased = 1022;
        e += 1;
    }
    let m = f64::from_bits(biased << 52 | (mant as u64) << 29);
    // z = (m - 1) / (m + 1), the reciprocal by Newton's method from 1/2: the relative error squares
    // each step, 0.21 to below 1e-21 in five.
    let den = m + 1.0;
    let mut rc = 0.5f64;
    for _ in 0..5 {
        rc *= 2.0 - den * rc;
    }
    let z = (m - 1.0) * rc;
    // log2 m = (2 / ln 2) atanh z = (2 / ln 2) z (1 + y/3 + y^2/5 + ...), y = z^2 <= 0.0295.
    let y = z * z;
    let mut q = 1.0 / 19.0;
    for n in (1..=8).rev() {
        q = 1.0 / (2 * n + 1) as f64 + y * q;
    }
    q = 1.0 + y * q;
    let frac = z * q * TWO_OVER_LN2;
    (e as f64 + frac) as f32
}

/// 2^x, correctly rounded.
pub fn exp2(x: f32) -> f32 {
    let ix = x.to_bits();
    let ex = ix >> 23 & 0xff;
    if ex == 255 {
        return if ix & 0x7f_ffff != 0 { f32::NAN } else if ix >> 31 != 0 { 0.0 } else { f32::INFINITY };
    }
    let neg = ix >> 31 != 0;
    // 2^128 and up overflows; below 2^-150.5 rounds to zero (2^-150 is the tie under the least denormal).
    if !neg && ex >= 134 {
        return f32::INFINITY;
    }
    if neg && ex >= 135 {
        return 0.0;
    }
    if ix << 1 == 0 || ex == 0 {
        return 1.0;
    }
    let xd = x as f64;
    // x = n + r, r in [-1/2, 1/2], both exact in a double.
    let n = (xd + 0.5).floor() as i32;
    if n <= -151 {
        return 0.0;
    }
    let r = xd - n as f64;
    // e^w, w = r ln 2, |w| <= 0.347: the Taylor series to w^14 / 14!.
    let w = r * LN2;
    let mut facts = [1u64; 15];
    for k in 1..15 {
        facts[k] = facts[k - 1] * k as u64;
    }
    let mut q = 1.0 / facts[14] as f64;
    for k in (0..=13).rev() {
        q = 1.0 / facts[k] as f64 + w * q;
    }
    // Scale by 2^n through the exponent field (q is within [0.70, 1.42]).
    f64::from_bits(q.to_bits().wrapping_add((n as i64 as u64) << 52)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_cases() {
        assert_eq!(log2(1.0), 0.0);
        assert_eq!(log2(8.0), 3.0);
        assert_eq!(log2(0.125), -3.0);
        assert_eq!(log2(1.0e-6).to_bits(), ((1.0e-6f32 as f64).log2() as f32).to_bits());
        assert!(log2(-1.0).is_nan() && log2(0.0) == f32::NEG_INFINITY && log2(f32::INFINITY) == f32::INFINITY);
        assert_eq!(exp2(0.0), 1.0);
        assert_eq!(exp2(-3.0), 0.125);
        assert_eq!(exp2(10.0), 1024.0);
        assert_eq!(exp2(128.0), f32::INFINITY);
        assert_eq!(exp2(-150.0), 0.0);
        assert_eq!(exp2(-149.0), f32::from_bits(1));
    }

    /// Against the double libm result rounded once: the same value except in a double-rounding case
    /// (about one in 2^29), over a spread of inputs.
    #[test]
    fn agrees_with_the_double_libm_rounded() {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let (mut diff, mut n) = (0, 0);
        for _ in 0..200_000 {
            let x = f32::from_bits((next() >> 33) as u32 % 0x7f80_0000);
            n += 1;
            diff += (log2(x).to_bits() != ((x as f64).log2() as f32).to_bits()) as u32;
            let y = ((next() >> 40) as f32 / (1u64 << 24) as f32 - 0.8) * 160.0;
            diff += (exp2(y).to_bits() != ((y as f64).exp2() as f32).to_bits()) as u32;
        }
        assert!(diff <= 2, "{diff} of {n} pairs differ");
    }
}
