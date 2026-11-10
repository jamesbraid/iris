//! cpu-tests with one case per test the guest reports, on libtest-mimic.
//!
//! The alternative to `cpu_tests.rs` (one case per CPU), kept for comparison.
//! Each CPU's guest runs once, before the cases are listed, because the names
//! come from the guest; each case then only looks up its result. Cases are
//! `CPU::(run)`, which is the whole-run verdict and exactly what `cpu_tests.rs`
//! judges, and `CPU::group/test` for every test the guest reported, which fails
//! only for a test that fails and is not an expected failure, or one listed as
//! expected that now passes in a complete run. A test the guest skipped on this
//! CPU is reported as ignored, with the CPU it is for.
//!
//! Unlike `#[ignore]`, a case here can be ignored at run time, so a missing
//! guest binary or a debug build is reported as ignored with its reason
//! without `--ignored`.

mod common;

use std::sync::Arc;

use common::cputest::*;
use common::*;
use libtest_mimic::{Arguments, Completion, Failed, Trial};

const CPUS: &[&str] = &["r4400", "r5000"];

fn main() {
    let args = Arguments::from_args();
    let elf = guest_elf_path();
    let skip = if cfg!(debug_assertions) {
        Some("needs an optimised build: cargo test --release".to_string())
    } else if !elf.is_file() {
        Some(format!(
            "no cpu-tests guest at {}: build it with `make -C cpu-tests` in the toolchain \
             container or set IRIS_CPUTEST_ELF",
            elf.display()
        ))
    } else {
        None
    };

    let mut trials = Vec::new();
    if let Some(reason) = skip {
        for cpu in CPUS {
            let reason = reason.clone();
            trials.push(Trial::ignorable_test(format!("{cpu}::(run)"), move || {
                Ok(Completion::ignored_with(reason))
            }));
        }
    } else {
        // Both CPUs at once; each is a fraction of a second.
        let runs: Vec<_> = CPUS
            .iter()
            .map(|cpu| {
                let elf = elf.clone();
                std::thread::spawn(move || run_guest(cpu, &elf))
            })
            .collect();
        for (cpu, run) in CPUS.iter().zip(runs) {
            let run = Arc::new(run.join().expect("guest runner panicked"));
            trials.extend(cell_trials(cpu, run));
        }
    }
    libtest_mimic::run(&args, trials).exit();
}

fn cell_trials(cpu: &str, run: Arc<Result<GuestRun, String>>) -> Vec<Trial> {
    let cell = format!("{}-{}", cpu, engine());
    let mut trials = Vec::new();
    {
        let (run, cell) = (run.clone(), cell.clone());
        trials.push(Trial::test(format!("{cpu}::(run)"), move || match &*run {
            Err(e) => Err(format!("{cell}: {e}").into()),
            Ok(r) => match r.errors(&cell) {
                errs if errs.is_empty() => Ok(()),
                errs => Err(Failed::from(errs.join("\n"))),
            },
        }));
    }
    let Ok(r) = &*run else { return trials };
    let want = expected_failures(&cell);
    let complete = r.tap.plan == Some(r.tap.results.len());
    for (i, (name, ok)) in r.tap.results.iter().enumerate() {
        let (name, ok, expected) = (name.clone(), *ok, want.contains(name));
        let skip = r.tap.skipped.get(i).copied().unwrap_or(false);
        let cell = cell.clone();
        trials.push(Trial::ignorable_test(format!("{cpu}::{name}"), move || {
            if skip {
                return Ok(Completion::ignored_with(format!("not run on {cell}")));
            }
            match (ok, expected) {
                (false, false) => Err(format!("{name} failed and is not an expected failure on {cell}").into()),
                (true, true) if complete => Err(format!(
                    "{name} now passes on {cell}: remove it from [{cell}] in expected-failures.txt"
                )
                .into()),
                _ => Ok(Completion::Completed),
            }
        }));
    }
    trials
}
