//! The GR2 engine behind `BoardEngine`: register accesses in, events and frames out, time passed in, no
//! emulator behind it. The board below the register window is iris's own, mounted unchanged; this is the
//! handle around it. The call contract is in the `sgi_engine` crate docs. What is GR2's own:
//!
//! - The HQ2 and RE3 run on their own threads, so there is no inline mode; `quiesce` joins them and
//!   keeps the queues, and `resume` starts them.
//! - Time is the shell's. A stall only that time ends is `Idle::Stalled(StallOn::Time(t))`, reported once the
//!   board has moved nothing for 250 ms of host time (`STALL_QUIET`). That rule reads the host clock, so a
//!   shell that records and replays calls sees the answer vary with host speed: record the `Stalled` it
//!   got (and the time it then advanced to), and replay that, not the rule.
//! - HQ2_GEDMA is a data port (`push_words`) and a read port (`pull_words`).
//! - No interrupt is raised yet (`irq_levels` is 0), and the retrace is the shell's: `frame` is told of it.
//! - `scanout` composes on the CPU and waits a bounded time for queued work (`OBSERVE_WAIT`). Its size and
//!   `head_timing` come from the VC1 and the monitor code (`display.rs`).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sgi_engine::{BoardEngine, Event, Fault, HeadTiming, HostMem, Idle, Mark, Outbox, Pump, Read, Rect, Refused, StallOn, StateSink, StateSource, Write};

use super::board::{Gr2, Gr2Probe, GR2_BASE, GR2_SLOT_SIZE};
use super::display;
use super::gr2comp;
use super::re3::{FB_H, FB_W};
use crate::traits::{BUS_BUSY, BUS_OK};

/// The version of the state `save` writes.
pub const GR2_STATE_VERSION: u32 = 2;

/// The board offset of the HQ2_GEDMA port: the data port the kernel's pixel DMA writes, and the read port
/// its context saves and pixel reads come back on.
pub const GEDMA_PORT: u32 = 0x6a068;

const FIFO: u32 = 0x40000;
const FIFO_END: u32 = 0x60000;

/// How long `scanout` and `observe` wait for queued work, in host time, before they go on without it: a
/// stalled port cannot be drained from inside the call.
const OBSERVE_WAIT: Duration = Duration::from_millis(100);
/// How long the board must show no progress, in host time, before a stall that only the shell's time can
/// end is reported as one. A long command is not a stall: reporting it early would jump the shell's clock
/// past the guest's own poll.
const STALL_QUIET: Duration = Duration::from_millis(250);
/// A budget no host clock overflows on (one year, in ns).
const FOREVER_NS: u64 = 31_536_000_000_000_000;

pub struct Config {
    pub probe: Gr2Probe,
    /// The RE3 span JIT: -1 the board's default (the environment, else compile in the background), 0
    /// off, 1 compile on first use and wait, 2 compile in the background. Without the `gr2-jit`
    /// feature the interpreter alone draws.
    pub jit: i32,
}

impl Config {
    pub fn new(probe: Gr2Probe) -> Config {
        Config { probe, jit: -1 }
    }
}

/// What stopped the engine, shared with the board's panic hook.
#[derive(Default)]
struct Failure {
    text: Mutex<Option<String>>,
}

impl Failure {
    fn get(&self) -> Option<String> {
        self.text.lock().clone()
    }

    /// Record the first fault and tell the shell.
    fn set(&self, why: &str, out: &Outbox) {
        {
            let mut t = self.text.lock();
            if t.is_some() {
                return;
            }
            *t = Some(why.to_string());
        }
        out.post(Event::DeviceFault(why.to_string()));
        out.wake_now();
    }
}

pub struct Gr2Engine<M: HostMem> {
    gr: Arc<Gr2>,
    config: Config,
    out: Arc<Outbox>,
    failure: Arc<Failure>,
    _mem: M,
    /// The board's threads are stopped (`quiesce`) until `resume`.
    parked: bool,
    /// Moves when the picture may have changed; `frame_generation` adds the board's own pending flag.
    generation: u64,
    /// The composed picture (`0x00RRGGBB`, the size `size()` gave) and the generation it shows.
    picture: Option<(u64, Vec<u32>)>,
    /// The board's progress position and the host time it was last seen to move.
    progress: Mutex<(u64, Instant)>,
    /// The timing last reported (its key) and how many times it changed.
    timing: Mutex<(u64, u64)>,
    /// `compose`'s output, stride `OUT_STRIDE`.
    compose_buf: Vec<u32>,
    clock_ns: u64,
}

fn new_board(config: &Config, out: &Arc<Outbox>, failure: &Arc<Failure>) -> Result<Arc<Gr2>, String> {
    let p = config.probe;
    if !matches!(p.ges, 1 | 2 | 4 | 8) {
        return Err(format!("{} geometry engines: the board has 1, 2, 4 or 8", p.ges));
    }
    let gr = Gr2::build(p, crate::disp::StatusSource::detached(), None)?;
    #[cfg(feature = "gr2-jit")]
    {
        use super::re3_jit::{MODE_ASYNC, MODE_OFF, MODE_SYNC};
        match config.jit {
            0 => gr.set_jit_mode(MODE_OFF),
            1 => gr.set_jit_mode(MODE_SYNC),
            2 => gr.set_jit_mode(MODE_ASYNC),
            _ => {}
        }
    }
    let (o, f) = (out.clone(), failure.clone());
    gr.set_panic_hook(Box::new(move |why| f.set(why, &o)));
    Ok(gr)
}

impl<M: HostMem> Gr2Engine<M> {
    /// A board with `config`'s probe answers, at power-on, with its threads running. `mem` is the guest
    /// memory the shell lends; the board does no DMA of its own (the memory controller moves the
    /// words through the data and read ports), so it is not read. `notify` only schedules the shell's pump.
    pub fn new(config: Config, mem: M, notify: Option<Box<dyn Fn() + Send + Sync>>) -> Result<Self, String> {
        let out = Arc::new(Outbox::new(notify));
        let failure = Arc::new(Failure::default());
        let gr = new_board(&config, &out, &failure)?;
        gr.start_engine();
        Ok(Gr2Engine { gr, config, out, failure, _mem: mem, parked: false, generation: 0, picture: None, progress: Mutex::new((0, Instant::now())), timing: Mutex::new((0, 0)), compose_buf: Vec::new(), clock_ns: 0 })
    }

    /// The board, for tests and for shells that read its registers directly.
    pub fn board(&self) -> &Gr2 {
        &self.gr
    }

    /// The hardware cursor as the VC1's registers hold it: the glyph's top-left pixel in screen pixels,
    /// and whether it is shown.
    pub fn cursor_position(&self) -> (i32, i32, bool) {
        let vc1 = &self.gr.regs().vc1;
        let (x, y) = vc1.cursor_pos();
        (x, y, vc1.cursor_visible())
    }

    fn faulted(&self) -> bool {
        self.failure.get().is_some() || self.gr.panic_message().is_some()
    }

    /// The fault, taking a panic the board kept whose hook has not run yet.
    fn fault_text(&self) -> Option<String> {
        if let Some(p) = self.gr.panic_message() {
            self.failure.set(&p, &self.out);
        }
        self.failure.get()
    }

    fn valid(bits: u32, off: u32) -> bool {
        matches!(bits, 8 | 16 | 32 | 64) && off < GR2_SLOT_SIZE
    }

    /// Wait up to `limit` of host time for the board's queues to drain; whether they did.
    fn wait_for_idle(&self, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        loop {
            if self.gr.idle() {
                return true;
            }
            if self.parked || self.faulted() || Instant::now() >= end || self.stalled_until().is_some() {
                return false;
            }
            std::thread::sleep(Duration::from_micros(50));
        }
    }

    /// Note drawing or colour-table changes since the last call: a damage mark for the whole picture.
    fn sync_dirty(&mut self) {
        if self.gr.take_dirty() {
            self.generation += 1;
            self.out.damage(&[Mark { store: 0, planes: !0, rect: Rect::new(0, 0, FB_W as i32 - 1, FB_H as i32 - 1) }]);
        }
    }

    fn generation_now(&self) -> u64 {
        self.generation + self.gr.dirty_pending() as u64
    }

    /// The shell time a stall ends at, once the board has shown no progress for `STALL_QUIET`.
    fn stalled_until(&self) -> Option<u64> {
        let t = self.gr.stall_deadline().filter(|&t| t > self.clock_ns)?;
        let pos = self.gr.progress_position();
        let mut seen = self.progress.lock();
        if seen.0 != pos {
            *seen = (pos, Instant::now());
            return None;
        }
        (seen.1.elapsed() >= STALL_QUIET).then_some(t)
    }

    fn idle_state(&self) -> Idle {
        if self.faulted() {
            Idle::Faulted
        } else if self.gr.idle() {
            Idle::Yes
        } else {
            self.stalled_until().map_or(Idle::Working, |t| Idle::Stalled(StallOn::Time(t)))
        }
    }

    fn park(&mut self) {
        if !self.parked {
            self.gr.park_engine();
            self.parked = true;
        }
    }

    fn need_parked(&self) -> Result<(), Refused> {
        if let Some(f) = self.fault_text() {
            return Err(Refused::Faulted(f));
        }
        if self.parked { Ok(()) } else { Err(Refused::NotQuiesced) }
    }

    fn time(&mut self, now_ns: u64) {
        self.clock_ns = self.clock_ns.max(now_ns);
        self.gr.set_time(now_ns);
    }

    /// The picture's size: the VC1's, or the monitor code's before it is programmed.
    fn size(&self) -> (u32, u32) {
        display::size(&self.gr.regs().vc1, &self.config.probe)
    }

    /// Compose the picture unless the one held still shows the board's state. The part of the 1280 by
    /// 1024 composition a smaller mode shows is its top left corner; where the hardware puts it is not
    /// known.
    fn compose(&mut self) {
        let now = self.generation_now();
        if matches!(&self.picture, Some((g, _)) if *g == now) {
            return;
        }
        let r = self.gr.regs();
        self.compose_buf.resize(gr2comp::OUT_STRIDE * FB_H, 0);
        gr2comp::compose(self.gr.vram(), &r.vc1, &r.xmap, &r.dac, &mut self.compose_buf);
        let mut px = self.picture.take().map_or_else(Vec::new, |(_, v)| v);
        px.clear();
        let (w, h) = self.size();
        for row in self.compose_buf.chunks(gr2comp::OUT_STRIDE).take(h as usize) {
            // compose gives 0xAABBGGRR; the interface wants 0x00RRGGBB.
            px.extend(row[..w as usize].iter().map(|&w| ((w & 0xff) << 16) | (w & 0xff00) | ((w >> 16) & 0xff)));
        }
        self.picture = Some((now, px));
    }
}

impl<M: HostMem> BoardEngine for Gr2Engine<M> {
    type Region = Rect;

    fn write(&mut self, off: u32, bits: u32, val: u64) -> Write {
        if self.faulted() || !Self::valid(bits, off) {
            return Write::Fault;
        }
        let st = match bits {
            32 => self.gr.reg_write(off & (GR2_SLOT_SIZE - 4), val as u32),
            64 => self.gr.write64_inner(GR2_BASE + off as u64, val),
            _ => self.gr.reg_write_narrow(off & (GR2_SLOT_SIZE - 4), val as u32),
        };
        match st {
            BUS_OK => Write::Done,
            BUS_BUSY => Write::Busy,
            _ => Write::Fault,
        }
    }

    fn write_at(&mut self, now_ns: u64, off: u32, bits: u32, val: u64) -> Write {
        self.time(now_ns);
        self.write(off, bits, val)
    }

    fn try_store(&mut self, off: u32, bits: u32, val: u64) -> Write {
        if bits != 32 || !(FIFO..FIFO_END).contains(&off) || self.parked || self.faulted() {
            return Write::Busy;
        }
        match self.gr.reg_write(off & (GR2_SLOT_SIZE - 4), val as u32) {
            BUS_OK => Write::Done,
            _ => Write::Busy,
        }
    }

    fn push_words(&mut self, port: u32, words: &[u32]) -> usize {
        if self.faulted() {
            return 0;
        }
        if port != 0 && port != GEDMA_PORT {
            self.out.post(Event::Note(format!("push_words: port {port:#x} is not the HQ2_GEDMA data port ({GEDMA_PORT:#x})")));
            return 0;
        }
        self.gr.push_gedma(words)
    }

    fn pull_words(&mut self, port: u32, out: &mut [u32]) -> usize {
        if self.faulted() || (port != 0 && port != GEDMA_PORT) {
            return 0;
        }
        let mut n = 0;
        while n < out.len() {
            let Some(w) = self.gr.gedma_pop() else { break };
            out[n] = w;
            n += 1;
        }
        n
    }

    fn read(&mut self, off: u32, bits: u32) -> Read {
        if self.faulted() || !Self::valid(bits, off) {
            return Read::Fault;
        }
        let word = |at: u32| {
            let r = self.gr.reg_read(at & (GR2_SLOT_SIZE - 4));
            match r.status {
                BUS_OK => Ok(r.data),
                BUS_BUSY => Err(Read::Busy),
                _ => Err(Read::Fault),
            }
        };
        let hi = match word(off) {
            Ok(v) => v,
            Err(r) => return r,
        };
        match bits {
            8 => Read::Value(hi as u8 as u64),
            16 => Read::Value(hi as u16 as u64),
            32 => Read::Value(hi as u64),
            _ => match word((off & !7) + 4) {
                Ok(lo) => Read::Value(((hi as u64) << 32) | lo as u64),
                Err(r) => r,
            },
        }
    }

    fn access_waits(&self, off: u32, bits: u32, write: bool) -> bool {
        !self.faulted() && Self::valid(bits, off) && self.gr.access_would_wait(off, write)
    }

    fn run(&mut self, _max_ops: usize) -> usize {
        // The threads run the queues; nothing runs inside a call.
        0
    }

    fn frame(&mut self, now_ns: u64) {
        self.time(now_ns);
        self.sync_dirty();
    }

    fn heads(&self) -> u32 {
        1
    }

    fn scanout(&mut self, head: u32, out: &mut Vec<u32>) -> Result<Option<(u32, u32)>, Fault> {
        if let Some(f) = self.fault_text() {
            return Err(Fault(f));
        }
        if head != 0 {
            return Ok(None);
        }
        self.wait_for_idle(OBSERVE_WAIT);
        self.sync_dirty();
        self.compose();
        out.clear();
        if let Some((_, px)) = &self.picture {
            out.extend_from_slice(px);
        }
        Ok(Some(self.size()))
    }

    fn frame_size(&self, head: u32) -> Option<(u32, u32)> {
        (head == 0).then(|| self.size())
    }

    fn head_timing(&self, head: u32) -> Option<HeadTiming> {
        if head != 0 {
            return None;
        }
        let hz = display::refresh_hz(&self.gr.regs().vc1, &self.config.probe)?;
        let (_, h) = self.size();
        let frame_clocks = (display::TIMING_CLOCK_HZ / hz) as u64;
        let key = (hz as u64) << 32 | h as u64;
        let mut t = self.timing.lock();
        if t.0 != key {
            *t = (key, t.1 + 1);
        }
        // `lines` counts blanking, which is not known: 0 means no line total.
        Some(HeadTiming { frame_clocks, clock_hz: display::TIMING_CLOCK_HZ, fields: 1, lines: 0, epoch_ns: -1, generation: t.1 })
    }

    fn frame_generation(&self, head: u32) -> u64 {
        if head == 0 { self.generation_now() } else { 0 }
    }

    fn events_waiting(&self) -> Option<Arc<AtomicBool>> {
        Some(self.out.waiting())
    }

    fn advance(&mut self, now_ns: u64) -> Pump {
        self.time(now_ns);
        if self.fault_text().is_none() {
            self.sync_dirty();
        }
        self.out.rearm();
        Pump { idle: self.idle_state(), events: self.out.pending(), next_deadline: None, irq_levels: 0 }
    }

    fn wait_idle(&mut self, now_ns: u64, budget_ns: u64) -> Idle {
        self.time(now_ns);
        if self.faulted() {
            return Idle::Faulted;
        }
        if !self.parked {
            self.wait_for_idle(Duration::from_nanos(budget_ns.min(FOREVER_NS)));
        }
        self.idle_state()
    }

    fn irq_levels(&self) -> u64 {
        0
    }

    fn drain_events(&mut self, out: &mut Vec<Event>, max: usize) {
        self.fault_text();
        self.out.drain(out, max);
    }

    fn observe(&mut self, _region: &Rect) -> Result<(), Fault> {
        if let Some(f) = self.fault_text() {
            return Err(Fault(f));
        }
        self.wait_for_idle(OBSERVE_WAIT);
        Ok(())
    }

    fn observe_all(&mut self) -> Result<(), Refused> {
        self.need_parked()
    }

    fn quiesce(&mut self, budget_ns: u64) -> Result<(), Refused> {
        if !self.faulted() && !self.parked && !self.wait_for_idle(Duration::from_nanos(budget_ns.min(FOREVER_NS))) && !self.faulted() {
            return Err(Refused::Midway(match self.gr.stall_deadline() {
                Some(t) => format!("the board waits for the shell's time to reach {t} ns"),
                None => "the board was still drawing at the budget".into(),
            }));
        }
        self.park();
        match self.fault_text() {
            Some(f) => Err(Refused::Faulted(f)),
            None => Ok(()),
        }
    }

    fn resume(&mut self) {
        if self.parked {
            self.parked = false;
            self.gr.start_engine();
        }
    }

    fn save(&mut self, sink: &mut dyn StateSink) -> Result<(), Refused> {
        self.need_parked()?;
        super::state::save(&self.gr, sink).map_err(|e| Refused::Faulted(e.0))
    }

    fn load(&mut self, src: &mut dyn StateSource) -> Result<(), Refused> {
        self.need_parked()?;
        let fresh = new_board(&self.config, &self.out, &self.failure).map_err(Refused::Mismatch)?;
        super::state::load(&fresh, src)?;
        self.gr = fresh;
        self.picture = None;
        self.generation += 1;
        self.out.post(Event::Irq);
        Ok(())
    }

    fn state_version(&self) -> u32 {
        GR2_STATE_VERSION
    }

    fn reset(&mut self) -> Result<(), Refused> {
        if !self.parked {
            return Err(Refused::NotQuiesced);
        }
        self.gr = new_board(&self.config, &self.out, &self.failure).map_err(Refused::Mismatch)?;
        *self.failure.text.lock() = None;
        self.out.clear();
        self.picture = None;
        self.generation += 1;
        self.out.post(Event::Irq);
        Ok(())
    }

    fn fault(&self) -> Option<String> {
        self.failure.get().or_else(|| self.gr.panic_message())
    }

    fn fail(&self, why: &str) {
        self.failure.set(why, &self.out);
    }
}

impl<M: HostMem> Drop for Gr2Engine<M> {
    fn drop(&mut self) {
        self.gr.stop_engine();
    }
}
