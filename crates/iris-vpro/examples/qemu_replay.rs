//! Replays a QEMU Odyssey recording (the `sgi-replay 2` format that
//! `analyze command-port-trace --head sgi-odyssey` writes) into the VPro
//! engine, inline, and reports what it cost and how the last frame compares.
//!
//! Usage: qemu_replay [--ns-per-record N] [--dump FILE.ppm] [--ref FILE.ppm] FILE.replay
//!
//! The recording has no time. A read that differs from the recorded value
//! moves the engine's clock on by one frame period and reads again, up to
//! four times, so retrace-latched state (the displayed buffer, the retrace
//! interrupt flag) catches up the way the capture saw it.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::Arc;
use std::time::Instant;

use iris_vpro::dev::odyssey::engine::{memory_bytes, Config, Engine, Event, Lent, Mode};
use iris_vpro::traits::{BusDevice, BusRead8, BUS_OK};

const PAGE: u64 = 4096;

/// Guest memory a replay can preload (`dma` lines); everything else reads 0.
struct Sparse(parking_lot::Mutex<HashMap<u64, Box<[u8; PAGE as usize]>>>);

impl BusDevice for Sparse {
    fn read8(&self, a: u64) -> BusRead8 {
        let m = self.0.lock();
        BusRead8::ok(m.get(&(a / PAGE)).map_or(0, |p| p[(a % PAGE) as usize]))
    }
    fn write8(&self, a: u64, v: u8) -> u32 {
        self.0.lock().entry(a / PAGE).or_insert_with(|| Box::new([0; PAGE as usize]))[(a % PAGE) as usize] = v;
        BUS_OK
    }
    fn read_block(&self, a: u64, buf: &mut [u64]) -> u32 {
        let m = self.0.lock();
        for (i, w) in buf.iter_mut().enumerate() {
            let at = a + 8 * i as u64;
            *w = m.get(&(at / PAGE)).map_or(0, |p| {
                let o = (at % PAGE) as usize;
                u64::from_be_bytes(p[o..o + 8].try_into().unwrap())
            });
        }
        BUS_OK
    }
    fn write_block(&self, a: u64, buf: &[u64]) -> u32 {
        let mut m = self.0.lock();
        for (i, w) in buf.iter().enumerate() {
            let at = a + 8 * i as u64;
            let o = (at % PAGE) as usize;
            m.entry(at / PAGE).or_insert_with(|| Box::new([0; PAGE as usize]))[o..o + 8].copy_from_slice(&w.to_be_bytes());
        }
        BUS_OK
    }
}

#[derive(Clone)]
enum Cmd {
    Write { size: u8, off: u32, val: u64 },
    Read { size: u8, off: u32, val: u64, mask: u64 },
    Dma { addr: u64, at: usize, len: usize },
}

fn num(s: &str) -> u64 {
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).unwrap(),
        None => s.parse().unwrap(),
    }
}

fn parse(text: &str) -> (String, Vec<Cmd>, Vec<u8>) {
    let (mut head, mut cmds, mut blob) = (String::new(), Vec::new(), Vec::new());
    for l in text.lines() {
        let mut w = l.split_whitespace();
        let Some(verb) = w.next() else { continue };
        match verb {
            "write" => {
                let (s, o, v) = (w.next().unwrap(), w.next().unwrap(), w.next().unwrap());
                cmds.push(Cmd::Write { size: num(s) as u8, off: num(o) as u32, val: num(v) });
            }
            "read" => {
                let (s, o, v) = (w.next().unwrap(), w.next().unwrap(), w.next().unwrap());
                let mask = w.next().map_or(u64::MAX, num);
                cmds.push(Cmd::Read { size: num(s) as u8, off: num(o) as u32, val: num(v), mask });
            }
            "dma" => {
                let (a, n, hex) = (w.next().unwrap(), w.next().unwrap(), w.next().unwrap());
                let (at, len) = (blob.len(), num(n) as usize);
                for i in 0..len {
                    blob.push(u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap());
                }
                cmds.push(Cmd::Dma { addr: num(a), at, len });
            }
            "head" => head = w.next().unwrap().to_string(),
            _ => {}
        }
    }
    (head, cmds, blob)
}

fn read_ppm(path: &str) -> (usize, usize, Vec<u32>) {
    let b = std::fs::read(path).unwrap();
    let mut p = 0;
    let mut tok = || {
        while b[p].is_ascii_whitespace() {
            p += 1;
        }
        let s = p;
        while !b[p].is_ascii_whitespace() {
            p += 1;
        }
        std::str::from_utf8(&b[s..p]).unwrap().to_string()
    };
    assert_eq!(tok(), "P6");
    let (w, h) = (tok().parse::<usize>().unwrap(), tok().parse::<usize>().unwrap());
    tok();
    p += 1;
    let px = b[p..].chunks_exact(3).map(|c| (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32).collect();
    (w, h, px)
}

fn write_ppm(path: &str, w: u32, h: u32, px: &[u32]) {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    write!(f, "P6\n{w} {h}\n255\n").unwrap();
    for &p in px {
        f.write_all(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]).unwrap();
    }
}

fn main() {
    let (mut dump, mut reference, mut file, mut ns_per_record) = (None, None, None, 0i64);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dump" => dump = args.next(),
            "--ref" => reference = args.next(),
            "--ns-per-record" => ns_per_record = args.next().unwrap().parse().unwrap(),
            _ => file = Some(a),
        }
    }
    let file = file.expect("a .replay file");
    let t = Instant::now();
    let text = std::fs::read_to_string(&file).unwrap();
    let (head, cmds, blob) = parse(&text);
    drop(text);
    eprintln!("parsed {} records for head {head} in {:.1} s", cmds.len(), t.elapsed().as_secs_f64());

    let product = "v12";
    let bytes = memory_bytes(product).unwrap();
    let mut board_mem = vec![0u64; (bytes / 8) as usize];
    let guest = Arc::new(Sparse(Default::default()));
    // SAFETY: board_mem outlives the engine (dropped after it) and nothing else touches it.
    let engine = unsafe {
        Engine::new(
            Config { product: product.into(), widget: 11, mode: Mode::Inline, pool: (0, 0), gpu: None },
            Some(Lent { ptr: board_mem.as_mut_ptr(), bytes }),
            guest.clone(),
        )
    }
    .unwrap();
    engine.reset();

    let mut now = 1_000_000_000i64;
    let frame_ns = 16_666_667i64;
    let mut mismatches: HashMap<u32, (u64, u64, u64)> = HashMap::new();
    let (mut writes, mut reads, mut retries, mut events, mut busy) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut fifo_words = 0u64;
    let mut evs = Vec::new();
    // Time per segment of 1<<18 records, ns, and the FIFO words in it.
    let mut segs: Vec<(u64, u64)> = Vec::new();
    let (mut seg_t, mut seg_w) = (Instant::now(), 0u64);
    let started = Instant::now();
    for (i, c) in cmds.iter().enumerate() {
        match *c {
            Cmd::Write { size, off, val } => {
                writes += 1;
                if off >= 0x100000 {
                    fifo_words += if size == 8 { 2 } else { 1 };
                    seg_w += if size == 8 { 2 } else { 1 };
                }
                if !engine.write(off as u64, size as u32, val) {
                    busy += 1;
                }
                if ns_per_record != 0 {
                    now += ns_per_record;
                    engine.advance(now);
                }
            }
            Cmd::Read { size, off, val, mask } => {
                reads += 1;
                let mut got = engine.read(off as u64, size as u32).unwrap_or(!val);
                let mut tries = 0;
                while (got ^ val) & mask != 0 && tries < 4 {
                    now += frame_ns;
                    engine.advance(now);
                    got = engine.read(off as u64, size as u32).unwrap_or(!val);
                    tries += 1;
                    retries += 1;
                }
                if (got ^ val) & mask != 0 {
                    let e = mismatches.entry(off).or_default();
                    e.0 += 1;
                    if e.0 == 1 {
                        e.1 = val;
                        e.2 = got;
                    }
                }
            }
            Cmd::Dma { addr, at, len } => {
                for (k, b) in blob[at..at + len].iter().enumerate() {
                    guest.write8(addr + k as u64, *b);
                }
            }
        }
        if i % 256 == 0 {
            engine.drain_events(&mut evs);
            events += evs.len() as u64;
            evs.clear();
        }
        if i % (1 << 18) == (1 << 18) - 1 {
            segs.push((seg_t.elapsed().as_nanos() as u64, seg_w));
            seg_t = Instant::now();
            seg_w = 0;
        }
    }
    let total = started.elapsed();
    engine.drain_events(&mut evs);
    events += evs.len() as u64;
    let faults = evs.iter().filter(|e| matches!(e, Event::Fault(_))).count();

    // Let the last swap latch, then compose.
    now += 2 * frame_ns;
    engine.advance(now);
    assert!(engine.quiesce(std::time::Duration::from_secs(5)));
    println!("records {} writes {writes} reads {reads} fifo words {fifo_words} busy {busy}", cmds.len());
    println!("replay loop {:.3} s = {:.1} ns per FIFO word overall (reads, DMA preloads and retries included)", total.as_secs_f64(), total.as_nanos() as f64 / fifo_words as f64);
    println!("read retries {retries}; events {events} (faults {faults}); displayed buffer {:#x}", engine.displayed_buffer());
    println!("segments of 262144 records, ns per FIFO word (FIFO words in segment):");
    for (k, (ns, w)) in segs.iter().enumerate() {
        if *w > 0 {
            println!("  seg {k:2}: {:8.1}  ({w})", *ns as f64 / *w as f64);
        }
    }
    let mut m: Vec<_> = mismatches.iter().collect();
    m.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    println!("reads still differing after retries: {} offsets", m.len());
    for (off, (n, want, got)) in m.iter().take(12) {
        println!("  {off:#08x} x{n}  first: recorded {want:#x} read {got:#x}");
    }
    match engine.frame() {
        None => println!("no frame: the scan-out has no mode"),
        Some((w, h, px)) => {
            println!("frame {w}x{h}");
            if let Some(d) = &dump {
                write_ppm(d, w, h, &px);
            }
            if let Some(r) = &reference {
                let (rw, rh, rp) = read_ppm(r);
                if (rw, rh) != (w as usize, h as usize) {
                    println!("reference is {rw}x{rh}: not compared");
                } else {
                    let (mut exact, mut near2, mut near8, mut far) = (0, 0, 0, 0);
                    for (a, b) in px.iter().zip(&rp) {
                        let d = (0..3).map(|s| ((a >> (8 * s)) & 255).abs_diff((b >> (8 * s)) & 255)).max().unwrap();
                        match d {
                            0 => exact += 1,
                            1..=2 => near2 += 1,
                            3..=8 => near8 += 1,
                            _ => far += 1,
                        }
                    }
                    let n = px.len();
                    println!("vs reference: exact {exact} ({:.2}%), within 2 {near2}, within 8 {near8}, further {far} of {n}", 100.0 * exact as f64 / n as f64);
                }
            }
        }
    }
}
