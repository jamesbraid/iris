# Compare builds with paired, shuffled A/B, not stored baselines or nightlies

Written 2026-11-08, after chasing a reported 0.58x Dhrystone drop through a
week of commits on the shared 224-core host. Tool: `iris-bench ab` (`make -C
bench ab A=.. B=..`; method in `bench/README.md`, "Measuring a change" and
"Comparing two builds").

**Rule.** To ask "did this change make iris slower", build base and candidate,
run both now, side by side, on the same guest image, and read the paired ratio
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

The list of builds users run, with and without `idle-pause`, is in
`bench/README.md`, "Measuring a change". aded5989 showed only in the lightning
interpreter; the default and jitv2 builds were flat. A change to the CPU or bus
code needs at least the lightning interpreter and one jitv2 row. Build both
sides with the release profile as `Cargo.toml` defines it (fat LTO, one codegen
unit): a profile that inlines differently says nothing about layout.

## How the verdict is made

A row is SLOWER when the 99.9% bootstrap interval of the median per-round B/A
ratio lies above 1 and the median passes 3%; the same when the interval lies
within 3% either way; inconclusive otherwise; nothing with fewer than eight
rounds. The first version flagged when the median and the best-round ratios
both passed 3%, and called a row inconclusive when one round's scatter (the
noise column) exceeded 3%. That scatter does not shrink with rounds, so on this
host the rule could not conclude at any length (31 of 46 rows inconclusive on
51 concurrent A/A rounds). The interval does shrink: in draws of 25 concurrent
rounds it found the aded5989 loss in alu, resize and motion_est every time and
flagged nothing in 19 of 20 A/A draws (at 20 rounds, resize in 93%). That was
at 99%; the default is now 99.9% and 30 rounds (table under "Measured"), which
should take about 20 minutes on the shared 224-core host, scaled from the 13
minutes 20 rounds took.

Do not normalise rounds by the suite or by a reference kernel. Tried on the
same data: it raised the A/A scatter and removed most of the real effect,
because the regression is global and host speed is not common to kernels that
run seconds apart.

A flag means "this pair of binaries differs", which includes code layout;
`ab` randomises run order only, not layout, link order or environment size
(the remedy in Mytkowicz et al., "Producing Wrong Data Without Doing Anything
Obviously Wrong!", ASPLOS 2009), so rerun and check the host instruction count
before blaming the diff.

## Measured, 2026-11-08

Lightning-interpreter builds of 6fc2c7e5 (A) and aded5989 (B), guest image
blake3:be520bc16f2f88d8, the default settings (concurrent 2+2 cores, time_pct
10, three repeats, 99% interval, 20 rounds, 3% effect; the defaults then), on
the shared two-socket EPYC 7663 host at load 190-400. Each run took 13-15 minutes for 20 rounds. Paired B/A
cost with its 99% interval and the row's noise; host counters are per guest
instruction, B/A.

A/B:

| rounds | int/alu | img/resize | vid/motion_est | suite | rows SLOWER | host insn | host cycles |
|---:|---|---|---|---|---:|---:|---:|
| 20 | +18.7% [+9.0, +40.5] SLOWER | +11.0% [+3.6, +20.6] SLOWER | +8.1% [-3.0, +29.5] inconclusive | +7.2% [+0.7, +12.0] SLOWER | 7 of 46 | -0.4% | +6.1% |
| 40 | +13.8% [+6.2, +17.1] SLOWER | +16.3% [+9.4, +26.8] SLOWER | +14.4% [+11.8, +19.9] SLOWER | +7.6% [+4.9, +9.3] SLOWER | 30 of 46 | -0.4% | +7.9% |

A/A, the 6fc2c7e5 binary against itself, 20 rounds each:

| run | int/alu | img/resize | vid/motion_est | suite | flagged | median row noise |
|---|---|---|---|---|---|---:|
| 1 | +1.2% [-21.1, +30.9] | +1.9% [-4.5, +6.1] | +0.0% [-21.2, +43.0] | -0.6% [-5.9, +2.9] | none | 24.8% |
| 2 | +1.8% [-8.8, +7.7] | +1.1% [-7.0, +9.2] | -0.6% [-7.0, +5.0] | -0.7% [-4.3, +1.5] | mem/latency_l1 faster, -4.8% [-16.1, -0.4] | 9.7% |
| loaded | -1.5% [-8.6, +7.2] | -2.7% [-18.3, +20.1] | +0.5% [-9.2, +14.2] | +0.0% [-7.0, +3.4] | none; all 46 rows and the suite inconclusive | 19.4% |

"loaded" ran in one cache group held through its lock, with four copies of the
suite started and stopped at random (3-15 s on, 1-8 s off) on that group's 14
CPUs for the whole run. Every row came out inconclusive and none was flagged,
which is the behaviour wanted when a run cannot tell. Run 2's false flag came
from three rounds in which B ran latency_l1 about 1.8 times as fast as A, and
two more at 1.2-1.3 times.

Drawing subsets of these 60 A/A and 60 A/B rounds:

| interval, rounds | false flags per A/A run | runs with any | alu, resize, motion_est found |
|---|---:|---:|---|
| 99%, 20 (the first default) | 0.45 | 32% | 82%, 92%, 88% |
| 99.9%, 20 | 0.03 | 2% | 65-75% |
| 99.9%, 30 (the default) | 0 | 0% | 90-100% |

The default moved to 99.9% and 30 rounds because the 99% rule at 20 rounds gave a
false flag in a third of A/A runs, and 99.9% alone at 20 rounds found the
losses less often (65-75% against 82-92%). On a host this busy, still rerun a run with a
single flagged row before acting on it. The
earlier figures in this note came from load 70-120, where one round's noise was
about 4% rather than 8-25%.

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
