//! Replays QEMU's MGRAS recordings (`tests/data/mgras/*.replay`, the
//! `sgi-replay 2` format) into the engine, the way an Octane's HQ4 front
//! would feed it, and reports what was drawn and what it cost.
//!
//! Usage: qemu_replay [--jit off|sync] [--runs N] [--queued] [--dump DIR] FILE...

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use iris_mgras::dev::mgras::engine::{Board, Config, Engine, Event, Mode, Read};
use iris_mgras::dev::mgras::hq3::host;
use iris_mgras::dev::mgras::pixmem::{Buffer, Kind};
use iris_mgras::dev::mgras::rss::reg;
use iris_mgras::dev::mgras::hostmem::{Fault, HostMem};

/// Host memory a replay can preload (`dma` lines); everything else reads 0.
struct Sparse(parking_lot::Mutex<HashMap<u64, u8>>);

impl HostMem for Sparse {
    fn read32(&self, a: u64) -> Option<u32> {
        let m = self.0.lock();
        let b = |i: u64| m.get(&(a + i)).copied().unwrap_or(0) as u32;
        Some(b(0) << 24 | b(1) << 16 | b(2) << 8 | b(3))
    }
    fn read(&self, a: u64, out: &mut [u8]) -> Result<(), Fault> {
        let m = self.0.lock();
        for (i, b) in out.iter_mut().enumerate() {
            *b = m.get(&(a + i as u64)).copied().unwrap_or(0);
        }
        Ok(())
    }
    fn write(&self, a: u64, data: &[u8]) -> Result<(), Fault> {
        let mut m = self.0.lock();
        for (i, b) in data.iter().enumerate() {
            m.insert(a + i as u64, *b);
        }
        Ok(())
    }
}

#[derive(Clone)]
enum Cmd {
    Write { bits: u32, off: u32, val: u64 },
    Read { bits: u32, off: u32, val: u64, mask: u64 },
    Pixel { x: u32, y: u32, val: u32, back: bool },
    Dma { addr: u64, bytes: Vec<u8> },
}

struct Replay {
    head: String,
    cmds: Vec<Cmd>,
}

fn num(s: &str) -> u64 {
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).unwrap(),
        None => s.parse().unwrap(),
    }
}

fn parse(text: &str) -> Replay {
    let mut head = String::new();
    let mut cmds = Vec::new();
    for l in text.lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        if w.is_empty() || w[0].starts_with('#') {
            continue;
        }
        match w[0] {
            "sgi-replay" => {}
            "head" => head = w[1].to_string(),
            "write" => cmds.push(Cmd::Write { bits: num(w[1]) as u32 * 8, off: num(w[2]) as u32, val: num(w[3]) }),
            "read" => cmds.push(Cmd::Read {
                bits: num(w[1]) as u32 * 8,
                off: num(w[2]) as u32,
                val: num(w[3]),
                mask: w.get(4).map_or(u64::MAX, |m| num(m)),
            }),
            "pixel" | "back-pixel" => cmds.push(Cmd::Pixel { x: num(w[1]) as u32, y: num(w[2]) as u32, val: num(w[3]) as u32, back: w[0] == "back-pixel" }),
            "pixels" => {
                let (x, y, w_, h, v) = (num(w[1]) as u32, num(w[2]) as u32, num(w[3]) as u32, num(w[4]) as u32, num(w[5]) as u32);
                for dy in 0..h {
                    for dx in 0..w_ {
                        cmds.push(Cmd::Pixel { x: x + dx, y: y + dy, val: v, back: false });
                    }
                }
            }
            "dma" => {
                let hex = w[3].as_bytes();
                let bytes = (0..hex.len() / 2)
                    .map(|i| u8::from_str_radix(std::str::from_utf8(&hex[2 * i..2 * i + 2]).unwrap(), 16).unwrap())
                    .collect();
                cmds.push(Cmd::Dma { addr: num(w[1]), bytes });
            }
            // Scan-out and plane checks need the display chips, which are
            // not in the engine.
            _ => {}
        }
    }
    Replay { head, cmds }
}

/// The HQ4 window routing and context-switch handshake of iris's `hq4.rs`,
/// reduced to what a command stream needs.
struct Hq4Front {
    context_word_written: bool,
    entry_selected: bool,
    first_ack: bool,
    switching: bool,
    commands: bool,
    image_words: u32,
    stats: Stats,
}

#[derive(Default, Clone, Copy)]
struct Stats {
    fifo_words: u64,
    raster_writes: u64,
    local_writes: u64,
    dropped_words: u64,
    reads: u64,
}

const COMMAND: u32 = 0x120400;
const MAPPING: u32 = 0x130400;
const SELECT: u32 = 0x130500;
const FIFO: u32 = 0x020400;
const CONTEXT_WORD: u32 = 0x011000;
const ACK: u32 = 0x011028;
const FLAGS_BIT: u32 = 1 << 19;

impl Hq4Front {
    fn new() -> Self {
        Hq4Front { context_word_written: false, entry_selected: false, first_ack: false, switching: false, commands: false, image_words: 0, stats: Stats::default() }
    }

    fn whole_packet(word: u64) -> bool {
        let h = (word >> 32) as u32;
        h & 0x8000_0000 == 0 && h & 0xFF == 4
    }

    fn fifo(&mut self, e: &mut Engine, bits: u32, val: u64) {
        self.stats.fifo_words += (bits / 32) as u64;
        e.write(host::CFIFO, bits, val);
    }

    fn write(&mut self, e: &mut Engine, bits: u32, off: u32, val: u64) {
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
                } else {
                    self.stats.dropped_words += (bits / 32) as u64;
                }
            }
            MAPPING => {
                self.stats.fifo_words += (bits / 32) as u64;
                e.write(host::CFIFO_PRIVILEGED, bits, val);
            }
            SELECT => {
                if bits == 32 && val == 0x4FC && self.context_word_written {
                    e.write(host::CONTEXT_SWITCH, 32, 0);
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
                    e.write(host::CLEAR_FLAGS, 32, host::FLAG_CP0 as u64);
                }
            }
            0x02C000..=0x02FFFF => {
                self.stats.raster_writes += 1;
                e.write(host::RASTER + (off - 0x02C000), bits, val);
            }
            0x020010 | 0x020018 => e.write(if off == 0x020010 { host::SET_FLAGS } else { host::CLEAR_FLAGS }, 32, val & !(1 << 6 | 1 << 10 | 1 << 19)),
            0x050000..=0x054FFF => e.write(host::UCODE + (off - 0x050000), bits, val),
            _ => self.stats.local_writes += 1,
        }
    }
}

fn pixels(e: &Engine, page: u32, y_flip: bool, w: u32, h: u32) -> Vec<u32> {
    let drbsize = e.raster_reg(reg::DRBSIZE);
    let b = Buffer::new(page, Kind::Wide, drbsize);
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        let yy = if y_flip { h - 1 - y } else { y };
        for x in 0..w {
            v.push(e.pixel(&b, x, yy) as u32 & 0xFF_FFFF);
        }
    }
    v
}

fn plane_hash(e: &Engine) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (b, _) in e.touched().iter().filter(|(b, _)| b.kind == Kind::Wide) {
        let buf = Buffer::new(b.ptr, Kind::Wide, e.raster_reg(reg::DRBSIZE));
        for y in 0..1024 {
            for x in 0..1280 {
                h.write_u64(e.pixel(&buf, x, y));
            }
        }
    }
    h.finish()
}

fn run(path: &str, rep: &Replay, jit: Option<u32>, queued: bool, dump: Option<&str>, runs: u32) {
    let board = match rep.head.as_str() {
        "sgi-mgras-si-t" => Board::High,
        "sgi-mgras-ssi" => Board::Max,
        _ => Board::Solid,
    };
    let name = std::path::Path::new(path).file_stem().unwrap().to_string_lossy().to_string();
    let mut times = Vec::new();
    let mut last: Option<(Engine, Stats, Vec<Event>, Vec<String>)> = None;
    for _ in 0..runs {
        let cfg = Config {
            board,
            octane: true,
            mode: if queued { Mode::Queued } else { Mode::Inline },
            #[cfg(feature = "gr4-jit")]
            jit,
        };
        #[cfg(not(feature = "gr4-jit"))]
        let _ = jit;
        let mut e = Engine::new(cfg, None).expect("engine");
        let mem = Arc::new(Sparse(Default::default()));
        e.set_mem(mem.clone());
        let mut front = Hq4Front::new();
        let mut read_bad: Vec<String> = Vec::new();
        let t = Instant::now();
        for c in &rep.cmds {
            match c {
                Cmd::Write { bits, off, val } => front.write(&mut e, *bits, *off, *val),
                Cmd::Dma { addr, bytes } => {
                    mem.write(*addr, bytes).unwrap();
                }
                Cmd::Read { bits, off, val, mask } => {
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
                        let got = match e.read(o, *bits) {
                            Read::Value(v) => v,
                            Read::Queued(_) => unreachable!("run() drains each command"),
                        };
                        front.stats.reads += 1;
                        if (got ^ val) & mask != 0 {
                            read_bad.push(format!("read {off:#x} got {got:#x} expected {val:#x} mask {mask:#x}"));
                        }
                    }
                }
                Cmd::Pixel { .. } => {}
            }
            if queued {
                e.advance(0, usize::MAX);
            }
        }
        e.advance(0, usize::MAX);
        let dt = t.elapsed();
        e.sync().expect("sync");
        times.push(dt.as_secs_f64());
        let mut ev = Vec::new();
        e.drain_events(&mut ev);
        last = Some((e, front.stats, ev, read_bad));
    }
    let (e, st, ev, read_bad) = last.unwrap();
    println!("  state reads compared: {} mismatched: {}", st.reads, read_bad.len());
    for r in read_bad.iter().take(10) {
        println!("    {r}");
    }
    let first = times[0];
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = times[times.len() / 2];
    let words = st.fifo_words.max(1);
    println!(
        "{name}: head {} fifo_words {} raster_writes {} dropped_words {} runs {} time_ms first {:.3} min {:.3} med {:.3} max {:.3} ns_per_fifo_word first {:.0} min {:.0} median {:.0} hq3_words {} raster_writes_by_hq {}",
        rep.head,
        st.fifo_words,
        st.raster_writes,
        st.dropped_words,
        runs,
        first * 1e3,
        times[0] * 1e3,
        med * 1e3,
        times[times.len() - 1] * 1e3,
        first * 1e9 / words as f64,
        times[0] * 1e9 / words as f64,
        med * 1e9 / words as f64,
        e.stats().words,
        e.stats().raster_writes,
    );
    if std::env::var_os("DESCRIBE").is_some() {
        println!("{}", e.describe());
    }
    println!("  plane hash {:016x} mode {}", plane_hash(&e), if queued { "queued" } else { "inline" });
    #[cfg(feature = "gr4-jit")]
    println!("  jit hits/misses/declined {:?}", e.jit_counts());
    println!("  execs {} swaps {} ctx_switches {} pixel_cmds {} other_cmds {}", e.stats().raster_execs, e.stats().swaps, e.stats().context_switches, e.stats().pixel_cmds, e.stats().other_cmds);
    let notes: Vec<&Event> = ev.iter().filter(|x| matches!(x, Event::Note(_))).collect();
    println!("  notes: {} damage_events: {} flags_events: {} flags {:#x}", notes.len(), ev.iter().filter(|x| matches!(x, Event::Damage)).count(), ev.iter().filter(|x| matches!(x, Event::Flags)).count(), e.flags());
    for n in notes.iter().take(40) {
        if let Event::Note(t) = n {
            println!("    {t}");
        }
    }
    // QEMU's recorded pixel expectations against every wide buffer drawn
    // into, in the orientations and decodings a 36-bit word allows. QEMU's
    // values are 0x00BBGGRR from its own planes (a model, not hardware).
    let checks: Vec<(u32, u32, u32)> = rep.cmds.iter().filter_map(|c| if let Cmd::Pixel { x, y, val, .. } = c { Some((*x, *y, *val)) } else { None }).collect();
    if !checks.is_empty() {
        let n444 = |n: u32| (((n >> 8) & 15) * 17) << 16 | (((n >> 4) & 15) * 17) << 8 | (n & 15) * 17;
        let near = |a: u32, b: u32| (0..3).all(|i| ((a >> (8 * i)) & 255).abs_diff((b >> (8 * i)) & 255) <= 17);
        let mut best: Option<(usize, String)> = None;
        for (buf, _) in e.touched().iter().filter(|(b, _)| b.kind == Kind::Wide) {
            let b = Buffer::new(buf.ptr, Kind::Wide, e.raster_reg(reg::DRBSIZE));
            for flip in [false, true] {
                for (mode, f) in [("raw24", 0), ("lo444", 1), ("hi444", 2)] {
                    let (mut exact, mut close) = (0, 0);
                    for &(x, y, v) in &checks {
                        let w = e.pixel(&b, x, if flip { 1023 - y } else { y }) as u32 & 0xFF_FFFF;
                        let w = match f { 0 => w, 1 => n444(w & 0xFFF), _ => n444(w >> 12) };
                        exact += (w == v) as usize;
                        close += near(w, v) as usize;
                    }
                    let line = format!("page {:#x} flip {flip} {mode}: exact {exact} within17 {close} of {}", buf.ptr, checks.len());
                    if best.as_ref().map_or(true, |(c, _)| close > *c) {
                        best = Some((close, line));
                    }
                }
            }
        }
        match best {
            Some((_, l)) => println!("  pixel checks, best: {l}"),
            None => println!("  pixel checks: {} expected, no wide buffer drawn", checks.len()),
        }
    }
    for (b, r) in e.touched() {
        println!("  touched {:?} page {:#x} box ({},{})-({},{})", b.kind, b.ptr, r.x0, r.y0, r.x1, r.y1);
    }
    if let Some(dir) = dump {
        for (b, _) in e.touched().iter().filter(|(b, _)| b.kind == Kind::Wide) {
            let px = pixels(&e, b.ptr, true, 1280, 1024);
            let mut out = format!("P6\n1280 1024\n255\n").into_bytes();
            for p in px {
                out.extend_from_slice(&[(p & 0xFF) as u8, (p >> 8 & 0xFF) as u8, (p >> 16 & 0xFF) as u8]);
            }
            std::fs::write(format!("{dir}/{name}.{:x}.ppm", b.ptr), out).unwrap();
        }
    }
}

fn main() {
    let mut jit: Option<u32> = None;
    let mut runs = 1;
    let mut queued = false;
    let mut dump: Option<String> = None;
    let mut files = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--jit" => {
                #[cfg(feature = "gr4-jit")]
                {
                    use iris_mgras::dev::mgras::rss_jit::{MODE_OFF, MODE_SYNC};
                    jit = Some(if args.next().unwrap() == "sync" { MODE_SYNC } else { MODE_OFF });
                }
                #[cfg(not(feature = "gr4-jit"))]
                panic!("built without gr4-jit");
            }
            "--runs" => runs = args.next().unwrap().parse().unwrap(),
            "--queued" => queued = true,
            "--dump" => dump = args.next(),
            f => files.push(f.to_string()),
        }
    }
    for f in &files {
        let rep = parse(&std::fs::read_to_string(f).unwrap());
        run(f, &rep, jit, queued, dump.as_deref(), runs);
    }
}
