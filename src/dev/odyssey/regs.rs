//! Ported from hw/misc/sgi-odyssey.c in jamesbraid/qemu-sgi-origami.

use super::*;

pub fn odyssey_identity(s: &mut SGIOdysseyState) -> u32 {
    (s.revision << 28) | (ODSY_PART << 12) | (ODSY_MANUFACTURER << 1) | 1
}

pub fn odyssey_unimplemented(s: &mut SGIOdysseyState, addr: u64, size: u32, write: bool) {
    let token = (addr as u32) | if write { ODSY_UNIMP_WRITE } else { 0 };

    for i in 0..s.reported_addr_count as usize {
        if s.reported_addr[i] == token {
            return;
        }
    }
    if s.reported_addr_count == SGI_ODYSSEY_UNIMP_REPORTS {
        return;
    }
    s.reported_addr[s.reported_addr_count as usize] = token;
    s.reported_addr_count += 1;
    crate::qlog!(
        "sgi-odyssey: {}-byte {} at {:#x} is not modeled{}\n",
        size,
        if write { "write" } else { "read" },
        addr,
        if s.reported_addr_count == SGI_ODYSSEY_UNIMP_REPORTS { "; further offsets stay silent" } else { "" }
    );
}

/// Report an unmodeled command word once. note follows the message: the
/// parser's framing decision for a word it does not know, or "".
pub fn odyssey_cmd_report(s: &mut SGIOdysseyState, word: u32, note: &str) {
    for i in 0..s.reported_cmd_count as usize {
        if s.reported_cmd[i] == word {
            return;
        }
    }
    if s.reported_cmd_count == SGI_ODYSSEY_UNIMP_REPORTS {
        return;
    }
    s.reported_cmd[s.reported_cmd_count as usize] = word;
    s.reported_cmd_count += 1;
    crate::qlog!(
        "sgi-odyssey: command FIFO word {:#010x} is not modeled{}{}\n",
        word,
        note,
        if s.reported_cmd_count == SGI_ODYSSEY_UNIMP_REPORTS { "; further words stay silent" } else { "" }
    );
}

pub fn odyssey_cmd_unimplemented(s: &mut SGIOdysseyState, word: u32) {
    odyssey_cmd_report(s, word, "");
}

/// The modeled register an access names, or -1. A configuration-block
/// register answers its 64-bit address and the 32-bit spelling of either
/// half identically (a live V12 board reads the same value at a register's
/// base and at base+4 across the block). Every other register has only been
/// seen as a 32-bit access. Other widths, alignments and offsets are
/// unmodeled rather than an error.
pub fn odyssey_reg(addr: u64, size: u32) -> i64 {
    if addr < ODSY_BLOCK_END {
        if (size == 8 || size == 4) && (addr & 7) == 0 {
            return addr as i64;
        }
        if size == 4 && (addr & 7) == 4 {
            return (addr & !7) as i64;
        }
        return -1;
    }
    if size != 4 {
        return -1;
    }
    match addr {
        ODSY_INTR_FLAGS
        | ODSY_INTR_CLEAR
        | ODSY_INTR_ENABLE_SET
        | ODSY_INTR_ENABLE_CLEAR
        | ODSY_ROUTE_DEST_UPPER
        | ODSY_ROUTE_DEST_LOWER
        | ODSY_ROUTE_VECTORS
        | ODSY_STATUS0
        | ODSY_CFIFO_SELFTEST
        | ODSY_MEMORY_CONFIG
        | ODSY_FLCTL_CONFIG
        | ODSY_FLCTL_TARGET
        | ODSY_SW_TOKEN
        | ODSY_NIC_PROM
        | ODSY_NIC_KERNEL
        | ODSY_BUZZ_1
        | ODSY_BUZZ_2
        | ODSY_BUZZ_3
        | ODSY_BUZZ_4
        | ODSY_BUZZ_5
        | ODSY_BUZZ_6
        | ODSY_SWAP_ARM
        | ODSY_SWAP_PENDING
        | ODSY_SWAP_CONTROL
        | ODSY_SWAP_CONTROL_COMPANION
        | ODSY_DISPLAYED_BUFFER
        | ODSY_I2C_MAIN_CMD_READ
        | ODSY_I2C_MAIN_STATUS_READ
        | ODSY_I2C_OPT_CMD_READ
        | ODSY_I2C_OPT_STATUS_READ
        | ODSY_HOST_WRITE_ADDR_UPPER
        | ODSY_HOST_WRITE_ADDR_LOWER
        | ODSY_STATS_ADDR_UPPER
        | ODSY_STATS_ADDR_LOWER
        | ODSY_STATS_CONTROL
        | ODSY_CTX_SHADOW_ADDR_UPPER
        | ODSY_CTX_SHADOW_ADDR_LOWER
        | ODSY_RESET_CONTROL
        | ODSY_MEMORY_CONTROL
        | ODSY_BACKEND_VERSION => addr as i64,
        _ => -1,
    }
}

/// An interrupt packet into the widget-addressed fabric, through the XIO
/// widget layer.
pub fn odyssey_send_packet(s: &mut SGIOdysseyState, where_: u64, vector: u8, set: bool) {
    // Retain the packet for host-only qtest inspection.
    s.test_last_interrupt_addr = where_;
    s.test_last_interrupt_value = vector as u64 | if set { SGI_XIO_INTERRUPT_SET } else { 0 };
    sgi_xio_send_interrupt(s, where_, vector, set);
}

/// The fabric address of a widget/address pair: the widget in bits 51:48.
pub fn odyssey_xio_compose(_s: &mut SGIOdysseyState, widget: u32, address: u64, fabric: &mut u64) -> bool {
    sgi_xio_compose("sgi-odyssey", widget, address, fabric)
}

/// One routed source's interrupt. Every routed source shares one
/// destination (ODSY_ROUTE_DEST_UPPER/_LOWER), distinct from
/// int_dest_upper/int_dest_lower, which the kernel driver gives its
/// crosstalk error handler; the sources differ only in their byte of
/// ODSY_ROUTE_VECTORS. The vectors are read from the register, never
/// assumed. The flag source is a level the guest clears through
/// ODSY_INTR_CLEAR, so it sends a clearing packet as it falls
/// (odyssey_update_irq()); the softsync, retrace and DMA-done handlers ack
/// nothing, and those sources send a single set pulse.
pub fn odyssey_deliver_routed(s: &mut SGIOdysseyState, shift: u32, what: &str, set: bool) {
    let widget = (s.route_dest_upper >> ODSY_ROUTE_DEST_WIDGET_SHIFT) & ((1u32 << ODSY_INT_DEST_WIDGET_BITS) - 1);
    let addr = (((s.route_dest_upper & 0xffff) as u64) << 32) | s.route_dest_lower as u64;
    let vector = s.route_vectors.checked_shr(shift).unwrap_or(0) as u8;
    let mut where_ = 0u64;

    if s.route_dest_upper == 0 && s.route_dest_lower == 0 {
        crate::qlog!(
            "sgi-odyssey: {} interrupt {} with no destination programmed; no packet sent\n",
            what,
            if set { "raised" } else { "lowered" }
        );
        return;
    }
    if !odyssey_xio_compose(s, widget, addr, &mut where_) {
        return;
    }
    odyssey_send_packet(s, where_, vector, set);
}

/// The softsync source ODSY_REG_HOST_WRITE (0x0a5a) raises.
pub fn odyssey_deliver_softsync(s: &mut SGIOdysseyState) {
    odyssey_deliver_routed(s, ODSY_ROUTE_VECTOR_SOFTSYNC_SHIFT, "softsync", true);
}

pub fn odyssey_update_irq(s: &mut SGIOdysseyState) {
    let level = (s.intr_flags & s.intr_enable) != 0;

    if level != s.interrupt_asserted {
        s.interrupt_asserted = level;
        odyssey_deliver_routed(s, ODSY_ROUTE_VECTOR_FLAG_SHIFT, "flag", level);
    }
}

/// The command FIFO's level: the stores waiting for the board's work.
pub fn odyssey_fifo_level(s: &mut SGIOdysseyState) -> u32 {
    s.work_queue.len() as u32
}

/// The FIFO's level went from 'before' to its current value. Falling below
/// the low-water mark latches the low-water flag whatever the enable mask
/// says; it reaches the driver only while the source is enabled.
pub fn odyssey_fifo_level_changed(s: &mut SGIOdysseyState, before: u32) {
    if before >= ODSY_CFIFO_LOW_WATER && odyssey_fifo_level(s) < ODSY_CFIFO_LOW_WATER {
        s.intr_flags |= ODSY_INTR_FIFO_LOW_WATER;
        odyssey_update_irq(s);
    }
}

/// One bit slot, reported finished inside the write the guest polls after.
pub fn odyssey_nic_slot(s: &mut SGIOdysseyState, value: u32) {
    let pulse = (value & ODSY_NIC_PULSE_MASK) >> ODSY_NIC_PULSE_SHIFT;
    let sample = (value & ODSY_NIC_SAMPLE_MASK) >> ODSY_NIC_SAMPLE_SHIFT;
    let level = match s.nic_bus.as_mut() {
        Some(bus) => crate::dev::w1::slot::slot(bus, pulse, sample),
        None => false,
    };

    s.nic = (value & !(ODSY_NIC_LEVEL | ODSY_NIC_DONE)) | ODSY_NIC_DONE | if level { ODSY_NIC_LEVEL } else { 0 };
}

// The display back end's 32 by 32 hardware cursor (the datasheet's
// "Hardware cursor"), drawn over the resolved picture: after the lookup RAM
// and the gamma table, over the main and overlay planes alike. Entries:
//   0x2500  128 words  glyph: 32 rows of four words, eight 4-bit pixels a
//                      word, the leftmost pixel in bits 3:0
//   0x2580             position: x in 31:16, y in 15:0
//   0x2581             control
//   0x2590  15 words   colours, ten bits a channel, as in the lookup RAM
//   0x25c0  64 words   plane: 32 rows of two words, sixteen 2-bit pixels a
//                      word, the leftmost pixel in bits 1:0
// The layouts are established; what the board makes of the values is
// inferred. APPROXIMATION: under plane 3 a glyph value k shows colour word k
// and glyph 0 shows word 0; under plane 0 the pixel is transparent. Glyph
// value 15 and plane values 1 and 2 have no known meaning: reported once and
// drawn transparent. A channel shows the top eight bits of its ten. Reset
// values are unknown; the model resets every entry to zero, cursor off.

pub fn odyssey_cursor_report(s: &mut SGIOdysseyState, bit: u32, what: &str, index: u32, data: u32) {
    if s.cursor_reported & bit != 0 {
        return;
    }
    s.cursor_reported |= bit;
    crate::qlog!("sgi-odyssey: cursor entry {:#x} word {:#010x}: {}\n", ODSY_CURSOR_BASE + index, data, what);
}

/// The position's components, each a signed 16-bit value (inferred).
pub fn odyssey_cursor_x(s: &mut SGIOdysseyState) -> i32 {
    (s.cursor[ODSY_CURSOR_POSITION as usize] >> 16) as i16 as i32
}

pub fn odyssey_cursor_y(s: &mut SGIOdysseyState) -> i32 {
    s.cursor[ODSY_CURSOR_POSITION as usize] as i16 as i32
}

/// Whether anything is drawn: bit 0 set and bit 5 clear. APPROXIMATION: on
/// the second channel the cursor is on a display the model does not have.
pub fn odyssey_cursor_shown(s: &mut SGIOdysseyState) -> bool {
    let ctl = s.cursor[ODSY_CURSOR_CONTROL as usize];

    (ctl & ODSY_CURSOR_CTL_ENABLE) != 0 && (ctl & ODSY_CURSOR_CTL_CHANNEL1) == 0
}

/// The display lines the cursor covers as it stands: first > last if none.
pub fn odyssey_cursor_span(s: &mut SGIOdysseyState, first: &mut i32, last: &mut i32) {
    *first = 0;
    *last = -1;
    if !odyssey_cursor_shown(s) {
        return;
    }
    if s.cursor[ODSY_CURSOR_CONTROL as usize] & ODSY_CURSOR_CTL_CROSSHAIR != 0 {
        // The vertical line crosses every line.
        *last = s.scanout.mode.height as i32 - 1;
        return;
    }
    *first = odyssey_cursor_y(s).max(0);
    *last = (odyssey_cursor_y(s) + ODSY_CURSOR_SIZE as i32 - 1).min(s.scanout.mode.height as i32 - 1);
}

pub fn odyssey_cursor_invalidate(s: &mut SGIOdysseyState, first: i32, last: i32) {
    if first <= last {
        sgi_scanout_invalidate(&mut s.scanout, first, last - first + 1);
    }
}

/// A colour word, ten bits a channel, as 0x00RRGGBB from the top eight.
pub fn odyssey_cursor_rgb(word: u32) -> u32 {
    (((word >> ODSY_LUT_RED_SHIFT) & 0xff) << 16) | (((word >> ODSY_LUT_GREEN_SHIFT) & 0xff) << 8) | ((word >> ODSY_LUT_BLUE_SHIFT) & 0xff)
}

/// Report the values a stored word carries that have no known meaning.
pub fn odyssey_cursor_check(s: &mut SGIOdysseyState, index: u32, data: u32) {
    if index < ODSY_CURSOR_POSITION {
        let mut i = 0;
        while i < 32 {
            if (data >> i) & 0xf == 0xf {
                odyssey_cursor_report(
                    s,
                    ODSY_CURSOR_REPORT_GLYPH15,
                    "glyph value 15 has no colour word; drawn transparent",
                    index,
                    data,
                );
                break;
            }
            i += 4;
        }
    } else if index >= ODSY_CURSOR_PLANE {
        let mut i = 0;
        while i < 32 {
            let v = (data >> i) & 3;

            if v == 1 || v == 2 {
                odyssey_cursor_report(
                    s,
                    ODSY_CURSOR_REPORT_PLANE,
                    "plane values 1 and 2 are not modeled; drawn transparent",
                    index,
                    data,
                );
                break;
            }
            i += 2;
        }
    } else if index == ODSY_CURSOR_CONTROL {
        if (data & ODSY_CURSOR_CTL_ENABLE) != 0 && (data & ODSY_CURSOR_CTL_CROSSHAIR) != 0 {
            odyssey_cursor_report(
                s,
                ODSY_CURSOR_REPORT_CROSSHAIR,
                "crosshair drawn as full-raster lines in colour word 1, an approximation",
                index,
                data,
            );
        }
        if data & ODSY_CURSOR_CTL_CHANNEL1 != 0 {
            odyssey_cursor_report(
                s,
                ODSY_CURSOR_REPORT_CHANNEL1,
                "second display channel is not modeled; nothing drawn",
                index,
                data,
            );
        }
        if data & !ODSY_CURSOR_CTL_KNOWN != 0 {
            odyssey_cursor_report(
                s,
                ODSY_CURSOR_REPORT_CONTROL,
                "control bits outside 0-5 and 31:30 have no known meaning",
                index,
                data,
            );
        }
    } else if index != ODSY_CURSOR_POSITION && (index < ODSY_CURSOR_COLOURS || index >= ODSY_CURSOR_COLOURS + ODSY_CURSOR_COLOUR_COUNT) {
        odyssey_cursor_report(s, ODSY_CURSOR_REPORT_ENTRY, "no known function; stored", index, data);
    }
}

/// One backend data word for a cursor entry: store it, report what it
/// carries that has no known meaning, and repaint the lines the cursor
/// covered before and covers now.
pub fn odyssey_cursor_write(s: &mut SGIOdysseyState, index: u32, data: u32) {
    let (mut old_first, mut old_last, mut first, mut last) = (0, 0, 0, 0);

    odyssey_cursor_span(s, &mut old_first, &mut old_last);
    s.cursor[index as usize] = data;
    odyssey_cursor_check(s, index, data);
    odyssey_cursor_span(s, &mut first, &mut last);
    odyssey_cursor_invalidate(s, old_first, old_last);
    odyssey_cursor_invalidate(s, first, last);
}

/// Draw the cursor over display line y, already resolved into dst. With bit
/// 2 it is a crosshair: APPROXIMATION, a one-pixel horizontal and vertical
/// line across the whole raster through the position, in colour word 1. The
/// crosshair's extent, width and colour are unknown.
pub fn odyssey_cursor_row(s: &mut SGIOdysseyState, y: u32, dst: &mut [u32], width: u32) {
    let cx = odyssey_cursor_x(s);
    let cy = odyssey_cursor_y(s);
    let row = (y as i32).wrapping_sub(cy);

    if !odyssey_cursor_shown(s) {
        return;
    }
    if s.cursor[ODSY_CURSOR_CONTROL as usize] & ODSY_CURSOR_CTL_CROSSHAIR != 0 {
        let colour = odyssey_cursor_rgb(s.cursor[ODSY_CURSOR_COLOURS as usize + 1]);

        if row == 0 {
            for x in 0..width as usize {
                dst[x] = colour;
            }
        } else if cx >= 0 && cx < width as i32 {
            dst[cx as usize] = colour;
        }
        return;
    }
    if row < 0 || row >= ODSY_CURSOR_SIZE as i32 {
        return;
    }
    for col in 0..ODSY_CURSOR_SIZE as i32 {
        let x = cx.wrapping_add(col);
        let plane = (s.cursor[(ODSY_CURSOR_PLANE as i32 + row * 2 + col / 16) as usize] >> ((col % 16) * 2)) & 3;
        let glyph = (s.cursor[(ODSY_CURSOR_GLYPH as i32 + row * 4 + col / 8) as usize] >> ((col % 8) * 4)) & 0xf;

        if x < 0 || x >= width as i32 || plane != 3 || glyph == 0xf {
            continue;
        }
        dst[x as usize] = odyssey_cursor_rgb(s.cursor[ODSY_CURSOR_COLOURS as usize + glyph as usize]);
    }
}

/// A backend read-window offset that names a cursor entry: its stored word.
pub fn odyssey_cursor_read(s: &mut SGIOdysseyState, addr: u64, size: u32, value: &mut u64) -> bool {
    let first = ODSY_BACKEND_READ_BASE + ((ODSY_CURSOR_BASE as u64) << 3);

    if size != 4 || addr < first || (addr & 7) != 0 || addr >= first + ((ODSY_CURSOR_ENTRIES as u64) << 3) {
        return false;
    }
    *value = s.cursor[((addr - first) >> 3) as usize] as u64;
    true
}

// Back-end entries 0x2000-0x2fff: the line and frame tables, timing, cursor,
// DAC, clock, I2C, genlock, scanout and display-mode entries. Every word
// written there is kept, and a 32-bit load at offset 4 of the entry's 8-byte
// slot in the read window returns it (established by the kernel's
// read-modify-writes of 0x26c0, 0x2482, 0x2483 and 0x2700; that the board
// returns the last word written is inferred). Entries the board itself
// updates answer from their own models. Reset values are unknown; the model
// resets them to zero.
//
// The line table (0x2000-0x23ff) and frame table (0x2400-0x247f) are only
// stored. APPROXIMATION: the retrace period stays the 1280x1024 60 Hz one
// (ODSY_FRAME_CLOCKS) rather than following the frame table.
//
// The scanout entries 0x2800 and 0x2801, 0x2807 and 0x2809 are stored and
// not used: APPROXIMATION, the model scans out its own colour buffers
// (odyssey_resolve_row()) rather than locating the displayed buffer in board
// memory by these entries. The 16 x 16-pixel tiling of board memory is not
// modeled.

/// The entries the kernel writes (the datasheet's back-end entry table).
pub fn odyssey_backend_ctl_known(index: u32) -> bool {
    (0x2000..=0x247f).contains(&index)
        || (0x2481..=0x2483).contains(&index)
        || index == 0x2600
        || index == 0x2680
        || index == 0x26c0
        || index == 0x2700
        || index == 0x2741
        || index == 0x2781
        || (0x27c0..=0x27cd).contains(&index)
        || (0x2800..=0x2809).contains(&index)
        || (0x2929..=0x292c).contains(&index)
        || index == 0x2b00
}

pub fn odyssey_backend_ctl_read(s: &mut SGIOdysseyState, addr: u64, size: u32, value: &mut u64) -> bool {
    let first = ODSY_BACKEND_READ_BASE + ((ODSY_BACKEND_CTL_BASE as u64) << 3);

    if size != 4 || addr < first || (addr & 7) != 4 || addr >= first + ((ODSY_BACKEND_CTL_ENTRIES as u64) << 3) {
        return false;
    }
    *value = s.backend_ctl[((addr - first) >> 3) as usize] as u64;
    true
}

/// +0x1004 (ODSY_MEMORY_CONTROL): kept, and checked against the strap.
pub fn odyssey_memory_control(s: &mut SGIOdysseyState, value: u32) {
    let strap = s.memory_config;
    let size = (value >> ODSY_MEMORY_CONTROL_SIZE_SHIFT) & ODSY_MEMORY_CONTROL_SIZE_MASK;
    // The kernel programs strap code 3 like 128 MB, code 2.
    let strap_size = (strap >> ODSY_CONFIG_SIZE_SHIFT).min(2);

    s.memory_control = value;
    if !s.memory_control_reported {
        s.memory_control_reported = true;
        crate::qlog!("sgi-odyssey: memory controller configuration {:#x} stored; the controller is not modeled\n", value);
    }
    // The master id goes in first, with the strap fields still zero.
    if value & !(0xfu32 << 19) == 0 {
        return;
    }
    // MODEL CHOICE: a mismatch is reported; see ODSY_MEMORY_CONTROL.
    if size != strap_size
        || ((value & ODSY_MEMORY_CONTROL_FOUR_BANKS) != 0) != ((strap & ODSY_CONFIG_FOUR_BANKS) != 0)
        || ((value & ODSY_MEMORY_CONTROL_LATENCY_2) != 0) != ((strap & ODSY_CONFIG_LATENCY_2) != 0)
    {
        crate::qlog!("sgi-odyssey: memory controller configuration {:#x} disagrees with the strap word {:#x}\n", value, strap);
    }
}

pub fn odyssey_i2c_status(command: u32) -> u32 {
    ODSY_I2C_STATUS_READY | if command != 0 { ODSY_I2C_STATUS_DONE } else { 0 }
}

pub fn odyssey_read(s: &mut SGIOdysseyState, addr: u64, size: u32) -> u64 {
    let mut value: u64 = 0;
    let sequence = s.trace_sequence;
    s.trace_sequence = s.trace_sequence.wrapping_add(1);

    let reg = odyssey_reg(addr, size);
    let key = if reg < 0 { u64::MAX } else { reg as u64 };
    match key {
        ODSY_IDENTITY => value = odyssey_identity(s) as u64,
        ODSY_CONTROL => value = s.widget_control as u64,
        ODSY_XT_REQ_TIMEOUT => value = s.xt_req_timeout as u64,
        ODSY_INT_DEST_UPPER => value = s.int_dest_upper as u64,
        ODSY_INT_DEST_LOWER => value = s.int_dest_lower as u64,
        ODSY_ERROR_COMMAND => value = s.error_command as u64,
        ODSY_STATUS | ODSY_ERROR_ADDR_UPPER | ODSY_ERROR_ADDR_LOWER => {
            // No crosstalk error is ever posted, so the error class is clear.
        }
        ODSY_INTR_FLAGS => value = s.intr_flags as u64,
        ODSY_INTR_ENABLE_SET => value = s.intr_enable as u64,
        ODSY_STATUS0 => {
            // The waiting stores are its level (odyssey_fifo_level()).
            let level = odyssey_fifo_level(s);
            value = (ODSY_STATUS0_FIFO_ENABLED
                | if level != 0 { 0 } else { ODSY_STATUS0_FIFO_EMPTY }
                | if level < ODSY_CFIFO_LOW_WATER { ODSY_STATUS0_FIFO_LOW_WATER } else { 0 }) as u64;
        }
        ODSY_CFIFO_SELFTEST => {
            // Answers OK only after the kernel driver's command-FIFO
            // initialisation preamble; zero before.
            value = if s.cfifo_selftest_ready { ODSY_CFIFO_SELFTEST_OK as u64 } else { 0 };
        }
        ODSY_MEMORY_CONFIG => value = s.memory_config as u64,
        ODSY_FLCTL_CONFIG => value = s.flctl_config as u64,
        ODSY_FLCTL_TARGET => value = s.flctl_target as u64,
        ODSY_SW_TOKEN => value = s.sw_token as u64,
        ODSY_BUZZ_1 => value = s.buzz[0] as u64,
        ODSY_BUZZ_2 => value = s.buzz[1] as u64,
        ODSY_BUZZ_3 => value = s.buzz[2] as u64,
        ODSY_BUZZ_4 => value = s.buzz[3] as u64,
        ODSY_BUZZ_5 => value = s.buzz[4] as u64,
        ODSY_BUZZ_6 => value = s.buzz[5] as u64,
        ODSY_SWAP_ARM => value = s.swap_arm as u64,
        ODSY_SWAP_PENDING => value = s.swap_pending as u64,
        ODSY_SWAP_CONTROL => value = s.swap_control as u64,
        ODSY_SWAP_CONTROL_COMPANION => value = s.swap_control_companion as u64,
        // The buffer the last latched swap put on screen (odyssey_retrace()).
        ODSY_DISPLAYED_BUFFER => value = s.displayed_buffer as u64,
        ODSY_NIC_PROM | ODSY_NIC_KERNEL => value = s.nic as u64,
        ODSY_I2C_MAIN_CMD_READ => value = s.i2c_main_cmd as u64,
        ODSY_I2C_MAIN_STATUS_READ => value = odyssey_i2c_status(s.i2c_main_cmd) as u64,
        ODSY_I2C_OPT_CMD_READ => value = s.i2c_opt_cmd as u64,
        ODSY_I2C_OPT_STATUS_READ => value = odyssey_i2c_status(s.i2c_opt_cmd) as u64,
        ODSY_BACKEND_VERSION => value = (s.backend_version & 7) as u64,
        ODSY_ROUTE_DEST_UPPER
        | ODSY_ROUTE_DEST_LOWER
        | ODSY_ROUTE_VECTORS
        | ODSY_HOST_WRITE_ADDR_UPPER
        | ODSY_HOST_WRITE_ADDR_LOWER
        | ODSY_STATS_ADDR_UPPER
        | ODSY_STATS_ADDR_LOWER
        | ODSY_STATS_CONTROL
        | ODSY_CTX_SHADOW_ADDR_UPPER
        | ODSY_CTX_SHADOW_ADDR_LOWER
        | ODSY_RESET_CONTROL
        | ODSY_MEMORY_CONTROL => {
            crate::qlog!("sgi-odyssey: read of the write-only register at {:#x}\n", addr);
        }
        _ => {
            if odyssey_cursor_read(s, addr, size, &mut value) || odyssey_backend_ctl_read(s, addr, size, &mut value) {
                crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_mmio: seq {} offset {:#x} size {} value {:#x} write=0", sequence, addr, size, value);
                return value;
            }
            odyssey_unimplemented(s, addr, size, false);
        }
    }
    crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_mmio: seq {} offset {:#x} size {} value {:#x} write=0", sequence, addr, size, value);
    value
}

/// Present the plane's top-left width x height at the 1280x1024 rate.
pub fn odyssey_set_scanout_size(s: &mut SGIOdysseyState, width: u32, height: u32) {
    sgi_scanout_set_mode(
        &mut s.scanout,
        &SGIScanoutMode { width, height, frame_clocks: ODSY_FRAME_CLOCKS as u64, clock_hz: ODSY_PIXEL_CLOCK_HZ },
    );
}

/// One of the display channel's two size entries. A zero is the guest
/// blanking the channel while it changes format and leaves the picture as it
/// was. A size past the modeled planes is reported and kept to them.
pub fn odyssey_channel_size(s: &mut SGIOdysseyState, index: u32, data: u32) {
    let mut width = s.scanout.mode.width;
    let mut height = s.scanout.mode.height;

    if data == 0 {
        return;
    }
    if index == ODSY_CHANNEL_WIDTH {
        width = data;
    } else {
        height = data;
    }
    if width > SGI_ODYSSEY_WIDTH || height > SGI_ODYSSEY_HEIGHT {
        crate::qlog!(
            "sgi-odyssey: display channel {}x{} is larger than the modeled {}x{} planes\n",
            width,
            height,
            SGI_ODYSSEY_WIDTH,
            SGI_ODYSSEY_HEIGHT
        );
        width = width.min(SGI_ODYSSEY_WIDTH);
        height = height.min(SGI_ODYSSEY_HEIGHT);
    }
    if width != s.scanout.mode.width || height != s.scanout.mode.height {
        odyssey_set_scanout_size(s, width, height);
    }
}

/// One data word of a backend record, delivered to the entry the record's
/// header selected. The colour lookup RAM and the per-drawable display-mode
/// registers take their words here; the two I2C command indices keep their
/// existing dispatch. Entries 0x2000-0x2fff are also kept for the read
/// window (odyssey_backend_ctl_read()); one the kernel is not known to write
/// is still reported.
pub fn odyssey_backend_data(s: &mut SGIOdysseyState, index: u32, data: u32) {
    if index >= ODSY_BACKEND_CTL_BASE && index < ODSY_BACKEND_CTL_BASE + ODSY_BACKEND_CTL_ENTRIES {
        s.backend_ctl[(index - ODSY_BACKEND_CTL_BASE) as usize] = data;
    }
    // Scanout resolves through both, so either change repaints.
    if index < ODSY_LUT_SIZE {
        s.lut[index as usize] = data & ODSY_LUT_ENTRY_MASK;
        sgi_scanout_invalidate_all(&mut s.scanout);
        return;
    }
    if index >= ODSY_DISPMODE_BASE && index < ODSY_DISPMODE_BASE + ODSY_DISPMODE_COUNT {
        s.dispmode[(index - ODSY_DISPMODE_BASE) as usize] = data;
        sgi_scanout_invalidate_all(&mut s.scanout);
        return;
    }
    if index >= ODSY_CURSOR_BASE && index < ODSY_CURSOR_BASE + ODSY_CURSOR_ENTRIES {
        odyssey_cursor_write(s, index - ODSY_CURSOR_BASE, data);
        return;
    }
    if index >= ODSY_OVERLAY_MODE_BASE && index < ODSY_OVERLAY_MODE_BASE + ODSY_OVERLAY_IDS {
        s.overlay_mode[(index - ODSY_OVERLAY_MODE_BASE) as usize] = data;
        if (data & !ODSY_OVERLAY_MODE_KNOWN) != 0 && !s.overlay_mode_reported {
            s.overlay_mode_reported = true;
            crate::qlog!("sgi-odyssey: overlay mode {:#x} has bits this model does not decode\n", data);
        }
        sgi_scanout_invalidate_all(&mut s.scanout);
        return;
    }
    match index {
        ODSY_CHANNEL_WIDTH | ODSY_CHANNEL_HEIGHT => odyssey_channel_size(s, index, data),
        ODSY_I2C_MAIN_CMD_INDEX => s.i2c_main_cmd = data,
        ODSY_I2C_OPT_CMD_INDEX => s.i2c_opt_cmd = data,
        _ => {
            if !odyssey_backend_ctl_known(index) {
                odyssey_unimplemented(s, ODSY_BACKEND_SELECT, 8, true);
            }
        }
    }
}

/// One 32-bit word of the backend word stream. While no record is open the
/// word is a header: its 0b11 tag opens a record of `count` data words
/// starting at `index` (a zero count is a pad). Any word without the tag at a
/// header position has no established meaning and is reported rather than
/// guessed. While a record is open the word is a data word for the running
/// index, which then advances (see odyssey_backend_data).
pub fn odyssey_backend_word(s: &mut SGIOdysseyState, word: u32) {
    if s.backend_count == 0 {
        if ((word >> ODSY_BACKEND_TAG_SHIFT) & ODSY_BACKEND_TAG_MASK) != ODSY_BACKEND_TAG_HEADER {
            odyssey_unimplemented(s, ODSY_BACKEND_SELECT, 8, true);
            return;
        }
        s.backend_index = (word >> ODSY_BACKEND_SELECTOR_INDEX_SHIFT) & ODSY_BACKEND_SELECTOR_INDEX_MASK;
        s.backend_count = word & ODSY_BACKEND_COUNT_MASK;
        return;
    }
    odyssey_backend_data(s, s.backend_index, word);
    s.backend_index = (s.backend_index + 1) & ODSY_BACKEND_SELECTOR_INDEX_MASK;
    s.backend_count -= 1;
}

/// The indexed register file's write port: an 8-byte store is two words,
/// upper half first, a 4-byte store is one word (see ODSY_BACKEND_SELECT).
pub fn odyssey_backend_write(s: &mut SGIOdysseyState, value: u64, size: u32) {
    if size == 8 {
        odyssey_backend_word(s, (value >> 32) as u32);
        odyssey_backend_word(s, value as u32);
    } else {
        odyssey_backend_word(s, value as u32);
    }
}

/// Deterministic model defaults; physical back-material reset is unknown.
/// (The ODSY_QUERY_* enum and the hardware shadow-page constants are in
/// draw.rs and defs.rs.)
pub const odyssey_back_material_defaults: [u32; 16] = [
    ODSY_F32_POINT_TWO,
    ODSY_F32_POINT_TWO,
    ODSY_F32_POINT_TWO,
    ODSY_F32_ONE,
    ODSY_F32_POINT_EIGHT,
    ODSY_F32_POINT_EIGHT,
    ODSY_F32_POINT_EIGHT,
    ODSY_F32_ONE,
    0,
    0,
    0,
    ODSY_F32_ONE,
    0,
    0,
    0,
    ODSY_F32_ONE,
];
