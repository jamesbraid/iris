//! `iris-accept` — boot a machine from a prepared disk and check it against a
//! scenario file (`test/accept/*.toml`, schema in TESTING.md).
//!
//! It starts `iris` itself with a generated config, watches the console log
//! for the boot phases, drives the guest with `iris-ci run` (as `iris-bench
//! irix` does), asks the monitor questions that have no guest-side answer,
//! and writes one JSON result per run. The scenario and the assets are the
//! only machine-specific input; nothing here knows what an O2 is.
//!
//! Exit status: 0 pass, 1 a run failed, 2 bad scenario or usage, 3 a run was
//! invalidated by the host, 77 every scenario was skipped for want of media.

use std::process::ExitCode;

#[cfg(not(unix))]
fn main() -> ExitCode {
    eprintln!("iris-accept: unix hosts only");
    ExitCode::from(2)
}

#[cfg(unix)]
fn main() -> ExitCode {
    unix::main()
}

#[cfg(unix)]
mod unix {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitCode, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use clap::{Args, Parser, Subcommand};

    use iris::accept::check::{check_step, count_forbidden, find_after};
    use iris::accept::config::{self, Wiring};
    use iris::accept::result::{
        summarize, ForbidResult, Host, PhaseResult, RunResult, Status, StepResult, SCHEMA,
    };
    use iris::accept::safety;
    use iris::accept::scenario::Scenario;

    #[derive(Parser)]
    #[command(name = "iris-accept", about = "Boot a machine from a disk and check it against a scenario.",
              version = iris::VERSION)]
    struct Cli {
        #[command(subcommand)]
        cmd: Cmd,
    }

    #[derive(Subcommand)]
    enum Cmd {
        /// Boot each scenario's machine and run its checks.
        Run(RunArgs),
        /// Parse and validate scenario files; boots nothing.
        Check { scenarios: Vec<PathBuf> },
        /// Print min/median/max of each phase over result files.
        Summary { results: Vec<PathBuf> },
    }

    #[derive(Args)]
    struct RunArgs {
        scenarios: Vec<PathBuf>,
        /// Directory holding the files each scenario's [assets] names.
        #[arg(long)]
        assets: Option<PathBuf>,
        /// Prepared disk image, overriding [assets] disk (one scenario).
        #[arg(long)]
        disk: Option<PathBuf>,
        /// PROM image, overriding [assets] prom (one scenario).
        #[arg(long)]
        prom: Option<PathBuf>,
        /// Where run directories are made. Required: there is no safe default,
        /// and it must not be in the system temporary directory.
        #[arg(long)]
        run_root: PathBuf,
        /// Where result files go. Defaults to RUN_ROOT/results.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Directory with the `iris` and `iris-ci` binaries. Defaults to this
        /// program's own directory.
        #[arg(long)]
        bin_dir: Option<PathBuf>,
        /// Repeat each scenario this many times, back to back.
        #[arg(long, default_value_t = 1)]
        runs: u32,
        /// Refuse to boot with less free space than this on the run root.
        #[arg(long, default_value_t = 150)]
        min_free_gb: u64,
        /// Refuse to boot when this many guests already run under the run root.
        #[arg(long, default_value_t = 2)]
        max_guests: usize,
        /// Multiply every timeout. Default: 1, or more when the host load
        /// exceeds its CPU count.
        #[arg(long)]
        timeout_scale: Option<f64>,
        /// Recorded in each result, e.g. the commit under test.
        #[arg(long, default_value = "")]
        label: String,
        /// Keep each run directory instead of removing it.
        #[arg(long)]
        keep: bool,
    }

    // ---- cleanup on every exit path ------------------------------------------

    /// The release profile aborts on panic, so Drop does not run there; the
    /// panic hook and the signal handler kill the guest through this.
    static CHILD_PID: AtomicI32 = AtomicI32::new(0);
    static INTERRUPTED: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_signal(_: libc::c_int) {
        INTERRUPTED.store(true, Ordering::Relaxed);
    }

    fn kill_child_now() {
        let pid = CHILD_PID.swap(0, Ordering::Relaxed);
        if pid > 0 {
            // SAFETY: SIGKILL to the one process this program started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }

    fn install_handlers() {
        // SAFETY: the handler only stores to an atomic.
        unsafe {
            libc::signal(libc::SIGINT, on_signal as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_signal as libc::sighandler_t);
        }
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |i| {
            kill_child_now();
            prev(i)
        }));
    }

    /// Owns the guest process and the run directory.
    struct RunDir {
        path: PathBuf,
        child: Option<Child>,
        keep: bool,
    }

    impl Drop for RunDir {
        fn drop(&mut self) {
            if let Some(c) = self.child.as_mut() {
                let _ = c.kill();
                let _ = c.wait();
            }
            CHILD_PID.store(0, Ordering::Relaxed);
            if !self.keep {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }

    // ---- driving the guest ---------------------------------------------------

    struct Bins {
        iris: PathBuf,
        ci: PathBuf,
    }

    struct Ci<'a> {
        bin: &'a Path,
        socket: PathBuf,
    }

    struct CiOut {
        stdout: String,
        stderr: String,
        code: Option<i32>,
    }

    impl Ci<'_> {
        fn call(&self, args: &[&str]) -> Result<CiOut, String> {
            let out = Command::new(self.bin)
                .arg("--socket").arg(&self.socket)
                .args(args)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("{}: {e}", self.bin.display()))?;
            Ok(CiOut {
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                code: out.status.code(),
            })
        }

        fn ok(&self, args: &[&str]) -> Result<(), String> {
            let o = self.call(args)?;
            if o.code == Some(0) { Ok(()) } else { Err(format!("iris-ci {args:?}: {}", o.stderr.trim())) }
        }

        /// A guest command: its output and exit status. iris-ci reports a
        /// nonzero guest status as `guest exit N` on stderr.
        fn run(&self, cmd: &str, shell: &str, timeout: u64) -> Result<(String, Option<i32>), String> {
            let t = timeout.to_string();
            let o = self.call(&["run", cmd, "--shell", shell, "--timeout", &t])?;
            let rc = match o.code {
                Some(0) => Some(0),
                _ => o.stderr.split("guest exit ").nth(1).and_then(|r| {
                    r.trim().split_whitespace().next()?.parse().ok()
                }),
            };
            if rc.is_none() {
                return Err(format!("iris-ci run: {}", o.stderr.trim()));
            }
            Ok((o.stdout, rc))
        }
    }

    /// One monitor command: connect, read to the first `> ` prompt, send,
    /// read to the next.
    fn monitor(port: u16, cmd: &str, timeout: Duration) -> Result<String, String> {
        let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("monitor: {e}"))?;
        s.set_read_timeout(Some(timeout)).ok();
        let to_prompt = |s: &mut TcpStream| -> Result<String, String> {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !(buf.ends_with(b"\n> ") || buf == b"> ") {
                let n = s.read(&mut chunk).map_err(|e| format!("monitor read: {e}"))?;
                if n == 0 {
                    return Err("monitor closed".into());
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Ok(String::from_utf8_lossy(&buf).replace('\r', ""))
        };
        to_prompt(&mut s)?;
        s.write_all(format!("{cmd}\n").as_bytes()).map_err(|e| e.to_string())?;
        let out = to_prompt(&mut s)?;
        Ok(out.trim_end_matches("> ").trim().to_string())
    }

    fn free_ports(n: usize) -> Vec<u16> {
        let ls: Vec<_> = (0..n)
            .filter_map(|_| std::net::TcpListener::bind("127.0.0.1:0").ok())
            .collect();
        ls.iter().filter_map(|l| l.local_addr().ok()).map(|a| a.port()).collect()
    }

    fn now_unix() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }

    fn secs(d: Duration) -> f64 {
        (d.as_secs_f64() * 1000.0).round() / 1000.0
    }

    fn read_lossy(p: &Path) -> String {
        String::from_utf8_lossy(&std::fs::read(p).unwrap_or_default()).into_owned()
    }

    // ---- one run -------------------------------------------------------------

    struct Plan<'a> {
        args: &'a RunArgs,
        bins: &'a Bins,
        out: &'a Path,
        scale: f64,
        host: Host,
    }

    fn scaled(secs: u64, scale: f64) -> Duration {
        Duration::from_secs_f64(secs as f64 * scale)
    }

    fn run_once(s: &Scenario, disk: &Path, prom: Option<&Path>, plan: &Plan) -> Result<RunResult, String> {
        let a = plan.args;
        // Checked before every boot, not once per invocation: earlier runs
        // and other users fill the same filesystem.
        safety::check_free_space(safety::free_bytes(&a.run_root)?, a.min_free_gb)?;
        safety::check_guest_slots(safety::live_guests(&a.run_root), a.max_guests)?;

        let started_unix = now_unix();
        let dir = safety::run_dir(&a.run_root, &s.name, started_unix, std::process::id());
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut rd = RunDir { path: dir.clone(), child: None, keep: a.keep };
        std::fs::write(dir.join(safety::PID_FILE), std::process::id().to_string()).map_err(|e| e.to_string())?;

        let ports = free_ports(7);
        if ports.len() != 7 {
            return Err("no free loopback ports".into());
        }
        let cfg = config::render(s, &Wiring {
            run_dir: &dir, prom, disk,
            monitor_port: ports[0], serial_ports: [ports[1], ports[2]],
            extra_ports: [ports[3], ports[4], ports[5], ports[6]],
        })?;
        let cfg_path = dir.join("iris.toml");
        std::fs::write(&cfg_path, cfg).map_err(|e| e.to_string())?;

        let stderr = std::fs::File::create(dir.join("stderr.log")).map_err(|e| e.to_string())?;
        let child = Command::new(&plan.bins.iris)
            .arg("--config").arg(&cfg_path)
            .current_dir(&dir)
            // iris keeps a CI run's disk overlay under $TMPDIR.
            .env("TMPDIR", &dir)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr)
            .spawn()
            .map_err(|e| format!("{}: {e}", plan.bins.iris.display()))?;
        CHILD_PID.store(child.id() as i32, Ordering::Relaxed);
        rd.child = Some(child);

        let ci = Ci { bin: &plan.bins.ci, socket: dir.join(config::CI_SOCKET) };
        let mut r = RunResult {
            schema: SCHEMA,
            scenario: s.name.clone(),
            status: Status::Pass,
            started_unix,
            label: a.label.clone(),
            build_features: vec![],
            iris_blake3: blake3::hash(&std::fs::read(&plan.bins.iris).unwrap_or_default()).to_hex().to_string(),
            host: plan.host.clone(),
            phases: vec![],
            steps: vec![],
            forbid: vec![],
            exited_by_itself: None,
            total_secs: 0.0,
            problems: vec![],
        };

        let ready = Instant::now() + Duration::from_secs(60);
        while ci.ok(&["ping"]).is_err() {
            if Instant::now() > ready || exited(&mut rd) {
                r.status = Status::Fail;
                r.problems.push("iris did not open its CI socket within 60 s".into());
                return Ok(finish_run(r, &mut rd, s, plan, None));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        ci.ok(&["start"])?;
        let t0 = Instant::now();
        r.build_features = iris::bench_report::parse_features(&read_lossy(&dir.join("stderr.log")));

        let log_path = dir.join(config::SERIAL_LOG);
        let mut booted = phases(s, &ci, &log_path, &dir, &mut rd, plan.scale, t0, &mut r);
        if booted {
            booted = login(s, &ci, plan.scale, &mut r);
        }
        if booted {
            for st in &s.steps {
                let t = Instant::now();
                let timeout = scaled(st.timeout, plan.scale);
                let got = match (&st.run, &st.monitor) {
                    (Some(c), _) => ci.run(c, &s.shell, timeout.as_secs().max(1)),
                    (_, Some(m)) => monitor(ports[0], m, timeout).map(|o| (o, None)),
                    _ => unreachable!("validated"),
                };
                let (output, problems) = match got {
                    Ok((out, rc)) => {
                        let bad = check_step(st, &out, rc);
                        (out, bad)
                    }
                    Err(e) => (String::new(), vec![e]),
                };
                r.steps.push(StepResult {
                    name: st.name.clone(),
                    passed: problems.is_empty(),
                    secs: secs(t.elapsed()),
                    output: output.chars().take(2000).collect(),
                    problems,
                });
            }
        }
        Ok(finish_run(r, &mut rd, s, plan, Some(t0)))
    }

    fn exited(rd: &mut RunDir) -> bool {
        rd.child.as_mut().map(|c| matches!(c.try_wait(), Ok(Some(_)))).unwrap_or(true)
    }

    /// Wait for each console phase in turn. False when one did not arrive.
    #[allow(clippy::too_many_arguments)]
    fn phases(s: &Scenario, ci: &Ci, log: &Path, dir: &Path, rd: &mut RunDir, scale: f64,
              t0: Instant, r: &mut RunResult) -> bool {
        let (mut cursor, mut prev) = (0usize, 0.0f64);
        for p in &s.phases {
            let deadline = Instant::now() + scaled(p.timeout, scale);
            let mut polls = 0u32;
            loop {
                let bytes = std::fs::read(log).unwrap_or_default();
                if let Some(end) = find_after(&bytes, cursor, &p.expect) {
                    cursor = end;
                    let at = secs(t0.elapsed());
                    r.phases.push(PhaseResult { name: p.name.clone(), at_secs: at, delta_secs: secs(Duration::from_secs_f64(at - prev)) });
                    prev = at;
                    break;
                }
                polls += 1;
                // The error log can be large; look at it every few seconds.
                if polls % 20 == 0 && safety::invalidating_line(&read_lossy(&dir.join("stderr.log"))).is_some() {
                    return false;
                }
                if INTERRUPTED.load(Ordering::Relaxed) {
                    r.problems.push("interrupted".into());
                    return false;
                }
                if exited(rd) {
                    r.problems.push(format!("iris exited before phase {:?}", p.name));
                    return false;
                }
                if Instant::now() > deadline {
                    r.problems.push(format!("phase {:?} ({:?}) not seen within {} s", p.name, p.expect,
                                            scaled(p.timeout, scale).as_secs()));
                    return false;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            if let Some(text) = &p.send {
                std::thread::sleep(Duration::from_secs(p.delay));
                if let Err(e) = ci.ok(&["serial-send", text]) {
                    r.problems.push(e);
                    return false;
                }
            }
        }
        true
    }

    /// Log in on the console and prove the shell answers. The prompt text
    /// differs between installs, so readiness is a command that comes back.
    fn login(s: &Scenario, ci: &Ci, scale: f64, r: &mut RunResult) -> bool {
        let _ = ci.ok(&["serial-send", "root"]);
        let _ = ci.call(&["serial-wait", "TERM =", "--timeout", "10"]);
        let _ = ci.ok(&["serial-send", ""]);
        for _ in 0..6 {
            std::thread::sleep(Duration::from_secs(2));
            if let Ok((out, Some(0))) = ci.run("echo iris-accept-ready", &s.shell, (20.0 * scale) as u64) {
                if out.contains("iris-accept-ready") {
                    return true;
                }
            }
        }
        r.problems.push("no shell answered after login".into());
        false
    }

    /// Power the guest off the way the scenario says, count forbidden text,
    /// decide the verdict, write the files. The guard stops iris on return.
    fn finish_run(mut r: RunResult, rd: &mut RunDir, s: &Scenario, plan: &Plan, t0: Option<Instant>) -> RunResult {
        let ci = Ci { bin: &plan.bins.ci, socket: rd.path.join(config::CI_SOCKET) };
        if let (Some(f), Some(_)) = (&s.finish, t0) {
            if r.problems.is_empty() {
                let _ = ci.ok(&["serial-send", &f.command]);
                let deadline = Instant::now() + scaled(f.exit_timeout, plan.scale);
                while !exited(rd) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(500));
                }
                let by_itself = exited(rd);
                r.exited_by_itself = Some(by_itself);
                if by_itself {
                    let at = secs(t0.map(|t| t.elapsed()).unwrap_or_default());
                    let prev = r.phases.last().map(|p| p.at_secs).unwrap_or(0.0);
                    r.phases.push(PhaseResult { name: "exit".into(), at_secs: at, delta_secs: secs(Duration::from_secs_f64((at - prev).max(0.0))) });
                }
                if !by_itself {
                    r.problems.push(format!("iris still running {} s after {:?}", f.exit_timeout, f.command));
                }
            }
        }
        if !exited(rd) {
            let _ = ci.ok(&["quit"]);
            let deadline = Instant::now() + Duration::from_secs(20);
            while !exited(rd) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        r.total_secs = t0.map(|t| secs(t.elapsed())).unwrap_or(0.0);

        let serial = read_lossy(&rd.path.join(config::SERIAL_LOG));
        let stderr = read_lossy(&rd.path.join("stderr.log"));
        let both = format!("{serial}\n{stderr}");
        r.forbid = count_forbidden(&both, &s.forbid)
            .into_iter()
            .map(|(pattern, count, max)| ForbidResult { pattern, count, max })
            .collect();
        for f in r.forbid.iter().filter(|f| f.count > f.max) {
            r.problems.push(format!("{:?} seen {} times, at most {} allowed", f.pattern, f.count, f.max));
        }
        for st in r.steps.iter().filter(|st| !st.passed) {
            r.problems.push(format!("step {}: {}", st.name, st.problems.join("; ")));
        }
        r.status = if let Some(l) = safety::invalidating_line(&both) {
            r.problems.insert(0, format!("run invalid: {l}"));
            Status::Invalid
        } else if r.problems.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        };

        // The run directory goes away with the guard; keep what explains a
        // bad run.
        let stem = format!("{}-{}", s.name, r.started_unix);
        if r.status != Status::Pass {
            let _ = std::fs::create_dir_all(plan.out);
            let _ = std::fs::write(plan.out.join(format!("{stem}.serial.log")), &serial);
            let tail = &stderr[stderr.len().saturating_sub(65536)..];
            let _ = std::fs::write(plan.out.join(format!("{stem}.stderr.tail")), tail);
        }
        r
    }

    // ---- driver --------------------------------------------------------------

    fn load(p: &Path) -> Result<Scenario, String> {
        let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
        Scenario::parse(&text).map_err(|e| format!("{}: {e}", p.display()))
    }

    fn write_result(out: &Path, r: &RunResult, n: u32) -> Result<PathBuf, String> {
        std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
        let p = out.join(format!("{}-{}-{}.json", r.scenario, r.started_unix, n));
        let text = serde_json::to_string_pretty(r).map_err(|e| e.to_string())?;
        std::fs::write(&p, text + "\n").map_err(|e| e.to_string())?;
        Ok(p)
    }

    fn run(a: RunArgs) -> Result<u8, String> {
        safety::check_run_root(&a.run_root, &std::env::temp_dir())?;
        std::fs::create_dir_all(&a.run_root).map_err(|e| format!("{}: {e}", a.run_root.display()))?;
        if a.scenarios.is_empty() || (a.scenarios.len() > 1 && (a.disk.is_some() || a.prom.is_some())) {
            return Err("give one or more scenarios; --disk and --prom need exactly one".into());
        }
        let bin_dir = match &a.bin_dir {
            Some(d) => d.clone(),
            None => std::env::current_exe().map_err(|e| e.to_string())?
                .parent().map(Path::to_path_buf).unwrap_or_default(),
        };
        let bins = Bins { iris: bin_dir.join("iris"), ci: bin_dir.join("iris-ci") };
        for b in [&bins.iris, &bins.ci] {
            if !b.exists() {
                return Err(format!("{} not found (build it, or pass --bin-dir)", b.display()));
            }
        }
        let scenarios: Vec<Scenario> = a.scenarios.iter().map(|p| load(p)).collect::<Result<_, _>>()?;
        let out = a.out.clone().unwrap_or_else(|| a.run_root.join("results"));
        install_handlers();

        let (mut code, mut ran) = (0u8, 0usize);
        for s in &scenarios {
            let find = |explicit: &Option<PathBuf>, name: &str| -> Option<PathBuf> {
                explicit.clone().or_else(|| a.assets.as_ref().map(|d| d.join(name)))
            };
            let disk = find(&a.disk, &s.assets.disk).filter(|p| p.exists());
            let prom = match &s.assets.prom {
                Some(n) => find(&a.prom, n).filter(|p| p.exists()).map(Some),
                None => Some(None),
            };
            let (Some(disk), Some(prom)) = (disk, prom) else {
                println!("SKIP {}: disk or PROM not found (--assets or --disk/--prom)", s.name);
                continue;
            };
            ran += 1;
            let mut results = Vec::new();
            for n in 1..=a.runs {
                // The load changes between runs; record what this one saw.
                let (load1, cpus) = safety::host_load();
                let plan = Plan {
                    args: &a, bins: &bins, out: &out,
                    scale: a.timeout_scale.unwrap_or_else(|| safety::auto_scale(load1, cpus)),
                    host: Host {
                        load1, cpus,
                        free_gb: safety::free_bytes(&a.run_root).map(|b| b >> 30).unwrap_or(0),
                        timeout_scale: a.timeout_scale.unwrap_or_else(|| safety::auto_scale(load1, cpus)),
                    },
                };
                let r = run_once(s, &disk, prom.as_deref(), &plan)?;
                let path = write_result(&out, &r, n)?;
                let login = r.phases.last().map(|p| p.at_secs).unwrap_or(0.0);
                println!("{:7} {} run {n}/{}: last phase at {login} s, total {} s, load {:.1} -> {}",
                         format!("{:?}", r.status).to_uppercase(), s.name, a.runs, r.total_secs, r.host.load1, path.display());
                for p in &r.problems {
                    println!("        {p}");
                }
                code = code.max(match r.status {
                    Status::Pass | Status::Skipped => 0,
                    Status::Fail => 1,
                    Status::Invalid => 3,
                });
                results.push(r);
                if INTERRUPTED.load(Ordering::Relaxed) {
                    return Ok(1);
                }
            }
            if results.len() > 1 {
                print_summary(&results);
            }
        }
        Ok(if ran == 0 { 77 } else { code })
    }

    fn print_summary(runs: &[RunResult]) {
        for (name, s) in summarize(runs) {
            println!("  {name:<20} min {:>8.1}  median {:>8.1}  max {:>8.1}", s.min, s.median, s.max);
        }
    }

    pub fn main() -> ExitCode {
        let cli = Cli::parse();
        let res = match cli.cmd {
            Cmd::Run(a) => run(a),
            Cmd::Check { scenarios } => scenarios.iter().try_for_each(|p| {
                load(p).map(|s| println!("ok {} ({} phases, {} steps)", s.name, s.phases.len(), s.steps.len()))
            }).map(|_| 0),
            Cmd::Summary { results } => results.iter()
                .map(|p| std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))
                    .and_then(|t| serde_json::from_str::<RunResult>(&t).map_err(|e| format!("{}: {e}", p.display()))))
                .collect::<Result<Vec<_>, _>>()
                .map(|rs| { print_summary(&rs); 0 }),
        };
        match res {
            Ok(c) => ExitCode::from(c),
            Err(e) => {
                eprintln!("iris-accept: {e}");
                ExitCode::from(2)
            }
        }
    }
}
