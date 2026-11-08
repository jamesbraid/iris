//! `iris-bench ab` — a paired comparison of two emulator builds.
//!
//! The question is "is B slower than A?", asked on a host that is busy with
//! other work and has no quiet cores to give. Three things make the usual
//! answer, two sequential runs and a glance at the totals, untrustworthy:
//!
//!   - **The host changes under the runs.** Load moves within seconds, and
//!     another process on a core's SMT sibling or on the shared cache slows
//!     ours by tens of percent. So A and B run at the same moment, each on its
//!     own physical cores, one thread per core, inside one last-level-cache
//!     group, and the verdict is read from the per-round B/A ratio, which
//!     cancels what both sides see. The cores are re-picked each round and the
//!     sides assigned at random, so what one core suffers alone averages out.
//!     Where the topology cannot be read (not Linux, or no two cores share a
//!     cache) A and B run one at a time in a random order each round instead,
//!     and the output says so.
//!   - **Outliers.** A row is judged by a bootstrap interval of the median
//!     per-round ratio, which one spoiled round in ten barely moves, and is
//!     INCONCLUSIVE when that interval is too wide to say. Each kernel's timed
//!     run is short (25 ms by default) and repeated three times, the guest
//!     keeping the fastest, so a burst of other work spoils one repeat rather
//!     than the sample.
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
//! The pairing loops, [`concurrent_rounds`] and [`paired_rounds`], do not know
//! what a run is. A later `iris-bench irix` comparison can reuse them with its
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Slower,
    Faster,
    Same,
    /// The interval is too wide to say either: a change of the threshold's size
    /// could be hiding in it.
    Inconclusive,
}

/// Bootstrap settings. The seed is fixed, not taken from the run's order seed,
/// so the same rounds always print the same interval. 99% rather than 95%
/// because a table has 46 kernels: at 95% an A/A run would flag about two of
/// them by chance.
pub const BOOT_RESAMPLES: usize = 10_000;
pub const BOOT_CONFIDENCE: f64 = 0.99;
pub const BOOT_SEED: u64 = 0x1215_B007_57A9_0001;

/// With fewer rounds an interval over the median says nothing: three rounds
/// is the least that can disagree with itself.
pub const MIN_ROUNDS: usize = 3;

/// Fewest rounds that can give a verdict. Below about eight the 99% interval
/// of the median is the range of the ratios, so it excludes 1 whenever every
/// round agrees, which an A/A run of three to seven rounds does by chance in
/// several of its 46 rows. Eight agreeing rounds has a chance under 1%.
pub const MIN_VERDICT_ROUNDS: usize = 8;

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

/// The verdict for one row, from the per-round B/A cost ratios (above 1 is
/// slower) and the effect size `threshold` (0.03 for 3%).
///
/// - SLOWER: the [`BOOT_CONFIDENCE`] bootstrap interval of the median ratio
///   lies above 1 and the median itself is past 1 + threshold. FASTER is the
///   mirror image, 1/(1 + threshold), so a 3% gain and a 3% loss are equally
///   visible.
/// - SAME: the whole interval lies within the threshold either way, so a
///   change of that size would have shown.
/// - INCONCLUSIVE: anything else, including fewer than [`MIN_VERDICT_ROUNDS`]
///   ratios.
///   "No flag" is never reported as "no change" when the run cannot tell.
///
/// Unlike the per-round scatter, the interval narrows with more rounds, so a
/// noisy host still reaches a verdict given enough of them.
pub fn decide(ratios: &[f64], threshold: f64) -> Verdict {
    if ratios.len() < MIN_VERDICT_ROUNDS { return Verdict::Inconclusive; }
    let Some((lo, hi)) = bootstrap_ci(ratios, BOOT_RESAMPLES, BOOT_CONFIDENCE, BOOT_SEED) else {
        return Verdict::Inconclusive;
    };
    let m = median(ratios);
    let up = 1.0 + threshold;
    let down = 1.0 / up;
    if lo > 1.0 && m > up {
        Verdict::Slower
    } else if hi < 1.0 && m < down {
        Verdict::Faster
    } else if lo >= down && hi <= up {
        Verdict::Same
    } else {
        Verdict::Inconclusive
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
    /// The order the sides ran in this round. Meaningless when `together`.
    pub order: [Side; 2],
    /// A and B ran at the same time, on the cores in `cpus`.
    pub together: bool,
    /// Logical CPUs each side was pinned to, A then B; empty when unpinned or
    /// pinned by `--pin`.
    pub cpus: [Vec<usize>; 2],
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
        out.push(Round { round, order, together: false, cpus: [vec![], vec![]], a: a.unwrap(), b: b.unwrap() });
    }
    Ok(out)
}

/// Run A and B at the same moment each round, A on the CPUs `place` gives
/// first and B on the second, so both see the same host: whatever else is
/// running slows both, and the per-round ratio cancels it. `place` is called
/// once per round, so the cores, and which side gets which, can change. Stops
/// at the first error, after both runs of that round have ended.
pub fn concurrent_rounds<T: Send>(
    rounds: usize,
    rng: &mut Rng,
    mut place: impl FnMut(&mut Rng) -> [Vec<usize>; 2],
    f: impl Fn(Side, usize, &[usize]) -> Result<T, String> + Sync,
    mut done: impl FnMut(&Round<T>),
) -> Result<Vec<Round<T>>, String> {
    let mut out = Vec::with_capacity(rounds);
    for round in 1..=rounds {
        let [ca, cb] = place(rng);
        let (a, b) = std::thread::scope(|s| {
            let ha = s.spawn(|| f(Side::A, round, &ca));
            let hb = s.spawn(|| f(Side::B, round, &cb));
            (ha.join().unwrap_or_else(|_| Err("run thread panicked".into())),
             hb.join().unwrap_or_else(|_| Err("run thread panicked".into())))
        });
        let r = Round { round, order: [Side::A, Side::B], together: true, cpus: [ca, cb], a: a?, b: b? };
        done(&r);
        out.push(r);
    }
    Ok(out)
}

// ─── host topology ───────────────────────────────────────────────────────────
//
// Concurrent pairs need two sets of physical cores that share a last-level
// cache, so that load from other processes on that cache, and on the memory
// behind it, lands on both sides. Everything here is read at run time; nothing
// knows about a particular machine.

/// A physical core: the logical CPUs (SMT threads) that share it, those this
/// process may run on first, each part ascending. A side is pinned to the first.
pub type Core = Vec<usize>;

/// A Linux CPU list ("0-6,112-118") as CPU numbers. `None` if malformed.
pub fn parse_cpu_list(s: &str) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (usize, usize) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
                if b < a { return None; }
                out.extend(a..=b);
            }
            None => out.push(part.trim().parse().ok()?),
        }
    }
    Some(out)
}

pub fn format_cpu_list(cpus: &[usize]) -> String {
    let mut v = cpus.to_vec();
    v.sort_unstable();
    v.dedup();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let mut j = i;
        while j + 1 < v.len() && v[j + 1] == v[j] + 1 { j += 1; }
        out.push(if j > i { format!("{}-{}", v[i], v[j]) } else { v[i].to_string() });
        i = j + 1;
    }
    out.join(",")
}

/// The allowed logical CPUs grouped into physical cores, and the cores grouped
/// by the last-level cache they share. `read` returns the contents of a file
/// under `/sys/devices/system/cpu/`. The last level is the highest cache
/// `level` a CPU lists; on a chiplet part that is the L3 of one CCD. Empty
/// when the files are missing, which callers treat as "no topology".
pub fn llc_groups(allowed: &[usize], read: impl Fn(&str) -> Option<String>) -> Vec<Vec<Core>> {
    let mut groups: Vec<(Vec<usize>, Vec<Core>)> = Vec::new();
    for &c in allowed {
        let Some(mut core) = read(&format!("cpu{c}/topology/thread_siblings_list")).and_then(|t| parse_cpu_list(&t))
        else { return Vec::new() };
        core.sort_by_key(|x| (!allowed.contains(x), *x));
        let mut llc: Option<(u32, Vec<usize>)> = None;
        for i in 0..16 {
            let Some(level) = read(&format!("cpu{c}/cache/index{i}/level")) else { break };
            let Ok(level) = level.trim().parse::<u32>() else { continue };
            let Some(shared) = read(&format!("cpu{c}/cache/index{i}/shared_cpu_list"))
                .and_then(|t| parse_cpu_list(&t)) else { continue };
            if llc.as_ref().is_none_or(|(l, _)| level > *l) { llc = Some((level, shared)); }
        }
        let Some((_, llc)) = llc else { return Vec::new() };
        let g = match groups.iter_mut().position(|(k, _)| *k == llc) {
            Some(i) => &mut groups[i].1,
            None => { groups.push((llc, Vec::new())); &mut groups.last_mut().unwrap().1 }
        };
        if !g.contains(&core) { g.push(core); }
    }
    let mut out: Vec<Vec<Core>> = groups.into_iter().map(|(_, mut g)| { g.sort(); g }).collect();
    out.sort();
    out
}

/// Per-CPU (total, idle) jiffies from `/proc/stat` text.
pub fn cpu_times(stat: &str) -> std::collections::HashMap<usize, (u64, u64)> {
    stat.lines().filter_map(|l| {
        let mut f = l.split_whitespace();
        let cpu: usize = f.next()?.strip_prefix("cpu")?.parse().ok()?;
        let v: Vec<u64> = f.filter_map(|x| x.parse().ok()).collect();
        // user nice system idle iowait ...: idle and iowait are not busy.
        (v.len() >= 5).then(|| (cpu, (v.iter().sum(), v[3] + v[4])))
    }).collect()
}

/// How busy each CPU was between two `/proc/stat` readings, 0 to 1.
pub fn busy_between(
    a: &std::collections::HashMap<usize, (u64, u64)>,
    b: &std::collections::HashMap<usize, (u64, u64)>,
) -> std::collections::HashMap<usize, f64> {
    b.iter().filter_map(|(c, (tb, ib))| {
        let (ta, ia) = a.get(c)?;
        let dt = tb.saturating_sub(*ta);
        (dt > 0).then(|| (*c, 1.0 - ib.saturating_sub(*ia) as f64 / dt as f64))
    }).collect()
}

/// A core is as busy as its busiest thread: another process on the sibling
/// thread slows ours as much as one on our own.
pub fn core_load(core: &Core, busy: &std::collections::HashMap<usize, f64>) -> f64 {
    core.iter().map(|c| busy.get(c).copied().unwrap_or(0.0)).fold(0.0, f64::max)
}

/// How a concurrent run lays out its sides.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// The cores of the last-level-cache group the run uses.
    pub group: Vec<Core>,
    /// Physical cores per side.
    pub per_side: usize,
}

/// Pick the group and the cores per side. Two cores per side when some group
/// has four, so the emulator's helper threads do not take turns with its CPU
/// thread; else one; `None` when no group has two cores. Of the groups that
/// fit, the one whose least busy cores are least busy.
pub fn choose_layout(groups: &[Vec<Core>], busy: &std::collections::HashMap<usize, f64>) -> Option<Layout> {
    let per_side = if groups.iter().any(|g| g.len() >= 4) { 2 } else if groups.iter().any(|g| g.len() >= 2) { 1 } else { return None };
    let cost = |g: &Vec<Core>| -> f64 {
        let mut l: Vec<f64> = g.iter().map(|c| core_load(c, busy)).collect();
        l.sort_by(|a, b| a.total_cmp(b));
        l.iter().take(2 * per_side).sum()
    };
    groups.iter().filter(|g| g.len() >= 2 * per_side)
        .min_by(|a, b| cost(a).total_cmp(&cost(b)))
        .map(|g| Layout { group: g.clone(), per_side })
}

/// This round's CPUs: the 2 x `per_side` least busy cores of the group, shuffled
/// and split between A and B. Each side gets one logical CPU per core, the
/// first, and never its SMT sibling.
pub fn pick_sides(layout: &Layout, busy: &std::collections::HashMap<usize, f64>, rng: &mut Rng) -> [Vec<usize>; 2] {
    let mut cores: Vec<&Core> = layout.group.iter().collect();
    cores.sort_by(|a, b| core_load(a, busy).total_cmp(&core_load(b, busy)));
    let mut pick: Vec<usize> = cores.iter().take(2 * layout.per_side).map(|c| c[0]).collect();
    for i in (1..pick.len()).rev() {
        let j = (rng.next() >> 33) as usize % (i + 1);
        pick.swap(i, j);
    }
    let b = pick.split_off(layout.per_side);
    [pick, b]
}

#[cfg(target_os = "linux")]
fn allowed_cpus() -> Vec<usize> {
    // SAFETY: a zeroed cpu_set_t is a valid empty set, and the call only
    // writes into it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return Vec::new();
        }
        (0..libc::CPU_SETSIZE as usize).filter(|&c| libc::CPU_ISSET(c, &set)).collect()
    }
}

#[cfg(target_os = "linux")]
fn sample_busy() -> std::collections::HashMap<usize, f64> {
    let read = || std::fs::read_to_string("/proc/stat").map(|t| cpu_times(&t)).unwrap_or_default();
    let a = read();
    std::thread::sleep(std::time::Duration::from_millis(250));
    busy_between(&a, &read())
}

#[cfg(not(target_os = "linux"))]
fn sample_busy() -> std::collections::HashMap<usize, f64> { Default::default() }

/// The concurrent layout for this host, or why there is none.
#[cfg(target_os = "linux")]
fn host_layout() -> Result<Layout, String> {
    let groups = llc_groups(&allowed_cpus(), |p| std::fs::read_to_string(format!("/sys/devices/system/cpu/{p}")).ok());
    if groups.is_empty() {
        return Err("no CPU or cache topology under /sys/devices/system/cpu".into());
    }
    choose_layout(&groups, &sample_busy())
        .ok_or_else(|| "no two physical cores share a last-level cache".into())
}

#[cfg(not(target_os = "linux"))]
fn host_layout() -> Result<Layout, String> {
    Err("CPU pinning and topology are read on Linux only".into())
}

/// Pin the command's process to `cpus` before it runs. Linux only; a no-op
/// for an empty list.
#[cfg(target_os = "linux")]
fn pin(cmd: &mut Command, cpus: &[usize]) {
    use std::os::unix::process::CommandExt;
    if cpus.is_empty() { return; }
    // SAFETY: as in allowed_cpus; the set is built before the fork.
    let set = unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for &c in cpus { libc::CPU_SET(c, &mut set); }
        set
    };
    // SAFETY: sched_setaffinity is a plain system call, safe between fork and
    // exec. The pinning is inherited by perf and the emulator it starts.
    unsafe {
        cmd.pre_exec(move || {
            if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn pin(_cmd: &mut Command, _cpus: &[usize]) {}

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

fn run_once(bin: &Path, ctx: &Ctx, tag: &str, cpus: &[usize]) -> Result<Sample, String> {
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
    pin(&mut cmd, cpus);
    let started = std::time::Instant::now();
    let out = super::run_with_timeout(cmd, ctx.timeout)?;
    let wall_s = started.elapsed().as_secs_f64();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let p = parse_block(&stdout).map_err(|e| {
        format!("{}: {e}\n--- last 20 lines of emulator output ---\n{}",
                bin.display(), super::tail(&stdout, 20))
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
    /// Median of the per-round B/A ratios, its bootstrap interval, which
    /// decides the verdict, and the scatter of the ratios.
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
        verdict: decide(&ratios, thr),
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
        "| kernel | {la} med | {lb} med | {la} best | {lb} best | med B/A | best B/A | paired B/A [{ci:.0}% CI] | noise | flag |\n\
         |---|---:|---:|---:|---:|---:|---:|---|---:|---|\n", ci = BOOT_CONFIDENCE * 100.0));
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
        positive means B is slower. \"paired\" is the median of the per-round B/A ratios with \
        its {:.0}% bootstrap interval ({} resamples of the rounds, fixed seed). A row is SLOWER \
        or faster when that interval excludes no change and the median passes {:.1}%, the same \
        when the whole interval is within {:.1}% either way, and inconclusive otherwise; more \
        rounds narrow the interval. \"noise\" is the robust scatter of the per-round ratios \
        (1.4826 x MAD / median), the run's own estimate of one A/A round.\n",
        BOOT_CONFIDENCE * 100.0, BOOT_RESAMPLES, thr * 100.0, thr * 100.0));
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
    if s.rounds < MIN_VERDICT_ROUNDS {
        o.push_str(&format!("\nOnly {} round(s): too few to judge anything. Use at least {MIN_VERDICT_ROUNDS}; 20 or more on a busy host.\n", s.rounds));
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
            "\nWARNING: the whole-run B/A ratio scattered by {:.1}% between rounds, so this host was \
             noisy. Rows are judged by their interval, which accounts for that, but more rounds \
             or quieter cores are needed to resolve small changes.\n", n * 100.0));
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
        o.push_str(&format!("Inconclusive (the interval is too wide to say): {}.\n",
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
    /// Run A and B one at a time, in a random order each round, instead of
    /// together. Implied by --pin, and the fallback when the host's CPU
    /// topology cannot be read.
    #[arg(long)]
    sequential: bool,
    /// Run every process on these host CPUs, as a list ("8-14"), one at a
    /// time. Linux only. Keep them free of SMT siblings.
    #[arg(long)]
    pin: Option<String>,
    /// Emulated CPU, forwarded to the emulator as `--cpu`.
    #[arg(long)]
    cpu: Option<String>,
    /// Effect size, percent: a row is flagged when its interval excludes no
    /// change and its median ratio passes this; "same" when the whole interval
    /// is within it.
    #[arg(long, default_value_t = 3.0)]
    threshold: f64,
    /// Names for the table columns and files.
    #[arg(long, default_value = "A")]
    label_a: String,
    #[arg(long, default_value = "B")]
    label_b: String,
    /// Seed for the A/B order and core choice. Default: from the clock.
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
    mode: &'a str,
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
    let pin_list = match &args.pin {
        Some(_) if !cfg!(target_os = "linux") => return Err("--pin is Linux only".into()),
        Some(p) => Some(parse_cpu_list(p).filter(|v| !v.is_empty())
            .ok_or_else(|| format!("--pin {p}: not a CPU list"))?),
        None => None,
    };
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

    let layout = if args.sequential {
        Err("--sequential".to_string())
    } else if args.pin.is_some() {
        Err("--pin".to_string())
    } else {
        host_layout()
    };
    let mode = match &layout {
        Ok(l) => {
            let cpus: Vec<usize> = l.group.iter().flatten().copied().collect();
            format!("concurrent pairs: A and B start together, each on {} physical core(s), one \
                     thread per core, from the {} cores sharing a last-level cache (CPUs {}). The \
                     least busy cores are picked again each round and given to the sides at random.",
                    l.per_side, l.group.len(), format_cpu_list(&cpus))
        }
        Err(why) => format!("sequential interleaving, one run at a time in a random order each \
                             round ({why}){}", match &args.pin {
            Some(p) => format!(", pinned to CPUs {p}"),
            None => String::new(),
        }),
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
    println!("mode: {mode}");
    println!("rounds {}, perf {}, threshold {:.1}%", args.rounds, if perf { "on" } else { "off" }, args.threshold);

    let mut rng = Rng::new(seed);
    let mips = |s: &Sample| s.total_icount as f64 * 1e3 / s.total_ns.max(1) as f64;
    let label = |side: Side| match side { Side::A => (&bin_a, &args.label_a), Side::B => (&bin_b, &args.label_b) };
    let rounds = match &layout {
        Ok(l) => {
            let cost = |s: &Sample| s.total_ns as f64 / s.total_icount.max(1) as f64;
            concurrent_rounds(args.rounds, &mut rng, |rng| pick_sides(l, &sample_busy(), rng),
                |side, round, cpus| {
                    let (bin, lab) = label(side);
                    run_once(bin, &ctx, &format!("{stem}-r{round}-{lab}"), cpus)
                },
                |r| println!("round {}/{}: {} {:.1} MIPS on CPUs {}, {} {:.1} MIPS on CPUs {}, suite B/A {}, {:.0}s",
                             r.round, args.rounds, args.label_a, mips(&r.a), format_cpu_list(&r.cpus[0]),
                             args.label_b, mips(&r.b), format_cpu_list(&r.cpus[1]),
                             pct(cost(&r.b) / cost(&r.a)), r.a.wall_s.max(r.b.wall_s)))?
        }
        Err(_) => paired_rounds(args.rounds, &mut rng, |side, round| {
            let (bin, lab) = label(side);
            let s = run_once(bin, &ctx, &format!("{stem}-r{round}-{lab}"), pin_list.as_deref().unwrap_or(&[]))?;
            println!("round {round}/{} {lab}: {:.1} MIPS, {:.0}s", args.rounds, mips(&s), s.wall_s);
            Ok(s)
        })?,
    };

    let summary = summarise(&rounds, args.threshold / 100.0);
    let text = table(&summary, &args.label_a, &args.label_b, args.threshold / 100.0);
    let report = Report {
        tool: "iris-bench ab",
        a: Side_ { label: &args.label_a, bin: bin_a.display().to_string() },
        b: Side_ { label: &args.label_b, bin: bin_b.display().to_string() },
        elf: elf.display().to_string(),
        suite_id: suite_id.clone(),
        elf_source,
        time_pct: settings.map(|s| s.0),
        repeats: settings.map(|s| s.1),
        mode: &mode,
        pin: &args.pin,
        cpu: &args.cpu,
        perf,
        threshold_pct: args.threshold,
        bootstrap: Bootstrap {
            resamples: BOOT_RESAMPLES, confidence: BOOT_CONFIDENCE, seed: BOOT_SEED,
            statistic: "median of per-round B/A cost ratios, percentile interval; decides the verdict",
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
    let header = format!("guest image {suite_id} (from {elf_source}, {run_cfg}), {} rounds\n\nMode: {mode}\n\n",
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
    fn a_verdict_needs_the_interval_clear_of_one_and_the_median_past_the_threshold() {
        let t = 0.03;
        assert_eq!(decide(&[1.10; 9], t), Verdict::Slower);
        assert_eq!(decide(&[0.90; 9], t), Verdict::Faster);
        // A steady 2% change: real, but under the threshold, and the interval
        // shows a 3% one would have been seen.
        assert_eq!(decide(&[1.02; 9], t), Verdict::Same);
        assert_eq!(decide(&[1.0; 9], t), Verdict::Same);
        // Scattered around 1 by more than the threshold: cannot tell.
        assert_eq!(decide(&[0.9, 1.1, 1.0, 0.92, 1.08, 1.0, 0.95, 1.05, 1.0], t), Verdict::Inconclusive);
        // Too few rounds to have an interval.
        assert_eq!(decide(&[1.2, 1.2], t), Verdict::Inconclusive);
    }

    #[test]
    fn the_threshold_is_exclusive_and_symmetric() {
        let t = 0.05;
        assert_eq!(decide(&[1.05; 8], t), Verdict::Same);
        assert_eq!(decide(&[1.0501; 8], t), Verdict::Slower);
        // A 5% slowdown and the matching speedup are equally visible.
        assert_eq!(decide(&[1.0 / 1.0501; 8], t), Verdict::Faster);
        // Seven agreeing rounds are not enough.
        assert_eq!(decide(&[1.10; 7], t), Verdict::Inconclusive);
    }

    #[test]
    fn one_spoiled_round_in_many_does_not_move_the_verdict() {
        let mut r = vec![1.0; 11];
        r[4] = 1.3;
        assert_eq!(decide(&r, 0.03), Verdict::Same);
        let mut r = vec![1.08; 11];
        r[7] = 1.4;
        assert_eq!(decide(&r, 0.03), Verdict::Slower);
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
        // Why the default is 20 rounds: with n = 5 the percentile interval of
        // the median is [min, max] of the ratios, so one spoiled round keeps
        // it from excluding 1.0.
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
            round: i + 1, order: [Side::A, Side::B], together: false, cpus: [vec![], vec![]],
            a: sample(*a, Some(1_000_000)), b: sample(*b, Some(1_100_000)),
        }).collect()
    }

    #[test]
    fn summary_flags_a_steady_regression_and_ignores_one_slow_round() {
        // B is 10% slower every round.
        let s = summarise(&rounds_of(&[10.0; 8], &[11.0; 8]), 0.03);
        assert!(s.kernels.iter().all(|k| k.verdict == Verdict::Slower));
        assert!((s.total.median_ratio - 1.1).abs() < 1e-9);
        assert!((s.total.paired_ratio - 1.1).abs() < 1e-9);
        let [lo, hi] = s.total.ci.unwrap();
        assert!(lo > 1.03 && hi < 1.2);
        assert_eq!(s.total.noise, Some(0.0));
        let h = s.host.as_ref().unwrap();
        assert!((h.b_insn / h.a_insn - 1.1).abs() < 1e-9);

        // Same build, one B round in eleven 30% slow: the median stays put.
        let mut b = [10.0; 11];
        b[2] = 13.0;
        let s = summarise(&rounds_of(&[10.0; 11], &b), 0.03);
        assert!(s.kernels.iter().all(|k| k.verdict == Verdict::Same));
    }

    #[test]
    fn a_noisy_a_vs_a_run_is_inconclusive_and_a_quiet_one_is_not() {
        let noisy = [10.0, 13.0, 8.0, 10.0, 12.0, 9.0, 10.0, 11.0];
        let s = summarise(&rounds_of(&[10.0; 8], &noisy), 0.03);
        assert!(s.kernels.iter().all(|k| k.verdict == Verdict::Inconclusive), "{:?}", s.kernels[0]);
        let t = table(&s, "A", "B", 0.03);
        assert!(t.contains("Inconclusive") && t.contains("WARNING: the whole-run B/A ratio scattered"), "{t}");
        let s = summarise(&rounds_of(&[10.0; 8], &[10.0; 8]), 0.03);
        assert!(s.kernels.iter().all(|k| k.verdict == Verdict::Same));
        assert!(!table(&s, "A", "B", 0.03).contains("Inconclusive"));
        assert!(!table(&s, "A", "B", 0.03).contains("WARNING"));
    }

    #[test]
    fn too_few_rounds_judge_nothing() {
        let s = summarise(&rounds_of(&[10.0; 2], &[12.0; 2]), 0.03);
        assert!(s.kernels.iter().all(|k| k.verdict == Verdict::Inconclusive));
        assert!(table(&s, "A", "B", 0.03).contains("too few to judge"));
        let s = summarise(&rounds_of(&[10.0; 7], &[12.0; 7]), 0.03);
        assert!(s.kernels.iter().all(|k| k.verdict == Verdict::Inconclusive));
        let s = summarise(&rounds_of(&[10.0; 8], &[12.0; 8]), 0.03);
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

    #[test]
    fn concurrent_rounds_run_both_sides_together_on_their_cpus() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (active, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let mut rng = Rng::new(3);
        let mut seen = 0;
        let rounds = concurrent_rounds(4, &mut rng, |_| [vec![1, 2], vec![3, 4]], |side, _, cpus| {
            let n = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(n, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(30));
            active.fetch_sub(1, Ordering::SeqCst);
            Ok((side, cpus.to_vec()))
        }, |_| seen += 1).unwrap();
        assert_eq!(seen, 4);
        assert_eq!(peak.load(Ordering::SeqCst), 2, "the two sides did not overlap");
        for r in &rounds {
            assert!(r.together);
            assert_eq!(r.a, (Side::A, vec![1, 2]));
            assert_eq!(r.b, (Side::B, vec![3, 4]));
        }
        let e = concurrent_rounds(3, &mut rng, |_| [vec![], vec![]], |side, round, _| {
            if round == 2 && side == Side::B { Err("crashed".to_string()) } else { Ok(()) }
        }, |_| {}).unwrap_err();
        assert_eq!(e, "crashed");
    }

    #[test]
    fn cpu_lists_parse_and_print() {
        assert_eq!(parse_cpu_list("0-2,112-113\n"), Some(vec![0, 1, 2, 112, 113]));
        assert_eq!(parse_cpu_list("5"), Some(vec![5]));
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("x"), None);
        assert_eq!(format_cpu_list(&[3, 0, 1, 2, 7, 9, 8]), "0-3,7-9");
        assert_eq!(format_cpu_list(&[4]), "4");
    }

    /// sysfs for `ccds` chiplets of `cores` cores each, two SMT threads per core
    /// numbered like Linux on x86 (sibling = cpu + total cores), an L3 per
    /// chiplet, private L1 and L2.
    fn fake_sysfs(ccds: usize, cores: usize) -> impl Fn(&str) -> Option<String> {
        let n = ccds * cores;
        move |p: &str| {
            let cpu: usize = p.strip_prefix("cpu")?.split('/').next()?.parse().ok()?;
            let core = cpu % n;
            let ccd = core / cores;
            if p.ends_with("thread_siblings_list") { return Some(format!("{},{}\n", core, core + n)); }
            let idx: usize = p.split("/index").nth(1)?.split('/').next()?.parse().ok()?;
            let level = [1, 1, 2, 3].get(idx)?;
            if p.ends_with("/level") { return Some(format!("{level}\n")); }
            Some(if *level == 3 {
                format!("{}-{},{}-{}\n", ccd * cores, ccd * cores + cores - 1, n + ccd * cores, n + ccd * cores + cores - 1)
            } else {
                format!("{},{}\n", core, core + n)
            })
        }
    }

    #[test]
    fn cores_are_grouped_by_the_cache_they_share() {
        let all: Vec<usize> = (0..16).collect();
        let g = llc_groups(&all, fake_sysfs(2, 4));
        assert_eq!(g, vec![
            vec![vec![0, 8], vec![1, 9], vec![2, 10], vec![3, 11]],
            vec![vec![4, 12], vec![5, 13], vec![6, 14], vec![7, 15]],
        ]);
        // Only some CPUs allowed (a cgroup or taskset): only their cores count.
        let g = llc_groups(&[1, 2, 9, 5], fake_sysfs(2, 4));
        assert_eq!(g, vec![vec![vec![1, 9], vec![2, 10]], vec![vec![5, 13]]]);
        // Only the second thread of a core allowed: that is the one to pin to.
        assert_eq!(llc_groups(&[9], fake_sysfs(2, 4)), vec![vec![vec![9, 1]]]);
        // No topology files at all.
        assert!(llc_groups(&all, |_| None).is_empty());
    }

    fn busy(v: &[(usize, f64)]) -> std::collections::HashMap<usize, f64> { v.iter().copied().collect() }

    #[test]
    fn the_layout_takes_two_cores_a_side_from_the_quietest_group() {
        let g = llc_groups(&(0..16).collect::<Vec<_>>(), fake_sysfs(2, 4));
        // The first group's sibling threads are busy: the second is quieter.
        let b = busy(&[(8, 1.0), (9, 1.0), (10, 0.5)]);
        let l = choose_layout(&g, &b).unwrap();
        assert_eq!((l.per_side, l.group[0][0]), (2, 4));
        // Groups of two or three cores: one core a side.
        let g = llc_groups(&(0..12).collect::<Vec<_>>(), fake_sysfs(2, 3));
        assert_eq!(choose_layout(&g, &busy(&[])).unwrap().per_side, 1);
        // No group with two cores: no concurrent layout.
        let g = llc_groups(&(0..4).collect::<Vec<_>>(), fake_sysfs(2, 1));
        assert_eq!(choose_layout(&g, &busy(&[])), None);
    }

    #[test]
    fn sides_get_the_least_busy_cores_and_no_siblings() {
        let g = llc_groups(&(0..16).collect::<Vec<_>>(), fake_sysfs(1, 8));
        let l = choose_layout(&g, &busy(&[])).unwrap();
        // Cores 0 and 5 are busy, 3 is busy through its sibling 11.
        let b = busy(&[(0, 0.9), (5, 0.8), (11, 0.7)]);
        let mut rng = Rng::new(9);
        let mut a_got_1 = 0;
        for _ in 0..40 {
            let [a, bb] = pick_sides(&l, &b, &mut rng);
            assert_eq!((a.len(), bb.len()), (2, 2));
            let mut all: Vec<usize> = a.iter().chain(&bb).copied().collect();
            all.sort();
            assert_eq!(all, vec![1, 2, 4, 6]);
            if a.contains(&1) { a_got_1 += 1; }
        }
        assert!((10..=30).contains(&a_got_1), "sides are not shuffled: {a_got_1}/40");
    }

    #[test]
    fn proc_stat_gives_per_cpu_busy() {
        let a = cpu_times("cpu  1 2 3 4 5\ncpu0 100 0 0 100 0 0 0\ncpu1 0 0 0 200 0\nintr 5\n");
        let b = cpu_times("cpu0 150 0 0 150 0 0 0\ncpu1 0 0 10 290 0\n");
        let v = busy_between(&a, &b);
        assert!((v[&0] - 0.5).abs() < 1e-9 && (v[&1] - 0.1).abs() < 1e-9, "{v:?}");
        assert!(!v.contains_key(&2));
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
