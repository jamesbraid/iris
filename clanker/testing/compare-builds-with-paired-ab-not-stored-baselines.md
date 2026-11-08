# Compare builds with paired, shuffled A/B, not stored baselines or nightlies

Written 2026-11-08, after chasing a reported 0.58x Dhrystone drop through a
week of commits on the shared 224-core host. Tool: `iris-bench ab` (`make -C
bench ab A=.. B=..`; method in `bench/README.md`, "Comparing two builds").

**Rule.** To ask "did this change make iris slower", build base and candidate,
run both now, alternating, on the same guest image, and read the paired ratio
with its interval. Do not compare against a number written down last week, and
do not wait for a nightly to notice.

## Why a stored baseline lies

- **The guest binary moves.** 6adc7927 rebuilt `bench/prebuilt/irisbench.elf`
  with GCC 14 instead of 16. Dhrystone went from 313 to 428 guest instructions
  per iteration (+37%), rotate90 from 1.05M to 1.44M. DMIPS fell with MIPS
  flat. An in-process `iris-bench run` uses the image linked into that commit,
  so any bisect across 6adc7927 shows a regression that is not in the emulator.
  Both sides of an A/B run one ELF (`--elf FILE` when the builds span the
  rebuild), and the tool warns when instructions per iteration differ.
- **The host moves.** Load went 90-220 during the investigation. A baseline
  from an earlier hour was taken on a different machine. Alternating A and B
  within a round, in random order, makes the ratio see one host.
- **Busy-host outliers.** About 1 run in 10 is 25-35% slow, in wall time and
  in user instructions, probably threads spinning while they wait. A mean, or a
  best-of-N from different days, is moved by it. The paired median is not.
- **SMT siblings and shared cache.** With sibling threads in the CPU set, wall
  rates were bimodal by about 30%. Another process landing on the sibling of
  the emulator's core moves one run by 10-30%, and nothing short of root can
  keep the siblings free. So `ab` runs A and B at the same moment on their own
  physical cores of one last-level-cache group, re-picking the least busy cores
  each round, and reads the per-round ratio. That cut the A/A scatter of one
  round from 6-17% (sequential) to about 4% per kernel.

## Host instruction counts miss layout and branch regressions

aded5989 ("explain empty-slot bus errors") added no work to the hot path.
Host instructions per guest instruction: 515 to 517, flat. Cycles per guest
instruction: 136.6 to 150.0 (+7-9%). Branch misses per guest instruction: 0.43
to 0.69 (+60%); front-end stall cycles doubled. A String argument in four
never-taken bus-error branches changed how fat LTO laid out `step_int`. An
instruction-count check, which is what is cheap and quiet on a loaded host,
passes this. So `ab` records cycles too, and the rule is to read both: flat
instructions with rising cycles means layout or branches, not extra work. The
fix (each report in its own `#[cold] #[inline(never)]` method) is in the
investigation notes, not landed.

Layout luck cuts both ways: the same fix cost 3% in the jitv2 dispatch-off
build. Check the matrix, not one build.

## The build matrix to check

| build | features / env |
|---|---|
| default | none (`cargo build --release`) |
| lightning | `lightning,gpujit,idle-pause` (full cache model, the interpreter) |
| jitv2, JIT on | `lightning,jitv2,gpujit,idle-pause` |
| jitv2, dispatch off | the same binary with `IRIS_JIT_DISPATCH=off` |

aded5989 showed only in lightning; plain and jitv2 were flat. A change to the
CPU or bus code needs at least lightning and one of the jitv2 rows. Use ship
profile (`IRIS_PROFILE=ship`: fat LTO, one codegen unit) for both sides; the
loop profile has different inlining and tells you nothing about the layout.

## How the verdict is made

A row is SLOWER when the 99% bootstrap interval of the median per-round B/A
ratio lies above 1 and the median passes 3%; the same when the interval lies
within 3% either way; inconclusive otherwise; nothing with fewer than eight
rounds. The first version flagged when the median and the best-round ratios
both passed 3%, and called a row inconclusive when one round's scatter (the
noise column) exceeded 3%. That scatter does not shrink with rounds, so on this
host the rule could not conclude at any length (31 of 46 rows inconclusive on
51 concurrent A/A rounds). The interval does shrink: in draws of 25 concurrent
rounds it found the aded5989 loss in alu, resize and motion_est every time and
flagged nothing in 19 of 20 A/A draws (at 20 rounds, resize in 93%). Default 20
rounds, about 13 minutes on the shared 224-core host.

Do not normalise rounds by the suite or by a reference kernel. Tried on the
same data: it raised the A/A scatter and removed most of the real effect,
because the regression is global and host speed is not common to kernels that
run seconds apart.

A flag means "this pair of binaries differs", which includes code layout;
`ab` randomises run order only, not layout, link order or environment size
(the remedy in Mytkowicz et al., "Producing Wrong Data Without Doing Anything
Obviously Wrong!", ASPLOS 2009), so rerun and check the host instruction count
before blaming the diff.

## hyperfine, for whole-command wall time

Checked 2026-11-08 against hyperfine 2.0.0 (README, `--help`, source, real
runs; installed with cargo into a scratch `CARGO_HOME`). Not an iris
dependency.

- Runs each command's batch in turn (`A A A A B B B B`), warm-ups included.
  `--parameter-list` does the same per value. `--setup`, `--prepare`,
  `--conclude`, `--cleanup` do not reorder anything. No interleaving option;
  to interleave you call it once per round with `-r 1` and lose its statistics.
- Reports mean and standard deviation, median, min, max, and a relative speed
  that is a ratio of means. No confidence interval, no test, no pairing.
- Outlier handling is a warning for a slow first run only. The modified-Z-score
  detector in the source is `#[cfg(test)]` in 2.0.0. The mean is not robust to
  the 1-in-10 slow run; use the median.
- `--export-json` has every measurement of every collected metric (wall, user,
  system, peak memory, and counters where the host exposes them), and a
  summary block. Good input for our own pairing.
- It randomises the environment size per run, which touches one of the layout
  biases that Mytkowicz et al. 2009 describe. Commands run without a shell by
  default.

Use it for: the wall time of an `iris` bare-metal run, a boot to `login:`, a
single IRIX step in a wrapper script. Still use `iris-bench ab` for: per-kernel
guest rates, the fixed ELF rule, host cycles per guest instruction, and
interleaved rounds with an interval. A later `iris-bench irix` comparison can
reuse the pairing loop (`paired_rounds`) with its own closure.

Evidence: the investigation's raw numbers and commit bisect are in the
2026-11-07/08 Indy IRIX performance write-up (not in the repository).
