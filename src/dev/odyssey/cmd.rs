//! Ported from hw/misc/sgi-odyssey.c in jamesbraid/qemu-sgi-origami.
//!
//! The command-FIFO interpreter: word classification, operand gathering and
//! the one big dispatch of a completed command.

use super::*;

pub fn odyssey_cmd_dispatch(s: &mut SGIOdysseyState, tag: u32) {
    let mut after_tail = false;

    // Only the kernel command right after its window-origin write can be its tail.
    if s.cmd_from_kernel && tag != ODSY_REG_WINDOW_ORIGIN && tag != ODSY_REG_WINDOW_CLIP {
        s.win_origin_pending = false;
    }
    // And only the one right after the tail's 0x204b is its 0x840.
    if s.cmd_from_kernel {
        after_tail = s.win_after_tail;
        s.win_after_tail = false;
    }
    if odyssey_is_program_word(tag) {
        odyssey_gl_program_word(s, tag);
        return;
    }
    if tag >= ODSY_REG_TEX_DETAIL_FUNCTION
        && tag < ODSY_REG_TEX_DETAIL_FUNCTION + ODSY_TEX_DETAIL_FUNCTION_BLOCKS
    {
        let block = tag - ODSY_REG_TEX_DETAIL_FUNCTION;

        if s.cmd_operand_count != ODSY_TEX_DETAIL_FUNCTION_BLOCK_WORDS {
            s.tex_detail_function_valid &= !(1u32 << block);
            odyssey_cmd_unimplemented(s, tag);
            return;
        }
        let w = ODSY_TEX_DETAIL_FUNCTION_BLOCK_WORDS as usize;
        let at = block as usize * w;
        s.tex_detail_function[at..at + w].copy_from_slice(&s.cmd_operand[..w]);
        s.tex_detail_function_valid |= 1u32 << block;
        return;
    }
    if tag >= ODSY_REG_TEX_TABLE3 && tag < ODSY_REG_TEX_TABLE3 + ODSY_TEX_TABLE3_BLOCKS {
        odyssey_cmd_unimplemented(s, tag);
        return;
    }
    if (tag >= ODSY_REG_TEX_COLOUR_TABLE0
        && tag < ODSY_REG_TEX_COLOUR_TABLE0 + ODSY_TEX_COLOUR_TABLE_BLOCKS)
        || (tag >= ODSY_REG_TEX_COLOUR_TABLE1
            && tag < ODSY_REG_TEX_COLOUR_TABLE1 + ODSY_TEX_COLOUR_TABLE_BLOCKS)
    {
        let plane = (tag >= ODSY_REG_TEX_COLOUR_TABLE1) as usize;
        let first_tag = if plane != 0 { ODSY_REG_TEX_COLOUR_TABLE1 } else { ODSY_REG_TEX_COLOUR_TABLE0 };
        let block = tag - first_tag;

        if s.cmd_operand_count != ODSY_TEX_COLOUR_TABLE_BLOCK_WORDS {
            odyssey_cmd_unimplemented(s, tag);
            return;
        }
        let w = ODSY_TEX_COLOUR_TABLE_BLOCK_WORDS as usize;
        let at = block as usize * w;
        s.tex_colour_table[plane][at..at + w].copy_from_slice(&s.cmd_operand[..w]);
        s.tex_colour_table_valid[plane] |= 1u32 << block;
        return;
    }
    if tag >= ODSY_REG_TEX_LEVEL && tag < ODSY_REG_TEX_LEVEL + SGI_ODYSSEY_TEX_LEVELS {
        let at = (tag - ODSY_REG_TEX_LEVEL) as usize * 3;
        s.tex_levels[at..at + 3].copy_from_slice(&s.cmd_operand[..3]);
        return;
    }
    if tag == ODSY_GL_TEXCOORD_3F || tag == ODSY_GL_TEXCOORD_4F {
        s.tex_coord.copy_from_slice(&s.cmd_operand[..2]);
        s.tex_coord_r = s.cmd_operand[2];
        s.tex_coord_q = if tag == ODSY_GL_TEXCOORD_4F { s.cmd_operand[3] } else { ODSY_F32_ONE };
        return;
    }
    if tag == ODSY_GL_TEXCOORD_1I
        || tag == ODSY_GL_TEXCOORD_2I
        || tag == ODSY_GL_TEXCOORD_3I
        || tag == ODSY_GL_TEXCOORD_4I
    {
        let n = odyssey_cmd_arity(tag);
        let mut c: [u32; 4] = [0, 0, 0, ODSY_F32_ONE];

        for i in 0..n as usize {
            c[i] = odyssey_f32_bits((s.cmd_operand[i] as i32) as f32);
        }
        s.tex_coord[0] = c[0];
        s.tex_coord[1] = c[1];
        s.tex_coord_r = c[2];
        s.tex_coord_q = c[3];
        return;
    }
    if (tag & ODSY_GL_TEXCOORD_SHAPE) == ODSY_GL_TEXCOORD_TAG && tag != ODSY_GL_TEXCOORD_2F {
        // Other two-coordinate forms remain observable and change nothing.
        odyssey_cmd_unimplemented(s, tag);
        return;
    }
    if (tag & ODSY_GL_MATRIX_SELECT_SHAPE) == ODSY_GL_MATRIX_SELECT_TAG {
        s.gl_seq[((ODSY_GL_MATRIX_SELECT_TAG >> ODSY_PROG_CLASS_SHIFT) - ODSY_PROG_SEQ_FIRST) as usize] =
            odyssey_prog_value(tag);
        // Pick the shared matrix slot the following load targets.
        s.gl_matrix_slot = match tag & 0xff {
            ODSY_GL_MATRIX_SELECT_COMBINED => ODSY_GL_SLOT_COMBINED,
            ODSY_GL_MATRIX_SELECT_TEXTURE => ODSY_GL_SLOT_TEXTURE,
            ODSY_GL_MATRIX_SELECT_MODELVIEW => SGI_GL_MODELVIEW,
            ODSY_GL_MATRIX_SELECT_BLOCK16 => ODSY_GL_SLOT_BLOCK16,
            ODSY_GL_MATRIX_SELECT_NORMAL => ODSY_GL_SLOT_NORMAL_MATRIX,
            // Any other block: no home.
            _ => ODSY_GL_SLOT_NORMAL,
        };
        return;
    }
    match tag {
        ODSY_GL_COLOR_3UB => {
            s.gl_color = odyssey_pack_pixel(s.cmd_operand[0], s.cmd_operand[1], s.cmd_operand[2]);
            s.gl_color_is_index = false;
            if !odyssey_gl_index_mode(s) {
                let mut colour = [0u32; SGI_GL_GET_WORDS];
                let mut n = 0u32;
                let opd = s.cmd_operand;

                odyssey_gl_feed(s, SGI_GL_OP_COLOR3UB as u16, &opd[..3]);
                let queried = sgi_gl_get(&s.gl_ctx, SGI_GL_CURRENT_COLOR, &mut colour, &mut n);
                assert!(queried && n as usize == s.gl_colour.len());
                let l = s.gl_colour.len();
                s.gl_colour.copy_from_slice(&colour[..l]);
                s.gl_colour_overridden = false;
            }
        }
        ODSY_GL_COLOR_INDEX | ODSY_GL_REPLAY_INDEX => {
            // The user's index word, or the kernel's replay of a saved one.
            s.gl_color_index = s.cmd_operand[0];
            s.gl_color_is_index = true;
        }
        ODSY_GL_COLOR_INDEX_F | ODSY_GL_REPLAY_INDEX_F => {
            s.gl_color_index = odyssey_float_index(s.cmd_operand[0]);
            s.gl_color_is_index = true;
        }
        ODSY_REG_DEST_SELECT => {
            // Destination-buffer select: its third operand marks an id-plane
            // write when it is ODSY_DEST_ID_PLANES. A colour select's non-zero
            // first operand names the colour buffer, the base a DMA header's
            // destination word is matched against (the all-zero blocks are GL
            // context setup, not a buffer).
            s.dest_buffer_word = s.cmd_operand[ODSY_DEST_SELECT_VALUE_INDEX];
            s.dest_base = s.cmd_operand[ODSY_DEST_SELECT_BASE_INDEX];
            if s.dest_base == 0 || odyssey_dest_is_id_planes(s) {
                return;
            }
            let base = s.dest_base;
            if !odyssey_board_address_valid(s, base) || (s.dest_buffer_word & !ODSY_DEST_FLAGS) == 0 {
                return;
            }
            if (s.dest_buffer_word >> ODSY_DEST_FORMAT_SHIFT) == ODSY_DEST_FORMAT_SCREEN
                || s.dest_base == s.dma_colour_base
            {
                s.dma_colour_base = s.dest_base;
            } else {
                // An off-screen object: create it, or update its row length.
                let width = s.cmd_operand[ODSY_DEST_WIDTH_INDEX].wrapping_mul(ODSY_DEST_WIDTH_UNIT);
                odyssey_object_select(s, base, width);
            }
        }
        ODSY_REG_PLANE_MASK => {
            // Plane write mask (mask << 12), consumed by an id-plane write.
            s.plane_write_mask = s.cmd_operand[0];
        }
        ODSY_MODE_INDEX_ZEROS
        | ODSY_GL_INDEX_MODE_COLOR_3I
        | ODSY_GL_INDEX_MODE_COLOR_3F
        | ODSY_GL_INDEX_MODE_COLOR_4F => {
            // An RGBA colour sent in index mode: GL does not draw with it.
        }
        ODSY_GL_VERTEX_2I => {
            if s.gl_begin_open {
                s.gl_integer_prim = true;
            }
            // Bit 13 distinguishes the observed GL and X11 integer streams.
            // Its hardware meaning is unknown. This is a model routing policy,
            // not a claim that the bit enables hardware vertex transforms.
            if s.gl_begin_open
                && !s.image_active
                && ((s.pattern_mode & (1u32 << 13)) != 0
                    || (s.tex_mode & (ODSY_TEX_MODE_1D | ODSY_TEX_MODE_2D | ODSY_TEX_MODE_3D)) != 0
                    || s.gl_primitive == ODSY_GL_TRIANGLES
                    || s.gl_primitive == ODSY_GL_TRIANGLE_STRIP
                    || s.gl_primitive == ODSY_GL_TRIANGLE_FAN
                    || s.gl_primitive == ODSY_GL_QUAD_STRIP)
            {
                // Lighting takes the signed integers as object coordinates.
                let xyz: [f64; 3] = [
                    (s.cmd_operand[0] as i32) as f64,
                    (s.cmd_operand[1] as i32) as f64,
                    0.0,
                ];
                let opd = s.cmd_operand;

                if !s.gl_in_prim {
                    let mode = s.gl_primitive;

                    odyssey_gl_feed(s, SGI_GL_OP_BEGIN as u16, &[mode]);
                    s.gl_in_prim = true;
                }
                odyssey_gl_vertex_colour(s, &xyz);
                odyssey_texture_vertex(s);
                odyssey_gl_feed(s, SGI_GL_OP_VERTEX2I as u16, &opd[..2]);
            } else {
                // The X path uses window-relative integer coordinates.
                let x = s.cmd_operand[0].wrapping_add(s.win_origin[0] as u32);
                let y = s.cmd_operand[1].wrapping_add(s.win_origin[1] as u32);
                odyssey_gl_vertex(s, x, y);
            }
        }
        ODSY_GL_COLOR_3F
        | ODSY_GL_COLOR_3I
        | ODSY_GL_COLOR_4F
        | ODSY_GL_COLOR_3US
        | ODSY_GL_COLOR_3UI
        | ODSY_GL_COLOR_3B
        | ODSY_GL_COLOR_3S
        | ODSY_GL_COLOR_4UB
        | ODSY_GL_COLOR_4US
        | ODSY_GL_COLOR_4UI
        | ODSY_GL_COLOR_4B
        | ODSY_GL_COLOR_4S
        | ODSY_GL_COLOR_4I => {
            // The current colour of the true-colour GL stream (glColor3f,
            // glColor4f and the integer forms), or the kernel's replay of one
            // odyssey_ctx_save() saved. Kept so an unlit vertex after lit ones
            // takes it back, and so colour material lights with it.
            let n = odyssey_cmd_arity(tag);
            let mut rgba = [0u32; 4];

            rgba.copy_from_slice(&s.cmd_operand[..4]);
            if tag != ODSY_GL_COLOR_3F && tag != ODSY_GL_COLOR_4F {
                for i in 0..n as usize {
                    rgba[i] = odyssey_integer_colour(tag, s.cmd_operand[i]);
                }
            }
            if n == 3 {
                rgba[3] = ODSY_F32_ONE;
            }
            s.gl_colour.copy_from_slice(&rgba);
            s.gl_colour_overridden = false;
            if n == 4 {
                odyssey_gl_feed(s, SGI_GL_OP_COLOR4F as u16, &rgba[..4]);
            } else {
                odyssey_gl_feed(s, SGI_GL_OP_COLOR3F as u16, &rgba[..3]);
            }
            if !odyssey_gl_index_mode(s) {
                s.gl_color = odyssey_pack_pixel(
                    odyssey_float_channel(rgba[0]),
                    odyssey_float_channel(rgba[1]),
                    odyssey_float_channel(rgba[2]),
                );
                s.gl_color_is_index = false;
            }
        }
        ODSY_GL_NORMAL_3B | ODSY_GL_NORMAL_3S | ODSY_GL_NORMAL_3I => {
            for i in 0..3 {
                s.gl_normal[i] = odyssey_signed_to_float(s.cmd_operand[i], tag & 0x3f);
            }
        }
        ODSY_GL_NORMAL_3F => {
            s.gl_normal.copy_from_slice(&s.cmd_operand[..3]);
        }
        ODSY_GL_MATERIAL_AMBIENT
        | ODSY_GL_MATERIAL_DIFFUSE
        | ODSY_GL_MATERIAL_SPECULAR
        | ODSY_GL_MATERIAL_EMISSION => {
            // The low byte counts 0x2f, 0x30, 0x31, 0x32 through the four.
            let at = (((tag & 0xff) - (ODSY_GL_MATERIAL_AMBIENT & 0xff)) * 4) as usize;
            s.gl_material[at..at + 4].copy_from_slice(&s.cmd_operand[..4]);
        }
        ODSY_GL_SHININESS_SQRT => {
            s.gl_shininess_sqrt = s.cmd_operand[0];
        }
        ODSY_GL_BACK_AMBIENT | ODSY_GL_BACK_DIFFUSE | ODSY_GL_BACK_SPECULAR | ODSY_GL_BACK_EMISSION => {
            let at = (((tag & 0xff) - (ODSY_GL_BACK_AMBIENT & 0xff)) * 4) as usize;
            s.gl_back_material[at..at + 4].copy_from_slice(&s.cmd_operand[..4]);
        }
        ODSY_GL_BACK_SHININESS_SQRT => {
            s.gl_back_shininess_sqrt = s.cmd_operand[0];
        }
        ODSY_GL_SHININESS_DERIVED | ODSY_PROG_RUN_ONCE => {
            // Consumed, not kept: the derived shininess floats and the one-shot
            // run have no modelled effect. Block 0x1ea (ODSY_GL_SHININESS_DERIVED)
            // is also the second read path of attribute slots 448 and 544: every
            // full context replay sends it with the selected slot-448 entry's
            // first operand, that with bit 31 flipped, and the same for slot 544
            // (ESTABLISHED as the kernel's replay). What the block does on the
            // board is UNKNOWN; the model gives neither use an effect.
        }
        ODSY_GL_CLIP_PLANE_LOAD => {
            // Public controls identify the effective S, T, R and Q planes.
            if s.gl_light_vec_sel == 0x10c {
                s.tex_linear_plane_q.copy_from_slice(&s.cmd_operand[..4]);
                s.tex_linear_valid |= 8;
                return;
            }
            if s.gl_light_vec_sel == 0x108 {
                s.tex_linear_plane_r.copy_from_slice(&s.cmd_operand[..4]);
                s.tex_linear_valid |= 4;
                return;
            }
            if s.gl_light_vec_sel == 0x100 || s.gl_light_vec_sel == 0x104 {
                let axis = ((s.gl_light_vec_sel - 0x100) / 4) as usize;

                s.tex_linear_plane[axis].copy_from_slice(&s.cmd_operand[..4]);
                s.tex_linear_valid |= 1u32 << axis;
                return;
            }
            // See ODSY_GL_CLIP_PLANE_BASE's comment: accept and pass through
            // loads into the shared layer's six slots, silently drop loads past
            // that budget (expected library traffic, not an error), and report
            // only a selector this model cannot place at all.
            if s.gl_light_vec_sel >= ODSY_GL_CLIP_PLANE_BASE
                && (s.gl_light_vec_sel - ODSY_GL_CLIP_PLANE_BASE) % ODSY_GL_CLIP_PLANE_STRIDE == 0
            {
                let plane = (s.gl_light_vec_sel - ODSY_GL_CLIP_PLANE_BASE) / ODSY_GL_CLIP_PLANE_STRIDE;

                if plane < SGI_GL_CLIP_PLANES {
                    let at = plane as usize * 4;
                    s.gl_clip_eye[at..at + 4].copy_from_slice(&s.cmd_operand[..4]);
                    odyssey_gl_apply_clip_plane(s, plane);
                }
            } else if !s.gl_block_logged {
                s.gl_block_logged = true;
                crate::qlog!(
                    "sgi-odyssey: class 0x31 clip-plane load at index {:#x} is not modeled",
                    s.gl_light_vec_sel
                );
            }
        }
        ODSY_GL_LIGHT_COLOUR_LOAD => {
            if s.gl_light_colour_sel < SGI_ODYSSEY_GL_LIGHT_COLOURS {
                let at = s.gl_light_colour_sel as usize * 4;
                s.gl_light_colour[at..at + 4].copy_from_slice(&s.cmd_operand[..4]);
            } else if !s.gl_block_logged {
                s.gl_block_logged = true;
                crate::qlog!(
                    "sgi-odyssey: class 0x32 load at index {:#x} is not modeled",
                    s.gl_light_colour_sel
                );
            }
        }
        ODSY_GL_LIGHT_VEC_LOAD => {
            if s.gl_light_vec_sel == 0x100 {
                s.tex_linear_plane[0].copy_from_slice(&s.cmd_operand[0..4]);
                s.tex_linear_plane[1].copy_from_slice(&s.cmd_operand[4..8]);
                s.tex_linear_plane_r.copy_from_slice(&s.cmd_operand[8..12]);
                s.tex_linear_plane_q.copy_from_slice(&s.cmd_operand[12..16]);
                s.tex_linear_valid |= 0xf;
                return;
            }
            // Index 16 i is light i. Other context setup blocks at 0x80
            // to 0x1e0 remain unknown and are reported once, not per index.
            if s.gl_light_vec_sel % ODSY_GL_LIGHT_VEC_FLOATS == 0
                && s.gl_light_vec_sel / ODSY_GL_LIGHT_VEC_FLOATS < SGI_ODYSSEY_GL_LIGHTS
            {
                let at = s.gl_light_vec_sel as usize;
                let n = ODSY_GL_LIGHT_VEC_FLOATS as usize;
                s.gl_light_vec[at..at + n].copy_from_slice(&s.cmd_operand[..n]);
            } else if !s.gl_block_logged {
                s.gl_block_logged = true;
                crate::qlog!(
                    "sgi-odyssey: class 0x31 load at index {:#x} is not modeled",
                    s.gl_light_vec_sel
                );
            }
        }
        ODSY_GL_VERTEX_2F | ODSY_GL_VERTEX_3F | ODSY_GL_VERTEX_3I | ODSY_GL_VERTEX_4F => {
            // A float vertex uses the shared GL path, and so does an integer 3D
            // vertex once converted to float (ODSY_GL_VERTEX_3I). Open the
            // primitive lazily on its first vertex, as for shared integer
            // geometry above. The open-primitive record keeps the word and its
            // operands as sent. glVertex4f (ODSY_GL_VERTEX_4F) passes its W on.
            let components: u32 = if tag == ODSY_GL_VERTEX_4F {
                4
            } else if tag == ODSY_GL_VERTEX_2F {
                2
            } else {
                3
            };
            let mut xyz = [0u32; 4];
            xyz.copy_from_slice(&s.cmd_operand[..4]);
            let mut obj = [0f64; 3];

            if tag == ODSY_GL_VERTEX_3I {
                for k in 0..3 {
                    xyz[k] = odyssey_f32_bits((s.cmd_operand[k] as i32) as f32);
                }
            }
            if !s.gl_in_prim && s.gl_restart_absorb == 0 {
                let mode = s.gl_primitive;

                odyssey_gl_feed(s, SGI_GL_OP_BEGIN as u16, &[mode]);
                s.gl_in_prim = true;
            }
            if s.gl_begin_open && !s.gl_integer_prim && s.gl_primitive <= 8 {
                odyssey_open_record_vertex(s, tag);
            }
            obj[0] = odyssey_f32(xyz[0]);
            obj[1] = odyssey_f32(xyz[1]);
            obj[2] = if components > 2 { odyssey_f32(xyz[2]) } else { 0.0 };
            if components == 4 && odyssey_f32(xyz[3]) != 0.0 {
                // (x, y, z, w) and (x/w, y/w, z/w, 1) are the same point.
                for k in 0..3 {
                    obj[k] /= odyssey_f32(xyz[3]);
                }
            }
            odyssey_gl_vertex_colour(s, &obj);
            odyssey_texture_vertex(s);
            if s.gl_restart_absorb != 0 {
                let slot = (s.gl_restart_total - s.gl_restart_absorb) as usize;

                if slot < s.gl_restart_vertex.len() {
                    sgi_gl_capture_vertex(
                        &mut s.gl_ctx,
                        s.gl_restart_type,
                        &xyz,
                        components,
                        &mut s.gl_restart_vertex[slot],
                    );
                }
                s.gl_restart_absorb -= 1;
                odyssey_restart_finish(s);
                return;
            }
            let op = if components == 4 {
                SGI_GL_OP_VERTEX4F
            } else if components == 3 {
                SGI_GL_OP_VERTEX3F
            } else {
                SGI_GL_OP_VERTEX2F
            };
            odyssey_gl_feed(s, op as u16, &xyz[..components as usize]);
        }
        ODSY_GL_LOAD_4F => {
            odyssey_gl_load_block16(s);
        }
        ODSY_REG_DEPTH_FUNC => {
            if (s.cmd_operand[0] & !ODSY_DEPTH_FUNC_MASK) != 0 {
                // Only bits 2:0 are known; every capture leaves the rest 0.
                crate::qlog!(
                    "sgi-odyssey: depth function word {:#x} has bits beyond 2:0, not modeled",
                    s.cmd_operand[0]
                );
            }
            s.gl_depth_func = s.cmd_operand[0];
            odyssey_gl_apply_depth(s);
        }
        ODSY_REG_WRITE_MASK => {
            let stencil_mask = (s.cmd_operand[0] >> ODSY_WRITE_STENCIL_SHIFT) & ODSY_WRITE_STENCIL_MASK;

            s.gl_write_mask = s.cmd_operand[0];
            odyssey_gl_apply_depth(s);
            // The library forces it to 0 for a drawable with no stencil.
            odyssey_gl_feed(s, SGI_GL_OP_STENCIL_MASK as u16, &[stencil_mask]);
        }
        ODSY_REG_ALPHA_FUNC => {
            let word = s.cmd_operand[0];
            let mut args = [0u32; 2];

            args[0] = SGI_GL_NEVER_ENUM + ((word >> ODSY_ALPHA_FUNC_SHIFT) & ODSY_ALPHA_FUNC_MASK);
            args[1] = odyssey_f32_bits((word & ODSY_ALPHA_REF_MASK) as f32 / ODSY_ALPHA_REF_SCALE);
            odyssey_gl_feed(s, SGI_GL_OP_ALPHA_FUNC as u16, &args);
        }
        ODSY_REG_STENCIL_FUNC => {
            let word = s.cmd_operand[0];
            let mut args = [0u32; 3];

            args[0] = SGI_GL_NEVER_ENUM + ((word >> ODSY_STENCIL_FUNC_SHIFT) & ODSY_STENCIL_FUNC_MASK);
            args[1] = word & ODSY_STENCIL_REF_MASK;
            args[2] = (word >> ODSY_STENCIL_VALUE_MASK_SHIFT) & ODSY_STENCIL_VALUE_MASK_MASK;
            odyssey_gl_feed(s, SGI_GL_OP_STENCIL_FUNC as u16, &args);

            let fail = odyssey_stencil_op((word >> ODSY_STENCIL_OP_FAIL_SHIFT) & ODSY_STENCIL_OP_MASK);
            let zfail = odyssey_stencil_op((word >> ODSY_STENCIL_OP_ZFAIL_SHIFT) & ODSY_STENCIL_OP_MASK);
            let zpass = odyssey_stencil_op((word >> ODSY_STENCIL_OP_ZPASS_SHIFT) & ODSY_STENCIL_OP_MASK);
            if fail == u32::MAX || zfail == u32::MAX || zpass == u32::MAX {
                crate::qlog!("sgi-odyssey: stencil op word {:#x} is not modeled", word);
            } else {
                args[0] = fail;
                args[1] = zfail;
                args[2] = zpass;
                odyssey_gl_feed(s, SGI_GL_OP_STENCIL_OP as u16, &args);
            }
        }
        ODSY_REG_BLEND => {
            let mut args = [0u32; 4];

            // Every operand is kept whatever bit 17 says (ODSY_MODE_BLEND): the
            // library sends glBlendFunc's factors, and the X server its
            // opaque-stipple background, before the 0x0404 that sets the bit.
            s.const_colour[0] = s.cmd_operand[1];
            s.const_colour[1] = s.cmd_operand[2];
            s.blend_word = s.cmd_operand[0];
            let word = s.cmd_operand[0];
            let eq = (word >> ODSY_BLEND_EQ_SHIFT) & ODSY_BLEND_EQ_MASK;
            let src = odyssey_blend_factor((word >> ODSY_BLEND_SRC_SHIFT) & ODSY_BLEND_SRC_MASK, false);
            let dst = odyssey_blend_factor(word & ODSY_BLEND_DST_MASK, true);
            if src == u32::MAX || dst == u32::MAX {
                crate::qlog!("sgi-odyssey: blend factor word {:#x} is not modeled", word);
            } else {
                args[0] = src;
                args[1] = dst;
                odyssey_gl_feed(s, SGI_GL_OP_BLEND_FUNC as u16, &args[..2]);
            }
            if eq == 3 || eq == 4 {
                crate::qlog!(
                    "sgi-odyssey: blend equation {} (the SGIX alpha min/max extension) is not modeled",
                    eq
                );
            } else {
                args[0] = odyssey_blend_equation[eq as usize];
                odyssey_gl_feed(s, SGI_GL_OP_BLEND_EQUATION as u16, &args[..1]);
            }
            let c1 = s.cmd_operand[1];
            let c2 = s.cmd_operand[2];
            args[0] = odyssey_f32_bits(((c1 >> ODSY_BLEND_COLOUR_SHIFT) & ODSY_BLEND_COLOUR_MASK) as f32 / ODSY_BLEND_COLOUR_SCALE);
            args[1] = odyssey_f32_bits((c1 & ODSY_BLEND_COLOUR_MASK) as f32 / ODSY_BLEND_COLOUR_SCALE);
            args[2] = odyssey_f32_bits(((c2 >> ODSY_BLEND_COLOUR_SHIFT) & ODSY_BLEND_COLOUR_MASK) as f32 / ODSY_BLEND_COLOUR_SCALE);
            args[3] = odyssey_f32_bits((c2 & ODSY_BLEND_COLOUR_MASK) as f32 / ODSY_BLEND_COLOUR_SCALE);
            odyssey_gl_feed(s, SGI_GL_OP_BLEND_COLOR as u16, &args);
        }
        ODSY_REG_LOGIC_OP => {
            let mode = SGI_GL_CLEAR + (s.cmd_operand[0] & ODSY_LOGIC_OP_MASK);

            odyssey_gl_feed(s, SGI_GL_OP_LOGIC_OP as u16, &[mode]);
        }
        ODSY_REG_LINE_POINT_SIZE => {
            let mut args = [0u32; 2];

            args[0] = odyssey_f32_bits(
                (s.cmd_operand[0] & ODSY_LINE_POINT_SIZE_MASK) as f32 / ODSY_LINE_POINT_SIZE_SCALE,
            );
            odyssey_gl_feed_raster_state(s, SGI_GL_OP_LINE_WIDTH as u16, &args[0..1]);
            args[1] = odyssey_f32_bits(
                (s.cmd_operand[1] & ODSY_LINE_POINT_SIZE_MASK) as f32 / ODSY_LINE_POINT_SIZE_SCALE,
            );
            odyssey_gl_feed_raster_state(s, SGI_GL_OP_POINT_SIZE as u16, &args[1..2]);
        }
        ODSY_REG_POLYGON_MODE => {
            let mut front = [0u32; 2];
            let mut back = [0u32; 2];

            front[0] = SGI_GL_FRONT;
            front[1] = odyssey_polygon_mode(
                (s.cmd_operand[0] >> ODSY_POLYGON_MODE_FRONT_SHIFT) & ODSY_POLYGON_MODE_MASK,
            );
            odyssey_gl_feed_raster_state(s, SGI_GL_OP_POLYGON_MODE as u16, &front);
            back[0] = SGI_GL_BACK;
            back[1] = odyssey_polygon_mode(
                (s.cmd_operand[0] >> ODSY_POLYGON_MODE_BACK_SHIFT) & ODSY_POLYGON_MODE_MASK,
            );
            odyssey_gl_feed_raster_state(s, SGI_GL_OP_POLYGON_MODE as u16, &back);
        }
        ODSY_REG_POLYGON_OFFSET => {
            let mut args = [0u32; 2];

            args[0] = odyssey_f32_bits(0.0f32);
            args[1] = odyssey_f32_bits(
                sextract32(s.cmd_operand[0], 0, ODSY_POLYGON_OFFSET_BITS) as f32 * ODSY_POLYGON_OFFSET_SCALE,
            );
            odyssey_gl_feed(s, SGI_GL_OP_POLYGON_OFFSET_EXT as u16, &args);
        }
        ODSY_REG_CLIP_BOX => {
            s.gl_clip_box[0] = s.cmd_operand[0];
            s.gl_clip_box[1] = s.cmd_operand[1];
            odyssey_gl_apply_clip_box(s);
        }
        ODSY_REG_FOG => {
            let n = s.gl_fog.len();
            s.gl_fog.copy_from_slice(&s.cmd_operand[..n]);
        }
        ODSY_REG_TEX_MODE => {
            s.tex_mode = s.cmd_operand[0];
            odyssey_gl_apply_clip_planes(s);
        }
        ODSY_REG_TEX_SAMPLER => {
            let n = s.tex_sampler.len();
            s.tex_sampler.copy_from_slice(&s.cmd_operand[..n]);
        }
        ODSY_REG_TEX_ENV => {
            let n = s.tex_env.len();
            s.tex_env.copy_from_slice(&s.cmd_operand[..n]);
        }
        ODSY_REG_TEX_LEVEL0 => {
            s.tex_level0 = s.cmd_operand[0];
        }
        ODSY_REG_TEX_LOD => {
            let n = s.tex_lod.len();
            s.tex_lod.copy_from_slice(&s.cmd_operand[..n]);
        }
        ODSY_REG_TEX_SCALE_BIAS => {
            let n = s.tex_scale_bias.len();
            s.tex_scale_bias.copy_from_slice(&s.cmd_operand[..n]);
        }
        ODSY_REG_TEX_OFFSET => {
            let n = s.tex_offset.len();
            s.tex_offset.copy_from_slice(&s.cmd_operand[..n]);
        }
        ODSY_REG_POST_FILTER_DEFAULT => {
            odyssey_cmd_unimplemented(s, tag);
        }
        ODSY_REG_FIFO_SETUP_FIRST..=ODSY_REG_FIFO_SETUP_LAST => {
            s.fifo_setup[(tag - ODSY_REG_FIFO_SETUP_FIRST) as usize] = s.cmd_operand[0];
        }
        ODSY_REG_FIFO_SETUP_7B5 => {
            s.fifo_setup[ODSY_FIFO_SETUP_7B5_INDEX] = s.cmd_operand[0];
        }
        ODSY_GL_TEXCOORD_1F => {
            s.tex_coord[0] = s.cmd_operand[0];
            s.tex_coord[1] = 0;
            s.tex_coord_r = 0;
            s.tex_coord_q = ODSY_F32_ONE;
        }
        ODSY_GL_TEXCOORD_2F => {
            s.tex_coord.copy_from_slice(&s.cmd_operand[..2]);
            s.tex_coord_r = 0;
            s.tex_coord_q = ODSY_F32_ONE;
        }
        ODSY_GL_TEX_RECT => {
            if !odyssey_texture_build_level(s) && !s.tex_rect_reported {
                s.tex_rect_reported = true;
                crate::qlog!(
                    "sgi-odyssey: textured rectangle into buffer {:#x} has unsupported construction state or missing backing",
                    s.dest_base
                );
            }
        }
        ODSY_REG_GL_BEGIN => {
            // Record the primitive and, for an image, open the raster path. The
            // shared GL path opens on a float vertex, an integer GL stream,
            // a textured integer vertex or an assembly absent from the 2D
            // interpreter. Integer X11 geometry retains the 2D path.
            s.gl_primitive = s.cmd_operand[0];
            s.gl_vertex_count = 0;
            s.gl_begin_open = true;
            s.gl_integer_prim = false;
            s.gl_open_r = 0;
            s.gl_open_c = 0;
            s.gl_open_m3 = 0;
            s.gl_open_m4 = 0;
            s.gl_open_held = 0;
            // A polygon left open by a BEGIN without END encloses nothing.
            odyssey_gl_polygon_close(s, false);
            s.image_active = s.cmd_operand[0] == ODSY_GL_PRIMITIVE_IMAGE;
            if s.image_active {
                s.image_x1 = 0;
                s.image_y1 = 0;
                s.image_x2 = 0;
                s.image_y2 = 0;
                s.image_cur_x = 0;
                s.image_cur_y = 0;
                s.image_fmt = 0;
                s.image_group_len = 0;
                s.image_group_painted = 0;
                s.image_bytes = 0;
                s.image_nbytes = 0;
            }
        }
        ODSY_REG_GL_END => {
            // A GL geometry primitive closes on the shared context; the greeter's
            // QUADS fill (and odyssey_putchar()'s stray END) close on the 2D path.
            s.gl_begin_open = false;
            s.gl_open_held = 0;
            if s.gl_in_prim {
                odyssey_gl_feed(s, SGI_GL_OP_END as u16, &[]);
                s.gl_in_prim = false;
            } else {
                odyssey_gl_end(s);
            }
            s.gl_integer_prim = false;
        }
        0x0400 => {
            // Pending primitive for context restore.
            s.gl_restart_type = s.cmd_operand[0];
        }
        0x0444 => {
            // Drop an outgoing primitive, without draw.
            if s.cmd_operand[0] == 0 && !s.gl_integer_prim && !(s.gl_begin_open && s.gl_primitive == 9) {
                sgi_gl_discard_primitive(&mut s.gl_ctx);
                s.gl_in_prim = false;
                s.gl_begin_open = false;
                s.gl_open_held = 0;
                s.gl_restart_absorb = 0;
                s.gl_restart_armed = false;
            } else if s.cmd_operand[0] != 0 {
                odyssey_cmd_unimplemented(s, 0x10444);
            }
        }
        0x0850 => {
            let type_ = s.cmd_operand[0] & 0xf;

            if type_ == 0xf {
                if !s.gl_integer_prim && !(s.gl_begin_open && s.gl_primitive == 9) {
                    sgi_gl_discard_primitive(&mut s.gl_ctx);
                    s.gl_in_prim = false;
                    s.gl_begin_open = false;
                    s.gl_open_held = 0;
                    s.gl_restart_absorb = 0;
                    s.gl_restart_armed = false;
                }
            } else if type_ <= 8 {
                s.gl_primitive = type_;
                s.gl_restart_type = type_;
                s.gl_restart_absorb = (s.cmd_operand[0] >> 19) & 3;
                s.gl_restart_total = s.gl_restart_absorb;
                s.gl_restart_armed = true;
                s.gl_restart_select = 0;
                s.gl_begin_open = true;
                s.gl_integer_prim = false;
                s.gl_open_r = 0;
                s.gl_open_c = 0;
                s.gl_open_m3 = 0;
                s.gl_open_m4 = 0;
                s.gl_open_held = 0;
            } else {
                odyssey_cmd_unimplemented(s, 0x10850);
            }
        }
        0x0740 => {
            s.gl_restart_select = s.cmd_operand[0];
            if s.gl_restart_select != 0x1e0 {
                odyssey_cmd_unimplemented(s, 0x740);
            }
        }
        0x0741 => {
            if s.gl_restart_select == 0x1e0 && s.gl_restart_armed {
                let word = s.cmd_operand[0];

                s.gl_restart_c = (word >> 21) & 7;
                s.gl_restart_m3 = (word >> 12) & 3;
                s.gl_restart_m4 = (word >> 14) & 3;
                odyssey_restart_finish(s);
            } else {
                odyssey_cmd_unimplemented(s, 0x741);
            }
        }
        ODSY_CMD_IMAGE_HEADER => {
            // The transfer header opens a pixel group: its count (bits 23:20, 15
            // standing for 16, 0 meaning a full burst) is how many byte-format
            // pixels or SHORT-format words the group's first burst carries; any
            // more words in that burst are padding, and the bursts after it are
            // full, running on to the box edge. Reset the per-group count and
            // any partial pixel.
            //
            // That the count covers only the first burst is INFERRED from the
            // X server's emitter: a colour-index image 56 pixels wide is sent
            // as a header with count 0 and three 16-pixel bursts, then for each
            // later group a header with count 8 and bit 16 set, an 8-pixel
            // burst finishing the previous row, and three 16-pixel bursts. A
            // count that bounded the whole group drew 8 pixels of every 56 and
            // left the root window's background in grey bands.
            let glen = (s.cmd_operand[0] >> ODSY_IMAGE_HEADER_GROUP_SHIFT) & ODSY_IMAGE_HEADER_GROUP_MASK;

            s.image_group_len = if glen == ODSY_IMAGE_HEADER_GROUP_MASK {
                ODSY_IMAGE_HEADER_GROUP_FULL
            } else {
                glen
            };
            s.image_group_painted = 0;
            s.image_code = s.cmd_operand[0] & ODSY_IMAGE_CODE_MASK;
            s.image_nbytes = 0;
            s.image_bytes = 0;
        }
        ODSY_REG_PATTERN_SHAPE => {
            s.pattern_shape = s.cmd_operand[0];
            odyssey_gl_apply_polygon_stipple(s);
        }
        ODSY_REG_PATTERN_MODE => {
            s.pattern_mode = s.cmd_operand[0];
            odyssey_gl_apply_mode(s);
        }
        ODSY_REG_WINDOW_ORIGIN => {
            s.win_origin[0] = sextract32(s.cmd_operand[0], 0, ODSY_ORIGIN_BITS);
            s.win_origin[1] = sextract32(s.cmd_operand[1], 0, ODSY_ORIGIN_BITS);
            if s.cmd_from_kernel {
                s.win_prior_origin = s.win_committed_origin;
                s.win_committed_origin = s.win_origin;
                s.win_origin_pending = true;
            }
        }
        ODSY_REG_ID_COMPARE => {
            s.id_compare = s.cmd_operand[1];
            if s.cmd_from_kernel && !after_tail {
                s.win_committed_id_compare = s.id_compare;
            }
            // A non-zero pair still ends a pending query (which clears the
            // operands), as when the word was undecoded: whether the board does
            // is unknown, and no traced query has one inside it.
            if s.cmd_operand[0] != 0 || s.cmd_operand[1] != 0 {
                odyssey_query_cancel(s);
            }
        }
        ODSY_REG_WINDOW_CLIP => {
            let n = s.clip_words.len();
            s.clip_words.copy_from_slice(&s.cmd_operand[..n]);
            if s.cmd_from_kernel
                && s.win_origin_pending
                && s.win_origin[0] == 0
                && s.win_origin[1] == 0
                && odyssey_clip_is_offscreen(s)
            {
                // The off-screen tail: keep the window pushed before it.
                s.win_committed_origin = s.win_prior_origin;
                s.win_after_tail = true;
            } else if s.cmd_from_kernel {
                s.win_committed_clip = s.clip_words;
            }
            if s.cmd_from_kernel {
                s.win_origin_pending = false;
            }
            odyssey_clip_update(s);
        }
        ODSY_CMD_WINDOW_RESTORE => {
            // The committed window back (ODSY_CMD_WINDOW_RESTORE; INFERRED).
            s.win_origin = s.win_committed_origin;
            s.clip_words = s.win_committed_clip;
            s.id_compare = s.win_committed_id_compare;
            odyssey_clip_update(s);
        }
        ODSY_REG_LINE_STIPPLE => {
            let n = s.line_stipple.len();
            s.line_stipple.copy_from_slice(&s.cmd_operand[..n]);
            odyssey_gl_apply_line_stipple(s);
            if s.line_stipple[ODSY_LINE_STIPPLE_CONTROL] != ODSY_LINE_STIPPLE_CONTROL_32
                && (s.line_stipple[ODSY_LINE_STIPPLE_CONTROL] & !0xffu32) != 0
                && !s.line_stipple_reported
            {
                s.line_stipple_reported = true;
                crate::qlog!(
                    "sgi-odyssey: line stipple control {:#x} has unclassified high bits",
                    s.line_stipple[ODSY_LINE_STIPPLE_CONTROL]
                );
            }
        }
        ODSY_REG_QUICK_FLUSH_ARG
        | ODSY_REG_STROBE
        | ODSY_REG_FULL_FLUSH_EXTRA
        | ODSY_REG_PIXEL_MODE_EXT
        | ODSY_REG_BLIT_PREFIX => {}
        ODSY_SWAP_SYNC_A => {
            // The bound drawing and reading drawable words, kept raw and
            // independently (ODSY_REG_DRAW_MASK): neither changes where the
            // model draws. The swap completes on the retrace hook
            // (odyssey_retrace()).
            s.draw_word = s.cmd_operand[0];
            crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_buffer_word: reg {:#x} word {:#x}", tag, s.draw_word);
        }
        ODSY_SWAP_SYNC_B => {
            s.read_word = s.cmd_operand[0];
            crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_buffer_word: reg {:#x} word {:#x}", tag, s.read_word);
        }
        ODSY_REG_READ_ORIGIN => {
            s.read_origin[0] = sextract32(s.cmd_operand[0], 0, 16);
            s.read_origin[1] = sextract32(s.cmd_operand[0], 16, 16);
        }
        0x0000060a | 0x0000060c..=ODSY_REG_DRAWABLE_LAST => {
            if !s.drawable_word_reported {
                s.drawable_word_reported = true;
                crate::qlog!(
                    "sgi-odyssey: drawable word {:#x} with operand {:#x} is consumed but not modeled",
                    tag,
                    s.cmd_operand[0]
                );
            }
        }
        ODSY_REG_DRAW_MASK => {
            // The drawing mask: which colour buffers the next writes reach.
            s.draw_mask = s.cmd_operand[0];
            crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_draw_mask: mask {:#x}", s.draw_mask);
            if (s.draw_mask & !ODSY_DRAW_MASK_BITS) != 0 && !s.draw_mask_high_reported {
                s.draw_mask_high_reported = true;
                crate::qlog!(
                    "sgi-odyssey: drawing mask {:#x} bits above 3 are not modeled; ignored",
                    s.draw_mask
                );
            }
            if (s.draw_mask & ODSY_DRAW_MASK_RIGHT) != 0 && !s.draw_mask_stereo_reported {
                s.draw_mask_stereo_reported = true;
                crate::qlog!(
                    "sgi-odyssey: stereo is not modeled; drawing mask {:#x} draws its right-eye buffers into the left-eye ones",
                    s.draw_mask
                );
            }
        }
        ODSY_REG_HOST_WRITE => {
            let op0 = s.cmd_operand[0];
            let op1 = s.cmd_operand[1];
            // The standalone host-write/sync command is the board's own DMA
            // completion signal, so it raises the same fence the DMA engine's
            // completion does, through the one fence implementation: op0's slot
            // and interrupt bit and op1's token become the completion cookie,
            // published to odyssey_publish() -> odyssey_fence(). op0's interrupt
            // bit and op1's token already sit at the fence word's own positions
            // (ODSY_HOST_WRITE_INTERRUPT is the fence word's bit 15).
            let word: u32 = ((op1 & 0xffff) << ODSY_DMA_FENCE_TOKEN_SHIFT)
                | (op0 & (ODSY_DMA_FENCE_INTERRUPT | ODSY_DMA_FENCE_SLOT_MASK));

            sgi_head_publish(
                &odyssey_head_ops,
                P::new(s).opaque(),
                0,
                SGI_DONE_WRITTEN,
                ODSY_FENCE_VALID | word as u64,
                0,
            );
        }
        ODSY_DMA_DESC_UPLOAD => {
            odyssey_dma_descriptor(s, false);
        }
        ODSY_DMA_DESC_READBACK => {
            odyssey_dma_descriptor(s, true);
        }
        _ => {
            // A recognized register-select word (valid shape, field-derived
            // arity) whose side effect this model does not track -- a mode, mask
            // or state register. Consumed like the flush and pixel-mode registers
            // above rather than treated as impossible.
        }
    }
}

pub fn odyssey_cmd_start(s: &mut SGIOdysseyState, tag: u32, operands: u32) {
    s.cmd_tag = tag;
    s.cmd_operand_count = operands;
    s.cmd_operand_index = 0;
    if operands == 0 {
        odyssey_query_update(s, tag);
        if tag != 0x2f00c02e && tag != 0x3000c02e {
            odyssey_cmd_dispatch(s, tag);
        }
    } else {
        s.cmd_state = ODSY_CMD_OPERANDS;
    }
}

// A register-select word (0x0001rrrr): the most common command word, since
// every flush and state write is one. No other word classify matches has
// this shape, so it is tested first rather than after every by-value match.
pub fn odyssey_cmd_classify_register(s: &mut SGIOdysseyState, word: u32) {
    let reg = word & !ODSY_CMD_REGISTER_SHAPE;
    let operands = odyssey_cmd_arity(reg);
    let mut fmt = 0u32;

    // A pixel burst while an image is open: its data words paint into the
    // box rather than streaming through the generic operand path. Outside
    // an image the same register-select is an ordinary state word.
    if s.image_active && operands > 0 && odyssey_image_burst_fmt(reg, &mut fmt) {
        s.image_fmt = fmt;
        if !odyssey_image_pairing_supported(s.image_code, fmt) {
            odyssey_cmd_unimplemented(s, word);
        }
        s.cmd_state = ODSY_CMD_IMAGE_BURST;
        s.cmd_operand_count = operands;
        s.cmd_operand_index = 0;
        return;
    }

    // The 32x32-bit pattern loads: a 16-word burst each, more than
    // cmd_operand[] holds, so they stream straight into the pattern buffer
    // (0x3c4e -> words 0-15, 0x3c4f -> words 16-31) rather than through the
    // generic operand path.
    if reg == ODSY_REG_PATTERN_LOW || reg == ODSY_REG_PATTERN_HIGH {
        s.cmd_state = ODSY_CMD_PATTERN_LOAD;
        s.cmd_operand_count = operands;
        s.cmd_operand_index = 0;
        s.pattern_load_offset = if reg == ODSY_REG_PATTERN_HIGH { ODSY_PATTERN_WORDS } else { 0 };
        return;
    }
    odyssey_cmd_start(s, reg, operands);
}

/// Classify a word arriving while the parser is idle: a new command.
pub fn odyssey_cmd_classify(s: &mut SGIOdysseyState, word: u32) {
    if (word & ODSY_CMD_REGISTER_SHAPE) == ODSY_CMD_REGISTER_TAG {
        odyssey_cmd_classify_register(s, word);
        return;
    }
    if word == 0x4c0 || word == 0x4c0 + ODSY_QUERY_KIND_RASTER {
        odyssey_query_begin(s);
        s.query_kind = word - 0x4c0;
        return;
    }
    if word == 0x2f00c02e || word == 0x3000c02e {
        odyssey_cmd_start(s, word, 0);
        return;
    }
    if word == 0x740 || word == 0x741 {
        odyssey_cmd_start(s, word, 1);
        return;
    }
    if (word >= ODSY_REG_FIFO_SETUP_FIRST && word <= ODSY_REG_FIFO_SETUP_LAST)
        || word == ODSY_REG_FIFO_SETUP_7B5
    {
        odyssey_cmd_start(s, word, 1);
        return;
    }

    if word == ODSY_CMD_COPY_RECT_TRIGGER {
        s.cmd_state = ODSY_CMD_COPY_RECT;
        s.cmd_operand_index = 0;
        return;
    }
    // A page-table DMA descriptor: the opcode word, then three more words the
    // generic operand path gathers before odyssey_cmd_dispatch() runs the
    // transfer (the two-doubleword descriptor reaches here as four words).
    if word == ODSY_DMA_DESC_UPLOAD || word == ODSY_DMA_DESC_READBACK {
        odyssey_cmd_start(s, word, ODSY_DMA_DESC_OPERANDS);
        return;
    }
    // The two buffer-select doublewords: each opcode word takes one operand
    // word. Consumed cleanly so the parser stays word-aligned; the swap
    // completion is delivered from the retrace hook, not here (see the
    // ODSY_SWAP_SYNC_* constants and odyssey_retrace()).
    if word == ODSY_SWAP_SYNC_A || word == ODSY_SWAP_SYNC_B {
        odyssey_cmd_start(s, word, ODSY_SWAP_SYNC_OPERANDS);
        return;
    }
    if word == ODSY_REG_READ_ORIGIN {
        odyssey_cmd_start(s, word, 1);
        return;
    }
    if word >= ODSY_REG_DRAWABLE_FIRST && word <= ODSY_REG_DRAWABLE_LAST {
        // 0x60a, 0x60c to 0x60f; the three known ones are matched above.
        odyssey_cmd_start(s, word, 1);
        return;
    }
    if word == ODSY_GL_COLOR_3UB {
        odyssey_cmd_start(s, word, 3);
        return;
    }
    if word == ODSY_GL_VERTEX_2I {
        odyssey_cmd_start(s, word, 2);
        return;
    }
    if word == ODSY_GL_TEXCOORD_1F
        || word == ODSY_GL_TEXCOORD_3F
        || word == ODSY_GL_TEXCOORD_4F
        || word == ODSY_GL_TEXCOORD_1I
        || word == ODSY_GL_TEXCOORD_3I
        || word == ODSY_GL_TEXCOORD_4I
        || (word & ODSY_GL_TEXCOORD_SHAPE) == ODSY_GL_TEXCOORD_TAG
        || word == ODSY_GL_TEX_RECT
    {
        odyssey_cmd_start(s, word, odyssey_cmd_arity(word));
        return;
    }
    if word == ODSY_CMD_WINDOW_RESTORE {
        odyssey_cmd_start(s, word, 0);
        return;
    }
    if word == ODSY_GL_COLOR_INDEX
        || word == ODSY_GL_REPLAY_INDEX
        || word == ODSY_GL_COLOR_INDEX_F
        || word == ODSY_GL_REPLAY_INDEX_F
        || word == ODSY_GL_INDEX_MODE_COLOR_3I
        || word == ODSY_GL_INDEX_MODE_COLOR_3F
        || word == ODSY_GL_INDEX_MODE_COLOR_4F
        || word == ODSY_GL_COLOR_3I
        || word == ODSY_GL_COLOR_4F
        || odyssey_is_integer_colour(word)
        || word == ODSY_GL_NORMAL_3B
        || word == ODSY_GL_NORMAL_3S
        || word == ODSY_GL_NORMAL_3I
    {
        odyssey_cmd_start(s, word, odyssey_cmd_arity(word));
        return;
    }
    // The true-colour GL words, matched by value: the float colour and vertex
    // siblings, the three matrix target-selects (0 operands each) and the
    // matrix load (16 floats, more than cmd_operand[] holds, so it streams into
    // gl_matrix[] through its own state).
    if word == ODSY_GL_COLOR_3F {
        odyssey_cmd_start(s, word, 3);
        return;
    }
    if word == ODSY_GL_VERTEX_2F {
        odyssey_cmd_start(s, word, 2);
        return;
    }
    if word == ODSY_GL_VERTEX_3F || word == ODSY_GL_VERTEX_3I {
        odyssey_cmd_start(s, word, 3);
        return;
    }
    if word == ODSY_GL_VERTEX_4F {
        odyssey_cmd_start(s, word, 4);
        return;
    }
    if word == ODSY_GL_LOAD_4F {
        odyssey_cmd_start(s, word, ODSY_GL_BLOCK16_FLOATS);
        return;
    }
    // The lighting words, matched by value; each carries its operand count
    // in bits 13:10 like every other word. The program words carry none and
    // are matched by class ahead of the image path's 0x29 state word, which
    // is the same class (odyssey_is_program_word()).
    for i in 0..odyssey_gl_light_words.len() {
        if word == odyssey_gl_light_words[i] {
            odyssey_cmd_start(s, word, odyssey_cmd_arity(word));
            return;
        }
    }
    if odyssey_is_program_word(word) {
        odyssey_cmd_start(s, word, 0);
        return;
    }
    if (word & ODSY_GL_MATRIX_SELECT_SHAPE) == ODSY_GL_MATRIX_SELECT_TAG {
        odyssey_cmd_start(s, word, 0);
        return;
    }
    if word == ODSY_GL_MATRIX_LOAD {
        // 16 floats, more than cmd_operand[] holds, so gathered in gl_matrix[].
        s.cmd_state = ODSY_CMD_MATRIX_LOAD;
        s.cmd_operand_count = ODSY_GL_MATRIX_FLOATS;
        s.cmd_operand_index = 0;
        return;
    }
    if word == ODSY_GL_RECT || word == ODSY_GL_CLEAR_BOX {
        // 10 floats, more than cmd_operand[] holds: gathered in gl_rect[].
        s.cmd_state = ODSY_CMD_GL_RECT;
        s.cmd_operand_count = ODSY_GL_RECT_FLOATS;
        s.cmd_operand_index = 0;
        return;
    }
    if word == ODSY_MODE_INDEX_ZEROS {
        odyssey_cmd_start(s, word, 3);
        return;
    }
    // The BEGIN-0xa image raster path's structural words, matched by value
    // rather than as register-selects. The raster-box floats need indices 4,
    // 5, 7 and 8 -- past what cmd_operand[] holds -- so they stream through a
    // dedicated state; the state word 0x29008000 | v and its teardown twin (0
    // operands), the transfer header (1) and its tail (0) run the generic
    // path, the header setting the group length as it dispatches.
    if word == ODSY_CMD_IMAGE_BOX {
        s.cmd_state = ODSY_CMD_IMAGE_BOX_LOAD;
        s.cmd_operand_count = odyssey_cmd_arity(word);
        s.cmd_operand_index = 0;
        return;
    }
    if (word & ODSY_CMD_STATE_WORD_MASK) == ODSY_CMD_STATE_WORD
        || word == ODSY_CMD_IMAGE_HEADER
        || word == ODSY_CMD_IMAGE_HEADER_TAIL
    {
        odyssey_cmd_start(s, word, odyssey_cmd_arity(word));
        return;
    }
    // A word this model does not know still carries its operand count in
    // bits 13:10 (odyssey_cmd_arity()), so pass over that many words and
    // keep the parser on the stream's command positions. The kernel's
    // context restore replays words of every class this way: 0x3cf8 to
    // 0x3cfb and 0x3f41 with 16 zero operands each, 0x0f41 with 3, 0x04b0
    // and 0xc480c426 with one, 0x2400800a with none. Taking their operands
    // as commands instead read zeros and floats as command words.
    //
    // A word of the GL classes (a non-zero top byte) with bit 23 clear takes
    // none, whatever its count field says: the restore's 0x9000bda2, whose
    // field reads 16, is followed at once by a flush, the window origin
    // (0x0841) and the clip list, which it would otherwise swallow, leaving
    // the next client drawing at the previous one's origin. Every bit-23-
    // clear GL word the model knows has a zero field and no operands, and
    // every one with operands has the bit set (0x9080bda2, 0x90808800).
    // INFERRED from that pattern; the bit's meaning on the board is UNKNOWN.
    if odyssey_cmd_arity(word) != 0 && ((word >> 24) == 0 || (word & ODSY_CMD_GL_OPERANDS) != 0) {
        let note = format!("; skipping {} operands", odyssey_cmd_arity(word));
        odyssey_cmd_report(s, word, &note);
        s.cmd_state = ODSY_CMD_SKIP;
        s.cmd_operand_count = odyssey_cmd_arity(word);
        s.cmd_operand_index = 0;
    } else if odyssey_cmd_arity(word) != 0 {
        odyssey_cmd_report(s, word, "; bit 23 clear, no operands");
    } else {
        odyssey_cmd_report(s, word, "; no operands");
    }
}

/// One command-FIFO word, wherever the parser currently is.
pub fn odyssey_cmd_word(s: &mut SGIOdysseyState, word: u32) {
    match s.query_stage {
        1 => {
            s.query_count = word;
            s.query_stage = 2;
            return;
        }
        2 => {
            if word == 0x10a5a {
                s.query_stage = 3;
                return;
            }
            s.query_stage = 0;
            crate::qlog!("sgi-odyssey: unpaired current-query sequence");
            odyssey_cmd_classify(s, word);
            return;
        }
        3 => {
            s.query_sync_index = word;
            s.query_stage = 4;
            return;
        }
        4 => {
            odyssey_query_finish(s, word);
            return;
        }
        _ => {}
    }
    match s.cmd_state {
        ODSY_CMD_SKIP => {
            s.cmd_operand_index += 1;
            if s.cmd_operand_index == s.cmd_operand_count {
                s.cmd_state = ODSY_CMD_IDLE;
            }
        }
        ODSY_CMD_OPERANDS => {
            // cmd_operand[] holds a whole burst; the bound is belt and braces.
            if (s.cmd_operand_index as usize) < s.cmd_operand.len() {
                s.cmd_operand[s.cmd_operand_index as usize] = word;
            }
            s.cmd_operand_index += 1;
            if s.cmd_operand_index == s.cmd_operand_count {
                let tag = s.cmd_tag;
                odyssey_query_update(s, tag);
                odyssey_cmd_dispatch(s, tag);
                s.cmd_state = ODSY_CMD_IDLE;
            }
        }
        ODSY_CMD_COPY_RECT => {
            s.cmd_operand[s.cmd_operand_index as usize] = word;
            match s.cmd_operand_index {
                ODSY_CMD_COPY_RECT_SRC_INDEX => s.copy_src = word,
                ODSY_CMD_COPY_RECT_SIZE_INDEX => s.copy_size = word,
                ODSY_CMD_COPY_RECT_DST_INDEX => s.copy_dst = word,
                _ => {}
            }
            s.cmd_operand_index += 1;
            if s.cmd_operand_index == ODSY_CMD_COPY_RECT_WORDS {
                odyssey_copy_rect(s);
                s.cmd_state = ODSY_CMD_IDLE;
            }
        }
        ODSY_CMD_PATTERN_LOAD => {
            // A 16-word burst into the pattern buffer at the load's own offset.
            if s.cmd_operand_index < ODSY_PATTERN_WORDS {
                s.pattern_buffer[(s.pattern_load_offset + s.cmd_operand_index) as usize] = word;
            }
            s.cmd_operand_index += 1;
            if s.cmd_operand_index == s.cmd_operand_count {
                odyssey_gl_apply_polygon_stipple(s);
                s.cmd_state = ODSY_CMD_IDLE;
            }
        }
        ODSY_CMD_IMAGE_BOX_LOAD => {
            // The ten raster-box floats. Only 4, 5, 7 and 8 carry the destination
            // corners; the rest are the zero the DDX's private was allocated with.
            // Set the cursor to the top-left corner once all ten have arrived.
            match s.cmd_operand_index {
                ODSY_IMAGE_BOX_X1_INDEX => s.image_x1 = odyssey_float_to_u32(word),
                ODSY_IMAGE_BOX_Y1_INDEX => s.image_y1 = odyssey_float_to_u32(word),
                ODSY_IMAGE_BOX_X2_INDEX => s.image_x2 = odyssey_float_to_u32(word),
                ODSY_IMAGE_BOX_Y2_INDEX => s.image_y2 = odyssey_float_to_u32(word),
                _ => {}
            }
            s.cmd_operand_index += 1;
            if s.cmd_operand_index == s.cmd_operand_count {
                s.image_cur_x = if s.image_x2 < s.image_x1 { s.image_x1.wrapping_sub(1) } else { s.image_x1 };
                s.image_cur_y = if s.image_y2 < s.image_y1 { s.image_y1.wrapping_sub(1) } else { s.image_y1 };
                s.cmd_state = ODSY_CMD_IDLE;
            }
        }
        ODSY_CMD_IMAGE_BURST => {
            // Each data word paints into the open image at the raster cursor.
            odyssey_image_burst_word(s, word);
            s.cmd_operand_index += 1;
            if s.cmd_operand_index == s.cmd_operand_count {
                // The header's count bounds only the group's first burst; the
                // bursts after it are full (ODSY_CMD_IMAGE_HEADER).
                s.image_group_len = 0;
                s.cmd_state = ODSY_CMD_IDLE;
            }
        }
        ODSY_CMD_MATRIX_LOAD => {
            // A board-side matrix: 16 floats, loaded once all have arrived.
            if s.cmd_operand_index < ODSY_GL_MATRIX_FLOATS {
                s.gl_matrix[s.cmd_operand_index as usize] = word;
            }
            s.cmd_operand_index += 1;
            if s.cmd_operand_index == s.cmd_operand_count {
                odyssey_gl_load_matrix(s);
                s.cmd_state = ODSY_CMD_IDLE;
            }
        }
        ODSY_CMD_GL_RECT => {
            // The rectangle's colour and corners, painted once all have arrived.
            s.gl_rect[s.cmd_operand_index as usize] = word;
            s.cmd_operand_index += 1;
            if s.cmd_operand_index == s.cmd_operand_count {
                odyssey_gl_rect(s);
                s.cmd_state = ODSY_CMD_IDLE;
            }
        }
        _ => {
            odyssey_cmd_classify(s, word);
        }
    }
}

/// The command FIFO accepts both 32-bit and 64-bit stores. An 8-byte store
/// carries two sequential 32-bit command words; which half is logically
/// first is not independently established by any allowed source, so it is
/// modeled by analogy to the backend-selector port's own
/// (selector << 32) | data convention and this device's big-endian byte
/// order, upper half first. Other widths are not established for this
/// port, so they are reported unimplemented rather than fed into the
/// command stream and risking desync.
pub fn odyssey_cmd_fifo_write(s: &mut SGIOdysseyState, value: u64, size: u32) {
    match size {
        4 => odyssey_cmd_word(s, value as u32),
        8 => {
            odyssey_cmd_word(s, (value >> 32) as u32);
            odyssey_cmd_word(s, value as u32);
        }
        _ => odyssey_unimplemented(s, ODSY_CMD_FIFO, size, true),
    }
}

/// One step of the command-FIFO self-test preamble.
pub struct OdysseyCfifoPreamble {
    pub port: u64,
    pub size: u32,
    pub value: u64,
}

/// The kernel driver's command-FIFO initialisation self-test preamble, in
/// order, all through the command FIFO: three 4-byte command words, five
/// 8-byte pushes (a 32-bit selector in the upper half, data in the lower),
/// and one closing 4-byte word. Seeing the whole sequence makes
/// ODSY_CFIFO_SELFTEST answer OK. (The real driver sends the five 8-byte
/// words to 0x110000, not to the backend-select port, confirmed by a boot
/// trace.)
pub static odyssey_cfifo_preamble: [OdysseyCfifoPreamble; 9] = [
    OdysseyCfifoPreamble { port: ODSY_CMD_FIFO, size: 4, value: 0x00010854 },
    OdysseyCfifoPreamble { port: ODSY_CMD_FIFO, size: 4, value: 0x00000000 },
    OdysseyCfifoPreamble { port: ODSY_CMD_FIFO, size: 4, value: 0x0fffffff },
    OdysseyCfifoPreamble { port: ODSY_CMD_FIFO, size: 8, value: 0x000006b000000000 },
    OdysseyCfifoPreamble { port: ODSY_CMD_FIFO, size: 8, value: 0x000006b100000100 },
    OdysseyCfifoPreamble { port: ODSY_CMD_FIFO, size: 8, value: 0x000006b300005e50 },
    OdysseyCfifoPreamble { port: ODSY_CMD_FIFO, size: 8, value: 0x000006b200007dc0 },
    OdysseyCfifoPreamble { port: ODSY_CMD_FIFO, size: 8, value: 0x000007b500000001 },
    OdysseyCfifoPreamble { port: ODSY_CMD_FIFO, size: 4, value: 0x00010046 },
];

pub fn odyssey_selftest_track(s: &mut SGIOdysseyState, port: u64, size: u32, value: u64) {
    let next = s.selftest_progress as usize;

    if next < odyssey_cfifo_preamble.len()
        && odyssey_cfifo_preamble[next].port == port
        && odyssey_cfifo_preamble[next].size == size
        && odyssey_cfifo_preamble[next].value == value
    {
        s.selftest_progress = next as u32 + 1;
        if s.selftest_progress as usize == odyssey_cfifo_preamble.len() {
            s.cfifo_selftest_ready = true;
            s.selftest_progress = 0;
        }
        return;
    }
    // A break in the sequence restarts it, counting a fresh first word.
    s.selftest_progress = if odyssey_cfifo_preamble[0].port == port
        && odyssey_cfifo_preamble[0].size == size
        && odyssey_cfifo_preamble[0].value == value
    {
        1
    } else {
        0
    };
}
