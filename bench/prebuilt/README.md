# bench/prebuilt — the guest binary, checked in

`iris` links `irisbench.elf` in with `include_bytes!` (`src/benchsuite.rs`) so
the benchmark runs on a machine with no MIPS cross toolchain and no build step:
a released app, a sandboxed one, or anyone who just wants the number.
`make -C bench` still builds `build/irisbench.elf` for development; this is a
copy of a known-good one.

A checked-in build product that can drift is worse than no build product, and
this one drifts dangerously: accuracy is scored against golden checksums
compiled *into* the image, so a stale image against fresh goldens reports
failures that are not real. And the compiler decides how much guest code each
kernel runs, so an image from another compiler is a different benchmark.

Refresh it with `make -C bench prebuilt` after changing anything the guest is
built from — the kernels, the harness, `cpu-tests/harness/`, the link script,
the compiler flags — and commit the result alongside the source change. It
builds with the pinned compiler in its container, from clean.

`.github/workflows/suites.yml` checks it twice. `make -C bench check-prebuilt`
reads the compiler GCC recorded in the image's debug information and fails if
it is not the pinned one, with no toolchain needed. Then CI rebuilds the image
and fails on any byte difference; builds are reproducible, so a difference
means the image is stale, hand-copied, or from another compiler.

Neither check can tell you the image *works*. The embedded-runner test does, by
running the suite out of the binary the image is linked into and requiring 100%
accuracy against the golden checksums compiled into it.
