//! The seam between the host interface processor and whatever draws.
//!
//! The front end frames the command stream, answers the commands the host
//! interface processor answers itself (pages, replies, the configuration
//! request, the flash, DMA and display lists) and hands every command that
//! changes drawing or display state to a `Backend` as a typed `Job`. A back
//! end implements `Backend`; until one exists, `NullBackend` consumes the jobs
//! and completes retrace requests at once.
//!
//! A back end runs on the host interface processor's thread, in stream order.
//! It reaches the front end only through the `BackPort` it is handed: the
//! return and reason pages, the mailbox, the retrace completion and the
//! pipe's address space for its own DMA.

use super::attach::HostDma;
use super::handle::DisplayHandle;
use super::pipe::Core;

/// Which grammar a command belongs to; selectors repeat between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grammar {
    /// The PROM's textport, after its sync word.
    Textport,
    /// The command page's kernel, X server and graphics-process commands.
    Kernel,
    /// The GL client's commands.
    Gl,
}

/// What happened to a rendering context the host interface processor tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextEvent {
    /// The context in table entry `slot` became current. `fresh`: it starts
    /// afresh (a new save area), not a restore of one the pipe has seen.
    Activated { slot: usize, fresh: bool },
    /// The current context was switched out and its save area written.
    Deactivated { slot: usize },
    /// The driver destroyed the current context's window.
    WindowReleased { slot: usize },
}

/// One thing for the back end to do.
#[derive(Debug)]
pub enum Job<'a> {
    /// A command the front end framed. `handled`: the front end already did
    /// its part (a page registered, a reply posted) and this is the back end's
    /// look at it, for state that depends on seeing every token. `packed` is
    /// the top half of a GL command's opening word, which the byte and short
    /// forms use for data.
    Command { grammar: Grammar, selector: u32, args: &'a [u32], packed: u32, handled: bool },
    /// The words behind a command that carried them: a colour map, a gamma
    /// table, a cursor, a texture table, or a run the back end asked for.
    Data { grammar: Grammar, selector: u32, args: &'a [u32], words: &'a [u32] },
    /// A textport bitmap blit: six header words, then the bits.
    Bitmap { header: &'a [u32], payload: &'a [u32] },
    /// A video command whose checksum verified: the command number and its
    /// data words.
    Video { command: u32, data: &'a [u32] },
    /// A retrace request: complete it with `BackPort::retrace_done`.
    Retrace { tag: u32 },
    /// A context change.
    Context(ContextEvent),
    /// A store to the indirect data register under a selector the front end
    /// does not answer itself (the display generator's cursor registers).
    Indirect { selector: u32, value: u32 },
    /// A DMA packet no display list is waiting for: `lines` rows of
    /// `per_line` bytes at `io`, `pitch` apart. `physical`: `io` is a host
    /// physical address (the packet's high word had bit 28 set); otherwise it
    /// is a device address the map RAM translates. The back end reads the rows
    /// through `BackPort::dma_read`.
    DmaIn { io: u64, physical: bool, lines: u32, per_line: u32, pitch: u64 },
    /// The loader released the firmware (a store of 1 to the unstall
    /// register): the video format combination stored in the flash, if its
    /// marker and count are good.
    FirmwareStart { combination: &'a [u32] },
}

/// What a job leaves the command stream waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// Nothing more.
    Done,
    /// This many data words follow the command; they come back as one
    /// `Job::Data`.
    Data(u32),
    /// A `DmaIn` nothing was waiting for.
    Unclaimed,
}

/// Whatever draws and displays. Runs on the host interface processor's
/// thread.
pub trait Backend: Send {
    fn job(&mut self, port: &mut BackPort<'_>, job: Job<'_>) -> Reply;

    /// The board is starting; no thread of the board's runs yet. A back end
    /// with threads of its own spawns them here and keeps the handle for the
    /// display thread. Called again after each `stop`.
    fn attach(&mut self, _handle: DisplayHandle) {}

    /// The board was reset. Cancel pending tickets: the front end has begun a
    /// new epoch and drops calls made in the old one.
    fn reset(&mut self) {}

    /// The board stopped, after the Host Interface Processor's thread joined
    /// and the jobs it had not taken ran (a dead board skips those, not this).
    /// Join the back end's threads. Called
    /// with no board lock held, since a thread of the back end may be inside
    /// a handle call that sends an interrupt packet.
    fn stop(&mut self) {}

    /// The machine's processors are stopped and the command ring is empty:
    /// finish the queued work and park every thread, for a snapshot. A command
    /// the framers hold part-delivered stays part-framed; saving that is the
    /// front end's.
    fn quiesce(&mut self) {}

    /// Undo `quiesce`.
    fn resume(&mut self) {}
}

/// The front end as a back end may use it.
pub struct BackPort<'a> {
    pub(super) core: &'a mut Core,
    pub(super) host: &'a super::front::Host,
}

impl BackPort<'_> {
    /// Answer on the return page: `data` at byte `offset`, then the ready
    /// word 1 at byte 0, the only thing the waiting process polls. False,
    /// logged, with no return page registered.
    pub fn post_return(&mut self, offset: u32, data: &[u8]) -> bool {
        self.core.post_return(self.host, 0, offset, data)
    }

    /// Post a completion reason on the reason page and raise the
    /// geometry-engine interrupt.
    pub fn post_reason(&mut self, token: u32, reason: u32) {
        self.core.post_reason(self.host, token, &[reason]);
    }

    /// Queue reply words for the driver to read from the mailbox.
    pub fn mailbox_reply(&mut self, words: &[u32]) {
        self.host.mailbox_reply(words);
    }

    /// A retrace request was honoured: the tag register reads `tag` and the
    /// video interrupt rises.
    pub fn retrace_done(&mut self, tag: u32) {
        self.host.with_front(|f| ((), f.retrace(tag)));
    }

    /// The managed area's size, which the configuration request reports.
    pub fn set_managed_size(&mut self, width: u32, height: u32) {
        self.core.managed = (width, height);
    }

    /// Whether a return page is registered.
    pub fn return_page_valid(&self) -> bool {
        self.core.return_page.is_some()
    }

    /// The table entry of the current context.
    pub fn context(&self) -> Option<usize> {
        self.core.current
    }

    /// Read pipe-address-space bytes (the device addresses the driver's
    /// packets carry, translated through the map RAM). False if a page is
    /// unmapped or the host read fails.
    pub fn device_read(&mut self, device: u64, buf: &mut [u8]) -> bool {
        self.core.device_read(self.host, device, buf)
    }

    /// Write pipe-address-space bytes.
    pub fn device_write(&mut self, device: u64, data: &[u8]) -> bool {
        self.core.device_write(self.host, device, data)
    }

    /// Read `buf.len()` bytes of a DMA packet's buffer: at host physical
    /// address `io` when `physical`, else at device address `io` through the
    /// map RAM. False if a page is unmapped or the host read fails.
    pub fn dma_read(&mut self, io: u64, physical: bool, buf: &mut [u8]) -> bool {
        if physical {
            self.host.attach.dma.read(io, buf)
        } else {
            self.core.device_read(self.host, io, buf)
        }
    }

    /// A transfer the back end made failed on the bus: the DMA packet it
    /// served is completed as failed, withholding its completion word.
    pub fn dma_fault(&mut self) {
        self.core.dma_fault = true;
    }

    /// The host's DMA, for physical addresses.
    pub fn host_dma(&self) -> &dyn HostDma {
        &*self.host.attach.dma
    }
}

/// A back end that draws nothing: it consumes every job, counts them, and
/// completes retrace requests at once so a driver waiting on one goes on.
#[derive(Default)]
pub struct NullBackend {
    pub jobs: u64,
}

impl Backend for NullBackend {
    fn job(&mut self, port: &mut BackPort<'_>, job: Job<'_>) -> Reply {
        self.jobs += 1;
        match job {
            Job::Retrace { tag } => port.retrace_done(tag),
            Job::DmaIn { .. } => return Reply::Unclaimed,
            _ => {}
        }
        Reply::Done
    }
}
