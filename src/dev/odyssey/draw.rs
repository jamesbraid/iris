//! Ported from hw/misc/sgi-odyssey.c in jamesbraid/qemu-sgi-origami.

use super::*;

/// The clip-rectangle runs of one row (Board::row_runs).
#[derive(Clone, Copy)]
pub struct RowRuns {
    pub n: u32,
    pub start: [i64; SGI_ODYSSEY_CLIP_RECTS as usize],
    pub end: [i64; SGI_ODYSSEY_CLIP_RECTS as usize],
}

impl Default for RowRuns {
    fn default() -> Self {
        RowRuns { n: 0, start: [0; SGI_ODYSSEY_CLIP_RECTS as usize], end: [0; SGI_ODYSSEY_CLIP_RECTS as usize] }
    }
}

// Anonymous enum: current-value query kinds.
pub const ODSY_QUERY_INDEX: u32 = 0;
pub const ODSY_QUERY_COLOUR: u32 = 1;
pub const ODSY_QUERY_NORMAL: u32 = 2;
pub const ODSY_QUERY_TEXCOORD: u32 = 3;
pub const ODSY_QUERY_EDGE: u32 = 4;

impl Board {
    pub fn back_material_reset(&mut self) {
        self.gl_back_material.copy_from_slice(&BACK_MATERIAL_DEFAULTS);
        self.gl_back_shininess_sqrt = 0;
    }

    pub fn shadow_put(&mut self, base: u64, off: u32, word: u32) {
        let at = base.wrapping_add(off as u64);
        if !write_guest(&self.guest, at, &word.to_be_bytes()) && !self.ctx_shadow_fail_reported {
            self.ctx_shadow_fail_reported = true;
            crate::dlog!(LogModule::Odyssey, "sgi-odyssey: context shadow store to {:#x} failed", at);
        }
    }

    /// One selected ring entry: attribute a's word and its raw operands.
    pub fn shadow_ring(&mut self, base: u64, a: u32, slot: u32, word: u32, ops: &[u32]) {
        self.shadow_put(base, ODSY_HW_RING_WORD + ODSY_HW_RING_WORD_STRIDE * a + 4 * slot, word);
        for (i, op) in ops.iter().enumerate() {
            self.shadow_put(
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
    pub fn shadow_descriptor(&mut self, base: u64, off: u32, record: &[[u32; 5]; 19], slot: u32) {
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
        self.shadow_put(base, off, word[0]);
        self.shadow_put(base, off + 4, word[1]);
    }

    pub fn open_current_records(&mut self, record: &mut [[u32; 5]; 19]) {
        const material: [u32; 4] = [
            ODSY_GL_MATERIAL_AMBIENT,
            ODSY_GL_MATERIAL_DIFFUSE,
            ODSY_GL_MATERIAL_SPECULAR,
            ODSY_GL_MATERIAL_EMISSION,
        ];
        const back_material: [u32; 4] = [ODSY_GL_BACK_AMBIENT, ODSY_GL_BACK_DIFFUSE, ODSY_GL_BACK_SPECULAR, ODSY_GL_BACK_EMISSION];

        *record = [[0u32; 5]; 19];
        record[ODSY_HW_ATTR_INDEX] = self.query_current[ODSY_QUERY_INDEX as usize];
        record[ODSY_HW_ATTR_COLOUR] = self.query_current[ODSY_QUERY_COLOUR as usize];
        record[ODSY_HW_ATTR_NORMAL] = self.query_current[ODSY_QUERY_NORMAL as usize];
        record[ODSY_HW_ATTR_TEXTURE] = self.query_current[ODSY_QUERY_TEXCOORD as usize];
        record[ODSY_HW_ATTR_EDGE] = self.query_current[ODSY_QUERY_EDGE as usize];
        for i in 0..4usize {
            record[ODSY_HW_ATTR_MATERIAL + i][0] = material[i];
            record[ODSY_HW_ATTR_MATERIAL + i][1..5].copy_from_slice(&self.gl_material[4 * i..4 * i + 4]);
            record[ODSY_HW_ATTR_BACK_MATERIAL + i][0] = back_material[i];
            record[ODSY_HW_ATTR_BACK_MATERIAL + i][1..5].copy_from_slice(&self.gl_back_material[4 * i..4 * i + 4]);
        }
        record[ODSY_HW_ATTR_SHININESS][0] = ODSY_GL_SHININESS_SQRT;
        record[ODSY_HW_ATTR_SHININESS][1] = self.gl_shininess_sqrt;
        record[ODSY_HW_ATTR_BACK_SHININESS][0] = ODSY_GL_BACK_SHININESS_SQRT;
        record[ODSY_HW_ATTR_BACK_SHININESS][1] = self.gl_back_shininess_sqrt;
    }

    pub fn open_record_vertex(&mut self, word: u32) {
        let mut v = OpenVertex { word, slot: self.gl_open_r, ..Default::default() };
        let mut n = self.gl_open_held as usize;
        let arity = (cmd_arity(word) as usize).min(4);

        v.operands[..arity].copy_from_slice(&self.cmd_operand[..arity]);
        self.open_current_records(&mut v.attribute);
        if self.gl_primitive == 2 || self.gl_primitive == 6 {
            self.gl_open_r = if self.gl_open_r == 3 { 1 } else { self.gl_open_r + 1 };
        } else {
            self.gl_open_r = (self.gl_open_r + 1) & 3;
        }
        self.gl_open_c = (self.gl_open_c + 1).min(7);
        self.gl_open_m3 = (self.gl_open_m3 + 1) % 3;
        self.gl_open_m4 = (self.gl_open_m4 + 1) & 3;

        match self.gl_primitive {
            0 => {
                // Points retain no vertices.
                n = 0;
            }
            1 => {
                // Lines retain one of each pair.
                if n != 0 {
                    n = 0;
                } else {
                    self.gl_open_vertex[0] = v;
                    n = 1;
                }
            }
            2 | 6 => {
                // Anchor and latest vertex.
                self.gl_open_vertex[if n != 0 { 1 } else { 0 }] = v;
                n = (n + 1).min(2);
            }
            3 => {
                // Line strip.
                self.gl_open_vertex[0] = v;
                n = 1;
            }
            4 | 7 => {
                // Triangles and quads.
                if n == (if self.gl_primitive == 4 { 2 } else { 3 }) {
                    n = 0;
                } else {
                    self.gl_open_vertex[n] = v;
                    n += 1;
                }
            }
            5 => {
                // Triangle strip.
                if n == 2 {
                    self.gl_open_vertex[0] = self.gl_open_vertex[1];
                    self.gl_open_vertex[1] = v;
                } else {
                    self.gl_open_vertex[n] = v;
                    n += 1;
                }
            }
            8 => {
                // Quad strip.
                if n == 3 {
                    self.gl_open_vertex[0] = self.gl_open_vertex[2];
                    self.gl_open_vertex[1] = v;
                    n = 2;
                } else {
                    self.gl_open_vertex[n] = v;
                    n += 1;
                }
            }
            _ => {}
        }
        self.gl_open_held = n as u32;
    }

    pub fn restart_finish(&mut self) {
        let n = self.gl_open_held;
        let mut count;

        if !self.gl_restart_armed || self.gl_restart_absorb != 0 {
            return;
        }
        if self.gl_restart_m3 >= 3 || n != self.gl_restart_total {
            self.cmd_unimplemented(0x741);
            self.gl_restart_armed = false;
            return;
        }
        self.gl_open_c = if self.gl_restart_c == 7 { 7 } else { (self.gl_restart_c + n).min(7) };
        self.gl_open_m3 = (self.gl_restart_m3 + n) % 3;
        self.gl_open_m4 = (self.gl_restart_m4 + n) & 3;
        count = self.gl_open_c;
        if count == 7 {
            while (count % 3 != self.gl_open_m3 || count % 4 != self.gl_open_m4) && count < 19 {
                count += 1;
            }
        }
        self.gl_in_prim = self.gl_ctx.resume_primitive(self.gl_restart_type, Some(&self.gl_restart_vertex[..]), n, count);
        self.gl_restart_armed = false;
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
    pub fn ctx_save(&mut self, base: u64) {
        const material: [u32; 4] = [
            ODSY_GL_MATERIAL_AMBIENT,
            ODSY_GL_MATERIAL_DIFFUSE,
            ODSY_GL_MATERIAL_SPECULAR,
            ODSY_GL_MATERIAL_EMISSION,
        ];
        const back_material: [u32; 4] = [ODSY_GL_BACK_AMBIENT, ODSY_GL_BACK_DIFFUSE, ODSY_GL_BACK_SPECULAR, ODSY_GL_BACK_EMISSION];
        let index = self.query_current[ODSY_QUERY_INDEX as usize];
        let colour = self.query_current[ODSY_QUERY_COLOUR as usize];
        let normal = self.query_current[ODSY_QUERY_NORMAL as usize];
        let edge = self.query_current[ODSY_QUERY_EDGE as usize][0];
        let open = self.gl_begin_open && !self.gl_integer_prim && self.gl_primitive <= 8;
        let ar = |w: u32| (cmd_arity(w) as usize).min(4);

        if self.gl_begin_open && !open && !self.ctx_open_prim_logged {
            self.ctx_open_prim_logged = true;
            crate::dlog!(
                LogModule::Odyssey,
                "sgi-odyssey: context switch inside primitive {:#x}; its buffered vertices are not saved",
                self.gl_primitive
            );
        }
        self.shadow_put(base, ODSY_HW_PIPE_STATE, ODSY_HW_PIPE_IDLE);
        for i in 0..ODSY_HW_DESCRIPTOR_WORDS {
            self.shadow_put(base, ODSY_HW_CURRENT_ATTRIBUTES + 4 * i, 0);
        }
        self.shadow_put(base, ODSY_HW_INDEX_WORD, index[0]);
        for i in 0..cmd_arity(index[0]) {
            self.shadow_put(base, ODSY_HW_INDEX_OPERANDS + 4 * i, index[1 + (i as usize).min(3)]);
        }
        self.shadow_put(base, ODSY_HW_COLOUR_WORD, colour[0]);
        for i in 0..cmd_arity(colour[0]) {
            self.shadow_put(base, ODSY_HW_COLOUR_OPERANDS + 4 * i, colour[1 + (i as usize).min(3)]);
        }
        self.shadow_ring(base, ODSY_HW_ATTR_INDEX as u32, 0, index[0], &index[1..1 + ar(index[0])]);
        self.shadow_ring(base, ODSY_HW_ATTR_COLOUR as u32, 0, colour[0], &colour[1..1 + ar(colour[0])]);
        self.shadow_ring(base, ODSY_HW_ATTR_NORMAL as u32, 0, normal[0], &normal[1..1 + ar(normal[0])]);
        self.shadow_ring(base, ODSY_HW_ATTR_EDGE as u32, 0, edge, &[]);
        if self.tex_coord_q != ODSY_F32_ONE {
            let coordinate = [self.tex_coord[0], self.tex_coord[1], self.tex_coord_r, self.tex_coord_q];

            self.shadow_ring(base, ODSY_HW_ATTR_TEXTURE as u32, 0, ODSY_GL_TEXCOORD_4F, &coordinate);
        } else {
            let three = (self.tex_mode & ODSY_TEX_MODE_3D) != 0 || self.tex_coord_r != 0;
            let three_ops = [self.tex_coord[0], self.tex_coord[1], self.tex_coord_r];
            let two_ops = self.tex_coord;

            self.shadow_ring(
                base,
                ODSY_HW_ATTR_TEXTURE as u32,
                0,
                if three { ODSY_GL_TEXCOORD_3F } else { ODSY_GL_TEXCOORD_2F },
                if three { &three_ops[..] } else { &two_ops[..] },
            );
        }
        for i in 0..4usize {
            let m: [u32; 4] = self.gl_material[i * 4..i * 4 + 4].try_into().unwrap();

            self.shadow_ring(base, (ODSY_HW_ATTR_MATERIAL + i) as u32, 0, material[i], &m);
        }
        let sh = [self.gl_shininess_sqrt];
        self.shadow_ring(base, ODSY_HW_ATTR_SHININESS as u32, 0, ODSY_GL_SHININESS_SQRT, &sh);
        for i in 0..4usize {
            let m: [u32; 4] = self.gl_back_material[i * 4..i * 4 + 4].try_into().unwrap();

            self.shadow_ring(base, (ODSY_HW_ATTR_BACK_MATERIAL + i) as u32, 0, back_material[i], &m);
        }
        let bsh = [self.gl_back_shininess_sqrt];
        self.shadow_ring(base, ODSY_HW_ATTR_BACK_SHININESS as u32, 0, ODSY_GL_BACK_SHININESS_SQRT, &bsh);
        if open {
            let mut current = [[0u32; 5]; 19];
            let mut state = self.gl_primitive
                | (self.gl_open_held << 4)
                | (self.gl_open_r << 6)
                | (self.gl_open_m3 << 12)
                | (self.gl_open_m4 << 14)
                | (self.gl_open_c << 21);

            // After c >= 2 vertices the next triangle is number c - 2.
            if self.gl_primitive == ODSY_GL_TRIANGLE_STRIP && self.gl_open_c >= 2 && (self.gl_open_m4 & 1) != 0 {
                state |= ODSY_HW_PIPE_STRIP_ODD;
            }

            self.open_current_records(&mut current);
            self.shadow_put(base, ODSY_HW_PIPE_STATE, state);
            for i in 0..self.gl_open_held.min(3) {
                let v = self.gl_open_vertex[i as usize];
                let slot = v.slot;

                self.shadow_ring(base, 0, slot, v.word, &v.operands[..ar(v.word)]);
                self.shadow_descriptor(base, 0x8c + 8 * slot, &v.attribute, i);
                for a in 1..19usize {
                    if v.attribute[a][0] == 0 {
                        continue;
                    }
                    self.shadow_ring(
                        base,
                        a as u32,
                        i,
                        v.attribute[a][0],
                        &v.attribute[a][1..1 + ar(v.attribute[a][0])],
                    );
                }
            }
            self.shadow_descriptor(base, ODSY_HW_CURRENT_ATTRIBUTES, &current, 3);
            for i in 0..19usize {
                if current[i][0] == 0 {
                    continue;
                }
                self.shadow_ring(base, i as u32, 3, current[i][0], &current[i][1..1 + ar(current[i][0])]);
            }
        }
    }

    /// The kernel driver's context switch: a single 8-byte store to
    /// ODSY_CTX_SWITCH_PORT, not a command-FIFO word. The new context id in bits
    /// 31:16 is not modelled: the board learns the context from the shadow address
    /// pair, which names where the outgoing state is saved (Board::ctx_save()),
    /// and the kernel restores the rest. A store naming context 0x7f is the
    /// kernel driver's shadow snapshot and saves the same way. Any other shape at
    /// this port has no established meaning and is reported unimplemented instead
    /// of guessed.
    pub fn ctx_switch(&mut self, value: u64) {
        if ((value >> ODSY_CTX_SWITCH_OPCODE_SHIFT) & 0xff) != ODSY_CTX_SWITCH_OPCODE as u64
            || (value & ODSY_CTX_SWITCH_VALID) == 0
        {
            self.unimplemented(ODSY_CTX_SWITCH_PORT, 8, true);
            return;
        }
        if self.ctx_shadow_addr_upper == 0 && self.ctx_shadow_addr_lower == 0 {
            crate::dlog!(
                LogModule::Odyssey,
                "sgi-odyssey: context switch ({:#x}) with no shadow address programmed; no completion written",
                value
            );
            return;
        }
        self.query_cancel();
        let shadow_base = (((self.ctx_shadow_addr_upper & 0xffff) as u64) << 32) | self.ctx_shadow_addr_lower as u64;
        // A plain guest-physical address: interrupt packets go elsewhere.
        self.ctx_save(shadow_base);
        self.shadow_put(shadow_base, ODSY_CTX_SWITCH_DONE_OFFSET as u32, 1);
    }
}

/// 2D fills carry no alpha channel in this grammar; the framebuffer's fourth
/// byte is modelled fully opaque (the colour plane's red-high R,G,B,A packing,
/// resolved by resolve_row() below).
pub fn pack_pixel(r: u32, g: u32, b: u32) -> u32 {
    (r << 24) | (g << 16) | (b << 8) | 0xff
}

impl Board {
    /// Colour buffer b's board memory and plane (Board::realize()), b being 0 or
    /// 1 in the displayed-buffer bits' sense (ODSY_REG_DRAW_MASK).
    pub fn buffer_mem(&mut self, b: u32) -> P<BoardMemory> {
        if b != 0 { P::new(&mut self.buffer1_mem) } else { P::new(&mut self.mem) }
    }

    pub fn buffer_plane(&mut self, b: u32) -> P<Plane> {
        if b != 0 {
            self.buffer1_mem.plane(self.buffer1_plane)
        } else {
            self.mem.plane(self.colour_plane)
        }
    }

    pub fn buffer_load(&mut self, b: u32, x: i64, y: i64) -> u32 {
        let m = self.buffer_mem(b);
        let p = self.buffer_plane(b);

        m.plane_load(&p, x as i32, y as i32)
    }

    pub fn buffer_store(&mut self, b: u32, x: i64, y: i64, word: u32) {
        let mut m = self.buffer_mem(b);
        let p = self.buffer_plane(b);

        m.plane_store(&p, x as i32, y as i32, word);
    }

    /// Whether the last 0x1017 select named the id-plane destination, whatever
    /// ODSY_DEST_FLAGS it carried.
    pub fn dest_is_id_planes(&mut self) -> bool {
        (self.dest_buffer_word & !ODSY_DEST_FLAGS) == (ODSY_DEST_ID_PLANES & !ODSY_DEST_FLAGS)
    }

    /// The buffers a colour write lands in, bit b for buffer b, from the last
    /// 0x0424 drawing mask. A right-eye bit folds into its buffer: stereo is not
    /// modelled.
    pub fn draw_buffers(&mut self) -> u32 {
        (if self.draw_mask & ODSY_DRAW_MASK_BUFFER0 != 0 { 1 } else { 0 })
            | (if self.draw_mask & ODSY_DRAW_MASK_BUFFER1 != 0 { 2 } else { 0 })
    }

    /// The one completion fence, shared by the standalone 0x0a5a host-write
    /// command and the DMA engine's publish(): write the cookie's 16-bit token,
    /// big-endian, to the guest fence slot at the host-write base plus twice the
    /// slot, and pulse the softsync interrupt when the cookie asks.
    pub fn fence(&mut self, cookie: u64) {
        let word = (cookie & ODSY_FENCE_WORD_MASK as u64) as u32;
        let slot = word & ODSY_DMA_FENCE_SLOT_MASK;
        let token = (word >> ODSY_DMA_FENCE_TOKEN_SHIFT) & 0xffff;
        let base = (((self.host_write_addr_upper & 0xffff) as u64) << 32) | self.host_write_addr_lower as u64;

        write_guest(&self.guest, base.wrapping_add(2 * slot as u64), &(token as u16).to_be_bytes());
        if word & ODSY_DMA_FENCE_INTERRUPT != 0 {
            self.front.lock().deliver_softsync();
        }
    }
}

/// The shared graphics foundation's head hooks (head_ops.rs). resolve_row
/// turns the colour plane into displayed colour through each pixel's
/// drawable, display mode and the lookup RAM; publish raises the completion
/// fence a written transfer signals; retrace is the vertical-retrace callback
/// the shared scanout drives at the mode's rate. dma_address stays None: the
/// inbound path is direct-mapped.
pub fn publish(opaque: Opaque, c: &Completion) {
    let s = from_opaque::<Board>(opaque);

    // A transfer that reached board or guest memory raises the fence; a
    // failed one does not, so the driver's DMA wait times out as it would on
    // a board that never wrote the fence halfword. A cookie without the
    // validity marker wants no fence.
    if c.kind == SGI_DONE_WRITTEN && (c.cookie & ODSY_FENCE_VALID) != 0 {
        s.fence(c.cookie);
    }
}

/// A run of guest bytes the DMA engine read: the C only traced it.
pub fn dma_chunk(opaque: Opaque, seq: u64, addr: u64, bytes: &[u8]) {
    let _ = seq;
    // The chunks belong to the MMIO write that caused the RAM read.
    let s = from_opaque::<Board>(opaque);
    crate::dlog_dev!(
        LogModule::Odyssey,
        "sgi_odyssey_dma_chunk: seq {} addr {:#x} len {} hex {}",
        s.trace_sequence.wrapping_sub(1),
        addr,
        bytes.len(),
        bytes.iter().map(|b| format!("{:02x}", b)).collect::<String>()
    );
}

impl Board {
    /// The top eight bits of a lookup-RAM entry's ten-bit channels, as RGB.
    pub fn lut_rgb(&mut self, entry: u32) -> u32 {
        let word = if entry < ODSY_LUT_SIZE { self.lut[entry as usize] } else { 0 };

        (((word >> ODSY_LUT_RED_SHIFT) & 0xff) << 16) | (((word >> ODSY_LUT_GREEN_SHIFT) & 0xff) << 8) | ((word >> ODSY_LUT_BLUE_SHIFT) & 0xff)
    }

    /// One 8-bit RGB channel through the 1,536-entry gamma table: widened to 12
    /// bits as the X server widens it (c << 4 | c >> 4; the board's own widening
    /// is unknown), mapped to the table in three ranges -- v below 0x200, v / 2 +
    /// 0x100 below 0x400, else v / 4 + 0x200 -- and shown as the top eight bits of
    /// that entry's channel at shift. Inferred: that the table applies to RGB
    /// pixels at all (the blank zeroes it to blacken the screen).
    pub fn gamma(&mut self, c: u32, shift: u32) -> u32 {
        let v = (c << 4) | (c >> 4);
        let t = if v < 0x200 {
            v
        } else if v < 0x400 {
            v / 2 + 0x100
        } else {
            v / 4 + 0x200
        };

        (self.lut[(ODSY_GAMMA_BASE + t) as usize] >> shift) & 0xff
    }

    /// Pixel (x, y)'s main drawable id, bits 7:3 of its id-plane byte.
    pub fn main_id(&mut self, x: u32, y: u32) -> u32 {
        ((self.did[(y * SGI_ODYSSEY_WIDTH + x) as usize] as u32) >> ODSY_DID_MAIN_SHIFT) & (ODSY_DISPMODE_COUNT - 1)
    }

    /// The colormap group main drawable main_id shows through: bits 8:4 of its
    /// display mode. Below ODSY_GAMMA_GROUP the plane word is an index.
    pub fn display_group(&mut self, main_id: u32) -> u32 {
        let mode = self.dispmode[main_id as usize];

        (mode >> ODSY_DISPMODE_GROUP_SHIFT) & ODSY_DISPMODE_GROUP_MASK
    }
}

/// The channel byte c of plane word 0xRRGGBBAA as RGB display mode `mode`
/// shows it: a 4- or 5-bit visual's channel keeps its top bits, repeated down
/// to fill the byte as a full-scale value. How the board itself stores a
/// narrow pixel is unknown.
pub fn display_channel(mode: u32, c: u32) -> u32 {
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
pub fn resolve_row(opaque: Opaque, y: u32, dst: &mut [u32]) {
    let s = from_opaque::<Board>(opaque);
    let width = s.scanout.mode.width.min(SGI_ODYSSEY_WIDTH);
    // Swaps latch under the board lock (retrace()), so the selection
    // holds for the row.
    let displayed = s.front.lock().displayed_buffer;

    for x in 0..width {
        let at = (y * SGI_ODYSSEY_WIDTH + x) as usize;
        let main_id = s.main_id(x, y);
        let word = s.buffer_load((displayed >> main_id) & 1, x as i64, y as i64);
        let group = s.display_group(main_id);

        let omode = s.overlay_mode[((s.did[at] as u32) & ODSY_DID_OVERLAY_MASK) as usize];
        let opixel = s.overlay[at];

        if (omode & ODSY_OVERLAY_MODE_ENABLE) != 0 && opixel != 0 {
            dst[x as usize] = s.lut_rgb(
                256 * ((omode >> ODSY_OVERLAY_MODE_GROUP_SHIFT) & ODSY_OVERLAY_MODE_GROUP_MASK) + opixel as u32,
            );
            continue;
        }
        if group >= ODSY_GAMMA_GROUP {
            let mode = s.dispmode[main_id as usize];
            let r = display_channel(mode, word >> 24);
            let g = display_channel(mode, (word >> 16) & 0xff);
            let b = display_channel(mode, (word >> 8) & 0xff);

            dst[x as usize] = s.gamma(r, ODSY_LUT_RED_SHIFT) << 16
                | s.gamma(g, ODSY_LUT_GREEN_SHIFT) << 8
                | s.gamma(b, ODSY_LUT_BLUE_SHIFT);
            continue;
        }
        if group == ODSY_CMAP_UNVERIFIED_GROUP && !s.scanout_group25_reported {
            s.scanout_group25_reported = true;
            crate::dlog!(LogModule::Odyssey, "sgi-odyssey: colormap group 25 (0x1900) resolved as an index table; unverified");
        }
        dst[x as usize] = s.lut_rgb(if group != 0 { 256 * group + (word & 0xff) } else { word & ODSY_INDEX_MASK });
    }
    s.cursor_row(y, dst, width);
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
pub fn retrace(opaque: Opaque) {
    let s = from_opaque::<Board>(opaque);
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
        crate::dlog!(LogModule::Odyssey, "sgi_odyssey_swap_latch: arm {:#x} displayed {:#x}", flips, f.displayed_buffer);
        repaint = true;
    }
    if f.routed() {
        f.deliver_routed(ODSY_ROUTE_VECTOR_RETRACE_SHIFT, "retrace", true);
    }
    drop(f);
    if repaint {
        s.scanout.invalidate_all();
    }
}

/// Scanout reads the planes under the board lock, which the board's thread
/// holds for a whole slice, so no draw runs meanwhile. A processor's write
/// of the displayed-buffer register reached only the front: repaint for it.
pub fn pre_update(opaque: Opaque) {
    let s = from_opaque::<Board>(opaque);
    let written = std::mem::take(&mut s.front.lock().displayed_written);

    if written {
        s.scanout.invalidate_all();
    }
}

pub static HEAD_OPS: HeadOps = HeadOps {
    publish: Some(publish),
    resolve_row: Some(resolve_row),
    pre_update: Some(pre_update),
    convert_row: None,
    answer: None,
    dma_address: None,
    dma_chunk: Some(dma_chunk),
    retrace: Some(retrace),
};

impl Board {
    /// Rebuild the kept (non-empty) clip rectangles from the raw 0x204b words.
    pub fn clip_update(&mut self) {
        self.clip_kept = 0;
        for i in 0..SGI_ODYSSEY_CLIP_RECTS as usize {
            let lo = self.clip_words[2 * i];
            let hi = self.clip_words[2 * i + 1];
            let r = ClipRect {
                x0: ((lo >> ODSY_CLIP_X_SHIFT) & ODSY_CLIP_FIELD_MASK) as i32,
                y0: (lo & ODSY_CLIP_FIELD_MASK) as i32,
                x1: ((hi >> ODSY_CLIP_X_SHIFT) & ODSY_CLIP_FIELD_MASK) as i32,
                y1: (hi & ODSY_CLIP_FIELD_MASK) as i32,
            };

            if r.x0 <= r.x1 && r.y0 <= r.y1 {
                self.clip_rect[self.clip_kept as usize] = r;
                self.clip_kept += 1;
            }
        }
    }

    /// Whether clip_words is the off-screen form (ODSY_CMD_WINDOW_RESTORE).
    pub fn clip_is_offscreen(&mut self) -> bool {
        for i in 0..(2 * SGI_ODYSSEY_CLIP_RECTS) as usize {
            let want = if i == 1 { ODSY_CLIP_OFFSCREEN_HI } else { 0 };

            if (self.clip_words[i] & ODSY_CLIP_WORD_MASK) != want {
                return false;
            }
        }
        true
    }

    /// The plane bound. The plane itself drops an out-of-plane store, but the
    /// family checks first so the shared code never has to guess whose error it
    /// is. Board::pixel_allowed() adds the window tests for every write except a
    /// copy destination, which calls this helper directly.
    pub fn pixel_in_bounds(&mut self, x: i64, y: i64) -> bool {
        x >= 0 && y >= 0 && x < SGI_ODYSSEY_WIDTH as i64 && y < SGI_ODYSSEY_HEIGHT as i64
    }

    pub fn id_compare_active(&mut self) -> bool {
        self.id_compare & ODSY_ID_COMPARE_ENABLE != 0
    }

    /// The 0x840 ownership test at an in-plane pixel.
    pub fn id_compare_passes(&mut self, x: i64, y: i64) -> bool {
        let mask = (self.id_compare >> ODSY_ID_COMPARE_MASK_SHIFT) as u8;

        !self.id_compare_active()
            || ((self.did[(y * SGI_ODYSSEY_WIDTH as i64 + x) as usize] ^ self.id_compare as u8) & mask) == 0
    }

    /// The one pixel test every write but a copy destination passes: the plane
    /// bound and the 0x840 id test, then inside at least one kept clip rectangle
    /// -- with none kept nothing is drawn.
    pub fn pixel_allowed(&mut self, x: i64, y: i64) -> bool {
        if !self.pixel_in_bounds(x, y) || !self.id_compare_passes(x, y) {
            return false;
        }
        for i in 0..self.clip_kept as usize {
            let r = &self.clip_rect[i];

            if x >= r.x0 as i64 && x <= r.x1 as i64 && y >= r.y0 as i64 && y <= r.y1 as i64 {
                return true;
            }
        }
        false
    }

    /// The pixels of one row that Board::pixel_allowed() passes before its id
    /// test: the plane bound and the union of the kept clip rectangles, as
    /// ascending, disjoint, non-touching [start, end) runs. The 2D fills walk
    /// these a row at a time; they still apply the id test per pixel.
    pub fn row_runs(&mut self, y: i64, runs: &mut RowRuns) {
        let mut n = 0usize;
        let mut kept = 0usize;

        runs.n = 0;
        if y < 0 || y >= SGI_ODYSSEY_HEIGHT as i64 {
            return;
        }
        for i in 0..self.clip_kept as usize {
            let r = self.clip_rect[i];
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
    /// BoardMemory::plane_store() and BoardMemory::plane_load() reach one pixel at a time. Null
    /// otherwise, and the caller goes a pixel at a time.
    pub fn buffer_row(&mut self, b: u32, x: i64, y: i64, n: i64, plane: &mut P<Plane>, bpp: &mut u32) -> P<u8> {
        let m = self.buffer_mem(b);
        let p = self.buffer_plane(b);
        let size = plane_bytes_per_pixel(p.format);

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
    pub fn paint_pixel(&mut self, x: i64, y: i64, pixel: u32) {
        let buffers = self.draw_buffers();

        if !self.pixel_allowed(x, y) {
            return;
        }
        for b in 0..ODSY_BUFFERS {
            if buffers & (1u32 << b) != 0 {
                self.buffer_store(b, x, y, pixel);
            }
        }
    }
}

/// The plane word a fill stores: the packed RGB pixel of an RGB fill, or the
/// raw index of a colour-index fill. No colormap is applied here: scanout
/// resolves each pixel through its own drawable.
pub fn fill_pixel(is_index: bool, value: u32) -> u32 {
    if is_index { value & ODSY_INDEX_MASK } else { value }
}

impl Board {
    /// One pixel's id-plane write: the bits mask selects of the id byte when the
    /// drawing mask selects buffer 0, and of the overlay pixel byte when it
    /// selects buffer 1.
    pub fn aux_store(&mut self, at: u64, buffers: u32, value: u8, mask: u8) {
        let at_i = at as usize;

        if buffers & (1u32 << ODSY_AUX_ID_BUFFER) != 0 {
            self.did[at_i] = (self.did[at_i] & !mask) | (value & mask);
        }
        if buffers & (1u32 << ODSY_AUX_OVERLAY_BUFFER) != 0 {
            self.overlay[at_i] = (self.overlay[at_i] & !mask) | (value & mask);
        }
        if buffers & ((1u32 << ODSY_AUX_ID_BUFFER) | (1u32 << ODSY_AUX_OVERLAY_BUFFER)) == 0 && !self.aux_mask_reported {
            self.aux_mask_reported = true;
            crate::dlog!(
                LogModule::Odyssey,
                "sgi-odyssey: id-plane write with drawing mask {:#x} selects neither the id nor the overlay byte; dropped",
                self.draw_mask
            );
        }
    }

    /// One pixel the X server draws at the id-plane destination: the index's low
    /// byte combined through op with the byte it lands on -- the overlay byte when
    /// the drawing mask selects buffer 1, else the id byte -- and stored under the
    /// 0x1023 plane mask.
    pub fn paint_aux_at(&mut self, at: u64, index: u32, op: u8) {
        let buffers = self.draw_buffers();
        let mut byte = index as u8;

        if op as u32 != SGI_LOGIC_COPY {
            let dst = if buffers & (1u32 << ODSY_AUX_OVERLAY_BUFFER) != 0 { self.overlay[at as usize] } else { self.did[at as usize] };

            byte = logic_op(op, index, dst as u32) as u8;
        }
        self.aux_store(at, buffers, byte, (self.plane_write_mask >> ODSY_PLANE_MASK_SHIFT) as u8);
    }

    pub fn paint_aux(&mut self, x: i64, y: i64, index: u32, op: u8) {
        if self.pixel_allowed(x, y) {
            self.paint_aux_at(y as u64 * SGI_ODYSSEY_WIDTH as u64 + x as u64, index, op);
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
    pub fn paint_integer(&mut self, x: i64, y: i64, is_index: bool, value: u32) {
        let src = fill_pixel(is_index, value);
        let enable = if is_index { ODSY_MODE_INDEX_LOGIC_OP } else { ODSY_MODE_COLOR_LOGIC_OP };
        let op = self.gl_ctx.cur.logic_op_mode.wrapping_sub(SGI_GL_CLEAR) as u8;
        let buffers = self.draw_buffers();

        if self.dest_is_id_planes() {
            let use_op = (self.pattern_mode & enable) != 0 && op as u32 != SGI_LOGIC_COPY;

            self.paint_aux(x, y, src, if use_op { op } else { SGI_LOGIC_COPY as u8 });
            return;
        }
        if (self.pattern_mode & enable) == 0 || op as u32 == SGI_LOGIC_COPY {
            self.paint_pixel(x, y, src);
            return;
        }
        if !self.pixel_allowed(x, y) {
            return;
        }
        for b in 0..ODSY_BUFFERS {
            if buffers & (1u32 << b) != 0 {
                let out = logic_op(op, src, self.buffer_load(b, x, y));

                self.buffer_store(b, x, y, if is_index { out & ODSY_INDEX_MASK } else { out | 0xff });
            }
        }
    }

    /// Board::paint_integer() at each pixel of row y from x0 up to x1, given the
    /// row's runs, a run of the buffer's bytes at a time. Each pixel is tested,
    /// combined and stored exactly as the per-pixel path does it. Pixels are
    /// independent, so going a buffer at a time rather than a pixel at a time
    /// stores the same bytes.
    pub fn paint_integer_span(&mut self, runs: &RowRuns, y: i64, x0: i64, x1: i64, is_index: bool, value: u32) {
        let src = fill_pixel(is_index, value);
        let enable = if is_index { ODSY_MODE_INDEX_LOGIC_OP } else { ODSY_MODE_COLOR_LOGIC_OP };
        let op = self.gl_ctx.cur.logic_op_mode.wrapping_sub(SGI_GL_CLEAR) as u8;
        let logic = (self.pattern_mode & enable) != 0 && op as u32 != SGI_LOGIC_COPY;
        let id_test = self.id_compare_active();
        let buffers = self.draw_buffers();
        let aux = self.dest_is_id_planes();

        for r in 0..runs.n as usize {
            let a = x0.max(runs.start[r]);
            let e = x1.min(runs.end[r]);

            if a >= e {
                continue;
            }
            if aux {
                for x in a..e {
                    if !id_test || self.id_compare_passes(x, y) {
                        self.paint_aux_at(
                            y as u64 * SGI_ODYSSEY_WIDTH as u64 + x as u64,
                            src,
                            if logic { op } else { SGI_LOGIC_COPY as u8 },
                        );
                    }
                }
                continue;
            }
            for b in 0..ODSY_BUFFERS {
                let mut plane: P<Plane> = P::null();
                let mut word = [0u8; 4];
                let mut bpp = 0u32;

                if buffers & (1u32 << b) == 0 {
                    continue;
                }
                let row = self.buffer_row(b, a, y, e - a, &mut plane, &mut bpp);
                if row.is_null() {
                    for x in a..e {
                        let mut out = src;

                        if id_test && !self.id_compare_passes(x, y) {
                            continue;
                        }
                        if logic {
                            out = logic_op(op, src, self.buffer_load(b, x, y));
                            out = if is_index { out & ODSY_INDEX_MASK } else { out | 0xff };
                        }
                        self.buffer_store(b, x, y, out);
                    }
                    continue;
                }
                plane.encode(src, &mut word);
                for x in a..e {
                    let at = ((x - a) as usize) * bpp as usize;
                    let px = unsafe { std::slice::from_raw_parts_mut(row.0.add(at), bpp as usize) };

                    if id_test && !self.id_compare_passes(x, y) {
                        continue;
                    }
                    if !logic {
                        px.copy_from_slice(&word[..bpp as usize]);
                        continue;
                    }
                    let out = logic_op(op, src, plane.decode(px));
                    plane.encode(if is_index { out & ODSY_INDEX_MASK } else { out | 0xff }, px);
                }
            }
        }
    }

    /// odyssey_fillrect(): BEGIN(QUADS)/COLOR_3UB or COLOR_INDEX/four VERTEX_2I
    /// corners/END drives the solid rectangle fill. Board::gl_end() routes a
    /// QUADS fill here when pattern mode is off, and to Board::pattern_fill()
    /// when it is on. is_index selects whether value is a packed RGB pixel or a
    /// PseudoColor index resolved per pixel.
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, is_index: bool, value: u32) {
        let mut runs = RowRuns::default();

        for row in 0..h {
            self.row_runs(y as i64 + row as i64, &mut runs);
            self.paint_integer_span(&runs, y as i64 + row as i64, x as i64, x as i64 + w as i64, is_index, value);
        }
        self.scanout.invalidate_all();
    }

    /// Board::clear_planes(): the X server's write to the id-plane destination.
    /// For each pixel of the rectangle, replace the plane bits the 0x1023 mask
    /// selects with the value the COLOR_INDEX word carried, in the id byte when
    /// the drawing mask selects buffer 0 and in the overlay pixel byte when it
    /// selects buffer 1, leaving the colour framebuffer untouched.
    pub fn clear_planes(&mut self, x: u32, y: u32, w: u32, h: u32, value: u8, mask: u8) {
        let buffers = self.draw_buffers();
        let id_test = self.id_compare_active();
        let mut runs = RowRuns::default();

        for row in 0..h {
            let py = y as i64 + row as i64;

            self.row_runs(py, &mut runs);
            for r in 0..runs.n as usize {
                let a = (x as i64).max(runs.start[r]);
                let e = (x as i64 + w as i64).min(runs.end[r]);

                for px in a..e {
                    if !id_test || self.id_compare_passes(px, py) {
                        self.aux_store(py as u64 * SGI_ODYSSEY_WIDTH as u64 + px as u64, buffers, value, mask);
                    }
                }
            }
        }
        // A pixel's drawable picks its colour at scanout.
        self.scanout.invalidate_all();
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
    /// Approximation, inferred: a destination pixel passes Board::pixel_in_bounds()
    /// only, not the window-clip rectangles Board::pixel_allowed() also tests.
    pub fn copy_rect_buffer(&mut self, source: u32, dest: u32) {
        let sx = self.copy_src & 0xffff;
        let sy = (self.copy_src >> 16) & 0xffff;
        let w = self.copy_size & 0xffff;
        let h = (self.copy_size >> 16) & 0xffff;
        let dx = self.copy_dst & 0xffff;
        let dy = (self.copy_dst >> 16) & 0xffff;
        let w_clip = if dx < SGI_ODYSSEY_WIDTH { w.min(SGI_ODYSSEY_WIDTH - dx) } else { 0 };
        let h_clip = if dy < SGI_ODYSSEY_HEIGHT { h.min(SGI_ODYSSEY_HEIGHT - dy) } else { 0 };
        let reverse_rows = dy > sy;
        let reverse_cols = dx > sx;

        for row in 0..h_clip {
            let sr = if reverse_rows { h_clip - 1 - row } else { row };
            let mut src_plane: P<Plane> = P::null();
            let mut dst_plane: P<Plane> = P::null();
            let mut src_bpp = 0u32;
            let mut dst_bpp = 0u32;
            let src_row = if w_clip != 0 {
                self.buffer_row(source, sx as i64, sy as i64 + sr as i64, w_clip as i64, &mut src_plane, &mut src_bpp)
            } else {
                P::null()
            };
            let dst_row = if !src_row.is_null() {
                self.buffer_row(dest, dx as i64, dy as i64 + sr as i64, w_clip as i64, &mut dst_plane, &mut dst_bpp)
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
                let pixel = self.buffer_load(source, sx.wrapping_add(sc) as i64, sy.wrapping_add(sr) as i64);

                if self.pixel_in_bounds(dx.wrapping_add(sc) as i64, dy.wrapping_add(sr) as i64) {
                    self.buffer_store(dest, dx.wrapping_add(sc) as i64, dy.wrapping_add(sr) as i64, pixel);
                }
            }
        }
    }

    pub fn copy_rect(&mut self) {
        let source = (self.cmd_operand[2] >> ODSY_COPY_RECT_SOURCE_SHIFT) & 0xf;
        let dest = (self.cmd_operand[7] >> ODSY_COPY_RECT_DEST_SHIFT) & 0xf;
        let format_source = (self.cmd_operand[4] >> ODSY_COPY_RECT_FORMAT_SOURCE_SHIFT) & 0xf;
        let format_dest = (self.cmd_operand[4] >> ODSY_COPY_RECT_DEST_SHIFT) & 0xf;

        if self.cmd_operand[0] == ODSY_DMA_SENTINEL_READ {
            if !self.copy_rect_to_object() {
                self.cmd_unimplemented(ODSY_CMD_COPY_RECT_TRIGGER);
            }
            return;
        }

        if source != format_source || dest != format_dest {
            self.cmd_unimplemented(ODSY_CMD_COPY_RECT_TRIGGER);
            return;
        }

        // The two observed copy blocks select 0 then 4 in both their source and
        // destination fields. This is an experimental model policy: the selector
        // values choose model buffers 0 and 1, but do not identify physical
        // memory banks. Source and destination choices stay separate so a copy
        // never changes another colour buffer.
        if (source == 0 || source == 4) && (dest == 0 || dest == 4) {
            self.copy_rect_buffer(source >> 2, dest >> 2);
        } else if source == dest && (source == 2 || source == 3) {
            // Existing public and modelled templates retain draw-mask behaviour.
            let buffers = self.draw_buffers();

            for b in 0..ODSY_BUFFERS {
                if buffers & (1u32 << b) != 0 {
                    self.copy_rect_buffer(b, b);
                }
            }
        } else {
            self.cmd_unimplemented(ODSY_CMD_COPY_RECT_TRIGGER);
            return;
        }
        self.scanout.invalidate_all();
    }

    /// Pattern mode is on when the 0x0404 mode's bits 3,4 are set and a shape is
    /// selected. A quad drawn in this state paints the current colour through the
    /// pattern instead of as a solid rectangle.
    pub fn pattern_enabled(&mut self) -> bool {
        if (self.pattern_mode & ODSY_PATTERN_MODE_ENABLE) != ODSY_PATTERN_MODE_ENABLE {
            return false;
        }
        matches!(
            self.pattern_shape & !ODSY_PATTERN_SHAPE_OPAQUE,
            ODSY_PATTERN_SHAPE_64X16 | ODSY_PATTERN_SHAPE_32X32 | ODSY_PATTERN_SHAPE_16X64
        )
    }

    /// The pattern bit at quad-relative column u, row v, for the current shape.
    /// Row 0 column 0 is the quad's top-left vertex and bit 31 of a row's first
    /// word is its leftmost pixel. A coordinate past the shape's own extent is
    /// treated as clear: the X server never draws a quad larger than its pattern.
    pub fn pattern_bit(&mut self, u: u32, v: u32) -> bool {
        let word;
        let bit;

        match self.pattern_shape & !ODSY_PATTERN_SHAPE_OPAQUE {
            ODSY_PATTERN_SHAPE_64X16 => {
                if u >= 64 || v >= 16 {
                    return false;
                }
                word = self.pattern_buffer[(2 * v + (u >> 5)) as usize];
                bit = 31 - (u & 31);
            }
            ODSY_PATTERN_SHAPE_32X32 => {
                if u >= 32 || v >= 32 {
                    return false;
                }
                word = self.pattern_buffer[v as usize];
                bit = 31 - u;
            }
            ODSY_PATTERN_SHAPE_16X64 => {
                if u >= 16 || v >= 64 {
                    return false;
                }
                word = self.pattern_buffer[(v >> 1) as usize];
                bit = 31 - 16 * (v & 1) - u;
            }
            _ => return false,
        }
        (word >> bit) & 1 != 0
    }

    /// The opaque pattern's background: the 0x0c21 constant colour, an index from
    /// operand 1 bits 23:12, or an RGB pixel from the 12-bit red, green and blue
    /// fields narrowed to 8 bits.
    pub fn pattern_background(&mut self, is_index: bool) -> u32 {
        let r = (self.const_colour[0] >> 12) & 0xfff;
        let g = self.const_colour[0] & 0xfff;
        let b = (self.const_colour[1] >> 12) & 0xfff;

        if is_index {
            return r;
        }
        pack_pixel(r >> 4, g >> 4, b >> 4)
    }

    /// A pattern quad: paint the current colour at each pixel whose pattern bit is
    /// set, and the background at each clear one when the pattern is opaque,
    /// leaving the rest as it was otherwise. Only the rows and columns the quad
    /// covers are read, so the stale part of the persistent pattern buffer beyond
    /// the tile never contributes.
    pub fn pattern_fill(&mut self, x: u32, y: u32, w: u32, h: u32, pat_u0: u32, pat_v0: u32, is_index: bool, value: u32) {
        let opaque = self.pattern_shape & ODSY_PATTERN_SHAPE_OPAQUE != 0;
        let background = self.pattern_background(is_index);
        let mut runs = RowRuns::default();

        // Each row as spans of equal pattern bits, painted a span at a time.
        for v in 0..h {
            let py = y as i64 + v as i64;

            self.row_runs(py, &mut runs);
            if runs.n == 0 {
                continue;
            }
            let mut u = 0u32;
            while u < w {
                let bit = self.pattern_bit(pat_u0.wrapping_add(u), pat_v0.wrapping_add(v));
                let first = u;

                loop {
                    u += 1;
                    if !(u < w && self.pattern_bit(pat_u0.wrapping_add(u), pat_v0.wrapping_add(v)) == bit) {
                        break;
                    }
                }
                if bit || opaque {
                    self.paint_integer_span(
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
        self.scanout.invalidate_all();
    }
}

/// A raster-box coordinate arrives as an IEEE-754 single the X server built
/// with an integer-to-float convert, so the 32-bit operand is a float bit
/// pattern, not an integer. Truncate toward zero; a NaN or negative clamps to
/// the screen origin.
pub fn float_to_u32(bits: u32) -> u32 {
    let f = f32::from_bits(bits);

    if !(f > 0.0f32) {
        return 0;
    }
    f as u32
}

impl Board {
    /// The index of the object at board base base, or -1. A base names at most
    /// one live object (Board::object_select()), so the index's entry is the one
    /// a search of obj_base would find.
    pub fn object_find(&mut self, base: u32) -> i32 {
        if base == 0 {
            return -1;
        }
        match self.obj_index.get(&base) {
            Some(slot) if *slot != 0 => (*slot - 1) as i32,
            _ => -1,
        }
    }

    /// Give object i the base base, keeping obj_index in step.
    pub fn object_set_base(&mut self, i: i32, base: u32) {
        let i = i as usize;

        if self.obj_base[i] != 0 {
            self.obj_index.remove(&self.obj_base[i]);
        }
        self.obj_base[i] = base;
        if base != 0 {
            self.obj_index.insert(base, i as u32 + 1);
        }
    }

    /// Rebuild obj_index from obj_base, after a load. Where a loaded table names
    /// one base twice, the lower index wins, as a search would find it.
    pub fn object_index_rebuild(&mut self) {
        self.obj_index.clear();
        for i in (0..self.obj_count.min(SGI_ODYSSEY_OBJECTS) as usize).rev() {
            if self.obj_base[i] != 0 {
                self.obj_index.insert(self.obj_base[i], i as u32 + 1);
            }
        }
    }

    pub fn object_word_valid(&self, word: u64) -> bool {
        word < ODSY_OBJ_POOL_WORDS as u64 && (self.obj_valid[(word >> 3) as usize] & (1u8 << (word & 7))) != 0
    }

    pub fn object_word_set_valid(&mut self, word: u64, valid: bool) {
        let mask = 1u8 << (word & 7);

        assert!(word < ODSY_OBJ_POOL_WORDS as u64);
        if valid {
            self.obj_valid[(word >> 3) as usize] |= mask;
        } else {
            self.obj_valid[(word >> 3) as usize] &= !mask;
        }
    }

    pub fn object_valid_range(&mut self, first: u64, count: u64, valid: bool) {
        for i in 0..count {
            self.object_word_set_valid(first + i, valid);
        }
    }

    pub fn object_valid_move(&mut self, source: u64, dest: u64, count: u64) {
        if dest > source {
            for i in (1..=count).rev() {
                let v = self.object_word_valid(source + i - 1);

                self.object_word_set_valid(dest + i - 1, v);
            }
        } else {
            for i in 0..count {
                let v = self.object_word_valid(source + i);

                self.object_word_set_valid(dest + i, v);
            }
        }
    }

    pub fn object_region_written(&self, object: i32, x: u32, y: u32, width: u32, height: u32) -> bool {
        let o = object as usize;

        if x > self.obj_width[o] || width > self.obj_width[o] - x || y > self.obj_rows[o] || height > self.obj_rows[o] - y {
            return false;
        }
        for dy in 0..height {
            let row = self.obj_offset[o] as u64 + (y + dy) as u64 * self.obj_width[o] as u64 + x as u64;

            for dx in 0..width {
                if !self.object_word_valid(row + dx as u64) {
                    return false;
                }
            }
        }
        true
    }

    /// Mark object i as the most recently used.
    pub fn object_touch(&mut self, i: i32) {
        self.obj_clock += 1;
        self.obj_stamp[i as usize] = self.obj_clock;
    }

    /// Forget object i: its slot stays, with base 0, for a later object.
    pub fn object_forget(&mut self, i: i32) {
        self.object_set_base(i, 0);
        let i = i as usize;
        self.obj_width[i] = 0;
        self.obj_rows[i] = 0;
        self.obj_offset[i] = 0;
        self.obj_stamp[i] = 0;
    }

    /// The least recently used live object other than keep, or -1.
    pub fn object_oldest(&mut self, keep: i32) -> i32 {
        let mut oldest: i32 = -1;

        for i in 0..self.obj_count as usize {
            if i as i32 != keep && self.obj_base[i] != 0 && (oldest < 0 || self.obj_stamp[i] < self.obj_stamp[oldest as usize]) {
                oldest = i as i32;
            }
        }
        oldest
    }

    /// Pack the objects that hold rows to the front of the store, in store order,
    /// so the space between them is reclaimed, then object last (when not -1)
    /// after them all, so that it can grow in place. Indices do not change.
    pub fn object_compact(&mut self, last: i32) {
        let last_size: u64 = if last >= 0 { self.obj_width[last as usize] as u64 * self.obj_rows[last as usize] as u64 } else { 0 };
        let mut held: Vec<u8> = Vec::new();
        let mut held_valid: Vec<bool> = Vec::new();
        let mut next: u64 = 0;

        // Packing the others may overwrite last's words: keep them aside.
        if last_size != 0 {
            let off = self.obj_offset[last as usize] as u64;

            held = self.obj_mem.bytes[(off * 4) as usize..((off + last_size) * 4) as usize].to_vec();
            for k in 0..last_size {
                held_valid.push(self.object_word_valid(off + k));
            }
        }
        loop {
            let mut pick: i32 = -1;

            // The lowest-placed object not yet packed: at or past next.
            for i in 0..self.obj_count as usize {
                if i as i32 != last
                    && self.obj_base[i] != 0
                    && self.obj_rows[i] != 0
                    && self.obj_offset[i] as u64 >= next
                    && (pick < 0 || self.obj_offset[i] < self.obj_offset[pick as usize])
                {
                    pick = i as i32;
                }
            }
            if pick < 0 {
                break;
            }
            let p = pick as usize;
            let size = self.obj_width[p] as u64 * self.obj_rows[p] as u64;
            if self.obj_offset[p] as u64 != next {
                let from = self.obj_offset[p] as usize * 4;
                self.obj_mem.bytes.copy_within(from..from + size as usize * 4, next as usize * 4);
                self.object_valid_move(self.obj_offset[p] as u64, next, size);
                self.obj_offset[p] = next as u32;
            }
            next += size;
        }
        if last_size != 0 {
            let at = next as usize * 4;
            self.obj_mem.bytes[at..at + held.len()].copy_from_slice(&held);
            for k in 0..last_size {
                self.object_word_set_valid(next + k, held_valid[k as usize]);
            }
            self.obj_offset[last as usize] = next as u32;
            next += last_size;
        }
        // Growth in place assumes the words past the store's end are zero.
        let tail = (self.obj_used as u64).saturating_sub(next);
        let at = next as usize * 4;
        self.obj_mem.bytes[at..at + tail as usize * 4].fill(0);
        self.object_valid_range(next, tail, false);
        self.obj_used = next as u32;
    }

    /// The object at base, created with the given row length in pixels when it
    /// does not exist; a new row length for an existing object discards its
    /// storage (its layout changed). When the table is full, the least recently
    /// used object is forgotten, reported once.
    pub fn object_select(&mut self, base: u32, width: u32) -> i32 {
        let mut i = self.object_find(base);

        if i < 0 {
            for j in 0..self.obj_count as usize {
                if self.obj_base[j] == 0 {
                    i = j as i32;
                    break;
                }
            }
        }
        if i < 0 && self.obj_count == SGI_ODYSSEY_OBJECTS {
            i = self.object_oldest(-1);
            if !self.obj_full_reported {
                self.obj_full_reported = true;
                crate::dlog!(
                    LogModule::Odyssey,
                    "sgi-odyssey: more than {} board-memory objects; the least recently used, {:#x}, is forgotten",
                    SGI_ODYSSEY_OBJECTS,
                    self.obj_base[i as usize]
                );
            }
            self.object_forget(i);
        }
        if i < 0 || self.obj_base[i as usize] == 0 {
            if i < 0 {
                i = self.obj_count as i32;
                self.obj_count += 1;
            }
            self.object_set_base(i, base);
            self.obj_width[i as usize] = width;
            self.obj_offset[i as usize] = 0;
            self.obj_rows[i as usize] = 0;
        } else if width != 0 && self.obj_width[i as usize] != width {
            self.obj_width[i as usize] = width;
            self.obj_rows[i as usize] = 0;
        }
        self.object_touch(i);
        i
    }

    /// Make room for rows rows in object i: grow it in place when it is the last
    /// in the store, else move it to the end. When there is no room, forget the
    /// least recently used other objects until the live objects and i's new size
    /// fit, reporting the first object forgotten, then pack the store once with i
    /// last. False, reported once, when i alone does not fit.
    pub fn object_rows(&mut self, i: i32, rows: u32) -> bool {
        let iu = i as usize;
        let pool = ODSY_OBJ_POOL_WORDS as u64;
        let need = self.obj_width[iu] as u64 * rows as u64;
        let have = self.obj_width[iu] as u64 * self.obj_rows[iu] as u64;

        if rows <= self.obj_rows[iu] {
            return true;
        }
        self.object_touch(i);
        if need <= pool
            && !(have != 0 && self.obj_offset[iu] as u64 + have == self.obj_used as u64 && self.obj_offset[iu] as u64 + need <= pool)
            && self.obj_used as u64 + need > pool
        {
            let mut live: u64 = 0;

            for j in 0..self.obj_count as usize {
                if j != iu && self.obj_base[j] != 0 {
                    live += self.obj_width[j] as u64 * self.obj_rows[j] as u64;
                }
            }
            while live + need > pool {
                let victim = self.object_oldest(i);

                if victim < 0 {
                    break;
                }
                let v = victim as usize;
                live -= self.obj_width[v] as u64 * self.obj_rows[v] as u64;
                if !self.obj_forget_reported {
                    self.obj_forget_reported = true;
                    crate::dlog!(
                        LogModule::Odyssey,
                        "sgi-odyssey: the model's {} MiB board-memory store is full; the least recently used object, {:#x}, is forgotten",
                        ODSY_OBJ_POOL_BYTES / (1024 * 1024),
                        self.obj_base[v]
                    );
                }
                self.object_forget(victim);
            }
            self.object_compact(if have != 0 { i } else { -1 });
        }
        if have != 0 && self.obj_offset[iu] as u64 + have == self.obj_used as u64 && self.obj_offset[iu] as u64 + need <= pool {
            self.obj_used = (self.obj_offset[iu] as u64 + need) as u32;
            self.object_valid_range(self.obj_offset[iu] as u64 + have, need - have, false);
        } else if self.obj_used as u64 + need <= pool {
            let new_offset = self.obj_used;
            let from = self.obj_offset[iu] as usize * 4;

            self.obj_mem.bytes.copy_within(from..from + have as usize * 4, new_offset as usize * 4);
            let z = (new_offset as usize + have as usize) * 4;
            self.obj_mem.bytes[z..z + (need - have) as usize * 4].fill(0);
            self.object_valid_move(self.obj_offset[iu] as u64, new_offset as u64, have);
            self.object_valid_range(new_offset as u64 + have, need - have, false);
            self.obj_offset[iu] = new_offset;
            self.obj_used += need as u32;
        } else {
            if !self.obj_full_reported {
                self.obj_full_reported = true;
                crate::dlog!(
                    LogModule::Odyssey,
                    "sgi-odyssey: board-memory object {:#x} ({} x {}) exceeds the model's {} MiB store",
                    self.obj_base[iu],
                    self.obj_width[iu],
                    rows,
                    ODSY_OBJ_POOL_BYTES / (1024 * 1024)
                );
            }
            return false;
        }
        self.obj_rows[iu] = rows;
        true
    }

    /// Object i's texel (x, y) as 0xRRGGBBAA; outside its rows or width, dropped.
    pub fn object_store(&mut self, i: i32, x: u32, y: u32, rgba: u32) {
        if x >= self.obj_width[i as usize] || !self.object_rows(i, y.wrapping_add(1)) {
            return;
        }
        let word = self.obj_offset[i as usize] as u64 + y as u64 * self.obj_width[i as usize] as u64 + x as u64;
        self.obj_mem.bytes[(word * 4) as usize..][..4].copy_from_slice(&(rgba as u32).to_be_bytes());
        self.object_word_set_valid(word, true);
    }

    pub fn object_load(&mut self, i: i32, x: u32, y: u32) -> u32 {
        let iu = i as usize;

        if x >= self.obj_width[iu] || y >= self.obj_rows[iu] {
            return 0;
        }
        u32::from_be_bytes(self.obj_mem.bytes[((self.obj_offset[iu] as u64 + y as u64 * self.obj_width[iu] as u64 + x as u64) * 4) as usize..][..4].try_into().unwrap())
    }

    /// Copy a recognised GL read-drawable rectangle into its scratch object.
    pub fn copy_rect_to_object(&mut self) -> bool {
        let w = self.copy_size & 0xffff;
        let h = self.copy_size >> 16;
        let dx = self.copy_dst & 0xffff;
        let dy = self.copy_dst >> 16;
        let sx = (self.copy_src & 0xffff) as i64 + self.read_origin[0] as i64;
        let sy = (self.copy_src >> 16) as i64 + self.read_origin[1] as i64;
        let fmt = self.cmd_operand[4];
        let tail = self.cmd_operand[7];
        let rgba8 = fmt == 0x92233002 && tail == 0x3801210b;
        let rgba16 = (fmt & !ODSY_DMA_FMT_FRONT) == 0x12633042 && (tail == 0x3802204b || tail == 0xf802204b);
        let obj = self.object_find(self.cmd_operand[5]);

        if self.cmd_operand[0] != ODSY_DMA_SENTINEL_READ
            || self.cmd_operand[2] != 0x80200050
            || (!rgba8 && !rgba16)
            || self.cmd_operand[5] != self.dest_base
            || obj < 0
            || w == 0
            || h == 0
            || dx as u64 + w as u64 > self.obj_width[obj as usize] as u64
            || sx < 0
            || sy < 0
            || sx + w as i64 > SGI_ODYSSEY_WIDTH as i64
            || sy + h as i64 > SGI_ODYSSEY_HEIGHT as i64
            || !self.object_rows(obj, dy.wrapping_add(h))
        {
            return false;
        }
        let displayed = self.front.lock().displayed_buffer;
        for y in 0..h {
            for x in 0..w {
                let shown = (displayed >> self.main_id((sx + x as i64) as u32, (sy + y as i64) as u32)) & 1;
                let source = if fmt & ODSY_DMA_FMT_FRONT != 0 { shown } else { shown ^ 1 };
                let px = self.buffer_load(source, sx + x as i64, sy + y as i64);

                self.object_store(obj, dx + x, dy + y, px);
            }
        }
        true
    }

    /// The object an image transfer writes, or -1 for the screen: the last 0x1017
    /// select, when it named a base other than the colour buffer's.
    pub fn image_object(&mut self) -> i32 {
        if self.dest_base == 0 || self.dest_base == self.dma_colour_base || self.dest_is_id_planes() {
            return -1;
        }
        self.object_find(self.dest_base)
    }

    /// Lay one image pixel, as 0xRRGGBBAA, at the raster cursor in the selected
    /// buffer (Board::image_object()) and advance it. Each axis is a directed
    /// half-open interval: increasing endpoints visit [first, last), while
    /// decreasing endpoints visit [first - 1, last]. Payload advances along X,
    /// then along Y.
    pub fn image_put(&mut self, rgba: u32, is_index: bool, preserve_alpha: bool) {
        let x_decreasing = self.image_x2 < self.image_x1;
        let y_decreasing = self.image_y2 < self.image_y1;
        let y_finished = if y_decreasing {
            self.image_cur_y == u32::MAX || self.image_cur_y < self.image_y2
        } else {
            self.image_cur_y >= self.image_y2
        };

        if self.image_x2 == self.image_x1 || self.image_y2 == self.image_y1 || y_finished {
            return;
        }
        let obj = self.image_object();
        if obj >= 0 {
            // An off-screen object: box coordinates, no window or id tests.
            self.object_store(obj, self.image_cur_x, self.image_cur_y, rgba);
        } else if self.dest_is_id_planes() {
            // Menu cascade arrows: index images into the overlay.
            if is_index {
                self.paint_aux(
                    self.image_cur_x as i64 + self.win_origin[0] as i64,
                    self.image_cur_y as i64 + self.win_origin[1] as i64,
                    rgba,
                    SGI_LOGIC_COPY as u8,
                );
            } else if !self.aux_image_reported {
                self.aux_image_reported = true;
                crate::dlog!(LogModule::Odyssey, "sgi-odyssey: an RGB image to the id-plane destination is not modeled; dropped");
            }
        } else {
            // Screen pixels are window-relative; object pixels are not.
            self.paint_pixel(
                self.image_cur_x as i64 + self.win_origin[0] as i64,
                self.image_cur_y as i64 + self.win_origin[1] as i64,
                if is_index || preserve_alpha { rgba } else { (rgba & !0xffu32) | 0xff },
            );
        }
        if (x_decreasing && self.image_cur_x == self.image_x2) || (!x_decreasing && self.image_cur_x == self.image_x2.wrapping_sub(1)) {
            self.image_cur_x = if x_decreasing { self.image_x1.wrapping_sub(1) } else { self.image_x1 };
            if y_decreasing {
                self.image_cur_y = if self.image_cur_y == self.image_y2 { u32::MAX } else { self.image_cur_y.wrapping_sub(1) };
            } else {
                self.image_cur_y = self.image_cur_y.wrapping_add(1);
            }
        } else if x_decreasing {
            self.image_cur_x = self.image_cur_x.wrapping_sub(1);
        } else {
            self.image_cur_x = self.image_cur_x.wrapping_add(1);
        }
        self.scanout.invalidate_all();
    }

    /// One byte-stream texel of the open image at the raster cursor, bounded by
    /// the header's group length in pixels.
    pub fn image_texel(&mut self, rgba: u32, is_index: bool) {
        if self.image_group_len != 0 && self.image_group_painted >= self.image_group_len {
            return;
        }
        self.image_put(rgba, is_index, false);
        self.image_group_painted += 1;
    }
}

pub fn unorm16_to_8(v: u32) -> u32 {
    (v.wrapping_mul(255).wrapping_add(65535 / 2)) / 65535
}

/// Components of a 12-bit texture source code, or 0 for any other code.
pub fn texel12_components(code: u32) -> u32 {
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
pub fn decode_texel12(code: u32, b: &[u8], wide: bool) -> u32 {
    let mut c = [0u32; 4];
    let n = texel12_components(code) as usize;

    for i in 0..n {
        c[i] = if wide { unorm16_to_8(u16::from_be_bytes(b[2 * i..][..2].try_into().unwrap()) as u32) } else { b[i] as u32 };
    }
    match code {
        ODSY_IMAGE_CODE_LUM12 => pack_pixel(c[0], c[0], c[0]),
        ODSY_IMAGE_CODE_LUM_ALPHA12 => (pack_pixel(c[0], c[0], c[0]) & !0xffu32) | c[1],
        ODSY_IMAGE_CODE_RGB12 => pack_pixel(c[0], c[1], c[2]),
        ODSY_IMAGE_CODE_ABGR12 => (pack_pixel(c[3], c[2], c[1]) & !0xffu32) | c[0],
        _ => (pack_pixel(c[0], c[1], c[2]) & !0xffu32) | c[3],
    }
}

impl Board {
    /// Decode one pixel of a byte-stream source format whose bytes are b[0 ..]:
    /// its 0xRRGGBBAA word, or for 0x41 and 0x61 the raw index. False for a code
    /// this model does not know.
    pub fn decode_pixel(&mut self, code: u32, b: &[u8], rgba: &mut u32) -> bool {
        match code {
            ODSY_IMAGE_CODE_ABGR => {
                *rgba = pack_pixel(b[3] as u32, b[2] as u32, b[1] as u32);
                true
            }
            ODSY_IMAGE_CODE_FALLBACK_XRGB => {
                *rgba = pack_pixel(b[1] as u32, b[2] as u32, b[3] as u32);
                true
            }
            ODSY_IMAGE_CODE_RGBA | ODSY_IMAGE_CODE_RGBA12 => {
                *rgba = u32::from_be_bytes(b[..4].try_into().unwrap());
                true
            }
            ODSY_IMAGE_CODE_RGB => {
                *rgba = pack_pixel(b[0] as u32, b[1] as u32, b[2] as u32);
                true
            }
            ODSY_IMAGE_CODE_LUM => {
                *rgba = pack_pixel(b[0] as u32, b[0] as u32, b[0] as u32);
                true
            }
            ODSY_IMAGE_CODE_LUM_ALPHA => {
                *rgba = (pack_pixel(b[0] as u32, b[0] as u32, b[0] as u32) & !0xffu32) | b[1] as u32;
                true
            }
            ODSY_IMAGE_CODE_LUM12 | ODSY_IMAGE_CODE_LUM_ALPHA12 => {
                // A burst's 16-bit components (pixel_bytes()).
                *rgba = decode_texel12(code, b, true);
                true
            }
            ODSY_IMAGE_CODE_CI => {
                // The raw index: scanout resolves it through the pixel's drawable.
                *rgba = b[0] as u32;
                true
            }
            ODSY_IMAGE_CODE_CI16 => {
                *rgba = u16::from_be_bytes(b[..2].try_into().unwrap()) as u32 & ODSY_INDEX_MASK;
                true
            }
            _ => false,
        }
    }
}

/// Whether a source format carries colour indices rather than colours.
pub fn code_is_index(code: u32) -> bool {
    code == ODSY_IMAGE_CODE_CI || code == ODSY_IMAGE_CODE_CI16
}

/// Bytes a pixel of a source format takes, 0 for an unknown code.
pub fn pixel_bytes(code: u32) -> u32 {
    match code {
        ODSY_IMAGE_CODE_ABGR | ODSY_IMAGE_CODE_FALLBACK_XRGB | ODSY_IMAGE_CODE_RGBA | ODSY_IMAGE_CODE_RGBA12 => 4,
        ODSY_IMAGE_CODE_RGB => 3,
        ODSY_IMAGE_CODE_LUM | ODSY_IMAGE_CODE_CI => 1,
        ODSY_IMAGE_CODE_LUM_ALPHA | ODSY_IMAGE_CODE_LUM12 | ODSY_IMAGE_CODE_CI16 => 2,
        ODSY_IMAGE_CODE_LUM_ALPHA12 => 4,
        _ => 0,
    }
}

pub fn image_pairing_supported(code: u32, fmt: u32) -> bool {
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

impl Board {
    /// One data word of a pixel burst, laid out by the transfer header's format
    /// code: its bytes join a stream that yields a pixel every
    /// pixel_bytes() bytes, so a three-byte pixel may span two words. A
    /// bounded group's trailing padding (past image_group_len) is dropped with any
    /// partial pixel, so the raster position stays exact. A header with a code
    /// this model does not know falls back on the burst tag's format, 0x11 one
    /// 0x00RRGGBB pixel a word and 0x10 four indices, and is reported once.
    pub fn image_burst_word(&mut self, word: u32) {
        let mut code = self.image_code;
        let mut n = pixel_bytes(code);
        let mut b = [0u8; 4];

        if !image_pairing_supported(code, self.image_fmt) {
            return;
        }
        if self.image_fmt == ODSY_IMAGE_FMT_RGBA16 {
            // A stream of 16-bit components, high half first, a pixel every
            // texel12_components() halves: two words for 0x6b and 0x6c, a
            // word and a half for 0x69. At most two halves wait between words;
            // image_nbytes counts them, image_bytes holds them.
            let comps = texel12_components(code) as usize;
            let mut halves = [0u16; 4];
            let mut held = self.image_nbytes.min(2) as usize;

            if self.image_group_len != 0 && self.image_group_painted >= self.image_group_len {
                return;
            }
            for i in 0..held {
                halves[i] = (self.image_bytes >> (16 * (held - 1 - i))) as u16;
            }
            for h in 0..2 {
                let mut px = [0u8; 8];

                halves[held] = if h != 0 { word as u16 } else { (word >> 16) as u16 };
                held += 1;
                if held < comps {
                    continue;
                }
                for i in 0..comps {
                    px[2 * i..][..2].copy_from_slice(&(halves[i] as u16).to_be_bytes());
                }
                self.image_put(decode_texel12(code, &px, true), false, true);
                held = 0;
            }
            self.image_bytes = if held > 1 {
                ((halves[0] as u32) << 16) | halves[1] as u32
            } else if held != 0 {
                halves[0] as u32
            } else {
                0
            };
            self.image_nbytes = held as u32;
            self.image_group_painted += 1;
            if self.image_group_len != 0 && self.image_group_painted >= self.image_group_len && self.image_nbytes != 0 {
                self.image_bytes = 0;
                self.image_nbytes = 0;
            }
            return;
        }

        if n == 0 {
            if !self.image_code_reported {
                self.image_code_reported = true;
                crate::dlog!(LogModule::Odyssey, "sgi-odyssey: image source format {:#x} is not modeled; laid out by the burst tag", code);
            }
            code = if self.image_fmt == ODSY_IMAGE_FMT_RGB { ODSY_IMAGE_CODE_FALLBACK_XRGB } else { ODSY_IMAGE_CODE_CI };
            n = pixel_bytes(code);
        }
        b[..4].copy_from_slice(&(word as u32).to_be_bytes());
        for i in 0..4 {
            let mut px = [0u8; 4];
            let mut rgba = 0u32;

            self.image_bytes = (self.image_bytes << 8) | b[i] as u32;
            self.image_nbytes += 1;
            if self.image_nbytes < n {
                continue;
            }
            self.image_nbytes = 0;
            // The pixel's bytes, first byte highest, at the start of px[].
            px[..4].copy_from_slice(&(self.image_bytes << (8 * (4 - n))).to_be_bytes());
            self.decode_pixel(code, &px, &mut rgba);
            self.image_texel(rgba, code_is_index(code));
        }
    }
}

/// True, and set *fmt, when reg is a pixel-burst register-select word.
pub fn image_burst_fmt(reg: u32, fmt: &mut u32) -> bool {
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
/// Octane RE4 set is the only one RasterRules::rules_supported() implements,
/// and a flat single triangle in one colour exercises none of the parameters
/// that distinguish heads. The board's own subpixel grid and
/// interpolated-colour depth are unmeasured. The floating-point depth rule is
/// inferred, not measured. (A function, not a static: the struct is not const
/// constructible.)
pub fn gl_rules() -> RasterRules {
    RasterRules {
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
        // The board's own depth format; Board::gl_load_combined().
        depth_float: true,
        ..Default::default()
    }
}
