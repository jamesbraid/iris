# Interpreter slowdown and benchmark-tool audit, November 2026: every measurement

Status: measurement record, written 2026-11-08. The numbers are dated; re-measure before reusing them. The plan that follows from them is `plans/benchmark-regression-prevention.md`; the compiler pin is `plans/guest-toolchain-pin.md`; the earlier rule this slowdown broke is `perf/hot-path-inlining-must-be-explicit.md`. One measurement set is still being taken (see "Still in progress").

This note keeps the data from one investigation: why a benchmark figure fell for the maintainer after the 2026-10-27 to 2026-11-07 landings, what in the emulator and in the benchmark image caused it, how the benchmark tools failed to show it, and how noisy each way of measuring it is. Raw runs are not in the repository; each section says which build, commit pair, guest image, run count and spread a number comes from.

## Summary

- **Three effects moved the maintainer's figure, plus one location that is not confirmed.**
  1. The embedded bench image was rebuilt with GCC 14.2.0 at 6adc7927 (it had been GCC 16.2.1). Dhrystone then executes 428 guest instructions per iteration instead of 313, and rotate90 1,440,274 instead of 1,049,282 (both 1.37x). Rates fall about 0.73-0.76x at unchanged emulator cost. This is a workload change, not an emulator change.
  2. aded5989 ("bridge: explain empty-slot bus errors instead of silencing them", 2026-10-31) slowed the **lightning interpreter** only: bare-metal suite 92.1 to 84.9 MIPS (-7.8%), alu -14%, resize -17%, motion_est -13%, with host instructions per guest instruction unchanged and cycles up 9-10%. Under IRIX on an Indy the loss is 1-8%. The default-feature build and jitv2 are flat across it.
  3. Layout sensitivity: the `idle-pause` feature, which does nothing on bare metal, moves the suite 9% and alu 23% on one commit. A benchmark has to run on exactly the build being reported.
  4. A further alu step of about 0.90x was located at 73a1df35 ("bus: now it is 64 bits"), by bisecting without a same-experiment A/A arm. It is a location, not a confirmed cause.
- **The fix** (each bus-error report in its own `#[cold] #[inline(never)]` method) wins back more than the loss on x86-64: lightning suite 88.4 to 100.5 MIPS on 1cbc2a4a (6fc2c7e5 itself: 92.1), 1.17x median suite MIPS over main 404a86c7 on the maintainer's build (8b42d292, the same change rebased), and about 16% faster on every kernel on an Apple M-series Mac (78cd6a79). The default and jitv2-JIT builds are flat (+2%, within spread); jitv2 with dispatch off reads about -1 to -3%, inside layout noise.
- **The aded5989 loss does not reproduce on Apple silicon.** An Apple M-series Mac, 11 rounds, parent to main: suite +0.1% [-0.2, +0.5], no kernel flagged. The fix is about 16% faster on every kernel there anyway.
- **Not reproduced, and ruled out:** the maintainer's Dhrystone 0.58x on the Indy under IRIX (flat in the default, jitv2-JIT and jitv2 dispatch-off builds); the R4400 part change in #403 (rev 5.0 at 150 MHz instead of rev 4.0); every landing from 2026-11-06 to 2026-11-08; the maintainer's R10000 Dhrystone drop (0.71x).
- **Why it was missed:** nobody measured aded5989; the pass rule counted host instructions, which fell 0.75% on aded5989 while cycles rose 5.4%; the builds measured were not the build that regressed; every base compared against already contained it; and `iris-bench report` printed every slowdown as 1.00x.
- **Noise sets what is detectable.** On a busy shared 2x EPYC 7663 host a single kernel from one run is worth little (A/A ratios 0.5 to 2.0); sequential 7-core pinned A/A scatter is about 10% per kernel; concurrent pairs on one L3 group, two cores a side, are 3.9%. A 3% suite-level step is only just resolvable.
- **A hot-function disassembly count separates every slow lightning binary from every fast one** (16 formatting calls against 4-5, in the R4400 monomorph).

## Hosts, builds, images and method

### Hosts

| Host | Description | Used for |
|---|---|---|
| Busy shared host | 2x EPYC 7663, 224 hardware threads, SMT2, 16 L3 groups (CCX) of 7 cores, load 70-250 during the sessions, shared with builder VMs and other work | Nearly every measurement below |
| Quiet host | 24-core EPYC 7443 (SMT siblings are CPU N and N+24; L3 groups cores 0-5, 6-11, 12-17, 18-23), shared with some service VMs | Earlier quiet-host figure; the quiet A/B in progress |
| Apple M-series Mac | arm64 macOS | Cross-architecture checks, the pinned-toolchain build |

### Builds (Cargo feature sets, all with the release profile: fat LTO, one codegen unit)

| Name | Features | Notes |
|---|---|---|
| release CLI/GUI | `lightning,jitv2,gpujit,idle-pause,hostgl` | what `rust.yml` builds and releases ship; the indy-perf "jit" set is the same minus `hostgl` |
| Mac App Store | `iris/lightning,appstore` | the lightning interpreter on aarch64 and x86_64 (from a review of the workflows) |
| indy-perf "light" | `lightning,gpujit,idle-pause` | full cache model, no jitv2 |
| maintainer's cell | `lightning,gpujit` (reports `opcodefusion rex-jit gr4-jit gr2-jit lightning tlbvmap`) | no `idle-pause` |
| indy-perf "int" | the release set with `IRIS_JIT_DISPATCH=off` | jitv2 build, JIT not dispatched |
| default ("plain") | default features (`tlbvmap`, `rexdiag`) | what `cargo build --release` and `TESTING.md` give |
| `ci_clock` variants | plus `ci_clock` | deterministic counts only; a different binary |

### Guest images

| Image | Identity | Compiler |
|---|---|---|
| old bench image | `bench/prebuilt/irisbench.elf` at 092a6ee9, sha256 7f418a6c... | GNU C11 16.2.1 20260926 |
| GCC 14 bench image | the same file from 6adc7927 on, sha256 21dc4ead..., suite id blake3:be520bc16f2f88d8 | GNU C11 14.2.0 |
| pinned image | built by the pinned Fedora 45 container, sha256 0c92ebe4b0e81a76bd4a06a2960543c9cb12a025593b00d54326a4d552a00eac | GCC 16.2.1 |
| IRIX disk | Indy `indy_ip24`, R4400, IRIX 6.5.22 with MIPSpro, a copy holding the public Dhrystone and Whetstone programs and `iris_bench`, run under a copy-on-write overlay | n/a |

Machine for the IRIX runs: `indy_ip24`, R4400, `banks = [128,128,0,0]`, Newport attached (`ci = true`, no window), no audio, fresh NVRAM. With `headless = true` the PROM stops at the maintenance menu and never autoboots.

### Method

- **Bare-metal:** `iris-bench run --iris BIN --elf FIXED --config bare.toml -- --cpu r4400`, under `perf stat`, with one `iris-bench` driver and one fixed ELF for every commit. An in-process `iris-bench run` uses the commit's own embedded image, so a comparison across 6adc7927 compares compilers.
- **IRIX:** `iris-bench irix` with a local steps file (stock steps minus graphics and ping, plus Dhrystone 10M, Whetstone 100k, `iris_bench` x20). Times are host wall time per step with the round-trip floor subtracted. Boot is the time from start to `login:` on serial, plus `instructions:u` of the iris process.
- **Pinning:** the 7 physical cores of one L3 group, one emulator at a time per group (at most two at once). With SMT siblings in the CPU set, wall rates were bimodal by about 30%.
- **Statistics used below:** "paired" is the per-round B/A ratio, median (min-max) over rounds. "best" is the best sample of each arm over all rounds, then the ratio. "geomean" is the per-round geometric mean over all 46 kernels. "cyc" is the ratio of each arm's lowest `cycles:u` per guest instruction. Kernel rates are the harness's own units per second; DMIPS is the Dhrystone rate divided by 1757; suite MIPS is total guest instructions over total timed nanoseconds.

## Cause 1: the GCC 14 bench image (6adc7927)

6adc7927 (2026-10-31) rebuilt `bench/prebuilt/irisbench.elf`. Its message says "regenerate it with make -C bench prebuilt before landing"; it landed with the GCC 14.2.0 image anyway. `check-prebuilt` hashes the sources, not the compiler or the work, so CI passed it.

Guest instructions per iteration, deterministic and identical across all 7 runs of each arm (R4400 and R10000, the maintainer's cell build, n=7):

| kernel | old ELF (GCC 16.2.1) R4400 | new ELF (GCC 14.2.0) R4400 | ratio | old R10000 | new R10000 |
|---|---|---|---|---|---|
| dhrystone | 312.1 | 427.2 | 1.369 | 313.4 | 427.3 |
| rotate90 | 1,049,280 | 1,440,273 | 1.373 | 1,049,293 | 1,255,952 |
| alu | 27.0 | 27.0 | 1.000 | 27.0 | 27.0 |
| latency_l1 | 195.0 | 195.0 | 1.000 | 195.0 | 195.0 |
| resize | 3,098,158 | 3,098,166 | 1.000 | 3,098,214 | 3,098,161 |

Measured effect of the image alone (068d0e08 lightning, subprocess runs with the old and new ELF, n=7):

| new / old, paired | DMIPS | rotate90 | alu | latency_l1 | suite MIPS | cyc |
|---|---|---|---|---|---|---|
| R4400 | 0.76 (0.40-0.89) | 0.76 (0.74-0.89) | 0.95 (0.85-1.46) | 0.93 (0.52-1.09) | 0.98 (0.80-1.05) | 0.998 |
| R4400 A/A (new dup / new) | 1.06 (0.91-1.72) | 0.98 (0.82-1.09) | 1.00 (0.59-1.71) | 1.06 (0.90-2.42) | 1.04 (0.95-1.21) | 0.998 |
| R10000 | 1.26 (0.63-1.87) | 1.29 (0.63-1.96) | 1.37 (0.64-1.95) | 1.41 (0.70-2.60) | 1.32 (0.87-1.58) | 0.646 |
| R10000 A/A | 1.02 (0.88-1.76) | 0.98 (0.89-1.39) | 1.01 (0.84-1.88) | 0.97 (0.54-1.53) | 1.02 (0.91-1.40) | 0.996 |

- On the R4400 the image costs Dhrystone and rotate90 a quarter (0.76x), which 1.37x more guest instructions per iteration gives at unchanged emulator cost. alu, latency_l1 and resize do identical guest work; their ratios are inside the A/A spread.
- On the R10000 the new image is faster overall (suite 1.32x, host cycles per guest instruction 0.646x), because the new harness sets KSEG0 cacheable (Config.K0 = 3: `config 0x6c01a003` against `0x6c01a002`). That outweighs Dhrystone's 1.37x more instructions. The suite figure matches the maintainer's R10000 30.9 to 40.9 MIPS (1.32x); his R10000 Dhrystone drop (68.6 to 49.0) does not reproduce (see "Decomposition"). An earlier note argued that the K0 change plus the new Dhrystone mix fit his figures, without measuring an R10000.
- Ratios from the per-iteration counts: Dhrystone 313.1/428.2 = 0.73x and rotate90 1,049,282/1,440,274 = 0.73x. The Apple Mac figure 185 to 146 DMIPS (0.79x, flat MIPS) includes other effects.
- Another binary on both images (audit experiment, same build 6fc2c7e5-light, same host, two paired rounds): Dhrystone DMIPS fell 0.82x in both rounds while instructions per iteration rose 1.37x, so Dhrystone's guest MIPS rose about 12% on the GCC 14 image. Ten kernels changed their instructions per iteration by more than 0.5% (composite -3.2%, lz +1.1%, rle -1.1% among them). The suite total moved +3%, inside that host's noise. "Guest MIPS is comparable across images" is therefore false per kernel.
- DWARF producer strings confirm the compilers (0205d36d's image "GNU C11 16.2.1 20260926"; 6adc7927 and main "GNU C11 14.2.0").
- GCC 16.2.1 and GCC 14 builds of the current sources, compared as a per-kernel table of guest instructions per iteration (plain interpreter, R4400, full cache, deterministic): 7 kernels differ by more than 0.5%: dhrystone 428.15 to 313.10 (-26.9%), rotate90 -27.1%, lz -9.0%, rle +4.4%, composite +3.1%, whetstone -0.6%, huffman +0.6%. A Fedora GCC 16.2.1 build of the current sources is identical on all 46 kernels to the 0205d36d GCC 16.2.1 image, so the pinned rebuild restores the earlier figures.
- The GUI benchmark score runs in-process on the embedded image, so it moves whenever the image does. That is how the maintainer's Mac figures moved.
- The GUI tooltip says "A real 150 MHz Indy scores about 130", but suite DMIPS depends on the guest compiler (the same emulator reads 0.73x on the GCC 14 image), so a comparison with hardware means something only for a named compiler and flags.

## Cause 2: aded5989 in the lightning interpreter

### What the commit did

aded5989 adds a `fault_note()` String argument to the `eprintln!` on four bus-error branches: instruction fetch, uncached read, uncached write and `write64_masked`. Those branches sit inside the hot fetch, load and store functions, which fat LTO inlines into `step_int` (about 50% of samples; `fill_l2_line_for` 5.9%, `read_data_impl` about 6.5%). No new work runs on the hot path: the same instructions run slower. The commit message says "the hook runs on the failure path alone, so a good access costs nothing"; that was reasoning, not a measurement. The specific branches or alignment that moved were not identified; the counters are consistent with a layout and branch-predictor effect.

### Bare metal, lightning build, across the range

`light` build (`lightning,gpujit,idle-pause`), R4400, fixed old ELF, n=2-3, median and (min-max). Rounds: 10 points from 092a6ee9 to 1cbc2a4a, 10 inside ae9dacfd..2091b746, 7 inside 4d551f60..73b589ca. The first bad commit is aded5989; every later point stays slow.

| commit | n | Dhrystone DMIPS | alu MIPS | resize | rotate90 | motion_est | suite MIPS | host insn / guest insn | host cycles / guest insn |
|---|---|---|---|---|---|---|---|---|---|
| 092a6ee9 | 2 | 145.4 (145-146) | 96.2 (96-97) | 101.4 (101-102) | 118.4 (117-120) | 117.4 (117-118) | 94.5 (94-95) | 513.2 (513-513) | 133.7 (134-134) |
| 7c63ff2d | 2 | 135.0 (127-143) | 97.6 (96-99) | 101.5 (101-102) | 120.6 (118-123) | 121.7 (122-122) | 95.0 (95-95) | 511.4 (510-512) | 132.9 (133-133) |
| b71b09a4 | 2 | 140.8 (141-141) | 93.3 (91-96) | 97.0 (97-97) | 118.9 (119-119) | 117.8 (117-118) | 90.6 (90-91) | 516.6 (516-517) | 140.4 (138-143) |
| ae9dacfd | 2 | 139.2 (139-140) | 93.2 (92-94) | 95.5 (95-96) | 117.3 (117-118) | 117.1 (117-117) | 90.8 (89-93) | 511.9 (506-517) | 140.8 (137-144) |
| 2eb2d3ba | 2 | 142.0 (141-143) | 91.8 (92-92) | 97.1 (96-98) | 118.8 (118-119) | 116.8 (117-117) | 92.8 (92-93) | 514.8 (514-515) | 136.2 (136-137) |
| 2930a0e3 | 2 | 136.6 (136-137) | 89.3 (88-91) | 96.4 (96-97) | 118.6 (118-119) | 118.4 (118-119) | 92.3 (92-92) | 515.3 (515-516) | 137.2 (137-137) |
| 44be6c42 | 2 | 135.8 (135-137) | 90.6 (90-91) | 96.1 (96-96) | 119.6 (119-120) | 116.0 (116-116) | 91.9 (92-92) | 516.4 (516-517) | 137.8 (138-138) |
| c779d1f2 | 2 | 138.6 (137-140) | 88.1 (88-88) | 97.4 (96-99) | 119.0 (119-119) | 118.0 (118-118) | 92.0 (92-92) | 513.6 (511-516) | 137.3 (136-138) |
| fbaa265d | 2 | 131.7 (126-137) | 89.8 (89-91) | 96.5 (96-97) | 119.3 (119-120) | 118.5 (118-118) | 90.8 (91-91) | 516.8 (516-517) | 139.3 (139-139) |
| 4d551f60 | 2 | 133.8 (131-137) | 90.5 (89-92) | 96.9 (97-97) | 119.9 (119-120) | 118.7 (118-119) | 91.6 (92-92) | 515.8 (515-516) | 137.9 (138-138) |
| 0205d36d | 2 | 138.5 (138-139) | 88.7 (87-91) | 95.7 (96-96) | 113.9 (113-114) | 115.5 (115-116) | 91.8 (92-92) | 516.2 (516-517) | 137.6 (137-138) |
| 6adc7927 | 2 | 137.7 (136-139) | 90.7 (87-94) | 95.6 (95-96) | 114.3 (113-115) | 115.9 (115-117) | 91.6 (91-92) | 516.0 (516-516) | 137.6 (137-138) |
| 91b059cd | 2 | 139.0 (138-140) | 89.2 (88-91) | 96.3 (96-96) | 113.9 (113-114) | 115.9 (114-117) | 91.8 (91-92) | 516.1 (516-516) | 137.2 (137-138) |
| 6fc2c7e5 | 3 | 139.0 (138-139) | 91.0 (91-91) | 95.8 (96-96) | 114.9 (115-115) | 116.4 (116-117) | 92.1 (92-92) | 515.1 (515-516) | 136.9 (137-137) |
| aded5989 | 3 | 128.9 (129-130) | 78.2 (78-78) | 79.5 (79-80) | 108.3 (108-110) | 101.1 (101-103) | 84.9 (85-85) | 516.7 (516-517) | 149.8 (149-151) |
| 2a39002c | 2 | 126.6 (127-127) | 76.5 (75-78) | 77.2 (77-77) | 103.1 (100-106) | 101.5 (101-101) | 84.4 (84-85) | 516.9 (517-517) | 151.7 (151-152) |
| ad897736 | 2 | 127.4 (127-128) | 77.2 (77-78) | 79.6 (79-80) | 106.8 (106-107) | 100.6 (100-101) | 84.0 (84-84) | 517.8 (517-519) | 152.0 (151-153) |
| 73b589ca | 2 | 129.5 (129-130) | 74.9 (73-77) | 80.7 (80-82) | 117.1 (116-118) | 111.9 (111-112) | 87.3 (87-87) | 513.4 (511-516) | 148.4 (146-151) |
| b8fca3ad | 2 | 129.8 (129-130) | 74.9 (74-76) | 82.7 (82-84) | 115.8 (115-116) | 110.4 (110-111) | 84.9 (84-86) | 516.9 (516-518) | 150.7 (147-155) |
| 2091b746 | 2 | 128.3 (128-129) | 76.6 (75-78) | 81.0 (81-81) | 116.3 (115-118) | 109.4 (109-110) | 86.7 (86-87) | 516.1 (516-516) | 146.9 (146-148) |
| 4a4d42e0 | 2 | 126.5 (126-127) | 77.9 (78-78) | 80.6 (79-82) | 110.2 (110-111) | 105.3 (104-107) | 84.8 (85-85) | 514.3 (512-517) | 150.3 (150-150) |
| c3979d34 | 2 | 127.7 (127-128) | 75.3 (74-77) | 80.4 (80-81) | 115.5 (113-118) | 109.3 (107-112) | 86.2 (86-86) | 512.0 (508-516) | 147.4 (147-147) |
| 3cba53ad | 2 | 126.2 (125-127) | 74.8 (73-76) | 81.8 (79-85) | 111.0 (111-111) | 107.5 (107-108) | 84.9 (85-85) | 516.7 (516-517) | 149.2 (149-150) |
| e0e8d952 | 2 | 129.3 (129-130) | 75.1 (73-77) | 80.2 (78-82) | 111.9 (111-112) | 106.8 (106-108) | 85.2 (85-85) | 516.4 (516-516) | 148.7 (148-149) |
| 3132f685 | 2 | 128.8 (128-130) | 75.4 (74-77) | 79.5 (79-80) | 117.0 (117-117) | 110.6 (109-112) | 86.1 (86-87) | 516.3 (516-516) | 147.2 (146-149) |
| 1cbc2a4a | 3 | 132.7 (132-133) | 76.7 (77-86) | 81.9 (81-83) | 112.6 (112-113) | 111.0 (109-111) | 88.4 (88-90) | 510.1 (510-510) | 143.8 (143-144) |

The default build over the same range and fixed ELF is flat; host instructions per guest instruction stay at 664-678:

| commit | n | Dhrystone DMIPS | alu MIPS | resize | rotate90 | motion_est | suite MIPS | host insn / guest insn | host cycles / guest insn |
|---|---|---|---|---|---|---|---|---|---|
| 092a6ee9 | 2 | 98.8 (99-99) | 67.7 (67-68) | 66.9 (67-67) | 83.6 (83-84) | 74.8 (73-76) | 69.5 (69-70) | 675.9 (675-677) | 185.7 (186-186) |
| 7c63ff2d | 2 | 99.0 (98-100) | 68.7 (68-69) | 68.4 (68-69) | 86.7 (86-87) | 76.6 (74-79) | 71.7 (71-72) | 676.4 (676-677) | 178.4 (177-180) |
| b71b09a4 | 2 | 100.9 (100-102) | 66.1 (64-68) | 67.9 (66-70) | 85.7 (85-87) | 78.1 (75-81) | 70.8 (70-72) | 678.0 (678-678) | 181.7 (180-184) |
| ae9dacfd | 2 | 102.2 (102-102) | 68.6 (68-69) | 66.1 (63-69) | 85.7 (84-88) | 74.9 (75-75) | 71.6 (71-72) | 675.8 (675-676) | 179.9 (178-182) |
| 2091b746 | 2 | 98.9 (98-100) | 61.2 (57-65) | 63.3 (61-65) | 84.2 (84-85) | 70.1 (68-72) | 67.9 (67-69) | 669.5 (668-671) | 189.8 (186-193) |
| 4a4d42e0 | 2 | 101.4 (101-101) | 65.8 (65-67) | 66.7 (67-67) | 86.0 (86-86) | 73.9 (74-74) | 70.3 (70-70) | 667.4 (667-668) | 182.7 (182-183) |
| c3979d34 | 2 | 99.7 (99-101) | 67.0 (66-68) | 66.1 (66-66) | 84.6 (83-86) | 72.0 (71-73) | 69.4 (69-70) | 668.7 (667-671) | 186.3 (183-190) |
| 3cba53ad | 2 | 98.3 (95-102) | 66.0 (66-66) | 63.8 (62-66) | 79.3 (72-87) | 72.4 (71-74) | 69.8 (69-70) | 667.7 (667-668) | 184.6 (183-186) |
| e0e8d952 | 2 | 96.9 (95-99) | 64.9 (64-66) | 63.1 (61-65) | 83.5 (80-87) | 70.5 (69-72) | 67.8 (67-68) | 670.5 (670-671) | 191.6 (189-195) |
| 3132f685 | 2 | 98.4 (95-102) | 65.4 (65-66) | 64.2 (64-65) | 85.7 (85-86) | 72.6 (72-73) | 69.3 (69-70) | 668.3 (667-669) | 186.2 (183-189) |
| 1cbc2a4a | 3 | 97.3 (95-97) | 63.8 (64-66) | 64.2 (64-64) | 83.9 (80-85) | 71.3 (70-72) | 69.6 (69-70) | 663.7 (663-665) | 185.2 (185-186) |
| 8a73bc20 | 3 | 99.1 (99-99) | 66.7 (67-70) | 67.2 (67-67) | 86.9 (86-87) | 79.8 (78-80) | 71.1 (71-71) | 654.0 (653-654) | 180.9 (181-181) |

jitv2 with dispatch off showed no step in the first-round samples (n=1, too noisy to bisect on), and the IRIX boot bisect shows none either. jitv2 with the JIT on was never measured at aded5989, only main against the fix.

Parent, culprit, main and fix in all four builds (bare metal, n=3 except where noted):

light:

| commit | n | Dhrystone DMIPS | alu MIPS | resize | rotate90 | motion_est | suite MIPS | host insn / guest insn | host cycles / guest insn |
|---|---|---|---|---|---|---|---|---|---|
| 6fc2c7e5 | 3 | 139.0 (138-139) | 91.0 (91-91) | 95.8 (96-96) | 114.9 (115-115) | 116.4 (116-117) | 92.1 (92-92) | 515.1 (515-516) | 136.9 (137-137) |
| aded5989 | 3 | 128.9 (129-130) | 78.2 (78-78) | 79.5 (79-80) | 108.3 (108-110) | 101.1 (101-103) | 84.9 (85-85) | 516.7 (516-517) | 149.8 (149-151) |
| 1cbc2a4a | 3 | 132.7 (132-133) | 76.7 (77-86) | 81.9 (81-83) | 112.6 (112-113) | 111.0 (109-111) | 88.4 (88-90) | 510.1 (510-510) | 143.8 (143-144) |
| 8a73bc20 | 3 | 148.8 (147-150) | 91.8 (92-92) | 95.9 (95-99) | 133.8 (134-134) | 129.1 (129-129) | 100.5 (100-101) | 487.7 (488-488) | 125.2 (125-125) |

plain:

| commit | n | Dhrystone DMIPS | alu MIPS | resize | rotate90 | motion_est | suite MIPS | host insn / guest insn | host cycles / guest insn |
|---|---|---|---|---|---|---|---|---|---|
| 1cbc2a4a | 3 | 97.3 (95-97) | 63.8 (64-66) | 64.2 (64-64) | 83.9 (80-85) | 71.3 (70-72) | 69.6 (69-70) | 663.7 (663-665) | 185.2 (185-186) |
| 8a73bc20 | 3 | 99.1 (99-99) | 66.7 (67-70) | 67.2 (67-67) | 86.9 (86-87) | 79.8 (78-80) | 71.1 (71-71) | 654.0 (653-654) | 180.9 (181-181) |

int (jitv2 build, dispatch off):

| commit | n | Dhrystone DMIPS | alu MIPS | resize | rotate90 | motion_est | suite MIPS | host insn / guest insn | host cycles / guest insn |
|---|---|---|---|---|---|---|---|---|---|
| 1cbc2a4a | 3 | 108.1 (107-108) | 76.0 (76-77) | 79.1 (79-79) | 91.4 (91-91) | 91.7 (90-92) | 81.3 (81-82) | 617.9 (618-618) | 154.2 (154-155) |
| 8a73bc20 | 3 | 105.6 (103-106) | 78.3 (76-80) | 77.0 (77-78) | 87.9 (87-88) | 88.5 (88-89) | 78.8 (78-79) | 621.9 (622-623) | 159.9 (160-161) |

jit (jitv2, JIT on):

| commit | n | Dhrystone DMIPS | alu MIPS | resize | rotate90 | motion_est | suite MIPS | host insn / guest insn | host cycles / guest insn |
|---|---|---|---|---|---|---|---|---|---|
| 1cbc2a4a | 3 | 731.3 (727-737) | 451.9 (450-452) | 707.8 (704-708) | 874.7 (860-879) | 1051.7 (1051-1053) | 742.4 (740-750) | 80.5 (80-81) | 20.0 (20-20) |
| 8a73bc20 | 3 | 721.6 (721-724) | 453.2 (453-454) | 709.1 (709-709) | 870.1 (865-872) | 1057.4 (1057-1058) | 739.8 (731-753) | 80.9 (80-83) | 20.0 (20-20) |

`perf stat` on one full lightning bare-metal run each, per guest instruction. IPC is host instructions per cycle.

| build | host insn | cycles | IPC | branch misses | L1I misses | front-end stall cycles |
|---|---|---|---|---|---|---|
| 6fc2c7e5 | 516.2 | 136.6 | 3.78 | 0.429 | 0.00048 | 2.27 |
| aded5989 | 516.6 | 150.0 | 3.44 | 0.690 | 0.00074 | 4.72 |
| 1cbc2a4a (main) | 510.6 | 145.0 | 3.52 | 0.698 | 0.00047 | 4.76 |
| 8a73bc20 (fix) | 485.2 | 124.6 | 3.89 | 0.506 | 0.00044 | 2.45 |

Host instructions do not change (515 to 517) while cycles rise 136.6 to 150.0 (+9.8%), branch misses 0.43 to 0.69 (+60%) and front-end stall cycles 2.27 to 4.72.

### Under IRIX (Indy, IRIX 6.5.22)

Medians (min-max) over n; times in seconds. dhry is Dhrystone 10M, whet is Whetstone 100k, ib is `iris_bench` x20. `rev4oldpart` is main with the Indy R4400 set back to rev 4.0 / 66 MHz (a local build, not published). The 092a6ee9 light run is n=1 for the workload columns because two other runs hung in the guest. `jit` is the release set minus `hostgl`, `int` the same with `IRIS_JIT_DISPATCH=off`, `light` and `plain` as above.

| commit | mode | n | boot | host insn to login (G) | dhry | whet | ib | fs/create 500 | gzip 16 MB | tar /usr/bin |
|---|---|---|---|---|---|---|---|---|---|---|
| 092a6ee9 | int | 1 | 249.07 (249.07-249.07) | 2495.66 (2495.66-2495.66) | - | - | - | - | - | - |
| 1cbc2a4a | int | 5 | 238.96 (237.99-317.14) | 2469.30 (2452.01-3361.49) | 118.20 (117.60-160.41) | 55.65 (54.84-57.84) | 49.93 (48.93-51.14) | 23.72 (22.62-52.13) | 17.51 (14.41-41.93) | 26.22 (25.32-35.72) |
| 3132f685 | int | 1 | 246.21 (246.21-246.21) | 2452.27 (2452.27-2452.27) | 129.69 (129.69-129.69) | 57.34 (57.34-57.34) | 50.13 (50.13-50.13) | 25.22 (25.22-25.22) | 32.82 (32.82-32.82) | 28.92 (28.92-28.92) |
| rev4oldpart | int | 5 | 247.29 (239.55-251.38) | 2486.72 (2435.66-2529.36) | 118.18 (116.39-125.08) | 56.34 (54.54-71.35) | 51.13 (49.14-58.84) | 22.92 (22.71-23.12) | 15.51 (14.61-21.12) | 25.01 (15.51-25.32) |
| 092a6ee9 | jit | 3 | 104.53 (101.71-1500.01) | 1983.53 (1959.34-2004.76) | 18.01 (18.01-18.01) | 17.51 (17.51-17.51) | 10.21 (10.21-10.21) | 12.11 (12.11-12.11) | 1.60 (1.60-1.60) | 3.80 (3.80-3.80) |
| 1cbc2a4a | jit | 5 | 97.99 (97.02-100.94) | 1710.20 (1676.61-1723.09) | 18.41 (18.32-20.72) | 14.11 (12.11-23.12) | 12.41 (8.41-13.41) | 10.21 (9.81-13.01) | 1.50 (1.40-2.50) | 3.40 (3.40-4.60) |
| 3132f685 | jit | 3 | 101.29 (98.06-101.81) | 1728.15 (1717.79-1742.87) | 18.41 (18.31-18.52) | 13.41 (12.41-13.71) | 13.01 (12.71-13.51) | 12.01 (11.01-12.01) | 1.40 (1.30-1.40) | 3.50 (3.50-3.60) |
| rev4oldpart | jit | 5 | 99.11 (97.83-103.49) | 1708.48 (1669.47-1732.44) | 18.71 (18.41-19.31) | 15.41 (12.11-17.01) | 10.31 (8.91-13.11) | 10.91 (10.41-11.51) | 1.50 (1.40-2.60) | 3.90 (3.40-4.10) |
| 092a6ee9 | light | 3 | 196.56 (196.54-199.62) | 1978.48 (1953.53-1987.37) | 97.36 (97.36-97.36) | 45.43 (45.43-45.43) | 38.73 (38.73-38.73) | 19.21 (19.21-19.21) | 10.11 (10.11-10.11) | 17.61 (17.61-17.61) |
| 1cbc2a4a | light | 4 | 203.29 (198.66-217.63) | 1932.97 (1872.50-1939.95) | 106.92 (104.97-108.27) | 43.88 (43.43-44.23) | 42.93 (42.23-43.83) | 19.56 (18.71-20.91) | 10.41 (10.31-10.51) | 18.21 (15.01-18.91) |
| 3132f685 | light | 4 | 204.20 (202.56-204.83) | 1923.80 (1917.53-1939.24) | 106.42 (105.37-108.68) | 46.68 (45.94-50.13) | 43.48 (41.73-44.53) | 19.82 (19.61-20.22) | 10.36 (10.21-10.81) | 19.86 (18.91-20.61) |
| 6fc2c7e5 | light | 4 | 197.43 (195.70-200.94) | 1932.42 (1910.50-1976.78) | 105.92 (104.26-107.97) | 44.28 (44.02-44.43) | 40.58 (40.32-42.13) | 19.16 (18.41-20.01) | 10.16 (9.91-10.31) | 16.51 (13.41-18.51) |
| 8a73bc20 | light | 4 | 186.99 (184.48-189.38) | 1873.12 (1827.21-1941.00) | 101.47 (98.26-104.76) | 39.93 (39.62-40.02) | 38.27 (37.92-38.62) | 18.06 (17.51-18.91) | 9.41 (9.41-9.51) | 11.21 (9.81-16.21) |
| aded5989 | light | 4 | 201.05 (198.87-203.09) | 1885.96 (1854.35-1912.56) | 106.97 (106.47-109.28) | 47.68 (47.33-47.83) | 42.73 (42.53-43.63) | 19.21 (18.71-19.31) | 10.51 (10.51-10.71) | 17.81 (14.61-18.21) |
| 092a6ee9 | plain | 3 | 229.94 (224.99-301.59) | 2528.49 (2496.54-2531.37) | 125.18 (121.59-171.33) | 67.54 (67.45-68.85) | 56.54 (56.03-57.14) | 20.31 (19.92-39.93) | 15.21 (14.71-38.93) | 22.61 (19.71-23.22) |
| 1cbc2a4a | plain | 3 | 223.38 (220.71-225.61) | 2465.31 (2456.92-2466.12) | 126.19 (123.78-155.12) | 76.16 (66.75-90.17) | 66.05 (57.14-66.55) | 19.92 (19.82-21.01) | 27.52 (15.61-28.12) | 15.41 (13.91-23.12) |
| 3132f685 | plain | 3 | 225.53 (223.58-241.74) | 2478.54 (2411.79-2489.97) | 124.30 (123.79-126.68) | 64.94 (64.74-65.24) | 55.94 (55.64-56.83) | 20.52 (20.42-20.71) | 16.91 (16.01-18.31) | 23.92 (23.02-24.12) |

- aded5989 under IRIX versus its parent 6fc2c7e5 (light, n=4): Whetstone 44.3 to 47.7 s, `iris_bench` x20 40.6 to 42.7 s, Dhrystone 10M 105.9 to 107.0 s (1-8%).
- Guest-reported figures: JIT main Dhrystone 316 DMIPS (555556 Dhrystones/s), Whetstone 625-833 MIPS; lightning about 53-56 DMIPS; plain about 45 DMIPS. The maintainer's binaries round to 1 s and 0.1 us, so host step times carry the comparison.
- 092a6ee9 jitv2 hung twice (once at boot, once in `fs/create_files`): an old JIT bug already fixed on main.
- Noise: about 1 run in 10 is a 25-35% outlier in both wall time and `instructions:u`, probably threads spinning while they wait.

### The same step on the maintainer's own build

The maintainer's build is `lightning,gpujit` (no `idle-pause`), built the same way, R4400, both ELFs, run in-process (n=7 unless noted). Old ELF throughout, subprocess runs, 6fc2c7e5 against aded5989 (n=15 on one L3 group):

| aded / 6fc | DMIPS | rotate90 | alu | latency_l1 | resize | motion_est | suite MIPS | geomean | best-of geomean | cyc |
|---|---|---|---|---|---|---|---|---|---|---|
| paired | 0.99 (0.57-1.78) | 0.89 (0.55-1.43) | 0.89 (0.66-1.44) | 1.00 (0.53-1.30) | 0.90 (0.55-1.86) | 0.97 (0.56-1.56) | 0.97 (0.84-1.15) | 0.967 (0.783-1.153) | 0.979 | 1.027 |
| best / best | 1.010 | 0.981 | 0.863 | 0.964 | 0.938 | 0.946 | 0.973 | | | |
| A/A 6fc dup / 6fc | 1.03 / 1.017 | 1.01 / 1.013 | 1.01 / 0.966 | 1.02 / 0.993 | 1.03 / 1.003 | 0.99 / 0.972 | 1.00 / 1.030 | 0.993 | 1.002 | 1.000 |

The same pair in-process with the embedded GCC 14.2.0 image (both commits embed it), n=10: alu 0.86 paired (best 0.861), resize 0.89 (best 0.888), suite MIPS 0.98 (best 0.988), geomean 0.987, best-of geomean 0.971, cyc 1.002. Its A/A: alu 1.02 (best 0.981), geomean 1.031, best-of 1.018, cyc 0.969.

So aded5989 costs alu about 11-14%, resize about 6-11% and the suite about 3% on this build; Dhrystone, latency_l1 and motion_est show nothing outside their A/A. That is smaller than the `light` build's 7-17% per kernel, whose layout is different (see "idle-pause" below).

The rest of the range, old ELF, R4400, n=15 (092a6ee9 to 6fc2c7e5, and aded5989 to 068d0e08):

| step | DMIPS | rotate90 | alu | latency_l1 | suite MIPS | geomean | best-of geomean | cyc |
|---|---|---|---|---|---|---|---|---|
| 6fc / 092, paired | 0.96 (0.49-1.91) | 1.01 (0.74-1.91) | 0.87 (0.54-1.47) | 0.90 (0.51-1.23) | 0.97 (0.89-1.07) | 0.962 (0.833-1.099) | 0.958 | 1.031 |
| 6fc / 092, best/best | 0.937 | 0.990 | 0.841 | 0.903 | 0.971 | | | |
| A/A 092 dup / 092 | 1.00 / 1.029 | 1.00 / 0.995 | 1.01 / 0.991 | 0.98 / 1.001 | 1.01 / 1.041 | 1.047 | 0.993 | 0.990 |
| 068 / aded, paired | 0.99 (0.77-1.84) | 0.91 (0.51-1.82) | 1.00 (0.88-1.46) | 0.94 (0.53-1.87) | 0.98 (0.87-1.24) | 0.983 (0.863-1.202) | 0.963 | 1.040 |
| 068 / aded, best/best | 0.963 | 0.943 | 0.932 | 0.967 | 0.960 | | | |
| A/A aded dup / aded | 1.03 / 1.024 | 1.01 / 0.961 | 1.08 / 1.055 | 0.97 / 1.014 | 1.02 / 1.006 | 0.996 | 0.992 | 0.991 |

aded5989 to 068d0e08 shows about 3-4% at suite level (best-of geomean 0.963, cyc 1.040, against A/A 0.992 and 0.991); no single kernel clears its A/A by a clear margin.

### Does a deterministic host-instruction check catch it? No

Recorded practice was `iris-bench run --quick --deterministic` in-process, ship profile, `perf stat -e instructions:u`, n=3 alternating, host instructions compared. Reproduced on `lightning,ci_clock` builds of 6fc2c7e5 and aded5989 (deterministic mode cherry-picked onto both), R4400 full cache model, quick deterministic, alternating, each pinned to the 7 physical cores of one free L3 group; n=6 for 6fc2c7e5, n=5 for aded5989:

| | 6fc2c7e5 (fast parent) | aded5989 (slow) | change |
|---|---|---|---|
| host instructions | 266.397 G | 264.408 G | **-0.75%** |
| cycles | 76.36 G (75.96-79.02) | 80.52 G (80.03-82.61) | **+5.4%** |
| branch misses | 314.5 M | 381.4 M | **+21%** |
| guest instructions | 0.6139 G | 0.6139 G | identical, every kernel, every run |
| printed figures | 100.0 MIPS, 133.1 DMIPS | 100.0 MIPS, 133.1 DMIPS | identical by construction |

The pass rule "host instructions equal or lower" passes aded5989 and reads it as a 0.75% improvement. Instruction spread between identical runs here was 0.10-0.11%, not the 0.002% the PR bodies quoted. Two more ways the check differs from what users run: a `ci_clock` build adds a compare and branch on every retired guest instruction (`step_cycles!`), so the counted loop is not the shipped one, and with jitv2 deterministic mode compiles synchronously on the CPU thread while users run the asynchronous compile pool.

## Disassembly separates every slow binary from every fast one

Counting calls to `std::io::_eprint`, `alloc::fmt::format` and `core::fmt::write` in the methods of the R4400 lightning monomorph `MipsExecutor<MipsTlb, CpuCache<16384,16,1,1024,16384,16,1,1024,2048,1048576,...>>` in the kept binaries:

| binary | formatting calls | in |
|---|---|---|
| 6fc2c7e5-light (parent, fast) | 4 | `backtrace_from`, `idle_profile_report` only |
| aded5989-light (slow) | 16 | `read_data_impl` x4, `write_data_impl` x4, `fetch_instr_impl`, `write_data64_masked_impl`, `step_int`, `fault_note`, + the two above |
| 1cbc2a4a-light (main, slow) | 16 | same as aded5989 |
| 404a86c7-light (main now) | 16 | same |
| 8a73bc20-light (fix) | 5 | `fault_note`, `backtrace_from`, `idle_profile_report` |
| 8b42d292-light (fix, rebased) | 5 | same |

The count separates every slow binary from every fast one. A prototype check (`objdump -d -C`, a rule list of hot functions, and an anchor rule that fails when an expected symbol does not exist) was run over the kept binaries; "jit" is `lightning,jitv2,gpujit,idle-pause`, "rel" the exact release set with `hostgl`, "light" `lightning,gpujit,idle-pause`:

| commit | role | light | jit | rel |
|---|---|---|---|---|
| 6fc2c7e5 | fast parent | pass | pass | pass |
| aded5989 | slow | FAIL 39 calls | FAIL 38 | FAIL 38 |
| 3132f685 | slow (after aded5989) | FAIL 39 | FAIL 38 | - |
| 1cbc2a4a | slow | FAIL 47 | FAIL 44 | - |
| 404a86c7 | slow (main) | FAIL 47 | FAIL 44 | FAIL 44 |
| 8a73bc20 | fix, per-site cold | pass | pass | - |
| 8b42d292 | fix, rebased | pass | pass | - |
| 78cd6a79 | fix, one shared cold function | pass | - | pass |
| 78cd6a79 + `#[inline(never)]` on `CpuCache::fetch` | constructed | FAIL: 2 out-of-line `fetch` | - | - |

- 0 false results over the lightning, jit and rel binaries: all 10 slow binaries fail and all 9 fast ones pass; the unlabelled fix variants pass too. A forced out-of-line `CpuCache::fetch` fails the inline rule (the earlier 1431f560 failure mode; that commit is no longer reachable, so it was reproduced with a one-line patch). A renamed method fails with "no symbol matches", a stripped binary fails every rule. GNU objdump 2.47 and LLVM 19 `llvm-objdump` give identical results on five binaries.
- Default builds always fail it (80-176 calls): without `lightning`, `step_int` keeps the breakpoint and run-until paths, whose `format!` is inlined by design. The default build did not slow down at aded5989. So the check applies to lightning builds, and the release binary CI builds is one.
- It cannot see a layout regression with no formatting call, a non-cold out-of-line helper that formats inside (the hot function then calls `fault_note`, not `_eprint`), formatting inlined into a function not on the list, or speed as such.
- Cost: about 5 s of objdump plus Python per 110 MB binary (3.3 M lines); no extra build and no timing.
- Main 404a86c7 fails this check today; the step must land with or after the fix.
- Not run: macOS Mach-O (needs a comparison of its output with Linux once; the step is `continue-on-error` there) and Windows MSVC (symbols live in the PDB).

## The lightning interpreter undercounts guest instructions

Lightning's instruction counter is not a guest-instruction count: fused pairs count once. On 404a86c7-light, timed, against the deterministic plain-interpreter table, 12 kernels read more than 0.5% low, up to -14.7% (sys/exception), histogram -13%, crc32 -10%. The plan's "Dhrystone 428.2 plain against 427.1 lightning" is the small end. Guest MIPS is not comparable between engines. jitv2 counts differently again (Dhrystone 390.0 instructions per iteration in the older JIT runs against 427.1-428.3 in the interpreters, rotate90 1,428,743 against about 1,440,275; the JIT's counting changed in #439), so an exact match against a table of per-iteration counts needs a named reference build (the plain interpreter in deterministic mode) and a tolerance of 0.5% or less, with the JIT excluded.

## The fix

Branch `cpu-cold-bus-error-reports`: each bus-error report moves into its own `#[cold] #[inline(never)]` method, with `fault_note` and `post_refused_store` also `#[inline(never)]`, message text unchanged. Versions measured: 8a73bc20 on 1cbc2a4a (per-site methods); 8b42d292 (the same, rebased on 404a86c7); 78cd6a79 (one shared cold function, on 404a86c7). All three cut the formatting calls in the monomorph from 16 to 5 (`fault_note`, `backtrace_from`, `idle_profile_report`).

### 8a73bc20 versus 1cbc2a4a, x86-64, bare metal (see the four-build tables above)

- lightning: suite 88.4 to 100.5 MIPS (6fc2c7e5: 92.1), Dhrystone 148.8 DMIPS, 125 cycles and 488 host instructions per guest instruction, branch misses 0.51 per guest instruction.
- IRIX, light, n=4: boot 187 s against 203 s on main (1cbc2a4a), Whetstone 39.9 against 43.9 s, `iris_bench` 38.3 against 42.9 s, Dhrystone 101.5 against 106.9 s.
- default: +2% (suite 69.6 to 71.1). jitv2 JIT on: no change (742 to 740 MIPS, within spread).
- jitv2 with `IRIS_JIT_DISPATCH=off`: -3% (suite 81.3 to 78.8; cycles per guest instruction 154 to 160). Layout luck runs both ways.

### 78cd6a79 versus main 404a86c7, interleaved, cores pinned, medians

| build | main | this PR |
|---|---|---|
| lightning | 84.1 (n=4) | 95.5 (n=4); 6fc2c7e5, before aded5989: 92.1 |
| default features | 65.9 (n=5) | 68.7 (n=5) |
| jitv2, JIT on | 722 (n=5) | 733 (n=5) |
| jitv2, `IRIS_JIT_DISPATCH=off` | 79.8 (n=19) | 79.1 (n=15) |

The last row is about 1% down, inside layout noise: the same change split into per-site methods measured 78.1 (n=19), and `#[cold]` without `#[inline(never)]`, or without the attribute on `fault_note`, landed at 76-78 (n=5 each).

### 8b42d292 versus 404a86c7 on the maintainer's build (old ELF, n=7)

| fix / main | DMIPS | rotate90 | alu | latency_l1 | resize | motion_est | suite MIPS | geomean | best-of geomean | cyc |
|---|---|---|---|---|---|---|---|---|---|---|
| R4400 paired | 1.09 (0.59-1.32) | 1.21 (0.60-2.63) | 1.01 (0.83-1.32) | 1.03 (0.58-1.65) | 1.13 (1.01-2.00) | 1.23 (0.67-2.52) | 1.17 (1.00-1.19) | 1.068 (0.938-1.206) | 1.146 | 0.888 |
| R4400 A/A | 0.99 | 1.01 | 0.98 | 0.98 | 1.06 | 0.95 | 1.02 (0.85-1.25) | 1.043 | 1.009 | 1.009 |
| R10000 paired | 1.08 (0.97-1.15) | 1.11 (0.85-1.22) | 0.93 (0.52-1.42) | 1.05 (0.60-1.36) | 0.97 (0.84-1.77) | 0.89 (0.68-1.48) | 1.01 (0.97-1.07) | 1.018 (0.926-1.110) | 1.047 | 0.962 |
| R10000 A/A | 1.02 (0.65-1.64) | | | | | | 1.01 (0.84-1.11) | 0.991 | 0.993 | 0.948 |

R4400: never slower than main on suite MIPS in any round (minimum 1.00, median 1.17), 11% fewer host cycles per guest instruction (A/A 1.009), which is more than aded5989's own measured cost on that build (cyc 1.027). R10000 with the old image (uncached harness): nothing outside the A/A.

### Paired `ab` tool runs (11 rounds, pinned to 7 cores, `lightning,gpujit,idle-pause`, blake3:be520bc16f2f88d8)

Selected rows of a 46-kernel table, 6fc2c7e5 against aded5989 (busy host, load 100-200; whole-run speed of one binary swings 68-88 MIPS between rounds):

| kernel | 6fc2c7e5 med | aded5989 med | 6fc2c7e5 best | aded5989 best | med B/A | best B/A | paired B/A [95% CI] | noise | flag |
|---|---:|---:|---:|---:|---:|---:|---|---:|---|
| int/alu | 75.5 | 66.5 | 79.6 | 72.8 | +13.6% | +9.4% | +10.3% [+3.5%, +21.8%] | 12.3% | inconclusive |
| int/branch | 78.2 | 71.1 | 91.6 | 76.0 | +10.0% | +20.6% | +21.7% [+11.2%, +26.0%] | 6.3% | SLOWER |
| int/bitops | 98.0 | 90.4 | 110.6 | 102.8 | +8.4% | +7.6% | +10.3% [+2.6%, +12.8%] | 5.8% | SLOWER |
| fpu/matmul | 68.5 | 75.7 | 72.3 | 83.0 | -9.5% | -12.9% | -12.5% [-13.3%, +6.7%] | 9.2% | faster |
| img/resize | 77.1 | 64.3 | 82.9 | 77.1 | +19.9% | +7.5% | +17.9% [+3.6%, +22.7%] | 10.9% | inconclusive |
| vid/motion_est | 88.5 | 83.8 | 103.4 | 101.0 | +5.6% | +2.3% | +13.9% [+1.1%, +15.6%] | 6.2% | inconclusive |
| codec/crc32 | 87.8 | 80.0 | 102.1 | 90.5 | +9.8% | +12.9% | +11.5% [-5.5%, +23.0%] | 15.3% | inconclusive |
| (suite) | 73.1 | 68.4 | 78.9 | 75.2 | +6.9% | +4.9% | +7.0% [+1.3%, +11.3%] | 6.0% | inconclusive |

The same binary against itself (6fc2c7e5 twice):

| kernel | good med | good2 med | good best | good2 best | med B/A | best B/A | paired B/A [95% CI] | noise | flag |
|---|---:|---:|---:|---:|---:|---:|---|---:|---|
| int/alu | 76.1 | 73.6 | 86.1 | 93.8 | +3.3% | -8.3% | +2.5% [-7.3%, +12.7%] | 14.2% | inconclusive |
| int/branch | 83.5 | 72.3 | 90.6 | 93.0 | +15.6% | -2.6% | +13.8% [-6.6%, +23.2%] | 20.5% | inconclusive |
| int/bitops | 98.7 | 90.6 | 117.0 | 113.2 | +8.9% | +3.4% | +3.9% [-3.8%, +18.3%] | 11.1% | inconclusive |
| fpu/matmul | 65.3 | 67.3 | 74.4 | 77.7 | -2.9% | -4.3% | -4.3% [-9.8%, +3.7%] | 8.6% | inconclusive |
| img/resize | 77.1 | 75.2 | 94.7 | 89.0 | +2.5% | +6.3% | -2.3% [-4.9%, +8.5%] | 5.8% | inconclusive |
| vid/motion_est | 95.9 | 88.3 | 113.8 | 96.0 | +8.7% | +18.6% | +8.0% [+3.8%, +13.3%] | 5.8% | SLOWER |
| codec/crc32 | 89.8 | 88.6 | 112.7 | 99.7 | +1.3% | +13.0% | +0.6% [-13.2%, +10.6%] | 14.7% | inconclusive |
| (suite) | 73.8 | 71.9 | 84.0 | 82.5 | +2.6% | +1.8% | +2.1% [-4.8%, +4.8%] | 4.1% | inconclusive |

Cycles per guest instruction +4.4% with host instructions flat (517.0 against 517.1) in the A/B; +6.2% between two runs of one binary in the A/A, so on this host a 3-5% difference is below what 11 rounds resolve with that tool version. Flagged in the A/B: int/branch and int/bitops slower, fpu/matmul faster. alu, resize and motion_est read +6-20% medians with noise (6-12%) as large as the effect. In an earlier, quieter 7-round run of the same pair (same rule, recomputed offline from the saved rounds), alu, resize, motion_est and the suite row were all flagged slower. In the A/A, vid/motion_est was a false flag.

### Apple M-series Mac (arm64), pinned GCC 16.2.1 ELF, lightning CLI, `iris-bench ab --no-perf`

Relayed from the Mac's own tables (kept there, not reachable from the busy host):

- Parent 6fc2c7e5 to main 404a86c7, 11 rounds: suite +0.1% [-0.2, +0.5], no kernel flagged.
- Parent to aded5989: no suite change; a few kernels under 3-4% in both directions, a layout-shift signature. **So the aded5989 regression does not reproduce on Apple silicon.**
- Main to the fix 78cd6a79, 7 rounds: about 16% faster on every kernel: suite 139.9 to 166.7 median, alu -21.7% cost, Dhrystone -15.1%, rotate90 -16.2%, resize -17.3%, motion_est -15.2%, latency_l1 -15.0%; noise 0.4-2% per row. Parent to the fix gave the same within 1-2%.
- The container builds a byte-identical `irisbench.elf` on Apple silicon (0c92ebe4...0eac).
- Binary sha256 prefixes: parent 5abf158f..., aded5989 d3439e3d..., main aa8f88e4....
- The Mac's earlier 5-round claim that it saw the regression was retracted.

## An unconfirmed step at 73a1df35

On the maintainer's build (old ELF, R4400, n=15, then bisected): 092a6ee9 to 6fc2c7e5 holds a second alu step, 0.87 paired and 0.84 best against an A/A of 1.01 and 0.99, and latency_l1 0.90 against 0.98-1.00. Bisecting the 109 commits, four slots at once, 4 rounds each pooled as 16 paired rounds (these rounds carry no A/A arm of their own; the A/A reference is the 092a6ee9 duplicate arm above, alu paired 1.01, best 0.991).

Round 1, points 27, 55 and 82 commits in:

| step | alu paired | alu best | latency_l1 paired | latency_l1 best | suite MIPS paired | best-of geomean | cyc |
|---|---|---|---|---|---|---|---|
| 092a6ee9 → 56e0cc37 (27 commits) | 0.92 (0.76-1.49) | 0.880 | 0.88 (0.53-0.98) | 0.914 | 0.97 (0.84-1.09) | 0.960 | 1.019 |
| 56e0cc37 → 5a631b7c | 0.98 | 1.010 | 1.06 | 1.056 | 1.02 | 1.028 | 0.988 |
| 5a631b7c → bbe192ef | 1.01 | 1.005 | 0.97 | 1.055 | 0.96 | 1.002 | 1.030 |
| bbe192ef → 6fc2c7e5 | 1.02 | 1.003 | 0.94 | 0.923 | 1.02 | 0.983 | 1.032 |
| 092a6ee9 → 6fc2c7e5 | 0.96 (0.59-1.39) | 0.897 | 0.91 (0.62-1.15) | 0.940 | 0.96 (0.74-1.18) | 0.972 | 1.070 |

Round 2, inside 092a6ee9..56e0cc37 (73a1df35 is the first commit after 092a6ee9; the others are 7, 14 and 20 commits in):

| step | alu paired | alu best | latency_l1 paired | latency_l1 best | suite MIPS paired | best-of geomean | cyc |
|---|---|---|---|---|---|---|---|
| 092a6ee9 → 73a1df35 "bus: now it is 64 bits" | 0.90 (0.67-1.49) | 0.850 | 0.97 (0.67-1.19) | 1.017 | 1.00 (0.80-1.24) | 1.000 | 0.968 |
| 73a1df35 → 4c2396af | 0.97 | 0.899 | 0.97 | 0.947 | 0.97 | 0.970 | 1.024 |
| 4c2396af → e687174b | 1.00 | 1.028 | 1.02 | 1.024 | 1.02 | 1.000 | 1.034 |
| e687174b → f6344fc6 | 1.02 | 0.991 | 1.02 | 1.007 | 1.00 | 0.994 | 0.953 |
| f6344fc6 → 56e0cc37 | 1.02 | 1.039 | 1.01 | 1.040 | 1.01 | 1.028 | 0.988 |
| 092a6ee9 → 56e0cc37 | 0.89 (0.50-1.36) | 0.808 | 0.96 (0.57-1.47) | 1.034 | 1.02 (0.82-1.49) | 0.991 | 0.965 |

- The alu step is in 73a1df35 ("bus: now it is 64 bits"): alu 0.90 paired and 0.85 best, the only step in round 2 below 0.97 paired. The whole-range alu step reappeared in each round (0.92/0.880 and 0.89/0.808). It does not move the suite: suite MIPS 1.00, cycles per guest instruction 0.968.
- The latency_l1 step that round 1 put in the first 27 commits (0.88 paired) did not reappear in round 2 (0.96 paired, 1.034 best), so it is not established.
- Neither round bisected further or ran a same-experiment A/A. 73a1df35 is a measured location, not a confirmed cause. It has not been compared on any other build or host.

## Decomposition against the maintainer's report

The maintainer reported, on the R4400: Dhrystone 112 to 65 DMIPS (0.58x), rotate90 0.56x, alu 0.92x, latency_l1 0.87x; on the R10000: suite 30.9 to 40.9 MIPS (1.32x), Dhrystone 68.6 to 49.0 (0.71x). Reproduced end to end on the maintainer's build, `iris-bench run --cpu <cpu>` in-process with each binary's own embedded ELF, three arms interleaved, 092a6ee9 (2026-10-27) against 068d0e08 and 1cbc2a4a, n=7:

R4400, absolute values (median and range over 7 rounds):

| build | n | Dhrystone DMIPS | rotate90 (px/s) | alu (ops/s) | latency_l1 (rate) | suite MIPS | wall s |
|---|---|---|---|---|---|---|---|
| 092a6ee9 | 7 | 161.2 (138.9-204.6) | 16.95 M (15.14-20.60 M) | 64.8 M (58.3-84.6 M) | 32.3 M (28.8-41.8 M) | 79.7 (72.1-94.0) | 44.5 (41.6-49.9) |
| 068d0e08 | 7 | 102.8 (96.4-125.0) | 13.39 M (10.56-15.84 M) | 41.8 M (39.8-49.7 M) | 31.1 M (26.4-34.7 M) | 74.9 (71.2-83.6) | 45.8 (43.9-48.0) |
| 1cbc2a4a | 7 | 108.6 (96.8-113.5) | 13.37 M (10.93-15.55 M) | 46.0 M (32.7-49.4 M) | 27.7 M (24.0-32.2 M) | 75.6 (71.5-81.3) | 46.5 (43.4-49.3) |

R4400, ratios:

| ratio | DMIPS | rotate90 | alu | latency_l1 | suite MIPS | cyc |
|---|---|---|---|---|---|---|
| maintainer's report | 0.58 (112 → 65) | 0.56 | 0.92 | 0.87 | - | - |
| 068 / 092, paired | 0.65 (0.56-0.75) | 0.72 (0.60-0.97) | 0.69 (0.49-0.73) | 0.91 (0.80-1.01) | 0.92 (0.83-1.04) | 1.083 |
| 1cbc / 092, paired | 0.66 (0.49-0.82) | 0.73 (0.65-1.00) | 0.68 (0.51-0.81) | 0.81 (0.64-1.03) | 0.93 (0.84-1.02) | - |
| A/A 1cbc / 068 | 1.00 (0.77-1.18) | 0.98 (0.87-1.22) | 1.08 (0.72-1.23) | 0.93 (0.76-1.12) | 1.00 (0.94-1.09) | 1.028 |

R10000, absolute values:

| build | n | Dhrystone DMIPS | rotate90 (px/s) | alu (ops/s) | latency_l1 (rate) | suite MIPS |
|---|---|---|---|---|---|---|
| 092a6ee9 | 7 | 86.5 (75.1-92.1) | 10.62 M (9.90-11.50 M) | 31.5 M (26.3-39.8 M) | 21.4 M (12.7-23.5 M) | 53.5 (50.0-55.8) |
| 068d0e08 | 7 | 88.9 (81.1-105.7) | 12.14 M (10.72-13.73 M) | 40.1 M (35.5-43.9 M) | 24.9 M (23.7-28.9 M) | 70.7 (68.8-73.2) |
| 1cbc2a4a | 7 | 100.3 (88.3-114.3) | 13.61 M (12.04-15.11 M) | 42.9 M (38.5-48.5 M) | 27.8 M (23.5-32.8 M) | 75.6 (69.6-84.3) |

R10000, ratios:

| ratio | DMIPS | suite MIPS | cyc |
|---|---|---|---|
| maintainer's report | 0.71 (68.6 → 49.0) | 1.32 (30.9 → 40.9) | - |
| 068 / 092, paired | 1.08 (1.01-1.15) | 1.32 (1.27-1.41) | 0.733 |
| 1cbc / 092, paired | 1.18 (1.01-1.32) | 1.39 (1.28-1.67) | - |
| A/A 1cbc / 068 | 1.13 (0.94-1.31) | 1.09 (1.01-1.19) | 0.912 |

The `iris-bench` binaries of 068d0e08, 1cbc2a4a and 404a86c7 are byte-identical (md5 94911c81...), so 068 against 1cbc is an A/A pair, not two "after" candidates.

- R4400 partly reproduces: Dhrystone 0.65x (the maintainer's 0.58x is inside the per-round range 0.56-0.75), latency_l1 0.91x (0.87 inside 0.80-1.01). rotate90 drops less here (0.72, range 0.60-0.97, against 0.56) and alu much more (0.69, range 0.49-0.73, against 0.92); neither of his two values is inside the measured range. Absolute DMIPS differ (161 to 103 here, 112 to 65 for him); his host and setup are unknown.
- R10000: suite MIPS reproduces exactly (1.32x); his Dhrystone drop does not (1.08x paired, minimum 1.01; the identical-binary A/A read 1.13x, so what is measured is "no drop", not a rise).

Products of the measured R4400 per-round medians (computed, not run as one cell), against the end-to-end cell:

| kernel | (a) image | (b) aded | (a)x(b) | (c) 092→6fc | (c) aded→068 | (a)x(b)x(c) | (1) measured, paired |
|---|---|---|---|---|---|---|---|
| DMIPS | 0.76 | 0.99 | 0.75 | 0.96 | 0.99 | 0.72 | 0.65 (0.56-0.75) |
| rotate90 | 0.76 | 0.89 | 0.68 | 1.01 | 0.91 | 0.62 | 0.72 (0.60-0.97) |
| alu | 0.95 | 0.89 | 0.85 | 0.87 | 1.00 | 0.74 | 0.69 (0.49-0.73) |
| latency_l1 | 0.93 | 1.00 | 0.93 | 0.90 | 0.94 | 0.79 | 0.91 (0.80-1.01) |
| suite MIPS | 0.98 | 0.97 | 0.95 | 0.97 | 0.98 | 0.90 | 0.92 (0.83-1.04) |

- (a) image times (b) aded5989 does not cover alu (0.85 against a measured 0.69, range 0.49-0.73). Adding the 092a6ee9 to 6fc2c7e5 step brings the product to 0.74. For DMIPS, rotate90 and suite MIPS, both products land inside the end-to-end per-round range; latency_l1's full product (0.79) falls just below it (0.80-1.01). The ranges are wide, so "inside the range" is a weak test.
- The decomposition does establish: the image change accounts for the R4400 Dhrystone and rotate90 drop (deterministic 1.37x guest instructions per iteration), and the alu drop comes from two emulator steps of about 10-14% each (73a1df35, aded5989). The maintainer's rotate90 0.56 and alu 0.92 are still not explained: the two known effects multiplied (image 0.73 times aded5989 0.94 for rotate90 = 0.69; alu 1.0 times 0.86 = 0.86) do not give them.

What could still explain 0.58x on the maintainer's side, untested: a different host (Windows or macOS), the GUI build with the window open, `hostgl`, `[clock] fixed_mhz`, or a "before" that predates 092a6ee9. His exact build features, config and before-commit would settle it.

## `idle-pause` moves bare-metal results

The indy-perf "light" builds had `idle-pause`; the maintainer's build does not. Same commit 1cbc2a4a, old ELF, R4400, subprocess, n=7: the build without `idle-pause` against the build with it (features `opcodefusion idle-pause rex-jit gr4-jit gr2-jit lightning tlbvmap`):

| no idle-pause / idle-pause | DMIPS | rotate90 | alu | latency_l1 | suite MIPS | geomean | best-of geomean | cyc |
|---|---|---|---|---|---|---|---|---|
| paired | 1.03 (0.59-1.16) | 1.11 (0.57-1.90) | 1.23 (1.18-1.75) | 0.97 (0.62-0.99) | 1.11 (0.98-1.21) | 1.093 (0.959-1.168) | 1.088 | 0.931 |

A feature that does nothing on bare metal moves the suite 9% and alu 23%, in every round. Code layout in the fat-LTO step loop is that sensitive. A build list with only one of the two variants measures neither the release build nor the maintainer's.

Same bytes, different file: three copies of 068d0e08's `iris-bench` at different paths, plus one with a 27-character label instead of 2, R10000 in-process, n=7: copy2/copy1 geomean 0.919 (0.878-1.140), best-of 0.970, cyc 1.020; copy3/copy1 1.028, 0.994, 1.009; long label/copy1 1.019, 1.003, 1.006. This does not separate a per-file placement effect from the A/A noise; the 1cbc/068 identical-binary pair on the R10000 (1.09-1.10 in every statistic) is still unexplained.

## Ruled out

- **The #403 R4400 part change.** Main's default Indy part is rev 5.0 at 150 MHz (Count 75 MHz); before it was rev 4.0 at Count 33 MHz, and `hinv` now says "150 MHZ". A local build of main with the part set back to rev 4.0 / 66 MHz measures the same as main in int and jit mode, n=5 interleaved: Dhrystone 118.2 against 118.2 s, Whetstone 56.3 against 55.7 s, boot 247 against 239 s.
- **The 2026-11-06 to 2026-11-08 landings.** Range examined 092a6ee9 (2026-10-27) to 1cbc2a4a (2026-11-07), 278 plus 25 first-parent commits. CPU- and JIT-touching landings since 2026-10-25: #403 (2c598c1e..b7ff7303), #406 (ba117230..068d0e08), #438 (c9ae561e, d0c7bb9b, a0c6f969), #439 (3f80a629, 19e35da3), #442 (a4495a1c, c86905c9), #446 (506de44e), #447 (efbc9787..1763face), and the earlier aded5989. None of the 2026-11-06..08 landings moves Indy IRIX in any mode. Boot-only bisect, jitv2 with dispatch off, host instructions to `login:`:

| commit | boot | host instructions |
|---|---|---|
| 068d0e08-int-1 | 247 s | 2466 G |
| 1763face-int-1 | 245 s | 2479 G |
| 19e35da3-int-1 | 251 s | 2453 G |
| 19e35da3-int-2 | 244 s | 2389 G |
| 1cbc2a4a-int-1 | 249 s | 2485 G |
| 3132f685-int-1 | 258 s | 2506 G |
| 3132f685-int-2 | 251 s | 2508 G |
| 506de44e-int-1 | 242 s | 2452 G |
| 60a77007-int-1 | 248 s | 2461 G |
| 73478b24-int-1 | 248 s | 2457 G |
| 73478b24-int-2 | 252 s | 2487 G |
| a0c6f969-int-1 | 242 s | 2394 G |
| b7ff7303-int-1 | 243 s | 2367 G |
| c86905c9-int-1 | 245 s | 2498 G |

- **The maintainer's 112 to 65 DMIPS (0.58x) under IRIX.** Indy, R4400, from 092a6ee9 or 3132f685 (2026-11-06) to main 1cbc2a4a: default flat; jitv2 (JIT on) flat; jitv2 dispatch off flat (an early single run showed 0.81x Dhrystone and +37% host instructions to boot, but it was a host-load outlier; the boot-only bisect and 4 more runs reproduce none of it); lightning shows the aded5989 step plus 092a6ee9 to 6fc2c7e5 Dhrystone 97 to 106 s (that 092a6ee9 figure is n=1, so unconfirmed).
- **The Mac's bare-metal Dhrystone drop (185 to 146, 0.79x, flat MIPS)** is the bench-image change. An in-process `iris-bench run` uses each commit's embedded image, so a bisect across 6adc7927 sees a DMIPS drop at equal MIPS. Hold the ELF fixed (`--iris BIN --elf FIXED`) when bisecting across it.
- **The R10000.** Main no longer lets an Indy take an R10000, so the maintainer's R10000 Dhrystone 68.6 to 49.0 was not reproduced on an Indy; on the bare-metal R10000 the drop does not reproduce (above).
- **Default-feature bisects found nothing and were right about their build.** An earlier bisect of the report used default-feature binaries (`features: ["tlbvmap"]`), found no emulator step and concluded the emulator was fine. That holds for the default build only; the lightning build has the aded5989 step.

## Noise: how much each way of measuring can resolve

### Same-binary (A/A) spread, sequential, busy host

Each row interleaved, identical binaries, pinned to one L3 group (7 cores), the maintainer's build. This is the number every other row has to be read against.

| A/A pair | CPU | n | paired DMIPS | paired alu | paired suite MIPS | geomean | best-of geomean | cyc |
|---|---|---|---|---|---|---|---|---|
| 1cbc/068 in-process (identical bytes, different files) | R4400 | 7 | 1.00 (0.77-1.18) | 1.08 (0.72-1.23) | 1.00 (0.94-1.09) | 1.022 (0.928-1.034) | 1.011 | 1.028 |
| same, R10000 | R10000 | 7 | 1.13 (0.94-1.31) | 1.05 (0.94-1.24) | 1.09 (1.01-1.19) | 1.101 (1.021-1.204) | 1.096 | 0.912 |
| 068 new ELF dup | R4400 | 7 | 1.06 (0.91-1.72) | 1.00 (0.59-1.71) | 1.04 (0.95-1.21) | 1.059 (0.956-1.279) | 1.023 | 0.998 |
| 068 old ELF dup | R4400 | 7 | 0.78 (0.55-0.98) | 0.89 (0.71-1.01) | 0.91 (0.65-1.12) | 0.888 (0.683-1.147) | 0.973 | 1.015 |
| 404 old ELF dup | R4400 | 7 | 0.99 (0.61-1.06) | 0.98 (0.86-1.69) | 1.02 (0.85-1.25) | 1.043 (0.914-1.247) | 1.009 | 1.009 |
| 6fc old ELF dup | R4400 | 15 | 1.03 (0.73-1.23) | 1.01 (0.84-1.23) | 1.00 (0.91-1.11) | 0.993 (0.895-1.081) | 1.002 | 1.000 |
| 092 old ELF dup | R4400 | 15 | 1.00 (0.54-1.96) | 1.01 (0.69-1.43) | 1.01 (0.89-1.20) | 1.047 (0.889-1.199) | 0.993 | 0.990 |
| aded old ELF dup | R4400 | 15 | 1.03 (0.58-1.86) | 1.08 (0.68-1.59) | 1.02 (0.91-1.16) | 0.996 (0.883-1.210) | 0.992 | 0.991 |
| 6fc in-process full dup | R4400 | 10 | 0.99 (0.75-1.24) | 1.02 (0.90-1.66) | 1.02 (0.95-1.09) | 1.031 (0.929-1.106) | 1.018 | 0.969 |

- A single kernel's rate from one run is worth very little: per-round A/A ratios for one kernel range from about 0.5 to 2.0. A single kernel's A/A median over 15 rounds landed between 0.97 and 1.08; over 7 rounds, between 0.78 and 1.13. One A/A median was 22% off (068 old-ELF dup, DMIPS 0.78 over 7 rounds), and the identical-binary R10000 pair read 1.09-1.13 in every statistic over 7 rounds. A 7-round median is not safe against a 10% step on this host.
- The tightest statistics are suite-level: the best-of-n geomean over 46 kernels (R4400 A/A 0.973-1.023) and the lowest host cycles per guest instruction (R4400 A/A 0.969-1.028; the R10000 pair reached 0.912). `cycles:u` per guest instruction excludes time descheduled but not SMT or cache contention.
- A single Dhrystone run read 113.9 DMIPS against 140.5 +- 1.1 over two other runs. About 1 run in 10 is 25-35% slow. Busy-host spread between rounds is +-15-33% even pinned; a quiet host pinned to two cores showed +-1-3% (an earlier figure; the quiet-host rerun is in progress, below). Quick mode on a JIT build is worse (below).

### SMT siblings and the old pinning

- Pinning the 14 threads of a CCX (SMT siblings included) made wall rates bimodal by about 30%. Pinning only the 7 physical cores is what the later runs do.
- Over 177 runs (7-core pin, siblings left to others), the busy share of the pinned cores' SMT siblings during a run correlated 0.55 with that run's host cycles per guest instruction (normalised to its arm's median). Runs with siblings over 70% busy cost a median 1.069x (n=32); under 50%, 0.94-0.98x.
- Per round in the concurrent prototype (below), the share of time a side's sibling was busy (`/proc/stat` every 20 ms) tracked the ratio directly (A/A 0.87 when A's sibling was 68% busy and B's 23%; 1.29 when the reverse). `cpu_s` from rusage was within 2% of wall, so time-slicing on our own CPU was small.
- Putting both builds on the two SMT threads of one core cut A/A spread to 1.0% but measured something else: the effect shrank to about half (alu +5% against +13-15%) and, once the faster side finished, the slower one had the whole core. Rejected.

### Whole-CCX holds with sibling reservation

A host-local `benchslot.sh --ccx` holds a whole L3 group (7 cores plus their 7 SMT siblings reserved by lock) and pins one thread per core, choosing the quietest eligible group by sampled `/proc/stat` busyness; it excludes only jobs using the same lock files, so it is not isolation. During-run sibling busyness averaged 26.7% (range 8-57%) across the 16 whole-CCX runs even on a group chosen as quietest.

A/A, fixed binary `6fc2c7e5-light`, 404a86c7's `iris-bench` (`run --iris ... --elf old-092a6ee9.elf --config bare.toml -- --cpu r4400`) under `perf stat`; per round, per mode, the identical command twice, modes interleaved in random order; n=8 pairs per mode, 32 runs of about 50 s, chunks launched two at a time so up to two groups were held by the test itself:

| metric | old benchslot (14 threads, first free slot) | whole-CCX (7 threads, quietest) |
|---|---|---|
| suite MIPS | 1.021 (0.880-1.194), 8.1% | 1.018 (0.823-1.064), 8.2% |
| Dhrystone DMIPS | 0.994 (0.519-1.174), 21.7% | 1.064 (0.776-1.144), 11.2% |
| alu | 1.054 (0.605-1.424), 17.3% | 0.956 (0.714-1.131), 12.1% |
| host cycles per guest insn | 0.969 (0.847-1.138), 8.5% | 0.995 (0.915-1.293), 10.6% |

Spread over all 16 runs of a mode (max/min): suite MIPS 1.24 old, 1.31 whole-CCX; DMIPS 2.13 old, 1.46; alu 1.76 old, 1.63. Suite-level noise is the same (about 8% mean deviation, 20-25% worst case) in both modes. Single-kernel numbers look tighter under whole-CCX (DMIPS 22% to 11%, alu 17% to 12%), but with n=8 and overlapping chunks that is suggestive, not established; cycles per instruction did not improve. Sibling busyness did not explain the remaining spread in these 16 runs: correlation with cycles per guest instruction -0.39 (wrong sign), with suite MIPS 0.03, so the +0.55 above is not reproduced; pre-run and during-run sibling busyness correlate at 0.81. The mean clock of the used CPUs varied 2.4-3.0 GHz and correlated -0.29 with suite MIPS, so boost variation is a candidate; L3/memory contention from other groups and the test's own overlapped chunks are others. None of these was tested.

Root-only options that were written up but never measured: a cgroup v2 cpuset isolated partition per group (the only option that keeps other tenants off; needs cpuset delegation and capacity taken from the host's other users), `systemd-run -p AllowedCPUs=` (adds nothing over `taskset` alone), the performance governor (host governor is `schedutil` on `acpi-cpufreq`, boost on), and boost off (probably the most useful single change if clock matters; affects everyone and makes numbers incomparable with earlier ones).

### Concurrent A and B

Goal: an `ab` that resolves 3-5% on a busy shared host. All rows: busy host, suite image blake3:be520bc16f2f88d8, kept lightning builds of 6fc2c7e5 and aded5989 (`lightning,gpujit,idle-pause`). "10%x3" patches the image to `bench_time_pct=10`, `bench_repeats=3` (25 ms timed runs, best of 3); "10%x1" is 10% with one repeat; "full" is the image's own 100% with 2 repeats. Effects are the pooled median per-round B/A cost ratio (positive means aded5989 slower). The plan's quieter reference was alu +16%, resize +20%, motion_est +15% as cost, suite +8.5%. Spread is the robust scatter (1.4826 MAD / median) of the per-round B/A ratio.

| protocol | A/A pairs | A/A spread per kernel | A/A suite spread | A/B pairs | alu | resize | motion_est | suite | round wall | load |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| seq, 7-core pin, full image (today) | 10 | 10.3% | 5.5% | 10 | +8.9% | +18.3% | +15.9% | +8.6% | 93 s | 112-146 |
| seq, 7-core pin, 10%x1 | 12 | 16.6% | 7.9% | 11 | +8.7% | +21.5% | +0.2% | +7.2% | 42 s | 140-238 |
| seq, 7-core pin, 10%x3 | 22 | 7.6% | 3.1% | 21 | +19.0% | +23.4% | +11.7% | +9.4% | 64 s | 108-243 |
| seq, 1 least-busy core, 10%x3 | 15 | 13.9% | 20.0% | 25 | +8.0% | +20.6% | +12.4% | +6.9% | 69 s | 93-236 |
| (b) seq per kernel group, A/B per group, 1 core, 10%x3 | 12 | 8.2% | 6.1% | 7 | +22.5% | +11.0% | +8.3% | +9.1% | 72 s | 74-91 |
| (a) conc 1+1 core, 10%x1 | 42 | 9.2% | 7.0% | 41 | +12.9% | +10.5% | +13.1% | +8.3% | 22 s | 140-246 |
| (a) conc 1+1 core, full image | 32 | 6.4% | 6.1% | 20 | +16.8% | +9.6% | +5.5% | +5.6% | 51 s | 89-209 |
| (a) conc 1+1 core, 10%x3 | 85 | 4.3% | 2.2% | 73 | +12.7% | +9.8% | +10.6% | +6.0% | 33 s | 89-243 |
| **(a) conc 2+2 cores, 10%x3 (chosen)** | 39 | **3.9%** | **2.0%** | 46 | +14.8% | +14.9% | +10.3% | +7.9% | 34 s | 71-116 |
| conc 2+2, +3 busy threads on the same CCD | 12 | 6.1% | 5.8% | 11 | +15.2% | +13.0% | +11.7% | +9.1% | 36 s | 81-115 |
| conc, 3 pairs at once in one L3, 10%x1 | 57 | 11.1% | 8.1% | 36 | +14.8% | +7.9% | +7.1% | +7.6% | 24 s | 131-223 |
| conc, A and B in two different L3 groups, 10%x3 | 24 | 7.2% | 7.3% | 21 | +12.0% | +25.6% | +14.0% | +9.4% | 38 s | 91-202 |
| conc on the two SMT threads of one core, 10%x1 | 14 | 1.0% | 0.2% | 14 | +5.2% | +5.1% | +6.3% | +0.2% | 34 s | 131-212 |

A/A spread by load (pairs, spread):

| protocol | load <110 | 110-160 | >160 |
|---|---|---|---|
| conc 1+1, 10%x3 | 25, 4.5% | 38, 3.9% | 22, 4.4% |
| conc 2+2, 10%x3 | 38, 3.9% | - | - |
| seq 7-core, 10%x3 | - | 16, 7.5% | 6, 7.1% |
| seq 7-core, 10%x1 | - | 5, 18.0% | 7, 10.7% |
| conc 1+1, 10%x1 | - | 13, 8.4% | 29, 10.4% |

Headline (concurrent 2+2 cores, 10%x3 against today's sequential):

| | today's `ab` (sequential, 7-core pin, full image, median+best rule) | new default (concurrent 2+2 cores, 10%x3, interval rule) |
|---|---|---|
| A/A scatter of one round, median over kernels | 10.3% (6-17% across batches) | 3.9% (6.1% with 3 extra busy threads on the CCD) |
| A/A suite-row scatter | 5.5% | 2.0% |
| wall time per round | 93 s | 34 s |
| alu, resize, motion_est flagged | 44 of 47 rows inconclusive in 11 rounds (earlier run) | flagged in every 25-round draw; flagged in the real 14-round tool run |
| A/A false flags | 1 in 11 rounds (earlier run) | 0 in 19 of 20 25-round draws; 0 in the real 14-round tool run |
| 20 rounds | ~31 min | ~12-13 min |

Rounds needed (60 random draws per n, rule: 99% interval plus a 3% effect floor; pool of 51 A/A and 57 A/B pairs, overlapping subsets, so rates are optimistic):

| rounds | A/A false flags per run (of 46) | A/A runs with any flag | synthetic +4% detected (per kernel) | alu | resize | motion_est |
|---:|---:|---:|---:|---:|---:|---:|
| 8 | 0.63 | 48% | 30% | 58% | 58% | 95% |
| 11 | 0.35 | 25% | 45% | 87% | 70% | 100% |
| 15 | 0.32 | 27% | 53% | 95% | 90% | 100% |
| 20 | 0.18 | 18% | 62% | 100% | 93% | 100% |
| 25 | 0.05 | 5% | 67% | 100% | 100% | 100% |
| 30 | 0.05 | 5% | 73% | 100% | 100% | 100% |

- Conc 1+1, 10%x3: at 20 rounds 0.07 false flags per run (7% of runs with any), +4% detected in 63% of kernels, targets 100%. Sequential 7-core 10%x3: at 20 rounds 0.35 false flags per run (35% any), +4% 31%, motion_est 68%. Sequential full image: 0.53 false flags per run at 10 rounds (the whole pool). With a 95% interval instead of 99% (conc 2+2, 25 rounds): 0.28 false flags per run (20% of runs); with a 2% effect floor instead of 3%: 0.15 (13%).
- So a per-kernel effect of about 5% or more is resolved in 20-25 rounds (12-15 min); 3-4% is caught in about two thirds of kernels; the suite row (spread 2%) resolves about 2-3%.
- Same-L3 pairs: 3.9-4.3% spread; the same pairs split across two L3 groups: 7.2%. Common-mode cancellation comes from sharing cache and memory, not just the clock.
- Two cores a side beat one: with one core a side the emulator's helper threads (SCC polling, timers) take turns with its CPU thread (about 2,500 involuntary context switches per run against about 150 with 7 CPUs). Matched mixed batches: A/A 3.9% against 4.8%, suite 2.3% against 8.4%, suite effect +7.9% against +5.1% (sequential in the same rounds +8.5%).
- Three pairs in one group is worse (11.1%): picking six of seven cores means taking busy ones.
- Does running together dilute the effect? Pooled across batches the concurrent resize effect (+9.8%) was far below sequential (+23.4%) with disjoint intervals; in matched batches (concurrent and sequential rounds alternated on the same groups) it was not: resize +11.7 against +15.3 (n=15/18) and +17.0 / +14.9 (2+2) / +16.0 (sequential) (n=15/21/14), intervals overlapping. The effect size of a memory-heavy kernel depends on which group and when, for either protocol. Not settled.
- Shorter rounds help only through the repeats: 10%x1 is worse than the full image (25 ms single samples are hit harder); 10%x3 is best (concurrent full image 5.7% against 10%x3 4.1% in matched rounds, 50 s against 38 s). Running each kernel group as its own A/B pair sequentially: 8.2% at 72 s per round, worse than concurrent at half the time. Of a 28 s run at 10%x3, about 16 s is timed; the rest is the checksum pass and calibration.
- Normalising hurts: dividing each kernel's cost by the run's suite cost, by int/dhrystone or by fpu/matmul raised A/A spread in every protocol (conc 4.3% to 5.0/6.4/5.9%; sequential 7.6% to 8.3/14.7/12.8%) and removed half or more of the real effect (conc alu +12.5% to +6.1-6.7%). Regressing out the sibling-busy difference helped the suite row (11.2% to 3.5% on 1+1 cores) but not the kernels. Not implemented.
- Verdict rule: the median-and-best rule with the per-round noise check cannot conclude at any length on this host (on 51 concurrent A/A rounds it left 31 of 46 rows inconclusive, 26 on A/B). Replaced by: SLOWER/faster when the 99% bootstrap interval of the median ratio excludes 1 and the median passes 3%; same when the interval is inside +-3%; inconclusive otherwise; no verdict below eight rounds (a 3-round sequential run flagged the suite row from three agreeing rounds before the minimum was added).
- Load: concurrent spread was flat across load where both buckets exist; sequential was not. What matters is the load on the chosen L3 group. 3 busy threads on the same group: spread 6.1% (12 rounds, 4 A/A flags of 46 under the 95% rule); the 99% 20-round rule was not run under ballast.
- Pairing on the two SMT threads of one core and 3 pairs in one group were rejected (above).
- End to end, `iris-bench ab` with defaults except 14 rounds, A/B and A/A at once, load 100-145, 9.5 min each:

| run | group chosen | int/alu | img/resize | vid/motion_est | suite | flagged | inconclusive |
|---|---|---|---|---|---|---|---|
| 6fc2c7e5 vs aded5989 | CPUs 84-90 | +14.3% [+10.1, +17.5] SLOWER | +15.4% [+3.7, +22.1] SLOWER | +9.2% [+3.5, +13.8] SLOWER | +6.4% [+4.3, +7.3] SLOWER | 24 kernels slower | 20 |
| 6fc2c7e5 vs itself | CPUs 21-27 | +0.2% inconclusive | +0.1% inconclusive | -0.7% same | +0.1% same | none | 22 |

- The prototype reads topology from sysfs (`thread_siblings_list`, highest cache level `shared_cpu_list`), respects affinity, uses 2 cores a side in groups of 4 or more (1 in groups of 2-3, sequential otherwise), and sets the guest's run settings by patching two initialised `.data` globals in its copy of the image. Cited technique background: duet benchmarking (Bulej et al., ICPE 2020, DOI 10.1145/3358960.3379132; abstract only), pyperf `system tune` (root-only), Google Benchmark random interleaving. Not verified: the non-Linux code paths (only compiled for Linux). Open: more data at load above 120, a guest option to skip verification on timed rounds (would cut rounds about 40%), one pair per L3 group on several groups at once.

### Quick mode on a JIT build, and calibration

Main's `iris-bench` (jitv2, lightning, idle-pause, gpujit), in-process, three quick and three full runs alternating, pinned:

| | quick (n=3) | full (n=3) |
|---|---|---|
| Dhrystone DMIPS | 310, 701, 728 | 721, 722, 689 |
| Whetstone guest MIPS | 48, 56, 55 | 438, 436, 438 |
| suite guest MIPS (median) | 661.5 | 730.8 |
| kernels more than 2% off full | 28 of 46 | - |
| worst kernels | stream_copy -92%, transcend -89%, whetstone -88% | - |

Quick mode's shorter runs finish before the compile pool has published code for several kernels, so they are measured mostly interpreted. The GUI defaults to quick. Two identical quick runs differed 2.3x on Dhrystone. Autoscaling to 250 ms does not hold under the JIT: `calibrate` gives up after four attempts even when the run is below its floor; timed runs below the floor: 15 of 138 in full JIT runs (shortest 13.7 ms) and 44 of 138 in quick (shortest 2.2 ms); the lightning interpreter: 2 of 276, shortest 120.7 ms. A full run reports the best of two timed passes, quick reports one; between two identical full runs of the default build Dhrystone read 94.7 and 90.7 DMIPS. On the aded5989 data, one kernel running twice as slow moves the suite figure by only 0.2% (latency_dram) to 1.3% (Dhrystone), while the real regression moved int/alu64 by -38% in one run (n=1) and the suite by -7.8%.

### Statistics of the first `ab` verdict rule

Reviewing the `ab` prototype's bootstrap (10,000 resamples, 95% percentile, flag when the whole interval clears 3%): at n=5 the median of a 5-sample resample equals the smallest ratio with probability about 5.8%, so the interval is exactly [min, max] and SLOWER means all five paired ratios exceed 1.03; one slow A run (about 1 run in 10 is 25-35% slow) pushes a round's ratio below 1 and the regression goes unflagged. A simulation of that noise model (1% Gaussian plus a 10% chance of a 25-35% slowdown per run; 400 trials; a model, not a measurement):

| true slowdown | bootstrap rule, n=5 | committed median+best rule, n=5 | bootstrap, n=7 |
|---|---|---|---|
| 0% (A/A) | 0.00 flagged | 0.00 | 0.00 |
| 5% | 0.41 | 0.94 | 0.66 |
| 7% (aded5989's Dhrystone) | 0.63 | 0.98 | 0.84 |
| 10% | 0.63 | 0.99 | 0.90 |

The median-ratio-and-best-round rule does much better under that model; the bootstrap rule at n=5 does not. Interleaving answers drift, not layout bias (Mytkowicz et al., ASPLOS 2009, DOI 10.1145/1508244.1508275, remedy: randomise setup such as link order and environment size); `ab` does not randomise layout, and aded5989 is a layout effect (the fix landing 9% above the parent, 100.5 against 92.1 MIPS, suggests layout luck too). A flag means "this pair of binaries differs", not "this change costs X". The Efron (1979) citation was not independently re-verified.

## Where the time goes

Bare-metal `iris-bench run`, 068d0e08, R4400, in-process, n=3 each, each output line timestamped:

| phase | full run | `--quick` |
|---|---|---|
| process start, machine build, ELF load, Count calibration, memory probe (until the plan line) | 0.25-0.45 s | 0.25-0.26 s |
| 46 kernels | 43.9-45.7 s | 21.5-24.3 s |
| of which the timed run the score uses (best of 2, or the only one) | 11.6-12.1 s | 7.0-8.1 s |
| of which the second timed repeat (full only) | about 11.8 s (it is at least the best one) | none |
| of which calibration and the verify() run (by subtraction) | about 20.8 s | about 14.9 s |
| report and exit | 0.02-0.04 s | 0.02-0.04 s |
| total | 44.4-46.0 s | 21.8-24.6 s |

- Subprocess runs (`--iris --elf`, n=3) took 45.9-52.3 s. Six kernels take a third of a full run (median wall): codec/lz 6.0 s, img/dct8x8 2.75 s, img/sharpen5x5 1.85 s, vid/motion_est 1.65 s, codec/huffman 1.65 s, mem/stream_triad 1.3 s; most others take 0.45-0.95 s. `--quick` halves the run but not the expensive kernels (lz 4.4 s, dct8x8 2.05 s, sharpen5x5 1.5 s). The four kernels the maintainer quotes plus resize and motion_est take 5.4 s of a full run's 44.4 s.
- A ship build: `iris-bench` alone at 068d0e08 seeded from a warm ship target, nothing else building, 214 s (36 crates compiled, mostly `iris`); adding the `iris` binary afterwards 135 s more; a no-op rebuild 0.7 s; both binaries with six commits building in parallel 235-279 s each; three in parallel with `--timings` 308-341 s. `cargo --timings` for 5a631b7c: dependencies 6.6 s, the `iris` library 129.8 s (one codegen unit), then the `iris-bench` binary 128.6 s and the `iris` binary 199.8 s in parallel (fat-LTO link and codegen), total 336 s. Building only `iris-bench` saves about 70 s against both; building A and B in parallel costs about the same wall time as one.
- An IRIX run (404a86c7, `lightning,gpujit`, Indy R4400, IRIX 6.5.22, COW overlay):

| phase | measured | n |
|---|---|---|
| container and iris start until the CI socket answers | 0.51-0.52 s | 8 |
| `start` to `login:` on serial (boot) | median 246 s, range 228-286 s | 8 |
| `iris-ci login` | 1.2-1.9 s | 8 |
| `save` of the logged-in machine | 0.8-1.2 s | 2 |
| `restore` of it in the same process | 1.3-3.0 s | 3 |
| the 17 stock plus the maintainer's steps (`steps-full.toml`) | 366.9 s wall, 355.9 s of step time | 1 |
| the maintainer's three long steps (`steps-long.toml`) | 282.7-307.0 s | 3 |
| the maintainer's three short steps (`steps-short.toml`, 1/5 of the work) | 57.1-92.8 s | 7 |
| `quit` and container removal | 3.1-3.4 s | 8 |

  A whole run with the full step list took 619 s; with only the maintainer's three long steps, 534-596 s. Boot is about 40% of either. Per-step host times:

| step | long, n=4 (incl. `full-save`) | short, boot, n=3 | short, restore, n=4 |
|---|---|---|---|
| Dhrystone (10M long, 2M short) | 153.3, 160.8, 162.9, 170.6 s | 32.4, 33.8, 40.6 s | 27.8, 29.7, 30.4, 33.3 s |
| Whetstone (100k long, 20k short) | 61.9, 62.1, 67.6, 78.1 s | 10.7, 11.7, 16.7 s | 13.1, 13.9, 16.2, 16.9 s |
| iris_bench (20 long, 4 short) | 57.8, 59.5, 63.8, 63.9 s | 11.1, 12.2, 33.3 s | 12.2, 13.4, 16.7, 19.1 s |

- A snapshot cuts each extra sample's fixed cost from about 250 s (start, boot, login) to 1.3-3 s. A snapshot restored into a fresh process did not work (`restore login` returned "restored", the CPU ran, but the guest never answered on the CI serial and the screen stayed black; two attempts). A paired A/B under IRIX therefore still needs one boot per binary.
- On the busy host none of the IRIX configurations resolves 5%: range over median for one step is 11% for long Dhrystone (n=4), 24% for short Dhrystone and 3x for short `iris_bench` between boots (n=3), 18-52% for the three short steps within one process (n=4). Short Dhrystone (2M) takes 28-41 s against 153-171 s for 10M, so the per-step round-trip floor (0.3-0.4 s) is not the limit. Earlier indy-perf figures for scale: Dhrystone 10M 125.2 s (121.6-171.3, n=3) on a default build and 18.0 s on the jitv2 build at 092a6ee9, and 105.0-108.3 s on the 1cbc2a4a `lightning,gpujit,idle-pause` build, against 153-171 s on this note's `lightning,gpujit` build at 404a86c7; those were not run paired, so the difference is not attributed.

### Fastest configuration that still resolved a step of aded5989's size

Each row is one experiment; none was repeated. "Detects" means the A/B statistic sat outside that experiment's A/A by more than the A/A's own distance from 1. True cost on this build: about 3% suite, 11-14% alu.

| configuration | wall per run | A/A: best-of geomean | A/A: cyc | detects aded5989 (true cost here about 3% suite, 11-14% alu)? |
|---|---|---|---|---|
| full, n=15 interleaved (`out/d2`) | 45 s | 1.002 | 1.000 | yes: best-of 0.979, cyc 1.027, alu best 0.863 |
| full, n=10 (`out/d2 --first 10`) | 45 s | 1.006 | 1.000 | yes: 0.979, 1.027 |
| full, n=5 (`out/d2 --first 5`) | 45 s | 1.001 | 1.002 | cyc only: best-of 0.998, cyc 1.033 |
| full, n=10 in-process (`out/t-cfg`) | 44 s | 1.018 | 0.969 | no at suite level (0.971, 1.002); alu yes (best 0.861) |
| `--quick`, n=10 (`out/t-cfg`) | 21 s | 0.980 | 1.017 | yes: best-of 0.956, cyc 1.061, alu best 0.858 |
| `--quick`, n=5 | 21 s | 0.983 | 1.017 | yes: best-of 0.960, cyc 1.073 |
| full, A and B at once on two CCDs, n=10 (`out/p-par`) | 45 s for both | 1.022 | not recorded | alu 0.88 paired, best-of geomean 0.938; per-round A/A geomean range 0.705-1.372, wider than serial |
| six kernels only (computed from the full runs, not run) | about 5.7 s | 0.954-1.032 over 5 A/A pairs | - | best-of 0.901 / 0.919 against 0.946 / 0.938 for all 46 |

- `--quick` cannot be combined with `--iris` ("Not accepted with --iris") and the iris CLI has no flag for the run-config register, so a quick run is in-process, which holds the image fixed only when both builds embed the same ELF (false across 6adc7927).
- There is no kernel filter in `iris-bench run`, so the six-kernel row is computed from full runs. Parallel A/B ran without `perf`.
- Recommendation recorded: in-process `--quick`, A, B and a second A interleaved in random order, n=10, one L3 group, under `perf stat`, judged on the lowest host cycles per guest instruction and the best-of-n geomean over 46 kernels, with the A/A arm as noise floor: 30 runs at about 21 s, 10.5 minutes, plus one pair of parallel `iris-bench`-only ship builds (about 4.5 minutes). Per-kernel verdicts need n=15 or more on the busy host, and even then a 10% kernel step is only just outside its A/A. Holding the image fixed needs `--iris`, full runs only: 22.5 minutes at n=10.
- The first tool's cost per PR (4 feature sets x 2 revisions of fat-LTO builds, then 4 x 2 x 5 runs of about 41 s each) is easily an hour before any IRIX run. If the A/A spread needs extra rounds it adds 50-100%.

## Why the slowdown was missed

1. Nobody measured aded5989. It was a bridge logging change on a branch of device work; its message was reasoning, not a measurement, and none of that branch's messages had a performance section. The rule "PRs carry performance results" was applied to CPU, FPU and similar stacks, not to a device change that edited `src/cpu/mips_exec.rs`. With fat LTO and one codegen unit, a change anywhere in the crate can move the hot loop, so no path rule is complete.
2. A rule for this exact case existed and was not applied: `perf/hot-path-inlining-must-be-explicit.md` (2026-10-21) records an earlier x0.84 lightning regression (1431f560, a 4-word snapshot added to a cold path in `CpuCache::fetch` pushed `fetch` out of line; `sys/uncached` x0.51, `int/branch` x0.75, IRIX Dhrystone 71428 to 58823; fixed in 28d8c01a back to x1.02). It says miss paths reachable from `fetch`, `read`, `write` or `step_int` must be `#[cold] #[inline(never)]` and the symbol table checked. aded5989 landed ten days later.
3. Measured our way it would have passed (above): on 2026-11-06 the pass bar became "deterministic host instructions equal or lower than base", and real-clock dips were waived as layout. aded5989 is a layout regression.
4. The builds measured were not the build that regressed: only the lightning interpreter regressed; our timed PR tables used jitv2 ship builds; `TESTING.md` builds `iris-bench` with default features, which are flat across aded5989. Among the PR bodies reviewed, the lightning interpreter was timed on the real clock in exactly one (`lightning-fused-count`, R10000, against base 45b7f745, which already contained aded5989); another counted `lightning,ci_clock` deterministically against a base that also contained it.
5. Every base compared against already contained the regression: aded5989 landed 2026-10-31; PR bases 45b7f745, 783920bc (2026-11-06) and 404a86c7 all post-date it. Nothing compared main with a known-good build, and losses under 3% can pile up.
6. The tools hid it (next section). No test ran IRIX; the IRIX half was documented "written, not yet run" and did not work. The 1-8% IRIX loss and 7-17% per-kernel bare-metal loss meant only a bare-metal check on the right build could catch it.
7. The 6adc7927 miss has the same shape: the commit message asked for a regenerate, `check-prebuilt` compared source digests, CI passed.
8. Nobody asked for the reporter's build features, commit and host up front. `iris` has no `--version`; features appear only in the startup `build features:` line (`iris --help` prints it first, so it is a cheap probe).
9. The memory note making host instructions the pass condition waived exactly the layout class; the earlier rule ("zero regression ... layout effects included: fix placement in source, e.g. cold paths out of hot text") was right and was relaxed.

## Bugs found in `iris-bench`

All observed on saved results unless marked "code reading" (12 items, ranked by impact):

1. `report`'s "vs base" column divides the best of all emulated cells by the base, so it never goes below 1.00x and every regression reads 1.00x. 6fc2c7e5-light against aded5989-light (same image): int/alu 53.67 M to 46.26 M, resize 1.52 M to 1.26 M, Dhrystone 244 k to 226 k all read 1.00x; with `--baseline aded5989-light` they read 1.16x, 1.20x, 1.08x. Another example: Dhrystone 245 k to 183 k read 1.00x.
2. `report` compares runs from different guest images without warning: suites be520bc1 and 3bc1cce2, 139.7 to 104.4 DMIPS, no warning.
3. It also mixes quick and full, deterministic and timed runs, hosts and feature sets, printing only the first run's host. One report held an EPYC 7443 host (2 cores) and an EPYC 7663 host, a quick deterministic `ci_clock` run and two full timed runs, with one host line; the deterministic run's fixed 100.0 MIPS / 133.1 DMIPS ranked above the real 84.9 MIPS.
4. No repeated-run support: `save` writes `<label>.json`, so a rerun overwrites the last; six columns came out as three `6fc2c7e5-light` and three `aded5989-light`; no median, spread or count.
5. Deterministic results print fake speeds: a `ci_clock` run always reports 100.0 guest MIPS, and its DMIPS just reflects the compiler (1e8 / 428 / 1757 = 133). Both commits read 100.0 MIPS and 133.1 DMIPS, every kernel 1.00x. `reference` took a full `+deterministic` run as a reference row (64.9 MIPS, 94.7 DMIPS).
6. `run --deterministic --iris BIN` does not check that BIN is a `ci_clock` build: 1cbc2a4a-plain twice gave total guest instructions 857.8 M and 865.6 M, with 21 of 46 kernels different.
7. A failed `--iris` run shows the tail of stdout and drops stderr; and `--deterministic --iris` cannot work on any lightning build because `iris` refuses `--jitv2-compile sync` there while the in-process runner accepts lightning plus deterministic.
8. `iris-bench irix` step times are rounded to 100 ms (a 100 ms `try_wait` poll and a floor rounded the same way); all 880 recorded step times: every value under 1 s is 100, 101, 200, 201 ms. JIT medians: read_cold 0.10 s, read_warm 0.10 s, walk_usr 0.10 s, syscall_copy 0.20 s, stat_files 0.20 s. The "cold" read is served from IRIX's buffer cache (a 256 MB Indy keeps both 16 MB files): JIT 0.10/0.10 s, lightning 0.60/0.60 s.
9. `report` renders IRIX results badly: accuracy "0.0% (0/0)", the CPU column holds the `uname` line with echoed shell input, and steps with `work = 0` (`tar`, `find`) show "-" so their times appear nowhere.
10. `engine_of` knows only interp and jitv2, so a lightning result is labelled "interp" and matched against plain-interpreter reference rows 1.3x apart (92.1 against 69.6 suite MIPS).
11. `matrix` reuses `bench/build/iris-<features>` if it exists unless `--force-build` is given (code reading).
12. `matrix` and `run --iris` default to `bench/build/irisbench.elf` built with whatever cross compiler is installed, while in-process `run` and the GUI use the prebuilt image (code reading); CI builds its ELF with Ubuntu 24.04's `gcc-mips-linux-gnu`, a different compiler again, so CI's bench numbers are never comparable with the prebuilt image's.

Doc claims that did not match behaviour: `make -C bench bench` runs in-process with the tool's own features and embedded ELF; `--quick` "about half the wall clock, the same numbers within a couple of percent" is false on a JIT build; the 250 ms autoscale does not hold under the JIT; `make -C bench matrix` defaults to four cells with no lightning and no R10000 family; "three kernels report in units with four decades" lists two; `src/benchsuite.rs` points at a `bench.yml` that does not exist (`suites.yml` compares a digest of the sources, not the image); `BenchOptions::quick` says "Fewer groups" but `QUICK.groups` is 0; the `bench/irix` status line "written, not yet run" is stale; `steps.toml` says "Keep each step above a second of work" while most steps take 0.1-1 s on the JIT build. A host pinning script pinned each group's SMT siblings as well, and its comment "Deterministic runs do not need this" built the instruction-only habit into the tool. Checksums: accuracy comes from each kernel's single verification pass before calibration; the timed iterations are never checked; "40 of 40" is 40 of the 46 kernels (six `sys/` kernels have no checksum); `bench/irix` steps are not checked at all. Not measured: in a jitv2 build much of each verification pass may run before compiled code is published, so "100% accuracy" may mostly cover the interpreter.

Other plan-review findings with numbers: a PR check for the compiler "lock" does not lock anything while `prebuilt-stamp.py` writes the PROVENANCE the test compares against (a rebuild with GCC 14 followed by a restamp passes everything); the plan's "cycles rose 7-9%" is 9-10% (136.9 to 149.8, +9.4%, n=3; 136.6 to 150.0, +9.8% from `perf`); "0.73-0.79x" is two numbers (0.73 per-iteration ratio, 0.79 the Mac figure); the per-iteration figures in the recorded JSON are Dhrystone 313.15-313.24 and rotate90 1,049,286-1,049,291 (old image), 427.1-428.3 and 1,440,269-1,440,285 (new image, interpreters); `iris --version` exits with "unexpected argument" on 404a86c7; the probe bug was in the `requires` probe (backslashes in `tr : \" \"` surviving in backquotes, 72c9918d), not in `tr`.

## What building the prototypes taught

### Report fixes (branch `bench-report-fixes`, 8 commits off 404a86c7)

Warnings for mixed guest images, run lengths, clocks, hosts and features; per-cell "vs base" and "native"; repeated runs kept (`label.json`, `label.2.json`...) and grouped with median, [min-max] and n; deterministic runs print counts not speeds, and `reference` refuses them; `--deterministic --iris` probes the binary's features first; a failed `--iris` run shows exit status and stderr; lightning is its own engine. Gate: `cargo test --bin iris-bench` debug and release, 35 passed each; every commit tested alone. Corrected output on the saved inputs: aded5989 against 6fc2c7e5 (same image) alu 0.86x, Dhrystone 0.93x, resize 0.83x (all 1.00x before); three runs each gave `(n=3)` and `92.1 [92.1-92.2]` MIPS; deterministic runs show 613,905,711 guest instructions and no MIPS/DMIPS. Lessons: `ci_clock` is itself a feature, so a naive "features differ" check fires on every timed-versus-counted pair (the clock is its own axis); `matrix` compares features on purpose, so it passes `across_features`; host instructions need a counter inside `iris-bench` (a `perf_event_open` wrapper, user-only, inherit; worked natively with `perf_event_paranoid` = 1, untested inside the build container); lightning cannot take `--jitv2-compile sync`; the lightning+jitv2 refusal is inference from the config check message, not a measured nondeterminism. Not done: bugs 8, 9, 11, 12 above; `report --baseline NAME` with an unknown name falls back silently; no positive end-to-end deterministic `--iris` run on a `ci_clock` binary.

### Two CI checks (branch `bench-ci-checks`, off 404a86c7)

The disassembly check (section above), and a per-kernel workload table: `make -C bench workload` builds `iris-bench` with `ci_clock` only (LTO off, 16 codegen units), runs the embedded image with `run --deterministic --cache full` on the plain interpreter, R4400, and writes `bench/prebuilt/workload.txt` (icount / iters to two decimals); a `suites.yml` job regenerates it and fails on a diff. Measured: two deterministic runs identical on all 46 kernels; in-process against `--iris --elf` identical; regenerated table byte-identical to the committed one; a one-kernel edit (`x ^= x >> 13;` in `alu64_chain`) changes only int/alu64 (18.00 to 21.00); a comment-only edit changes the ELF hash (DWARF line numbers) but not `.text/.rodata/.data` or the table; a plain build without `ci_clock`, two timed runs: fpu/transcend -1.1%/-0.9%, fpu/matmul +0.9% (their work depends on the iteration count the clock picks), everything else within 0.5%; so a tolerance check against a timed run needs 1.5% or exclusions. Cost: 1 m 22 s build into a fresh target with 64 jobs plus a 96 s deterministic run, 3 m 05 s in all (CI cost on 4 vCPUs guessed 8-15 minutes, not measured). It says what changed (Dhrystone 428.15 to 313.10 and five other kernels), separates bytes changed from work changed, and covers the emulator side; it does not detect a compiler change as such. Known weakness: exact match is brittle by about 1% on transcend and matmul when iteration counts move; rounding to 4 significant figures would also shorten the diff a compiler bump makes in about 15 kernels (mem/copy 1835044.69 to 1835043.69).

### Pinned guest toolchain (branch `guest-toolchain-pin-int` at 3d80957d, off 404a86c7)

One compiler for every MIPS guest binary: Fedora 45 GCC 16.2.1 (`gcc-mips64-linux-gnu-16.2.1-1.fc45`, `binutils-mips64-linux-gnu-2.47-1.fc45`, `cross-gcc-common-16.2.1-1.fc45`), pinned in `cpu-tests/toolchain.mk`, run in a container, reproducible flags (`-ffile-prefix-map`), CI rebuild-and-diff, a `check-prebuilt` that reads the producer string, two `cpu-tests` bug fixes and a test-count gate. Verified on x86-64 Linux with rootless Podman 5.8.6: the toolchain image builds in 22 s; `irisbench.elf` is 0c92ebe4...0eac from two host paths and a different mount path (and equals the Apple Mac build), `cputest.elf` aef8b3a5...5c3d; `make -C bench prebuilt` leaves the tree clean; with the GCC 14 image back, `check-prebuilt` exits 2 ("built by GCC 14.2.0, the pinned compiler is 16.2.1") and the rebuild-and-diff exits 1 (308876 to 309628 bytes). The cpu-tests cells (CI's exact command, loop-profile release builds):

| cell | pinned GCC 16.2.1 | GCC 12.4 (Ubuntu 24.04), this branch | GCC 12.4, 404a86c7 (pre-switch) |
|---|---|---|---|
| R4400 interp | 262 tests, 48 failed checks (15 tests) | same, test by test | same, test by test |
| R4400 jitv2 | 262, 50 (16 tests) | same | same |
| R5000 interp | 262, 35 (10 tests) | same | same |
| R5000 jitv2 | 262, 37 (11 tests) | same | same |

Per-test pass/fail lists identical in every cell; one check differs for a reason that is not the compiler (`cp0/compare_sets_ip7` prints "timer did not fire" and passes; 2698 against 2699 passed). With the `index_tag_rt` fix reverted, the pinned R4400 interpreter ran 256 of 262 tests and still reported exactly 48 failures (R5000 256 and 35); the old count-only gate passed both and the new check fails both. Fork CI: the `guests` job succeeded in every job (toolchain image built in about 12 s with Podman, `--userns=keep-id`; cpu-tests exactly the new limits 48/50/35/37 with 262 tests; bench 40/40 matched in all four cells), and failed at `check-prebuilt` with the GCC 14 image put back. Remaining risks: the toolchain image is rebuilt from Docker Hub and Fedora's mirrors on every run (rate limits, mirror outages); Fedora 45 end of life 2027-12-08, after which the pin must move to the archive or a newer release, and a bump changes every benchmark figure; only three packages are named (their dependencies come from the frozen release repository); the failure limits now equal the measured counts exactly.

### IRIX harness and public workloads (branches `bench-irix-fixes`, `irix-dhry-whet`)

`iris-bench irix` had never been run: against an Indy on IRIX 6.5.22 it skipped every step (the `requires` probe), aborted on the first quiet failure and discarded step output; also `dd bs=1m` fails ("Bad numeric argument"; use 1048576), `ping -i 0` is rejected (use `-f`), `xwd` hangs at the xdm login with no window (fails at its 60 s timeout). After the fixes, the default steps plus the three benchmark steps ran end to end on `indy_ip24` (R4400, 256 MB, Newport), jitv2 build of 404a86c7: Dhrystone 10M reported 500000/s, Whetstone 833.3 MIPS, `iris_bench` 5.85 MB/s; 24 tests in `cargo test --bin iris-bench`. Dhrystone 2.1 and Whetstone 1.2 from public netlib sources, built with `cc -O2 -n32 -mips3` (plus `-DTIME` for Dhrystone): an unstripped `-O2` link has byte-identical `.text` to the maintainer's binaries; `-O3`, `-O1`, `-O0`, `-Ofast` and `-g3` do not, and `-O1`/`-O0` are 1.5x to 4x slower. Indy R4400, IRIX 6.5.22, 404a86c7, median of seven alternating pairs: Dhrystone 10M 25.4 s (maintainer's) against 25.5 s (built here), Whetstone 100k 14.3 s against 14.0 s; host instructions agree within 2%.

## Related measurements from the cache-as-RAM study

The study of how iris should model cache lines that the R10000-family PROMs use as RAM measured the same tools; its design is recorded elsewhere. The measured parts that bear on benchmarking:

- A candidate (each CPU reads memory through its own copy of ppmem's window; a valid line makes its page private in that view) changes none of the load, store or fetch paths. Deterministic `iris-bench run --quick --deterministic`, ship profile, `perf stat -e instructions:u`, alternating main/spike/spike/main/main/spike, n=3 each, 40/40 checksums every run; final spike against main 068d0e08:

| Build | CPU, cache | main (G instr) | spike (G instr) | delta |
|---|---|---|---|---|
| `ci_clock` | R10000 shadow | 344.307 | 344.413 | +0.03 % |
| `ci_clock` | R4400 shadow | 344.807 | 344.680 | -0.04 % |
| `ci_clock,jitv2` | R10000 shadow | 205.640 | 205.316 | -0.16 % |
| `ci_clock,jitv2` | R4400 shadow | 205.051 | 205.002 | -0.02 % |

  The first spike commit alone measured +0.07, -0.06, +0.10/+0.06 and +0.11% on the same rows and +0.01% on the R4400 full cache (control). The same main binary moved 0.22% between sessions (jitv2 R10000 205.19 then 205.64), so +-0.2% is the floor and no difference is resolvable. `iris-bench` runs in KSEG0, so this says nothing about TLB-mapped user code.
- The cost of the earlier design's machine-wide switch-off (#448: compiled code off while any staged line exists), measured with A and B concurrent in pairs, two pairs each, light logging, `loop` profile, against the same build forced to keep compiled code on:

| Machine | Phase wall, switch-off | Phase wall, forced on | Boot to login, off / on |
|---|---|---|---|
| O2 R10000 | 49.6, 48.8 ms | 46.0, 48.1 ms | 96.9, 95.6 / 99.5, 89.3 s |
| O2 R12000 | 65.7, 56.0 ms | 54.8, 53.8 ms | 100.9, 89.5 / 102.0, 100.6 s |
| Octane | 1.27, 1.11 ms | 1.22, 1.20 ms | 97.2, 87.2 / 92.8, 87.4 s |
| Fuel (long phase) | 38.1 s (322.1 M instr), 35.4 s (319.7 M) | 30.9 s (376.6 M), 25.6 s (381.5 M) | 137, 144 / 130, 125 s |

  O2 and Octane: no difference beyond noise. Fuel: the switch-off costs about 8.5 s of the long phase (mean 36.8 s against 28.2 s, about 23%, ratio 8.5/36.8 = 0.23), or about 13 s of boot to login (mean 140.5 s against 127.5 s, about 9%; 13/140.5 = 0.093); during the phase the interpreter ran at 8.5 and 9.0 MIPS and the forced runs at 12.2 and 14.9 MIPS. Tezro (2 CPUs, three runs each, alternated): the hold covers almost the whole PROM up to the BASEIO banner, 35-39 s on the default build and 56-67 s on the jitv2 build (the jitv2 build reached BASEIO 22-29 s later than the default build because every CPU holds lines for the whole phase, so no compiled code runs).
- Tezro primary-instruction staging is required on the model (the PROM hangs at `Copying PROM code to memory` with it off; a build with only secondary lines did not get past it in 520 s on 1 or 2 CPUs; the build with it served printed `DONE` after 32.6 s). The spike booted the Fuel (login 117, 126, 115 s), O2 R10000 (93.5 s with a shared window, 84.1 s with per-CPU views), O2 R12000 (92.4 s), Octane 1 CPU (91.9 s) and 2 CPUs (80.4 s), Indigo2 IP28 (117.7 s) and Indy (149.8 s), single runs on a loaded host and not like-for-like timing; the library tests passed (3294, 0 failed, including six spike tests).
- An instrumentation build that scanned a linear list of held lines on every tag store slowed the PROM enough that a 2-CPU Tezro boot spun for 10 minutes past `Copying`: the probe perturbs timing, so runs of that build were discarded.

## Still in progress

- **Cold-bus fix on the quiet host.** An A/B of the fix against main on the 24-core EPYC 7443 host, one L3 group (cores 12-17, one thread per core), was started and blocked before any measurement: the quiet host's SMT siblings are CPU N and N+24, cores 12-17 were the quietest group when sampled (about 10-13% busy on the first thread, siblings about 9-26%), the builds (main and fix in default, lightning and release feature sets) were finished on the busy host and 6fc2c7e5/aded5989 release builds and the 73a1df35^ / 73a1df35 pairs were in flight, but SSH to the quiet and busy hosts then failed after the key was accepted. No numbers exist yet. Nothing was copied to the quiet host.
- **R5000 and R10000 lightning, macOS Mac App Store build under the GUI, Windows**: not measured.
- **jitv2 with the JIT on at aded5989**: not measured (only main against the fix).
- **A confirming A/A for 73a1df35** and a check on other hosts.
- **The unexplained part of the maintainer's rotate90 0.56 and alu 0.92.**
- **What a quiet pinned host resolves**, and whether the busy host's A/A spread falls with boost off or a cpuset partition.
