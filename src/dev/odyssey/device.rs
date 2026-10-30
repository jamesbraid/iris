//! Ported from hw/misc/sgi-odyssey.c in jamesbraid/qemu-sgi-origami.

use super::*;

impl SGIOdysseyState {
    pub fn write_dispatch(&mut self, addr: u64, value: u64, size: u32) {
        crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_mmio: seq {} offset {:#x} size {} value {:#x} write=1", self.trace_sequence, addr, size, value);
        self.trace_sequence = self.trace_sequence.wrapping_add(1);

        if addr == ODSY_BACKEND_SELECT && (size == 8 || size == 4) {
            self.backend_write(value, size);
            return;
        }
        if addr == ODSY_CTX_SWITCH_PORT && size == 8 {
            self.ctx_switch(value);
            return;
        }
        if addr == ODSY_CMD_FIFO {
            self.selftest_track(ODSY_CMD_FIFO, size, value);
            self.cmd_from_kernel = true;
            self.cmd_fifo_write(value, size);
            return;
        }
        if addr == ODSY_USER_CMD_FIFO {
            // The user-mapped command FIFO has the kernel port's grammar but
            // never carries the self-test preamble, so it skips the tracker.
            self.cmd_from_kernel = false;
            self.cmd_fifo_write(value, size);
            return;
        }

        // Registers that do not wait for the board's work live in the front
        // (front.rs).
        if odyssey_front_register(addr, size, true) {
            self.front.lock().write(addr, value, size);
            return;
        }

        // -1 (no register) becomes u64::MAX, which no constant matches.
        match odyssey_reg(addr, size) as u64 {
            ODSY_RESET_CONTROL => {
                crate::qlog!(
                    "sgi-odyssey: write of {:#x} to the reset control register accepted; its effect is unknown\n",
                    value
                );
            }
            ODSY_MEMORY_CONTROL => self.memory_control(value as u32),
            ODSY_FLCTL_CONFIG => self.flctl_config = value as u32,
            ODSY_FLCTL_TARGET => {
                self.flctl_target = value as u32;
                if !self.flctl_target_reported {
                    self.flctl_target_reported = true;
                    crate::qlog!(
                        "sgi-odyssey: flow-control interrupt target {} and credit target {} stored; credits still return through the node interface\n",
                        (value as u32) >> ODSY_FLCTL_INTR_TARGET_SHIFT,
                        ((value >> ODSY_FLCTL_TARGET_SHIFT) as u32) & ODSY_FLCTL_TARGET_MASK
                    );
                }
            }
            ODSY_BUZZ_1 => self.buzz[0] = value as u32,
            ODSY_BUZZ_2 => self.buzz[1] = value as u32,
            ODSY_BUZZ_3 => self.buzz[2] = value as u32,
            ODSY_BUZZ_4 => self.buzz[3] = value as u32,
            ODSY_BUZZ_5 => self.buzz[4] = value as u32,
            ODSY_BUZZ_6 => self.buzz[5] = value as u32,
            ODSY_NIC_PROM | ODSY_NIC_KERNEL => self.nic_slot(value as u32),
            ODSY_HOST_WRITE_ADDR_UPPER => {
                self.query_cancel();
                self.host_write_addr_upper = value as u32;
            }
            ODSY_HOST_WRITE_ADDR_LOWER => {
                self.query_cancel();
                self.host_write_addr_lower = value as u32;
            }
            ODSY_STATS_ADDR_UPPER | ODSY_STATS_ADDR_LOWER | ODSY_STATS_CONTROL => {
                if addr == ODSY_STATS_ADDR_UPPER {
                    self.stats_addr_upper = value as u32;
                } else if addr == ODSY_STATS_ADDR_LOWER {
                    self.stats_addr_lower = value as u32;
                } else {
                    self.stats_control = value as u32;
                }
                if value != 0 && !self.stats_reported {
                    self.stats_reported = true;
                    crate::qlog!(
                        "sgi-odyssey: statistics region word {:#x} at {:#x} stored; the board's writes into the region are not modeled\n",
                        value as u32,
                        addr
                    );
                }
            }
            ODSY_CTX_SHADOW_ADDR_UPPER => {
                self.query_cancel();
                self.ctx_shadow_addr_upper = value as u32;
            }
            ODSY_CTX_SHADOW_ADDR_LOWER => {
                self.query_cancel();
                self.ctx_shadow_addr_lower = value as u32;
            }
            ODSY_MEMORY_CONFIG
            | ODSY_I2C_MAIN_CMD_READ
            | ODSY_I2C_MAIN_STATUS_READ
            | ODSY_I2C_OPT_CMD_READ
            | ODSY_I2C_OPT_STATUS_READ
            | ODSY_BACKEND_VERSION => {
                crate::qlog!("sgi-odyssey: write of {:#x} to the read-only register at {:#x}\n", value, addr);
            }
            _ => self.unimplemented(addr, size, true),
        }
    }

    // Command work after the store.
    //
    // The board draws a primitive after the store that completes it has
    // finished: the processor goes on, and the driver learns of completion from
    // a host-write fence, the context-switch word in guest memory, or an
    // interrupt. A model that draws inside the store keeps the processor from
    // its scheduler tick for as long as the draw takes, and the guest does not
    // make up lost ticks.
    //
    // So every store that joins the command stream waits in a queue, in
    // order, and the board's thread (worker.rs) runs the queue in slices under
    // the board lock. Accesses that must see the drawing take the board lock
    // and run what is still queued first (odyssey_work_drain). With no thread
    // running (unit tests) the storing caller runs the store itself.
    //
    // The Octane has no PI link (only Hub and Bedrock machines have graphics
    // credits), so the front's `pi` stays None there and every credit call is
    // skipped.

    pub fn work_pending(&mut self) -> bool {
        self.front.lock().pending()
    }
}

pub fn odyssey_fifo_port(addr: u64) -> bool {
    addr == ODSY_USER_CMD_FIFO || addr == ODSY_CMD_FIFO
}

/// The stores that join the command stream: the two command-FIFO ports, and
/// the kernel's context switch, the shadow address it names and the switch
/// store itself.
pub fn odyssey_command_store(addr: u64, size: u32) -> bool {
    odyssey_fifo_port(addr)
        || (addr == ODSY_CTX_SWITCH_PORT && size == 8)
        || odyssey_reg(addr, size) == ODSY_CTX_SHADOW_ADDR_UPPER as i64
        || odyssey_reg(addr, size) == ODSY_CTX_SHADOW_ADDR_LOWER as i64
}

impl SGIOdysseyState {
    /// Run the oldest waiting stores until the budget is spent. The caller
    /// holds the board lock and not the front lock: the board's thread for a
    /// slice, or an access that drains.
    pub fn work_step(&mut self, b: &mut OdysseyBudget) {
        #[cfg(test)]
        {
            // A slice that holds the board as a long draw would.
            let sleep_ms = std::mem::take(&mut self.test_slice_sleep_ms);
            if sleep_ms != 0 {
                std::thread::sleep(std::time::Duration::from_millis(sleep_ms as u64));
            }
        }

        loop {
            let Some(q) = self.front.lock().work_take() else {
                break;
            };
            self.write_dispatch(q.addr as u64, q.value, q.size as u32);
            {
                let mut f = self.front.lock();
                let level = f.work_queue.len() as u32 + 1;
                f.fifo_level_changed(level);
            }
            if b.spent() {
                break;
            }
        }
        let f = self.front.lock();
        if !f.work_queue.is_empty() {
            crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_work_behind: {} stores wait, the oldest for {:?}", f.work_queue.len(), f.work_queue[0].queued_at.elapsed());
        }
    }

    /// Finish the waiting work now, for an access at addr that must see it. The
    /// caller holds the board lock, so no slice runs. The stores queued when
    /// the drain starts run here; any a processor adds meanwhile are left to
    /// the board's thread.
    pub fn work_drain(&mut self, addr: u64) {
        let waiting = self.front.lock().work_queue.len() as u32;
        if waiting == 0 {
            return;
        }
        let start = std::time::Instant::now();
        self.work_step(&mut OdysseyBudget::words(waiting));
        crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_work: drained offset {:#x} in {:?}", addr, start.elapsed());
    }
}

/// Accesses that need not wait for the queued stores, and act on the board
/// as it stands. Inferred from what a driver does while the board works,
/// not measured on a board; every other access waits.
pub fn odyssey_access_waits(addr: u64, size: u32, write: bool) -> bool {
    let cursor = ODSY_BACKEND_READ_BASE + ((ODSY_CURSOR_BASE as u64) << 3);

    if odyssey_fifo_port(addr) {
        return !write;
    }
    if odyssey_reg(addr, size) < 0 {
        // Outside the modeled registers only the back-end and
        // context-switch ports and the cursor do anything; the rest is
        // logged and answers zero, so it need not wait.
        if write {
            return addr == ODSY_BACKEND_SELECT || addr == ODSY_CTX_SWITCH_PORT;
        }
        return false;
    }
    if !write && addr >= cursor && addr < cursor + ((ODSY_CURSOR_ENTRIES as u64) << 3) {
        return false;
    }
    !matches!(
        odyssey_reg(addr, size) as u64,
        ODSY_IDENTITY
            | ODSY_STATUS
            | ODSY_ERROR_ADDR_UPPER
            | ODSY_ERROR_ADDR_LOWER
            | ODSY_CONTROL
            | ODSY_XT_REQ_TIMEOUT
            | ODSY_INT_DEST_UPPER
            | ODSY_INT_DEST_LOWER
            | ODSY_ERROR_COMMAND
            | ODSY_STATUS0
            | ODSY_INTR_FLAGS
            | ODSY_INTR_CLEAR
            | ODSY_INTR_ENABLE_SET
            | ODSY_INTR_ENABLE_CLEAR
            | ODSY_ROUTE_DEST_UPPER
            | ODSY_ROUTE_DEST_LOWER
            | ODSY_ROUTE_VECTORS
            | ODSY_SW_TOKEN
            | ODSY_SWAP_ARM
            | ODSY_SWAP_PENDING
            | ODSY_SWAP_CONTROL
            | ODSY_SWAP_CONTROL_COMPANION
            | ODSY_DISPLAYED_BUFFER
    )
}

impl SGIOdysseyState {
    /// A read with the board lock held.
    pub fn mmio_read(&mut self, addr: u64, size: u32) -> u64 {
        if odyssey_access_waits(addr, size, false) && self.work_pending() {
            self.work_drain(addr);
        }
        self.read(addr, size)
    }
}

/// Whether this store is counted: the processor interface's mark.
pub fn odyssey_store_counted(front: &parking_lot::Mutex<OdysseyFront>) -> bool {
    // The Octane has no PI link, so nothing sets the mark there.
    #[cfg(test)]
    if front.lock().test_counted_stores != 0 {
        return true;
    }
    let _ = front;
    sgi_pi_gfx_take_mark()
}

impl SGIOdysseyState {
    /// A store with the board lock held.
    pub fn mmio_write(&mut self, addr: u64, value: u64, size: u32) {
        let counted = odyssey_store_counted(&self.front);
        self.mmio_write_counted(addr, value, size, counted);
    }

    pub fn mmio_write_counted(&mut self, addr: u64, value: u64, size: u32, mut counted: bool) {
        // The queue decision and the append are one front section, so the
        // board's thread cannot go idle between them.
        let queued = {
            let mut f = self.front.lock();
            if f.store_queues(addr, size) && f.work_queue.len() < ODSY_WORK_QUEUE_MAX as usize {
                f.work_append(addr, value, size, counted);
                true
            } else {
                false
            }
        };
        if queued {
            // Credited as it enters the FIFO (odyssey_work_admit()). With no
            // board thread running the storing caller runs it.
            counted = false;
            let f = self.front.lock();
            let run_here = f.worker.is_none() && !f.queue_held();
            drop(f);
            if run_here {
                self.work_drain(addr);
            }
        } else if odyssey_command_store(addr, size) {
            // The queue is full: run what waits and then this store, in order.
            self.work_drain(addr);
            self.write_dispatch(addr, value, size);
        } else {
            if odyssey_access_waits(addr, size, true) && self.work_pending() {
                self.work_drain(addr);
            }
            self.write_dispatch(addr, value, size);
        }
        if counted {
            // It went through the FIFO inside the store.
            self.front.lock().return_credit();
        }
    }
}

impl OdysseyHandles {
    /// A processor's read: through the front alone when it can, else under the
    /// board lock.
    pub fn read(&self, addr: u64, size: u32) -> u64 {
        match odyssey_front_mmio_read(&self.front, addr, size) {
            Some(v) => v,
            None => self.board.lock().mmio_read(addr, size),
        }
    }

    /// A processor's store: through the front alone when it can, else under the
    /// board lock.
    pub fn write(&self, addr: u64, value: u64, size: u32) {
        let counted = odyssey_store_counted(&self.front);
        if !odyssey_front_mmio_write(&self.front, addr, value, size, counted) {
            self.board.lock().mmio_write_counted(addr, value, size, counted);
        }
    }
}

impl SGIOdysseyState {
    /// Reset clears the FIFO and abandons the waiting stores. The caller holds
    /// the board lock, so no slice runs.
    pub fn work_reset(&mut self) {
        let mut f = self.front.lock();
        let f = &mut *f;
        for q in f.work_queue.iter() {
            if q.owed {
                if let Some(pi) = &f.pi {
                    sgi_pi_gfx_owe_credits(pi.as_ref(), f.slot, -1);
                }
            }
        }
        f.work_queue.clear();
        f.work_in_fifo = 0;
    }


    pub fn fifo_page_read(&mut self, addr: u64, size: u32) -> u64 {
        self.mmio_read(addr, size)
    }

    pub fn fifo_page_write(&mut self, addr: u64, value: u64, size: u32) {
        self.mmio_write(addr, value, size)
    }

    /// The board-identity record. Its field spelling is the one captured off
    /// real firmware, and the prologue (a format byte, four spaces and a
    /// semicolon, so a scanning reader meets ";Name:" and a positional reader
    /// finds the value at byte 11) follows the Bridge model's convention.
    ///
    /// One field by default, because the stock PROM was observed reading
    /// exactly one 32-byte page and stopping; a longer record is truncated
    /// where the master stops clocking. Without a nic-record, a selected
    /// product supplies its board's name, and otherwise the default is the
    /// public owner record of the board the Odyssey contract describes.
    pub fn build_nic_rom(&mut self, rom: &mut [u8; ODSY_NIC_ROM_SIZE as usize]) {
        let fields: &str = if !self.nic_record.is_empty() {
            &self.nic_record
        } else if !self.product_record.is_empty() {
            &self.product_record
        } else {
            ODSY_NIC_RECORD_DEFAULT
        };
        let record = format!("\u{1}    ;{};", fields);
        let record = record.as_bytes();
        let length = record.len().min(ODSY_NIC_ROM_SIZE as usize);

        rom.fill(0xff);
        rom[..length].copy_from_slice(&record[..length]);
        if length < ODSY_NIC_PAGE as usize {
            rom[length..ODSY_NIC_PAGE as usize].fill(b' ');
        }
    }

    pub fn attach_nic(&mut self) -> Result<(), String> {
        let mut rom = [0u8; ODSY_NIC_ROM_SIZE as usize];

        if self.nic_serial & !ODSY_NIC_SERIAL_MASK != 0 {
            return Err(format!("the sgi-odyssey one-wire serial {:#x} does not fit in 48 bits", self.nic_serial));
        }
        self.build_nic_rom(&mut rom);
        let mut bus = crate::dev::w1::Bus::new();
        bus.add(Box::new(crate::dev::w1::Ds2505::new(ODSY_NIC_FAMILY as u8, self.nic_serial, &rom)));
        self.nic_bus = Some(bus);
        Ok(())
    }

    pub fn compose_memory_config(&mut self) -> Result<(), String> {
        let code: u32 = match self.memory_size {
            32 => 0,
            64 => 1,
            128 => 2,
            _ => {
                return Err(format!("sgi-odyssey memory-size must be 32, 64 or 128 megabytes, not {}", self.memory_size));
            }
        };
        if self.memory_banks != 2 && self.memory_banks != 4 {
            return Err(format!("sgi-odyssey memory-banks must be 2 or 4, not {}", self.memory_banks));
        }
        if self.cas_latency != 2 && self.cas_latency != 3 {
            return Err(format!("sgi-odyssey cas-latency must be 2 or 3, not {}", self.cas_latency));
        }
        // The outstanding-operation count and the data-FIFO level, which the
        // driver waits to reach zero, stay zero: neither is modeled.
        self.memory_config = code << ODSY_CONFIG_SIZE_SHIFT
            | (if self.memory_banks == 4 { ODSY_CONFIG_FOUR_BANKS } else { 0 })
            | (if self.cas_latency == 2 { ODSY_CONFIG_LATENCY_2 } else { 0 })
            | ODSY_CONFIG_COPY_FIELD;
        Ok(())
    }

    pub fn reset_registers(&mut self) {
        // The driver reads its own widget number out of the low nibble before
        // anything has written the register. The rest of the reset value is not
        // established, so it is zero.
        let identity = self.identity();
        let mut f = self.front.lock();
        f.slot = self.slot;
        f.identity = identity;
        f.widget_control = self.slot & ODSY_CONTROL_WIDGET_MASK;
        f.xt_req_timeout = ODSY_XT_REQ_TIMEOUT_RESET;
        f.int_dest_upper = 0;
        f.int_dest_lower = 0;
        f.error_command = 0;
        // The command FIFO is empty out of reset, so its low-water condition
        // holds and the low-water interrupt is pending. The kernel driver's
        // board reset arms a watchdog and then tears the board down unless the
        // driver sees this interrupt once, which is what lets it create the
        // direct rendering node the X server opens. Latch it at reset, because
        // the guest cannot reach the later FIFO-drain edge (a command
        // submission) until that node exists. Masked here until the driver
        // enables the source; enabling it then asserts.
        f.intr_flags = ODSY_INTR_FIFO_LOW_WATER;
        f.intr_enable = 0;
        // The level falls with the enables. The routed destination is reset
        // below, so no clearing packet has anywhere to go.
        f.interrupt_asserted = false;
        f.sw_token = 0;
        f.route_dest_upper = 0;
        f.route_dest_lower = 0;
        f.route_vectors = 0;
        f.swap_arm = 0;
        f.swap_pending = 0;
        f.swap_control = 0;
        f.swap_control_companion = 0;
        f.swap_set = 0;
        f.swap_clear = 0;
        f.displayed_buffer = 0;
        f.displayed_written = false;
        drop(f);
        self.flctl_config = ODSY_FLCTL_CONFIG_RESET;
        self.memory_control = 0;
        self.flctl_target = 0;
        self.fifo_setup = [0; 5];
        self.host_write_addr_upper = 0;
        self.host_write_addr_lower = 0;
        self.stats_addr_upper = 0;
        self.stats_addr_lower = 0;
        self.stats_control = 0;
        self.ctx_shadow_addr_upper = 0;
        self.ctx_shadow_addr_lower = 0;
        self.buzz = [0; 6];
        self.draw_mask = ODSY_DRAW_MASK_RESET;
        self.draw_word = 0;
        self.read_word = 0;
        self.draw_mask_stereo_reported = false;
        self.draw_mask_high_reported = false;
        self.drawable_word_reported = false;
        self.nic = 0;
        self.i2c_main_cmd = 0;
        self.i2c_opt_cmd = 0;
        self.lut.fill(0);
        self.backend_ctl.fill(0);
        self.dispmode = [0; 32];
        self.overlay_mode = [0; 8];
        self.overlay_mode_reported = false;
        self.cursor = [0; 0x100];
        self.cursor_reported = 0;
        self.gl_aux_reported = false;
        self.aux_mask_reported = false;
        self.aux_image_reported = false;
        self.backend_index = 0;
        self.backend_count = 0;
        self.reported_addr_count = 0;
        self.reported_cmd_count = 0;
        self.line_stipple_reported = false;
        self.dma_bpp_reported = false;
        self.dma_readback_reported = false;
        self.dma_code_reported = false;
        self.dma_read_reported = 0;
        self.selftest_progress = 0;
        self.cfifo_selftest_ready = false;

        self.query_cancel();
        self.query_records_reset();
        self.win_origin = [0; 2];
        self.read_origin = [0; 2];
        for i in 0..SGI_ODYSSEY_CLIP_RECTS as usize {
            self.clip_words[2 * i] = if i != 0 { ODSY_CLIP_RESET_EMPTY } else { 0 };
            self.clip_words[2 * i + 1] = if i != 0 { 0 } else { ODSY_CLIP_RESET_FULL };
        }
        self.clip_update();
        self.id_compare = 0;
        self.win_committed_origin = self.win_origin;
        self.win_committed_clip = self.clip_words;
        self.win_committed_id_compare = 0;
        self.win_prior_origin = [0; 2];
        self.win_origin_pending = false;
        self.win_after_tail = false;
        self.cmd_from_kernel = false;
        self.gl_primitive = 0;
        self.gl_color = 0;
        // GL's initial index is 1 (OpenGL 1.2.1 section 2.7), the value the
        // index record's default saves and a replay restores, so a context that
        // never sets an index draws GL vertices in index 1 before and after its
        // first switch. The board's reset index is unknown; this is a model
        // default. Limitation: the integer rectangle fill takes the index only
        // after an index word, so with none since reset it draws the packed
        // colour 0, and index 1 after a replay. Only a register test reaches
        // that: the X server sends an index or a colour before every fill.
        self.gl_color_index = 1;
        self.gl_color_is_index = false;
        // Out of reset the mode is unknown; RGBA until a context says otherwise.
        self.colour_mode = [0; 2];
        self.colour_mode_reported = false;
        self.gl_index_lit_reported = false;
        self.dest_buffer_word = 0;
        self.plane_write_mask = 0;
        self.dma_colour_base = 0;
        self.dest_base = 0;
        self.obj_count = 0;
        self.obj_used = 0;
        self.obj_base.fill(0);
        self.obj_index.clear();
        self.obj_width.fill(0);
        self.obj_offset.fill(0);
        self.obj_rows.fill(0);
        self.obj_stamp.fill(0);
        self.obj_clock = 0;
        self.obj_valid.fill(0);
        self.obj_full_reported = false;
        self.obj_forget_reported = false;
        self.board_oob_reported = false;
        self.tex_mode = 0;
        self.tex_sampler = [0; 7];
        for t in self.tex_colour_table.iter_mut() {
            t.fill(0);
        }
        self.tex_colour_table_valid = [0; 2];
        self.tex_detail_function.fill(0);
        self.tex_detail_function_valid = 0;
        self.tex_env = [0; 5];
        self.tex_levels.fill(0);
        self.tex_level0 = 0;
        self.tex_lod[0] = 0;
        self.tex_lod[1] = 0;
        self.tex_lod[2] = 0x3100;
        self.tex_lod[3] = 0x0f00;
        for c in 0..4usize {
            self.tex_scale_bias[c * 2] = ODSY_TEX_SCALE_BIAS_ONE;
            self.tex_scale_bias[c * 2 + 1] = 0;
        }
        self.tex_offset = [0; 3];
        self.tex_coord = [0; 2];
        self.tex_coord_r = 0;
        self.tex_coord_q = ODSY_F32_ONE;
        self.tex_rect_reported = false;
        self.gl_vertex_x.fill(0);
        self.gl_vertex_y.fill(0);
        self.gl_vertex_count = 0;
        self.line_stipple[0] = 0xffff;
        self.line_stipple[1] = 0xffff;
        self.line_stipple[2] = 0;
        // No polygon is open; reset_hold clears the marks themselves.
        self.poly_row_lo = SGI_ODYSSEY_HEIGHT;
        self.poly_row_hi = 0;
        self.poly_col_lo = SGI_ODYSSEY_WIDTH;
        // The shared GL context to OpenGL's initial state (identity matrices,
        // white current colour, depth test off), unbound until the first GL
        // command binds the drawable. A guest reset can land mid-primitive, with
        // an open polygon's vertex buffer still allocated; sgi_gl_reset() frees
        // it before reinitializing. It is safe here even before realize
        // allocates the colour plane. Its viewport starts empty: nothing is
        // drawn until the client's combined matrix arrives.
        self.gl_ctx.reset(ODSY_GL_CONTEXT_HW_ID);
        {
            // Opt into the shared context's per-face polygon modes (0x0415) and
            // line stipple: unset, it silently keeps every face at legacy FILL
            // no matter what 0x0415 decodes to. The sink built here always
            // carries the checked draw callback, this call's one precondition;
            // sgi_gl_reset() above has already cleared in_begin, satisfying the
            // other precondition even when reset lands mid-primitive.
            let sink = self.gl_sink();

            assert!(self.gl_ctx.enable_polygon_modes(Some(&sink)));
            assert!(self.gl_ctx.enable_line_stipple(Some(&sink)));
        }
        self.gl_in_prim = false;
        self.gl_begin_open = false;
        self.gl_integer_prim = false;
        self.gl_open_r = 0;
        self.gl_open_c = 0;
        self.gl_open_m3 = 0;
        self.gl_open_m4 = 0;
        self.gl_open_held = 0;
        self.gl_restart_type = 0;
        self.gl_restart_select = 0;
        self.gl_restart_absorb = 0;
        self.gl_restart_total = 0;
        self.gl_restart_c = 0;
        self.gl_restart_m3 = 0;
        self.gl_restart_m4 = 0;
        self.gl_restart_armed = false;
        // The clip box and its scissor test: sgi_gl_init() leaves the shared
        // context's scissor box and test both at zero, which would clip away
        // every draw if the test were simply switched on. There is no reset
        // pattern-mode value that already matches a usable scissor default, so
        // the full window is set here explicitly before the test is enabled,
        // once and for good.
        self.gl_clip_box[0] = ODSY_CLIP_BOX_FIELD_MASK & (SGI_ODYSSEY_WIDTH - 1);
        self.gl_clip_box[1] = ODSY_CLIP_BOX_FIELD_MASK & (SGI_ODYSSEY_HEIGHT - 1);
        self.gl_fog = [0; 6];
        self.gl_apply_clip_box();
        {
            let scissor_test = SGI_GL_SCISSOR_TEST;

            self.gl_feed(SGI_GL_OP_ENABLE as u16, &[scissor_test]);
        }
        self.gl_matrix_slot = SGI_GL_MODELVIEW;
        self.gl_modelview = [0; 16];
        for i in 0..4usize {
            self.gl_modelview[i * 5] = ODSY_F32_ONE;
        }
        self.gl_clip_eye.fill(0);
        self.gl_matrix = [0; 16];
        self.gl_texture_matrix = [0; 16];
        for i in 0..4usize {
            self.gl_texture_matrix[i * 5] = ODSY_F32_ONE;
        }
        self.gl_rect = [0; 10];
        self.gl_depth_func = ODSY_DEPTH_FUNC_RESET;
        self.gl_write_mask = ODSY_WRITE_MASK_RESET;
        self.gl_combined = [0; 16];
        // The identity projection's (-0, -0, -0, 1): the depth row unflipped.
        self.gl_block16[0] = 0x80000000;
        self.gl_block16[1] = 0x80000000;
        self.gl_block16[2] = 0x80000000;
        self.gl_block16[3] = 0x3f800000;
        self.gl_rect_depth_logged = false;
        self.gl_rect_stencil_logged = false;
        self.gl_rect_pattern_logged = false;
        self.gl_index_mask_logged = false;
        // Lighting out of reset. The hardware's values are unknown; the model
        // starts at GL's initial material, light-model ambient, normal and
        // colour, zero light colours and vectors (the library loads every light
        // before a lit draw), an identity normal matrix and no program.
        self.gl_const.fill(0);
        // Program memory's reset content is unknown; the kernel loads it.
        self.gl_program_mem.fill(0);
        self.gl_program_addr = 0;
        self.gl_seq = [0; 22];
        self.gl_program_half = 0;
        self.gl_program_mem_reported = false;
        self.gl_const_index = 0;
        self.gl_entry = [0; SGI_ODYSSEY_GL_ENTRIES as usize];
        // GL's own initial state, colour material off, as the library sends.
        self.gl_colour_material = 0;
        self.gl_light_colour.fill(0);
        for i in 0..3usize {
            self.gl_light_colour[ODSY_GL_LIGHT_MODEL_AMBIENT as usize * 4 + i] = ODSY_F32_POINT_TWO;
            self.gl_material[i] = ODSY_F32_POINT_TWO; // ambient
            self.gl_material[4 + i] = ODSY_F32_POINT_EIGHT; // diffuse
            self.gl_material[8 + i] = 0; // specular
            self.gl_material[12 + i] = 0; // emission
        }
        for i in 0..4usize {
            self.gl_material[i * 4 + 3] = ODSY_F32_ONE;
            self.gl_colour[i] = ODSY_F32_ONE;
        }
        self.gl_light_colour[ODSY_GL_LIGHT_MODEL_AMBIENT as usize * 4 + 3] = ODSY_F32_ONE;
        self.gl_light_vec.fill(0);
        self.gl_light_colour_sel = 0;
        self.gl_light_vec_sel = 0;
        self.gl_shininess_sqrt = 0;
        self.back_material_reset();
        self.gl_normal[0] = 0;
        self.gl_normal[1] = 0;
        self.gl_normal[2] = ODSY_F32_ONE;
        self.gl_normal_matrix = [0; 16];
        for i in 0..4usize {
            self.gl_normal_matrix[i * 5] = ODSY_F32_ONE;
        }
        self.gl_lit = false;
        self.gl_local_viewer = false;
        self.gl_colour_overridden = false;
        self.gl_program_logged_count = 0;
        self.gl_two_sided_logged = false;
        self.gl_texgen_logged = false;
        self.gl_linear_texgen = 0;
        self.tex_linear_plane = [[0; 4]; 2];
        self.tex_linear_plane_r = [0; 4];
        self.tex_linear_plane_q = [0; 4];
        self.tex_linear_valid = 0;
        self.gl_clip_logged = false;
        self.gl_texture_stage_logged = false;
        self.gl_block_logged = false;
        self.ctx_open_prim_logged = false;
        self.ctx_shadow_fail_reported = false;
        self.gl_dither = false;
        self.gl_dither_reported = false;
        self.scanout_group25_reported = false;
        self.copy_src = 0;
        self.copy_size = 0;
        self.copy_dst = 0;
        self.pattern_buffer = [0; 32];
        self.pattern_shape = 0;
        self.const_colour = [0; 2];
        self.blend_word = 0;
        self.pattern_mode = 0;
        self.pattern_load_offset = 0;
        self.image_active = false;
        self.image_x1 = 0;
        self.image_y1 = 0;
        self.image_x2 = 0;
        self.image_y2 = 0;
        self.image_cur_x = 0;
        self.image_cur_y = 0;
        self.image_fmt = 0;
        self.image_group_len = 0;
        self.image_group_painted = 0;
        self.image_code = 0;
        self.image_bytes = 0;
        self.image_nbytes = 0;
        self.image_code_reported = false;
        self.dma_seq = 0;
    }
}

/// One marketed board: name, revision, memory size, banks, back-end
/// version, identity record.
pub struct OdysseyProduct {
    pub name: &'static str,
    pub revision: u32,
    pub memory_size: u32,
    pub memory_banks: u32,
    pub backend_version: u32,
    pub record: &'static str,
}

/// The four marketed boards. A driver turns widget revision 1-3 into one
/// part generation and 9-15 into the next, and the board-info tool prints
/// the minor number as the revision's offset less one: revision 2 is "A.1",
/// 10 is "B.1". The inventory names the board from those two facts and the
/// size: A at 32 or 128 MB is V6 or V8, B is V10 or V12. Public captures
/// give A.1 for a 32 MB V6, B.1 for a 32 MB V10 and a 128 MB V12, and four
/// banks for the V12. The V8's A.1 and four banks are inferred from its
/// generation and size; the 32 MB boards' two banks are inferred, since the
/// tool prints a fixed "Banks: 2" line for any 32 MB board. Every product is
/// CAS latency 3. The identity record is the Octane board's name as the
/// inventory prints it.
pub static odyssey_products: [OdysseyProduct; 4] = [
    OdysseyProduct { name: "v12", revision: 0xa, memory_size: 128, memory_banks: 4, backend_version: 1, record: "Name:ODY128VERSIONB" }, // 030-1611
    OdysseyProduct { name: "v10", revision: 0xa, memory_size: 32, memory_banks: 2, backend_version: 1, record: "Name:ODY32VERSIONB" }, // 030-1655
    OdysseyProduct { name: "v8", revision: 0x2, memory_size: 128, memory_banks: 4, backend_version: 1, record: "Name:ODY128" }, // 030-1404
    OdysseyProduct { name: "v6", revision: 0x2, memory_size: 32, memory_banks: 2, backend_version: 1, record: "Name:ODY32" }, // 030-1403
];

impl SGIOdysseyState {
    pub fn resolve_product(&mut self) -> Result<(), String> {
        if self.product.is_empty() {
            return Ok(());
        }
        for p in odyssey_products.iter() {
            if self.product == p.name {
                self.revision = p.revision;
                self.memory_size = p.memory_size;
                self.memory_banks = p.memory_banks;
                self.backend_version = p.backend_version;
                self.product_record = p.record.to_string();
                return Ok(());
            }
        }
        Err(format!("sgi-odyssey product must be v12, v10, v8 or v6, not {}", self.product))
    }

    pub fn build_widget(&mut self) -> Result<(), String> {
        self.resolve_product()?;
        if self.revision > 0xf {
            return Err(format!("sgi-odyssey revision must fit in four bits, not {}", self.revision));
        }
        self.compose_memory_config()?;
        self.reset_registers();
        Ok(())
    }

    pub fn realize(&mut self) -> Result<(), String> {
        let mut err: Option<String> = None;

        self.attach_nic()?;

        // The colour framebuffer is two RGBA8 planes, one per colour buffer,
        // each over device-backed board memory of its own: a model-owned byte
        // array. It is not page-backed onto guest RAM. add_plane can only fail
        // on a programming error in these fixed sizes, so a failure panics.
        let pixels = SGI_ODYSSEY_WIDTH as u64 * SGI_ODYSSEY_HEIGHT as u64;
        self.mem.init(pixels * 4);
        {
            let plane = SGIPlane {
                format: SGI_PLANE_RGBA8,
                packing: SGI_PACK_BE_RED_HIGH,
                width: SGI_ODYSSEY_WIDTH,
                height: SGI_ODYSSEY_HEIGHT,
                stride: SGI_ODYSSEY_WIDTH * 4,
                base: 0,
                len: pixels * 4,
                bottom_origin: false,
                ..Default::default()
            };

            self.colour_plane = self.mem.add_plane(&plane, &mut err);
            // Colour buffer 1, the same shape in its own memory. Two model
            // planes preserve the visible contract; they do not claim the
            // board's two-buffer layout.
            self.buffer1_mem.init(plane.len);
            self.buffer1_plane = self.buffer1_mem.add_plane(&plane, &mut err);
        }

        // The board-memory object store: device-backed words the objects are
        // laid in by odyssey_object_rows(), addressed directly.
        self.obj_mem.init(ODSY_OBJ_POOL_BYTES as u64);

        self.did = vec![0u8; pixels as usize];
        self.overlay = vec![0u8; pixels as usize];
        self.poly_marks = vec![0u8; pixels as usize];

        // The depth plane: 24-bit depth in a 32-bit word per frame pixel, in
        // device-backed memory of its own. Where the board keeps the depth
        // buffer, and its width and layout, are unknown, so no aperture reaches
        // this plane. 24 bits matches the Z24 names in the GL library.
        self.depth_mem.init(pixels * 4);
        {
            let depth = SGIPlane {
                format: SGI_PLANE_Z24,
                packing: SGI_PACK_BE_RED_HIGH,
                width: SGI_ODYSSEY_WIDTH,
                height: SGI_ODYSSEY_HEIGHT,
                stride: SGI_ODYSSEY_WIDTH * 4,
                base: 0,
                len: pixels * 4,
                bottom_origin: false,
                ..Default::default()
            };

            self.depth_plane = self.depth_mem.add_plane(&depth, &mut err);
        }

        // The stencil plane: 8 bits a pixel, in device-backed memory of its own;
        // like the depth plane, no aperture reaches it.
        self.stencil_mem.init(pixels);
        {
            let stencil = SGIPlane {
                format: SGI_PLANE_S8,
                packing: SGI_PACK_BE_RED_HIGH,
                width: SGI_ODYSSEY_WIDTH,
                height: SGI_ODYSSEY_HEIGHT,
                stride: SGI_ODYSSEY_WIDTH,
                base: 0,
                len: pixels,
                bottom_origin: false,
                ..Default::default()
            };

            self.stencil_plane = self.stencil_mem.add_plane(&stencil, &mut err);
        }
        if let Some(e) = err {
            panic!("sgi-odyssey: {}", e);
        }

        // The shared scanout owns the presented frame, resolving the colour
        // plane through odyssey_head_ops.resolve_row. Setting the mode allocates
        // the frame, marks it all dirty and arms the retrace at the mode's rate.
        // The state never moves after sgi_odyssey_new returns, so its address is
        // a stable opaque.
        let opaque = P::new(&mut *self).opaque();
        self.scanout.init(&odyssey_head_ops, opaque);
        self.set_scanout_size(SGI_ODYSSEY_WIDTH, SGI_ODYSSEY_HEIGHT);
        self.front.lock().work_queue = Vec::new();
        Ok(())
    }

    pub fn unrealize(&mut self) {
        {
            // The board's thread may wait for the board lock the caller holds;
            // it is stopped beforehand (odyssey_work_stop()).
            let mut f = self.front.lock();
            assert!(f.worker.is_none(), "odyssey: the work thread outlived the machine");
            f.work_queue = Vec::new();
        }
        self.gl_ctx.cleanup();
        self.scanout.cleanup();
        self.did = Vec::new();
        self.overlay = Vec::new();
        self.poly_marks = Vec::new();
        self.mem.cleanup();
        self.buffer1_mem.cleanup();
        self.obj_mem.cleanup();
        self.depth_mem.cleanup();
        self.stencil_mem.cleanup();
    }

    pub fn reset_hold(&mut self) {
        self.work_reset();
        self.reset_registers();
        self.front.lock().update_irq();
        // odyssey_build_widget() also calls odyssey_reset_registers(), before
        // the board memory exists (realize initializes it afterwards) -- this
        // runs strictly after realize, so the board memory exists here. Zero the
        // device-backed colour plane's bytes directly; its planes are realize-
        // time configuration and stay. Redraw because reset clears it.
        self.mem.bytes.fill(0);
        self.buffer1_mem.bytes.fill(0);
        self.obj_mem.bytes.fill(0);
        self.stencil_mem.bytes.fill(0);
        self.did.fill(0);
        self.overlay.fill(0);
        self.poly_marks.fill(0);
        // The depth plane starts at the far value, all ones (a model choice).
        // Under the floating-point code depth 1.0 is 0xfe0000, and every code of
        // a depth in [0, 1] is at most that, so all ones still compares as
        // farther than any fragment.
        let depth = *self.depth_mem.plane(self.depth_plane).get();
        for y in 0..SGI_ODYSSEY_HEIGHT as i32 {
            for x in 0..SGI_ODYSSEY_WIDTH as i32 {
                self.depth_mem.plane_store(&depth, x, y, ODSY_DEPTH24_MAX);
            }
        }
        // Back to the boot size, then forget swaps and the cursor, repaint the
        // cleared plane and restart the phase.
        if self.scanout.mode.width != SGI_ODYSSEY_WIDTH || self.scanout.mode.height != SGI_ODYSSEY_HEIGHT {
            self.set_scanout_size(SGI_ODYSSEY_WIDTH, SGI_ODYSSEY_HEIGHT);
        }
        self.scanout.reset();
    }

    pub fn instance_init(&mut self) {
        self.obj_index = Default::default();
        // The fixed part of the drawable odyssey_gl_bind() hands over.
        self.gl_bind_drawable.bound = 1;
        self.gl_bind_drawable.front = 0;
        self.gl_bind_drawable.back = 1;
    }
}

/// The synthetic one-wire serial for a component letter and index.
pub fn sgi_synthetic_onewire_serial(letter: char, index: u32) -> u64 {
    0x51454d55u64 << 16 | (letter as u64) << 8 | index as u64
}

/// The board as the QEMU device is built: instance_init, the property
/// defaults (odyssey_properties), `product` and the slot, then realize.
pub fn sgi_odyssey_new(product: &str, slot: u32) -> Result<Box<SGIOdysseyState>, String> {
    let mut s = Box::new(SGIOdysseyState::default());

    s.instance_init();
    // Property defaults.
    s.revision = 0xa;
    s.memory_size = 128;
    s.memory_banks = 4;
    s.cas_latency = 3;
    s.backend_version = 1;
    s.nic_serial = sgi_synthetic_onewire_serial('G', 0);
    s.raster_bands = true;
    s.raster_band_pixels = 0;
    s.posted_fifo = true;
    s.product = product.to_string();
    s.slot = slot;
    s.build_widget()?;
    s.realize()?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::InterruptController;
    use parking_lot::Mutex;
    use std::sync::Arc;

    // Ported from tests/qtest/octane-vpro-test.c: the register contract of
    // the board in the Octane's graphics port. The posted-FIFO test is left
    // out (QEMU TCG-specific).

    const VPRO_WIDGET: u32 = 11;
    const HEART_WIDGET: u64 = 8;
    const HEART_PACKET: u64 = 0x80;
    const TEST_VECTOR: u32 = 18;
    // Aperture offsets: the identity and control words are the low halves.
    const IDENTITY: u64 = 0x0004;
    const MEMORY_CONFIG: u64 = 0x106c;
    const CONTROL: u64 = 0x0024;
    const ROUTE_DEST_UPPER: u64 = 0x1044;
    const ROUTE_DEST_LOWER: u64 = 0x104c;
    const ROUTE_VECTORS: u64 = 0x1054;
    const INTR_CLEAR: u64 = 0x101c;
    const INTR_ENABLE_SET: u64 = 0x1034;
    const INTR_FIFO_LOW_WATER: u64 = 0x200;
    const IDENTITY_DEFAULT: u32 = 0xac013047;

    /// Records every (line, level) the board drives.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<(u32, bool)>>);

    impl InterruptController for Recorder {
        fn set_line(&self, line: u32, level: bool) {
            self.0.lock().push((line, level));
        }
    }

    impl Recorder {
        /// The level of `line` after the last event on it.
        fn level(&self, line: u32) -> bool {
            self.0.lock().iter().rev().find(|e| e.0 == line).map(|e| e.1).unwrap_or(false)
        }
    }

    fn board(product: &str) -> (Box<SGIOdysseyState>, Arc<Recorder>) {
        let rec = Arc::new(Recorder::default());
        let s = sgi_odyssey_new(product, VPRO_WIDGET).expect("board");
        s.front.lock().irq = Some(rec.clone());
        (s, rec)
    }

    #[test]
    fn widget_identity() {
        let (mut s, _) = board("v12");
        assert_eq!(s.mmio_read(IDENTITY, 4) as u32, IDENTITY_DEFAULT);
        assert_eq!(s.mmio_read(CONTROL, 4) as u32 & ODSY_CONTROL_WIDGET_MASK, VPRO_WIDGET);
    }

    #[test]
    fn profile_identity_and_routing() {
        // (product, revision, memory size code)
        let cases = [("v6", 2u32, 0u32), ("v8", 2, 2), ("v10", 0xa, 0), ("v12", 0xa, 2)];

        for (name, revision, size_code) in cases {
            let (mut s, rec) = board(name);
            let identity = s.mmio_read(IDENTITY, 4) as u32;
            let memory = s.mmio_read(MEMORY_CONFIG, 4) as u32;

            assert_eq!(identity >> 28, revision, "{name}");
            assert_eq!(identity & 0x0fff_ffff, IDENTITY_DEFAULT & 0x0fff_ffff, "{name}");
            assert_eq!((memory >> 30) & 3, size_code, "{name}");
            assert_eq!(s.mmio_read(CONTROL, 4) as u32 & ODSY_CONTROL_WIDGET_MASK, VPRO_WIDGET);

            s.mmio_write(ROUTE_DEST_UPPER, HEART_WIDGET << 25, 4);
            s.mmio_write(ROUTE_DEST_LOWER, HEART_PACKET, 4);
            s.mmio_write(ROUTE_VECTORS, TEST_VECTOR as u64, 4);
            s.mmio_write(INTR_ENABLE_SET, INTR_FIFO_LOW_WATER, 4);
            assert!(rec.level(TEST_VECTOR), "{name}: vector raised");
            s.mmio_write(INTR_CLEAR, INTR_FIFO_LOW_WATER, 4);
            assert!(!rec.level(TEST_VECTOR), "{name}: vector lowered");
        }
    }

    /// A raised and a lowered board interrupt each reach HEART, setting and
    /// clearing the vector's pending bit.
    #[test]
    fn interrupt_reaches_heart() {
        let (mut s, rec) = board("v12");

        s.mmio_write(ROUTE_DEST_UPPER, HEART_WIDGET << 25, 4);
        s.mmio_write(ROUTE_DEST_LOWER, HEART_PACKET, 4);
        s.mmio_write(ROUTE_VECTORS, TEST_VECTOR as u64, 4);
        assert!(!rec.level(TEST_VECTOR));

        // The FIFO is empty, so its low-water flag is already pending.
        s.mmio_write(INTR_ENABLE_SET, INTR_FIFO_LOW_WATER, 4);
        assert!(rec.level(TEST_VECTOR));

        s.mmio_write(INTR_CLEAR, INTR_FIFO_LOW_WATER, 4);
        assert!(!rec.level(TEST_VECTOR));
    }

    /// A board on its own thread, routed to the recorder and with the
    /// low-water source enabled and clear.
    fn threaded_board() -> (OdysseyHandles, Arc<Recorder>) {
        let (s, rec) = board("v12");
        let h = OdysseyHandles::new(s);

        h.work_start();
        h.write(ROUTE_DEST_UPPER, HEART_WIDGET << 25, 4);
        h.write(ROUTE_DEST_LOWER, HEART_PACKET, 4);
        h.write(ROUTE_VECTORS, TEST_VECTOR as u64, 4);
        h.write(INTR_CLEAR, INTR_FIFO_LOW_WATER, 4);
        h.write(INTR_ENABLE_SET, INTR_FIFO_LOW_WATER, 4);
        assert!(!rec.level(TEST_VECTOR));
        (h, rec)
    }

    /// The self-test preamble twice: it completes only in order.
    fn preamble_twice() -> Vec<&'static OdysseyCfifoPreamble> {
        let words: Vec<_> = odyssey_cfifo_preamble.iter().chain(odyssey_cfifo_preamble.iter()).collect();
        assert!(words.len() as u32 > ODSY_CFIFO_LOW_WATER);
        words
    }

    /// Queue every FIFO store and have the first slice hold the board for
    /// `hold_ms` as a long draw would; store the first word and return
    /// once that slice holds the board lock.
    fn start_long_slice(h: &OdysseyHandles, words: &[&OdysseyCfifoPreamble], hold_ms: u32) {
        h.front.lock().test_words = 1;
        h.board.lock().test_slice_sleep_ms = hold_ms;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        h.write(words[0].port, words[0].value, words[0].size);
        while !(h.front.lock().slice && h.board.is_locked()) {
            assert!(std::time::Instant::now() < deadline, "the first slice did not start");
            std::thread::yield_now();
        }
    }

    /// A burst of command-FIFO stores queued behind a long slice runs on the
    /// board's thread in the order stored, and the low-water flag rises, and
    /// its interrupt with it, once the queue drains below the low-water
    /// mark. While the slice holds the board, the stores queue and the
    /// status register answers without waiting for it.
    #[test]
    fn work_thread_runs_queued_stores_in_order() {
        const STATUS0: u64 = 0x1064;
        let (h, rec) = threaded_board();
        let words = preamble_twice();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);

        start_long_slice(&h, &words, 2000);
        // The rest go in while the first slice holds the board lock: only
        // the front lock is needed.
        for w in &words[1..] {
            h.write(w.port, w.value, w.size);
        }
        {
            let level = h.front.lock().fifo_level();
            assert!(level >= ODSY_CFIFO_LOW_WATER, "level {level}: the first slice should still hold the queue");
            assert_eq!(h.front.lock().intr_flags & ODSY_INTR_FIFO_LOW_WATER, 0);
            // The status register does not wait: not empty, above low water.
            let status = h.read(STATUS0, 4) as u32;
            assert_eq!(status & (ODSY_STATUS0_FIFO_EMPTY | ODSY_STATUS0_FIFO_LOW_WATER), 0);
            assert_eq!(h.front.lock().fifo_level(), level);
            // All of that while the slice still held the board.
            assert!(h.board.is_locked() && h.front.lock().slice);
        }

        // Watch the queue drain on the board's thread, without draining it
        // from here: the flag is up exactly when the level is below the mark.
        loop {
            {
                let f = h.front.lock();
                let level = f.fifo_level();
                let flag = f.intr_flags & ODSY_INTR_FIFO_LOW_WATER != 0;
                assert_eq!(flag, level < ODSY_CFIFO_LOW_WATER, "level {level}");
                if level == 0 && !f.slice {
                    break;
                }
            }
            assert!(std::time::Instant::now() < deadline, "work thread did not drain the queue");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        {
            let s = h.board.lock();
            assert!(s.cfifo_selftest_ready);
            assert_eq!(s.selftest_progress, 0);
        }
        assert!(rec.level(TEST_VECTOR));
        let status = h.read(STATUS0, 4) as u32;
        assert_eq!(
            status & (ODSY_STATUS0_FIFO_EMPTY | ODSY_STATUS0_FIFO_LOW_WATER),
            ODSY_STATUS0_FIFO_EMPTY | ODSY_STATUS0_FIFO_LOW_WATER
        );
        h.work_stop();
        assert!(h.front.lock().worker.is_none());
    }

    /// A read that waits for the work, issued while a long slice holds the
    /// board, answers only after every store queued before it has run: the
    /// self-test register reads OK, though the board's thread runs one
    /// store per slice and most were still queued when the read began.
    #[test]
    fn waiting_read_sees_all_earlier_stores() {
        let (h, _rec) = threaded_board();
        let words = preamble_twice();

        start_long_slice(&h, &words, 300);
        for w in &words[1..] {
            h.write(w.port, w.value, w.size);
        }
        assert!(h.front.lock().fifo_level() > 1);
        let selftest = h.read(ODSY_CFIFO_SELFTEST, 4) as u32;
        assert_eq!(selftest, ODSY_CFIFO_SELFTEST_OK);
        assert_eq!(h.front.lock().fifo_level(), 0);
        h.work_stop();
    }

    /// Records every call a board makes on its processor-interface link.
    #[derive(Default)]
    struct PiStub(Mutex<Vec<(&'static str, u32, i64)>>);

    impl SGIPIGfxLink for PiStub {
        fn return_credits(&self, widget: u32, n: u32) {
            self.0.lock().push(("return", widget, n as i64));
        }
        fn owe_credits(&self, widget: u32, n: i32) {
            self.0.lock().push(("owe", widget, n as i64));
        }
        fn repay_credits(&self, widget: u32, n: u32) {
            self.0.lock().push(("repay", widget, n as i64));
        }
    }

    impl PiStub {
        fn count(&self, kind: &str) -> i64 {
            self.0.lock().iter().filter(|c| c.0 == kind).map(|c| c.2).sum()
        }
        /// Credits given back so far, returned or repaid.
        fn settled(&self) -> i64 {
            self.count("return") + self.count("repay")
        }
    }

    /// A board whose stores are all counted and queued, with a stub link.
    fn credit_board() -> (Box<SGIOdysseyState>, Arc<PiStub>) {
        let (s, _) = board("v12");
        let pi = Arc::new(PiStub::default());
        {
            let mut f = s.front.lock();
            f.pi = Some(pi.clone());
            f.test_counted_stores = 1;
            // No work thread runs: hold the queue and step by hand.
            f.test_words = 1;
        }
        (s, pi)
    }

    /// Ported from tests/qtest/sn1-odyssey-test.c,
    /// test_command_work_credits_at_fifo_entry: a counted store's credit
    /// comes back when the board takes it into its FIFO, the oldest 64
    /// waiting stores at once; each store behind them is owed, then
    /// repaid as the work ahead runs and it moves up. A store that does
    /// not wait is credited inside the store.
    #[test]
    fn counted_store_credits_at_fifo_entry() {
        let (mut s, pi) = credit_board();
        let stores = 72u32;

        for _ in 0..stores {
            s.mmio_write(ODSY_CMD_FIFO, 0, 8);
        }
        assert_eq!(s.front.lock().work_queue.len() as u32, stores);
        assert_eq!(s.front.lock().work_in_fifo, ODSY_CFIFO_DEPTH);
        assert_eq!(pi.count("return"), ODSY_CFIFO_DEPTH as i64);
        assert_eq!(pi.count("owe"), (stores - ODSY_CFIFO_DEPTH) as i64);
        assert_eq!(pi.count("repay"), 0);
        assert!(pi.0.lock().iter().all(|c| c.1 == VPRO_WIDGET));

        // One store runs per slice: the next one moves up and is repaid.
        s.work_step(&mut OdysseyBudget::words(1));
        assert_eq!(pi.count("repay"), 1);
        assert_eq!(pi.settled(), ODSY_CFIFO_DEPTH as i64 + 1);

        s.work_step(&mut OdysseyBudget::unlimited());
        assert!(s.front.lock().work_queue.is_empty());
        assert_eq!(pi.settled(), stores as i64);

        // A store that does not wait is credited inside the store.
        s.front.lock().test_words = 0;
        s.mmio_write(ODSY_CMD_FIFO, 0, 8);
        assert!(!s.work_pending());
        assert_eq!(pi.settled(), stores as i64 + 1);
        assert_eq!(pi.count("owe"), (stores - ODSY_CFIFO_DEPTH) as i64);
    }

    /// A reset abandons the waiting stores and cancels each owed credit
    /// (one owe of -1 per owed store); stores not owed cancel nothing.
    #[test]
    fn reset_cancels_owed_credits() {
        let (mut s, pi) = credit_board();

        for _ in 0..ODSY_CFIFO_DEPTH + 8 {
            s.mmio_write(ODSY_CMD_FIFO, 0, 8);
        }
        assert_eq!(pi.count("owe"), 8);
        s.work_reset();
        assert!(s.front.lock().work_queue.is_empty());
        assert_eq!(s.front.lock().work_in_fifo, 0);
        assert_eq!(pi.count("owe"), 0);
        assert_eq!(pi.0.lock().iter().filter(|c| c.0 == "owe" && c.2 == -1).count(), 8);
        assert_eq!(pi.count("repay"), 0);
    }

    /// The processor interface's mark makes a store counted, once, and a
    /// counted store with no link logs and returns nothing.
    #[test]
    fn processor_mark_counts_one_store() {
        let (mut s, pi) = credit_board();

        {
            let mut f = s.front.lock();
            f.test_counted_stores = 0;
            f.test_words = 0;
        }
        s.mmio_write(ODSY_CMD_FIFO, 0, 8);
        assert_eq!(pi.settled(), 0);

        let previous = sgi_pi_gfx_mark_store(true);
        s.mmio_write(ODSY_CMD_FIFO, 0, 8);
        sgi_pi_gfx_unmark_store(previous);
        assert_eq!(pi.0.lock().as_slice(), &[("return", VPRO_WIDGET, 1)]);

        // One-shot: the mark was taken by the first store.
        s.mmio_write(ODSY_CMD_FIFO, 0, 8);
        assert_eq!(pi.settled(), 1);

        // The Octane: no link, so the credit is dropped with a log line.
        s.front.lock().pi = None;
        sgi_pi_gfx_mark_store(true);
        s.mmio_write(ODSY_CMD_FIFO, 0, 8);
        assert_eq!(pi.settled(), 1);
        assert!(!sgi_pi_gfx_take_mark());
    }

    /// The register block's pure state (pi-gfx.c).
    #[test]
    fn pi_gfx_block_counts_and_starves() {
        let mut g = SGIPIGfx::default();

        g.write(SGI_PI_GFX_PAGE, SGI_PI_GFX_PAGE_ENABLE | 0x2_0000);
        g.write(SGI_PI_GFX_CREDIT, 3);
        g.write(SGI_PI_GFX_BIAS, 2);
        assert!(g.covers(0, 0x2_1000));
        assert!(!g.covers(0, 0x4_0000));
        assert!(g.count_store(0, 0x2_0000, "t"));
        assert!(!g.take_starved(0, "t"));
        assert!(g.count_store(0, 0x2_0000, "t"));
        assert!(g.take_starved(0, "t"));
        assert!(!g.take_starved(0, "t"));
        g.add_credits(0, 5, "t");
        assert_eq!(g.read(SGI_PI_GFX_CREDIT), 6);
        assert!(!g.starved[0]);
    }
}
