# Whetstone (C, double precision) for IRIX

The "C Converted Double Precision Whetstones" benchmark, plus a prebuilt N32
MIPS-III binary for running it under IRIX on an emulated machine. Layout as in
`test/ib/`: source, `whet_irix.sh` to build on the guest, `whet_linux.sh` for a
host baseline and the checked-in `whetstone` binary.

## Source and licence

- Source: Rich Painter's version 1.2 (22 March 1998), from
  `https://www.netlib.org/benchmark/whetstone.c`. `whetstone.c` is that file
  byte for byte:
  `333e4ceca042c146f63eec605573d16ae8b07166cbc44a17bec1ea97c6f1efbf`.
- Licence, quoted from the file header:

  > (c) Copyright 1998 Painter Engineering, Inc. All Rights Reserved.
  > Permission is granted to use, duplicate, and publish this text and program
  > as long as it includes this entire comment block and limited rights
  > reference.

  That permits redistribution of the unmodified file, which is how it is kept
  here. The phrase "limited rights reference" is not defined anywhere in the
  file, so the whole header comment is left as is.
- Edits: **none.** The program already times itself with `time(2)`, which
  IRIX has.

## Build

On the IRIX guest, with MIPSpro cc (built here with 7.4.4m):

```sh
sh whet_irix.sh        # cc -O2 -n32 -mips3 -s whetstone.c -lm -o whetstone
```

`whetstone` in this directory was built that way inside an emulated Indy
(IRIX 6.5.22). `-s` strips the symbol table and debug information, which would
otherwise carry the guest's C library build paths. Host baseline: `sh whet_linux.sh` writes `whetstone.host`
(gcc, -O2). The host finishes 100k loops in under a second and the program
refuses to report below one second, so give it a loop count in the hundreds of
thousands.

## Run

```sh
./whetstone 100000
```

The argument is the loop count (default 1000). 100k loops take about 12 s on
the emulated R4400. The program prints its duration in whole seconds and
`C Converted Double Precision Whetstones: N MIPS` (N KIPS when under 1000).
The figure is 100 x loops / seconds, so a 1 s step in the duration moves it
by about 8% at this size.

## Flags

The checked-in binary is `-O2 -n32 -mips3 -lm`, stripped. The same comparison,
of the link without `-s` against a prebuilt N32 MIPS-III Whetstone built with
MIPSpro 7.44 and shared by Dominik Behr, gives an identical `.text` section. Every other section has the same
size except `.debug_line` and `.comment`, which differ by a few bytes (the
build directory name is recorded in them). `-O2 -r10000` is identical in
`.text` too.

| Flags | `.text` vs prebuilt | Host wall, median (s) | Result |
|---|---|---|---|
| `-O2` | identical | 12.5 (n=7, below) | reference |
| `-O2 -g3` | identical size, different bytes | 13.5 | within noise |
| `-O3` | different | 12.1 | within noise, about 8% fewer host instructions |
| `-O1` | different | 35.5 | 2.7x slower |
| `-O0` | different | 50.7 | 3.8x slower |
| `-Ofast` | different | 0.8 | under a second, so the loops were probably removed. Not a valid score |
| `-mips4` | different | not run | needs R5000 or later, SIGILL on an R4400 |

Rows other than `-O2` are n=3 against an n=3 prebuilt reference of 13.3 s. Only `-O2` reproduces the code, so it is the default here. `-O3` and `-g3`
land near the same timing without matching it.

## Results

Same host, guest, iris build and method as in `../dhry/README.md`: one boot
per table pinned to seven cores, seven alternating rounds, wall time from the
host clock less a no-op run. The instruction row is host `instructions:u` of
the iris process (a stand-in; the guest count is not exposed under IRIX).

Binary as checked in (stripped), against the prebuilt one. The host was busy
and slowing down during this run, which moves both binaries alike:

| 100k loops, n=7 | Prebuilt | This directory |
|---|---|---|
| Wall time, median (s) | 14.26 | 13.97 |
| Wall time, min (s) | 13.11 | 13.19 |
| Wall time, max (s) | 14.99 | 14.80 |
| Host instructions, median (G) | 187 | 186 |
| Host instructions, min (G) | 179 | 179 |

Earlier on a quieter host, with the unstripped build (n=7): 12.63 s against
12.45 s median wall, 12.30 s against 12.38 s min, 187 G against 188 G host
instructions.

The medians are within 2% in both runs, inside the spread, and the instruction
counts agree. The printed figure changes in steps of one second (about 8%), so
read the host step time.

## Running it from iris-bench

`../../bench/irix/benchies.toml` holds the `iris-bench irix` step for this
program, and explains how to get the binary into a guest with `iris-ci put`.
