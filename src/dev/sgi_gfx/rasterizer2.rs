//! Ported from hw/display/sgi/rasterizer.c in jamesbraid/qemu-sgi-origami.
//!
//! The second half of the software rasterizer: texture plane helpers,
//! triangles, lines, points, polygons, the draw entry point, bitmaps,
//! pixel rectangles, clears and raster ops.

use super::*;

pub fn sgi_soft_texture_values(v: &SGISoftVertex, volume: bool, one_dimensional: bool, value: &mut [f64; 4]) {
    value[0] = v.texcoord[0] * v.inv_w;
    value[1] = if one_dimensional { 0.0 } else { v.texcoord[1] * v.inv_w };
    value[2] = v.texcoord[3] * v.inv_w;
    value[3] = if volume { v.texcoord[2] * v.inv_w } else { 0.0 };
}

pub fn sgi_soft_texture_plane(v: &[SGISoftVertex], sub: u32, volume: bool, one_dimensional: bool, g: &mut SGISoftTexPlane) -> bool {
    let scale = (1i32 << sub) as f64;
    let x1 = (v[1].x - v[0].x) as f64 / scale;
    let y1 = (v[1].y - v[0].y) as f64 / scale;
    let x2 = (v[2].x - v[0].x) as f64 / scale;
    let y2 = (v[2].y - v[0].y) as f64 / scale;
    let area = x1 * y2 - x2 * y1;
    let mut values = [[0.0f64; 4]; 3];

    *g = SGISoftTexPlane { x: v[0].x as f64 / scale, y: v[0].y as f64 / scale, ..Default::default() };
    if area == 0.0 {
        return true;
    }
    if !area.is_finite() {
        return false;
    }
    for i in 0..3 {
        sgi_soft_texture_values(&v[i], volume, one_dimensional, &mut values[i]);
    }
    for k in 0..4 {
        let a = values[1][k] - values[0][k];
        let b = values[2][k] - values[0][k];

        g.value[k] = values[0][k];
        g.dx[k] = (a * y2 - b * y1) / area;
        g.dy[k] = (b * x1 - a * x2) / area;
        if !g.value[k].is_finite() || !g.dx[k].is_finite() || !g.dy[k].is_finite() {
            return false;
        }
    }
    true
}

pub fn sgi_soft_texture_line_gradient(
    a: &SGISoftVertex,
    b: &SGISoftVertex,
    sub: u32,
    volume: bool,
    one_dimensional: bool,
    values: &mut [f64; 4],
    delta: &mut [f64; 4],
    length: &mut f64,
) -> bool {
    let mut end = [0.0f64; 4];

    *length = ((b.x - a.x) as f64).hypot((b.y - a.y) as f64) / (1i32 << sub) as f64;
    if !length.is_finite() {
        return false;
    }
    sgi_soft_texture_values(a, volume, one_dimensional, values);
    sgi_soft_texture_values(b, volume, one_dimensional, &mut end);
    for k in 0..4 {
        delta[k] = end[k] - values[k];
        if !values[k].is_finite() || !delta[k].is_finite() {
            return false;
        }
    }
    true
}

pub fn sgi_soft_texture_coefficients(r: &SGISoftDraw, v: &[SGISoftVertex]) -> bool {
    let mut g = SGISoftTexPlane::default();

    if r.texture_base.is_null()
        || sgi_soft_texture_plane(v, r.rules.subpixel_bits as u32, r.texture.volume, r.texture.one_dimensional, &mut g)
    {
        return true;
    }
    sgi_soft_texture_report(r, 16, "invalid primitive coefficients, primitive dropped");
    false
}

/// Whether a triangle's fragments can take sgi_soft_simple_row() instead of
/// the full per-fragment path.
pub fn sgi_soft_simple_fragments(r: &SGISoftDraw) -> bool {
    let st = &*r.st;
    let t = &*r.t;

    if !r.texture.is_null()
        || st.fog != 0
        || st.clip_planes != 0
        || !r.polygon_stipple.is_null()
        || st.index_bits != 0
        || r.blend
        || r.logic_op
        || st.alpha_test != 0
        || st.stencil_test != 0
    {
        return false;
    }
    // sgi_raster_depth_stencil() drops every fragment of such a draw.
    if st.depth_test != 0 && (t.load_depth.is_none() || (st.depth_mask != 0 && t.store_depth.is_none())) {
        return false;
    }
    if t.store_rgba.is_some() {
        return !r.rgba_read;
    }
    r.colour_mask == 0 || r.colour_mask == sgi_soft_colour_bits(&r.rules)
}

/// One row of a triangle under sgi_soft_simple_fragments(): pixels px0 to
/// px1 of row py, whose edge values for v[2]-v[0] and v[0]-v[1] at px0 are
/// e1 and e2.
pub fn sgi_soft_simple_row(
    r: &SGISoftDraw,
    v: &[SGISoftVertex],
    py: i32,
    mut px0: i32,
    mut px1: i32,
    mut e1: i64,
    mut e2: i64,
    step1: i64,
    step2: i64,
    area: i64,
    offset: f64,
    flat: &SGISoftFrag,
    smooth: bool,
    rgba: bool,
    depth_row: bool,
) {
    let rules = &*r.rules;
    let st = &*r.st;
    let t = &*r.t;
    let y = r.win.oy + py;
    // A flat colour is the same at every pixel unless dithered.
    let fixed = !smooth && !r.dither;
    let store = if rgba { r.rgba_mask != 0 && r.ntargets != 0 } else { r.colour_mask != 0 };
    let clipped = r.win.clip.n > 1;
    let mut fixed_rgba = SGIRasterColour::default();
    let mut fixed_colour = 0u32;

    if st.scissor_test != 0 && !clipped {
        // sgi_soft_owned_at()'s scissor test, decided for the whole row.
        let sx0 = st.scissor[0] as i64;
        let sx1 = st.scissor[0] as i64 + st.scissor[2] as i64 - 1;

        if !(py >= st.scissor[1] && (py as i64) < st.scissor[1] as i64 + st.scissor[3] as i64) {
            return;
        }
        if sx0 > px0 as i64 {
            if sx0 > px1 as i64 {
                return;
            }
            e1 += (sx0 - px0 as i64) * step1;
            e2 += (sx0 - px0 as i64) * step2;
            px0 = sx0 as i32;
        }
        px1 = (px1 as i64).min(sx1) as i32;
    }
    if fixed && store {
        if rgba {
            let alpha = sgi_soft_alpha(rules, flat.rgba[3]) as f64 / sgi_soft_alpha_max(rules) as f64;

            fixed_rgba = sgi_soft_rgba_value(rules, sgi_soft_rgb(rules, &flat.rgba), alpha);
        } else {
            fixed_colour = sgi_soft_rgb(rules, &flat.rgba);
        }
    }
    let mut px = px0;
    while px <= px1 {
        let x = r.win.ox + px;
        let b1 = e1 as f64 / area as f64;
        let b2 = e2 as f64 / area as f64;
        let mut z = v[0].z + b1 * (v[1].z - v[0].z) + b2 * (v[2].z - v[0].z);
        let mut off = offset;
        let mut zw = 0.0f64;
        let mut colour = [0.0f64; 4];

        'pixel: {
            if depth_row {
                let depth_w = (1.0 - b1 - b2) * v[0].depth_w + b1 * v[1].depth_w + b2 * v[2].depth_w;
                let slack = rules.depth_slack;

                z = (z + off) / depth_w;
                off = 0.0;
                if z < 0.0 && z >= -slack {
                    z = 0.0;
                } else if z > 1.0 && z <= 1.0 + slack {
                    z = 1.0;
                }
            }
            if !sgi_raster_window_depth(st, z, off, &mut zw) || !(zw >= 0.0 && zw <= 1.0) {
                break 'pixel;
            }
            if clipped && !sgi_soft_owned_at(r, px, py) {
                break 'pixel;
            }
            if st.depth_test != 0 || store {
                let zq = sgi_raster_depth_code(rules, zw);

                if st.depth_test != 0 {
                    if !sgi_soft_compare(st.depth_func, zq, (t.load_depth.unwrap())(t.opaque, x, y)) {
                        break 'pixel;
                    }
                    if st.depth_mask != 0 {
                        (t.store_depth.unwrap())(t.opaque, x, y, zq);
                    }
                }
            }
            if !store {
                break 'pixel;
            }
            if smooth {
                let w0 = (1.0 - b1 - b2) * v[0].inv_w;
                let w1 = b1 * v[1].inv_w;
                let w2 = b2 * v[2].inv_w;
                let sum = w0 + w1 + w2;

                for k in 0..4 {
                    colour[k] = (w0 * v[0].rgba[k] + w1 * v[1].rgba[k] + w2 * v[2].rgba[k]) / sum;
                }
            } else {
                colour = flat.rgba;
            }
            if rgba {
                let mut out = fixed_rgba;

                if !fixed {
                    let alpha = sgi_soft_alpha(rules, colour[3]) as f64 / sgi_soft_alpha_max(rules) as f64;

                    out = sgi_soft_rgba_value(rules, sgi_soft_store_rgb(r, x, y, &colour), alpha);
                }
                for i in 0..r.ntargets as usize {
                    (t.store_rgba.unwrap())(t.opaque, r.targets[i], x, y, &out);
                }
            } else {
                let out = if fixed { fixed_colour } else { sgi_soft_store_rgb(r, x, y, &colour) };

                for i in 0..r.ntargets as usize {
                    (t.store_colour.unwrap())(t.opaque, r.targets[i], x, y, out);
                }
            }
        }
        px += 1;
        e1 += step1;
        e2 += step2;
    }
}

/// Out of line, once a row.
pub fn sgi_soft_simple_span(
    r: &SGISoftDraw,
    v: &[SGISoftVertex],
    py: i32,
    px0: i32,
    px1: i32,
    e1: i64,
    e2: i64,
    step1: i64,
    step2: i64,
    area: i64,
    offset: f64,
    flat: &SGISoftFrag,
    smooth: bool,
) {
    let rgba = r.t.store_rgba.is_some();
    let depth_row = r.rules.depth_row_given;

    sgi_soft_simple_row(r, v, py, px0, px1, e1, e2, step1, step2, area, offset, flat, smooth, rgba, depth_row);
}

/// Map and rasterize one triangle of a clipped polygon, given its three
/// vertices in the view volume and, for flat shading, the provoking vertex
/// of the triangle it was clipped from.
pub fn sgi_soft_clipped_triangle(
    r: &SGISoftDraw,
    va: &SGIVolumeVertex,
    vb: &SGIVolumeVertex,
    vc: &SGIVolumeVertex,
    provoking: &[f32],
) {
    let st = &*r.st;
    let verts: [&SGIVolumeVertex; 3] = [va, vb, vc];
    let mut flat = &provoking[SGI_GL_V_COLOR..SGI_GL_V_COLOR + 4];
    let mut flat_index = provoking[SGI_GL_V_INDEX];
    let mut flat_frag = SGISoftFrag::default();
    let mut frag;
    let mut v: [SGISoftVertex; 3] = Default::default();
    let mut gradient = SGISoftTexPlane::default();
    // The footprint's per-axis scales, and its value where Q/W is flat.
    let mut tex_ns = 0.0f64;
    let mut tex_nt = 0.0f64;
    let mut tex_nr = 0.0f64;
    let mut flat_rho = f64::NAN;
    let mut flat_q = false;
    let mut flat_ok = false;
    // Fragments sample in fixed point, and their lambda where it is flat.
    let fixed: bool;
    let mut fixed_flat: Option<i32> = None;
    // The row loop for textured fragments, and its inputs.
    let mut span: Option<SGISoftTexSpanFn> = None;
    let mut sp = SGISoftTexSpan::default();
    let mut area: i64;
    let offset: f64;
    let ccw: bool;
    let front: bool;
    let smooth = st.shade_model != SGI_GL_FLAT;
    let simple = sgi_soft_simple_fragments(r);
    let mut tex_row = [0.0f64; 4];
    let sub = r.rules.subpixel_bits as i32;

    for i in 0..3 {
        if !sgi_soft_project(r, verts[i], &mut v[i]) {
            return;
        }
    }

    area = sgi_soft_edge(&v[0], &v[1], v[2].x, v[2].y);
    if area == 0 {
        return;
    }
    if !r.texture_base.is_null()
        && !sgi_soft_texture_plane(&v, sub as u32, r.texture.volume, r.texture.one_dimensional, &mut gradient)
    {
        sgi_soft_texture_report(r, 16, "invalid primitive coefficients, primitive dropped");
        return;
    }
    // An image with no pyramid has no planes to fail: it samples in double.
    fixed = !r.fixed.is_null()
        && (!r.texture_base.is_null() || sgi_soft_texture_plane(&v, sub as u32, false, false, &mut gradient));
    if fixed && r.fixed.lod && gradient.dx[2] == 0.0 && gradient.dy[2] == 0.0 {
        // Q/W flat, as for the double footprint below: every fragment's
        // lambda is this one, which sgi_soft_fixed_coords() would compute
        // for each of them, since s dQ/dx and t dQ/dx are then zero.
        let q = gradient.value[2];

        if q > 0.0 {
            let iq = 1.0 / q;
            let a = r.fixed.ns * gradient.dx[0] * iq;
            let b = r.fixed.nt * gradient.dx[1] * iq;
            let c = r.fixed.ns * gradient.dy[0] * iq;
            let e = r.fixed.nt * gradient.dy[1] * iq;
            let rho2 = if a * a + b * b > c * c + e * e { a * a + b * b } else { c * c + e * e };

            if rho2 <= f64::MAX {
                fixed_flat = Some(sgi_soft_fixed_decide(&r.fixed, rho2, sgi_soft_fixed_lambda(rho2)));
            }
        }
    }
    if !r.texture_base.is_null() {
        tex_ns = r.texture_base.width as f64 * sgi_soft_texture_lod_scale(r.texture.lod_scale_s);
        tex_nt = if r.texture.one_dimensional {
            0.0
        } else {
            r.texture_base.height as f64 * sgi_soft_texture_lod_scale(r.texture.lod_scale_t)
        };
        tex_nr = if r.texture.volume {
            r.texture_base.depth as f64 * sgi_soft_texture_lod_scale(r.texture.lod_scale_r)
        } else {
            0.0
        };
        // With Q/W the same at every vertex the footprint is one number.
        flat_q = gradient.dx[2] == 0.0 && gradient.dy[2] == 0.0;
        if flat_q {
            let value = [0.0, 0.0, gradient.value[2], 0.0];
            let mut out = [0.0f64; 5];

            flat_ok = sgi_soft_texture_derivatives(&value, &gradient.dx, &gradient.dy, tex_ns, tex_nt, tex_nr, &mut out);
            flat_rho = out[4];
        }
    }
    ccw = if st.window_matrix_given != 0 && st.window_rows_down != 0 { area < 0 } else { area > 0 };
    front = if st.front_face == SGI_GL_CW { !ccw } else { ccw };
    if st.cull_face != 0
        && (st.cull_face_mode == SGI_GL_FRONT_AND_BACK
            || st.cull_face_mode == (if front { SGI_GL_FRONT } else { SGI_GL_BACK }))
    {
        return;
    }
    if !front && r.back_colors {
        flat = &provoking[SGI_GL_V_BACK_COLOR..SGI_GL_V_BACK_COLOR + 4];
        flat_index = provoking[SGI_GL_V_BACK_INDEX];
        for i in 0..3 {
            let back = &verts[i].attr[SGI_GL_V_BACK_COLOR..SGI_GL_V_BACK_COLOR + 4];

            for k in 0..4 {
                v[i].rgba[k] = back[k].max(0.0).min(1.0) as f64;
            }
            v[i].index = verts[i].attr[SGI_GL_V_BACK_INDEX] as f64;
        }
    }
    for k in 0..4 {
        flat_frag.rgba[k] = flat[k] as f64;
    }
    flat_frag.index = flat_index as f64;
    flat_frag.fixed = false;
    offset = sgi_soft_offset(r, &v);
    if area < 0 {
        // Rasterize either winding the same way from here.
        v.swap(1, 2);
        area = -area;
    }

    let min_x = v[0].x.min(v[1].x.min(v[2].x)) >> sub;
    let max_x = v[0].x.max(v[1].x.max(v[2].x)) >> sub;
    let min_y = v[0].y.min(v[1].y.min(v[2].y)) >> sub;
    let max_y = v[0].y.max(v[1].y.max(v[2].y)) >> sub;

    // Window pixels, bounded by the viewport and the owned rectangle.
    let (mut px0, mut py0, mut px1, mut py1) = (0i32, 0i32, 0i32, 0i32);
    if !sgi_soft_bounds(r, &mut px0, &mut py0, &mut px1, &mut py1) {
        return;
    }
    px0 = (min_x as i32).max(px0);
    px1 = (max_x as i32).min(px1);
    py0 = (min_y as i32).max(py0);
    py1 = (max_y as i32).min(py1);

    // Each edge function is linear in x, so across a row it steps by an
    // exact integer; solving for the ends of each edge's run of owned
    // centres visits only the pixels a test of every centre would keep.
    let step0: i64 = -(v[2].y - v[1].y) * (1i64 << sub);
    let step1: i64 = -(v[0].y - v[2].y) * (1i64 << sub);
    let step2: i64 = -(v[1].y - v[0].y) * (1i64 << sub);
    if fixed && r.fixed.span.is_some() {
        span = r.fixed.span;
        sp = SGISoftTexSpan {
            v: P::new(&mut v),
            g: P::new(&mut gradient),
            row: P::new(&mut tex_row),
            flat_lambda: fixed_flat,
            flat: P::new(&mut flat_frag),
            offset,
            area,
            step1,
            step2,
            smooth,
            constant: 0,
        };
        for k in 0..4 {
            let c = v[0].rgba[k];

            if (c == 0.0 || c == 1.0) && v[1].rgba[k] == c && v[2].rgba[k] == c {
                sp.constant |= 1u8 << k;
            }
        }
    }
    let mut py = py0;
    while py <= py1 {
        let cur_py = py;
        py += 1;
        if !sgi_soft_row_in_band(r, cur_py) {
            continue;
        }
        let py = cur_py;
        let cy: i64 = (py as i64 * (1i64 << sub)) + (1i64 << (sub - 1));
        let cx: i64 = (px0 as i64 * (1i64 << sub)) + (1i64 << (sub - 1));
        let e0 = sgi_soft_edge(&v[1], &v[2], cx, cy);
        let mut e1 = sgi_soft_edge(&v[2], &v[0], cx, cy);
        let mut e2 = sgi_soft_edge(&v[0], &v[1], cx, cy);
        let mut lo: i64 = 0;
        let mut hi: i64 = px1 as i64 - px0 as i64;
        sgi_soft_owned_run(&v[1], &v[2], e0, step0, &mut lo, &mut hi);
        sgi_soft_owned_run(&v[2], &v[0], e1, step1, &mut lo, &mut hi);
        sgi_soft_owned_run(&v[0], &v[1], e2, step2, &mut lo, &mut hi);
        if lo > hi {
            continue;
        }
        e1 += lo * step1;
        e2 += lo * step2;
        if simple {
            sgi_soft_simple_span(
                r,
                &v,
                py,
                px0 + lo as i32,
                px0 + hi as i32,
                e1,
                e2,
                step1,
                step2,
                area,
                offset,
                &flat_frag,
                smooth,
            );
            continue;
        }
        if !r.texture_base.is_null() || fixed {
            for k in 0..4 {
                tex_row[k] = (py as f64 + 0.5 - gradient.y) * gradient.dy[k];
            }
        }
        let mut px = px0 + lo as i32;
        while px <= px0 + hi as i32 {
            let mut background = false;

            if let Some(span) = span {
                // The row loop draws up to a fragment the fixed sampler
                // cannot take, which this loop then draws in double.
                let stop = span(r, &sp, py, px, px0 + hi as i32, e1, e2);

                e1 = e1.wrapping_add((stop - px) as i64 * step1);
                e2 = e2.wrapping_add((stop - px) as i64 * step2);
                px = stop;
                if px > px0 + hi as i32 {
                    break;
                }
            }
            let cur_px = px;
            let cur_e1 = e1;
            let cur_e2 = e2;
            px += 1;
            e1 += step1;
            e2 += step2;
            let px = cur_px;
            // Window phase precedes plane translation and fragment work.
            if !sgi_soft_stipple_pass(r, px, py, &mut background) {
                continue;
            }
            frag = flat_frag.clone();
            let b1 = cur_e1 as f64 / area as f64;
            let b2 = cur_e2 as f64 / area as f64;
            let z = v[0].z + b1 * (v[1].z - v[0].z) + b2 * (v[2].z - v[0].z);
            let w0 = (1.0 - b1 - b2) * v[0].inv_w;
            let w1 = b1 * v[1].inv_w;
            let w2 = b2 * v[2].inv_w;
            let sum = w0 + w1 + w2;
            let depth_w = (1.0 - b1 - b2) * v[0].depth_w + b1 * v[1].depth_w + b2 * v[2].depth_w;
            if fixed && span.is_none() {
                let mut s = 0.0f64;
                let mut t = 0.0f64;
                let mut lambda = 0i32;

                frag.fixed = sgi_soft_fixed_coords(&r.fixed, &gradient, &tex_row, px, fixed_flat, &mut s, &mut t, &mut lambda)
                    && sgi_soft_fixed_sample::<SGI_SOFT_FIXED_ANY, false>(r, s, t, lambda, &mut frag.texel);
            }
            // Only texturing reads the coordinates, only fog the distance.
            if !r.texture.is_null() && !frag.fixed {
                for k in 0..4 {
                    frag.texcoord[k] = (w0 * v[0].texcoord[k] + w1 * v[1].texcoord[k] + w2 * v[2].texcoord[k]) / sum;
                }
            }
            if st.fog != 0 {
                frag.fog = (w0 * v[0].fog + w1 * v[1].fog + w2 * v[2].fog) / sum;
            }
            if st.clip_planes != 0 {
                let mut d = [0.0f64; SGI_GL_CLIP_PLANES as usize];

                for i in 0..SGI_GL_CLIP_PLANES as usize {
                    d[i] = (w0 * v[0].clip[i] + w1 * v[1].clip[i] + w2 * v[2].clip[i]) / sum;
                }
                if sgi_soft_clipped(st, &d) {
                    continue;
                }
            }
            if smooth {
                for k in 0..4 {
                    frag.rgba[k] = (w0 * v[0].rgba[k] + w1 * v[1].rgba[k] + w2 * v[2].rgba[k]) / sum;
                }
                // An index is taken as a difference from the first vertex's,
                // so a constant one stays exact before its integer part is taken.
                frag.index = v[0].index + (w1 * (v[1].index - v[0].index) + w2 * (v[2].index - v[0].index)) / sum;
            }
            if !r.texture_base.is_null() && !frag.fixed {
                let mut value = [0.0f64; 4];
                let mut out = [0.0f64; 5];

                for k in 0..4 {
                    value[k] = gradient.value[k] + (px as f64 + 0.5 - gradient.x) * gradient.dx[k] + tex_row[k];
                }
                if flat_q {
                    let d = value[2];

                    frag.texture_rho = if flat_ok
                        && (value[0] / d).is_finite()
                        && (value[1] / d).is_finite()
                        && (tex_nr == 0.0 || (value[3] / d).is_finite())
                    {
                        flat_rho
                    } else {
                        f64::NAN
                    };
                } else {
                    frag.texture_rho = if sgi_soft_texture_derivatives(&value, &gradient.dx, &gradient.dy, tex_ns, tex_nt, tex_nr, &mut out) {
                        out[4]
                    } else {
                        f64::NAN
                    };
                }
            }
            if background {
                sgi_soft_stipple_background(r, &mut frag);
            }
            sgi_soft_fragment(r, px, py, z, offset, depth_w, &frag);
        }
    }
}

/// Transform, clip and rasterize one triangle. Clipped to the view volume
/// in clip coordinates, what is left is drawn as a fan from its first
/// vertex, each flat shaded from the triangle's provoking vertex.
pub fn sgi_soft_clip_triangle(
    r: &SGISoftDraw,
    va: &[f32],
    vb: &[f32],
    vc: &[f32],
    poly: &mut [SGIVolumeVertex; SGI_VOLUME_TRIANGLE_MAX],
    n: &mut u32,
) -> bool {
    let verts: [&[f32]; 3] = [va, vb, vc];
    let mut in_: [SGIVolumeVertex; 3] = Default::default();

    for i in 0..3 {
        sgi_volume_from_draw_vertex(&r.st, verts[i], &mut in_[i]);
    }
    *n = sgi_volume_clip_triangle(&in_, poly);
    for i in 0..*n as usize {
        if !sgi_soft_texture_vertex_valid(r, &poly[i]) {
            return false;
        }
    }
    if !r.texture_base.is_null() {
        let mut i = 1usize;
        while i + 1 < *n as usize {
            let vertices: [&SGIVolumeVertex; 3] = [&poly[0], &poly[i], &poly[i + 1]];
            let mut projected: [SGISoftVertex; 3] = Default::default();

            for j in 0..3 {
                if !sgi_soft_project(r, vertices[j], &mut projected[j]) {
                    return false;
                }
            }
            if !sgi_soft_texture_coefficients(r, &projected) {
                return false;
            }
            i += 1;
        }
    }
    true
}

/// A quad or polygon validates all its clipped pieces before any are drawn.
pub fn sgi_soft_texture_triangle_valid(r: &SGISoftDraw, a: &[f32], b: &[f32], c: &[f32]) -> bool {
    let mut poly: [SGIVolumeVertex; SGI_VOLUME_TRIANGLE_MAX] = Default::default();
    let mut n = 0u32;

    sgi_soft_clip_triangle(r, a, b, c, &mut poly, &mut n)
}

pub fn sgi_soft_triangle(r: &SGISoftDraw, va: &[f32], vb: &[f32], vc: &[f32], provoking: &[f32]) {
    let mut poly: [SGIVolumeVertex; SGI_VOLUME_TRIANGLE_MAX] = Default::default();
    let mut n = 0u32;
    let (mut x0, mut y0, mut x1, mut y1) = (0i32, 0i32, 0i32, 0i32);

    if !sgi_soft_bounds(r, &mut x0, &mut y0, &mut x1, &mut y1) || !sgi_soft_clip_triangle(r, va, vb, vc, &mut poly, &mut n) {
        return;
    }
    let mut i = 1usize;
    while i + 1 < n as usize {
        sgi_soft_clipped_triangle(r, &poly[0], &poly[i], &poly[i + 1], provoking);
        i += 1;
    }
}

// MIN and MAX as the C macros evaluate them (a NaN operand picks the
// second), unlike f64::min and f64::max.
fn r2_max<T: PartialOrd>(a: T, b: T) -> T {
    if a > b { a } else { b }
}

fn r2_min<T: PartialOrd>(a: T, b: T) -> T {
    if a < b { a } else { b }
}

/// Sign of v + e1 * epsilon + e2 * epsilon squared as epsilon -> 0+.
pub fn sgi_soft_limit_sign(v: i64, e1: i64, e2: i64) -> i32 {
    let first = if v != 0 { v } else if e1 != 0 { e1 } else { e2 };

    (first > 0) as i32 - (first < 0) as i32
}

/// Whether |v + e1*epsilon + e2*epsilon squared| is strictly below bound.
pub fn sgi_soft_limit_abs(v: i64, e1: i64, e2: i64, bound: i64) -> bool {
    let sign = sgi_soft_limit_sign(v, e1, e2) as i64;

    sgi_soft_limit_sign(
        sign.wrapping_mul(v).wrapping_sub(bound),
        sign.wrapping_mul(e1),
        sign.wrapping_mul(e2),
    ) < 0
}

/// The endpoint minus (epsilon, epsilon squared) lies in the open diamond.
pub fn sgi_soft_diamond_contains(x: i64, y: i64, half: i64) -> bool {
    let sx = sgi_soft_limit_sign(x, -1, 0) as i64;
    let sy = sgi_soft_limit_sign(y, 0, -1) as i64;

    sgi_soft_limit_sign(
        sx.wrapping_mul(x).wrapping_add(sy.wrapping_mul(y)).wrapping_sub(half),
        -sx,
        -sy,
    ) < 0
}

/// A segment produces a fragment if it enters the open diamond and its
/// final endpoint is outside (OpenGL 1.2.1, 3.4.1, with the limiting
/// perturbation done in lexicographic coefficients on the snapped grid).
pub fn sgi_soft_line_covers(a: &SGISoftVertex, b: &SGISoftVertex, px: i64, py: i64, sub: i32) -> bool {
    let half: i64 = 1i64 << (sub - 1);
    let cx = px.wrapping_mul(1i64 << sub).wrapping_add(half);
    let cy = py.wrapping_mul(1i64 << sub).wrapping_add(half);
    let dx = b.x.wrapping_sub(a.x);
    let dy = b.y.wrapping_sub(a.y);
    let ex = cx.wrapping_sub(a.x);
    let ey = cy.wrapping_sub(a.y);
    let sdx: i32 = if dx > 0 { 1 } else { -1 };
    let sdy: i32 = if dy > 0 { 1 } else { -1 };

    if sgi_soft_diamond_contains(b.x.wrapping_sub(cx), b.y.wrapping_sub(cy), half) {
        return false;
    }
    if sgi_soft_diamond_contains(ex.wrapping_neg(), ey.wrapping_neg(), half) {
        return true;
    }
    // The perturbed column crossing has parameter (ex + epsilon) / dx.
    if dx != 0
        && sgi_soft_limit_sign(ex, 1, 0) * sdx >= 0
        && sgi_soft_limit_sign(dx.wrapping_sub(ex), -1, 0) * sdx >= 0
        && sgi_soft_limit_abs(
            dy.wrapping_mul(ex).wrapping_sub(ey.wrapping_mul(dx)),
            dy,
            dx.wrapping_neg(),
            half.wrapping_mul(dx.wrapping_abs()),
        )
    {
        return true;
    }
    dy != 0
        && sgi_soft_limit_sign(ey, 0, 1) * sdy >= 0
        && sgi_soft_limit_sign(dy.wrapping_sub(ey), 0, -1) * sdy >= 0
        && sgi_soft_limit_abs(
            dx.wrapping_mul(ey).wrapping_sub(ex.wrapping_mul(dy)),
            dy.wrapping_neg(),
            dx,
            half.wrapping_mul(dy.wrapping_abs()),
        )
}

#[derive(Clone, Copy, Default)]
pub struct SGISoftPoint2 {
    pub x: f64,
    pub y: f64,
}

pub fn sgi_soft_clip_edge(
    in_: &[SGISoftPoint2],
    n: u32,
    out: &mut [SGISoftPoint2],
    axis: u32,
    bound: f64,
    greater: bool,
) -> u32 {
    let mut count = 0usize;

    for i in 0..n as usize {
        let a = in_[i];
        let b = in_[(i + 1) % n as usize];
        let av = if axis != 0 { a.y } else { a.x };
        let bv = if axis != 0 { b.y } else { b.x };
        let ai = if greater { av >= bound } else { av <= bound };
        let bi = if greater { bv >= bound } else { bv <= bound };

        if ai != bi {
            let t = (bound - av) / (bv - av);
            let mut p = SGISoftPoint2 { x: a.x + t * (b.x - a.x), y: a.y + t * (b.y - a.y) };

            if axis != 0 {
                p.y = bound;
            } else {
                p.x = bound;
            }
            out[count] = p;
            count += 1;
        }
        if bi {
            out[count] = b;
            count += 1;
        }
    }
    count as u32
}

pub fn sgi_soft_polygon_square_area(polygon: &[SGISoftPoint2], n: u32, x: i32, y: i32) -> f64 {
    let mut a = [SGISoftPoint2::default(); 12];
    let mut b = [SGISoftPoint2::default(); 12];
    let mut area = 0.0f64;
    let mut n = n;

    a[..n as usize].copy_from_slice(&polygon[..n as usize]);
    n = sgi_soft_clip_edge(&a, n, &mut b, 0, x as f64, true);
    n = sgi_soft_clip_edge(&b, n, &mut a, 0, (x + 1) as f64, false);
    n = sgi_soft_clip_edge(&a, n, &mut b, 1, y as f64, true);
    n = sgi_soft_clip_edge(&b, n, &mut a, 1, (y + 1) as f64, false);
    for i in 0..n as usize {
        let p = &a[i];
        let q = &a[(i + 1) % n as usize];

        area += p.x * q.y - p.y * q.x;
    }
    r2_min(1.0, area.abs() * 0.5)
}

pub fn sgi_soft_line_coverage(ax: f64, ay: f64, bx: f64, by: f64, width: f64, x: i32, y: i32) -> f64 {
    let dx = bx - ax;
    let dy = by - ay;
    let length = dx.hypot(dy);

    if !(length > 0.0) || !(width > 0.0) {
        return 0.0;
    }
    let nx = -dy * width / (2.0 * length);
    let ny = dx * width / (2.0 * length);
    let rectangle = [
        SGISoftPoint2 { x: ax + nx, y: ay + ny },
        SGISoftPoint2 { x: bx + nx, y: by + ny },
        SGISoftPoint2 { x: bx - nx, y: by - ny },
        SGISoftPoint2 { x: ax - nx, y: ay - ny },
    ];
    sgi_soft_polygon_square_area(&rectangle, 4, x, y)
}

pub fn sgi_soft_circle_integral(x: f64, radius: f64) -> f64 {
    let y = r2_max(0.0, radius * radius - x * x).sqrt();

    0.5 * (x * y + radius * radius * (x / radius).asin())
}

pub fn sgi_soft_circle_coverage(cx: f64, cy: f64, radius: f64, x: i32, y: i32) -> f64 {
    let mut cuts = [0.0f64; 8];
    let mut area = 0.0f64;
    let mut n = 0usize;
    let lo = r2_max(x as f64 - cx, -radius);
    let hi = r2_min((x + 1) as f64 - cx, radius);

    if !(lo < hi) || !(radius > 0.0) {
        return 0.0;
    }
    cuts[n] = lo;
    n += 1;
    cuts[n] = hi;
    n += 1;
    for edge in 0..2 {
        let yedge = (if edge != 0 { y + 1 } else { y }) as f64 - cy;

        if yedge.abs() < radius {
            let root = (radius * radius - yedge * yedge).sqrt();

            if -root > lo && -root < hi {
                cuts[n] = -root;
                n += 1;
            }
            if root > lo && root < hi {
                cuts[n] = root;
                n += 1;
            }
        }
    }
    for i in 1..n {
        let value = cuts[i];
        let mut j = i;

        while j != 0 && cuts[j - 1] > value {
            cuts[j] = cuts[j - 1];
            j -= 1;
        }
        cuts[j] = value;
    }
    for i in 0..n.saturating_sub(1) {
        let a = cuts[i];
        let b = cuts[i + 1];
        let mid = (a + b) * 0.5;
        let circle = r2_max(0.0, radius * radius - mid * mid).sqrt();
        let ymin = y as f64 - cy;
        let ymax = (y + 1) as f64 - cy;
        let top = r2_min(ymax, circle);
        let bottom = r2_max(ymin, -circle);
        let arc = sgi_soft_circle_integral(b, radius) - sgi_soft_circle_integral(a, radius);

        if top <= bottom {
            continue;
        }
        area += (if top == circle { arc } else { ymax * (b - a) })
            - (if bottom == -circle { -arc } else { ymin * (b - a) });
    }
    r2_min(1.0, r2_max(0.0, area))
}

/// SGI_SMOOTH_TWO_PIXEL_FILTER's weight in one axis for a fragment whose
/// centre is d from a footprint of the given size: a box of that size
/// filtered by a one-pixel tent.
pub fn sgi_soft_filter_weight(size: f64, d: f64) -> f64 {
    r2_min(1.0, r2_max(0.0, (size + 1.0) * 0.5 - d.abs()))
}

pub fn sgi_soft_point_filter(cx: f64, cy: f64, size: f64, x: i32, y: i32) -> f64 {
    sgi_soft_filter_weight(size, cx - (x as f64 + 0.5)) * sgi_soft_filter_weight(size, cy - (y as f64 + 0.5))
}

pub fn sgi_soft_line_filter(ax: f64, ay: f64, bx: f64, by: f64, width: f64, x: i32, y: i32) -> f64 {
    let x_major = (bx - ax).abs() >= (by - ay).abs();
    let mut l0 = if x_major { ax } else { ay };
    let mut l1 = if x_major { bx } else { by };
    let mut n0 = if x_major { ay } else { ax };
    let mut n1 = if x_major { by } else { bx };
    let m = if x_major { x } else { y };
    let n = if x_major { y } else { x };

    if !(l1 != l0) {
        return 0.0;
    }
    if l1 < l0 {
        let mut t = l0;

        l0 = l1;
        l1 = t;
        t = n0;
        n0 = n1;
        n1 = t;
    }
    let lo = r2_max(l0, m as f64);
    let hi = r2_min(l1, m as f64 + 1.0);
    if !(hi > lo) {
        return 0.0;
    }
    let slope = (n1 - n0) / (l1 - l0);
    let minor = n0 + slope * ((lo + hi) * 0.5 - l0);
    let d = minor - (n as f64 + 0.5);
    r2_min(1.0, sgi_soft_filter_weight(width, d) * (1.0 + slope * slope).sqrt()) * (hi - lo)
}

pub fn sgi_raster_point_coverage(r: &SGIRasterRules, cx: f64, cy: f64, size: f64, x: i32, y: i32) -> f64 {
    if r.smooth_rule as u32 == SGI_SMOOTH_TWO_PIXEL_FILTER {
        return sgi_soft_point_filter(cx, cy, size, x, y);
    }
    sgi_soft_circle_coverage(cx, cy, size * 0.5, x, y)
}

pub fn sgi_raster_line_coverage(r: &SGIRasterRules, ax: f64, ay: f64, bx: f64, by: f64, width: f64, x: i32, y: i32) -> f64 {
    if r.smooth_rule as u32 == SGI_SMOOTH_TWO_PIXEL_FILTER {
        return sgi_soft_line_filter(ax, ay, bx, by, width, x, y);
    }
    sgi_soft_line_coverage(ax, ay, bx, by, width, x, y)
}

/// How far from a smoothed point's centre, or a smoothed line, in pixels,
/// a fragment can have coverage.
pub fn sgi_soft_smooth_reach(r: &SGIRasterRules, size: f64) -> f64 {
    if r.smooth_rule as u32 == SGI_SMOOTH_TWO_PIXEL_FILTER {
        (size + 1.0) * 0.5
    } else {
        size * 0.5
    }
}

#[derive(Clone, Copy, Default)]
pub struct SGISoftLineStipple {
    pub counter: u32,
    pub repeat: u32,
    pub pattern: u32,
    pub enabled: bool,
    pub consumed: bool,
}

pub fn sgi_soft_line_vertex_at(a: &SGISoftVertex, b: &SGISoftVertex, t: f64, out: &mut SGISoftVertex) {
    let wa = (1.0 - t) * a.inv_w;
    let wb = t * b.inv_w;
    let sum = wa + wb;

    out.x = (a.x as f64 + t * (b.x - a.x) as f64).round() as i64;
    out.y = (a.y as f64 + t * (b.y - a.y) as f64).round() as i64;
    out.z = a.z + t * (b.z - a.z);
    out.inv_w = (1.0 - t) * a.inv_w + t * b.inv_w;
    out.depth_w = (1.0 - t) * a.depth_w + t * b.depth_w;
    for k in 0..4 {
        out.rgba[k] = (wa * a.rgba[k] + wb * b.rgba[k]) / sum;
        out.texcoord[k] = (wa * a.texcoord[k] + wb * b.texcoord[k]) / sum;
    }
    out.index = (wa * a.index + wb * b.index) / sum;
    out.fog = (wa * a.fog + wb * b.fog) / sum;
    for i in 0..SGI_GL_CLIP_PLANES as usize {
        out.clip[i] = (wa * a.clip[i] + wb * b.clip[i]) / sum;
    }
}

/// Map and rasterize one aliased segment, already projected, with the
/// provoking vertex's colour when flat shaded. Width follows the
/// minor-axis shift and replication of 3.4.3. A smoothed segment takes the
/// coverage of the rules' smooth_rule (3.4.2), stippled as one-pixel pieces
/// along its length.
pub fn sgi_soft_line_projected(
    r: &SGISoftDraw,
    pa: &SGISoftVertex,
    pb: &SGISoftVertex,
    provoking: &[f32],
    offset: f64,
    stipple: Option<&mut SGISoftLineStipple>,
    client_clip: bool,
) {
    let mut stipple = stipple;
    let st = &*r.st;
    let smooth = st.shade_model != SGI_GL_FLAT;
    let sub = r.rules.subpixel_bits as i32;
    let one_sub = (1i32 << sub) as f64;
    let a = pa.clone();
    let b = pb.clone();
    let (mut bx0, mut by0, mut bx1, mut by1) = (0i32, 0i32, -1i32, -1i32);
    let visible = sgi_soft_bounds(r, &mut bx0, &mut by0, &mut bx1, &mut by1);

    if (!visible && stipple.is_none()) || (a.x == b.x && a.y == b.y) {
        return;
    }
    if r.line_smooth && stipple.as_ref().map_or(false, |s| !s.enabled) {
        let length = ((b.x - a.x) as f64 / one_sub).hypot((b.y - a.y) as f64 / one_sub);
        let s = stipple.as_mut().unwrap();
        let period = 16u32.wrapping_mul(s.repeat);

        // Unstippled, the segment is one rectangle (3.4.2): its fragments
        // are not split between one-pixel pieces, each blended on its own.
        // The count still moves on by the pieces a stipple would take.
        s.counter = s.counter.wrapping_add((length.ceil() % period as f64) as u32) % period;
        s.consumed = true;
        if !visible {
            return;
        }
        stipple = None;
    }
    if r.line_smooth && stipple.is_some() {
        let s = stipple.as_mut().unwrap();
        let length = ((b.x - a.x) as f64 / one_sub).hypot((b.y - a.y) as f64 / one_sub);
        let segments = length.ceil();
        let mut i = 0.0f64;

        while i < segments {
            let bit = (s.counter / s.repeat) % 16;
            let keep = !s.enabled || (s.pattern & (1u32 << bit)) != 0;

            s.counter = (s.counter.wrapping_add(1)) % 16u32.wrapping_mul(s.repeat);
            s.consumed = true;
            if keep {
                let mut start = SGISoftVertex::default();
                let mut end = SGISoftVertex::default();
                let t0 = i / length;
                let t1 = r2_min((i + 1.0) / length, 1.0);

                sgi_soft_line_vertex_at(&a, &b, t0, &mut start);
                sgi_soft_line_vertex_at(&a, &b, t1, &mut end);
                sgi_soft_line_projected(r, &start, &end, provoking, offset, None, client_clip);
            }
            i += 1.0;
        }
        return;
    }
    let mut texture_value = [0.0f64; 4];
    let mut texture_delta = [0.0f64; 4];
    let mut texture_length = 0.0f64;
    if !r.texture_base.is_null()
        && !sgi_soft_texture_line_gradient(
            &a,
            &b,
            sub as u32,
            r.texture.volume,
            r.texture.one_dimensional,
            &mut texture_value,
            &mut texture_delta,
            &mut texture_length,
        )
    {
        sgi_soft_texture_report(r, 16, "invalid primitive coefficients, primitive dropped");
        return;
    }
    let mut flat = [0.0f64; 4];
    for k in 0..4 {
        flat[k] = provoking[SGI_GL_V_COLOR + k] as f64;
    }
    let len2 = (b.x - a.x) as f64 * (b.x - a.x) as f64 + (b.y - a.y) as f64 * (b.y - a.y) as f64;
    let x_major = (b.x - a.x).wrapping_abs() >= (b.y - a.y).wrapping_abs();
    let mut shifted_a = a.clone();
    let mut shifted_b = b.clone();
    // Width <= INT32_MAX and sub <= 12 give a shift below 2^42.
    let shift = (r.line_width.wrapping_sub(1) as i64) * (1i64 << (sub - 1));
    if x_major {
        shifted_a.y -= shift;
        shifted_b.y -= shift;
    } else {
        shifted_a.x -= shift;
        shifted_b.x -= shift;
    }

    // Walk the major axis a pixel at a time, and in each step try the
    // minor-axis pixels within one of those the segment spans there.
    let mut major0 = ((if x_major { a.x.min(b.x) } else { a.y.min(b.y) }) >> sub) as i32;
    let mut major1 = ((if x_major { a.x.max(b.x) } else { a.y.max(b.y) }) >> sub) as i32;
    major0 -= 1;
    major1 += 1;
    if r.line_smooth {
        let pad = sgi_soft_smooth_reach(&r.rules, r.smooth_line_width);
        let l0 = (if x_major { a.x } else { a.y }) as f64;
        let l1 = (if x_major { b.x } else { b.y }) as f64;

        major0 = (r2_min(l0, l1) / one_sub - pad).floor() as i32 - 1;
        major1 = (r2_max(l0, l1) / one_sub + pad).ceil() as i32 + 1;
    }
    let mut step = 1i32;
    if stipple.is_some() {
        // Count coverage before ownership/scissor, in submitted direction.
        if if x_major { b.x < a.x } else { b.y < a.y } {
            let tmp = major0;

            major0 = major1;
            major1 = tmp;
            step = -1;
        }
    } else {
        major0 = major0.max(if x_major { bx0 } else { by0 });
        major1 = major1.min(if x_major { bx1 } else { by1 });
    }
    let mut mm = major0;
    loop {
        if !(if step > 0 { mm <= major1 } else { mm >= major1 }) {
            break;
        }
        let m = mm;
        mm += step;
        let (lo, hi) = if x_major {
            if a.x <= b.x { (&shifted_a, &shifted_b) } else { (&shifted_b, &shifted_a) }
        } else if a.y <= b.y {
            (&shifted_a, &shifted_b)
        } else {
            (&shifted_b, &shifted_a)
        };
        let l0 = (if x_major { lo.x } else { lo.y }) as f64;
        let l1 = (if x_major { hi.x } else { hi.y }) as f64;
        let n0 = (if x_major { lo.y } else { lo.x }) as f64;
        let n1 = (if x_major { hi.y } else { hi.x }) as f64;
        let mut e0 = r2_max(m as f64 * one_sub, l0);
        let mut e1 = r2_min((m + 1) as f64 * one_sub, l1);
        let f0: f64;
        let f1: f64;

        if l1 > l0 {
            e0 = r2_min(e0, l1);
            e1 = r2_max(e1, l0);
            f0 = n0 + (n1 - n0) * (e0 - l0) / (l1 - l0);
            f1 = n0 + (n1 - n0) * (e1 - l0) / (l1 - l0);
        } else {
            f0 = n0;
            f1 = n1;
        }
        let mut minor0 = (r2_min(f0, f1) / one_sub).floor() as i64 - 1;
        let mut minor1 = (r2_max(f0, f1) / one_sub).floor() as i64 + 1;
        if r.line_smooth {
            let pad = sgi_soft_smooth_reach(&r.rules, r.smooth_line_width);
            let q0 = (if x_major { a.y } else { a.x }) as f64;
            let q1 = (if x_major { b.y } else { b.x }) as f64;

            minor0 = (r2_min(q0, q1) / one_sub - pad).floor() as i64 - 1;
            minor1 = (r2_max(q0, q1) / one_sub + pad).ceil() as i64 + 1;
        }
        let mut nn = minor0;
        while nn <= minor1 {
            let n = nn;
            nn += 1;
            let base_x: i64 = if x_major { m as i64 } else { n };
            let base_y: i64 = if x_major { n } else { m as i64 };
            let replica0: i64 = r2_max(n, (if x_major { by0 } else { bx0 }) as i64);
            let replica1: i64 = if r.line_smooth {
                replica0
            } else {
                r2_min(n + r.line_width as i64 - 1, (if x_major { by1 } else { bx1 }) as i64)
            };
            let mut keep = true;
            let coverage = if r.line_smooth {
                sgi_raster_line_coverage(
                    &r.rules,
                    a.x as f64 / one_sub,
                    a.y as f64 / one_sub,
                    b.x as f64 / one_sub,
                    b.y as f64 / one_sub,
                    r.smooth_line_width,
                    base_x as i32,
                    base_y as i32,
                )
            } else if sgi_soft_line_covers(&shifted_a, &shifted_b, base_x, base_y, sub) {
                1.0
            } else {
                0.0
            };

            if !(coverage > 0.0) {
                continue;
            }
            if let Some(s) = stipple.as_mut() {
                let bit = (s.counter / s.repeat) % 16;

                keep = !s.enabled || (s.pattern & (1u32 << bit)) != 0;
                s.counter = s.counter.wrapping_add(1) % 16u32.wrapping_mul(s.repeat);
                s.consumed = true;
            }
            if !keep
                || !visible
                || replica0 > replica1
                || m < (if x_major { bx0 } else { by0 })
                || m > (if x_major { bx1 } else { by1 })
            {
                continue;
            }
            // Visit only drawable-visible replicas, never width entries.
            let mut replica = replica0;
            while replica <= replica1 {
                let rep = replica;
                replica += 1;
                let px: i32 = if x_major { m } else { rep as i32 };
                let py: i32 = if x_major { rep as i32 } else { m };
                let cx = (px as f64 + 0.5) * one_sub;
                let cy = (py as f64 + 0.5) * one_sub;
                let mut frag = SGISoftFrag::default();

                frag.fixed = false;
                let mut t = ((cx - a.x as f64) * (b.x - a.x) as f64 + (cy - a.y as f64) * (b.y - a.y) as f64) / len2;
                // Approximation: retain endpoint values, not extrapolation.
                t = r2_min(r2_max(t, 0.0), 1.0);
                let wa = (1.0 - t) * a.inv_w;
                let wb = t * b.inv_w;
                frag.index = if smooth {
                    a.index + wb * (b.index - a.index) / (wa + wb)
                } else {
                    provoking[SGI_GL_V_INDEX] as f64
                };
                for k in 0..4 {
                    frag.texcoord[k] = (wa * a.texcoord[k] + wb * b.texcoord[k]) / (wa + wb);
                }
                frag.fog = (wa * a.fog + wb * b.fog) / (wa + wb);
                if st.clip_planes != 0 && client_clip {
                    let mut d = [0.0f64; SGI_GL_CLIP_PLANES as usize];

                    for i in 0..SGI_GL_CLIP_PLANES as usize {
                        d[i] = (wa * a.clip[i] + wb * b.clip[i]) / (wa + wb);
                    }
                    if sgi_soft_clipped(st, &d) {
                        continue;
                    }
                }
                for k in 0..4 {
                    frag.rgba[k] = if smooth { (wa * a.rgba[k] + wb * b.rgba[k]) / (wa + wb) } else { flat[k] };
                }
                if !r.texture_base.is_null() {
                    let mut value = [0.0f64; 4];
                    let mut out = [0.0f64; 5];
                    let zero = [0.0f64; 4];

                    for k in 0..4 {
                        value[k] = texture_value[k] + t * texture_delta[k];
                    }
                    frag.texture_rho = if sgi_soft_texture_derivatives(
                        &value,
                        &texture_delta,
                        &zero,
                        r.texture_base.width as f64 * sgi_soft_texture_lod_scale(r.texture.lod_scale_s),
                        if r.texture.one_dimensional {
                            0.0
                        } else {
                            r.texture_base.height as f64 * sgi_soft_texture_lod_scale(r.texture.lod_scale_t)
                        },
                        if r.texture.volume {
                            r.texture_base.depth as f64 * sgi_soft_texture_lod_scale(r.texture.lod_scale_r)
                        } else {
                            0.0
                        },
                        &mut out,
                    ) {
                        out[4] / texture_length
                    } else {
                        f64::NAN
                    };
                }
                sgi_soft_fragment_coverage(
                    r,
                    px,
                    py,
                    a.z + t * (b.z - a.z),
                    offset,
                    (1.0 - t) * a.depth_w + t * b.depth_w,
                    &frag,
                    coverage,
                    r.line_smooth,
                );
            }
        }
    }
}

/// Keep standalone client-plane rejection after geometric counting.
pub fn sgi_soft_line(r: &SGISoftDraw, va: &[f32], vb: &[f32], provoking: &[f32], stipple: Option<&mut SGISoftLineStipple>) {
    let mut ca = SGIVolumeVertex::default();
    let mut cb = SGIVolumeVertex::default();
    let mut a = SGISoftVertex::default();
    let mut b = SGISoftVertex::default();
    let (mut bx0, mut by0, mut bx1, mut by1) = (0i32, 0i32, 0i32, 0i32);

    if stipple.is_none() && !sgi_soft_bounds(r, &mut bx0, &mut by0, &mut bx1, &mut by1) {
        return;
    }
    sgi_volume_from_draw_vertex(&r.st, va, &mut ca);
    sgi_volume_from_draw_vertex(&r.st, vb, &mut cb);
    if sgi_volume_clip_segment(&mut ca, &mut cb) && sgi_soft_project(r, &ca, &mut a) && sgi_soft_project(r, &cb, &mut b) {
        sgi_soft_line_projected(r, &a, &b, provoking, 0.0, stipple, true);
    }
}

/// Aliased square-point centring follows OpenGL 1.2.1, 3.3, on the shared
/// snapped subpixel grid. A smoothed point takes the coverage of the
/// rules' smooth_rule about its snapped centre (3.3.2).
pub fn sgi_soft_point_projected(r: &SGISoftDraw, pv: &SGISoftVertex, provoking: Option<&[f32]>, offset: f64) {
    let sub = r.rules.subpixel_bits as i32;
    let v = pv.clone();
    let mut frag = SGISoftFrag::default();
    let (mut bx0, mut by0, mut bx1, mut by1) = (0i32, 0i32, 0i32, 0i32);
    let psize = r.point_size as i64;

    if !sgi_soft_bounds(r, &mut bx0, &mut by0, &mut bx1, &mut by1) {
        return;
    }
    // Even dimensions centre on a pixel corner; odd ones on a centre.
    let px: i64 = (v.x + (if r.point_size % 2 != 0 { 0 } else { 1i64 << (sub - 1) })) >> sub;
    let py: i64 = (v.y + (if r.point_size % 2 != 0 { 0 } else { 1i64 << (sub - 1) })) >> sub;
    let mut x0: i64 = r2_max(px - psize / 2, bx0 as i64);
    let mut y0: i64 = r2_max(py - psize / 2, by0 as i64);
    let mut x1: i64 = r2_min(px - psize / 2 + psize - 1, bx1 as i64);
    let mut y1: i64 = r2_min(py - psize / 2 + psize - 1, by1 as i64);
    frag.rgba = v.rgba;
    frag.index = v.index;
    frag.fixed = false;
    if let Some(p) = provoking {
        if r.st.shade_model == SGI_GL_FLAT {
            frag.index = p[SGI_GL_V_INDEX] as f64;
            for k in 0..4 {
                frag.rgba[k] = p[SGI_GL_V_COLOR + k] as f64;
            }
        }
    }
    frag.fog = v.fog;
    frag.texture_rho = 1.0;
    frag.texcoord = v.texcoord;
    if r.point_smooth {
        let scale = (1i32 << sub) as f64;
        let cx = v.x as f64 / scale;
        let cy = v.y as f64 / scale;
        let radius = sgi_soft_smooth_reach(&r.rules, r.smooth_point_size);

        x0 = r2_max((cx - radius).floor() as i64, bx0 as i64);
        y0 = r2_max((cy - radius).floor() as i64, by0 as i64);
        x1 = r2_min((cx + radius).ceil() as i64 - 1, bx1 as i64);
        y1 = r2_min((cy + radius).ceil() as i64 - 1, by1 as i64);
        let mut y = y0;
        while y <= y1 {
            let mut x = x0;
            while x <= x1 {
                let coverage = sgi_raster_point_coverage(&r.rules, cx, cy, r.smooth_point_size, x as i32, y as i32);

                sgi_soft_fragment_coverage(r, x as i32, y as i32, v.z, offset, v.depth_w, &frag, coverage, true);
                x += 1;
            }
            y += 1;
        }
        return;
    }
    let mut y = y0;
    while y <= y1 {
        let mut x = x0;
        while x <= x1 {
            sgi_soft_fragment(r, x as i32, y as i32, v.z, offset, v.depth_w, &frag);
            x += 1;
        }
        y += 1;
    }
}

pub fn sgi_soft_point(r: &SGISoftDraw, vp: &[f32]) {
    let mut cv = SGIVolumeVertex::default();
    let mut v = SGISoftVertex::default();

    sgi_volume_from_draw_vertex(&r.st, vp, &mut cv);
    if sgi_volume_contains(&cv) && sgi_soft_project(r, &cv, &mut v) && !sgi_soft_clipped(&r.st, &v.clip) {
        sgi_soft_point_projected(r, &v, None, 0.0);
    }
}

/// Internal representation bound, not an SGI width range. Half ties round up.
pub fn sgi_soft_dimension(requested: f32, rounded: &mut u32) -> bool {
    let n = if requested == 0.0 { 1.0 } else { (requested as f64 + 0.5).floor() };

    if !requested.is_finite() || requested < 0.0 || n > i32::MAX as f64 {
        crate::qlog!("sgi-gl: unsupported aliased dimension {}", requested);
        return false;
    }
    *rounded = r2_max(n, 1.0) as u32;
    true
}

pub fn sgi_soft_boundary_edge(v: &SGIVolumeVertex) -> bool {
    v.attr[SGI_GL_V_EDGE].to_bits() != 0
}

pub fn sgi_soft_boundary_set_edge(v: &mut SGIVolumeVertex, enabled: bool) {
    v.attr[SGI_GL_V_EDGE] = f32::from_bits(enabled as u32);
}

pub fn sgi_soft_boundary_plane(r: &SGISoftDraw, v: &SGIVolumeVertex, p: u32) -> f64 {
    if p < SGI_VOLUME_PLANES as u32 {
        sgi_volume_plane(v, p as _)
    } else {
        let plane = &r.st.clip_plane[(p - SGI_VOLUME_PLANES as u32) as usize];

        plane[0] as f64 * v.eye[0] + plane[1] as f64 * v.eye[1] + plane[2] as f64 * v.eye[2] + plane[3] as f64 * v.eye[3]
    }
}

/// Carry outgoing flags through complete polygon clipping (2.11). Original
/// retained edges keep their flags; the newly connecting cut edges are
/// true. The C swaps its two workspace pointers; here the result is left
/// in `poly` by swapping the vectors at the end.
pub fn sgi_soft_boundary_clip(
    r: &SGISoftDraw,
    poly: &mut Vec<SGIVolumeVertex>,
    other: &mut Vec<SGIVolumeVertex>,
    count: &mut usize,
    capacity: usize,
) -> bool {
    let mut src_is_poly = true;
    let mut n = *count;

    for p in 0..(SGI_VOLUME_PLANES as u32 + SGI_GL_CLIP_PLANES) {
        let mut m = 0usize;

        if p >= SGI_VOLUME_PLANES as u32 && (r.st.clip_planes & (1u32 << (p - SGI_VOLUME_PLANES as u32))) == 0 {
            continue;
        }
        let (src, dst): (&Vec<SGIVolumeVertex>, &mut Vec<SGIVolumeVertex>) =
            if src_is_poly { (&*poly, &mut *other) } else { (&*other, &mut *poly) };
        for i in 0..n {
            let a = &src[i];
            let b = &src[(i + 1) % n];
            let da = sgi_soft_boundary_plane(r, a, p);
            let db = sgi_soft_boundary_plane(r, b, p);

            if da >= 0.0 {
                if m == capacity {
                    return false;
                }
                dst[m] = a.clone();
                m += 1;
                if !(db >= 0.0) {
                    if da == 0.0 {
                        sgi_soft_boundary_set_edge(&mut dst[m - 1], true);
                    } else {
                        if m == capacity {
                            return false;
                        }
                        sgi_volume_cut(a, da, b, db, &mut dst[m]);
                        sgi_soft_boundary_set_edge(&mut dst[m], true);
                        m += 1;
                    }
                }
            } else if db > 0.0 {
                if m == capacity {
                    return false;
                }
                sgi_volume_cut(b, db, a, da, &mut dst[m]);
                let edge = sgi_soft_boundary_edge(a);
                sgi_soft_boundary_set_edge(&mut dst[m], edge);
                m += 1;
            }
        }
        n = m;
        src_is_poly = !src_is_poly;
        if n == 0 {
            break;
        }
    }
    if !src_is_poly {
        std::mem::swap(poly, other);
    }
    *count = n;
    true
}

pub fn sgi_soft_polygon_vertex<'a>(d: &SGIGLDraw, indices: Option<&[u32]>, i: usize) -> &'a [f32] {
    let index = match indices {
        Some(ix) => ix[i] as usize,
        None => i,
    };

    unsafe { std::slice::from_raw_parts(d.vertices.0.add(index * SGI_GL_VERTEX_FLOATS), SGI_GL_VERTEX_FLOATS) }
}

#[derive(Clone, Copy, Default)]
pub struct SGISoftAAPoint {
    pub x: f64,
    pub y: f64,
}

pub type SGISoftAAEdge = u32;
pub const SGI_AA_LEFT: SGISoftAAEdge = 0;
pub const SGI_AA_RIGHT: SGISoftAAEdge = 1;
pub const SGI_AA_BOTTOM: SGISoftAAEdge = 2;
pub const SGI_AA_TOP: SGISoftAAEdge = 3;

pub fn sgi_soft_aa_inside(p: &SGISoftAAPoint, edge: SGISoftAAEdge, value: f64) -> bool {
    match edge {
        SGI_AA_LEFT => p.x >= value,
        SGI_AA_RIGHT => p.x <= value,
        SGI_AA_BOTTOM => p.y >= value,
        SGI_AA_TOP => p.y <= value,
        _ => unreachable!(),
    }
}

pub fn sgi_soft_aa_intersection(a: &SGISoftAAPoint, b: &SGISoftAAPoint, edge: SGISoftAAEdge, value: f64) -> SGISoftAAPoint {
    let t = if edge <= SGI_AA_RIGHT { (value - a.x) / (b.x - a.x) } else { (value - a.y) / (b.y - a.y) };

    SGISoftAAPoint { x: a.x + t * (b.x - a.x), y: a.y + t * (b.y - a.y) }
}

/// Clip a convex window-space polygon to one side of a pixel square.
pub fn sgi_soft_aa_clip(
    src: &[SGISoftAAPoint],
    n: usize,
    dst: &mut [SGISoftAAPoint],
    capacity: usize,
    edge: SGISoftAAEdge,
    value: f64,
    out_n: &mut usize,
) -> bool {
    let mut m = 0usize;

    if n == 0 {
        *out_n = 0;
        return true;
    }
    let mut a = &src[n - 1];
    for i in 0..n {
        let b = &src[i];
        let ai = sgi_soft_aa_inside(a, edge, value);
        let bi = sgi_soft_aa_inside(b, edge, value);

        if ai != bi {
            if m == capacity {
                return false;
            }
            dst[m] = sgi_soft_aa_intersection(a, b, edge, value);
            m += 1;
        }
        if bi {
            if m == capacity {
                return false;
            }
            dst[m] = *b;
            m += 1;
        }
        a = b;
    }
    *out_n = m;
    true
}

pub fn sgi_soft_aa_edge(a: &SGISoftVertex, b: &SGISoftVertex, x: f64, y: f64) -> f64 {
    (b.x.wrapping_sub(a.x)) as f64 * (y - a.y as f64) - (b.y.wrapping_sub(a.y)) as f64 * (x - a.x as f64)
}

/// A convex polygon contains the square when it contains all four corners.
pub fn sgi_soft_aa_contains_pixel(v: &[SGISoftVertex], n: usize, px: i32, py: i32, scale: f64, ccw: bool) -> bool {
    for i in 0..n {
        let a = &v[i];
        let b = &v[(i + 1) % n];
        let x = px as f64 * scale;
        let y = py as f64 * scale;

        if ccw {
            if sgi_soft_aa_edge(a, b, x, y) < 0.0
                || sgi_soft_aa_edge(a, b, x + scale, y) < 0.0
                || sgi_soft_aa_edge(a, b, x, y + scale) < 0.0
                || sgi_soft_aa_edge(a, b, x + scale, y + scale) < 0.0
            {
                return false;
            }
        } else if sgi_soft_aa_edge(a, b, x, y) > 0.0
            || sgi_soft_aa_edge(a, b, x + scale, y) > 0.0
            || sgi_soft_aa_edge(a, b, x, y + scale) > 0.0
            || sgi_soft_aa_edge(a, b, x + scale, y + scale) > 0.0
        {
            return false;
        }
    }
    true
}

/// Rasterize one clipped convex polygon once. Pixel-square clipping
/// computes geometric coverage; centroid sampling is exact for affine
/// datums and an approximation for perspective-correct varyings.
pub fn sgi_soft_polygon_coverage(
    r: &SGISoftDraw,
    v: &[SGISoftVertex],
    n: usize,
    flat_color: &[f32],
    flat_index: f64,
    offset: f64,
) {
    let mut flat = SGISoftFrag::default();
    let (mut px0, mut py0, mut px1, mut py1) = (0i32, 0i32, 0i32, 0i32);
    let sub = r.rules.subpixel_bits as i32;
    let scale = (1i32 << sub) as f64;
    let mut polygon_area = 0.0f64;

    if n < 3 || n > usize::MAX - 8 || !sgi_soft_bounds(r, &mut px0, &mut py0, &mut px1, &mut py1) {
        return;
    }
    let capacity = n + 8;
    let mut a = vec![SGISoftAAPoint::default(); capacity];
    let mut b = vec![SGISoftAAPoint::default(); capacity];
    let mut min_x = v[0].x;
    let mut max_x = v[0].x;
    let mut min_y = v[0].y;
    let mut max_y = v[0].y;
    for i in 0..n {
        let next = &v[(i + 1) % n];

        min_x = min_x.min(v[i].x);
        max_x = max_x.max(v[i].x);
        min_y = min_y.min(v[i].y);
        max_y = max_y.max(v[i].y);
        polygon_area += v[i].x.wrapping_mul(next.y).wrapping_sub(next.x.wrapping_mul(v[i].y)) as f64;
    }
    if !(polygon_area.abs() > 0.0) {
        return;
    }
    px0 = px0.max((min_x >> sub) as i32);
    px1 = px1.min((max_x >> sub) as i32);
    py0 = py0.max((min_y >> sub) as i32);
    py1 = py1.min((max_y >> sub) as i32);
    for k in 0..4 {
        flat.rgba[k] = flat_color[k] as f64;
    }
    flat.index = flat_index;

    for py in py0..=py1 {
        for px in px0..=px1 {
            let coverage: f64;
            let x: f64;
            let y: f64;
            let z: f64;
            let mut bary = [0.0f64; 3];
            let mut w = [0.0f64; 3];
            let mut fan_i = 0usize;
            let mut frag = flat.clone();
            let mut found = false;
            let mut background = false;

            if !sgi_soft_stipple_pass(r, px, py, &mut background) {
                continue;
            }
            if sgi_soft_aa_contains_pixel(v, n, px, py, scale, polygon_area > 0.0) {
                coverage = 1.0;
                x = px as f64 + 0.5;
                y = py as f64 + 0.5;
            } else {
                let mut count = n;
                let mut twice_area = 0.0f64;
                let mut cx = 0.0f64;
                let mut cy = 0.0f64;
                let mut src_is_a = true;

                // Restore the polygon before clipping this pixel.
                for i in 0..n {
                    a[i] = SGISoftAAPoint { x: v[i].x as f64 / scale, y: v[i].y as f64 / scale };
                }
                for e in SGI_AA_LEFT..=SGI_AA_TOP {
                    let bound = if e == SGI_AA_LEFT {
                        px as f64
                    } else if e == SGI_AA_RIGHT {
                        (px + 1) as f64
                    } else if e == SGI_AA_BOTTOM {
                        py as f64
                    } else {
                        (py + 1) as f64
                    };
                    let ok = if src_is_a {
                        sgi_soft_aa_clip(&a, count, &mut b, capacity, e, bound, &mut count)
                    } else {
                        sgi_soft_aa_clip(&b, count, &mut a, capacity, e, bound, &mut count)
                    };

                    if !ok {
                        crate::qlog!("sgi-gl: polygon coverage clip exceeded its bound");
                        return;
                    }
                    src_is_a = !src_is_a;
                    if count < 3 {
                        break;
                    }
                }
                if count < 3 {
                    continue;
                }
                let src = if src_is_a { &a } else { &b };
                for i in 0..count {
                    let p = &src[i];
                    let q = &src[(i + 1) % count];
                    let pxp = p.x - px as f64;
                    let pyp = p.y - py as f64;
                    let qxp = q.x - px as f64;
                    let qyp = q.y - py as f64;
                    let cross = pxp * qyp - qxp * pyp;

                    twice_area += cross;
                    cx += (pxp + qxp) * cross;
                    cy += (pyp + qyp) * cross;
                }
                if !(twice_area.abs() > 0.0) {
                    continue;
                }
                coverage = r2_min(1.0, twice_area.abs() * 0.5);
                x = px as f64 + cx / (3.0 * twice_area);
                y = py as f64 + cy / (3.0 * twice_area);
            }

            // Find the fan triangle containing the overlap centroid.
            let mut i = 1usize;
            while i + 1 < n {
                let den = sgi_soft_aa_edge(&v[0], &v[i], v[i + 1].x as f64, v[i + 1].y as f64);

                if den == 0.0 {
                    i += 1;
                    continue;
                }
                bary[0] = sgi_soft_aa_edge(&v[i], &v[i + 1], x * scale, y * scale) / den;
                bary[1] = sgi_soft_aa_edge(&v[i + 1], &v[0], x * scale, y * scale) / den;
                bary[2] = 1.0 - bary[0] - bary[1];
                if bary[0] >= -0.000000001 && bary[1] >= -0.000000001 && bary[2] >= -0.000000001 {
                    found = true;
                    fan_i = i;
                    break;
                }
                i += 1;
            }
            if !found {
                continue;
            }
            {
                let i = fan_i;

                w[0] = bary[0] * v[0].inv_w;
                w[1] = bary[1] * v[i].inv_w;
                w[2] = bary[2] * v[i + 1].inv_w;
                let sum = w[0] + w[1] + w[2];
                let depth_w = bary[0] * v[0].depth_w + bary[1] * v[i].depth_w + bary[2] * v[i + 1].depth_w;

                if !(sum > 0.0) || !sum.is_finite() {
                    continue;
                }
                let fan = [v[0].clone(), v[i].clone(), v[i + 1].clone()];

                z = bary[0] * v[0].z + bary[1] * v[i].z + bary[2] * v[i + 1].z;
                for k in 0..4 {
                    frag.texcoord[k] =
                        (w[0] * v[0].texcoord[k] + w[1] * v[i].texcoord[k] + w[2] * v[i + 1].texcoord[k]) / sum;
                    if r.st.shade_model != SGI_GL_FLAT {
                        frag.rgba[k] = (w[0] * v[0].rgba[k] + w[1] * v[i].rgba[k] + w[2] * v[i + 1].rgba[k]) / sum;
                    }
                }
                frag.fog = (w[0] * v[0].fog + w[1] * v[i].fog + w[2] * v[i + 1].fog) / sum;
                if r.st.shade_model != SGI_GL_FLAT {
                    frag.index = v[0].index
                        + (w[1] * (v[i].index - v[0].index) + w[2] * (v[i + 1].index - v[0].index)) / sum;
                }
                if r.st.clip_planes != 0 {
                    let mut d = [0.0f64; SGI_GL_CLIP_PLANES as usize];

                    for k in 0..SGI_GL_CLIP_PLANES as usize {
                        d[k] = (w[0] * fan[0].clip[k] + w[1] * fan[1].clip[k] + w[2] * fan[2].clip[k]) / sum;
                    }
                    if sgi_soft_clipped(&r.st, &d) {
                        continue;
                    }
                }
                if !r.texture_base.is_null() {
                    let mut gradient = SGISoftTexPlane::default();
                    let mut value = [0.0f64; 4];
                    let mut out = [0.0f64; 5];

                    if !sgi_soft_texture_plane(&fan, sub as u32, r.texture.volume, r.texture.one_dimensional, &mut gradient) {
                        continue;
                    }
                    for k in 0..4 {
                        value[k] = gradient.value[k]
                            + (px as f64 + 0.5 - gradient.x) * gradient.dx[k]
                            + (py as f64 + 0.5 - gradient.y) * gradient.dy[k];
                    }
                    frag.texture_rho = if sgi_soft_texture_derivatives(
                        &value,
                        &gradient.dx,
                        &gradient.dy,
                        r.texture_base.width as f64 * sgi_soft_texture_lod_scale(r.texture.lod_scale_s),
                        if r.texture.one_dimensional {
                            0.0
                        } else {
                            r.texture_base.height as f64 * sgi_soft_texture_lod_scale(r.texture.lod_scale_t)
                        },
                        if r.texture.volume {
                            r.texture_base.depth as f64 * sgi_soft_texture_lod_scale(r.texture.lod_scale_r)
                        } else {
                            0.0
                        },
                        &mut out,
                    ) {
                        out[4]
                    } else {
                        f64::NAN
                    };
                }
                if background {
                    sgi_soft_stipple_background(r, &mut frag);
                }
                sgi_soft_fragment_coverage(r, px, py, z, offset, depth_w, &frag, coverage, true);
            }
        }
    }
}

pub fn sgi_soft_polygon_smooth(
    r: &SGISoftDraw,
    poly: &[SGIVolumeVertex],
    n: usize,
    provoking: &[f32],
    front: bool,
    slope: f64,
) {
    let mut v: Vec<SGISoftVertex> = vec![SGISoftVertex::default(); n];
    let mut offset = 0.0f64;
    let mut flat_color = &provoking[SGI_GL_V_COLOR..SGI_GL_V_COLOR + 4];
    let mut flat_index = provoking[SGI_GL_V_INDEX];

    if !front && r.back_colors {
        flat_color = &provoking[SGI_GL_V_BACK_COLOR..SGI_GL_V_BACK_COLOR + 4];
        flat_index = provoking[SGI_GL_V_BACK_INDEX];
    }
    for i in 0..n {
        if !sgi_soft_project(r, &poly[i], &mut v[i]) {
            return;
        }
        if !front && r.back_colors {
            let back = &poly[i].attr[SGI_GL_V_BACK_COLOR..SGI_GL_V_BACK_COLOR + 4];

            for k in 0..4 {
                v[i].rgba[k] = r2_min(r2_max(back[k], 0.0f32), 1.0f32) as f64;
            }
            v[i].index = poly[i].attr[SGI_GL_V_BACK_INDEX] as f64;
        }
    }
    if !r.texture_base.is_null() {
        let mut i = 1usize;
        while i + 1 < n {
            let fan = [v[0].clone(), v[i].clone(), v[i + 1].clone()];

            if !sgi_soft_texture_coefficients(r, &fan) {
                return;
            }
            i += 1;
        }
    }
    if r.st.polygon_offset & SGI_GL_OFFSET_FILL != 0 {
        offset = slope * r.st.offset_factor as f64
            + (r.st.offset_units / sgi_soft_depth_max(&r.rules) as f32) as f64
            + r.st.offset_bias as f64;
    }
    sgi_soft_polygon_coverage(r, &v, n, flat_color, flat_index as f64, offset);
}

/// Replay the original fan, including its validation and fragment ordering.
pub fn sgi_soft_polygon_fill(r: &SGISoftDraw, d: &SGIGLDraw, indices: Option<&[u32]>, n: usize, provoking: &[f32]) {
    let v0 = sgi_soft_polygon_vertex(d, indices, 0);
    let prevalidate = !r.texture.is_null() && !(d.mode == SGI_GL_POLYGON && d.fill_by_triangle);

    if prevalidate {
        let mut i = 1usize;
        while i + 1 < n {
            if !sgi_soft_texture_triangle_valid(
                r,
                v0,
                sgi_soft_polygon_vertex(d, indices, i),
                sgi_soft_polygon_vertex(d, indices, i + 1),
            ) {
                return;
            }
            i += 1;
        }
    }
    let mut i = 1usize;
    while i + 1 < n {
        sgi_soft_triangle(
            r,
            v0,
            sgi_soft_polygon_vertex(d, indices, i),
            sgi_soft_polygon_vertex(d, indices, i + 1),
            provoking,
        );
        i += 1;
    }
}

/// Workspace bounds are internal representations, not hardware maxima.
pub fn sgi_soft_polygon(
    r: &SGISoftDraw,
    d: &SGIGLDraw,
    indices: Option<&[u32]>,
    count: usize,
    provoking: &[f32],
    force_edges: bool,
) -> SGIGLDrawResult {
    let mut positions = r.clone();
    let mut coverage = r.clone();
    let mut fan: [SGISoftVertex; 3] = Default::default();
    let mut first = SGISoftVertex::default();
    let mut previous: SGISoftVertex;
    let mut current = SGISoftVertex::default();
    let mut stipple = SGISoftLineStipple {
        enabled: d.line_stipple_enabled,
        repeat: r2_max(1, r2_min(d.line_stipple_repeat, 256)),
        pattern: d.line_stipple_pattern,
        ..Default::default()
    };
    let mut flat = [0.0f32; SGI_GL_VERTEX_FLOATS];
    let mut n = count;
    let mut area = 0.0f64;
    let mut slope = 0.0f64;
    let mut offset = 0.0f64;
    let front: bool;
    let mode: u32;

    if count < 3 {
        return SGI_GL_DRAW_OK;
    }
    if count > usize::MAX - SGI_VOLUME_PLANES as usize - SGI_GL_CLIP_PLANES as usize {
        return SGI_GL_DRAW_OUT_OF_MEMORY;
    }
    let capacity = count + SGI_VOLUME_PLANES as usize + SGI_GL_CLIP_PLANES as usize;
    let mut poly: Vec<SGIVolumeVertex> = vec![SGIVolumeVertex::default(); capacity];
    let mut other: Vec<SGIVolumeVertex> = vec![SGIVolumeVertex::default(); capacity];
    // Facing must not prevalidate texture Q and erase a valid FILL prefix.
    positions.texture = P::null();
    for i in 0..count {
        sgi_volume_from_draw_vertex(&r.st, sgi_soft_polygon_vertex(d, indices, i), &mut poly[i]);
        for k in 0..4 {
            if !poly[i].clip[k].is_finite() || !poly[i].eye[k].is_finite() {
                crate::qlog!("sgi-gl: invalid polygon position dropped");
                return SGI_GL_DRAW_OK;
            }
        }
        if force_edges {
            sgi_soft_boundary_set_edge(&mut poly[i], true);
        }
    }
    if !sgi_soft_boundary_clip(r, &mut poly, &mut other, &mut n, capacity) {
        crate::qlog!("sgi-gl: polygon exceeds convex clipping bound");
        return SGI_GL_DRAW_OK;
    }
    if n == 0 || !sgi_soft_project(&positions, &poly[0], &mut first) {
        return SGI_GL_DRAW_OK;
    }
    previous = first.clone();
    fan[0] = first.clone();
    for i in 1..n {
        if !sgi_soft_project(&positions, &poly[i], &mut current) {
            return SGI_GL_DRAW_OK;
        }
        if i >= 2 {
            area += sgi_soft_edge(&first, &previous, current.x, current.y) as f64;
            fan[1] = previous.clone();
            fan[2] = current.clone();
            slope = r2_max(slope, sgi_soft_slope(r, &fan));
        }
        previous = current.clone();
    }
    if r.st.window_matrix_given != 0 && r.st.window_rows_down != 0 {
        area = -area;
    }
    if r.st.front_face == SGI_GL_CW {
        area = -area;
    }
    front = area > 0.0; // Zero remains back-facing through both reversals.
    if r.st.cull_face != 0
        && (r.st.cull_face_mode == SGI_GL_FRONT_AND_BACK
            || r.st.cull_face_mode == (if front { SGI_GL_FRONT } else { SGI_GL_BACK }))
    {
        return SGI_GL_DRAW_OK;
    }
    mode = unsafe { *d.polygon_modes.0.add(if front { 0 } else { 1 }) };
    if mode == SGI_GL_FILL {
        if r.polygon_smooth {
            sgi_soft_polygon_smooth(r, &poly, n, provoking, front, slope);
            return SGI_GL_DRAW_OK;
        }
        sgi_soft_polygon_fill(r, d, indices, count, provoking);
        return SGI_GL_DRAW_OK;
    }
    if mode != SGI_GL_LINE && mode != SGI_GL_POINT {
        crate::qlog!("sgi-gl: invalid polygon mode {:#x} ignored", mode);
        return SGI_GL_DRAW_OK;
    }
    if (mode == SGI_GL_LINE && !sgi_soft_dimension(d.line_width, &mut coverage.line_width))
        || (mode == SGI_GL_POINT && !sgi_soft_dimension(d.point_size, &mut coverage.point_size))
    {
        return SGI_GL_DRAW_OK;
    }
    flat.copy_from_slice(&provoking[..SGI_GL_VERTEX_FLOATS]);
    if !front && r.back_colors {
        flat[SGI_GL_V_COLOR..SGI_GL_V_COLOR + 4].copy_from_slice(&provoking[SGI_GL_V_BACK_COLOR..SGI_GL_V_BACK_COLOR + 4]);
        for i in 0..n {
            poly[i].attr.copy_within(SGI_GL_V_BACK_COLOR..SGI_GL_V_BACK_COLOR + 4, SGI_GL_V_COLOR);
        }
    }
    // Validate the entire boundary before any LINE/POINT fragment or sample.
    for i in 0..n {
        if !sgi_soft_project(r, &poly[i], &mut current) {
            return SGI_GL_DRAW_OK;
        }
        if !r.texture_base.is_null() && mode == SGI_GL_LINE {
            let mut next = SGISoftVertex::default();
            let mut values = [0.0f64; 4];
            let mut delta = [0.0f64; 4];
            let mut length = 0.0f64;

            if !sgi_soft_project(r, &poly[(i + 1) % n], &mut next)
                || !sgi_soft_texture_line_gradient(
                    &current,
                    &next,
                    r.rules.subpixel_bits as u32,
                    r.texture.volume,
                    r.texture.one_dimensional,
                    &mut values,
                    &mut delta,
                    &mut length,
                )
            {
                sgi_soft_texture_report(r, 16, "invalid primitive coefficients, primitive dropped");
                return SGI_GL_DRAW_OK;
            }
        }
    }
    let offset_enable = if mode == SGI_GL_LINE { SGI_GL_OFFSET_LINE } else { SGI_GL_OFFSET_POINT };
    if r.st.polygon_offset & offset_enable != 0 {
        offset = slope * r.st.offset_factor as f64
            + (r.st.offset_units / sgi_soft_depth_max(&r.rules) as f32) as f64
            + r.st.offset_bias as f64;
    }
    for i in 0..n {
        let mut next = SGISoftVertex::default();

        if !sgi_soft_boundary_edge(&poly[i]) {
            continue;
        }
        sgi_soft_project(r, &poly[i], &mut current);
        if mode == SGI_GL_POINT {
            sgi_soft_point_projected(&coverage, &current, Some(&flat), offset);
        } else {
            sgi_soft_project(r, &poly[(i + 1) % n], &mut next);
            sgi_soft_line_projected(&coverage, &current, &next, &flat, offset, Some(&mut stipple), false);
        }
    }
    SGI_GL_DRAW_OK
}

/// Dispatch each independently assembled polygon, preserving provoking rules.
pub fn sgi_soft_polygon_modes(r: &SGISoftDraw, d: &SGIGLDraw) -> SGIGLDrawResult {
    let fill_modes: [u32; 2] = [SGI_GL_FILL, SGI_GL_FILL];
    let mut indices = [0u32; 4];
    let n = d.count;

    if d.polygon_modes.is_null() {
        let mut fill = *d;

        fill.polygon_modes = P(fill_modes.as_ptr() as *mut u32);
        return sgi_soft_polygon_modes(r, &fill);
    }

    if d.mode == SGI_GL_POLYGON {
        return sgi_soft_polygon(r, d, None, n as usize, sgi_soft_polygon_vertex(d, None, 0), false);
    }
    let stride: u32 = if d.mode == SGI_GL_TRIANGLES {
        3
    } else if d.mode == SGI_GL_QUADS {
        4
    } else if d.mode == SGI_GL_QUAD_STRIP {
        2
    } else {
        1
    };
    let mut i = 0u32;
    while i < n {
        let count: u32 = if d.mode == SGI_GL_QUADS || d.mode == SGI_GL_QUAD_STRIP { 4 } else { 3 };
        let provoking = i + count - 1;
        let force = stride < 3;

        if n - i < count {
            break;
        }
        for k in 0..count {
            indices[k as usize] = i + k;
        }
        if d.mode == SGI_GL_TRIANGLE_STRIP && (i & 1) != 0 {
            indices[0] = i + 1;
            indices[1] = i;
        } else if d.mode == SGI_GL_TRIANGLE_FAN {
            indices[0] = 0;
        } else if d.mode == SGI_GL_QUAD_STRIP {
            indices[2] = i + 3;
            indices[3] = i + 2;
        }
        let result = sgi_soft_polygon(
            r,
            d,
            Some(&indices[..count as usize]),
            count as usize,
            sgi_soft_polygon_vertex(d, None, provoking as usize),
            force,
        );
        if result != SGI_GL_DRAW_OK {
            return result;
        }
        i += stride;
    }
    SGI_GL_DRAW_OK
}

/// The filled primitives of a draw, from SGI_GL_TRIANGLES on, in order.
pub fn sgi_soft_draw_triangles(r: &SGISoftDraw, d: &SGIGLDraw) {
    let n = d.count as usize;
    let v = |i: usize| -> &[f32] { sgi_soft_polygon_vertex(d, None, i) };

    match d.mode {
        SGI_GL_TRIANGLES => {
            let mut i = 0usize;
            while i + 2 < n {
                sgi_soft_triangle(r, v(i), v(i + 1), v(i + 2), v(i + 2));
                i += 3;
            }
        }
        SGI_GL_TRIANGLE_STRIP => {
            let mut i = 0usize;
            while i + 2 < n {
                if i & 1 != 0 {
                    sgi_soft_triangle(r, v(i + 1), v(i), v(i + 2), v(i + 2));
                } else {
                    sgi_soft_triangle(r, v(i), v(i + 1), v(i + 2), v(i + 2));
                }
                i += 1;
            }
        }
        SGI_GL_TRIANGLE_FAN | SGI_GL_POLYGON => {
            if !r.texture.is_null() && d.mode == SGI_GL_POLYGON && !d.fill_by_triangle {
                let mut i = 0usize;
                while i + 2 < n {
                    if !sgi_soft_texture_triangle_valid(r, v(0), v(i + 1), v(i + 2)) {
                        return;
                    }
                    i += 1;
                }
            }
            let mut i = 0usize;
            while i + 2 < n {
                sgi_soft_triangle(r, v(0), v(i + 1), v(i + 2), if d.mode == SGI_GL_POLYGON { v(0) } else { v(i + 2) });
                i += 1;
            }
        }
        SGI_GL_QUADS => {
            let mut i = 0usize;
            while i + 3 < n {
                if !r.texture.is_null()
                    && (!sgi_soft_texture_triangle_valid(r, v(i), v(i + 1), v(i + 2))
                        || !sgi_soft_texture_triangle_valid(r, v(i), v(i + 2), v(i + 3)))
                {
                    i += 4;
                    continue;
                }
                sgi_soft_triangle(r, v(i), v(i + 1), v(i + 2), v(i + 3));
                sgi_soft_triangle(r, v(i), v(i + 2), v(i + 3), v(i + 3));
                i += 4;
            }
        }
        SGI_GL_QUAD_STRIP => {
            // Vertices 2i - 1, 2i, 2i + 2 and 2i + 1 in order (2.6.1).
            let mut i = 0usize;
            while i + 3 < n {
                if !r.texture.is_null()
                    && (!sgi_soft_texture_triangle_valid(r, v(i), v(i + 1), v(i + 3))
                        || !sgi_soft_texture_triangle_valid(r, v(i), v(i + 3), v(i + 2)))
                {
                    i += 2;
                    continue;
                }
                sgi_soft_triangle(r, v(i), v(i + 1), v(i + 3), v(i + 3));
                sgi_soft_triangle(r, v(i), v(i + 3), v(i + 2), v(i + 3));
                i += 2;
            }
        }
        _ => unreachable!(),
    }
}

/// About how many pixels a draw's filled primitives cover: the area of each
/// one with its vertices moved into the pixels the window owns, summed
/// until it reaches limit. A vertex with no window position counts as
/// reaching it.
pub fn sgi_soft_draw_pixels(r: &SGISoftDraw, d: &SGIGLDraw, limit: u64) -> u64 {
    let n = d.count as usize;
    let mut sum = 0u64;
    let (mut x0, mut y0, mut x1, mut y1) = (0i32, 0i32, 0i32, 0i32);

    if !sgi_soft_bounds(r, &mut x0, &mut y0, &mut x1, &mut y1) || n < 3 {
        return 0;
    }
    let mut xy = vec![0.0f64; 2 * n];
    for i in 0..n {
        let mut cv = SGIVolumeVertex::default();
        let mut win = [0.0f64; 3];

        sgi_volume_from_draw_vertex(&r.st, sgi_soft_polygon_vertex(d, None, i), &mut cv);
        if !(cv.clip[3] > 0.0) {
            return limit;
        }
        sgi_gl_clip_to_window(&r.st, &cv.clip, &mut win);
        xy[2 * i] = win[0];
        xy[2 * i + 1] = win[1];
    }
    // Each primitive's own vertices; a fan's first is left out.
    let step: usize = if d.mode == SGI_GL_TRIANGLES {
        3
    } else if d.mode == SGI_GL_QUADS {
        4
    } else if d.mode == SGI_GL_QUAD_STRIP {
        2
    } else {
        1
    };
    let size: usize = if d.mode == SGI_GL_QUADS || d.mode == SGI_GL_QUAD_STRIP { 4 } else { 3 };
    let mut i = 0usize;
    while i + size <= n && sum < limit {
        let mut px = [0.0f64; 4];
        let mut py = [0.0f64; 4];
        let mut twice = 0.0f64;

        for k in 0..size {
            px[k] = r2_min(r2_max(xy[2 * (i + k)], x0 as f64), (x1 + 1) as f64);
            py[k] = r2_min(r2_max(xy[2 * (i + k) + 1], y0 as f64), (y1 + 1) as f64);
        }
        for k in 0..size {
            let next = (k + 1) % size;

            twice += px[k] * py[next] - px[next] * py[k];
        }
        sum = sum.wrapping_add((twice.abs() / 2.0) as u64);
        i += step;
    }
    sum
}

/// Whether every vertex of a draw has the texture coordinates its image
/// uses: a draw with one that has not drops that primitive unread, so it
/// reads its texels as it reaches them rather than all of them first.
pub fn sgi_soft_fixed_coordinates_valid(r: &SGISoftDraw, d: &SGIGLDraw) -> bool {
    for i in 0..d.count as usize {
        let mut v = SGIVolumeVertex::default();

        sgi_volume_from_draw_vertex(&r.st, sgi_soft_polygon_vertex(d, None, i), &mut v);
        if !sgi_soft_texcoord_valid(&r.texture, r.texture_base.as_opt().map(|b| &*b), &v) {
            return false;
        }
    }
    true
}

/// A draw handed to the band pool: pointers to the caller's draw, which
/// stays alive until every band has returned.
pub struct SGISoftBandDraw {
    pub r: P<SGISoftDraw>,
    pub d: P<SGIGLDraw>,
}

pub fn sgi_soft_band_draw(opaque: Opaque, band: u32, bands: u32) {
    let job: &SGISoftBandDraw = from_opaque(opaque);
    let mut r: SGISoftDraw = *job.r;

    r.band = band;
    r.bands = bands;
    if !r.texture.is_null() {
        r.texels = sgi_soft_texel_cache_begin(&r.texture);
    }
    if !r.fixed.is_null() && !r.fixed.prefilled && !sgi_soft_fixed_begin(&r) {
        r.fixed = P::null();
    }
    sgi_soft_band_quiet.with(|q| q.set(band > 0));
    sgi_soft_draw_triangles(&r, &job.d);
    sgi_soft_band_quiet.with(|q| q.set(false));
}

/// Draw the filled primitives in bands when the target allows it and they
/// cover enough pixels to pay for repeating each primitive's setup in
/// every band; false, having drawn nothing, otherwise.
pub fn sgi_soft_draw_parallel(r: &SGISoftDraw, d: &SGIGLDraw) -> bool {
    let bands = sgi_soft_bands();
    let min_pixels = sgi_soft_band_min_pixels(&r.t);
    let mut job = SGISoftBandDraw { r: P::from_ref(r), d: P::from_ref(d) };

    if !r.t.concurrent_pixels || bands < 2 || sgi_soft_draw_pixels(r, d, min_pixels) < min_pixels {
        return false;
    }
    sgi_soft_bands_run(sgi_soft_band_draw, P::new(&mut job).opaque(), bands)
}

/// Draw a draw's primitives.
pub fn sgi_soft_draw(rules: &SGIRasterRules, t: &SGIRasterTarget, d: &SGIGLDraw) -> SGIGLDrawResult {
    let mut r = SGISoftDraw {
        rules: P::from_ref(rules),
        t: P::from_ref(t),
        st: d.state,
        back_colors: d.back_colors,
        point_smooth: rules.smooth_primitives && d.point_smooth,
        line_smooth: rules.smooth_primitives && d.line_smooth,
        smooth_point_size: d.point_size as f64,
        smooth_line_width: d.line_width as f64,
        polygon_smooth: rules.smooth_primitives && d.polygon_smooth,
        polygon_stipple: if d.polygon_stipple_enabled { d.polygon_stipple } else { P::null() },
        ..Default::default()
    };
    let mut line = SGISoftLineStipple {
        counter: if !d.line_phase.is_null() && !d.line_phase_reset { *d.line_phase } else { 0 },
        enabled: d.line_stipple_enabled,
        repeat: r2_max(1, r2_min(d.line_stipple_repeat, 256)),
        pattern: d.line_stipple_pattern,
        consumed: false,
    };
    let use_stipple = !d.line_phase.is_null() || d.line_stipple_enabled;
    let n = d.count as usize;
    let mut texture_reports = 0u32;
    let mut rgba_reported = false;
    let mut fixed = SGISoftFixedTex::default();

    line.counter %= 16u32.wrapping_mul(line.repeat);
    r.texture_reports = P::new(&mut texture_reports);
    r.rgba_reported = P::new(&mut rgba_reported);

    let v = |i: usize| -> &[f32] { sgi_soft_polygon_vertex(d, None, i) };
    r.ntargets = sgi_gl_draw_targets(&d.state, &mut r.targets);
    if !sgi_soft_check_rules(rules) || !sgi_soft_check_index(t, d.state.index_bits) {
        return SGI_GL_DRAW_OK;
    }
    if !sgi_soft_window(&d.state, &mut r.win) {
        if !use_stipple
            || d.mode < SGI_GL_LINES
            || d.mode > SGI_GL_LINE_STRIP
            || d.state.drawable.bound == 0
            || d.state.drawable.clip.n != 0
        {
            return SGI_GL_DRAW_OK;
        }
        // Empty ownership still counts standalone geometric coverage.
        r.win.x1 = -1;
        r.win.y1 = -1;
    }
    if !sgi_soft_texture_setup(&mut r, true) {
        return SGI_GL_DRAW_OK;
    }
    if !r.texture.is_null() {
        r.texels = sgi_soft_texel_cache_begin(&r.texture);
    }
    if !sgi_soft_setup_ops(&mut r) {
        return SGI_GL_DRAW_OK;
    }
    if d.mode >= SGI_GL_TRIANGLES && d.mode <= SGI_GL_POLYGON && sgi_soft_fixed_setup(&r, &mut fixed) {
            let mut texels = 0u64;
        let mut first = 0u32;
        let mut last = 0u32;

        r.fixed = P::new(&mut fixed);
        r.fixed.get().span = sgi_soft_fixed_span_select(&r);
        sgi_soft_fixed_reach(&r.fixed, &mut first, &mut last);
        for i in first..=last {
            texels += r.fixed.level[i as usize].width as u64 * r.fixed.level[i as usize].height as u64;
        }
        // A draw as large as the smallest that bands share, at least.
        texels = r2_max(texels, SGI_RASTER_PARALLEL_MIN_PIXELS);
        if !(sgi_soft_draw_pixels(&r, d, texels) >= texels
            && sgi_soft_fixed_coordinates_valid(&r, d)
            && sgi_soft_fixed_prefill(&r, r.fixed.get()))
            && !sgi_soft_fixed_begin(&r)
        {
            r.fixed = P::null();
        }
    }
    if !rules.smooth_primitives {
        let mut smooth: Option<&str> = None;

        if d.mode == SGI_GL_POINTS && d.point_smooth {
            smooth = Some("point");
        } else if d.mode >= SGI_GL_LINES && d.mode <= SGI_GL_LINE_STRIP && d.line_smooth {
            smooth = Some("line");
        } else if d.mode >= SGI_GL_TRIANGLES && d.mode <= SGI_GL_POLYGON && d.polygon_smooth {
            smooth = Some("polygon");
        }
        if let Some(s) = smooth {
            crate::qlog!("sgi-gl: {} smoothing requested but unsupported by raster rules; drawn aliased", s);
        }
    }
    if r.polygon_smooth && d.mode >= SGI_GL_TRIANGLES && d.mode <= SGI_GL_POLYGON {
        return sgi_soft_polygon_modes(&r, d);
    }
    if !d.polygon_modes.is_null()
        && d.mode >= SGI_GL_TRIANGLES
        && d.mode <= SGI_GL_POLYGON
        && (unsafe { *d.polygon_modes.0 } != SGI_GL_FILL || unsafe { *d.polygon_modes.0.add(1) } != SGI_GL_FILL)
    {
        return sgi_soft_polygon_modes(&r, d);
    }
    match d.mode {
        SGI_GL_POINTS => {
            if !sgi_soft_dimension(d.point_size, &mut r.point_size) {
                return SGI_GL_DRAW_OK;
            }
            for i in 0..n {
                sgi_soft_point(&r, v(i));
            }
            SGI_GL_DRAW_OK
        }
        SGI_GL_LINES => {
            if !sgi_soft_dimension(d.line_width, &mut r.line_width) {
                return SGI_GL_DRAW_OK;
            }
            let mut i = 0usize;
            while i + 1 < n {
                if d.line_phase.is_null() {
                    line.counter = 0;
                }
                sgi_soft_line(&r, v(i), v(i + 1), v(i + 1), if use_stipple { Some(&mut line) } else { None });
                i += 2;
            }
            if !d.line_phase.is_null() && line.consumed {
                *d.line_phase.get() = line.counter;
            }
            SGI_GL_DRAW_OK
        }
        SGI_GL_LINE_STRIP | SGI_GL_LINE_LOOP => {
            if !sgi_soft_dimension(d.line_width, &mut r.line_width) {
                return SGI_GL_DRAW_OK;
            }
            let mut i = 0usize;
            while i + 1 < n {
                sgi_soft_line(&r, v(i), v(i + 1), v(i + 1), if use_stipple { Some(&mut line) } else { None });
                i += 1;
            }
            if d.mode == SGI_GL_LINE_LOOP && n >= 2 {
                sgi_soft_line(&r, v(n - 1), v(0), v(0), if use_stipple { Some(&mut line) } else { None });
            }
            if !d.line_phase.is_null() && line.consumed {
                *d.line_phase.get() = line.counter;
            }
            SGI_GL_DRAW_OK
        }
        SGI_GL_TRIANGLES | SGI_GL_TRIANGLE_STRIP | SGI_GL_TRIANGLE_FAN | SGI_GL_POLYGON | SGI_GL_QUADS
        | SGI_GL_QUAD_STRIP => {
            if !sgi_soft_draw_parallel(&r, d) {
                sgi_soft_draw_triangles(&r, d);
            }
            SGI_GL_DRAW_OK
        }
        _ => {
            crate::qlog!("sgi-gl: draw of mode {} ignored", d.mode);
            SGI_GL_DRAW_OK
        }
    }
}

fn r2_clamp(val: f64, low: f64, high: f64) -> f64 {
    if val > high { high } else if val < low { low } else { val }
}

/// A run of a bitmap's image (OpenGL 1.2.1, 3.7): a fragment for each set
/// bit, at the image's corner plus the bit's column and row, bounded by the
/// window the draw state's drawable owns and not by the viewport.
pub fn sgi_soft_bitmap(rules: &SGIRasterRules, t: &SGIRasterTarget, op: &SGIRasterOp) {
    let b = &op.bitmap;
    let mut r = SGISoftDraw { rules: P::from_ref(rules), t: P::from_ref(t), st: b.state, ..Default::default() };
    let row_bytes: u32 = (b.width + 7) / 8;
    let mut rgba_reported = false;
    let mut texture_reports = 0u32;
    let mut frag = SGISoftFrag::default();

    frag.rgba = [b.rgba[0] as f64, b.rgba[1] as f64, b.rgba[2] as f64, b.rgba[3] as f64];
    frag.texcoord = [b.texcoord[0] as f64, b.texcoord[1] as f64, b.texcoord[2] as f64, b.texcoord[3] as f64];
    frag.index = b.index as f64;
    frag.fog = b.distance as f64;

    r.rgba_reported = P::new(&mut rgba_reported);
    r.texture_reports = P::new(&mut texture_reports);
    if !sgi_soft_texture_setup(&mut r, true) || !sgi_soft_window(&b.state, &mut r.win) || !sgi_soft_check_index(t, b.state.index_bits) {
        return;
    }
    if !r.texture.is_null() {
        r.texels = sgi_soft_texel_cache_begin(&r.texture);
    }
    r.ntargets = r2_min(op.ntargets, SGI_GL_MAX_TARGETS);
    r.targets = op.targets;
    if !sgi_soft_setup_ops(&mut r) {
        return;
    }
    let mut i = 0u32;
    while i < b.count && row_bytes != 0 {
        let byte = b.first.wrapping_add(i);
        let row: i64 = (byte / row_bytes) as i64;
        let col0: i64 = ((byte % row_bytes).wrapping_mul(8)) as i64;

        if row >= b.height as i64 {
            break;
        }
        let mut bit = 0i32;
        while bit < 8 && col0 + (bit as i64) < b.width as i64 {
            let px: i64 = b.x as i64 + col0 + bit as i64;
            let py: i64 = b.y as i64 + row;
            let byte_value = unsafe { *b.bytes.0.add(i as usize) } as i32;

            bit += 1;
            let bit_now = bit - 1;
            if (byte_value & (0x80 >> bit_now)) == 0
                || px < (r.win.x0 - r.win.ox) as i64
                || px > (r.win.x1 - r.win.ox) as i64
                || py < (r.win.y0 - r.win.oy) as i64
                || py > (r.win.y1 - r.win.oy) as i64
            {
                continue;
            }
            sgi_soft_fragment_at(&r, px as i32, py as i32, b.z as f64, &frag, 1.0, false);
        }
        i += 1;
    }
}

pub fn sgi_soft_pixels(rules: &SGIRasterRules, t: &SGIRasterTarget, op: &SGIRasterOp) {
    let p = &op.pixels;
    let image = !p.rgba.is_null()
        || !p.rgba_float.is_null()
        || !p.rgba_double.is_null()
        || !p.depth.is_null()
        || !p.stencil.is_null()
        || !p.index.is_null();
    let mut r = SGISoftDraw { rules: P::from_ref(rules), t: P::from_ref(t), st: p.state, ..Default::default() };
    let mut frag = SGISoftFrag::default();
    let mut rgba_reported = false;
    let mut texture_reports = 0u32;
    let mut stencil_max = 0u32;
    let mut stencil_mask = 0u32;
    let ex = p.x + p.zoom[0] as f64 * p.width as f64;
    let ey = p.y + p.zoom[1] as f64 * p.height as f64;

    frag.fog = p.distance as f64;
    frag.texcoord = [p.texcoord[0] as f64, p.texcoord[1] as f64, p.texcoord[2] as f64, p.texcoord[3] as f64];
    r.rgba_reported = P::new(&mut rgba_reported);
    r.texture_reports = P::new(&mut texture_reports);
    if (p.stencil.is_null() && !sgi_soft_texture_setup(&mut r, true))
        || (p.stencil.is_null() && !sgi_soft_check_index(t, p.state.index_bits))
        || p.width == 0
        || p.height == 0
        || !image
        || !p.x.is_finite()
        || !p.y.is_finite()
        || !p.zoom[0].is_finite()
        || !p.zoom[1].is_finite()
        || p.zoom[0] == 0.0
        || p.zoom[1] == 0.0
        || !sgi_soft_window(&p.state, &mut r.win)
    {
        return;
    }
    if !r.texture.is_null() {
        r.texels = sgi_soft_texel_cache_begin(&r.texture);
    }
    r.ntargets = r2_min(op.ntargets, SGI_GL_MAX_TARGETS);
    r.targets = op.targets;
    if !p.stencil.is_null() {
        if !sgi_soft_stencil_max(rules, t, &mut stencil_max) {
            return;
        }
        stencil_mask = p.state.stencil_write_mask & stencil_max;
        if stencil_mask == 0 {
            return;
        }
    } else if !sgi_soft_setup_ops(&mut r) {
        return;
    }
    // Include lower/left centres and exclude upper/right centres (3.6.4).
    let bx0 = r2_max((r2_min(p.x, ex) - 0.5).ceil(), (r.win.x0 as i64 - r.win.ox as i64) as f64);
    let by0 = r2_max((r2_min(p.y, ey) - 0.5).ceil(), (r.win.y0 as i64 - r.win.oy as i64) as f64);
    let bx1 = r2_min((r2_max(p.x, ex) - 0.5).ceil() - 1.0, (r.win.x1 as i64 - r.win.ox as i64) as f64);
    let by1 = r2_min((r2_max(p.y, ey) - 0.5).ceil() - 1.0, (r.win.y1 as i64 - r.win.oy as i64) as f64);
    if bx0 > bx1 || by0 > by1 {
        return;
    }
    // Visible bounds keep conversion and work independent of zoom size.
    let x0 = bx0 as i64;
    let y0 = by0 as i64;
    let x1 = bx1 as i64;
    let y1 = by1 as i64;
    let mut y = y0;
    while y <= y1 {
        let cy = y;
        y += 1;
        let qy = (cy as f64 + 0.5 - p.y) / p.zoom[1] as f64;
        let sy = if p.zoom[1] > 0.0 { qy.floor() } else { qy.ceil() - 1.0 };

        if !(sy >= 0.0 && sy < p.height as f64) {
            continue;
        }
        let mut x = x0;
        while x <= x1 {
            let cx = x;
            x += 1;
            let qx = (cx as f64 + 0.5 - p.x) / p.zoom[0] as f64;
            let sx = if p.zoom[0] > 0.0 { qx.floor() } else { qx.ceil() - 1.0 };

            if !(sx >= 0.0 && sx < p.width as f64) {
                continue;
            }
            let at: u64 = (sy as u64).wrapping_mul(p.width as u64).wrapping_add(sx as u32 as u64);
            if !p.stencil.is_null() {
                let px = r.win.ox + cx as i32;
                let py = r.win.oy + cy as i32;

                if sgi_soft_owned_at(&r, cx as i32, cy as i32) {
                    let old = (t.load_stencil.unwrap())(t.opaque, px, py);
                    let src = unsafe { *p.stencil.0.add(at as usize) };

                    (t.store_stencil.unwrap())(t.opaque, px, py, ((old & !stencil_mask) | (src & stencil_mask)) & stencil_max);
                }
                continue;
            }
            frag.index = if !p.index.is_null() {
                (unsafe { *p.index.0.add(at as usize) }) as f64
            } else {
                p.colour_index as f64
            };
            let rgba: u32 = if !p.rgba.is_null() { unsafe { *p.rgba.0.add(at as usize) } } else { 0 };
            for k in 0..4 {
                let component: f64 = if !p.depth.is_null() {
                    p.colour[k] as f64
                } else if !p.rgba_double.is_null() {
                    unsafe { (*p.rgba_double.0.add(at as usize))[k] }
                } else if !p.rgba_float.is_null() {
                    unsafe { (*p.rgba_float.0.add(at as usize))[k] as f64 }
                } else {
                    ((rgba >> (24 - 8 * k)) & 0xff) as f64 / 255.0
                };

                frag.rgba[k] = r2_clamp(component * p.transfer.scale[k] as f64 + p.transfer.bias[k] as f64, 0.0, 1.0);
            }
            let z: f64 = if !p.depth.is_null() {
                r2_clamp(
                    unsafe { *p.depth.0.add(at as usize) } * p.depth_scale as f64 + p.depth_bias as f64,
                    0.0,
                    1.0,
                )
            } else {
                p.z as f64
            };

            sgi_soft_fragment_at(&r, cx as i32, cy as i32, z, &frag, 1.0, false);
        }
    }
}

/// Whether one rectangle of a non-empty clip list holds all of row y from
/// x0 to x1, so that each pixel's own test would pass.
pub fn sgi_soft_clip_covers_row(clip: &SGIClipList, x0: i32, x1: i32, y: i32) -> bool {
    for i in 0..r2_min(clip.n, SGI_CLIP_MAX as u32) {
        let r = &clip.r[i as usize];

        if y >= r.y0 && y <= r.y1 && x0 >= r.x0 && x1 <= r.x1 {
            return true;
        }
    }
    false
}

/// A clear's decided values, which every row of it stores.
pub struct SGISoftClear<'a> {
    pub rules: &'a SGIRasterRules,
    pub t: &'a SGIRasterTarget,
    pub op: &'a SGIRasterOp,
    pub colour: bool,
    pub depth: bool,
    pub stencil: bool,
    pub rgba: bool,
    pub rgba_read: bool,
    pub rgb: u32,
    pub smax: u32,
    pub mask: u32,
    pub rgba_mask: u32,
    pub ntargets: u32,
    pub depth_value: u32,
    pub clear: SGIRasterColour,
    pub rgba_reported: P<bool>,
}

/// The rows of a clear that are band's of bands (sgi_soft_row_in_band()).
pub fn sgi_soft_clear_rows(c: &SGISoftClear, band: u32, bands: u32) {
    let rules = c.rules;
    let t = c.t;
    let op = c.op;
    // Only a read fills it, and only a read's targets are read back.
    let mut dest = [SGIRasterColour::default(); SGI_GL_MAX_TARGETS as usize];

    for y in op.y0..=op.y1 {
        if !(bands <= 1 || ((y as u32) >> SGI_SOFT_BAND_ROWS_SHIFT) % bands == band) {
            continue;
        }
        let whole_row = op.clip.n <= 1 || sgi_soft_clip_covers_row(&op.clip, op.x0, op.x1, y);
        for x in op.x0..=op.x1 {
            if !whole_row && !sgi_clip_contains(&op.clip, x, y) {
                continue;
            }

            if c.rgba && c.rgba_read {
                dest = [SGIRasterColour::default(); SGI_GL_MAX_TARGETS as usize];
                if !sgi_soft_rgba_load(rules, t, &op.targets, c.ntargets, x, y, &mut dest, c.rgba_reported.get()) {
                    continue;
                }
            }
            if c.depth {
                (t.store_depth.unwrap())(t.opaque, x, y, c.depth_value);
            }
            if c.stencil {
                let smask = op.stencil_mask & c.smax;

                (t.store_stencil.unwrap())(
                    t.opaque,
                    x,
                    y,
                    ((t.load_stencil.unwrap())(t.opaque, x, y) & !smask & c.smax) | (op.clear_stencil & smask),
                );
            }
            let mut i = 0u32;
            while c.colour && i < c.ntargets {
                if op.index_bits != 0 {
                    (t.store_index.unwrap())(t.opaque, op.targets[i as usize], x, y, op.clear_index, op.index_mask);
                } else if c.rgba {
                    let mut out = c.clear;

                    for k in 0..4 {
                        if c.rgba_mask & (1u32 << k) == 0 {
                            out.component[k] = dest[i as usize].component[k];
                        }
                    }
                    (t.store_rgba.unwrap())(t.opaque, op.targets[i as usize], x, y, &out);
                } else if c.mask == sgi_soft_colour_bits(rules) {
                    (t.store_colour.unwrap())(t.opaque, op.targets[i as usize], x, y, c.rgb);
                } else {
                    (t.store_colour.unwrap())(
                        t.opaque,
                        op.targets[i as usize],
                        x,
                        y,
                        (c.rgb & c.mask) | ((t.load_colour.unwrap())(t.opaque, op.targets[i as usize], x, y) & !c.mask),
                    );
                }
                i += 1;
            }
        }
    }
}

/// sgi_soft_clear_rows() as the band pool calls it.
pub fn sgi_soft_clear_rows_band(opaque: Opaque, band: u32, bands: u32) {
    let c: &SGISoftClear = from_opaque(opaque);

    sgi_soft_clear_rows(c, band, bands);
}

/// CLEAR writes the clear colour under the colour mask, or in colour-index
/// mode the clear index under the index mask, to the op's targets, the
/// clear depth to the depth plane and the clear stencil value under its
/// mask to the stencil buffer, where the planes have one, at every pixel of
/// its rectangle its clip list keeps. BITMAP draws its run of a bitmap, and
/// PIXELS draws a zoomed RGBA rectangle. The other kinds are not executed
/// here yet.
pub fn sgi_soft_raster(rules: &SGIRasterRules, t: &SGIRasterTarget, op: &SGIRasterOp) {
    let mut colour = op.clear_buffers & SGI_GL_COLOR_BUFFER_BIT != 0;
    let depth = op.clear_buffers & SGI_GL_DEPTH_BUFFER_BIT != 0;
    let mut stencil = op.clear_buffers & SGI_GL_STENCIL_BUFFER_BIT != 0;
    let mut rgb = 0u32;
    let mut smax = 0u32;
    let mut mask = sgi_soft_colour_bits(rules);
    let rgba_mask = sgi_soft_rgba_mask(rules, op.clear_colour_mask);
    let rgba = op.index_bits == 0 && t.store_rgba.is_some();
    let rgba_read = colour
        && op.ntargets != 0
        && rgba_mask != 0
        && rgba_mask != sgi_soft_rgba_mask(rules, SGI_GL_COLOUR_MASK_ALL);
    let ntargets = r2_min(op.ntargets, SGI_GL_MAX_TARGETS);
    let mut rgba_reported = false;
    let mut clear = SGIRasterColour::default();

    if !sgi_soft_check_rules(rules) {
        return;
    }
    // After the check: the depth maximum shifts by 32 - depth_bits.
    let depth_value = op.clear_depth24 & sgi_soft_depth_max(rules);
    if op.kind == SGI_RASTER_PIXELS {
        sgi_soft_pixels(rules, t, op);
        return;
    }
    if op.kind == SGI_RASTER_BITMAP {
        sgi_soft_bitmap(rules, t, op);
        return;
    }
    if op.kind != SGI_RASTER_CLEAR {
        crate::qlog!("sgi-gl: raster op of kind {} is not executed", op.kind);
        return;
    }
    if op.clip.n == 0 && op.clip.empty != 0 {
        return;
    }
    if stencil && !sgi_soft_stencil_max(rules, t, &mut smax) {
        stencil = false;
    }
    if colour && op.index_bits != 0 && !sgi_soft_check_index(t, op.index_bits) {
        colour = false;
    }
    if op.index_bits == 0 && !sgi_soft_rgba_capable(rules, t, rgba_read) {
        return;
    }
    if colour && op.index_bits == 0 {
        let c = [op.clear_rgba[0] as f64, op.clear_rgba[1] as f64, op.clear_rgba[2] as f64];

        mask = sgi_raster_colour_mask(rules, op.clear_colour_mask);
        if !rgba && mask != sgi_soft_colour_bits(rules) && mask != 0 && t.load_colour.is_none() {
            crate::qlog!("sgi-gl: a partial colour mask needs planes that can be read back; cleared without it");
            mask = sgi_soft_colour_bits(rules);
        }
        colour = if rgba { rgba_mask != 0 } else { mask != 0 };
        rgb = sgi_soft_rgb(rules, &c);
        // Clears remain undithered: no dither state is submitted.
        clear = sgi_soft_rgba_value(rules, rgb, op.clear_rgba[3] as f64);
    }
    let rows = SGISoftClear {
        rules,
        t,
        op,
        colour,
        depth,
        stencil,
        rgba,
        rgba_read,
        rgb,
        smax,
        mask,
        rgba_mask,
        ntargets,
        depth_value,
        clear,
        rgba_reported: P::new(&mut rgba_reported),
    };
    let bands = sgi_soft_bands();
    if !t.concurrent_pixels
        || bands < 2
        || op.y1 < op.y0
        || op.x1 < op.x0
        || ((op.x1 - op.x0 + 1) as u64) * ((op.y1 - op.y0 + 1) as u64) < sgi_soft_band_min_pixels(t)
        || !sgi_soft_bands_run(sgi_soft_clear_rows_band, P::from_ref(&rows).opaque(), bands)
    {
        sgi_soft_clear_rows(&rows, 0, 1);
    }
}
