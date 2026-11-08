# Dhrystone 2.1 for IRIX

Dhrystone 2.1 in C, plus a prebuilt N32 MIPS-III binary for running it under
IRIX on an emulated machine. It follows the `test/ib/` layout: sources,
`dhry_irix.sh` to build on the guest, `dhry_linux.sh` for a host baseline and
the checked-in `dhrystone` binary.

## Source and licence

- Where it came from: netlib, `https://www.netlib.org/benchmark/dhry-c`, the
  shar of Reinhold P. Weicker's C Dhrystone, version 2.1, dated 25 May 1988.
  `dhry.h`, `dhry_1.c` and `dhry_2.c` are the unpacked files, byte for byte
  (sha256 below), so they can be checked against a fresh download.
- **Redistribution terms are not stated.** Neither the source files nor the
  shar carry a copyright notice or a licence. The rationale shipped in the same
  shar says version 2.1 "will be distributed as widely as possible", and the
  header asks users to report results and follow the published ground rules.
  That shows an intent to share. It is not a grant, and no permission to
  redistribute the files, or binaries built from them, is written down
  anywhere we found. Until the maintainer decides, treat the status of
  redistributing them here as unconfirmed. If the answer is no, the sources
  and the binary can be dropped and fetched from netlib at build time
  instead; `dhry_irix.sh` and the steps file do not otherwise change.
- The header block in each file stays intact.
- Edits: **none.** The files are unmodified. The times() clock would need an
  edit on IRIX (`dhry_1.c` redeclares `times()` and the header defines no
  `HZ`), so the build uses the stock `-DTIME` path, which calls `time(2)`.

```
6593c6e4e8fe3a483925b2fb6e100d9e65451633da5f519cc40ba49b93859010  dhry.h
442e47760cd43c1e6d87cfc0c03134101d97110d417e11a29b5ed42c0eb4767b  dhry_1.c
7954383013caead70e8283375b249ecbaa1aaf092b526acbcac3f187a015c108  dhry_2.c
```

## Build

On the IRIX guest, with MIPSpro cc (built here with 7.4.4m):

```sh
sh dhry_irix.sh        # cc -O2 -n32 -mips3 -s -DTIME dhry_1.c dhry_2.c -o dhrystone
```

`dhrystone` in this directory was built that way inside an emulated Indy
(IRIX 6.5.22). `-s` strips the symbol table and debug information, which would
otherwise carry the guest's C library build paths. MIPSpro prints a licence-server notice and a few warnings
(missing returns, pointer-to-int casts). It still produces the binary.

Host baseline: `sh dhry_linux.sh` writes `dhrystone.host` (gcc, gnu89, -O2).
`time(2)` has one-second resolution and a modern host finishes 10M runs in
well under a second, so give it a run count of a few hundred million.

## Run

The program reads the run count from stdin and times itself with `time(2)`:

```sh
echo 10000000 | ./dhrystone
```

It prints `Dhrystones per Second`, which has one-second resolution. It
refuses to report under 2 s of runtime. 10M runs take about 20 s on the
emulated R4400. Check the "should be" lines too: `Arr_2_Glob[8][7]` must be
the run count plus 10.

## Flags

The checked-in binary is `-O2 -n32 -mips3 -DTIME`, stripped. A prebuilt N32
MIPS-III Dhrystone, built with MIPSpro 7.44 and shared by Dominik Behr, was
compared against the same link without `-s`, and the code in the two is identical: same `.text` bytes, and every
other section has the same size except `.debug_line` and `.comment`, which
differ by a few bytes (the build directory name is recorded in them).
`-O2 -r10000` also produces the identical `.text` for this program, so
that flag changes nothing here. No other level matched:

| Flags | `.text` vs prebuilt | Host wall, median (s) | Result |
|---|---|---|---|
| `-O2` | identical | 20.6 (n=7, below) | reference |
| `-O2 -g3` | identical size, different bytes | 21.4 | within noise |
| `-O3` | different | 21.1 | within noise, 1% more host instructions |
| `-O1` | different | 33.9 | 1.5x slower |
| `-O0` | different | 37.3 | 1.7x slower |
| `-Ofast` | different | 10.7 | 2x faster, but a different program |
| `-mips4` | different | not run | needs R5000 or later, SIGILL on an R4400 |

The `-O1`..`-Ofast` and `-g3` rows are n=3 against an n=3 prebuilt reference of 22.3 s. `-O3` and `-O2 -g3` reproduce the timing without reproducing the code. The
only flag set that reproduces the code is `-O2`, so that is the default here.

## Results

One Indy (`indy_ip24`, R4400 at 150 MHz, 2x128 MB), IRIX 6.5.22m, iris
`404a86c7` (upstream main at the time) with the default JIT dispatch. One
boot per table, pinned to seven cores, the two binaries alternated A/B then B/A
over seven rounds. Wall time is the host clock around one `iris-ci run`, minus a
measured no-op run (about 0.21 s). The instruction figure is the iris
process's host `instructions:u` over the run. It is a stand-in: the guest
instruction count is not exposed under IRIX (CP0 Count follows the wall clock,
and the bare-metal test device's counter only exists for `bench/` kernels). It
also counts the JIT compile threads.

Binary as checked in (stripped), against the prebuilt one. The host was busy and
slowing down during this run (load average 100 to 170 on a shared machine),
so the wall times climb from round to round for both binaries alike:

| 10M runs, n=7 | Prebuilt | This directory |
|---|---|---|
| Wall time, median (s) | 25.43 | 25.49 |
| Wall time, min (s) | 21.55 | 20.98 |
| Wall time, max (s) | 26.99 | 30.03 |
| Host instructions, median (G) | 265 | 261 |
| Host instructions, min (G) | 261 | 252 |

Earlier on a quieter host, the same comparison with the unstripped build
(n=7): 20.33 s against 20.59 s median wall, 19.31 s against 20.13 s min, 265 G
against 266 G host instructions.

The medians are within 1.3% in both runs, far inside the spread within each
binary, and the instruction counts agree, so the two are the same workload.
The printed Dhrystones/s only takes values 1 s apart (about 5%) and tracked the
host load. Use the host step time and compare runs on the same boot.
