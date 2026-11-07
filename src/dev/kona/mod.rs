//! The InfiniteReality (KONA) graphics pipe's front end: the host interface.
//!
//! The pipe is a crosstalk-style widget. The processor stores commands to a
//! command page; a Host Interface Processor on the board takes them, answers
//! the ones that are about the board itself and passes the ones that draw to
//! the Geometry Distributor, Geometry Engines, Raster Managers and Display
//! Generator behind it. This module is the first part: the widget header and
//! identity registers, the one-wire identity record, the board's flash
//! EEPROM and the operation the graphics install tool programs it with,
//! reset, the command FIFO, command framing, DMA and display-list storage.
//! Drawing is behind the typed job interface in `job`: a later back end
//! implements `Backend`.
//!
//! - `attach`: what a machine gives the board: an interrupt packet sender, an
//!   inbound DMA handle and the FIFO-drained signal. The machine calls
//!   `Kona::bus_read` / `bus_write` with offsets inside the 16 MiB aperture.
//! - `front`: the registers a processor reaches without waiting for the
//!   command stream, behind one short lock.
//! - `regs`: which accesses join the stream, which are served at once and
//!   which wait; the registers the Host Interface Processor owns.
//! - `pipe`: the Host Interface Processor's state and the command stream it
//!   runs (framers, pages, contexts, reasons, swaps).
//! - `frame`, `tables`: command-word framing and the selector tables.
//! - `dma`: DMA packets, display-list memory and list invocation.
//! - `flash`: the flash EEPROM and its programming operation.
//! - `ident`: widget id, one-wire record, configuration block.
//! - `job`: the back end's interface.
//! - `back`: the back end that draws and displays.
//! - `handle`: what a back end's display thread may call on the front end.
//!
//! # Threads
//!
//! One thread, `kona-hip`, stands for the Host Interface Processor. The
//! processor stores into a command FIFO and goes on, as it does on the
//! board, and a display list the processor invokes can run for as long as
//! its words take to decode. A store to the command page (and the few other
//! stream registers) goes into the shared graphics ring (`GFifo`) with no
//! lock beyond the ring's; a full ring answers the bus busy, never a spin on
//! the processor's thread. The thread takes a run of stores at a time and
//! runs them in order. A register the stream changes or the stream reads
//! (`regs::classify`) is answered busy until the ring is empty and the thread
//! idle; a register the driver polls while the pipe works is served from the
//! register file at once.
//!
//! Without the thread running (before `start`, in unit tests) a stream store
//! runs on the caller's thread, in order.
//!
//! # Locks
//!
//! `Host::front` guards the register file and is held for a few instructions,
//! never across a call out of the board. `Shared::pipe` holds the Host
//! Interface Processor's state and the back end in a slot. Whoever works on it
//! takes it out (the mutex is held for an instant) and puts it back after: the
//! thread for a batch, a processor only when the ring is empty (a taken slot
//! is busy). So no board lock is held while the machine's DMA, interrupt and
//! FIFO handles are called, and none is held across the back end.
//!
//! Ported from hw/misc/sgi-kona.c, sgi-kona-ports.c, sgi-kona-dma.c and
//! sgi-command-port.c in jamesbraid/qemu-sgi-origami.

// The crate allows dead code; this board's code does not hide any.
#![warn(dead_code, unused_imports)]

mod attach;
mod back;
mod consts;
mod dma;
mod flash;
mod frame;
mod front;
mod handle;
mod ident;
mod job;
mod pipe;
mod regs;
mod tables;
#[cfg(test)]
mod tests;

pub use attach::{FifoLow, HostDma, InterruptPacket, KonaAttach, PacketSender};
pub use back::pack::{pack_bitmap_row, unpack_bitmap_row, BusFormat, HostFormat, Rgba16};
pub use back::store::layout::{BufferConfig, Layout, Refusal};
pub use back::raster::{FillRule, Surface};
pub use back::store::{PixelStore, WordArray, MAX_WORDS};
pub use back::token::textport::{Textport, COLOUR_WORD};
pub use back::{BackendConfig, BoardSet, KonaBackend};
pub use consts::APERTURE;
pub use flash::FLASH_SIZE;
pub use handle::DisplayHandle;
pub use ident::KonaConfig;
pub use job::{Backend, BackPort, ContextEvent, Grammar, Job, NullBackend, Reply};
pub use pipe::LoadSection;

use crate::gfifo::GFifo;
use crate::traits::{Device, Resettable, BUS_BUSY, BUS_ERR, BUS_OK};
use front::{Front, Host};
use parking_lot::{Condvar, Mutex};
use pipe::Core;
use regs::{classify, Access};
use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering::{Relaxed, SeqCst}};
use std::sync::Arc;
use std::thread::JoinHandle;

/// The host ring under the command stream: 64K stores, as the other
/// graphics boards' rings are.
type Ring = GFifo<{ 1 << 16 }>;

/// The most stores the thread looks at before running them.
const RUN_MAX: usize = 32;

/// The Host Interface Processor's state with the back end it hands jobs to.
struct Pipe {
    core: Core,
    backend: Box<dyn Backend>,
}

struct Shared {
    /// Its own `Arc` so a back end's display thread can hold it weakly: the
    /// register file does not own the back end, the pipe slot does.
    host: Arc<Host>,
    /// The Host Interface Processor's state, or None while someone has it
    /// out (`Shared::pipe`). The mutex is held for an instant to take it or
    /// put it back, never across work.
    pipe: Mutex<Option<Box<Pipe>>>,
    /// Signalled when the state is put back: what a monitor or reset waits
    /// on.
    freed: Condvar,
    ring: Box<Ring>,
    /// The thread died (its back end panicked): bus accesses fail.
    dead: AtomicBool,
    /// The thread runs, so stream stores queue.
    running: AtomicBool,
    exit: AtomicBool,
    /// The thread is parked in `wait_for_work`: a producer wakes it only
    /// then, so a store reads a line nobody writes instead of locking.
    asleep: AtomicBool,
    sleeper: Mutex<Option<std::thread::Thread>>,
}

/// The Host Interface Processor's state taken out of its slot, put back when
/// dropped. Whoever holds it holds no lock: the work, including every call to
/// the machine's DMA, interrupt and FIFO handles, runs outside the mutex.
struct PipeGuard<'a> {
    shared: &'a Shared,
    pipe: Option<Box<Pipe>>,
}

impl std::ops::Deref for PipeGuard<'_> {
    type Target = Pipe;
    fn deref(&self) -> &Pipe {
        self.pipe.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for PipeGuard<'_> {
    fn deref_mut(&mut self) -> &mut Pipe {
        self.pipe.as_mut().unwrap()
    }
}

impl Drop for PipeGuard<'_> {
    fn drop(&mut self) {
        *self.shared.pipe.lock() = self.pipe.take();
        self.shared.freed.notify_all();
    }
}

impl Shared {
    /// The state, if nobody else has it out.
    fn try_pipe(&self) -> Option<PipeGuard<'_>> {
        let pipe = self.pipe.lock().take()?;
        Some(PipeGuard { shared: self, pipe: Some(pipe) })
    }

    /// The state, spinning while someone else has it out. For the
    /// Host Interface Processor's thread only: what it waits out is a
    /// processor's access to the state, a few instructions, and a sleep
    /// would cost more than the wait.
    fn pipe_spin(&self) -> PipeGuard<'_> {
        loop {
            if let Some(g) = self.try_pipe() {
                return g;
            }
            std::thread::yield_now();
        }
    }

    /// The state, sleeping until whoever has it out puts it back. For a
    /// monitor or reset, which can wait behind a long batch of stores and
    /// must not burn a core while it does.
    fn pipe_wait(&self) -> PipeGuard<'_> {
        let mut slot = self.pipe.lock();
        loop {
            if let Some(pipe) = slot.take() {
                return PipeGuard { shared: self, pipe: Some(pipe) };
            }
            self.freed.wait(&mut slot);
        }
    }

    fn wake(&self) {
        // Store the entry, then look at `asleep`; the thread stores `asleep`,
        // then looks at the ring. Both sides need the fence for either to
        // see the other (a store-then-load handshake).
        std::sync::atomic::fence(SeqCst);
        if self.asleep.load(SeqCst) {
            if let Some(t) = self.sleeper.lock().as_ref() {
                t.unpark();
            }
        }
    }

    /// The thread: sleep until a store waits or `stop` asks. False when the
    /// thread is to finish. The timeout is only a backstop for a wake the
    /// fences should never lose.
    fn wait_for_work(&self) -> bool {
        *self.sleeper.lock() = Some(std::thread::current());
        while self.ring.is_empty() && !self.exit.load(SeqCst) {
            self.asleep.store(true, Relaxed);
            std::sync::atomic::fence(SeqCst);
            if self.ring.is_empty() && !self.exit.load(SeqCst) {
                std::thread::park_timeout(std::time::Duration::from_millis(50));
            }
            self.asleep.store(false, Relaxed);
        }
        !self.exit.load(SeqCst)
    }

    /// Run the stores waiting, a run at a time, in order, until the ring is
    /// empty.
    fn run_stores(&self) -> bool {
        let mut run = [(0u32, 0u64); RUN_MAX];
        let mut worked = false;
        loop {
            let n = self.ring.peek_run_at(0, &mut run);
            if n == 0 {
                return worked;
            }
            worked = true;
            {
                let mut guard = self.pipe_spin();
                let pipe = &mut *guard;
                for &(off, val) in &run[..n] {
                    pipe.core.stream_store(&self.host, &mut *pipe.backend, off, val as u32);
                }
                pipe.core.reason_drain(&self.host);
                self.publish(&pipe.core);
                // Taken out of the ring only now that they have run: a
                // register that waits for the ring to be empty then finds the
                // state the stores left.
                self.ring.consume_n_at(0, n);
                self.ring.flush_head();
            }
        }
    }

    /// What the polled registers read of the stream.
    fn publish(&self, core: &Core) {
        self.host.held_words.store(core.held_words(), SeqCst);
        self.host.reasons_waiting.store(core.reasons_waiting(), SeqCst);
    }

    /// The ring is empty and the thread has finished its work: the FIFO-low
    /// cause, if the driver enabled it, and the machine's signal.
    fn drained(&self) {
        self.host.with_front(|f| ((), f.lowater()));
        self.host.attach.fifo_low.drained();
    }

    fn thread_main(self: &Arc<Self>) {
        let _dead = DeadOnPanic(self);
        while self.wait_for_work() {
            if self.run_stores() {
                self.drained();
            }
        }
    }

    /// Run whatever is left in the ring on the calling thread.
    fn drain_inline(&self) {
        if self.run_stores() {
            self.drained();
        }
    }
}

/// Marks the board dead if the thread unwinds: its back end panicked. The
/// bus then fails visibly instead of answering busy forever to a thread that
/// is gone.
struct DeadOnPanic<'a>(&'a Shared);

impl Drop for DeadOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.dead.store(true, SeqCst);
            self.0.running.store(false, SeqCst);
            crate::dlog!(crate::devlog::LogModule::Kona, "kona: the Host Interface Processor thread panicked; the board is dead");
        }
    }
}

/// The InfiniteReality pipe's host interface.
pub struct Kona {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// How a pipe is built.
pub struct KonaBuilder {
    cfg: KonaConfig,
    attach: KonaAttach,
    backend: Box<dyn Backend>,
    flash: Option<File>,
    probe: u32,
}

impl KonaBuilder {
    pub fn new(cfg: KonaConfig, attach: KonaAttach) -> Self {
        KonaBuilder { cfg, attach, backend: Box::new(NullBackend::default()), flash: None, probe: 0 }
    }

    /// The back end that draws.
    pub fn backend(mut self, backend: Box<dyn Backend>) -> Self {
        self.backend = backend;
        self
    }

    /// Hold the flash in `file`, which must be exactly `FLASH_SIZE` bytes:
    /// every byte the pipe programs is written through, so the contents
    /// outlast the process as they outlast power-off on a pipe.
    pub fn flash_file(mut self, file: File) -> Self {
        self.flash = Some(file);
        self
    }

    /// The value the probe register reads (default 0). Its bits 5:3 size the
    /// map RAM: `N << 15` entries, zero meaning 8.
    pub fn probe_value(mut self, v: u32) -> Self {
        self.probe = v;
        self
    }

    pub fn build(self) -> Result<Arc<Kona>, String> {
        let flash = match self.flash {
            Some(f) => flash::Flash::backed(f)?,
            None => flash::Flash::fresh(),
        };
        let mut front = Front::new(&self.cfg);
        front.probe_value = self.probe;
        let host = Arc::new(Host {
            front: Mutex::new(front),
            attach: self.attach,
            held_words: Default::default(),
            reasons_waiting: Default::default(),
            list_running: Default::default(),
            activity: Default::default(),
        });
        // SAFETY: a zeroed GFifo is an empty one (`GFifo::new`); built in
        // place because the ring is too big for the stack.
        let ring: Box<Ring> = unsafe { Box::new_zeroed().assume_init() };
        Ok(Arc::new(Kona {
            shared: Arc::new(Shared {
                host,
                pipe: Mutex::new(Some(Box::new(Pipe { core: Core::new(self.cfg, flash), backend: self.backend }))),
                freed: Condvar::new(),
                ring,
                dead: AtomicBool::new(false),
                running: AtomicBool::new(false),
                exit: AtomicBool::new(false),
                asleep: AtomicBool::new(false),
                sleeper: Mutex::new(None),
            }),
            thread: Mutex::new(None),
        }))
    }
}

impl Kona {
    /// An InfiniteReality2 with the default configuration and no back end.
    pub fn new(attach: KonaAttach) -> Arc<Kona> {
        KonaBuilder::new(KonaConfig::default(), attach).build().expect("the default configuration is valid")
    }

    /// A read of `size` bytes at aperture offset `off`: the bus status and
    /// the data. The registers are word-sized: an 8-byte read is two word
    /// reads, high word at the lower address.
    pub fn bus_read(&self, off: u64, size: u32) -> (u32, u64) {
        if self.shared.dead.load(Relaxed) {
            return (BUS_ERR, 0);
        }
        if off >= APERTURE {
            return (BUS_ERR, 0);
        }
        let off = off as u32;
        match size {
            4 => match self.read_word(off) {
                Some(v) => (BUS_OK, v as u64),
                None => (BUS_BUSY, 0),
            },
            8 => {
                crate::dlog!(crate::devlog::LogModule::Kona, "kona: 8-byte load at offset {off:#x} split into two 4-byte accesses");
                match (self.read_word(off), self.read_word(off.wrapping_add(4))) {
                    (Some(hi), Some(lo)) => (BUS_OK, (hi as u64) << 32 | lo as u64),
                    _ => (BUS_BUSY, 0),
                }
            }
            _ => (BUS_ERR, 0),
        }
    }

    /// A store of `size` bytes at aperture offset `off`. BUS_BUSY: the caller
    /// retries the same store.
    ///
    /// An 8-byte store to the command port is two words, high first, each as
    /// a 4-byte store at the same offset (the X driver module's bulk data
    /// helpers store aligned pairs of data words that way, and the GL client's
    /// vector senders store a selector and its first argument as one
    /// doubleword at any stream position). Any other 8-byte store is split
    /// into two word stores at `off` and `off + 4`, logged: no evidence says
    /// what a real pipe does with one.
    pub fn bus_write(&self, off: u64, size: u32, val: u64) -> u32 {
        if self.shared.dead.load(Relaxed) {
            return BUS_ERR;
        }
        if off >= APERTURE {
            return BUS_ERR;
        }
        let off = off as u32;
        match size {
            4 => self.write_word(off, val as u32),
            8 => {
                let (hi, lo) = ((val >> 32) as u32, val as u32);
                if off == consts::COMMAND_PAGE {
                    self.write_words(&[(off, hi), (off, lo)])
                } else {
                    crate::dlog!(crate::devlog::LogModule::Kona, "kona: 8-byte store at offset {off:#x} split into two 4-byte accesses");
                    self.write_words(&[(off, hi), (off.wrapping_add(4), lo)])
                }
            }
            _ => BUS_ERR,
        }
    }

    fn read_word(&self, off: u32) -> Option<u32> {
        let host = &self.shared.host;
        match classify(off, false) {
            Access::Now => Some(self.read_now(off)),
            _ => self.with_idle_pipe(|pipe| pipe.core.read_wait(host, off)),
        }
    }

    fn read_now(&self, off: u32) -> u32 {
        use consts::*;
        let s = &*self.shared;
        let pending = !s.ring.is_empty();
        match off {
            INT_CAUSE => {
                // Reasons waiting behind an unread one are looked for when
                // the cause register is accessed.
                if s.host.reasons_waiting.load(SeqCst) {
                    if let Some(mut p) = s.try_pipe() {
                        p.core.reason_drain(&s.host);
                        s.publish(&p.core);
                    }
                }
            }
            ACTIVITY => {
                // The latch reads set while work waits, the pipe being busy,
                // and the work sets it again as it consumes words.
                return if s.host.activity.load(Relaxed) || pending { ACTIVITY_BUS } else { 0 };
            }
            _ => {}
        }
        // The register is the graphics FIFO's fill level: the words the pipe
        // holds, those of commands still arriving and the stores waiting in
        // the ring, rounded up to two (the field's bit 0 never sets). It
        // reads full while a display list runs: one store can stand for a
        // long list, so a count of words would understate the wait. Software
        // expectation: users wait for it to fall (to zero, below 0x5dd, or
        // to 0x300) before storing more. Inferred: the unit is 32-bit words.
        // Unknown: the FIFO's depth (levels of 0x70c have been read from real
        // pipes, so it holds at least that).
        let credit = if s.host.list_running.load(SeqCst) {
            CREDIT_MASK
        } else {
            let words = s.host.held_words.load(SeqCst) + s.ring.len() as u32;
            ((words + 1) & !1).min(CREDIT_MASK)
        };
        s.host.front.lock().read(off, credit).unwrap_or_else(|| {
            crate::dlog!(crate::devlog::LogModule::Kona, "kona: unimplemented read of offset {off:#x}");
            0
        })
    }

    /// Run `f` on the pipe once the stream has run: None, for the bus to
    /// answer busy, while stores wait in the ring or the thread is mid-run.
    fn with_idle_pipe<R>(&self, f: impl FnOnce(&mut Pipe) -> R) -> Option<R> {
        let s = &*self.shared;
        if !s.running.load(SeqCst) {
            return s.try_pipe().map(|mut p| f(&mut p));
        }
        if !s.ring.is_empty() {
            s.wake();
            return None;
        }
        let mut pipe = s.try_pipe()?;
        Some(f(&mut pipe))
    }

    /// One word of a store. A stream store goes into the ring and nothing
    /// more; the rest is the slow path.
    #[inline]
    fn write_word(&self, off: u32, val: u32) -> u32 {
        let s = &*self.shared;
        if classify(off, true) == Access::Queues && s.running.load(Relaxed) {
            let ok = s.ring.try_push(off, val as u64);
            s.wake();
            return if ok { BUS_OK } else { BUS_BUSY };
        }
        self.write_words(&[(off, val)])
    }

    /// The words of one store, together: they all go in or none does, so a
    /// busy answer can be retried.
    fn write_words(&self, words: &[(u32, u32)]) -> u32 {
        let s = &*self.shared;
        let mut kinds = [Access::Waits; 2];
        for (k, &(o, _)) in kinds.iter_mut().zip(words) {
            *k = classify(o, true);
        }
        let kinds = &kinds[..words.len()];
        if kinds.iter().all(|k| *k == Access::Queues) && s.running.load(Relaxed) {
            let ok = match words {
                [(o, v)] => s.ring.try_push(*o, *v as u64),
                [(o0, v0), (o1, v1)] => s.ring.try_push2(*o0, *v0 as u64, *o1, *v1 as u64),
                _ => unreachable!(),
            };
            s.wake();
            return if ok { BUS_OK } else { BUS_BUSY };
        }
        if kinds.iter().all(|k| *k == Access::Now) {
            for &(o, v) in words {
                self.write_now(o, v);
            }
            return BUS_OK;
        }
        let done = self.with_idle_pipe(|pipe| {
            let Pipe { core, backend } = pipe;
            for (&(o, v), k) in words.iter().zip(kinds) {
                match k {
                    Access::Queues => core.stream_store(&s.host, &mut **backend, o, v),
                    Access::Now => self.write_now(o, v),
                    Access::Waits => core.write_wait(&s.host, &mut **backend, o, v),
                }
            }
            s.publish(core);
        });
        // Without the thread, stream stores ran on this one: the FIFO-low
        // signal is the end of the run.
        if done.is_some() && !s.running.load(SeqCst) && kinds.contains(&Access::Queues) {
            s.drained();
        }
        if done.is_some() { BUS_OK } else { BUS_BUSY }
    }

    fn write_now(&self, off: u32, value: u32) {
        let s = &*self.shared;
        let host = &s.host;
        if off == consts::ACTIVITY {
            if value & consts::ACTIVITY_BUS != 0 {
                host.activity.store(false, Relaxed);
            }
            if value & !consts::ACTIVITY_BUS != 0 {
                crate::dlog!(crate::devlog::LogModule::Kona, "kona: unimplemented write of offset {off:#x} (value {value:#x})");
            }
            return;
        }
        let mut packet = None;
        let known = host.front.lock().write(off, value, &mut packet);
        if let Some(p) = packet {
            host.attach.packets.send(p);
        }
        if !known && off != consts::INTR_END {
            crate::dlog!(crate::devlog::LogModule::Kona, "kona: unimplemented write of offset {off:#x} (value {value:#x})");
        }
        if off == consts::INT_CAUSE && host.reasons_waiting.load(SeqCst) {
            if let Some(mut p) = s.try_pipe() {
                p.core.reason_drain(host);
                s.publish(&p.core);
            }
        }
    }

    /// The widget the pipe's DMA requests go to: bits 25:22 of the widget
    /// control register, which host software fills with its interrupt
    /// target. A machine's DMA handle may choose the destination with it.
    pub fn dma_widget(&self) -> u32 {
        (self.shared.host.front.lock().widget_control >> 22) & 0xf
    }

    /// Why the pipe stopped taking commands, if it did.
    pub fn failure(&self) -> Option<String> {
        if self.shared.dead.load(Relaxed) {
            return Some("kona: the back end panicked on the Host Interface Processor thread; the board is dead".into());
        }
        self.shared.pipe_wait().core.failed.clone()
    }

    /// The flash's contents.
    pub fn flash_image(&self) -> Vec<u8> {
        self.shared.pipe_wait().core.flash.image().to_vec()
    }

    /// The sections of the last completed firmware load.
    pub fn load_record(&self) -> Vec<LoadSection> {
        self.shared.pipe_wait().core.load_record().to_vec()
    }

    /// Run `f` on the back end, with the thread's work finished first.
    pub fn with_backend<R>(&self, f: impl FnOnce(&mut dyn Backend) -> R) -> R {
        let s = &*self.shared;
        // The ring is empty or the thread would be running a batch.
        let mut guard = s.pipe_wait();
        f(&mut *guard.backend)
    }

    /// Bring the back end to rest for a snapshot. The machine's processors are
    /// stopped: the stores in the ring run to completion, then the back end
    /// finishes its queued work and parks its threads. `resume` undoes it.
    pub fn quiesce(&self) {
        let s = &*self.shared;
        while s.running.load(SeqCst) && !s.dead.load(SeqCst) && !s.ring.is_empty() {
            s.wake();
            std::thread::yield_now();
        }
        s.pipe_wait().backend.quiesce();
    }

    /// Undo `quiesce`.
    pub fn resume(&self) {
        self.shared.pipe_wait().backend.resume();
    }

    fn reset(&self) {
        let was_running = self.is_running();
        Device::stop(self);
        let s = &*self.shared;
        s.ring.reset();
        {
            let mut pipe = s.pipe_wait();
            pipe.core.reset();
            // A new epoch before the back end cancels its tickets, so a call
            // a thread of its made in the old one is dropped.
            s.host.front.lock().advance_epoch();
            pipe.backend.reset();
            s.publish(&pipe.core);
        }
        s.host.front.lock().reset();
        s.host.activity.store(false, Relaxed);
        if was_running {
            Device::start(self);
        }
    }
}

impl Device for Kona {
    fn step(&self, _cycles: u64) {}

    fn start(&self) {
        let mut t = self.thread.lock();
        if t.is_some() {
            return;
        }
        let s = self.shared.clone();
        s.pipe_wait().backend.attach(DisplayHandle::new(&s.host));
        s.exit.store(false, SeqCst);
        s.running.store(true, SeqCst);
        *t = Some(
            std::thread::Builder::new()
                .name("kona-hip".into())
                .spawn(move || s.thread_main())
                .expect("kona thread"),
        );
    }

    fn stop(&self) {
        let Some(t) = self.thread.lock().take() else { return };
        let s = &*self.shared;
        s.exit.store(true, SeqCst);
        s.wake();
        if let Some(th) = s.sleeper.lock().as_ref() {
            th.unpark();
        }
        let _ = t.join();
        s.running.store(false, SeqCst);
        // What the thread had not taken runs here, unless it died: its back
        // end would only panic again.
        if !s.dead.load(SeqCst) {
            s.drain_inline();
        }
        // The back end's own threads join with the slot taken and no lock
        // held: one of them may be inside a handle call that sends a packet.
        // Always, even for a dead board: `start` attaches again, and the
        // first run's threads must be gone by then. A back end that panics
        // here is logged, not rethrown, since this runs from `Drop`.
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.pipe_wait().backend.stop()));
        if stopped.is_err() {
            crate::dlog!(crate::devlog::LogModule::Kona, "kona: the back end panicked while stopping");
        }
    }

    fn is_running(&self) -> bool {
        self.shared.running.load(SeqCst)
    }

    fn get_clock(&self) -> u64 {
        0
    }

    fn register_commands(&self) -> Vec<(String, String)> {
        vec![("kona".into(), "InfiniteReality pipe: status|counters (kona help)".into())]
    }

    fn execute_command(&self, cmd: &str, args: &[&str], mut w: Box<dyn std::io::Write + Send>) -> Result<(), String> {
        if cmd != "kona" {
            return Err(format!("unknown kona command: {cmd}"));
        }
        match self.with_backend(|b| b.monitor(args, &mut *w)) {
            Ok(true) => Ok(()),
            Ok(false) => Err("kona: status|counters".into()),
            Err(e) => Err(e.to_string()),
        }
    }
}

impl Drop for Kona {
    fn drop(&mut self) {
        Device::stop(self);
    }
}

impl Resettable for Kona {
    fn power_on(&self) {
        self.reset();
    }
}
