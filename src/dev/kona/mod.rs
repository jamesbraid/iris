//! The InfiniteReality (KONA) graphics pipe's front end: the host interface.
//!
//! Ported from hw/misc/sgi-kona.c, sgi-kona-ports.c, sgi-kona-dma.c and
//! sgi-command-port.c in jamesbraid/qemu-sgi-origami.

mod attach;
mod consts;
mod dma;
mod flash;
mod frame;
mod front;
mod ident;
mod job;
mod pipe;
mod regs;
mod tables;
#[cfg(test)]
mod tests;

pub use attach::{FifoLow, HostDma, InterruptPacket, KonaAttach, PacketSender};
pub use consts::APERTURE;
pub use flash::FLASH_SIZE;
pub use ident::KonaConfig;
pub use job::{Backend, BackPort, ContextEvent, Grammar, Job, NullBackend, Reply};
pub use pipe::LoadSection;

use crate::traits::{Resettable, BUS_BUSY, BUS_ERR, BUS_OK};
use front::{Front, Host};
use parking_lot::{Condvar, Mutex};
use pipe::Core;
use regs::{classify, Access};
use std::fs::File;
use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::sync::Arc;

/// The Host Interface Processor's state with the back end it hands jobs to.
struct Pipe {
    core: Core,
    backend: Box<dyn Backend>,
}

struct Shared {
    host: Host,
    /// The Host Interface Processor's state, or None while someone has it
    /// out (`Shared::pipe`). The mutex is held for an instant to take it or
    /// put it back, never across work.
    pipe: Mutex<Option<Box<Pipe>>>,
    /// Signalled when the state is put back: what a monitor or reset waits
    /// on.
    freed: Condvar,
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

}

/// The InfiniteReality pipe's host interface.
pub struct Kona {
    shared: Arc<Shared>,
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
        let host = Host {
            front: Mutex::new(front),
            attach: self.attach,
            held_words: Default::default(),
            reasons_waiting: Default::default(),
            list_running: Default::default(),
            activity: Default::default(),
        };
        Ok(Arc::new(Kona {
            shared: Arc::new(Shared {
                host,
                pipe: Mutex::new(Some(Box::new(Pipe { core: Core::new(self.cfg, flash), backend: self.backend }))),
                freed: Condvar::new(),
            }),
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
        let pending = false;
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
            let words = s.host.held_words.load(SeqCst);
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
        self.shared.try_pipe().map(|mut p| f(&mut p))
    }

    /// One word of a store. A stream store goes into the ring and nothing
    /// more; the rest is the slow path.
    fn write_word(&self, off: u32, val: u32) -> u32 {
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
        // Stream stores ran on this thread: the FIFO-low signal is the end of
        // the run.
        if done.is_some() && kinds.contains(&Access::Queues) {
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

    fn reset(&self) {
        let s = &*self.shared;
        {
            let mut pipe = s.pipe_wait();
            pipe.core.reset();
            pipe.backend.reset();
            s.publish(&pipe.core);
        }
        s.host.front.lock().reset();
        s.host.activity.store(false, Relaxed);
    }
}

impl Resettable for Kona {
    fn power_on(&self) {
        self.reset();
    }
}
