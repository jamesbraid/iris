# Testing and benchmarking

The suite commands and benchmarking guidance previously in README.md.
Build-feature and configuration combinations are listed in
[FEATURES.md](FEATURES.md); outstanding functionality is tracked in
[TODO.md](clanker/TODO.md).

## Testing and benchmarking

Two bare-metal MIPS suites run on the emulated CPU with no operating system in
the way. They answer different questions and neither replaces the other.

**`cpu-tests/`** — is this instruction correct? ~250 self-checking tests over
ALU, FPU, TLB, caches, exceptions and the MIPS IV additions, one instruction at
a time with clean state. The expectations are validated on real Indys (R4400
and R5000, both passing every check); see
[cpu-tests/README.md](cpu-tests/README.md).

```sh
sudo apt-get install gcc-mips-linux-gnu binutils-mips-linux-gnu   # or: make -C cpu-tests toolchain-local
make -C cpu-tests && make -C cpu-tests run
cpu-tests/run/matrix.sh                 # R4400/R5000 x interp/jitv2
```

**`bench/`** — how fast is this build, and is it still right after ten million
of them? 46 kernels covering integer, FPU, the cache hierarchy, image and video
editing inner loops, compression, and the emulator-only paths (TLB refill,
exception round trip, cache maintenance, uncached I/O). Every kernel checksums
its result against a golden value computed by building the same C natively, so
each run reports an accuracy percentage next to its throughput — and per-kernel
**guest instructions per host second**, which is directly comparable between the
interpreter and jitv2.

```sh
cargo build --release --bin iris-bench
./target/release/iris-bench run         # ~60 s; --quick for about half that
./target/release/iris-bench run --quick

make -C bench && make -C bench hostbench            # only to change the suite
./target/release/iris-bench matrix      # builds and runs every CPU x engine cell
./target/release/iris-bench host        # the same kernels, natively, for the ratio
```

`run` needs **no MIPS toolchain and no build step**: a known-good guest binary
is checked in at `bench/prebuilt/` and linked into `iris`, and the run happens
in-process on a headless machine the emulator builds for itself. That is also
what the GUI's **Benchmark tab** does, on every platform and inside the App
Store sandbox — one button, and the accuracy score sits next to the speed.

Includes Dhrystone 2.1 (DMIPS) and LINPACK 100x100 (MFLOPS), so an emulated
Indy can be put next to published figures for a real one, plus a Whetstone mix
(reported in passes/s — see bench/README.md for why not MWIPS). See
[bench/README.md](bench/README.md) — and
[clanker/testing/benchmark-suite-gotchas.md](clanker/testing/benchmark-suite-gotchas.md)
before adding a kernel.

`bench/irix/` is the other half: real workloads under a booted IRIX (filesystem,
buffer cache, IRIX's own tools, X on REX3), driven over `iris-ci`.

**GPU parity tests** — with the `wgpu` feature, the host-GPU raster path
(`src/wgpu`, `src/dev/odyssey/wgpu`) is checked against the CPU pipeline: the
same triangles, shaded fragments and clears go through both, and the two board
memories are compared word for word. They need a GPU, or lavapipe, and print
`skipped` without one.

```sh
cargo test --features wgpu --lib wgpu::tests odyssey::wgpu
```

**GL suites in a live guest** — `test/gltest/glsuite.sh` runs every GL check
program inside a booted IRIX (glstate, blendtest, seltest, cicopy, pixtest in
four visuals, the visual, texture and line sweeps, glprim scenes read back
through GL and captured with `scrsave`, and piglit's VPro list), and
`test/gltest/xsweep.sh` restarts the X server in each root visual (PseudoColor
8 and 12, TrueColor 12, 15, 24 and 30, DirectColor 24) and checks a GL scene
in each. Both write to `/shared/results/TAG`; the host-side
`test/gltest/glsuite-compare.sh A B` lists every difference between two runs.
Running both on the CPU and the GPU raster unit and comparing is how the GPU
unit is checked against the reference end to end.
The same suite on a real board, from the same binaries, against a guest
emulating that board is how the model is checked against hardware; the
board's quirks then show up as differences instead of as failures.

```sh
mkdir -p shared/gltest && cp test/gltest/* shared/gltest/       # piglit: shared/piglit (clanker/testing/piglit-on-irix.md)
tools/irix-sh.py run 'cp /shared/gltest/glsuite.sh /shared/gltest/xsweep.sh /usr/tmp/' \
    '(nohup sh -c "sh /usr/tmp/glsuite.sh gpu; sh /usr/tmp/xsweep.sh gpu" > /usr/tmp/suite.out 2>&1 &)'
test/gltest/glsuite-compare.sh shared/results/cpu shared/results/gpu
```

The suite copies `/shared/gltest` to `/usr/tmp` and runs the programs from
there, so build them once in the guest (`sh irix.sh` in a copy, see the
`irix-gfx-loop` skill) and copy the binaries back into `shared/gltest`. The whole run takes about 45 minutes, most of it piglit. Short
recordings of a failing case (`vpro rec`) replay on both raster units offline:
`the_raster_units_agree_on_recorded_sessions` replays every recording in
`ignore/recordings/gpu-cases` (or `VPRO_PARITY_DIR`) on the CPU unit, the GPU
unit and the GPU unit with live-style slice ends, and compares what the board
wrote to guest memory and its final memory.

## Disk persistence on exit

`src/exit_persistence_tests.rs` builds every machine (Indy, Indigo2, Indigo2
IMPACT, O2, Octane) with one disk of each kind (raw with an overlay, raw in
place, compressed CHD, CHD with the copy-on-write flag, scratch volume), writes
sectors with WRITE(10), ends the session each way iris can end (guest power-off,
`quit` over the CI socket, `quit` at the terminal console, `main` returning,
drop), with and without a guest reset first, then builds the machine again over
the same files and reads every sector back. Another test fails on any
`process::exit` in the library that does not go through
`workstation::exit_process`, which makes its caller flush the disks first.

```sh
cargo nextest run --release --lib exit_persistence_tests
```

**Acceptance scenarios** — `iris-accept` boots a machine from a prepared disk
and checks the things that need a whole guest: the console reaches a login,
`hinv` reports the CPUs, `l2 probe` shows no held cache lines, the network
answers, and `halt -p` ends the iris process by itself. One TOML file per
machine lives in `test/accept/`; the program knows nothing about any machine.
`cargo test` parses and validates every scenario but boots none, so it passes
with no disk image. A run needs the images the scenario's `[assets]` names, in
a directory you point it at; a scenario whose images are missing is skipped
(exit status 77).

```sh
cargo build --release --bin iris --bin iris-ci --bin iris-accept
target/release/iris-accept check test/accept/*.toml        # validate only
target/release/iris-accept run test/accept/o2-r5000.toml \
    --run-root /big/disk/runs --assets /big/disk/assets --runs 3
target/release/iris-accept summary /big/disk/runs/results/*.json
```

A scenario has an `[iris]` table that is merged into the generated
`iris.toml`, `[[phase]]` entries (a console string, a timeout, optionally text
to type), `[[step]]` entries (a guest command with `run`, or an emulator
monitor command with `monitor`, plus `expect`/`reject` regexes matched per
line and the required exit status), `[[forbid]]` strings with an allowed
count, and `[finish]` (the command that powers the guest off and how long iris
has to exit). Each run writes one JSON file under `--out` with the time each
phase was reached, every step's verdict and output, the host load, and the
BLAKE3 of the iris binary.

The harness boots the disk with `overlay = true` and puts the overlay in the
run directory, so the image is never written; give it a copy made for the
purpose anyway. It refuses to start with less than `--min-free-gb` (150) free on
the run root, with `--max-guests` (2) guests already running under it, or with
a run root inside the system temporary directory. A run whose logs show
`Media error: Write error` is reported as invalid, not failed. Timeouts
stretch when the host's load exceeds its CPU count (`--timeout-scale` sets it).

