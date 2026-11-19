//! Replay a QEMU KONA fixture (`sgi-replay 2`) into an `Engine` and compare the final frame
//! with its screendump. Reads are checked against QEMU's values and counted, not fatal; `pixel`
//! lines read QEMU's own planes, so they are skipped. `--eager` runs the stream after each store
//! (QEMU's order), `--runs N` times fresh engines, `--snapshot-at N` saves and reloads mid-replay.
//!
//! `qemu_replay [--eager] [--threads N] [--gpu] [--runs N] [--ref REF.ppm] [--out OUT.ppm] FILE`

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use iris_kona::dev::kona::engine::{Engine, Options, Out, PipeMemory, Read, Source, Write};
use iris_kona::dev::kona::ident::KonaConfig;
use parking_lot::Mutex;

#[derive(Default)]
struct Ram(Mutex<HashMap<u64, Box<[u8; 4096]>>>);

impl PipeMemory for Ram {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let pages = self.0.lock();
        for (i, b) in buf.iter_mut().enumerate() {
            let a = addr + i as u64;
            *b = pages.get(&(a >> 12)).map_or(0, |p| p[(a & 0xfff) as usize]);
        }
        true
    }

    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let mut pages = self.0.lock();
        for (i, b) in data.iter().enumerate() {
            let a = addr + i as u64;
            pages.entry(a >> 12).or_insert_with(|| Box::new([0; 4096]))[(a & 0xfff) as usize] = *b;
        }
        true
    }
}

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

/// The processor interface charges a credit for a store into the command page only.
fn source(off: u32) -> Source {
    Source { hub: Some(0), charged: (0xc10000..0xc14000).contains(&off) }
}

fn run(rp: &Replay, cfg: KonaConfig, eager: bool, ram: Arc<Ram>, snapshot_at: Option<usize>, gpu: &Option<iris_kona::SharedGpu>) -> (Engine, Tally, std::time::Duration) {
    let opts = || Options { gpu: gpu.clone(), ..Options::default() };
    let mut e = Engine::new(cfg, ram.clone(), opts()).expect("engine");
    let mut t = Tally::default();
    let mut out = Vec::new();
    let t0 = Instant::now();
    let mut snapped = snapshot_at.is_none();
    for (n, rec) in rp.recs.iter().enumerate() {
        // Quiesce, save and load into a new engine at the first command boundary from `snapshot_at`.
        if !snapped && n >= snapshot_at.unwrap() {
            e.run(usize::MAX);
            let dir = std::env::temp_dir().join(format!("kona-replay-{}", std::process::id()));
            let cs = iris_kona::chunk_store::ChunkStore::new(&dir);
            if let Ok(chunks) = e.save_chunks(&cs) {
                let mut next = Engine::new(cfg, ram.clone(), opts()).expect("engine");
                next.load_chunks(&cs, &chunks).expect("load");
                println!("snapshot at record {n}: {} chunks, loaded into a new engine", chunks.len());
                e = next;
                snapped = true;
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
        match rec {
            Rec::Write { off, size, val } => {
                if (0xc10000..0xc14000).contains(off) {
                    t.command_words += (*size as u64) / 4;
                }
                while e.write(*off, *size, *val, source(*off)) == Write::Busy {
                    t.busy += 1;
                    e.run(1 << 12);
                }
                if eager {
                    e.run(usize::MAX);
                }
            }
            Rec::Read { off, size, val, mask, line } => {
                t.reads += 1;
                let mut got = 0u64;
                for k in 0..(*size / 4) {
                    let o = off + 4 * k;
                    let v = loop {
                        match e.read(o) {
                            Read::Value(v) => break v,
                            Read::Wait => {
                                e.run(usize::MAX);
                            }
                        }
                    };
                    got = got << 32 | v as u64;
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
            Rec::Reset => e.reset(),
        }
        out.clear();
        e.drain_out(&mut out);
        for o in &out {
            match o {
                Out::Interrupt { .. } => t.interrupts += 1,
                Out::Credits { credits, .. } => t.credits += *credits as u64,
            }
        }
    }
    e.run(usize::MAX);
    e.frame();
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
    let mut snapshot_at: Option<usize> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--eager" => eager = true,
            "--threads" => threads = args.next().unwrap().parse().unwrap(),
            "--runs" => runs = args.next().unwrap().parse().unwrap(),
            "--ref" => refp = args.next(),
            "--out" => outp = args.next(),
            "--gpu" => use_gpu = true,
            "--snapshot-at" => snapshot_at = args.next().map(|n| n.parse().unwrap()),
            "--widget" => widget = args.next().unwrap().parse().unwrap(),
            _ => file = Some(a),
        }
    }
    let file = file.expect("a replay file");
    let rp = parse(&file);
    let cfg = KonaConfig::ir2(2).unwrap().with_widget(widget).unwrap().with_pool(threads, 32);
    println!("{file}: head {}, {} records, {} pixel checks skipped", rp.head, rp.recs.len(), rp.pixel_checks);

    let gpu = use_gpu.then(|| iris_kona::wgpu::Gpu::new(&iris_kona::wgpu::GpuOptions::default(), Box::new(|m| panic!("GPU failed: {m}"))).expect("a GPU"));
    if let Some(g) = &gpu {
        println!("GPU: {}", g.caps);
    }
    let ram = Arc::new(Ram::default());
    let (mut e, t, d) = run(&rp, cfg, eager, ram, snapshot_at, &gpu);
    println!("reads {}: {} differ from QEMU's; {} interrupt packets, {} credits returned, {} busy retries, {} command-page words", t.reads, t.mismatches, t.interrupts, t.credits, t.busy, t.command_words);
    for l in &t.first {
        println!("  {l}");
    }
    for (o, n) in &t.by_offset {
        println!("  {n} mismatched reads at {o:#x}");
    }
    println!("status {:?}", e.status());
    println!("run: {:.3} s ({} ns per command-page word)", d.as_secs_f64(), d.as_nanos() as u64 / t.command_words.max(1));

    let c0 = Instant::now();
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
            let (_e, _t, d) = run(&rp, cfg, eager, Arc::new(Ram::default()), None, &gpu);
            best = best.min(d);
            all.push(d.as_secs_f64());
        }
        all.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("{runs} runs: best {:.3} s, median {:.3} s; best {} ns per command-page word", best.as_secs_f64(), all[all.len() / 2], best.as_nanos() as u64 / t.command_words.max(1));
    }
}
