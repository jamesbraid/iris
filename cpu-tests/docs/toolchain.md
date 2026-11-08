# Toolchain

## What the suite is built with

One pinned compiler builds every MIPS guest binary in the repository: this
suite, `bench/`, and the benchmark image iris embeds. It is GCC 16.2.1 for
`mips64-linux-gnu` from Fedora 45, and `toolchain.mk` is the only place its
version is written. The compiler is part of what the benchmark measures (GCC
14.2 runs Dhrystone in 37% more guest instructions than 16.2.1), and a change
in code size moves this suite's stack, so an unpinned compiler changes results
in both suites. It targets:

```
-march=mips3 -mabi=n32 -EB -mno-abicalls -fno-pic -G0 -msoft-float
```

Two of those are load-bearing and were arrived at the hard way.

### `-mabi=n32`, not `-mabi=32`

Under **o32 a `long long` lives in an even/odd register pair**, so an inline-asm
operand like `"=r"(u64)` binds only the *first* register of the pair. Every
64-bit test silently reads half a result — and on a big-endian target the half
it reads is the high word, so `addu` producing `0x80000000` came back as
`0x8000000000000000` and looked like a CPU bug. n32 keeps 64-bit values in
single registers, which is exactly what a MIPS III/IV suite needs.

n32 is still **ELF32 MSB** (`elf32-ntradbigmips`), so `--load-elf` and the PROM
both take it; only `e_flags` differs (`abi2`).

### No libgcc

Cross packages may ship only an **o32** `libgcc.a` (Debian's does). Linking it into an
n32 image would be silently wrong. It is also unnecessary: under n32 every
64-bit operation the C code performs — including the `% 10` / `/= 10` in
`con_udec` — is a native instruction rather than a `__udivdi3` call. `LIBGCC`
is deliberately empty in the Makefile, so if a helper reference ever does
appear the link fails loudly instead of pulling in the wrong ABI.

### Numeric registers in `start.S`

o32 and n32/n64 disagree about which hardware register each `$tN` name means,
and under n32 `$t4`–`$t7` **do not exist** (`$8`–`$11` are `$a4`–`$a7` there).
The bootstrap uses numeric names (`$8`–`$14`) so it assembles identically under
any ABI.

## Getting it

With Docker or Podman, on Linux or macOS, x86-64 or arm64, nothing needs
installing. Build the toolchain container once, then put `container-` in front
of any target:

```sh
make -C cpu-tests toolchain-image
make -C cpu-tests container-all     # same as `make all`, with the pinned GCC
make -C bench container-all
```

The container runs natively on either architecture; both build the same bytes.
On macOS with Colima the checkout must be under your home directory, because
Colima shares only that with its VM.

On Fedora 45 itself, install the packages `toolchain.mk` names
(`GUEST_TC_PACKAGES`) and plain `make` finds `mips64-linux-gnu-gcc`.

Any other MIPS cross compiler still works for quick local runs (`make
CROSS=...`, or one of the names `toolchain.mk` probes for), with a warning:
its binaries are not the ones CI tests, and benchmark figures from it are not
comparable.
