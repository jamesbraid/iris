//! The board's host interface, under a lock of its own.
//!
//! QEMU's model runs every access under the big lock, and lets go of it
//! only inside a scalar draw on the board's thread. While that draw runs a
//! processor may do two things to the board: store into the command FIFO
//! (the store queues behind the work) and touch the registers that do not
//! wait for the work (`odyssey_access_waits`): identity, control, the
//! interrupt bank, the routing, the swap bank and the FIFO status. Any
//! other access waits for the draw and then for all the queued work.
//!
//! In Rust a draw that lets go of the state's lock while it still holds
//! references into the state, and a processor that takes the lock and
//! makes its own, alias the same memory. So the state is split along the
//! line the C draws: the host interface (`OdysseyFront`) sits behind its
//! own mutex, the rest of the board (`SGIOdysseyState`) behind the board
//! lock. The board's thread holds the board lock for a whole slice and
//! never lets go of it there; it takes the front lock in short sections
//! (one queued store out, the level change, an interrupt). A processor's
//! front access and its queued stores take only the front lock, so they
//! never wait for a draw; every other access takes the board lock, which
//! waits out the slice exactly where the C's waits out the unlocked draw.
//!
//! Lock order: board, then front. Nothing takes the board lock while it
//! holds the front lock.
use super::*;

use parking_lot::Mutex;
use std::sync::Arc;

pub struct OdysseyFront {
    /// The crosstalk widget number.
    pub slot: u32,
    /// Where interrupt packets go (the iris glue's HEART).
    pub irq: Option<Arc<dyn crate::irq::InterruptController>>,
    /// The master node's processor interface, for graphics credits.
    pub pi: Option<Arc<dyn SGIPIGfxLink>>,
    /// The widget number of HEART, the packets' destination.
    pub heart_widget: u32,
    /// ODSY_IDENTITY, composed from the revision at reset.
    pub identity: u32,

    pub widget_control: u32,
    pub xt_req_timeout: u32,
    pub int_dest_upper: u32,
    pub int_dest_lower: u32,
    pub error_command: u32,
    pub intr_flags: u32,
    pub intr_enable: u32,
    /// The flag source's transition state, for the packet edge.
    pub interrupt_asserted: bool,
    pub sw_token: u32,
    pub route_dest_upper: u32,
    pub route_dest_lower: u32,
    pub route_vectors: u32,
    pub swap_arm: u32,
    pub swap_pending: u32,
    pub swap_control: u32,
    pub swap_control_companion: u32,
    pub swap_set: u32,
    pub swap_clear: u32,
    pub displayed_buffer: u32,
    /// A processor wrote ODSY_DISPLAYED_BUFFER: the scanout, which lives
    /// under the board lock, repaints at its next update.
    pub displayed_written: bool,

    // Command-FIFO stores waiting behind the board's work.
    pub work: SGIHeadWork,
    pub work_queue: Vec<OdysseyQueuedStore>,
    /// The oldest waiting stores, this many, are in the FIFO and credited.
    pub work_in_fifo: u32,
    /// Until when (virtual clock) FIFO stores queue after a long one.
    pub work_queued_until: i64,
    /// qtest: stores from outside a processor count as counted.
    pub test_counted_stores: u8,

    pub test_last_interrupt_addr: u64,
    pub test_last_interrupt_value: u64,
    /// The front accesses' own trace numbering; the board's accesses keep
    /// the state's.
    pub trace_sequence: u64,
}

impl Default for OdysseyFront {
    fn default() -> Self {
        OdysseyFront {
            slot: 0,
            irq: None,
            pi: None,
            heart_widget: 8,
            identity: 0,
            widget_control: 0,
            xt_req_timeout: 0,
            int_dest_upper: 0,
            int_dest_lower: 0,
            error_command: 0,
            intr_flags: 0,
            intr_enable: 0,
            interrupt_asserted: false,
            sw_token: 0,
            route_dest_upper: 0,
            route_dest_lower: 0,
            route_vectors: 0,
            swap_arm: 0,
            swap_pending: 0,
            swap_control: 0,
            swap_control_companion: 0,
            swap_set: 0,
            swap_clear: 0,
            displayed_buffer: 0,
            displayed_written: false,
            work: Default::default(),
            work_queue: Vec::new(),
            work_in_fifo: 0,
            work_queued_until: 0,
            test_counted_stores: 0,
            test_last_interrupt_addr: 0,
            test_last_interrupt_value: 0,
            trace_sequence: 0,
        }
    }
}

/// The front lock as the head work's lock: the work's scheduling fields
/// (`OdysseyFront::work`) live under it.
pub struct OdysseyFrontLock(pub Arc<Mutex<OdysseyFront>>);

impl SGIHeadWorkBql for OdysseyFrontLock {
    fn lock(&self) {
        std::mem::forget(self.0.lock());
    }
    unsafe fn unlock(&self) {
        // SAFETY: the caller holds the lock (trait contract).
        unsafe { self.0.force_unlock() };
    }
}

/// The board lock as the state the head work's step runs under.
pub struct OdysseyBoardLock(pub Arc<Mutex<Box<SGIOdysseyState>>>);

impl SGIHeadWorkBql for OdysseyBoardLock {
    fn lock(&self) {
        std::mem::forget(self.0.lock());
    }
    unsafe fn unlock(&self) {
        // SAFETY: the caller holds the lock (trait contract).
        unsafe { self.0.force_unlock() };
    }
}

/// The two locks a processor's access goes through.
#[derive(Clone)]
pub struct OdysseyHandles {
    pub board: Arc<Mutex<Box<SGIOdysseyState>>>,
    pub front: Arc<Mutex<OdysseyFront>>,
}

impl OdysseyHandles {
    pub fn new(s: Box<SGIOdysseyState>) -> Self {
        let front = s.front.clone();
        OdysseyHandles { board: Arc::new(Mutex::new(s)), front }
    }
}

// Interrupts. Every source is delivered from the front, which holds the
// routing and the flag bank.

/// An interrupt packet into the widget-addressed fabric.
pub fn odyssey_send_packet(f: &mut OdysseyFront, where_: u64, vector: u8, set: bool) {
    // Retain the packet for host-only qtest inspection.
    f.test_last_interrupt_addr = where_;
    f.test_last_interrupt_value = vector as u64 | if set { SGI_XIO_INTERRUPT_SET } else { 0 };
    sgi_xio_send_interrupt(f, where_, vector, set);
}

/// The fabric address of a widget/address pair: the widget in bits 51:48.
pub fn odyssey_xio_compose(widget: u32, address: u64, fabric: &mut u64) -> bool {
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
pub fn odyssey_deliver_routed(f: &mut OdysseyFront, shift: u32, what: &str, set: bool) {
    let widget = (f.route_dest_upper >> ODSY_ROUTE_DEST_WIDGET_SHIFT) & ((1u32 << ODSY_INT_DEST_WIDGET_BITS) - 1);
    let addr = (((f.route_dest_upper & 0xffff) as u64) << 32) | f.route_dest_lower as u64;
    let vector = f.route_vectors.checked_shr(shift).unwrap_or(0) as u8;
    let mut where_ = 0u64;

    if f.route_dest_upper == 0 && f.route_dest_lower == 0 {
        crate::qlog!(
            "sgi-odyssey: {} interrupt {} with no destination programmed; no packet sent\n",
            what,
            if set { "raised" } else { "lowered" }
        );
        return;
    }
    if !odyssey_xio_compose(widget, addr, &mut where_) {
        return;
    }
    odyssey_send_packet(f, where_, vector, set);
}

/// Whether the routed destination is programmed.
pub fn odyssey_routed(f: &OdysseyFront) -> bool {
    f.route_dest_upper != 0 || f.route_dest_lower != 0
}

/// The softsync source ODSY_REG_HOST_WRITE (0x0a5a) raises.
pub fn odyssey_deliver_softsync(f: &mut OdysseyFront) {
    odyssey_deliver_routed(f, ODSY_ROUTE_VECTOR_SOFTSYNC_SHIFT, "softsync", true);
}

pub fn odyssey_update_irq(f: &mut OdysseyFront) {
    let level = (f.intr_flags & f.intr_enable) != 0;

    if level != f.interrupt_asserted {
        f.interrupt_asserted = level;
        odyssey_deliver_routed(f, ODSY_ROUTE_VECTOR_FLAG_SHIFT, "flag", level);
    }
}

/// The command FIFO's level: the stores waiting for the board's work.
pub fn odyssey_fifo_level(f: &OdysseyFront) -> u32 {
    f.work_queue.len() as u32
}

/// The FIFO's level went from 'before' to its current value. Falling below
/// the low-water mark latches the low-water flag whatever the enable mask
/// says; it reaches the driver only while the source is enabled.
pub fn odyssey_fifo_level_changed(f: &mut OdysseyFront, before: u32) {
    if before >= ODSY_CFIFO_LOW_WATER && odyssey_fifo_level(f) < ODSY_CFIFO_LOW_WATER {
        f.intr_flags |= ODSY_INTR_FIFO_LOW_WATER;
        odyssey_update_irq(f);
    }
}

// The work queue.

pub fn odyssey_front_pending(f: &OdysseyFront) -> bool {
    !f.work_queue.is_empty()
}

/// The head work's pending hook: its opaque is the front, and the head's
/// thread calls it holding the front lock.
pub fn odyssey_front_pending_cb(opaque: Opaque) -> bool {
    odyssey_front_pending(from_opaque::<OdysseyFront>(opaque))
}

/// A counted store's credit, to the processor interface the node names.
pub fn odyssey_return_credit(f: &mut OdysseyFront) {
    if let Some(pi) = &f.pi {
        sgi_pi_gfx_return_credits(pi.as_ref(), f.slot, 1);
    } else {
        crate::qlog!("sgi-odyssey: counted graphics store has no processor interface for its return\n");
    }
}

/// Waiting stores that have moved up into the FIFO get their credits. One
/// that waited outside it repays a credit the node holds as owed
/// (sgi_pi_gfx_owe_credits()).
pub fn odyssey_work_admit(f: &mut OdysseyFront) {
    while f.work_in_fifo < f.work_queue.len() as u32 && f.work_in_fifo < crate::dev::odyssey::device::ODSY_CFIFO_DEPTH {
        let q = f.work_queue[f.work_in_fifo as usize];

        if q.owed {
            if let Some(pi) = &f.pi {
                sgi_pi_gfx_repay_credits(pi.as_ref(), f.slot, 1);
            }
            f.work_queue[f.work_in_fifo as usize].owed = false;
        } else if q.counted {
            odyssey_return_credit(f);
        }
        f.work_in_fifo += 1;
    }
}

/// Queue a store and have the board's thread run it. With no thread
/// started the caller, which holds the board lock, drains it.
pub fn odyssey_work_append(f: &mut OdysseyFront, addr: u64, value: u64, size: u32, counted: bool) {
    let q = OdysseyQueuedStore {
        value,
        addr: addr as u32,
        size: size as u8,
        counted,
        owed: false,
        queued_at: qemu_clock_virtual_ns(),
    };

    f.work_queue.push(q);
    odyssey_work_admit(f);
    if counted && f.work_in_fifo < f.work_queue.len() as u32 && f.pi.is_some() {
        let last = f.work_queue.len() - 1;
        f.work_queue[last].owed = true;
        sgi_pi_gfx_owe_credits(f.pi.as_ref().unwrap().as_ref(), f.slot, 1);
    }
    sgi_head_work_schedule(&mut f.work);
}

/// The oldest waiting store, out of the queue and the FIFO.
pub fn odyssey_work_take(f: &mut OdysseyFront) -> Option<OdysseyQueuedStore> {
    if f.work_queue.is_empty() {
        return None;
    }
    let q = f.work_queue.remove(0);
    // The store leaves the FIFO, and the next waiting one moves up.
    f.work_in_fifo = f.work_in_fifo.wrapping_sub(1);
    odyssey_work_admit(f);
    Some(q)
}

/// Whether a store that joins the command stream waits in the queue: behind
/// any store that already waits, while the board's thread runs a slice, and
/// within ODSY_WORK_HOLD_NS of a FIFO store that ran long.
pub fn odyssey_store_queues(f: &OdysseyFront, addr: u64, size: u32) -> bool {
    if !odyssey_command_store(addr, size) {
        return false;
    }
    odyssey_front_pending(f) || f.work.slice || f.work.test_words != 0 || qemu_clock_virtual_ns() < f.work_queued_until
}

// The front registers.

/// Whether `addr` is a front register for this access: one of the
/// registers that do not wait for the board's work. The other accesses
/// that do not wait (the back end's read window, unmodeled offsets) report
/// through state the board lock covers, so they take it.
pub fn odyssey_front_register(addr: u64, size: u32, write: bool) -> bool {
    if odyssey_fifo_port(addr) || odyssey_reg(addr, size) < 0 || odyssey_access_waits(addr, size, write) {
        return false;
    }
    if !write {
        // Reads of these two fall to the unmodeled-access report.
        return !matches!(odyssey_reg(addr, size) as u64, ODSY_INTR_CLEAR | ODSY_INTR_ENABLE_CLEAR);
    }
    true
}

pub fn odyssey_front_read(f: &mut OdysseyFront, addr: u64, size: u32) -> u64 {
    let mut value: u64 = 0;
    let sequence = f.trace_sequence;
    f.trace_sequence = f.trace_sequence.wrapping_add(1);

    match odyssey_reg(addr, size) as u64 {
        ODSY_IDENTITY => value = f.identity as u64,
        ODSY_CONTROL => value = f.widget_control as u64,
        ODSY_XT_REQ_TIMEOUT => value = f.xt_req_timeout as u64,
        ODSY_INT_DEST_UPPER => value = f.int_dest_upper as u64,
        ODSY_INT_DEST_LOWER => value = f.int_dest_lower as u64,
        ODSY_ERROR_COMMAND => value = f.error_command as u64,
        ODSY_STATUS | ODSY_ERROR_ADDR_UPPER | ODSY_ERROR_ADDR_LOWER => {
            // No crosstalk error is ever posted, so the error class is clear.
        }
        ODSY_INTR_FLAGS => value = f.intr_flags as u64,
        ODSY_INTR_ENABLE_SET => value = f.intr_enable as u64,
        ODSY_STATUS0 => {
            // The waiting stores are its level (odyssey_fifo_level()).
            let level = odyssey_fifo_level(f);
            value = (ODSY_STATUS0_FIFO_ENABLED
                | if level != 0 { 0 } else { ODSY_STATUS0_FIFO_EMPTY }
                | if level < ODSY_CFIFO_LOW_WATER { ODSY_STATUS0_FIFO_LOW_WATER } else { 0 }) as u64;
        }
        ODSY_SW_TOKEN => value = f.sw_token as u64,
        ODSY_SWAP_ARM => value = f.swap_arm as u64,
        ODSY_SWAP_PENDING => value = f.swap_pending as u64,
        ODSY_SWAP_CONTROL => value = f.swap_control as u64,
        ODSY_SWAP_CONTROL_COMPANION => value = f.swap_control_companion as u64,
        // The buffer the last latched swap put on screen (odyssey_retrace()).
        ODSY_DISPLAYED_BUFFER => value = f.displayed_buffer as u64,
        ODSY_ROUTE_DEST_UPPER | ODSY_ROUTE_DEST_LOWER | ODSY_ROUTE_VECTORS => {
            crate::qlog!("sgi-odyssey: read of the write-only register at {:#x}\n", addr);
        }
        _ => unreachable!("odyssey_front_register() admits only these"),
    }
    crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_mmio: front seq {} offset {:#x} size {} value {:#x} write=0", sequence, addr, size, value);
    value
}

pub fn odyssey_front_write(f: &mut OdysseyFront, addr: u64, value: u64, size: u32) {
    crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_odyssey_mmio: front seq {} offset {:#x} size {} value {:#x} write=1", f.trace_sequence, addr, size, value);
    f.trace_sequence = f.trace_sequence.wrapping_add(1);

    match odyssey_reg(addr, size) as u64 {
        ODSY_CONTROL => f.widget_control = value as u32,
        ODSY_XT_REQ_TIMEOUT => f.xt_req_timeout = value as u32,
        ODSY_INT_DEST_UPPER => f.int_dest_upper = value as u32,
        ODSY_INT_DEST_LOWER => f.int_dest_lower = value as u32,
        // Written to acknowledge; the data itself is discarded.
        ODSY_ERROR_COMMAND => f.error_command = 0,
        ODSY_INTR_CLEAR => {
            f.intr_flags &= !(value as u32);
            odyssey_update_irq(f);
        }
        ODSY_INTR_ENABLE_SET => {
            f.intr_enable |= value as u32;
            odyssey_update_irq(f);
        }
        ODSY_INTR_ENABLE_CLEAR => {
            f.intr_enable &= !(value as u32);
            odyssey_update_irq(f);
        }
        ODSY_INTR_FLAGS => {
            // The flags take the value written: an inferred model choice.
            // A driver's flow control writes the low-water bit here before
            // enabling that source, and its DMA diagnostic clears one flag
            // by writing back the flags it read without it; only a
            // replacing write serves the second. The board's paired
            // registers (set enables, clear enables, clear flags) make a
            // set-only write the competing reading.
            f.intr_flags = value as u32;
            odyssey_update_irq(f);
        }
        ODSY_SW_TOKEN => f.sw_token = value as u32,
        ODSY_SWAP_ARM => {
            // Queue flips for the next retrace. Writes accumulate: the
            // driver can issue several windows or swap groups between two
            // retraces, and its retrace handler re-writes one window's bit
            // when a flip has not shown after two retraces. Whether the
            // board ORs or replaces is unknown; ORing loses no flip.
            f.swap_arm |= value as u32;
        }
        ODSY_SWAP_PENDING => f.swap_pending = value as u32,
        ODSY_SWAP_CONTROL => {
            // The windows that must show buffer 1 from the next retrace on.
            // A later write to either register for the same window replaces
            // this one before the retrace latches it.
            f.swap_control = value as u32;
            f.swap_set |= value as u32;
            f.swap_clear &= !(value as u32);
        }
        ODSY_SWAP_CONTROL_COMPANION => {
            f.swap_control_companion = value as u32;
            f.swap_clear |= value as u32;
            f.swap_set &= !(value as u32);
        }
        ODSY_DISPLAYED_BUFFER => {
            // Board-produced status; a guest write is not seen in the
            // capture, but store it rather than drop it.
            f.displayed_buffer = value as u32;
            f.displayed_written = true;
        }
        ODSY_ROUTE_DEST_UPPER => f.route_dest_upper = value as u32,
        ODSY_ROUTE_DEST_LOWER => f.route_dest_lower = value as u32,
        ODSY_ROUTE_VECTORS => f.route_vectors = value as u32,
        ODSY_IDENTITY | ODSY_STATUS | ODSY_ERROR_ADDR_UPPER | ODSY_ERROR_ADDR_LOWER => {
            crate::qlog!("sgi-odyssey: write of {:#x} to the read-only register at {:#x}\n", value, addr);
        }
        _ => unreachable!("odyssey_front_register() admits only these"),
    }
}

/// A processor's read through the front alone: Some when it is a front
/// register, None when the access needs the board lock.
pub fn odyssey_front_mmio_read(f: &Mutex<OdysseyFront>, addr: u64, size: u32) -> Option<u64> {
    if !odyssey_front_register(addr, size, false) {
        return None;
    }
    Some(odyssey_front_read(&mut f.lock(), addr, size))
}

/// A processor's store through the front alone: true when it went to a
/// front register or joined the queue for the board's thread, false when
/// it needs the board lock. `counted`: the processor interface counted it.
pub fn odyssey_front_mmio_write(f: &Mutex<OdysseyFront>, addr: u64, value: u64, size: u32, counted: bool) -> bool {
    let mut f = f.lock();
    if odyssey_front_register(addr, size, true) {
        odyssey_front_write(&mut f, addr, value, size);
        if counted {
            odyssey_return_credit(&mut f);
        }
        return true;
    }
    // A queued store needs the board's thread to run it, and the queue to
    // have room; otherwise the store drains the queue under the board lock.
    if !f.work.started || !odyssey_store_queues(&f, addr, size) || f.work_queue.len() >= crate::dev::odyssey::device::ODSY_WORK_QUEUE_MAX {
        return false;
    }
    odyssey_work_append(&mut f, addr, value, size, counted);
    true
}
