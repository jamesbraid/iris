//! cpu-tests (`cpu-tests/`) under the emulator this build produced: one case
//! per CPU, for the engine the build was compiled with.
//!
//!     cargo test --release --test cpu_tests -- --ignored
//!     cargo test --release --features jitv2 --test cpu_tests -- --ignored
//!
//! The guest binary needs the pinned MIPS cross compiler
//! (`cpu-tests/docs/toolchain.md`) and is not checked in. The cases read it
//! from `IRIS_CPUTEST_ELF`, else `cpu-tests/build/cputest.elf`. They are
//! `#[ignore]`d, so a plain `cargo test` reports them as ignored rather than
//! building nothing and calling it a pass; asked for explicitly with no guest
//! binary, they fail and say how to build one.
//!
//! A cell passes when exactly the tests listed for it in
//! `cpu-tests/expected-failures.txt` fail and every planned test reported;
//! the failure names each test that broke the rule. It does not run on the
//! R10000: every expectation in the suite is chosen by PRId, and it has none
//! for that CPU.

mod common;

use common::cputest::*;
use common::*;

fn cell(cpu: &str) {
    require_optimised_build();
    let cell = format!("{}-{}", cpu, engine());
    let elf = guest_elf_path();
    assert!(
        elf.is_file(),
        "no cpu-tests guest at {}: build it with `make -C cpu-tests` in the toolchain \
         container (cpu-tests/docs/toolchain.md) or point IRIS_CPUTEST_ELF at one",
        elf.display()
    );
    let run = run_guest(cpu, &elf).unwrap_or_else(|e| panic!("{cell}: {e}"));
    let errs = run.errors(&cell);
    let reported = run.tap.results.len();
    let failed = run.tap.results.iter().filter(|(_, ok)| !ok).count();
    if !errs.is_empty() {
        panic!(
            "{cell}: {reported} reported, {failed} failed (log: {})\n{}\n--- last 20 lines ---\n{}",
            run.log.display(),
            errs.join("\n"),
            tail(&run.stdout, 20)
        );
    }
    println!("check: {cell} ok ({reported} reported, {failed} expected failures)");
}

#[test]
#[ignore = "bare-metal suite: needs an optimised build and the cpu-tests guest; cargo test --release --test cpu_tests -- --ignored"]
fn r4400() {
    cell("r4400");
}

#[test]
#[ignore = "bare-metal suite: needs an optimised build and the cpu-tests guest; cargo test --release --test cpu_tests -- --ignored"]
fn r5000() {
    cell("r5000");
}

mod parser {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn tap_names_end_at_the_first_space_and_skips_pass() {
        let t = parse_tap("junk\r\n1..3\r\nok 1 - a/b\r\nnot ok 2 - c/d\r\nok 3 - e/f # SKIP\r\n");
        assert_eq!(t.plan, Some(3));
        assert_eq!(
            t.results,
            vec![("a/b".into(), true), ("c/d".into(), false), ("e/f".into(), true)]
        );
        assert_eq!(t.skipped, vec![false, false, true]);
    }

    #[test]
    fn only_listed_failures_in_a_complete_run_pass() {
        let want: BTreeSet<String> = ["c/d".to_string()].into();
        let t = parse_tap("1..2\nok 1 - a/b\nnot ok 2 - c/d\n");
        assert!(judge("x", &t, &want).is_empty());
        let t = parse_tap("1..2\nnot ok 1 - a/b\nok 2 - c/d\n");
        assert_eq!(judge("x", &t, &want).len(), 2, "a new failure and a new pass");
        let t = parse_tap("1..3\nok 1 - a/b\nnot ok 2 - c/d\n");
        assert_eq!(judge("x", &t, &want), vec!["2 of 3 tests reported: the run stopped early"]);
        assert_eq!(judge("x", &parse_tap("hello\n"), &want).len(), 1, "no plan");
    }
}
