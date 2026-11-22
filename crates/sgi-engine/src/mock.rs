//! A small inline board that uses every part of the interface, for the
//! tests of this crate and of the boards that implement it. It is a model of
//! the contract, not of any hardware.
//!
//! Stores queue as operations and run on `run` or `wait_idle`. `SWAP` holds
//! the stream until `frame`; `WAIT` holds it until `advance` reaches a time;
//! `FENCE` writes back at `advance` and only then raises its interrupt.
//! With `gpu_unit`, the first half of lent memory is trapped: operations
//! write a GPU copy, and `observe_lent` and `cpu_wrote` move words between it
//! and the lent memory.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::engine::*;
use crate::inject::{hit, Point};
use crate::mem::{DirtyBlocks, DirtyRun, HostMem, Lent, Swap, DIRTY_BLOCK};
use crate::outbox::Outbox;
use crate::state::{Chunk, StateSink, StateSource};

pub mod reg {
    /// Queued: add the value to the sum, and return a credit if charged.
    pub const DATA: u32 = 0x00;
    /// Immediate: set and clear interrupt line bits.
    pub const IRQ_SET: u32 = 0x04;
    pub const IRQ_CLR: u32 = 0x08;
    /// Queued: hold the stream until `frame`.
    pub const SWAP: u32 = 0x10;
    /// Queued: hold the stream until `advance` reaches the value, in ns.
    pub const WAIT: u32 = 0x14;
    /// Queued: store 8 bytes (`DMA_DATA`) at the bus address written.
    pub const DMA_WRITE: u32 = 0x18;
    pub const DMA_DATA: u32 = 0x1C;
    /// Queued: load 8 bytes from the bus address written, for `DMA_RESULT`.
    pub const DMA_READ: u32 = 0x24;
    /// Queued: write the value back at `advance`, then send a packet for line 3.
    pub const FENCE: u32 = 0x20;
    /// Queued: send an interrupt packet that sets, or clears, the line of the vector the value names.
    pub const PACKET: u32 = 0x28;
    pub const PACKET_CLR: u32 = 0x3C;
    /// Queued: panic when it runs.
    pub const BOOM: u32 = 0x2C;
    /// Queued: publish the 2 or 4 bytes of `DMA_DATA` at the bus address written, as one write.
    pub const PUBLISH16: u32 = 0x40;
    pub const PUBLISH32: u32 = 0x44;
    /// Queued: stop the stream for good, as a fetch fault stops a command processor.
    pub const STOP: u32 = 0x48;
    /// Immediate: the guest's own reset of the stream, which starts it again.
    pub const STREAM_RESET: u32 = 0x4C;
    /// Reads: wait for queued work. The sum, and the last DMA read.
    pub const SUM: u32 = 0x30;
    pub const DMA_RESULT: u32 = 0x34;
    /// Reads: the interrupt lines, never wait.
    pub const IRQ: u32 = 0x38;
    /// Queued: store, or add, the value in lent word `(off - base) / 8`.
    pub const LENT_STORE: u32 = 0x1_0000;
    pub const LENT_ADD: u32 = 0x2_0000;
}

pub const STATE_VERSION: u32 = 3;
const SHADOW_CHUNK: usize = 512;

pub struct MockConfig {
    pub queue_ops: usize,
    pub lent: Option<Lent>,
    pub gpu_unit: bool,
}

impl Default for MockConfig {
    fn default() -> Self {
        MockConfig { queue_ops: 64, lent: None, gpu_unit: false }
    }
}

enum Op {
    Data(u64, Source),
    Swap,
    Wait(u64),
    DmaWrite(u64, u32),
    DmaRead(u64),
    Publish(u64, u32, usize),
    Fence(u64, u32),
    Packet(u8, bool),
    Boom,
    Stop,
    LentStore(usize, u64),
    LentAdd(usize, u64),
}

pub struct Mock<M: HostMem> {
    cfg: MockConfig,
    mem: M,
    out: Outbox,
    queue: VecDeque<Op>,
    source: Source,
    dma_data: u32,
    held_swap: bool,
    /// Swaps consumed since the build or reset, and the shell time last given.
    swaps: u64,
    clock_ns: u64,
    stopped: bool,
    wait_until: Option<u64>,
    fences: Vec<(u64, u32)>,
    sum: u64,
    dma_last: u64,
    irq: AtomicU64,
    parked: bool,
    dirty: DirtyBlocks,
    shadow: Vec<u64>,
    fault: Mutex<Option<String>>,
    dead: AtomicBool,
    pub observed: Vec<(u64, u64)>,
    pub frames: u64,
    /// The times `write_at` was given, oldest first.
    pub write_times: Vec<u64>,
    /// Moves with every store, CPU write, load and reset (`frame_generation`).
    generation: u64,
}

impl<M: HostMem> Mock<M> {
    pub fn new(cfg: MockConfig, mem: M, notify: Option<Box<dyn Fn() + Send + Sync>>) -> Mock<M> {
        let len = cfg.lent.map_or(0, |l| l.len());
        let shadow = if cfg.gpu_unit { vec![0; (len / 8) as usize] } else { Vec::new() };
        Mock {
            cfg,
            mem,
            out: Outbox::new(notify),
            queue: VecDeque::new(),
            source: Source::default(),
            dma_data: 0,
            held_swap: false,
            swaps: 0,
            clock_ns: 0,
            stopped: false,
            wait_until: None,
            fences: Vec::new(),
            sum: 0,
            dma_last: 0,
            irq: AtomicU64::new(0),
            parked: false,
            dirty: DirtyBlocks::new(len),
            shadow,
            fault: Mutex::new(None),
            dead: AtomicBool::new(false),
            observed: Vec::new(),
            frames: 0,
            write_times: Vec::new(),
            generation: 0,
        }
    }

    fn dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    fn trapped_words(&self) -> usize {
        if self.cfg.gpu_unit { self.shadow.len() / 2 } else { 0 }
    }

    fn stall(&self) -> Option<StallOn> {
        if self.held_swap {
            Some(StallOn::Frame)
        } else {
            self.wait_until.map(StallOn::Time)
        }
    }

    fn idle(&self) -> Idle {
        if self.dead() {
            Idle::Faulted
        } else if self.stopped {
            Idle::Stopped
        } else if self.queue.is_empty() {
            Idle::Yes
        } else {
            self.stall().map_or(Idle::Working, Idle::Stalled)
        }
    }

    fn bus_fault(&self, addr: u64, write: bool) {
        self.out.post(Event::BusFault { addr, write });
    }

    fn exec(&mut self, op: Op) {
        hit(Point::Exec);
        match op {
            Op::Data(v, src) => {
                self.sum = self.sum.wrapping_add(v);
                if src.charged {
                    self.out.post(Event::Credits { source: src.id, retired: 1 });
                }
            }
            Op::Swap => {
                self.held_swap = true;
                self.swaps += 1;
                self.out.post(Event::Swap { count: self.swaps, time_ns: self.clock_ns });
            }
            Op::Wait(t) => self.wait_until = Some(t),
            Op::DmaWrite(addr, data) => {
                if self.mem.ram_write(addr, &(data as u64).to_be_bytes(), Swap::None).is_err() {
                    self.bus_fault(addr, true);
                }
            }
            Op::Publish(addr, data, size) => {
                let bytes = data.to_be_bytes();
                if self.mem.ram_write(addr, &bytes[4 - size..], Swap::None).is_err() {
                    self.bus_fault(addr, true);
                }
            }
            Op::DmaRead(addr) => {
                let mut b = [0u8; 8];
                match self.mem.ram_read(addr, &mut b, Swap::None) {
                    Ok(()) => self.dma_last = u64::from_be_bytes(b),
                    Err(_) => self.bus_fault(addr, false),
                }
            }
            Op::Fence(addr, val) => self.fences.push((addr, val)),
            Op::Packet(v, level) => self.out.post(Event::Interrupt(Interrupt { sender: 7, widget: 8, addr: 0x1000, vector: v, level })),
            Op::Boom => panic!("the mock board was told to"),
            Op::Stop => {
                self.stopped = true;
                self.queue.clear();
            }
            Op::LentStore(w, v) => self.lent_put(w, v),
            Op::LentAdd(w, v) => {
                let old = self.lent_get(w);
                self.lent_put(w, old.wrapping_add(v));
            }
        }
    }

    fn lent_get(&self, w: usize) -> u64 {
        if w < self.trapped_words() { self.shadow[w] } else { self.cfg.lent.map_or(0, |l| l.load(w)) }
    }

    fn lent_put(&mut self, w: usize, v: u64) {
        if w < self.trapped_words() {
            self.shadow[w] = v;
        } else if let Some(l) = self.cfg.lent {
            l.store(w, v);
            self.dirty.mark(w as u64 * 8, 8);
        }
    }

    fn words_of(&self, off: u64, len: u64) -> std::ops::Range<usize> {
        let end = ((off + len).div_ceil(8) as usize).min(self.trapped_words());
        (off / 8) as usize..end.max((off / 8) as usize)
    }

    fn need_parked(&self, healthy: bool) -> Result<(), Refused> {
        match self.fault() {
            Some(f) if healthy => return Err(Refused::Faulted(f)),
            _ => {}
        }
        if !self.parked || !self.queue.is_empty() {
            return Err(Refused::NotQuiesced);
        }
        Ok(())
    }

    fn room(&self, n: usize) -> bool {
        self.queue.len() + n <= self.cfg.queue_ops
    }

    fn push(&mut self, op: Op) {
        if self.queue.is_empty() {
            self.out.wake();
        }
        self.queue.push_back(op);
    }

    fn meta(&self) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend(STATE_VERSION.to_le_bytes());
        m.extend(self.sum.to_le_bytes());
        m.extend(self.irq.load(Ordering::Acquire).to_le_bytes());
        m
    }
}

impl<M: HostMem> BoardEngine for Mock<M> {
    type Region = (u32, u32);

    fn write(&mut self, off: u32, bits: u32, val: u64) -> Write {
        if self.dead() || !matches!(bits, 32 | 64) {
            return Write::Fault;
        }
        self.generation += 1;
        match off {
            reg::IRQ_SET | reg::IRQ_CLR => {
                if off == reg::IRQ_SET {
                    self.irq.fetch_or(val, Ordering::AcqRel);
                } else {
                    self.irq.fetch_and(!val, Ordering::AcqRel);
                }
                self.out.post(Event::Irq);
                return Write::Done;
            }
            reg::DMA_DATA => {
                self.dma_data = val as u32;
                return Write::Done;
            }
            reg::STREAM_RESET => {
                self.stopped = false;
                return Write::Done;
            }
            _ => {}
        }
        if self.stopped {
            return Write::Done;
        }
        if !self.room(1) {
            return Write::Busy;
        }
        let op = match off {
            reg::DATA => Op::Data(val, self.source),
            reg::SWAP => Op::Swap,
            reg::WAIT => Op::Wait(val),
            reg::DMA_WRITE => Op::DmaWrite(val, self.dma_data),
            reg::DMA_READ => Op::DmaRead(val),
            reg::PUBLISH16 => Op::Publish(val, self.dma_data, 2),
            reg::PUBLISH32 => Op::Publish(val, self.dma_data, 4),
            reg::FENCE => Op::Fence(val, self.dma_data),
            reg::PACKET => Op::Packet(val as u8, true),
            reg::PACKET_CLR => Op::Packet(val as u8, false),
            reg::BOOM => Op::Boom,
            reg::STOP => Op::Stop,
            o if (reg::LENT_STORE..reg::LENT_ADD).contains(&o) => Op::LentStore(((o - reg::LENT_STORE) / 8) as usize, val),
            o if o >= reg::LENT_ADD => Op::LentAdd(((o - reg::LENT_ADD) / 8) as usize, val),
            _ => return Write::Fault,
        };
        self.push(op);
        Write::Done
    }

    fn write_at(&mut self, now_ns: u64, off: u32, bits: u32, val: u64) -> Write {
        self.clock_ns = now_ns;
        self.write_times.push(now_ns);
        self.write(off, bits, val)
    }

    fn heads(&self) -> u32 {
        2
    }

    fn busy_ns(&self) -> u64 {
        12_345
    }

    fn swaps_dropped(&self) -> u64 {
        self.out.swaps_dropped()
    }

    fn events_waiting(&self) -> Option<Arc<AtomicBool>> {
        Some(self.out.waiting())
    }

    fn head_bits(&self, head: u32) -> u32 {
        if head == 1 { 10 } else { 8 }
    }

    fn head_timing(&self, head: u32) -> Option<HeadTiming> {
        // Head 1 runs two fields; head 0 has none loaded.
        (head == 1).then_some(HeadTiming { frame_clocks: 1_000_000, clock_hz: 60_000_000, fields: 2, lines: 525, epoch_ns: -1, generation: 3 })
    }

    fn frame_size(&self, head: u32) -> Option<(u32, u32)> {
        (head < 2).then_some((4, 2))
    }

    fn frame_generation(&self, head: u32) -> u64 {
        if head < 2 { self.generation } else { 0 }
    }

    /// `dst` is a `Mutex<Vec<u32>>` standing in for the GPU buffer.
    fn compose_into(&mut self, head: u32, dst: &dyn std::any::Any, stride_words: u32) -> Result<Option<(u32, u32)>, Fault> {
        if let Some(f) = self.fault() {
            return Err(Fault(f));
        }
        if head >= 2 {
            return Ok(None);
        }
        let Some(buf) = dst.downcast_ref::<Mutex<Vec<u32>>>() else { return Err(Fault("not a mock buffer".into())) };
        let mut buf = buf.lock();
        if stride_words < 4 || buf.len() < (stride_words * 2) as usize {
            return Err(Fault("a buffer too small for the frame".into()));
        }
        for y in 0..2u32 {
            for x in 0..4u32 {
                buf[(y * stride_words + x) as usize] = (head + 1) << 16 | (y * 4 + x);
            }
        }
        Ok(Some((4, 2)))
    }

    /// As `compose_into`, in `XB30` words.
    fn compose_into10(&mut self, head: u32, dst: &dyn std::any::Any, stride_words: u32) -> Result<Option<(u32, u32)>, Fault> {
        if let Some(f) = self.fault() {
            return Err(Fault(f));
        }
        if head >= 2 {
            return Ok(None);
        }
        let Some(buf) = dst.downcast_ref::<Mutex<Vec<u32>>>() else { return Err(Fault("not a mock buffer".into())) };
        let mut buf = buf.lock();
        if stride_words < 4 || buf.len() < (stride_words * 2) as usize {
            return Err(Fault("a buffer too small for the frame".into()));
        }
        for y in 0..2u32 {
            for x in 0..4u32 {
                let p = (head + 1) << 16 | (y * 4 + x);
                let widen = |c: u32| (c << 2) | (c >> 6);
                buf[(y * stride_words + x) as usize] = widen(p & 0xff) << 20 | widen(p >> 8 & 0xff) << 10 | widen(p >> 16 & 0xff);
            }
        }
        Ok(Some((4, 2)))
    }

    fn scanout(&mut self, head: u32, out: &mut Vec<u32>) -> Result<Option<(u32, u32)>, Fault> {
        if let Some(f) = self.fault() {
            return Err(Fault(f));
        }
        out.clear();
        if head >= 2 {
            return Ok(None);
        }
        out.extend((0..4 * 2).map(|i| (head + 1) << 16 | i));
        Ok(Some((4, 2)))
    }

    fn head_deadline(&self, head: u32) -> Option<u64> {
        (head < 2).then_some(1_000 * (head as u64 + 1))
    }

    fn push_words(&mut self, _port: u32, words: &[u32]) -> usize {
        if self.dead() {
            return 0;
        }
        let take = words.len().min(self.cfg.queue_ops - self.queue.len().min(self.cfg.queue_ops));
        for &w in &words[..take] {
            self.push(Op::Data(w as u64, self.source));
        }
        take
    }

    fn read(&mut self, off: u32, _bits: u32) -> Read {
        if self.dead() {
            return Read::Fault;
        }
        match off {
            reg::IRQ => Read::Value(self.irq.load(Ordering::Acquire)),
            reg::SUM | reg::DMA_RESULT if !self.queue.is_empty() => Read::Busy,
            reg::SUM => Read::Value(self.sum),
            reg::DMA_RESULT => Read::Value(self.dma_last),
            _ => Read::Fault,
        }
    }

    fn set_source(&mut self, source: Source) {
        self.source = source;
    }

    fn access_waits(&self, off: u32, _bits: u32, write: bool) -> bool {
        if self.dead() {
            return false;
        }
        if write {
            !matches!(off, reg::IRQ_SET | reg::IRQ_CLR | reg::DMA_DATA) && !self.room(1)
        } else {
            matches!(off, reg::SUM | reg::DMA_RESULT) && !self.queue.is_empty()
        }
    }

    fn run(&mut self, max_ops: usize) -> usize {
        let mut ran = 0;
        while ran < max_ops && !self.dead() && self.stall().is_none() {
            let Some(op) = self.queue.pop_front() else { break };
            self.exec(op);
            ran += 1;
        }
        ran
    }

    fn frame(&mut self, now_ns: u64) {
        self.clock_ns = now_ns;
        self.frames += 1;
        self.held_swap = false;
    }

    fn advance(&mut self, now_ns: u64) -> Pump {
        self.clock_ns = now_ns;
        self.out.rearm();
        if self.dead() {
            return Pump { idle: Idle::Faulted, events: self.out.pending(), next_deadline: None, irq_levels: self.irq_levels() };
        }
        if self.wait_until.is_some_and(|t| now_ns >= t) {
            self.wait_until = None;
        }
        for (addr, val) in std::mem::take(&mut self.fences) {
            if self.mem.ram_write(addr, &(val as u64).to_be_bytes(), Swap::None).is_err() {
                self.bus_fault(addr, true);
            }
            self.out.post(Event::Interrupt(Interrupt { sender: 7, widget: 8, addr: 0x2000, vector: 3, level: true }));
        }
        Pump { idle: self.idle(), events: self.out.pending(), next_deadline: self.wait_until, irq_levels: self.irq_levels() }
    }

    fn wait_idle(&mut self, now_ns: u64, _budget_ns: u64) -> Idle {
        self.clock_ns = now_ns;
        self.run(usize::MAX);
        self.idle()
    }

    fn irq_levels(&self) -> u64 {
        self.irq.load(Ordering::Acquire)
    }

    fn drain_events(&mut self, out: &mut Vec<Event>, max: usize) {
        self.out.drain(out, max);
    }

    fn drain_dirty(&mut self, out: &mut Vec<DirtyRun>) {
        self.dirty.drain(out);
    }

    fn trapped(&self, out: &mut Vec<DirtyRun>) {
        let len = (self.trapped_words() as u64 * 8) / DIRTY_BLOCK * DIRTY_BLOCK;
        if len != 0 {
            out.push(DirtyRun { offset: 0, len });
        }
    }

    fn observe_lent(&mut self, off: u64, len: u64) -> Result<(), Fault> {
        if let Some(f) = self.fault() {
            return Err(Fault(f));
        }
        self.run(usize::MAX);
        let Some(l) = self.cfg.lent else { return Ok(()) };
        for w in self.words_of(off, len) {
            l.store(w, self.shadow[w]);
        }
        self.dirty.mark(off, len);
        Ok(())
    }

    fn cpu_wrote(&mut self, off: u64, len: u64) {
        self.generation += 1;
        let Some(l) = self.cfg.lent else { return };
        for w in self.words_of(off, len) {
            self.shadow[w] = l.load(w);
        }
    }

    fn observe(&mut self, region: &(u32, u32)) -> Result<(), Fault> {
        self.run(usize::MAX);
        if let Some(f) = self.fault() {
            return Err(Fault(f));
        }
        self.observed.push((region.0 as u64, region.1 as u64));
        Ok(())
    }

    fn observe_all(&mut self) -> Result<(), Refused> {
        self.need_parked(true)
    }

    fn quiesce(&mut self, _budget_ns: u64) -> Result<(), Refused> {
        self.run(usize::MAX);
        if let Some(f) = self.fault() {
            self.parked = true;
            return Err(Refused::Faulted(f));
        }
        match self.stall() {
            Some(StallOn::Frame) if !self.queue.is_empty() => Err(Refused::Midway("work waits for a frame".into())),
            Some(StallOn::Time(t)) if !self.queue.is_empty() => Err(Refused::Midway(format!("work waits for {t} ns"))),
            _ => {
                self.parked = true;
                Ok(())
            }
        }
    }

    fn resume(&mut self) {
        self.parked = false;
    }

    fn save(&mut self, sink: &mut dyn StateSink) -> Result<(), Refused> {
        self.need_parked(true)?;
        if self.held_swap || self.wait_until.is_some() {
            return Err(Refused::Midway("a swap or a wait is held".into()));
        }
        let put = |sink: &mut dyn StateSink, name: &str, index: u64, bytes: Vec<u8>| sink.put(Chunk { name: name.into(), index, bytes }).map_err(|e| Refused::Faulted(e.0));
        put(sink, "meta", 0, self.meta())?;
        for (i, w) in self.shadow.chunks(SHADOW_CHUNK).enumerate().filter(|(_, w)| w.iter().any(|&x| x != 0)) {
            put(sink, "shadow", i as u64, w.iter().flat_map(|x| x.to_le_bytes()).collect())?;
        }
        Ok(())
    }

    fn load(&mut self, src: &mut dyn StateSource) -> Result<(), Refused> {
        self.generation += 1;
        self.need_parked(true)?;
        let bad = |m: String| Refused::Mismatch(m);
        let mut first = true;
        while let Some(c) = src.next().map_err(|e| bad(e.0))? {
            if std::mem::take(&mut first) && c.name != "meta" {
                return Err(bad("state does not start with meta".into()));
            }
            match c.name.as_str() {
                "meta" => {
                    let b = &c.bytes;
                    if b.len() != 20 || u32::from_le_bytes(b[..4].try_into().unwrap()) != STATE_VERSION {
                        return Err(bad("meta is not this version".into()));
                    }
                    self.sum = u64::from_le_bytes(b[4..12].try_into().unwrap());
                    self.irq.store(u64::from_le_bytes(b[12..20].try_into().unwrap()), Ordering::Release);
                    self.shadow.fill(0);
                }
                "shadow" => {
                    let at = c.index as usize * SHADOW_CHUNK;
                    if c.bytes.len() != SHADOW_CHUNK * 8 || at + SHADOW_CHUNK > self.shadow.len() {
                        return Err(bad(format!("shadow chunk {} does not fit", c.index)));
                    }
                    for (d, b) in self.shadow[at..at + SHADOW_CHUNK].iter_mut().zip(c.bytes.chunks_exact(8)) {
                        *d = u64::from_le_bytes(b.try_into().unwrap());
                    }
                }
                other => return Err(bad(format!("unknown state chunk {other}"))),
            }
        }
        if first {
            return Err(bad("state is empty".into()));
        }
        self.out.post(Event::Irq);
        Ok(())
    }

    fn state_version(&self) -> u32 {
        STATE_VERSION
    }

    fn reset(&mut self) -> Result<(), Refused> {
        if !self.queue.is_empty() {
            return Err(Refused::NotQuiesced);
        }
        self.generation += 1;
        self.held_swap = false;
        self.swaps = 0;
        self.stopped = false;
        self.wait_until = None;
        self.fences.clear();
        self.sum = 0;
        self.dma_last = 0;
        self.irq.store(0, Ordering::Release);
        self.shadow.fill(0);
        self.dirty.drain(&mut Vec::new());
        self.out.clear();
        *self.fault.lock() = None;
        self.dead.store(false, Ordering::Release);
        self.out.post(Event::Irq);
        Ok(())
    }

    fn fault(&self) -> Option<String> {
        self.fault.lock().clone()
    }

    fn fail(&self, why: &str) {
        let mut f = self.fault.lock();
        if f.is_none() {
            *f = Some(why.to_string());
            self.dead.store(true, Ordering::Release);
            self.out.post(Event::DeviceFault(why.to_string()));
        }
    }
}
