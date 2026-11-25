//! What the replay tests share: QEMU's MGRAS recordings (`tests/data/mgras`,
//! the `sgi-replay 2` format, cut from IRIX 6.5.30 sessions on QEMU
//! `505b2e597dc`) and an Octane HQ4 front reduced to what a command stream
//! needs, which routes recorded stores to the engine.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::ThreadId;

use sgi_mgras::dev::mgras::engine::{Board, Config, MgrasEngine, Mode};
use sgi_mgras::dev::mgras::hq3::host;
use sgi_engine::{BoardEngine, Event, HostMem, Idle, MemFault, Read, Swap, Write};
use parking_lot::Mutex;

macro_rules! replays {
    ($($n:literal),*) => { &[$(($n, include_str!(concat!("../data/mgras/", $n, ".replay")))),*] };
}

/// The recordings that carry writes.
pub const REPLAYS: &[(&str, &str)] = replays!(
    "char-modes",
    "cmap-high-byte",
    "cmap-xmap-wid",
    "dma-image-transfer",
    "fill",
    "gl-deep-swap-clock-ssi",
    "gl-eviction-i3dm",
    "gl-new-context-slot-reuse",
    "gl-pixels-rectwrite",
    "gl-swap-tmesh-sphere",
    "gl-texture-rgba4-quad",
    "overlay",
    "pio-read-oglsnoop",
    "root-icon"
);

/// Host memory a replay preloads (`dma` lines); everything else reads 0.
/// Counts which threads used the RAM path and how often the bus path ran.
#[derive(Default)]
pub struct Sparse {
    bytes: Mutex<HashMap<u64, u8>>,
    pub bus_calls: AtomicU64,
    pub ram_threads: Mutex<HashSet<ThreadId>>,
    /// Addresses at and above this are not RAM.
    pub ram_limit: Mutex<Option<u64>>,
    /// Writes reach no RAM.
    pub fail_writes: std::sync::atomic::AtomicBool,
}

#[derive(Clone, Default)]
pub struct Mem(pub Arc<Sparse>);

impl Mem {
    pub fn copy_of(&self) -> Mem {
        let m = Mem::default();
        *m.0.bytes.lock() = self.0.bytes.lock().clone();
        m
    }

    pub fn put(&self, addr: u64, data: &[u8]) {
        let mut m = self.0.bytes.lock();
        for (i, b) in data.iter().enumerate() {
            m.insert(addr + i as u64, *b);
        }
    }

    pub fn get(&self, addr: u64, n: usize) -> Vec<u8> {
        let m = self.0.bytes.lock();
        (0..n as u64).map(|i| m.get(&(addr + i)).copied().unwrap_or(0)).collect()
    }
}

impl HostMem for Mem {
    fn ram_read(&self, addr: u64, out: &mut [u8], _: Swap) -> Result<(), MemFault> {
        self.0.ram_threads.lock().insert(std::thread::current().id());
        if self.0.ram_limit.lock().is_some_and(|l| addr + out.len() as u64 > l) {
            return Err(MemFault);
        }
        out.copy_from_slice(&self.get(addr, out.len()));
        Ok(())
    }
    fn ram_write(&self, addr: u64, data: &[u8], _: Swap) -> Result<(), MemFault> {
        self.0.ram_threads.lock().insert(std::thread::current().id());
        if self.0.fail_writes.load(Ordering::Relaxed) || self.0.ram_limit.lock().is_some_and(|l| addr + data.len() as u64 > l) {
            return Err(MemFault);
        }
        self.put(addr, data);
        Ok(())
    }
    fn bus_read(&self, addr: u64, out: &mut [u8], swap: Swap) -> Result<(), MemFault> {
        self.0.bus_calls.fetch_add(1, Ordering::Relaxed);
        self.ram_read(addr, out, swap)
    }
    fn bus_write(&self, addr: u64, data: &[u8], swap: Swap) -> Result<(), MemFault> {
        self.0.bus_calls.fetch_add(1, Ordering::Relaxed);
        self.ram_write(addr, data, swap)
    }
}

#[derive(Clone)]
pub enum Cmd {
    Write { bits: u32, off: u32, val: u64 },
    Read { bits: u32, off: u32, val: u64, mask: u64 },
    Dma { addr: u64, bytes: Vec<u8> },
}

pub struct Replay {
    pub head: String,
    pub cmds: Vec<Cmd>,
}

fn num(s: &str) -> u64 {
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).unwrap(),
        None => s.parse().unwrap(),
    }
}

pub fn parse(text: &str) -> Replay {
    let mut head = String::new();
    let mut cmds = Vec::new();
    for l in text.lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        if w.is_empty() || w[0].starts_with('#') {
            continue;
        }
        match w[0] {
            "head" => head = w[1].to_string(),
            "write" => cmds.push(Cmd::Write { bits: num(w[1]) as u32 * 8, off: num(w[2]) as u32, val: num(w[3]) }),
            "read" => cmds.push(Cmd::Read {
                bits: num(w[1]) as u32 * 8,
                off: num(w[2]) as u32,
                val: num(w[3]),
                mask: w.get(4).map_or(u64::MAX, |m| num(m)),
            }),
            "dma" => {
                let hex = w[3].as_bytes();
                let bytes = (0..hex.len() / 2).map(|i| u8::from_str_radix(std::str::from_utf8(&hex[2 * i..2 * i + 2]).unwrap(), 16).unwrap()).collect();
                cmds.push(Cmd::Dma { addr: num(w[1]), bytes });
            }
            // Pixel and scan-out checks need the display chips and QEMU's
            // own planes; the engine is compared with itself.
            _ => {}
        }
    }
    Replay { head, cmds }
}

pub fn board_of(head: &str) -> Board {
    match head {
        "sgi-mgras-si-t" => Board::High,
        "sgi-mgras-ssi" => Board::Max,
        _ => Board::Solid,
    }
}

/// The HQ4 window routing and context-switch handshake of iris's `hq4.rs`,
/// reduced to what a command stream needs.
#[derive(Clone)]
pub struct Front {
    context_word_written: bool,
    entry_selected: bool,
    first_ack: bool,
    switching: bool,
    commands: bool,
    image_words: u32,
    pub fifo_words: u64,
}

const COMMAND: u32 = 0x120400;
const MAPPING: u32 = 0x130400;
const SELECT: u32 = 0x130500;
const FIFO: u32 = 0x020400;
const CONTEXT_WORD: u32 = 0x011000;
const ACK: u32 = 0x011028;
const FLAGS_BIT: u32 = 1 << 19;

/// Write, driving the engine when its queue is full.
pub fn put<E: BoardEngine>(e: &mut E, off: u32, bits: u32, val: u64) {
    loop {
        match e.write(off, bits, val) {
            Write::Done => return,
            Write::Busy => drive(e),
            Write::Fault => panic!("engine fault at write {off:#x}"),
        }
    }
}

/// Run the queue (inline) or wait for the worker (threaded).
pub fn drive<E: BoardEngine>(e: &mut E) {
    e.run(usize::MAX);
    assert_eq!(e.wait_idle(0, 60_000_000_000), Idle::Yes, "the engine did not go idle");
}

/// Read, driving the engine while the answer waits on queued work.
pub fn get<E: BoardEngine>(e: &mut E, off: u32, bits: u32) -> u64 {
    loop {
        match e.read(off, bits) {
            Read::Value(v) => return v,
            Read::Busy => drive(e),
            Read::Fault => panic!("engine fault at read {off:#x}"),
        }
    }
}

impl Front {
    pub fn new() -> Self {
        Front { context_word_written: false, entry_selected: false, first_ack: false, switching: false, commands: false, image_words: 0, fifo_words: 0 }
    }

    fn whole_packet(word: u64) -> bool {
        let h = (word >> 32) as u32;
        h & 0x8000_0000 == 0 && h & 0xFF == 4
    }

    fn fifo<E: BoardEngine>(&mut self, e: &mut E, bits: u32, val: u64) {
        self.fifo_words += (bits / 32) as u64;
        put(e, host::CFIFO, bits, val);
    }

    pub fn write<E: BoardEngine>(&mut self, e: &mut E, bits: u32, off: u32, val: u64) {
        match off {
            COMMAND | FIFO => {
                if off == FIFO || self.commands {
                    return self.fifo(e, bits, val);
                }
                if bits == 32 && self.entry_selected && self.switching && self.first_ack && self.image_words < 63 {
                    self.fifo(e, 32, val);
                    self.image_words += 1;
                    if self.image_words == 63 {
                        self.switching = false;
                        self.commands = true;
                    }
                } else if bits == 64 && !self.entry_selected && Self::whole_packet(val) {
                    self.fifo(e, bits, val);
                }
            }
            MAPPING => {
                self.fifo_words += (bits / 32) as u64;
                put(e, host::CFIFO_PRIVILEGED, bits, val);
            }
            SELECT => {
                if bits == 32 && val == 0x4FC && self.context_word_written {
                    put(e, host::CONTEXT_SWITCH, 32, 0);
                    self.entry_selected = true;
                    self.switching = true;
                    self.commands = false;
                    self.first_ack = false;
                    self.image_words = 0;
                }
            }
            CONTEXT_WORD => self.context_word_written = true,
            ACK => {
                if val as u32 & FLAGS_BIT != 0 {
                    self.first_ack = true;
                }
                if val as u32 & (1 << 10) != 0 {
                    // A guest clears a flag after seeing it set, so the work
                    // that sets it has run.
                    drive(e);
                    put(e, host::CLEAR_FLAGS, 32, host::FLAG_CP0 as u64);
                }
            }
            0x02C000..=0x02FFFF => put(e, host::RASTER + (off - 0x02C000), bits, val),
            0x020010 | 0x020018 => {
                drive(e);
                put(e, if off == 0x020010 { host::SET_FLAGS } else { host::CLEAR_FLAGS }, 32, val & !(1 << 6 | 1 << 10 | 1 << 19));
            }
            0x050000..=0x054FFF => put(e, host::UCODE + (off - 0x050000), bits, val),
            _ => {}
        }
    }
}

/// How often the inline engine runs its queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drive {
    /// After every recorded command.
    Each,
    /// Only when a write is refused or a read waits, and at the end.
    Lazy,
}

/// What a run leaves, to compare between modes and splits.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub hash: u64,
    pub flags: u32,
    pub fifo_words: u64,
    pub hq3_words: u64,
    pub raster_writes: u64,
    /// Every compared read's value, in order.
    pub reads: Vec<(u32, u64)>,
    pub notes: Vec<String>,
    /// The box drawn in each store.
    pub damage: BTreeMap<u64, (i32, i32, i32, i32)>,
}

pub struct Player {
    pub e: MgrasEngine<Mem>,
    pub mem: Mem,
    front: Front,
    reads: Vec<(u32, u64)>,
    notes: Vec<String>,
    damage: BTreeMap<u64, (i32, i32, i32, i32)>,
    drive: Drive,
    /// The DMA faults the engine reported, address and direction.
    pub bus_faults: Vec<(u64, bool)>,
    /// The damage marks seen since the caller last cleared them.
    pub fresh: Vec<sgi_engine::Mark>,
}

pub fn config(rep_head: &str, mode: Mode, jit: Option<u32>) -> Config {
    let mut cfg = Config::new(board_of(rep_head), mode);
    cfg.octane = true;
    cfg.queue_ops = 4096;
    #[cfg(feature = "gr4-jit")]
    {
        cfg.jit = jit.or(Some(sgi_mgras::dev::mgras::rss_jit::MODE_OFF));
    }
    #[cfg(not(feature = "gr4-jit"))]
    let _ = jit;
    cfg
}

impl Player {
    pub fn new(cfg: Config, drive: Drive) -> Player {
        Player::on(cfg, drive, None)
    }

    /// As `new`, drawing on `gpu`.
    pub fn on(cfg: Config, drive: Drive, gpu: Option<&Arc<sgi_mgras::wgpu::Gpu>>) -> Player {
        let mem = Mem::default();
        let e = MgrasEngine::new(cfg, mem.clone(), gpu, None).expect("engine");
        Player { e, mem, front: Front::new(), reads: Vec::new(), notes: Vec::new(), damage: BTreeMap::new(), drive, bus_faults: Vec::new(), fresh: Vec::new() }
    }

    /// A second player over a fresh engine, with this one's guest-side
    /// state: the front's handshake, the host memory and what was read.
    pub fn twin(&self, cfg: Config) -> Player {
        let mem = self.mem.copy_of();
        let e = MgrasEngine::new(cfg, mem.clone(), None, None).expect("engine");
        Player { e, mem, front: self.front.clone(), reads: self.reads.clone(), notes: self.notes.clone(), damage: self.damage.clone(), drive: self.drive, bus_faults: Vec::new(), fresh: Vec::new() }
    }

    pub fn apply(&mut self, c: &Cmd) {
        match c {
            Cmd::Write { bits, off, val } => self.front.write(&mut self.e, *bits, *off, *val),
            Cmd::Dma { addr, bytes } => self.mem.put(*addr, bytes),
            Cmd::Read { bits, off, mask, .. } => {
                let hq3 = match *off {
                    0x028000 => Some(host::GE_READBACK_HI),
                    0x028004 => Some(host::GE_READBACK_LO),
                    0x020000 => Some(host::STATUS),
                    0x020010 => Some(host::SET_FLAGS),
                    0x020200 => Some(host::DMABUSY),
                    0x02C000..=0x02FFFF => Some(host::RASTER + (*off - 0x02C000)),
                    _ => None,
                };
                if let Some(o) = hq3 {
                    // QEMU took the value with everything before it run; a
                    // guest polls a status or flag until it shows.
                    drive(&mut self.e);
                    let v = get(&mut self.e, o, *bits);
                    self.reads.push((o, v & mask));
                }
            }
        }
        if self.drive == Drive::Each {
            drive(&mut self.e);
        }
        self.collect();
    }

    fn collect(&mut self) {
        let mut ev = Vec::new();
        self.e.drain_events(&mut ev, usize::MAX);
        for x in ev {
            match x {
                Event::Note(t) => self.notes.push(t),
                Event::BusFault { addr, write } => self.bus_faults.push((addr, write)),
                Event::Damage(m) => {
                    self.fresh.push(m);
                    let r = &m.rect;
                    let d = self.damage.entry(m.store).or_insert((r.x0, r.y0, r.x1, r.y1));
                    *d = (d.0.min(r.x0), d.1.min(r.y0), d.2.max(r.x1), d.3.max(r.y1));
                }
                _ => {}
            }
        }
    }

    pub fn finish(&mut self) -> Outcome {
        drive(&mut self.e);
        self.collect();
        let hash = self.e.state_hash().expect("hash");
        let st = self.e.stats();
        let mut notes = std::mem::take(&mut self.notes);
        notes.sort();
        Outcome {
            hash,
            flags: self.e.flags(),
            fifo_words: self.front.fifo_words,
            hq3_words: st.words,
            raster_writes: st.raster_writes,
            reads: self.reads.clone(),
            notes,
            damage: self.damage.clone(),
        }
    }
}

pub fn play(rep: &Replay, mode: Mode, jit: Option<u32>, drive: Drive) -> Outcome {
    let mut p = Player::new(config(&rep.head, mode, jit), drive);
    for c in &rep.cmds {
        p.apply(c);
    }
    p.finish()
}

/// The hash of an engine that drew nothing.
pub fn empty_hash(head: &str) -> u64 {
    let mut p = Player::new(config(head, Mode::Inline, None), Drive::Each);
    p.e.state_hash().unwrap()
}
