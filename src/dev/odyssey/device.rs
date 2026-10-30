//! Ported from hw/misc/sgi-odyssey.c in jamesbraid/qemu-sgi-origami.

use super::*;

pub fn odyssey_write_dispatch(s: &mut SGIOdysseyState, addr: u64, value: u64, size: u32) {
    crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_mmio: seq {} offset {:#x} size {} value {:#x} write=1", s.trace_sequence, addr, size, value);
    s.trace_sequence = s.trace_sequence.wrapping_add(1);

    if addr == ODSY_BACKEND_SELECT && (size == 8 || size == 4) {
        odyssey_backend_write(s, value, size);
        return;
    }
    if addr == ODSY_CTX_SWITCH_PORT && size == 8 {
        odyssey_ctx_switch(s, value);
        return;
    }
    if addr == ODSY_CMD_FIFO {
        odyssey_selftest_track(s, ODSY_CMD_FIFO, size, value);
        s.cmd_from_kernel = true;
        odyssey_cmd_fifo_write(s, value, size);
        return;
    }
    if addr == ODSY_USER_CMD_FIFO {
        // The user-mapped command FIFO has the kernel port's grammar but
        // never carries the self-test preamble, so it skips the tracker.
        s.cmd_from_kernel = false;
        odyssey_cmd_fifo_write(s, value, size);
        return;
    }

    // -1 (no register) becomes u64::MAX, which no constant matches.
    match odyssey_reg(addr, size) as u64 {
        ODSY_CONTROL => s.widget_control = value as u32,
        ODSY_XT_REQ_TIMEOUT => s.xt_req_timeout = value as u32,
        ODSY_INT_DEST_UPPER => s.int_dest_upper = value as u32,
        ODSY_INT_DEST_LOWER => s.int_dest_lower = value as u32,
        // Written to acknowledge; the data itself is discarded.
        ODSY_ERROR_COMMAND => s.error_command = 0,
        ODSY_INTR_CLEAR => {
            s.intr_flags &= !(value as u32);
            odyssey_update_irq(s);
        }
        ODSY_INTR_ENABLE_SET => {
            s.intr_enable |= value as u32;
            odyssey_update_irq(s);
        }
        ODSY_INTR_ENABLE_CLEAR => {
            s.intr_enable &= !(value as u32);
            odyssey_update_irq(s);
        }
        ODSY_INTR_FLAGS => {
            // The flags take the value written: an inferred model choice.
            // A driver's flow control writes the low-water bit here before
            // enabling that source, and its DMA diagnostic clears one flag
            // by writing back the flags it read without it; only a
            // replacing write serves the second. The board's paired
            // registers (set enables, clear enables, clear flags) make a
            // set-only write the competing reading.
            s.intr_flags = value as u32;
            odyssey_update_irq(s);
        }
        ODSY_RESET_CONTROL => {
            crate::qlog!(
                "sgi-odyssey: write of {:#x} to the reset control register accepted; its effect is unknown\n",
                value
            );
        }
        ODSY_MEMORY_CONTROL => odyssey_memory_control(s, value as u32),
        ODSY_FLCTL_CONFIG => s.flctl_config = value as u32,
        ODSY_FLCTL_TARGET => {
            s.flctl_target = value as u32;
            if !s.flctl_target_reported {
                s.flctl_target_reported = true;
                crate::qlog!(
                    "sgi-odyssey: flow-control interrupt target {} and credit target {} stored; credits still return through the node interface\n",
                    (value as u32) >> ODSY_FLCTL_INTR_TARGET_SHIFT,
                    ((value >> ODSY_FLCTL_TARGET_SHIFT) as u32) & ODSY_FLCTL_TARGET_MASK
                );
            }
        }
        ODSY_SW_TOKEN => s.sw_token = value as u32,
        ODSY_BUZZ_1 => s.buzz[0] = value as u32,
        ODSY_BUZZ_2 => s.buzz[1] = value as u32,
        ODSY_BUZZ_3 => s.buzz[2] = value as u32,
        ODSY_BUZZ_4 => s.buzz[3] = value as u32,
        ODSY_BUZZ_5 => s.buzz[4] = value as u32,
        ODSY_BUZZ_6 => s.buzz[5] = value as u32,
        ODSY_SWAP_ARM => {
            // Queue flips for the next retrace. Writes accumulate: the
            // driver can issue several windows or swap groups between two
            // retraces, and its retrace handler re-writes one window's bit
            // when a flip has not shown after two retraces. Whether the
            // board ORs or replaces is unknown; ORing loses no flip.
            s.swap_arm |= value as u32;
        }
        ODSY_SWAP_PENDING => s.swap_pending = value as u32,
        ODSY_SWAP_CONTROL => {
            // The windows that must show buffer 1 from the next retrace on.
            // A later write to either register for the same window replaces
            // this one before the retrace latches it.
            s.swap_control = value as u32;
            s.swap_set |= value as u32;
            s.swap_clear &= !(value as u32);
        }
        ODSY_SWAP_CONTROL_COMPANION => {
            s.swap_control_companion = value as u32;
            s.swap_clear |= value as u32;
            s.swap_set &= !(value as u32);
        }
        ODSY_DISPLAYED_BUFFER => {
            // Board-produced status; a guest write is not seen in the
            // capture, but store it rather than drop it.
            s.displayed_buffer = value as u32;
            sgi_scanout_invalidate_all(&mut s.scanout);
        }
        ODSY_NIC_PROM | ODSY_NIC_KERNEL => odyssey_nic_slot(s, value as u32),
        ODSY_ROUTE_DEST_UPPER => s.route_dest_upper = value as u32,
        ODSY_ROUTE_DEST_LOWER => s.route_dest_lower = value as u32,
        ODSY_ROUTE_VECTORS => s.route_vectors = value as u32,
        ODSY_HOST_WRITE_ADDR_UPPER => {
            odyssey_query_cancel(s);
            s.host_write_addr_upper = value as u32;
        }
        ODSY_HOST_WRITE_ADDR_LOWER => {
            odyssey_query_cancel(s);
            s.host_write_addr_lower = value as u32;
        }
        ODSY_STATS_ADDR_UPPER | ODSY_STATS_ADDR_LOWER | ODSY_STATS_CONTROL => {
            if addr == ODSY_STATS_ADDR_UPPER {
                s.stats_addr_upper = value as u32;
            } else if addr == ODSY_STATS_ADDR_LOWER {
                s.stats_addr_lower = value as u32;
            } else {
                s.stats_control = value as u32;
            }
            if value != 0 && !s.stats_reported {
                s.stats_reported = true;
                crate::qlog!(
                    "sgi-odyssey: statistics region word {:#x} at {:#x} stored; the board's writes into the region are not modeled\n",
                    value as u32,
                    addr
                );
            }
        }
        ODSY_CTX_SHADOW_ADDR_UPPER => {
            odyssey_query_cancel(s);
            s.ctx_shadow_addr_upper = value as u32;
        }
        ODSY_CTX_SHADOW_ADDR_LOWER => {
            odyssey_query_cancel(s);
            s.ctx_shadow_addr_lower = value as u32;
        }
        ODSY_IDENTITY
        | ODSY_STATUS
        | ODSY_ERROR_ADDR_UPPER
        | ODSY_ERROR_ADDR_LOWER
        | ODSY_MEMORY_CONFIG
        | ODSY_I2C_MAIN_CMD_READ
        | ODSY_I2C_MAIN_STATUS_READ
        | ODSY_I2C_OPT_CMD_READ
        | ODSY_I2C_OPT_STATUS_READ
        | ODSY_BACKEND_VERSION => {
            crate::qlog!("sgi-odyssey: write of {:#x} to the read-only register at {:#x}\n", value, addr);
        }
        _ => odyssey_unimplemented(s, addr, size, true),
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
// So once a command-FIFO store from a processor has taken longer than one
// slice of host time (SGI_HEAD_WORK_SLICE_NS), the FIFO stores that follow
// within ODSY_WORK_HOLD_NS of machine time wait in a queue, in order, and
// run in slices on the board's thread (sgi_gfx/head_work.rs), their scalar
// draws without the board lock. The store that first runs long has already
// run whole. Light streams stay synchronous. Stores from outside any
// processor (unit tests) run whole unless work.test_words is set, which
// queues every FIFO store.
//
// The Octane has no PI link (only Hub and Bedrock machines have graphics
// credits), so `s.pi` stays None there and every credit call the C makes
// through s->pi is skipped, as it is in QEMU with the link unset.


pub fn odyssey_work_pending(s: &mut SGIOdysseyState) -> bool {
    !s.work_queue.is_empty()
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

/// A counted store's credit, to the processor interface the node names.
pub fn odyssey_return_credit(s: &mut SGIOdysseyState) {
    if let Some(pi) = &s.pi {
        sgi_pi_gfx_return_credits(pi.as_ref(), s.slot, 1);
    } else {
        crate::qlog!("sgi-odyssey: counted graphics store has no processor interface for its return\n");
    }
}

/// Waiting stores that have moved up into the FIFO get their credits. One
/// that waited outside it repays a credit the node holds as owed
/// (sgi_pi_gfx_owe_credits()).
pub fn odyssey_work_admit(s: &mut SGIOdysseyState) {
    while s.work_in_fifo < s.work_queue.len() as u32 && s.work_in_fifo < ODSY_CFIFO_DEPTH {
        let q = s.work_queue[s.work_in_fifo as usize];

        if q.owed {
            if let Some(pi) = &s.pi {
                sgi_pi_gfx_repay_credits(pi.as_ref(), s.slot, 1);
            }
            s.work_queue[s.work_in_fifo as usize].owed = false;
        } else if q.counted {
            odyssey_return_credit(s);
        }
        s.work_in_fifo += 1;
    }
}

pub fn odyssey_work_append(s: &mut SGIOdysseyState, addr: u64, value: u64, size: u32, counted: bool) {
    let q = OdysseyQueuedStore {
        value,
        addr: addr as u32,
        size: size as u8,
        counted,
        owed: false,
        queued_at: qemu_clock_virtual_ns(),
    };

    s.work_queue.push(q);
    odyssey_work_admit(s);
    if counted && s.work_in_fifo < s.work_queue.len() as u32 && s.pi.is_some() {
        let last = s.work_queue.len() - 1;
        s.work_queue[last].owed = true;
        sgi_pi_gfx_owe_credits(s.pi.as_ref().unwrap().as_ref(), s.slot, 1);
    }
    sgi_head_work_schedule(&mut s.work);
}

/// Run the oldest waiting stores until the budget is spent.
pub fn odyssey_work_step(opaque: Opaque, b: &mut SGIHeadWorkBudget) {
    let s = from_opaque::<SGIOdysseyState>(opaque);
    // qtest: a slice that holds the board as a long unlocked draw would.
    let sleep_ms = s.test_slice_sleep_ms;

    if sleep_ms != 0 && sgi_head_work_may_unlock(&mut s.work) {
        sgi_head_work_unlock(&mut s.work);
        std::thread::sleep(std::time::Duration::from_millis(sleep_ms as u64));
        sgi_head_work_relock(&mut s.work);
    }

    while !s.work_queue.is_empty() {
        let q = s.work_queue.remove(0);

        // The store leaves the FIFO, and the next waiting one moves up.
        s.work_in_fifo = s.work_in_fifo.wrapping_sub(1);
        odyssey_work_admit(s);
        odyssey_write_dispatch(s, q.addr as u64, q.value, q.size as u32);
        let level = s.work_queue.len() as u32 + 1;
        odyssey_fifo_level_changed(s, level);
        if sgi_head_work_spent(b) {
            break;
        }
    }
    if !s.work_queue.is_empty() {
        crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_work_behind: {} stores wait, the oldest for {} ns", s.work_queue.len(), qemu_clock_virtual_ns() - s.work_queue[0].queued_at);
    }
}

pub fn odyssey_work_pending_cb(opaque: Opaque) -> bool {
    odyssey_work_pending(from_opaque::<SGIOdysseyState>(opaque))
}

pub static odyssey_work_ops: SGIHeadWorkOps = SGIHeadWorkOps {
    step: Some(odyssey_work_step),
    pending: Some(odyssey_work_pending_cb),
    slice_done: None,
};

/// Finish the waiting work now, for an access at addr that must see it.
pub fn odyssey_work_drain(s: &mut SGIOdysseyState, addr: u64) {
    sgi_head_work_wait_slice(&mut s.work);
    if !odyssey_work_pending(s) {
        return;
    }
    let start = qemu_clock_realtime_ns();
    sgi_head_work_drain(&mut s.work);
    crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_work: drained offset {:#x} ns {}", addr, qemu_clock_realtime_ns() - start);
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

/// An access from inside any SGI head's access or work on this thread: a
/// board's DMA aimed at these registers. Refused and logged.
pub fn odyssey_refuse(addr: u64, write: bool) -> MemTxResult {
    crate::qlog!(
        "sgi-odyssey: {} at {:#x} from inside a graphics head's work refused\n",
        if write { "write" } else { "read" },
        addr
    );
    MEMTX_ERROR
}

pub fn odyssey_mmio_read(s: &mut SGIOdysseyState, addr: u64, size: u32) -> u64 {
    if !sgi_head_work_enter() {
        odyssey_refuse(addr, false);
        return 0;
    }
    if odyssey_work_pending(s) && odyssey_access_waits(addr, size, false) {
        odyssey_work_drain(s, addr);
    }
    let data = odyssey_read(s, addr, size);
    sgi_head_work_leave();
    data
}

/// Whether a store that joins the command stream waits in the queue: behind
/// any store that already waits, while a slice's draw has let go of the
/// board lock, and within ODSY_WORK_HOLD_NS of a FIFO store that ran long.
pub fn odyssey_store_queues(s: &mut SGIOdysseyState, addr: u64, size: u32) -> bool {
    if !odyssey_command_store(addr, size) {
        return false;
    }
    odyssey_work_pending(s)
        || s.work.drawing
        || s.work.test_words != 0
        || qemu_clock_virtual_ns() < s.work_queued_until
}

pub fn odyssey_mmio_write(s: &mut SGIOdysseyState, addr: u64, value: u64, size: u32) {
    // The Octane has no PI link, so nothing sets the mark there.
    let cpu = odyssey_current_cpu();
    let mut counted = sgi_pi_gfx_take_mark() || (s.test_counted_stores != 0 && !cpu);

    if !sgi_head_work_enter() {
        odyssey_refuse(addr, true);
        return;
    }
    if odyssey_store_queues(s, addr, size) {
        if s.work_queue.len() >= ODSY_WORK_QUEUE_MAX as usize && !s.work.drawing {
            odyssey_work_drain(s, addr);
        }
        odyssey_work_append(s, addr, value, size, counted);
        // Credited as it enters the FIFO (odyssey_work_admit()).
        counted = false;
    } else if odyssey_fifo_port(addr) && cpu {
        let start = qemu_clock_realtime_ns();

        odyssey_write_dispatch(s, addr, value, size);
        if qemu_clock_realtime_ns() - start > SGI_HEAD_WORK_SLICE_NS {
            s.work_queued_until = qemu_clock_virtual_ns() + ODSY_WORK_HOLD_NS;
            crate::dlog!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_work: queueing offset {:#x} ns {}", addr, qemu_clock_realtime_ns() - start);
        }
    } else {
        if odyssey_work_pending(s) && odyssey_access_waits(addr, size, true) {
            odyssey_work_drain(s, addr);
        }
        odyssey_write_dispatch(s, addr, value, size);
    }
    if counted {
        // It went through the FIFO inside the store.
        odyssey_return_credit(s);
    }
    sgi_head_work_leave();
}

/// Work is finished before the machine stops, so none is ever saved.
pub fn odyssey_work_vm_state(opaque: Opaque, running: bool, state: u32) {
    let _ = state;
    if !running {
        sgi_head_work_enter_work();
        odyssey_work_drain(from_opaque::<SGIOdysseyState>(opaque), u64::MAX);
        sgi_head_work_leave();
    }
}

/// Reset clears the FIFO and abandons the waiting stores.
pub fn odyssey_work_reset(s: &mut SGIOdysseyState) {
    sgi_head_work_wait_slice(&mut s.work);
    for q in s.work_queue.iter() {
        if q.owed {
            if let Some(pi) = &s.pi {
                sgi_pi_gfx_owe_credits(pi.as_ref(), s.slot, -1);
            }
        }
    }
    s.work_queue.clear();
    s.work_in_fifo = 0;
    s.work_queued_until = 0;
    sgi_head_work_idle(&mut s.work);
}


pub fn odyssey_fifo_page_read(s: &mut SGIOdysseyState, addr: u64, size: u32) -> u64 {
    odyssey_mmio_read(s, addr, size)
}

pub fn odyssey_fifo_page_write(s: &mut SGIOdysseyState, addr: u64, value: u64, size: u32) {
    odyssey_mmio_write(s, addr, value, size)
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
pub fn odyssey_build_nic_rom(s: &mut SGIOdysseyState, rom: &mut [u8; ODSY_NIC_ROM_SIZE as usize]) {
    let fields: &str = if !s.nic_record.is_empty() {
        &s.nic_record
    } else if !s.product_record.is_empty() {
        &s.product_record
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

pub fn odyssey_attach_nic(s: &mut SGIOdysseyState) -> Result<(), String> {
    let mut rom = [0u8; ODSY_NIC_ROM_SIZE as usize];

    if s.nic_serial & !ODSY_NIC_SERIAL_MASK != 0 {
        return Err(format!("the sgi-odyssey one-wire serial {:#x} does not fit in 48 bits", s.nic_serial));
    }
    odyssey_build_nic_rom(s, &mut rom);
    let mut bus = crate::dev::w1::Bus::new();
    bus.add(Box::new(crate::dev::w1::Ds2505::new(ODSY_NIC_FAMILY as u8, s.nic_serial, &rom)));
    s.nic_bus = Some(bus);
    Ok(())
}

pub fn odyssey_compose_memory_config(s: &mut SGIOdysseyState) -> Result<(), String> {
    let code: u32 = match s.memory_size {
        32 => 0,
        64 => 1,
        128 => 2,
        _ => {
            return Err(format!("sgi-odyssey memory-size must be 32, 64 or 128 megabytes, not {}", s.memory_size));
        }
    };
    if s.memory_banks != 2 && s.memory_banks != 4 {
        return Err(format!("sgi-odyssey memory-banks must be 2 or 4, not {}", s.memory_banks));
    }
    if s.cas_latency != 2 && s.cas_latency != 3 {
        return Err(format!("sgi-odyssey cas-latency must be 2 or 3, not {}", s.cas_latency));
    }
    // The outstanding-operation count and the data-FIFO level, which the
    // driver waits to reach zero, stay zero: neither is modeled.
    s.memory_config = code << ODSY_CONFIG_SIZE_SHIFT
        | (if s.memory_banks == 4 { ODSY_CONFIG_FOUR_BANKS } else { 0 })
        | (if s.cas_latency == 2 { ODSY_CONFIG_LATENCY_2 } else { 0 })
        | ODSY_CONFIG_COPY_FIELD;
    Ok(())
}

pub fn odyssey_reset_registers(s: &mut SGIOdysseyState) {
    // The driver reads its own widget number out of the low nibble before
    // anything has written the register. The rest of the reset value is not
    // established, so it is zero.
    s.widget_control = s.slot & ODSY_CONTROL_WIDGET_MASK;
    s.xt_req_timeout = ODSY_XT_REQ_TIMEOUT_RESET;
    s.int_dest_upper = 0;
    s.int_dest_lower = 0;
    s.error_command = 0;
    // The command FIFO is empty out of reset, so its low-water condition
    // holds and the low-water interrupt is pending. The kernel driver's
    // board reset arms a watchdog and then tears the board down unless the
    // driver sees this interrupt once, which is what lets it create the
    // direct rendering node the X server opens. Latch it at reset, because
    // the guest cannot reach the later FIFO-drain edge (a command
    // submission) until that node exists. Masked here until the driver
    // enables the source; enabling it then asserts.
    s.intr_flags = ODSY_INTR_FIFO_LOW_WATER;
    s.intr_enable = 0;
    // The level falls with the enables. The routed destination is reset
    // below, so no clearing packet has anywhere to go.
    s.interrupt_asserted = false;
    s.flctl_config = ODSY_FLCTL_CONFIG_RESET;
    s.memory_control = 0;
    s.flctl_target = 0;
    s.fifo_setup = [0; 5];
    s.sw_token = 0;
    s.route_dest_upper = 0;
    s.route_dest_lower = 0;
    s.route_vectors = 0;
    s.host_write_addr_upper = 0;
    s.host_write_addr_lower = 0;
    s.stats_addr_upper = 0;
    s.stats_addr_lower = 0;
    s.stats_control = 0;
    s.ctx_shadow_addr_upper = 0;
    s.ctx_shadow_addr_lower = 0;
    s.buzz = [0; 6];
    s.swap_arm = 0;
    s.swap_pending = 0;
    s.swap_control = 0;
    s.swap_control_companion = 0;
    s.swap_set = 0;
    s.swap_clear = 0;
    s.displayed_buffer = 0;
    s.draw_mask = ODSY_DRAW_MASK_RESET;
    s.draw_word = 0;
    s.read_word = 0;
    s.draw_mask_stereo_reported = false;
    s.draw_mask_high_reported = false;
    s.drawable_word_reported = false;
    s.nic = 0;
    s.i2c_main_cmd = 0;
    s.i2c_opt_cmd = 0;
    s.lut.fill(0);
    s.backend_ctl.fill(0);
    s.dispmode = [0; 32];
    s.overlay_mode = [0; 8];
    s.overlay_mode_reported = false;
    s.cursor = [0; 0x100];
    s.cursor_reported = 0;
    s.gl_aux_reported = false;
    s.aux_mask_reported = false;
    s.aux_image_reported = false;
    s.backend_index = 0;
    s.backend_count = 0;
    s.reported_addr_count = 0;
    s.reported_cmd_count = 0;
    s.line_stipple_reported = false;
    s.dma_bpp_reported = false;
    s.dma_readback_reported = false;
    s.dma_code_reported = false;
    s.dma_read_reported = 0;
    s.selftest_progress = 0;
    s.cfifo_selftest_ready = false;

    odyssey_query_cancel(s);
    odyssey_query_records_reset(s);
    s.win_origin = [0; 2];
    s.read_origin = [0; 2];
    for i in 0..SGI_ODYSSEY_CLIP_RECTS as usize {
        s.clip_words[2 * i] = if i != 0 { ODSY_CLIP_RESET_EMPTY } else { 0 };
        s.clip_words[2 * i + 1] = if i != 0 { 0 } else { ODSY_CLIP_RESET_FULL };
    }
    odyssey_clip_update(s);
    s.id_compare = 0;
    s.win_committed_origin = s.win_origin;
    s.win_committed_clip = s.clip_words;
    s.win_committed_id_compare = 0;
    s.win_prior_origin = [0; 2];
    s.win_origin_pending = false;
    s.win_after_tail = false;
    s.cmd_from_kernel = false;
    s.gl_primitive = 0;
    s.gl_color = 0;
    // GL's initial index is 1 (OpenGL 1.2.1 section 2.7), the value the
    // index record's default saves and a replay restores, so a context that
    // never sets an index draws GL vertices in index 1 before and after its
    // first switch. The board's reset index is unknown; this is a model
    // default. Limitation: the integer rectangle fill takes the index only
    // after an index word, so with none since reset it draws the packed
    // colour 0, and index 1 after a replay. Only a register test reaches
    // that: the X server sends an index or a colour before every fill.
    s.gl_color_index = 1;
    s.gl_color_is_index = false;
    // Out of reset the mode is unknown; RGBA until a context says otherwise.
    s.colour_mode = [0; 2];
    s.colour_mode_reported = false;
    s.gl_index_lit_reported = false;
    s.dest_buffer_word = 0;
    s.plane_write_mask = 0;
    s.dma_colour_base = 0;
    s.dest_base = 0;
    s.obj_count = 0;
    s.obj_used = 0;
    s.obj_base.fill(0);
    s.obj_index.clear();
    s.obj_width.fill(0);
    s.obj_offset.fill(0);
    s.obj_rows.fill(0);
    s.obj_stamp.fill(0);
    s.obj_clock = 0;
    s.obj_valid.fill(0);
    s.obj_full_reported = false;
    s.obj_forget_reported = false;
    s.board_oob_reported = false;
    s.tex_mode = 0;
    s.tex_sampler = [0; 7];
    for t in s.tex_colour_table.iter_mut() {
        t.fill(0);
    }
    s.tex_colour_table_valid = [0; 2];
    s.tex_detail_function.fill(0);
    s.tex_detail_function_valid = 0;
    s.tex_env = [0; 5];
    s.tex_levels.fill(0);
    s.tex_level0 = 0;
    s.tex_lod[0] = 0;
    s.tex_lod[1] = 0;
    s.tex_lod[2] = 0x3100;
    s.tex_lod[3] = 0x0f00;
    for c in 0..4usize {
        s.tex_scale_bias[c * 2] = ODSY_TEX_SCALE_BIAS_ONE;
        s.tex_scale_bias[c * 2 + 1] = 0;
    }
    s.tex_offset = [0; 3];
    s.tex_coord = [0; 2];
    s.tex_coord_r = 0;
    s.tex_coord_q = ODSY_F32_ONE;
    s.tex_rect_reported = false;
    s.gl_vertex_x.fill(0);
    s.gl_vertex_y.fill(0);
    s.gl_vertex_count = 0;
    s.line_stipple[0] = 0xffff;
    s.line_stipple[1] = 0xffff;
    s.line_stipple[2] = 0;
    // No polygon is open; reset_hold clears the marks themselves.
    s.poly_row_lo = SGI_ODYSSEY_HEIGHT;
    s.poly_row_hi = 0;
    s.poly_col_lo = SGI_ODYSSEY_WIDTH;
    // The shared GL context to OpenGL's initial state (identity matrices,
    // white current colour, depth test off), unbound until the first GL
    // command binds the drawable. A guest reset can land mid-primitive, with
    // an open polygon's vertex buffer still allocated; sgi_gl_reset() frees
    // it before reinitializing. It is safe here even before realize
    // allocates the colour plane. Its viewport starts empty: nothing is
    // drawn until the client's combined matrix arrives.
    sgi_gl_reset(&mut s.gl_ctx, ODSY_GL_CONTEXT_HW_ID);
    {
        // Opt into the shared context's per-face polygon modes (0x0415) and
        // line stipple: unset, it silently keeps every face at legacy FILL
        // no matter what 0x0415 decodes to. The sink built here always
        // carries the checked draw callback, this call's one precondition;
        // sgi_gl_reset() above has already cleared in_begin, satisfying the
        // other precondition even when reset lands mid-primitive.
        let sink = odyssey_gl_sink(s);

        assert!(sgi_gl_enable_polygon_modes(&mut s.gl_ctx, Some(&sink)));
        assert!(sgi_gl_enable_line_stipple(&mut s.gl_ctx, Some(&sink)));
    }
    s.gl_in_prim = false;
    s.gl_begin_open = false;
    s.gl_integer_prim = false;
    s.gl_open_r = 0;
    s.gl_open_c = 0;
    s.gl_open_m3 = 0;
    s.gl_open_m4 = 0;
    s.gl_open_held = 0;
    s.gl_restart_type = 0;
    s.gl_restart_select = 0;
    s.gl_restart_absorb = 0;
    s.gl_restart_total = 0;
    s.gl_restart_c = 0;
    s.gl_restart_m3 = 0;
    s.gl_restart_m4 = 0;
    s.gl_restart_armed = false;
    // The clip box and its scissor test: sgi_gl_init() leaves the shared
    // context's scissor box and test both at zero, which would clip away
    // every draw if the test were simply switched on. There is no reset
    // pattern-mode value that already matches a usable scissor default, so
    // the full window is set here explicitly before the test is enabled,
    // once and for good.
    s.gl_clip_box[0] = ODSY_CLIP_BOX_FIELD_MASK & (SGI_ODYSSEY_WIDTH - 1);
    s.gl_clip_box[1] = ODSY_CLIP_BOX_FIELD_MASK & (SGI_ODYSSEY_HEIGHT - 1);
    s.gl_fog = [0; 6];
    odyssey_gl_apply_clip_box(s);
    {
        let scissor_test = SGI_GL_SCISSOR_TEST;

        odyssey_gl_feed(s, SGI_GL_OP_ENABLE as u16, &[scissor_test]);
    }
    s.gl_matrix_slot = SGI_GL_MODELVIEW;
    s.gl_modelview = [0; 16];
    for i in 0..4usize {
        s.gl_modelview[i * 5] = ODSY_F32_ONE;
    }
    s.gl_clip_eye.fill(0);
    s.gl_matrix = [0; 16];
    s.gl_texture_matrix = [0; 16];
    for i in 0..4usize {
        s.gl_texture_matrix[i * 5] = ODSY_F32_ONE;
    }
    s.gl_rect = [0; 10];
    s.gl_depth_func = ODSY_DEPTH_FUNC_RESET;
    s.gl_write_mask = ODSY_WRITE_MASK_RESET;
    s.gl_combined = [0; 16];
    // The identity projection's (-0, -0, -0, 1): the depth row unflipped.
    s.gl_block16[0] = 0x80000000;
    s.gl_block16[1] = 0x80000000;
    s.gl_block16[2] = 0x80000000;
    s.gl_block16[3] = 0x3f800000;
    s.gl_rect_depth_logged = false;
    s.gl_rect_stencil_logged = false;
    s.gl_rect_pattern_logged = false;
    s.gl_index_mask_logged = false;
    // Lighting out of reset. The hardware's values are unknown; the model
    // starts at GL's initial material, light-model ambient, normal and
    // colour, zero light colours and vectors (the library loads every light
    // before a lit draw), an identity normal matrix and no program.
    s.gl_const.fill(0);
    // Program memory's reset content is unknown; the kernel loads it.
    s.gl_program_mem.fill(0);
    s.gl_program_addr = 0;
    s.gl_seq = [0; 22];
    s.gl_program_half = 0;
    s.gl_program_mem_reported = false;
    s.gl_const_index = 0;
    s.gl_entry = [0; SGI_ODYSSEY_GL_ENTRIES as usize];
    // GL's own initial state, colour material off, as the library sends.
    s.gl_colour_material = 0;
    s.gl_light_colour.fill(0);
    for i in 0..3usize {
        s.gl_light_colour[ODSY_GL_LIGHT_MODEL_AMBIENT as usize * 4 + i] = ODSY_F32_POINT_TWO;
        s.gl_material[i] = ODSY_F32_POINT_TWO; // ambient
        s.gl_material[4 + i] = ODSY_F32_POINT_EIGHT; // diffuse
        s.gl_material[8 + i] = 0; // specular
        s.gl_material[12 + i] = 0; // emission
    }
    for i in 0..4usize {
        s.gl_material[i * 4 + 3] = ODSY_F32_ONE;
        s.gl_colour[i] = ODSY_F32_ONE;
    }
    s.gl_light_colour[ODSY_GL_LIGHT_MODEL_AMBIENT as usize * 4 + 3] = ODSY_F32_ONE;
    s.gl_light_vec.fill(0);
    s.gl_light_colour_sel = 0;
    s.gl_light_vec_sel = 0;
    s.gl_shininess_sqrt = 0;
    odyssey_back_material_reset(s);
    s.gl_normal[0] = 0;
    s.gl_normal[1] = 0;
    s.gl_normal[2] = ODSY_F32_ONE;
    s.gl_normal_matrix = [0; 16];
    for i in 0..4usize {
        s.gl_normal_matrix[i * 5] = ODSY_F32_ONE;
    }
    s.gl_lit = false;
    s.gl_local_viewer = false;
    s.gl_colour_overridden = false;
    s.gl_program_logged_count = 0;
    s.gl_two_sided_logged = false;
    s.gl_texgen_logged = false;
    s.gl_linear_texgen = 0;
    s.tex_linear_plane = [[0; 4]; 2];
    s.tex_linear_plane_r = [0; 4];
    s.tex_linear_plane_q = [0; 4];
    s.tex_linear_valid = 0;
    s.gl_clip_logged = false;
    s.gl_texture_stage_logged = false;
    s.gl_block_logged = false;
    s.ctx_open_prim_logged = false;
    s.ctx_shadow_fail_reported = false;
    s.gl_dither = false;
    s.gl_dither_reported = false;
    s.scanout_group25_reported = false;
    s.copy_src = 0;
    s.copy_size = 0;
    s.copy_dst = 0;
    s.pattern_buffer = [0; 32];
    s.pattern_shape = 0;
    s.const_colour = [0; 2];
    s.blend_word = 0;
    s.pattern_mode = 0;
    s.pattern_load_offset = 0;
    s.image_active = false;
    s.image_x1 = 0;
    s.image_y1 = 0;
    s.image_x2 = 0;
    s.image_y2 = 0;
    s.image_cur_x = 0;
    s.image_cur_y = 0;
    s.image_fmt = 0;
    s.image_group_len = 0;
    s.image_group_painted = 0;
    s.image_code = 0;
    s.image_bytes = 0;
    s.image_nbytes = 0;
    s.image_code_reported = false;
    s.dma_seq = 0;
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

pub fn odyssey_resolve_product(s: &mut SGIOdysseyState) -> Result<(), String> {
    if s.product.is_empty() {
        return Ok(());
    }
    for p in odyssey_products.iter() {
        if s.product == p.name {
            s.revision = p.revision;
            s.memory_size = p.memory_size;
            s.memory_banks = p.memory_banks;
            s.backend_version = p.backend_version;
            s.product_record = p.record.to_string();
            return Ok(());
        }
    }
    Err(format!("sgi-odyssey product must be v12, v10, v8 or v6, not {}", s.product))
}

pub fn odyssey_build_widget(s: &mut SGIOdysseyState) -> Result<(), String> {
    odyssey_resolve_product(s)?;
    if s.revision > 0xf {
        return Err(format!("sgi-odyssey revision must fit in four bits, not {}", s.revision));
    }
    odyssey_compose_memory_config(s)?;
    odyssey_reset_registers(s);
    Ok(())
}

pub fn odyssey_realize(s: &mut SGIOdysseyState) -> Result<(), String> {
    let mut err: Option<String> = None;

    odyssey_attach_nic(s)?;

    // The colour framebuffer is two RGBA8 planes, one per colour buffer,
    // each over device-backed board memory of its own: a model-owned byte
    // array. It is not page-backed onto guest RAM. add_plane can only fail
    // on a programming error in these fixed sizes, so a failure panics.
    let pixels = SGI_ODYSSEY_WIDTH as u64 * SGI_ODYSSEY_HEIGHT as u64;
    sgi_board_memory_init(&mut s.mem, pixels * 4);
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

        s.colour_plane = sgi_board_memory_add_plane(&mut s.mem, &plane, &mut err);
        // Colour buffer 1, the same shape in its own memory. Two model
        // planes preserve the visible contract; they do not claim the
        // board's two-buffer layout.
        sgi_board_memory_init(&mut s.buffer1_mem, plane.len);
        s.buffer1_plane = sgi_board_memory_add_plane(&mut s.buffer1_mem, &plane, &mut err);
    }

    // The board-memory object store: device-backed words the objects are
    // laid in by odyssey_object_rows(), addressed directly.
    sgi_board_memory_init(&mut s.obj_mem, ODSY_OBJ_POOL_BYTES as u64);

    s.did = vec![0u8; pixels as usize];
    s.overlay = vec![0u8; pixels as usize];
    s.poly_marks = vec![0u8; pixels as usize];

    // The depth plane: 24-bit depth in a 32-bit word per frame pixel, in
    // device-backed memory of its own. Where the board keeps the depth
    // buffer, and its width and layout, are unknown, so no aperture reaches
    // this plane. 24 bits matches the Z24 names in the GL library.
    sgi_board_memory_init(&mut s.depth_mem, pixels * 4);
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

        s.depth_plane = sgi_board_memory_add_plane(&mut s.depth_mem, &depth, &mut err);
    }

    // The stencil plane: 8 bits a pixel, in device-backed memory of its own;
    // like the depth plane, no aperture reaches it.
    sgi_board_memory_init(&mut s.stencil_mem, pixels);
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

        s.stencil_plane = sgi_board_memory_add_plane(&mut s.stencil_mem, &stencil, &mut err);
    }
    if let Some(e) = err {
        panic!("sgi-odyssey: {}", e);
    }

    // The shared scanout owns the presented frame, resolving the colour
    // plane through odyssey_head_ops.resolve_row. Setting the mode allocates
    // the frame, marks it all dirty and arms the retrace at the mode's rate.
    // The state never moves after sgi_odyssey_new returns, so its address is
    // a stable opaque.
    let opaque = P::new(&mut *s).opaque();
    sgi_scanout_init(&mut s.scanout, &odyssey_head_ops, opaque);
    odyssey_set_scanout_size(s, SGI_ODYSSEY_WIDTH, SGI_ODYSSEY_HEIGHT);
    s.work_queue = Vec::new();
    sgi_head_work_init(&mut s.work, "sgi-odyssey-work", &odyssey_work_ops, opaque);
    Ok(())
}

pub fn odyssey_unrealize(s: &mut SGIOdysseyState) {
    sgi_head_work_cleanup(&mut s.work);
    s.work_queue = Vec::new();
    sgi_gl_cleanup(&mut s.gl_ctx);
    sgi_scanout_cleanup(&mut s.scanout);
    s.did = Vec::new();
    s.overlay = Vec::new();
    s.poly_marks = Vec::new();
    sgi_board_memory_cleanup(&mut s.mem);
    sgi_board_memory_cleanup(&mut s.buffer1_mem);
    sgi_board_memory_cleanup(&mut s.obj_mem);
    sgi_board_memory_cleanup(&mut s.depth_mem);
    sgi_board_memory_cleanup(&mut s.stencil_mem);
}

pub fn odyssey_reset_hold(s: &mut SGIOdysseyState) {
    odyssey_work_reset(s);
    odyssey_reset_registers(s);
    odyssey_update_irq(s);
    // odyssey_build_widget() also calls odyssey_reset_registers(), before
    // the board memory exists (realize initializes it afterwards) -- this
    // runs strictly after realize, so the board memory exists here. Zero the
    // device-backed colour plane's bytes directly; its planes are realize-
    // time configuration and stay. Redraw because reset clears it.
    s.mem.bytes.fill(0);
    s.buffer1_mem.bytes.fill(0);
    s.obj_mem.bytes.fill(0);
    s.stencil_mem.bytes.fill(0);
    s.did.fill(0);
    s.overlay.fill(0);
    s.poly_marks.fill(0);
    // The depth plane starts at the far value, all ones (a model choice).
    // Under the floating-point code depth 1.0 is 0xfe0000, and every code of
    // a depth in [0, 1] is at most that, so all ones still compares as
    // farther than any fragment.
    let depth = *sgi_board_memory_plane(&s.depth_mem, s.depth_plane).get();
    for y in 0..SGI_ODYSSEY_HEIGHT as i32 {
        for x in 0..SGI_ODYSSEY_WIDTH as i32 {
            sgi_plane_store(&mut s.depth_mem, &depth, x, y, ODSY_DEPTH24_MAX);
        }
    }
    // Back to the boot size, then forget swaps and the cursor, repaint the
    // cleared plane and restart the phase.
    if s.scanout.mode.width != SGI_ODYSSEY_WIDTH || s.scanout.mode.height != SGI_ODYSSEY_HEIGHT {
        odyssey_set_scanout_size(s, SGI_ODYSSEY_WIDTH, SGI_ODYSSEY_HEIGHT);
    }
    sgi_scanout_reset(&mut s.scanout);
}

pub fn odyssey_instance_init(s: &mut SGIOdysseyState) {
    s.obj_index = Default::default();
    // The fixed part of the drawable odyssey_gl_bind() hands over.
    s.gl_bind_drawable.bound = 1;
    s.gl_bind_drawable.front = 0;
    s.gl_bind_drawable.back = 1;
}

/// The synthetic one-wire serial for a component letter and index.
pub fn sgi_synthetic_onewire_serial(letter: char, index: u32) -> u64 {
    0x51454d55u64 << 16 | (letter as u64) << 8 | index as u64
}

/// The board as the QEMU device is built: instance_init, the property
/// defaults (odyssey_properties), `product` and the slot, then realize.
pub fn sgi_odyssey_new(product: &str, slot: u32) -> Result<Box<SGIOdysseyState>, String> {
    let mut s = Box::new(SGIOdysseyState::default());

    odyssey_instance_init(&mut s);
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
    odyssey_build_widget(&mut s)?;
    odyssey_realize(&mut s)?;
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
        let mut s = sgi_odyssey_new(product, VPRO_WIDGET).expect("board");
        s.irq = Some(rec.clone());
        (s, rec)
    }

    #[test]
    fn widget_identity() {
        let (mut s, _) = board("v12");
        assert_eq!(odyssey_mmio_read(&mut s, IDENTITY, 4) as u32, IDENTITY_DEFAULT);
        assert_eq!(odyssey_mmio_read(&mut s, CONTROL, 4) as u32 & ODSY_CONTROL_WIDGET_MASK, VPRO_WIDGET);
    }

    #[test]
    fn profile_identity_and_routing() {
        // (product, revision, memory size code)
        let cases = [("v6", 2u32, 0u32), ("v8", 2, 2), ("v10", 0xa, 0), ("v12", 0xa, 2)];

        for (name, revision, size_code) in cases {
            let (mut s, rec) = board(name);
            let identity = odyssey_mmio_read(&mut s, IDENTITY, 4) as u32;
            let memory = odyssey_mmio_read(&mut s, MEMORY_CONFIG, 4) as u32;

            assert_eq!(identity >> 28, revision, "{name}");
            assert_eq!(identity & 0x0fff_ffff, IDENTITY_DEFAULT & 0x0fff_ffff, "{name}");
            assert_eq!((memory >> 30) & 3, size_code, "{name}");
            assert_eq!(odyssey_mmio_read(&mut s, CONTROL, 4) as u32 & ODSY_CONTROL_WIDGET_MASK, VPRO_WIDGET);

            odyssey_mmio_write(&mut s, ROUTE_DEST_UPPER, HEART_WIDGET << 25, 4);
            odyssey_mmio_write(&mut s, ROUTE_DEST_LOWER, HEART_PACKET, 4);
            odyssey_mmio_write(&mut s, ROUTE_VECTORS, TEST_VECTOR as u64, 4);
            odyssey_mmio_write(&mut s, INTR_ENABLE_SET, INTR_FIFO_LOW_WATER, 4);
            assert!(rec.level(TEST_VECTOR), "{name}: vector raised");
            odyssey_mmio_write(&mut s, INTR_CLEAR, INTR_FIFO_LOW_WATER, 4);
            assert!(!rec.level(TEST_VECTOR), "{name}: vector lowered");
        }
    }

    /// A raised and a lowered board interrupt each reach HEART, setting and
    /// clearing the vector's pending bit.
    #[test]
    fn interrupt_reaches_heart() {
        let (mut s, rec) = board("v12");

        odyssey_mmio_write(&mut s, ROUTE_DEST_UPPER, HEART_WIDGET << 25, 4);
        odyssey_mmio_write(&mut s, ROUTE_DEST_LOWER, HEART_PACKET, 4);
        odyssey_mmio_write(&mut s, ROUTE_VECTORS, TEST_VECTOR as u64, 4);
        assert!(!rec.level(TEST_VECTOR));

        // The FIFO is empty, so its low-water flag is already pending.
        odyssey_mmio_write(&mut s, INTR_ENABLE_SET, INTR_FIFO_LOW_WATER, 4);
        assert!(rec.level(TEST_VECTOR));

        odyssey_mmio_write(&mut s, INTR_CLEAR, INTR_FIFO_LOW_WATER, 4);
        assert!(!rec.level(TEST_VECTOR));
    }

    /// A burst of command-FIFO stores queued while the board is locked runs on
    /// the board's thread in the order stored, and the low-water flag rises,
    /// and its interrupt with it, once the queue drains below the low-water
    /// mark.
    #[test]
    fn work_thread_runs_queued_stores_in_order() {
        const STATUS0: u64 = 0x1064;
        let (s, rec) = board("v12");
        let state = Arc::new(Mutex::new(s));

        odyssey_work_start(&state);
        {
            let mut s = state.lock();
            odyssey_mmio_write(&mut s, ROUTE_DEST_UPPER, HEART_WIDGET << 25, 4);
            odyssey_mmio_write(&mut s, ROUTE_DEST_LOWER, HEART_PACKET, 4);
            odyssey_mmio_write(&mut s, ROUTE_VECTORS, TEST_VECTOR as u64, 4);
            odyssey_mmio_write(&mut s, INTR_CLEAR, INTR_FIFO_LOW_WATER, 4);
            odyssey_mmio_write(&mut s, INTR_ENABLE_SET, INTR_FIFO_LOW_WATER, 4);
            assert!(!rec.level(TEST_VECTOR));
            // Queue every FIFO store and run one per slice.
            s.work.test_words = 1;
        }

        // The self-test preamble twice: it completes only in order.
        let words: Vec<&OdysseyCfifoPreamble> = odyssey_cfifo_preamble.iter().chain(odyssey_cfifo_preamble.iter()).collect();
        assert!(words.len() as u32 > ODSY_CFIFO_LOW_WATER);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        {
            // Every store goes in under one hold of the board lock, so the
            // board's thread cannot start on them until it is let go.
            let mut s = state.lock();
            for w in &words {
                odyssey_mmio_write(&mut s, w.port, w.value, w.size);
            }
            let level = odyssey_fifo_level(&mut s);
            assert!(level >= ODSY_CFIFO_LOW_WATER, "level {level}: the first slice should still hold the queue");
            assert!(!s.cfifo_selftest_ready);
            assert_eq!(s.intr_flags & ODSY_INTR_FIFO_LOW_WATER, 0);
            // The status register does not wait: not empty, above low water.
            let status = odyssey_mmio_read(&mut s, STATUS0, 4) as u32;
            assert_eq!(status & (ODSY_STATUS0_FIFO_EMPTY | ODSY_STATUS0_FIFO_LOW_WATER), 0);
            assert_eq!(odyssey_fifo_level(&mut s), level);
        }

        // Watch the queue drain on the board's thread, without draining it
        // from here: the flag is up exactly when the level is below the mark.
        loop {
            {
                let mut s = state.lock();
                let level = odyssey_fifo_level(&mut s);
                let flag = s.intr_flags & ODSY_INTR_FIFO_LOW_WATER != 0;
                assert_eq!(flag, level < ODSY_CFIFO_LOW_WATER, "level {level}");
                if level == 0 && !s.work.slice {
                    break;
                }
            }
            assert!(std::time::Instant::now() < deadline, "work thread did not drain the queue");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        {
            let mut s = state.lock();
            assert!(s.cfifo_selftest_ready);
            assert_eq!(s.selftest_progress, 0);
            assert!(rec.level(TEST_VECTOR));
            let status = odyssey_mmio_read(&mut s, STATUS0, 4) as u32;
            assert_eq!(
                status & (ODSY_STATUS0_FIFO_EMPTY | ODSY_STATUS0_FIFO_LOW_WATER),
                ODSY_STATUS0_FIFO_EMPTY | ODSY_STATUS0_FIFO_LOW_WATER
            );
        }
        odyssey_work_stop(&state);
        assert!(!state.lock().work.started);
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
        let (mut s, _) = board("v12");
        let pi = Arc::new(PiStub::default());
        s.pi = Some(pi.clone());
        s.test_counted_stores = 1;
        s.work.test_words = 1;
        // No work thread runs: with none started the queue would drain on
        // the storing caller, so mark it started and step by hand.
        s.work.started = true;
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
            odyssey_mmio_write(&mut s, ODSY_CMD_FIFO, 0, 8);
        }
        assert_eq!(s.work_queue.len() as u32, stores);
        assert_eq!(s.work_in_fifo, ODSY_CFIFO_DEPTH);
        assert_eq!(pi.count("return"), ODSY_CFIFO_DEPTH as i64);
        assert_eq!(pi.count("owe"), (stores - ODSY_CFIFO_DEPTH) as i64);
        assert_eq!(pi.count("repay"), 0);
        assert!(pi.0.lock().iter().all(|c| c.1 == VPRO_WIDGET));

        // One store runs per slice: the next one moves up and is repaid.
        let mut b = SGIHeadWorkBudget { deadline: i64::MAX, words: 1 };
        odyssey_work_step(P::from_ref(&*s).opaque(), &mut b);
        assert_eq!(pi.count("repay"), 1);
        assert_eq!(pi.settled(), ODSY_CFIFO_DEPTH as i64 + 1);

        let mut b = SGIHeadWorkBudget { deadline: i64::MAX, words: u32::MAX };
        odyssey_work_step(P::from_ref(&*s).opaque(), &mut b);
        assert!(s.work_queue.is_empty());
        assert_eq!(pi.settled(), stores as i64);

        // A store that does not wait is credited inside the store.
        s.work.test_words = 0;
        odyssey_mmio_write(&mut s, ODSY_CMD_FIFO, 0, 8);
        assert!(!odyssey_work_pending(&mut s));
        assert_eq!(pi.settled(), stores as i64 + 1);
        assert_eq!(pi.count("owe"), (stores - ODSY_CFIFO_DEPTH) as i64);
    }

    /// A reset abandons the waiting stores and cancels each owed credit
    /// (one owe of -1 per owed store); stores not owed cancel nothing.
    #[test]
    fn reset_cancels_owed_credits() {
        let (mut s, pi) = credit_board();

        for _ in 0..ODSY_CFIFO_DEPTH + 8 {
            odyssey_mmio_write(&mut s, ODSY_CMD_FIFO, 0, 8);
        }
        assert_eq!(pi.count("owe"), 8);
        odyssey_work_reset(&mut s);
        assert!(s.work_queue.is_empty());
        assert_eq!(s.work_in_fifo, 0);
        assert_eq!(pi.count("owe"), 0);
        assert_eq!(pi.0.lock().iter().filter(|c| c.0 == "owe" && c.2 == -1).count(), 8);
        assert_eq!(pi.count("repay"), 0);
    }

    /// The processor interface's mark makes a store counted, once, and a
    /// counted store with no link logs and returns nothing.
    #[test]
    fn processor_mark_counts_one_store() {
        let (mut s, pi) = credit_board();

        s.test_counted_stores = 0;
        s.work.test_words = 0;
        s.work.started = false;
        odyssey_mmio_write(&mut s, ODSY_CMD_FIFO, 0, 8);
        assert_eq!(pi.settled(), 0);

        let previous = sgi_pi_gfx_mark_store(true);
        odyssey_mmio_write(&mut s, ODSY_CMD_FIFO, 0, 8);
        sgi_pi_gfx_unmark_store(previous);
        assert_eq!(pi.0.lock().as_slice(), &[("return", VPRO_WIDGET, 1)]);

        // One-shot: the mark was taken by the first store.
        odyssey_mmio_write(&mut s, ODSY_CMD_FIFO, 0, 8);
        assert_eq!(pi.settled(), 1);

        // The Octane: no link, so the credit is dropped with a log line.
        s.pi = None;
        sgi_pi_gfx_mark_store(true);
        odyssey_mmio_write(&mut s, ODSY_CMD_FIFO, 0, 8);
        assert_eq!(pi.settled(), 1);
        assert!(!sgi_pi_gfx_take_mark());
    }

    /// The register block's pure state (pi-gfx.c).
    #[test]
    fn pi_gfx_block_counts_and_starves() {
        let mut g = SGIPIGfx::default();

        sgi_pi_gfx_write(&mut g, SGI_PI_GFX_PAGE, SGI_PI_GFX_PAGE_ENABLE | 0x2_0000);
        sgi_pi_gfx_write(&mut g, SGI_PI_GFX_CREDIT, 3);
        sgi_pi_gfx_write(&mut g, SGI_PI_GFX_BIAS, 2);
        assert!(sgi_pi_gfx_covers(&g, 0, 0x2_1000));
        assert!(!sgi_pi_gfx_covers(&g, 0, 0x4_0000));
        assert!(sgi_pi_gfx_count_store(&mut g, 0, 0x2_0000, "t"));
        assert!(!sgi_pi_gfx_take_starved(&mut g, 0, "t"));
        assert!(sgi_pi_gfx_count_store(&mut g, 0, 0x2_0000, "t"));
        assert!(sgi_pi_gfx_take_starved(&mut g, 0, "t"));
        assert!(!sgi_pi_gfx_take_starved(&mut g, 0, "t"));
        sgi_pi_gfx_add_credits(&mut g, 0, 5, "t");
        assert_eq!(sgi_pi_gfx_read(&g, SGI_PI_GFX_CREDIT), 6);
        assert!(!g.starved[0]);
    }
}
