# Plan: stop interpreter slowdowns and workload changes from landing unseen

Status: **proposed** (2026-11-08). Lessons, then the steps that prevent each
one, then the implementation with acceptance tests. *Measured* marks a
number from a recorded run; *to measure* marks one not yet taken. The
measurements will be summarised in a `clanker/perf/` note when the first
step lands.

Terms:
- A **build** is a feature set, for example `lightning,jitv2,gpujit,idle-pause,hostgl`.
- The **lightning interpreter** is a build with `lightning` and without
  `jitv2`, so every instruction is interpreted with the full cache model.
  This is what the Mac App Store build ships (`iris/lightning,appstore`).
- **Paired A/B** means running two emulator binaries alternately, in
  shuffled rounds, on the same guest image and the same machine.

## Lessons learned

### 1. This was the second time, and the rule written after the first was never enforced

- **The first time.** 1431f560 added a snapshot to a cold miss path in
  `CpuCache::fetch`. That pushed `fetch` out of line, and the lightning
  interpreter fell to 0.84x of main. `clanker/perf/hot-path-inlining-must-be-explicit.md`
  (2026-10-21) records it and sets the rule: hit paths are
  `#[inline(always)]`, miss paths are `#[cold] #[inline(never)]`, and after
  touching anything reachable from `fetch`, `read`, `write` or `step_int`,
  check the symbol table.
- **The second time.** Ten days later, aded5989 ("bridge: explain
  empty-slot bus errors instead of silencing them", 2026-10-31) added
  `eprintln!` formatting with a `fault_note()` String to four bus-error
  branches inside `read_data_impl`, `write_data_impl`, `fetch_instr_impl`
  and `write_data64_masked_impl`. Its message says "a good access costs
  nothing". That was reasoned, not measured.
- **The cost.** *Measured*, lightning interpreter, R4400, fixed guest image,
  n=3 interleaved:
  - suite 92.1 → 84.9 MIPS;
  - alu -14%, resize -17%, motion_est -13%, Dhrystone -7%, rotate90 -6%;
  - under IRIX on an Indy (n=4): Whetstone -8%, `iris_bench` -5%,
    Dhrystone -1%;
  - per guest instruction: host instructions flat (515 → 517), cycles up
    9-10% (136.6 → 150.0), branch misses up 60% (0.43 → 0.69), front-end
    stalls doubled;
  - the default build was flat across the commit (n=2 per point).
- **Disassembly** (*measured*, R4400 lightning monomorph): the slow binaries
  have 12 more direct calls to `_eprint`, `fmt::format` and `fmt::write`
  inside those four functions and `step_int` than the fast ones. That count
  separates every slow binary from every fast one: 6fc2c7e5 and the fix
  against aded5989, 1cbc2a4a and 404a86c7.

A rule in a note is not a check. The commit was a device change that
happened to touch `src/cpu/mips_exec.rs`. With fat LTO and one codegen unit,
a change anywhere in the crate can move the hot loop, so no rule of the
form "measure if you touched path X" is complete.

### 2. Our pass rule measured the wrong quantity, on a binary users don't run

- From 2026-11-06, a CPU or interpreter PR passed when deterministic-mode
  host instructions were equal or lower than its base. Real-clock dips did
  not block it.
- Our own PR check on aded5989 (*measured*: `run --quick --deterministic`,
  `lightning,ci_clock`, n=6 against n=5) read host instructions 0.75%
  **lower**, a pass. Cycles rose 5.4% and branch misses 21%.
- A `ci_clock` build adds a compare and branch on every guest instruction,
  so the loop we counted was not the loop users run.
- Speed is wall time on real work. Instruction counts, cycles and branch
  misses can explain a result. None of them is the result.

### 3. We measured the wrong builds

Users run:
- the release CLI and GUI (`lightning,jitv2,gpujit,idle-pause,hostgl`);
- the Mac App Store build, the lightning interpreter on aarch64 and
  x86_64 (`iris/lightning,appstore`);
- the maintainer's build, a lightning interpreter without idle-pause.

What we measured:
- our timed PR tables used jitv2 builds;
- `TESTING.md` builds `iris-bench` with default features;
- a separate bisect of the maintainer's report used default builds and
  concluded iris was fine.

Only the lightning interpreter regressed. Each CPU model is also its own
monomorph with its own `step_int`, and only the R4400 was measured.

### 4. Every base we compared against already contained the regression

The PR bases (45b7f745, 783920bc, 404a86c7) all post-date aded5989.
Head-against-base can't see a loss already in the base. Nothing compared
main with a known-good build or a release, and losses under any threshold
can pile up from PR to PR.

### 5. The workload changed under us, and the checks let it

- 6adc7927 rebuilt `bench/prebuilt/irisbench.elf` with GCC 14.2.0. The old
  image was built with GCC 16.2.1. The images' DWARF producer strings are
  "GNU C11 14.2.0" and "GNU C11 16.2.1 20260926".
- *Measured*: Dhrystone went from 313.1 to 428.2 guest instructions per
  iteration, and rotate90 from 1,049,282 to 1,440,274. That is 0.73x on
  both at equal speed. Ten kernels changed by more than 0.5%.
- An in-process `iris-bench run` uses each commit's own embedded image, so
  any comparison across 6adc7927 measured the compiler.
- The commit's message said to regenerate the image before landing. It
  landed with the GCC 14 image anyway.
- `check-prebuilt` hashes the source files, not the compiler or the work,
  so CI passed it.

### 6. The tools hid the regressions they were meant to show

All of these are *measured* on saved results, worst first.
- `report`'s "vs base" divides the best of all cells by the base, so it
  never reads below 1.00x. 6fc2c7e5 against aded5989 showed alu, resize
  and Dhrystone at 1.00x despite 8-17% losses.
- `report` silently mixes guest images, quick and full runs, deterministic
  and timed runs, hosts and builds, and prints only the first run's host.
- A rerun with the same label overwrites the last one, and `report` has
  no medians.
- Deterministic runs print fake speeds (always 100.0 MIPS and 133.1 DMIPS),
  and `reference` accepts them.
- `--deterministic --iris` on a binary without `ci_clock` is not
  deterministic: 21 of 46 kernels varied between runs.
- A failed `--iris` run drops the emulator's stderr.
- Lightning results are labelled "interp".
- Quick mode on a JIT build is the GUI's default. There Dhrystone read
  310-728 DMIPS against 689-722 for a full run, and 28 of 46 kernels were
  more than 2% off. Calibration gives up after four tries, leaving 44 of
  138 timed runs below their floor.

### 7. Real use was never measured

- The IRIX half (`iris-bench irix`) was documented as "written, not yet
  run". Every `requires` step was skipped, because a quoting bug in the
  probe passed literal backslashes to `tr`. `dd bs=1m` failed, and so did
  `ping -i 0`. One failed step aborted the run, and step output was
  discarded.
- Step times are rounded to 100 ms.
- `disk/read_cold` reads from IRIX's buffer cache.

### 8. A shared host is noisy, and single runs mislead

*Measured*:
- ±15-33% between rounds on a busy 224-core host at load ~200, even pinned.
  The cause is SMT siblings and shared cache, not time slicing.
- ±1-3% on a quiet host pinned to two cores.
- About 1 run in 10 is 25-35% slow.
- A single Dhrystone run read 113.9 DMIPS, against 140.5 ± 1.1 over two
  runs.

Our pinning script pins SMT siblings as well, which makes rates bimodal by
about 30%.

### 9. More than one cause, and a build flag that moves results

- *Measured* on the maintainer's build (`lightning,gpujit`, no idle-pause),
  R4400, paired: the GCC 14 image costs Dhrystone and rotate90 0.76x;
  aded5989 costs alu 0.89x and resize 0.90x; and a further alu step of
  0.90x sits at 73a1df35 ("bus: now it is 64 bits"). The last is the
  bisected location without an A/A arm, so it is *to measure* as a cause.
- `idle-pause` alone moves bare-metal results: on the same commit the
  build without it is 1.09x faster on the suite and 1.23x on alu. A build
  list without both variants measures neither the release build nor the
  maintainer's.
- The lightning interpreter counts a fused instruction pair as one, so its
  guest instruction count is up to 15% low (histogram -13%,
  sys/exception -15%). Guest MIPS is not comparable between engines.

### 10. We didn't know the reporter's setup

Nobody asked for the build features, the commit and the host up front.
iris has no `--version`; the features appear only in the startup
`build features:` line.

## Steps that prevent each lesson

| Lesson | Prevention | Enforced by |
|---|---|---|
| 1 | A CI check fails when formatting calls (`_eprint`, `fmt::format`, `fmt::write`) appear in the interpreter's hot functions (`step_int`, `read_data_impl`, `write_data_impl`, `fetch_instr_impl`, `write_data64_masked_impl`, `fetch`). It checks every CPU monomorph, in the release-features binary CI already builds. It also fails when a function that must be inline has become a symbol, which is the existing rule's symbol-table test, and when it finds none of the listed functions, so a rename cannot pass it vacuously. | CI, deterministic |
| 2 | Performance means paired wall time on the builds users run. Counters are reported only as explanation. A change passes when it is no slower than its base on every build it can affect. | `iris-bench ab`, PR template |
| 3 | One list of builds, taken from the workflows: release (JIT on, and dispatch off), App Store, lightning, default. One list of CPUs: R4400, R5000, R10000. `ab` runs the lists by default. | `iris-bench ab` |
| 4 | When a release is cut, `ab` runs with the previous release's binary as A and the candidate as B. | release checklist |
| 5 | Every MIPS guest binary is built by one pinned compiler (Fedora 45 GCC 16.2.1) in a container, reproducibly, and CI rebuilds the embedded image and fails on any byte difference. `check-prebuilt` checks the image's producer string. Owned by `clanker/plans/guest-toolchain-pin.md`. A generated per-kernel table of guest instructions per iteration, rewritten with the image like `bench/golden/golden.h`, makes any change in work a reviewable diff. | CI, deterministic |
| 6 | Fix `report`, deterministic mode, `--iris` failures, the engine labels and quick mode, so the tools show a regression instead of hiding it. | tests in `iris-bench` |
| 7 | `iris-bench irix` works and keeps timing to the millisecond. Dhrystone and Whetstone are rebuilt from public sources under `test/`. `ab --irix` pairs IRIX workloads. | `iris-bench irix`, `ab --irix` |
| 8 | `ab` pins to physical cores only, reruns outlier rounds, and reports INCONCLUSIVE when its own spread is too wide. It never reports "no change" when it can't tell. | `iris-bench ab` |
| 9 | The build list carries `idle-pause` on and off. Guest MIPS is compared within one engine only. |  `iris-bench ab`, `report` |
| 10 | `iris --version` prints the build features. Performance bug reports ask for it, the commit, the host, the CPU and the command. | issue template |

## Implementation

Each step lands as its own PR, with the test that proves it.

| # | Step | Acceptance test | Branch and status |
|---|---|---|---|
| 1 | **Fix aded5989.** Each bus-error report moves into its own `#[cold] #[inline(never)]` method, with the message text unchanged. | No slower than main on any build in the list, paired. *Measured* on the first version, lightning: suite 88.4 → 100.5 MIPS; IRIX boot 203 → 187 s. jitv2 with dispatch off read about 3% lower and must reach parity first. | `cpu-cold-bus-error-reports`, local, being re-measured |
| 2 | **The hot-function disassembly check** (lesson 1 row), as a script plus a `rust.yml` step on the release binary. Valid on lightning builds only: without `lightning`, `step_int` formats in its breakpoint paths by design. An anchor rule (`fill_l1i_line`, `cache_op`, `fetch_slow` must exist) stops a rename passing the must-not-exist rule vacuously. Lands with or after step 1, since main fails it today. Linux only for now; Windows needs the PDB. | *Measured*: fails all 10 slow lightning binaries (38-47 formatting calls each), passes all 9 fast ones, fails a forced out-of-line `fetch`, a renamed rule and a stripped binary. Identical with GNU and llvm objdump. | prototype on `bench-ci-checks` |
| 3 | **The pinned guest toolchain** (lesson 5 row), as designed in `clanker/plans/guest-toolchain-pin.md`: the container, reproducible flags, the CI rebuild-and-diff, the producer check, the two `cpu-tests` fixes and the test-count gate. Plus `bench/prebuilt/workload.txt`, guest instructions per iteration per kernel on the plain interpreter in deterministic mode, regenerated by `make prebuilt` and checked by regenerate-and-diff like `golden.h`. `make prebuilt` becomes two stages: the ELF in the container, the table on the host. | That plan's "Done when". *Measured*: the table changes on a GCC 16.2.1 rebuild (7 kernels, Dhrystone 428.15 → 313.10) and on a one-kernel edit (only int/alu64), not on a comment-only edit; a Fedora GCC 16.2.1 build matches the pre-6adc7927 image on all 46 kernels. | toolchain: being built on Linux (`guest-toolchain-pin-int`); table: prototype on `bench-ci-checks` |
| 4 | **Rebuild the image with GCC 16.2.1** in the pinned container. Debian and Ubuntu package no GCC 16.2 MIPS cross-compiler; Fedora 45 does, and its build is byte-identical in code to the image before 6adc7927. | `.text`, `.rodata` and `.data` match that image; Dhrystone runs 313 instructions per iteration. | part of step 3 |
| 5 | **`report` and deterministic-mode fixes** (lesson 6): "vs base" per cell, warnings and blanked ratios for unlike runs, no overwrite on rerun, a median over repeated runs, counts instead of speeds for deterministic runs, `reference` refusing them, `--deterministic --iris` refusing a binary without `ci_clock`, stderr shown on failure, lightning as its own engine. | Each bug's saved reproduction (the audit's e1-e7) gives the right output. | not started |
| 6 | **`iris-bench ab`** with the build and CPU lists, physical-core pinning, an outlier rerun, INCONCLUSIVE, guest instructions per iteration, refusal of mismatched builds and `ci_clock` binaries, a signature line naming both binaries, and `make -C bench ab`. **Verdict:** median ratio and best-round ratio both past the effect size (default 3%), 7 rounds by default; INCONCLUSIVE when A's own round-to-round spread exceeds the effect size. | Flags alu, resize and motion_est on 6fc2c7e5 against aded5989. Flags nothing on A/A. Gives INCONCLUSIVE on a deliberately loaded host. | `bench-ab`: core done; statistics, make target and hyperfine note in progress; the rest not started |
| 7 | **Review practice.** The PR template's performance section is an `ab` table across the lists, against the base. Host instruction counts alone are not a performance claim. A CI check that requires the `ab` signature when `src/cpu/`, `src/physical.rs`, `src/traits.rs` or `src/jitv2/` change is offered upstream; whether it runs is the maintainer's call. | The template exists. The check, if adopted, fails a PR touching those paths without the signature. | not started |
| 8 | **Release comparison** (lesson 4 row) in the release checklist. Results go in the release notes, with the platforms checked. | The next release records it. | not started |
| 9 | **`iris-bench irix`** fixes, with millisecond step timing, a cold `read_cold`, and IRIX results rendered in `report`. | Runs every default step on an Indy running IRIX 6.5.22, with no step time rounded to 100 ms. | `bench-irix-fixes`: probe, `dd`, failures, output, timeouts, ping done; timing, cold read and `report` not started |
| 10 | **Dhrystone 2.1 and Whetstone from public sources** under `test/`, following `test/ib/`, with IRIX MIPSpro build scripts and checked-in binaries. | Their instructions and times match the existing prebuilt IRIX binaries within noise, at the chosen flags. | `irix-dhry-whet`, in progress |
| 11 | **`ab --irix`**: paired boot to login, Dhrystone, Whetstone and `iris_bench`, one build per run, opt-in. | Detects aded5989's IRIX Whetstone loss (8%) on the lightning build. | not started |
| 12 | **`iris --version`**, the issue template, and corrected docs: `TESTING.md` points at the build list; README and `benchsuite.rs` stop claiming checks that don't exist; quick mode's claims match its measured accuracy. The pinning script pins physical cores only. | Doc claims match behaviour. | not started |
| 13 | **Quick mode**: calibrate until each timed run reaches its floor, or default the GUI to a full run on JIT builds. | Quick results within 2% of full on every kernel on the JIT build. | not started |

**Order.** 1 first, on its own. Then 2 and 3, which catch both lessons'
causes in CI with no timing. Then 5 and 6. Then 7 and 8. Then 4, as soon
as the container produces a matching image. Then 9-13.

## To measure

- The maintainer's setup end to end, lightning without idle-pause, before
  and after, R4400 and R10000, and its split into the image effect and the
  aded5989 effect. The two known effects multiplied don't give the reported
  rotate90 0.56 and alu 0.92, so something is still unexplained.
- Where the time goes in a suite run, a release build and an IRIX run, and
  the fastest configuration that still resolves a 5% step. Today a suite
  run takes about 41 s and a fat-LTO build about 4 minutes. A full IRIX run
  takes about 10 minutes.
- jitv2 with the JIT on, at aded5989.
- R5000 and R10000 interpreters; aarch64 macOS (does aded5989 slow it?);
  `ab` without `perf`.
