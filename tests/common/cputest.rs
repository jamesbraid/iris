//! Running cpu-tests and judging its TAP stream: a cell passes when exactly
//! the tests listed for it in `cpu-tests/expected-failures.txt` fail and every
//! planned test reported.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

/// Seconds before a hung guest is killed. A healthy run takes about a second;
/// the margin is for a loaded machine.
const TIMEOUT_S: u64 = 900;

/// `IRIS_CPUTEST_ELF`, else what `make -C cpu-tests` builds.
pub fn guest_elf_path() -> PathBuf {
    std::env::var_os("IRIS_CPUTEST_ELF")
        .map(Into::into)
        .unwrap_or_else(|| repo("cpu-tests/build/cputest.elf"))
}

/// The tests a cell is expected to fail: its `[cpu-engine]` section of
/// expected-failures.txt. `#` starts a comment anywhere on a line.
pub fn expected_failures(cell: &str) -> BTreeSet<String> {
    let text = std::fs::read_to_string(repo("cpu-tests/expected-failures.txt"))
        .expect("read cpu-tests/expected-failures.txt");
    let mut want = BTreeSet::new();
    let mut section = None;
    for line in text.lines() {
        let line = line.split('#').next().unwrap().trim();
        if line.starts_with('[') {
            section = Some(line.trim_matches(|c| c == '[' || c == ']').to_string());
        } else if !line.is_empty() && section.as_deref() == Some(cell) {
            want.insert(line.to_string());
        }
    }
    want
}

/// What the guest's TAP stream says: the planned count, and each reported
/// test's name and whether it passed. A skipped test reports `ok`.
pub struct Tap {
    pub plan: Option<usize>,
    pub results: Vec<(String, bool)>,
    /// Parallel to `results`: the guest skipped it on this CPU (`# SKIP`).
    pub skipped: Vec<bool>,
}

/// `1..N` on a line of its own, and `ok N - name` / `not ok N - name` at the
/// start of a line, where the name runs to the first space (`# SKIP` follows).
pub fn parse_tap(log: &str) -> Tap {
    let mut tap = Tap { plan: None, results: Vec::new(), skipped: Vec::new() };
    // The console ends lines with CR LF. A bare CR ends one too.
    let log = log.replace("\r\n", "\n").replace('\r', "\n");
    for line in log.split('\n') {
        if tap.plan.is_none() {
            if let Some(n) = line.strip_prefix("1..") {
                if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
                    tap.plan = n.parse().ok();
                }
            }
        }
        let (ok, rest) = match line.strip_prefix("not ok ") {
            Some(r) => (false, r),
            None => match line.strip_prefix("ok ") {
                Some(r) => (true, r),
                None => continue,
            },
        };
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let Some(tail) = (digits > 0).then(|| &rest[digits..]).and_then(|r| r.strip_prefix(" - "))
        else {
            continue;
        };
        let name: String = tail.chars().take_while(|c| !c.is_whitespace()).collect();
        if !name.is_empty() {
            tap.skipped.push(tail[name.len()..].trim_start().starts_with("# SKIP"));
            tap.results.push((name, ok));
        }
    }
    tap
}

/// Every way the run disagrees with expected-failures.txt.
pub fn judge(cell: &str, tap: &Tap, want: &BTreeSet<String>) -> Vec<String> {
    let failed: BTreeSet<String> =
        tap.results.iter().filter(|(_, ok)| !ok).map(|(n, _)| n.clone()).collect();
    let Some(plan) = tap.plan else {
        return vec!["no plan line: the guest never reached its first test".into()];
    };
    let mut errs = Vec::new();
    for n in failed.difference(want) {
        errs.push(format!("{n} failed and is not an expected failure on {cell}"));
    }
    if tap.results.len() != plan {
        errs.push(format!(
            "{} of {} tests reported: the run stopped early",
            tap.results.len(),
            plan
        ));
    } else {
        for n in want.difference(&failed) {
            errs.push(format!(
                "{n} now passes on {cell}: remove it from [{cell}] in expected-failures.txt"
            ));
        }
    }
    errs
}

/// One finished run of the guest.
pub struct GuestRun {
    pub tap: Tap,
    pub crash: Option<String>,
    pub stdout: String,
    pub log: PathBuf,
}

impl GuestRun {
    /// The cell's verdict: empty when it passes.
    pub fn errors(&self, cell: &str) -> Vec<String> {
        let mut errs = judge(cell, &self.tap, &expected_failures(cell));
        if let Some(c) = &self.crash {
            errs.insert(0, c.clone());
        }
        errs.iter().map(|e| format!("check: {e}")).collect()
    }
}

/// Run the guest on `cpu` under this build's `iris`. An error is a run that
/// never finished (a timeout or a failure to start); a crash is reported by
/// the run and judged with it.
pub fn run_guest(cpu: &str, elf: &Path) -> Result<GuestRun, String> {
    let dir = run_dir(&format!("cpu-tests-{cpu}"));
    let config = machine_config("cpu-tests/run/bare.toml", &dir);
    let log = dir.join("serial.log");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_iris"));
    cmd.current_dir(&dir)
        .arg("--config").arg(&config)
        .arg("--load-elf").arg(elf)
        .arg("--test-device")
        .arg("--test-device-dump").arg(dir.join("dump.json"))
        .args(["--headless", "--noaudio", "--cpu", cpu]);
    let out = run(cmd, timeout(TIMEOUT_S), &log)?;
    Ok(GuestRun { tap: parse_tap(&out.stdout), crash: out.crash(), stdout: out.stdout, log })
}
