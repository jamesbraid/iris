//! `iris-bench ab` — a paired comparison of two emulator builds.
//!
//! The question is "is B slower than A?", asked on a host that is busy with
//! other work and has no quiet cores to give. Three things make the usual
//! answer, two sequential runs and a glance at the totals, untrustworthy:
//!
//!   - **Drift.** Load changes over minutes. Running A five times and then B
//!     five times compares two different hosts. Here every round runs A and B
//!     once each, in a fresh random order, one process at a time.
//!   - **Outliers.** On a shared host roughly one run in ten is 25-35% slow.
//!     A median shrugs that off but also hides a small real change; the best
//!     run is the least disturbed but is one sample. A kernel is flagged only
//!     when the median ratio and the best-run ratio both clear the threshold
//!     in the same direction. Because that rule is a heuristic, the table also
//!     gives the per-round paired B/A ratio with a bootstrap interval, for
//!     reading, and a noise figure, the run's own estimate of an A/A result. A
//!     row whose scatter exceeds the effect (the threshold for "no flag", the
//!     observed change for a flag) is reported INCONCLUSIVE.
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
//! The pairing loop, [`paired_rounds`], does not know what a run is. A later
//! `iris-bench irix` comparison can reuse it with its own closure.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Slower,
    Faster,
    Same,
    /// Nothing flagged, but the rounds scatter by more than the threshold, so
    /// a change of that size could be hiding in the noise.
    Inconclusive,
}

/// Bootstrap settings. The seed is fixed, not taken from the run's order seed,
/// so the same rounds always print the same interval.
pub const BOOT_RESAMPLES: usize = 10_000;
pub const BOOT_CONFIDENCE: f64 = 0.95;
pub const BOOT_SEED: u64 = 0x1215_B007_57A9_0001;

/// With fewer rounds an interval over the median says nothing: three rounds
/// is the least that can disagree with itself.
pub const MIN_ROUNDS: usize = 3;

/// Percentile bootstrap confidence interval of the median of `ratios`
/// (Efron 1979): resample the rounds with replacement `resamples` times, take
/// the median of each resample, and return the central `confidence` span of
/// those medians. `None` below [`MIN_ROUNDS`] samples or on a non-finite ratio.
///
/// Resampling whole rounds keeps the pairing: each ratio is already B over A
/// from the same moment. With few rounds the interval is coarse (the median of
/// five values can only be one of those values) and percentile intervals
/// under-cover somewhat at small n; the effect-size threshold absorbs that.
pub fn bootstrap_ci(ratios: &[f64], resamples: usize, confidence: f64, seed: u64) -> Option<(f64, f64)> {
    let n = ratios.len();
    if n < MIN_ROUNDS || resamples == 0 || ratios.iter().any(|r| !r.is_finite()) {
        return None;
    }
    let mut rng = Rng::new(seed);
    let mut buf = vec![0.0; n];
    let mut meds: Vec<f64> = (0..resamples).map(|_| {
        for slot in buf.iter_mut() { *slot = ratios[(rng.next() >> 11) as usize % n]; }
        median(&buf)
    }).collect();
    meds.sort_by(|a, b| a.total_cmp(b));
    let tail = (1.0 - confidence) / 2.0;
    let lo = ((tail * resamples as f64).floor() as usize).min(resamples - 1);
    let hi = (((1.0 - tail) * resamples as f64).ceil() as usize).clamp(1, resamples) - 1;
    Some((meds[lo], meds[hi]))
}

/// The flag rule. Ratios are B over A of a cost, so above 1 is slower.
/// `threshold` is a fraction (0.03 for 3%). A direction needs both ratios past
/// it: a median that moved while the best run did not is load, and a best run
/// that moved while the median did not is one lucky sample. "Faster" is the
/// mirror image, 1/(1+t), so a 3% gain and a 3% loss are equally visible.
/// NaN, from a kernel with no samples, never flags.
///
/// The bootstrap interval is deliberately not part of this rule. With the
/// five-to-seven rounds a run can afford, a 95% percentile interval over the
/// median is close to [min, max] of the ratios and flags only when every
/// round agrees, which a host that spoils one run in ten rarely allows.
pub fn verdict(median_ratio: f64, min_ratio: f64, threshold: f64) -> Verdict {
    let up = 1.0 + threshold;
    let down = 1.0 / up;
    if median_ratio > up && min_ratio > up {
        Verdict::Slower
    } else if median_ratio < down && min_ratio < down {
        Verdict::Faster
    } else {
        Verdict::Same
    }
}

/// Robust scatter of the per-round ratios, as a fraction: 1.4826 times the
/// median absolute deviation (the normal-consistent scale), over the median.
/// It is the run's own measure of what an A/A comparison would show. `None`
/// below [`MIN_ROUNDS`] ratios or on a non-finite one.
pub fn noise(ratios: &[f64]) -> Option<f64> {
    if ratios.len() < MIN_ROUNDS || ratios.iter().any(|r| !r.is_finite()) {
        return None;
    }
    let m = median(ratios);
    let dev: Vec<f64> = ratios.iter().map(|r| (r - m).abs()).collect();
    Some(1.4826 * median(&dev) / m.abs())
}

/// The A/A self-check. `noise` is what an A/A comparison of this run would
/// show, so a result has to stand clear of it:
///
/// - "No flag" only means "no change" when the scatter is smaller than the
///   effect being looked for, the threshold. Otherwise a change of that size
///   could be in there: inconclusive.
/// - A flag only stands when its effect, the weaker of the two ratios that
///   passed, is larger than the scatter. Otherwise the same build could have
///   produced it: inconclusive.
pub fn conclude(v: Verdict, median_ratio: f64, min_ratio: f64, noise: Option<f64>, threshold: f64) -> Verdict {
    let Some(n) = noise else { return v };
    match v {
        Verdict::Same if n > threshold => Verdict::Inconclusive,
        Verdict::Slower if n > median_ratio.min(min_ratio) - 1.0 => Verdict::Inconclusive,
        Verdict::Faster if n > 1.0 - median_ratio.max(min_ratio) => Verdict::Inconclusive,
        _ => v,
    }
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
    pin: Option<String>,
    perf: bool,
    timeout: u64,
    extra: Vec<String>,
    scratch: PathBuf,
}

fn run_once(bin: &Path, ctx: &Ctx, tag: &str) -> Result<Sample, String> {
    let mut argv: Vec<std::ffi::OsString> = Vec::new();
    if let Some(cpus) = &ctx.pin {
        argv.extend(["taskset".into(), "-c".into(), cpus.into()]);
    }
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
    let p = parse_block(&stdout).map_err(|e| {
        format!("{}: {e}\n--- last 20 lines of emulator output ---\n{}",
                bin.display(), super::tail(&stdout, 20))
    })?;

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
    /// Median of the per-round B/A ratios, its 95% bootstrap interval, and the
    /// scatter of the ratios. For reading and for the INCONCLUSIVE check; the
    /// flag itself comes from `median_ratio` and `min_ratio`.
    pub paired_ratio: f64,
    pub noise: Option<f64>,
    pub ci: Option<[f64; 2]>,
    pub verdict: Verdict,
    /// Guest instructions per iteration, median per side. Differs only when
    /// the guest code differs or the emulator miscounts.
    pub ipi_a: f64,
    pub ipi_b: f64,
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
    /// Rounds the summary is built from. Below [`MIN_ROUNDS`] there is no interval
    /// and nothing is flagged.
    pub rounds: usize,
    /// Kernels whose checksum did not verify on some run.
    pub bad_status: Vec<String>,
}

fn per_insn(k: &KernelSample) -> Option<f64> {
    (k.icount > 0 && k.ns > 0 && k.status != "SKIP").then(|| k.ns as f64 / k.icount as f64)
}

/// `a` and `b` hold one entry per round, `None` where that run had no usable
/// figure for the kernel. Only rounds where both sides have one are paired.
fn summarise_series(name: &str, a: &[Option<(f64, f64)>], b: &[Option<(f64, f64)>], thr: f64) -> KernelSummary {
    let side = |v: &[Option<(f64, f64)>]| -> Vec<(f64, f64)> { v.iter().flatten().copied().collect() };
    let (sa, sb) = (side(a), side(b));
    let (ca, cb) = (sa.iter().map(|x| x.0).collect::<Vec<_>>(), sb.iter().map(|x| x.0).collect::<Vec<_>>());
    let (a_cost, b_cost) = (Cost::of(&ca), Cost::of(&cb));
    let ratios: Vec<f64> = a.iter().zip(b).filter_map(|(x, y)| Some(y.as_ref()?.0 / x.as_ref()?.0)).collect();
    let ci = bootstrap_ci(&ratios, BOOT_RESAMPLES, BOOT_CONFIDENCE, BOOT_SEED);
    let (median_ratio, min_ratio) = (b_cost.median / a_cost.median, b_cost.min / a_cost.min);
    let noise = noise(&ratios);
    KernelSummary {
        name: name.to_string(),
        verdict: if ratios.len() < MIN_ROUNDS {
            Verdict::Same
        } else {
            conclude(verdict(median_ratio, min_ratio, thr), median_ratio, min_ratio, noise, thr)
        },
        median_ratio,
        min_ratio,
        noise,
        paired_ratio: median(&ratios),
        ci: ci.map(|(lo, hi)| [lo, hi]),
        ipi_a: median(&sa.iter().map(|x| x.1).collect::<Vec<_>>()),
        ipi_b: median(&sb.iter().map(|x| x.1).collect::<Vec<_>>()),
        a: a_cost,
        b: b_cost,
    }
}

pub fn summarise(rounds: &[Round<Sample>], thr: f64) -> Summary {
    let mut names: Vec<String> = Vec::new();
    for k in &rounds[0].a.kernels {
        if per_insn(k).is_some() { names.push(k.name.clone()); }
    }
    let series = |side: Side, name: &str| -> Vec<Option<(f64, f64)>> {
        rounds.iter().map(|r| {
            let s = if side == Side::A { &r.a } else { &r.b };
            let k = s.kernels.iter().find(|k| k.name == name)?;
            Some((per_insn(k)?, k.icount as f64 / k.iters.max(1) as f64))
        }).collect()
    };
    let kernels = names.iter()
        .map(|n| summarise_series(n, &series(Side::A, n), &series(Side::B, n), thr))
        .collect();
    let tot = |side: Side| -> Vec<Option<(f64, f64)>> {
        rounds.iter().map(|r| {
            let s = if side == Side::A { &r.a } else { &r.b };
            Some((s.total_ns as f64 / s.total_icount.max(1) as f64, s.total_icount as f64))
        }).collect()
    };
    let total = summarise_series("(suite)", &tot(Side::A), &tot(Side::B), thr);

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
pub fn table(s: &Summary, la: &str, lb: &str, thr: f64) -> String {
    let mut o = String::new();
    o.push_str(&format!(
        "| kernel | {la} med | {lb} med | {la} best | {lb} best | med B/A | best B/A | paired B/A [95% CI] | noise | flag |\n\
         |---|---:|---:|---:|---:|---:|---:|---|---:|---|\n"));
    let row = |k: &KernelSummary| -> String {
        let flag = match k.verdict {
            Verdict::Slower => "SLOWER",
            Verdict::Faster => "faster",
            Verdict::Same => "",
            Verdict::Inconclusive => "inconclusive",
        };
        let ci = match k.ci {
            Some([lo, hi]) => format!("{} [{}, {}]", pct(k.paired_ratio), pct(lo), pct(hi)),
            None => "n/a".to_string(),
        };
        let noise = k.noise.map_or("n/a".to_string(), |n| format!("{:.1}%", n * 100.0));
        format!("| {} | {:.1} | {:.1} | {:.1} | {:.1} | {} | {} | {} | {} | {} |\n",
                k.name, mips(k.a.median), mips(k.b.median), mips(k.a.min), mips(k.b.min),
                pct(k.median_ratio), pct(k.min_ratio), ci, noise, flag)
    };
    for k in &s.kernels { o.push_str(&row(k)); }
    o.push_str(&row(&s.total));
    o.push_str(&format!("\nRates are guest MIPS (guest instructions per host microsecond). \
        \"best\" is the fastest round. Ratios are cost per guest instruction, B over A: \
        positive means B is slower. A kernel is flagged when the median and best ratios both \
        pass {:.1}% in the same direction. \"paired\" is the median of the per-round B/A ratios \
        with its bootstrap interval ({} resamples of the rounds, fixed seed); it is shown for \
        reading and does not decide the flag. \"noise\" is the robust scatter of those ratios \
        (1.4826 x MAD / median), the run's own estimate of an A/A result. A row is \
        \"inconclusive\" when its noise is above the threshold and nothing is flagged, or above \
        the size of the change that was flagged. More rounds help.\n",
        thr * 100.0, BOOT_RESAMPLES));
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
    if s.rounds < MIN_ROUNDS {
        o.push_str(&format!("\nOnly {} round(s): too few to flag anything. Use at least {MIN_ROUNDS}, preferably 5 or more.\n", s.rounds));
    }
    let drift: Vec<&str> = s.kernels.iter()
        .filter(|k| (k.ipi_a - k.ipi_b).abs() > 0.005 * k.ipi_a.max(1.0))
        .map(|k| k.name.as_str()).collect();
    if !drift.is_empty() {
        o.push_str(&format!(
            "\nWARNING: guest instructions per iteration differ between builds for {}. \
             The guest code is not the same, or one emulator miscounts; the ratios above \
             do not compare like with like.\n", drift.join(", ")));
    }
    if !s.bad_status.is_empty() {
        o.push_str(&format!("\nWARNING: checksum not OK: {}\n", s.bad_status.join(", ")));
    }
    if let Some(n) = s.total.noise.filter(|n| *n > thr) {
        o.push_str(&format!(
            "\nWARNING: the whole-run speed scattered by {:.1}% between rounds, so this host was \
             noisy. Differences smaller than that, in any row, are unreliable; run more rounds \
             (9 or more) or on quieter cores.\n", n * 100.0));
    }
    let flagged: Vec<String> = s.kernels.iter()
        .filter(|k| matches!(k.verdict, Verdict::Slower | Verdict::Faster))
        .map(|k| format!("{} {}", k.name, if k.verdict == Verdict::Slower { "slower" } else { "faster" }))
        .collect();
    o.push_str(&if flagged.is_empty() {
        "\nNo kernel flagged.\n".to_string()
    } else {
        format!("\nFlagged: {}.\n", flagged.join(", "))
    });
    let unsure: Vec<&str> = s.kernels.iter().chain([&s.total])
        .filter(|k| k.verdict == Verdict::Inconclusive).map(|k| k.name.as_str()).collect();
    if !unsure.is_empty() {
        o.push_str(&format!("Inconclusive (the run's scatter is larger than the effect): {}.\n",
                            unsure.join(", ")));
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
    /// this `iris-bench`, written next to the results; the output names it.
    /// Hold it fixed when the builds span a rebuild of the image, which moves
    /// the rates on its own.
    #[arg(long)]
    elf: Option<PathBuf>,
    /// Machine config. Defaults to bench/run/bare.toml.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Rounds; each runs A and B once.
    #[arg(long, default_value_t = 5)]
    rounds: usize,
    /// Pin every run to these host cores, as a `taskset -c` list ("8-14").
    /// Linux only. Give both builds the same cores and keep them free of
    /// SMT siblings.
    #[arg(long)]
    pin: Option<String>,
    /// Emulated CPU, forwarded to the emulator as `--cpu`.
    #[arg(long)]
    cpu: Option<String>,
    /// Flag a kernel when its median and best ratios both pass this percentage;
    /// also the noise level above which a row without a flag is "inconclusive".
    #[arg(long, default_value_t = 3.0)]
    threshold: f64,
    /// Names for the table columns and files.
    #[arg(long, default_value = "A")]
    label_a: String,
    #[arg(long, default_value = "B")]
    label_b: String,
    /// Seed for the A/B order. Default: from the clock. Recorded in the JSON.
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
    pin: &'a Option<String>,
    cpu: &'a Option<String>,
    perf: bool,
    threshold_pct: f64,
    bootstrap: Bootstrap,
    seed: u64,
    host: serde_json::Value,
    rounds: &'a [Round<Sample>],
    summary: &'a Summary,
}

#[derive(Serialize)]
struct Bootstrap {
    resamples: usize,
    confidence: f64,
    seed: u64,
    statistic: &'static str,
}

#[derive(Serialize)]
struct Side_<'a> {
    label: &'a str,
    bin: String,
}

pub fn run(args: AbArgs) -> Result<(), String> {
    if args.rounds == 0 { return Err("--rounds must be at least 1".into()); }
    if args.pin.is_some() && !cfg!(target_os = "linux") {
        return Err("--pin needs taskset, which is Linux only".into());
    }
    for (what, p) in [("A", &args.a), ("B", &args.b)] {
        if !p.exists() { return Err(format!("no emulator for {what} at {}", p.display())); }
    }
    let abs = |p: &Path| std::fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()));
    let out = args.out.clone().unwrap_or_else(super::default_out);
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let stem = format!("ab-{}-vs-{}", args.label_a, args.label_b);

    let (elf, elf_source) = match &args.elf {
        Some(p) => (abs(p)?, "--elf"),
        None => {
            let p = out.join(format!("{stem}.elf"));
            std::fs::write(&p, iris::benchsuite::SUITE_ELF)
                .map_err(|e| format!("{}: {e}", p.display()))?;
            (abs(&p)?, "the image linked into this iris-bench")
        }
    };
    let suite_id = super::suite_id_of(&elf)?;
    let config = abs(&args.config.clone().unwrap_or_else(|| super::repo_relative("bench/run/bare.toml")))?;
    let cwd = config.parent().and_then(|p| p.parent()).map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let (bin_a, bin_b) = (abs(&args.a)?, abs(&args.b)?);

    let perf = !args.no_perf && perf_usable();
    let mut extra = Vec::new();
    if let Some(c) = &args.cpu { extra.extend(["--cpu".to_string(), c.clone()]); }
    extra.extend(args.extra.iter().cloned());
    let ctx = Ctx {
        elf: elf.clone(), config, cwd, pin: args.pin.clone(), perf,
        timeout: args.timeout, extra, scratch: out.clone(),
    };

    let seed = args.seed.unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64).unwrap_or(1)
    });
    println!("A/B: {} = {}\n     {} = {}", args.label_a, bin_a.display(), args.label_b, bin_b.display());
    println!("guest image: {} ({}), from {elf_source}", elf.display(), suite_id);
    println!("rounds {}, pin {}, perf {}, threshold {:.1}%", args.rounds,
             args.pin.as_deref().unwrap_or("none"), if perf { "on" } else { "off" }, args.threshold);

    let mut rng = Rng::new(seed);
    let rounds = paired_rounds(args.rounds, &mut rng, |side, round| {
        let (bin, label) = match side { Side::A => (&bin_a, &args.label_a), Side::B => (&bin_b, &args.label_b) };
        let s = run_once(bin, &ctx, &format!("{stem}-r{round}-{label}"))?;
        println!("round {round}/{} {label}: {:.1} MIPS, {:.0}s", args.rounds,
                 s.total_icount as f64 * 1e3 / s.total_ns.max(1) as f64, s.wall_s);
        Ok(s)
    })?;

    let summary = summarise(&rounds, args.threshold / 100.0);
    let text = table(&summary, &args.label_a, &args.label_b, args.threshold / 100.0);
    let report = Report {
        tool: "iris-bench ab",
        a: Side_ { label: &args.label_a, bin: bin_a.display().to_string() },
        b: Side_ { label: &args.label_b, bin: bin_b.display().to_string() },
        elf: elf.display().to_string(),
        suite_id: suite_id.clone(),
        elf_source,
        pin: &args.pin,
        cpu: &args.cpu,
        perf,
        threshold_pct: args.threshold,
        bootstrap: Bootstrap {
            resamples: BOOT_RESAMPLES, confidence: BOOT_CONFIDENCE, seed: BOOT_SEED,
            statistic: "median of per-round B/A cost ratios, percentile interval; informational",
        },
        seed,
        host: serde_json::to_value(host_info()).map_err(|e| e.to_string())?,
        rounds: &rounds,
        summary: &summary,
    };
    let json_path = out.join(format!("{stem}.json"));
    let md_path = out.join(format!("{stem}.md"));
    std::fs::write(&json_path, serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?)
        .map_err(|e| format!("{}: {e}", json_path.display()))?;
    let header = format!("guest image {suite_id} (from {elf_source}), {} rounds, pin {}\n\n",
                         args.rounds, args.pin.as_deref().unwrap_or("none"));
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
    fn flag_needs_both_ratios_past_the_threshold() {
        let t = 0.03;
        assert_eq!(verdict(1.10, 1.09, t), Verdict::Slower);
        // Median moved, best run did not: host load, not the build.
        assert_eq!(verdict(1.10, 1.01, t), Verdict::Same);
        // Best run moved, median did not: one lucky sample.
        assert_eq!(verdict(1.01, 1.10, t), Verdict::Same);
        // Opposite directions never agree.
        assert_eq!(verdict(1.10, 0.90, t), Verdict::Same);
        assert_eq!(verdict(0.90, 0.91, t), Verdict::Faster);
    }

    #[test]
    fn threshold_is_exclusive_and_symmetric() {
        let t = 0.05;
        assert_eq!(verdict(1.05, 1.05, t), Verdict::Same);
        assert_eq!(verdict(1.0501, 1.0501, t), Verdict::Slower);
        // A 5% slowdown and the matching speedup are equally visible.
        assert_eq!(verdict(1.0 / 1.0501, 1.0 / 1.0501, t), Verdict::Faster);
        assert_eq!(verdict(1.0, 1.0, t), Verdict::Same);
    }

    #[test]
    fn noise_is_the_scaled_mad_over_the_median() {
        // Median 1.0, absolute deviations 0.1 0 0.1 0.1 0.2 -> MAD 0.1.
        let n = noise(&[0.9, 1.0, 1.1, 1.1, 0.8]).unwrap();
        assert!((n - 0.14826).abs() < 1e-9, "{n}");
        // One 30% round among steady ones barely registers.
        assert!(noise(&[1.0, 1.01, 0.99, 1.0, 1.3]).unwrap() < 0.02);
        assert_eq!(noise(&[1.0; 5]), Some(0.0));
        assert_eq!(noise(&[1.0, 1.1]), None);
        assert_eq!(noise(&[1.0, f64::NAN, 1.0]), None);
    }

    #[test]
    fn a_quiet_pass_stays_a_pass_and_a_noisy_one_is_inconclusive() {
        let t = 0.03;
        assert_eq!(conclude(Verdict::Same, 1.0, 1.0, Some(0.01), t), Verdict::Same);
        assert_eq!(conclude(Verdict::Same, 1.0, 1.0, Some(0.0301), t), Verdict::Inconclusive);
        assert_eq!(conclude(Verdict::Same, 1.0, 1.0, None, t), Verdict::Same);
    }

    #[test]
    fn a_flag_stands_only_clear_of_the_noise() {
        let t = 0.03;
        // Weaker of the two ratios is +8%.
        assert_eq!(conclude(Verdict::Slower, 1.12, 1.08, Some(0.05), t), Verdict::Slower);
        assert_eq!(conclude(Verdict::Slower, 1.12, 1.08, Some(0.09), t), Verdict::Inconclusive);
        // Faster: weaker is 0.92, an 8% gain.
        assert_eq!(conclude(Verdict::Faster, 0.88, 0.92, Some(0.05), t), Verdict::Faster);
        assert_eq!(conclude(Verdict::Faster, 0.88, 0.92, Some(0.09), t), Verdict::Inconclusive);
        assert_eq!(conclude(Verdict::Slower, 1.12, 1.08, None, t), Verdict::Slower);
    }

    const SEED: u64 = 42;
    fn ci(r: &[f64]) -> Option<(f64, f64)> { bootstrap_ci(r, 10_000, 0.95, SEED) }

    #[test]
    fn bootstrap_is_reproducible_for_a_seed() {
        let r = [1.01, 0.98, 1.03, 0.99, 1.00, 1.02, 0.97];
        assert_eq!(ci(&r), ci(&r));
        assert_eq!(bootstrap_ci(&r, 10_000, 0.95, 7), bootstrap_ci(&r, 10_000, 0.95, 7));
    }

    #[test]
    fn a_vs_a_interval_straddles_one() {
        // Same build both sides: ratios scatter around 1, one of them a 30%
        // slow run, as on a busy host.
        let r = [1.01, 0.99, 1.30, 0.98, 1.02, 1.00, 0.99];
        let (lo, hi) = ci(&r).unwrap();
        assert!(lo <= 1.0 && hi >= 1.0, "[{lo}, {hi}]");
    }

    #[test]
    fn a_clear_shift_has_an_interval_that_excludes_one() {
        let r = [1.10, 1.12, 1.09, 1.11, 1.10];
        let (lo, hi) = ci(&r).unwrap();
        assert!(lo > 1.03 && hi < 1.15, "[{lo}, {hi}]");
        assert!(lo <= median(&r) && median(&r) <= hi);
    }

    #[test]
    fn five_rounds_collapse_the_interval_to_the_range() {
        // The reason the interval is not the flag rule: with n = 5 the 95%
        // percentile interval of the median is [min, max] of the ratios, so
        // one spoiled round keeps it from excluding 1.0.
        let r = [1.07, 1.07, 1.00, 1.07, 1.07];
        let (lo, hi) = ci(&r).unwrap();
        assert_eq!((lo, hi), (1.00, 1.07));
    }

    #[test]
    fn fewer_than_three_rounds_give_no_interval() {
        assert_eq!(ci(&[]), None);
        assert_eq!(ci(&[1.2]), None);
        assert_eq!(ci(&[1.2, 1.2]), None);
        assert!(ci(&[1.2, 1.2, 1.2]).is_some());
        assert_eq!(ci(&[1.0, f64::NAN, 1.0]), None);
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
    fn summary_flags_a_steady_regression_and_ignores_one_slow_round() {
        // B is 10% slower every round.
        let s = summarise(&rounds_of(&[10.0; 5], &[11.0; 5]), 0.03);
        assert!(s.kernels.iter().all(|k| k.verdict == Verdict::Slower));
        assert!((s.total.median_ratio - 1.1).abs() < 1e-9);
        assert!((s.total.paired_ratio - 1.1).abs() < 1e-9);
        let [lo, hi] = s.total.ci.unwrap();
        assert!(lo > 1.03 && hi < 1.2);
        assert_eq!(s.total.noise, Some(0.0));
        let h = s.host.as_ref().unwrap();
        assert!((h.b_insn / h.a_insn - 1.1).abs() < 1e-9);

        // Same build, one B round 30% slow: the median of the ratios stays put.
        let s = summarise(&rounds_of(&[10.0; 5], &[10.0, 10.0, 13.0, 10.0, 10.0]), 0.03);
        assert!(s.kernels.iter().all(|k| k.verdict == Verdict::Same));
    }

    #[test]
    fn a_noisy_a_vs_a_run_is_inconclusive_and_a_quiet_one_is_not() {
        let noisy = [10.0, 13.0, 8.0, 10.0, 12.0, 9.0, 10.0];
        let s = summarise(&rounds_of(&[10.0; 7], &noisy), 0.03);
        assert!(s.kernels.iter().all(|k| k.verdict == Verdict::Inconclusive), "{:?}", s.kernels[0]);
        let t = table(&s, "A", "B", 0.03);
        assert!(t.contains("Inconclusive") && t.contains("WARNING: the whole-run speed scattered"), "{t}");
        let s = summarise(&rounds_of(&[10.0; 7], &[10.0; 7]), 0.03);
        assert!(s.kernels.iter().all(|k| k.verdict == Verdict::Same));
        assert!(!table(&s, "A", "B", 0.03).contains("Inconclusive"));
        assert!(!table(&s, "A", "B", 0.03).contains("WARNING"));
    }

    #[test]
    fn too_few_rounds_flag_nothing() {
        let s = summarise(&rounds_of(&[10.0; 2], &[12.0; 2]), 0.03);
        assert!(s.kernels.iter().all(|k| k.verdict == Verdict::Same));
        assert!(table(&s, "A", "B", 0.03).contains("too few to flag"));
        let s = summarise(&rounds_of(&[10.0; 3], &[12.0; 3]), 0.03);
        assert!(s.kernels.iter().all(|k| k.verdict == Verdict::Slower));
    }

    #[test]
    fn only_a_mismatch_is_a_bad_checksum() {
        let mut r = rounds_of(&[10.0; 3], &[10.0; 3]);
        for x in &mut r { x.a.kernels[0].status = "UNCHECKED".into(); }
        assert!(summarise(&r, 0.03).bad_status.is_empty());
        r[1].b.kernels[1].status = "MISMATCH".into();
        assert_eq!(summarise(&r, 0.03).bad_status, vec!["img/resize (B: MISMATCH)".to_string()]);
    }

    #[test]
    fn missing_perf_leaves_the_host_block_out() {
        let mut r = rounds_of(&[10.0; 3], &[10.0; 3]);
        r[1].b.host_cycles = None;
        let s = summarise(&r, 0.03);
        assert!(s.host.is_none());
        assert!(table(&s, "A", "B", 0.03).contains("perf unavailable"));
    }

    #[test]
    fn differing_instructions_per_iteration_are_called_out() {
        let mut r = rounds_of(&[10.0; 3], &[10.0; 3]);
        for x in &mut r { x.b.kernels[0].icount = 13_000; x.b.kernels[0].ns = 130_000; }
        let t = table(&summarise(&r, 0.03), "A", "B", 0.03);
        assert!(t.contains("WARNING: guest instructions per iteration differ"), "{t}");
        assert!(t.contains("int/alu"));
    }
}
