//! Ported from hw/misc/sgi-odyssey.c in jamesbraid/qemu-sgi-origami.

use super::*;

/// The clip-rectangle runs of one row (odyssey_row_runs).
#[derive(Clone, Copy)]
pub struct OdysseyRowRuns {
    pub n: u32,
    pub start: [i64; SGI_ODYSSEY_CLIP_RECTS as usize],
    pub end: [i64; SGI_ODYSSEY_CLIP_RECTS as usize],
}

impl Default for OdysseyRowRuns {
    fn default() -> Self {
        OdysseyRowRuns { n: 0, start: [0; SGI_ODYSSEY_CLIP_RECTS as usize], end: [0; SGI_ODYSSEY_CLIP_RECTS as usize] }
    }
}

// Anonymous enum: current-value query kinds.
pub const ODSY_QUERY_INDEX: u32 = 0;
pub const ODSY_QUERY_COLOUR: u32 = 1;
pub const ODSY_QUERY_NORMAL: u32 = 2;
pub const ODSY_QUERY_TEXCOORD: u32 = 3;
pub const ODSY_QUERY_EDGE: u32 = 4;

pub fn odyssey_back_material_reset(s: &mut SGIOdysseyState) {
    s.gl_back_material.copy_from_slice(&odyssey_back_material_defaults);
    s.gl_back_shininess_sqrt = 0;
}

pub fn odyssey_shadow_put(s: &mut SGIOdysseyState, base: u64, off: u32, word: u32) {
    let at = base.wrapping_add(off as u64);
    let res = address_space_stl_be(&address_space_memory(), at, word);

    if res != MEMTX_OK && !s.ctx_shadow_fail_reported {
        s.ctx_shadow_fail_reported = true;
        crate::qlog!("sgi-odyssey: context shadow store to {:#x} failed", at);
    }
}

/// One selected ring entry: attribute a's word and its raw operands.
pub fn odyssey_shadow_ring(s: &mut SGIOdysseyState, base: u64, a: u32, slot: u32, word: u32, ops: &[u32]) {
    odyssey_shadow_put(s, base, ODSY_HW_RING_WORD + ODSY_HW_RING_WORD_STRIDE * a + 4 * slot, word);
    for (i, op) in ops.iter().enumerate() {
        odyssey_shadow_put(
            s,
            base,
            ODSY_HW_RING_OPERANDS + ODSY_HW_RING_OPERAND_STRIDE * a + 16 * slot + 4 * i as u32,
            *op,
        );
    }
}

/// One two-word descriptor of an open-primitive save. Attribute A's 2-bit
/// ring-slot selector is at bit 2 * (16 - A) of the first word for A 1..16
/// and at bit 2 * (32 - A) of the second for A 17 and 18. An attribute the
/// model keeps no record of (record[A][0] zero) selects slot 0, which this
/// save never writes for it, so the replay sends back what that slot already
/// holds. This is an approximation: the board's values for these attributes,
/// and the slots it saves them in, are unknown.
pub fn odyssey_shadow_descriptor(s: &mut SGIOdysseyState, base: u64, off: u32, record: &[[u32; 5]; 19], slot: u32) {
    let mut word = [0u32; 2];

    for a in 1..19usize {
        if record[a][0] == 0 {
            continue;
        }
        if a <= 16 {
            word[0] |= slot << (2 * (16 - a));
        } else {
            word[1] |= slot << (2 * (32 - a));
        }
    }
    odyssey_shadow_put(s, base, off, word[0]);
    odyssey_shadow_put(s, base, off + 4, word[1]);
}

pub fn odyssey_open_current_records(s: &mut SGIOdysseyState, record: &mut [[u32; 5]; 19]) {
    const material: [u32; 4] = [
        ODSY_GL_MATERIAL_AMBIENT,
        ODSY_GL_MATERIAL_DIFFUSE,
        ODSY_GL_MATERIAL_SPECULAR,
        ODSY_GL_MATERIAL_EMISSION,
    ];
    const back_material: [u32; 4] = [ODSY_GL_BACK_AMBIENT, ODSY_GL_BACK_DIFFUSE, ODSY_GL_BACK_SPECULAR, ODSY_GL_BACK_EMISSION];

    *record = [[0u32; 5]; 19];
    record[ODSY_HW_ATTR_INDEX] = s.query_current[ODSY_QUERY_INDEX as usize];
    record[ODSY_HW_ATTR_COLOUR] = s.query_current[ODSY_QUERY_COLOUR as usize];
    record[ODSY_HW_ATTR_NORMAL] = s.query_current[ODSY_QUERY_NORMAL as usize];
    record[ODSY_HW_ATTR_TEXTURE] = s.query_current[ODSY_QUERY_TEXCOORD as usize];
    record[ODSY_HW_ATTR_EDGE] = s.query_current[ODSY_QUERY_EDGE as usize];
    for i in 0..4usize {
        record[ODSY_HW_ATTR_MATERIAL + i][0] = material[i];
        record[ODSY_HW_ATTR_MATERIAL + i][1..5].copy_from_slice(&s.gl_material[4 * i..4 * i + 4]);
        record[ODSY_HW_ATTR_BACK_MATERIAL + i][0] = back_material[i];
        record[ODSY_HW_ATTR_BACK_MATERIAL + i][1..5].copy_from_slice(&s.gl_back_material[4 * i..4 * i + 4]);
    }
    record[ODSY_HW_ATTR_SHININESS][0] = ODSY_GL_SHININESS_SQRT;
    record[ODSY_HW_ATTR_SHININESS][1] = s.gl_shininess_sqrt;
    record[ODSY_HW_ATTR_BACK_SHININESS][0] = ODSY_GL_BACK_SHININESS_SQRT;
    record[ODSY_HW_ATTR_BACK_SHININESS][1] = s.gl_back_shininess_sqrt;
}

pub fn odyssey_open_record_vertex(s: &mut SGIOdysseyState, word: u32) {
    let mut v = SGIOdysseyOpenVertex { word, slot: s.gl_open_r, ..Default::default() };
    let mut n = s.gl_open_held as usize;
    let arity = (odyssey_cmd_arity(word) as usize).min(4);

    v.operands[..arity].copy_from_slice(&s.cmd_operand[..arity]);
    odyssey_open_current_records(s, &mut v.attribute);
    if s.gl_primitive == 2 || s.gl_primitive == 6 {
        s.gl_open_r = if s.gl_open_r == 3 { 1 } else { s.gl_open_r + 1 };
    } else {
        s.gl_open_r = (s.gl_open_r + 1) & 3;
    }
    s.gl_open_c = (s.gl_open_c + 1).min(7);
    s.gl_open_m3 = (s.gl_open_m3 + 1) % 3;
    s.gl_open_m4 = (s.gl_open_m4 + 1) & 3;

    match s.gl_primitive {
        0 => {
            // Points retain no vertices.
            n = 0;
        }
        1 => {
            // Lines retain one of each pair.
            if n != 0 {
                n = 0;
            } else {
                s.gl_open_vertex[0] = v;
                n = 1;
            }
        }
        2 | 6 => {
            // Anchor and latest vertex.
            s.gl_open_vertex[if n != 0 { 1 } else { 0 }] = v;
            n = (n + 1).min(2);
        }
        3 => {
            // Line strip.
            s.gl_open_vertex[0] = v;
            n = 1;
        }
        4 | 7 => {
            // Triangles and quads.
            if n == (if s.gl_primitive == 4 { 2 } else { 3 }) {
                n = 0;
            } else {
                s.gl_open_vertex[n] = v;
                n += 1;
            }
        }
        5 => {
            // Triangle strip.
            if n == 2 {
                s.gl_open_vertex[0] = s.gl_open_vertex[1];
                s.gl_open_vertex[1] = v;
            } else {
                s.gl_open_vertex[n] = v;
                n += 1;
            }
        }
        8 => {
            // Quad strip.
            if n == 3 {
                s.gl_open_vertex[0] = s.gl_open_vertex[2];
                s.gl_open_vertex[1] = v;
                n = 2;
            } else {
                s.gl_open_vertex[n] = v;
                n += 1;
            }
        }
        _ => {}
    }
    s.gl_open_held = n as u32;
}

pub fn odyssey_restart_finish(s: &mut SGIOdysseyState) {
    let n = s.gl_open_held;
    let mut count;

    if !s.gl_restart_armed || s.gl_restart_absorb != 0 {
        return;
    }
    if s.gl_restart_m3 >= 3 || n != s.gl_restart_total {
        odyssey_cmd_unimplemented(s, 0x741);
        s.gl_restart_armed = false;
        return;
    }
    s.gl_open_c = if s.gl_restart_c == 7 { 7 } else { (s.gl_restart_c + n).min(7) };
    s.gl_open_m3 = (s.gl_restart_m3 + n) % 3;
    s.gl_open_m4 = (s.gl_restart_m4 + n) & 3;
    count = s.gl_open_c;
    if count == 7 {
        while (count % 3 != s.gl_open_m3 || count % 4 != s.gl_open_m4) && count < 19 {
            count += 1;
        }
    }
    s.gl_in_prim = sgi_gl_resume_primitive(&mut s.gl_ctx, s.gl_restart_type, Some(&s.gl_restart_vertex[..]), n, count);
    s.gl_restart_armed = false;
}

/// The board's save of the outgoing context's geometry-engine state into the
/// hardware shadow, before the completion word. Register state needs no save:
/// the kernel re-pushes it from the owner's software shadow on every
/// switch-in, so the model keeps its single copy. What the kernel cannot
/// restore itself is the current vertex attributes, which it replays from this
/// page. Each record goes out as the last command word it retained with its
/// raw operands; nothing is converted between a setter, this save and a later
/// query. This is an approximation: the physical board's saved words and
/// operand encodings are unknown. Every save writes all ten descriptor words.
/// Idle selects ring slot 0. A float primitive of type 0..8 packs its retained
/// raw attribute records into ring slots 0..N-1 at save time, and current
/// records into slot 3. Integer 2D, type 10 and type 9 polygon assembly still
/// report an unsupported switch and save idle. This producer and its restart
/// decode are model approximations, not established board behaviour.
pub fn odyssey_ctx_save(s: &mut SGIOdysseyState, base: u64) {
    const material: [u32; 4] = [
        ODSY_GL_MATERIAL_AMBIENT,
        ODSY_GL_MATERIAL_DIFFUSE,
        ODSY_GL_MATERIAL_SPECULAR,
        ODSY_GL_MATERIAL_EMISSION,
    ];
    const back_material: [u32; 4] = [ODSY_GL_BACK_AMBIENT, ODSY_GL_BACK_DIFFUSE, ODSY_GL_BACK_SPECULAR, ODSY_GL_BACK_EMISSION];
    let index = s.query_current[ODSY_QUERY_INDEX as usize];
    let colour = s.query_current[ODSY_QUERY_COLOUR as usize];
    let normal = s.query_current[ODSY_QUERY_NORMAL as usize];
    let edge = s.query_current[ODSY_QUERY_EDGE as usize][0];
    let open = s.gl_begin_open && !s.gl_integer_prim && s.gl_primitive <= 8;
    let ar = |w: u32| (odyssey_cmd_arity(w) as usize).min(4);

    if s.gl_begin_open && !open && !s.ctx_open_prim_logged {
        s.ctx_open_prim_logged = true;
        crate::qlog!(
            "sgi-odyssey: context switch inside primitive {:#x}; its buffered vertices are not saved",
            s.gl_primitive
        );
    }
    odyssey_shadow_put(s, base, ODSY_HW_PIPE_STATE, ODSY_HW_PIPE_IDLE);
    for i in 0..ODSY_HW_DESCRIPTOR_WORDS {
        odyssey_shadow_put(s, base, ODSY_HW_CURRENT_ATTRIBUTES + 4 * i, 0);
    }
    odyssey_shadow_put(s, base, ODSY_HW_INDEX_WORD, index[0]);
    for i in 0..odyssey_cmd_arity(index[0]) {
        odyssey_shadow_put(s, base, ODSY_HW_INDEX_OPERANDS + 4 * i, index[1 + (i as usize).min(3)]);
    }
    odyssey_shadow_put(s, base, ODSY_HW_COLOUR_WORD, colour[0]);
    for i in 0..odyssey_cmd_arity(colour[0]) {
        odyssey_shadow_put(s, base, ODSY_HW_COLOUR_OPERANDS + 4 * i, colour[1 + (i as usize).min(3)]);
    }
    odyssey_shadow_ring(s, base, ODSY_HW_ATTR_INDEX as u32, 0, index[0], &index[1..1 + ar(index[0])]);
    odyssey_shadow_ring(s, base, ODSY_HW_ATTR_COLOUR as u32, 0, colour[0], &colour[1..1 + ar(colour[0])]);
    odyssey_shadow_ring(s, base, ODSY_HW_ATTR_NORMAL as u32, 0, normal[0], &normal[1..1 + ar(normal[0])]);
    odyssey_shadow_ring(s, base, ODSY_HW_ATTR_EDGE as u32, 0, edge, &[]);
    if s.tex_coord_q != ODSY_F32_ONE {
        let coordinate = [s.tex_coord[0], s.tex_coord[1], s.tex_coord_r, s.tex_coord_q];

        odyssey_shadow_ring(s, base, ODSY_HW_ATTR_TEXTURE as u32, 0, ODSY_GL_TEXCOORD_4F, &coordinate);
    } else {
        let three = (s.tex_mode & ODSY_TEX_MODE_3D) != 0 || s.tex_coord_r != 0;
        let three_ops = [s.tex_coord[0], s.tex_coord[1], s.tex_coord_r];
        let two_ops = s.tex_coord;

        odyssey_shadow_ring(
            s,
            base,
            ODSY_HW_ATTR_TEXTURE as u32,
            0,
            if three { ODSY_GL_TEXCOORD_3F } else { ODSY_GL_TEXCOORD_2F },
            if three { &three_ops[..] } else { &two_ops[..] },
        );
    }
    for i in 0..4usize {
        let m: [u32; 4] = s.gl_material[i * 4..i * 4 + 4].try_into().unwrap();

        odyssey_shadow_ring(s, base, (ODSY_HW_ATTR_MATERIAL + i) as u32, 0, material[i], &m);
    }
    let sh = [s.gl_shininess_sqrt];
    odyssey_shadow_ring(s, base, ODSY_HW_ATTR_SHININESS as u32, 0, ODSY_GL_SHININESS_SQRT, &sh);
    for i in 0..4usize {
        let m: [u32; 4] = s.gl_back_material[i * 4..i * 4 + 4].try_into().unwrap();

        odyssey_shadow_ring(s, base, (ODSY_HW_ATTR_BACK_MATERIAL + i) as u32, 0, back_material[i], &m);
    }
    let bsh = [s.gl_back_shininess_sqrt];
    odyssey_shadow_ring(s, base, ODSY_HW_ATTR_BACK_SHININESS as u32, 0, ODSY_GL_BACK_SHININESS_SQRT, &bsh);
    if open {
        let mut current = [[0u32; 5]; 19];
        let mut state = s.gl_primitive
            | (s.gl_open_held << 4)
            | (s.gl_open_r << 6)
            | (s.gl_open_m3 << 12)
            | (s.gl_open_m4 << 14)
            | (s.gl_open_c << 21);

        // After c >= 2 vertices the next triangle is number c - 2.
        if s.gl_primitive == ODSY_GL_TRIANGLE_STRIP && s.gl_open_c >= 2 && (s.gl_open_m4 & 1) != 0 {
            state |= ODSY_HW_PIPE_STRIP_ODD;
        }

        odyssey_open_current_records(s, &mut current);
        odyssey_shadow_put(s, base, ODSY_HW_PIPE_STATE, state);
        for i in 0..s.gl_open_held.min(3) {
            let v = s.gl_open_vertex[i as usize];
            let slot = v.slot;

            odyssey_shadow_ring(s, base, 0, slot, v.word, &v.operands[..ar(v.word)]);
            odyssey_shadow_descriptor(s, base, 0x8c + 8 * slot, &v.attribute, i);
            for a in 1..19usize {
                if v.attribute[a][0] == 0 {
                    continue;
                }
                odyssey_shadow_ring(
                    s,
                    base,
                    a as u32,
                    i,
                    v.attribute[a][0],
                    &v.attribute[a][1..1 + ar(v.attribute[a][0])],
                );
            }
        }
        odyssey_shadow_descriptor(s, base, ODSY_HW_CURRENT_ATTRIBUTES, &current, 3);
        for i in 0..19usize {
            if current[i][0] == 0 {
                continue;
            }
            odyssey_shadow_ring(s, base, i as u32, 3, current[i][0], &current[i][1..1 + ar(current[i][0])]);
        }
    }
}

/// The kernel driver's context switch: a single 8-byte store to
/// ODSY_CTX_SWITCH_PORT, not a command-FIFO word. The new context id in bits
/// 31:16 is not modelled: the board learns the context from the shadow address
/// pair, which names where the outgoing state is saved (odyssey_ctx_save()),
/// and the kernel restores the rest. A store naming context 0x7f is the
/// kernel driver's shadow snapshot and saves the same way. Any other shape at
/// this port has no established meaning and is reported unimplemented instead
/// of guessed.
pub fn odyssey_ctx_switch(s: &mut SGIOdysseyState, value: u64) {
    if ((value >> ODSY_CTX_SWITCH_OPCODE_SHIFT) & 0xff) != ODSY_CTX_SWITCH_OPCODE as u64
        || (value & ODSY_CTX_SWITCH_VALID) == 0
    {
        odyssey_unimplemented(s, ODSY_CTX_SWITCH_PORT, 8, true);
        return;
    }
    if s.ctx_shadow_addr_upper == 0 && s.ctx_shadow_addr_lower == 0 {
        crate::qlog!(
            "sgi-odyssey: context switch ({:#x}) with no shadow address programmed; no completion written",
            value
        );
        return;
    }
    odyssey_query_cancel(s);
    let shadow_base = (((s.ctx_shadow_addr_upper & 0xffff) as u64) << 32) | s.ctx_shadow_addr_lower as u64;
    // A plain guest-physical address: interrupt packets go elsewhere.
    odyssey_ctx_save(s, shadow_base);
    odyssey_shadow_put(s, shadow_base, ODSY_CTX_SWITCH_DONE_OFFSET as u32, 1);
}

/// 2D fills carry no alpha channel in this grammar; the framebuffer's fourth
/// byte is modelled fully opaque (the colour plane's red-high R,G,B,A packing,
/// resolved by odyssey_resolve_row() below).
pub fn odyssey_pack_pixel(r: u32, g: u32, b: u32) -> u32 {
    (r << 24) | (g << 16) | (b << 8) | 0xff
}

/// Colour buffer b's board memory and plane (odyssey_realize()), b being 0 or
/// 1 in the displayed-buffer bits' sense (ODSY_REG_DRAW_MASK).
pub fn odyssey_buffer_mem(s: &mut SGIOdysseyState, b: u32) -> P<SGIBoardMemory> {
    if b != 0 { P::new(&mut s.buffer1_mem) } else { P::new(&mut s.mem) }
}

pub fn odyssey_buffer_plane(s: &mut SGIOdysseyState, b: u32) -> P<SGIPlane> {
    if b != 0 {
        sgi_board_memory_plane(&s.buffer1_mem, s.buffer1_plane)
    } else {
        sgi_board_memory_plane(&s.mem, s.colour_plane)
    }
}

pub fn odyssey_buffer_load(s: &mut SGIOdysseyState, b: u32, x: i64, y: i64) -> u32 {
    let m = odyssey_buffer_mem(s, b);
    let p = odyssey_buffer_plane(s, b);

    sgi_plane_load(&m, &p, x as i32, y as i32)
}

pub fn odyssey_buffer_store(s: &mut SGIOdysseyState, b: u32, x: i64, y: i64, word: u32) {
    let mut m = odyssey_buffer_mem(s, b);
    let p = odyssey_buffer_plane(s, b);

    sgi_plane_store(&mut m, &p, x as i32, y as i32, word);
}

/// Whether the last 0x1017 select named the id-plane destination, whatever
/// ODSY_DEST_FLAGS it carried.
pub fn odyssey_dest_is_id_planes(s: &mut SGIOdysseyState) -> bool {
    (s.dest_buffer_word & !ODSY_DEST_FLAGS) == (ODSY_DEST_ID_PLANES & !ODSY_DEST_FLAGS)
}

/// The buffers a colour write lands in, bit b for buffer b, from the last
/// 0x0424 drawing mask. A right-eye bit folds into its buffer: stereo is not
/// modelled.
pub fn odyssey_draw_buffers(s: &mut SGIOdysseyState) -> u32 {
    (if s.draw_mask & ODSY_DRAW_MASK_BUFFER0 != 0 { 1 } else { 0 })
        | (if s.draw_mask & ODSY_DRAW_MASK_BUFFER1 != 0 { 2 } else { 0 })
}

/// The one completion fence, shared by the standalone 0x0a5a host-write
/// command and the DMA engine's publish(): write the cookie's 16-bit token,
/// big-endian, to the guest fence slot at the host-write base plus twice the
/// slot, and pulse the softsync interrupt when the cookie asks.
pub fn odyssey_fence(s: &mut SGIOdysseyState, cookie: u64) {
    let word = (cookie & ODSY_FENCE_WORD_MASK as u64) as u32;
    let slot = word & ODSY_DMA_FENCE_SLOT_MASK;
    let token = (word >> ODSY_DMA_FENCE_TOKEN_SHIFT) & 0xffff;
    let base = (((s.host_write_addr_upper & 0xffff) as u64) << 32) | s.host_write_addr_lower as u64;

    address_space_stw_be(&address_space_memory(), base.wrapping_add(2 * slot as u64), token);
    if word & ODSY_DMA_FENCE_INTERRUPT != 0 {
        odyssey_deliver_softsync(&mut s.front.lock());
    }
}

/// The shared graphics foundation's head hooks (head_ops.rs). resolve_row
/// turns the colour plane into displayed colour through each pixel's
/// drawable, display mode and the lookup RAM; publish raises the completion
/// fence a written transfer signals; retrace is the vertical-retrace callback
/// the shared scanout drives at the mode's rate. dma_address stays None: the
/// inbound path is direct-mapped.
pub fn odyssey_publish(opaque: Opaque, c: &SGICompletion) {
    let s = from_opaque::<SGIOdysseyState>(opaque);

    // A transfer that reached board or guest memory raises the fence; a
    // failed one does not, so the driver's DMA wait times out as it would on
    // a board that never wrote the fence halfword. A cookie without the
    // validity marker wants no fence.
    if c.kind == SGI_DONE_WRITTEN && (c.cookie & ODSY_FENCE_VALID) != 0 {
        odyssey_fence(s, c.cookie);
    }
}

/// A run of guest bytes the DMA engine read: the C only traced it.
pub fn odyssey_dma_chunk(opaque: Opaque, seq: u64, addr: u64, bytes: &[u8]) {
    let _ = seq;
    // The chunks belong to the MMIO write that caused the RAM read.
    let s = from_opaque::<SGIOdysseyState>(opaque);
    crate::dlog_dev!(
        crate::devlog::LogModule::Odyssey,
        "sgi_odyssey_dma_chunk: seq {} addr {:#x} len {} hex {}",
        s.trace_sequence.wrapping_sub(1),
        addr,
        bytes.len(),
        bytes.iter().map(|b| format!("{:02x}", b)).collect::<String>()
    );
}

/// The top eight bits of a lookup-RAM entry's ten-bit channels, as RGB.
pub fn odyssey_lut_rgb(s: &mut SGIOdysseyState, entry: u32) -> u32 {
    let word = if entry < ODSY_LUT_SIZE { s.lut[entry as usize] } else { 0 };

    (((word >> ODSY_LUT_RED_SHIFT) & 0xff) << 16) | (((word >> ODSY_LUT_GREEN_SHIFT) & 0xff) << 8) | ((word >> ODSY_LUT_BLUE_SHIFT) & 0xff)
}

/// One 8-bit RGB channel through the 1,536-entry gamma table: widened to 12
/// bits as the X server widens it (c << 4 | c >> 4; the board's own widening
/// is unknown), mapped to the table in three ranges -- v below 0x200, v / 2 +
/// 0x100 below 0x400, else v / 4 + 0x200 -- and shown as the top eight bits of
/// that entry's channel at shift. Inferred: that the table applies to RGB
/// pixels at all (the blank zeroes it to blacken the screen).
pub fn odyssey_gamma(s: &mut SGIOdysseyState, c: u32, shift: u32) -> u32 {
    let v = (c << 4) | (c >> 4);
    let t = if v < 0x200 {
        v
    } else if v < 0x400 {
        v / 2 + 0x100
    } else {
        v / 4 + 0x200
    };

    (s.lut[(ODSY_GAMMA_BASE + t) as usize] >> shift) & 0xff
}

/// Pixel (x, y)'s main drawable id, bits 7:3 of its id-plane byte.
pub fn odyssey_main_id(s: &mut SGIOdysseyState, x: u32, y: u32) -> u32 {
    ((s.did[(y * SGI_ODYSSEY_WIDTH + x) as usize] as u32) >> ODSY_DID_MAIN_SHIFT) & (ODSY_DISPMODE_COUNT - 1)
}

/// The colormap group main drawable main_id shows through: bits 8:4 of its
/// display mode. Below ODSY_GAMMA_GROUP the plane word is an index.
pub fn odyssey_display_group(s: &mut SGIOdysseyState, main_id: u32) -> u32 {
    let mode = s.dispmode[main_id as usize];

    (mode >> ODSY_DISPMODE_GROUP_SHIFT) & ODSY_DISPMODE_GROUP_MASK
}

/// The channel byte c of plane word 0xRRGGBBAA as RGB display mode `mode`
/// shows it: a 4- or 5-bit visual's channel keeps its top bits, repeated down
/// to fill the byte as a full-scale value. How the board itself stores a
/// narrow pixel is unknown.
pub fn odyssey_display_channel(mode: u32, c: u32) -> u32 {
    match (mode >> ODSY_DISPMODE_WIDTH_SHIFT) & ODSY_DISPMODE_WIDTH_MASK {
        ODSY_DISPMODE_WIDTH_4 => (c & 0xf0) | (c >> 4),
        ODSY_DISPMODE_WIDTH_5 => (c & 0xf8) | (c >> 5),
        _ => c,
    }
}

/// Resolve the colour buffers at scanout. Per pixel: the id plane's main
/// drawable (bits 7:3) selects the buffer shown, by its displayed-buffer bit,
/// and display mode 0x2900 + id, whose bits 8:4 are the group. Inferred: that
/// the displayed-buffer bits index windows by main drawable id. A group at or
/// above 26 is an RGB mode: each channel of the plane word goes through the
/// gamma table. Below it the plane word is an index: lut[256 * group + (index
/// & 0xff)], or lut[index & 0xfff] for group 0; no second gamma pass. Group 25
/// resolves as an index table and is reported once as unverified. Over all of
/// that, a pixel whose overlay id's mode is enabled and whose overlay pixel is
/// not zero shows the overlay colormap entry instead.
pub fn odyssey_resolve_row(opaque: Opaque, y: u32, dst: &mut [u32]) {
    let s = from_opaque::<SGIOdysseyState>(opaque);
    let width = s.scanout.mode.width.min(SGI_ODYSSEY_WIDTH);
    // Swaps latch under the board lock (odyssey_retrace()), so the selection
    // holds for the row.
    let displayed = s.front.lock().displayed_buffer;

    for x in 0..width {
        let at = (y * SGI_ODYSSEY_WIDTH + x) as usize;
        let main_id = odyssey_main_id(s, x, y);
        let word = odyssey_buffer_load(s, (displayed >> main_id) & 1, x as i64, y as i64);
        let group = odyssey_display_group(s, main_id);

        let omode = s.overlay_mode[((s.did[at] as u32) & ODSY_DID_OVERLAY_MASK) as usize];
        let opixel = s.overlay[at];

        if (omode & ODSY_OVERLAY_MODE_ENABLE) != 0 && opixel != 0 {
            dst[x as usize] = odyssey_lut_rgb(
                s,
                256 * ((omode >> ODSY_OVERLAY_MODE_GROUP_SHIFT) & ODSY_OVERLAY_MODE_GROUP_MASK) + opixel as u32,
            );
            continue;
        }
        if group >= ODSY_GAMMA_GROUP {
            let mode = s.dispmode[main_id as usize];
            let r = odyssey_display_channel(mode, word >> 24);
            let g = odyssey_display_channel(mode, (word >> 16) & 0xff);
            let b = odyssey_display_channel(mode, (word >> 8) & 0xff);

            dst[x as usize] = odyssey_gamma(s, r, ODSY_LUT_RED_SHIFT) << 16
                | odyssey_gamma(s, g, ODSY_LUT_GREEN_SHIFT) << 8
                | odyssey_gamma(s, b, ODSY_LUT_BLUE_SHIFT);
            continue;
        }
        if group == ODSY_CMAP_UNVERIFIED_GROUP && !s.scanout_group25_reported {
            s.scanout_group25_reported = true;
            crate::qlog!("sgi-odyssey: colormap group 25 (0x1900) resolved as an index table; unverified");
        }
        dst[x as usize] = odyssey_lut_rgb(s, if group != 0 { 256 * group + (word & 0xff) } else { word & ODSY_INDEX_MASK });
    }
    odyssey_cursor_row(s, y, dst, width);
}

/// A vertical retrace, driven by the shared scanout at the mode's rate. The
/// driver issues a swap by writing ODSY_SWAP_ARM, a toggle mask with one bit
/// per window whose displayed buffer must flip at the next retrace, then waits
/// for the retrace interrupt. At each retrace the board first latches the
/// pending flips into the displayed-buffer bits and then raises the retrace
/// source, whose vector is byte 23:16 of ODSY_ROUTE_VECTORS. The interrupt
/// fires at every retrace once the routing is programmed, not only when a swap
/// is armed; before the driver programs the routing nothing is delivered.
/// Whether the board can mask this source is unknown and it is left unmasked.
///
/// Approximation: the board's encoding of ODSY_DISPLAYED_BUFFER is inferred
/// from the driver's compare, and the latch's timing within the retrace is
/// unknown. The model latches at its retrace callback and repaints the whole
/// frame from the new selection; a frame is never presented half from each
/// buffer.
pub fn odyssey_retrace(opaque: Opaque) {
    let s = from_opaque::<SGIOdysseyState>(opaque);
    let mut f = s.front.lock();
    let flips = f.swap_arm;
    let mut repaint = false;

    if f.swap_set != 0 || f.swap_clear != 0 {
        f.displayed_buffer = (f.displayed_buffer | f.swap_set) & !f.swap_clear;
        f.swap_set = 0;
        f.swap_clear = 0;
        repaint = true;
    }
    if flips != 0 {
        f.swap_arm = 0;
        f.displayed_buffer ^= flips;
        crate::dlog!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_swap_latch: arm {:#x} displayed {:#x}", flips, f.displayed_buffer);
        repaint = true;
    }
    if odyssey_routed(&f) {
        odyssey_deliver_routed(&mut f, ODSY_ROUTE_VECTOR_RETRACE_SHIFT, "retrace", true);
    }
    drop(f);
    if repaint {
        sgi_scanout_invalidate_all(&mut s.scanout);
    }
}

/// Scanout reads the planes under the board lock, which the board's thread
/// holds for a whole slice, so no draw runs meanwhile. A processor's write
/// of the displayed-buffer register reached only the front: repaint for it.
pub fn odyssey_pre_update(opaque: Opaque) {
    let s = from_opaque::<SGIOdysseyState>(opaque);
    let written = std::mem::take(&mut s.front.lock().displayed_written);

    if written {
        sgi_scanout_invalidate_all(&mut s.scanout);
    }
}

pub static odyssey_head_ops: SGIHeadOps = SGIHeadOps {
    publish: Some(odyssey_publish),
    resolve_row: Some(odyssey_resolve_row),
    pre_update: Some(odyssey_pre_update),
    convert_row: None,
    answer: None,
    dma_address: None,
    dma_chunk: Some(odyssey_dma_chunk),
    retrace: Some(odyssey_retrace),
};

/// Rebuild the kept (non-empty) clip rectangles from the raw 0x204b words.
pub fn odyssey_clip_update(s: &mut SGIOdysseyState) {
    s.clip_kept = 0;
    for i in 0..SGI_ODYSSEY_CLIP_RECTS as usize {
        let lo = s.clip_words[2 * i];
        let hi = s.clip_words[2 * i + 1];
        let r = SGIClipRect {
            x0: ((lo >> ODSY_CLIP_X_SHIFT) & ODSY_CLIP_FIELD_MASK) as i32,
            y0: (lo & ODSY_CLIP_FIELD_MASK) as i32,
            x1: ((hi >> ODSY_CLIP_X_SHIFT) & ODSY_CLIP_FIELD_MASK) as i32,
            y1: (hi & ODSY_CLIP_FIELD_MASK) as i32,
        };

        if r.x0 <= r.x1 && r.y0 <= r.y1 {
            s.clip_rect[s.clip_kept as usize] = r;
            s.clip_kept += 1;
        }
    }
}

/// Whether clip_words is the off-screen form (ODSY_CMD_WINDOW_RESTORE).
pub fn odyssey_clip_is_offscreen(s: &mut SGIOdysseyState) -> bool {
    for i in 0..(2 * SGI_ODYSSEY_CLIP_RECTS) as usize {
        let want = if i == 1 { ODSY_CLIP_OFFSCREEN_HI } else { 0 };

        if (s.clip_words[i] & ODSY_CLIP_WORD_MASK) != want {
            return false;
        }
    }
    true
}

/// The plane bound. The plane itself drops an out-of-plane store, but the
/// family checks first so the shared code never has to guess whose error it
/// is. odyssey_pixel_allowed() adds the window tests for every write except a
/// copy destination, which calls this helper directly.
pub fn odyssey_pixel_in_bounds(s: &mut SGIOdysseyState, x: i64, y: i64) -> bool {
    x >= 0 && y >= 0 && x < SGI_ODYSSEY_WIDTH as i64 && y < SGI_ODYSSEY_HEIGHT as i64
}

pub fn odyssey_id_compare_active(s: &mut SGIOdysseyState) -> bool {
    s.id_compare & ODSY_ID_COMPARE_ENABLE != 0
}

/// The 0x840 ownership test at an in-plane pixel.
pub fn odyssey_id_compare_passes(s: &mut SGIOdysseyState, x: i64, y: i64) -> bool {
    let mask = (s.id_compare >> ODSY_ID_COMPARE_MASK_SHIFT) as u8;

    !odyssey_id_compare_active(s)
        || ((s.did[(y * SGI_ODYSSEY_WIDTH as i64 + x) as usize] ^ s.id_compare as u8) & mask) == 0
}

/// The one pixel test every write but a copy destination passes: the plane
/// bound and the 0x840 id test, then inside at least one kept clip rectangle
/// -- with none kept nothing is drawn.
pub fn odyssey_pixel_allowed(s: &mut SGIOdysseyState, x: i64, y: i64) -> bool {
    if !odyssey_pixel_in_bounds(s, x, y) || !odyssey_id_compare_passes(s, x, y) {
        return false;
    }
    for i in 0..s.clip_kept as usize {
        let r = &s.clip_rect[i];

        if x >= r.x0 as i64 && x <= r.x1 as i64 && y >= r.y0 as i64 && y <= r.y1 as i64 {
            return true;
        }
    }
    false
}

/// The pixels of one row that odyssey_pixel_allowed() passes before its id
/// test: the plane bound and the union of the kept clip rectangles, as
/// ascending, disjoint, non-touching [start, end) runs. The 2D fills walk
/// these a row at a time; they still apply the id test per pixel.
pub fn odyssey_row_runs(s: &mut SGIOdysseyState, y: i64, runs: &mut OdysseyRowRuns) {
    let mut n = 0usize;
    let mut kept = 0usize;

    runs.n = 0;
    if y < 0 || y >= SGI_ODYSSEY_HEIGHT as i64 {
        return;
    }
    for i in 0..s.clip_kept as usize {
        let r = s.clip_rect[i];
        let a = (r.x0 as i64).max(0);
        let b = (r.x1 as i64 + 1).min(SGI_ODYSSEY_WIDTH as i64);
        let mut j = n;

        n += 1;
        if y < r.y0 as i64 || y > r.y1 as i64 || a >= b {
            n -= 1;
            continue;
        }
        // Insertion by start: there are at most four.
        while j != 0 && runs.start[j - 1] > a {
            runs.start[j] = runs.start[j - 1];
            runs.end[j] = runs.end[j - 1];
            j -= 1;
        }
        runs.start[j] = a;
        runs.end[j] = b;
    }
    for i in 0..n {
        if kept != 0 && runs.start[i] <= runs.end[kept - 1] {
            runs.end[kept - 1] = runs.end[kept - 1].max(runs.end[i]);
        } else {
            runs.start[kept] = runs.start[i];
            runs.end[kept] = runs.end[i];
            kept += 1;
        }
    }
    runs.n = kept as u32;
}

/// Colour buffer b's bytes for the n pixels of row y from x, when all of them
/// lie in its plane and its device-backed memory: the same bytes
/// sgi_plane_store() and sgi_plane_load() reach one pixel at a time. Null
/// otherwise, and the caller goes a pixel at a time.
pub fn odyssey_buffer_row(s: &mut SGIOdysseyState, b: u32, x: i64, y: i64, n: i64, plane: &mut P<SGIPlane>, bpp: &mut u32) -> P<u8> {
    let m = odyssey_buffer_mem(s, b);
    let p = odyssey_buffer_plane(s, b);
    let size = sgi_plane_bytes_per_pixel(p.format);

    if m.bytes.is_empty() || n <= 0 || x < 0 || y < 0 || x + n > p.width as i64 || y >= p.height as i64 {
        return P::null();
    }
    let at = p.base.wrapping_add((y as u64).wrapping_mul(p.stride as u64)).wrapping_add((x as u64).wrapping_mul(size as u64));
    let len = (n as u64).wrapping_mul(size as u64);
    if at > m.size || len > m.size - at {
        return P::null();
    }
    *plane = p;
    *bpp = size;
    P(unsafe { m.bytes.as_ptr().add(at as usize) } as *mut u8)
}

/// A colour write, into each buffer the drawing mask selects.
pub fn odyssey_paint_pixel(s: &mut SGIOdysseyState, x: i64, y: i64, pixel: u32) {
    let buffers = odyssey_draw_buffers(s);

    if !odyssey_pixel_allowed(s, x, y) {
        return;
    }
    for b in 0..ODSY_BUFFERS {
        if buffers & (1u32 << b) != 0 {
            odyssey_buffer_store(s, b, x, y, pixel);
        }
    }
}

/// The plane word a fill stores: the packed RGB pixel of an RGB fill, or the
/// raw index of a colour-index fill. No colormap is applied here: scanout
/// resolves each pixel through its own drawable.
pub fn odyssey_fill_pixel(is_index: bool, value: u32) -> u32 {
    if is_index { value & ODSY_INDEX_MASK } else { value }
}

/// One pixel's id-plane write: the bits mask selects of the id byte when the
/// drawing mask selects buffer 0, and of the overlay pixel byte when it
/// selects buffer 1.
pub fn odyssey_aux_store(s: &mut SGIOdysseyState, at: u64, buffers: u32, value: u8, mask: u8) {
    let at_i = at as usize;

    if buffers & (1u32 << ODSY_AUX_ID_BUFFER) != 0 {
        s.did[at_i] = (s.did[at_i] & !mask) | (value & mask);
    }
    if buffers & (1u32 << ODSY_AUX_OVERLAY_BUFFER) != 0 {
        s.overlay[at_i] = (s.overlay[at_i] & !mask) | (value & mask);
    }
    if buffers & ((1u32 << ODSY_AUX_ID_BUFFER) | (1u32 << ODSY_AUX_OVERLAY_BUFFER)) == 0 && !s.aux_mask_reported {
        s.aux_mask_reported = true;
        crate::qlog!(
            "sgi-odyssey: id-plane write with drawing mask {:#x} selects neither the id nor the overlay byte; dropped",
            s.draw_mask
        );
    }
}

/// One pixel the X server draws at the id-plane destination: the index's low
/// byte combined through op with the byte it lands on -- the overlay byte when
/// the drawing mask selects buffer 1, else the id byte -- and stored under the
/// 0x1023 plane mask.
pub fn odyssey_paint_aux_at(s: &mut SGIOdysseyState, at: u64, index: u32, op: u8) {
    let buffers = odyssey_draw_buffers(s);
    let mut byte = index as u8;

    if op as u32 != SGI_LOGIC_COPY {
        let dst = if buffers & (1u32 << ODSY_AUX_OVERLAY_BUFFER) != 0 { s.overlay[at as usize] } else { s.did[at as usize] };

        byte = sgi_logic_op(op, index, dst as u32) as u8;
    }
    odyssey_aux_store(s, at, buffers, byte, (s.plane_write_mask >> ODSY_PLANE_MASK_SHIFT) as u8);
}

pub fn odyssey_paint_aux(s: &mut SGIOdysseyState, x: i64, y: i64, index: u32, op: u8) {
    if odyssey_pixel_allowed(s, x, y) {
        odyssey_paint_aux_at(s, y as u64 * SGI_ODYSSEY_WIDTH as u64 + x as u64, index, op);
    }
}

/// One pixel of an integer primitive -- a QUADS rectangle, a pattern quad, a
/// line, point or polygon pixel -- combined with each selected buffer through
/// the logical operation 0x0422 names, when 0x0404 enables one for the
/// colour's kind: bit 19 for an index, bit 20 for an RGB colour. An index
/// result keeps the index bits; an RGB result keeps the fourth byte opaque.
/// Images and copies do not come through here: whether the board applies the
/// operation to them is unknown.
///
/// At the id-plane destination the pixel goes to the id or overlay byte
/// instead, under the 0x1023 plane mask. Its logical operation reads the
/// overlay byte when buffer 1 is selected and the id byte otherwise.
pub fn odyssey_paint_integer(s: &mut SGIOdysseyState, x: i64, y: i64, is_index: bool, value: u32) {
    let src = odyssey_fill_pixel(is_index, value);
    let enable = if is_index { ODSY_MODE_INDEX_LOGIC_OP } else { ODSY_MODE_COLOR_LOGIC_OP };
    let op = s.gl_ctx.cur.logic_op_mode.wrapping_sub(SGI_GL_CLEAR) as u8;
    let buffers = odyssey_draw_buffers(s);

    if odyssey_dest_is_id_planes(s) {
        let use_op = (s.pattern_mode & enable) != 0 && op as u32 != SGI_LOGIC_COPY;

        odyssey_paint_aux(s, x, y, src, if use_op { op } else { SGI_LOGIC_COPY as u8 });
        return;
    }
    if (s.pattern_mode & enable) == 0 || op as u32 == SGI_LOGIC_COPY {
        odyssey_paint_pixel(s, x, y, src);
        return;
    }
    if !odyssey_pixel_allowed(s, x, y) {
        return;
    }
    for b in 0..ODSY_BUFFERS {
        if buffers & (1u32 << b) != 0 {
            let out = sgi_logic_op(op, src, odyssey_buffer_load(s, b, x, y));

            odyssey_buffer_store(s, b, x, y, if is_index { out & ODSY_INDEX_MASK } else { out | 0xff });
        }
    }
}

/// odyssey_paint_integer() at each pixel of row y from x0 up to x1, given the
/// row's runs, a run of the buffer's bytes at a time. Each pixel is tested,
/// combined and stored exactly as the per-pixel path does it. Pixels are
/// independent, so going a buffer at a time rather than a pixel at a time
/// stores the same bytes.
pub fn odyssey_paint_integer_span(s: &mut SGIOdysseyState, runs: &OdysseyRowRuns, y: i64, x0: i64, x1: i64, is_index: bool, value: u32) {
    let src = odyssey_fill_pixel(is_index, value);
    let enable = if is_index { ODSY_MODE_INDEX_LOGIC_OP } else { ODSY_MODE_COLOR_LOGIC_OP };
    let op = s.gl_ctx.cur.logic_op_mode.wrapping_sub(SGI_GL_CLEAR) as u8;
    let logic = (s.pattern_mode & enable) != 0 && op as u32 != SGI_LOGIC_COPY;
    let id_test = odyssey_id_compare_active(s);
    let buffers = odyssey_draw_buffers(s);
    let aux = odyssey_dest_is_id_planes(s);

    for r in 0..runs.n as usize {
        let a = x0.max(runs.start[r]);
        let e = x1.min(runs.end[r]);

        if a >= e {
            continue;
        }
        if aux {
            for x in a..e {
                if !id_test || odyssey_id_compare_passes(s, x, y) {
                    odyssey_paint_aux_at(
                        s,
                        y as u64 * SGI_ODYSSEY_WIDTH as u64 + x as u64,
                        src,
                        if logic { op } else { SGI_LOGIC_COPY as u8 },
                    );
                }
            }
            continue;
        }
        for b in 0..ODSY_BUFFERS {
            let mut plane: P<SGIPlane> = P::null();
            let mut word = [0u8; 4];
            let mut bpp = 0u32;

            if buffers & (1u32 << b) == 0 {
                continue;
            }
            let row = odyssey_buffer_row(s, b, a, y, e - a, &mut plane, &mut bpp);
            if row.is_null() {
                for x in a..e {
                    let mut out = src;

                    if id_test && !odyssey_id_compare_passes(s, x, y) {
                        continue;
                    }
                    if logic {
                        out = sgi_logic_op(op, src, odyssey_buffer_load(s, b, x, y));
                        out = if is_index { out & ODSY_INDEX_MASK } else { out | 0xff };
                    }
                    odyssey_buffer_store(s, b, x, y, out);
                }
                continue;
            }
            sgi_plane_encode(&plane, src, &mut word);
            for x in a..e {
                let at = ((x - a) as usize) * bpp as usize;
                let px = unsafe { std::slice::from_raw_parts_mut(row.0.add(at), bpp as usize) };

                if id_test && !odyssey_id_compare_passes(s, x, y) {
                    continue;
                }
                if !logic {
                    px.copy_from_slice(&word[..bpp as usize]);
                    continue;
                }
                let out = sgi_logic_op(op, src, sgi_plane_decode(&plane, px));
                sgi_plane_encode(&plane, if is_index { out & ODSY_INDEX_MASK } else { out | 0xff }, px);
            }
        }
    }
}

/// odyssey_fillrect(): BEGIN(QUADS)/COLOR_3UB or COLOR_INDEX/four VERTEX_2I
/// corners/END drives the solid rectangle fill. odyssey_gl_end() routes a
/// QUADS fill here when pattern mode is off, and to odyssey_pattern_fill()
/// when it is on. is_index selects whether value is a packed RGB pixel or a
/// PseudoColor index resolved per pixel.
pub fn odyssey_fill_rect(s: &mut SGIOdysseyState, x: u32, y: u32, w: u32, h: u32, is_index: bool, value: u32) {
    let mut runs = OdysseyRowRuns::default();

    for row in 0..h {
        odyssey_row_runs(s, y as i64 + row as i64, &mut runs);
        odyssey_paint_integer_span(s, &runs, y as i64 + row as i64, x as i64, x as i64 + w as i64, is_index, value);
    }
    sgi_scanout_invalidate_all(&mut s.scanout);
}

/// odyssey_clear_planes(): the X server's write to the id-plane destination.
/// For each pixel of the rectangle, replace the plane bits the 0x1023 mask
/// selects with the value the COLOR_INDEX word carried, in the id byte when
/// the drawing mask selects buffer 0 and in the overlay pixel byte when it
/// selects buffer 1, leaving the colour framebuffer untouched.
pub fn odyssey_clear_planes(s: &mut SGIOdysseyState, x: u32, y: u32, w: u32, h: u32, value: u8, mask: u8) {
    let buffers = odyssey_draw_buffers(s);
    let id_test = odyssey_id_compare_active(s);
    let mut runs = OdysseyRowRuns::default();

    for row in 0..h {
        let py = y as i64 + row as i64;

        odyssey_row_runs(s, py, &mut runs);
        for r in 0..runs.n as usize {
            let a = (x as i64).max(runs.start[r]);
            let e = (x as i64 + w as i64).min(runs.end[r]);

            for px in a..e {
                if !id_test || odyssey_id_compare_passes(s, px, py) {
                    odyssey_aux_store(s, py as u64 * SGI_ODYSSEY_WIDTH as u64 + px as u64, buffers, value, mask);
                }
            }
        }
    }
    // A pixel's drawable picks its colour at scanout.
    sgi_scanout_invalidate_all(&mut s.scanout);
}

/// odyssey_copyrect()'s screen-to-screen blit, the console-scroll primitive.
/// Copies in whichever row/column direction keeps a read ahead of the matching
/// write, so an overlapping source and destination in the same framebuffer --
/// the case a scroll relies on -- reads the old contents before they are
/// overwritten.
///
/// Approximation: which buffer the board reads the source from is part of the
/// uncleared read selection, so the copy runs within each buffer the drawing
/// mask selects, source and destination in the same buffer.
///
/// Approximation, inferred: a destination pixel passes odyssey_pixel_in_bounds()
/// only, not the window-clip rectangles odyssey_pixel_allowed() also tests.
pub fn odyssey_copy_rect_buffer(s: &mut SGIOdysseyState, source: u32, dest: u32) {
    let sx = s.copy_src & 0xffff;
    let sy = (s.copy_src >> 16) & 0xffff;
    let w = s.copy_size & 0xffff;
    let h = (s.copy_size >> 16) & 0xffff;
    let dx = s.copy_dst & 0xffff;
    let dy = (s.copy_dst >> 16) & 0xffff;
    let w_clip = if dx < SGI_ODYSSEY_WIDTH { w.min(SGI_ODYSSEY_WIDTH - dx) } else { 0 };
    let h_clip = if dy < SGI_ODYSSEY_HEIGHT { h.min(SGI_ODYSSEY_HEIGHT - dy) } else { 0 };
    let reverse_rows = dy > sy;
    let reverse_cols = dx > sx;

    for row in 0..h_clip {
        let sr = if reverse_rows { h_clip - 1 - row } else { row };
        let mut src_plane: P<SGIPlane> = P::null();
        let mut dst_plane: P<SGIPlane> = P::null();
        let mut src_bpp = 0u32;
        let mut dst_bpp = 0u32;
        let src_row = if w_clip != 0 {
            odyssey_buffer_row(s, source, sx as i64, sy as i64 + sr as i64, w_clip as i64, &mut src_plane, &mut src_bpp)
        } else {
            P::null()
        };
        let dst_row = if !src_row.is_null() {
            odyssey_buffer_row(s, dest, dx as i64, dy as i64 + sr as i64, w_clip as i64, &mut dst_plane, &mut dst_bpp)
        } else {
            P::null()
        };

        // A row wholly inside both planes, in the same encoding, is the byte
        // copy the pixel loop below makes. The loop runs its columns so that
        // a read stays ahead of the write it could meet, which is what a
        // memmove does for the one overlapping case, a row copied along
        // itself.
        if !dst_row.is_null() && src_plane.format == dst_plane.format && src_plane.packing == dst_plane.packing {
            unsafe {
                std::ptr::copy(src_row.0, dst_row.0, w_clip as usize * dst_bpp as usize);
            }
            continue;
        }
        for col in 0..w_clip {
            let sc = if reverse_cols { w_clip - 1 - col } else { col };
            // An out-of-plane source reads zero, as the guarded read did.
            let pixel = odyssey_buffer_load(s, source, sx.wrapping_add(sc) as i64, sy.wrapping_add(sr) as i64);

            if odyssey_pixel_in_bounds(s, dx.wrapping_add(sc) as i64, dy.wrapping_add(sr) as i64) {
                odyssey_buffer_store(s, dest, dx.wrapping_add(sc) as i64, dy.wrapping_add(sr) as i64, pixel);
            }
        }
    }
}

pub fn odyssey_copy_rect(s: &mut SGIOdysseyState) {
    let source = (s.cmd_operand[2] >> ODSY_COPY_RECT_SOURCE_SHIFT) & 0xf;
    let dest = (s.cmd_operand[7] >> ODSY_COPY_RECT_DEST_SHIFT) & 0xf;
    let format_source = (s.cmd_operand[4] >> ODSY_COPY_RECT_FORMAT_SOURCE_SHIFT) & 0xf;
    let format_dest = (s.cmd_operand[4] >> ODSY_COPY_RECT_DEST_SHIFT) & 0xf;

    if s.cmd_operand[0] == ODSY_DMA_SENTINEL_READ {
        if !odyssey_copy_rect_to_object(s) {
            odyssey_cmd_unimplemented(s, ODSY_CMD_COPY_RECT_TRIGGER);
        }
        return;
    }

    if source != format_source || dest != format_dest {
        odyssey_cmd_unimplemented(s, ODSY_CMD_COPY_RECT_TRIGGER);
        return;
    }

    // The two observed copy blocks select 0 then 4 in both their source and
    // destination fields. This is an experimental model policy: the selector
    // values choose model buffers 0 and 1, but do not identify physical
    // memory banks. Source and destination choices stay separate so a copy
    // never changes another colour buffer.
    if (source == 0 || source == 4) && (dest == 0 || dest == 4) {
        odyssey_copy_rect_buffer(s, source >> 2, dest >> 2);
    } else if source == dest && (source == 2 || source == 3) {
        // Existing public and modelled templates retain draw-mask behaviour.
        let buffers = odyssey_draw_buffers(s);

        for b in 0..ODSY_BUFFERS {
            if buffers & (1u32 << b) != 0 {
                odyssey_copy_rect_buffer(s, b, b);
            }
        }
    } else {
        odyssey_cmd_unimplemented(s, ODSY_CMD_COPY_RECT_TRIGGER);
        return;
    }
    sgi_scanout_invalidate_all(&mut s.scanout);
}

/// Pattern mode is on when the 0x0404 mode's bits 3,4 are set and a shape is
/// selected. A quad drawn in this state paints the current colour through the
/// pattern instead of as a solid rectangle.
pub fn odyssey_pattern_enabled(s: &mut SGIOdysseyState) -> bool {
    if (s.pattern_mode & ODSY_PATTERN_MODE_ENABLE) != ODSY_PATTERN_MODE_ENABLE {
        return false;
    }
    matches!(
        s.pattern_shape & !ODSY_PATTERN_SHAPE_OPAQUE,
        ODSY_PATTERN_SHAPE_64X16 | ODSY_PATTERN_SHAPE_32X32 | ODSY_PATTERN_SHAPE_16X64
    )
}

/// The pattern bit at quad-relative column u, row v, for the current shape.
/// Row 0 column 0 is the quad's top-left vertex and bit 31 of a row's first
/// word is its leftmost pixel. A coordinate past the shape's own extent is
/// treated as clear: the X server never draws a quad larger than its pattern.
pub fn odyssey_pattern_bit(s: &mut SGIOdysseyState, u: u32, v: u32) -> bool {
    let word;
    let bit;

    match s.pattern_shape & !ODSY_PATTERN_SHAPE_OPAQUE {
        ODSY_PATTERN_SHAPE_64X16 => {
            if u >= 64 || v >= 16 {
                return false;
            }
            word = s.pattern_buffer[(2 * v + (u >> 5)) as usize];
            bit = 31 - (u & 31);
        }
        ODSY_PATTERN_SHAPE_32X32 => {
            if u >= 32 || v >= 32 {
                return false;
            }
            word = s.pattern_buffer[v as usize];
            bit = 31 - u;
        }
        ODSY_PATTERN_SHAPE_16X64 => {
            if u >= 16 || v >= 64 {
                return false;
            }
            word = s.pattern_buffer[(v >> 1) as usize];
            bit = 31 - 16 * (v & 1) - u;
        }
        _ => return false,
    }
    (word >> bit) & 1 != 0
}

/// The opaque pattern's background: the 0x0c21 constant colour, an index from
/// operand 1 bits 23:12, or an RGB pixel from the 12-bit red, green and blue
/// fields narrowed to 8 bits.
pub fn odyssey_pattern_background(s: &mut SGIOdysseyState, is_index: bool) -> u32 {
    let r = (s.const_colour[0] >> 12) & 0xfff;
    let g = s.const_colour[0] & 0xfff;
    let b = (s.const_colour[1] >> 12) & 0xfff;

    if is_index {
        return r;
    }
    odyssey_pack_pixel(r >> 4, g >> 4, b >> 4)
}

/// A pattern quad: paint the current colour at each pixel whose pattern bit is
/// set, and the background at each clear one when the pattern is opaque,
/// leaving the rest as it was otherwise. Only the rows and columns the quad
/// covers are read, so the stale part of the persistent pattern buffer beyond
/// the tile never contributes.
pub fn odyssey_pattern_fill(s: &mut SGIOdysseyState, x: u32, y: u32, w: u32, h: u32, pat_u0: u32, pat_v0: u32, is_index: bool, value: u32) {
    let opaque = s.pattern_shape & ODSY_PATTERN_SHAPE_OPAQUE != 0;
    let background = odyssey_pattern_background(s, is_index);
    let mut runs = OdysseyRowRuns::default();

    // Each row as spans of equal pattern bits, painted a span at a time.
    for v in 0..h {
        let py = y as i64 + v as i64;

        odyssey_row_runs(s, py, &mut runs);
        if runs.n == 0 {
            continue;
        }
        let mut u = 0u32;
        while u < w {
            let bit = odyssey_pattern_bit(s, pat_u0.wrapping_add(u), pat_v0.wrapping_add(v));
            let first = u;

            loop {
                u += 1;
                if !(u < w && odyssey_pattern_bit(s, pat_u0.wrapping_add(u), pat_v0.wrapping_add(v)) == bit) {
                    break;
                }
            }
            if bit || opaque {
                odyssey_paint_integer_span(
                    s,
                    &runs,
                    py,
                    x as i64 + first as i64,
                    x as i64 + u as i64,
                    is_index,
                    if bit { value } else { background },
                );
            }
        }
    }
    sgi_scanout_invalidate_all(&mut s.scanout);
}

/// A raster-box coordinate arrives as an IEEE-754 single the X server built
/// with an integer-to-float convert, so the 32-bit operand is a float bit
/// pattern, not an integer. Truncate toward zero; a NaN or negative clamps to
/// the screen origin.
pub fn odyssey_float_to_u32(bits: u32) -> u32 {
    let f = f32::from_bits(bits);

    if !(f > 0.0f32) {
        return 0;
    }
    f as u32
}

/// The index of the object at board base base, or -1. A base names at most
/// one live object (odyssey_object_select()), so the index's entry is the one
/// a search of obj_base would find.
pub fn odyssey_object_find(s: &mut SGIOdysseyState, base: u32) -> i32 {
    if base == 0 {
        return -1;
    }
    match s.obj_index.get(&base) {
        Some(slot) if *slot != 0 => (*slot - 1) as i32,
        _ => -1,
    }
}

/// Give object i the base base, keeping obj_index in step.
pub fn odyssey_object_set_base(s: &mut SGIOdysseyState, i: i32, base: u32) {
    let i = i as usize;

    if s.obj_base[i] != 0 {
        s.obj_index.remove(&s.obj_base[i]);
    }
    s.obj_base[i] = base;
    if base != 0 {
        s.obj_index.insert(base, i as u32 + 1);
    }
}

/// Rebuild obj_index from obj_base, after a load. Where a loaded table names
/// one base twice, the lower index wins, as a search would find it.
pub fn odyssey_object_index_rebuild(s: &mut SGIOdysseyState) {
    s.obj_index.clear();
    for i in (0..s.obj_count.min(SGI_ODYSSEY_OBJECTS) as usize).rev() {
        if s.obj_base[i] != 0 {
            s.obj_index.insert(s.obj_base[i], i as u32 + 1);
        }
    }
}

pub fn odyssey_object_word_valid(s: &SGIOdysseyState, word: u64) -> bool {
    word < ODSY_OBJ_POOL_WORDS as u64 && (s.obj_valid[(word >> 3) as usize] & (1u8 << (word & 7))) != 0
}

pub fn odyssey_object_word_set_valid(s: &mut SGIOdysseyState, word: u64, valid: bool) {
    let mask = 1u8 << (word & 7);

    assert!(word < ODSY_OBJ_POOL_WORDS as u64);
    if valid {
        s.obj_valid[(word >> 3) as usize] |= mask;
    } else {
        s.obj_valid[(word >> 3) as usize] &= !mask;
    }
}

pub fn odyssey_object_valid_range(s: &mut SGIOdysseyState, first: u64, count: u64, valid: bool) {
    for i in 0..count {
        odyssey_object_word_set_valid(s, first + i, valid);
    }
}

pub fn odyssey_object_valid_move(s: &mut SGIOdysseyState, source: u64, dest: u64, count: u64) {
    if dest > source {
        for i in (1..=count).rev() {
            let v = odyssey_object_word_valid(s, source + i - 1);

            odyssey_object_word_set_valid(s, dest + i - 1, v);
        }
    } else {
        for i in 0..count {
            let v = odyssey_object_word_valid(s, source + i);

            odyssey_object_word_set_valid(s, dest + i, v);
        }
    }
}

pub fn odyssey_object_region_written(s: &SGIOdysseyState, object: i32, x: u32, y: u32, width: u32, height: u32) -> bool {
    let o = object as usize;

    if x > s.obj_width[o] || width > s.obj_width[o] - x || y > s.obj_rows[o] || height > s.obj_rows[o] - y {
        return false;
    }
    for dy in 0..height {
        let row = s.obj_offset[o] as u64 + (y + dy) as u64 * s.obj_width[o] as u64 + x as u64;

        for dx in 0..width {
            if !odyssey_object_word_valid(s, row + dx as u64) {
                return false;
            }
        }
    }
    true
}

/// Mark object i as the most recently used.
pub fn odyssey_object_touch(s: &mut SGIOdysseyState, i: i32) {
    s.obj_clock += 1;
    s.obj_stamp[i as usize] = s.obj_clock;
}

/// Forget object i: its slot stays, with base 0, for a later object.
pub fn odyssey_object_forget(s: &mut SGIOdysseyState, i: i32) {
    odyssey_object_set_base(s, i, 0);
    let i = i as usize;
    s.obj_width[i] = 0;
    s.obj_rows[i] = 0;
    s.obj_offset[i] = 0;
    s.obj_stamp[i] = 0;
}

/// The least recently used live object other than keep, or -1.
pub fn odyssey_object_oldest(s: &mut SGIOdysseyState, keep: i32) -> i32 {
    let mut oldest: i32 = -1;

    for i in 0..s.obj_count as usize {
        if i as i32 != keep && s.obj_base[i] != 0 && (oldest < 0 || s.obj_stamp[i] < s.obj_stamp[oldest as usize]) {
            oldest = i as i32;
        }
    }
    oldest
}

/// Pack the objects that hold rows to the front of the store, in store order,
/// so the space between them is reclaimed, then object last (when not -1)
/// after them all, so that it can grow in place. Indices do not change.
pub fn odyssey_object_compact(s: &mut SGIOdysseyState, last: i32) {
    let last_size: u64 = if last >= 0 { s.obj_width[last as usize] as u64 * s.obj_rows[last as usize] as u64 } else { 0 };
    let mut held: Vec<u8> = Vec::new();
    let mut held_valid: Vec<bool> = Vec::new();
    let mut next: u64 = 0;

    // Packing the others may overwrite last's words: keep them aside.
    if last_size != 0 {
        let off = s.obj_offset[last as usize] as u64;

        held = s.obj_mem.bytes[(off * 4) as usize..((off + last_size) * 4) as usize].to_vec();
        for k in 0..last_size {
            held_valid.push(odyssey_object_word_valid(s, off + k));
        }
    }
    loop {
        let mut pick: i32 = -1;

        // The lowest-placed object not yet packed: at or past next.
        for i in 0..s.obj_count as usize {
            if i as i32 != last
                && s.obj_base[i] != 0
                && s.obj_rows[i] != 0
                && s.obj_offset[i] as u64 >= next
                && (pick < 0 || s.obj_offset[i] < s.obj_offset[pick as usize])
            {
                pick = i as i32;
            }
        }
        if pick < 0 {
            break;
        }
        let p = pick as usize;
        let size = s.obj_width[p] as u64 * s.obj_rows[p] as u64;
        if s.obj_offset[p] as u64 != next {
            let from = s.obj_offset[p] as usize * 4;
            s.obj_mem.bytes.copy_within(from..from + size as usize * 4, next as usize * 4);
            odyssey_object_valid_move(s, s.obj_offset[p] as u64, next, size);
            s.obj_offset[p] = next as u32;
        }
        next += size;
    }
    if last_size != 0 {
        let at = next as usize * 4;
        s.obj_mem.bytes[at..at + held.len()].copy_from_slice(&held);
        for k in 0..last_size {
            odyssey_object_word_set_valid(s, next + k, held_valid[k as usize]);
        }
        s.obj_offset[last as usize] = next as u32;
        next += last_size;
    }
    // Growth in place assumes the words past the store's end are zero.
    let tail = (s.obj_used as u64).saturating_sub(next);
    let at = next as usize * 4;
    s.obj_mem.bytes[at..at + tail as usize * 4].fill(0);
    odyssey_object_valid_range(s, next, tail, false);
    s.obj_used = next as u32;
}

/// The object at base, created with the given row length in pixels when it
/// does not exist; a new row length for an existing object discards its
/// storage (its layout changed). When the table is full, the least recently
/// used object is forgotten, reported once.
pub fn odyssey_object_select(s: &mut SGIOdysseyState, base: u32, width: u32) -> i32 {
    let mut i = odyssey_object_find(s, base);

    if i < 0 {
        for j in 0..s.obj_count as usize {
            if s.obj_base[j] == 0 {
                i = j as i32;
                break;
            }
        }
    }
    if i < 0 && s.obj_count == SGI_ODYSSEY_OBJECTS {
        i = odyssey_object_oldest(s, -1);
        if !s.obj_full_reported {
            s.obj_full_reported = true;
            crate::qlog!(
                "sgi-odyssey: more than {} board-memory objects; the least recently used, {:#x}, is forgotten",
                SGI_ODYSSEY_OBJECTS,
                s.obj_base[i as usize]
            );
        }
        odyssey_object_forget(s, i);
    }
    if i < 0 || s.obj_base[i as usize] == 0 {
        if i < 0 {
            i = s.obj_count as i32;
            s.obj_count += 1;
        }
        odyssey_object_set_base(s, i, base);
        s.obj_width[i as usize] = width;
        s.obj_offset[i as usize] = 0;
        s.obj_rows[i as usize] = 0;
    } else if width != 0 && s.obj_width[i as usize] != width {
        s.obj_width[i as usize] = width;
        s.obj_rows[i as usize] = 0;
    }
    odyssey_object_touch(s, i);
    i
}

/// Make room for rows rows in object i: grow it in place when it is the last
/// in the store, else move it to the end. When there is no room, forget the
/// least recently used other objects until the live objects and i's new size
/// fit, reporting the first object forgotten, then pack the store once with i
/// last. False, reported once, when i alone does not fit.
pub fn odyssey_object_rows(s: &mut SGIOdysseyState, i: i32, rows: u32) -> bool {
    let iu = i as usize;
    let pool = ODSY_OBJ_POOL_WORDS as u64;
    let need = s.obj_width[iu] as u64 * rows as u64;
    let have = s.obj_width[iu] as u64 * s.obj_rows[iu] as u64;

    if rows <= s.obj_rows[iu] {
        return true;
    }
    odyssey_object_touch(s, i);
    if need <= pool
        && !(have != 0 && s.obj_offset[iu] as u64 + have == s.obj_used as u64 && s.obj_offset[iu] as u64 + need <= pool)
        && s.obj_used as u64 + need > pool
    {
        let mut live: u64 = 0;

        for j in 0..s.obj_count as usize {
            if j != iu && s.obj_base[j] != 0 {
                live += s.obj_width[j] as u64 * s.obj_rows[j] as u64;
            }
        }
        while live + need > pool {
            let victim = odyssey_object_oldest(s, i);

            if victim < 0 {
                break;
            }
            let v = victim as usize;
            live -= s.obj_width[v] as u64 * s.obj_rows[v] as u64;
            if !s.obj_forget_reported {
                s.obj_forget_reported = true;
                crate::qlog!(
                    "sgi-odyssey: the model's {} MiB board-memory store is full; the least recently used object, {:#x}, is forgotten",
                    ODSY_OBJ_POOL_BYTES / (1024 * 1024),
                    s.obj_base[v]
                );
            }
            odyssey_object_forget(s, victim);
        }
        odyssey_object_compact(s, if have != 0 { i } else { -1 });
    }
    if have != 0 && s.obj_offset[iu] as u64 + have == s.obj_used as u64 && s.obj_offset[iu] as u64 + need <= pool {
        s.obj_used = (s.obj_offset[iu] as u64 + need) as u32;
        odyssey_object_valid_range(s, s.obj_offset[iu] as u64 + have, need - have, false);
    } else if s.obj_used as u64 + need <= pool {
        let new_offset = s.obj_used;
        let from = s.obj_offset[iu] as usize * 4;

        s.obj_mem.bytes.copy_within(from..from + have as usize * 4, new_offset as usize * 4);
        let z = (new_offset as usize + have as usize) * 4;
        s.obj_mem.bytes[z..z + (need - have) as usize * 4].fill(0);
        odyssey_object_valid_move(s, s.obj_offset[iu] as u64, new_offset as u64, have);
        odyssey_object_valid_range(s, new_offset as u64 + have, need - have, false);
        s.obj_offset[iu] = new_offset;
        s.obj_used += need as u32;
    } else {
        if !s.obj_full_reported {
            s.obj_full_reported = true;
            crate::qlog!(
                "sgi-odyssey: board-memory object {:#x} ({} x {}) exceeds the model's {} MiB store",
                s.obj_base[iu],
                s.obj_width[iu],
                rows,
                ODSY_OBJ_POOL_BYTES / (1024 * 1024)
            );
        }
        return false;
    }
    s.obj_rows[iu] = rows;
    true
}

/// Object i's texel (x, y) as 0xRRGGBBAA; outside its rows or width, dropped.
pub fn odyssey_object_store(s: &mut SGIOdysseyState, i: i32, x: u32, y: u32, rgba: u32) {
    if x >= s.obj_width[i as usize] || !odyssey_object_rows(s, i, y.wrapping_add(1)) {
        return;
    }
    let word = s.obj_offset[i as usize] as u64 + y as u64 * s.obj_width[i as usize] as u64 + x as u64;
    stl_be_p(&mut s.obj_mem.bytes[(word * 4) as usize..], rgba);
    odyssey_object_word_set_valid(s, word, true);
}

pub fn odyssey_object_load(s: &mut SGIOdysseyState, i: i32, x: u32, y: u32) -> u32 {
    let iu = i as usize;

    if x >= s.obj_width[iu] || y >= s.obj_rows[iu] {
        return 0;
    }
    ldl_be_p(&s.obj_mem.bytes[((s.obj_offset[iu] as u64 + y as u64 * s.obj_width[iu] as u64 + x as u64) * 4) as usize..])
}

/// Copy a recognised GL read-drawable rectangle into its scratch object.
pub fn odyssey_copy_rect_to_object(s: &mut SGIOdysseyState) -> bool {
    let w = s.copy_size & 0xffff;
    let h = s.copy_size >> 16;
    let dx = s.copy_dst & 0xffff;
    let dy = s.copy_dst >> 16;
    let sx = (s.copy_src & 0xffff) as i64 + s.read_origin[0] as i64;
    let sy = (s.copy_src >> 16) as i64 + s.read_origin[1] as i64;
    let fmt = s.cmd_operand[4];
    let tail = s.cmd_operand[7];
    let rgba8 = fmt == 0x92233002 && tail == 0x3801210b;
    let rgba16 = (fmt & !ODSY_DMA_FMT_FRONT) == 0x12633042 && (tail == 0x3802204b || tail == 0xf802204b);
    let obj = odyssey_object_find(s, s.cmd_operand[5]);

    if s.cmd_operand[0] != ODSY_DMA_SENTINEL_READ
        || s.cmd_operand[2] != 0x80200050
        || (!rgba8 && !rgba16)
        || s.cmd_operand[5] != s.dest_base
        || obj < 0
        || w == 0
        || h == 0
        || dx as u64 + w as u64 > s.obj_width[obj as usize] as u64
        || sx < 0
        || sy < 0
        || sx + w as i64 > SGI_ODYSSEY_WIDTH as i64
        || sy + h as i64 > SGI_ODYSSEY_HEIGHT as i64
        || !odyssey_object_rows(s, obj, dy.wrapping_add(h))
    {
        return false;
    }
    let displayed = s.front.lock().displayed_buffer;
    for y in 0..h {
        for x in 0..w {
            let shown = (displayed >> odyssey_main_id(s, (sx + x as i64) as u32, (sy + y as i64) as u32)) & 1;
            let source = if fmt & ODSY_DMA_FMT_FRONT != 0 { shown } else { shown ^ 1 };
            let px = odyssey_buffer_load(s, source, sx + x as i64, sy + y as i64);

            odyssey_object_store(s, obj, dx + x, dy + y, px);
        }
    }
    true
}

/// The object an image transfer writes, or -1 for the screen: the last 0x1017
/// select, when it named a base other than the colour buffer's.
pub fn odyssey_image_object(s: &mut SGIOdysseyState) -> i32 {
    if s.dest_base == 0 || s.dest_base == s.dma_colour_base || odyssey_dest_is_id_planes(s) {
        return -1;
    }
    odyssey_object_find(s, s.dest_base)
}

/// Lay one image pixel, as 0xRRGGBBAA, at the raster cursor in the selected
/// buffer (odyssey_image_object()) and advance it. Each axis is a directed
/// half-open interval: increasing endpoints visit [first, last), while
/// decreasing endpoints visit [first - 1, last]. Payload advances along X,
/// then along Y.
pub fn odyssey_image_put(s: &mut SGIOdysseyState, rgba: u32, is_index: bool, preserve_alpha: bool) {
    let x_decreasing = s.image_x2 < s.image_x1;
    let y_decreasing = s.image_y2 < s.image_y1;
    let y_finished = if y_decreasing {
        s.image_cur_y == u32::MAX || s.image_cur_y < s.image_y2
    } else {
        s.image_cur_y >= s.image_y2
    };

    if s.image_x2 == s.image_x1 || s.image_y2 == s.image_y1 || y_finished {
        return;
    }
    let obj = odyssey_image_object(s);
    if obj >= 0 {
        // An off-screen object: box coordinates, no window or id tests.
        odyssey_object_store(s, obj, s.image_cur_x, s.image_cur_y, rgba);
    } else if odyssey_dest_is_id_planes(s) {
        // Menu cascade arrows: index images into the overlay.
        if is_index {
            odyssey_paint_aux(
                s,
                s.image_cur_x as i64 + s.win_origin[0] as i64,
                s.image_cur_y as i64 + s.win_origin[1] as i64,
                rgba,
                SGI_LOGIC_COPY as u8,
            );
        } else if !s.aux_image_reported {
            s.aux_image_reported = true;
            crate::qlog!("sgi-odyssey: an RGB image to the id-plane destination is not modeled; dropped");
        }
    } else {
        // Screen pixels are window-relative; object pixels are not.
        odyssey_paint_pixel(
            s,
            s.image_cur_x as i64 + s.win_origin[0] as i64,
            s.image_cur_y as i64 + s.win_origin[1] as i64,
            if is_index || preserve_alpha { rgba } else { (rgba & !0xffu32) | 0xff },
        );
    }
    if (x_decreasing && s.image_cur_x == s.image_x2) || (!x_decreasing && s.image_cur_x == s.image_x2.wrapping_sub(1)) {
        s.image_cur_x = if x_decreasing { s.image_x1.wrapping_sub(1) } else { s.image_x1 };
        if y_decreasing {
            s.image_cur_y = if s.image_cur_y == s.image_y2 { u32::MAX } else { s.image_cur_y.wrapping_sub(1) };
        } else {
            s.image_cur_y = s.image_cur_y.wrapping_add(1);
        }
    } else if x_decreasing {
        s.image_cur_x = s.image_cur_x.wrapping_sub(1);
    } else {
        s.image_cur_x = s.image_cur_x.wrapping_add(1);
    }
    sgi_scanout_invalidate_all(&mut s.scanout);
}

/// One byte-stream texel of the open image at the raster cursor, bounded by
/// the header's group length in pixels.
pub fn odyssey_image_texel(s: &mut SGIOdysseyState, rgba: u32, is_index: bool) {
    if s.image_group_len != 0 && s.image_group_painted >= s.image_group_len {
        return;
    }
    odyssey_image_put(s, rgba, is_index, false);
    s.image_group_painted += 1;
}

pub fn odyssey_unorm16_to_8(v: u32) -> u32 {
    (v.wrapping_mul(255).wrapping_add(65535 / 2)) / 65535
}

/// Components of a 12-bit texture source code, or 0 for any other code.
pub fn odyssey_texel12_components(code: u32) -> u32 {
    match code {
        ODSY_IMAGE_CODE_LUM12 => 1,
        ODSY_IMAGE_CODE_LUM_ALPHA12 => 2,
        ODSY_IMAGE_CODE_RGB12 => 3,
        ODSY_IMAGE_CODE_RGBA12 | ODSY_IMAGE_CODE_ABGR12 => 4,
        _ => 0,
    }
}

/// A 12-bit texture source pixel as 0xRRGGBBAA: its components, each a byte or
/// (wide) a big-endian 16-bit word, in the order its code names.
pub fn odyssey_decode_texel12(code: u32, b: &[u8], wide: bool) -> u32 {
    let mut c = [0u32; 4];
    let n = odyssey_texel12_components(code) as usize;

    for i in 0..n {
        c[i] = if wide { odyssey_unorm16_to_8(lduw_be_p(&b[2 * i..])) } else { b[i] as u32 };
    }
    match code {
        ODSY_IMAGE_CODE_LUM12 => odyssey_pack_pixel(c[0], c[0], c[0]),
        ODSY_IMAGE_CODE_LUM_ALPHA12 => (odyssey_pack_pixel(c[0], c[0], c[0]) & !0xffu32) | c[1],
        ODSY_IMAGE_CODE_RGB12 => odyssey_pack_pixel(c[0], c[1], c[2]),
        ODSY_IMAGE_CODE_ABGR12 => (odyssey_pack_pixel(c[3], c[2], c[1]) & !0xffu32) | c[0],
        _ => (odyssey_pack_pixel(c[0], c[1], c[2]) & !0xffu32) | c[3],
    }
}

/// Decode one pixel of a byte-stream source format whose bytes are b[0 ..]:
/// its 0xRRGGBBAA word, or for 0x41 and 0x61 the raw index. False for a code
/// this model does not know.
pub fn odyssey_decode_pixel(s: &mut SGIOdysseyState, code: u32, b: &[u8], rgba: &mut u32) -> bool {
    match code {
        ODSY_IMAGE_CODE_ABGR => {
            *rgba = odyssey_pack_pixel(b[3] as u32, b[2] as u32, b[1] as u32);
            true
        }
        ODSY_IMAGE_CODE_FALLBACK_XRGB => {
            *rgba = odyssey_pack_pixel(b[1] as u32, b[2] as u32, b[3] as u32);
            true
        }
        ODSY_IMAGE_CODE_RGBA | ODSY_IMAGE_CODE_RGBA12 => {
            *rgba = ldl_be_p(b);
            true
        }
        ODSY_IMAGE_CODE_RGB => {
            *rgba = odyssey_pack_pixel(b[0] as u32, b[1] as u32, b[2] as u32);
            true
        }
        ODSY_IMAGE_CODE_LUM => {
            *rgba = odyssey_pack_pixel(b[0] as u32, b[0] as u32, b[0] as u32);
            true
        }
        ODSY_IMAGE_CODE_LUM_ALPHA => {
            *rgba = (odyssey_pack_pixel(b[0] as u32, b[0] as u32, b[0] as u32) & !0xffu32) | b[1] as u32;
            true
        }
        ODSY_IMAGE_CODE_LUM12 | ODSY_IMAGE_CODE_LUM_ALPHA12 => {
            // A burst's 16-bit components (odyssey_pixel_bytes()).
            *rgba = odyssey_decode_texel12(code, b, true);
            true
        }
        ODSY_IMAGE_CODE_CI => {
            // The raw index: scanout resolves it through the pixel's drawable.
            *rgba = b[0] as u32;
            true
        }
        ODSY_IMAGE_CODE_CI16 => {
            *rgba = lduw_be_p(b) & ODSY_INDEX_MASK;
            true
        }
        _ => false,
    }
}

/// Whether a source format carries colour indices rather than colours.
pub fn odyssey_code_is_index(code: u32) -> bool {
    code == ODSY_IMAGE_CODE_CI || code == ODSY_IMAGE_CODE_CI16
}

/// Bytes a pixel of a source format takes, 0 for an unknown code.
pub fn odyssey_pixel_bytes(code: u32) -> u32 {
    match code {
        ODSY_IMAGE_CODE_ABGR | ODSY_IMAGE_CODE_FALLBACK_XRGB | ODSY_IMAGE_CODE_RGBA | ODSY_IMAGE_CODE_RGBA12 => 4,
        ODSY_IMAGE_CODE_RGB => 3,
        ODSY_IMAGE_CODE_LUM | ODSY_IMAGE_CODE_CI => 1,
        ODSY_IMAGE_CODE_LUM_ALPHA | ODSY_IMAGE_CODE_LUM12 | ODSY_IMAGE_CODE_CI16 => 2,
        ODSY_IMAGE_CODE_LUM_ALPHA12 => 4,
        _ => 0,
    }
}

pub fn odyssey_image_pairing_supported(code: u32, fmt: u32) -> bool {
    if code == ODSY_IMAGE_CODE_RGBA12 {
        return fmt == ODSY_IMAGE_FMT_RGB || fmt == ODSY_IMAGE_FMT_RGBA16;
    }
    if code == ODSY_IMAGE_CODE_RGB12 || code == ODSY_IMAGE_CODE_ABGR12 {
        return fmt == ODSY_IMAGE_FMT_RGBA16;
    }
    if code == ODSY_IMAGE_CODE_LUM_ALPHA {
        return fmt == ODSY_IMAGE_FMT_CI;
    }
    fmt != ODSY_IMAGE_FMT_RGBA16
}

/// One data word of a pixel burst, laid out by the transfer header's format
/// code: its bytes join a stream that yields a pixel every
/// odyssey_pixel_bytes() bytes, so a three-byte pixel may span two words. A
/// bounded group's trailing padding (past image_group_len) is dropped with any
/// partial pixel, so the raster position stays exact. A header with a code
/// this model does not know falls back on the burst tag's format, 0x11 one
/// 0x00RRGGBB pixel a word and 0x10 four indices, and is reported once.
pub fn odyssey_image_burst_word(s: &mut SGIOdysseyState, word: u32) {
    let mut code = s.image_code;
    let mut n = odyssey_pixel_bytes(code);
    let mut b = [0u8; 4];

    if !odyssey_image_pairing_supported(code, s.image_fmt) {
        return;
    }
    if s.image_fmt == ODSY_IMAGE_FMT_RGBA16 {
        // A stream of 16-bit components, high half first, a pixel every
        // odyssey_texel12_components() halves: two words for 0x6b and 0x6c, a
        // word and a half for 0x69. At most two halves wait between words;
        // image_nbytes counts them, image_bytes holds them.
        let comps = odyssey_texel12_components(code) as usize;
        let mut halves = [0u16; 4];
        let mut held = s.image_nbytes.min(2) as usize;

        if s.image_group_len != 0 && s.image_group_painted >= s.image_group_len {
            return;
        }
        for i in 0..held {
            halves[i] = (s.image_bytes >> (16 * (held - 1 - i))) as u16;
        }
        for h in 0..2 {
            let mut px = [0u8; 8];

            halves[held] = if h != 0 { word as u16 } else { (word >> 16) as u16 };
            held += 1;
            if held < comps {
                continue;
            }
            for i in 0..comps {
                stw_be_p(&mut px[2 * i..], halves[i] as u32);
            }
            odyssey_image_put(s, odyssey_decode_texel12(code, &px, true), false, true);
            held = 0;
        }
        s.image_bytes = if held > 1 {
            ((halves[0] as u32) << 16) | halves[1] as u32
        } else if held != 0 {
            halves[0] as u32
        } else {
            0
        };
        s.image_nbytes = held as u32;
        s.image_group_painted += 1;
        if s.image_group_len != 0 && s.image_group_painted >= s.image_group_len && s.image_nbytes != 0 {
            s.image_bytes = 0;
            s.image_nbytes = 0;
        }
        return;
    }

    if n == 0 {
        if !s.image_code_reported {
            s.image_code_reported = true;
            crate::qlog!("sgi-odyssey: image source format {:#x} is not modeled; laid out by the burst tag", code);
        }
        code = if s.image_fmt == ODSY_IMAGE_FMT_RGB { ODSY_IMAGE_CODE_FALLBACK_XRGB } else { ODSY_IMAGE_CODE_CI };
        n = odyssey_pixel_bytes(code);
    }
    stl_be_p(&mut b, word);
    for i in 0..4 {
        let mut px = [0u8; 4];
        let mut rgba = 0u32;

        s.image_bytes = (s.image_bytes << 8) | b[i] as u32;
        s.image_nbytes += 1;
        if s.image_nbytes < n {
            continue;
        }
        s.image_nbytes = 0;
        // The pixel's bytes, first byte highest, at the start of px[].
        stl_be_p(&mut px, s.image_bytes << (8 * (4 - n)));
        odyssey_decode_pixel(s, code, &px, &mut rgba);
        odyssey_image_texel(s, rgba, odyssey_code_is_index(code));
    }
}

/// True, and set *fmt, when reg is a pixel-burst register-select word.
pub fn odyssey_image_burst_fmt(reg: u32, fmt: &mut u32) -> bool {
    let f = reg & ODSY_IMAGE_BURST_FMT_MASK;

    if (reg & ODSY_IMAGE_BURST_BASE) == 0 || (reg & ODSY_IMAGE_BURST_RESERVED_MASK) != 0 {
        return false;
    }
    match f {
        ODSY_IMAGE_FMT_CI | ODSY_IMAGE_FMT_RGB | ODSY_IMAGE_FMT_RGBA16 => {
            *fmt = f;
            true
        }
        // Other documented pixel-burst formats remain unsupported.
        _ => false,
    }
}

/// The family side of the shared OpenGL context and software rasterizer: a
/// thin decoder. The rasterizer rule set is chosen, pending capture: the
/// Octane RE4 set is the only one sgi_raster_rules_supported() implements,
/// and a flat single triangle in one colour exercises none of the parameters
/// that distinguish heads. The board's own subpixel grid and
/// interpolated-colour depth are unmeasured. The floating-point depth rule is
/// inferred, not measured. (A function, not a static: the struct is not const
/// constructible.)
pub fn odyssey_gl_rules() -> SGIRasterRules {
    SGIRasterRules {
        subpixel_bits: 8,
        color_bits: 8,
        depth_bits: 24,
        stencil_bits: 8,
        rounding: SGI_ROUND_HALF_AWAY as u8,
        tie_rule: SGI_TIE_TOP_LEFT_BOTTOM_ORIGIN as u8,
        arith: SGI_ARITH_DOUBLE as u8,
        line_rule: SGI_LINE_COVERAGE_DOUBLE as u8,
        alpha_test_bits: 12,
        smooth_primitives: true,
        // The board's own depth format; odyssey_gl_load_combined().
        depth_float: true,
        ..Default::default()
    }
}
