//! `iris-bench ab` — a paired comparison of two emulator builds.
//!
//! The question is "is B slower than A?", asked on a host that is busy with
//! other work and has no quiet cores to give. Three things make the usual
//! answer, two sequential runs and a glance at the totals, untrustworthy:
//!
//!   - **The host changes under the runs.** Load drifts between two runs made
//!     minutes apart. So A and B alternate, one at a time, in a fresh random
//!     order each round, and the comparison is read from the per-round B/A
//!     ratio, whose two runs were seconds apart.
//!   - **Outliers.** The table reports medians, which one spoiled round in
//!     ten barely moves, beside the best round. Each kernel's timed run is
//!     short (25 ms by default) and repeated three times, the guest keeping
//!     the fastest, so a burst of other work spoils one repeat rather than the
//!     sample.
//!   - **A moving guest.** The suite's guest binary is rebuilt now and then,
//!     and a new compiler changes the instructions per iteration, which moves
//!     the rates with no emulator change at all. Both builds run the same ELF.
//!
//! Each side is an `iris` emulator binary, run the way `iris-bench run --iris`
//! runs one: this tool supplies the guest image and the machine config, so the
//! two builds differ only in the emulator. The in-process runner cannot do
//! that, since it measures the image linked into whichever binary it is.
//!
//! Rates are guest instructions per host second, compared as the inverse,
//! nanoseconds per guest instruction, so a ratio above 1 reads "B is slower".
//! The kernels autoscale their iteration counts, so only a per-instruction
//! figure is comparable between runs.
//!
//! The pairing loop, [`paired_rounds`], does not know what a run is. A later `iris-bench irix` comparison can reuse them with its
//! own closure.

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args;
use serde::Serialize;

use iris::bench_report::{host_info, parse_block, parse_features};

// ─── statistics ──────────────────────────────────────────────────────────────

pub fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    match s.len() {
        0 => f64::NAN,
        n if n % 2 == 1 => s[n / 2],
        n => (s[n / 2 - 1] + s[n / 2]) / 2.0,
    }
}

pub fn minimum(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::NAN, f64::min)
}

// ─── the pairing loop ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Side {
    A,
    B,
}

/// xorshift64*, enough to shuffle two items. A dependency for this would cost
/// more than the generator.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self { Self(seed | 1) }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

#[derive(Debug, Serialize)]
pub struct Round<T> {
    pub round: usize,
    /// The order the sides ran in this round.
    pub order: [Side; 2],
    pub a: T,
    pub b: T,
}

/// Run `f` once per side per round, A and B in a random order each round, one
/// at a time. Stops at the first error: a build that hangs or crashes is the
/// answer, not something to average around.
pub fn paired_rounds<T>(
    rounds: usize,
    rng: &mut Rng,
    mut f: impl FnMut(Side, usize) -> Result<T, String>,
) -> Result<Vec<Round<T>>, String> {
    let mut out = Vec::with_capacity(rounds);
    for round in 1..=rounds {
        let order = if rng.next() >> 63 == 0 { [Side::A, Side::B] } else { [Side::B, Side::A] };
        let (mut a, mut b) = (None, None);
        for side in order {
            let t = f(side, round)?;
            match side { Side::A => a = Some(t), Side::B => b = Some(t) }
        }
        out.push(Round { round, order, a: a.unwrap(), b: b.unwrap() });
    }
    Ok(out)
}

// ─── the guest's run settings ────────────────────────────────────────────────

/// Set initial values of 32-bit globals in an ELF32 image, by symbol name. The
/// suite reads its run settings (`bench_time_pct`, `bench_repeats`) from the
/// test device when the emulator publishes them, and an `iris` binary run from
/// the command line publishes none, so the defaults compiled into the image
/// are what it runs. Rewriting them in a copy of the image is how both builds
/// get the same shorter settings, including old builds that predate any flag
/// for it. Errors when the image has no symbol table or lacks a symbol.
pub fn set_image_defaults(elf: &mut [u8], vals: &[(&str, u32)]) -> Result<(), String> {
    let bad = || "not an ELF32 image with a symbol table".to_string();
    if elf.len() < 52 || &elf[..4] != b"\x7fELF" || elf[4] != 1 { return Err(bad()); }
    let be = match elf[5] { 1 => false, 2 => true, _ => return Err(bad()) };
    let u16_at = |b: &[u8], o: usize| -> Option<u32> {
        let x: [u8; 2] = b.get(o..o + 2)?.try_into().ok()?;
        Some(if be { u16::from_be_bytes(x) } else { u16::from_le_bytes(x) } as u32)
    };
    let u32_at = |b: &[u8], o: usize| -> Option<u32> {
        let x: [u8; 4] = b.get(o..o + 4)?.try_into().ok()?;
        Some(if be { u32::from_be_bytes(x) } else { u32::from_le_bytes(x) })
    };
    let shoff = u32_at(elf, 0x20).ok_or_else(bad)? as usize;
    let shentsize = u16_at(elf, 0x2E).ok_or_else(bad)? as usize;
    let shnum = u16_at(elf, 0x30).ok_or_else(bad)? as usize;
    // name, type, flags, addr, offset, size, link, info, align, entsize
    let sh = |i: usize, field: usize| u32_at(elf, shoff + i * shentsize + field * 4);
    let symtab = (0..shnum).find(|&i| sh(i, 1) == Some(2)).ok_or_else(bad)?;
    let (off, size, link) = (sh(symtab, 4).ok_or_else(bad)? as usize, sh(symtab, 5).ok_or_else(bad)? as usize,
                             sh(symtab, 6).ok_or_else(bad)? as usize);
    let stroff = sh(link, 4).ok_or_else(bad)? as usize;
    let mut writes = Vec::new();
    for (name, val) in vals {
        let hit = (0..size / 16).find_map(|k| {
            let e = off + k * 16;
            let n = stroff + u32_at(elf, e)? as usize;
            let end = n + elf.get(n..)?.iter().position(|&c| c == 0)?;
            if &elf[n..end] != name.as_bytes() || u32_at(elf, e + 8)? != 4 { return None; }
            let (value, shndx) = (u32_at(elf, e + 4)?, u16_at(elf, e + 14)? as usize);
            // A PROGBITS section holds the initial value; .bss has none to change.
            if sh(shndx, 1)? != 1 { return None; }
            let at = sh(shndx, 4)? as usize + value.checked_sub(sh(shndx, 3)?)? as usize;
            (at + 4 <= elf.len()).then_some(at)
        }).ok_or_else(|| format!("the image has no initialised 4-byte `{name}`"))?;
        writes.push((hit, *val));
    }
    for (at, val) in writes {
        elf[at..at + 4].copy_from_slice(&if be { val.to_be_bytes() } else { val.to_le_bytes() });
    }
    Ok(())
}

// ─── one run ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct KernelSample {
    pub name: String,
    pub iters: u64,
    pub icount: u64,
    pub ns: u64,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct Sample {
    pub wall_s: f64,
    pub features: Vec<String>,
    pub total_icount: u64,
    pub total_ns: u64,
    /// `perf stat` counts for the whole emulator process, user mode only.
    /// They include start-up and the suite's calibration, which is the same
    /// for both builds. `None` when perf is not usable.
    pub host_instructions: Option<u64>,
    pub host_cycles: Option<u64>,
    pub kernels: Vec<KernelSample>,
}

/// `perf stat -x,` writes one `value,unit,event,...` line per event, with
/// `<not supported>` or `<not counted>` in place of a number when the counter
/// is missing, which is the normal case in a VM.
pub fn parse_perf(text: &str, event: &str) -> Option<u64> {
    text.lines()
        .filter(|l| l.split(',').nth(2).is_some_and(|e| e.starts_with(event)))
        .find_map(|l| l.split(',').next()?.trim().parse().ok())
}

#[cfg(target_os = "linux")]
fn perf_usable() -> bool {
    Command::new("perf")
        .args(["stat", "-x,", "-e", "instructions:u,cycles:u", "--", "true"])
        .output()
        .map(|o| {
            let t = String::from_utf8_lossy(&o.stderr);
            o.status.success()
                && parse_perf(&t, "instructions").is_some()
                && parse_perf(&t, "cycles").is_some()
        })
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn perf_usable() -> bool { false }

struct Ctx {
    elf: PathBuf,
    config: PathBuf,
    cwd: PathBuf,
    /// The settings the image was given, checked against what each run reports.
    expect: Option<(u32, u32)>,
    perf: bool,
    timeout: u64,
    extra: Vec<String>,
    scratch: PathBuf,
}

fn run_once(bin: &Path, ctx: &Ctx, tag: &str) -> Result<Sample, String> {
    let mut argv: Vec<std::ffi::OsString> = Vec::new();
    let perf_out = ctx.scratch.join(format!("{tag}.perf"));
    if ctx.perf {
        argv.extend(["perf", "stat", "-x,", "-e", "instructions:u,cycles:u", "-o"].map(Into::into));
        argv.extend([perf_out.clone().into_os_string(), "--".into()]);
    }
    argv.push(bin.into());
    for a in [
        "--config".as_ref(), ctx.config.as_os_str(),
        "--load-elf".as_ref(), ctx.elf.as_os_str(),
        "--test-device".as_ref(), "--headless".as_ref(), "--noaudio".as_ref(),
    ] {
        argv.push(a.into());
    }
    argv.extend(ctx.extra.iter().map(Into::into));

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(&ctx.cwd);
    let started = std::time::Instant::now();
    let out = super::run_with_timeout(cmd, ctx.timeout)?;
    let wall_s = started.elapsed().as_secs_f64();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // A bad path or config fails before the guest prints anything, and the
    // reason is on stderr, so show both streams.
    let p = parse_block(&stdout).map_err(|e| {
        format!("{}: {e} ({})\n--- last 20 lines of standard output ---\n{}\n\
                 --- last 20 lines of standard error ---\n{}",
                bin.display(), out.status, super::tail(&stdout, 20), super::tail(&stderr, 20))
    })?;
    if let Some((pct, reps)) = ctx.expect {
        if (p.settings.time_pct, p.settings.repeats) != (pct, reps) {
            return Err(format!("{}: the guest ran time_pct={} repeats={}, not the {pct} and {reps} \
                written into its image", bin.display(), p.settings.time_pct, p.settings.repeats));
        }
    }

    let (mut hi, mut hc) = (None, None);
    if ctx.perf {
        let t = std::fs::read_to_string(&perf_out).unwrap_or_default();
        hi = parse_perf(&t, "instructions");
        hc = parse_perf(&t, "cycles");
        let _ = std::fs::remove_file(&perf_out);
    }
    Ok(Sample {
        wall_s,
        features: parse_features(&stderr),
        total_icount: p.total_icount,
        total_ns: p.total_ns,
        host_instructions: hi,
        host_cycles: hc,
        kernels: p.rows.into_iter().map(|r| KernelSample {
            name: r.name, iters: r.iters, icount: r.icount, ns: r.ns, status: r.status,
        }).collect(),
    })
}

// ─── summary ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct Cost {
    pub median: f64,
    pub min: f64,
}

impl Cost {
    fn of(v: &[f64]) -> Self { Self { median: median(v), min: minimum(v) } }
}

#[derive(Debug, Serialize)]
pub struct KernelSummary {
    pub name: String,
    /// Nanoseconds per guest instruction across the rounds.
    pub a: Cost,
    pub b: Cost,
    pub median_ratio: f64,
    pub min_ratio: f64,
    /// Median of the per-round B/A ratios.
    pub paired_ratio: f64,
}

#[derive(Debug, Serialize)]
pub struct HostCost {
    pub a_insn: f64,
    pub b_insn: f64,
    pub a_cycles: f64,
    pub b_cycles: f64,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub kernels: Vec<KernelSummary>,
    pub total: KernelSummary,
    /// Host instructions and cycles per guest instruction, median over rounds.
    pub host: Option<HostCost>,
    /// Rounds the summary is built from.
    pub rounds: usize,
    /// Kernels whose checksum did not verify on some run.
    pub bad_status: Vec<String>,
}

fn per_insn(k: &KernelSample) -> Option<f64> {
    (k.icount > 0 && k.ns > 0 && k.status != "SKIP").then(|| k.ns as f64 / k.icount as f64)
}

/// `a` and `b` hold one entry per round, `None` where that run had no usable
/// figure for the kernel. Only rounds where both sides have one are paired.
fn summarise_series(name: &str, a: &[Option<f64>], b: &[Option<f64>]) -> KernelSummary {
    let side = |v: &[Option<f64>]| -> Vec<f64> { v.iter().flatten().copied().collect() };
    let (a_cost, b_cost) = (Cost::of(&side(a)), Cost::of(&side(b)));
    let ratios: Vec<f64> = a.iter().zip(b).filter_map(|(x, y)| Some((*y)? / (*x)?)).collect();
    KernelSummary {
        name: name.to_string(),
        median_ratio: b_cost.median / a_cost.median,
        min_ratio: b_cost.min / a_cost.min,
        paired_ratio: median(&ratios),
        a: a_cost,
        b: b_cost,
    }
}

pub fn summarise(rounds: &[Round<Sample>]) -> Summary {
    let mut names: Vec<String> = Vec::new();
    for k in &rounds[0].a.kernels {
        if per_insn(k).is_some() { names.push(k.name.clone()); }
    }
    let series = |side: Side, name: &str| -> Vec<Option<f64>> {
        rounds.iter().map(|r| {
            let s = if side == Side::A { &r.a } else { &r.b };
            per_insn(s.kernels.iter().find(|k| k.name == name)?)
        }).collect()
    };
    let kernels = names.iter()
        .map(|n| summarise_series(n, &series(Side::A, n), &series(Side::B, n)))
        .collect();
    let tot = |side: Side| -> Vec<Option<f64>> {
        rounds.iter().map(|r| {
            let s = if side == Side::A { &r.a } else { &r.b };
            Some(s.total_ns as f64 / s.total_icount.max(1) as f64)
        }).collect()
    };
    let total = summarise_series("(suite)", &tot(Side::A), &tot(Side::B));

    let host = || -> Option<HostCost> {
        let per = |side: Side, f: fn(&Sample) -> Option<u64>| -> Option<f64> {
            let v: Option<Vec<f64>> = rounds.iter().map(|r| {
                let s = if side == Side::A { &r.a } else { &r.b };
                Some(f(s)? as f64 / s.total_icount.max(1) as f64)
            }).collect();
            Some(median(&v?))
        };
        Some(HostCost {
            a_insn: per(Side::A, |s| s.host_instructions)?,
            b_insn: per(Side::B, |s| s.host_instructions)?,
            a_cycles: per(Side::A, |s| s.host_cycles)?,
            b_cycles: per(Side::B, |s| s.host_cycles)?,
        })
    }();

    let mut bad: Vec<String> = Vec::new();
    for r in rounds {
        for (tag, s) in [("A", &r.a), ("B", &r.b)] {
            for k in &s.kernels {
                if k.status == "MISMATCH" {
                    let m = format!("{} ({tag}: {})", k.name, k.status);
                    if !bad.contains(&m) { bad.push(m); }
                }
            }
        }
    }
    Summary { kernels, total, host, rounds: rounds.len(), bad_status: bad }
}

fn mips(ns_per_insn: f64) -> f64 { 1e3 / ns_per_insn }

fn pct(ratio: f64) -> String { format!("{:+.1}%", (ratio - 1.0) * 100.0) }

/// The table. Rates are shown as MIPS (higher is better); the ratios are of
/// cost, B over A, so +5% means B takes 5% longer per guest instruction.
pub fn table(s: &Summary, la: &str, lb: &str) -> String {
    let mut o = String::new();
    o.push_str(&format!(
        "| kernel | {la} med | {lb} med | {la} best | {lb} best | med B/A | best B/A | paired B/A |\n\
         |---|---:|---:|---:|---:|---:|---:|---:|\n"));
    let row = |k: &KernelSummary| -> String {
        format!("| {} | {:.1} | {:.1} | {:.1} | {:.1} | {} | {} | {} |\n",
                k.name, mips(k.a.median), mips(k.b.median), mips(k.a.min), mips(k.b.min),
                pct(k.median_ratio), pct(k.min_ratio), pct(k.paired_ratio))
    };
    for k in &s.kernels { o.push_str(&row(k)); }
    o.push_str(&row(&s.total));
    o.push_str(&format!("\nRates are guest MIPS (guest instructions per host microsecond). \
        \"best\" is the fastest round. Ratios are cost per guest instruction, B over A: \
        positive means B is slower. \"paired\" is the median of the per-round B/A ratios.\n"));
    if let Some(h) = &s.host {
        o.push_str(&format!(
            "\nHost per guest instruction (perf, user mode, median of rounds; includes start-up):\n\n\
             | | {la} | {lb} | B/A |\n|---|---:|---:|---:|\n\
             | instructions | {:.1} | {:.1} | {} |\n| cycles | {:.1} | {:.1} | {} |\n",
            h.a_insn, h.b_insn, pct(h.b_insn / h.a_insn),
            h.a_cycles, h.b_cycles, pct(h.b_cycles / h.a_cycles)));
    } else {
        o.push_str("\nHost cycles and instructions: not measured (perf unavailable).\n");
    }
    if !s.bad_status.is_empty() {
        o.push_str(&format!("\nWARNING: checksum not OK: {}\n", s.bad_status.join(", ")));
    }
    o
}

// ─── command ─────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct AbArgs {
    /// Emulator binary A, the baseline. An `iris` build, not an `iris-bench`.
    #[arg(long)]
    a: PathBuf,
    /// Emulator binary B, the candidate.
    #[arg(long)]
    b: PathBuf,
    /// Guest suite image both builds run. Defaults to the image linked into
    /// this `iris-bench`. Either way a copy with the run settings below is
    /// written next to the results; the output names the source by suite id.
    /// Hold it fixed when the builds span a rebuild of the image, which moves
    /// the rates on its own.
    #[arg(long)]
    elf: Option<PathBuf>,
    /// Machine config. Defaults to bench/run/bare.toml.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Rounds; each runs A and B once.
    #[arg(long, default_value_t = 20)]
    rounds: usize,
    /// Each kernel's timed run, as a percentage of the suite's default (about
    /// 250 ms). Shorter runs are less often hit by another process.
    #[arg(long, default_value_t = 10)]
    time_pct: u32,
    /// Timed runs per kernel; the guest keeps the fastest.
    #[arg(long, default_value_t = 3)]
    repeats: u32,
    /// Emulated CPU, forwarded to the emulator as `--cpu`.
    #[arg(long)]
    cpu: Option<String>,
    /// Names for the table columns and files.
    #[arg(long, default_value = "A")]
    label_a: String,
    #[arg(long, default_value = "B")]
    label_b: String,
    /// Seed for the A/B order. Default: from the clock.
    /// Recorded in the JSON.
    #[arg(long)]
    seed: Option<u64>,
    /// Per-run timeout, seconds.
    #[arg(long, default_value_t = 1800)]
    timeout: u64,
    /// Results directory.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Do not use perf even if it works.
    #[arg(long)]
    no_perf: bool,
    /// Extra arguments passed to both emulators.
    #[arg(last = true)]
    extra: Vec<String>,
}

#[derive(Serialize)]
struct Report<'a> {
    tool: &'static str,
    a: Side_<'a>,
    b: Side_<'a>,
    elf: String,
    suite_id: String,
    elf_source: &'static str,
    /// Run settings written into the image, `None` when it could not be.
    time_pct: Option<u32>,
    repeats: Option<u32>,
    cpu: &'a Option<String>,
    perf: bool,
    seed: u64,
    host: serde_json::Value,
    rounds: &'a [Round<Sample>],
    summary: &'a Summary,
}

#[derive(Serialize)]
struct Side_<'a> {
    label: &'a str,
    bin: String,
}

pub fn run(args: AbArgs) -> Result<(), String> {
    if args.rounds == 0 { return Err("--rounds must be at least 1".into()); }
    for (what, p) in [("A", &args.a), ("B", &args.b)] {
        if !p.exists() { return Err(format!("no emulator for {what} at {}", p.display())); }
    }
    let abs = |p: &Path| std::fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()));
    let out = args.out.clone().unwrap_or_else(super::default_out);
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let stem = format!("ab-{}-vs-{}", args.label_a, args.label_b);

    let (mut image, elf_source) = match &args.elf {
        Some(p) => (std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?, "--elf"),
        None => (iris::benchsuite::SUITE_ELF.to_vec(), "the image linked into this iris-bench"),
    };
    let suite_id = iris::benchsuite::suite_id_of(&image);
    let settings = match set_image_defaults(&mut image, &[("bench_time_pct", args.time_pct), ("bench_repeats", args.repeats)]) {
        Ok(()) => Some((args.time_pct, args.repeats)),
        Err(e) => {
            println!("note: running the image's own settings, not time_pct {} repeats {}: {e}",
                     args.time_pct, args.repeats);
            None
        }
    };
    let elf = out.join(format!("{stem}.elf"));
    std::fs::write(&elf, &image).map_err(|e| format!("{}: {e}", elf.display()))?;
    let elf = abs(&elf)?;
    let config = abs(&args.config.clone().unwrap_or_else(|| super::repo_relative("bench/run/bare.toml")))?;
    let cwd = config.parent().and_then(|p| p.parent()).map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let (bin_a, bin_b) = (abs(&args.a)?, abs(&args.b)?);

    let perf = !args.no_perf && perf_usable();
    let mut extra = Vec::new();
    if let Some(c) = &args.cpu { extra.extend(["--cpu".to_string(), c.clone()]); }
    extra.extend(args.extra.iter().cloned());
    let ctx = Ctx {
        elf: elf.clone(), config, cwd, expect: settings, perf,
        timeout: args.timeout, extra, scratch: out.clone(),
    };

    let seed = args.seed.unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64).unwrap_or(1)
    });
    let run_cfg = match settings {
        Some((p, r)) => format!("time_pct {p}, repeats {r}"),
        None => "the image's own settings".to_string(),
    };
    println!("A/B: {} = {}\n     {} = {}", args.label_a, bin_a.display(), args.label_b, bin_b.display());
    println!("guest image: {suite_id} from {elf_source}, run with {run_cfg} ({})", elf.display());
    println!("rounds {}, perf {}", args.rounds, if perf { "on" } else { "off" });

    let mut rng = Rng::new(seed);
    let mips = |s: &Sample| s.total_icount as f64 * 1e3 / s.total_ns.max(1) as f64;
    let label = |side: Side| match side { Side::A => (&bin_a, &args.label_a), Side::B => (&bin_b, &args.label_b) };
    let rounds = paired_rounds(args.rounds, &mut rng, |side, round| {
        let (bin, lab) = label(side);
        let s = run_once(bin, &ctx, &format!("{stem}-r{round}-{lab}"))?;
        println!("round {round}/{} {lab}: {:.1} MIPS, {:.0}s", args.rounds, mips(&s), s.wall_s);
        Ok(s)
    })?;

    let summary = summarise(&rounds);
    let text = table(&summary, &args.label_a, &args.label_b);
    let report = Report {
        tool: "iris-bench ab",
        a: Side_ { label: &args.label_a, bin: bin_a.display().to_string() },
        b: Side_ { label: &args.label_b, bin: bin_b.display().to_string() },
        elf: elf.display().to_string(),
        suite_id: suite_id.clone(),
        elf_source,
        time_pct: settings.map(|s| s.0),
        repeats: settings.map(|s| s.1),
        cpu: &args.cpu,
        perf,
        seed,
        host: serde_json::to_value(host_info()).map_err(|e| e.to_string())?,
        rounds: &rounds,
        summary: &summary,
    };
    let json_path = out.join(format!("{stem}.json"));
    let md_path = out.join(format!("{stem}.md"));
    std::fs::write(&json_path, serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?)
        .map_err(|e| format!("{}: {e}", json_path.display()))?;
    let header = format!("guest image {suite_id} (from {elf_source}, {run_cfg}), {} rounds\n\n",
                         args.rounds);
    std::fs::write(&md_path, format!("{header}{text}"))
        .map_err(|e| format!("{}: {e}", md_path.display()))?;
    println!("\n{header}{text}");
    println!("wrote {} and {}", json_path.display(), md_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_odd_and_even_counts() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
        assert!(median(&[]).is_nan());
    }

    #[test]
    fn a_single_slow_outlier_moves_neither_median_nor_min() {
        let v = [10.0, 10.1, 9.9, 13.5, 10.0];
        assert_eq!(median(&v), 10.0);
        assert_eq!(minimum(&v), 9.9);
    }

    #[test]
    fn every_round_runs_each_side_once_and_orders_vary() {
        let mut rng = Rng::new(7);
        let mut calls = Vec::new();
        let rounds = paired_rounds(40, &mut rng, |s, r| { calls.push((r, s)); Ok(()) }).unwrap();
        assert_eq!(rounds.len(), 40);
        assert_eq!(calls.len(), 80);
        for r in 1..=40 {
            let in_round: Vec<Side> = calls.iter().filter(|c| c.0 == r).map(|c| c.1).collect();
            assert_eq!(in_round.len(), 2);
            assert_ne!(in_round[0], in_round[1]);
        }
        let a_first = rounds.iter().filter(|r| r.order[0] == Side::A).count();
        assert!((10..=30).contains(&a_first), "order is not shuffled: A first {a_first}/40");
    }

    #[test]
    fn a_failing_run_stops_the_loop() {
        let mut rng = Rng::new(1);
        let mut n = 0;
        let e = paired_rounds(5, &mut rng, |_, _| -> Result<(), String> {
            n += 1;
            if n == 3 { Err("hung".into()) } else { Ok(()) }
        }).unwrap_err();
        assert_eq!(e, "hung");
        assert_eq!(n, 3);
    }

    #[test]
    fn perf_lines_parse_and_unsupported_counters_do_not() {
        let ok = "123456,,instructions:u,100,100.00,,\n654321,,cycles:u,100,100.00,,\n";
        assert_eq!(parse_perf(ok, "instructions"), Some(123456));
        assert_eq!(parse_perf(ok, "cycles"), Some(654321));
        let vm = "<not supported>,,instructions:u,0,100.00,,\n";
        assert_eq!(parse_perf(vm, "instructions"), None);
        assert_eq!(parse_perf("", "cycles"), None);
    }

    fn sample(ns_per_insn: f64, host: Option<u64>) -> Sample {
        let k = |name: &str, f: f64| KernelSample {
            name: name.into(), iters: 10, icount: 10_000, ns: (10_000.0 * ns_per_insn * f) as u64,
            status: "OK".into(),
        };
        Sample {
            wall_s: 1.0, features: vec![], total_icount: 20_000,
            total_ns: (20_000.0 * ns_per_insn) as u64,
            host_instructions: host, host_cycles: host,
            kernels: vec![k("int/alu", 1.0), k("img/resize", 1.0)],
        }
    }

    fn rounds_of(a: &[f64], b: &[f64]) -> Vec<Round<Sample>> {
        a.iter().zip(b).enumerate().map(|(i, (a, b))| Round {
            round: i + 1, order: [Side::A, Side::B],
            a: sample(*a, Some(1_000_000)), b: sample(*b, Some(1_100_000)),
        }).collect()
    }

    #[test]
    fn summary_measures_a_steady_regression_and_ignores_one_slow_round() {
        // B is 10% slower every round.
        let s = summarise(&rounds_of(&[10.0; 8], &[11.0; 8]));
        for k in s.kernels.iter().chain([&s.total]) {
            assert!((k.median_ratio - 1.1).abs() < 1e-9 && (k.min_ratio - 1.1).abs() < 1e-9, "{k:?}");
            assert!((k.paired_ratio - 1.1).abs() < 1e-9, "{k:?}");
        }
        let h = s.host.as_ref().unwrap();
        assert!((h.b_insn / h.a_insn - 1.1).abs() < 1e-9);
        assert!(table(&s, "A", "B").contains("| int/alu | 100.0 | 90.9 |"));

        // Same build, one B round in eleven 30% slow: the median stays put.
        let mut b = [10.0; 11];
        b[2] = 13.0;
        let s = summarise(&rounds_of(&[10.0; 11], &b));
        assert!(s.kernels.iter().all(|k| (k.paired_ratio - 1.0).abs() < 1e-9));
    }

    #[test]
    fn only_a_mismatch_is_a_bad_checksum() {
        let mut r = rounds_of(&[10.0; 3], &[10.0; 3]);
        for x in &mut r { x.a.kernels[0].status = "UNCHECKED".into(); }
        assert!(summarise(&r).bad_status.is_empty());
        r[1].b.kernels[1].status = "MISMATCH".into();
        assert_eq!(summarise(&r).bad_status, vec!["img/resize (B: MISMATCH)".to_string()]);
    }

    #[test]
    fn missing_perf_leaves_the_host_block_out() {
        let mut r = rounds_of(&[10.0; 3], &[10.0; 3]);
        r[1].b.host_cycles = None;
        let s = summarise(&r);
        assert!(s.host.is_none());
        assert!(table(&s, "A", "B").contains("perf unavailable"));
    }

    #[test]
    fn the_run_settings_are_written_into_a_copy_of_the_image() {
        let mut img = iris::benchsuite::SUITE_ELF.to_vec();
        set_image_defaults(&mut img, &[("bench_time_pct", 10), ("bench_repeats", 3)]).unwrap();
        let changed: Vec<usize> = (0..img.len()).filter(|&i| img[i] != iris::benchsuite::SUITE_ELF[i]).collect();
        assert!(!changed.is_empty() && changed.len() <= 8, "{} bytes changed", changed.len());
        // Writing the same values again changes nothing more.
        let again = img.clone();
        set_image_defaults(&mut img, &[("bench_time_pct", 10), ("bench_repeats", 3)]).unwrap();
        assert_eq!(img, again);
        assert!(set_image_defaults(&mut img, &[("no_such_symbol", 1)]).is_err());
        assert!(set_image_defaults(&mut b"not an elf at all, not an elf at all, not an elf at all".to_vec(),
                                   &[("bench_repeats", 3)]).is_err());
    }
}
