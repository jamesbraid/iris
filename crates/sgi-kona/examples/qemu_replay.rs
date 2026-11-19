//! Replay a QEMU KONA fixture (`sgi-replay 2`) into a `KonaEngine` and compare the final frame
//! with its screendump. Reads are checked against QEMU's values and counted, not fatal; `pixel`
//! lines read QEMU's own planes, so they are skipped. `--eager` runs the stream after each store
//! (QEMU's order), `--runs N` times fresh engines, `--snapshot-at N` saves and reloads mid-replay.
//! `--skip-onewire` leaves the one-wire register out of the mismatch count: the maintenance-menu
//! fixture was recorded from an unconfigured QEMU device, whose chain has two parts to this
//! engine's one. The stream waits at each swap for a frame, which the replay gives only when a write
//! finds the stream full or a read waits for it (`sgi_engine::shell::drain_stalls`), and once at the
//! end; `--frame-every N` adds a frame every N records.
//!
//! `qemu_replay [--eager] [--frame-every N] [--skip-onewire] [--threads N] [--gpu] [--runs N] [--ref REF.ppm] [--out OUT.ppm] FILE`

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use sgi_kona::dev::kona::defs::{CAUSE_LOWATER, INT_ENABLE};
use sgi_kona::dev::kona::engine::{KonaEngine, Options};
use sgi_kona::dev::kona::ident::KonaConfig;
use parking_lot::Mutex;
use sgi_engine::{shell, BoardEngine, Chunk, Event, HostMem, Idle, MemFault, Read, Source, Swap, VecSource, Write};

#[derive(Clone, Default)]
struct Ram(Arc<Mutex<HashMap<u64, Box<[u8; 4096]>>>>);

impl Ram {
    fn write(&self, addr: u64, data: &[u8]) {
        let mut pages = self.0.lock();
        for (i, b) in data.iter().enumerate() {
            let a = addr + i as u64;
            pages.entry(a >> 12).or_insert_with(|| Box::new([0; 4096]))[(a & 0xfff) as usize] = *b;
        }
    }
}

impl HostMem for Ram {
    fn ram_read(&self, addr: u64, buf: &mut [u8], _: Swap) -> Result<(), MemFault> {
        let pages = self.0.lock();
        for (i, b) in buf.iter_mut().enumerate() {
            let a = addr + i as u64;
            *b = pages.get(&(a >> 12)).map_or(0, |p| p[(a & 0xfff) as usize]);
        }
        Ok(())
    }

    fn ram_write(&self, addr: u64, data: &[u8], _: Swap) -> Result<(), MemFault> {
        self.write(addr, data);
        Ok(())
    }
}

type Engine = KonaEngine<Ram>;

enum Rec {
    Write { off: u32, size: u32, val: u64 },
    Read { off: u32, size: u32, val: u64, mask: u64, line: usize },
    Dma { addr: u64, data: Vec<u8> },
    Reset,
}

struct Replay {
    head: String,
    recs: Vec<Rec>,
    pixel_checks: usize,
    screendump: Option<String>,
}

fn num(s: &str) -> u64 {
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).unwrap(),
        None => s.parse().unwrap(),
    }
}

fn parse(path: &str) -> Replay {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut r = Replay { head: String::new(), recs: Vec::new(), pixel_checks: 0, screendump: None };
    for (n, line) in text.lines().enumerate() {
        let w: Vec<&str> = line.split_whitespace().collect();
        match w.first().copied() {
            None | Some("sgi-replay") => {}
            Some(c) if c.starts_with('#') => {}
            Some("head") => r.head = w[1].to_string(),
            Some("write") => r.recs.push(Rec::Write { size: num(w[1]) as u32, off: num(w[2]) as u32, val: num(w[3]) }),
            Some("read") => r.recs.push(Rec::Read { size: num(w[1]) as u32, off: num(w[2]) as u32, val: num(w[3]), mask: w.get(4).map_or(u64::MAX, |m| num(m)), line: n + 1 }),
            Some("dma") => {
                let hex = w[3].as_bytes();
                let data = hex.chunks(2).map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap()).collect();
                r.recs.push(Rec::Dma { addr: num(w[1]), data });
            }
            Some("reset") => r.recs.push(Rec::Reset),
            Some("pixel" | "display-pixel" | "back-pixel" | "pixels" | "wid" | "overlay") => r.pixel_checks += 1,
            Some("screendump") => r.screendump = Some(w[1].to_string()),
            Some(other) => panic!("{path}:{}: unknown verb {other}", n + 1),
        }
    }
    r
}

#[derive(Default, Debug)]
struct Tally {
    reads: usize,
    mismatches: usize,
    first: Vec<String>,
    by_offset: std::collections::BTreeMap<u32, usize>,
    interrupts: usize,
    credits: u64,
    busy: usize,
    command_words: u64,
}

/// The processor interface charges a credit for a store into the command page only. Hub 0 is
/// source 1: source 0 is a store with no sender.
fn source(off: u32) -> Source {
    Source { id: 1, charged: (0xc10000..0xc14000).contains(&off) }
}

/// Host time in ns, the shell's clock for `frame`.
fn now(t0: Instant) -> u64 {
    t0.elapsed().as_nanos() as u64
}

/// Count what the engine has queued for the shell.
fn tally(e: &mut Engine, out: &mut Vec<Event>, t: &mut Tally) {
    out.clear();
    e.drain_events(out, usize::MAX);
    for ev in out.iter() {
        match ev {
            Event::Interrupt(_) => t.interrupts += 1,
            Event::Credits { retired, .. } => t.credits += *retired as u64,
            Event::DeviceFault(why) => println!("device fault: {why}"),
            _ => {}
        }
    }
}

fn run(rp: &Replay, cfg: KonaConfig, eager: bool, ram: Ram, snapshot_at: Option<usize>, gpu: &Option<sgi_kona::SharedGpu>, frame_every: usize, sleep_on_drain: bool) -> (Engine, Tally, std::time::Duration) {
    let opts = || Options { gpu: gpu.clone(), ..Options::default() };
    let mut e = Engine::new(cfg, ram.clone(), opts(), None).expect("engine");
    let mut t = Tally::default();
    let mut out = Vec::new();
    let t0 = Instant::now();
    let mut snapped = snapshot_at.is_none();
    // The stream waits at a swap for a frame; the replay gives one when something needs the stream to move.
    let settle = |e: &mut Engine| match shell::drain_stalls(e, &mut now(t0), 1_000_000, usize::MAX) {
        Idle::Faulted => panic!("the pipe faulted: {:?}", e.fault()),
        _ => {}
    };
    for (n, rec) in rp.recs.iter().enumerate() {
        // Quiesce, save and load into a new engine at the first command boundary from `snapshot_at`.
        if !snapped && n >= snapshot_at.unwrap() {
            settle(&mut e);
            let mut at = now(t0);
            shell::quiesce(&mut e, &mut at, 1_000_000, usize::MAX).expect("quiesce");
            let mut chunks: Vec<Chunk> = Vec::new();
            if e.save(&mut chunks).is_ok() {
                let mut next = Engine::new(cfg, ram.clone(), opts(), None).expect("engine");
                next.quiesce(u64::MAX).expect("quiesce");
                next.load(&mut VecSource::new(chunks.clone())).expect("load");
                next.resume();
                println!("snapshot at record {n}: {} chunks, loaded into a new engine", chunks.len());
                e = next;
                snapped = true;
            } else {
                e.resume();
            }
        }
        if frame_every != 0 && n % frame_every == 0 {
            e.frame(now(t0));
        }
        match rec {
            Rec::Write { off, size, val } => {
                if (0xc10000..0xc14000).contains(off) {
                    t.command_words += (*size as u64) / 4;
                }
                e.set_source(source(*off));
                while e.write(*off, *size * 8, *val) == Write::Busy {
                    t.busy += 1;
                    shell::drain_stalls(&mut e, &mut now(t0), 1_000_000, usize::MAX);
                }
                if eager {
                    e.run(usize::MAX);
                }
                // A guest enables the FIFO-low cause to sleep until the pipe drains, and the pipe
                // drains once the shell gives it its frames; the recording has the guest's reads
                // after the interrupt, so the replay waits here as that guest does.
                if sleep_on_drain && *off == INT_ENABLE && *val & CAUSE_LOWATER as u64 != 0 {
                    settle(&mut e);
                }
            }
            Rec::Read { off, size, val, mask, line } => {
                t.reads += 1;
                let mut got = 0u64;
                for k in 0..(*size / 4) {
                    let o = off + 4 * k;
                    let v = loop {
                        match e.read(o, 32) {
                            Read::Value(v) => break v,
                            Read::Busy => settle(&mut e),
                            Read::Fault => panic!("line {line}: the pipe faulted: {:?}", e.fault()),
                        }
                    };
                    got = got << 32 | v;
                }
                if (got ^ val) & mask != 0 {
                    t.mismatches += 1;
                    *t.by_offset.entry(*off).or_default() += 1;
                    if t.first.len() < 8 {
                        t.first.push(format!("line {line}: read {off:#x} gave {got:#x}, QEMU {val:#x} (mask {mask:#x})"));
                    }
                }
            }
            Rec::Dma { addr, data } => {
                ram.write(*addr, data);
            }
            Rec::Reset => {
                settle(&mut e);
                e.reset().expect("reset");
            }
        }
        tally(&mut e, &mut out, &mut t);
    }
    settle(&mut e);
    e.frame(now(t0));
    tally(&mut e, &mut out, &mut t);
    let d = t0.elapsed();
    (e, t, d)
}

fn read_ppm(path: &str) -> (usize, usize, Vec<u8>) {
    let b = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut i = 0;
    let mut tok = || {
        while b[i].is_ascii_whitespace() {
            i += 1;
        }
        let s = i;
        while !b[i].is_ascii_whitespace() {
            i += 1;
        }
        String::from_utf8_lossy(&b[s..i]).to_string()
    };
    assert_eq!(tok(), "P6");
    let (w, h, _max) = (tok().parse().unwrap(), tok().parse().unwrap(), tok());
    (w, h, b[i + 1..].to_vec())
}

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let (mut eager, mut threads, mut runs, mut refp, mut outp, mut file) = (false, 0usize, 1usize, None, None, None);
    let mut widget = 0u8;
    let mut use_gpu = false;
    let mut skip_onewire = false;
    let mut frame_every = 0usize;
    let mut sleep_on_drain = false;
    let mut snapshot_at: Option<usize> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--eager" => eager = true,
            "--threads" => threads = args.next().unwrap().parse().unwrap(),
            "--runs" => runs = args.next().unwrap().parse().unwrap(),
            "--ref" => refp = args.next(),
            "--out" => outp = args.next(),
            "--gpu" => use_gpu = true,
            "--skip-onewire" => skip_onewire = true,
            "--frame-every" => frame_every = args.next().unwrap().parse().unwrap(),
            "--sleep-on-drain" => sleep_on_drain = true,
            "--snapshot-at" => snapshot_at = args.next().map(|n| n.parse().unwrap()),
            "--widget" => widget = args.next().unwrap().parse().unwrap(),
            _ => file = Some(a),
        }
    }
    let file = file.expect("a replay file");
    let rp = parse(&file);
    let cfg = KonaConfig::ir2(2).unwrap().with_widget(widget).unwrap().with_pool(threads, 32);
    println!("{file}: head {}, {} records, {} pixel checks skipped", rp.head, rp.recs.len(), rp.pixel_checks);

    let gpu = use_gpu.then(|| sgi_kona::wgpu::Gpu::new(&sgi_kona::wgpu::GpuOptions::default(), Box::new(|m| panic!("GPU failed: {m}"))).expect("a GPU"));
    if let Some(g) = &gpu {
        println!("GPU: {}", g.caps);
    }
    let ram = Ram::default();
    let (mut e, mut t, d) = run(&rp, cfg, eager, ram, snapshot_at, &gpu, frame_every, sleep_on_drain);
    if skip_onewire {
        let skipped: usize = [0x98, 0x9c].iter().filter_map(|o| t.by_offset.remove(o)).sum();
        t.mismatches -= skipped;
        println!("{skipped} one-wire reads left out of the count");
    }
    println!("reads {}: {} differ from QEMU's; {} interrupt packets, {} credits returned, {} busy retries, {} command-page words", t.reads, t.mismatches, t.interrupts, t.credits, t.busy, t.command_words);
    for l in &t.first {
        println!("  {l}");
    }
    for (o, n) in &t.by_offset {
        println!("  {n} mismatched reads at {o:#x}");
    }
    println!("status: {} stores queued, swap waiting {}, fault {:?}", e.queued(), e.swap_waiting(), e.fault());
    println!("run: {:.3} s ({} ns per command-page word)", d.as_secs_f64(), d.as_nanos() as u64 / t.command_words.max(1));

    let c0 = Instant::now();
    if gpu.is_some() {
        match sgi_engine::BoardEngine::lend_frame(&mut e, 0) {
            Ok(Some(f)) => println!("lent frame {}x{} (wgpu buffer: {})", f.width, f.height, f.buffer.downcast_ref::<wgpu::Buffer>().is_some()),
            Ok(None) => println!("no lent frame"),
            Err(x) => println!("lend failed: {x}"),
        }
    }
    let (w, h, words) = e.compose();
    println!("compose (waits for the drawing still queued, then reads the frame): {:.3} s", c0.elapsed().as_secs_f64());
    if let Some(p) = &outp {
        let mut b = format!("P6\n{w} {h}\n255\n").into_bytes();
        for px in &words {
            b.extend_from_slice(&[*px as u8, (*px >> 8) as u8, (*px >> 16) as u8]);
        }
        std::fs::write(p, b).unwrap();
    }
    let reference = refp.or(rp.screendump.clone().map(|s| std::path::Path::new(&file).with_file_name(s).to_string_lossy().to_string()));
    if let Some(r) = reference {
        let (rw, rh, rb) = read_ppm(&r);
        if (rw, rh) != (w as usize, h as usize) {
            println!("screen: {w}x{h} against {rw}x{rh} in {r}");
        } else {
            let (mut differ, mut max, mut sum) = (0usize, 0u32, 0u64);
            for (i, px) in words.iter().enumerate() {
                let m = [*px as u8, (*px >> 8) as u8, (*px >> 16) as u8];
                let q = &rb[3 * i..3 * i + 3];
                let d = (0..3).map(|c| (m[c] as i32 - q[c] as i32).unsigned_abs()).max().unwrap();
                if d != 0 {
                    differ += 1;
                    max = max.max(d);
                    sum += d as u64;
                }
            }
            println!("screen {w}x{h} against {r}: {differ} pixels differ ({:.3}%), largest channel difference {max}, mean over the differing {:.1}", 100.0 * differ as f64 / words.len() as f64, sum as f64 / differ.max(1) as f64);
        }
    }

    if runs > 1 {
        let mut best = std::time::Duration::MAX;
        let mut all = Vec::new();
        for _ in 0..runs {
            let (_e, _t, d) = run(&rp, cfg, eager, Ram::default(), None, &gpu, frame_every, sleep_on_drain);
            best = best.min(d);
            all.push(d.as_secs_f64());
        }
        all.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("{runs} runs: best {:.3} s, median {:.3} s; best {} ns per command-page word", best.as_secs_f64(), all[all.len() / 2], best.as_nanos() as u64 / t.command_words.max(1));
    }
}
