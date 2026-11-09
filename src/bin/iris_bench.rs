//! `iris-bench` — drive the benchmark suite and turn its output into a report.
//!
//! The suite itself is `bench/`: a bare-metal MIPS binary that runs under IRIS
//! with no operating system, and the same C compiled natively for the host.
//! Both print the same machine-readable block. This program runs them, parses
//! it, and answers the three questions worth asking:
//!
//!   - **how fast** is a given build of IRIS, per kernel and overall, in guest
//!     instructions per host second;
//!   - **how correct** is it, as the share of kernels whose result checksum
//!     matched an independently computed golden value;
//!   - **where does the time go**, both as share of wall clock and as
//!     emulation efficiency, which are different lists.
//!
//! `matrix` builds each engine (a compile-time cargo feature) and runs every
//! CPU x engine combination, passing the CPU model as `--cpu` — so comparing
//! engines means comparing binaries, and comparing CPUs means comparing flags.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use serde::Deserialize;

// The report's data model, its parser and the reference table live in the
// library: this binary, the in-process runner and the GUI all need them, and
// "what accuracy means" should have exactly one definition.
use iris::bench_report::{
    fmt_rate, host_info, parse_block, reference_entry, GuestStep, MachineInfo, ReferenceTable,
    Row, Run, RunSettings, EXPECT_EXC, GUEST_OUTPUT_MAX,
};
use iris::bench_runner::{self, BenchOptions};
use iris::cpu::parts::{self, Part};

// ─── running ─────────────────────────────────────────────────────────────────

/// Everything `bench/` produces lives under one directory so a report can be
/// assembled from whatever happens to be there.
fn default_out() -> PathBuf { PathBuf::from("bench/build/results") }

fn repo_relative(p: &str) -> PathBuf {
    // Run from the repo root by preference, but tolerate being run from
    // bench/ — every path in this program is repo-relative.
    let direct = PathBuf::from(p);
    if direct.exists() { return direct; }
    let up = PathBuf::from("..").join(p);
    if up.exists() { return up; }
    direct
}

fn run_guest(
    iris: &Path,
    elf: &Path,
    config: &Path,
    label: &str,
    timeout_s: u64,
    extra: &[String],
) -> Result<Run, String> {
    if !iris.exists() { return Err(format!("no emulator at {}", iris.display())); }
    if !elf.exists() {
        return Err(format!("no suite binary at {} — run `make -C bench`", elf.display()));
    }

    // Run from bench/, where the suite's own relative paths resolve and where
    // the emulator's stray output files belong. Everything handed to the
    // emulator is absolute, so it does not matter what those paths looked like
    // on the way in — a --elf pointing somewhere else entirely still works.
    let abs = |p: &Path| -> Result<PathBuf, String> {
        std::fs::canonicalize(p).map_err(|e| format!("{}: {}", p.display(), e))
    };
    let (iris, elf, config) = (abs(iris)?, abs(elf)?, abs(config)?);
    let suite_id = suite_id_of(&elf)?;
    let cwd = config.parent().and_then(|p| p.parent()).map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut cmd = Command::new(&iris);
    cmd.current_dir(&cwd)
        .arg("--config").arg(&config)
        .arg("--load-elf").arg(&elf)
        .arg("--test-device")
        .arg("--headless")
        .arg("--noaudio");
    for e in extra { cmd.arg(e); }

    let started = Instant::now();
    let out = run_with_timeout(cmd, timeout_s)?;
    let wall_s = started.elapsed().as_secs_f64();

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let p = parse_block(&stdout).map_err(|e| {
        format!("{}\n--- last 20 lines of emulator output ---\n{}", e, tail(&stdout, 20))
    })?;

    Ok(Run {
        cell: label.to_string(),
        features: iris::bench_report::parse_features(&stderr),
        machine: p.machine,
        host: host_info(),
        rows: p.rows,
        checked: p.checked,
        matched: p.matched,
        total_ns: p.total_ns,
        total_icount: p.total_icount,
        wall_s,
        suite_id,
        settings: p.settings,
    })
}

fn suite_id_of(elf: &Path) -> Result<String, String> {
    let bytes = std::fs::read(elf).map_err(|e| format!("{}: {}", elf.display(), e))?;
    Ok(iris::benchsuite::suite_id_of(&bytes))
}

fn run_host(exe: &Path, timeout_s: u64) -> Result<Run, String> {
    if !exe.exists() {
        return Err(format!("no host build at {} — run `make -C bench hostbench`", exe.display()));
    }
    let started = Instant::now();
    let out = run_with_timeout(Command::new(exe), timeout_s)?;
    let wall_s = started.elapsed().as_secs_f64();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut p = parse_block(&stdout)?;
    p.machine.cpu = "host".to_string();
    Ok(Run {
        cell: "host".to_string(),
        features: Vec::new(),
        machine: p.machine,
        host: host_info(),
        rows: p.rows,
        checked: p.checked,
        matched: p.matched,
        total_ns: p.total_ns,
        total_icount: p.total_icount,
        wall_s,
        suite_id: String::new(),
        settings: p.settings,
    })
}

/// `--cpu`: a part or a family of the ones the suite measures.
fn bench_cpu(name: &str) -> Result<&'static Part, String> {
    parts::find(bench_runner::BENCH_CPUS, name).ok_or_else(|| format!("unknown cpu {name:?}"))
}

/// Run the suite inside this process, streaming the guest's console as it
/// arrives.
///
/// The guest prints its table a row at a time so that a run which shows nothing
/// for a minute is not mistaken for a hang, and that property is worth keeping
/// at the command line — so this echoes every line rather than waiting for the
/// end and printing the parsed summary.
fn run_embedded(label: &str, timeout_s: u64, quick: bool, cpu: &'static Part,
                cache: iris::config::CacheModel, deterministic: bool) -> Result<Run, String> {
    let opts = BenchOptions {
        quick,
        label: label.to_string(),
        timeout: Duration::from_secs(timeout_s),
        cpu,
        cache,
        deterministic,
        ..Default::default()
    };
    bench_runner::run(&opts, |p| {
        if let bench_runner::Progress::Line(l) = p {
            println!("{}", l);
        }
    })
}

/// How often `run_with_timeout` checks whether the child has exited. The
/// guest-OS suite times each step around one of these waits, so this is the
/// resolution of every step time. It used to be 100 ms, which put most
/// sub-second steps on a multiple of 100 ms.
const WAIT_POLL: Duration = Duration::from_millis(2);

/// Wait for a child, killing it after `timeout_s`. A benchmark that hangs is a
/// finding, not a reason to block a matrix run forever.
fn run_with_timeout(mut cmd: Command, timeout_s: u64) -> Result<std::process::Output, String> {
    use std::process::Stdio;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("spawn: {}", e))?;

    // Drain both pipes on their own threads: a child that fills a pipe buffer
    // while we sleep would deadlock instead of timing out.
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let t_out = std::thread::spawn(move || { let mut v = Vec::new(); let _ = std::io::copy(&mut so, &mut v); v });
    let t_err = std::thread::spawn(move || { let mut v = Vec::new(); let _ = std::io::copy(&mut se, &mut v); v });

    let deadline = Instant::now() + std::time::Duration::from_secs(timeout_s);
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(s) => break s,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {}s", timeout_s));
                }
                std::thread::sleep(WAIT_POLL);
            }
        }
    };
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();
    Ok(std::process::Output { status, stdout, stderr })
}

fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn save(run: &Run, dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    let path = dir.join(format!("{}.json", run.cell));
    let json = serde_json::to_string_pretty(run).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("{}: {}", path.display(), e))?;
    Ok(path)
}

/// Most recently modified `*.json` in `dir`, skipping the host run (it has no
/// suite_id and is not a machine anyone compares against).
fn newest_result(dir: &Path) -> Result<PathBuf, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        if p.file_stem().and_then(|s| s.to_str()) == Some("host") { continue; }
        let Ok(m) = e.metadata().and_then(|m| m.modified()) else { continue };
        if best.as_ref().map_or(true, |(t, _)| m > *t) { best = Some((m, p)); }
    }
    best.map(|(_, p)| p)
        .ok_or_else(|| format!("no result files in {} — run `iris-bench run` first", dir.display()))
}

fn load_all(dir: &Path) -> Result<Vec<Run>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    let mut runs = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {}", p.display(), e))?;
        match serde_json::from_str::<Run>(&text) {
            Ok(r) => runs.push(r),
            Err(e) => eprintln!("iris-bench: skipping {}: {}", p.display(), e),
        }
    }
    if runs.is_empty() { return Err(format!("no result files in {}", dir.display())); }
    // Emulated cells first, host last: the host is the reference, not a peer.
    runs.sort_by(|a, b| (a.cell == "host", &a.cell).cmp(&(b.cell == "host", &b.cell)));
    Ok(runs)
}

// ─── the matrix ──────────────────────────────────────────────────────────────

/// A cell is a cargo feature set plus a CPU. Only the feature set needs a build:
/// the CPU is a runtime setting, so the r4400 and r5000 cells of an engine share
/// one binary. `expect_cpu` is still checked, because a cell silently running
/// the wrong CPU is a mistake this repo has made before.
struct Cell {
    name: &'static str,
    features: &'static str,
    /// Passed as `--cpu`. Runtime since 96e5ddd, so cells that differ only by
    /// CPU share one emulator binary instead of each needing its own build.
    cpu: &'static str,
    /// What the guest must print in `#machine cpu=`. Checked, because an
    /// overwritten target/release/iris silently turning an "R4400" cell into
    /// an R5000 run is a mistake this repo has made before — see
    /// cpu-tests/run/matrix.sh.
    expect_cpu: &'static str,
}

const CELLS: &[Cell] = &[
    Cell { name: "r4400-interp",    features: "",           cpu: "r4400", expect_cpu: "R4400" },
    Cell { name: "r5000-interp",    features: "",           cpu: "r5000", expect_cpu: "R5000" },
    Cell { name: "r4400-jitv2",     features: "jitv2",      cpu: "r4400", expect_cpu: "R4400" },
    Cell { name: "r5000-jitv2",     features: "jitv2",      cpu: "r5000", expect_cpu: "R5000" },
    Cell { name: "r4400-lightning", features: "lightning",  cpu: "r4400", expect_cpu: "R4400" },
    Cell { name: "r4400-jitv2-lightning", features: "jitv2,lightning", cpu: "r4400", expect_cpu: "R4400" },
    // The R10000 family. It always runs the shadow caches, so the default
    // build covers it. Full chip names: a bare family name would pick the
    // chip of the first machine that lists it.
    Cell { name: "r10000-interp",   features: "",           cpu: "r10000_195", expect_cpu: "R10000" },
    Cell { name: "r12000-interp",   features: "",           cpu: "r12000_360", expect_cpu: "R12000" },
    Cell { name: "r14000-interp",   features: "",           cpu: "r14000_600", expect_cpu: "R14000" },
];

fn build_cell(cell: &Cell, root: &Path, force: bool) -> Result<PathBuf, String> {
    // Keyed by feature set: cells differing only in --cpu share a binary.
    let slug = if cell.features.is_empty() { "default".to_string() }
               else { cell.features.replace(',', "-") };
    let dest = root.join("bench/build").join(format!("iris-{}", slug));
    if dest.exists() && !force {
        println!("  reusing {}", dest.display());
        return Ok(dest);
    }
    println!("  building {} {}", cell.name,
             if cell.features.is_empty() { "(default features)".to_string() }
             else { format!("--features {}", cell.features) });

    let mut cmd = Command::new("cargo");
    cmd.current_dir(root).args(["build", "--release", "--bin", "iris"]);
    if !cell.features.is_empty() { cmd.args(["--features", cell.features]); }
    let st = cmd.status().map_err(|e| format!("cargo: {}", e))?;
    if !st.success() { return Err(format!("cargo build failed for {}", cell.name)); }

    let src = root.join("target/release/iris");
    std::fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
    // Copy rather than run in place: the next cell's build overwrites
    // target/release/iris, and a matrix that races its own artefacts produces
    // results labelled with the wrong build.
    std::fs::copy(&src, &dest).map_err(|e| format!("copy {}: {}", src.display(), e))?;
    Ok(dest)
}


// ─── the guest-OS level suite ────────────────────────────────────────────────
//
// bench/ measures the emulated CPU with no operating system in the way. This
// measures the machine as a user meets it: a filesystem on an emulated SCSI
// disk, IRIX's buffer cache and syscall path, the tools that shipped in the
// box, and the X server driving REX3. None of that is visible to a bare-metal
// kernel, and all of it is what "is the emulator fast enough to use" means.
//
// Every step is timed on the host around one `iris-ci run`, with the measured
// no-op round trip subtracted, so nothing depends on the guest having a usable
// clock or a working `time`.

/// Directories an IRIX 6.5 install actually puts programs in. `which` is a csh
/// script there and `command -v` is not in its Bourne shell, so a program is
/// probed by testing for it directly.
const IRIX_PATH: &str =
    "/bin:/usr/bin:/usr/sbin:/usr/bsd:/usr/etc:/usr/bin/X11:/usr/local/bin:/sbin:/usr/gfx";

#[derive(Debug, Deserialize)]
struct StepFile {
    step: Vec<Step>,
}

#[derive(Debug, Default, Deserialize)]
struct Step {
    name: String,
    unit: String,
    /// Work units the command performs. 0 means "not a fixed quantity" (a
    /// `find` over /usr, an x11perf run) — the duration is still comparable
    /// between runs on the same disk image, the rate is not.
    #[serde(default)]
    work: u64,
    /// Untimed preparation.
    #[serde(default)]
    setup: Option<String>,
    cmd: String,
    /// Program that must exist, or the step is skipped rather than failed.
    #[serde(default)]
    requires: Option<String>,
    /// Run it, but do not record a row (cleanup).
    #[serde(default)]
    skip_timing: bool,
    /// Caption a benchmark prints before its own figure, such as
    /// "Dhrystones per Second:". The first number after the last occurrence in
    /// the step's output lands in the result next to the host timing.
    #[serde(default)]
    metric: Option<String>,
    /// Seconds to wait for the command, in place of `--timeout`. A step that
    /// can hang, such as an X client talking to a server that is not
    /// answering, gets a short one so it fails instead of holding up the run.
    #[serde(default)]
    timeout: Option<u64>,
    /// Save the guest's screen under this name once the command has run (see
    /// `shot_at`). A step that checks what a program drew rather than what it
    /// printed needs one.
    #[serde(default)]
    screenshot: Option<String>,
    /// Take the screenshot this many seconds into the command instead of after
    /// it, so a step that runs a demo in the foreground can show it mid-run.
    #[serde(default)]
    shot_at: Option<u64>,
    /// Signature (`iris-bench screen-sig`) the screenshot must match within
    /// `tolerance`. A full-size reference image would change with every
    /// pixel of a clock or a cursor; a coarse grid of mean colours does not,
    /// and still sees a window that is missing, black or the wrong colour.
    #[serde(default)]
    golden: Option<String>,
    /// Largest distance from `golden` that still passes, as a mean absolute
    /// difference per colour channel (0 to 255). Default `DEFAULT_TOLERANCE`.
    #[serde(default)]
    tolerance: Option<f64>,
    /// Name of an earlier step's screenshot that this one must differ from
    /// by at least `min_change`: a program that was meant to open a window and
    /// draw in it left the screen as it found it.
    #[serde(default)]
    differs_from: Option<String>,
    /// Smallest distance from `differs_from` that passes. Default
    /// `DEFAULT_MIN_CHANGE`.
    #[serde(default)]
    min_change: Option<f64>,
    /// Monitor commands sent before the command, a second apart, such as
    /// `key type root` and `key enter` to log in at the X greeter. Only the
    /// emulator's keyboard reaches a program that reads the keyboard, and the
    /// greeter is one. Needs `--monitor`.
    #[serde(default)]
    keys: Vec<String>,
}

/// Cells across and down in a screen signature. Coarse on purpose: a cell
/// is about 80 pixels on a side at 1280x1024, so a moving cursor or a clock
/// changes its mean by a fraction of a level, while a missing window or a
/// black screen moves many cells by tens of levels.
const SIG_COLS: usize = 16;
const SIG_ROWS: usize = 12;

/// Default largest distance from a golden. Two desktops of the same guest
/// differ by the cursor, the clock and the order windows came up in.
const DEFAULT_TOLERANCE: f64 = 6.0;

/// Default smallest change from an earlier screenshot. A program window of
/// a few hundred pixels on a side moves the mean over the whole screen by
/// several levels; nothing drawn at all leaves it under one.
const DEFAULT_MIN_CHANGE: f64 = 2.0;

/// A screenshot reduced to the mean colour of each cell of a
/// `SIG_COLS` x `SIG_ROWS` grid, with the frame size it came from.
#[derive(Debug, Clone, PartialEq)]
struct ScreenSig {
    width: u32,
    height: u32,
    /// RGB per cell, row by row.
    cells: Vec<u8>,
}

impl ScreenSig {
    /// Reduce an 8-bit RGB or RGBA frame.
    fn from_pixels(width: u32, height: u32, channels: usize, px: &[u8]) -> Result<Self, String> {
        let (w, h) = (width as usize, height as usize);
        if w < SIG_COLS || h < SIG_ROWS || px.len() < w * h * channels || channels < 3 {
            return Err(format!("a {width}x{height} frame is too small to reduce"));
        }
        let mut cells = Vec::with_capacity(SIG_COLS * SIG_ROWS * 3);
        for cy in 0..SIG_ROWS {
            let (y0, y1) = (cy * h / SIG_ROWS, (cy + 1) * h / SIG_ROWS);
            for cx in 0..SIG_COLS {
                let (x0, x1) = (cx * w / SIG_COLS, (cx + 1) * w / SIG_COLS);
                let mut sum = [0u64; 3];
                for y in y0..y1 {
                    for x in x0..x1 {
                        let p = &px[(y * w + x) * channels..];
                        for c in 0..3 { sum[c] += p[c] as u64; }
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as u64;
                for s in sum { cells.push(((s + n / 2) / n) as u8); }
            }
        }
        Ok(ScreenSig { width, height, cells })
    }

    fn from_png(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        let mut reader = png::Decoder::new(std::io::BufReader::new(file))
            .read_info()
            .map_err(|e| format!("{}: {}", path.display(), e))?;
        let size = reader.output_buffer_size()
            .ok_or_else(|| format!("{}: frame too large", path.display()))?;
        let mut buf = vec![0; size];
        let info = reader.next_frame(&mut buf).map_err(|e| format!("{}: {}", path.display(), e))?;
        let channels = match (info.color_type, info.bit_depth) {
            (png::ColorType::Rgb, png::BitDepth::Eight) => 3,
            (png::ColorType::Rgba, png::BitDepth::Eight) => 4,
            (c, d) => return Err(format!("{}: {:?} {:?} is not 8-bit RGB", path.display(), c, d)),
        };
        Self::from_pixels(info.width, info.height, channels, &buf)
    }

    /// Mean absolute difference per channel, 0 to 255. Frames of different
    /// sizes are not comparable: the guest changed video mode.
    fn distance(&self, other: &ScreenSig) -> Result<f64, String> {
        if (self.width, self.height) != (other.width, other.height) {
            return Err(format!("the screen is {}x{}, the reference {}x{}",
                               self.width, self.height, other.width, other.height));
        }
        let total: u64 = self.cells.iter().zip(&other.cells)
            .map(|(a, b)| a.abs_diff(*b) as u64).sum();
        Ok(total as f64 / self.cells.len() as f64)
    }
}

/// `sig1 16x12 1280x1024 <hex>`: the grid, the frame size and the cells.
impl std::fmt::Display for ScreenSig {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "sig1 {}x{} {}x{} ", SIG_COLS, SIG_ROWS, self.width, self.height)?;
        for b in &self.cells { write!(f, "{:02x}", b)?; }
        Ok(())
    }
}

impl std::str::FromStr for ScreenSig {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("not a screen signature (want \"sig1 {SIG_COLS}x{SIG_ROWS} WxH HEX\"): {s:.40}");
        let f: Vec<&str> = s.split_whitespace().collect();
        if f.len() != 4 || f[0] != "sig1" || f[1] != format!("{SIG_COLS}x{SIG_ROWS}") {
            return Err(bad());
        }
        let (w, h) = f[2].split_once('x').ok_or_else(bad)?;
        let (width, height) = (w.parse().map_err(|_| bad())?, h.parse().map_err(|_| bad())?);
        let hex = f[3].as_bytes();
        if hex.len() != SIG_COLS * SIG_ROWS * 3 * 2 {
            return Err(bad());
        }
        let cells = hex.chunks(2)
            .map(|p| std::str::from_utf8(p).ok().and_then(|t| u8::from_str_radix(t, 16).ok()))
            .collect::<Option<Vec<u8>>>()
            .ok_or_else(bad)?;
        Ok(ScreenSig { width, height, cells })
    }
}

/// The number printed after `label` in `output`: the first number following
/// the last place the label appears. Programs print their figure after a
/// fixed caption ("Dhrystones per Second:  555555.6"), so a caption is enough
/// and no pattern language is needed.
fn extract_metric(label: &str, output: &str) -> Option<f64> {
    let after = &output[output.rfind(label)? + label.len()..];
    let start = after.find(|c: char| c.is_ascii_digit() || c == '-' || c == '.')?;
    let rest = &after[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '.' | ',')))
        .unwrap_or(rest.len());
    rest[..end].trim_end_matches('.').replace(',', "").parse().ok()
}

/// The last `max` bytes of `s`, noting how much was cut from the front.
fn keep_tail(s: &str, max: usize) -> String {
    if s.len() <= max { return s.to_string(); }
    let mut at = s.len() - max;
    while !s.is_char_boundary(at) { at += 1; }
    format!("[{} bytes cut]\n{}", at, &s[at..])
}

/// Longest command line the guest's tty takes. IRIX cuts a line at 256
/// characters, and what is left reaches the shell as a command that fails
/// with "Unmatched '" (or runs a truncated one). Measured on IRIX 6.5.22: a
/// 255-character line runs and a 256-character one does not.
const MAX_COMMAND_LINE: usize = 255;

/// Fail a steps file up front for a mistake that would otherwise surface
/// halfway through a long run, or as a confusing quoting error in the guest.
fn check_steps(steps: &[Step]) -> Result<(), String> {
    for (i, s) in steps.iter().enumerate() {
        if steps[..i].iter().any(|o| o.name == s.name) {
            return Err(format!("step {}: name used twice", s.name));
        }
        let texts = [Some(&s.cmd), s.setup.as_ref(), s.requires.as_ref()];
        if texts.iter().flatten().any(|t| t.contains('\'')) {
            return Err(format!(
                "step {}: a single quote would end the sh -c '...' wrapper early", s.name));
        }
        let lines = [
            ("command", Some(wrap(&s.cmd))),
            ("setup", s.setup.as_ref().map(|t| wrap(t))),
            ("requires probe", s.requires.as_ref().map(|r| wrap(&probe_command(r)))),
        ];
        for (what, line) in lines {
            if let Some(n) = line.map(|l| l.len()).filter(|&n| n > MAX_COMMAND_LINE) {
                return Err(format!(
                    "step {}: the {} is {} characters once wrapped, over the {} the guest's \
                     tty accepts on one line", s.name, what, n, MAX_COMMAND_LINE));
            }
        }
        let compares = s.golden.is_some() || s.differs_from.is_some() || s.shot_at.is_some();
        if compares && s.screenshot.is_none() {
            return Err(format!(
                "step {}: golden, differs_from and shot_at need a screenshot to work on", s.name));
        }
        if let Some(g) = &s.golden {
            g.parse::<ScreenSig>().map_err(|e| format!("step {}: golden: {}", s.name, e))?;
        }
    }
    Ok(())
}

/// Check what one step says about another, once every file is merged: an
/// extra steps file may compare with a screenshot the main file takes.
fn check_screenshot_names(steps: &[Step]) -> Result<(), String> {
    for (i, s) in steps.iter().enumerate() {
        if let Some(name) = &s.screenshot {
            if steps[..i].iter().any(|o| o.screenshot.as_ref() == Some(name)) {
                return Err(format!("step {}: screenshot {} is taken twice", s.name, name));
            }
        }
        if let Some(earlier) = &s.differs_from {
            if !steps[..i].iter().any(|o| o.screenshot.as_ref() == Some(earlier)) {
                return Err(format!(
                    "step {}: differs_from names {}, which no earlier step takes", s.name, earlier));
            }
        }
    }
    Ok(())
}

fn load_steps(path: &Path) -> Result<Vec<Step>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let file: StepFile = toml::from_str(&text).map_err(|e| format!("{}: {}", path.display(), e))?;
    check_steps(&file.step).map_err(|e| format!("{}: {}", path.display(), e))?;
    Ok(file.step)
}

/// Add `extra` to `base`. A step with the name of one already there replaces
/// it in place. The rest go before the steps at the end of `base` that record
/// no row (the default file ends with a cleanup), so they still run with the
/// files the earlier steps left behind.
fn merge_steps(mut base: Vec<Step>, extra: Vec<Step>) -> Vec<Step> {
    let mut at = base.len();
    while at > 0 && base[at - 1].skip_timing { at -= 1; }
    for e in extra {
        match base.iter().position(|b| b.name == e.name) {
            Some(i) => base[i] = e,
            None => { base.insert(at, e); at += 1; }
        }
    }
    base
}

/// Wrap a guest command so it runs under a Bourne shell with a sane PATH,
/// whatever the login shell is. Consequence, documented in steps.toml: a step
/// command may not contain a single quote.
fn wrap(cmd: &str) -> String {
    format!("sh -c 'PATH={}; export PATH; {}'", IRIX_PATH, cmd)
}

/// Shell that prints `IRISBENCH-HAVE` when `prog` is an executable the guest can
/// run. A name is searched for in `$PATH` by splitting on `:` with `IFS`; a path
/// is tested as given.
///
/// This used to split PATH with `tr : \" \"`. Inside backquotes the backslashes
/// survive, so IRIX's `tr` was handed literal `\"` arguments, printed its usage
/// and the loop saw no directories, so no program was ever found.
///
/// The marker is typed as `HAV\E` so the tty's echo of the command line cannot
/// itself contain the word the output is searched for.
fn probe_command(prog: &str) -> String {
    if prog.contains('/') {
        format!("test -x {prog} && echo IRISBENCH-HAV\\E; true")
    } else {
        format!("IFS=:; for d in $PATH; do test -x $d/{prog} && echo IRISBENCH-HAV\\E; done; true")
    }
}

fn probe_found(out: &str) -> bool {
    out.contains("IRISBENCH-HAVE")
}

struct Ci {
    bin: PathBuf,
    socket: Option<PathBuf>,
    shell: String,
    timeout: u64,
    /// The emulator's monitor, `host:port`, for steps that type keys.
    monitor: Option<String>,
}

/// One command run in the guest.
struct CiRun {
    stdout: String,
    /// Host wall time around the `iris-ci run`.
    secs: f64,
    /// The guest command's exit status. -1 when it timed out.
    exit: i32,
    /// The guest did not finish the command in time. It may still be running it.
    timed_out: bool,
}

/// Something that runs shell commands in a guest. `Ci` is the real one; the
/// step logic is written against this so it can be tested without a guest.
trait Guest {
    /// Run `guest_cmd`, waiting `secs` seconds for it (the default when
    /// `None`). An `Err` means the guest could not be asked (iris-ci failed or
    /// the emulator is gone). A command that ran and failed is an `Ok` with a
    /// nonzero `exit`, and one that did not finish is an `Ok` with `timed_out`.
    fn run_within(&self, guest_cmd: &str, secs: Option<u64>) -> Result<CiRun, String>;

    fn run(&self, guest_cmd: &str) -> Result<CiRun, String> {
        self.run_within(guest_cmd, None)
    }

    /// Send Ctrl-C to whatever the guest's shell is running.
    fn interrupt(&self) -> Result<(), String>;

    /// Save the guest's screen as a PNG at `path`. The emulator writes the
    /// file, so the path is on its host.
    fn screenshot(&self, path: &Path) -> Result<(), String>;

    /// Run `guest_cmd` as `run_within` does and save the screen `at` seconds
    /// into it. The two outcomes are separate: a demo can run to the end
    /// with the capture failing, and the other way round.
    fn run_with_shot(&self, guest_cmd: &str, secs: Option<u64>, at: u64, path: &Path)
        -> Result<(CiRun, Result<(), String>), String>;

    /// Send one command to the emulator's monitor and return its reply.
    fn monitor(&self, command: &str) -> Result<String, String>;
}

/// The exit status in `iris-ci run`'s complaint about a failing guest command,
/// `iris-ci: iris error: guest exit N`.
fn guest_exit(stderr: &str) -> Option<i32> {
    let rest = &stderr[stderr.rfind("guest exit ")? + "guest exit ".len()..];
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '-')).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Turn the outcome of one `iris-ci run` process into a `CiRun`.
///
/// `iris-ci run` exits nonzero when the guest command does, and says so on
/// stderr. That is a result about the step, not a failure of the harness, and
/// it must not depend on whether the command printed anything. Only an
/// `iris-ci` failure with no guest status (no emulator, a timeout) is an error.
fn classify_run(success: bool, stdout: String, stderr: &str, secs: f64) -> Result<CiRun, String> {
    if success {
        return Ok(CiRun { stdout, secs, exit: 0, timed_out: false });
    }
    if let Some(exit) = guest_exit(stderr) {
        return Ok(CiRun { stdout, secs, exit, timed_out: false });
    }
    // The serial wait for the end-of-command marker ran out.
    if stderr.contains("timeout after") {
        return Ok(CiRun { stdout, secs, exit: -1, timed_out: true });
    }
    Err(format!("iris-ci run failed: {}", stderr.trim()))
}

impl Guest for Ci {
    fn run_within(&self, guest_cmd: &str, secs: Option<u64>) -> Result<CiRun, String> {
        let secs = secs.unwrap_or(self.timeout);
        let mut cmd = Command::new(&self.bin);
        if let Some(s) = &self.socket { cmd.arg("--socket").arg(s); }
        cmd.arg("run")
            .arg(guest_cmd)
            .arg("--shell").arg(&self.shell)
            .arg("--timeout").arg(secs.to_string());
        let started = Instant::now();
        let out = run_with_timeout(cmd, secs + 30)?;
        let secs = started.elapsed().as_secs_f64();
        classify_run(
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            &String::from_utf8_lossy(&out.stderr),
            secs,
        )
    }

    fn interrupt(&self) -> Result<(), String> {
        self.send_interrupt()
    }

    fn screenshot(&self, path: &Path) -> Result<(), String> {
        let mut cmd = Command::new(&self.bin);
        if let Some(s) = &self.socket { cmd.arg("--socket").arg(s); }
        cmd.arg("screenshot").arg(path);
        let out = run_with_timeout(cmd, 60)?;
        if !out.status.success() {
            return Err(format!("iris-ci screenshot failed: {}",
                               String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(())
    }

    fn run_with_shot(&self, guest_cmd: &str, secs: Option<u64>, at: u64, path: &Path)
        -> Result<(CiRun, Result<(), String>), String>
    {
        // The CI socket serves each connection on its own thread, so the
        // capture does not wait for the command's serial round trip.
        std::thread::scope(|s| {
            let run = s.spawn(|| self.run_within(guest_cmd, secs));
            std::thread::sleep(Duration::from_secs(at));
            let shot = self.screenshot(path);
            let run = run.join().map_err(|_| "the guest command's thread panicked".to_string())?;
            Ok((run?, shot))
        })
    }

    fn monitor(&self, command: &str) -> Result<String, String> {
        use std::io::Read;
        let addr = self.monitor.as_deref()
            .ok_or("typing keys needs the emulator's monitor: pass --monitor HOST:PORT")?;
        let mut conn = std::net::TcpStream::connect(addr)
            .map_err(|e| format!("monitor {}: {}", addr, e))?;
        conn.write_all(format!("{}\n", command).as_bytes())
            .map_err(|e| format!("monitor {}: {}", addr, e))?;
        // The monitor answers and waits for the next line, with no end
        // marker, so read until it has been quiet for a moment.
        conn.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let mut reply = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = conn.read(&mut buf) {
            if n == 0 { break; }
            reply.extend_from_slice(&buf[..n]);
        }
        let reply = String::from_utf8_lossy(&reply).into_owned();
        if reply.contains("Unknown command") {
            return Err(format!("monitor refused {:?}", command));
        }
        Ok(reply)
    }
}

impl Ci {
    /// Ctrl-C is a raw byte on the serial line, with no carriage return after it.
    fn send_interrupt(&self) -> Result<(), String> {
        let mut cmd = Command::new(&self.bin);
        if let Some(s) = &self.socket { cmd.arg("--socket").arg(s); }
        cmd.arg("serial-send").arg("--no-cr").arg("\u{3}");
        let out = run_with_timeout(cmd, 30)?;
        if !out.status.success() {
            return Err(format!("iris-ci serial-send failed: {}",
                               String::from_utf8_lossy(&out.stderr).trim()));
        }
        // Let the tty flush and the shell redraw its prompt.
        std::thread::sleep(Duration::from_secs(2));
        Ok(())
    }
}

fn has(guest: &impl Guest, prog: &str) -> Result<bool, String> {
    Ok(probe_found(&guest.run(&wrap(&probe_command(prog)))?.stdout))
}

/// Smallest round trip we can measure: everything a step's timing has in
/// common with doing nothing at all — serial latency, prompt matching,
/// process spawn. Subtracted from every step, so a one-second step is a
/// second of work rather than a second of work plus the harness.
fn baseline(guest: &impl Guest) -> Result<f64, String> {
    let mut best = f64::MAX;
    for _ in 0..5 {
        let s = guest.run(&wrap(":"))?.secs;
        if s < best { best = s; }
    }
    Ok(best)
}

/// Get the guest's shell back after a step hung. Interrupts it and checks that a
/// trivial command answers; if it does not, nothing after this can be trusted.
fn recover(guest: &impl Guest) -> Result<(), String> {
    guest.interrupt()?;
    let r = guest.run_within(&wrap(":"), Some(30))?;
    if r.timed_out {
        return Err("the guest shell did not answer after Ctrl-C".into());
    }
    Ok(())
}

/// A row for a step that has no timing to report. The caller adds what the
/// guest printed.
fn untimed_row(step: &Step, status: &str) -> Row {
    Row {
        name: step.name.clone(),
        unit: step.unit.clone(),
        iters: 1,
        work: step.work,
        ns: 0,
        icount: 0,
        count: 0,
        exc: 0,
        checksum: "0x0000000000000000".into(),
        golden: "0x0000000000000000".into(),
        status: status.into(),
        guest: None,
    }
}

/// Where a run's screenshots go, and the prefix that keeps two runs' files
/// apart in one directory.
struct Shots {
    dir: PathBuf,
    label: String,
}

impl Shots {
    fn path(&self, name: &str) -> PathBuf {
        let safe = |s: &str| s.replace(|c: char| !(c.is_ascii_alphanumeric() || c == '-'), "_");
        self.dir.join(format!("{}-{}.png", safe(&self.label), safe(name)))
    }
}

/// Hold a screenshot up against what its step expects. Returns the distance
/// to report, and the reason the step fails if it does.
fn judge_screen(step: &Step, sig: &ScreenSig, taken: &[(String, ScreenSig)])
    -> (Option<f64>, Option<String>)
{
    let mut distance = None;
    if let Some(golden) = &step.golden {
        let golden: ScreenSig = match golden.parse() {
            Ok(g) => g,
            Err(e) => return (None, Some(e)),
        };
        let tol = step.tolerance.unwrap_or(DEFAULT_TOLERANCE);
        match sig.distance(&golden) {
            Err(e) => return (None, Some(e)),
            Ok(d) if d > tol => {
                return (Some(d), Some(format!("screen is {:.1} from its golden, over {:.1}", d, tol)));
            }
            Ok(d) => distance = Some(d),
        }
    }
    if let Some(earlier) = &step.differs_from {
        let Some((_, before)) = taken.iter().find(|(n, _)| n == earlier) else {
            return (distance, Some(format!("no {} screenshot to compare with", earlier)));
        };
        let min = step.min_change.unwrap_or(DEFAULT_MIN_CHANGE);
        match sig.distance(before) {
            Err(e) => return (distance, Some(e)),
            Ok(d) if d < min => {
                return (Some(d), Some(format!(
                    "screen is only {:.1} from {}, under {:.1}: nothing was drawn", d, earlier, min)));
            }
            Ok(d) => { distance.get_or_insert(d); }
        }
    }
    (distance, None)
}

/// Send a step's monitor commands, with a pause after each so the guest has
/// taken one key sequence before the next arrives.
fn type_keys(guest: &impl Guest, keys: &[String]) -> Result<(), String> {
    for k in keys {
        guest.monitor(k)?;
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

/// Run `steps` in order. Returns a row per step that is timed, skipped or
/// failed, and the total timed nanoseconds.
///
/// A step whose command exits nonzero, or whose screenshot does not match, is
/// recorded as FAILED and the suite goes on: one missing tool or broken
/// workload says nothing about the next step. Only a failure to talk to the
/// guest at all stops the run.
fn run_steps(guest: &impl Guest, steps: &[Step], baseline: f64, shots: &Shots)
    -> Result<(Vec<Row>, u64), String>
{
    let mut rows = Vec::new();
    let mut total_ns = 0u64;
    let mut taken: Vec<(String, ScreenSig)> = Vec::new();

    for step in steps {
        if let Some(prog) = &step.requires {
            if !has(guest, prog)? {
                println!("  {:<22} skipped ({} not installed)", step.name, prog);
                rows.push(untimed_row(step, "SKIP"));
                continue;
            }
        }
        if let Some(setup) = &step.setup {
            let r = guest.run_within(&wrap(setup), step.timeout)?;
            if r.timed_out {
                println!("  {:<22} FAILED (setup did not finish in {} s)", step.name,
                         step.timeout.unwrap_or(0));
                if !step.skip_timing { rows.push(untimed_row(step, "FAILED")); }
                recover(guest)?;
                continue;
            }
            if r.exit != 0 {
                println!("  {:<22} FAILED (setup exited {})", step.name, r.exit);
                if !step.skip_timing { rows.push(untimed_row(step, "FAILED")); }
                continue;
            }
        }

        if let Err(e) = type_keys(guest, &step.keys) {
            println!("  {:<22} FAILED ({})", step.name, e);
            if !step.skip_timing { rows.push(untimed_row(step, "FAILED")); }
            continue;
        }

        let shot_path = step.screenshot.as_ref().map(|n| shots.path(n));
        let (r, mut shot) = match (&shot_path, step.shot_at) {
            (Some(p), Some(at)) => {
                let (r, s) = guest.run_with_shot(&wrap(&step.cmd), step.timeout, at, p)?;
                (r, Some(s))
            }
            _ => (guest.run_within(&wrap(&step.cmd), step.timeout)?, None),
        };
        if r.timed_out {
            println!("  {:<22} FAILED (did not finish in {} s)", step.name,
                     step.timeout.unwrap_or(0));
            if !step.skip_timing { rows.push(untimed_row(step, "FAILED")); }
            recover(guest)?;
            continue;
        }
        if step.skip_timing { continue; }
        if let (Some(p), None) = (&shot_path, &shot) {
            shot = Some(guest.screenshot(p));
        }
        let metric = match &step.metric {
            Some(p) => extract_metric(p, &r.stdout),
            None => None,
        };
        let mut seen = GuestStep {
            exit: r.exit,
            output: keep_tail(&r.stdout, GUEST_OUTPUT_MAX),
            metric,
            ..Default::default()
        };
        let mut screen_fault = None;
        if let (Some(p), Some(shot), Some(name)) = (&shot_path, shot, &step.screenshot) {
            match shot.and_then(|()| ScreenSig::from_png(p)) {
                Err(e) => screen_fault = Some(e),
                Ok(sig) => {
                    let (distance, fault) = judge_screen(step, &sig, &taken);
                    seen.screenshot = Some(p.display().to_string());
                    seen.signature = Some(sig.to_string());
                    seen.distance = distance;
                    screen_fault = fault;
                    taken.push((name.clone(), sig));
                }
            }
        }
        if r.exit != 0 || screen_fault.is_some() {
            match &screen_fault {
                Some(why) if r.exit == 0 => println!("  {:<22} FAILED ({})", step.name, why),
                _ => println!("  {:<22} FAILED (exited {})", step.name, r.exit),
            }
            let last = tail(r.stdout.trim_end(), 3);
            if !last.is_empty() {
                for l in last.lines() { println!("      {}", l); }
            }
            let mut row = untimed_row(step, "FAILED");
            row.guest = Some(seen);
            rows.push(row);
            continue;
        }

        let ns = ((r.secs - baseline).max(0.0) * 1e9) as u64;
        total_ns += ns;
        println!(
            "  {:<22} {:>8.2} s{}",
            step.name,
            ns as f64 / 1e9,
            if step.work > 0 && ns > 0 {
                format!("   {}/s", fmt_rate(step.work as f64 * 1e9 / ns as f64))
            } else {
                String::new()
            }
        );
        if let Some(p) = &step.metric {
            match seen.metric {
                Some(v) => println!("      metric {}", v),
                None => println!("      metric {:?} found nothing in the output", p),
            }
        }
        if let Some(path) = &seen.screenshot {
            match seen.distance {
                Some(d) => println!("      screen {} at {:.1}", path, d),
                // Nothing to compare with yet: print what a golden would be.
                None => println!("      screen {} {}", path, seen.signature.as_deref().unwrap_or("")),
            }
        }
        let mut row = untimed_row(step, "UNCHECKED");
        row.ns = ns;
        row.guest = Some(seen);
        // Nothing here is checksummed: these are IRIX's own tools against
        // IRIX's own filesystem, and their output is not ours to predict.
        rows.push(row);
    }
    Ok((rows, total_ns))
}

/// Steps that ran cleanly, failed and were skipped.
fn step_outcomes(run: &Run) -> (usize, usize, usize) {
    let count = |s: &str| run.rows.iter().filter(|r| r.status == s).count();
    (count("UNCHECKED"), count("FAILED"), count("SKIP"))
}

/// Prints the guest's `uname -a` after a marker. The tty echoes the typed
/// command line ahead of the output, and a line of that echo can run into the
/// start of the real output. Searching for "IRIX" then picked up shell input.
/// The marker is typed as `UNAM\E` so the echo cannot contain it.
const UNAME_COMMAND: &str = "echo IRISBENCH-UNAM\\E `uname -a`";

/// The guest's `uname -a` line, from the one line that starts with the marker.
fn guest_name(out: &str) -> String {
    out.lines()
        .find_map(|l| l.trim().strip_prefix("IRISBENCH-UNAME "))
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn run_irix(ci: &Ci, steps_path: &Path, extra: &[PathBuf], label: &str, shots: &Shots)
    -> Result<Run, String>
{
    let mut steps = load_steps(steps_path)?;
    for path in extra {
        steps = merge_steps(steps, load_steps(path)?);
    }
    check_screenshot_names(&steps)?;
    if steps.iter().any(|s| s.screenshot.is_some()) {
        std::fs::create_dir_all(&shots.dir)
            .map_err(|e| format!("{}: {}", shots.dir.display(), e))?;
    }

    // Identify the guest before anything else: a result recorded against the
    // wrong disk image is worse than no result.
    let cpu = guest_name(&ci.run(&wrap(UNAME_COMMAND))?.stdout);
    println!("  guest: {}", cpu);

    let floor = baseline(ci)?;
    println!("  round-trip floor: {:.0} ms", floor * 1e3);

    let (rows, total_ns) = run_steps(ci, &steps, floor, shots)?;

    Ok(Run {
        cell: label.to_string(),
        features: Vec::new(),
        machine: MachineInfo { cpu, ..Default::default() },
        host: host_info(),
        rows,
        checked: 0,
        matched: 0,
        total_ns,
        total_icount: 0,
        wall_s: total_ns as f64 / 1e9,
        // The guest-OS suite runs IRIX's own tools, not the bare-metal binary,
        // so there is no suite hash and its numbers never join the reference
        // table — they are only comparable against runs on the same disk image.
        suite_id: String::new(),
        // Not the bare-metal harness, so its run configuration does not apply.
        settings: RunSettings::default(),
    })
}

// ─── reports ─────────────────────────────────────────────────────────────────

fn fmt_ratio(a: f64, b: f64) -> String {
    if b <= 0.0 || a <= 0.0 { return "-".into(); }
    let r = a / b;
    if r >= 100.0 { format!("{:.0}x", r) } else { format!("{:.2}x", r) }
}

/// Accuracy as the report prints it: blank for a guest-OS run, which has
/// nothing checked, rather than "0.0% (0/0)".
fn accuracy_text(r: &Run) -> String {
    if r.is_guest_os() { "-".into() } else { format!("{:.1}% ({}/{})", r.accuracy(), r.matched, r.checked) }
}

/// Every step of a guest-OS run, in the order it ran. Steps with no fixed
/// quantity of work (`find`, `tar`) have no rate, so the time is what they show.
fn guest_steps_section(r: &Run) -> String {
    let mut o = format!("### {}\n\nGuest: {}\n\n", r.cell, r.machine.cpu);
    o.push_str("Times are host wall clock around each step, less the round trip of an \
                empty command.\n\n");
    o.push_str("| step | status | time | rate | program's own figure |\n|---|---|---:|---:|---:|\n");
    for row in &r.rows {
        let (status, time) = match row.status.as_str() {
            "UNCHECKED" => ("ok", format!("{:.3} s", row.ns as f64 / 1e9)),
            "SKIP" => ("skipped", "-".into()),
            other => (other, "-".into()),
        };
        let rate = if row.rate() > 0.0 { format!("{} {}/s", fmt_rate(row.rate()), row.unit) } else { "-".into() };
        let own = row.guest.as_ref().and_then(|g| g.metric)
            .map(|m| m.to_string()).unwrap_or_else(|| "-".into());
        o.push_str(&format!("| {} | {} | {} | {} | {} |\n", row.name, status, time, rate, own));
    }
    o.push('\n');
    o
}

fn markdown(all: &[Run], baseline: Option<&str>) -> String {
    let mut o = String::new();
    o.push_str("# IRIS benchmark report\n\n");
    if let Some(h) = all.first() {
        o.push_str(&format!(
            "Host: {} {} / {} / {} cores\n\n",
            h.host.os, h.host.arch, h.host.cpu_model, h.host.cores
        ));
    }

    // ── per-cell summary ────────────────────────────────────────────────────
    o.push_str("## Cells\n\n");
    o.push_str("| cell | features | CPU | accuracy | guest MIPS | DMIPS | whet/s | LINPACK MFLOPS | timed | wall |\n");
    o.push_str("|---|---|---|---:|---:|---:|---:|---:|---:|---:|\n");
    for r in all {
        let feats = if r.features.is_empty() { "-".to_string() } else { r.features.join(" ") };
        o.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {:.1} s | {:.1} s |\n",
            r.cell, feats, r.machine.cpu,
            accuracy_text(r),
            if r.mips() > 0.0 { format!("{:.1}", r.mips()) } else { "n/a".into() },
            r.dmips().map(|v| format!("{:.1}", v)).unwrap_or_else(|| "-".into()),
            r.whet_loops().map(|v| format!("{:.0}", v)).unwrap_or_else(|| "-".into()),
            r.linpack_mflops().map(|v| format!("{:.2}", v)).unwrap_or_else(|| "-".into()),
            r.total_ns as f64 / 1e9, r.wall_s,
        ));
    }
    o.push('\n');

    // The guest-OS steps have their own section. Everything below compares the
    // bare-metal kernels, which these runs do not have.
    let guest: Vec<&Run> = all.iter().filter(|r| r.is_guest_os()).collect();
    if !guest.is_empty() {
        o.push_str("## Guest steps\n\n");
        for r in &guest { o.push_str(&guest_steps_section(r)); }
    }
    let bare: Vec<Run> = all.iter().filter(|r| !r.is_guest_os()).cloned().collect();
    if bare.is_empty() { return o; }
    let runs = &bare[..];
    let emulated: Vec<&Run> = runs.iter().filter(|r| r.cell != "host").collect();
    let host = runs.iter().find(|r| r.cell == "host");
    let base = baseline
        .and_then(|b| runs.iter().find(|r| r.cell == b))
        .or_else(|| emulated.first().copied());

    // ── accuracy detail ─────────────────────────────────────────────────────
    let mut any_bad = false;
    for r in runs {
        for row in &r.rows {
            if row.status == "MISMATCH" {
                if !any_bad {
                    o.push_str("## Checksum mismatches\n\n");
                    o.push_str("A kernel whose result differs from the independently computed \
                                golden value. Either the emulator computed something wrong, or \
                                the kernel is not as deterministic as it claims — both are worth \
                                chasing.\n\n");
                    o.push_str("| cell | benchmark | got | want |\n|---|---|---|---|\n");
                    any_bad = true;
                }
                o.push_str(&format!("| {} | {} | `{}` | `{}` |\n",
                                    r.cell, row.name, row.checksum, row.golden));
            }
        }
    }
    if any_bad { o.push('\n'); }

    let mut any_exc = false;
    for r in runs {
        for row in &r.rows {
            if row.exc > 0 && !EXPECT_EXC.contains(&row.name.as_str()) {
                if !any_exc {
                    o.push_str("## Unexpected exceptions\n\n");
                    o.push_str("The harness steps over a faulting instruction and carries on, so a \
                                kernel that faults still reports a throughput — for doing something \
                                other than what it claims. Anything listed here is measuring the \
                                exception path.\n\n");
                    o.push_str("| cell | benchmark | exceptions |\n|---|---|---:|\n");
                    any_exc = true;
                }
                o.push_str(&format!("| {} | {} | {} |\n", r.cell, row.name, row.exc));
            }
        }
    }
    if any_exc { o.push('\n'); }

    // ── per-kernel rates ────────────────────────────────────────────────────
    o.push_str("## Per-kernel throughput\n\n");
    o.push_str("Work units per second, in each kernel's own unit. `vs base` is \
                relative to **");
    o.push_str(base.map(|b| b.cell.as_str()).unwrap_or("-"));
    o.push_str("**");
    if host.is_some() {
        o.push_str("; `native` is the fraction of the host's own rate on the identical kernel");
    }
    o.push_str(".\n\n");

    o.push_str("| benchmark | unit |");
    for r in &emulated { o.push_str(&format!(" {} |", r.cell)); }
    if base.is_some() && emulated.len() > 1 { o.push_str(" vs base |"); }
    if host.is_some() { o.push_str(" host | native |"); }
    o.push('\n');
    o.push_str("|---|---|");
    for _ in &emulated { o.push_str("---:|"); }
    if base.is_some() && emulated.len() > 1 { o.push_str("---:|"); }
    if host.is_some() { o.push_str("---:|---:|"); }
    o.push('\n');

    let names = ordered_names(runs);
    for name in &names {
        let unit = runs.iter().find_map(|r| r.row(name).map(|x| x.unit.clone()))
            .unwrap_or_default();
        o.push_str(&format!("| {} | {} |", name, unit));
        for r in &emulated {
            o.push_str(&format!(" {} |", r.row(name).map(|x| fmt_rate(x.rate()))
                                             .unwrap_or_else(|| "-".into())));
        }
        if base.is_some() && emulated.len() > 1 {
            let b = base.unwrap().row(name).map(|x| x.rate()).unwrap_or(0.0);
            let best = emulated.iter().filter_map(|r| r.row(name)).map(|x| x.rate())
                .fold(0.0f64, f64::max);
            o.push_str(&format!(" {} |", fmt_ratio(best, b)));
        }
        if let Some(h) = host {
            let hv = h.row(name).map(|x| x.rate()).unwrap_or(0.0);
            let best = emulated.iter().filter_map(|r| r.row(name)).map(|x| x.rate())
                .fold(0.0f64, f64::max);
            o.push_str(&format!(" {} |", fmt_rate(hv)));
            o.push_str(&format!(" {} |", if hv > 0.0 && best > 0.0 {
                format!("1/{:.0}", hv / best)
            } else { "-".into() }));
        }
        o.push('\n');
    }
    o.push('\n');

    // ── efficiency ──────────────────────────────────────────────────────────
    for r in &emulated {
        if r.mips() <= 0.0 { continue; }
        o.push_str(&format!("## Where {} spends its time\n\n", r.cell));

        let mut by_time: Vec<&Row> = r.rows.iter().filter(|x| x.ns > 0).collect();
        by_time.sort_by(|a, b| b.ns.cmp(&a.ns));
        o.push_str("| benchmark | share of wall clock | guest MIPS |\n|---|---:|---:|\n");
        for row in by_time.iter().take(8) {
            o.push_str(&format!("| {} | {:.1}% | {:.1} |\n",
                row.name, row.ns as f64 * 100.0 / r.total_ns.max(1) as f64, row.mips()));
        }
        o.push('\n');

        let mut by_mips: Vec<&Row> = r.rows.iter().filter(|x| x.icount > 0).collect();
        by_mips.sort_by(|a, b| a.mips().partial_cmp(&b.mips()).unwrap());
        o.push_str("Least efficient — the kernels where the emulator does the most host work \
                    per guest instruction:\n\n");
        o.push_str("| benchmark | guest MIPS | vs this cell's average |\n|---|---:|---:|\n");
        let avg = r.mips();
        for row in by_mips.iter().take(8) {
            o.push_str(&format!("| {} | {:.1} | {:.2}x |\n",
                row.name, row.mips(), if avg > 0.0 { row.mips() / avg } else { 0.0 }));
        }
        o.push('\n');
    }

    o
}

/// Every kernel name that appears anywhere, in the order the first run lists
/// them — the suite's own order, which groups related kernels together.
fn ordered_names(runs: &[Run]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for r in runs {
        for row in &r.rows {
            if !names.iter().any(|n| n == &row.name) { names.push(row.name.clone()); }
        }
    }
    names
}

fn text_summary(runs: &[Run]) -> String {
    let mut o = String::new();
    for r in runs {
        if r.is_guest_os() {
            let count = |s: &str| r.rows.iter().filter(|x| x.status == s).count();
            o.push_str(&format!(
                "{:<24} guest steps: {} timed, {} skipped, {} failed   {:>7.1} s timed\n",
                r.cell, count("UNCHECKED"), count("SKIP"), count("FAILED"),
                r.total_ns as f64 / 1e9,
            ));
            continue;
        }
        o.push_str(&format!(
            "{:<24} {:>6.1}% accuracy   {:>8}   {:>7} DMIPS   {:>7.1} s timed\n",
            r.cell, r.accuracy(),
            if r.mips() > 0.0 { format!("{:.1} MIPS", r.mips()) } else { "n/a".into() },
            r.dmips().map(|v| format!("{:.1}", v)).unwrap_or_else(|| "-".into()),
            r.total_ns as f64 / 1e9,
        ));
    }
    o
}



// ─── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "iris-bench",
    about = "Run the IRIS benchmark suite and report on it.",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the suite once.
    ///
    /// In-process by default: this binary *is* an emulator, and the guest image
    /// is linked into it, so there is no subprocess, no ELF on disk and nothing
    /// platform-specific. Pass `--iris` to measure a different emulator binary
    /// instead, which is what `matrix` does.
    Run {
        /// Measure this emulator binary in a subprocess instead of this one
        /// in-process. Needs --elf too, or a built bench/build/irisbench.elf.
        #[arg(long)]
        iris: Option<PathBuf>,
        /// Suite binary for --iris. Defaults to bench/build/irisbench.elf.
        #[arg(long)]
        elf: Option<PathBuf>,
        /// Machine config for --iris. Defaults to bench/run/bare.toml.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Name this result. Defaults to "local".
        #[arg(long, default_value = "local")]
        label: String,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 1800)]
        timeout: u64,
        /// Shorter timed runs and one pass per kernel instead of best of two.
        /// Every kernel still runs and still verifies, so accuracy means the
        /// same thing; only the rates are noisier. Not accepted with --iris —
        /// the register that carries it is set at machine construction.
        #[arg(long)]
        quick: bool,
        /// Emulated CPU for the in-process run. With --iris, forward it to the
        /// emulator instead: `-- --cpu r5000`.
        #[arg(long, default_value = "r4400", value_parser = bench_cpu)]
        cpu: &'static Part,
        /// Cache model for the in-process run: auto (shadow in a jitv2
        /// build), full or shadow. With --iris: `-- --cache shadow`.
        #[arg(long, default_value = "auto")]
        cache: iris::config::CacheModel,
        /// Make two runs do the same work: JIT compiles on the CPU thread and,
        /// in a `ci_clock` build (required in process), the suite reads a clock
        /// driven by retired instructions, so it picks the same iteration
        /// counts every time. The rates it prints are then
        /// constant by construction; compare instruction counts and host cost.
        /// The result is labelled `<label>+deterministic`. With --iris this
        /// passes `--jitv2-compile sync`; the emulator must itself be a
        /// `ci_clock` build.
        #[arg(long)]
        deterministic: bool,
        /// Extra arguments passed through to --iris.
        #[arg(last = true)]
        extra: Vec<String>,
    },

    /// Measure the machine the emulator runs on, with the same kernels.
    Host {
        /// Host build of the suite. Defaults to bench/build/irisbench-host.
        #[arg(long)]
        exe: Option<PathBuf>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },

    /// Build every CPU x engine cell and run all of them.
    Matrix {
        /// Comma-separated cell names. Default: the four core cells.
        #[arg(long)]
        cells: Option<String>,
        #[arg(long)]
        out: Option<PathBuf>,
        /// Rebuild even when a cached emulator binary exists.
        #[arg(long)]
        force_build: bool,
        /// Skip the host baseline run.
        #[arg(long)]
        no_host: bool,
        #[arg(long, default_value_t = 1800)]
        timeout: u64,
    },

    /// Turn saved results into a report.
    Report {
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Cell to express speedups against. Defaults to the first emulated cell.
        #[arg(long)]
        baseline: Option<String>,
        #[arg(long, value_parser = ["md", "json", "text"], default_value = "md")]
        format: String,
        /// Write here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Run the guest-OS level suite against a booted, logged-in IRIX.
    ///
    /// Needs an emulator started with `--ci`, a guest sitting at a shell
    /// prompt, and `iris-ci` on hand. Unlike every other subcommand this one
    /// measures the machine as a user meets it — filesystem, buffer cache,
    /// syscalls, IRIX's own tools, and X on REX3.
    Irix {
        /// CI socket. Defaults to iris-ci's own default.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Step definitions, replacing bench/irix/steps.toml.
        #[arg(long)]
        steps: Option<PathBuf>,
        /// More steps, added to the ones above. May be given more than once.
        /// A step named like an existing one replaces it; the others run
        /// after the defaults and before the final cleanup.
        #[arg(long = "extra-steps")]
        extra_steps: Vec<PathBuf>,
        /// iris-ci binary. Defaults to target/release/iris-ci.
        #[arg(long)]
        ci: Option<PathBuf>,
        /// Guest login shell, passed through to `iris-ci run`.
        #[arg(long, default_value = "csh")]
        shell: String,
        #[arg(long, default_value = "irix")]
        label: String,
        #[arg(long)]
        out: Option<PathBuf>,
        /// Per-command timeout inside the guest.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// Directory for the steps' screenshots, as the emulator sees it:
        /// the emulator writes them. Defaults to the results directory.
        #[arg(long)]
        shots: Option<PathBuf>,
        /// The emulator's monitor, HOST:PORT (its `monitor_port`), for
        /// steps that type keys.
        #[arg(long)]
        monitor: Option<String>,
        /// Exit nonzero when any step failed, after saving the result. A
        /// skipped step is not a failure.
        #[arg(long)]
        strict: bool,
    },

    /// Print a screenshot's signature, the form a step's `golden` takes.
    ///
    /// With a second file, print how far apart the two are as well: the
    /// figure a step's `tolerance` and `min_change` are measured in.
    ScreenSig {
        png: PathBuf,
        other: Option<PathBuf>,
    },

    /// Print a `data/bench_reference.json` row for a saved result.
    ///
    /// The reference table is a static file updated by hand — run the suite on
    /// a machine, run this, paste the row in, commit. There is no upload and no
    /// user-writable override; a machine with no row simply has no comparison.
    Reference {
        /// Result JSON to convert. Defaults to the newest in the results dir.
        #[arg(long)]
        from: Option<PathBuf>,
        /// Short stable identifier, e.g. `m1-max-interp`.
        #[arg(long)]
        id: String,
        /// Human label. Defaults to "<host cpu> — <engine>".
        #[arg(long)]
        label: Option<String>,
        /// Measurement date, `YYYY-MM-DD`. Defaults to today.
        #[arg(long)]
        measured: Option<String>,
        /// Merge into this table in place instead of printing the row.
        #[arg(long)]
        into: Option<PathBuf>,
    },

    /// List the cells `matrix` knows about.
    Cells,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = dispatch(cli.cmd) {
        eprintln!("iris-bench: {}", e);
        std::process::exit(1);
    }
}

fn dispatch(cmd: Cmd) -> Result<(), String> {
    match cmd {
        Cmd::Cells => {
            println!("{:<24} {:<22} {}", "cell", "cargo features", "--cpu");
            for c in CELLS {
                println!("{:<24} {:<22} {}", c.name,
                         if c.features.is_empty() { "(default)" } else { c.features }, c.cpu);
            }
            Ok(())
        }

        Cmd::Run { iris, elf, config, label, out, timeout, quick, cpu, cache, deterministic, mut extra } => {
            let out = out.unwrap_or_else(default_out);
            let label = if deterministic { format!("{label}+deterministic") } else { label };
            let run = match iris {
                Some(iris) => {
                    if quick {
                        return Err("--quick needs the in-process runner; drop --iris".into());
                    }
                    let elf = elf.unwrap_or_else(|| repo_relative("bench/build/irisbench.elf"));
                    let config = config.unwrap_or_else(|| repo_relative("bench/run/bare.toml"));
                    if deterministic {
                        extra.extend(["--jitv2-compile", "sync"].map(String::from));
                    }
                    run_guest(&iris, &elf, &config, &label, timeout, &extra)?
                }
                None => {
                    // Silently ignoring these would be worse than refusing:
                    // they all describe a subprocess that is not being started,
                    // and a run that quietly measured the wrong thing is the
                    // failure mode this whole suite exists to avoid.
                    let stray = [
                        elf.is_some().then_some("--elf"),
                        config.is_some().then_some("--config"),
                        (!extra.is_empty()).then_some("trailing emulator arguments"),
                    ];
                    let stray: Vec<&str> = stray.into_iter().flatten().collect();
                    if !stray.is_empty() {
                        let (verb, obj) = if stray.len() == 1 { ("applies", "it") }
                                          else { ("apply", "them") };
                        return Err(format!(
                            "{} only {} to --iris, and `run` is in-process by default. \
                             Add --iris PATH, or drop {}.", stray.join(" and "), verb, obj));
                    }
                    run_embedded(&label, timeout, quick, cpu, cache, deterministic)?
                }
            };
            let path = save(&run, &out)?;
            print!("{}", text_summary(std::slice::from_ref(&run)));
            println!("wrote {}", path.display());
            Ok(())
        }

        Cmd::Host { exe, out, timeout } => {
            let exe = exe.unwrap_or_else(|| repo_relative("bench/build/irisbench-host"));
            let out = out.unwrap_or_else(default_out);
            let run = run_host(&exe, timeout)?;
            let path = save(&run, &out)?;
            print!("{}", text_summary(std::slice::from_ref(&run)));
            println!("wrote {}", path.display());
            Ok(())
        }

        Cmd::Matrix { cells, out, force_build, no_host, timeout } => {
            let root = if PathBuf::from("Cargo.toml").exists() { PathBuf::from(".") }
                       else { PathBuf::from("..") };
            let out = out.unwrap_or_else(default_out);
            let wanted: Vec<&Cell> = match &cells {
                Some(list) => {
                    let names: Vec<&str> = list.split(',').map(str::trim).collect();
                    let sel: Vec<&Cell> = CELLS.iter().filter(|c| names.contains(&c.name)).collect();
                    for n in &names {
                        if !CELLS.iter().any(|c| &c.name == n) {
                            return Err(format!("unknown cell '{}' (try `iris-bench cells`)", n));
                        }
                    }
                    sel
                }
                // The four that answer "which CPU, which engine". The
                // lightning cells are opt-in: they trade away breakpoints and
                // the traceback buffer, so they are a release-build question
                // rather than a default comparison.
                None => CELLS.iter().take(4).collect(),
            };

            let elf = root.join("bench/build/irisbench.elf");
            if !elf.exists() {
                return Err(format!("no suite binary at {} — run `make -C bench` first", elf.display()));
            }
            let config = root.join("bench/run/bare.toml");

            let mut failures = Vec::new();
            for cell in &wanted {
                println!("== {} ==", cell.name);
                let iris = match build_cell(cell, &root, force_build) {
                    Ok(p) => p,
                    Err(e) => { eprintln!("  {}", e); failures.push(cell.name); continue; }
                };
                let cpu_arg = ["--cpu".to_string(), cell.cpu.to_string()];
                match run_guest(&iris, &elf, &config, cell.name, timeout, &cpu_arg) {
                    Ok(run) => {
                        // The guest reads PRId, so its banner is the authority
                        // on which CPU actually ran.
                        if run.machine.cpu != cell.expect_cpu {
                            eprintln!("  FAIL {} — guest reports cpu={}, expected {}",
                                      cell.name, run.machine.cpu, cell.expect_cpu);
                            failures.push(cell.name);
                            continue;
                        }
                        let path = save(&run, &out)?;
                        print!("  ");
                        print!("{}", text_summary(std::slice::from_ref(&run)));
                        println!("  wrote {}", path.display());
                    }
                    Err(e) => { eprintln!("  FAIL {} — {}", cell.name, e); failures.push(cell.name); }
                }
            }

            if !no_host {
                println!("== host ==");
                let exe = root.join("bench/build/irisbench-host");
                match run_host(&exe, timeout) {
                    Ok(run) => { let p = save(&run, &out)?; println!("  wrote {}", p.display()); }
                    Err(e) => eprintln!("  host baseline skipped: {}", e),
                }
            }

            println!();
            if failures.is_empty() { println!("matrix: every cell completed"); }
            else { println!("matrix: {} cell(s) failed: {}", failures.len(), failures.join(" ")); }

            let runs = load_all(&out)?;
            let md = markdown(&runs, None);
            let report = out.join("report.md");
            std::fs::write(&report, &md).map_err(|e| format!("{}: {}", report.display(), e))?;
            println!("report: {}", report.display());
            if !failures.is_empty() { return Err("some cells failed".into()); }
            Ok(())
        }

        Cmd::ScreenSig { png, other } => {
            let sig = ScreenSig::from_png(&png)?;
            println!("{}", sig);
            if let Some(other) = other {
                println!("distance {:.2}", sig.distance(&ScreenSig::from_png(&other)?)?);
            }
            Ok(())
        }

        Cmd::Irix { socket, steps, extra_steps, ci, shell, label, out, timeout, shots, monitor, strict } => {
            let ci_bin = ci.unwrap_or_else(|| repo_relative(
                if cfg!(windows) { "target/release/iris-ci.exe" } else { "target/release/iris-ci" }));
            if !ci_bin.exists() {
                return Err(format!("no iris-ci at {} — cargo build --release --bin iris-ci",
                                   ci_bin.display()));
            }
            let steps = steps.unwrap_or_else(|| repo_relative("bench/irix/steps.toml"));
            let out = out.unwrap_or_else(default_out);
            let ci = Ci { bin: ci_bin, socket, shell, timeout, monitor };
            let shots = Shots { dir: shots.unwrap_or_else(|| out.clone()), label: label.clone() };
            let run = run_irix(&ci, &steps, &extra_steps, &label, &shots)?;
            let path = save(&run, &out)?;
            println!("wrote {}", path.display());
            let (ok, failed, skipped) = step_outcomes(&run);
            println!("{}: {} ok, {} failed, {} skipped", label, ok, failed, skipped);
            if strict && failed > 0 {
                return Err(format!("{} of {} steps failed", failed, run.rows.len()));
            }
            Ok(())
        }

        Cmd::Reference { from, id, label, measured, into } => {
            let path = match from {
                Some(p) => p,
                None => newest_result(&default_out())?,
            };
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("{}: {}", path.display(), e))?;
            let run: Run = serde_json::from_str(&text)
                .map_err(|e| format!("{}: {}", path.display(), e))?;
            if run.suite_id.is_empty() {
                return Err(format!(
                    "{} has no suite_id — it predates the field, or it is a host run. \
                     Re-run the suite to record one.", path.display()));
            }
            // A shortened run is accurate but imprecise, and the table is what
            // every other machine gets compared against. Refuse rather than
            // quietly enshrine a noisy row.
            if let Some(why) = run.settings.shortened_because() {
                return Err(format!(
                    "{} was not a full run ({}). Reference rows must be full runs — \
                     re-run without --quick.", path.display(), why));
            }
            let entry = reference_entry(&run, &id, label.as_deref(), measured.as_deref());

            let Some(table_path) = into else {
                println!("{}", serde_json::to_string_pretty(&entry).map_err(|e| e.to_string())?);
                eprintln!("\n// paste into data/bench_reference.json \"entries\" \
                           (suite_id {})", run.suite_id);
                return Ok(());
            };

            let mut table: ReferenceTable = match std::fs::read_to_string(&table_path) {
                Ok(t) => serde_json::from_str(&t).map_err(|e| format!("{}: {}", table_path.display(), e))?,
                Err(_) => ReferenceTable { schema: 1, suite_id: run.suite_id.clone(), entries: Vec::new() },
            };
            // An empty table adopts the incoming suite; a populated one must
            // agree, or the merged file would hold two different workloads'
            // numbers under one name.
            if table.entries.is_empty() {
                table.suite_id = run.suite_id.clone();
            } else if table.suite_id != run.suite_id {
                return Err(format!(
                    "suite mismatch: {} holds {} but this result is {}. \
                     The suite changed — regenerate every row, or start a new table.",
                    table_path.display(), table.suite_id, run.suite_id));
            }
            table.entries.retain(|e| e.id != entry.id);
            table.entries.push(entry);
            table.entries.sort_by(|a, b| a.id.cmp(&b.id));
            std::fs::write(&table_path, serde_json::to_string_pretty(&table).map_err(|e| e.to_string())? + "\n")
                .map_err(|e| format!("{}: {}", table_path.display(), e))?;
            println!("{}: {} entries (suite {})", table_path.display(), table.entries.len(), table.suite_id);
            Ok(())
        }

        Cmd::Report { dir, baseline, format, out } => {
            let dir = dir.unwrap_or_else(default_out);
            let runs = load_all(&dir)?;
            let text = match format.as_str() {
                "md" => markdown(&runs, baseline.as_deref()),
                "text" => text_summary(&runs),
                "json" => serde_json::to_string_pretty(&runs).map_err(|e| e.to_string())?,
                _ => unreachable!(),
            };
            match out {
                Some(p) => {
                    std::fs::write(&p, &text).map_err(|e| format!("{}: {}", p.display(), e))?;
                    println!("wrote {}", p.display());
                }
                None => { let mut so = std::io::stdout(); let _ = so.write_all(text.as_bytes()); }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only what is local to this binary. The report model, its parser and the
    /// derived figures moved to `iris::bench_report` and are tested there —
    /// keeping a second copy here is how two parsers drift apart.
    #[test]
    fn every_cell_name_is_unique_and_maps_to_a_cpu() {
        for (i, a) in CELLS.iter().enumerate() {
            // `expect_cpu` is what the `matrix` guard compares the guest's
            // `#machine cpu=` line against, so it must describe the `--cpu`
            // this cell actually passes. CPU is a runtime flag (see `Cell::cpu`),
            // not a cargo feature, so the two agreeing is the whole contract.
            let want = a.cpu.split('_').next().unwrap().to_uppercase();
            assert_eq!(a.expect_cpu, want, "{}", a.name);
            assert!(bench_cpu(a.cpu).is_ok());
            for b in CELLS.iter().skip(i + 1) { assert_ne!(a.name, b.name); }
        }
    }

    /// Run a guest command line through the local Bourne shell, the way the
    /// guest's `sh -c` does after `iris-ci` types it.
    #[cfg(unix)]
    fn local_sh(cmd: &str) -> String {
        let out = Command::new("sh").arg("-c").arg(cmd).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn probe_finds_a_program_on_the_path_and_not_a_missing_one() {
        // `sh` is on every Unix PATH, and on IRIX_PATH's /bin or /usr/bin.
        assert!(probe_found(&local_sh(&probe_command("sh"))));
        assert!(!probe_found(&local_sh(&probe_command("irisbench-no-such-program"))));
    }

    #[cfg(unix)]
    #[test]
    fn probe_tests_an_absolute_path_as_given() {
        let sh = ["/bin/sh", "/usr/bin/sh"].into_iter()
            .find(|p| Path::new(p).exists()).unwrap();
        assert!(probe_found(&local_sh(&probe_command(sh))));
        assert!(!probe_found(&local_sh(&probe_command("/nonexistent/irisbench"))));
    }

    #[test]
    fn the_probe_command_line_does_not_contain_its_own_answer() {
        // The tty echoes the typed line back into the captured output.
        assert!(!probe_found(&probe_command("dd")));
        assert!(!probe_found(&probe_command("/usr/bin/dd")));
    }

    #[test]
    fn the_probe_survives_the_wrapper_quoting() {
        // Nothing in the wrapped command may close the single quote around it,
        // or the login shell parses the rest of the line itself.
        let w = wrap(&probe_command("dd"));
        assert_eq!(w.matches('\'').count(), 2, "{w}");
    }

    /// A scripted guest: programs it "has", commands that exit nonzero, and a
    /// log of every command line it was asked to run.
    struct FakeGuest {
        have: Vec<&'static str>,
        failing: Vec<&'static str>,
        broken: Option<&'static str>,
        hanging: Vec<&'static str>,
        output: &'static str,
        log: std::cell::RefCell<Vec<String>>,
        /// The colour the whole screen shows, for screenshots. A command
        /// containing "draw" turns it to `drawn`.
        screen: std::cell::Cell<[u8; 3]>,
        drawn: [u8; 3],
    }

    impl FakeGuest {
        fn new() -> Self {
            FakeGuest {
                have: vec![], failing: vec![], broken: None, hanging: vec![], output: "",
                log: Default::default(), screen: std::cell::Cell::new([40, 80, 120]),
                drawn: [200, 40, 40],
            }
        }
    }

    /// A frame of one colour, written as the emulator's screenshot would be.
    fn write_png(path: &Path, w: u32, h: u32, rgb: [u8; 3]) {
        let file = std::fs::File::create(path).unwrap();
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let px: Vec<u8> = (0..w * h).flat_map(|_| rgb).collect();
        enc.write_header().unwrap().write_image_data(&px).unwrap();
    }

    /// A screenshot directory of the test's own.
    fn shots_in(test: &str) -> Shots {
        let dir = std::env::temp_dir().join(format!("iris-bench-{}-{}", std::process::id(), test));
        std::fs::create_dir_all(&dir).unwrap();
        Shots { dir, label: "t".into() }
    }

    /// For runs that take no screenshot.
    fn no_shots() -> Shots {
        Shots { dir: PathBuf::from("/nonexistent"), label: "t".into() }
    }

    impl Guest for FakeGuest {
        fn interrupt(&self) -> Result<(), String> {
            self.log.borrow_mut().push("^C".into());
            Ok(())
        }

        fn run_within(&self, cmd: &str, _secs: Option<u64>) -> Result<CiRun, String> {
            self.log.borrow_mut().push(cmd.to_string());
            if self.hanging.iter().any(|h| cmd.contains(h)) {
                return Ok(CiRun { stdout: String::new(), secs: 9.0, exit: -1, timed_out: true });
            }
            if let Some(b) = self.broken {
                if cmd.contains(b) { return Err("iris-ci run failed: timed out".into()); }
            }
            let mut out = self.output.to_string();
            if self.have.iter().any(|p| cmd.contains(&probe_command(p))) {
                out.push_str("IRISBENCH-HAVE\n");
            }
            let exit = if self.failing.iter().any(|f| cmd.contains(f)) { 1 } else { 0 };
            if cmd.contains("draw") { self.screen.set(self.drawn); }
            Ok(CiRun { stdout: out, secs: 2.0, exit, timed_out: false })
        }

        fn screenshot(&self, path: &Path) -> Result<(), String> {
            self.log.borrow_mut().push(format!("shot {}", path.display()));
            write_png(path, 64, 48, self.screen.get());
            Ok(())
        }

        fn run_with_shot(&self, cmd: &str, secs: Option<u64>, at: u64, path: &Path)
            -> Result<(CiRun, Result<(), String>), String>
        {
            self.log.borrow_mut().push(format!("shot at {at}"));
            let r = self.run_within(cmd, secs)?;
            Ok((r, self.screenshot(path)))
        }

        fn monitor(&self, command: &str) -> Result<String, String> {
            self.log.borrow_mut().push(format!("monitor {command}"));
            if command.contains("refused") { return Err("monitor refused it".into()); }
            Ok(String::new())
        }
    }

    fn step(name: &str, cmd: &str, requires: Option<&str>) -> Step {
        Step {
            name: name.into(), unit: "op".into(), work: 10,
            cmd: cmd.into(), requires: requires.map(String::from),
            ..Default::default()
        }
    }

    fn shot(name: &str, cmd: &str, screenshot: &str) -> Step {
        Step { screenshot: Some(screenshot.into()), ..step(name, cmd, None) }
    }

    fn sig_of(w: u32, h: u32, rgb: [u8; 3]) -> ScreenSig {
        let px: Vec<u8> = (0..w * h).flat_map(|_| rgb).collect();
        ScreenSig::from_pixels(w, h, 3, &px).unwrap()
    }

    #[test]
    fn a_signature_survives_its_text_form() {
        let mut px: Vec<u8> = (0..640u32 * 480).flat_map(|i| [(i % 251) as u8, (i / 640) as u8, 7]).collect();
        px[0] = 255;
        let sig = ScreenSig::from_pixels(640, 480, 3, &px).unwrap();
        let text = sig.to_string();
        assert!(text.starts_with("sig1 16x12 640x480 "), "{text}");
        assert_eq!(text.parse::<ScreenSig>().unwrap(), sig);
        assert!("sig1 16x12 640x480 00".parse::<ScreenSig>().is_err());
        assert!("not a signature".parse::<ScreenSig>().is_err());
    }

    #[test]
    fn distance_is_the_mean_channel_difference_and_needs_one_frame_size() {
        let a = sig_of(64, 48, [10, 20, 30]);
        assert_eq!(a.distance(&a).unwrap(), 0.0);
        assert_eq!(a.distance(&sig_of(64, 48, [16, 20, 30])).unwrap(), 2.0);
        assert!(a.distance(&sig_of(32, 24, [10, 20, 30])).is_err());
        // RGBA reduces the same as RGB.
        let rgba: Vec<u8> = (0..64 * 48).flat_map(|_| [10, 20, 30, 255]).collect();
        assert_eq!(ScreenSig::from_pixels(64, 48, 4, &rgba).unwrap(), a);
    }

    #[test]
    fn a_screen_that_matches_its_golden_passes_and_one_that_does_not_fails() {
        let g = FakeGuest::new();
        let shots = shots_in("golden");
        let mut good = shot("desk", "sleep 1", "desktop");
        good.golden = Some(sig_of(64, 48, [41, 79, 121]).to_string());
        let mut bad = shot("desk2", "sleep 1", "desktop2");
        bad.golden = Some(sig_of(64, 48, [0, 0, 0]).to_string());
        let (rows, _) = run_steps(&g, &[good, bad], 0.0, &shots).unwrap();
        assert_eq!(rows[0].status, "UNCHECKED");
        assert_eq!(rows[0].guest.as_ref().unwrap().distance, Some(1.0));
        assert_eq!(rows[1].status, "FAILED");
        let seen = rows[1].guest.as_ref().unwrap();
        assert_eq!(seen.distance, Some(80.0));
        assert_eq!(seen.exit, 0);
        // The failing frame is kept, with the signature that would replace the golden.
        assert!(Path::new(seen.screenshot.as_ref().unwrap()).exists());
        assert!(seen.signature.as_ref().unwrap().starts_with("sig1 "));
    }

    #[test]
    fn a_program_that_draws_nothing_fails_its_differs_from_check() {
        let g = FakeGuest::new();
        let shots = shots_in("differs");
        let desk = shot("desk", "sleep 1", "desktop");
        let mut idle = shot("idle", "sleep 1", "idle");
        idle.differs_from = Some("desktop".into());
        let mut game = shot("game", "draw it", "game");
        game.differs_from = Some("desktop".into());
        let (rows, _) = run_steps(&g, &[desk, idle, game], 0.0, &shots).unwrap();
        let st: Vec<_> = rows.iter().map(|r| r.status.as_str()).collect();
        assert_eq!(st, ["UNCHECKED", "FAILED", "UNCHECKED"]);
        assert_eq!(rows[1].guest.as_ref().unwrap().distance, Some(0.0));
        assert!(rows[2].guest.as_ref().unwrap().distance.unwrap() > DEFAULT_MIN_CHANGE);
    }

    #[test]
    fn shot_at_takes_the_screenshot_while_the_command_runs() {
        let g = FakeGuest::new();
        let shots = shots_in("shot-at");
        let mut s = shot("demo", "draw demo", "demo");
        s.shot_at = Some(20);
        let (rows, _) = run_steps(&g, &[s], 0.0, &shots).unwrap();
        assert_eq!(rows[0].status, "UNCHECKED");
        let log = g.log.borrow();
        assert!(log.iter().any(|c| c == "shot at 20"));
        assert_eq!(log.iter().filter(|c| c.starts_with("shot /")).count(), 1);
    }

    #[test]
    fn a_skipped_step_takes_no_screenshot() {
        let g = FakeGuest::new();
        let mut s = shot("game", "draw", "game");
        s.requires = Some("/usr/games/none".into());
        let (rows, _) = run_steps(&g, &[s], 0.0, &no_shots()).unwrap();
        assert_eq!(rows[0].status, "SKIP");
        assert!(!g.log.borrow().iter().any(|c| c.starts_with("shot")));
    }

    #[test]
    fn outcomes_count_ok_failed_and_skipped_steps() {
        let mut g = FakeGuest::new();
        g.failing = vec!["bad"];
        let steps = [step("a", "ok", None), step("b", "bad", None), step("c", "x", Some("absent"))];
        let (rows, _) = run_steps(&g, &steps, 0.0, &no_shots()).unwrap();
        let run = Run { rows, ..guest_run("t") };
        assert_eq!(step_outcomes(&run), (1, 1, 1));
    }

    #[test]
    fn keys_are_typed_before_the_command_and_a_refused_key_fails_the_step() {
        let g = FakeGuest::new();
        let mut login = step("login", "wait-for-desktop", None);
        login.keys = vec!["key type root".into(), "key enter".into()];
        let mut bad = step("bad", "never-run", None);
        bad.keys = vec!["refused".into()];
        let (rows, _) = run_steps(&g, &[login, bad], 0.0, &no_shots()).unwrap();
        let st: Vec<_> = rows.iter().map(|r| r.status.as_str()).collect();
        assert_eq!(st, ["UNCHECKED", "FAILED"]);
        let log = g.log.borrow();
        let at = |c: &str| log.iter().position(|l| l.contains(c)).unwrap();
        assert!(at("monitor key type root") < at("monitor key enter"));
        assert!(at("monitor key enter") < at("wait-for-desktop"));
        assert!(!log.iter().any(|l| l.contains("never-run")));
    }

    #[test]
    fn screenshot_fields_are_checked_when_the_steps_load() {
        let mut lone = step("a", "x", None);
        lone.golden = Some(sig_of(64, 48, [0, 0, 0]).to_string());
        assert!(check_steps(&[lone]).unwrap_err().contains("need a screenshot"));
        let mut bad = shot("a", "x", "a");
        bad.golden = Some("sig1 nonsense".into());
        assert!(check_steps(&[bad]).unwrap_err().contains("golden"));
        let mut later = shot("b", "x", "b");
        later.differs_from = Some("a".into());
        assert!(check_screenshot_names(&[later]).unwrap_err().contains("no earlier step"));
        let twice = [shot("a", "x", "s"), shot("b", "y", "s")];
        assert!(check_screenshot_names(&twice).unwrap_err().contains("taken twice"));
    }

    #[cfg(unix)]
    #[test]
    fn a_quick_child_is_timed_to_the_millisecond_not_to_the_poll() {
        // The wait used to poll every 100 ms, so a child that exited at once
        // took 100 ms. Taking the best of several keeps a loaded host from
        // failing this: only a coarse poll can make every attempt slow.
        let best = (0..10)
            .map(|_| {
                let t = Instant::now();
                run_with_timeout(Command::new("true"), 30).unwrap();
                t.elapsed()
            })
            .min()
            .unwrap();
        assert!(best < Duration::from_millis(60), "best of 10 was {best:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_outlives_the_timeout_is_killed() {
        let mut c = Command::new("sleep");
        c.arg("30");
        let t = Instant::now();
        let e = run_with_timeout(c, 1).unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn a_finished_child_keeps_its_output_and_status() {
        let mut c = Command::new("sh");
        c.args(["-c", "echo out; echo err >&2; exit 3"]);
        let o = run_with_timeout(c, 30).unwrap();
        assert_eq!(o.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&o.stdout), "out\n");
        assert_eq!(String::from_utf8_lossy(&o.stderr), "err\n");
    }

    #[test]
    fn the_guest_name_comes_from_the_marked_line_only() {
        // The echo of the typed line first, then the output.
        let out = "echo IRISBENCH-UNAM\\E `uname -a`\r\nIRISBENCH-UNAME IRIX IRIS 6.5 10070055 IP22\r\n";
        assert_eq!(guest_name(out), "IRIX IRIS 6.5 10070055 IP22");
        // Echoed input that runs into the output does not leak into the name.
        let out = "echo IRIRISBENCH-UNAME IRIX64 IP27\n";
        assert_eq!(guest_name(out), "unknown");
        assert_eq!(guest_name(""), "unknown");
        assert_eq!(guest_name("IRISBENCH-UNAME \n"), "unknown");
    }

    #[test]
    fn the_uname_command_line_does_not_contain_its_own_marker() {
        assert!(!UNAME_COMMAND.contains("IRISBENCH-UNAME"));
        assert!(!UNAME_COMMAND.contains('\''));
    }

    fn guest_run(label: &str) -> Run {
        let mut timed = untimed_row(&step("disk/write", "x", None), "UNCHECKED");
        timed.unit = "B".into();
        timed.work = 16 << 20;
        timed.ns = 600_000_000;
        let mut find = untimed_row(&step("fs/walk_usr", "x", None), "UNCHECKED");
        find.work = 0;
        find.ns = 1_234_000_000;
        let mut dhry = untimed_row(&step("bench/dhry", "x", None), "UNCHECKED");
        dhry.work = 1000;
        dhry.ns = 2_000_000_000;
        dhry.guest = Some(GuestStep { metric: Some(555555.6), ..Default::default() });
        let rows = vec![
            timed, find, dhry,
            untimed_row(&step("gfx/x11perf", "x", None), "SKIP"),
            untimed_row(&step("gfx/xwd", "x", None), "FAILED"),
        ];
        Run {
            cell: label.into(),
            features: Vec::new(),
            machine: MachineInfo { cpu: "IRIX IRIS 6.5 10070055 IP22".into(), ..Default::default() },
            host: iris::bench_report::HostInfo::default(),
            rows,
            checked: 0,
            matched: 0,
            total_ns: 3_834_000_000,
            total_icount: 0,
            wall_s: 3.8,
            suite_id: String::new(),
            settings: RunSettings::default(),
        }
    }

    #[test]
    fn a_guest_run_is_reported_by_step_with_no_accuracy() {
        let md = markdown(&[guest_run("indy")], None);
        assert!(!md.contains("0.0% (0/0)"), "{md}");
        assert!(md.contains("| indy |") && md.contains("| IRIX IRIS 6.5 10070055 IP22 | - |"), "{md}");
        // Every step is listed, including the ones with no fixed amount of work,
        // the skipped one and the failed one.
        assert!(md.contains("| disk/write | ok | 0.600 s | 27.96 M B/s | - |"), "{md}");
        assert!(md.contains("| fs/walk_usr | ok | 1.234 s | - | - |"), "{md}");
        assert!(md.contains("| bench/dhry | ok | 2.000 s | 500.0 op/s | 555555.6 |"), "{md}");
        assert!(md.contains("| gfx/x11perf | skipped | - | - | - |"), "{md}");
        assert!(md.contains("| gfx/xwd | FAILED | - | - | - |"), "{md}");
        // Nothing that compares bare-metal kernels.
        assert!(!md.contains("Per-kernel"), "{md}");
    }

    #[test]
    fn a_guest_run_in_the_text_summary_counts_its_steps() {
        let t = text_summary(&[guest_run("indy")]);
        assert!(t.contains("3 timed, 1 skipped, 1 failed"), "{t}");
        assert!(!t.contains("accuracy"), "{t}");
    }

    #[test]
    fn guest_exit_is_read_from_iris_ci_stderr() {
        assert_eq!(guest_exit("iris-ci: iris error: guest exit 1\n"), Some(1));
        assert_eq!(guest_exit("iris-ci: iris error: guest exit 127"), Some(127));
        assert_eq!(guest_exit("iris-ci: iris error: guest exit -1"), Some(-1));
        assert_eq!(guest_exit("iris-ci: connect /x: No such file"), None);
        assert_eq!(guest_exit("iris-ci: timed out waiting for the prompt"), None);
    }

    #[test]
    fn a_failing_guest_command_with_no_output_is_a_result_not_an_error() {
        let r = classify_run(false, String::new(), "iris-ci: iris error: guest exit 2\n", 1.5).unwrap();
        assert_eq!(r.exit, 2);
        let r = classify_run(true, "x".into(), "", 1.5).unwrap();
        assert_eq!(r.exit, 0);
        // iris-ci itself failing is still an error, output or not.
        assert!(classify_run(false, String::new(), "iris-ci: connect /x: refused", 1.5).is_err());
        assert!(classify_run(false, "partial".into(), "iris-ci: timed out", 1.5).is_err());
        // A command that never finished is a step result too.
        let r = classify_run(false, String::new(),
            "iris-ci: iris error: wait-serial: wait-serial: timeout after 30000ms waiting for \"M\"", 31.0).unwrap();
        assert!(r.timed_out);
    }

    #[test]
    fn present_programs_run_and_absent_ones_are_skipped() {
        let mut g = FakeGuest::new();
        g.have = vec!["dd"];
        let steps = [step("a", "dd-work", Some("dd")), step("b", "x11-work", Some("x11perf"))];
        let (rows, total) = run_steps(&g, &steps, 0.5, &no_shots()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].name.as_str(), rows[0].status.as_str()), ("a", "UNCHECKED"));
        assert_eq!(rows[0].ns, 1_500_000_000);
        assert_eq!((rows[1].name.as_str(), rows[1].status.as_str()), ("b", "SKIP"));
        assert_eq!(rows[1].ns, 0);
        assert_eq!(total, 1_500_000_000);
        // The skipped step's command never reached the guest.
        assert!(!g.log.borrow().iter().any(|c| c.contains("x11-work")));
    }

    #[test]
    fn a_failed_step_is_recorded_and_the_suite_goes_on() {
        let mut g = FakeGuest::new();
        g.failing = vec!["bad-cmd"];
        let steps = [step("first", "ok-cmd", None), step("bad", "bad-cmd", None),
                     step("last", "ok-cmd2", None)];
        let (rows, total) = run_steps(&g, &steps, 0.0, &no_shots()).unwrap();
        let st: Vec<_> = rows.iter().map(|r| r.status.as_str()).collect();
        assert_eq!(st, ["UNCHECKED", "FAILED", "UNCHECKED"]);
        // A failed step contributes no time to the total.
        assert_eq!(rows[1].ns, 0);
        assert_eq!(total, 4_000_000_000);
    }

    #[test]
    fn a_failing_setup_fails_the_step_without_running_it() {
        let mut g = FakeGuest::new();
        g.failing = vec!["bad-setup"];
        let mut s = step("s", "the-cmd", None);
        s.setup = Some("bad-setup".into());
        let (rows, _) = run_steps(&g, &[s], 0.0, &no_shots()).unwrap();
        assert_eq!(rows[0].status, "FAILED");
        assert!(!g.log.borrow().iter().any(|c| c.contains("the-cmd")));
    }

    #[test]
    fn losing_the_guest_aborts_the_run() {
        let mut g = FakeGuest::new();
        g.broken = Some("hang-cmd");
        let steps = [step("a", "ok-cmd", None), step("b", "hang-cmd", None), step("c", "ok-cmd", None)];
        assert!(run_steps(&g, &steps, 0.0, &no_shots()).is_err());
    }

    #[test]
    fn metric_is_the_first_number_after_the_label() {
        let out = "Dhrystones per Second:                      555555.6\nother 12\n";
        assert_eq!(extract_metric("Dhrystones per Second:", out), Some(555555.6));
        assert_eq!(extract_metric("MIPS:", "MIPS: 1,234"), Some(1234.0));
        assert_eq!(extract_metric("Whetstones:", "Whetstones:   1666.7 MIPS\n"), Some(1666.7));
        // A number before the label, or on an earlier line, is not the figure.
        assert_eq!(extract_metric("rate", "10 loops, rate 7.5/s"), Some(7.5));
        // A sentence-ending full stop is not part of the number.
        assert_eq!(extract_metric("took", "it took 12."), Some(12.0));
    }

    #[test]
    fn metric_uses_the_last_place_the_label_appears() {
        let out = "throughput: 1.0 MB/s\nthroughput: 2.5 MB/s wall\n";
        assert_eq!(extract_metric("throughput:", out), Some(2.5));
    }

    #[test]
    fn metric_that_is_absent_or_not_a_number_is_none() {
        assert_eq!(extract_metric("throughput:", "no figure here"), None);
        assert_eq!(extract_metric("name:", "name: abc"), None);
        assert_eq!(extract_metric("total:", "total:"), None);
    }

    #[test]
    fn long_output_keeps_its_tail_on_a_character_boundary() {
        assert_eq!(keep_tail("short", 16), "short");
        let s = format!("{}END", "é".repeat(10));
        let k = keep_tail(&s, 5);
        assert!(k.ends_with("END"), "{k}");
        assert!(k.starts_with("[18 bytes cut]\n"), "{k}");
    }

    #[test]
    fn step_output_and_metric_are_kept_in_the_result() {
        let mut g = FakeGuest::new();
        g.output = "Dhrystones per Second: 1000.5\n";
        let mut s = step("dhry", "run-it", None);
        s.metric = Some("Second:".into());
        let (rows, _) = run_steps(&g, &[s], 0.0, &no_shots()).unwrap();
        let guest = rows[0].guest.as_ref().unwrap();
        assert_eq!(guest.output, "Dhrystones per Second: 1000.5\n");
        assert_eq!(guest.metric, Some(1000.5));
        assert_eq!(guest.exit, 0);
        // And it survives the trip through the JSON result file.
        let json = serde_json::to_string(&rows[0]).unwrap();
        let back: Row = serde_json::from_str(&json).unwrap();
        assert_eq!(back.guest.unwrap().metric, Some(1000.5));
    }

    #[test]
    fn a_failed_step_keeps_what_it_printed() {
        let mut g = FakeGuest::new();
        g.failing = vec!["bad-cmd"];
        g.output = "boom\n";
        let (rows, _) = run_steps(&g, &[step("bad", "bad-cmd", None)], 0.0, &no_shots()).unwrap();
        assert_eq!(rows[0].status, "FAILED");
        let guest = rows[0].guest.as_ref().unwrap();
        assert_eq!((guest.exit, guest.output.as_str()), (1, "boom\n"));
    }

    #[test]
    fn rows_without_guest_output_serialise_as_before() {
        let g = FakeGuest::new();
        let (mut rows, _) = run_steps(&g, &[step("a", "c", Some("nothere"))], 0.0, &no_shots()).unwrap();
        let json = serde_json::to_string(&rows.remove(0)).unwrap();
        assert!(!json.contains("guest"), "{json}");
    }

    #[test]
    fn extra_steps_run_before_the_trailing_cleanup() {
        let mut clean = step("cleanup", "rm", None);
        clean.skip_timing = true;
        let base = vec![step("a", "x", None), step("b", "y", None), clean];
        let m = merge_steps(base, vec![step("c", "z", None), step("d", "w", None)]);
        let names: Vec<_> = m.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "d", "cleanup"]);
    }

    #[test]
    fn an_extra_step_with_an_existing_name_replaces_it_in_place() {
        let base = vec![step("a", "x", None), step("b", "old", None), step("c", "z", None)];
        let m = merge_steps(base, vec![step("b", "new", None)]);
        let cmds: Vec<_> = m.iter().map(|s| s.cmd.as_str()).collect();
        assert_eq!(cmds, ["x", "new", "z"]);
    }

    #[test]
    fn a_steps_file_with_a_quote_or_a_repeated_name_is_refused() {
        assert!(check_steps(&[step("a", "echo 'hi'", None)]).is_err());
        assert!(check_steps(&[step("a", "x", Some("it's"))]).is_err());
        assert!(check_steps(&[step("a", "x", None), step("a", "y", None)]).is_err());
        assert!(check_steps(&[step("a", "x", None), step("b", "y", None)]).is_ok());
    }

    #[test]
    fn the_shipped_steps_parse_and_pass_the_checks() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/irix/steps.toml");
        let steps = load_steps(&p).unwrap_or_else(|e| panic!("{e}"));
        assert!(!steps.is_empty());
    }

    #[test]
    fn the_graphics_acceptance_steps_parse_and_pass_the_checks() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/irix/gfx");
        for m in ["fuel", "octane", "indy"] {
            let base = load_steps(&dir.join("steps.toml")).unwrap_or_else(|e| panic!("{e}"));
            let mine = load_steps(&dir.join(format!("{m}.toml"))).unwrap_or_else(|e| panic!("{e}"));
            let n = base.len();
            let steps = merge_steps(base, mine);
            assert_eq!(steps.len(), n, "{m}: a machine file only replaces shared steps");
            check_screenshot_names(&steps).unwrap_or_else(|e| panic!("{m}: {e}"));
            // The Fuel's L1 console hands a command and iris-ci's end
            // marker to IRIX together, and the pair must fit its tty.
            for s in &steps {
                for line in [Some(&s.cmd), s.setup.as_ref()].into_iter().flatten() {
                    assert!(wrap(line).len() <= 200, "{m}: {}: {} characters", s.name, wrap(line).len());
                }
            }
            let desktop = steps.iter().find(|s| s.name == "gfx/desktop").unwrap();
            assert!(desktop.golden.is_some(), "{m}: the desktop has no golden");
        }
    }

    #[test]
    fn the_cold_read_does_not_read_the_file_the_write_step_just_wrote() {
        let steps = load_steps(&Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/irix/steps.toml")).unwrap();
        let get = |n: &str| steps.iter().find(|s| s.name == n).unwrap();
        let (write, cold, warm) = (get("disk/write"), get("disk/read_cold"), get("disk/read_warm"));
        // That file is still in the buffer cache when the next step starts.
        assert!(write.cmd.contains("irisbench.dat") && !cold.cmd.contains("irisbench.dat"));
        // The cold file is written by the step's setup, larger than memory,
        // and the warm step reads its own file once beforehand.
        assert!(cold.setup.as_deref().unwrap().contains("hinv -c memory"));
        assert!(warm.setup.as_deref().unwrap().contains("irisbench.dat"));
    }

    #[test]
    fn a_command_the_guests_tty_would_cut_is_refused() {
        // A wrapped line is 12 + IRIX_PATH + 15 + command + 1 characters.
        let room = MAX_COMMAND_LINE - wrap("").len();
        assert!(check_steps(&[step("a", &"x".repeat(room), None)]).is_ok());
        let e = check_steps(&[step("a", &"x".repeat(room + 1), None)]).unwrap_err();
        assert!(e.contains("256 characters once wrapped"), "{e}");
        let mut s = step("b", "ok", None);
        s.setup = Some("y".repeat(room + 1));
        assert!(check_steps(&[s]).unwrap_err().contains("the setup"));
    }

    #[test]
    fn a_hung_step_fails_and_the_guest_is_interrupted_before_the_next_one() {
        let mut g = FakeGuest::new();
        g.hanging = vec!["hang-cmd"];
        let steps = [step("a", "hang-cmd", None), step("b", "ok-cmd", None)];
        let (rows, _) = run_steps(&g, &steps, 0.0, &no_shots()).unwrap();
        let st: Vec<_> = rows.iter().map(|r| r.status.as_str()).collect();
        assert_eq!(st, ["FAILED", "UNCHECKED"]);
        let log = g.log.borrow();
        let ctrl_c = log.iter().position(|c| c == "^C").unwrap();
        assert!(log[..ctrl_c].iter().any(|c| c.contains("hang-cmd")));
        assert!(log[ctrl_c..].iter().any(|c| c.contains("ok-cmd")));
    }

    #[test]
    fn a_guest_that_ignores_ctrl_c_ends_the_run() {
        let mut g = FakeGuest::new();
        // The recovery check is the no-op `:`; it hangs too.
        g.hanging = vec!["hang-cmd", "export PATH; :"];
        let steps = [step("a", "hang-cmd", None), step("b", "ok-cmd", None)];
        assert!(run_steps(&g, &steps, 0.0, &no_shots()).is_err());
    }
}
