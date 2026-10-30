//! Ported from hw/misc/sgi-odyssey.c in jamesbraid/qemu-sgi-origami.

use super::*;

// C's MIN/MAX: the second operand wins ties and NaNs.
fn cmin<T: PartialOrd>(a: T, b: T) -> T {
    if a < b { a } else { b }
}
fn cmax<T: PartialOrd>(a: T, b: T) -> T {
    if a > b { a } else { b }
}

#[derive(Clone, Copy, Default)]
pub struct OdysseyTexture {
    pub s: P<SGIOdysseyState>,
    pub offset: u32,
    pub stride: u32,
    pub width: u32,
    pub height: u32,
    pub base_format: u32,
    pub words: u32,
    /// A 2D level's border rows are stored, so a one-row level is not 1D.
    pub border_rows: bool,
}

/// First and last detail function weights, and whether the table is flat.
#[derive(Clone, Copy, Default)]
pub struct OdysseyTextureDetailWeight {
    pub constant: bool,
    pub first: f64,
    pub last: f64,
}

#[derive(Clone, Copy, Default)]
pub struct OdysseyTextureVolume {
    pub s: P<SGIOdysseyState>,
    pub base: u32,
    pub stride_units: u32,
    pub level_word2: u32,
    /// Its slices in tex_slice_offset/tex_slice_width (texture setup).
    pub first_slice: u32,
    pub slices: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub base_format: u32,
    pub words: u32,
    pub colour_table_alpha: bool,
    pub border: bool,
    pub colour_table: bool,
    pub scale_bias: bool,
    pub detail_weight: OdysseyTextureDetailWeight,
}

/// `volume` comes first (repr(C)) so one opaque pointer serves both the
/// post-filter callback and the 3D reader, as in the C.
#[derive(Clone, Default)]
#[repr(C)]
pub struct OdysseyTextureChain {
    /// Also supplies the state pointer for the post-filter callback.
    pub volume: OdysseyTextureVolume,
    pub volumes: [OdysseyTextureVolume; SGI_GL_TEX_LEVELS_MAX as usize],
    pub storage: [OdysseyTexture; SGI_GL_TEX_LEVELS_MAX as usize],
    pub levels: [SGIGLTexLevel; SGI_GL_TEX_LEVELS_MAX as usize],
    pub pyramid: SGIGLTexPyramid,
    pub detail_storage: OdysseyTexture,
    pub detail_image: SGIGLTexLevel,
}

#[derive(Clone, Copy, Default)]
pub struct OdysseyTextureDetailSampler {
    pub descriptor: u32,
    pub width: u32,
    pub height: u32,
    pub level: i32,
    pub mode: u32,
}

/// Whether a GL fragment's colour is an index (ODSY_PROG_COLOUR_MODE).
pub fn odyssey_gl_index_mode(s: &mut SGIOdysseyState) -> bool {
    s.colour_mode[0] & ODSY_COLOUR_MODE_INDEX != 0
}

/// A colour index carried through the shared rasterizer, which knows only
/// RGBA: bits 7:0 of the index ride in blue and bits 11:8 in green, so at
/// the rules' eight colour bits the stored colour's low twelve bits are the
/// index again (odyssey_gl_store_colour()). Smooth shading then
/// interpolates the index as GL does, within one run of 256.
pub fn odyssey_gl_index_rgba(index: u32, rgba: &mut [f32; 4]) {
    rgba[0] = 0.0;
    rgba[1] = ((index >> 8) & 0xf) as f32 / 255.0f32;
    rgba[2] = (index & 0xff) as f32 / 255.0f32;
    rgba[3] = 1.0;
}

/// A float index (glIndexf, glClearIndex): its integer part, wrapped to the
/// twelve index bits the way a write keeps only the buffer's bits.
pub fn odyssey_float_index(bits: u32) -> u32 {
    let mut f = f32::from_bits(bits);

    if !f.is_finite() {
        return 0;
    }
    // The integer part, wrapped to the buffer's twelve bits without an
    // out-of-range conversion: fmodf is exact.
    f = f.floor() % 4096.0f32;
    if f < 0.0 {
        f += 4096.0f32;
    }
    (f as u32) & ODSY_INDEX_MASK
}

/// Whether GL draws go to the id-plane destination: it is selected and the
/// kernel has bound it as the draw drawable (0x608, bit 31 aside), as it
/// does for flight's overlay context (0x80142000; OBSERVED). The X server
/// selects the same destination for its own id writes, which a GL
/// context's draws do not follow.
///
/// APPROXIMATION: this rule reconciles those two observations; it is not a
/// known board mechanism. An equally consistent alternative is that GL
/// draws simply follow the drawable 0x608 binds, whatever 0x1017 last
/// selected.
pub fn odyssey_gl_to_aux(s: &mut SGIOdysseyState) -> bool {
    odyssey_dest_is_id_planes(s) && (s.draw_word & !ODSY_DRAW_WORD_STATE) == s.dest_base
}

/// A GL draw aimed at the id-plane destination that has no modelled home.
pub fn odyssey_gl_aux_refused(s: &mut SGIOdysseyState, what: &str) {
    if !s.gl_aux_reported {
        s.gl_aux_reported = true;
        crate::qlog!("sgi-odyssey: {} to the id-plane destination is not modeled; dropped", what);
    }
}

/// A colour-index GL draw to the id-plane destination: an overlay
/// context's drawing (ODSY_OVERLAY_MODE_BASE). Buffer 1 is the overlay
/// pixel byte and takes the index's low eight bits; the id byte is the X
/// server's alone, so a GL write to buffer 0 is reported and dropped.
pub fn odyssey_gl_store_aux(s: &mut SGIOdysseyState, target: SGIColourTarget, x: i32, y: i32, colour: u32) {
    if target == ODSY_AUX_OVERLAY_BUFFER {
        let at = (y as i64 as u64).wrapping_mul(SGI_ODYSSEY_WIDTH as u64).wrapping_add(x as i64 as u64);
        s.overlay[at as usize] = (colour & 0xff) as u8;
    }
}

/// What the colour hooks below decline, reported once, from the draw rather
/// than from a pixel: the conditions are fixed for the whole draw, and the
/// hooks then only store. colour says whether the draw writes colour at
/// all.
pub fn odyssey_gl_report_refusals(s: &mut SGIOdysseyState, colour: bool) {
    let index_mask = s.gl_write_mask & ODSY_WRITE_INDEX_MASK;
    let mask = (s.gl_write_mask >> ODSY_WRITE_COLOUR_SHIFT) & ODSY_WRITE_COLOUR_MASK;
    let buffers = odyssey_draw_buffers(s);

    if !colour {
        return;
    }
    if !odyssey_gl_index_mode(s) {
        if buffers != 0 && odyssey_gl_to_aux(s) {
            odyssey_gl_aux_refused(s, "an RGBA draw");
        }
        return;
    }
    if index_mask != ODSY_WRITE_INDEX_MASK && !s.gl_index_mask_logged {
        s.gl_index_mask_logged = true;
        crate::qlog!("sgi-odyssey: index write mask {:#05x} is decoded but not applied", index_mask);
    }
    if mask != 0 && (buffers & !(1u32 << ODSY_AUX_OVERLAY_BUFFER)) != 0 && odyssey_gl_to_aux(s) {
        odyssey_gl_aux_refused(s, "a GL id-byte write");
    }
}

pub fn odyssey_gl_store_colour(opaque: Opaque, target: SGIColourTarget, x: i32, y: i32, colour: u32) {
    let s = from_opaque::<SGIOdysseyState>(opaque);
    let mask = (s.gl_write_mask >> ODSY_WRITE_COLOUR_SHIFT) & ODSY_WRITE_COLOUR_MASK;

    if mask == 0 || target >= ODSY_BUFFERS || !odyssey_pixel_allowed(s, x as i64, y as i64) {
        return;
    }
    if odyssey_gl_to_aux(s) {
        odyssey_gl_store_aux(s, target, x, y, colour);
        return;
    }
    odyssey_buffer_store(s, target, x as i64, y as i64, colour & ODSY_INDEX_MASK);
}

/// The shared renderer supplies all four already quantized channel bytes.
pub fn odyssey_gl_store_rgba(opaque: Opaque, target: SGIColourTarget, x: i32, y: i32, colour: &SGIRasterColour) {
    let s = from_opaque::<SGIOdysseyState>(opaque);
    let c = &colour.component;
    let allowed = target < ODSY_BUFFERS && odyssey_pixel_allowed(s, x as i64, y as i64);

    if allowed && !odyssey_gl_to_aux(s) {
        odyssey_buffer_store(
            s,
            target,
            x as i64,
            y as i64,
            (c[0] << 24) | (c[1] << 16) | (c[2] << 8) | c[3],
        );
    }
}

pub fn odyssey_gl_load_rgba(opaque: Opaque, target: SGIColourTarget, x: i32, y: i32, colour: &mut SGIRasterColour) -> bool {
    let s = from_opaque::<SGIOdysseyState>(opaque);

    if target >= ODSY_BUFFERS {
        return false;
    }
    let mem = odyssey_buffer_mem(s, target);
    let p = odyssey_buffer_plane(s, target);
    let limit = cmin(mem.size, mem.bytes_len as u64);
    // Invalid storage reads as zero through sgi_plane_load(), not success.
    if p.is_null() || mem.bytes.is_empty() || !sgi_plane_contains(&p, x, y) {
        return false;
    }
    let offset = (y as i64 as u64).wrapping_mul(p.stride as u64).wrapping_add((x as i64 as u64).wrapping_mul(4));
    if offset > p.len || 4 > p.len - offset {
        return false;
    }
    let at = match p.base.checked_add(offset) {
        Some(at) => at,
        None => return false,
    };
    if at > limit || 4 > limit - at {
        return false;
    }
    let word = sgi_plane_load(&mem, &p, x, y);
    for i in 0..4u32 {
        colour.component[i as usize] = (word >> (24 - 8 * i)) & 0xff;
    }
    true
}

/// How far past [0, 1] a fragment's depth may round and still be drawn,
/// clamped: 16 units in the last place of a single near 1 (ODSY_GL_DEPTH_SLACK
/// in defs.rs). APPROXIMATION: the depth row and block 16 arrive as
/// singles, and a library that maps the far plane to 0.99999999 of the range
/// leaves less margin than their rounding takes. A board does its
/// arithmetic in its own precision and the clip it applies at the far plane
/// is UNKNOWN.
///
/// The depth row over the block-16 row, in units of the client's [0, 1]
/// depth (odyssey_gl_load_combined()). The shared context's model-view is
/// the identity, so its eye coordinates are the object coordinates the
/// combined matrix takes. Block 16 is the divisor only when all four of its
/// words are finite and the fourth is not zero; otherwise the sign-only
/// fallback divides by a constant.
pub fn odyssey_gl_operation_rules(s: &mut SGIOdysseyState) -> SGIRasterRules {
    let mut rules = odyssey_gl_rules().clone();
    let scale = 2.0 * ODSY_GL_DEPTH_S * odyssey_gl_depth_scale(s);
    let mut block16 = [0.0f64; 4];
    let mut divisor = true;

    rules.alpha_bits = if odyssey_gl_index_mode(s) { 0 } else { 8 };
    rules.depth_row_given = true;
    for k in 0..4usize {
        rules.depth_row[k] = odyssey_f32(s.gl_combined[8 + k]) / scale;
        block16[k] = odyssey_f32(s.gl_block16[k]);
        divisor &= block16[k].is_finite();
    }
    if divisor && block16[3] != 0.0 {
        rules.depth_divisor_given = true;
        for k in 0..4usize {
            rules.depth_divisor[k] = block16[k] / block16[3];
        }
    }
    rules.depth_slack = ODSY_GL_DEPTH_SLACK;
    rules
}

/// The depth plane (odyssey_realize()): 24-bit depth, 0 nearest, through
/// the same pixel test as a colour write. It is a model plane because the
/// board memory the real depth buffer occupies is not known.
pub fn odyssey_gl_load_depth(opaque: Opaque, x: i32, y: i32) -> u32 {
    let s = from_opaque::<SGIOdysseyState>(opaque);
    let p = sgi_board_memory_plane(&s.depth_mem, s.depth_plane);

    if p.is_null() {
        return 0;
    }
    sgi_plane_load(&s.depth_mem, &p, x, y)
}

pub fn odyssey_gl_store_depth(opaque: Opaque, x: i32, y: i32, depth: u32) {
    let s = from_opaque::<SGIOdysseyState>(opaque);

    // The overlay has no depth; an overlay clear leaves the main depth.
    if odyssey_gl_to_aux(s) {
        return;
    }
    if odyssey_pixel_allowed(s, x as i64, y as i64) {
        let p = sgi_board_memory_plane(&s.depth_mem, s.depth_plane);

        if !p.is_null() {
            sgi_plane_store(&mut s.depth_mem, &p, x, y, depth & ODSY_DEPTH24_MAX);
        }
    }
}

/// The stencil plane (odyssey_realize()): 8 bits, through the same pixel
/// test as a depth write. A model plane: where the board keeps its stencil
/// planes is not known.
pub fn odyssey_gl_load_stencil(opaque: Opaque, x: i32, y: i32) -> u32 {
    let s = from_opaque::<SGIOdysseyState>(opaque);
    let p = sgi_board_memory_plane(&s.stencil_mem, s.stencil_plane);

    if p.is_null() {
        return 0;
    }
    sgi_plane_load(&s.stencil_mem, &p, x, y)
}

pub fn odyssey_gl_store_stencil(opaque: Opaque, x: i32, y: i32, stencil: u32) {
    let s = from_opaque::<SGIOdysseyState>(opaque);

    if odyssey_pixel_allowed(s, x as i64, y as i64) {
        let p = sgi_board_memory_plane(&s.stencil_mem, s.stencil_plane);

        if !p.is_null() {
            sgi_plane_store(&mut s.stencil_mem, &p, x, y, stencil & ODSY_STENCIL_MAX);
        }
    }
}

/// Experimental policy; the board's scale sign and clamps are unmeasured.
pub fn odyssey_gl_fog_factor(opaque: Opaque, window_z: f32, eye_z: f32) -> f32 {
    let s = from_opaque::<SGIOdysseyState>(opaque);
    let w = &s.gl_fog;
    let scale = if (w[2] & 0xffffff) != 0 {
        odyssey_f32(((w[2] & 0xffffff) << 5).wrapping_add(96u32 << 23)).abs()
    } else {
        0.0
    };
    let mut t = (scale * window_z as f64 + sextract32(w[3], 0, 12) as f64 / 256.0)
        * 2.0f64.powi(((w[3] >> 12) & 7) as i32);
    let p = [
        sextract32(w[4] >> 12, 0, 12) as f64 / 256.0,
        sextract32(w[4], 0, 12) as f64 / 256.0,
        sextract32(w[5] >> 12, 0, 12) as f64 / 256.0,
        sextract32(w[5], 0, 12) as f64 / 256.0,
    ];

    let _ = eye_z;
    t = if t >= 1.0 { 1.0 } else if t > 0.0 { t } else { 0.0 };
    let u = 1.0 - t;
    let f = p[0] * u * u * u + 3.0 * p[1] * u * u * t + 3.0 * p[2] * u * t * t + p[3] * t * t * t;
    (if f >= 1.0 { 1.0 } else if f > 0.0 { f } else { 0.0 }) as f32
}

/// An opaque polygon stipple (0x043f bit 4 with shape 0, the GL stipple):
/// clear bits take the 0x0c21 constant colour, as the 2D path's opaque
/// patterns do (odyssey_pattern_background()). That the board paints the
/// clear bits here as it does for the 2D quads is INFERRED: it is the only
/// reading under which the X server's FillOpaqueStippled sequence draws an
/// opaque stipple.
pub fn odyssey_gl_stipple_background(s: &mut SGIOdysseyState, t: &mut SGIRasterTarget) {
    if (s.pattern_shape & ODSY_PATTERN_SHAPE_OPAQUE) == 0 {
        return;
    }
    t.stipple_opaque = true;
    if odyssey_gl_index_mode(s) {
        t.stipple_background_index = odyssey_pattern_background(s, true);
        let idx = t.stipple_background_index;
        odyssey_gl_index_rgba(idx, &mut t.stipple_background);
    } else {
        let pixel = odyssey_pattern_background(s, false);

        for k in 0..3u32 {
            t.stipple_background[k as usize] = ((pixel >> (24 - 8 * k)) & 0xff) as f32 / 255.0f32;
        }
        t.stipple_background[3] = 1.0;
    }
}

/// The callbacks may run on band workers in the C; here they run serially.
/// Each stores only its own pixel's bytes. Their once-only reports are
/// made before the draw (odyssey_gl_report_refusals()).
pub fn odyssey_gl_target(s: &mut SGIOdysseyState) -> SGIRasterTarget {
    let index = odyssey_gl_index_mode(s);
    let mut t = SGIRasterTarget {
        store_colour: if index { Some(odyssey_gl_store_colour) } else { None },
        store_rgba: if index { None } else { Some(odyssey_gl_store_rgba) },
        load_rgba: if index { None } else { Some(odyssey_gl_load_rgba) },
        load_depth: Some(odyssey_gl_load_depth),
        store_depth: Some(odyssey_gl_store_depth),
        load_stencil: Some(odyssey_gl_load_stencil),
        store_stencil: Some(odyssey_gl_store_stencil),
        fog_factor: Some(odyssey_gl_fog_factor),
        opaque: P::new(s).opaque(),
        concurrent_pixels: s.raster_bands,
        band_min_pixels: s.raster_band_pixels,
        mixed_frame: false,
        ..Default::default()
    };

    odyssey_gl_stipple_background(s, &mut t);
    t
}

pub fn odyssey_texture_read(opaque: Opaque, x: u32, y: u32, rgba: &mut u32) -> bool {
    let texture = from_opaque::<OdysseyTexture>(opaque);
    let s = texture.s.get();

    if x >= texture.width || y >= texture.height {
        return false;
    }
    // y * stride cannot overflow 64 bits.
    let word = y as u64 * texture.stride as u64 + texture.offset as u64 + x as u64;
    let end = word + 1;
    if end > texture.words as u64 || end > (s.obj_mem.bytes_len / 4) as u64 {
        return false;
    }
    let at = (word * 4) as usize;
    if at + 4 > s.obj_mem.bytes.len() {
        return false;
    }
    *rgba = ldl_be_p(&s.obj_mem.bytes[at..]);
    if texture.base_format == SGI_GL_ALPHA {
        *rgba = (*rgba >> 24) & 0xff;
    }
    true
}

/// A volume level's slices sit 64 << word 2 address units apart. OBSERVED
/// three ways on the model: the captured 8x8x8 levels (word 2 1, 0x80
/// apart), the captured stored 2x2x2 (word 2 4, 0x400 apart) and a
/// 128x128x64 RGBA volume (word 2 6, 64 slice objects 0x1000 apart). That
/// word 2 is the log2 of the slice allocation is INFERRED.

pub fn odyssey_volume_slice_stride(level_word2: u32) -> u64 {
    64u64.wrapping_shl(level_word2)
}

pub fn odyssey_texture_read_3d_at(volume: &OdysseyTextureVolume, x: i32, y: i32, z: i32, border: bool, rgba: &mut u32) -> bool {
    let s = volume.s.get();

    if (border
        && (x < -1 || y < -1 || z < -1 || x > volume.width as i32 || y > volume.height as i32 || z > volume.depth as i32))
        || (!border
            && (x < 0 || y < 0 || z < 0 || x >= volume.width as i32 || y >= volume.height as i32 || z >= volume.depth as i32))
    {
        return false;
    }
    // Setup found and checked the object of every slice a read in these
    // bounds can reach (odyssey_texture_setup()), so the slice's place in
    // the object store is read from its table, not looked up per texel.
    let slice = if border { (z + 1) as u32 } else { z as u32 };
    if slice >= volume.slices {
        return false;
    }
    let at = (volume.first_slice + slice) as usize;
    let stride = s.tex_slice_width[at];
    let width = if border { volume.width + 2 } else { volume.width };
    let height = if border { volume.height + 2 } else { volume.height };
    let px = if border { (x + 1) as u32 } else { x as u32 };
    let py = if border { (y + 1) as u32 } else { y as u32 };
    let mut offset = 0u64;
    if border {
        offset = stride as u64 * 15 + 15;
        offset = match offset.checked_add(s.tex_slice_offset[at] as u64) {
            Some(o) => o,
            None => return false,
        };
    }
    let texture = OdysseyTexture {
        s: volume.s,
        offset: if border { offset as u32 } else { s.tex_slice_offset[at] },
        stride,
        width,
        height,
        base_format: volume.base_format,
        words: volume.words,
        border_rows: false,
    };
    odyssey_texture_read(P::from_ref(&texture).opaque(), px, py, rgba)
}

pub fn odyssey_texture_read_3d(opaque: Opaque, x: u32, y: u32, z: u32, rgba: &mut u32) -> bool {
    let volume = from_opaque::<OdysseyTextureVolume>(opaque);

    odyssey_texture_read_3d_at(volume, x as i32, y as i32, z as i32, volume.border, rgba)
}

pub fn odyssey_texture_read_border_3d(opaque: Opaque, x: i32, y: i32, z: i32, rgba: &mut u32) -> bool {
    odyssey_texture_read_3d_at(from_opaque::<OdysseyTextureVolume>(opaque), x, y, z, true, rgba)
}

pub fn odyssey_texture_read_volume_level(opaque: Opaque, level: u32, x: u32, y: u32, z: u32, rgba: &mut u32) -> bool {
    let chain = from_opaque::<OdysseyTextureChain>(opaque);

    (level as usize) < chain.volumes.len()
        && odyssey_texture_read_3d(P::from_ref(&chain.volumes[level as usize]).opaque(), x, y, z, rgba)
}

pub fn odyssey_texture_read_level(opaque: Opaque, level: u32, x: u32, y: u32, rgba: &mut u32) -> bool {
    let chain = from_opaque::<OdysseyTextureChain>(opaque);

    (level as usize) < chain.storage.len()
        && odyssey_texture_read(P::from_ref(&chain.storage[level as usize]).opaque(), x, y, rgba)
}

pub fn odyssey_texture_read_detail(opaque: Opaque, level: u32, x: u32, y: u32, rgba: &mut u32) -> bool {
    let chain = from_opaque::<OdysseyTextureChain>(opaque);

    level == 0 && odyssey_texture_read(P::from_ref(&chain.detail_storage).opaque(), x, y, rgba)
}

/// The stock driver's two-point function (-4, 1/4), (0, 1/2) fills entry 0
/// with 1/2, falls by 1/512 per entry to 1/4 at entry 128 and holds 1/4
/// through entry 255. That is consistent with entry i serving LOD -i/32, so
/// the table runs out at LOD -255/32. SGIS_detail_texture fixes the weight
/// at and beyond each end point, and those are the only weights used here.
/// How the board indexes and interpolates the table at LODs in between is
/// not known, so a nonconstant table refuses them.

pub fn odyssey_texture_detail_weight(opaque: Opaque, lambda: f64, weight: &mut f64) -> bool {
    let volume = from_opaque::<OdysseyTextureVolume>(opaque);
    let detail = &volume.detail_weight;

    if detail.constant || lambda <= ODSY_TEX_DETAIL_FUNCTION_LAST_LOD {
        *weight = detail.last;
        return true;
    }
    if lambda >= 0.0 {
        *weight = detail.first;
        return true;
    }
    false
}

pub fn odyssey_texture_read_border_level(opaque: Opaque, level: u32, x: i32, y: i32, rgba: &mut u32) -> bool {
    let chain = from_opaque::<OdysseyTextureChain>(opaque);

    if level as usize >= chain.storage.len() {
        return false;
    }
    let mut texture = chain.storage[level as usize];
    if texture.height == 1 && !texture.border_rows {
        if (texture.width != 2 && texture.width != 4 && texture.width != 8 && texture.width != 64)
            || y != 0
            || texture.offset == 0
            || (x != -1 && x != texture.width as i32)
        {
            return false;
        }
        texture.offset -= 1;
        texture.width += 2;
        return odyssey_texture_read(P::from_ref(&texture).opaque(), (x + 1) as u32, 0, rgba);
    }
    if x < -1
        || x > texture.width as i32
        || y < -1
        || y > texture.height as i32
        || (x >= 0 && x < texture.width as i32 && y >= 0 && y < texture.height as i32)
        || (texture.offset as u64) < texture.stride as u64 + 1
    {
        return false;
    }
    // Setup validates the complete captured 4-by-4 border region; for a
    // larger bordered image it checks only that the rows exist.
    texture.offset -= texture.stride + 1;
    texture.width += 2;
    texture.height += 2;
    odyssey_texture_read(P::from_ref(&texture).opaque(), (x + 1) as u32, (y + 1) as u32, rgba)
}

/// Each normalized component indexes the table after filtering.
pub fn odyssey_texture_colour_table(texture: &OdysseyTextureVolume, rgba: &mut [f64; 4]) {
    let s = texture.s.get();

    for channel in 0..3usize {
        let index = (cmax(0.0, cmin(rgba[channel], 1.0)) * 255.0 + 0.5).floor() as u32;
        let plane = (channel == 2) as usize;
        let word = s.tex_colour_table[plane][index as usize];
        let component = if channel == 1 { word & 0xfff } else { (word >> 12) & 0xfff };

        rgba[channel] = component as f64 / 4095.0;
    }
    if texture.colour_table_alpha {
        let index = (cmax(0.0, cmin(rgba[3], 1.0)) * 255.0 + 0.5).floor() as u32;
        let word = s.tex_colour_table[1][index as usize];

        rgba[3] = (word & 0xfff) as f64 / 4095.0;
    }
}

/// Post-filter scale and bias (0x203b, where texture validation sends a
/// texture's words): eight words, a scale and a bias for each of R, G, B
/// and A in turn, each signed 3.12 in bits 15:0. The GL library clamps both
/// to -8 through 7.999755859375 and defaults them to 4096 (1.0) and 0
/// (ESTABLISHED as the library's conversion). Pairing scale before bias
/// within a channel is INFERRED from the order the datasheet names them.
/// The library never sets bits 31:16, so a word that does is unknown state
/// and drops the draw. The reset value is the library's default, a model
/// choice.

pub fn odyssey_texture_scale_bias_known(s: &SGIOdysseyState) -> bool {
    for i in 0..ODSY_TEX_SCALE_BIAS_WORDS as usize {
        if (s.tex_scale_bias[i] >> ODSY_TEX_SCALE_BIAS_BITS) != 0 {
            return false;
        }
    }
    true
}

pub fn odyssey_texture_scale_bias_identity(s: &SGIOdysseyState) -> bool {
    for c in 0..4usize {
        if s.tex_scale_bias[c * 2] != ODSY_TEX_SCALE_BIAS_ONE || s.tex_scale_bias[c * 2 + 1] != 0 {
            return false;
        }
    }
    true
}

/// GL_SGIX_texture_scale_bias applies scale, then bias, then a clamp to
/// [0, 1] to the filtered texel, before the texture colour table and the
/// environment. That the board follows the extension's order and clamp is
/// INFERRED: no source gives the board's arithmetic.
pub fn odyssey_texture_scale_bias(s: &SGIOdysseyState, rgba: &mut [f64; 4]) {
    for c in 0..4usize {
        let scale = sextract32(s.tex_scale_bias[c * 2], 0, ODSY_TEX_SCALE_BIAS_BITS) as f64;
        let bias = sextract32(s.tex_scale_bias[c * 2 + 1], 0, ODSY_TEX_SCALE_BIAS_BITS) as f64;

        rgba[c] = (rgba[c] * scale + bias) / ODSY_TEX_SCALE_BIAS_ONE as f64;
        rgba[c] = cmin(cmax(rgba[c], 0.0), 1.0);
    }
}

pub fn odyssey_texture_post_filter(opaque: Opaque, rgba: &mut [f64; 4]) {
    let texture = from_opaque::<OdysseyTextureVolume>(opaque);

    if texture.scale_bias {
        odyssey_texture_scale_bias(texture.s.get(), rgba);
    }
    if texture.colour_table {
        odyssey_texture_colour_table(texture, rgba);
    }
}

pub fn odyssey_texture_detail_sampler(sampler: u32, detail: &mut OdysseyTextureDetailSampler) -> bool {
    *detail = OdysseyTextureDetailSampler { descriptor: 62, width: 2, height: 2, level: -2, mode: SGI_GL_ADD };
    match sampler {
        0x02108000 => {
            detail.descriptor = 63;
            detail.level = -1;
            true
        }
        0x04108000 => true,
        0x06108000 => {
            detail.descriptor = 61;
            detail.level = -3;
            true
        }
        0x04208000 => {
            detail.width = 4;
            true
        }
        0x04110000 => {
            detail.height = 4;
            true
        }
        0x84108000 => {
            detail.mode = SGI_GL_MODULATE;
            true
        }
        _ => false,
    }
}

pub fn odyssey_texture_detail_entry_weight(value: u32, weight: &mut f64) -> bool {
    match value {
        0x00000000 | 0x00400000 | 0x00555000 | 0x00800000 | 0x00fff000 => {
            *weight = value as f64 / 16777216.0;
            true
        }
        _ => false,
    }
}

pub fn odyssey_texture_detail_function_weight(s: &SGIOdysseyState, weight: &mut OdysseyTextureDetailWeight) -> bool {
    let table = &s.tex_detail_function;
    let last = (SGI_ODYSSEY_TEX_DETAIL_FUNCTION_ENTRIES - 1) as usize;

    if s.tex_detail_function_valid != 0xffff {
        return false;
    }
    weight.constant = true;
    for i in 1..=last {
        if table[i] != table[0] {
            weight.constant = false;
            break;
        }
    }
    odyssey_texture_detail_entry_weight(table[0], &mut weight.first)
        && odyssey_texture_detail_entry_weight(table[last], &mut weight.last)
}

/// Sampler word 4 bits 7:5 are the minification code (0 nearest, 1 linear,
/// 2 to 5 the four mipmap modes in GL order) and bits 4:1 the
/// magnification code (2 linear detail, 3 detail alpha, 4 detail colour).
/// With a detail magnification filter the GL library sends GL_NEAREST as
/// code 2 and GL_LINEAR as code 5 (ESTABLISHED as the library's setter), so
/// a detail word carries code 2 or 5. The model samples each code as the
/// mipmap mode it names: INFERRED; what the board does with code 2 or 5
/// under a detail filter is UNKNOWN.
/// Word 4 bit 17: set for GL_LINEAR_MIPMAP_LINEAR and for GL_PHASE_SGIX,
/// clear otherwise (ESTABLISHED as the library's setter). Its effect on the
/// board is UNKNOWN. With minification code 5 the model samples as that
/// code says and ignores the bit; with any other code it can only mean
/// GL_PHASE_SGIX, which the model does not implement, so the draw drops.

pub fn odyssey_texture_detail_filter(word: u32) -> u32 {
    let min = word & ODSY_TEX_MIN_MASK;

    if min != ODSY_TEX_MIN_NEAREST_MIPMAP_NEAREST && min != ODSY_TEX_MIN_LINEAR_MIPMAP_LINEAR {
        return 0;
    }
    match word & !ODSY_TEX_MIN_MASK {
        0x13000004 => SGI_GL_LINEAR_DETAIL_SGIS,
        0x13000006 => SGI_GL_LINEAR_DETAIL_ALPHA_SGIS,
        0x13000008 => SGI_GL_LINEAR_DETAIL_COLOR_SGIS,
        _ => 0,
    }
}

pub fn odyssey_texture_detail_backing_setup(s: &mut SGIOdysseyState, chain: &mut OdysseyTextureChain, descriptor: u32, width: u32, height: u32, base_format: u32) -> bool {
    let e = (descriptor * 3) as usize;
    let entry = [s.tex_levels[e], s.tex_levels[e + 1], s.tex_levels[e + 2]];
    let base = entry[0] & 0x1fffffff;

    if base == 0 || entry[1] != 0x41100002 || entry[2] != 0 || s.obj_count > SGI_ODYSSEY_OBJECTS {
        return false;
    }
    let object = odyssey_object_find(s, base);
    if object < 0 {
        return false;
    }
    let o = object as usize;
    let end = (height.wrapping_sub(1) as u64)
        .wrapping_mul(s.obj_width[o] as u64)
        .wrapping_add(s.obj_offset[o] as u64)
        .wrapping_add(width as u64);
    let region_written = odyssey_object_region_written(s, object, 0, 0, width, height);
    if s.obj_width[o] != 32
        || s.obj_rows[o] < height
        || end > s.obj_used as u64
        || end > (s.obj_mem.bytes_len / 4) as u64
        || !region_written
    {
        return false;
    }
    chain.detail_storage = OdysseyTexture {
        s: P::new(s),
        offset: s.obj_offset[o],
        stride: s.obj_width[o],
        width,
        height,
        base_format,
        words: s.obj_used,
        border_rows: false,
    };
    chain.detail_image = SGIGLTexLevel {
        level: 0,
        width,
        height,
        depth: 1,
        base_format,
        internal_format: base_format,
        read_texel: Some(odyssey_texture_read_detail),
        opaque: P::new(chain).opaque(),
        ..Default::default()
    };
    true
}

/// The four 0x1013 words are 14-bit two's complement with 8 fraction bits:
/// the GL library writes trunc(x * 256) & 0x3fff (ESTABLISHED as the
/// library's conversion). It never sets bits 31:14, so a word that does is
/// unknown state and drops the draw.

pub fn odyssey_texture_lod_word(word: u32) -> i32 {
    sextract32(word, 0, ODSY_TEX_LOD_BITS)
}

/// Minimum and maximum LOD (words 2 and 3). The library clamps both to
/// +/-15 before writing them (ESTABLISHED); the field holds +/-32. What the
/// board does with a limit past 15 is UNKNOWN, as no stock client sends
/// one: the model clamps it the way the library does, an approximation.
pub fn odyssey_texture_lod_limit(word: u32) -> i32 {
    cmin(cmax(odyssey_texture_lod_word(word), -(ODSY_TEX_LOD_LIMIT as i32)), ODSY_TEX_LOD_LIMIT as i32)
}

/// LOD bias S and T (words 0 and 1). The library clamps each to -32 through
/// 31.99609375, the field's whole range, so every value is one it can send
/// (ESTABLISHED). The R bias never reaches a register. The model applies a
/// bias as GL_SGIX_texture_lod_bias defines it, scaling that axis's
/// derivatives by 2^bias before the LOD is computed: INFERRED, since the
/// board's LOD arithmetic is not documented. A zero bias leaves the scale
/// unset, so an unbiased draw keeps the exact unscaled path.
pub fn odyssey_texture_lod_bias(s: &SGIOdysseyState, image: &mut SGIGLTexImage) {
    let bias_s = odyssey_texture_lod_word(s.tex_lod[0]);
    let bias_t = odyssey_texture_lod_word(s.tex_lod[1]);

    image.lod_scale_s = if bias_s != 0 { (bias_s as f64 / ODSY_TEX_LOD_ONE as f64).exp2() } else { 0.0 };
    image.lod_scale_t = if bias_t != 0 { (bias_t as f64 / ODSY_TEX_LOD_ONE as f64).exp2() } else { 0.0 };
}

/// Sampler words 1 and 2, bits 16:0, hold each axis's maximum clamp in
/// whole texels of the base level: the library writes trunc(2^log2size *
/// GL_TEXTURE_MAX_CLAMP_*_SGIX), so the default 1.0 gives the size itself
/// (ESTABLISHED as the library's setter). A smaller value under GL_CLAMP
/// (wrap 2) clamps that axis's coordinate to value / size, as
/// GL_SGIX_texture_coordinate_clamp defines it, and the size comes from
/// word 0's log2 field. That the board applies it only under GL_CLAMP, and
/// at every level as the same fraction of the image, is INFERRED from the
/// extension. Under other wraps a smaller value is ignored. A value above
/// the size cannot come from the library and is left to fail the size
/// check. Limitation: the R axis's clamp (word 3) is not applied; a volume
/// whose word 3 is below its depth stays unsupported.
pub fn odyssey_texture_max_clamp(image: &mut SGIGLTexImage, sampler_size: u32, ws: u32, wt: u32, width: &mut u32, height: &mut u32) {
    let span_s = 1u32 << ((sampler_size >> 10) & 31);
    let span_t = 1u32 << ((sampler_size >> 5) & 31);

    image.max_clamp_s = 1.0;
    image.max_clamp_t = 1.0;
    image.max_clamp_r = 1.0;
    if *width < span_s {
        if ws == 2 {
            image.max_clamp = true;
            image.max_clamp_s = *width as f64 / span_s as f64;
        }
        *width = span_s;
    }
    if *height < span_t {
        if wt == 2 {
            image.max_clamp = true;
            image.max_clamp_t = *height as f64 / span_t as f64;
        }
        *height = span_t;
    }
}

fn gl_umul_ovf(a: u64, b: u64, r: &mut u64) -> bool {
    let (v, o) = a.overflowing_mul(b);
    *r = v;
    o
}

fn gl_uadd_ovf(a: u64, b: u64, r: &mut u64) -> bool {
    let (v, o) = a.overflowing_add(b);
    *r = v;
    o
}

pub fn odyssey_texture_setup(s: &mut SGIOdysseyState, image: &mut SGIGLTexImage, chain: &mut OdysseyTextureChain, resolved: &mut P<SGIGLTexImage>) -> bool {
    let width_word = s.tex_sampler[1];
    let height_word = s.tex_sampler[2];
    let mut width = width_word;
    let mut height = height_word;
    let depth = s.tex_sampler[3];
    let phase = (s.tex_sampler[4] & ODSY_TEX_SAMPLER_BIT17) != 0
        && (s.tex_sampler[4] & ODSY_TEX_MIN_MASK) != ODSY_TEX_MIN_LINEAR_MIPMAP_LINEAR;
    let word = s.tex_sampler[4] & !ODSY_TEX_SAMPLER_BIT17;
    let components = word & 0x0f800000;
    let mut sampler_size = s.tex_sampler[0];
    let mut detail_sampler = OdysseyTextureDetailSampler::default();
    let mut detail_weight = OdysseyTextureDetailWeight::default();
    let detail_identity = odyssey_texture_detail_sampler(sampler_size, &mut detail_sampler);
    let detail_filter = if detail_identity { odyssey_texture_detail_filter(word) } else { 0 };
    let tex_shape = s.tex_mode & (ODSY_TEX_MODE_1D | ODSY_TEX_MODE_2D | ODSY_TEX_MODE_3D);
    let three_d = (s.tex_mode & ODSY_TEX_MODE_3D) != 0;
    let three_d_shape = tex_shape == ODSY_TEX_MODE_3D
        && width >= 2
        && height >= 2
        && depth >= 2
        && is_power_of_2(width as u64)
        && is_power_of_2(height as u64)
        && is_power_of_2(depth as u64)
        && s.tex_level0 == 0;
    let ws = (word >> 12) & 3;
    let wt = (word >> 10) & 3;
    let clamp = ws == 2 || wt == 2;
    let border = ws == 3 || wt == 3;
    let one_d_shape = tex_shape == ODSY_TEX_MODE_1D && height == 1 && depth == 1 && s.tex_level0 == 0;
    let two_d_shape =
        tex_shape == ODSY_TEX_MODE_2D && (width == 2 || width == 4) && height == 2 && depth == 1 && s.tex_level0 == 0;
    let stored_border = ((one_d_shape
        && (width == 2 || width == 4 || width == 8 || width == 64)
        && (word == 0x03002000 || word == 0x03002022))
        || (two_d_shape && (word == 0x03002800 || word == 0x03002822))
        || (three_d_shape && (word == 0x03002a00 || word == 0x03002a22)))
        && (s.tex_sampler[5] & 0xff000000) == 0
        && (s.tex_sampler[6] & 0xff000000) == 0x01000000;
    let three_d_stored = stored_border && three_d_shape;
    let three_d_clamp = three_d_shape && (three_d_stored || word == 0x13002a00 || word == 0x13002a22);
    let one_d_clamp = one_d_shape && (stored_border || word == 0x13002000 || word == 0x13002022);
    let two_d_clamp = two_d_shape && (stored_border || word == 0x13002800 || word == 0x13002822);
    let captured_clamp = one_d_clamp || two_d_clamp || three_d_clamp;
    let two_d_stored = stored_border && two_d_shape;
    let filter = word & 0xe2;
    let min = filter & 0xe0;
    // A two-dimensional image of any size with its one-texel border stored
    // around it, under any filter: word 4's bit 28 clear and word 6's top
    // byte 1, the marks the captured stored-border uploads carry. Seen from
    // a 512x512 GL_CLAMP linear-mipmap image whose ten levels each sit 16
    // texels in from their allocation's corner, with the border at row and
    // column 15. The captured small shapes keep their own checks.
    let general_border = !stored_border
        && !detail_identity
        && !three_d
        && tex_shape == ODSY_TEX_MODE_2D
        && ws != 3
        && wt != 3
        && word == (components | (ws << 12) | (wt << 10) | filter)
        && (s.tex_sampler[5] & 0xff000000) == 0
        && (s.tex_sampler[6] & 0xff000000) == 0x01000000;
    let bordered = stored_border || general_border;
    // GL_CLAMP (wrap field 2) on both axes outside the captured shapes,
    // under any filter. The board's sampling is assumed to be the OpenGL
    // 1.2.1 rule (section 3.8.4), which the shared sampler implements:
    // linear taps past the edge take the border colour, words 5 and 6.
    // GL_CLAMP on one axis only stays refused; no client has been seen to
    // send it.
    let general_clamp = ws == 2 && wt == 2 && !captured_clamp;
    let colour_table_l8 = word == 0x11500001 || word == 0x11500003 || word == 0x11600001;
    let colour_table_rgba8_rgb = word == 0x13500001;
    let colour_table_rgba8_rgba = word == 0x13600001;
    let colour_table = colour_table_l8 || colour_table_rgba8_rgb || colour_table_rgba8_rgba;
    let colour_table_disabled = word == 0x11500000;
    let first = s.tex_level0 & 0x3f;
    let count: u32;
    let terminal: u32;
    let lo = odyssey_texture_lod_limit(s.tex_lod[2]);
    let hi = odyssey_texture_lod_limit(s.tex_lod[3]);
    const MIN_FILTERS: [u32; 6] = [
        SGI_GL_NEAREST,
        SGI_GL_LINEAR,
        SGI_GL_NEAREST_MIPMAP_NEAREST,
        SGI_GL_LINEAR_MIPMAP_NEAREST,
        SGI_GL_NEAREST_MIPMAP_LINEAR,
        SGI_GL_LINEAR_MIPMAP_LINEAR,
    ];
    let mut end: u64 = 0;

    *resolved = P::null();
    'unsupported: {
        if detail_identity {
            if detail_filter == 0
                || !odyssey_texture_detail_function_weight(s, &mut detail_weight)
                || s.tex_level0 != 0
                || width != 1
                || height != 1
                || depth != 1
                || tex_shape != ODSY_TEX_MODE_2D
            {
                break 'unsupported;
            }
            sampler_size = 0x08000000;
        }
        if border {
            width &= 0xffff;
            height &= 0xffff;
        } else {
            // An axis with wrap field 1 (clamp to edge) carries 0x4 in bits
            // 23:16 of its size word, as a Performer-style client sends.
            // Other axes carry none. INFERRED from that pairing, axis by
            // axis, the way clamp to border's axes carry 0xfc.
            if (width_word >> 16) == (if ws == 1 { 0x4u32 } else { 0 }) {
                width &= 0xffff;
            }
            if (height_word >> 16) == (if wt == 1 { 0x4u32 } else { 0 }) {
                height &= 0xffff;
            }
        }
        if !detail_identity {
            odyssey_texture_max_clamp(image, sampler_size, ws, wt, &mut width, &mut height);
        }
        if odyssey_gl_index_mode(s) {
            break 'unsupported;
        }
        match components {
            0x01000000 => image.base_format = SGI_GL_LUMINANCE,
            0x01800000 => image.base_format = SGI_GL_LUMINANCE_ALPHA,
            0x00800000 => image.base_format = SGI_GL_ALPHA,
            0x02000000 => image.base_format = SGI_GL_INTENSITY,
            0x02800000 => image.base_format = SGI_GL_RGB,
            0x03000000 => image.base_format = SGI_GL_RGBA,
            _ => break 'unsupported,
        }
        if detail_identity && image.base_format != SGI_GL_RGBA {
            break 'unsupported;
        }
        let bf = image.base_format;
        if width == 0
            || height == 0
            || width > i32::MAX as u32
            || height > i32::MAX as u32
            || !is_power_of_2(width as u64)
            || !is_power_of_2(height as u64)
            || (three_d
                && (depth == 0
                    || depth > SGI_ODYSSEY_OBJECTS
                    || !is_power_of_2(depth as u64)
                    || (s.tex_mode & (ODSY_TEX_MODE_1D | ODSY_TEX_MODE_2D)) != 0))
            || (!three_d && depth != 1)
            || sampler_size
                != (0x08000000 | (ctz32(width) << 10) | (ctz32(height) << 5) | (if three_d { ctz32(depth) } else { 0 }))
            || ws > 3
            || wt > 3
            || (!border
                && !captured_clamp
                && !general_clamp
                && !general_border
                && (s.tex_sampler[5] != 0 || s.tex_sampler[6] != 0))
            || (general_clamp && !general_border && ((s.tex_sampler[5] | s.tex_sampler[6]) & 0xff000000) != 0)
            || (captured_clamp && !stored_border && ((s.tex_sampler[5] | s.tex_sampler[6]) & 0xff000000) != 0)
            || (border
                && (ws != 3
                    || wt != 3
                    || width_word != (0x00fc0000 | width)
                    || height_word != (0x00fc0000 | height)
                    || ((s.tex_sampler[5] | s.tex_sampler[6]) & 0xff000000) != 0))
            || (general_clamp && three_d)
            || (clamp && !general_clamp && !captured_clamp)
            || (border
                && !captured_clamp
                && ((clamp && (ws != 2 || wt != 2)) || tex_shape != ODSY_TEX_MODE_2D || bf != SGI_GL_RGBA || filter != 0))
            || (min != 0 && min != 0x20 && min != 0x40 && min != 0x60 && min != 0x80 && min != 0xa0)
            || (!colour_table
                && !colour_table_disabled
                && !bordered
                && !three_d_clamp
                && word != (0x10000000 | components | (ws << 12) | (wt << 10) | filter)
                && detail_filter == 0)
            || ((s.tex_lod[0] | s.tex_lod[1] | s.tex_lod[2] | s.tex_lod[3]) & !ODSY_TEX_LOD_MASK) != 0
            || !odyssey_texture_scale_bias_known(s)
            || phase
            || s.tex_offset[0] != 0
            || s.tex_offset[1] != 0
            || s.tex_offset[2] != 0
            || (min >= 0x40 && lo > hi)
            || (colour_table
                && ((colour_table_l8 && bf != SGI_GL_LUMINANCE)
                    || ((colour_table_rgba8_rgb || colour_table_rgba8_rgba) && bf != SGI_GL_RGBA)
                    || three_d
                    || tex_shape != ODSY_TEX_MODE_2D
                    || s.tex_colour_table_valid[0] != 0xffff
                    || s.tex_colour_table_valid[1] != 0xffff))
            || (colour_table_disabled && (bf != SGI_GL_LUMINANCE || three_d || tex_shape != ODSY_TEX_MODE_2D))
            || s.tex_env[0] < 1
            || s.tex_env[0] > 4
            || (s.tex_env[0] == 3
                && (bf == SGI_GL_ALPHA || bf == SGI_GL_LUMINANCE || bf == SGI_GL_LUMINANCE_ALPHA || bf == SGI_GL_INTENSITY))
            || (s.tex_env[0] == 4
                && (((s.tex_env[1] | s.tex_env[2]) & 0xff000000) != 0 || s.tex_env[3] != 0 || s.tex_env[4] != 0))
        {
            break 'unsupported;
        }
        image.border = bordered as u32;
        image.width = width;
        image.height = height;
        image.wrap_s = if border {
            SGI_GL_CLAMP_TO_BORDER
        } else if ws == 2 {
            SGI_GL_CLAMP
        } else if ws != 0 {
            SGI_GL_CLAMP_TO_EDGE
        } else {
            SGI_GL_REPEAT
        };
        // A 1D image must not acquire a T-axis border tap during filtering.
        image.wrap_t = if one_d_clamp {
            SGI_GL_CLAMP_TO_EDGE
        } else if border {
            SGI_GL_CLAMP_TO_BORDER
        } else if wt == 2 {
            SGI_GL_CLAMP
        } else if wt != 0 {
            SGI_GL_CLAMP_TO_EDGE
        } else {
            SGI_GL_REPEAT
        };
        if border || captured_clamp || general_clamp {
            image.border_color[0] = ((s.tex_sampler[5] >> 12) & 0xfff) as f32 / 4095.0f32;
            image.border_color[1] = (s.tex_sampler[5] & 0xfff) as f32 / 4095.0f32;
            image.border_color[2] = ((s.tex_sampler[6] >> 12) & 0xfff) as f32 / 4095.0f32;
            image.border_color[3] = (s.tex_sampler[6] & 0xfff) as f32 / 4095.0f32;
        }
        image.mag_filter = if detail_filter != 0 {
            detail_filter
        } else if (filter & 2) != 0 {
            SGI_GL_LINEAR
        } else {
            SGI_GL_NEAREST
        };
        image.min_filter = MIN_FILTERS[(min >> 5) as usize];
        odyssey_texture_lod_bias(s, image);

        if three_d {
            let e = (first * 3) as usize;
            let entry = [s.tex_levels[e], s.tex_levels[e + 1], s.tex_levels[e + 2]];
            let stride_units = entry[1] & 0xfff;
            let expected_units = if three_d_stored { 4 } else { ((width + 15) / 16 + 1) & !1u32 };
            let mut base = entry[0] & 0x1fffffff;
            let mipmap = min >= 0x40;

            // Observed small-volume mip levels follow the preceding level's
            // slices, with one 128-byte address unit per slice. Larger-volume
            // packing and nonzero table selectors are not established. Keep
            // this bounded model policy separate from the level-zero address
            // rule.
            if first != 0
                || s.tex_level0 != 0
                || (!mipmap && word != 0x13000000 && word != 0x13000022 && !three_d_clamp)
                || (mipmap
                    && ((filter != 0x40 && filter != 0xa0)
                        || ws != 0
                        || wt != 0
                        || (bf != SGI_GL_RGB && bf != SGI_GL_RGBA)
                        || width > 16
                        || height > 16
                        || depth > 16
                        || stride_units != 2
                        || entry[2] != 1))
                || s.tex_env[0] > 2
                || entry[1] != (0x41100000 | stride_units)
                || (three_d_stored && entry[2] != height + 2)
                || entry[2] == 0
                || stride_units != expected_units
                || base == 0
                || (three_d_stored
                    && (bf != SGI_GL_RGBA || width > 48 || height > 30 || depth > 30 || (filter != 0 && filter != 0x22)))
                || s.obj_count > SGI_ODYSSEY_OBJECTS
                || entry[2] > ODSY_VOLUME_SLICE_SHIFT_MAX
            {
                break 'unsupported;
            }
            let slice_stride = odyssey_volume_slice_stride(entry[2]);
            // The captured 2x2x2 stored upload starts 15 slice strides above
            // the level base. For this one captured profile the rule applies
            // at any level base: a compatibility approximation of model
            // traces that showed it at two bases. Physical V12 texture layout
            // remains unqualified.
            if three_d_stored && width == 2 && height == 2 && depth == 2 {
                let mut first_slice = 0u64;

                if gl_umul_ovf(slice_stride, 15, &mut first_slice)
                    || gl_uadd_ovf(base as u64, first_slice, &mut first_slice)
                    || first_slice > u32::MAX as u64
                {
                    break 'unsupported;
                }
                base = first_slice as u32;
            }
            terminal = cmax(cmax(ctz32(width), ctz32(height)), ctz32(depth));
            count = if mipmap && hi > 0 { cmin(terminal, ((hi + 255) / 256) as u32) + 1 } else { 1 };
            if count > SGI_GL_TEX_LEVELS_MAX {
                break 'unsupported;
            }
            *chain = OdysseyTextureChain::default();
            let chain_opaque = P::new(chain).opaque();
            let mut slice_next: u32 = 0;
            for i in 0..count {
                let level_width = cmax(1, width >> i);
                let level_height = cmax(1, height >> i);
                let level_depth = cmax(1, depth >> i);
                let mut next_base = 0u64;

                if i != 0
                    && (s.tex_levels[(i * 3) as usize] != 0
                        || s.tex_levels[(i * 3 + 1) as usize] != 0
                        || s.tex_levels[(i * 3 + 2) as usize] != 0)
                {
                    break 'unsupported;
                }
                let level_slice = slice_next;
                let mut z = 0u32;
                while z < level_depth + (if three_d_stored { 2 } else { 0 }) {
                    let mut offset = 0u64;
                    let mut slice_base = 0u64;
                    let mut storage_end = 0u64;

                    if gl_umul_ovf(z as u64, slice_stride, &mut offset)
                        || gl_uadd_ovf(base as u64, offset, &mut slice_base)
                        || slice_base > u32::MAX as u64
                    {
                        break 'unsupported;
                    }
                    let object = odyssey_object_find(s, slice_base as u32);
                    if object < 0 {
                        break 'unsupported;
                    }
                    let o = object as usize;
                    let sb: u32 = if three_d_stored { 15 } else { 0 };
                    let sb2: u32 = if three_d_stored { 2 } else { 0 };
                    if s.obj_width[o] != stride_units * 16
                        || s.obj_rows[o] < sb + level_height + sb2
                        || gl_umul_ovf(
                            sb as u64 + level_height as u64 - 1 + sb2 as u64,
                            s.obj_width[o] as u64,
                            &mut storage_end,
                        )
                        || gl_uadd_ovf(storage_end, s.obj_offset[o] as u64, &mut storage_end)
                        || gl_uadd_ovf(
                            storage_end,
                            if three_d_stored { level_width as u64 + 18 } else { level_width as u64 },
                            &mut storage_end,
                        )
                        || storage_end > s.obj_used as u64
                        || storage_end > (s.obj_mem.bytes_len / 4) as u64
                    {
                        break 'unsupported;
                    }
                    if three_d_stored && !odyssey_object_region_written(s, object, 15, 15, level_width + 2, level_height + 2) {
                        break 'unsupported;
                    }
                    if slice_next as usize >= s.tex_slice_offset.len() {
                        break 'unsupported;
                    }
                    s.tex_slice_offset[slice_next as usize] = s.obj_offset[o];
                    s.tex_slice_width[slice_next as usize] = s.obj_width[o];
                    slice_next += 1;
                    z += 1;
                }
                chain.volumes[i as usize] = OdysseyTextureVolume {
                    s: P::new(s),
                    base,
                    stride_units,
                    level_word2: entry[2],
                    width: level_width,
                    height: level_height,
                    depth: level_depth,
                    base_format: image.base_format,
                    words: s.obj_used,
                    border: three_d_stored,
                    first_slice: level_slice,
                    slices: slice_next - level_slice,
                    ..Default::default()
                };
                chain.levels[i as usize] = SGIGLTexLevel {
                    level: i,
                    width: level_width,
                    height: level_height,
                    depth: level_depth,
                    base_format: image.base_format,
                    internal_format: image.base_format,
                    read_texel_3d: Some(odyssey_texture_read_volume_level),
                    opaque: chain_opaque,
                    ..Default::default()
                };
                if i + 1 < count {
                    if gl_umul_ovf(level_depth as u64, slice_stride, &mut next_base)
                        || gl_uadd_ovf(base as u64, next_base, &mut next_base)
                        || next_base > u32::MAX as u64
                    {
                        break 'unsupported;
                    }
                    base = next_base as u32;
                }
            }
            chain.volume = chain.volumes[0];
            image.depth = depth;
            image.volume = true;
            image.wrap_r = if three_d_stored {
                SGI_GL_CLAMP
            } else if three_d_clamp && (word & 0x200) != 0 {
                SGI_GL_CLAMP
            } else {
                SGI_GL_REPEAT
            };
            image.read_texel_3d = Some(odyssey_texture_read_3d);
            image.border = three_d_stored as u32;
            if three_d_stored {
                image.wrap_s = SGI_GL_CLAMP;
                image.wrap_t = SGI_GL_CLAMP;
                image.read_border_texel_3d = Some(odyssey_texture_read_border_3d);
            }
            image.opaque = P::new(&mut chain.volume).opaque();
            if !odyssey_texture_scale_bias_identity(s) {
                chain.volume.scale_bias = true;
                image.post_filter = Some(odyssey_texture_post_filter);
            }
            if mipmap {
                chain.pyramid = SGIGLTexPyramid {
                    levels: P(chain.levels.as_mut_ptr()),
                    count,
                    base_level: 0,
                    max_level: count - 1,
                    min_lod: lo as f64 / 256.0,
                    max_lod: hi as f64 / 256.0,
                };
                image.pyramid = P::new(&mut chain.pyramid);
            }
            *resolved = P::new(image);
            return true;
        }

        terminal = cmax((sampler_size >> 10) & 31, (sampler_size >> 5) & 31);
        count = if min >= 0x40 && hi > 0 { cmin(terminal, ((hi + 255) / 256) as u32) + 1 } else { 1 };
        if count > SGI_GL_TEX_LEVELS_MAX || first + count > SGI_ODYSSEY_TEX_LEVELS {
            break 'unsupported;
        }
        *chain = OdysseyTextureChain::default();
        let chain_opaque = P::new(chain).opaque();
        chain.volume.s = P::new(s);
        chain.volume.colour_table_alpha = colour_table_rgba8_rgba || word == 0x11600001;
        image.opaque = P::new(&mut chain.volume).opaque();
        chain.volume.colour_table = colour_table;
        chain.volume.scale_bias = !odyssey_texture_scale_bias_identity(s);
        if chain.volume.colour_table || chain.volume.scale_bias {
            image.post_filter = Some(odyssey_texture_post_filter);
        }
        for i in 0..count {
            let e = ((first + i) * 3) as usize;
            let entry = [s.tex_levels[e], s.tex_levels[e + 1], s.tex_levels[e + 2]];
            let level_width = cmax(1, width >> i);
            let level_height = cmax(1, height >> i);
            let stride_units = entry[1] & 0xfff;
            // A bordered level's row is its width plus 16 texels each side,
            // in 16-texel units rounded up to even, as the unbordered rule.
            let expected_units = if stored_border {
                if one_d_shape && level_width == 64 { 6 } else { 4 }
            } else if general_border {
                ((level_width + 32 + 15) / 16 + 1) & !1u32
            } else {
                ((level_width + 15) / 16 + 1) & !1u32
            };
            let base = entry[0] & 0x1fffffff;
            let interior_x: u32 = if bordered { 16 } else { 0 };
            let interior_y: u32 = if two_d_stored || general_border { 16 } else { 0 };
            let rows = level_height + interior_y + (two_d_stored || general_border) as u32;
            let require_backing = stored_border;

            if (stored_border && (entry[1] != (if expected_units == 6 { 0x41100006 } else { 0x41100004 }) || base == 0))
                || (two_d_clamp
                    && !stored_border
                    && (filter != 0 || s.tex_sampler[5] != 0 || s.tex_sampler[6] != 0)
                    && entry[1] != 0x41100002)
                || (entry[1] & 0x3c0ff000) != 0
                || entry[2] != 0
                || (i != 0 && (entry[1] & 0xc3f00000) != (s.tex_levels[(first * 3 + 1) as usize] & 0xc3f00000))
                || (base != 0 && stride_units != expected_units && !(count == 1 && stride_units == 0))
            {
                break 'unsupported;
            }
            chain.levels[i as usize] = SGIGLTexLevel {
                level: i,
                width: level_width,
                height: level_height,
                base_format: image.base_format,
                internal_format: image.base_format,
                opaque: chain_opaque,
                ..Default::default()
            };
            if base == 0 || s.obj_count > SGI_ODYSSEY_OBJECTS {
                if require_backing {
                    break 'unsupported;
                }
                continue;
            }
            let object = odyssey_object_find(s, base);
            let backed = object >= 0 && {
                let o = object as usize;
                !(s.obj_width[o] < level_width
                    || (stride_units != 0 && s.obj_width[o] != stride_units * 16)
                    || s.obj_rows[o] < rows
                    || gl_umul_ovf((rows - 1) as u64, s.obj_width[o] as u64, &mut end)
                    || gl_uadd_ovf(end, s.obj_offset[o] as u64, &mut end)
                    || gl_uadd_ovf(end, interior_x as u64, &mut end)
                    || gl_uadd_ovf(end, level_width as u64 + bordered as u64, &mut end)
                    || end > s.obj_used as u64
                    || end > (s.obj_mem.bytes_len / 4) as u64)
            };
            if !backed {
                if require_backing {
                    break 'unsupported;
                }
                continue;
            }
            let o = object as usize;
            if require_backing {
                let written = odyssey_object_region_written(
                    s,
                    object,
                    interior_x - 1,
                    if two_d_stored { interior_y - 1 } else { 0 },
                    level_width + 2,
                    if one_d_shape { 1 } else { level_height + 2 },
                );

                if !written {
                    break 'unsupported;
                }
            }
            chain.storage[i as usize] = OdysseyTexture {
                // Captured stored-border policy uses interior coordinate 16.
                s: P::new(s),
                offset: s.obj_offset[o].wrapping_add(interior_y.wrapping_mul(s.obj_width[o])).wrapping_add(interior_x),
                stride: s.obj_width[o],
                width: level_width,
                height: level_height,
                base_format: image.base_format,
                words: s.obj_used,
                border_rows: two_d_stored || general_border,
            };
            chain.levels[i as usize].read_texel = Some(odyssey_texture_read_level);
            if bordered {
                chain.levels[i as usize].border = 1;
                chain.levels[i as usize].read_border_texel = Some(odyssey_texture_read_border_level);
            }
        }
        chain.pyramid = SGIGLTexPyramid {
            levels: P(chain.levels.as_mut_ptr()),
            count,
            base_level: 0,
            max_level: count - 1,
            min_lod: lo as f64 / 256.0,
            max_lod: if min >= 0x40 { hi as f64 / 256.0 } else { 15.0 },
        };
        image.pyramid = P::new(&mut chain.pyramid);
        if detail_identity {
            if !odyssey_texture_detail_backing_setup(
                s,
                chain,
                detail_sampler.descriptor,
                detail_sampler.width,
                detail_sampler.height,
                image.base_format,
            ) {
                break 'unsupported;
            }
            chain.volume.detail_weight = detail_weight;
            image.detail_image = P::new(&mut chain.detail_image);
            image.detail_level = detail_sampler.level;
            image.detail_mode = detail_sampler.mode;
            image.read_filter_weight = Some(odyssey_texture_detail_weight);
        }
        *resolved = P::new(image);
        return true;
    }
    let index_mode = odyssey_gl_index_mode(s);
    crate::qlog!(
        "sgi-odyssey: unsupported texture state sampler {:#x} environment {}{}, draw dropped",
        s.tex_sampler[4],
        s.tex_env[0],
        if index_mode { " colour-index" } else { "" }
    );
    false
}

/// Rebuild shared state from family words; no borrowed image survives a draw.
pub fn odyssey_texture_vertex(s: &mut SGIOdysseyState) {
    const ENVIRONMENTS: [u32; 5] = [SGI_GL_REPLACE, SGI_GL_REPLACE, SGI_GL_MODULATE, SGI_GL_DECAL, SGI_GL_BLEND];
    let mut strq: [u32; 4] = [s.tex_coord[0], s.tex_coord[1], s.tex_coord_r, s.tex_coord_q];
    let one_d = (s.tex_mode & (ODSY_TEX_MODE_1D | ODSY_TEX_MODE_2D)) == ODSY_TEX_MODE_1D;
    let mut matrix = [0.0f32; 16];

    s.gl_ctx.cur.texture_2d = ((s.tex_mode & (ODSY_TEX_MODE_1D | ODSY_TEX_MODE_2D)) != 0) as u32;
    s.gl_ctx.cur.texture_3d = ((s.tex_mode & ODSY_TEX_MODE_3D) != 0) as u32;
    // Excluded environments are rejected by the family before rasterization.
    s.gl_ctx.cur.texture_env_mode = SGI_GL_REPLACE;
    if (s.tex_env[0] as usize) < ENVIRONMENTS.len() {
        s.gl_ctx.cur.texture_env_mode = ENVIRONMENTS[s.tex_env[0] as usize];
    }
    // Captured linear planes already include the eye-space transform.
    if (s.cmd_tag == ODSY_GL_VERTEX_2I
        || s.cmd_tag == ODSY_GL_VERTEX_2F
        || s.cmd_tag == ODSY_GL_VERTEX_3F
        || s.cmd_tag == ODSY_GL_VERTEX_3I
        || s.cmd_tag == ODSY_GL_VERTEX_4F)
        && s.gl_linear_texgen != 0
    {
        let mut vertex = [
            odyssey_f32(s.cmd_operand[0]),
            odyssey_f32(s.cmd_operand[1]),
            if s.cmd_tag != ODSY_GL_VERTEX_2I && s.cmd_tag != ODSY_GL_VERTEX_2F { odyssey_f32(s.cmd_operand[2]) } else { 0.0 },
            if s.cmd_tag == ODSY_GL_VERTEX_4F { odyssey_f32(s.cmd_operand[3]) } else { 1.0 },
        ];

        if s.cmd_tag == ODSY_GL_VERTEX_2I || s.cmd_tag == ODSY_GL_VERTEX_3I {
            vertex[0] = (s.cmd_operand[0] as i32) as f64;
            vertex[1] = (s.cmd_operand[1] as i32) as f64;
        }
        if s.cmd_tag == ODSY_GL_VERTEX_3I {
            vertex[2] = (s.cmd_operand[2] as i32) as f64;
        }
        for axis in 0..4usize {
            let plane = if axis == 3 {
                s.tex_linear_plane_q
            } else if axis == 2 {
                s.tex_linear_plane_r
            } else {
                s.tex_linear_plane[axis]
            };
            let mut value = 0.0f32;

            if (s.gl_linear_texgen & s.tex_linear_valid & (1u32 << axis)) == 0 {
                continue;
            }
            for k in 0..4usize {
                value = (value as f64 + odyssey_f32(plane[k]) * vertex[k]) as f32;
            }
            strq[axis] = value.to_bits();
        }
    }
    let rows = s.gl_texture_matrix;
    odyssey_gl_transpose(&rows, &mut matrix);
    // Input T may transform S, but transformed T cannot affect 1D sampling.
    if one_d {
        for i in 0..4usize {
            matrix[i * 4 + 1] = 0.0;
        }
    }
    sgi_gl_set_matrix(&mut s.gl_ctx, SGI_GL_TEXTURE, &matrix);
    odyssey_gl_feed(s, SGI_GL_OP_TEXCOORD4F as u16, &strq);
}

/// The context emits one assembled primitive here; rasterize it into the
/// plane. Shared between the fire-and-forget and checked sink callbacks
/// below; only the out-of-memory result differs in how it is reported back.
pub fn odyssey_gl_draw_result(opaque: Opaque, d: &SGIGLDraw) -> SGIGLDrawResult {
    let s = from_opaque::<SGIOdysseyState>(opaque);
    let mut t = odyssey_gl_target(s);
    let rules = odyssey_gl_operation_rules(s);
    let mut state: SGIGLDrawState = *d.state;
    let mut draw = *d;
    let mut image = SGIGLTexImage::default();
    // Some 7 KB that only a textured draw uses, and odyssey_texture_setup()
    // clears before it fills any of it, so it is not built for every
    // primitive.
    let mut texture: Option<Box<OdysseyTextureChain>> = None;

    if d.state.texture_2d != 0 || d.state.texture_3d != 0 {
        let chain = texture.insert(Box::default());

        if !odyssey_texture_setup(s, &mut image, chain, &mut t.texture) {
            crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_gl_draw_path: backend scalar-skip result {} called=0", SGI_GL_DRAW_OK);
            return SGI_GL_DRAW_OK;
        }
    }
    if (state.texture_2d != 0 || state.texture_3d != 0) && s.tex_env[0] == 4 {
        // The family words pack two normalized 12-bit components each.
        state.texture_env_color[0] = ((s.tex_env[1] >> 12) & 0xfff) as f32 / 4095.0f32;
        state.texture_env_color[1] = (s.tex_env[1] & 0xfff) as f32 / 4095.0f32;
        state.texture_env_color[2] = ((s.tex_env[2] >> 12) & 0xfff) as f32 / 4095.0f32;
        state.texture_env_color[3] = (s.tex_env[2] & 0xfff) as f32 / 4095.0f32;
    }
    if !odyssey_gl_index_mode(s) {
        state.colour_mask = (s.gl_write_mask >> ODSY_WRITE_COLOUR_SHIFT) & ODSY_WRITE_COLOUR_MASK;
    }
    state.fog_color[0] = ((s.gl_fog[0] >> 12) & 0xfff) as f32 / 4096.0f32;
    state.fog_color[1] = (s.gl_fog[0] & 0xfff) as f32 / 4096.0f32;
    state.fog_color[2] = ((s.gl_fog[1] >> 12) & 0xfff) as f32 / 4096.0f32;
    state.fog_color[3] = (s.gl_fog[1] & 0xfff) as f32 / 4096.0f32;
    state.fog_index = ((s.gl_fog[0] >> 12) & 0xfff) as f32;
    draw.state = P::new(&mut state);
    odyssey_gl_report_refusals(s, true);
    // The board's thread holds the board lock for the whole draw; a
    // processor's accesses that need not wait for it use the front lock
    // (front.rs).
    let result = sgi_soft_draw(&rules, &t, &draw);
    crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_gl_draw_path: backend scalar result {} called=1", result);
    sgi_scanout_invalidate_all(&mut s.scanout);
    drop(texture);
    result
}

pub fn odyssey_gl_draw(opaque: Opaque, d: &SGIGLDraw) {
    odyssey_gl_draw_result(opaque, d);
}

/// The checked path the shared context requires once per-face polygon modes
/// are activated (odyssey_reset_registers): it relays sgi_soft_draw()'s
/// out-of-memory signal from GL_POLYGON's dynamic vertex assembly back to
/// the shared context synchronously instead of dropping it. The native
/// renderer does not exist here, so this is always the scalar draw.
pub fn odyssey_gl_draw_checked(opaque: Opaque, d: &SGIGLDraw) -> SGIGLDrawResult {
    odyssey_gl_draw_result(opaque, d)
}

/// glClear's raster op: not on the first-triangle path, wired for later.
pub fn odyssey_gl_raster(opaque: Opaque, op: &SGIRasterOp) {
    let s = from_opaque::<SGIOdysseyState>(opaque);
    let t = odyssey_gl_target(s);
    let rules = odyssey_gl_operation_rules(s);

    odyssey_gl_report_refusals(
        s,
        op.kind != SGI_RASTER_CLEAR || (op.clear_buffers & SGI_GL_COLOR_BUFFER_BIT) != 0,
    );
    sgi_soft_raster(&rules, &t, op);
    sgi_scanout_invalidate_all(&mut s.scanout);
}

pub fn odyssey_gl_sink(s: &mut SGIOdysseyState) -> SGIGLSink {
    SGIGLSink {
        draw: Some(odyssey_gl_draw),
        draw_checked: Some(odyssey_gl_draw_checked),
        raster: Some(odyssey_gl_raster),
        fence: None,
        opaque: P::new(s).opaque(),
        result: P::null(),
    }
}

fn gl_clip_rect_eq(a: &SGIClipRect, b: &SGIClipRect) -> bool {
    a.x0 == b.x0 && a.y0 == b.y0 && a.x1 == b.x1 && a.y1 == b.y1
}

// The C compares the two drawables byte for byte.
fn gl_drawable_eq(a: &SGIGLDrawable, b: &SGIGLDrawable) -> bool {
    a.bound == b.bound
        && a.origin == b.origin
        && a.width == b.width
        && a.height == b.height
        && a.clip.n == b.clip.n
        && a.clip.empty == b.clip.empty
        && a.clip.r.iter().zip(b.clip.r.iter()).all(|(x, y)| gl_clip_rect_eq(x, y))
        && a.front == b.front
        && a.back == b.back
        && a.extra == b.extra
        && a.overlay == b.overlay
}

/// Point the shared context at the current window: its origin from 0x841
/// and the kept 0x204b rectangles as the pixels it owns, in plane
/// coordinates. Called before every command, so a change to either
/// register takes effect at the next one. With no rectangle kept, one
/// inverted rectangle keeps the shared rasterizer drawing nothing rather
/// than logging an unbound window. The shared context's front and back
/// targets carry buffers 0 and 1, the raster target callbacks' buffer
/// numbers, and its draw buffers follow the 0x0424 drawing mask. The kernel
/// has already turned the client's front and back into buffer numbers
/// (ODSY_REG_DRAW_MASK), so the shared names are slots here, not the
/// client's roles: a client drawing its front after one swap draws the
/// context's "back", buffer 1.
///
/// The shared rasterizer adds window rows to the origin as they come; the
/// client's combined matrix already counts them down from the window's top
/// row, the one 0x841 names (odyssey_gl_load_combined()), so the origin,
/// rectangles and id plane all stay in the registers' top-down plane
/// coordinates.
pub fn odyssey_gl_bind(s: &mut SGIOdysseyState) {
    let buffers = odyssey_draw_buffers(s);
    let mut draw: u32 = (if buffers & 1 != 0 { SGI_GL_DRAW_FRONT } else { 0 })
        | (if buffers & 2 != 0 { SGI_GL_DRAW_BACK } else { 0 });
    let n = cmax(s.clip_kept, 1);
    let origin = s.win_origin;
    let clip_kept = s.clip_kept;
    let clip_rect = s.clip_rect;

    // This runs before every command, but the drawable changes only with
    // the window registers, so it is built in place and handed over only
    // when it differs from the shared context's. gl_bind_drawable's other
    // fields keep their instance_init values, and its rectangles past the
    // count stay zero, so it is the drawable a fresh build would make.
    {
        let d = &mut s.gl_bind_drawable;

        d.origin[0] = origin[0];
        d.origin[1] = origin[1];
        if clip_kept != 0 {
            for i in 0..clip_kept as usize {
                d.clip.r[i] = clip_rect[i];
            }
        } else {
            // An all-empty clip; SGIClipList.empty replaces this at rebase.
            d.clip.r[0] = SGIClipRect { x0: 1, y0: 1, x1: 0, y1: 0 };
        }
        if d.clip.n > n {
            for i in n as usize..d.clip.n as usize {
                d.clip.r[i] = SGIClipRect::default();
            }
        }
        d.clip.n = n;
    }
    if !gl_drawable_eq(&s.gl_bind_drawable, &s.gl_ctx.cur.drawable) {
        let d = s.gl_bind_drawable;
        sgi_gl_set_drawable(&mut s.gl_ctx, &d);
    }
    if s.gl_ctx.cur.draw_buffers != draw {
        let sink = odyssey_gl_sink(s);
        let cmd = SGIGLCmd::new(SGI_GL_OP_DRAW_BUFFERS as u16, std::slice::from_mut(&mut draw));

        sgi_gl_command(&mut s.gl_ctx, &sink, &cmd);
    }
}

/// Decode one command into the shared context.
pub fn odyssey_gl_feed(s: &mut SGIOdysseyState, op: u16, args: &[u32]) {
    let sink = odyssey_gl_sink(s);
    let cmd = SGIGLCmd::new(op, args);

    odyssey_gl_bind(s);
    sgi_gl_command(&mut s.gl_ctx, &sink, &cmd);
}

// odyssey_gl_feed with the op as the shared layer's u32 enum.
fn gl_feed32(s: &mut SGIOdysseyState, op: SGIGLOp, args: &[u32]) {
    odyssey_gl_feed(s, op as u16, args);
}

/// 0x404 bits 10:9 to the shared layer's cull-face enum: GL_BACK 01,
/// GL_FRONT 10, GL_FRONT_AND_BACK 11. Returns UINT32_MAX for 00, which no
/// probe word has sent; the shared layer's own glCullFace validation then
/// rejects it, leaving the previous mode in place.
pub fn odyssey_cull_face_mode(code: u32) -> u32 {
    match code {
        1 => SGI_GL_BACK,
        2 => SGI_GL_FRONT,
        3 => SGI_GL_FRONT_AND_BACK,
        _ => u32::MAX,
    }
}

/// 0x404 bit 23 to the shared layer's glFrontFace sense, inverted -- see
/// the ODSY_MODE_FRONT_FACE_CW comment for why.
pub fn odyssey_front_face(pattern_mode: u32) -> u32 {
    if (pattern_mode & ODSY_MODE_FRONT_FACE_CW) != 0 { SGI_GL_CCW } else { SGI_GL_CW }
}

/// Feed one raster-state register (polygon mode, line width and point
/// size, line stipple and its enable). A context swap drops the outgoing
/// primitive and only then re-pushes the owner's polygon mode, sizes and
/// stipple; on the board no primitive is open by then. The model is
/// different: it keeps a GL_POLYGON open across the switch, because it
/// assembles a polygon whole and the board's way of continuing one after a
/// switch is unknown (odyssey_cmd_dispatch()'s 0x0444 and 0x0850 cases).
/// The shared layer would refuse the restored values as GL calls inside
/// glBegin and keep what the other context left, so while a GL_POLYGON is
/// held open its begin flag is set aside for the one call. APPROXIMATION:
/// this follows the model's hold, not any board behaviour; for every other
/// primitive the shared layer's rule stands. The hold is right only because
/// the model saves no GL_POLYGON vertices to the shadow, so the kernel's
/// restart replays nothing: if the model ever saves them, it must stop
/// holding at the same time, or the polygon draws twice.
pub fn odyssey_gl_feed_raster_state(s: &mut SGIOdysseyState, op: u16, args: &[u32]) {
    let in_begin = s.gl_ctx.in_begin;
    let held_polygon = s.gl_begin_open && s.gl_primitive == 9;

    if held_polygon {
        s.gl_ctx.in_begin = 0;
    }
    odyssey_gl_feed(s, op, args);
    s.gl_ctx.in_begin = in_begin;
}

/// Hand the GL state register 0x0404 carries to the shared context: bit 12,
/// the shade model, bit 16, the depth test, bit 14, the alpha test, bit 15,
/// the stencil test, bit 17, blending, bit 19, index logic op, bit 20,
/// colour logic op, bit 8, GL_CULL_FACE, bits 10:9, the face it culls, and
/// bit 23, front face (fed inverted; see the ODSY_MODE_FRONT_FACE_CW
/// comment), and bit 3, float GL line stipple. The shared rasterizer fills
/// a flat triangle from its provoking vertex, the last one for triangles
/// and strips as GL specifies. Nothing in the stream says which vertex the
/// board takes; the library never reorders or repeats colours to steer it,
/// so GL's own rule is INFERRED. Called on every 0x0404, including the X
/// server's pattern-mode writes, which also update the separate integer 2D
/// interpreter.
///
/// Bit 15, the stencil test, gates fragments through the model's stencil
/// plane (ODSY_MODE_STENCIL_TEST).
pub fn odyssey_gl_apply_polygon_stipple(s: &mut SGIOdysseyState) {
    let cap = SGI_GL_POLYGON_STIPPLE;
    let enable = (s.pattern_mode & ODSY_GL_RECT_PATTERN_ENABLE) != 0 && (s.pattern_shape & !ODSY_PATTERN_SHAPE_OPAQUE) == 0;

    if s.gl_in_prim {
        return;
    }
    if enable {
        let mut rows = [0u32; 32];

        for i in 0..rows.len() {
            rows[i] = s.pattern_buffer[31 - i];
        }
        if !sgi_gl_polygon_stipple(&mut s.gl_ctx, &rows) {
            return;
        }
    }
    gl_feed32(s, if enable { SGI_GL_OP_ENABLE } else { SGI_GL_OP_DISABLE }, &[cap]);
}

/// 0x0404 bit 17 (ODSY_MODE_BLEND) blends only into an RGBA drawable:
/// OpenGL blends RGBA fragments alone (OpenGL 1.2.1, 4.1.6), and the X
/// server sets the bit with factors ONE, ZERO for its opaque stipples,
/// whose background is the 0x0c21 constant colour and needs no destination
/// (ODSY_PATTERN_SHAPE_OPAQUE). Fed again when the colour mode changes.
pub fn odyssey_gl_apply_blend(s: &mut SGIOdysseyState) {
    let blend = SGI_GL_BLEND;
    let on = (s.pattern_mode & ODSY_MODE_BLEND) != 0 && !odyssey_gl_index_mode(s);

    gl_feed32(s, if on { SGI_GL_OP_ENABLE } else { SGI_GL_OP_DISABLE }, &[blend]);
}

pub fn odyssey_gl_apply_mode(s: &mut SGIOdysseyState) {
    let shade = if (s.pattern_mode & ODSY_MODE_FLAT_SHADE) != 0 { SGI_GL_FLAT } else { SGI_GL_SMOOTH };
    let cull_mode = odyssey_cull_face_mode((s.pattern_mode >> ODSY_MODE_CULL_MODE_SHIFT) & ODSY_MODE_CULL_MODE_MASK);
    let front_face = odyssey_front_face(s.pattern_mode);
    let pm = s.pattern_mode;
    let en = |bit: u32| if (pm & bit) != 0 { SGI_GL_OP_ENABLE } else { SGI_GL_OP_DISABLE };

    gl_feed32(s, SGI_GL_OP_SHADE_MODEL, &[shade]);
    // Bit assignments follow the library's fragment-state encoding.
    gl_feed32(s, en(ODSY_MODE_POINT_SMOOTH), &[SGI_GL_POINT_SMOOTH]);
    gl_feed32(s, en(ODSY_MODE_LINE_SMOOTH), &[SGI_GL_LINE_SMOOTH]);
    // The shared layer has no dither rule yet, so the bit is kept as family
    // state and reported at the first draw it would affect
    // (odyssey_gl_vertex_colour()). Once the shared rule lands this feeds
    // SGI_GL_OP_ENABLE/DISABLE of SGI_GL_DITHER instead.
    s.gl_dither = (s.pattern_mode & ODSY_MODE_DITHER) != 0;
    gl_feed32(s, en(ODSY_MODE_DEPTH_TEST), &[SGI_GL_DEPTH_TEST]);
    gl_feed32(s, en(ODSY_MODE_ALPHA_TEST), &[SGI_GL_ALPHA_TEST]);
    gl_feed32(s, en(ODSY_MODE_STENCIL_TEST), &[SGI_GL_STENCIL_TEST]);
    odyssey_gl_apply_blend(s);
    gl_feed32(s, en(ODSY_MODE_FOG), &[SGI_GL_FOG]);
    gl_feed32(s, en(ODSY_MODE_INDEX_LOGIC_OP), &[SGI_GL_INDEX_LOGIC_OP]);
    gl_feed32(s, en(ODSY_MODE_COLOR_LOGIC_OP), &[SGI_GL_COLOR_LOGIC_OP]);
    gl_feed32(s, en(ODSY_MODE_OFFSET_POINT), &[SGI_GL_POLYGON_OFFSET_POINT]);
    gl_feed32(s, en(ODSY_MODE_OFFSET_LINE), &[SGI_GL_POLYGON_OFFSET_LINE]);
    gl_feed32(s, en(ODSY_MODE_OFFSET_FILL), &[SGI_GL_POLYGON_OFFSET_FILL]);
    gl_feed32(s, en(ODSY_MODE_CULL_FACE), &[SGI_GL_CULL_FACE]);
    odyssey_gl_feed_raster_state(s, en(ODSY_MODE_GL_LINE_STIPPLE) as u16, &[SGI_GL_LINE_STIPPLE]);
    gl_feed32(s, SGI_GL_OP_CULL_FACE, &[cull_mode]);
    gl_feed32(s, SGI_GL_OP_FRONT_FACE, &[front_face]);
    odyssey_gl_apply_polygon_stipple(s);
}

pub fn odyssey_gl_apply_line_stipple(s: &mut SGIOdysseyState) {
    // Word 1 remains the integer 2D path's 32-bit rotated pattern.
    let args = [(s.line_stipple[2] & 0xff) + 1, s.line_stipple[0] & 0xffff];

    odyssey_gl_feed_raster_state(s, SGI_GL_OP_LINE_STIPPLE as u16, &args);
}

/// Push the family-shadowed clip box (ODSY_REG_CLIP_BOX) to the shared
/// context's scissor rectangle (see the macro's own comment for the
/// encoding and the always-on rationale). min > max -- including the
/// library's own 0x1000, 0x1000 empty encoding -- clips every fragment,
/// matched here with a zero-area box rather than a negative width or
/// height, which SGI_GL_OP_SCISSOR rejects.
pub fn odyssey_gl_apply_clip_box(s: &mut SGIOdysseyState) {
    let x_min = (s.gl_clip_box[0] >> ODSY_CLIP_BOX_MIN_SHIFT) & ODSY_CLIP_BOX_FIELD_MASK;
    let x_max = s.gl_clip_box[0] & ODSY_CLIP_BOX_FIELD_MASK;
    let y_min = (s.gl_clip_box[1] >> ODSY_CLIP_BOX_MIN_SHIFT) & ODSY_CLIP_BOX_FIELD_MASK;
    let y_max = s.gl_clip_box[1] & ODSY_CLIP_BOX_FIELD_MASK;
    let mut box_ = [x_min, y_min, 0, 0];

    if x_min <= x_max && y_min <= y_max {
        box_[2] = x_max - x_min + 1;
        box_[3] = y_max - y_min + 1;
    }
    gl_feed32(s, SGI_GL_OP_SCISSOR, &box_);
}

/// Feed eye-space clip plane i to the shared context as the object-space
/// plane the current model-view gives it: p_obj[k] = sum_r p_eye[r]
/// M[r][k], M row-major as gl_modelview holds it.
pub fn odyssey_gl_apply_clip_plane(s: &mut SGIOdysseyState, i: u32) {
    let mut args = [0u32; 5];
    let mut eye = [0.0f64; 4];
    let mut obj = [0.0f64; 4];

    args[0] = i;
    for r in 0..4usize {
        eye[r] = odyssey_f32(s.gl_clip_eye[i as usize * 4 + r]);
    }
    for r in 0..4usize {
        for k in 0..4usize {
            obj[k] += eye[r] * odyssey_f32(s.gl_modelview[r * 4 + k]);
        }
    }
    for k in 0..4usize {
        args[1 + k] = odyssey_f32_bits(obj[k] as f32);
    }
    gl_feed32(s, SGI_GL_OP_CLIP_PLANE, &args);
}

/// Recompute which of the shared context's six clip-plane slots are
/// tested, from 0x0405 bits 2:0's thermometer count n (see
/// ODSY_TEX_MODE_CLIP_COUNT_MASK's comment): slots 0..n-1 enabled, the rest
/// disabled. Called on every 0x0405 write and after migration; does not
/// touch the loaded coefficients themselves, which a separate load
/// (ODSY_GL_CLIP_PLANE_LOAD) feeds.
pub fn odyssey_gl_apply_clip_planes(s: &mut SGIOdysseyState) {
    let bits = s.tex_mode & ODSY_TEX_MODE_CLIP_COUNT_MASK;
    let n = ctpop32(bits);

    for i in 0..SGI_GL_CLIP_PLANES {
        let plane = SGI_GL_CLIP_PLANE0 + i;

        gl_feed32(s, if i < n { SGI_GL_OP_ENABLE } else { SGI_GL_OP_DISABLE }, &[plane]);
    }
}

/// 0x0c21's 6-bit blend factor code to the shared layer's enum
/// (established): 0 GL_ZERO, 0x08 GL_ONE, 0x10 "the other colour"
/// (GL_DST_COLOR as a source factor, GL_SRC_COLOR as a destination factor
/// -- the board never needs both names at the same position, so one code
/// covers both), 0x18 GL_SRC_ALPHA, 0x20 GL_DST_ALPHA, 0x28
/// GL_CONSTANT_COLOR, 0x30 GL_CONSTANT_ALPHA, 0x38 the "one minus" of the
/// 0x10 pairing, 0x39 through 0x3c the "one minus" of 0x18/0x20/0x28/0x30
/// in turn, and 0x40, a source-only code, GL_SRC_ALPHA_SATURATE. Returns
/// UINT32_MAX for a code with no factor at the requested position.
pub fn odyssey_blend_factor(code: u32, dst: bool) -> u32 {
    match code {
        0x00 => 0, // GL_ZERO
        0x08 => 1, // GL_ONE
        0x10 => {
            if dst { SGI_GL_SRC_COLOR } else { SGI_GL_DST_COLOR }
        }
        0x18 => SGI_GL_SRC_ALPHA,
        0x20 => SGI_GL_DST_ALPHA,
        0x28 => SGI_GL_CONSTANT_COLOR,
        0x30 => SGI_GL_CONSTANT_ALPHA,
        0x38 => {
            if dst { SGI_GL_ONE_MINUS_SRC_COLOR } else { SGI_GL_ONE_MINUS_DST_COLOR }
        }
        0x39 => SGI_GL_ONE_MINUS_SRC_ALPHA,
        0x3a => SGI_GL_ONE_MINUS_DST_ALPHA,
        0x3b => SGI_GL_ONE_MINUS_CONSTANT_COLOR,
        0x3c => SGI_GL_ONE_MINUS_CONSTANT_ALPHA,
        0x40 => {
            if dst { u32::MAX } else { SGI_GL_SRC_ALPHA_SATURATE }
        }
        _ => u32::MAX,
    }
}

/// 0x041f's three 3-bit stencil-op fields (fail, depth-fail, depth-pass)
/// to the shared layer's enum: KEEP 0, ZERO 1, REPLACE 2, INCR 3, DECR 4,
/// INVERT 5. Returns UINT32_MAX for the two codes the board leaves unused.
pub fn odyssey_stencil_op(code: u32) -> u32 {
    match code {
        0 => SGI_GL_KEEP,
        1 => 0, // GL_ZERO
        2 => SGI_GL_REPLACE,
        3 => SGI_GL_INCR,
        4 => SGI_GL_DECR,
        5 => SGI_GL_INVERT,
        _ => u32::MAX,
    }
}

/// 0x0415's 2-bit face code to the shared layer's polygon-mode enum,
/// GL_POINT 0 to GL_FILL 2. Returns UINT32_MAX for the one code the board
/// leaves undefined, which the shared layer's own glPolygonMode validation
/// then rejects, leaving the previous mode in place.
pub fn odyssey_polygon_mode(code: u32) -> u32 {
    match code {
        0 => SGI_GL_POINT,
        1 => SGI_GL_LINE,
        2 => SGI_GL_FILL,
        _ => u32::MAX,
    }
}

/// 0x0c21 word 0 bits 15:13 to the shared layer's blend equation. Codes 3
/// and 4, the SGIX alpha min/max extension, have no shared equivalent
/// (sgi_gl_blend_equation_valid() accepts only the other six), so the
/// dispatch case reports and skips them rather than feeding a code the
/// shared layer would reject.
pub static odyssey_blend_equation: [u32; 8] = [
    SGI_GL_FUNC_ADD,
    SGI_GL_MIN,
    SGI_GL_MAX,
    0,
    0,
    SGI_GL_LOGIC_OP,
    SGI_GL_FUNC_SUBTRACT,
    SGI_GL_FUNC_REVERSE_SUBTRACT,
];

/// 0x0420 and 0x0452 to the shared context's comparison and depth mask.
pub fn odyssey_gl_apply_depth(s: &mut SGIOdysseyState) {
    let func = SGI_GL_NEVER_ENUM + (s.gl_depth_func & ODSY_DEPTH_FUNC_MASK);
    let mask = ((s.gl_write_mask & ODSY_WRITE_DEPTH) != 0) as u32;

    gl_feed32(s, SGI_GL_OP_DEPTH_FUNC, &[func]);
    gl_feed32(s, SGI_GL_OP_DEPTH_MASK, &[mask]);
}

/// The lighting words matched by value (ODSY_GL_NORMAL_3F and the rest).
pub static odyssey_gl_light_words: [u32; 16] = [
    ODSY_GL_NORMAL_3F,
    ODSY_GL_MATERIAL_AMBIENT,
    ODSY_GL_MATERIAL_DIFFUSE,
    ODSY_GL_MATERIAL_SPECULAR,
    ODSY_GL_MATERIAL_EMISSION,
    ODSY_GL_BACK_AMBIENT,
    ODSY_GL_BACK_DIFFUSE,
    ODSY_GL_BACK_SPECULAR,
    ODSY_GL_BACK_EMISSION,
    ODSY_GL_SHININESS_SQRT,
    ODSY_GL_BACK_SHININESS_SQRT,
    ODSY_GL_SHININESS_DERIVED,
    ODSY_GL_LIGHT_COLOUR_LOAD,
    ODSY_GL_LIGHT_VEC_LOAD,
    ODSY_GL_CLIP_PLANE_LOAD,
    ODSY_PROG_RUN_ONCE,
];

/// The texture-coordinate and generation fragments.
pub static odyssey_ucode_texgen: [u32; 14] = [
    0x033, 0x04d, 0x052, 0x057, 0x05c, 0x061, 0x066, 0x06b, 0x06c, 0x077, 0x07b, 0x07f, 0x091, 0x09f,
];

pub fn odyssey_prog_value(word: u32) -> u32 {
    (word & ODSY_PROG_LOW_MASK) | (((word >> ODSY_PROG_HIGH_SHIFT) & 1) << 10)
}

pub fn odyssey_is_program_word(word: u32) -> bool {
    let cls = word >> ODSY_PROG_CLASS_SHIFT;

    if (word & ODSY_PROG_SHAPE) != ODSY_PROG_TAG {
        return false;
    }
    if cls >= ODSY_PROG_PROGRAM_HALFWORD && cls <= ODSY_PROG_PROGRAM_HALFWORD_LAST {
        return true;
    }
    if cls >= ODSY_PROG_SEQ_FIRST
        && cls <= ODSY_PROG_SEQ_LAST
        && (word & ODSY_GL_MATRIX_SELECT_SHAPE) != ODSY_GL_MATRIX_SELECT_TAG
    {
        return true;
    }
    matches!(
        cls,
        ODSY_PROG_COLOUR_MODE
            | ODSY_PROG_COLOUR_MODE_B
            | ODSY_PROG_SET_UCODE
            | ODSY_PROG_PROGRAM_ADDR
            | ODSY_PROG_CONST_SELECT
            | ODSY_PROG_ENTRY_29
            | ODSY_PROG_ENTRY_2A
            | ODSY_PROG_ENTRY_2C
            | ODSY_PROG_LIGHT_VEC_SELECT
            | ODSY_PROG_LIGHT_COLOUR_SELECT
            | ODSY_PROG_LIST_INDEX
            | ODSY_PROG_COLOUR_MATERIAL
            | ODSY_PROG_CONST_STORE
    )
}

pub fn odyssey_ucode_is_texgen(a: u32) -> bool {
    for i in 0..odyssey_ucode_texgen.len() {
        if a == odyssey_ucode_texgen[i] {
            return true;
        }
    }
    false
}
