//! The host interface processor's state and the command stream it runs.
//!
//! `Core` is everything the board's thread owns: the framers that turn the
//! command page's words into commands, the pages the pipe answers on, the
//! contexts, the flash, the map RAM, the display-list memory and the DMA
//! state. A processor's store to a command port is queued and reaches `Core`
//! in order; a register read that depends on the stream waits until the
//! stream has run (the bus answers busy meanwhile), then reaches it too.
//!
//! The command page takes plain words. A word is an argument of the command
//! still open, else it opens one: the textport's sync word starts a textport
//! command, a kernel or graphics-process token starts a command of the kernel
//! table, and a GL selector starts a GL command. How the grammars share the
//! page is inferred from the senders (see `Core::textport_push`); the pipe's
//! own rule is unknown.
//!
//! Ported from hw/misc/sgi-kona.c in jamesbraid/qemu-sgi-origami.

use super::consts::*;
use super::dma::Dma;
use super::flash::{Action as FlashAction, Flash, FlashOp};
use super::frame::{tables, Packet, Port, Step, Tables};
use super::front::Host;
use super::ident::{self, KonaConfig};
use super::job::{Backend, BackPort, ContextEvent, Grammar, Job, Reply};
use crate::devlog::LogModule;
use std::collections::VecDeque;

/// A run of words behind a command, or a command taking its words by a rule
/// of its own.
#[derive(Clone)]
pub(super) enum Run {
    None,
    /// Words behind a command: a colour map, a gamma table, a cursor, a
    /// texture table, or what the back end asked for. They come back as one
    /// `Job::Data`.
    Words { grammar: Grammar, selector: u32, args: Vec<u32>, words: Vec<u32>, remaining: u32 },
    /// A video command: its number, then data words and a checksum equal to
    /// the number plus the data words.
    Video { command: u32, data: Vec<u32>, sum: u32 },
    /// A flash EEPROM operation.
    Flash(FlashOp),
    /// A list invocation taking its (offset, word count) pairs.
    Calls { token: u32, need: usize, words: Vec<u32> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BitmapState {
    Idle,
    Header,
    Payload,
}

/// A textport bitmap blit in progress: six header words, then
/// ceil(width * height / 32) payload words.
#[derive(Clone)]
struct Bitmap {
    state: BitmapState,
    header: Vec<u32>,
    payload: Vec<u32>,
    expected: usize,
}

impl Bitmap {
    fn idle() -> Self {
        Bitmap { state: BitmapState::Idle, header: Vec::new(), payload: Vec::new(), expected: 0 }
    }
}

/// Where the command stream is: everything a context switch sets aside and
/// brings back with the context.
#[derive(Clone)]
pub(super) struct Input {
    textport: Port,
    gl: Port,
    /// A user command the kernel framer had open when a forced store set it
    /// aside.
    kernel_suspended: Port,
    /// The textport expects its sync word.
    expect_sync: bool,
    /// A sync word was seen after the GL grammar began: the next word says
    /// whether it was the textport's or the start of a buffer configuration.
    gl_sync_held: bool,
    /// 1 after a held `1 1`, 2 after its third word.
    level1: u8,
    level1_arg: u32,
    /// The top half of the opening word of the GL command open.
    gl_token_high: u32,
    bitmap: Bitmap,
    pub run: Run,
}

impl Input {
    fn new(t: &'static Tables) -> Self {
        Input {
            textport: Port::new(&t.textport),
            gl: Port::new(&t.gl),
            kernel_suspended: Port::new(&t.kernel),
            expect_sync: true,
            gl_sync_held: false,
            level1: 0,
            level1_arg: 0,
            gl_token_high: 0,
            bitmap: Bitmap::idle(),
            run: Run::None,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct Context {
    save_page: u32,
    save_offset: u32,
    return_page: Option<u64>,
}

/// The most contexts kept, and the entry transient ones (a restore of a save
/// area nobody set up) use. A host limit, not the pipe's capacity.
const CONTEXT_LIMIT: usize = 64;
const CONTEXT_TRANSIENT: usize = CONTEXT_LIMIT;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SwapStage {
    Idle,
    Mode,
    Data,
    Reason,
}

struct SwapIn {
    stage: SwapStage,
    header: [u32; 6],
    mode: u32,
    data_left: u64,
}

pub(super) struct Core {
    pub cfg: KonaConfig,
    tables: &'static Tables,
    pub input: Input,
    saved: Vec<Option<Box<Input>>>,
    /// The kernel/graphics-process framer and its dma-fifo twin.
    pub kernel: Port,
    pub dma_fifo: Port,
    /// The privileged bit of the bracket register is set: the words are a
    /// kernel command's.
    kernel_input: bool,
    /// The kernel command now open was opened under that bit.
    kernel_privileged: bool,
    /// A forced store suspended the current context's user command.
    suspended_owner: Option<usize>,
    /// Once a context has been activated the page speaks the GL grammar.
    gl_grammar: bool,
    swapin: SwapIn,

    // Pages the pipe answers on.
    pub return_page: Option<u64>,
    reason_page: Option<u64>,
    reason_unread: bool,
    reason_posted: u32,
    reason_queue: VecDeque<(u32, [u32; REASON_WORDS], usize)>,

    // Contexts.
    contexts: Vec<Context>,
    context_count: usize,
    pub current: Option<usize>,
    context_page_pending: bool,
    /// Why the pipe stopped taking commands, if it did.
    pub failed: Option<String>,

    pub managed: (u32, u32),

    pub map_ram: Box<[u32]>,
    pub flash: Flash,
    pub dma: Dma,
    pub dma_fault: bool,

    // The indirect port into the pipe-internal address space.
    pub(super) port_addr: u32,
    pub(super) icu_control: u32,
    pub(super) arm_low: [u32; ARM_LOW_WORDS],
    pub(super) arm_high: Vec<u32>,
    pub(super) load_open: bool,
    pub(super) load_count: usize,
    pub(super) load_sections: Vec<LoadSection>,
    pub(super) load_record: Vec<LoadSection>,
    pub(super) indirect_select: u32,
}

/// One section of the firmware load: where its words started and how many
/// there were.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadSection {
    pub addr: u32,
    pub words: u32,
}

impl Core {
    pub fn new(cfg: KonaConfig, flash: Flash) -> Self {
        let t = tables();
        Core {
            cfg,
            tables: t,
            input: Input::new(t),
            saved: (0..=CONTEXT_LIMIT).map(|_| None).collect(),
            kernel: Port::new(&t.kernel),
            dma_fifo: Port::new(&t.dma_fifo),
            kernel_input: false,
            kernel_privileged: false,
            suspended_owner: None,
            gl_grammar: false,
            swapin: SwapIn { stage: SwapStage::Idle, header: [0; 6], mode: 0, data_left: 0 },
            return_page: None,
            reason_page: None,
            reason_unread: false,
            reason_posted: 0,
            reason_queue: VecDeque::new(),
            contexts: vec![Context::default(); CONTEXT_LIMIT + 1],
            context_count: 0,
            current: None,
            context_page_pending: false,
            failed: None,
            managed: (1280, 1024),
            map_ram: vec![0u32; MAPRAM_ENTRIES].into_boxed_slice(),
            flash,
            dma: Dma::new(),
            dma_fault: false,
            port_addr: 0,
            icu_control: 0,
            arm_low: [0; ARM_LOW_WORDS],
            arm_high: vec![0; ARM_HIGH_WORDS],
            load_open: false,
            load_count: 0,
            load_sections: Vec::new(),
            load_record: Vec::new(),
            indirect_select: 0,
        }
    }

    /// Power-on state. The flash and the configuration stay.
    pub fn reset(&mut self) {
        let t = self.tables;
        self.input = Input::new(t);
        self.saved.iter_mut().for_each(|s| *s = None);
        self.kernel.reset();
        self.dma_fifo.reset();
        self.kernel_input = false;
        self.kernel_privileged = false;
        self.suspended_owner = None;
        self.gl_grammar = false;
        self.swapin.stage = SwapStage::Idle;
        self.return_page = None;
        self.reason_page = None;
        self.reason_unread = false;
        self.reason_posted = 0;
        self.reason_queue.clear();
        self.contexts.iter_mut().for_each(|c| *c = Context::default());
        self.context_count = 0;
        self.current = None;
        self.context_page_pending = false;
        self.failed = None;
        self.managed = (1280, 1024);
        self.map_ram.fill(0);
        self.dma.reset();
        self.dma_fault = false;
        self.port_addr = 0;
        self.icu_control = 0;
        self.arm_low = [0; ARM_LOW_WORDS];
        self.arm_high.fill(0);
        self.load_open = false;
        self.load_count = 0;
        self.load_sections.clear();
        self.load_record.clear();
        self.indirect_select = 0;
    }

    /// Words the framers hold of commands still arriving.
    pub fn held_words(&self) -> u32 {
        self.input.textport.held() + self.kernel.held() + self.input.gl.held() + self.dma_fifo.held()
    }

    pub fn reasons_waiting(&self) -> bool {
        !self.reason_queue.is_empty()
    }

    pub(super) fn job(&mut self, h: &Host, be: &mut dyn Backend, job: Job<'_>) -> Reply {
        be.job(&mut BackPort { core: self, host: h }, job)
    }

    // ----- pages and replies -----------------------------------------------

    /// The page a 0x1f1 or 0x1f2 command names in its first two words `page`
    /// and `offset`. With bit 28 of `page` set the page is host physical;
    /// otherwise `page << 12` plus `offset`'s low twelve bits is a device
    /// address, translated through the map RAM entry for its 16 KiB page.
    /// Both readings are inferred from the driver's uses. A device address
    /// the map RAM does not map yields nothing.
    pub(super) fn command_page(&self, h: &Host, what: &str, page: u32, offset: u32) -> Option<u64> {
        if page & PAGE_WORD_PHYSICAL != 0 {
            return Some(((page & PAGE_WORD_FRAME) as u64) << 12 | (offset & 0xfff) as u64);
        }
        let device = (page as u64) << 12 | (offset & 0xfff) as u64;
        match self.device_address(h, device) {
            Some(a) => Some(a),
            None => {
                crate::dlog!(
                    LogModule::Kona,
                    "kona: {what} page at device address {device:#x} is not mapped; not registered"
                );
                None
            }
        }
    }

    fn register_reason_page(&mut self, h: &Host, page: u32, offset: u32) {
        if let Some(a) = self.command_page(h, "reason", page, offset) {
            self.reason_page = Some(a);
            self.reason_unread = false;
        }
    }

    /// 0x1f2: the return-data page, where the pipe answers a process's
    /// requests and the process polls word 0. The kernel's context switch
    /// sends it straight after a context's first-activation 0x1fd, so that
    /// page becomes the context's own, restored with it by 0x1f9; a 0x1f2
    /// with no first activation waiting only replaces the page the pipe
    /// answers on. A page that does not translate leaves any earlier
    /// registration in place, except for a context waiting for its page,
    /// which is left with none rather than another context's.
    fn register_return_page(&mut self, h: &Host, page: u32, offset: u32) {
        let waiting = if self.context_page_pending { self.current } else { None };
        self.context_page_pending = false;
        match self.command_page(h, "return", page, offset) {
            Some(a) => {
                self.return_page = Some(a);
                if let Some(c) = waiting {
                    self.contexts[c].return_page = Some(a);
                }
            }
            None => {
                if waiting.is_some() {
                    self.return_page = None;
                }
            }
        }
    }

    /// Post a return-page answer: `data` at `offset`, then a non-zero word 0,
    /// the only thing the waiting process polls. The order is inferred:
    /// software may use the page as soon as word 0 goes non-zero, so the
    /// data must already be there. Approximated: the model answers at
    /// once rather than after queued work. With no return page it logs and
    /// writes nothing.
    pub(super) fn post_return(&mut self, h: &Host, token: u32, offset: u32, data: &[u8]) -> bool {
        let Some(page) = self.return_page else {
            crate::dlog!(LogModule::Kona, "kona: token {token:#x} with no return page registered; nothing posted");
            return false;
        };
        if !data.is_empty() && !h.attach.dma.write(page + offset as u64, data) {
            crate::dlog!(LogModule::Kona, "kona: {}-byte write to {:#x} failed", data.len(), page + offset as u64);
            return false;
        }
        h.attach.dma.write(page, &1u32.to_be_bytes())
    }

    fn flash_action(&mut self, h: &Host, action: Option<FlashAction>) {
        match action {
            None => {}
            Some(FlashAction::Reply(words)) => {
                // The word count, then the words, from word 1 of the reply
                // buffer, and word 0 last, as the ARM's reply routines write
                // it. Approximated: at once, where the ARM first programs the
                // part.
                let mut reply = Vec::with_capacity(4 + 4 * words.len());
                reply.extend_from_slice(&(words.len() as u32).to_be_bytes());
                for w in &words {
                    reply.extend_from_slice(&w.to_be_bytes());
                }
                self.post_return(h, CMD_EEPROM, 4, &reply);
            }
            Some(FlashAction::ReplyBuffer { page, offset }) => {
                // The pipe keeps the reply buffer in the same place 0x1f2
                // sets, so later answers to either land there. Unlike 0x1f2
                // it keeps the offset word's low 12 bits.
                if let Some(a) = self.command_page(h, "EEPROM reply", page, offset) {
                    self.return_page = Some(a);
                }
            }
        }
    }

    // ----- completion reasons ----------------------------------------------

    /// Post a completion reason on the reason page: written big-endian from
    /// word 0, then the geometry-engine cause bit set and its interrupt
    /// raised, which is what the driver's geometry-engine interrupt handler
    /// reads and dispatches on. The handler reads word 0 once an interrupt
    /// and writes it back ORed with a marker, so a second reason written
    /// before that read would replace the first. The model therefore posts
    /// one reason at a time: a reason that finds the last one's word 0 still
    /// on the page waits in a queue, and goes out once the page shows the
    /// driver's rewrite, looked for whenever the cause register is accessed
    /// and when the board's thread finishes a run.
    ///
    /// Approximated: hardware is inferred to post only once its queued work
    /// is done, and the model has no queued work, so a reason with nothing
    /// ahead of it posts at once. With no page registered it logs and posts
    /// nothing.
    pub(super) fn post_reason(&mut self, h: &Host, token: u32, words: &[u32]) {
        assert!(!words.is_empty() && words.len() <= REASON_WORDS);
        if self.reason_page.is_none() {
            crate::dlog!(LogModule::Kona, "kona: token {token:#x} reason {:#x} with no reason page registered; nothing posted", words[0]);
            return;
        }
        if self.reason_queue.is_empty() && !self.reason_pending(h) {
            self.reason_write(h, words);
            return;
        }
        if self.reason_queue.len() == REASON_QUEUE {
            crate::dlog!(LogModule::Kona, "kona: {REASON_QUEUE} completion reasons wait unread; reason {:#x} lost", words[0]);
            return;
        }
        let mut w = [0; REASON_WORDS];
        w[..words.len()].copy_from_slice(words);
        self.reason_queue.push_back((token, w, words.len()));
        self.reason_drain(h);
    }

    fn reason_write(&mut self, h: &Host, words: &[u32]) {
        let Some(page) = self.reason_page else { return };
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
        if !h.attach.dma.write(page, &bytes) {
            crate::dlog!(LogModule::Kona, "kona: {}-byte write to {page:#x} failed", bytes.len());
            return;
        }
        self.reason_unread = true;
        self.reason_posted = words[0];
        h.with_front(|f| {
            f.int_cause |= CAUSE_GE;
            ((), f.raise_vector(false))
        });
    }

    /// Whether the last reason's word 0 is still on the page unread. A page
    /// that cannot be read back cannot hold the next reason back.
    fn reason_pending(&mut self, h: &Host) -> bool {
        if !self.reason_unread {
            return false;
        }
        let mut word = [0u8; 4];
        let still = match self.reason_page {
            Some(page) => h.attach.dma.read(page, &mut word) && u32::from_be_bytes(word) == self.reason_posted,
            None => false,
        };
        self.reason_unread = still;
        still
    }

    /// Post queued reasons while the page is free.
    pub fn reason_drain(&mut self, h: &Host) {
        while !self.reason_queue.is_empty() && !self.reason_pending(h) {
            let (_, w, n) = self.reason_queue.pop_front().unwrap();
            self.reason_write(h, &w[..n]);
        }
    }

    // ----- contexts --------------------------------------------------------

    /// 0x1fd page offset len, or 0x1f9 word0 page offset sentinel: make the
    /// context whose save area the two words name current, and answer on its
    /// return page from here on. A 0x1fd starts the context afresh with no
    /// return page until the 0x1f2 that follows it. A 0x1f9 restores the page
    /// that 0x1f2 gave it; one naming a save area no 0x1fd named is logged and
    /// leaves the pipe with no return page, so an answer goes nowhere rather
    /// than to another context. The key is the two words as sent, which the
    /// kernel derives from one fixed allocation per context.
    fn context_activate(&mut self, h: &Host, be: &mut dyn Backend, token: u32, page: u32, offset: u32) {
        let first = token == CMD_CONTEXT_MAP;
        let mut fresh = first;
        let offset = offset & 0xfff;
        // The restore's 0x1e4 packets place the new context's regions.
        self.dma.list_base = 0;
        let mut slot = (0..self.context_count)
            .find(|&i| self.contexts[i].save_page == page && self.contexts[i].save_offset == offset);
        if slot.is_none() && !first {
            crate::dlog!(
                LogModule::Kona,
                "kona: context restore names save area {:#x}, which no {CMD_CONTEXT_MAP:#x} set up; no return page",
                (page as u64) << 12 | offset as u64
            );
            slot = Some(CONTEXT_TRANSIENT);
            fresh = true;
        }
        let slot = match slot {
            Some(s) => s,
            None => {
                if self.context_count == CONTEXT_LIMIT {
                    self.stop(format!(
                        "kona: command {token:#x} key {page:#x}:{offset:#x}: logical context limit reached ({CONTEXT_LIMIT} retained)"
                    ));
                    return;
                }
                self.context_count
            }
        };
        if fresh {
            self.saved[slot] = None;
            self.contexts[slot] = Context { save_page: page, save_offset: offset, return_page: None };
            if first && slot == self.context_count {
                self.context_count += 1;
            }
        }
        self.current = Some(slot);
        self.gl_grammar = true;
        self.context_page_pending = first;
        self.return_page = self.contexts[slot].return_page;
        self.job(h, be, Job::Context(ContextEvent::Activated { slot, fresh }));
    }

    /// 0x1fc index: switch the current context out. The kernel's context
    /// switch then sleeps until the pipe posts reason 0x101, and the driver's
    /// geometry-engine interrupt handler wakes it and cancels its 90 s
    /// deactivate timeout. When the context is restored later, the context
    /// switch reads words 0 and 2 of its save area back, so they are written
    /// before the post. The index names the context's entry in the kernel's
    /// table, not the save area, so the save area is the one the last 0x1fd or
    /// 0x1f9 named.
    ///
    /// Approximated: hardware writes a real context record into the save
    /// area; the model writes only the two words software reads back, and
    /// posts at once, since it keeps no queued work.
    fn context_deactivate(&mut self, h: &Host, be: &mut dyn Backend, index: u32) {
        match self.current {
            None => crate::dlog!(LogModule::Kona, "kona: context deactivate {index} with no context current; no save area written"),
            Some(slot) => {
                let c = self.contexts[slot];
                if let Some(addr) = self.command_page(h, "save area", c.save_page, c.save_offset) {
                    h.attach.dma.write(addr, &SAVE_AREA_MARKER.to_be_bytes());
                    h.attach.dma.write(addr + 8, &SAVE_AREA_TOKEN.to_be_bytes());
                }
                self.job(h, be, Job::Context(ContextEvent::Deactivated { slot }));
            }
        }
        self.current = None;
        self.context_page_pending = false;
        self.post_reason(h, CMD_CONTEXT_DEACTIVATE, &[REASON_CONTEXT_DEACTIVATED]);
    }

    /// The pipe stops taking commands: a limit the model keeps was reached.
    fn stop(&mut self, why: String) {
        crate::dlog!(LogModule::Kona, "{why}");
        self.failed = Some(why);
    }

    // ----- kernel pool swaps -----------------------------------------------

    /// The kernel's pipe-memory swaps. When a pool lacks room it evicts other
    /// contexts' regions with 0x1e3: class, offset and size in granules, the
    /// granule in bytes, the copy count, the host buffer's device address as
    /// page >> 12 and (address & 0xfff) | 0x9000, and the completion reason
    /// 0x102. Swap-in, 0x1e2, carries the same six words and then either the
    /// tagged offset, for the pipe to read the buffer back by DMA, or
    /// 0xffffffff and the buffer's words inline, then 0x102; it completes
    /// through the mailbox with 0x102.
    ///
    /// The kernel keeps the swapped-out buffer untouched and only hands it
    /// back on swap-in, so the bytes are the pipe's own. Approximated: the
    /// model keeps every texture and list in its own storage, whatever the
    /// pool placement, so a swap moves none of it. A swap-out writes zeros of
    /// the full length into the buffer, a swap-in reads the buffer and
    /// discards it, and both complete at once.
    fn swap_bytes(a: &[u32]) -> u64 {
        a[2] as u64 * a[3] as u64 * a[4] as u64
    }

    fn mem_swap_out(&mut self, h: &Host, a: &[u32]) {
        let bytes = Self::swap_bytes(a);
        let device = (a[5] as u64) << 12 | (a[6] & 0xfff) as u64;
        let ok = a[6] & !0xfff == DMA_ADDR_TAG && a[7] == REASON_SWAP && bytes <= SWAP_MAX_BYTES && self.device_zero(h, device, bytes);
        if !ok {
            crate::dlog!(
                LogModule::Kona,
                "kona: swap-out of class {} offset {:#x} size {:#x} to {:#x} {:#x} reason {:#x} not completed",
                a[0], a[1], a[2], a[5], a[6], a[7]
            );
            return;
        }
        self.post_reason(h, CMD_MEM_SWAP_OUT, &[REASON_SWAP]);
    }

    fn swap_in_done(&mut self, h: &Host) {
        let bytes = Self::swap_bytes(&self.swapin.header);
        let mut ok = bytes <= SWAP_MAX_BYTES;
        if ok && self.swapin.mode != u32::MAX {
            let device = (self.swapin.header[5] as u64) << 12 | (self.swapin.mode & 0xfff) as u64;
            let mut chunk = vec![0u8; 4096];
            let mut done = 0;
            while ok && done < bytes {
                let n = (bytes - done).min(4096) as usize;
                ok = self.device_read(h, device + done, &mut chunk[..n]);
                done += n as u64;
            }
        }
        if !ok {
            crate::dlog!(
                LogModule::Kona,
                "kona: swap-in of class {} offset {:#x} size {:#x} not completed",
                self.swapin.header[0], self.swapin.header[1], self.swapin.header[2]
            );
            return;
        }
        h.post_mailbox(REASON_SWAP);
    }

    /// A word after a swap-in's six. False for a word that does not belong to
    /// it, which ends the swap-in, logged, and is taken afresh.
    fn swap_in_word(&mut self, h: &Host, word: u32) -> bool {
        match self.swapin.stage {
            SwapStage::Mode => {
                if word == u32::MAX {
                    let bytes = Self::swap_bytes(&self.swapin.header);
                    self.swapin.mode = word;
                    if bytes % 4 == 0 && bytes <= SWAP_MAX_BYTES {
                        self.swapin.data_left = bytes / 4;
                        self.swapin.stage = if self.swapin.data_left != 0 { SwapStage::Data } else { SwapStage::Reason };
                        return true;
                    }
                } else if word & !0xfff == DMA_ADDR_TAG {
                    self.swapin.mode = word;
                    self.swapin.stage = SwapStage::Reason;
                    return true;
                }
            }
            SwapStage::Data => {
                self.swapin.data_left -= 1;
                if self.swapin.data_left == 0 {
                    self.swapin.stage = SwapStage::Reason;
                }
                return true;
            }
            SwapStage::Reason => {
                self.swapin.stage = SwapStage::Idle;
                if word == REASON_SWAP {
                    self.swap_in_done(h);
                    return true;
                }
            }
            SwapStage::Idle => return false,
        }
        crate::dlog!(
            LogModule::Kona,
            "kona: swap-in of class {} ended by word {word:#x}; not completed",
            self.swapin.header[0]
        );
        self.swapin.stage = SwapStage::Idle;
        false
    }

    // ----- the map RAM -----------------------------------------------------

    /// The host physical address a device address maps to through the XG map
    /// RAM, for the one entry encoding the driver's page mapper writes: bits
    /// 31:28 = 0x2 (a 16 KiB page), bits 27:0 the 4 KiB frame number of that
    /// page.
    pub(super) fn device_address(&self, h: &Host, device: u64) -> Option<u64> {
        let index = (device >> MAPRAM_PAGE_SHIFT) as usize;
        let entry = if index < h.front.lock().mapram_count() { self.map_ram[index] } else { 0 };
        if entry & MAPRAM_KIND_MASK != MAPRAM_KIND_16K {
            return None;
        }
        Some(((entry & PAGE_WORD_FRAME) as u64) << 12 | (device & ((1 << MAPRAM_PAGE_SHIFT) - 1)))
    }

    /// Read pipe-address-space bytes, page by page.
    pub(super) fn device_read(&self, h: &Host, device: u64, buf: &mut [u8]) -> bool {
        let mut done = 0;
        while done < buf.len() {
            let at = device + done as u64;
            let n = (buf.len() - done).min(((1u64 << MAPRAM_PAGE_SHIFT) - (at & ((1 << MAPRAM_PAGE_SHIFT) - 1))) as usize);
            let Some(host) = self.device_address(h, at) else {
                crate::dlog!(LogModule::Kona, "kona: read of device address {at:#x} has no map RAM entry");
                return false;
            };
            if !h.attach.dma.read(host, &mut buf[done..done + n]) {
                return false;
            }
            done += n;
        }
        true
    }

    /// Write pipe-address-space bytes, page by page.
    pub(super) fn device_write(&self, h: &Host, device: u64, data: &[u8]) -> bool {
        let mut done = 0;
        while done < data.len() {
            let at = device + done as u64;
            let n = (data.len() - done).min(((1u64 << MAPRAM_PAGE_SHIFT) - (at & ((1 << MAPRAM_PAGE_SHIFT) - 1))) as usize);
            let Some(host) = self.device_address(h, at) else {
                crate::dlog!(LogModule::Kona, "kona: write to device address {at:#x} has no map RAM entry");
                return false;
            };
            if !h.attach.dma.write(host, &data[done..done + n]) {
                return false;
            }
            done += n;
        }
        true
    }

    /// Zero `bytes` of the device address space, checking every page maps
    /// before writing any.
    fn device_zero(&self, h: &Host, device: u64, bytes: u64) -> bool {
        let page = 1u64 << MAPRAM_PAGE_SHIFT;
        let mut at = device;
        while at < device + bytes {
            if self.device_address(h, at).is_none() {
                return false;
            }
            at = (at | (page - 1)) + 1;
        }
        let zeros = vec![0u8; 1 << 16];
        let mut done = 0;
        while done < bytes {
            let n = (bytes - done).min(zeros.len() as u64) as usize;
            if !self.device_write(h, device + done, &zeros[..n]) {
                return false;
            }
            done += n as u64;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::kona::front::Front;
    use crate::dev::kona::job::NullBackend;
    use crate::dev::kona::tests::{attach, TestHost};
    use std::sync::Arc;

    fn rig() -> (Arc<TestHost>, Host, Core) {
        let t = TestHost::new();
        let cfg = KonaConfig::default();
        let h = Host {
            front: parking_lot::Mutex::new(Front::new(&cfg)),
            attach: attach(&t),
            held_words: Default::default(),
            reasons_waiting: Default::default(),
            list_running: Default::default(),
            activity: Default::default(),
        };
        (t, h, Core::new(cfg, Flash::fresh()))
    }

    const PHYS: u32 = PAGE_WORD_PHYSICAL;

    #[test]
    fn an_answer_stores_its_data_and_then_the_ready_word() {
        let (t, h, mut core) = rig();
        assert!(!core.post_return(&h, 0x1ec, 8, &[1, 2, 3, 4]), "no return page: nothing is posted");
        assert_eq!(t.word(0x1000), 0);
        core.return_page = Some(0x1000);
        assert!(core.post_return(&h, 0x1ec, 8, &[1, 2, 3, 4]));
        assert_eq!(t.bytes(0x1008, 4), [1, 2, 3, 4]);
        assert_eq!(t.word(0x1000), 1);
    }

    #[test]
    fn a_device_address_maps_through_its_16k_page() {
        let (t, h, mut core) = rig();
        core.map_ram[3] = MAPRAM_KIND_16K | (0x40_0000 >> 12);
        core.map_ram[4] = MAPRAM_KIND_16K | (0x50_0000 >> 12);
        let dev = |page: u64, off: u64| (page << MAPRAM_PAGE_SHIFT) + off;
        assert_eq!(core.device_address(&h, dev(3, 0x123)), Some(0x40_0123));
        assert_eq!(core.device_address(&h, dev(5, 0)), None, "an unmapped page");
        // A transfer that crosses into the next page goes to that page's frame.
        assert!(core.device_write(&h, dev(3, 0x3ffe), &[1, 2, 3, 4]));
        assert_eq!(t.bytes(0x40_3ffe, 2), [1, 2]);
        assert_eq!(t.bytes(0x50_0000, 2), [3, 4]);
        let mut back = [0u8; 4];
        assert!(core.device_read(&h, dev(3, 0x3ffe), &mut back));
        assert_eq!(back, [1, 2, 3, 4]);
        assert!(!core.device_write(&h, dev(4, 0x3fff), &[9, 9]), "the next page is unmapped");
    }

    #[test]
    fn a_reason_waits_until_the_last_one_has_been_read() {
        let (t, h, mut core) = rig();
        core.register_reason_page(&h, PHYS | (0x2000 >> 12), 0);
        core.post_reason(&h, 0x1f8, &[0x103]);
        core.post_reason(&h, 0x1dd, &[0x777]);
        assert_eq!(t.word(0x2000), 0x103);
        assert!(core.reasons_waiting());
        core.reason_drain(&h);
        assert_eq!(t.word(0x2000), 0x103, "still unread");
        t.put(0x2000, &(0x103u32 | 0xbad0_0000).to_be_bytes());
        core.reason_drain(&h);
        assert_eq!(t.word(0x2000), 0x777);
        assert!(!core.reasons_waiting());
        assert_eq!(h.front.lock().int_cause & CAUSE_GE, CAUSE_GE);
    }

    #[test]
    fn a_restored_context_answers_on_its_own_return_page() {
        let (t, h, mut core) = rig();
        let mut be = NullBackend::default();
        let save = |n: u32| PHYS | (0x10_0000 + 0x1_0000 * n) >> 12;
        core.context_activate(&h, &mut be, CMD_CONTEXT_MAP, save(0), 0);
        core.register_return_page(&h, PHYS | (0x3000 >> 12), 0);
        core.context_activate(&h, &mut be, CMD_CONTEXT_MAP, save(1), 0);
        core.register_return_page(&h, PHYS | (0x4000 >> 12), 0);
        assert_eq!(core.return_page, Some(0x4000));
        core.context_activate(&h, &mut be, CMD_CONTEXT_RESTORE, save(0), 0);
        assert_eq!(core.return_page, Some(0x3000));
        assert_eq!(core.current, Some(0));
        // A save area nobody set up has no page to answer on.
        core.context_activate(&h, &mut be, CMD_CONTEXT_RESTORE, save(7), 0);
        assert_eq!(core.return_page, None);
        let _ = t;
    }
}
