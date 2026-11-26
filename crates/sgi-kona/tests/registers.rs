//! The pipe's register contract through the engine: the save area a context deactivate leaves, the
//! activity latch, the widget-reset accesses, one return page per context, the kernel pool packets and
//! the periodic video cause and its mask. Ported from the iris board's own tests of the same names
//! (which came from QEMU's bring-up and context qtests), so a shell's copy of this crate carries them.
//!
//! What each value rests on is said where it is asserted. The kernel's programming (interrupt
//! vectors, destination, the commands it sends) is a software expectation; the values the engine
//! chooses itself are marked as its own choice.

use std::sync::Arc;

use parking_lot::Mutex;
use sgi_engine::{BoardEngine, Event, HostMem, MemFault, Read, Swap, Write};
use sgi_kona::dev::kona::defs::*;
use sgi_kona::dev::kona::engine::{KonaEngine, Options};
use sgi_kona::dev::kona::ident::KonaConfig;

/// Guest memory the test can read back: bytes, zero where nothing was written.
#[derive(Clone, Default)]
struct Mem(Arc<Mutex<Vec<u8>>>);

impl HostMem for Mem {
    fn ram_read(&self, addr: u64, buf: &mut [u8], _: Swap) -> Result<(), MemFault> {
        let m = self.0.lock();
        buf.iter_mut().enumerate().for_each(|(i, b)| *b = m.get(addr as usize + i).copied().unwrap_or(0));
        Ok(())
    }

    fn ram_write(&self, addr: u64, data: &[u8], _: Swap) -> Result<(), MemFault> {
        let mut m = self.0.lock();
        if m.len() < addr as usize + data.len() {
            m.resize(addr as usize + data.len(), 0);
        }
        m[addr as usize..addr as usize + data.len()].copy_from_slice(data);
        Ok(())
    }
}

impl Mem {
    fn word(&self, addr: u64) -> u32 {
        let mut b = [0u8; 4];
        self.ram_read(addr, &mut b, Swap::None).unwrap();
        u32::from_be_bytes(b)
    }
}

struct Rig {
    e: KonaEngine<Mem>,
    mem: Mem,
    /// Interrupt packets seen so far, drained from the engine as the shell would.
    packets: Vec<(u8, u8, u64, bool)>,
}

const REASON_PAGE: u64 = 0x30_0000;
const OTHER_PAGE: u64 = 0x31_0000;
const RETURN_PAGE: u64 = 0x32_0000;
// The registers the kernel programs (offsets in the pipe's window).
const WIDGET_CONTROL: u32 = 0x24;
const INT_DEST_UPPER: u32 = 0x34;
const INT_DEST_LOWER: u32 = 0x3c;
const INT_VECTORS_REG: u32 = 0x5c;
const WIDGET_ERROR: u32 = 0x6c;
const WIDGET_ERROR_CLEAR: u32 = 0x2c;
const WIDGET_STATUS: u32 = 0x74;
const RETRACE_TAG: u32 = 0x800248;
const PORT_ADDR: u32 = 0x8003e8;
const PORT_WRITE_NEXT: u32 = 0x8003f8;
/// The interrupt programming the kernel sends (software expectation, from the traced boot): vectors
/// 0xf (xg), 0x11 (geometry engine) and 0x10 (general), and node 0's interrupt-modify register at widget 9.
const KERNEL_INT_VECTORS: u32 = 0x3e2210;
const KERNEL_INT_DEST_UPPER: u32 = 0x98000;
const KERNEL_INT_DEST_LOWER: u32 = 0x180_0090;
const GE_VECTOR: u8 = 0x11;
const GENERAL_VECTOR: u8 = 0x10;

impl Rig {
    fn new() -> Rig {
        let mem = Mem::default();
        let e = KonaEngine::new(KonaConfig::ir2(2).unwrap().with_pool(0, 32), mem.clone(), Options::default(), None).unwrap();
        Rig { e, mem, packets: Vec::new() }
    }

    fn w(&mut self, off: u32, val: u32) {
        while self.e.write(off, 32, val as u64) == Write::Busy {
            self.e.run(usize::MAX);
        }
        self.e.run(usize::MAX);
    }

    fn r(&mut self, off: u32) -> u32 {
        loop {
            match self.e.read(off, 32) {
                Read::Value(v) => return v as u32,
                Read::Busy => {
                    self.e.run(usize::MAX);
                }
                Read::Fault => panic!("read {off:#x}: {:?}", self.e.fault()),
            }
        }
    }

    fn cmd(&mut self, words: &[u32]) {
        for &w in words {
            self.w(COMMAND_PAGE, w);
        }
    }

    /// Interrupt packets sent since the last call.
    fn sent(&mut self) -> Vec<(u8, u8, u64, bool)> {
        let mut ev = Vec::new();
        self.e.drain_events(&mut ev, usize::MAX);
        let new: Vec<_> = ev.iter().filter_map(|e| if let Event::Interrupt(i) = e { Some((i.vector, i.widget, i.addr, i.level)) } else { None }).collect();
        self.packets.extend(&new);
        new
    }

    fn program_interrupts(&mut self) {
        self.w(INT_DEST_UPPER, KERNEL_INT_DEST_UPPER);
        self.w(INT_DEST_LOWER, KERNEL_INT_DEST_LOWER);
        self.w(INT_VECTORS_REG, KERNEL_INT_VECTORS);
    }

    fn physical(addr: u64) -> u32 {
        0x1000_0000 | (addr >> 12) as u32
    }

    fn reason_page(&mut self, addr: u64) {
        self.cmd(&[0x1f1, Self::physical(addr), (addr & 0xfff) as u32, 0x4000]);
    }

    fn return_page(&mut self, addr: u64) {
        self.cmd(&[0x1f2, Self::physical(addr), (addr & 0xfff) as u32, 0x4000]);
    }

    /// The PROM's sync probe: its mailbox reply shows the command page is between commands.
    fn assert_probe_answers(&mut self) {
        self.cmd(&[1, 9, 0]);
        assert_eq!(self.r(INT_CAUSE) & CAUSE_MAILBOX, CAUSE_MAILBOX);
        assert_eq!(self.r(MAILBOX), SYNC_PROBE_REPLY);
        self.w(MAILBOX, 0);
        self.w(INT_CAUSE, CAUSE_MAILBOX);
    }

    fn assert_no_reply(&mut self) {
        assert_eq!(self.r(INT_CAUSE) & CAUSE_MAILBOX, 0, "no mailbox reply is queued");
        assert_eq!(self.r(MAILBOX), 0);
    }
}

/// A deactivate leaves two words in the context's save area, at the offset the kernel registered with
/// 0x1fd: word 0 is the engine's own marker ("KONA", `SAVE_AREA_MARKER`: a choice of the model, which the
/// restore checks back), word 2 the no-op token 0x172 the kernel sends back (a software expectation).
/// The reason 0x101, the geometry-engine cause and its one interrupt on the geometry vector are the
/// kernel's contract; the handler's write of the cause bit clears it. (iris `context_deactivate_posts`.)
#[test]
fn a_context_deactivate_leaves_the_models_marker_and_the_no_op_token_in_the_save_area() {
    let mut t = Rig::new();
    t.program_interrupts();
    t.reason_page(REASON_PAGE);
    // The kernel's first activation: a save area at +0x40 of a physical page, then the return page.
    t.cmd(&[0x1fd, Rig::physical(OTHER_PAGE), 0x40, 0x10000]);
    t.return_page(RETURN_PAGE);
    t.sent();
    t.cmd(&[0x1fc, 3]);
    assert_eq!(t.mem.word(REASON_PAGE), 0x101);
    assert_eq!(t.mem.word(OTHER_PAGE + 0x40), SAVE_AREA_MARKER, "word 0: the model's marker");
    assert_eq!(t.mem.word(OTHER_PAGE + 0x40 + 8), SAVE_AREA_TOKEN, "word 2: the no-op token");
    assert_eq!(t.mem.word(OTHER_PAGE + 0x40 + 4), 0, "the word between is left alone");
    assert_eq!(t.r(INT_CAUSE) & CAUSE_GE, CAUSE_GE);
    let sent = t.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, GE_VECTOR);
    t.w(INT_CAUSE, CAUSE_GE);
    assert_eq!(t.r(INT_CAUSE) & CAUSE_GE, 0);
}

/// The word after 0x1fc is its argument: an index of 1 does not open a textport packet, so a 0x1f8
/// 0x103 sent next still posts 0x103 and the textport's sync probe still answers afterwards.
/// (iris `context_deactivate_argument_is_framed`.)
#[test]
fn the_word_after_a_deactivate_is_its_argument_and_not_a_textport_sync() {
    let mut t = Rig::new();
    t.program_interrupts();
    t.reason_page(REASON_PAGE);
    t.cmd(&[0x1fc, 1]);
    assert_eq!(t.mem.word(REASON_PAGE), 0x101);
    // The driver's handler reads the reason and writes it back with a marker, then clears the cause.
    t.mem.ram_write(REASON_PAGE, &(0x101u32 | 0xbad0_0000).to_be_bytes(), Swap::None).unwrap();
    t.w(INT_CAUSE, CAUSE_GE);
    t.cmd(&[0x1f8, 0x103]);
    assert_eq!(t.mem.word(REASON_PAGE), 0x103);
    t.assert_probe_answers();
}

/// The driver's periodic timeout check reads the activity register, then writes 4 back: it sees bit 4 only
/// when a command-port word or a load word arrived since the previous round. The fatal-error path's write
/// of 8 neither sets nor clears the latch, and the kernel's once-per-video-interrupt write of bit 0 does
/// not clear it either. (iris `activity_latch`; the bits are the kernel's contract.)
#[test]
fn the_activity_latch_is_set_by_arriving_words_and_cleared_only_by_writing_its_bit_back() {
    let mut t = Rig::new();
    assert_eq!(t.r(ACTIVITY), 0);
    t.cmd(&[1, 9, 0]);
    assert_eq!(t.r(ACTIVITY), ACTIVITY_BUS);
    t.w(ACTIVITY, 8);
    assert_eq!(t.r(ACTIVITY), ACTIVITY_BUS, "a write of 8 does not clear it");
    t.w(ACTIVITY, ACTIVITY_VIDEO);
    assert_eq!(t.r(ACTIVITY), ACTIVITY_BUS, "nor does the video acknowledgement");
    t.w(ACTIVITY, ACTIVITY_BUS);
    assert_eq!(t.r(ACTIVITY), 0);
    // A firmware load word sets it again, whether it comes by the loader register or the port.
    t.w(0x80007c, 0xdead);
    assert_eq!(t.r(ACTIVITY), ACTIVITY_BUS);
    t.w(ACTIVITY, ACTIVITY_BUS);
    t.w(PORT_ADDR, 0);
    t.w(PORT_WRITE_NEXT, 1);
    assert_eq!(t.r(ACTIVITY), ACTIVITY_BUS);
}

/// What the kernel's widget reset does to the pipe's crosstalk registers: the control register stores and
/// reads back, the error register +0x6c reads 0 (the model raises no widget errors, so this value is the
/// engine's own) before and after it is cleared through +0x2c and written back, bit 17 of +0x24 sets by
/// read-modify-write, +0x74 is rewritten, then 0xfff goes to the interrupt cause and 0 to the two
/// registers at +0x800390 and +0x800398, which the model gives no meaning and accepts. The pipe still
/// answers afterwards. (iris `widget_reset_accesses`.)
#[test]
fn the_widget_reset_accesses_are_taken_and_the_pipe_still_answers() {
    let mut t = Rig::new();
    t.w(WIDGET_CONTROL, 0x3000);
    assert_eq!(t.r(WIDGET_CONTROL), 0x3000);
    t.program_interrupts();
    assert_eq!(t.r(WIDGET_ERROR), 0, "the model raises no widget errors");
    t.w(WIDGET_ERROR_CLEAR, 0);
    t.w(WIDGET_ERROR, 0);
    assert_eq!(t.r(WIDGET_ERROR), 0);
    let c = t.r(WIDGET_CONTROL);
    t.w(WIDGET_CONTROL, c | 1 << 17);
    assert_eq!(t.r(WIDGET_CONTROL), 0x3000 | 1 << 17);
    let s = t.r(WIDGET_STATUS);
    t.w(WIDGET_STATUS, (s & 0x0fff_0000) | 0x3f40);
    t.w(INT_CAUSE, 0xfff);
    t.w(0x800390, 0);
    t.w(0x800398, 0);
    assert_eq!(t.r(0x800390), 0);
    t.assert_probe_answers();
}

/// Each context keeps the return page it was given with the 0x1f2 after its 0x1fd; a restore brings back
/// its context's page, and a restore naming a save area nobody set up leaves the pipe with no return
/// page, so an answer goes nowhere rather than to another context's page. The query 0x128 answers on the
/// return page at word 2. (iris `contexts_keep_their_return_pages`; the restore's marker is the engine's own.)
#[test]
fn each_context_keeps_its_own_return_page() {
    let mut t = Rig::new();
    let (save_a, save_b) = (0x50_0000u64, 0x51_0000u64);
    let (page_a, page_b) = (0x52_0000u64, 0x53_0000u64);
    t.cmd(&[0x1fd, Rig::physical(save_a), 0, 0x10000]);
    t.return_page(page_a);
    t.cmd(&[0x1fd, Rig::physical(save_b), 0, 0x10000]);
    t.return_page(page_b);
    t.cmd(&[0x128, 1, 2, 3]);
    assert_eq!(t.mem.word(page_b + 8), 1, "context B answers on its page");
    t.cmd(&[0x1fc, 1]);
    // Restore A: its page comes back.
    t.cmd(&[0x1f9, SAVE_AREA_MARKER, Rig::physical(save_a), 0, 0xeded_eded]);
    t.cmd(&[0x128, 7, 8, 9]);
    assert_eq!(t.mem.word(page_a + 8), 7, "A is back on its page");
    assert_eq!(t.mem.word(page_b + 8), 1, "and B's page is as it was");
    // A save area nobody set up.
    t.cmd(&[0x1f9, SAVE_AREA_MARKER, Rig::physical(0x54_0000), 0, 0xeded_eded]);
    t.cmd(&[0x128, 4, 5, 6]);
    assert_eq!(t.mem.word(page_a + 8), 7, "an answer goes nowhere rather than to another context");
    assert_eq!(t.mem.word(page_b + 8), 1);
}

/// The kernel's pool packets are consumed whole: a free sends 0x1e4 with its offset, the write-gather
/// flush 0x1ff sent straight after a 0x172 0 frames as its own packet, and 0x1e1's words here are shaped
/// like the sync probe, which a model that dropped the token would answer on the mailbox. None of them is
/// answered, and the page is between commands afterwards. (iris `kernel_pool_packets`; the words are the
/// kernel's, from the traced boot: a software expectation.)
#[test]
fn the_kernel_pool_packets_are_consumed_whole_and_answer_nothing() {
    let mut t = Rig::new();
    t.w(0xc00108, 0x30000);
    t.cmd(&[0x1e4, 2, 9, 0, 1]);
    t.w(0xc00108, 0);
    t.cmd(&[0x172, 0, 0x1ff, 0xdead_beef]);
    t.cmd(&[0x1e4, 2, 9, 0, 0]);
    t.cmd(&[0x1e1, 1, 9, 0, 0]);
    t.assert_no_reply();
    t.assert_probe_answers();
}

/// The video cause rises every retrace but sends its vector once per cause and only while unmasked; a
/// retrace token sets the tag with no vector of its own. Three vectors in all, as QEMU's `periodic-video-mask`.
#[test]
fn the_periodic_video_cause_sends_its_vector_once_per_cause_and_only_while_unmasked() {
    let mut t = Rig::new();
    t.program_interrupts();
    let general = |t: &mut Rig| t.sent().iter().filter(|p| p.0 == GENERAL_VECTOR && p.3).count();
    t.e.frame(0);
    assert_eq!(t.r(INT_CAUSE), CAUSE_VIDEO);
    assert_eq!(t.r(RETRACE_TAG), 0);
    assert_eq!(general(&mut t), 0, "masked: no vector");
    t.w(INT_ENABLE, CAUSE_VIDEO);
    assert_eq!(general(&mut t), 1, "unmasked with the cause pending: one vector");
    t.e.frame(0);
    assert_eq!(general(&mut t), 0, "a second retrace with the cause pending adds none");
    t.w(INT_ENABLE, 0);
    t.w(INT_ENABLE, CAUSE_VIDEO);
    assert_eq!(general(&mut t), 0, "masking and unmasking with the cause pending sends nothing");
    t.w(INT_CAUSE, CAUSE_VIDEO);
    t.e.frame(0);
    t.w(INT_CAUSE, CAUSE_VIDEO);
    let again = general(&mut t);
    assert_eq!(again, 1, "a cause cleared and raised again sends the next vector");
    t.w(INT_ENABLE, 0);
    t.w(0xc00108, 0x10000);
    t.cmd(&[0x1e7, 0x8765]);
    assert_eq!(t.r(RETRACE_TAG), 0x8765, "the token sets the tag before any retrace");
    t.e.frame(0);
    assert_eq!(general(&mut t), 0, "masked: the retrace's vector waits");
    t.w(INT_ENABLE, CAUSE_VIDEO);
    t.e.frame(0);
    assert_eq!(general(&mut t), 1, "unmasked: it goes");
    assert_eq!(t.packets.iter().filter(|p| p.0 == GENERAL_VECTOR && p.3).count(), 3);
}

/// A link reset puts the widget's crosstalk interface registers (widget control, interrupt destination and
/// vectors) back to their reset values, which the driver's adaptor reset rewrites after it; the pipe's own
/// registers and the map RAM stay, so the shell need not power the pipe on for it. (iris
/// `a_link_reset_clears_the_crosstalk_interface_registers_only`; what else a real link reset touches is unknown.)
#[test]
fn a_link_reset_clears_the_crosstalk_interface_registers_only() {
    const MAPRAM: u32 = 0x100000;
    const RESET_ARG: u32 = 0x800220;
    const CREDIT_CONFIG: u32 = 0x800210;
    const EEPROM_ENABLE: u32 = 0x800308;
    const LOADER_3B0: u32 = 0x8003b0;
    let mut t = Rig::new();
    let reset_control = t.r(WIDGET_CONTROL);
    t.w(WIDGET_CONTROL, 9 << 22);
    t.program_interrupts();
    t.w(RESET_ARG, 0x8000);
    t.w(CREDIT_CONFIG, 0x700);
    t.w(EEPROM_ENABLE, 1);
    t.w(LOADER_3B0, 5);
    t.w(MAPRAM + 8, 0x2000_1234);
    t.e.link_reset();
    assert_eq!(t.r(WIDGET_CONTROL), reset_control);
    for reg in [INT_DEST_UPPER, INT_DEST_LOWER, INT_VECTORS_REG] {
        assert_eq!(t.r(reg), 0, "register {reg:#x} was reset");
    }
    assert_eq!((t.r(RESET_ARG), t.r(CREDIT_CONFIG), t.r(EEPROM_ENABLE), t.r(LOADER_3B0)), (0x8000, 0x700, 1, 5));
    assert_eq!(t.r(MAPRAM + 8), 0x2000_1234, "the map RAM stays");
    t.assert_probe_answers();
}
