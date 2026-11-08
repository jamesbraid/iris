# One pinned compiler for every MIPS guest binary

Status: design for review, not started (2026-11-08).

## Why

`cpu-tests/` and `bench/` are bare-metal MIPS III programs that iris runs.
Whatever compiler happens to be at hand builds them, and five different ones
are in play today:

| Who builds | Compiler | Used for |
|---|---|---|
| `cpu-tests/toolchain.mk` on a developer machine | first `mips-linux-gnu-`, `mips-unknown-linux-gnu-` or `mips64-unknown-linux-gnu-` on `PATH`, any version | local runs of both suites |
| CI, `guests` job in `.github/workflows/suites.yml` | Ubuntu 24.04 `gcc-mips-linux-gnu`: GCC 12.4.0 (the metapackage is labelled 12.2) | the binaries every CI cell tests |
| `make -C cpu-tests toolchain-local` | Debian GCC 12 packages | developers without root |
| the embedded image before 6adc7927 | GCC 16.2.1 20260926, built in `/mnt/fast/safe/gits/iris/bench` | `bench/prebuilt/irisbench.elf`, which `iris` embeds |
| the embedded image since 6adc7927 | Debian GCC 14.2.0, built in a container at `/src/bench` | the same file |

Nothing records or checks which compiler built what, and that has caused
three separate problems.

**1. The benchmark changed and nobody noticed.** The suite reports rates of
compiled guest code, so the compiler is part of the workload. With the same
sources and `-O2`, GCC 14 makes Dhrystone and rotate90 execute 37% more guest
instructions per pass (Dhrystone 313 against 428 per iteration, rotate90 5
against 7 per pixel). Their rates fell about 22% with the emulator unchanged,
which was then reported as an interpreter slowdown. `make -C bench
check-prebuilt` did not notice because it hashes only the sources, and it does
that only because two builds of the same sources were never byte-identical:
`-g` records the build directory in the debug information.

**2. `cpu-tests` passes only because of where its stack happens to land.**
Building it with GCC 16.2.1 shrinks the code about 0x7a0 bytes. The stack sits
right after `.bss`, so it moves, and on the R4400 interpreter (the full cache
model) two tests break. Both are test bugs; the emulator behaves as the R4400
manual says:
- `cache/index_tag_rt` writes zero tags into D-cache lines 0-3, which
  discards dirty lines without writing them back, as on hardware. With the
  stack on those lines the suite's own state is destroyed and the run derails:
  the rest of the cache group never runs and the MIPS IV tests misreport the
  CPU. Moving only the stack reproduces or removes it, in both directions.
- `umode/load_miss_third` maps one physical page at two virtual addresses
  that differ in bit 14 (its comment says bits 13:12). On an R4400 that raises
  a Virtual Coherency exception, which the suite's default exception handler
  steps over, silently losing the store. It was caught live in the monitor
  (`Cause 0x807c`, VCED, `BadVAddr 0x8825c000`). The GCC 12 layout only hides
  it by keeping a cache line alive.

Any change in code size can trip these, not only a compiler change.

**3. CI cannot see a derailed run.** The `cpu-tests` gate compares only the
number of failing checks with a per-cell limit. A derailed run reports fewer
failures (41 instead of 48 with 17 tests never run), or exactly the same
number (48 with 256 of 262 tests run), so it passes or reads as an
improvement. The limits (124/126/108/110) also sit about 70 above the real
counts (48/50/35/37), so new failures would pass too.

The emulator-side slowdown in the `lightning` build (aded5989) is a separate
fix and not part of this.

## Goals

1. One compiler, pinned to exact package versions, builds every MIPS guest
   binary the repository produces: `cpu-tests`, `bench`, and the embedded
   image.
2. The pin is written in one place, in plain text, so a bump is a visible
   change in review.
3. Anyone with Docker or Podman gets that compiler with one command, on Linux
   or macOS, x86-64 or arm64. Nobody has to install a MIPS cross compiler,
   and an installed one cannot leak into the embedded image.
4. Builds are reproducible: the same sources and pinned compiler give a
   byte-identical ELF in any directory, on any host architecture.
5. CI fails when the committed image is not exactly what the pinned compiler
   builds from the committed sources.
6. `cpu-tests` gives the same results whatever its code size, and CI fails
   when a run does not execute every test.
7. The embedded image is rebuilt with the pinned GCC 16.2.1 in the same
   change. That fixes the benchmark figures.
8. `iris-bench report` warns when the runs it compares used different suite
   images, so results from either side of a compiler change are never set
   side by side without notice.

## Non-goals

- Gating CI on benchmark speed. The workflow's own comment explains why.
- Programs built inside IRIX with MIPSpro (`test/dktest/build.sh` and
  similar).
- Host cross-compilation of iris itself (`scripts/dist`).
- The lightning interpreter regression from aded5989.

## Anti-goals

- A version written in two places that can disagree.
- New bespoke scripts where `make` and `git diff --exit-code` already do the
  job. CI already verifies two generated files that way
  (`bench/golden/golden.h`, `cpu-tests/tests/fpu/fpvectors.c`).
- Making a quick local `make -C cpu-tests run` impossible for someone with a
  different MIPS compiler who does not care about the pin.

## Decisions

**The compiler: Fedora 45, GCC 16.2.1.** Packages
`gcc-mips64-linux-gnu-16.2.1-1.fc45`, `binutils-mips64-linux-gnu-2.47-1.fc45`
and `cross-gcc-common-16.2.1-1.fc45`, installed from Fedora 45's release
repository only (`dnf --repo=fedora`), whose contents never change after
release. Fedora is the only choice that fits:
- It is the only distribution packaging GCC 16.2 for MIPS. Debian sid has
  16.1.0; Debian trixie and Ubuntu 24.04 stop at 14.2; Ubuntu 26.04 and later
  ship no MIPS cross compiler at all.
- Debian and Ubuntu are removing the MIPS cross toolchain (Debian bug
  #1126334 and a release-team block on `gcc-16-cross-mipsen`; Ubuntu
  removed `cross-mipsen` in 25.10, LP #2123952).
- It reproduces the GCC 16.2.1 image that historical results came from: the
  `.text`, `.rodata` and `.data` of a Fedora build are byte-identical to the
  image before 6adc7927. Only debug information differs. So the rebuild
  restores the earlier figures exactly.
- It builds both suites with no flag changes: its multilibs include n32, for
  hard and soft float.

Debian and Ubuntu builds of the same GCC version gave identical code in a
sample; the GCC point release is what changes output, which is why the pin
names exact package versions.

**Where the pin lives.** `cpu-tests/toolchain.mk`, which both suites already
include. It names the base image, by digest, and the exact package versions.
The container definition and CI read the values from there and never repeat
them. `toolchain.mk` also learns the Fedora triple, `mips64-linux-gnu-`.

**The container.** A container definition in the repository installs exactly
the pinned packages from the pinned base image. A `make` target builds the
image and runs a build inside it with `docker` or `podman`, whichever is
installed. The base image is pinned by its multi-architecture digest and runs
natively: Fedora's arm64 and amd64 builds of the same packages produced
byte-identical ELFs. Do not force `linux/amd64` on arm64 hosts: under
emulation `cc1` segfaulted at random in two separate experiments. CI's byte
comparison is the guard if the architectures ever diverge.

**How builds choose a compiler.**
- `make -C bench prebuilt` always builds inside the container. It is the only
  way the embedded image is produced.
- Ordinary `make -C cpu-tests` and `make -C bench` use the pinned compiler
  when it is installed, and print a one-line warning, not an error, when the
  compiler found is a different version. A flag runs them in the container
  instead. (My call: a warning keeps quick local experiments working; CI is
  where the pin is enforced.)

**Reproducibility.** Both suites compile with
`-ffile-prefix-map=<repository root>=.`. The root, not the suite directory,
because `bench` compiles `cpu-tests/harness` sources. The gcc driver passes
it on for `.S` files, so the assembler needs nothing extra. Verified: two
builds from different directories are byte-identical for both suites;
without the flag only the four DWARF sections differ.

**CI.** The `guests` job builds both suites inside the same container,
defined by the same repository files. It then runs `make -C bench prebuilt`
and `git diff --exit-code bench/prebuilt/`, the same check it already runs
for the golden checksums and the FP vectors. The matrix jobs keep consuming
the `guests` artifact, which is now byte-identical to the committed image.

**A fast check without the toolchain.** `make -C bench check-prebuilt`
remains as a quick check that needs no compiler. It confirms that the
compiler recorded in the committed image's debug information (each compile
unit's producer string, such as `GNU C11 16.2.1 20260819`) is the pinned
version, and prints both versions when it is not. It fails immediately and
readably, before the slower rebuild.

**`cpu-tests` fixes.** These make the suite independent of code size:
- `cache/index_tag_rt`: write each line back
  (`cache Index_Writeback_Invalidate_D`) before zeroing its tag, in the same
  asm block, so no load or store can dirty it in between. A linker-script
  change cannot fix this: with a direct-mapped 16 KB D-cache and a 600 KB
  image, some live data always shares those indexes. It still tests what it
  claims.
- `umode/load_miss_third`: give every mapped scratch page the same VA[14:12]
  as its user address. Move the second data page to `_scratch_start +
  0x10000`, align `_scratch_start` to 32 KB in `cpu-tests/harness/link.ld`
  (the TLB and exception tests also map it and only work because bit 14
  happens to be clear), and correct both comments.
- Verified together: Fedora GCC 16.2.1 and Ubuntu GCC 12.4 builds give the
  same results, test by test, on the R4400 and R5000 interpreter (48 and 35
  failing checks, all 262 tests run) and on the R4400 jitv2 cell (50). The
  R5000 jitv2 cell was not run with the fixes. Working patches exist from the
  investigation.

**CI gate on test count.** The runner already prints `RESULT: P checks
passed, F failed[, S tests skipped]  (N tests)`. The `cpu-tests` step fails
unless N equals the number of `TEST(` declarations in `tests/*/*.c` (262
today) and nothing was skipped. The per-cell failure limits are lowered to
the measured counts (48/50/35/37, re-measured on CI's runners after the
switch).

**What is removed.** `bench/prebuilt-stamp.py` and `bench/prebuilt/PROVENANCE`,
because the rebuild-and-diff replaces the source digest. `make -C cpu-tests
toolchain-local`, because it hands out a compiler other than the pin; the
container replaces it. Every document that describes these is updated in the
same change: `bench/README.md`, `bench/prebuilt/README.md`,
`cpu-tests/README.md`, the comments in `suites.yml`, and
`clanker/testing/benchmark-suite-gotchas.md`. Record the two test findings in
`clanker/testing/`.

**Bumping the compiler.** Edit the versions in `toolchain.mk` and run
`make -C bench prebuilt`. Re-run `cpu-tests` on all four cells and compare
test by test. Run the benchmark on the old and new image and put the
per-kernel change in the commit message, which also says that results from
either side are not comparable. The `suite_id` warning in `iris-bench
report` then flags any mixed comparison.

**Fedora 45's lifetime.** Fedora 45 reaches end of life on 2027-12-08, and
its repositories then move to
`https://archives.fedoraproject.org/pub/archive/fedora/linux/releases/45/`.
Before then, either bump to a newer Fedora deliberately or point the
container's repository at the archive. Record this next to the pin. Koji
(`kojipkgs.fedoraproject.org/packages/cross-gcc/`) keeps every build if both
fail.

**Commits.**
1. The `iris-bench report` warning. It is independent and touches only
   `src/bench_report.rs` and `src/bin/iris_bench.rs`; a tested version
   exists. Earlier prototypes of the compiler pin (a `bench/TOOLCHAIN` file,
   a `BENCH_GCC` check in `bench/Makefile`, a compiler line added to
   `PROVENANCE`) are superseded by this design and are not used.
2. The two `cpu-tests` fixes and the test-count gate. They stand on their
   own and must land before the compiler changes, so that the switch can be
   shown to change nothing.
3. The pinned toolchain, the container, reproducible flags, the CI rebuild
   check, the fast check, the removals, the documentation, and the rebuilt
   image. The message carries the before-and-after figures (Dhrystone 313
   against 428 instructions per iteration; about 147 to 187 DMIPS on an
   M2 Max) and says that results recorded on images built since 6adc7927 are
   not comparable with either side.

## Left to the builder

- Names of the `make` targets and the flag that runs a build in the
  container, and how the container definition receives the pinned values.
- The file layout for the container definition.
- Whether the `guests` job uses a `container:` image or calls the `make`
  target with Docker, provided the definition comes from the repository files.
- How to show a useful difference when the CI rebuild does not match (for
  example `diffoscope`, or disassembly of the two ELFs).

## Done when

- Two container builds from different directories, and one on an arm64 Mac
  against one on x86-64 Linux, are byte-identical.
- CI rebuilds the image, finds no difference, and fails when the committed
  image is replaced with the current GCC 14 one (show once, then restore).
- `check-prebuilt` fails clearly on the GCC 14 image and passes on the rebuilt
  one, with no compiler installed.
- The rebuilt image scores 100% accuracy on the R4400 and R5000, interpreter
  and jitv2, and Dhrystone runs 313 instructions per iteration.
- `cpu-tests` runs all 262 tests in every cell with the same failing tests as
  before the switch, and the CI gate fails a run that skips or stops early
  (show once with the unfixed test).
- The documents listed above no longer mention the removed pieces.

## Proposed follow-ups, not part of this change

- **A standard test report.** `cpu-tests` invents its own summary line, which
  is why CI had to count tests by hand. TAP fits a serial console: its plan
  line (`1..262`) makes a truncated run fail by definition, and its `# TODO`
  directive lets known failures be listed by test name next to the tests
  rather than as four counts in a workflow. TAP converts to JUnit XML on the
  host for GitHub's test views.
- **Report Virtual Coherency exceptions.** No test expects one, yet the
  default handler steps over them silently. Counting them and failing the
  test would have named the `load_miss_third` bug directly.
