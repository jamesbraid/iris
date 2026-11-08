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
    find_features, fmt_rate, host_info, parse_block, reference_entry, MachineInfo, ReferenceTable, Row, Run,
    RunSettings, EXPECT_EXC,
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

/// Counts the user-mode instructions this process and everything it starts
/// afterwards retire, so a deterministic run can say what it cost the host.
/// Linux only, through `perf_event_open`; where that is unavailable or not
/// permitted `finish` returns None and the report says the figure is missing.
struct InsnCounter {
    #[cfg(target_os = "linux")]
    fd: libc::c_int,
}

#[cfg(target_os = "linux")]
impl InsnCounter {
    fn start() -> Option<Self> {
        // The first 64 bytes of `struct perf_event_attr` (PERF_ATTR_SIZE_VER0),
        // which every kernel since 2.6.31 accepts.
        let mut attr = [0u64; 8];
        attr[0] = 0 /* PERF_TYPE_HARDWARE */ | (64u64 << 32) /* size */;
        attr[1] = 1; // PERF_COUNT_HW_INSTRUCTIONS
        // bit 1 inherit (threads and children started later), bit 5
        // exclude_kernel, bit 6 exclude_hv: what `perf stat -e instructions:u` counts.
        attr[5] = (1 << 1) | (1 << 5) | (1 << 6);
        // SAFETY: `attr` is a live, correctly sized perf_event_attr and the
        // remaining arguments are plain integers; the call only reads it.
        let fd = unsafe {
            libc::syscall(libc::SYS_perf_event_open, attr.as_ptr(), 0 as libc::pid_t,
                          -1 as libc::c_int, -1 as libc::c_int,
                          8 /* PERF_FLAG_FD_CLOEXEC */ as libc::c_ulong)
        };
        if fd < 0 { None } else { Some(InsnCounter { fd: fd as libc::c_int }) }
    }

    fn finish(self) -> Option<u64> {
        let mut v = 0u64;
        // SAFETY: reads at most 8 bytes into `v`, which is 8 bytes.
        let n = unsafe { libc::read(self.fd, (&mut v as *mut u64).cast(), 8) };
        // SAFETY: `fd` is ours and closed exactly once, here.
        unsafe { libc::close(self.fd) };
        (n == 8).then_some(v)
    }
}

#[cfg(not(target_os = "linux"))]
impl InsnCounter {
    fn start() -> Option<Self> { None }
    fn finish(self) -> Option<u64> { None }
}

/// Start counting, run `f`, stop counting.
fn with_host_instructions<T>(f: impl FnOnce() -> T) -> (T, Option<u64>) {
    let counter = InsnCounter::start();
    let out = f();
    (out, counter.and_then(InsnCounter::finish))
}

/// The cargo features an emulator binary was built with, from the banner it
/// prints on stderr before it reads any configuration. `--help` gets that
/// far and exits, so this costs one process start rather than a run.
fn probe_features(iris: &Path) -> Result<Vec<String>, String> {
    let mut cmd = Command::new(iris);
    cmd.arg("--help");
    let out = run_with_timeout(cmd, 60)?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    find_features(&stderr).ok_or_else(|| format!(
        "{} printed no `build features:` line for --help, so its build cannot be checked. \
         Is it an iris binary?", iris.display()))
}

/// The emulator arguments that make a run deterministic for a binary built
/// with `features`, or why it cannot be.
fn deterministic_args(features: &[String]) -> Result<Vec<String>, String> {
    let has = |f: &str| features.iter().any(|x| x == f);
    if !has("ci_clock") {
        return Err(format!(
            "--deterministic needs a `ci_clock` build: its test device clock counts retired \
             instructions, which is what makes the suite pick the same work every run. This \
             binary was built with [{}], so the run would be timed and its counts would vary. \
             Rebuild with --features ci_clock.", features.join(" ")));
    }
    if has("lightning") && has("jitv2") {
        return Err(
            "--deterministic cannot work on a build with both `lightning` and `jitv2`: lightning \
             always compiles on the pool, which refuses `--jitv2-compile sync`, so the JIT's \
             timing would vary the counts. Use lightning without jitv2, or jitv2 without lightning."
                .into());
    }
    // Lightning without the JIT has nothing to compile, and refuses the option.
    if has("lightning") { Ok(Vec::new()) }
    else { Ok(["--jitv2-compile", "sync"].map(String::from).to_vec()) }
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
    let (out, host_instructions) = with_host_instructions(|| run_with_timeout(cmd, timeout_s));
    let out = out?;
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
        host_instructions,
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
        host_instructions: None,
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
    let (run, host_instructions) = with_host_instructions(|| {
        bench_runner::run(&opts, |p| {
            if let bench_runner::Progress::Line(l) = p {
                println!("{}", l);
            }
        })
    });
    let mut run = run?;
    run.host_instructions = host_instructions;
    Ok(run)
}

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
                std::thread::sleep(std::time::Duration::from_millis(100));
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

/// Write a result without replacing an earlier one: `<label>.json`, then
/// `<label>.2.json`, `<label>.3.json`, and so on. A rerun under the same label
/// is another sample of that cell and `report` combines them; overwriting
/// would throw away the very repeat that tells you how noisy the cell is.
fn save(run: &Run, dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    let json = serde_json::to_string_pretty(run).map_err(|e| e.to_string())?;
    for n in 1u32.. {
        let name = if n == 1 { format!("{}.json", run.cell) } else { format!("{}.{}.json", run.cell, n) };
        let path = dir.join(name);
        // create_new, so two runs finishing together cannot pick one name.
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(json.as_bytes()).map_err(|e| format!("{}: {}", path.display(), e))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {}", path.display(), e)),
        }
    }
    unreachable!()
}

/// Most recently modified `*.json` in `dir`, skipping the host run (it has no
/// suite_id and is not a machine anyone compares against).
fn newest_result(dir: &Path) -> Result<PathBuf, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        // `host.json`, or `host.2.json` for a repeat.
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem.split('.').next() == Some("host") { continue; }
        let Ok(m) = e.metadata().and_then(|m| m.modified()) else { continue };
        if best.as_ref().map_or(true, |(t, _)| m > *t) { best = Some((m, p)); }
    }
    best.map(|(_, p)| p)
        .ok_or_else(|| format!("no result files in {} — run `iris-bench run` first", dir.display()))
}

fn load_all(dir: &Path) -> Result<Vec<Run>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json")).collect();
    // Directory order is arbitrary; a report of the same files should read the same.
    paths.sort();
    let mut runs = Vec::new();
    for p in paths {
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

#[derive(Debug, Deserialize)]
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
}

/// Wrap a guest command so it runs under a Bourne shell with a sane PATH,
/// whatever the login shell is. Consequence, documented in steps.toml: a step
/// command may not contain a single quote.
fn wrap(cmd: &str) -> String {
    format!("sh -c 'PATH={}; export PATH; {}'", IRIX_PATH, cmd)
}

struct Ci {
    bin: PathBuf,
    socket: Option<PathBuf>,
    shell: String,
    timeout: u64,
}

impl Ci {
    /// Returns the guest's stdout. An iris-ci failure is an error; a nonzero
    /// exit status inside the guest is not — several steps end on a command
    /// that legitimately returns nonzero.
    fn run(&self, guest_cmd: &str) -> Result<(String, f64), String> {
        let mut cmd = Command::new(&self.bin);
        if let Some(s) = &self.socket { cmd.arg("--socket").arg(s); }
        cmd.arg("run")
            .arg(guest_cmd)
            .arg("--shell").arg(&self.shell)
            .arg("--timeout").arg(self.timeout.to_string());
        let started = Instant::now();
        let out = run_with_timeout(cmd, self.timeout + 30)?;
        let secs = started.elapsed().as_secs_f64();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() && stdout.trim().is_empty() {
            return Err(format!(
                "iris-ci run failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok((stdout, secs))
    }

    fn has(&self, prog: &str) -> Result<bool, String> {
        let probe = format!(
            "for d in `echo {} | tr : \\\" \\\"`; do test -x $d/{} && echo IRISBENCH-HAVE; done",
            IRIX_PATH, prog
        );
        let (out, _) = self.run(&wrap(&probe))?;
        Ok(out.contains("IRISBENCH-HAVE"))
    }

    /// Smallest round trip we can measure: everything a step's timing has in
    /// common with doing nothing at all — serial latency, prompt matching,
    /// process spawn. Subtracted from every step, so a one-second step is a
    /// second of work rather than a second of work plus the harness.
    fn baseline(&self) -> Result<f64, String> {
        let mut best = f64::MAX;
        for _ in 0..3 {
            let (_, s) = self.run(&wrap(":"))?;
            if s < best { best = s; }
        }
        Ok(best)
    }
}

fn run_irix(ci: &Ci, steps_path: &Path, label: &str) -> Result<Run, String> {
    let text = std::fs::read_to_string(steps_path)
        .map_err(|e| format!("{}: {}", steps_path.display(), e))?;
    let file: StepFile = toml::from_str(&text)
        .map_err(|e| format!("{}: {}", steps_path.display(), e))?;

    // Identify the guest before anything else: a result recorded against the
    // wrong disk image is worse than no result.
    let (uname, _) = ci.run(&wrap("uname -a"))?;
    let cpu = uname.lines().find(|l| l.contains("IRIX")).unwrap_or("IRIX").trim().to_string();
    println!("  guest: {}", cpu);

    let baseline = ci.baseline()?;
    println!("  round-trip floor: {:.0} ms", baseline * 1e3);

    let mut rows = Vec::new();
    let mut total_ns = 0u64;

    for step in &file.step {
        if let Some(prog) = &step.requires {
            if !ci.has(prog)? {
                println!("  {:<22} skipped ({} not installed)", step.name, prog);
                continue;
            }
        }
        if let Some(setup) = &step.setup { ci.run(&wrap(setup))?; }

        let (_, secs) = ci.run(&wrap(&step.cmd))?;
        if step.skip_timing { continue; }

        let ns = ((secs - baseline).max(0.0) * 1e9) as u64;
        total_ns += ns;
        println!(
            "  {:<22} {:>8.2} s{}",
            step.name,
            ns as f64 / 1e9,
            if step.work > 0 {
                format!("   {}/s", fmt_rate(step.work as f64 * 1e9 / ns.max(1) as f64))
            } else {
                String::new()
            }
        );
        rows.push(Row {
            name: step.name.clone(),
            unit: step.unit.clone(),
            iters: 1,
            work: step.work,
            ns,
            icount: 0,
            count: 0,
            exc: 0,
            checksum: "0x0000000000000000".into(),
            golden: "0x0000000000000000".into(),
            // Nothing here is checksummed: these are IRIX's own tools against
            // IRIX's own filesystem, and their output is not ours to predict.
            status: "UNCHECKED".into(),
        });
    }

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
        host_instructions: None,
    })
}

// ─── reports ─────────────────────────────────────────────────────────────────

/// Why a result cannot be a reference row, if it cannot. A deterministic run
/// reports constant rates (100.0 MIPS, a DMIPS set by the compiler), so a row
/// made from one would compare every machine against a number that is not a speed.
fn reference_refusal(run: &Run) -> Result<(), String> {
    match clock_of(run) {
        Clock::Timed => Ok(()),
        Clock::Counted => Err("this is a deterministic (ci_clock) run: its rates are constants of \
            the build, not speeds. Reference rows must come from a timed run.".into()),
        Clock::Mislabelled => Err("this run is labelled +deterministic, which marks rates that are \
            not speeds. Reference rows must come from a timed run; re-run under a label without it.".into()),
    }
}

fn fmt_ratio(a: f64, b: f64) -> String {
    if b <= 0.0 || a <= 0.0 { return "-".into(); }
    let r = a / b;
    if r >= 100.0 { format!("{:.0}x", r) } else { format!("{:.2}x", r) }
}

/// The runs saved under one cell label. A rerun is a new sample of the same
/// cell, not a different cell, so the report combines them.
struct Group<'a> {
    cell: &'a str,
    runs: Vec<&'a Run>,
}

/// Group runs by cell label, in the order each label first appears.
fn group_runs(runs: &[Run]) -> Vec<Group<'_>> {
    let mut groups: Vec<Group<'_>> = Vec::new();
    for r in runs {
        match groups.iter_mut().find(|g| g.cell == r.cell) {
            Some(g) => g.runs.push(r),
            None => groups.push(Group { cell: &r.cell, runs: vec![r] }),
        }
    }
    groups
}

/// The middle, minimum and maximum of a cell's samples.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stat {
    median: f64,
    min: f64,
    max: f64,
    n: usize,
}

fn stat_of(mut v: Vec<f64>) -> Option<Stat> {
    if v.is_empty() { return None; }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    let median = if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 };
    Some(Stat { median, min: v[0], max: v[n - 1], n })
}

impl Group<'_> {
    /// A statistic over the group's runs; runs without a value are left out.
    fn stat(&self, f: impl Fn(&Run) -> Option<f64>) -> Option<Stat> {
        stat_of(self.runs.iter().filter_map(|r| f(r)).collect())
    }
    /// One kernel's rate over the group's runs. A run that lacks the kernel,
    /// or measured nothing for it, is not a sample.
    fn rate(&self, kernel: &str) -> Option<Stat> {
        self.stat(|r| r.row(kernel).map(|x| x.rate()).filter(|v| *v > 0.0))
    }
    /// The run whose overall speed is the group's median, for the sections
    /// that need one run's rows (where the time went).
    fn typical(&self) -> &Run {
        let mut by_speed: Vec<&Run> = self.runs.clone();
        by_speed.sort_by(|a, b| a.mips().partial_cmp(&b.mips()).unwrap_or(std::cmp::Ordering::Equal));
        by_speed[(by_speed.len() - 1) / 2]
    }
    /// The run with the lowest accuracy: a group is as correct as its worst run.
    fn least_accurate(&self) -> &Run {
        self.runs.iter().copied()
            .min_by(|a, b| a.accuracy().partial_cmp(&b.accuracy()).unwrap_or(std::cmp::Ordering::Equal))
            .expect("a group has at least one run")
    }
    fn label(&self) -> String {
        if self.runs.len() > 1 { format!("{} (n={})", self.cell, self.runs.len()) }
        else { self.cell.to_string() }
    }
}

/// `median`, and `[min-max]` after it when there is more than one sample.
fn fmt_stat(s: Option<Stat>, f: impl Fn(f64) -> String) -> String {
    match s {
        None => "-".into(),
        Some(s) if s.n == 1 => f(s.median),
        Some(s) => format!("{} [{}-{}]", f(s.median), f(s.min), f(s.max)),
    }
}

/// A whole count with thousands separators: `613,905,711`.
fn fmt_count(v: f64) -> String {
    let digits = format!("{:.0}", v);
    let mut o = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 { o.push(','); }
        o.push(c);
    }
    o
}

fn is_counted(g: &Group) -> bool { clock_of(g.runs[0]) == Clock::Counted }

/// Guest and host instruction counts for the counted (`ci_clock`) groups.
///
/// The suite's rates in these runs are constants of the build: the test
/// device's clock counts retired instructions, so every kernel "runs" at the
/// same speed however slow the emulator is. What a run does tell is how many
/// guest instructions the work took, which is exact, and how many host
/// instructions the emulator spent on it.
fn counted_markdown(counted: &[&Group], baseline: Option<&str>, cmp: Compare) -> String {
    let mut o = String::new();
    let base = baseline
        .and_then(|b| counted.iter().copied().find(|g| g.cell == b))
        .or_else(|| counted.first().copied());

    o.push_str("## Deterministic runs\n\n");
    o.push_str("These runs used the `ci_clock` clock, which counts retired instructions, so \
                they report instructions instead of speed. They are not compared with the \
                timed cells. Guest instructions are exact; host instructions are what the \
                emulator spent, counted in user mode over the whole run (`-` where the host \
                could not count them).\n\n");
    o.push_str("| cell | n | features | CPU | suite | run | accuracy | guest instructions | host instructions | wall |\n");
    o.push_str("|---|---:|---|---|---|---|---:|---:|---:|---:|\n");
    for g in counted {
        let r = g.runs[0];
        let feats = if r.features.is_empty() { "-".to_string() } else { r.features.join(" ") };
        let worst = g.least_accurate();
        o.push_str(&format!(
            "| {} | {} | {} | {} | `{}` | {} | {:.1}% ({}/{}) | {} | {} | {} |\n",
            g.cell, g.runs.len(), feats, r.machine.cpu, short_suite(&r.suite_id),
            if r.settings.is_full() { "full" } else { "quick" },
            worst.accuracy(), worst.matched, worst.checked,
            fmt_stat(g.stat(|r| Some(r.total_icount as f64).filter(|v| *v > 0.0)), fmt_count),
            fmt_stat(g.stat(|r| r.host_instructions.map(|v| v as f64)), fmt_count),
            fmt_stat(g.stat(|r| Some(r.wall_s)), |v| format!("{:.1} s", v)),
        ));
    }
    o.push('\n');

    let versus: Vec<&&Group> = counted.iter().filter(|g| base.map_or(false, |b| g.cell != b.cell)).collect();
    if let Some(b) = base {
        for g in &versus {
            let hi = |g: &Group| g.stat(|r| r.host_instructions.map(|v| v as f64)).map_or(0.0, |s| s.median);
            let gi = |g: &Group| g.stat(|r| Some(r.total_icount as f64)).map_or(0.0, |s| s.median);
            let ax = not_comparable(g, b, cmp);
            if !ax.is_empty() {
                o.push_str(&format!("{} against {}: n/c, they differ in {}.\n\n", g.cell, b.cell, axis_names(&ax)));
            } else {
                o.push_str(&format!("{} against {}: guest instructions {}, host instructions {}.\n\n",
                                    g.cell, b.cell, fmt_ratio(gi(g), gi(b)), fmt_ratio(hi(g), hi(b))));
            }
        }
    }

    o.push_str("### Guest instructions per kernel\n\n");
    o.push_str("A kernel whose count differs between two builds of the same image did \
                different work.\n\n");
    o.push_str("| benchmark |");
    for g in counted { o.push_str(&format!(" {} |", g.label())); }
    for g in &versus { o.push_str(&format!(" {} vs base |", g.cell)); }
    o.push('\n');
    o.push_str("|---|");
    for _ in 0..counted.len() + versus.len() { o.push_str("---:|"); }
    o.push('\n');
    let mut names: Vec<&str> = Vec::new();
    for g in counted { for r in &g.runs { for row in &r.rows {
        if row.icount > 0 && !names.contains(&row.name.as_str()) { names.push(&row.name); }
    } } }
    for name in names {
        o.push_str(&format!("| {} |", name));
        let stat = |g: &Group| g.stat(|r| r.row(name).map(|x| x.icount as f64).filter(|v| *v > 0.0));
        for g in counted { o.push_str(&format!(" {} |", fmt_stat(stat(g), fmt_count))); }
        if let Some(b) = base {
            for g in &versus {
                let cell = if not_comparable(g, b, cmp).is_empty() {
                    fmt_ratio(stat(g).map_or(0.0, |s| s.median), stat(b).map_or(0.0, |s| s.median))
                } else { "n/c".into() };
                o.push_str(&format!(" {} |", cell));
            }
        }
        o.push('\n');
    }
    o.push('\n');
    o
}

/// A cell's rate as a fraction of the host's own on the same kernel: `1/12`
/// when the emulator is twelve times slower, `1.50x` if it were faster.
fn fmt_native(rate: f64, host: f64) -> String {
    if rate <= 0.0 || host <= 0.0 { return "-".into(); }
    let slower = host / rate;
    if slower < 1.0 { format!("{:.2}x", rate / host) }
    else if slower < 10.0 { format!("1/{:.1}", slower) }
    else { format!("1/{:.0}", slower) }
}

/// The first 12 characters of a suite ID, enough to tell two images apart.
fn short_suite(id: &str) -> &str {
    if id.is_empty() { "-" } else { &id[..id.len().min(12)] }
}

/// A warning naming which runs used which guest image, when they did not all
/// use the same one; None when they agree.
///
/// Kernel rates and DMIPS count iterations of compiled guest code, so they
/// compare emulators only on the same image. A rebuild of the image with
/// another compiler changed the instructions per Dhrystone iteration by 37%
/// and read exactly like an emulator regression. Host runs carry no suite ID
/// and are left out.
fn suite_mismatch(runs: &[Run]) -> Option<String> {
    let mut by_suite: Vec<(&str, Vec<&str>)> = Vec::new();
    for r in runs.iter().filter(|r| !r.suite_id.is_empty()) {
        match by_suite.iter_mut().find(|(s, _)| *s == r.suite_id) {
            Some((_, cells)) => cells.push(&r.cell),
            None => by_suite.push((&r.suite_id, vec![&r.cell])),
        }
    }
    if by_suite.len() < 2 {
        return None;
    }
    let mut o = String::from(
        "These runs used different guest images, so kernel rates and DMIPS \
         are not comparable across them, and guest MIPS is only roughly so: \
         the instruction mix differs too.\n");
    for (suite, cells) in &by_suite {
        o.push_str(&format!("  {}: {}\n", short_suite(suite), cells.join(", ")));
    }
    Some(o)
}

/// What a run's clock was: the host's, or one that counts retired
/// instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clock {
    Timed,
    /// A `ci_clock` build: the test device reads retired instructions, so every
    /// rate the suite prints is a constant of the build, not a speed.
    Counted,
    /// Labelled `+deterministic` but the build has no `ci_clock`: not
    /// deterministic, and not a plain timed run either.
    Mislabelled,
}

fn clock_of(r: &Run) -> Clock {
    if r.features.iter().any(|f| f == "ci_clock") { Clock::Counted }
    else if r.cell.ends_with("+deterministic") { Clock::Mislabelled }
    else { Clock::Timed }
}

/// What can differ between two runs and make their rates incomparable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    /// The guest binary; see `suite_mismatch`.
    Suite,
    /// Quick or full: groups, timed-run length, passes per kernel.
    Mode,
    /// Timed on the host clock, or counted on `ci_clock`'s.
    Clock,
    /// The CPU the emulator ran on.
    Host,
    /// The emulator's cargo features: its engine, and what it traces.
    Features,
}

impl Axis {
    fn what(self) -> &'static str {
        match self {
            Axis::Suite => "guest image",
            Axis::Mode => "run length (quick or full)",
            Axis::Clock => "clock (timed, or deterministic with ci_clock)",
            Axis::Host => "host CPU",
            Axis::Features => "emulator build features",
        }
    }
    /// This run's value on the axis, or None when the axis does not apply to it
    /// (a host run or an IRIX run has no guest image).
    fn value(self, r: &Run) -> Option<String> {
        // The host run is the suite compiled natively: no emulator, so no
        // clock or build features to compare.
        if r.cell == "host" && matches!(self, Axis::Clock | Axis::Features) { return None; }
        match self {
            Axis::Suite => Some(r.suite_id.clone()).filter(|s| !s.is_empty()),
            Axis::Mode => Some(r.settings.shortened_because().map_or("full".into(), |w| format!("quick: {w}"))),
            Axis::Clock => Some(match clock_of(r) {
                Clock::Timed => "timed",
                Clock::Counted => "deterministic (ci_clock)",
                Clock::Mislabelled => "labelled +deterministic, but no ci_clock",
            }.into()),
            Axis::Host => Some(format!("{} ({} {})", r.host.cpu_model, r.host.os, r.host.arch)),
            // ci_clock is the clock axis, not a second difference.
            Axis::Features => {
                let f: Vec<&str> = r.features.iter().map(String::as_str).filter(|f| *f != "ci_clock").collect();
                Some(if f.is_empty() { "(none)".into() } else { f.join(" ") })
            }
        }
    }
}

/// How strictly a report treats differences.
#[derive(Debug, Clone, Copy, Default)]
struct Compare {
    /// Cells built with different cargo features are meant to be compared:
    /// that is what `matrix` is for. Without this, differing features are
    /// reported and their ratios withheld like any other difference.
    across_features: bool,
}

impl Compare {
    fn axes(self) -> Vec<Axis> {
        let mut v = vec![Axis::Suite, Axis::Mode, Axis::Clock, Axis::Host];
        if !self.across_features { v.push(Axis::Features); }
        v
    }
}

/// The axes, of those in `axes`, on which `runs` do not all agree.
fn differing(runs: &[&Run], axes: &[Axis]) -> Vec<Axis> {
    axes.iter().copied().filter(|a| {
        let mut seen: Vec<String> = Vec::new();
        for v in runs.iter().filter_map(|r| a.value(r)) {
            if !seen.contains(&v) { seen.push(v); }
        }
        seen.len() > 1
    }).collect()
}

/// The axes on which two cells' runs disagree, so their ratio means nothing.
fn not_comparable(a: &Group, b: &Group, cmp: Compare) -> Vec<Axis> {
    let all: Vec<&Run> = a.runs.iter().chain(b.runs.iter()).copied().collect();
    differing(&all, &cmp.axes())
}

/// A cell against the host's own run of the same kernels: only the host and
/// the run length matter there, the host run has no engine or guest image.
fn not_comparable_to_native(a: &Group, host: &Group) -> Vec<Axis> {
    let all: Vec<&Run> = a.runs.iter().chain(host.runs.iter()).copied().collect();
    differing(&all, &[Axis::Mode, Axis::Host])
}

/// Warnings for every axis other than the guest image (`suite_mismatch` owns
/// that one) on which the runs disagree. Each names the values and which
/// cells had them. None when they all agree.
///
/// Counted (`ci_clock`) runs are reported apart from timed ones and never set
/// beside them, so run length and features are judged within each kind. The
/// host CPU is judged across both: a counted run's host instruction figure
/// is still a property of the machine. The clock is only an issue for a run
/// labelled deterministic whose build has no ci_clock.
fn mixed_runs(runs: &[Run], cmp: Compare) -> Option<String> {
    let all: Vec<&Run> = runs.iter().collect();
    let timed: Vec<&Run> = runs.iter().filter(|r| clock_of(r) != Clock::Counted).collect();
    let counted: Vec<&Run> = runs.iter().filter(|r| clock_of(r) == Clock::Counted).collect();
    let mislabelled = timed.iter().any(|r| clock_of(r) == Clock::Mislabelled);

    let mut checks: Vec<(Axis, &[&Run])> = Vec::new();
    for axis in cmp.axes() {
        match axis {
            Axis::Suite => {}
            Axis::Mode | Axis::Features => {
                checks.push((axis, &timed));
                checks.push((axis, &counted));
            }
            Axis::Clock => if mislabelled { checks.push((axis, &all)) },
            Axis::Host => checks.push((axis, &all)),
        }
    }

    let mut o = String::new();
    for (axis, set) in checks {
        if differing(set, &[axis]).is_empty() { continue; }
        let mut by_value: Vec<(String, Vec<&str>)> = Vec::new();
        for r in set.iter().copied() {
            let Some(v) = axis.value(r) else { continue };
            match by_value.iter_mut().find(|(x, _)| *x == v) {
                Some((_, cells)) => if !cells.contains(&r.cell.as_str()) { cells.push(&r.cell) },
                None => by_value.push((v, vec![&r.cell])),
            }
        }
        o.push_str(&format!(
            "These runs differ in {}, so their rates are not comparable; \
             ratios between cells that differ are shown as n/c.\n", axis.what()));
        for (v, cells) in &by_value {
            o.push_str(&format!("  {}: {}\n", v, cells.join(", ")));
        }
    }
    if o.is_empty() { None } else { Some(o) }
}

/// Each distinct host and the cells that ran on it, one line per host.
fn host_lines(runs: &[Run]) -> Vec<String> {
    let mut hosts: Vec<(String, Vec<&str>)> = Vec::new();
    for r in runs {
        let h = format!("{} ({} {}, {} cores)", r.host.cpu_model, r.host.os, r.host.arch, r.host.cores);
        match hosts.iter_mut().find(|(x, _)| *x == h) {
            Some((_, cells)) => if !cells.contains(&r.cell.as_str()) { cells.push(&r.cell) },
            None => hosts.push((h, vec![&r.cell])),
        }
    }
    hosts.into_iter().map(|(h, cells)| format!("{}: {}", h, cells.join(", "))).collect()
}

fn axis_names(axes: &[Axis]) -> String {
    axes.iter().map(|a| a.what()).collect::<Vec<_>>().join(", ")
}

/// `markdown_with` under the default comparison rules, for the tests.
#[cfg(test)]
fn markdown(runs: &[Run], baseline: Option<&str>) -> String {
    markdown_with(runs, baseline, Compare::default())
}

fn markdown_with(runs: &[Run], baseline: Option<&str>, cmp: Compare) -> String {
    let mut o = String::new();
    let all_groups = group_runs(runs);
    // Counted (`ci_clock`) runs have no speed to report; they get their own section.
    let counted: Vec<&Group> = all_groups.iter().filter(|g| is_counted(g)).collect();
    let groups: Vec<&Group> = all_groups.iter().filter(|g| !is_counted(g)).collect();
    let emulated: Vec<&Group> = groups.iter().copied().filter(|g| g.cell != "host").collect();
    let host = groups.iter().copied().find(|g| g.cell == "host");
    let base = baseline
        .and_then(|b| groups.iter().copied().find(|g| g.cell == b))
        .or_else(|| emulated.first().copied());

    o.push_str("# IRIS benchmark report\n\n");
    // Every run's own host, not the first run's: results from several
    // machines end up in one directory.
    let hosts = host_lines(runs);
    if hosts.len() == 1 {
        if let Some(h) = runs.first() {
            o.push_str(&format!(
                "Host: {} {} / {} / {} cores\n\n",
                h.host.os, h.host.arch, h.host.cpu_model, h.host.cores
            ));
        }
    } else {
        o.push_str("Hosts:\n\n");
        for h in &hosts { o.push_str(&format!("- {}\n", h)); }
        o.push('\n');
    }
    if let Some(w) = suite_mismatch(runs) {
        for (i, line) in w.lines().enumerate() {
            o.push_str(if i == 0 { "> **Warning:** " } else { ">\n> " });
            o.push_str(line.trim_start());
            o.push('\n');
        }
        o.push('\n');
    }
    if let Some(w) = mixed_runs(runs, cmp) {
        for (i, line) in w.lines().enumerate() {
            o.push_str(if i == 0 { "> **Warning:** " } else { ">\n> " });
            o.push_str(line.trim_start());
            o.push('\n');
        }
        o.push('\n');
    }

    // ── per-cell summary ────────────────────────────────────────────────────
    if groups.is_empty() {
        o.push_str(&counted_markdown(&counted, baseline, cmp));
        return o;
    }
    o.push_str("## Cells\n\n");
    o.push_str("A cell run more than once shows the median, with `[min-max]` after it.\n\n");
    o.push_str("| cell | n | features | CPU | suite | run | clock | accuracy | guest MIPS | DMIPS | whet/s | LINPACK MFLOPS | timed | wall |\n");
    o.push_str("|---|---:|---|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|\n");
    for g in &groups {
        let r = g.runs[0];
        let feats = if r.features.is_empty() { "-".to_string() } else { r.features.join(" ") };
        let worst = g.least_accurate();
        o.push_str(&format!(
            "| {} | {} | {} | {} | `{}` | {} | {} | {:.1}% ({}/{}) | {} | {} | {} | {} | {} | {} |\n",
            g.cell, g.runs.len(), feats, r.machine.cpu, short_suite(&r.suite_id),
            if r.settings.is_full() { "full" } else { "quick" },
            match clock_of(r) { Clock::Timed => "timed", Clock::Counted => "ci_clock", Clock::Mislabelled => "timed?" },
            worst.accuracy(), worst.matched, worst.checked,
            fmt_stat(g.stat(|r| Some(r.mips()).filter(|v| *v > 0.0)), |v| format!("{:.1}", v)),
            fmt_stat(g.stat(|r| r.dmips()), |v| format!("{:.1}", v)),
            fmt_stat(g.stat(|r| r.whet_loops()), |v| format!("{:.0}", v)),
            fmt_stat(g.stat(|r| r.linpack_mflops()), |v| format!("{:.2}", v)),
            fmt_stat(g.stat(|r| Some(r.total_ns as f64 / 1e9)), |v| format!("{:.1} s", v)),
            fmt_stat(g.stat(|r| Some(r.wall_s)), |v| format!("{:.1} s", v)),
        ));
    }
    o.push('\n');

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
    o.push_str("Work units per second, in each kernel's own unit. Each `vs base` \
                column is that cell's median rate over the median rate of **");
    o.push_str(base.map(|b| b.cell).unwrap_or("-"));
    o.push_str("**");
    if host.is_some() {
        o.push_str("; each `native` column is that cell's median rate as a fraction of the \
                    host's own on the identical kernel");
    }
    o.push_str(".\n\n");

    let versus: Vec<&Group> = emulated.iter().copied()
        .filter(|g| base.map_or(false, |b| g.cell != b.cell)).collect();

    let blocked_vs_base = |g: &Group| -> Vec<Axis> {
        base.map_or(Vec::new(), |b| not_comparable(g, b, cmp))
    };
    let mut notes: Vec<String> = Vec::new();
    for g in &versus {
        let ax = blocked_vs_base(g);
        if !ax.is_empty() {
            notes.push(format!("{} vs base: differs from {} in {}", g.cell,
                               base.map_or("-", |b| b.cell), axis_names(&ax)));
        }
    }
    if let Some(h) = host {
        for g in &emulated {
            let ax = not_comparable_to_native(g, h);
            if !ax.is_empty() {
                notes.push(format!("{} native: differs from {} in {}", g.cell, h.cell, axis_names(&ax)));
            }
        }
    }

    o.push_str("| benchmark | unit |");
    for g in &emulated { o.push_str(&format!(" {} |", g.label())); }
    for g in &versus { o.push_str(&format!(" {} vs base |", g.cell)); }
    if let Some(h) = host {
        o.push_str(&format!(" {} |", h.label()));
        for g in &emulated { o.push_str(&format!(" {} native |", g.cell)); }
    }
    o.push('\n');
    o.push_str("|---|---|");
    for _ in 0..emulated.len() + versus.len() { o.push_str("---:|"); }
    if host.is_some() {
        for _ in 0..1 + emulated.len() { o.push_str("---:|"); }
    }
    o.push('\n');

    let timed_runs: Vec<&Run> = groups.iter().flat_map(|g| g.runs.iter().copied()).collect();
    let names = ordered_names(&timed_runs);
    for name in &names {
        let unit = timed_runs.iter().find_map(|r| r.row(name).map(|x| x.unit.clone()))
            .unwrap_or_default();
        o.push_str(&format!("| {} | {} |", name, unit));
        let median = |g: &Group| g.rate(name).map_or(0.0, |s| s.median);
        for g in &emulated { o.push_str(&format!(" {} |", fmt_stat(g.rate(name), fmt_rate))); }
        if let Some(b) = base {
            for g in &versus {
                let cell = if blocked_vs_base(g).is_empty() { fmt_ratio(median(g), median(b)) }
                           else { "n/c".into() };
                o.push_str(&format!(" {} |", cell));
            }
        }
        if let Some(h) = host {
            let hv = median(h);
            o.push_str(&format!(" {} |", fmt_stat(h.rate(name), fmt_rate)));
            for g in &emulated {
                let cell = if not_comparable_to_native(g, h).is_empty() { fmt_native(median(g), hv) }
                           else { "n/c".into() };
                o.push_str(&format!(" {} |", cell));
            }
        }
        o.push('\n');
    }
    o.push('\n');
    if !notes.is_empty() {
        o.push_str("`n/c` means not comparable, and no ratio is shown:\n\n");
        for n in &notes { o.push_str(&format!("- {}\n", n)); }
        o.push('\n');
    }

    // ── efficiency ──────────────────────────────────────────────────────────
    for g in &emulated {
        let r = g.typical();
        if r.mips() <= 0.0 { continue; }
        o.push_str(&format!("## Where {} spends its time\n\n", g.cell));
        if g.runs.len() > 1 {
            o.push_str(&format!("The median-speed run of {}.\n\n", g.runs.len()));
        }

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

    if !counted.is_empty() {
        o.push_str(&counted_markdown(&counted, baseline, cmp));
    }

    o
}

/// Every kernel name that appears anywhere, in the order the first run lists
/// them — the suite's own order, which groups related kernels together.
fn ordered_names(runs: &[&Run]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for r in runs.iter().copied() {
        for row in &r.rows {
            if !names.iter().any(|n| n == &row.name) { names.push(row.name.clone()); }
        }
    }
    names
}

fn text_summary(runs: &[Run]) -> String {
    text_summary_with(runs, Compare::default())
}

fn text_summary_with(runs: &[Run], cmp: Compare) -> String {
    let mut o = String::new();
    for g in group_runs(runs) {
        let worst = g.least_accurate();
        if is_counted(&g) {
            // Counts, not speeds: a ci_clock run's MIPS and DMIPS are constants.
            o.push_str(&format!(
                "{:<24} {:>6.1}% accuracy   {} guest instructions   {} host instructions\n",
                g.label(), worst.accuracy(),
                fmt_stat(g.stat(|r| Some(r.total_icount as f64).filter(|v| *v > 0.0)), fmt_count),
                fmt_stat(g.stat(|r| r.host_instructions.map(|v| v as f64)), fmt_count),
            ));
            continue;
        }
        o.push_str(&format!(
            "{:<24} {:>6.1}% accuracy   {:>8}   {:>7} DMIPS   {:>7} timed\n",
            g.label(), worst.accuracy(),
            match g.stat(|r| Some(r.mips()).filter(|v| *v > 0.0)) {
                Some(s) => format!("{} MIPS", fmt_stat(Some(s), |v| format!("{:.1}", v))),
                None => "n/a".into(),
            },
            fmt_stat(g.stat(|r| r.dmips()), |v| format!("{:.1}", v)),
            fmt_stat(g.stat(|r| Some(r.total_ns as f64 / 1e9)), |v| format!("{:.1} s", v)),
        ));
    }
    if let Some(w) = suite_mismatch(runs) {
        o.push_str("warning: ");
        o.push_str(&w);
    }
    if let Some(w) = mixed_runs(runs, cmp) {
        o.push_str("warning: ");
        o.push_str(&w);
    }
    for h in host_lines(runs) {
        o.push_str(&format!("host: {}\n", h));
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
        /// The result is labelled `<label>+deterministic`. With --iris the
        /// emulator is asked for its `build features:` line first and the run
        /// is refused unless it has `ci_clock`; `--jitv2-compile sync` is
        /// passed where the build allows it (not on a lightning build).
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
        /// Show ratios between cells built with different cargo features. By
        /// default they are withheld, since a feature change is a different
        /// emulator; `matrix` is the case where that is the point.
        #[arg(long)]
        across_features: bool,
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
        /// Step definitions. Defaults to bench/irix/steps.toml.
        #[arg(long)]
        steps: Option<PathBuf>,
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
                        // Refuse before the run: a binary without ci_clock
                        // gives a timed run, whatever the label says.
                        let features = probe_features(&iris)?;
                        let args = deterministic_args(&features)
                            .map_err(|e| format!("{}: {}", iris.display(), e))?;
                        extra.extend(args);
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
            // Its cells differ in features by design.
            let md = markdown_with(&runs, None, Compare { across_features: true });
            let report = out.join("report.md");
            std::fs::write(&report, &md).map_err(|e| format!("{}: {}", report.display(), e))?;
            println!("report: {}", report.display());
            if !failures.is_empty() { return Err("some cells failed".into()); }
            Ok(())
        }

        Cmd::Irix { socket, steps, ci, shell, label, out, timeout } => {
            let ci_bin = ci.unwrap_or_else(|| repo_relative(
                if cfg!(windows) { "target/release/iris-ci.exe" } else { "target/release/iris-ci" }));
            if !ci_bin.exists() {
                return Err(format!("no iris-ci at {} — cargo build --release --bin iris-ci",
                                   ci_bin.display()));
            }
            let steps = steps.unwrap_or_else(|| repo_relative("bench/irix/steps.toml"));
            let out = out.unwrap_or_else(default_out);
            let ci = Ci { bin: ci_bin, socket, shell, timeout };
            let run = run_irix(&ci, &steps, &label)?;
            let path = save(&run, &out)?;
            println!("wrote {}", path.display());
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
            if let Err(why) = reference_refusal(&run) {
                return Err(format!("{}: {}", path.display(), why));
            }
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

        Cmd::Report { dir, baseline, format, across_features, out } => {
            let dir = dir.unwrap_or_else(default_out);
            let runs = load_all(&dir)?;
            let cmp = Compare { across_features };
            let text = match format.as_str() {
                "md" => markdown_with(&runs, baseline.as_deref(), cmp),
                "text" => text_summary_with(&runs, cmp),
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

    fn run_on(cell: &str, suite: &str) -> Run {
        Run {
            cell: cell.into(),
            features: Vec::new(),
            machine: Default::default(),
            host: Default::default(),
            rows: Vec::new(),
            checked: 0,
            matched: 0,
            total_ns: 0,
            total_icount: 0,
            wall_s: 0.0,
            suite_id: suite.into(),
            settings: Default::default(),
            host_instructions: None,
        }
    }

    #[test]
    fn a_report_across_guest_images_says_so() {
        let runs = [
            run_on("before", "blake3:3bc1c0000000"),
            run_on("after", "blake3:be5200000000"),
            run_on("host", ""),
        ];
        let w = suite_mismatch(&runs).expect("two images must be reported");
        assert!(w.contains("blake3:3bc1c: before"), "{w}");
        assert!(w.contains("blake3:be520: after"), "{w}");
        assert!(!w.contains("host"), "a host run has no image: {w}");
        assert!(markdown(&runs, Some("before")).contains("**Warning:**"));
        assert!(text_summary(&runs).contains("warning: "));
    }

    #[test]
    fn a_report_on_one_guest_image_is_quiet() {
        let runs = [run_on("a", "blake3:3bc1c0000000"), run_on("b", "blake3:3bc1c0000000"), run_on("host", "")];
        assert_eq!(suite_mismatch(&runs), None);
        assert!(!markdown(&runs, None).contains("Warning"));
    }

    fn row(name: &str, work: u64, ns: u64) -> Row {
        Row {
            name: name.into(), unit: "ops".into(), iters: 1, work, ns,
            icount: 0, count: 0, exc: 0,
            checksum: "0x0".into(), golden: "0x0".into(), status: "OK".into(),
        }
    }

    /// A run whose kernels all do `work` units, taking `ns` each.
    fn run_rates(cell: &str, kernels: &[(&str, u64, u64)]) -> Run {
        let mut r = run_on(cell, "blake3:3bc1c0000000");
        r.rows = kernels.iter().map(|(n, w, ns)| row(n, *w, *ns)).collect();
        r
    }

    /// The line of the per-kernel table for `kernel`, split into its cells.
    fn kernel_cells(md: &str, kernel: &str) -> Vec<String> {
        let line = md.lines().find(|l| l.starts_with(&format!("| {} |", kernel)))
            .unwrap_or_else(|| panic!("no row for {kernel} in:\n{md}"));
        line.trim_matches('|').split('|').map(|c| c.trim().to_string()).collect()
    }

    /// The audit's first bug: "vs base" divided the best of all cells by the
    /// base, so a cell that lost 14% against the base still read 1.00x.
    #[test]
    fn vs_base_compares_each_cell_with_the_base_and_can_go_below_one() {
        // rates per second: base 100, slower 86, faster 125
        let runs = [
            run_rates("base", &[("int/alu", 100, 1_000_000_000)]),
            run_rates("faster", &[("int/alu", 125, 1_000_000_000)]),
            run_rates("slower", &[("int/alu", 86, 1_000_000_000)]),
        ];
        let md = markdown(&runs, Some("base"));
        assert!(md.contains("| faster vs base |"), "{md}");
        assert!(md.contains("| slower vs base |"), "{md}");
        // columns: kernel, unit, base, faster, slower, faster vs base, slower vs base
        let c = kernel_cells(&md, "int/alu");
        assert_eq!(c[5], "1.25x", "{md}");
        assert_eq!(c[6], "0.86x", "{md}");
    }

    #[test]
    fn vs_base_defaults_to_the_first_emulated_cell() {
        let runs = [
            run_rates("a", &[("int/alu", 100, 1_000_000_000)]),
            run_rates("b", &[("int/alu", 50, 1_000_000_000)]),
        ];
        let c = kernel_cells(&markdown(&runs, None), "int/alu");
        assert_eq!(c[4], "0.50x");
    }

    #[test]
    fn native_is_a_fraction_of_the_host_per_cell() {
        let runs = [
            run_rates("fast", &[("int/alu", 500, 1_000_000_000)]),
            run_rates("slow", &[("int/alu", 50, 1_000_000_000)]),
            run_rates("host", &[("int/alu", 1000, 1_000_000_000)]),
        ];
        let md = markdown(&runs, Some("fast"));
        // kernel, unit, fast, slow, slow vs base, host, fast native, slow native
        let c = kernel_cells(&md, "int/alu");
        assert_eq!(c[6], "1/2.0", "{md}");
        assert_eq!(c[7], "1/20", "{md}");
    }

    #[test]
    fn native_above_the_host_reads_as_a_multiple() {
        assert_eq!(fmt_native(1500.0, 1000.0), "1.50x");
        assert_eq!(fmt_native(0.0, 1000.0), "-");
    }

    /// A scratch directory under the system temp dir, empty and removed on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("iris-bench-test-{}-{}", std::process::id(), name));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    /// The audit's fourth bug: `save` wrote `<label>.json`, so a rerun with
    /// the same label replaced the earlier result.
    #[test]
    fn a_rerun_under_the_same_label_is_kept() {
        let dir = TempDir::new("rerun");
        let r = |ns| run_rates("cell", &[("int/alu", 100, ns)]);
        let a = save(&r(1_000_000_000), &dir.0).unwrap();
        let b = save(&r(2_000_000_000), &dir.0).unwrap();
        let c = save(&r(3_000_000_000), &dir.0).unwrap();
        assert_eq!(a.file_name().unwrap(), "cell.json");
        assert_eq!(b.file_name().unwrap(), "cell.2.json");
        assert_eq!(c.file_name().unwrap(), "cell.3.json");
        let runs = load_all(&dir.0).unwrap();
        assert_eq!(runs.len(), 3);
        // the first result is still the first result
        let first: Run = serde_json::from_str(&std::fs::read_to_string(&a).unwrap()).unwrap();
        assert_eq!(first.rows[0].ns, 1_000_000_000);
    }

    #[test]
    fn the_newest_result_skips_every_host_run() {
        let dir = TempDir::new("newest");
        save(&run_rates("host", &[("int/alu", 1, 1)]), &dir.0).unwrap();
        save(&run_rates("host", &[("int/alu", 1, 1)]), &dir.0).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let want = save(&run_rates("emu", &[("int/alu", 1, 1)]), &dir.0).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        save(&run_rates("host", &[("int/alu", 1, 1)]), &dir.0).unwrap();
        assert_eq!(newest_result(&dir.0).unwrap(), want);
    }

    #[test]
    fn stats_take_the_middle_of_odd_and_even_samples() {
        let s = stat_of(vec![3.0, 1.0, 2.0]).unwrap();
        assert_eq!((s.median, s.min, s.max, s.n), (2.0, 1.0, 3.0, 3));
        let s = stat_of(vec![4.0, 1.0, 2.0, 3.0]).unwrap();
        assert_eq!(s.median, 2.5);
        assert!(stat_of(Vec::new()).is_none());
    }

    /// Three samples of one cell are one column, not three columns with the
    /// same heading.
    #[test]
    fn repeated_runs_of_a_cell_report_median_min_max_and_count() {
        // 100, 80 and 120 units/s
        let runs = [
            run_rates("cell", &[("int/alu", 100, 1_000_000_000)]),
            run_rates("cell", &[("int/alu", 80, 1_000_000_000)]),
            run_rates("cell", &[("int/alu", 120, 1_000_000_000)]),
        ];
        let md = markdown(&runs, None);
        assert_eq!(md.matches("| cell (n=3) |").count(), 1, "{md}");
        assert_eq!(md.matches("cell (n=3)").count(), 1, "{md}");
        let c = kernel_cells(&md, "int/alu");
        assert_eq!(c.len(), 3, "one cell column and no ratio column: {md}");
        assert_eq!(c[2], "100.0 [80.0-120.0]", "{md}");
        // the cells table says how many samples stand behind the row
        let cells = md.lines().find(|l| l.starts_with("| cell | 3 |")).expect(&md);
        assert!(cells.contains("| 3 |"), "{cells}");
        assert!(text_summary(&runs).starts_with("cell (n=3)"), "{}", text_summary(&runs));
    }

    #[test]
    fn ratios_use_the_median_of_each_cells_runs() {
        let runs = [
            run_rates("base", &[("int/alu", 100, 1_000_000_000)]),
            run_rates("base", &[("int/alu", 100, 1_000_000_000)]),
            run_rates("base", &[("int/alu", 40, 1_000_000_000)]),
            run_rates("new", &[("int/alu", 90, 1_000_000_000)]),
            run_rates("new", &[("int/alu", 95, 1_000_000_000)]),
            run_rates("new", &[("int/alu", 10, 1_000_000_000)]),
        ];
        let c = kernel_cells(&markdown(&runs, Some("base")), "int/alu");
        // medians 100 and 90
        assert_eq!(c[4], "0.90x");
    }

    /// A run of one kernel at `rate` units/s, with the properties a report
    /// compares: host CPU, features, whether it was a quick run.
    fn run_with(cell: &str, rate: u64, host: &str, features: &[&str], quick: bool) -> Run {
        let mut r = run_rates(cell, &[("int/alu", rate, 1_000_000_000)]);
        r.host.cpu_model = host.into();
        r.features = features.iter().map(|s| s.to_string()).collect();
        if quick { r.settings.time_pct = 30; r.settings.repeats = 1; }
        r
    }

    /// Everything in a report that says "do not compare these".
    fn is_blanked(md: &str) -> bool { kernel_cells(md, "int/alu").iter().any(|c| c == "n/c") }

    #[test]
    fn a_quick_run_beside_a_full_run_warns_and_blanks_the_ratio() {
        let runs = [
            run_with("full", 100, "cpu", &[], false),
            run_with("quick", 80, "cpu", &[], true),
        ];
        let md = markdown(&runs, Some("full"));
        assert!(md.contains("**Warning:** These runs differ in run length"), "{md}");
        assert!(is_blanked(&md), "{md}");
        assert!(!md.contains("0.80x"), "{md}");
        assert!(text_summary(&runs).contains("run length"));
    }

    #[test]
    fn a_run_labelled_deterministic_without_ci_clock_is_called_out() {
        let runs = [
            run_with("a", 100, "cpu", &["tlbvmap"], false),
            run_with("b+deterministic", 100, "cpu", &["tlbvmap"], false),
        ];
        let md = markdown(&runs, Some("a"));
        assert!(md.contains("labelled +deterministic, but no ci_clock"), "{md}");
        assert!(is_blanked(&md), "{md}");
    }

    #[test]
    fn runs_from_two_hosts_warn_print_both_and_blank_the_ratio() {
        let runs = [
            run_with("a", 100, "AMD EPYC 7443", &[], false),
            run_with("b", 100, "AMD EPYC 7663", &[], false),
        ];
        let md = markdown(&runs, Some("a"));
        assert!(md.contains("differ in host CPU"), "{md}");
        assert!(md.contains("- AMD EPYC 7443 (") && md.contains("- AMD EPYC 7663 ("), "{md}");
        assert!(md.contains("): a\n") && md.contains("): b\n"), "{md}");
        assert!(is_blanked(&md), "{md}");
        assert!(text_summary(&runs).contains("host: AMD EPYC 7663"));
    }

    #[test]
    fn one_host_is_printed_once_and_is_quiet() {
        let runs = [run_with("a", 100, "cpu", &[], false), run_with("b", 90, "cpu", &[], false)];
        let md = markdown(&runs, Some("a"));
        assert!(!md.contains("Warning"), "{md}");
        assert_eq!(md.matches("Host: ").count(), 1, "{md}");
        assert!(!is_blanked(&md), "{md}");
    }

    #[test]
    fn different_features_warn_and_blank_unless_asked_to_compare_them() {
        let runs = [
            run_with("interp", 100, "cpu", &[], false),
            run_with("jit", 400, "cpu", &["jitv2"], false),
        ];
        let md = markdown(&runs, Some("interp"));
        assert!(md.contains("differ in emulator build features"), "{md}");
        assert!(is_blanked(&md), "{md}");
        let md = markdown_with(&runs, Some("interp"), Compare { across_features: true });
        assert!(!md.contains("Warning"), "{md}");
        assert_eq!(kernel_cells(&md, "int/alu")[4], "4.00x", "{md}");
    }

    /// The suite compiled for the host has no emulator, so it must not trip
    /// the clock and features checks that every emulated report would fail.
    #[test]
    fn the_native_host_run_does_not_count_as_a_different_build() {
        let runs = [
            run_with("a", 100, "cpu", &["tlbvmap"], false),
            run_with("host", 1000, "cpu", &[], false),
        ];
        let md = markdown(&runs, None);
        assert!(!md.contains("Warning"), "{md}");
        let c = kernel_cells(&md, "int/alu");
        assert_eq!(c.last().unwrap(), "1/10", "{md}");
    }

    #[test]
    fn native_is_withheld_when_the_host_run_was_on_another_cpu() {
        let runs = [
            run_with("a", 100, "AMD EPYC 7443", &[], false),
            run_with("host", 1000, "AMD EPYC 7663", &[], false),
        ];
        let md = markdown(&runs, None);
        assert_eq!(kernel_cells(&md, "int/alu").last().unwrap(), "n/c", "{md}");
        assert!(md.contains("a native: differs from host in host CPU"), "{md}");
    }

    #[test]
    fn rates_across_guest_images_are_withheld() {
        let mut after = run_with("after", 80, "cpu", &[], false);
        after.suite_id = "blake3:be5200000000".into();
        let runs = [run_with("before", 100, "cpu", &[], false), after];
        let md = markdown(&runs, Some("before"));
        assert!(is_blanked(&md), "{md}");
        assert!(md.contains("after vs base: differs from before in guest image"), "{md}");
    }

    /// A `ci_clock` run: every rate is a constant (the audit's 100.0 MIPS), the
    /// counts are the information.
    fn counted(cell: &str, guest: u64, host: Option<u64>) -> Run {
        let mut r = run_with(cell, 100, "cpu", &["lightning", "ci_clock"], false);
        r.rows[0].icount = guest;
        r.total_icount = guest;
        r.total_ns = guest * 10; // 10 ns per instruction: the instruction clock
        r.host_instructions = host;
        r
    }

    #[test]
    fn a_counted_run_prints_instruction_counts_not_speeds() {
        let runs = [counted("det+deterministic", 613_905_711, Some(266_397_000_000))];
        let t = text_summary(&runs);
        assert!(t.contains("613,905,711 guest instructions"), "{t}");
        assert!(t.contains("266,397,000,000 host instructions"), "{t}");
        assert!(!t.contains("MIPS") && !t.contains("DMIPS"), "{t}");
        let md = markdown(&runs, None);
        assert!(md.contains("## Deterministic runs"), "{md}");
        assert!(md.contains("| 613,905,711 | 266,397,000,000 |"), "{md}");
        assert!(!md.contains("MIPS") && !md.contains("## Cells"), "{md}");
    }

    #[test]
    fn counted_runs_are_listed_apart_from_timed_ones() {
        let runs = [
            run_with("timed", 100, "cpu", &["lightning"], false),
            counted("det+deterministic", 1_000, Some(5_000_000)),
        ];
        let md = markdown(&runs, None);
        let cells = md.split("## Deterministic runs").next().unwrap();
        assert!(cells.contains("| timed |") && !cells.contains("det+deterministic"), "{md}");
        // different clocks, but never set side by side, so there is nothing to warn about
        assert!(!md.contains("Warning") && !md.contains("n/c"), "{md}");
        let t = text_summary(&runs);
        assert!(t.lines().any(|l| l.starts_with("timed ") && l.contains("MIPS")), "{t}");
        assert!(t.lines().any(|l| l.starts_with("det+deterministic") && l.contains("guest instructions")), "{t}");
    }

    #[test]
    fn counted_runs_compare_their_instruction_counts_against_the_base() {
        let runs = [
            counted("a+deterministic", 1_000, Some(10_000)),
            counted("b+deterministic", 1_000, Some(10_750)),
        ];
        let md = markdown(&runs, None);
        assert!(md.contains("b+deterministic against a+deterministic: guest instructions 1.00x, host instructions 1.07x"), "{md}");
        assert_eq!(kernel_cells(&md, "int/alu").last().unwrap(), "1.00x", "{md}");
    }

    #[test]
    fn a_counted_run_without_host_counts_says_so_with_a_dash() {
        let md = markdown(&[counted("a+deterministic", 1_000, None)], None);
        assert!(md.contains("| 1,000 | - |"), "{md}");
    }

    #[test]
    fn reference_refuses_deterministic_runs() {
        assert!(reference_refusal(&run_with("x", 1, "cpu", &["lightning"], false)).is_ok());
        let e = reference_refusal(&counted("x+deterministic", 1, None)).unwrap_err();
        assert!(e.contains("ci_clock"), "{e}");
        // the audit's case: labelled deterministic, built without ci_clock
        let e = reference_refusal(&run_with("x+deterministic", 1, "cpu", &["tlbvmap"], false)).unwrap_err();
        assert!(e.contains("+deterministic"), "{e}");
    }

    #[test]
    fn counts_print_with_thousands_separators() {
        assert_eq!(fmt_count(0.0), "0");
        assert_eq!(fmt_count(999.0), "999");
        assert_eq!(fmt_count(1000.0), "1,000");
        assert_eq!(fmt_count(613_905_711.0), "613,905,711");
    }

    /// Skipped where the host will not count (a container, a strict
    /// `perf_event_paranoid`): the report prints "-" for the figure then.
    #[test]
    fn the_host_instruction_counter_counts_work_done_after_it_starts() {
        let (sum, n) = with_host_instructions(|| (0..5_000_000u64).fold(0u64, |a, b| a ^ b.wrapping_mul(31)));
        std::hint::black_box(sum);
        match n {
            Some(n) => assert!(n > 5_000_000, "{n}"),
            None => eprintln!("no host instruction counter here; skipped"),
        }
    }

    #[test]
    fn a_missing_host_count_stays_out_of_the_saved_result() {
        let r = run_on("c", "");
        assert!(!serde_json::to_string(&r).unwrap().contains("host_instructions"));
        let mut r = r;
        r.host_instructions = Some(7);
        let back: Run = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back.host_instructions, Some(7));
        // results saved before the field existed still load
        let mut j: serde_json::Value = serde_json::to_value(&r).unwrap();
        j.as_object_mut().unwrap().remove("host_instructions");
        assert_eq!(serde_json::from_value::<Run>(j).unwrap().host_instructions, None);
    }

    #[test]
    fn counted_runs_are_judged_among_themselves_not_against_the_timed_ones() {
        let mut quick_counted = counted("det+deterministic", 1_000, Some(5_000));
        quick_counted.settings.time_pct = 30;
        let runs = [run_with("timed", 100, "cpu", &["lightning"], false), quick_counted];
        let md = markdown(&runs, None);
        assert!(!md.contains("Warning"), "a quick counted run beside a full timed one is not a mix: {md}");
        // but two counted runs of different lengths are
        let mut other = counted("det2+deterministic", 1_000, Some(5_000));
        other.settings.repeats = 1;
        let runs = [counted("det+deterministic", 1_000, None), other];
        assert!(markdown(&runs, None).contains("differ in run length"));
    }

    /// Fake emulators are scripts written and run by the tests. A script that
    /// is still open for writing when another thread forks fails to execute
    /// (ETXTBSY), so every test that makes or runs one holds this.
    #[cfg(unix)]
    static FAKE_IRIS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A shell script standing in for an emulator binary.
    #[cfg(unix)]
    fn fake_iris(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn features(list: &str) -> Vec<String> { list.split_whitespace().map(String::from).collect() }

    #[test]
    fn deterministic_needs_ci_clock_and_picks_arguments_by_build() {
        let e = deterministic_args(&features("tlbvmap")).unwrap_err();
        assert!(e.contains("ci_clock") && e.contains("[tlbvmap]"), "{e}");
        assert!(deterministic_args(&[]).is_err());
        assert_eq!(deterministic_args(&features("tlbvmap ci_clock")).unwrap(),
                   ["--jitv2-compile", "sync"]);
        assert_eq!(deterministic_args(&features("jitv2 ci_clock")).unwrap(),
                   ["--jitv2-compile", "sync"]);
        // lightning refuses --jitv2-compile sync, and has no JIT to need it
        assert!(deterministic_args(&features("lightning ci_clock")).unwrap().is_empty());
        // lightning with the JIT compiles on the pool, which cannot be made deterministic
        assert!(deterministic_args(&features("lightning jitv2 ci_clock")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_build_features_come_from_the_binarys_own_banner() {
        let _g = FAKE_IRIS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = TempDir::new("probe");
        let with = fake_iris(&dir.0, "with", "echo 'iris: build features: lightning tlbvmap ci_clock' >&2");
        assert_eq!(probe_features(&with).unwrap(), features("lightning tlbvmap ci_clock"));
        let none = fake_iris(&dir.0, "none", "echo 'iris: build features: (none)' >&2");
        assert!(probe_features(&none).unwrap().is_empty());
        let other = fake_iris(&dir.0, "other", "echo hello");
        assert!(probe_features(&other).unwrap_err().contains("no `build features:` line"));
    }

    /// The audit's sixth bug: `run --deterministic --iris` on a plain binary
    /// ran, and labelled a non-deterministic result "+deterministic".
    #[cfg(unix)]
    #[test]
    fn deterministic_on_a_binary_without_ci_clock_is_refused_before_it_runs() {
        let _g = FAKE_IRIS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = TempDir::new("detrefuse");
        let marker = dir.0.join("ran");
        // --help is the probe; any other invocation is the real run
        let body = format!(
            "echo 'iris: build features: tlbvmap' >&2\n\
             case \"$1\" in --help) exit 0;; esac\n\
             touch {}", marker.display());
        let iris = fake_iris(&dir.0, "iris", &body);
        let out = dir.0.join("out");
        let cli = Cli::try_parse_from(["iris-bench", "run", "--deterministic", "--iris",
            iris.to_str().unwrap(), "--out", out.to_str().unwrap()]).unwrap();
        let e = dispatch(cli.cmd).unwrap_err();
        assert!(e.contains("ci_clock"), "{e}");
        assert!(!marker.exists(), "the emulator must not be started for the run");
        assert!(!out.exists(), "nothing is saved");
    }
}
